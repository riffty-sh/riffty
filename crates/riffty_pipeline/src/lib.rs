pub mod orchestrator;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub use orchestrator::{
    AnalyzerRegistry, PythonLanguageAnalyzer, WorkspaceOrchestrator, WorkspaceRunOutput,
    render_unified_diff, sanitize_diagnostic_for_source,
};
use riffty_core::{
    AnalysisProvenance, AnalysisView, Applicability, ByteOffset, CanonicalSyntaxTree, Diagnostic,
    DiffTarget, Edit, FileId, GitDiffPort, ImportManagerPort, ImportSpec, LineIndex, NodeKind,
    PatchApplierPort, PatternMatcherPort, RulePlugin, ScopedImportRequest, Severity,
    SyntaxParserPort, TextRange, TypeOraclePort, WorkspaceFile, WorkspaceVfsPort,
};
use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_FIXPOINT_PASSES: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineConfig {
    pub apply_fixes: bool,
    pub allow_unsafe_fixes: bool,
    pub max_passes: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            apply_fixes: false,
            allow_unsafe_fixes: false,
            max_passes: DEFAULT_MAX_FIXPOINT_PASSES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedFixRecord {
    pub file: String,
    pub line: u32,
    pub rule: String,
    pub action: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRunResult {
    pub file_id: FileId,
    pub relative_path: String,
    pub final_source: String,
    pub modified: bool,
    pub passes_executed: usize,
    pub applied_fixes: Vec<AppliedFixRecord>,
    pub remaining_diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictResolutionOutcome {
    pub concrete_edits: Vec<Edit>,
    pub accepted_diagnostics: Vec<Diagnostic>,
    pub deferred_diagnostics: Vec<Diagnostic>,
}

/// Traverses `CanonicalSyntaxTree` in a single $O(\text{nodes})$ pass (`R04`),
/// dispatching each node strictly to `RulePlugin`s subscribed to `node.kind`.
pub fn dispatch_single_pass(
    view: &AnalysisView<'_>,
    rules: &[Box<dyn RulePlugin>],
) -> Vec<Diagnostic> {
    let mut table: [Vec<&dyn RulePlugin>; NodeKind::COUNT] = std::array::from_fn(|_| Vec::new());
    for rule in rules {
        let mut registered = [false; NodeKind::COUNT];
        for &kind in rule.target_node_kinds() {
            let idx = kind.as_index();
            if !registered[idx] {
                registered[idx] = true;
                table[idx].push(rule.as_ref());
            }
        }
    }

    let mut diagnostics = Vec::new();
    for node in view.tree.iter_nodes() {
        let subscribed = &table[node.kind.as_index()];
        if subscribed.is_empty() {
            continue;
        }
        for plugin in subscribed {
            for diag in plugin.evaluate(&node, view) {
                if !view.tree.is_suppressed(diag.range.start, &diag.rule_id) {
                    diagnostics.push(diag);
                }
            }
        }
    }

    diagnostics.sort_by(|a, b| {
        a.file_id
            .cmp(&b.file_id)
            .then_with(|| a.range.start.cmp(&b.range.start))
            .then_with(|| a.range.end.cmp(&b.range.end))
            .then_with(|| b.severity.priority_rank().cmp(&a.severity.priority_rank()))
            .then_with(|| a.rule_id.cmp(&b.rule_id))
    });
    diagnostics
}

/// Resolves `Fix::required_imports` via lexical scope-aware `ImportManagerPort::resolve_scoped_import_edits` (`R06`, `R19`)
/// and selects a maximal non-overlapping set of fixes ordered by `(Severity desc, range.start asc, range.end asc, rule_id asc)` (`R07`).
pub fn resolve_non_overlapping_fixes_scoped(
    tree: &CanonicalSyntaxTree,
    diagnostics: &[Diagnostic],
    import_manager: &dyn ImportManagerPort,
    max_applicability: Applicability,
) -> ConflictResolutionOutcome {
    resolve_non_overlapping_fixes_inner(
        &tree.source,
        Some(tree),
        diagnostics,
        import_manager,
        max_applicability,
    )
}

/// Resolves `Fix::required_imports` via `ImportManagerPort` (`R06`) and selects a maximal
/// non-overlapping set of fixes ordered by `(Severity desc, range.start asc, range.end asc, rule_id asc)` (`R07`).
pub fn resolve_non_overlapping_fixes(
    source: &str,
    diagnostics: &[Diagnostic],
    import_manager: &dyn ImportManagerPort,
    max_applicability: Applicability,
) -> ConflictResolutionOutcome {
    resolve_non_overlapping_fixes_inner(
        source,
        None,
        diagnostics,
        import_manager,
        max_applicability,
    )
}

fn resolve_non_overlapping_fixes_inner(
    source: &str,
    tree: Option<&CanonicalSyntaxTree>,
    diagnostics: &[Diagnostic],
    import_manager: &dyn ImportManagerPort,
    max_applicability: Applicability,
) -> ConflictResolutionOutcome {
    resolve_non_overlapping_fixes_with_imports(
        source,
        diagnostics,
        max_applicability,
        |scoped_requests, required_imports| {
            if let Some(syntax_tree) = tree {
                import_manager.resolve_scoped_import_edits(syntax_tree, scoped_requests)
            } else {
                import_manager.resolve_import_edits(source, required_imports)
            }
        },
    )
}

pub(crate) fn resolve_non_overlapping_fixes_with_imports<F>(
    source: &str,
    diagnostics: &[Diagnostic],
    max_applicability: Applicability,
    resolve_imports: F,
) -> ConflictResolutionOutcome
where
    F: FnOnce(&[ScopedImportRequest], &[ImportSpec]) -> Vec<Edit>,
{
    let mut candidates: Vec<&Diagnostic> = diagnostics
        .iter()
        .filter(|d| {
            d.fix.as_ref().is_some_and(|f| {
                f.applicability <= max_applicability
                    && (!f.edits.is_empty() || !f.required_imports.is_empty())
            })
        })
        .collect();

    // Higher severity wins first; on equal severity, earlier byte offset wins (`R07`).
    candidates.sort_by(|a, b| {
        b.severity
            .priority_rank()
            .cmp(&a.severity.priority_rank())
            .then_with(|| a.range.start.cmp(&b.range.start))
            .then_with(|| a.range.end.cmp(&b.range.end))
            .then_with(|| a.rule_id.cmp(&b.rule_id))
    });

    let mut accepted_edits: Vec<Edit> = Vec::new();
    let mut accepted_diagnostics: Vec<Diagnostic> = Vec::new();
    let mut deferred_diagnostics: Vec<Diagnostic> = Vec::new();
    let mut required_imports: Vec<ImportSpec> = Vec::new();
    let mut seen_imports: HashSet<ImportSpec> = HashSet::new();
    let mut scoped_requests: Vec<ScopedImportRequest> = Vec::new();

    for diag in candidates {
        let Some(fix) = &diag.fix else {
            continue;
        };
        if !validate_internal_fix_edits(source, &fix.edits) {
            deferred_diagnostics.push(diag.clone());
            continue;
        }
        let overlaps_existing = fix.edits.iter().any(|candidate_edit| {
            accepted_edits
                .iter()
                .any(|acc| edits_conflict(candidate_edit, acc))
        });

        if overlaps_existing {
            deferred_diagnostics.push(diag.clone());
        } else {
            accepted_edits.extend(fix.edits.iter().cloned());
            let replaces_fqname = diag
                .type_proof
                .as_ref()
                .and_then(|tp| tp.get("resolved_fqname"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            for imp in &fix.required_imports {
                if seen_imports.insert(imp.clone()) {
                    required_imports.push(imp.clone());
                }
                scoped_requests.push(ScopedImportRequest {
                    spec: imp.clone(),
                    use_range: diag.range,
                    replaces_fqname: replaces_fqname.clone(),
                });
            }
            accepted_diagnostics.push(diag.clone());
        }
    }

    if !scoped_requests.is_empty() {
        let import_edits = resolve_imports(&scoped_requests, &required_imports);
        for imp_edit in import_edits {
            if validate_edit_bounds(source, &imp_edit)
                && !accepted_edits
                    .iter()
                    .any(|existing| edits_conflict(&imp_edit, existing))
            {
                accepted_edits.push(imp_edit);
            }
        }
    }

    accepted_edits.sort_by(|a, b| {
        a.range
            .start
            .cmp(&b.range.start)
            .then(a.range.end.cmp(&b.range.end))
    });

    ConflictResolutionOutcome {
        concrete_edits: accepted_edits,
        accepted_diagnostics,
        deferred_diagnostics,
    }
}

/// Applies sorted, non-overlapping UTF-8 byte `Edit`s strictly bottom-to-top (`R08`)
/// so earlier byte offsets remain invariant throughout patching.
pub fn apply_edits_bottom_to_top(source: &str, sorted_edits: &[Edit]) -> Result<String, String> {
    if sorted_edits.is_empty() {
        return Ok(source.to_string());
    }
    for (idx, edit) in sorted_edits.iter().enumerate() {
        if !validate_edit_bounds(source, edit) {
            return Err(format!(
                "edit range [{}..{}) is not a valid UTF-8 boundary for source of len {}",
                edit.range.start.0,
                edit.range.end.0,
                source.len()
            ));
        }
        if let Some(prev) = idx.checked_sub(1).and_then(|p| sorted_edits.get(p))
            && (prev.range.end.0 > edit.range.start.0
                || (prev.range.is_empty()
                    && edit.range.is_empty()
                    && prev.range.start.0 == edit.range.start.0))
        {
            return Err(format!(
                "overlapping or unsorted edits [{:?}, {:?}] passed to apply_edits_bottom_to_top",
                prev.range, edit.range
            ));
        }
    }

    let mut out = source.to_string();
    for edit in sorted_edits.iter().rev() {
        let start = edit.range.start.0 as usize;
        let end = edit.range.end.0 as usize;
        out.replace_range(start..end, &edit.replacement);
    }
    Ok(out)
}

/// Shifts a set of tracked `TextRange`s across a sequence of sorted non-overlapping `Edit`s,
/// preserving accurate diff-scope coordinates across multi-pass `auto_heal` iterations.
pub fn shift_ranges_after_edits(ranges: &[TextRange], sorted_edits: &[Edit]) -> Vec<TextRange> {
    if sorted_edits.is_empty() {
        return ranges.to_vec();
    }
    ranges
        .iter()
        .map(|r| {
            let mut new_start = i64::from(r.start.0);
            let mut new_end = i64::from(r.end.0);
            for edit in sorted_edits {
                let es = i64::from(edit.range.start.0);
                let ee = i64::from(edit.range.end.0);
                let delta = (edit.replacement.len() as i64) - (ee - es);
                if ee <= i64::from(r.start.0) {
                    new_start += delta;
                    new_end += delta;
                } else if es < i64::from(r.end.0) {
                    // Edit occurred inside or intersecting the tracked span: expand/adjust end
                    new_end = (new_end + delta).max(new_start);
                }
            }
            TextRange::new(new_start.max(0) as u32, new_end.max(0) as u32)
        })
        .collect()
}

pub(crate) fn validate_edit_bounds(source: &str, edit: &Edit) -> bool {
    let s = edit.range.start.0 as usize;
    let e = edit.range.end.0 as usize;
    s <= e && e <= source.len() && source.is_char_boundary(s) && source.is_char_boundary(e)
}

fn validate_internal_fix_edits(source: &str, edits: &[Edit]) -> bool {
    for (i, e1) in edits.iter().enumerate() {
        if !validate_edit_bounds(source, e1) {
            return false;
        }
        for e2 in edits.iter().skip(i + 1) {
            if edits_conflict(e1, e2) {
                return false;
            }
        }
    }
    true
}

pub fn edits_conflict(a: &Edit, b: &Edit) -> bool {
    if a.range.is_empty() && b.range.is_empty() {
        return a.range.start == b.range.start;
    }
    if a.range.is_empty() {
        return b.range.start.0 < a.range.start.0 && a.range.start.0 < b.range.end.0;
    }
    if b.range.is_empty() {
        return a.range.start.0 < b.range.start.0 && b.range.start.0 < a.range.end.0;
    }
    a.range.start.0 < b.range.end.0 && b.range.start.0 < a.range.end.0
}

pub struct RifftyEngine<'a> {
    pub parser: &'a dyn SyntaxParserPort,
    pub oracle: &'a dyn TypeOraclePort,
    pub import_manager: &'a dyn ImportManagerPort,
    pub pattern_matcher: Option<&'a dyn PatternMatcherPort>,
    pub rules: Vec<Box<dyn RulePlugin>>,
}

impl<'a> RifftyEngine<'a> {
    /// Runs the composable pipeline across `files`, executing up to `config.max_passes` (capped at 10)
    /// when `config.apply_fixes` is enabled (`R04`–`R09`).
    /// Optional `scoped_ranges` restricts diagnostics and auto-fixes per `FileId` (used by `--diff` and `ReviewFacade`).
    pub fn run_workspace(
        &self,
        files: &[WorkspaceFile],
        config: PipelineConfig,
        scoped_ranges: Option<&HashMap<FileId, Vec<TextRange>>>,
    ) -> Result<Vec<FileRunResult>, String> {
        let max_passes = config.max_passes.clamp(1, DEFAULT_MAX_FIXPOINT_PASSES);
        let max_app = if config.allow_unsafe_fixes {
            Applicability::Unsafe
        } else {
            Applicability::Safe
        };

        let mut working_files: Vec<WorkspaceFile> = files.to_vec();
        let mut active_scopes: Option<HashMap<FileId, Vec<TextRange>>> = scoped_ranges.cloned();
        let mut applied_by_file: BTreeMap<FileId, Vec<AppliedFixRecord>> = BTreeMap::new();
        let mut modified_files: HashSet<FileId> = HashSet::new();
        let mut seen_states: HashMap<FileId, HashSet<String>> = HashMap::new();
        for wf in &working_files {
            let mut set = HashSet::new();
            set.insert(wf.source.clone());
            seen_states.insert(wf.file_id, set);
        }

        let mut passes_executed = 0usize;
        let mut latest_diagnostics: BTreeMap<FileId, Vec<Diagnostic>> = BTreeMap::new();

        for _pass in 0..max_passes {
            passes_executed += 1;
            self.oracle.index_workspace(&working_files);

            let mut parsed_trees: HashMap<FileId, CanonicalSyntaxTree> = HashMap::new();
            for wf in &working_files {
                if let Some(scopes) = &active_scopes
                    && !scopes.contains_key(&wf.file_id)
                {
                    continue;
                }
                let tree = self.parser.parse(wf.file_id, &wf.source)?;
                parsed_trees.insert(wf.file_id, tree);
            }

            latest_diagnostics.clear();
            let mut any_file_patched = false;

            for wf in &mut working_files {
                let Some(tree) = parsed_trees.get(&wf.file_id) else {
                    continue;
                };
                let view = AnalysisView {
                    file_id: wf.file_id,
                    file_path: &wf.relative_path,
                    tree,
                    oracle: self.oracle,
                    pattern_matcher: self.pattern_matcher,
                };
                let mut diags = dispatch_single_pass(&view, &self.rules);
                if let Some(scopes) = &active_scopes {
                    let allowed_spans = scopes.get(&wf.file_id).map(Vec::as_slice).unwrap_or(&[]);
                    diags.retain(|d| allowed_spans.iter().any(|sp| sp.intersects(d.range)));
                }

                if !config.apply_fixes {
                    latest_diagnostics.insert(wf.file_id, diags);
                    continue;
                }

                let resolution = resolve_non_overlapping_fixes_scoped(
                    tree,
                    &diags,
                    self.import_manager,
                    max_app,
                );

                if resolution.concrete_edits.is_empty() {
                    latest_diagnostics.insert(wf.file_id, diags);
                    continue;
                }

                let patched = apply_edits_bottom_to_top(&wf.source, &resolution.concrete_edits)?;
                let state_set = seen_states.entry(wf.file_id).or_default();
                if patched == wf.source || !state_set.insert(patched.clone()) {
                    // Cycle or no-op guard (`R09`)
                    latest_diagnostics.insert(wf.file_id, diags);
                    continue;
                }

                for accepted in &resolution.accepted_diagnostics {
                    let action = accepted
                        .fix
                        .as_ref()
                        .map(|f| f.title.clone())
                        .unwrap_or_else(|| accepted.message.clone());
                    applied_by_file
                        .entry(wf.file_id)
                        .or_default()
                        .push(AppliedFixRecord {
                            file: wf.relative_path.clone(),
                            line: tree.line_of(accepted.range.start),
                            rule: accepted.rule_id.clone(),
                            action,
                        });
                }

                if let Some(scopes) = &mut active_scopes
                    && let Some(spans) = scopes.get_mut(&wf.file_id)
                {
                    *spans = shift_ranges_after_edits(spans, &resolution.concrete_edits);
                }

                wf.source = patched;
                modified_files.insert(wf.file_id);
                any_file_patched = true;
            }

            if !config.apply_fixes || !any_file_patched {
                break;
            }

            // If we hit the final allowed pass (`max_passes`), perform one final diagnostic pass on converged sources.
            if passes_executed == max_passes {
                self.oracle.index_workspace(&working_files);
                latest_diagnostics.clear();
                for wf in &working_files {
                    if let Some(scopes) = &active_scopes
                        && !scopes.contains_key(&wf.file_id)
                    {
                        continue;
                    }
                    let tree = self.parser.parse(wf.file_id, &wf.source)?;
                    let view = AnalysisView {
                        file_id: wf.file_id,
                        file_path: &wf.relative_path,
                        tree: &tree,
                        oracle: self.oracle,
                        pattern_matcher: self.pattern_matcher,
                    };
                    let mut diags = dispatch_single_pass(&view, &self.rules);
                    if let Some(scopes) = &active_scopes {
                        let allowed = scopes.get(&wf.file_id).map(Vec::as_slice).unwrap_or(&[]);
                        diags.retain(|d| allowed.iter().any(|sp| sp.intersects(d.range)));
                    }
                    latest_diagnostics.insert(wf.file_id, diags);
                }
            }
        }

        let mut results = Vec::with_capacity(working_files.len());
        for wf in working_files {
            if let Some(scopes) = &active_scopes
                && !scopes.contains_key(&wf.file_id)
            {
                continue;
            }
            let remaining = latest_diagnostics.remove(&wf.file_id).unwrap_or_default();
            let applied = applied_by_file.remove(&wf.file_id).unwrap_or_default();
            let modified = modified_files.contains(&wf.file_id);
            results.push(FileRunResult {
                file_id: wf.file_id,
                relative_path: wf.relative_path,
                final_source: wf.source,
                modified,
                passes_executed,
                applied_fixes: applied,
                remaining_diagnostics: remaining,
            });
        }
        Ok(results)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewChangesRequest {
    pub target: String,
    #[serde(default = "default_base_ref")]
    pub base_ref: String,
    #[serde(default = "default_auto_heal")]
    pub auto_heal: bool,
}

fn default_base_ref() -> String {
    "main".to_string()
}

const fn default_auto_heal() -> bool {
    true
}

impl ReviewChangesRequest {
    pub fn to_diff_target(&self) -> Result<DiffTarget, String> {
        match self.target.trim() {
            "unstaged" => Ok(DiffTarget::Unstaged),
            "staged" => Ok(DiffTarget::Staged),
            "branch_diff" => Ok(DiffTarget::BranchDiff {
                base_ref: if self.base_ref.trim().is_empty() {
                    "main".to_string()
                } else {
                    self.base_ref.trim().to_string()
                },
            }),
            other => Err(format!(
                "invalid target `{other}`: expected `unstaged`, `staged`, or `branch_diff`"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub files_reviewed: usize,
    pub auto_healed_count: usize,
    pub blocking_issues: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockingComment {
    pub id: String,
    pub file: String,
    pub line: u32,
    pub rule: String,
    pub severity: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_proof: Option<serde_json::Value>,
    pub suggested_action: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewVerdict {
    pub verdict: String,
    pub summary: ReviewSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<AnalysisProvenance>,
    pub auto_healed: Vec<AppliedFixRecord>,
    pub blocking_comments: Vec<BlockingComment>,
}

/// Application facade implementing `riffty_review_changes` (`docs/ach.md` §6):
/// 1. Extracts touched `TextRange`s from `GitDiffPort`.
/// 2. Expands blast radius via `TypeOraclePort::reverse_dependents`.
/// 3. Suppresses untouched baseline diagnostics.
/// 4. Applies `Applicability::Safe` fixes on disk when `auto_heal == true`.
/// 5. Emits compact deterministic `APPROVED` / `REQUEST_CHANGES` verdict.
pub struct ReviewFacade<'a> {
    pub engine: &'a RifftyEngine<'a>,
    pub vfs: &'a dyn WorkspaceVfsPort,
    pub patch_applier: &'a dyn PatchApplierPort,
    pub git_diff: &'a dyn GitDiffPort,
}

impl<'a> ReviewFacade<'a> {
    pub fn review_changes(
        &self,
        workspace_root: &Path,
        request: &ReviewChangesRequest,
    ) -> Result<ReviewVerdict, String> {
        let diff_target = request.to_diff_target()?;
        let workspace_files = self.vfs.discover_python_files(workspace_root)?;
        let diff_entries = self.git_diff.touched_ranges(workspace_root, &diff_target)?;

        if diff_entries.is_empty() {
            return Ok(ReviewVerdict {
                verdict: "APPROVED".to_string(),
                summary: ReviewSummary {
                    files_reviewed: 0,
                    auto_healed_count: 0,
                    blocking_issues: 0,
                },
                provenance: Vec::new(),
                auto_healed: Vec::new(),
                blocking_comments: Vec::new(),
            });
        }

        // Index full workspace so cross-file symbol resolution and `reverse_dependents` are populated.
        self.engine.oracle.index_workspace(&workspace_files);

        let path_to_id: HashMap<String, FileId> = workspace_files
            .iter()
            .map(|wf| (normalize_rel(&wf.relative_path), wf.file_id))
            .collect();

        let mut scoped_ranges: HashMap<FileId, Vec<TextRange>> = HashMap::new();
        for entry in &diff_entries {
            let key = normalize_rel(&entry.relative_path.to_string_lossy());
            if let Some(&fid) = path_to_id.get(&key) {
                scoped_ranges
                    .entry(fid)
                    .or_default()
                    .extend(entry.touched_ranges.iter().copied());
            }
        }

        // Expand blast radius via `TypeOraclePort::reverse_dependents` (`R15`)
        let touched_snapshot: Vec<(FileId, Vec<TextRange>)> = scoped_ranges
            .iter()
            .map(|(&fid, spans)| (fid, spans.clone()))
            .collect();
        for (fid, spans) in &touched_snapshot {
            for (dep_file_id, dep_range) in self.engine.oracle.reverse_dependents(*fid, spans) {
                scoped_ranges
                    .entry(dep_file_id)
                    .or_default()
                    .push(dep_range);
            }
        }

        let file_results = self.engine.run_workspace(
            &workspace_files,
            PipelineConfig {
                apply_fixes: request.auto_heal,
                allow_unsafe_fixes: false,
                max_passes: DEFAULT_MAX_FIXPOINT_PASSES,
            },
            Some(&scoped_ranges),
        )?;

        let id_to_path: HashMap<FileId, &Path> = workspace_files
            .iter()
            .map(|wf| (wf.file_id, wf.path.as_path()))
            .collect();

        let mut auto_healed = Vec::new();
        let mut blocking_comments = Vec::new();
        let mut comment_seq = 1usize;

        for res in &file_results {
            if request.auto_heal
                && res.modified
                && let Some(&abs_path) = id_to_path.get(&res.file_id)
            {
                self.patch_applier.write_patched_file(
                    workspace_root,
                    abs_path,
                    &res.final_source,
                )?;
            }
            auto_healed.extend(res.applied_fixes.iter().cloned());

            let line_index = LineIndex::new(&res.final_source);
            for diag in &res.remaining_diagnostics {
                if matches!(diag.severity, Severity::Error | Severity::Warning) {
                    let suggested_action = diag
                        .fix
                        .as_ref()
                        .map(|f| f.title.clone())
                        .unwrap_or_else(|| "Inspect and remediate diagnostic.".to_string());
                    blocking_comments.push(BlockingComment {
                        id: format!("rev-{comment_seq}"),
                        file: res.relative_path.clone(),
                        line: line_index.line_of(ByteOffset(diag.range.start.0)),
                        rule: diag.rule_id.clone(),
                        severity: diag.severity.as_upper_str().to_string(),
                        message: diag.message.clone(),
                        type_proof: diag.type_proof.clone(),
                        suggested_action,
                    });
                    comment_seq += 1;
                }
            }
        }

        let verdict = if blocking_comments.is_empty() {
            "APPROVED".to_string()
        } else {
            "REQUEST_CHANGES".to_string()
        };

        Ok(ReviewVerdict {
            verdict,
            summary: ReviewSummary {
                files_reviewed: file_results.len(),
                auto_healed_count: auto_healed.len(),
                blocking_issues: blocking_comments.len(),
            },
            provenance: Vec::new(),
            auto_healed,
            blocking_comments,
        })
    }
}

/// Multi-language review facade (Python + Java) for `riffty mcp` (`riffty_review_changes`).
/// Hard-locks `allow_unsafe_fixes = false` and runs through `AnalyzerRegistry` + `WorkspaceOrchestrator`.
pub struct MultiLanguageReviewFacade<'a> {
    pub registry: &'a AnalyzerRegistry<'a>,
    pub vfs: &'a dyn WorkspaceVfsPort,
    pub patch_applier: &'a dyn PatchApplierPort,
    pub git_diff: &'a dyn GitDiffPort,
}

impl<'a> MultiLanguageReviewFacade<'a> {
    pub fn review_changes(
        &self,
        workspace_root: &Path,
        rules_paths: &[std::path::PathBuf],
        request: &ReviewChangesRequest,
    ) -> Result<ReviewVerdict, String> {
        let diff_target = request.to_diff_target()?;
        let extensions = self.registry.supported_extensions();
        let workspace_files = self.vfs.discover_files(workspace_root, &extensions)?;
        let diff_entries = self.git_diff.touched_ranges(workspace_root, &diff_target)?;

        if diff_entries.is_empty() {
            return Ok(ReviewVerdict {
                verdict: "APPROVED".to_string(),
                summary: ReviewSummary {
                    files_reviewed: 0,
                    auto_healed_count: 0,
                    blocking_issues: 0,
                },
                provenance: Vec::new(),
                auto_healed: Vec::new(),
                blocking_comments: Vec::new(),
            });
        }

        let path_to_id: HashMap<String, FileId> = workspace_files
            .iter()
            .map(|wf| (normalize_rel(&wf.relative_path), wf.file_id))
            .collect();

        let mut scoped_ranges: HashMap<FileId, Vec<TextRange>> = HashMap::new();
        for entry in &diff_entries {
            let key = normalize_rel(&entry.relative_path.to_string_lossy());
            if let Some(&fid) = path_to_id.get(&key) {
                scoped_ranges
                    .entry(fid)
                    .or_default()
                    .extend(entry.touched_ranges.iter().copied());
            }
        }

        if scoped_ranges.is_empty() {
            return Ok(ReviewVerdict {
                verdict: "APPROVED".to_string(),
                summary: ReviewSummary {
                    files_reviewed: 0,
                    auto_healed_count: 0,
                    blocking_issues: 0,
                },
                provenance: Vec::new(),
                auto_healed: Vec::new(),
                blocking_comments: Vec::new(),
            });
        }

        let snapshot = scoped_ranges.clone();
        for analyzer in self.registry.analyzers() {
            for (dep_fid, dep_range) in
                analyzer.reverse_dependents(workspace_root, &workspace_files, &snapshot)
            {
                scoped_ranges.entry(dep_fid).or_default().push(dep_range);
            }
        }

        let orchestrator = WorkspaceOrchestrator {
            registry: self.registry,
        };
        let run_output = orchestrator.run(
            workspace_root,
            &workspace_files,
            rules_paths,
            PipelineConfig {
                apply_fixes: request.auto_heal,
                allow_unsafe_fixes: false,
                max_passes: DEFAULT_MAX_FIXPOINT_PASSES,
            },
            Some(&scoped_ranges),
        )?;

        let id_to_path: HashMap<FileId, &Path> = workspace_files
            .iter()
            .map(|wf| (wf.file_id, wf.path.as_path()))
            .collect();

        let mut auto_healed = Vec::new();
        let mut blocking_comments = Vec::new();
        let mut comment_seq = 1usize;

        for res in &run_output.files {
            if request.auto_heal
                && res.modified
                && let Some(&abs_path) = id_to_path.get(&res.file_id)
            {
                self.patch_applier.write_patched_file(
                    workspace_root,
                    abs_path,
                    &res.final_source,
                )?;
            }
            auto_healed.extend(res.applied_fixes.iter().cloned());

            let line_index = LineIndex::new(&res.final_source);
            for diag in &res.remaining_diagnostics {
                if matches!(diag.severity, Severity::Error | Severity::Warning) {
                    let suggested_action = diag
                        .fix
                        .as_ref()
                        .map(|f| f.title.clone())
                        .unwrap_or_else(|| "Inspect and remediate diagnostic.".to_string());
                    blocking_comments.push(BlockingComment {
                        id: format!("rev-{comment_seq}"),
                        file: res.relative_path.clone(),
                        line: line_index.line_of(ByteOffset(diag.range.start.0)),
                        rule: diag.rule_id.clone(),
                        severity: diag.severity.as_upper_str().to_string(),
                        message: diag.message.clone(),
                        type_proof: diag.type_proof.clone(),
                        suggested_action,
                    });
                    comment_seq += 1;
                }
            }
        }

        let verdict = if blocking_comments.is_empty() {
            "APPROVED".to_string()
        } else {
            "REQUEST_CHANGES".to_string()
        };

        Ok(ReviewVerdict {
            verdict,
            summary: ReviewSummary {
                files_reviewed: run_output.files.len(),
                auto_healed_count: auto_healed.len(),
                blocking_issues: blocking_comments.len(),
            },
            provenance: run_output.provenance,
            auto_healed,
            blocking_comments,
        })
    }
}

fn normalize_rel(p: &str) -> String {
    p.replace('\\', "/").trim_start_matches("./").to_string()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use riffty_core::{
        CanonicalNode, CanonicalType, FileDiffRanges, Fix, NodeId, NodeMeta, Purity, StoredNode,
        TriState, eval_is_subtype, extract_inline_suppressions,
    };

    use super::*;

    /// Minimal deterministic line/token parser for `riffty_pipeline` unit tests
    /// (zero upstream adapter dependency required in Milestone 2).
    struct LineTokenParser;

    impl SyntaxParserPort for LineTokenParser {
        fn parse(&self, file_id: FileId, source: &str) -> Result<CanonicalSyntaxTree, String> {
            let mut nodes = vec![StoredNode {
                id: NodeId(0),
                kind: NodeKind::Module,
                range: TextRange::new(0, source.len() as u32),
                parent: None,
                children: Vec::new(),
                meta: NodeMeta::None,
            }];

            let mut byte_cursor = 0u32;
            for line in source.split_inclusive('\n') {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if !trimmed.is_empty() && !trimmed.trim_start().starts_with('#') {
                    let id = NodeId(nodes.len() as u32);
                    let kind = if trimmed.contains('(') {
                        NodeKind::Call
                    } else {
                        NodeKind::Name
                    };
                    nodes[0].children.push(id);
                    nodes.push(StoredNode {
                        id,
                        kind,
                        range: TextRange::new(byte_cursor, byte_cursor + trimmed.len() as u32),
                        parent: Some(NodeId(0)),
                        children: Vec::new(),
                        meta: NodeMeta::None,
                    });
                }
                byte_cursor += line.len() as u32;
            }
            Ok(CanonicalSyntaxTree::new(
                file_id,
                source.to_string(),
                nodes,
                extract_inline_suppressions(source),
            ))
        }
    }

    struct StubOracle {
        rev_deps: Vec<(FileId, TextRange)>,
    }

    impl TypeOraclePort for StubOracle {
        fn type_of(&self, _file_id: FileId, _range: TextRange) -> CanonicalType {
            CanonicalType::Unknown
        }

        fn resolve_fqname(&self, _file_id: FileId, _range: TextRange) -> Option<String> {
            None
        }

        fn is_subtype(&self, sub: &CanonicalType, sup: &CanonicalType) -> TriState {
            eval_is_subtype(sub, sup)
        }

        fn can_be_none(&self, _file_id: FileId, _range: TextRange) -> TriState {
            TriState::Unknown
        }

        fn expression_purity(&self, _file_id: FileId, _range: TextRange) -> Purity {
            Purity::Pure
        }

        fn reverse_dependents(
            &self,
            _file_id: FileId,
            _changed_ranges: &[TextRange],
        ) -> Vec<(FileId, TextRange)> {
            self.rev_deps.clone()
        }
    }

    struct SimpleImportManager;

    impl ImportManagerPort for SimpleImportManager {
        fn resolve_import_edits(&self, source: &str, imports: &[ImportSpec]) -> Vec<Edit> {
            let mut insertion = String::new();
            for imp in imports {
                let stmt = match &imp.symbol {
                    None => format!("import {}\n", imp.module),
                    Some(sym) => format!("from {} import {sym}\n", imp.module),
                };
                if !source.contains(stmt.trim()) {
                    insertion.push_str(&stmt);
                }
            }
            if insertion.is_empty() {
                Vec::new()
            } else {
                vec![Edit {
                    range: TextRange::new(0, 0),
                    replacement: insertion,
                }]
            }
        }
    }

    struct CountingCallRule {
        eval_calls: Arc<AtomicUsize>,
    }

    impl RulePlugin for CountingCallRule {
        fn id(&self) -> &'static str {
            "TEST-CALL"
        }

        fn target_node_kinds(&self) -> &[NodeKind] {
            &[NodeKind::Call]
        }

        fn evaluate(&self, node: &CanonicalNode, view: &AnalysisView) -> Vec<Diagnostic> {
            self.eval_calls.fetch_add(1, Ordering::SeqCst);
            if node.raw_text.contains("legacy_fn()") {
                vec![Diagnostic {
                    rule_id: "TEST-CALL".to_string(),
                    rule_name: "legacy-call".to_string(),
                    severity: Severity::Warning,
                    file_id: view.file_id,
                    range: node.range,
                    message: "Replace legacy_fn() with modern_fn()".to_string(),
                    type_proof: None,
                    fix: Some(Fix {
                        title: "Rewrite legacy_fn() -> modern_fn()".to_string(),
                        edits: vec![Edit {
                            range: node.range,
                            replacement: node.raw_text.replace("legacy_fn()", "modern_fn()"),
                        }],
                        required_imports: vec![ImportSpec {
                            module: "modern_pkg".to_string(),
                            symbol: Some("modern_fn".to_string()),
                        }],
                        applicability: Applicability::Safe,
                    }),
                }]
            } else {
                Vec::new()
            }
        }
    }

    #[test]
    fn single_pass_dispatches_only_to_subscribed_node_kinds() {
        let parser = LineTokenParser;
        let oracle = StubOracle { rev_deps: vec![] };
        let src = "x = 1\ny = 2\nlegacy_fn()\nz = 3\n";
        let tree = parser.parse(FileId(1), src).unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let rule = CountingCallRule {
            eval_calls: Arc::clone(&counter),
        };
        let rules: Vec<Box<dyn RulePlugin>> = vec![Box::new(rule)];

        let view = AnalysisView {
            file_id: FileId(1),
            file_path: "sample.py",
            tree: &tree,
            oracle: &oracle,
            pattern_matcher: None,
        };
        let diags = dispatch_single_pass(&view, &rules);
        assert_eq!(diags.len(), 1);
        // Out of 1 Module + 3 Name + 1 Call = 5 nodes, CountingCallRule ran ONLY once!
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    struct OverlappingRuleError;
    impl RulePlugin for OverlappingRuleError {
        fn id(&self) -> &'static str {
            "RFT-ERR"
        }
        fn target_node_kinds(&self) -> &[NodeKind] {
            &[NodeKind::Call]
        }
        fn evaluate(&self, node: &CanonicalNode, view: &AnalysisView) -> Vec<Diagnostic> {
            if let Some(rel) = node.raw_text.find("bad_api") {
                let start = node.range.start.0 + rel as u32;
                let end = start + "bad_api".len() as u32;
                vec![Diagnostic {
                    rule_id: "RFT-ERR".to_string(),
                    rule_name: "bad-api".to_string(),
                    severity: Severity::Error,
                    file_id: view.file_id,
                    range: TextRange::new(start, end),
                    message: "Rename bad_api".to_string(),
                    type_proof: None,
                    fix: Some(Fix {
                        title: "Rename bad_api -> good_api".to_string(),
                        edits: vec![Edit {
                            range: TextRange::new(start, end),
                            replacement: "good_api".to_string(),
                        }],
                        required_imports: Vec::new(),
                        applicability: Applicability::Safe,
                    }),
                }]
            } else {
                Vec::new()
            }
        }
    }

    struct OverlappingRuleWarn;
    impl RulePlugin for OverlappingRuleWarn {
        fn id(&self) -> &'static str {
            "RFT-WARN"
        }
        fn target_node_kinds(&self) -> &[NodeKind] {
            &[NodeKind::Call]
        }
        fn evaluate(&self, node: &CanonicalNode, view: &AnalysisView) -> Vec<Diagnostic> {
            if node.raw_text.ends_with("()") && !node.raw_text.contains("timeout=5") {
                vec![Diagnostic {
                    rule_id: "RFT-WARN".to_string(),
                    rule_name: "missing-timeout".to_string(),
                    severity: Severity::Warning,
                    file_id: view.file_id,
                    range: node.range,
                    message: "Inject timeout=5".to_string(),
                    type_proof: None,
                    fix: Some(Fix {
                        title: "Inject timeout=5".to_string(),
                        edits: vec![Edit {
                            range: node.range,
                            replacement: format!(
                                "{}(timeout=5)",
                                &node.raw_text[..node.raw_text.len() - 2]
                            ),
                        }],
                        required_imports: Vec::new(),
                        applicability: Applicability::Safe,
                    }),
                }]
            } else {
                Vec::new()
            }
        }
    }

    #[test]
    fn overlapping_edits_resolve_higher_severity_first_and_converge_in_fixpoint_loop() {
        let parser = LineTokenParser;
        let oracle = StubOracle { rev_deps: vec![] };
        let imports = SimpleImportManager;
        let engine = RifftyEngine {
            parser: &parser,
            oracle: &oracle,
            import_manager: &imports,
            pattern_matcher: None,
            rules: vec![
                Box::new(OverlappingRuleWarn),
                Box::new(OverlappingRuleError),
            ],
        };

        let files = vec![WorkspaceFile {
            file_id: FileId(1),
            path: PathBuf::from("client.py"),
            relative_path: "client.py".to_string(),
            source: "# preserve top comment\nbad_api()\n# preserve bottom comment\n".to_string(),
        }];

        let results = engine
            .run_workspace(
                &files,
                PipelineConfig {
                    apply_fixes: true,
                    allow_unsafe_fixes: false,
                    max_passes: 10,
                },
                None,
            )
            .unwrap();

        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert!(r.modified);
        // Pass 1 applied RFT-ERR (`good_api()`), deferring overlapping RFT-WARN;
        // Pass 2 applied RFT-WARN (`good_api(timeout=5)`); Pass 3 confirmed convergence!
        assert_eq!(
            r.final_source,
            "# preserve top comment\ngood_api(timeout=5)\n# preserve bottom comment\n"
        );
        assert_eq!(r.applied_fixes.len(), 2);
        assert_eq!(r.applied_fixes[0].rule, "RFT-ERR");
        assert_eq!(r.applied_fixes[1].rule, "RFT-WARN");
        assert!(r.remaining_diagnostics.is_empty());
    }

    struct OscillatingCycleRule;
    impl RulePlugin for OscillatingCycleRule {
        fn id(&self) -> &'static str {
            "RFT-OSC"
        }
        fn target_node_kinds(&self) -> &[NodeKind] {
            &[NodeKind::Call]
        }
        fn evaluate(&self, node: &CanonicalNode, view: &AnalysisView) -> Vec<Diagnostic> {
            let next = if node.raw_text == "alpha()" {
                Some("beta()")
            } else if node.raw_text == "beta()" {
                Some("alpha()")
            } else {
                None
            };
            next.into_iter()
                .map(|repl| Diagnostic {
                    rule_id: "RFT-OSC".to_string(),
                    rule_name: "oscillate".to_string(),
                    severity: Severity::Warning,
                    file_id: view.file_id,
                    range: node.range,
                    message: "Oscillating test rule".to_string(),
                    type_proof: None,
                    fix: Some(Fix {
                        title: format!("Rewrite to {repl}"),
                        edits: vec![Edit {
                            range: node.range,
                            replacement: repl.to_string(),
                        }],
                        required_imports: Vec::new(),
                        applicability: Applicability::Safe,
                    }),
                })
                .collect()
        }
    }

    #[test]
    fn fixpoint_loop_terminates_on_oscillating_cycle_within_bound() {
        let parser = LineTokenParser;
        let oracle = StubOracle { rev_deps: vec![] };
        let imports = SimpleImportManager;
        let engine = RifftyEngine {
            parser: &parser,
            oracle: &oracle,
            import_manager: &imports,
            pattern_matcher: None,
            rules: vec![Box::new(OscillatingCycleRule)],
        };
        let files = vec![WorkspaceFile {
            file_id: FileId(1),
            path: PathBuf::from("osc.py"),
            relative_path: "osc.py".to_string(),
            source: "alpha()\n".to_string(),
        }];
        let res = engine
            .run_workspace(
                &files,
                PipelineConfig {
                    apply_fixes: true,
                    allow_unsafe_fixes: false,
                    max_passes: 10,
                },
                None,
            )
            .unwrap();
        assert!(res[0].passes_executed <= 10);
        assert_eq!(res[0].final_source, "beta()\n");
    }

    #[test]
    fn review_facade_suppresses_baseline_expands_reverse_dependents_and_auto_heals() {
        struct MemVfs(Vec<WorkspaceFile>);
        impl WorkspaceVfsPort for MemVfs {
            fn discover_files(
                &self,
                _root: &Path,
                _extensions: &[&str],
            ) -> Result<Vec<WorkspaceFile>, String> {
                Ok(self.0.clone())
            }
            fn read_file(&self, _path: &Path) -> Result<String, String> {
                Ok(String::new())
            }
        }

        struct MemPatcher(Mutex<HashMap<PathBuf, String>>);
        impl PatchApplierPort for MemPatcher {
            fn write_patched_file(
                &self,
                _workspace_root: &Path,
                target_path: &Path,
                updated_source: &str,
            ) -> Result<(), String> {
                self.0
                    .lock()
                    .unwrap()
                    .insert(target_path.to_path_buf(), updated_source.to_string());
                Ok(())
            }
        }

        struct MemDiff(Vec<FileDiffRanges>);
        impl GitDiffPort for MemDiff {
            fn touched_ranges(
                &self,
                _repo_root: &Path,
                _target: &DiffTarget,
            ) -> Result<Vec<FileDiffRanges>, String> {
                Ok(self.0.clone())
            }
        }

        struct BlockingCallerRule;
        impl RulePlugin for BlockingCallerRule {
            fn id(&self) -> &'static str {
                "RFT-CALLER"
            }
            fn target_node_kinds(&self) -> &[NodeKind] {
                &[NodeKind::Call]
            }
            fn evaluate(&self, node: &CanonicalNode, view: &AnalysisView) -> Vec<Diagnostic> {
                if node.raw_text.contains("broken_caller()") {
                    vec![Diagnostic {
                        rule_id: "RFT-CALLER".to_string(),
                        rule_name: "broken-caller".to_string(),
                        severity: Severity::Error,
                        file_id: view.file_id,
                        range: node.range,
                        message: "Caller invalidated by changed callee signature".to_string(),
                        type_proof: Some(serde_json::json!({ "callee": "compute" })),
                        fix: None,
                    }]
                } else {
                    Vec::new()
                }
            }
        }

        // File 1 (`service.py`): line 1 has untouched `legacy_fn()`, line 2 has touched `legacy_fn()`.
        let f1_src = "legacy_fn()\nlegacy_fn()\n";
        let touched_line2_start = "legacy_fn()\n".len() as u32;
        let touched_line2_end = f1_src.len() as u32;

        // File 2 (`caller.py`): untouched in git diff, but returned by `reverse_dependents`!
        let f2_src = "broken_caller()\n";

        let files = vec![
            WorkspaceFile {
                file_id: FileId(1),
                path: PathBuf::from("/repo/service.py"),
                relative_path: "service.py".to_string(),
                source: f1_src.to_string(),
            },
            WorkspaceFile {
                file_id: FileId(2),
                path: PathBuf::from("/repo/caller.py"),
                relative_path: "caller.py".to_string(),
                source: f2_src.to_string(),
            },
        ];

        let parser = LineTokenParser;
        let oracle = StubOracle {
            rev_deps: vec![(FileId(2), TextRange::new(0, f2_src.len() as u32))],
        };
        let imports = SimpleImportManager;
        let engine = RifftyEngine {
            parser: &parser,
            oracle: &oracle,
            import_manager: &imports,
            pattern_matcher: None,
            rules: vec![
                Box::new(CountingCallRule {
                    eval_calls: Arc::new(AtomicUsize::new(0)),
                }),
                Box::new(BlockingCallerRule),
            ],
        };

        let vfs = MemVfs(files);
        let patcher = MemPatcher(Mutex::new(HashMap::new()));
        let diff = MemDiff(vec![FileDiffRanges {
            relative_path: PathBuf::from("service.py"),
            touched_ranges: vec![TextRange::new(touched_line2_start, touched_line2_end)],
        }]);

        let facade = ReviewFacade {
            engine: &engine,
            vfs: &vfs,
            patch_applier: &patcher,
            git_diff: &diff,
        };

        let verdict = facade
            .review_changes(
                Path::new("/repo"),
                &ReviewChangesRequest {
                    target: "unstaged".to_string(),
                    base_ref: "main".to_string(),
                    auto_heal: true,
                },
            )
            .unwrap();

        assert_eq!(verdict.verdict, "REQUEST_CHANGES");
        assert_eq!(verdict.summary.files_reviewed, 2);
        // Only line 2 of service.py was auto-healed; line 1 untouched baseline was preserved!
        assert_eq!(verdict.summary.auto_healed_count, 1);
        assert_eq!(verdict.auto_healed[0].line, 2);
        let disk_patched = patcher
            .0
            .lock()
            .unwrap()
            .get(Path::new("/repo/service.py"))
            .cloned()
            .unwrap();
        assert_eq!(
            disk_patched,
            "from modern_pkg import modern_fn\nlegacy_fn()\nmodern_fn()\n"
        );

        // Reverse-dependent caller.py surfaced as a blocking comment!
        assert_eq!(verdict.summary.blocking_issues, 1);
        assert_eq!(verdict.blocking_comments[0].file, "caller.py");
        assert_eq!(verdict.blocking_comments[0].rule, "RFT-CALLER");
    }
}
