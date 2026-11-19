use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod config;

use clap::{Parser, Subcommand, ValueEnum};
use config::{RifftyTomlConfig, is_excluded};
use riffty_adapter_ast_grep::AstGrepAdapter;
use riffty_adapter_fs::FsWorkspaceAdapter;
use riffty_adapter_refaster::RefasterYamlRule;
use riffty_adapter_ruff_ast::{RuffAstAdapter, RuffImportAdapter};
use riffty_adapter_ty::{NullTypeOracle, TyOracleAdapter};
use riffty_core::{
    ByteOffset, DiffTarget, FileId, GitDiffPort, PatchApplierPort, RulePlugin, Severity,
    SyntaxParserPort, TextRange, TypeOraclePort, WorkspaceVfsPort,
};
use riffty_pipeline::{
    AppliedFixRecord, DEFAULT_MAX_FIXPOINT_PASSES, PipelineConfig, RifftyEngine,
};
use riffty_rules_builtin::builtin_rules;
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    name = "riffty",
    version,
    about = "Deterministic type-aware Python bug detector & surgical byte-offset remediation engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Analyze Python files and optionally apply deterministic byte-offset fixes
    Check(CheckArgs),
    /// Run the MCP stdio reviewer server exposing `riffty_review_changes`
    Mcp(McpArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, clap::Args)]
struct CheckArgs {
    /// Target Python file or workspace directory
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Automatically apply `Applicability::Safe` byte-offset fixes on disk
    #[arg(long)]
    fix: bool,

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

    /// Output format (`text` or `json`)
    #[arg(long, value_enum, env = "RIFFTY_FORMAT", default_value_t = OutputFormat::Text)]
    format: OutputFormat,

    /// Maximum fixpoint convergence passes (1..=10)
    #[arg(long, env = "RIFFTY_MAX_PASSES", default_value_t = DEFAULT_MAX_FIXPOINT_PASSES)]
    max_passes: usize,

    /// Optional Python virtual environment path for third-party `.pyi` stub mounting
    #[arg(long, env = "RIFFTY_PYTHON_VENV")]
    python_venv: Option<PathBuf>,

    /// Disable semantic type oracle (`NullTypeOracle` syntactic-only fallback)
    #[arg(long)]
    no_type_oracle: bool,
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
struct JsonDiagnosticEntry {
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
    }
}

fn run_check(args: CheckArgs) -> Result<bool, String> {
    let workspace_root_dir: &Path = if args.path.is_file() {
        args.path.parent().unwrap_or(Path::new("."))
    } else {
        &args.path
    };
    let toml_cfg = RifftyTomlConfig::discover(workspace_root_dir)?;
    let effective_python_venv = args.python_venv.or(toml_cfg.python_venv);
    let effective_rules = args.rules.or(toml_cfg.rules);

    let fs_adapter = FsWorkspaceAdapter;
    let parser = RuffAstAdapter;
    let import_mgr = RuffImportAdapter;
    let matcher = AstGrepAdapter;
    let ty_oracle = TyOracleAdapter::with_venv(effective_python_venv);
    let null_oracle = NullTypeOracle;
    let oracle: &dyn TypeOraclePort = if args.no_type_oracle {
        &null_oracle
    } else {
        &ty_oracle
    };

    let mut rules: Vec<Box<dyn RulePlugin>> = builtin_rules();
    if let Some(ref yaml_path) = effective_rules {
        let yaml_rules = RefasterYamlRule::from_yaml_file(yaml_path)?;
        rules.extend(yaml_rules);
    }

    let mut workspace_files = fs_adapter.discover_python_files(&args.path)?;
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

        oracle.index_workspace(&workspace_files);
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

        let snapshot: Vec<(FileId, Vec<TextRange>)> =
            map.iter().map(|(&k, v)| (k, v.clone())).collect();
        for (fid, spans) in &snapshot {
            for (dep_fid, dep_range) in oracle.reverse_dependents(*fid, spans) {
                map.entry(dep_fid).or_default().push(dep_range);
            }
        }
        Some(map)
    } else {
        None
    };

    let engine = RifftyEngine {
        parser: &parser,
        oracle,
        import_manager: &import_mgr,
        pattern_matcher: Some(&matcher),
        rules,
    };

    let results = engine.run_workspace(
        &workspace_files,
        PipelineConfig {
            apply_fixes: args.fix,
            allow_unsafe_fixes: args.fix && args.unsafe_fixes,
            max_passes: args.max_passes,
        },
        scoped_ranges.as_ref(),
    )?;

    let id_to_abs: HashMap<FileId, &Path> = workspace_files
        .iter()
        .map(|wf| (wf.file_id, wf.path.as_path()))
        .collect();

    let mut all_healed: Vec<AppliedFixRecord> = Vec::new();
    let mut json_diags: Vec<JsonDiagnosticEntry> = Vec::new();
    let mut has_blocking = false;

    for res in &results {
        if args.fix
            && res.modified
            && let Some(&abs) = id_to_abs.get(&res.file_id)
        {
            fs_adapter.write_patched_file(&args.path, abs, &res.final_source)?;
        }
        all_healed.extend(res.applied_fixes.iter().cloned());

        let parsed = parser.parse(res.file_id, &res.final_source)?;
        for diag in &res.remaining_diagnostics {
            if matches!(diag.severity, Severity::Error | Severity::Warning) {
                has_blocking = true;
            }
            let start = ByteOffset(diag.range.start.0);
            let line = parsed.line_of(start);
            let col = parsed.col_of(start);
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

    match args.format {
        OutputFormat::Text => {
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
            println!(
                "riffty: checked {} file(s), auto-healed {} fix(es), {} remaining diagnostic(s).",
                results.len(),
                all_healed.len(),
                json_diags.len()
            );
        }
        OutputFormat::Json => {
            let report = JsonCheckReport {
                files_checked: results.len(),
                auto_healed_count: all_healed.len(),
                diagnostics_count: json_diags.len(),
                auto_healed: all_healed,
                diagnostics: json_diags,
            };
            let out = serde_json::to_string_pretty(&report)
                .map_err(|e| format!("JSON serialization error: {e}"))?;
            println!("{out}");
        }
    }

    Ok(has_blocking)
}

fn normalize_rel(p: &str) -> String {
    p.replace('\\', "/").trim_start_matches("./").to_string()
}
