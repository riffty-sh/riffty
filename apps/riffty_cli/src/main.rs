use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod commands;
mod config;
mod infra_cmd;
mod sarif;

use clap::{Parser, Subcommand, ValueEnum};
use config::{RifftyTomlConfig, is_excluded};
use riffty_adapter_ast_grep::AstGrepAdapter;
use riffty_adapter_fs::{FsWorkspaceAdapter, WorkspaceResolver};
use riffty_adapter_java_fast::JavaFastAnalyzer;
use riffty_adapter_plugin::{JavaPluginAdapter, discover_java_sidecar_bundle};
use riffty_adapter_refaster::{JavaRefasterAnalyzer, RefasterYamlRule};
use riffty_adapter_tsrs::TsrsSemanticAnalyzer;
use riffty_adapter_ruff_ast::{RuffAstAdapter, RuffImportAdapter};
use riffty_adapter_ty::{NullTypeOracle, TyOracleAdapter};
use riffty_core::{
    AnalysisProvenance, AnalysisTier, ByteOffset, DiffTarget, FileId, GitDiffPort, Language,
    LineIndex, PatchApplierPort, RulePlugin, Severity, TextRange, TypeOraclePort, WorkspaceVfsPort,
};
use riffty_pipeline::{
    AnalyzerRegistry, AppliedFixRecord, DEFAULT_MAX_FIXPOINT_PASSES, PipelineConfig,
    PythonLanguageAnalyzer, RifftyEngine, WorkspaceOrchestrator, render_unified_diff,
};
use riffty_rules_builtin::builtin_rules;
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    name = "riffty",
    version,
    about = "Deterministic polyglot (Python & Java) bug detector & surgical byte-offset remediation engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Analyze Python and Java files and optionally apply deterministic byte-offset fixes
    Check(CheckArgs),
    /// Explain a built-in rule (or list all built-in rules)
    Explain(ExplainArgs),
    /// Inspect workspace root, config, Python/Java analyzer tiers, and runtime environment
    Doctor(DoctorArgs),
    /// Run the MCP stdio reviewer server exposing `riffty_review_changes`
    Mcp(McpArgs),
    /// Analyze before/after workload efficiency, SLO compliance, and estimated cloud cost savings
    Efficiency(infra_cmd::EfficiencyArgs),
    /// Inspect registered infrastructure intelligence provider plugins and runtime health
    Infra(infra_cmd::InfraArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
    Sarif,
}

#[derive(Debug, clap::Args)]
struct CheckArgs {
    /// Target Python file or workspace directory
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Automatically apply `Applicability::Safe` byte-offset fixes on disk
    #[arg(long)]
    fix: bool,

    /// With `--fix`, print unified diffs without modifying files on disk (exits 1 if changes pending)
    #[arg(long)]
    dry_run: bool,

    /// Allow `Applicability::Unsafe` fixes when `--fix` is enabled
    #[arg(long)]
    unsafe_fixes: bool,

    /// Restrict analysis to git-diff hunks plus `reverse_dependents` callers
    #[arg(long)]
    diff: bool,

    /// With `--diff`, inspect staged (`--cached`) changes instead of unstaged changes
    #[arg(long)]
    staged: bool,

    /// With `--diff --base-ref <REF>`, inspect branch diff `<REF>...HEAD`
    #[arg(long)]
    base_ref: Option<String>,

    /// Optional Refaster declarative YAML ruleset file
    #[arg(long, env = "RIFFTY_RULES_PATH")]
    rules: Option<PathBuf>,

    /// Output format (`text`, `json`, or `sarif`)
    #[arg(long, value_enum, env = "RIFFTY_FORMAT", default_value_t = OutputFormat::Text)]
    format: OutputFormat,

    /// Print a compact per-language provenance and per-rule summary table
    #[arg(long)]
    summary: bool,

    /// Fail closed (exit code 2) if any analyzed language degrades below Tier 2 semantic analysis
    #[arg(long)]
    require_tier2: bool,

    /// Maximum fixpoint convergence passes (1..=10)
    #[arg(long, env = "RIFFTY_MAX_PASSES", default_value_t = DEFAULT_MAX_FIXPOINT_PASSES)]
    max_passes: usize,

    /// Optional Python virtual environment path for third-party `.pyi` stub mounting
    #[arg(long, env = "RIFFTY_PYTHON_VENV")]
    python_venv: Option<PathBuf>,

    /// Disable semantic type oracle (`NullTypeOracle` syntactic-only fallback)
    #[arg(long)]
    no_type_oracle: bool,

    /// Force zero-JVM Java Tier 1 (`tree-sitter-java`) even if Tier 2 `riffty-java` is available
    #[arg(long)]
    java_tier1_only: bool,

    /// Additional static Java classpath entries (`:` separated) for Tier 2 `riffty-java`
    #[arg(long, env = "RIFFTY_JAVA_CLASSPATH")]
    java_classpath: Option<String>,

    /// Use Milestone 3 warm Unix-socket JDK 25 daemon for interactive sub-100ms Java Tier 2 analysis
    #[arg(long, env = "RIFFTY_JAVA_DAEMON")]
    java_daemon: bool,

    /// Custom Unix domain socket path for `--java-daemon`
    #[arg(long, env = "RIFFTY_JAVA_DAEMON_SOCKET")]
    java_daemon_socket: Option<PathBuf>,

    /// Opt-in Level-1 build-tool classpath resolution (`mvn -o dependency:build-classpath`); never enabled in MCP
    #[arg(long)]
    allow_build_tool: bool,
}

#[derive(Debug, clap::Args)]
struct ExplainArgs {
    /// Rule ID or slug to explain (e.g., `JAVA-BIGDEC-001`, `RFT-118`)
    rule: Option<String>,

    /// List all built-in Python and Java rules
    #[arg(long)]
    list: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum DoctorFormat {
    Text,
    Json,
}

#[derive(Debug, clap::Args)]
struct DoctorArgs {
    /// Target file or workspace directory to inspect
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Output format (`text` or `json`)
    #[arg(long, value_enum, default_value_t = DoctorFormat::Text)]
    format: DoctorFormat,
}

#[derive(Debug, clap::Args)]
struct McpArgs {
    /// Default workspace root directory for `riffty_review_changes`
    #[arg(long, env = "RIFFTY_WORKSPACE_ROOT")]
    workspace: Option<PathBuf>,

    /// Optional Refaster declarative YAML ruleset file
    #[arg(long, env = "RIFFTY_RULES_PATH")]
    rules: Option<PathBuf>,

    /// Optional Python virtual environment path for third-party `.pyi` stub mounting
    #[arg(long, env = "RIFFTY_PYTHON_VENV")]
    python_venv: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub(crate) struct JsonDiagnosticEntry {
    file: String,
    line: u32,
    column: u32,
    rule_id: String,
    rule_name: String,
    severity: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    applicability: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggested_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    type_proof: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct JsonCheckReport {
    files_checked: usize,
    auto_healed_count: usize,
    diagnostics_count: usize,
    dry_run: bool,
    provenance: Vec<AnalysisProvenance>,
    auto_healed: Vec<AppliedFixRecord>,
    diagnostics: Vec<JsonDiagnosticEntry>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::Check(args) => match run_check(args) {
            Ok(has_blocking) => {
                if has_blocking {
                    ExitCode::from(1)
                } else {
                    ExitCode::SUCCESS
                }
            }
            Err(err) => {
                eprintln!("riffty error: {err}");
                ExitCode::from(2)
            }
        },
        Commands::Explain(args) => match commands::run_explain(args.rule.as_deref(), args.list) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("riffty explain error: {err}");
                ExitCode::from(2)
            }
        },
        Commands::Doctor(args) => {
            match commands::run_doctor(&args.path, matches!(args.format, DoctorFormat::Json)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("riffty doctor error: {err}");
                    ExitCode::from(2)
                }
            }
        }
        Commands::Mcp(args) => {
            let root = args
                .workspace
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            let toml_cfg = match RifftyTomlConfig::discover(&root) {
                Ok(cfg) => cfg,
                Err(err) => {
                    eprintln!("riffty config error: {err}");
                    return ExitCode::from(2);
                }
            };
            let effective_rules = args.rules.or(toml_cfg.rules);
            let effective_venv = args.python_venv.or(toml_cfg.python_venv);
            if let Err(err) =
                riffty_mcp::run_stdio_server_with_venv(root, effective_rules, effective_venv).await
            {
                eprintln!("riffty mcp error: {err}");
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            }
        }
        Commands::Efficiency(args) => match infra_cmd::run_efficiency(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("riffty efficiency error: {err}");
                ExitCode::from(2)
            }
        },
        Commands::Infra(args) => match infra_cmd::run_infra(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("riffty infra error: {err}");
                ExitCode::from(2)
            }
        },
    }
}

fn run_check(args: CheckArgs) -> Result<bool, String> {
    let resolved_ws = WorkspaceResolver::resolve(&args.path);
    let workspace_root_dir: &Path = &resolved_ws.root;
    let toml_cfg = RifftyTomlConfig::discover(workspace_root_dir)?;
    let effective_python_venv = args.python_venv.or(toml_cfg.python_venv);
    let effective_rules = args.rules.or(toml_cfg.rules);

    let fs_adapter = FsWorkspaceAdapter;
    let parser = RuffAstAdapter;
    let import_mgr = RuffImportAdapter;
    let matcher = AstGrepAdapter;
    let ty_oracle = TyOracleAdapter::with_venv(effective_python_venv);
    let null_oracle = NullTypeOracle;
    let (oracle, py_tier, py_detail): (&dyn TypeOraclePort, AnalysisTier, Option<String>) =
        if args.no_type_oracle {
            (
                &null_oracle,
                AnalysisTier::Tier1Syntactic,
                Some("ruff-ast (syntactic only)".to_string()),
            )
        } else {
            (
                &ty_oracle,
                AnalysisTier::Tier2Semantic,
                Some("ruff-ast + ty".to_string()),
            )
        };

    let mut rules: Vec<Box<dyn RulePlugin>> = builtin_rules();
    let mut jsts_rules: Vec<RefasterYamlRule> = Vec::new();
    let mut java_yaml_rules: Vec<RefasterYamlRule> = Vec::new();
    let mut rules_paths: Vec<PathBuf> = Vec::new();
    if let Some(ref yaml_path) = effective_rules {
        let yaml_rules =
            RefasterYamlRule::from_yaml_file_for_language(yaml_path, Language::Python)?;
        rules.extend(yaml_rules);
        jsts_rules.extend(RefasterYamlRule::jsts_rules_from_file(yaml_path)?);
        java_yaml_rules.extend(RefasterYamlRule::java_rules_from_file(yaml_path)?);
        rules_paths.push(yaml_path.clone());
    }

    let mut registry = AnalyzerRegistry::new();
    registry.register(PythonLanguageAnalyzer {
        engine: RifftyEngine {
            parser: &parser,
            oracle,
            import_manager: &import_mgr,
            pattern_matcher: Some(&matcher),
            rules,
        },
        tier: py_tier,
        runtime_detail: py_detail,
    });

    if !args.java_tier1_only
        && let Some(bundle) = discover_java_sidecar_bundle()
    {
        let extra_classpath: Vec<String> = args
            .java_classpath
            .as_deref()
            .map(|cp| {
                cp.split([':', ';'])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        registry.register(JavaPluginAdapter {
            bundle,
            extra_classpath,
            use_daemon: args.java_daemon,
            daemon_socket: args.java_daemon_socket.clone(),
            architecture_forbid: toml_cfg.java.architecture.forbid.clone(),
            allow_build_tool: args.allow_build_tool,
        });
    } else {
        registry.register(JavaFastAnalyzer);
    }
    if !java_yaml_rules.is_empty() {
        registry.register(JavaRefasterAnalyzer {
            rules: java_yaml_rules,
        });
    }

    registry.register(TsrsSemanticAnalyzer {
        refaster_rules: jsts_rules,
        tier1_only: args.no_type_oracle,
        architecture_forbid: toml_cfg.jsts.architecture.forbid.clone(),
    });

    let extensions = registry.supported_extensions();
    let mut workspace_files = fs_adapter.discover_files(&args.path, &extensions)?;
    if !toml_cfg.exclude.is_empty() {
        workspace_files.retain(|wf| !is_excluded(&wf.relative_path, &toml_cfg.exclude));
    }

    let scoped_ranges = if args.diff {
        let diff_target = if let Some(base_ref) = &args.base_ref {
            DiffTarget::BranchDiff {
                base_ref: base_ref.clone(),
            }
        } else if args.staged {
            DiffTarget::Staged
        } else {
            DiffTarget::Unstaged
        };

        let diff_entries = fs_adapter.touched_ranges(workspace_root_dir, &diff_target)?;
        let path_to_id: HashMap<String, FileId> = workspace_files
            .iter()
            .map(|wf| (normalize_rel(&wf.relative_path), wf.file_id))
            .collect();

        let mut map: HashMap<FileId, Vec<TextRange>> = HashMap::new();
        for entry in &diff_entries {
            let key = normalize_rel(&entry.relative_path.to_string_lossy());
            if let Some(&fid) = path_to_id.get(&key) {
                map.entry(fid)
                    .or_default()
                    .extend(entry.touched_ranges.iter().copied());
            }
        }

        let snapshot = map.clone();
        for analyzer in registry.analyzers() {
            for (dep_fid, dep_range) in
                analyzer.reverse_dependents(workspace_root_dir, &workspace_files, &snapshot)
            {
                map.entry(dep_fid).or_default().push(dep_range);
            }
        }
        Some(map)
    } else {
        None
    };

    let orchestrator = WorkspaceOrchestrator {
        registry: &registry,
    };
    let run_output = orchestrator.run(
        workspace_root_dir,
        &workspace_files,
        &rules_paths,
        PipelineConfig {
            apply_fixes: args.fix,
            allow_unsafe_fixes: args.fix && args.unsafe_fixes,
            max_passes: args.max_passes,
        },
        scoped_ranges.as_ref(),
    )?;

    let id_to_file: HashMap<FileId, (&Path, &str)> = workspace_files
        .iter()
        .map(|wf| (wf.file_id, (wf.path.as_path(), wf.source.as_str())))
        .collect();

    let mut all_healed: Vec<AppliedFixRecord> = Vec::new();
    let mut json_diags: Vec<JsonDiagnosticEntry> = Vec::new();
    let mut dry_run_diffs: Vec<String> = Vec::new();
    let mut has_blocking = false;

    for res in &run_output.files {
        if args.fix
            && res.modified
            && let Some(&(abs, orig_source)) = id_to_file.get(&res.file_id)
        {
            if args.dry_run {
                let diff = render_unified_diff(&res.relative_path, orig_source, &res.final_source);
                if !diff.is_empty() {
                    dry_run_diffs.push(diff);
                }
                has_blocking = true;
            } else {
                fs_adapter.write_patched_file(workspace_root_dir, abs, &res.final_source)?;
            }
        }
        all_healed.extend(res.applied_fixes.iter().cloned());

        let line_index = LineIndex::new(&res.final_source);
        for diag in &res.remaining_diagnostics {
            if matches!(diag.severity, Severity::Error | Severity::Warning) {
                has_blocking = true;
            }
            let start = ByteOffset(diag.range.start.0);
            let (line, col) = line_index.line_col(&res.final_source, start);
            let applicability = diag.fix.as_ref().map(|f| format!("{:?}", f.applicability));
            let suggested_action = diag.fix.as_ref().map(|f| f.title.clone());

            json_diags.push(JsonDiagnosticEntry {
                file: res.relative_path.clone(),
                line,
                column: col,
                rule_id: diag.rule_id.clone(),
                rule_name: diag.rule_name.clone(),
                severity: diag.severity.as_upper_str().to_string(),
                message: diag.message.clone(),
                applicability,
                suggested_action,
                type_proof: diag.type_proof.clone(),
            });
        }
    }

    if args.require_tier2 {
        for prov in &run_output.provenance {
            if prov.tier != AnalysisTier::Tier2Semantic {
                let reason = prov
                    .degraded_reason
                    .as_deref()
                    .unwrap_or("degraded below Tier 2 semantic analysis");
                let hint = prov
                    .remediation_hint
                    .as_deref()
                    .map(|h| format!(" ({h})"))
                    .unwrap_or_default();
                return Err(format!(
                    "--require-tier2 failed for {}: {reason}{hint}",
                    prov.language.as_str()
                ));
            }
        }
    }

    match args.format {
        OutputFormat::Text => {
            for diff in &dry_run_diffs {
                print!("{diff}");
            }
            for d in &json_diags {
                let app_tag = d
                    .applicability
                    .as_deref()
                    .map(|a| format!(" [fix:{a}]"))
                    .unwrap_or_default();
                println!(
                    "{}:{}:{}: {} [{}]{} {}",
                    d.file, d.line, d.column, d.rule_id, d.severity, app_tag, d.message
                );
            }
            if args.summary {
                print_summary(&run_output.provenance, &all_healed, &json_diags);
            }
            let mode_note = if args.fix && args.dry_run {
                " (dry-run, no files written)"
            } else {
                ""
            };
            println!(
                "riffty: checked {} file(s), auto-healed {} fix(es){}, {} remaining diagnostic(s).",
                run_output.files.len(),
                all_healed.len(),
                mode_note,
                json_diags.len()
            );
        }
        OutputFormat::Json => {
            let report = JsonCheckReport {
                files_checked: run_output.files.len(),
                auto_healed_count: all_healed.len(),
                diagnostics_count: json_diags.len(),
                dry_run: args.fix && args.dry_run,
                provenance: run_output.provenance,
                auto_healed: all_healed,
                diagnostics: json_diags,
            };
            let out = serde_json::to_string_pretty(&report)
                .map_err(|e| format!("JSON serialization error: {e}"))?;
            println!("{out}");
        }
        OutputFormat::Sarif => {
            let sarif_doc = sarif::render_sarif_report(&json_diags, &run_output.provenance);
            let out = serde_json::to_string_pretty(&sarif_doc)
                .map_err(|e| format!("SARIF serialization error: {e}"))?;
            println!("{out}");
        }
    }

    Ok(has_blocking)
}

fn print_summary(
    provenance: &[AnalysisProvenance],
    healed: &[AppliedFixRecord],
    diags: &[JsonDiagnosticEntry],
) {
    println!("\nSummary:");
    for p in provenance {
        let tier_label = match p.tier {
            AnalysisTier::Tier2Semantic => "tier2_semantic",
            AnalysisTier::Tier1Syntactic => "tier1_syntactic",
        };
        let detail = p.runtime_detail.as_deref().unwrap_or("default");
        println!(
            "  - {:<6} | {:<15} | {} file(s) | {}",
            p.language.as_str(),
            tier_label,
            p.files_analyzed,
            detail
        );
        if let Some(ref reason) = p.degraded_reason {
            println!("    degraded: {reason}");
        }
    }
    let mut by_rule: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for d in diags {
        *by_rule.entry(d.rule_id.as_str()).or_insert(0) += 1;
    }
    if !by_rule.is_empty() {
        println!("  Remaining diagnostics by rule:");
        for (rule, count) in by_rule {
            println!("    {rule}: {count}");
        }
    }
    if !healed.is_empty() {
        println!("  Auto-healed fixes: {}", healed.len());
    }
}

fn normalize_rel(p: &str) -> String {
    p.replace('\\', "/").trim_start_matches("./").to_string()
}
