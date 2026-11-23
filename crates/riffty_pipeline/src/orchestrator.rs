use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use riffty_core::{
    AnalysisProvenance, AnalysisTier, AnalysisView, AnalyzePassRequest, AnalyzePassResponse,
    Applicability, Diagnostic, Edit, FileId, ImportManagerPort, ImportSpec, Language,
    LanguageAnalyzerPort, LineIndex, ScopedImportRequest, TextRange, WorkspaceFile,
};

use crate::{
    AppliedFixRecord, DEFAULT_MAX_FIXPOINT_PASSES, FileRunResult, PipelineConfig, RifftyEngine,
    apply_edits_bottom_to_top, dispatch_single_pass, resolve_non_overlapping_fixes_with_imports,
    shift_ranges_after_edits, validate_edit_bounds,
};

/// Adapts the existing in-process Python `RifftyEngine` to the coarse-grained
/// `LanguageAnalyzerPort` seam.
pub struct PythonLanguageAnalyzer<'a> {
    pub engine: RifftyEngine<'a>,
    pub tier: AnalysisTier,
    pub runtime_detail: Option<String>,
}

impl<'a> LanguageAnalyzerPort for PythonLanguageAnalyzer<'a> {
    fn language(&self) -> Language {
        Language::Python
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["py"]
    }

    fn max_fixpoint_passes(&self) -> usize {
        DEFAULT_MAX_FIXPOINT_PASSES
    }

    fn analyze_pass(&self, req: &AnalyzePassRequest<'_>) -> Result<AnalyzePassResponse, String> {
        self.engine.oracle.index_workspace(req.files);

        let mut diagnostics = Vec::new();
        let mut files_analyzed = 0usize;

        for wf in req.files {
            if let Some(scopes) = req.scoped_ranges
                && !scopes.contains_key(&wf.file_id)
            {
                continue;
            }
            files_analyzed += 1;
            let tree = self.engine.parser.parse(wf.file_id, &wf.source)?;
            let view = AnalysisView {
                file_id: wf.file_id,
                file_path: &wf.relative_path,
                tree: &tree,
                oracle: self.engine.oracle,
                pattern_matcher: self.engine.pattern_matcher,
            };
            let mut diags = dispatch_single_pass(&view, &self.engine.rules);
            if let Some(scopes) = req.scoped_ranges {
                let allowed = scopes.get(&wf.file_id).map(Vec::as_slice).unwrap_or(&[]);
                diags.retain(|d| allowed.iter().any(|sp| sp.intersects(d.range)));
            }
            diagnostics.extend(diags);
        }

        let (degraded_reason, remediation_hint) = match self.tier {
            AnalysisTier::Tier2Semantic => (None, None),
            AnalysisTier::Tier1Syntactic => (
                Some("Python semantic type oracle disabled (--no-type-oracle)".to_string()),
                Some("Remove `--no-type-oracle` to enable Tier 2 Ty semantic analysis".to_string()),
            ),
        };

        Ok(AnalyzePassResponse {
            diagnostics,
            provenance: AnalysisProvenance {
                language: Language::Python,
                tier: self.tier,
                files_analyzed,
                runtime_detail: self.runtime_detail.clone(),
                degraded_reason,
                remediation_hint,
            },
        })
    }

    fn import_manager(&self) -> Option<&dyn ImportManagerPort> {
        Some(self.engine.import_manager)
    }

    fn resolve_file_import_edits(
        &self,
        file: &WorkspaceFile,
        requests: &[ScopedImportRequest],
    ) -> Vec<Edit> {
        match self.engine.parser.parse(file.file_id, &file.source) {
            Ok(tree) => self
                .engine
                .import_manager
                .resolve_scoped_import_edits(&tree, requests),
            Err(_) => {
                let specs: Vec<ImportSpec> = requests.iter().map(|r| r.spec.clone()).collect();
                self.engine
                    .import_manager
                    .resolve_import_edits(&file.source, &specs)
            }
        }
    }

    fn reverse_dependents(
        &self,
        _root: &Path,
        files: &[WorkspaceFile],
        touched: &HashMap<FileId, Vec<TextRange>>,
    ) -> Vec<(FileId, TextRange)> {
        self.engine.oracle.index_workspace(files);
        let mut expanded = Vec::new();
        for (&fid, spans) in touched {
            expanded.extend(self.engine.oracle.reverse_dependents(fid, spans));
        }
        expanded
    }
}

/// Registry of language analyzers shared across CLI, CI, IDE, and MCP surfaces.
#[derive(Default)]
pub struct AnalyzerRegistry<'a> {
    analyzers: Vec<Box<dyn LanguageAnalyzerPort + 'a>>,
}

impl<'a> AnalyzerRegistry<'a> {
    pub fn new() -> Self {
        Self {
            analyzers: Vec::new(),
        }
    }

    pub fn register<A: LanguageAnalyzerPort + 'a>(&mut self, analyzer: A) {
        self.analyzers.push(Box::new(analyzer));
    }

    pub fn analyzers(&self) -> &[Box<dyn LanguageAnalyzerPort + 'a>] {
        &self.analyzers
    }

    pub fn supported_extensions(&self) -> Vec<&'static str> {
        let mut exts = Vec::new();
        for analyzer in &self.analyzers {
            for &ext in analyzer.extensions() {
                if !exts.contains(&ext) {
                    exts.push(ext);
                }
            }
        }
        exts
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRunOutput {
    pub files: Vec<FileRunResult>,
    pub provenance: Vec<AnalysisProvenance>,
}

/// Multi-language orchestrator owning file partitioning, span sanitization, fixpoint
/// convergence, conflict resolution, and cycle detection.
pub struct WorkspaceOrchestrator<'a> {
    pub registry: &'a AnalyzerRegistry<'a>,
}

impl<'a> WorkspaceOrchestrator<'a> {
    pub fn run(
        &self,
        workspace_root: &Path,
        files: &[WorkspaceFile],
        rules_paths: &[PathBuf],
        config: PipelineConfig,
        scoped_ranges: Option<&HashMap<FileId, Vec<TextRange>>>,
    ) -> Result<WorkspaceRunOutput, String> {
        let mut all_results: Vec<FileRunResult> = Vec::new();
        let mut all_provenance: Vec<AnalysisProvenance> = Vec::new();

        for analyzer in self.registry.analyzers() {
            let exts = analyzer.extensions();
            let lang_files: Vec<WorkspaceFile> = files
                .iter()
                .filter(|wf| file_matches_extensions(&wf.path, exts))
                .cloned()
                .collect();

            if lang_files.is_empty() {
                continue;
            }

            let (mut lang_results, prov) = run_analyzer_fixpoint(
                analyzer.as_ref(),
                workspace_root,
                lang_files,
                rules_paths,
                config,
                scoped_ranges,
            )?;
            all_results.append(&mut lang_results);
            all_provenance.push(prov);
        }

        all_results.sort_by_key(|r| r.file_id);
        Ok(WorkspaceRunOutput {
            files: all_results,
            provenance: all_provenance,
        })
    }
}

fn file_matches_extensions(path: &Path, extensions: &[&str]) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    extensions.iter().any(|e| ext.eq_ignore_ascii_case(e))
}

/// Validates external/plugin diagnostics against `source` UTF-8 boundaries.
/// Any diagnostic whose `Fix` contains an out-of-bounds or non-char-boundary `Edit`
/// is safely downgraded to `Applicability::ManualOnly` (with its edits cleared) rather
/// than crashing the pass or corrupting the file.
pub fn sanitize_diagnostic_for_source(diag: &mut Diagnostic, source: &str) {
    let max_len = source.len() as u32;
    let clamped_start = clamp_to_char_boundary(source, diag.range.start.0.min(max_len));
    let clamped_end =
        clamp_to_char_boundary(source, diag.range.end.0.clamp(clamped_start, max_len));
    diag.range = TextRange::new(clamped_start, clamped_end);

    if let Some(ref mut fix) = diag.fix {
        let all_valid = fix.edits.iter().all(|e| validate_edit_bounds(source, e));
        if !all_valid {
            fix.edits.clear();
            fix.applicability = Applicability::ManualOnly;
        }
    }
}

fn clamp_to_char_boundary(source: &str, byte_pos: u32) -> u32 {
    let mut idx = (byte_pos as usize).min(source.len());
    while idx > 0 && !source.is_char_boundary(idx) {
        idx -= 1;
    }
    idx as u32
}

fn run_analyzer_fixpoint(
    analyzer: &dyn LanguageAnalyzerPort,
    workspace_root: &Path,
    mut working_files: Vec<WorkspaceFile>,
    rules_paths: &[PathBuf],
    config: PipelineConfig,
    scoped_ranges: Option<&HashMap<FileId, Vec<TextRange>>>,
) -> Result<(Vec<FileRunResult>, AnalysisProvenance), String> {
    let max_passes = config
        .max_passes
        .clamp(1, DEFAULT_MAX_FIXPOINT_PASSES)
        .min(analyzer.max_fixpoint_passes().max(1));
    let max_app = if config.allow_unsafe_fixes {
        Applicability::Unsafe
    } else {
        Applicability::Safe
    };

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
    let mut latest_provenance = AnalysisProvenance {
        language: analyzer.language(),
        tier: AnalysisTier::Tier1Syntactic,
        files_analyzed: 0,
        runtime_detail: None,
        degraded_reason: None,
        remediation_hint: None,
    };

    for pass_index in 0..max_passes {
        passes_executed += 1;
        let req = AnalyzePassRequest {
            workspace_root,
            files: &working_files,
            scoped_ranges: active_scopes.as_ref(),
            rules_paths,
            allow_unsafe: config.allow_unsafe_fixes,
            pass_index,
        };
        let resp = analyzer.analyze_pass(&req)?;
        latest_provenance = resp.provenance;

        let mut diags_by_file: HashMap<FileId, Vec<Diagnostic>> = HashMap::new();
        for diag in resp.diagnostics {
            diags_by_file.entry(diag.file_id).or_default().push(diag);
        }

        latest_diagnostics.clear();
        let mut any_file_patched = false;

        for wf in &mut working_files {
            if let Some(scopes) = &active_scopes
                && !scopes.contains_key(&wf.file_id)
            {
                continue;
            }
            let mut diags = diags_by_file.remove(&wf.file_id).unwrap_or_default();
            for d in &mut diags {
                sanitize_diagnostic_for_source(d, &wf.source);
            }
            if let Some(scopes) = &active_scopes {
                let allowed = scopes.get(&wf.file_id).map(Vec::as_slice).unwrap_or(&[]);
                diags.retain(|d| allowed.iter().any(|sp| sp.intersects(d.range)));
            }

            if !config.apply_fixes {
                latest_diagnostics.insert(wf.file_id, diags);
                continue;
            }

            let resolution = resolve_non_overlapping_fixes_with_imports(
                &wf.source,
                &diags,
                max_app,
                |scoped_reqs, _| analyzer.resolve_file_import_edits(wf, scoped_reqs),
            );
            if resolution.concrete_edits.is_empty() {
                latest_diagnostics.insert(wf.file_id, diags);
                continue;
            }

            let patched = apply_edits_bottom_to_top(&wf.source, &resolution.concrete_edits)?;
            let state_set = seen_states.entry(wf.file_id).or_default();
            if patched == wf.source || !state_set.insert(patched.clone()) {
                latest_diagnostics.insert(wf.file_id, diags);
                continue;
            }

            let line_index = LineIndex::new(&wf.source);
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
                        line: line_index.line_of(accepted.range.start),
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

        if passes_executed == max_passes {
            let final_req = AnalyzePassRequest {
                workspace_root,
                files: &working_files,
                scoped_ranges: active_scopes.as_ref(),
                rules_paths,
                allow_unsafe: config.allow_unsafe_fixes,
                pass_index: passes_executed,
            };
            let final_resp = analyzer.analyze_pass(&final_req)?;
            latest_provenance = final_resp.provenance;
            let mut by_file: HashMap<FileId, Vec<Diagnostic>> = HashMap::new();
            for mut d in final_resp.diagnostics {
                if let Some(wf) = working_files.iter().find(|f| f.file_id == d.file_id) {
                    sanitize_diagnostic_for_source(&mut d, &wf.source);
                }
                by_file.entry(d.file_id).or_default().push(d);
            }
            latest_diagnostics.clear();
            for wf in &working_files {
                if let Some(scopes) = &active_scopes
                    && !scopes.contains_key(&wf.file_id)
                {
                    continue;
                }
                let mut diags = by_file.remove(&wf.file_id).unwrap_or_default();
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

    Ok((results, latest_provenance))
}

/// Generates a compact unified diff between `original` and `updated` for `--fix --dry-run`.
pub fn render_unified_diff(relative_path: &str, original: &str, updated: &str) -> String {
    if original == updated {
        return String::new();
    }

    let old_lines: Vec<&str> = original.lines().collect();
    let new_lines: Vec<&str> = updated.lines().collect();

    // Trim common prefix and suffix to emit a focused single-hunk diff without extra crates.
    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < (old_lines.len() - prefix)
        && suffix < (new_lines.len() - prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let context = 3usize;
    let hunk_old_start = prefix.saturating_sub(context);
    let hunk_new_start = prefix.saturating_sub(context);
    let old_changed_end = old_lines.len() - suffix;
    let new_changed_end = new_lines.len() - suffix;
    let hunk_old_end = (old_changed_end + context).min(old_lines.len());
    let hunk_new_end = (new_changed_end + context).min(new_lines.len());

    let old_count = hunk_old_end - hunk_old_start;
    let new_count = hunk_new_end - hunk_new_start;

    let mut out = String::new();
    out.push_str(&format!("--- a/{relative_path}\n"));
    out.push_str(&format!("+++ b/{relative_path}\n"));
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        hunk_old_start + 1,
        old_count,
        hunk_new_start + 1,
        new_count
    ));

    for line in &old_lines[hunk_old_start..prefix] {
        out.push(' ');
        out.push_str(line);
        out.push('\n');
    }
    for line in &old_lines[prefix..old_changed_end] {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in &new_lines[prefix..new_changed_end] {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    for line in &old_lines[old_changed_end..hunk_old_end] {
        out.push(' ');
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use riffty_core::{Fix, Severity};

    struct MockJavaAnalyzer {
        emit_malformed_edit: bool,
    }

    impl LanguageAnalyzerPort for MockJavaAnalyzer {
        fn language(&self) -> Language {
            Language::Java
        }

        fn extensions(&self) -> &'static [&'static str] {
            &["java"]
        }

        fn max_fixpoint_passes(&self) -> usize {
            2
        }

        fn analyze_pass(
            &self,
            req: &AnalyzePassRequest<'_>,
        ) -> Result<AnalyzePassResponse, String> {
            let mut diags = Vec::new();
            for wf in req.files {
                if let Some(pos) = wf.source.find("new BigDecimal(0.1)") {
                    let range = if self.emit_malformed_edit {
                        // Intentionally invalid out-of-bounds edit range
                        TextRange::new(999, 1024)
                    } else {
                        TextRange::new(pos as u32, (pos + "new BigDecimal(0.1)".len()) as u32)
                    };
                    diags.push(Diagnostic {
                        rule_id: "EP-BigDecimalDouble".to_string(),
                        rule_name: "bigdecimal-double".to_string(),
                        severity: Severity::Error,
                        file_id: wf.file_id,
                        range: TextRange::new(
                            pos as u32,
                            (pos + "new BigDecimal(0.1)".len()) as u32,
                        ),
                        message: "Avoid new BigDecimal(double)".to_string(),
                        type_proof: None,
                        fix: Some(Fix {
                            title: "Use BigDecimal.valueOf(0.1)".to_string(),
                            edits: vec![Edit {
                                range,
                                replacement: "BigDecimal.valueOf(0.1)".to_string(),
                            }],
                            required_imports: Vec::new(),
                            applicability: Applicability::Safe,
                        }),
                    });
                }
            }
            Ok(AnalyzePassResponse {
                diagnostics: diags,
                provenance: AnalysisProvenance {
                    language: Language::Java,
                    tier: AnalysisTier::Tier1Syntactic,
                    files_analyzed: req.files.len(),
                    runtime_detail: Some("mock-java-tier1".to_string()),
                    degraded_reason: None,
                    remediation_hint: None,
                },
            })
        }
    }

    #[test]
    fn orchestrator_partitions_and_fixes_java_plugin_and_downgrades_malformed_edits() {
        let mut reg = AnalyzerRegistry::new();
        reg.register(MockJavaAnalyzer {
            emit_malformed_edit: false,
        });
        let orch = WorkspaceOrchestrator { registry: &reg };

        let files = vec![
            WorkspaceFile {
                file_id: FileId(1),
                path: PathBuf::from("Order.java"),
                relative_path: "Order.java".to_string(),
                source: "var x = new BigDecimal(0.1);\n".to_string(),
            },
            WorkspaceFile {
                file_id: FileId(2),
                path: PathBuf::from("ignored.py"),
                relative_path: "ignored.py".to_string(),
                source: "x = 1\n".to_string(),
            },
        ];

        let out = orch
            .run(
                Path::new("."),
                &files,
                &[],
                PipelineConfig {
                    apply_fixes: true,
                    allow_unsafe_fixes: false,
                    max_passes: 10,
                },
                None,
            )
            .unwrap();

        assert_eq!(out.files.len(), 1);
        assert_eq!(
            out.files[0].final_source,
            "var x = BigDecimal.valueOf(0.1);\n"
        );
        assert!(out.files[0].modified);
        assert_eq!(out.provenance.len(), 1);
        assert_eq!(out.provenance[0].language, Language::Java);

        // Now verify malformed edit is downgraded to ManualOnly instead of crashing
        let mut bad_reg = AnalyzerRegistry::new();
        bad_reg.register(MockJavaAnalyzer {
            emit_malformed_edit: true,
        });
        let bad_orch = WorkspaceOrchestrator { registry: &bad_reg };
        let bad_out = bad_orch
            .run(
                Path::new("."),
                &files,
                &[],
                PipelineConfig {
                    apply_fixes: true,
                    allow_unsafe_fixes: false,
                    max_passes: 10,
                },
                None,
            )
            .unwrap();

        assert!(!bad_out.files[0].modified);
        assert_eq!(bad_out.files[0].remaining_diagnostics.len(), 1);
        assert_eq!(
            bad_out.files[0].remaining_diagnostics[0]
                .fix
                .as_ref()
                .unwrap()
                .applicability,
            Applicability::ManualOnly
        );
    }

    #[test]
    fn render_unified_diff_formats_single_line_change() {
        let orig = "line1\nold_call()\nline3\n";
        let updated = "line1\nnew_call()\nline3\n";
        let diff = render_unified_diff("src/app.py", orig, updated);
        assert!(diff.contains("--- a/src/app.py"));
        assert!(diff.contains("+++ b/src/app.py"));
        assert!(diff.contains("-old_call()"));
        assert!(diff.contains("+new_call()"));
    }
}
