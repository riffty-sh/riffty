use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use riffty_ab_core::{AbManifest, Verdict, classify_vendored_or_generated};
use serde::Serialize;

mod git_ops;

#[derive(Debug, Parser)]
#[command(
    name = "riffty-ab",
    version,
    about = "Deterministic A/B empirical verification runner & read-only discovery CLI for riffty"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run a declarative A/B verification manifest (`ab-case.toml`)
    Run(RunArgs),
    /// Generate a starter `ab-case.toml` manifest
    Init(InitArgs),
    /// Cryptographically attest a git checkout against an `ab-case.toml` manifest (read-only)
    Attest(AttestArgs),
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// Path to declarative `ab-case.toml` manifest
    manifest: PathBuf,

    /// Explicit opt-in required to execute target repository setup/test/perf commands
    #[arg(long)]
    allow_exec: bool,

    /// Validate manifest and print execution plan without executing any commands
    #[arg(long)]
    plan_only: bool,

    /// Existing local checkout directory to attest/use (optional; otherwise clones into workdir)
    #[arg(long)]
    repo_dir: Option<PathBuf>,

    /// Output format (`text` or `json`)
    #[arg(long, default_value = "text")]
    format: String,
}

#[derive(Debug, clap::Args)]
struct InitArgs {
    /// Output path for the generated `ab-case.toml`
    #[arg(default_value = "ab-case.toml")]
    output: PathBuf,

    /// Case identifier
    #[arg(long, default_value = "sample-upstream-case")]
    case_id: String,

    /// Target Git repository URL
    #[arg(long, default_value = "https://github.com/org/repo")]
    repo_url: String,

    /// Pinned 40-char commit SHA
    #[arg(long, default_value = "0000000000000000000000000000000000000000")]
    commit_sha: String,
}

#[derive(Debug, clap::Args)]
struct AttestArgs {
    /// Path to `ab-case.toml` manifest
    manifest: PathBuf,

    /// Path to local git checkout to attest (or destination directory when `--clone` is set)
    #[arg(long)]
    repo_dir: PathBuf,

    /// Shallow-clone and checkout the pinned commit SHA into `--repo-dir` before attesting
    #[arg(long)]
    clone: bool,

    /// Output JSON attestation
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct PlanSummary {
    case_id: String,
    case_kind: String,
    repo_url: String,
    commit_sha: String,
    allow_dirty_baseline: bool,
    target_paths: Vec<String>,
    test_command: String,
    test_parser: String,
    perf_enabled: bool,
    vendored_target_warnings: Vec<riffty_ab_core::VendoredMatch>,
}

fn load_manifest(path: &PathBuf) -> Result<AbManifest, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    AbManifest::from_toml_str(&raw)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::Init(args) => match run_init(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("riffty-ab init error: {e}");
                ExitCode::from(2)
            }
        },
        Commands::Attest(args) => run_attest(args),
        Commands::Run(args) => run_ab_command(args),
    }
}

fn run_init(args: InitArgs) -> Result<(), String> {
    let template = AbManifest::sample_template(&args.case_id, &args.repo_url, &args.commit_sha);
    template.validate().map_err(|e| e.to_string())?;
    let body = template.to_toml_string()?;
    std::fs::write(&args.output, body)
        .map_err(|e| format!("failed to write {}: {e}", args.output.display()))?;
    println!("Wrote {}", args.output.display());
    Ok(())
}

fn run_attest(args: AttestArgs) -> ExitCode {
    let manifest = match load_manifest(&args.manifest) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("riffty-ab error: {e}");
            return ExitCode::from(2);
        }
    };

    if args.clone
        && let Err(e) = git_ops::clone_and_checkout_pinned(
            &manifest.repo.url,
            &manifest.repo.commit_sha,
            &args.repo_dir,
            &manifest.repo.sparse_paths,
        )
    {
        eprintln!("riffty-ab clone error: {e}");
        return ExitCode::from(2);
    }

    let att = match git_ops::attest_baseline(
        &args.repo_dir,
        &manifest.repo.commit_sha,
        manifest.repo.tree_sha.as_deref(),
        manifest.repo.allow_dirty_baseline,
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("riffty-ab attest error: {e}");
            return ExitCode::from(2);
        }
    };

    let verdict = if att.attested {
        None
    } else {
        Some(Verdict::RejectedTampered)
    };

    if args.json {
        let payload = serde_json::json!({
            "case_id": manifest.case_id,
            "attestation": att,
            "verdict": verdict,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else if att.attested {
        println!(
            "ATTESTED: HEAD={} TREE={} clean={}",
            att.actual_head_sha, att.actual_tree_sha, att.working_tree_clean
        );
    } else {
        eprintln!(
            "VERDICT: {} ({})",
            Verdict::RejectedTampered,
            att.failure_reason.as_deref().unwrap_or("attestation failed")
        );
    }

    if att.attested {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(Verdict::RejectedTampered.exit_code())
    }
}

fn run_ab_command(args: RunArgs) -> ExitCode {
    let manifest = match load_manifest(&args.manifest) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("riffty-ab error: {e}");
            return ExitCode::from(2);
        }
    };

    let vendored_target_warnings: Vec<_> = manifest
        .riffty
        .target_paths
        .iter()
        .filter_map(|p| {
            let content = args
                .repo_dir
                .as_ref()
                .and_then(|dir| std::fs::read_to_string(dir.join(p)).ok());
            classify_vendored_or_generated(p, content.as_deref())
        })
        .collect();

    if args.plan_only {
        let summary = PlanSummary {
            case_id: manifest.case_id.clone(),
            case_kind: manifest.case_kind.to_string(),
            repo_url: manifest.repo.url.clone(),
            commit_sha: manifest.repo.commit_sha.clone(),
            allow_dirty_baseline: manifest.repo.allow_dirty_baseline,
            target_paths: manifest.riffty.target_paths.clone(),
            test_command: manifest.test.command.clone(),
            test_parser: manifest.test.parser.to_string(),
            perf_enabled: manifest.perf.enabled,
            vendored_target_warnings,
        };
        if args.format == "json" {
            println!(
                "{}",
                serde_json::to_string_pretty(&summary).unwrap_or_default()
            );
        } else {
            println!("Plan validated for case '{}':", summary.case_id);
            println!("  kind:     {}", summary.case_kind);
            println!("  repo:     {} @ {}", summary.repo_url, summary.commit_sha);
            println!("  dirty_ok: {}", summary.allow_dirty_baseline);
            println!("  targets:  {}", summary.target_paths.join(", "));
            println!("  test:     {} ({})", summary.test_command, summary.test_parser);
            if !summary.vendored_target_warnings.is_empty() {
                for w in &summary.vendored_target_warnings {
                    println!("  WARNING (vendored/generated): {} ({})", w.path, w.reason);
                }
            }
        }
        return ExitCode::SUCCESS;
    }

    if !args.allow_exec {
        eprintln!(
            "riffty-ab security gate: refusing to execute manifest commands without --allow-exec (use --plan-only for dry validation)"
        );
        return ExitCode::from(4);
    }

    if let Some(ref repo_dir) = args.repo_dir {
        match git_ops::attest_baseline(
            repo_dir,
            &manifest.repo.commit_sha,
            manifest.repo.tree_sha.as_deref(),
            manifest.repo.allow_dirty_baseline,
        ) {
            Ok(att) if !att.attested => {
                eprintln!(
                    "VERDICT: {} ({})",
                    Verdict::RejectedTampered,
                    att.failure_reason.as_deref().unwrap_or("attestation failed")
                );
                return ExitCode::from(Verdict::RejectedTampered.exit_code());
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("riffty-ab attest error: {e}");
                return ExitCode::from(2);
            }
        }
    }

    eprintln!("riffty-ab: full A/B execution pipeline lands in Milestone 1.");
    ExitCode::from(2)
}
