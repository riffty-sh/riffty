use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use riffty_ab_core::{
    RawDiagnostic, RifftyConfig, ScanReport, build_manifest_from_scan, detect_test_runner,
    rank_diagnostics,
};

use crate::git_ops;
use crate::riffty_invoker;

#[derive(Debug, clap::Args)]
pub struct ScanArgs {
    /// Local git repository path OR remote git URL (when `--repo` is used)
    #[arg(default_value = ".")]
    target: String,

    /// Treat `target` as a remote Git URL and shallow-clone read-only into a temp directory
    #[arg(long)]
    repo: bool,

    /// Number of recent commits to scan (default 1 = HEAD)
    #[arg(long, default_value_t = 1)]
    commits: usize,

    /// Optional Refaster YAML ruleset file
    #[arg(long)]
    rules: Option<PathBuf>,

    /// Require Tier 2 semantic analysis
    #[arg(long)]
    require_tier2: bool,

    /// Include vendored/generated files in candidate results (filtered out by default)
    #[arg(long)]
    include_vendored: bool,

    /// Emit a ready-to-run `ab-case.toml` manifest pinned to the best commit/candidates
    #[arg(long)]
    emit_manifest: Option<PathBuf>,

    /// Case ID to embed when `--emit-manifest` is specified
    #[arg(long, default_value = "discovered-upstream-case")]
    case_id: String,

    /// Maximum number of target files to include in generated manifest
    #[arg(long, default_value_t = 3)]
    max_targets: usize,

    /// Path to `riffty` binary
    #[arg(long, env = "RIFFTY_BIN")]
    riffty_bin: Option<PathBuf>,

    /// Output format (`text` or `json`)
    #[arg(long, default_value = "text")]
    format: String,
}

/// Read-only discovery scanner (Zero-RCE: never executes target repository code).
pub fn run_scan(args: ScanArgs) -> Result<(), String> {
    let riffty_bin = riffty_invoker::resolve_riffty_binary(args.riffty_bin.as_deref())?;

    let (repo_dir, repo_url, owned_temp) = if args.repo || is_remote_url(&args.target) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let tmp =
            std::env::temp_dir().join(format!("riffty_ab_scan_{}_{}", std::process::id(), nanos));
        git_ops::clone_head_shallow(&args.target, &tmp, args.commits)?;
        (tmp, args.target.clone(), true)
    } else {
        let p = PathBuf::from(&args.target);
        let abs = p
            .canonicalize()
            .map_err(|e| format!("failed to resolve {}: {e}", p.display()))?;
        (abs.clone(), abs.display().to_string(), false)
    };

    let scan_res = scan_repo(&riffty_bin, &repo_dir, &repo_url, &args);

    if owned_temp {
        let _ = std::fs::remove_dir_all(&repo_dir);
    }

    let report = scan_res?;

    if let Some(ref manifest_out) = args.emit_manifest {
        let rules_str = args.rules.as_ref().map(|p| p.display().to_string());
        let manifest = build_manifest_from_scan(
            &report,
            &args.case_id,
            rules_str,
            args.require_tier2,
            args.max_targets,
        );
        manifest.validate().map_err(|e| e.to_string())?;
        let toml_str = manifest.to_toml_string()?;
        std::fs::write(manifest_out, toml_str)
            .map_err(|e| format!("failed to write {}: {e}", manifest_out.display()))?;
    }

    if args.format == "json" {
        let out = serde_json::to_string_pretty(&report)
            .map_err(|e| format!("failed to serialize scan JSON: {e}"))?;
        println!("{out}");
    } else {
        println!(
            "Scan complete: {} @ {} (tree={})",
            report.repo_url, report.commit_sha, report.tree_sha
        );
        println!(
            "  commits_scanned: {}, total_diagnostics: {}, filtered_vendored: {}, ranked_candidates: {}",
            report.commits_scanned,
            report.total_diagnostics,
            report.filtered_vendored.len(),
            report.candidates.len()
        );
        println!(
            "  detected_runner: {} (`{}` | parser={})",
            report.detected_runner.ecosystem,
            report.detected_runner.command,
            report.detected_runner.parser
        );
        for c in report.candidates.iter().take(15) {
            let app = c.applicability.as_deref().unwrap_or("ManualOnly");
            println!(
                "  [score={:>3}] {}:{} {} [{}|{}] {}",
                c.score, c.file, c.line, c.rule_id, c.severity, app, c.message
            );
        }
        if let Some(ref out_path) = args.emit_manifest {
            println!("  emitted manifest: {}", out_path.display());
        }
    }

    Ok(())
}

fn scan_repo(
    riffty_bin: &Path,
    repo_dir: &Path,
    repo_url: &str,
    args: &ScanArgs,
) -> Result<ScanReport, String> {
    let shas = git_ops::recent_commit_shas(repo_dir, args.commits)?;
    if shas.is_empty() {
        return Err("no commits found in target repository".to_string());
    }

    let riffty_cfg = RifftyConfig {
        target_paths: vec![".".to_string()],
        rules: args.rules.as_ref().map(|p| p.display().to_string()),
        unsafe_fixes: false,
        require_tier2: args.require_tier2,
        extra_args: Vec::new(),
    };

    let mut best: Option<ScanReport> = None;

    for (idx, sha) in shas.iter().enumerate() {
        let (scan_dir, temp_wt) = if idx == 0 {
            (repo_dir.to_path_buf(), None)
        } else {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let wt = std::env::temp_dir().join(format!(
                "riffty_ab_scan_wt_{}_{}_{}",
                std::process::id(),
                idx,
                nanos
            ));
            git_ops::create_detached_worktree(repo_dir, sha, &wt)?;
            (wt.clone(), Some(wt))
        };

        let one_res = (|| -> Result<ScanReport, String> {
            let (head_sha, tree_sha) = git_ops::rev_parse_head_and_tree(&scan_dir)?;
            let check = riffty_invoker::run_riffty_check(
                riffty_bin,
                &scan_dir,
                Path::new("."),
                &riffty_cfg,
                false,
            )?;

            let raw_diags: Vec<RawDiagnostic> = check
                .diagnostics
                .iter()
                .filter_map(|v| serde_json::from_value::<RawDiagnostic>(v.clone()).ok())
                .collect();

            let (candidates, filtered_vendored) =
                rank_diagnostics(&raw_diags, args.include_vendored, |rel| {
                    std::fs::read_to_string(scan_dir.join(rel)).ok()
                });

            let root_entries: Vec<String> = std::fs::read_dir(&scan_dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter_map(|e| e.file_name().into_string().ok())
                        .collect()
                })
                .unwrap_or_default();
            let detected_runner = detect_test_runner(&root_entries);

            Ok(ScanReport {
                repo_url: repo_url.to_string(),
                commit_sha: head_sha,
                tree_sha,
                commits_scanned: shas.len(),
                total_diagnostics: raw_diags.len(),
                filtered_vendored,
                candidates,
                detected_runner,
            })
        })();

        if let Some(wt) = temp_wt {
            git_ops::remove_worktree(repo_dir, &wt);
        }

        let candidate_report = one_res?;
        let top_score = candidate_report
            .candidates
            .first()
            .map(|c| c.score)
            .unwrap_or(0);
        let best_score = best
            .as_ref()
            .and_then(|b| b.candidates.first())
            .map(|c| c.score)
            .unwrap_or(0);
        if best.is_none() || top_score > best_score {
            best = Some(candidate_report);
        }
    }

    best.ok_or_else(|| "scan produced no report".to_string())
}

fn is_remote_url(s: &str) -> bool {
    s.starts_with("https://")
        || s.starts_with("http://")
        || s.starts_with("git@")
        || s.starts_with("ssh://")
}
