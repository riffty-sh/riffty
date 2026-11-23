use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use riffty_ab_core::parsers::{junit::build_junit_summary, tap::build_tap_summary};
use riffty_ab_core::{
    AbManifest, AbReport, PerfComparison, TestParserKind, TestRunSummary, TreatmentSummary,
    Verdict, classify_vendored_or_generated, compute_perf_comparison, compute_sha256_hex,
    evaluate_verdict, extract_metric_value, render_ab_report_md, render_pr_for_review_md,
};

use crate::executor::{SandboxMode, run_sandboxed_command};
use crate::git_ops;
use crate::riffty_invoker;

pub struct AbRunOptions {
    pub manifest_path: PathBuf,
    pub repo_dir: Option<PathBuf>,
    pub out_dir: Option<PathBuf>,
    pub riffty_bin: Option<PathBuf>,
    pub sandbox: SandboxMode,
    pub emit_pr_review: bool,
}

pub fn execute_ab_run(manifest: &AbManifest, opts: &AbRunOptions) -> Result<AbReport, String> {
    let manifest_dir = opts
        .manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."));

    // 1. Acquire checkout (either existing --repo-dir or fresh temp clone)
    let (repo_dir, owned_temp) = match &opts.repo_dir {
        Some(existing) => (existing.clone(), false),
        None => {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let tmp = std::env::temp_dir().join(format!(
                "riffty_ab_work_{}_{}_{}",
                manifest.case_id,
                std::process::id(),
                nanos
            ));
            git_ops::clone_and_checkout_pinned(
                &manifest.repo.url,
                &manifest.repo.commit_sha,
                &tmp,
                &manifest.repo.sparse_paths,
            )?;
            (tmp, true)
        }
    };

    let result = execute_in_repo(manifest, manifest_dir, &repo_dir, opts);

    if owned_temp {
        let _ = std::fs::remove_dir_all(&repo_dir);
    }

    let report = result?;

    // Persist artifacts if --out-dir was provided
    if let Some(ref out_dir) = opts.out_dir {
        write_artifacts(out_dir, &report, opts.emit_pr_review)?;
    }

    Ok(report)
}

fn execute_in_repo(
    manifest: &AbManifest,
    manifest_dir: &Path,
    repo_dir: &Path,
    opts: &AbRunOptions,
) -> Result<AbReport, String> {
    // 2. Cryptographic Baseline Attestation
    let attestation = git_ops::attest_baseline(
        repo_dir,
        &manifest.repo.commit_sha,
        manifest.repo.tree_sha.as_deref(),
        manifest.repo.allow_dirty_baseline,
    )?;

    if !attestation.attested {
        let (verdict, cmp, reason) =
            evaluate_verdict(manifest.case_kind, &attestation, None, None, None);
        return Ok(AbReport::new(
            manifest.case_id.clone(),
            manifest.case_kind,
            manifest.repo.url.clone(),
            attestation,
            None,
            None,
            None,
            cmp,
            None,
            vec![],
            verdict,
            reason,
        ));
    }

    // 3. Optional one-time setup command (after clean tree attestation)
    if let Some(ref setup_cmd) = manifest.test.setup_cmd
        && !setup_cmd.trim().is_empty()
    {
        let setup_res = run_sandboxed_command(
            repo_dir,
            setup_cmd,
            manifest.test.timeout_secs,
            &manifest.test.env,
            &opts.sandbox,
        )?;
        if setup_res.timed_out || setup_res.exit_code != Some(0) {
            return Err(format!(
                "test.setup_cmd failed (exit={:?}, timed_out={}): {}",
                setup_res.exit_code,
                setup_res.timed_out,
                setup_res.stderr.trim()
            ));
        }
    }

    // 4. Locate riffty & run Variant A baseline static check + baseline tests
    let riffty_bin = riffty_invoker::resolve_riffty_binary(opts.riffty_bin.as_deref())?;
    let riffty_ver = riffty_invoker::riffty_version(&riffty_bin)?;

    let baseline_check = riffty_invoker::run_riffty_check(
        &riffty_bin,
        repo_dir,
        manifest_dir,
        &manifest.riffty,
        false,
    )?;

    let variant_a = run_test_suite(repo_dir, manifest, &opts.sandbox)?;

    // 5. Apply deterministic treatment (riffty check --fix) + capture diff + 2nd-run idempotency
    let fix_pass1 = riffty_invoker::run_riffty_check(
        &riffty_bin,
        repo_dir,
        manifest_dir,
        &manifest.riffty,
        true,
    )?;

    let unified_diff = git_ops::git_diff_unified(repo_dir)?;
    let patch_sha256 = compute_sha256_hex(unified_diff.as_bytes());
    let files_modified = git_ops::git_modified_files(repo_dir)?;

    // Inspect modified files for vendored/generated markers
    let vendored_warnings: Vec<_> = files_modified
        .iter()
        .filter_map(|rel| {
            let content = std::fs::read_to_string(repo_dir.join(rel)).ok();
            classify_vendored_or_generated(rel, content.as_deref())
        })
        .collect();

    // 2nd-run idempotency check
    let (pass2_auto_healed_count, idempotent) = if unified_diff.trim().is_empty() {
        (0, true)
    } else {
        let fix_pass2 = riffty_invoker::run_riffty_check(
            &riffty_bin,
            repo_dir,
            manifest_dir,
            &manifest.riffty,
            true,
        )?;
        let diff_after_pass2 = git_ops::git_diff_unified(repo_dir)?;
        let sha_after_pass2 = compute_sha256_hex(diff_after_pass2.as_bytes());
        let is_idem = fix_pass2.auto_healed_count == 0 && sha_after_pass2 == patch_sha256;
        (fix_pass2.auto_healed_count, is_idem)
    };

    let treatment = TreatmentSummary {
        riffty_version: riffty_ver,
        baseline_diagnostics_count: baseline_check.diagnostics_count,
        post_fix_diagnostics_count: fix_pass1.diagnostics_count,
        auto_healed_count: fix_pass1.auto_healed_count,
        pass2_auto_healed_count,
        idempotent,
        patch_sha256,
        files_modified,
        unified_diff,
        provenance: fix_pass1.provenance,
    };

    // Short-circuit Variant B if NO_OP (empty diff)
    if !treatment.has_edits() {
        let (verdict, cmp, reason) = evaluate_verdict(
            manifest.case_kind,
            &attestation,
            Some(&treatment),
            Some(&variant_a),
            None,
        );
        return Ok(AbReport::new(
            manifest.case_id.clone(),
            manifest.case_kind,
            manifest.repo.url.clone(),
            attestation,
            Some(treatment),
            Some(variant_a),
            None,
            cmp,
            None,
            vendored_warnings,
            verdict,
            reason,
        ));
    }

    // 6. Run Variant B (Treatment/Healed)
    let variant_b = run_test_suite(repo_dir, manifest, &opts.sandbox)?;

    // 7. Deterministic comparison & verdict
    let (verdict, comparison, verdict_reason) = evaluate_verdict(
        manifest.case_kind,
        &attestation,
        Some(&treatment),
        Some(&variant_a),
        Some(&variant_b),
    );

    // 8. Optional Interleaved A/B/A/B Performance & FinOps Benchmark using sibling git worktrees
    let perf = if manifest.perf.enabled
        && matches!(
            verdict,
            Verdict::VerifiedBugFix | Verdict::VerifiedFinopsOrCleanup | Verdict::VerifiedProbe
        ) {
        Some(run_interleaved_perf(
            repo_dir,
            manifest,
            &treatment,
            &opts.sandbox,
        )?)
    } else {
        None
    };

    Ok(AbReport::new(
        manifest.case_id.clone(),
        manifest.case_kind,
        manifest.repo.url.clone(),
        attestation,
        Some(treatment),
        Some(variant_a),
        Some(variant_b),
        comparison,
        perf,
        vendored_warnings,
        verdict,
        verdict_reason,
    ))
}

fn run_interleaved_perf(
    repo_dir: &Path,
    manifest: &AbManifest,
    treatment: &TreatmentSummary,
    sandbox: &SandboxMode,
) -> Result<PerfComparison, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let wt_base = std::env::temp_dir().join(format!(
        "riffty_ab_perf_{}_{}_{}",
        manifest.case_id,
        std::process::id(),
        nanos
    ));
    let wt_a = wt_base.join("worktree_a");
    let wt_b = wt_base.join("worktree_b");
    std::fs::create_dir_all(&wt_base)
        .map_err(|e| format!("failed to create perf worktree base: {e}"))?;

    let outcome = (|| -> Result<PerfComparison, String> {
        git_ops::create_detached_worktree(repo_dir, &manifest.repo.commit_sha, &wt_a)?;
        git_ops::create_detached_worktree(repo_dir, &manifest.repo.commit_sha, &wt_b)?;

        // Apply the exact attested patch into worktree_b and verify SHA-256
        git_ops::apply_patch_str(&wt_b, &treatment.unified_diff)?;
        let wt_b_diff = git_ops::git_diff_unified(&wt_b)?;
        let wt_b_sha = compute_sha256_hex(wt_b_diff.as_bytes());
        if wt_b_sha != treatment.patch_sha256 {
            return Err(format!(
                "perf worktree_b patch_sha256 mismatch: expected {}, got {}",
                treatment.patch_sha256, wt_b_sha
            ));
        }

        // Run optional setup_cmd in both worktrees before timing
        if let Some(ref setup_cmd) = manifest.test.setup_cmd
            && !setup_cmd.trim().is_empty()
        {
            for wt in [&wt_a, &wt_b] {
                let res = run_sandboxed_command(
                    wt,
                    setup_cmd,
                    manifest.test.timeout_secs,
                    &manifest.test.env,
                    sandbox,
                )?;
                if res.timed_out || res.exit_code != Some(0) {
                    return Err(format!(
                        "perf worktree setup_cmd failed in {}: {}",
                        wt.display(),
                        res.stderr.trim()
                    ));
                }
            }
        }

        let run_one = |wt: &Path| -> Result<f64, String> {
            let res = run_sandboxed_command(
                wt,
                &manifest.perf.command,
                manifest.perf.timeout_secs,
                &manifest.test.env,
                sandbox,
            )?;
            if res.timed_out || res.exit_code != Some(0) {
                return Err(format!(
                    "perf.command failed (exit={:?}, timed_out={}): {}",
                    res.exit_code,
                    res.timed_out,
                    res.stderr.trim()
                ));
            }
            extract_metric_value(
                &res.stdout,
                res.duration_ms,
                manifest.perf.metric_regex.as_deref(),
                manifest.perf.metric_json_key.as_deref(),
            )
        };

        // Warmup pairs (discarded)
        for _ in 0..manifest.perf.warmup_runs {
            let _ = run_one(&wt_a)?;
            let _ = run_one(&wt_b)?;
        }

        // Interleaved measured pairs with alternating A->B / B->A order to eliminate order bias
        let mut samples_a = Vec::with_capacity(manifest.perf.measured_pairs);
        let mut samples_b = Vec::with_capacity(manifest.perf.measured_pairs);
        for pair_idx in 0..manifest.perf.measured_pairs {
            if pair_idx % 2 == 0 {
                samples_a.push(run_one(&wt_a)?);
                samples_b.push(run_one(&wt_b)?);
            } else {
                samples_b.push(run_one(&wt_b)?);
                samples_a.push(run_one(&wt_a)?);
            }
        }

        let metric_name = manifest
            .perf
            .metric_name
            .clone()
            .unwrap_or_else(|| "duration_ms".to_string());

        compute_perf_comparison(
            metric_name,
            manifest.perf.higher_is_better,
            manifest.perf.warmup_runs,
            samples_a,
            samples_b,
        )
    })();

    git_ops::remove_worktree(repo_dir, &wt_a);
    git_ops::remove_worktree(repo_dir, &wt_b);
    let _ = std::fs::remove_dir_all(&wt_base);

    outcome
}

fn run_test_suite(
    repo_dir: &Path,
    manifest: &AbManifest,
    sandbox: &SandboxMode,
) -> Result<TestRunSummary, String> {
    if manifest.test.parser == TestParserKind::JunitXml
        && let Some(ref rel_pattern) = manifest.test.report_path
    {
        for existing in resolve_junit_files(repo_dir, rel_pattern) {
            let _ = std::fs::remove_file(existing);
        }
    }

    let exec = run_sandboxed_command(
        repo_dir,
        &manifest.test.command,
        manifest.test.timeout_secs,
        &manifest.test.env,
        sandbox,
    )?;

    match manifest.test.parser {
        TestParserKind::ExitCode => Ok(TestRunSummary::from_exit_code(
            exec.exit_code,
            exec.timed_out,
            exec.duration_ms,
            &exec.stdout,
            &exec.stderr,
        )),
        TestParserKind::Tap => Ok(build_tap_summary(
            exec.exit_code,
            exec.timed_out,
            exec.duration_ms,
            &exec.stdout,
            &exec.stderr,
        )),
        TestParserKind::JunitXml => {
            let rel_pattern =
                manifest.test.report_path.as_deref().ok_or_else(|| {
                    "test.report_path is required for junit_xml parser".to_string()
                })?;
            let files = resolve_junit_files(repo_dir, rel_pattern);
            if files.is_empty() {
                return Ok(TestRunSummary {
                    parser: TestParserKind::JunitXml,
                    exit_code: exec.exit_code,
                    timed_out: exec.timed_out,
                    duration_ms: exec.duration_ms,
                    total: 0,
                    passed: 0,
                    failed: usize::from(exec.exit_code != Some(0) || exec.timed_out),
                    errors: 0,
                    skipped: 0,
                    cases: Default::default(),
                    stdout_excerpt: riffty_ab_core::parsers::excerpt(&exec.stdout, 4000),
                    stderr_excerpt: riffty_ab_core::parsers::excerpt(&exec.stderr, 4000),
                });
            }
            let mut docs = Vec::with_capacity(files.len());
            for f in files {
                let text = std::fs::read_to_string(&f)
                    .map_err(|e| format!("failed to read JUnit XML {}: {e}", f.display()))?;
                docs.push(text);
            }
            build_junit_summary(
                &docs,
                exec.exit_code,
                exec.timed_out,
                exec.duration_ms,
                &exec.stdout,
                &exec.stderr,
            )
        }
    }
}

fn resolve_junit_files(repo_dir: &Path, rel_pattern: &str) -> Vec<PathBuf> {
    if !rel_pattern.contains('*') {
        let p = repo_dir.join(rel_pattern);
        return if p.is_file() { vec![p] } else { vec![] };
    }

    let pat_path = Path::new(rel_pattern);
    let parent_rel = pat_path.parent().unwrap_or_else(|| Path::new(""));
    let file_pat = pat_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("*.xml");
    let (prefix, suffix) = match file_pat.split_once('*') {
        Some((p, s)) => (p, s),
        None => (file_pat, ""),
    };

    let dir = repo_dir.join(parent_rel);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };

    let mut matched: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix) && n.ends_with(suffix))
        })
        .collect();
    matched.sort();
    matched
}

fn write_artifacts(out_dir: &Path, report: &AbReport, emit_pr_review: bool) -> Result<(), String> {
    std::fs::create_dir_all(out_dir)
        .map_err(|e| format!("failed to create out_dir {}: {e}", out_dir.display()))?;

    let json = report.to_json_pretty()?;
    std::fs::write(out_dir.join("ab_report.json"), json)
        .map_err(|e| format!("failed to write ab_report.json: {e}"))?;

    let md = render_ab_report_md(report);
    std::fs::write(out_dir.join("AB_REPORT.md"), md)
        .map_err(|e| format!("failed to write AB_REPORT.md: {e}"))?;

    if let Some(ref treat) = report.treatment
        && !treat.unified_diff.trim().is_empty()
    {
        std::fs::write(out_dir.join("treatment.patch"), &treat.unified_diff)
            .map_err(|e| format!("failed to write treatment.patch: {e}"))?;
    }

    if emit_pr_review && !matches!(report.verdict, Verdict::RejectedTampered | Verdict::NoOp) {
        let pr_md = render_pr_for_review_md(report);
        std::fs::write(out_dir.join("PR_FOR_REVIEW.md"), pr_md)
            .map_err(|e| format!("failed to write PR_FOR_REVIEW.md: {e}"))?;
    }

    Ok(())
}
