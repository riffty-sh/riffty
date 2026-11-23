use serde::{Deserialize, Serialize};

use crate::CaseKind;
use crate::manifest::CURRENT_SCHEMA_VERSION;
use crate::parsers::TestRunSummary;
use crate::stats::PerfComparison;
use crate::vendored::VendoredMatch;
use crate::verdict::{
    BaselineAttestation, TestComparison, TreatmentSummary, Verdict, compute_sha256_hex,
    evaluate_verdict,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AbReport {
    pub schema_version: u32,
    pub case_id: String,
    pub case_kind: CaseKind,
    pub repo_url: String,
    pub attestation: BaselineAttestation,
    pub treatment: Option<TreatmentSummary>,
    pub variant_a: Option<TestRunSummary>,
    pub variant_b: Option<TestRunSummary>,
    pub comparison: Option<TestComparison>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perf: Option<PerfComparison>,
    pub vendored_warnings: Vec<VendoredMatch>,
    pub verdict: Verdict,
    pub verdict_reason: String,
}

impl AbReport {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        case_id: String,
        case_kind: CaseKind,
        repo_url: String,
        attestation: BaselineAttestation,
        treatment: Option<TreatmentSummary>,
        variant_a: Option<TestRunSummary>,
        variant_b: Option<TestRunSummary>,
        comparison: Option<TestComparison>,
        perf: Option<PerfComparison>,
        vendored_warnings: Vec<VendoredMatch>,
        verdict: Verdict,
        verdict_reason: String,
    ) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            case_id,
            case_kind,
            repo_url,
            attestation,
            treatment,
            variant_a,
            variant_b,
            comparison,
            perf,
            vendored_warnings,
            verdict,
            verdict_reason,
        }
    }

    pub fn to_json_pretty(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| format!("failed to serialize ab_report.json: {e}"))
    }
}

/// Offline cryptographic & logical integrity verifier for `ab_report.json`.
/// Rejects hand-edited verdicts, forged diffs, or policy violations.
pub fn verify_report_integrity(report: &AbReport) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();

    if report.schema_version != CURRENT_SCHEMA_VERSION {
        errors.push(format!(
            "unsupported schema_version {} (expected {})",
            report.schema_version, CURRENT_SCHEMA_VERSION
        ));
    }

    if report.attestation.allow_dirty_baseline
        && matches!(
            report.case_kind,
            CaseKind::UpstreamBugVerification | CaseKind::FinopsOrCleanup
        )
    {
        errors.push(format!(
            "policy violation: allow_dirty_baseline=true is forbidden for case_kind={}",
            report.case_kind
        ));
    }

    if let Some(ref treat) = report.treatment {
        let actual_sha = compute_sha256_hex(treat.unified_diff.as_bytes());
        if actual_sha != treat.patch_sha256 {
            errors.push(format!(
                "patch_sha256 mismatch: report claims {}, actual SHA-256(unified_diff) is {}",
                treat.patch_sha256, actual_sha
            ));
        }
    }

    let (expected_verdict, _, _) = evaluate_verdict(
        report.case_kind,
        &report.attestation,
        report.treatment.as_ref(),
        report.variant_a.as_ref(),
        report.variant_b.as_ref(),
    );
    if expected_verdict != report.verdict {
        errors.push(format!(
            "verdict tampering detected: report claims {}, deterministic recomputation yields {}",
            report.verdict, expected_verdict
        ));
    }

    if let Some(ref perf) = report.perf
        && (perf.samples_a.len() != perf.measured_pairs
            || perf.samples_b.len() != perf.measured_pairs)
    {
        errors.push(format!(
            "perf sample length mismatch: measured_pairs={}, samples_a={}, samples_b={}",
            perf.measured_pairs,
            perf.samples_a.len(),
            perf.samples_b.len()
        ));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

pub fn render_ab_report_md(report: &AbReport) -> String {
    let mut md = String::new();
    md.push_str(&format!(
        "# `riffty-ab` Empirical Report — `{}`\n\n",
        report.case_id
    ));
    md.push_str(&format!("- **Verdict:** `{}`\n", report.verdict));
    md.push_str(&format!("- **Reason:** {}\n", report.verdict_reason));
    md.push_str(&format!("- **Case Kind:** `{}`\n", report.case_kind));
    md.push_str(&format!("- **Repository:** `{}`\n", report.repo_url));
    md.push_str(&format!(
        "- **Pinned Commit SHA:** `{}`\n",
        report.attestation.actual_head_sha
    ));
    md.push_str(&format!(
        "- **Attested `HEAD^{{tree}}` SHA:** `{}` (clean=`{}`, allow_dirty=`{}`)\n",
        report.attestation.actual_tree_sha,
        report.attestation.working_tree_clean,
        report.attestation.allow_dirty_baseline
    ));

    if let Some(ref treat) = report.treatment {
        md.push_str(&format!(
            "- **Riffty Version:** `{}`\n",
            treat.riffty_version
        ));
        md.push_str(&format!("- **Patch SHA-256:** `{}`\n", treat.patch_sha256));
        md.push_str(&format!(
            "- **2nd-Run Idempotent:** `{}` (pass1_healed={}, pass2_healed={})\n",
            treat.idempotent, treat.auto_healed_count, treat.pass2_auto_healed_count
        ));
    }

    if !report.vendored_warnings.is_empty() {
        md.push_str("\n## Vendored / Generated File Warnings\n\n");
        for w in &report.vendored_warnings {
            md.push_str(&format!("- `{}` — {}\n", w.path, w.reason));
        }
    }

    if let (Some(a), Some(b)) = (&report.variant_a, &report.variant_b) {
        md.push_str("\n## Variant A (Control) vs. Variant B (Treatment)\n\n");
        md.push_str("| Metric | Variant A (Baseline) | Variant B (Healed) |\n");
        md.push_str("| :--- | :---: | :---: |\n");
        md.push_str(&format!("| Parser | `{}` | `{}` |\n", a.parser, b.parser));
        md.push_str(&format!(
            "| Exit Code | `{:?}` | `{:?}` |\n",
            a.exit_code, b.exit_code
        ));
        md.push_str(&format!(
            "| Overall Pass | `{}` | `{}` |\n",
            a.passed_overall(),
            b.passed_overall()
        ));
        md.push_str(&format!("| Total Tests | {} | {} |\n", a.total, b.total));
        md.push_str(&format!("| Passed | {} | {} |\n", a.passed, b.passed));
        md.push_str(&format!(
            "| Failed / Errors | {} / {} | {} / {} |\n",
            a.failed, a.errors, b.failed, b.errors
        ));
        md.push_str(&format!("| Skipped | {} | {} |\n", a.skipped, b.skipped));
        md.push_str(&format!(
            "| Duration (ms) | {} | {} |\n",
            a.duration_ms, b.duration_ms
        ));
    }

    if let Some(ref perf) = report.perf {
        md.push_str("\n## Interleaved A/B/A/B Performance & FinOps Benchmark\n\n");
        md.push_str(&format!(
            "- **Metric:** `{}` (higher_is_better=`{}`, warmup_pairs={}, measured_pairs={})\n",
            perf.metric_name, perf.higher_is_better, perf.warmup_runs, perf.measured_pairs
        ));
        md.push_str(&format!(
            "- **Median Delta (B vs A):** `{:+.2}%` | **Paired Wins:** `{}/{}` | **Improved:** `{}`\n\n",
            perf.delta_pct, perf.paired_wins, perf.measured_pairs, perf.improved
        ));
        md.push_str("| Statistic | Variant A (Control) | Variant B (Treatment) |\n");
        md.push_str("| :--- | :---: | :---: |\n");
        md.push_str(&format!(
            "| Min | {:.3} | {:.3} |\n",
            perf.stats_a.min, perf.stats_b.min
        ));
        md.push_str(&format!(
            "| Median | {:.3} | {:.3} |\n",
            perf.stats_a.median, perf.stats_b.median
        ));
        md.push_str(&format!(
            "| Mean | {:.3} | {:.3} |\n",
            perf.stats_a.mean, perf.stats_b.mean
        ));
        md.push_str(&format!(
            "| p95 | {:.3} | {:.3} |\n",
            perf.stats_a.p95, perf.stats_b.p95
        ));
        md.push_str(&format!(
            "| Stddev | {:.3} | {:.3} |\n",
            perf.stats_a.stddev, perf.stats_b.stddev
        ));
    }

    if let Some(ref cmp) = report.comparison {
        if !cmp.fixed_tests.is_empty() {
            md.push_str("\n### Fixed Tests (`FAIL -> PASS`)\n\n");
            for t in &cmp.fixed_tests {
                md.push_str(&format!("- `{t}`\n"));
            }
        }
        if !cmp.regressed_tests.is_empty() {
            md.push_str("\n### Regressed Tests (`PASS -> FAIL`)\n\n");
            for t in &cmp.regressed_tests {
                md.push_str(&format!("- `{t}`\n"));
            }
        }
    }

    if let Some(ref treat) = report.treatment
        && !treat.unified_diff.trim().is_empty()
    {
        md.push_str("\n## Deterministic Unified Diff (`riffty check --fix`)\n\n");
        md.push_str("```diff\n");
        let diff_preview = if treat.unified_diff.len() > 8000 {
            let mut cutoff = 8000;
            while cutoff > 0 && !treat.unified_diff.is_char_boundary(cutoff) {
                cutoff -= 1;
            }
            format!(
                "{}\n... [truncated; see treatment.patch]\n",
                &treat.unified_diff[..cutoff]
            )
        } else {
            treat.unified_diff.clone()
        };
        md.push_str(&diff_preview);
        if !diff_preview.ends_with('\n') {
            md.push('\n');
        }
        md.push_str("```\n");
    }

    md.push_str("\n## Replication\n\n");
    md.push_str("```bash\n");
    md.push_str("riffty-ab run ab-case.toml --allow-exec --out-dir logs\n");
    md.push_str("riffty-ab verify-report logs/ab_report.json\n");
    md.push_str("```\n");

    md
}

pub fn render_pr_for_review_md(report: &AbReport) -> String {
    let mut md = String::new();
    md.push_str(&format!("# PR For Review — `{}`\n\n", report.case_id));
    md.push_str("> Human-gated upstream PR draft generated by `riffty-ab`. Review carefully before opening against upstream.\n\n");
    md.push_str("## Summary\n\n");
    md.push_str(&format!(
        "- **Upstream Repo:** `{}` @ `{}`\n",
        report.repo_url, report.attestation.actual_head_sha
    ));
    md.push_str(&format!("- **Empirical Verdict:** `{}`\n", report.verdict));
    md.push_str(&format!(
        "- **Verification Note:** {}\n",
        report.verdict_reason
    ));

    if let Some(ref perf) = report.perf {
        md.push_str(&format!(
            "- **Interleaved Perf (`{}`):** median `{:.3}` -> `{:.3}` (`{:+.2}%`, paired wins `{}/{}`)\n",
            perf.metric_name,
            perf.stats_a.median,
            perf.stats_b.median,
            perf.delta_pct,
            perf.paired_wins,
            perf.measured_pairs
        ));
    }

    if let Some(ref treat) = report.treatment {
        md.push_str(&format!(
            "- **Files Modified ({}):** {}\n",
            treat.files_modified.len(),
            treat
                .files_modified
                .iter()
                .map(|f| format!("`{f}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        md.push_str(&format!("- **Patch SHA-256:** `{}`\n", treat.patch_sha256));
        md.push_str(&format!(
            "- **Idempotent (2nd pass 0 edits):** `{}`\n\n",
            treat.idempotent
        ));
        if !treat.unified_diff.trim().is_empty() {
            md.push_str("## Patch\n\n```diff\n");
            md.push_str(&treat.unified_diff);
            if !treat.unified_diff.ends_with('\n') {
                md.push('\n');
            }
            md.push_str("```\n");
        }
    }

    md.push_str("\n## Empirical Verification Evidence\n\n");
    md.push_str("- Full structured report: [`ab_report.json`](./ab_report.json)\n");
    md.push_str("- Human summary: [`AB_REPORT.md`](./AB_REPORT.md)\n");
    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TestParserKind;
    use std::collections::BTreeMap;

    fn sample_report() -> AbReport {
        let att = BaselineAttestation::evaluate(
            "a13bdc05aa23c7fce32ad1bd7033d33f84158397",
            "a13bdc05aa23c7fce32ad1bd7033d33f84158397",
            None,
            "0b8bd50f432a71386bf975dc5e54e6e8df460e40",
            vec![],
            false,
        );
        let diff =
            "--- a/src/a.ts\n+++ b/src/a.ts\n-JSON.parse(JSON.stringify(x))\n+structuredClone(x)\n";
        let treat = TreatmentSummary {
            riffty_version: "riffty 0.2.0".to_string(),
            baseline_diagnostics_count: 1,
            post_fix_diagnostics_count: 0,
            auto_healed_count: 1,
            pass2_auto_healed_count: 0,
            idempotent: true,
            patch_sha256: compute_sha256_hex(diff.as_bytes()),
            files_modified: vec!["src/a.ts".to_string()],
            unified_diff: diff.to_string(),
            provenance: serde_json::json!([]),
        };
        let pass_run = TestRunSummary {
            parser: TestParserKind::ExitCode,
            exit_code: Some(0),
            timed_out: false,
            duration_ms: 50,
            total: 1,
            passed: 1,
            failed: 0,
            errors: 0,
            skipped: 0,
            cases: BTreeMap::new(),
            stdout_excerpt: String::new(),
            stderr_excerpt: String::new(),
        };
        AbReport::new(
            "demo-case".to_string(),
            CaseKind::FinopsOrCleanup,
            "https://github.com/org/repo".to_string(),
            att,
            Some(treat),
            Some(pass_run.clone()),
            Some(pass_run),
            None,
            None,
            vec![],
            Verdict::VerifiedFinopsOrCleanup,
            "all tests passed".to_string(),
        )
    }

    #[test]
    fn renders_json_markdown_and_pr_review_without_machine_local_links() {
        let report = sample_report();
        assert!(verify_report_integrity(&report).is_ok());
        let json = report.to_json_pretty().unwrap();
        assert!(json.contains("\"VERIFIED_FINOPS_OR_CLEANUP\""));
        let md = render_ab_report_md(&report);
        assert!(md.contains("VERIFIED_FINOPS_OR_CLEANUP"));
        assert!(!md.contains("file:///"));
        let pr = render_pr_for_review_md(&report);
        assert!(pr.contains("Human-gated upstream PR draft"));
        assert!(!pr.contains("file:///"));
    }

    #[test]
    fn verify_report_rejects_forged_verdict_and_patch_hash() {
        let mut forged = sample_report();
        forged.verdict = Verdict::VerifiedBugFix;
        let errs = verify_report_integrity(&forged).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("verdict tampering")));

        let mut forged_diff = sample_report();
        forged_diff
            .treatment
            .as_mut()
            .unwrap()
            .unified_diff
            .push_str("+hack\n");
        let errs2 = verify_report_integrity(&forged_diff).unwrap_err();
        assert!(errs2.iter().any(|e| e.contains("patch_sha256 mismatch")));
    }
}
