use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use riffty_adapter_jsts_fast::{
    JsTsFastAnalyzer, JsTsRuleDoc, OxcImportAdapter, analyze_jsts_source,
};
use riffty_adapter_refaster::{RefasterYamlRule, evaluate_jsts_refaster_rules};
use riffty_core::{
    AnalysisProvenance, AnalysisTier, AnalyzePassRequest, AnalyzePassResponse, Applicability,
    ByteOffset, Diagnostic, Edit, FileId, Fix, ImportManagerPort, Language, LanguageAnalyzerPort,
    LineIndex, Severity, TextRange, WorkspaceFile,
};
use tsrs_ast::{self as ast, Kind, Node, SourceFile};
use tsrs_checker::{Checker, SignatureKind, Type, TypeFlags};
use tsrs_compiler::{Context, Program, ProgramOptions};
use tsrs_core::tspath;
use tsrs_core::{
    CompilerOptions, JsxEmit, ModuleKind, ModuleResolutionKind, P, ScriptTarget, Tristate,
};
use tsrs_tsoptions::{ParseConfigHost, ParsedCommandLine};
use tsrs_vfs::{FS, bundled, cachedvfs, osvfs, wrapvfs};

/// Known AI / Cloud / DB SDK client classes that hold connection pools or heavy state.
const SDK_CLIENT_CLASSES: &[&str] = &["OpenAI", "Anthropic", "S3Client", "PrismaClient"];

/// Documentation metadata for all 5 Tier 2 `tsrs` semantic JS/TS rules (used by `riffty explain` and SARIF).
pub fn jsts_tier2_rule_catalog() -> &'static [JsTsRuleDoc] {
    &[
        JsTsRuleDoc {
            id: "JSTS-NUM-SORT-001",
            name: "numeric-array-sort-without-comparator",
            language: Language::JsTs,
            severity: Severity::Error,
            default_applicability: "Safe (Tier 2 proven number[])",
            summary: "`Array.prototype.sort()` on a proven `number[]` sorts lexicographically (`[10, 2]` stays `[10, 2]`).",
            rationale: "JavaScript's default `.sort()` converts elements to UTF-16 strings before comparing, silently corrupting numeric rankings, prices, and metrics. Tier 2 `tsrs` proves the receiver is `number[]` / `readonly number[]` and safely inserts `(a, b) => a - b`.",
            bad_example: "const prices: number[] = [100, 20, 3];\nprices.sort();",
            good_example: "const prices: number[] = [100, 20, 3];\nprices.sort((a, b) => a - b);",
        },
        JsTsRuleDoc {
            id: "JSTS-FLOAT-PROMISE-001",
            name: "floating-promise",
            language: Language::JsTs,
            severity: Severity::Error,
            default_applicability: "ManualOnly",
            summary: "Unhandled floating `Promise<T>` expression statement is neither `await`ed, `.catch()`ed, nor marked `void`.",
            rationale: "Unawaited Promises execute concurrently and silently swallow or crash the process on rejection (`UnhandledPromiseRejection`). Await the promise, attach `.catch(...)`, or explicitly mark fire-and-forget calls with `void`.",
            bad_example: "async function handler() {\n  sendAuditLog(event);\n}",
            good_example: "async function handler() {\n  await sendAuditLog(event);\n}",
        },
        JsTsRuleDoc {
            id: "JSTS-MISUSED-PROMISE-001",
            name: "misused-promise-in-condition",
            language: Language::JsTs,
            severity: Severity::Error,
            default_applicability: "Safe (`await` inside async fn) / ManualOnly",
            summary: "A proven `Promise<T>` is used directly as a boolean condition (a Promise object is always truthy!).",
            rationale: "Evaluating `if (checkAuth())` when `checkAuth()` returns `Promise<boolean>` ALWAYS takes the true branch regardless of the resolved boolean, causing severe auth/business-logic bypasses.",
            bad_example: "async function guard() {\n  if (isAuthorized(user)) { grant(); }\n}",
            good_example: "async function guard() {\n  if (await isAuthorized(user)) { grant(); }\n}",
        },
        JsTsRuleDoc {
            id: "JSTS-SDK-CLIENT-001",
            name: "per-request-sdk-client-allocation",
            language: Language::JsTs,
            severity: Severity::Warning,
            default_applicability: "Safe (stateless hoist) / ManualOnly (request-scoped)",
            summary: "Avoid instantiating `new OpenAI()`, `new Anthropic()`, `new S3Client()`, or `new PrismaClient()` inside request handlers.",
            rationale: "Allocating heavy AI/cloud/DB SDK clients per request destroys HTTP/TLS connection pooling, leaks sockets/handles, and adds latency & GC pressure. Stateless clients are safely hoisted to module scope.",
            bad_example: "export async function handle(prompt: string) {\n  const client = new OpenAI();\n  return client.responses.create({ input: prompt });\n}",
            good_example: "const __RIFFTY_CLIENT_1 = new OpenAI();\nexport async function handle(prompt: string) {\n  const client = __RIFFTY_CLIENT_1;\n  return client.responses.create({ input: prompt });\n}",
        },
        JsTsRuleDoc {
            id: "JSTS-AWAIT-LOOP-001",
            name: "sequential-await-in-loop",
            language: Language::JsTs,
            severity: Severity::Warning,
            default_applicability: "ManualOnly",
            summary: "Sequential `await` on a proven `Promise<T>` inside a loop creates a serial I/O waterfall.",
            rationale: "Awaiting independent network/database/LLM calls sequentially inside a loop multiplies tail latency by N. Batch independent calls with `Promise.all(...)` (or bounded concurrency).",
            bad_example: "for (const id of ids) {\n  results.push(await fetchItem(id));\n}",
            good_example: "const results = await Promise.all(ids.map((id) => fetchItem(id)));",
        },
    ]
}

struct TsConfigHost {
    fs: Arc<dyn FS>,
    cwd: String,
}

impl ParseConfigHost for TsConfigHost {
    fn fs(&self) -> &dyn FS {
        &*self.fs
    }
    fn get_current_directory(&self) -> &str {
        &self.cwd
    }
}

struct ProgramOwner {
    program: &'static Program,
}

impl Drop for ProgramOwner {
    fn drop(&mut self) {
        // SAFETY: Program is created exclusively here and all checker handles are dropped before ProgramOwner.
        unsafe {
            tsrs_compiler::free_unshared_program(self.program);
        }
    }
}

/// Tier 2 in-process TypeScript 7 (`tsrs`) semantic analyzer for `Language::JsTs`.
/// Automatically degrades to Tier 1 `oxc_semantic` with loud provenance if `tsconfig.json`
/// or required `node_modules` are absent.
#[derive(Default)]
pub struct TsrsSemanticAnalyzer {
    pub refaster_rules: Vec<RefasterYamlRule>,
    pub tier1_only: bool,
}

#[derive(Debug)]
enum Tier2Readiness {
    Ready {
        tsconfig_path: PathBuf,
        types_pkg_count: usize,
    },
    Degraded {
        reason: String,
        hint: String,
    },
}

fn check_tier2_readiness(workspace_root: &Path, files: &[WorkspaceFile]) -> Tier2Readiness {
    // 1. Locate tsconfig.json (at workspace_root or nearest ancestor of the first target file).
    let mut tsconfig_path = find_tsconfig(workspace_root, files);
    if tsconfig_path.is_none() {
        return Tier2Readiness::Degraded {
            reason: "tsconfig.json not found; type-dependent rules skipped".to_string(),
            hint: "Add a tsconfig.json (or pass a workspace with tsconfig.json) to enable Tier 2 tsrs semantic proofs".to_string(),
        };
    }
    let tsconfig = tsconfig_path.take().unwrap();
    let project_dir = tsconfig.parent().unwrap_or(workspace_root);

    // 2. Check package.json dependencies vs node_modules presence (strictly read-only, zero RCE!).
    let node_modules_dir = find_node_modules_dir(project_dir, workspace_root);
    let has_declared_deps = package_json_declares_deps(project_dir)
        || package_json_declares_deps(workspace_root);

    if has_declared_deps && node_modules_dir.is_none() {
        return Tier2Readiness::Degraded {
            reason: "node_modules not installed; type-dependent rules skipped".to_string(),
            hint: "run npm ci --ignore-scripts (or pnpm install --ignore-scripts)".to_string(),
        };
    }

    let types_pkg_count = node_modules_dir
        .as_deref()
        .map(count_at_types_packages)
        .unwrap_or(0);

    Tier2Readiness::Ready {
        tsconfig_path: tsconfig,
        types_pkg_count,
    }
}

fn find_tsconfig(workspace_root: &Path, files: &[WorkspaceFile]) -> Option<PathBuf> {
    if let Some(first) = files.first() {
        let start = first.path.parent().unwrap_or(workspace_root);
        for ancestor in start.ancestors() {
            let candidate = ancestor.join("tsconfig.json");
            if candidate.is_file() {
                return Some(candidate);
            }
            if ancestor == workspace_root {
                break;
            }
        }
    }
    let root_candidate = workspace_root.join("tsconfig.json");
    if root_candidate.is_file() {
        return Some(root_candidate);
    }
    None
}

fn find_node_modules_dir(project_dir: &Path, workspace_root: &Path) -> Option<PathBuf> {
    for ancestor in project_dir.ancestors() {
        let nm = ancestor.join("node_modules");
        if nm.is_dir() {
            return Some(nm);
        }
        if ancestor == workspace_root {
            break;
        }
    }
    let root_nm = workspace_root.join("node_modules");
    if root_nm.is_dir() {
        return Some(root_nm);
    }
    None
}

fn package_json_declares_deps(dir: &Path) -> bool {
    let pkg = dir.join("package.json");
    let Ok(raw) = std::fs::read_to_string(&pkg) else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    let non_empty_obj = |key: &str| {
        val.get(key)
            .and_then(|v| v.as_object())
            .is_some_and(|m| !m.is_empty())
    };
    non_empty_obj("dependencies") || non_empty_obj("devDependencies")
}

fn count_at_types_packages(node_modules: &Path) -> usize {
    let types_dir = node_modules.join("@types");
    let Ok(entries) = std::fs::read_dir(&types_dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .count()
}

impl LanguageAnalyzerPort for TsrsSemanticAnalyzer {
    fn language(&self) -> Language {
        Language::JsTs
    }

    fn extensions(&self) -> &'static [&'static str] {
        Language::JSTS_EXTENSIONS
    }

    /// REQ-M2-06: Incremental Pass 2 re-verifies modified files in-memory.
    fn max_fixpoint_passes(&self) -> usize {
        2
    }

    fn import_manager(&self) -> Option<&dyn ImportManagerPort> {
        static IMPORT_MGR: OxcImportAdapter = OxcImportAdapter;
        Some(&IMPORT_MGR)
    }

    fn analyze_pass(&self, req: &AnalyzePassRequest<'_>) -> Result<AnalyzePassResponse, String> {
        if self.tier1_only {
            let fast = JsTsFastAnalyzer {
                refaster_rules: self
                    .refaster_rules
                    .iter()
                    .map(|r| RefasterYamlRule::new(r.spec().clone()))
                    .collect(),
            };
            return fast.analyze_pass(req);
        }

        let readiness = check_tier2_readiness(req.workspace_root, req.files);
        let (tsconfig_path, types_pkg_count) = match readiness {
            Tier2Readiness::Ready {
                tsconfig_path,
                types_pkg_count,
            } => (tsconfig_path, types_pkg_count),
            Tier2Readiness::Degraded { reason, hint } => {
                let fast = JsTsFastAnalyzer {
                    refaster_rules: self
                        .refaster_rules
                        .iter()
                        .map(|r| RefasterYamlRule::new(r.spec().clone()))
                        .collect(),
                };
                let mut resp = fast.analyze_pass(req)?;
                resp.provenance.degraded_reason = Some(reason);
                resp.provenance.remediation_hint = Some(hint);
                return Ok(resp);
            }
        };

        let mut loaded_refaster: Vec<RefasterYamlRule> = Vec::new();
        for rp in req.rules_paths {
            loaded_refaster.extend(RefasterYamlRule::jsts_rules_from_file(rp)?);
        }
        let active_refaster: &[RefasterYamlRule] = if self.refaster_rules.is_empty() {
            &loaded_refaster
        } else {
            &self.refaster_rules
        };

        // Determine active files for this pass
        let target_files: Vec<&WorkspaceFile> = req
            .files
            .iter()
            .filter(|wf| {
                req.scoped_ranges
                    .is_none_or(|scopes| scopes.contains_key(&wf.file_id))
            })
            .collect();

        // 1. Build in-memory tsrs Program (zero Node.js, zero RCE, run_external_code = False)
        let mut diagnostics = analyze_workspace_with_tsrs(
            req.workspace_root,
            &tsconfig_path,
            req.files,
            &target_files,
        )?;

        // 2. Also run Tier 1 oxc_semantic rules + Refaster YAML rules so Tier 2 is a strict superset!
        for wf in &target_files {
            diagnostics.extend(analyze_jsts_source(wf.file_id, &wf.path, &wf.source));
            if !active_refaster.is_empty() {
                diagnostics.extend(evaluate_jsts_refaster_rules(
                    wf.file_id,
                    &wf.path,
                    &wf.relative_path,
                    &wf.source,
                    active_refaster,
                ));
            }
        }

        if let Some(scopes) = req.scoped_ranges {
            diagnostics.retain(|d| {
                let allowed = scopes.get(&d.file_id).map(Vec::as_slice).unwrap_or(&[]);
                allowed.iter().any(|sp| sp.intersects(d.range))
            });
        }

        let tsconfig_label = tsconfig_path
            .strip_prefix(req.workspace_root)
            .unwrap_or(&tsconfig_path)
            .display()
            .to_string();

        Ok(AnalyzePassResponse {
            diagnostics,
            provenance: AnalysisProvenance {
                language: Language::JsTs,
                tier: AnalysisTier::Tier2Semantic,
                files_analyzed: target_files.len(),
                runtime_detail: Some(format!(
                    "tsrs (TS7 Rust) · {} · {} @types packages",
                    tsconfig_label, types_pkg_count
                )),
                degraded_reason: None,
                remediation_hint: None,
            },
        })
    }
}

/// Public helper for Pass 2 semantic verification (`REQ-M2-06`): returns the number of
/// TypeScript semantic diagnostics emitted by `tsrs` on the given target files.
pub fn count_tsrs_semantic_errors(
    workspace_root: &Path,
    tsconfig_path: &Path,
    all_files: &[WorkspaceFile],
) -> Result<usize, String> {
    let (_diags, semantic_error_count) = run_tsrs_program(
        workspace_root,
        tsconfig_path,
        all_files,
        &all_files.iter().collect::<Vec<_>>(),
        true,
    )?;
    Ok(semantic_error_count)
}

fn analyze_workspace_with_tsrs(
    workspace_root: &Path,
    tsconfig_path: &Path,
    all_files: &[WorkspaceFile],
    target_files: &[&WorkspaceFile],
) -> Result<Vec<Diagnostic>, String> {
    let (diags, _) = run_tsrs_program(workspace_root, tsconfig_path, all_files, target_files, false)?;
    Ok(diags)
}

fn canonical_ts_path(path: &Path, cwd: &Path) -> String {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let resolved = abs.canonicalize().unwrap_or(abs);
    tspath::normalize_path(&resolved.to_string_lossy())
}

fn run_tsrs_program(
    workspace_root: &Path,
    tsconfig_path: &Path,
    all_files: &[WorkspaceFile],
    target_files: &[&WorkspaceFile],
    count_semantic_errors: bool,
) -> Result<(Vec<Diagnostic>, usize), String> {
    let project_dir = tsconfig_path.parent().unwrap_or(workspace_root);
    let cwd_str = canonical_ts_path(project_dir, workspace_root);
    let tsconfig_str = canonical_ts_path(tsconfig_path, workspace_root);

    // Build in-memory overrides map so Pass 1 and Pass 2 see exact in-memory `WorkspaceFile` content.
    let mut source_overrides: HashMap<String, String> = HashMap::with_capacity(all_files.len());
    let mut path_to_wf: HashMap<String, (FileId, String)> = HashMap::with_capacity(all_files.len());
    let mut target_norm_paths: Vec<String> = Vec::with_capacity(target_files.len());

    for wf in all_files {
        let norm = canonical_ts_path(&wf.path, workspace_root);
        source_overrides.insert(norm.clone(), wf.source.clone());
        path_to_wf.insert(norm, (wf.file_id, wf.source.clone()));
    }
    for wf in target_files {
        target_norm_paths.push(canonical_ts_path(&wf.path, workspace_root));
    }

    let overrides = Arc::new(source_overrides);
    let exists_ov = Arc::clone(&overrides);
    let read_ov = Arc::clone(&overrides);
    let stat_ov = Arc::clone(&overrides);
    let realpath_ov = Arc::clone(&overrides);

    let wrapped = wrapvfs::wrap(
        osvfs::fs(),
        wrapvfs::Replacements {
            file_exists: Some(Box::new(move |p| {
                exists_ov.contains_key(p) || osvfs::fs().file_exists(p)
            })),
            read_file: Some(Box::new(move |p| {
                if let Some(src) = read_ov.get(p) {
                    return Some(src.clone());
                }
                osvfs::fs().read_file(p)
            })),
            stat: Some(Box::new(move |p| {
                stat_ov.get(p).map_or_else(
                    || osvfs::fs().stat(p),
                    |src| {
                        Some(tsrs_vfs::FileInfo {
                            name: p.into(),
                            size: i64::try_from(src.len()).unwrap_or(0),
                            mode: tsrs_vfs::FileMode::None,
                            mod_time: None,
                        })
                    },
                )
            })),
            realpath: Some(Box::new(move |p| {
                if realpath_ov.contains_key(p) {
                    p.into()
                } else {
                    osvfs::fs().realpath(p)
                }
            })),
            ..Default::default()
        },
    );

    let fs: Arc<dyn FS> = Arc::new(bundled::wrap_fs(cachedvfs::from(Arc::new(wrapped))));
    let region = tsrs_core::arena::Region::new_scratch(1 << 20);
    let _scope = region.enter();

    let cfg_host = TsConfigHost {
        fs: Arc::clone(&fs),
        cwd: cwd_str.clone(),
    };

    // Parse tsconfig.json; ensure target files are included and `run_external_code` is strictly FALSE (Zero-RCE!).
    let mut parsed_cmd: ParsedCommandLine =
        match tsrs_tsoptions::get_parsed_command_line_of_config_file(
            &tsconfig_str,
            None,
            None,
            &cfg_host,
            None,
        )
        .0
        {
            Some(cmd) => cmd,
            None => tsrs_tsoptions::new_parsed_command_line(
                P::new(CompilerOptions {
                    allow_js: Tristate::True,
                    module: ModuleKind::ES_NEXT,
                    module_resolution: ModuleResolutionKind::BUNDLER,
                    target: ScriptTarget::ES2022,
                    jsx: JsxEmit::REACT_JSX,
                    strict_null_checks: Tristate::True,
                    strict_function_types: Tristate::True,
                    es_module_interop: Tristate::True,
                    resolve_json_module: Tristate::True,
                    run_external_code: Tristate::False,
                    ..Default::default()
                }),
                target_norm_paths.clone(),
                Vec::new(),
                tspath::ComparePathsOptions {
                    current_directory: cwd_str.clone(),
                    use_case_sensitive_file_names: fs.use_case_sensitive_file_names(),
                },
            ),
        };

    // Enforce Zero-RCE invariant: never allow tsconfig contentMappers external code execution.
    if let Some(opts) = parsed_cmd.compiler_options() {
        let mut safe_opts = (*opts).clone();
        safe_opts.run_external_code = Tristate::False;
        safe_opts.no_emit = Tristate::True;
        parsed_cmd.parsed_config.compiler_options = Some(P::new(safe_opts));
    }

    // Ensure all requested target files are present in root file names so tsrs loads & checks them.
    let existing_roots: HashSet<String> = parsed_cmd.file_names().iter().cloned().collect();
    for t in &target_norm_paths {
        if !existing_roots.contains(t) {
            parsed_cmd.parsed_config.file_names.push(t.clone());
        }
    }

    let config_ptr = P::new(parsed_cmd);
    let host = tsrs_compiler::new_compiler_host(
        &cwd_str,
        Arc::clone(&fs),
        &bundled::default_lib_path(),
        None,
        None,
        None,
    );
    let mut prog_opts = ProgramOptions::new(config_ptr, host);
    prog_opts.checker_regions = true;

    let owner = ProgramOwner {
        program: tsrs_compiler::new_program(prog_opts),
    };
    let program = owner.program;
    program.bind_source_files();

    let mut source_files: Vec<P<SourceFile>> = Vec::new();
    let mut file_metas: Vec<(FileId, String)> = Vec::new();
    for norm in &target_norm_paths {
        if let Some(sf) = program.get_source_file(norm)
            && let Some((fid, src)) = path_to_wf.get(norm)
        {
            source_files.push(sf);
            file_metas.push((*fid, src.clone()));
        }
    }

    let collected: Mutex<Vec<Diagnostic>> = Mutex::new(Vec::new());

    program.for_each_checker_group(&source_files, |checker, idx, sf| {
        let (file_id, ref source) = file_metas[idx];
        let diags = run_tier2_rules_on_file(file_id, source, sf, checker);
        collected.lock().unwrap().extend(diags);
    });

    let mut total_sem_errs = 0usize;
    if count_semantic_errors {
        let ctx = Context::default();
        for &sf in &source_files {
            total_sem_errs += program.get_semantic_diagnostics(&ctx, Some(sf)).len();
        }
    }

    let mut all_diags = collected.into_inner().unwrap();
    all_diags.sort_by_key(|d| (d.file_id, d.range.start, d.range.end));
    Ok((all_diags, total_sem_errs))
}

fn node_text_range(sf: P<SourceFile>, node: P<Node>) -> Option<TextRange> {
    let raw_pos = node.pos().expect_source("parsed AST node");
    let trimmed_start = tsrs_scanner::get_range_of_token_at_position(sf, raw_pos)
        .source_pos()
        .unwrap_or(raw_pos);
    let end = node.end().expect_source("parsed AST node");
    if trimmed_start <= end {
        Some(TextRange::new(trimmed_start, end))
    } else {
        None
    }
}

fn run_tier2_rules_on_file(
    file_id: FileId,
    source: &str,
    sf: P<SourceFile>,
    checker: &mut Checker,
) -> Vec<Diagnostic> {
    let mut ctx = Tier2FileVisitor {
        file_id,
        source,
        sf,
        checker,
        line_index: LineIndex::new(source),
        function_depth: 0,
        async_fn_stack: Vec::new(),
        loop_depth: 0,
        top_level_stmt_start: None,
        hoist_counter: 0,
        diagnostics: Vec::new(),
    };

    for &stmt in sf.statements().nodes() {
        ctx.top_level_stmt_start = node_text_range(sf, stmt).map(|r| r.start.0);
        ctx.visit_node(stmt);
    }

    ctx.diagnostics
}

struct Tier2FileVisitor<'a> {
    file_id: FileId,
    source: &'a str,
    sf: P<SourceFile>,
    checker: &'a mut Checker,
    line_index: LineIndex,
    function_depth: usize,
    async_fn_stack: Vec<bool>,
    loop_depth: usize,
    top_level_stmt_start: Option<u32>,
    hoist_counter: usize,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Tier2FileVisitor<'a> {
    fn in_async_function(&self) -> bool {
        self.async_fn_stack.last().copied().unwrap_or(false)
    }

    fn is_suppressed(&self, range: TextRange, rule_id: &str) -> bool {
        let line = self.line_index.line_of(range.start);
        if is_jsts_line_suppressed(self.line_index.line_slice(self.source, line), rule_id) {
            return true;
        }
        if line > 1 {
            let prev = self.line_index.line_slice(self.source, line - 1);
            if prev.trim_start().starts_with("//") && is_jsts_line_suppressed(prev, rule_id) {
                return true;
            }
        }
        false
    }

    fn push_diagnostic(&mut self, diag: Diagnostic) {
        if !self.is_suppressed(diag.range, &diag.rule_id) {
            self.diagnostics.push(diag);
        }
    }

    fn next_hoist_name(&mut self, prefix: &str) -> String {
        loop {
            self.hoist_counter += 1;
            let candidate = format!("{prefix}_{}", self.hoist_counter);
            if !self.source.contains(&candidate) {
                return candidate;
            }
        }
    }

    fn hoist_insert_offset_and_indent(&self) -> (u32, String) {
        let raw_start = self.top_level_stmt_start.unwrap_or(0);
        let line = self.line_index.line_of(ByteOffset(raw_start));
        let line_text = self.line_index.line_slice(self.source, line);
        let indent: String = line_text
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let bytes = self.source.as_bytes();
        let mut line_start = (raw_start as usize).min(bytes.len());
        while line_start > 0 && bytes[line_start - 1] != b'\n' {
            line_start -= 1;
        }
        (line_start as u32, indent)
    }

    fn visit_node(&mut self, node: P<Node>) {
        let kind = node.kind();
        match kind {
            Kind::CallExpression => self.check_num_sort(node),
            Kind::ExpressionStatement => self.check_floating_promise(node),
            Kind::IfStatement => {
                let if_stmt = node.as_if_statement();
                self.check_misused_promise_condition(if_stmt.expression);
            }
            Kind::ConditionalExpression => {
                let cond = node.as_conditional_expression();
                self.check_misused_promise_condition(cond.condition);
            }
            Kind::BinaryExpression => {
                let bin = node.as_binary_expression();
                if matches!(
                    bin.operator_token.kind(),
                    Kind::AmpersandAmpersandToken | Kind::BarBarToken
                ) {
                    self.check_misused_promise_condition(bin.left);
                }
            }
            Kind::PrefixUnaryExpression => {
                let unary = node.as_prefix_unary_expression();
                if unary.operator == Kind::ExclamationToken {
                    self.check_misused_promise_condition(unary.operand);
                }
            }
            Kind::NewExpression => self.check_sdk_client(node),
            Kind::AwaitExpression => self.check_await_in_loop(node),
            _ => {}
        }

        let is_fn = matches!(
            kind,
            Kind::FunctionDeclaration
                | Kind::FunctionExpression
                | Kind::ArrowFunction
                | Kind::MethodDeclaration
        );
        let is_loop = matches!(
            kind,
            Kind::ForStatement
                | Kind::ForInStatement
                | Kind::ForOfStatement
                | Kind::WhileStatement
                | Kind::DoStatement
        );

        if is_fn {
            let is_async = node
                .modifiers()
                .is_some_and(|mods| mods.nodes().iter().any(|m| m.kind() == Kind::AsyncKeyword));
            self.function_depth += 1;
            self.async_fn_stack.push(is_async);
        }
        let prev_loop = self.loop_depth;
        if is_fn {
            // Reset loop depth when entering a nested function/callback!
            self.loop_depth = 0;
        } else if is_loop {
            self.loop_depth += 1;
        }

        let mut children = Vec::new();
        node.for_each_child(&mut |child| {
            children.push(child);
            false
        });
        for child in children {
            self.visit_node(child);
        }

        self.loop_depth = prev_loop;
        if is_fn {
            self.function_depth -= 1;
            self.async_fn_stack.pop();
        }
    }

    /// Rule 1 (`JSTS-NUM-SORT-001`): Proven `number[]` / `readonly number[]` `.sort()` without comparator.
    fn check_num_sort(&mut self, node: P<Node>) {
        let call = node.as_call_expression();
        if !call.arguments.nodes().is_empty() {
            return;
        }
        let callee = call.expression;
        if !ast::is_access_expression(callee) {
            return;
        }
        let (prop_name, found) = self.checker.get_accessed_property_name(callee);
        if !found || !matches!(prop_name.as_ref(), "sort" | "toSorted") {
            return;
        }
        let Some(receiver) = callee.expression() else {
            return;
        };
        let raw_ty = self.checker.get_type_at_location(receiver);
        let ty = self
            .checker
            .get_base_constraint_of_type(raw_ty)
            .unwrap_or(raw_ty);

        // Fail-closed on `any`, `unknown`, `error` (`REQ-M2-03`)
        if is_any_or_unknown_or_error(ty) {
            return;
        }

        if !is_proven_number_array(self.checker, ty) {
            return;
        }

        let Some(call_range) = node_text_range(self.sf, node) else {
            return;
        };
        let Some(call_src) = call_range.slice(self.source) else {
            return;
        };
        let Some(open_paren_rel) = call_src.rfind('(') else {
            return;
        };
        let Some(close_paren_rel) = call_src.rfind(')') else {
            return;
        };
        if open_paren_rel >= close_paren_rel {
            return;
        }
        let insert_start = call_range.start.0 + (open_paren_rel as u32) + 1;
        let insert_end = call_range.start.0 + (close_paren_rel as u32);
        let type_str = self.checker.type_to_string_exported(ty);

        self.push_diagnostic(Diagnostic {
            rule_id: "JSTS-NUM-SORT-001".to_string(),
            rule_name: "numeric-array-sort-without-comparator".to_string(),
            severity: Severity::Error,
            file_id: self.file_id,
            range: call_range,
            message: format!(
                "Calling `.{prop_name}()` on `{type_str}` sorts lexicographically; pass numeric comparator `(a, b) => a - b`."
            ),
            type_proof: Some(serde_json::json!({
                "receiver_type": type_str,
                "method": prop_name.as_ref(),
            })),
            fix: Some(Fix {
                title: "Insert numeric comparator `(a, b) => a - b`".to_string(),
                applicability: Applicability::Safe,
                edits: vec![Edit {
                    range: TextRange::new(insert_start, insert_end),
                    replacement: "(a, b) => a - b".to_string(),
                }],
                required_imports: Vec::new(),
            }),
        });
    }

    /// Rule 2 (`JSTS-FLOAT-PROMISE-001`): Unhandled floating `Promise<T>` expression statement.
    fn check_floating_promise(&mut self, node: P<Node>) {
        let stmt = node.as_expression_statement();
        let expr = unwrap_parens(stmt.expression);

        // Exempt `void expr`, `await expr`, assignments, and `.catch(...)` / `.then(a, b)`
        if matches!(
            expr.kind(),
            Kind::VoidExpression | Kind::AwaitExpression
        ) {
            return;
        }
        if ast::is_assignment_expression(expr, true) {
            return;
        }
        if is_handled_promise_chain(expr, self.checker) {
            return;
        }

        let raw_ty = self.checker.get_type_at_location(expr);
        let ty = self
            .checker
            .get_base_constraint_of_type(raw_ty)
            .unwrap_or(raw_ty);

        if is_any_or_unknown_or_error(ty) {
            return;
        }
        if !contains_promise_like(self.checker, ty) {
            return;
        }

        let Some(range) = node_text_range(self.sf, node) else {
            return;
        };
        let type_str = self.checker.type_to_string_exported(ty);

        self.push_diagnostic(Diagnostic {
            rule_id: "JSTS-FLOAT-PROMISE-001".to_string(),
            rule_name: "floating-promise".to_string(),
            severity: Severity::Error,
            file_id: self.file_id,
            range,
            message: format!(
                "Floating `{type_str}` expression is neither awaited, handled with `.catch()`, nor explicitly marked `void`."
            ),
            type_proof: Some(serde_json::json!({
                "expression_type": type_str,
            })),
            fix: Some(Fix {
                title: "Await the promise, attach `.catch(...)`, or prefix with `void`".to_string(),
                applicability: Applicability::ManualOnly,
                edits: Vec::new(),
                required_imports: Vec::new(),
            }),
        });
    }

    /// Rule 3 (`JSTS-MISUSED-PROMISE-001`): Proven `Promise<T>` used directly in a boolean condition.
    fn check_misused_promise_condition(&mut self, cond_node: P<Node>) {
        let expr = unwrap_parens(cond_node);
        if expr.kind() == Kind::AwaitExpression {
            return;
        }
        let raw_ty = self.checker.get_type_at_location(expr);
        let ty = self
            .checker
            .get_base_constraint_of_type(raw_ty)
            .unwrap_or(raw_ty);

        if is_any_or_unknown_or_error(ty) {
            return;
        }
        // Only fire when ALL union branches are Promise-like (always truthy!).
        if !is_always_promise_like(self.checker, ty) {
            return;
        }

        let Some(range) = node_text_range(self.sf, expr) else {
            return;
        };
        let type_str = self.checker.type_to_string_exported(ty);

        let fix = if self.in_async_function() {
            Fix {
                title: "Await the Promise in condition".to_string(),
                applicability: Applicability::Safe,
                edits: vec![Edit {
                    range: TextRange::new(range.start.0, range.start.0),
                    replacement: "await ".to_string(),
                }],
                required_imports: Vec::new(),
            }
        } else {
            Fix {
                title: "Await the Promise (make enclosing function `async` or resolve before branching)".to_string(),
                applicability: Applicability::ManualOnly,
                edits: Vec::new(),
                required_imports: Vec::new(),
            }
        };

        self.push_diagnostic(Diagnostic {
            rule_id: "JSTS-MISUSED-PROMISE-001".to_string(),
            rule_name: "misused-promise-in-condition".to_string(),
            severity: Severity::Error,
            file_id: self.file_id,
            range,
            message: format!(
                "Condition has proven Promise type `{type_str}`, which is always truthy."
            ),
            type_proof: Some(serde_json::json!({
                "condition_type": type_str,
                "inside_async_fn": self.in_async_function(),
            })),
            fix: Some(fix),
        });
    }

    /// Rule 4 (`JSTS-SDK-CLIENT-001`): Per-request `new OpenAI()` / `new Anthropic()` / `new S3Client()` / `new PrismaClient()`.
    fn check_sdk_client(&mut self, node: P<Node>) {
        // Only flag inside function/handler bodies, NOT at module top-level.
        if self.function_depth == 0 {
            return;
        }
        let new_expr = node.as_new_expression();
        let Some(ctor_name) = extract_ctor_name(new_expr.expression) else {
            return;
        };
        if !SDK_CLIENT_CLASSES.contains(&ctor_name) {
            return;
        }

        let raw_ty = self.checker.get_type_at_location(node);
        let type_str = self.checker.type_to_string_exported(raw_ty);
        // Fail-closed: if tsrs resolved the constructor expression to `any` or `error`, never emit a Safe hoist!
        let type_is_proven = !is_any_or_unknown_or_error(raw_ty);

        let Some(range) = node_text_range(self.sf, node) else {
            return;
        };
        let Some(expr_src) = range.slice(self.source) else {
            return;
        };

        let is_stateless = new_expr.arguments.as_ref().is_none_or(|args| {
            args.nodes().iter().all(|&arg| is_tsrs_constant_literal(arg))
        });

        let fix = if type_is_proven && is_stateless {
            let var_name = self.next_hoist_name("__RIFFTY_CLIENT");
            let (insert_at, indent) = self.hoist_insert_offset_and_indent();
            let decl = format!("{indent}const {var_name} = {expr_src};\n");
            Fix {
                title: format!("Hoist stateless `{ctor_name}` client to module-scope `const {var_name}`"),
                applicability: Applicability::Safe,
                edits: vec![
                    Edit {
                        range: TextRange::new(insert_at, insert_at),
                        replacement: decl,
                    },
                    Edit {
                        range,
                        replacement: var_name,
                    },
                ],
                required_imports: Vec::new(),
            }
        } else {
            Fix {
                title: format!("Reuse a shared `{ctor_name}` instance instead of allocating per request"),
                applicability: Applicability::ManualOnly,
                edits: Vec::new(),
                required_imports: Vec::new(),
            }
        };

        self.push_diagnostic(Diagnostic {
            rule_id: "JSTS-SDK-CLIENT-001".to_string(),
            rule_name: "per-request-sdk-client-allocation".to_string(),
            severity: Severity::Warning,
            file_id: self.file_id,
            range,
            message: format!(
                "Avoid allocating `new {ctor_name}(...)` inside a function/handler; reuse a module-scoped client to preserve connection pools."
            ),
            type_proof: Some(serde_json::json!({
                "client_type": type_str,
                "stateless_args": is_stateless,
                "type_proven": type_is_proven,
            })),
            fix: Some(fix),
        });
    }

    /// Rule 5 (`JSTS-AWAIT-LOOP-001`): Sequential `await` inside loop on proven `Promise<T>`.
    fn check_await_in_loop(&mut self, node: P<Node>) {
        if self.loop_depth == 0 {
            return;
        }
        let await_expr = node.as_await_expression();
        let raw_ty = self.checker.get_type_at_location(await_expr.expression);
        let ty = self
            .checker
            .get_base_constraint_of_type(raw_ty)
            .unwrap_or(raw_ty);

        if is_any_or_unknown_or_error(ty) {
            return;
        }
        if !contains_promise_like(self.checker, ty) {
            return;
        }

        let Some(range) = node_text_range(self.sf, node) else {
            return;
        };
        let type_str = self.checker.type_to_string_exported(ty);

        self.push_diagnostic(Diagnostic {
            rule_id: "JSTS-AWAIT-LOOP-001".to_string(),
            rule_name: "sequential-await-in-loop".to_string(),
            severity: Severity::Warning,
            file_id: self.file_id,
            range,
            message: format!(
                "Sequential `await` on `{type_str}` inside a loop creates a serial I/O waterfall; consider `Promise.all(...)`."
            ),
            type_proof: Some(serde_json::json!({
                "awaited_type": type_str,
            })),
            fix: Some(Fix {
                title: "Batch independent loop awaits with `Promise.all(...)`".to_string(),
                applicability: Applicability::ManualOnly,
                edits: Vec::new(),
                required_imports: Vec::new(),
            }),
        });
    }
}

fn unwrap_parens(mut node: P<Node>) -> P<Node> {
    while node.kind() == Kind::ParenthesizedExpression {
        node = node.as_parenthesized_expression().expression.get();
    }
    node
}

fn is_any_or_unknown_or_error(ty: P<Type>) -> bool {
    // Note: in TypeScript / tsrs, errorType carries `TypeFlags::Any`.
    ty.flags().intersects(TypeFlags::Any | TypeFlags::Unknown)
}

fn union_parts(ty: P<Type>) -> Vec<P<Type>> {
    if ty.flags().intersects(TypeFlags::Union) {
        ty.types().to_vec()
    } else {
        vec![ty]
    }
}

fn is_proven_number_array(checker: &mut Checker, ty: P<Type>) -> bool {
    let parts = union_parts(ty);
    if parts.is_empty() {
        return false;
    }
    let num_key = checker.get_number_type();
    parts.into_iter().all(|part| {
        if is_any_or_unknown_or_error(part) {
            return false;
        }
        if !checker.is_array_or_tuple_type(part) {
            return false;
        }
        if let Some(elem_ty) = checker.get_index_type_of_type(part, num_key) {
            let elem = checker
                .get_base_constraint_of_type(elem_ty)
                .unwrap_or(elem_ty);
            if is_any_or_unknown_or_error(elem) {
                return false;
            }
            let elem_parts = union_parts(elem);
            return !elem_parts.is_empty()
                && elem_parts
                    .into_iter()
                    .all(|e| e.flags().intersects(TypeFlags::NumberLike));
        }
        false
    })
}

fn is_single_promise_like(checker: &mut Checker, ty: P<Type>) -> bool {
    if is_any_or_unknown_or_error(ty)
        || ty
            .flags()
            .intersects(TypeFlags::Undefined | TypeFlags::Null | TypeFlags::Void | TypeFlags::Never)
    {
        return false;
    }
    if let Some(then_sym) = checker.get_property_of_type(ty, "then") {
        let then_ty = checker.get_type_of_symbol(then_sym);
        if !is_any_or_unknown_or_error(then_ty)
            && !checker
                .get_signatures_of_type(then_ty, SignatureKind::Call)
                .is_empty()
        {
            return true;
        }
    }
    false
}

fn contains_promise_like(checker: &mut Checker, ty: P<Type>) -> bool {
    union_parts(ty)
        .into_iter()
        .any(|part| is_single_promise_like(checker, part))
}

fn is_always_promise_like(checker: &mut Checker, ty: P<Type>) -> bool {
    let parts = union_parts(ty);
    !parts.is_empty()
        && parts
            .into_iter()
            .all(|part| is_single_promise_like(checker, part))
}

fn is_handled_promise_chain(expr: P<Node>, checker: &mut Checker) -> bool {
    if expr.kind() != Kind::CallExpression {
        return false;
    }
    let call = expr.as_call_expression();
    if !ast::is_access_expression(call.expression) {
        return false;
    }
    let (prop, found) = checker.get_accessed_property_name(call.expression);
    if !found {
        return false;
    }
    match prop.as_ref() {
        "catch" => !call.arguments.nodes().is_empty(),
        "then" => call.arguments.nodes().len() >= 2,
        _ => false,
    }
}

fn extract_ctor_name<'a>(expr: P<Node>) -> Option<&'a str> {
    let inner = unwrap_parens(expr);
    match inner.kind() {
        Kind::Identifier => Some(inner.as_identifier().text()),
        Kind::PropertyAccessExpression => {
            let prop = inner.as_property_access_expression();
            if prop.name.kind() == Kind::Identifier {
                Some(prop.name.as_identifier().text())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn is_tsrs_constant_literal(node: P<Node>) -> bool {
    let inner = unwrap_parens(node);
    match inner.kind() {
        Kind::StringLiteral
        | Kind::NoSubstitutionTemplateLiteral
        | Kind::NumericLiteral
        | Kind::TrueKeyword
        | Kind::FalseKeyword
        | Kind::NullKeyword => true,
        Kind::ArrayLiteralExpression => inner
            .as_array_literal_expression()
            .elements
            .nodes()
            .iter()
            .all(|&el| is_tsrs_constant_literal(el)),
        Kind::ObjectLiteralExpression => inner
            .as_object_literal_expression()
            .properties
            .nodes()
            .iter()
            .all(|&prop| {
                if prop.kind() == Kind::PropertyAssignment {
                    is_tsrs_constant_literal(prop.as_property_assignment().initializer.get())
                } else {
                    false
                }
            }),
        _ => false,
    }
}

fn is_jsts_line_suppressed(line_text: &str, rule_id: &str) -> bool {
    let Some(comment_idx) = line_text.find("//") else {
        return false;
    };
    let comment = line_text[comment_idx + 2..].trim();
    let Some(rest) = comment.strip_prefix("riffty:") else {
        return false;
    };
    let rest = rest.trim();
    if let Some(after_ignore) = rest.strip_prefix("ignore") {
        let after_ignore = after_ignore.trim();
        if let Some(inner) = after_ignore.strip_prefix('[')
            && let Some(end) = inner.find(']')
        {
            return inner[..end]
                .split(',')
                .any(|tok| tok.trim().eq_ignore_ascii_case(rule_id) || tok.trim() == "*");
        }
        if after_ignore.is_empty() || after_ignore.starts_with("--") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use riffty_pipeline::{AnalyzerRegistry, PipelineConfig, WorkspaceOrchestrator};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_ts_project(name: &str, files: &[(&str, &str)], tsconfig: Option<&str>) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("riffty_tsrs_test_{name}_{nanos}"));
        std::fs::create_dir_all(&root).unwrap();
        if let Some(cfg) = tsconfig {
            std::fs::write(root.join("tsconfig.json"), cfg).unwrap();
        }
        for (rel, content) in files {
            let p = root.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
        }
        root
    }

    fn run_tsrs_on_project(
        root: &Path,
        rel_files: &[&str],
        apply_fixes: bool,
    ) -> riffty_pipeline::WorkspaceRunOutput {
        let mut registry = AnalyzerRegistry::new();
        registry.register(TsrsSemanticAnalyzer::default());
        let orch = WorkspaceOrchestrator {
            registry: &registry,
        };
        let wf_list: Vec<WorkspaceFile> = rel_files
            .iter()
            .enumerate()
            .map(|(i, rel)| {
                let p = root.join(rel);
                let src = std::fs::read_to_string(&p).unwrap();
                WorkspaceFile {
                    file_id: FileId((i + 1) as u32),
                    path: p,
                    relative_path: (*rel).to_string(),
                    source: src,
                }
            })
            .collect();
        orch.run(
            root,
            &wf_list,
            &[],
            PipelineConfig {
                apply_fixes,
                allow_unsafe_fixes: false,
                max_passes: 2,
            },
            None,
        )
        .expect("tsrs orchestrator ok")
    }

    const DEFAULT_TSCONFIG: &str = r#"{
  "compilerOptions": {
    "target": "ES2023",
    "module": "ESNext",
    "strict": true,
    "noEmit": true
  },
  "include": ["src/**/*"]
}"#;

    #[test]
    fn req_m2_01_healthy_tier2_vs_degraded_tier1_fallback() {
        // 1. Degraded: no tsconfig.json -> Tier 1 with loud degraded_reason
        let degraded_root = make_temp_ts_project(
            "degraded",
            &[("src/app.ts", "export const nums: number[] = [10, 2];\nnums.sort();\n")],
            None,
        );
        let out_deg = run_tsrs_on_project(&degraded_root, &["src/app.ts"], false);
        assert_eq!(out_deg.provenance[0].tier, AnalysisTier::Tier1Syntactic);
        assert!(out_deg.provenance[0].degraded_reason.is_some());
        assert!(out_deg.files[0].remaining_diagnostics.is_empty());

        // 2. Degraded: package.json has dependencies, node_modules absent -> Tier 1 with npm ci hint
        let missing_nm_root = make_temp_ts_project(
            "missing_nm",
            &[
                ("package.json", r#"{"name":"app","dependencies":{"openai":"^4.0.0"}}"#),
                ("src/app.ts", "export const nums: number[] = [10, 2];\nnums.sort();\n"),
            ],
            Some(DEFAULT_TSCONFIG),
        );
        let out_nm = run_tsrs_on_project(&missing_nm_root, &["src/app.ts"], false);
        assert_eq!(out_nm.provenance[0].tier, AnalysisTier::Tier1Syntactic);
        assert!(
            out_nm.provenance[0]
                .remediation_hint
                .as_deref()
                .unwrap()
                .contains("npm ci --ignore-scripts")
        );

        // 3. Healthy Tier 2: tsconfig.json + standalone (no missing deps) -> tier2_semantic!
        let healthy_root = make_temp_ts_project(
            "healthy",
            &[("src/app.ts", "export const nums: number[] = [10, 2];\nnums.sort();\n")],
            Some(DEFAULT_TSCONFIG),
        );
        let out_ok = run_tsrs_on_project(&healthy_root, &["src/app.ts"], true);
        assert_eq!(out_ok.provenance[0].tier, AnalysisTier::Tier2Semantic);
        assert!(
            out_ok.provenance[0]
                .runtime_detail
                .as_deref()
                .unwrap()
                .contains("tsrs (TS7 Rust)")
        );
        assert!(out_ok.files[0].final_source.contains("nums.sort((a, b) => a - b);"));

        let _ = std::fs::remove_dir_all(&degraded_root);
        let _ = std::fs::remove_dir_all(&missing_nm_root);
        let _ = std::fs::remove_dir_all(&healthy_root);
    }

    #[test]
    fn req_m2_02_and_03_all_5_tier2_rules_and_fail_closed_any_unknown() {
        let src = r#"
class OpenAI {
  constructor(opts?: { apiKey?: string; timeout?: number }) {}
  ping(): Promise<string> { return Promise.resolve("ok"); }
}

export function sortChecks(
  nums: number[],
  roNums: readonly number[],
  strs: string[],
  anyArr: any[],
  unkArr: unknown[]
) {
  nums.sort();
  roNums.toSorted();
  strs.sort();
  anyArr.sort();
  unkArr.sort();
}

async function fetchFlag(): Promise<boolean> { return true; }
function maybeAsync(): Promise<boolean> | undefined { return undefined; }
function returnsAny(): any { return 1; }

export async function asyncChecks(ids: string[], apiKey: string) {
  // Floating promise (MUST fire)
  fetchFlag();
  // Handled promises (MUST NOT fire)
  void fetchFlag();
  await fetchFlag();
  fetchFlag().catch(() => {});
  // Union containing Promise (MUST fire floating)
  maybeAsync();
  // Any (MUST NOT fire)
  returnsAny();

  // Misused promise in async fn -> Safe await fix
  if (fetchFlag()) {
    // Union Promise | undefined -> NOT always truthy, must NOT fire misused
    if (maybeAsync()) {}
  }

  // Stateless SDK client -> Safe hoist
  const c1 = new OpenAI({ timeout: 5000 });
  await c1.ping();
  // Request-scoped SDK client -> ManualOnly (NOT hoisted!)
  const c2 = new OpenAI({ apiKey });
  await c2.ping();

  // Sequential await in loop -> ManualOnly
  for (const _id of ids) {
    await fetchFlag();
  }
}

export function syncMisused(): void {
  // Outside async fn -> ManualOnly (NOT Safe edit!)
  if (fetchFlag()) {
    void 0;
  }
}
"#;
        let root = make_temp_ts_project("all_5_rules", &[("src/main.ts", src)], Some(DEFAULT_TSCONFIG));
        let out = run_tsrs_on_project(&root, &["src/main.ts"], true);
        assert_eq!(out.provenance[0].tier, AnalysisTier::Tier2Semantic);

        let res = &out.files[0];
        let fixed = &res.final_source;

        // 1. JSTS-NUM-SORT-001: fixed number[] and readonly number[], skipped string[], any[], unknown[]
        assert!(fixed.contains("nums.sort((a, b) => a - b);"), "fixed:\n{fixed}");
        assert!(fixed.contains("roNums.toSorted((a, b) => a - b);"), "fixed:\n{fixed}");
        assert!(fixed.contains("strs.sort();"), "string[] must not be touched");
        assert!(fixed.contains("anyArr.sort();"), "any[] must not be touched (fail-closed)");
        assert!(fixed.contains("unkArr.sort();"), "unknown[] must not be touched (fail-closed)");

        // 2. JSTS-MISUSED-PROMISE-001: async fn fixed with `await `, sync fn left as ManualOnly
        assert!(fixed.contains("if (await fetchFlag()) {"), "fixed:\n{fixed}");
        assert!(fixed.contains("if (maybeAsync()) {}"), "union | undefined must not be touched");
        assert!(
            res.remaining_diagnostics
                .iter()
                .any(|d| d.rule_id == "JSTS-MISUSED-PROMISE-001"),
            "sync misused promise must remain ManualOnly: {:?}",
            res.remaining_diagnostics
        );

        // 3. JSTS-FLOAT-PROMISE-001: flagged fetchFlag() and maybeAsync(), ignored void/await/.catch/any
        let float_count = res
            .remaining_diagnostics
            .iter()
            .filter(|d| d.rule_id == "JSTS-FLOAT-PROMISE-001")
            .count();
        assert_eq!(float_count, 2, "remaining: {:?}", res.remaining_diagnostics);

        // 4. JSTS-SDK-CLIENT-001: stateless hoisted, request-scoped ManualOnly
        assert!(fixed.contains("const __RIFFTY_CLIENT_1 = new OpenAI({ timeout: 5000 });"), "fixed:\n{fixed}");
        assert!(fixed.contains("const c2 = new OpenAI({ apiKey });"), "request-scoped must not be hoisted");
        assert!(
            res.remaining_diagnostics
                .iter()
                .any(|d| d.rule_id == "JSTS-SDK-CLIENT-001")
        );

        // 5. JSTS-AWAIT-LOOP-001: ManualOnly
        assert!(
            res.remaining_diagnostics
                .iter()
                .any(|d| d.rule_id == "JSTS-AWAIT-LOOP-001")
        );

        // REQ-M2-06: Confirm 0 new TypeScript semantic errors introduced after riffty fixes!
        let pre_files = vec![WorkspaceFile {
            file_id: FileId(1),
            path: root.join("src/main.ts"),
            relative_path: "src/main.ts".to_string(),
            source: src.to_string(),
        }];
        let pre_sem_errs =
            count_tsrs_semantic_errors(&root, &root.join("tsconfig.json"), &pre_files).unwrap();
        let post_files = vec![WorkspaceFile {
            file_id: FileId(1),
            path: root.join("src/main.ts"),
            relative_path: "src/main.ts".to_string(),
            source: fixed.clone(),
        }];
        let post_sem_errs =
            count_tsrs_semantic_errors(&root, &root.join("tsconfig.json"), &post_files).unwrap();
        assert!(
            post_sem_errs <= pre_sem_errs,
            "riffty fixes must introduce 0 new TypeScript semantic errors (pre={pre_sem_errs}, post={post_sem_errs})"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn req_m2_05_zero_rce_never_executes_malicious_package_json_or_eslint_config() {
        let root = make_temp_ts_project(
            "zero_rce",
            &[
                (
                    "package.json",
                    r#"{"name":"evil","scripts":{"postinstall":"exit 99","prepare":"exit 99"}}"#,
                ),
                ("eslint.config.js", "process.exit(99);\n"),
                ("src/safe.ts", "export const xs: number[] = [3, 1, 2];\nxs.sort();\n"),
            ],
            Some(DEFAULT_TSCONFIG),
        );
        let out = run_tsrs_on_project(&root, &["src/safe.ts"], true);
        assert_eq!(out.provenance[0].tier, AnalysisTier::Tier2Semantic);
        assert!(out.files[0].final_source.contains("xs.sort((a, b) => a - b);"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
