use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::CaseKind;
use crate::parsers::{TestOutcome, TestRunSummary};

/// Machine-verifiable A/B empirical outcome verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    /// Clean upstream baseline attested, Variant A failed, deterministic `riffty --fix` applied & idempotent, Variant B passed with 0 regressions.
    VerifiedBugFix,
    /// Clean upstream baseline attested, Variant A passed, deterministic `riffty --fix` applied & idempotent, Variant B passed with 0 regressions.
    VerifiedFinopsOrCleanup,
    /// Synthetic/probe case (`case_kind = feature_or_security_probe`) succeeded — explicitly distinct from `VERIFIED_BUG_FIX`.
    VerifiedProbe,
    /// Variant B introduced a test regression, failed to fix a failing baseline, or failed 2nd-run idempotency.
    RejectedRegression,
    /// Baseline working tree was dirty, mismatched `HEAD^{tree}`, or contained unauthorized edits when `allow_dirty_baseline = false`.
    RejectedTampered,
    /// `riffty check --fix` produced 0 edits (empty diff).
    NoOp,
}

impl Verdict {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::VerifiedBugFix
            | Self::VerifiedFinopsOrCleanup
            | Self::VerifiedProbe
            | Self::NoOp => 0,
            Self::RejectedRegression => 1,
            Self::RejectedTampered => 3,
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::VerifiedBugFix => "VERIFIED_BUG_FIX",
            Self::VerifiedFinopsOrCleanup => "VERIFIED_FINOPS_OR_CLEANUP",
            Self::VerifiedProbe => "VERIFIED_PROBE",
            Self::RejectedRegression => "REJECTED_REGRESSION",
            Self::RejectedTampered => "REJECTED_TAMPERED",
            Self::NoOp => "NO_OP",
        };
        write!(f, "{s}")
    }
}

/// Cryptographic baseline attestation recorded from `git` prior to Variant A.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineAttestation {
    pub expected_commit_sha: String,
    pub actual_head_sha: String,
    pub expected_tree_sha: Option<String>,
    pub actual_tree_sha: String,
    pub working_tree_clean: bool,
    pub allow_dirty_baseline: bool,
    pub dirty_entries: Vec<String>,
    pub attested: bool,
    pub failure_reason: Option<String>,
}

impl BaselineAttestation {
    pub fn evaluate(
        expected_commit_sha: &str,
        actual_head_sha: &str,
        expected_tree_sha: Option<&str>,
        actual_tree_sha: &str,
        dirty_entries: Vec<String>,
        allow_dirty_baseline: bool,
    ) -> Self {
        let working_tree_clean = dirty_entries.is_empty();
        let mut reasons = Vec::new();

        if !actual_head_sha.eq_ignore_ascii_case(expected_commit_sha) {
            reasons.push(format!(
                "HEAD SHA mismatch: expected {expected_commit_sha}, got {actual_head_sha}"
            ));
        }

        if let Some(exp_tree) = expected_tree_sha
            && !actual_tree_sha.eq_ignore_ascii_case(exp_tree)
        {
            reasons.push(format!(
                "HEAD^{{tree}} SHA mismatch: expected {exp_tree}, got {actual_tree_sha}"
            ));
        }

        if !working_tree_clean && !allow_dirty_baseline {
            reasons.push(format!(
                "dirty baseline working tree ({} modified/untracked path(s)) while allow_dirty_baseline = false",
                dirty_entries.len()
            ));
        }

        let attested = reasons.is_empty();
        let failure_reason = if attested {
            None
        } else {
            Some(reasons.join("; "))
        };

        Self {
            expected_commit_sha: expected_commit_sha.to_string(),
            actual_head_sha: actual_head_sha.to_string(),
            expected_tree_sha: expected_tree_sha.map(str::to_string),
            actual_tree_sha: actual_tree_sha.to_string(),
            working_tree_clean,
            allow_dirty_baseline,
            dirty_entries,
            attested,
            failure_reason,
        }
    }
}

/// Deterministic `riffty check --fix` treatment record & 2nd-run idempotency proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreatmentSummary {
    pub riffty_version: String,
    pub baseline_diagnostics_count: usize,
    pub post_fix_diagnostics_count: usize,
    pub auto_healed_count: usize,
    pub pass2_auto_healed_count: usize,
    pub idempotent: bool,
    pub patch_sha256: String,
    pub files_modified: Vec<String>,
    pub unified_diff: String,
    pub provenance: serde_json::Value,
}

impl TreatmentSummary {
    pub fn has_edits(&self) -> bool {
        !self.unified_diff.trim().is_empty()
    }
}

/// Deterministic test comparison between Variant A (Control) and Variant B (Treatment).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestComparison {
    pub variant_a_passed: bool,
    pub variant_b_passed: bool,
    pub fixed_tests: Vec<String>,
    pub regressed_tests: Vec<String>,
}

pub fn compute_sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

pub fn compare_test_runs(variant_a: &TestRunSummary, variant_b: &TestRunSummary) -> TestComparison {
    let mut fixed_tests = Vec::new();
    let mut regressed_tests = Vec::new();

    for (name, outcome_a) in &variant_a.cases {
        if let Some(outcome_b) = variant_b.cases.get(name) {
            if outcome_a.is_failure() && *outcome_b == TestOutcome::Pass {
                fixed_tests.push(name.clone());
            } else if *outcome_a == TestOutcome::Pass && outcome_b.is_failure() {
                regressed_tests.push(name.clone());
            }
        }
    }

    // Any brand-new failing test appearing in Variant B is also a regression.
    for (name, outcome_b) in &variant_b.cases {
        if !variant_a.cases.contains_key(name) && outcome_b.is_failure() {
            regressed_tests.push(name.clone());
        }
    }

    TestComparison {
        variant_a_passed: variant_a.passed_overall(),
        variant_b_passed: variant_b.passed_overall(),
        fixed_tests,
        regressed_tests,
    }
}

/// Pure, deterministic verdict state machine.
pub fn evaluate_verdict(
    case_kind: CaseKind,
    attestation: &BaselineAttestation,
    treatment: Option<&TreatmentSummary>,
    variant_a: Option<&TestRunSummary>,
    variant_b: Option<&TestRunSummary>,
) -> (Verdict, Option<TestComparison>, String) {
    if !attestation.attested {
        return (
            Verdict::RejectedTampered,
            None,
            attestation
                .failure_reason
                .clone()
                .unwrap_or_else(|| "baseline attestation failed".to_string()),
        );
    }

    let Some(treat) = treatment else {
        return (Verdict::NoOp, None, "no treatment executed".to_string());
    };

    if !treat.has_edits() {
        return (
            Verdict::NoOp,
            None,
            "riffty check --fix produced 0 edits (empty unified diff)".to_string(),
        );
    }

    if !treat.idempotent || treat.pass2_auto_healed_count > 0 {
        return (
            Verdict::RejectedRegression,
            None,
            format!(
                "non-idempotent treatment: 2nd riffty --fix pass still modified code (pass2_auto_healed_count={})",
                treat.pass2_auto_healed_count
            ),
        );
    }

    let (Some(a), Some(b)) = (variant_a, variant_b) else {
        return (
            Verdict::RejectedRegression,
            None,
            "missing Variant A or Variant B test results".to_string(),
        );
    };

    let cmp = compare_test_runs(a, b);

    if !cmp.regressed_tests.is_empty() {
        return (
            Verdict::RejectedRegression,
            Some(cmp.clone()),
            format!(
                "test-level regression detected in {} test(s): {}",
                cmp.regressed_tests.len(),
                cmp.regressed_tests.join(", ")
            ),
        );
    }

    if cmp.variant_a_passed && !cmp.variant_b_passed {
        return (
            Verdict::RejectedRegression,
            Some(cmp),
            "Variant A passed but Variant B failed after riffty check --fix".to_string(),
        );
    }

    if !cmp.variant_a_passed && !cmp.variant_b_passed {
        return (
            Verdict::RejectedRegression,
            Some(cmp),
            "Variant A failed and Variant B still failed after riffty check --fix".to_string(),
        );
    }

    if !cmp.variant_a_passed && cmp.variant_b_passed {
        if case_kind == CaseKind::FeatureOrSecurityProbe {
            return (
                Verdict::VerifiedProbe,
                Some(cmp),
                "synthetic probe transitioned FAIL -> PASS (capped at VERIFIED_PROBE)".to_string(),
            );
        }
        return (
            Verdict::VerifiedBugFix,
            Some(cmp),
            "clean upstream baseline failed in Variant A, deterministic riffty --fix applied & idempotent, Variant B passed with 0 regressions".to_string(),
        );
    }

    // Both Variant A and Variant B passed with 0 regressions and non-empty idempotent diff.
    if case_kind == CaseKind::FeatureOrSecurityProbe {
        return (
            Verdict::VerifiedProbe,
            Some(cmp),
            "synthetic probe remained PASS -> PASS with idempotent riffty fix".to_string(),
        );
    }

    (
        Verdict::VerifiedFinopsOrCleanup,
        Some(cmp),
        format!(
            "clean upstream baseline & Variant B both passed with 0 regressions; riffty deterministically healed {} fix(es) across {} file(s)",
            treat.auto_healed_count,
            treat.files_modified.len()
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::TestParserKind;

    const SHA_A: &str = "a13bdc05aa23c7fce32ad1bd7033d33f84158397";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const TREE_A: &str = "1111111111111111111111111111111111111111";
    const TREE_B: &str = "2222222222222222222222222222222222222222";

    fn clean_attestation() -> BaselineAttestation {
        BaselineAttestation::evaluate(SHA_A, SHA_A, Some(TREE_A), TREE_A, vec![], false)
    }

    fn sample_treatment(diff: &str, idempotent: bool) -> TreatmentSummary {
        TreatmentSummary {
            riffty_version: "riffty 0.2.0".to_string(),
            baseline_diagnostics_count: 1,
            post_fix_diagnostics_count: 0,
            auto_healed_count: usize::from(!diff.is_empty()),
            pass2_auto_healed_count: usize::from(!idempotent),
            idempotent,
            patch_sha256: compute_sha256_hex(diff.as_bytes()),
            files_modified: if diff.is_empty() {
                vec![]
            } else {
                vec!["src/app.py".to_string()]
            },
            unified_diff: diff.to_string(),
            provenance: serde_json::json!([]),
        }
    }

    fn summary_from_cases(cases: &[(&str, TestOutcome)], exit_code: i32) -> TestRunSummary {
        let mut map = BTreeMap::new();
        let mut passed = 0;
        let mut failed = 0;
        for (k, v) in cases {
            map.insert((*k).to_string(), *v);
            match v {
                TestOutcome::Pass => passed += 1,
                TestOutcome::Fail | TestOutcome::Error => failed += 1,
                TestOutcome::Skipped => {}
            }
        }
        TestRunSummary {
            parser: TestParserKind::JunitXml,
            exit_code: Some(exit_code),
            timed_out: false,
            duration_ms: 100,
            total: cases.len(),
            passed,
            failed,
            errors: 0,
            skipped: 0,
            cases: map,
            stdout_excerpt: String::new(),
            stderr_excerpt: String::new(),
        }
    }

    #[test]
    fn attests_clean_matching_commit_and_tree() {
        let att = clean_attestation();
        assert!(att.attested);
        assert!(att.failure_reason.is_none());
    }

    #[test]
    fn rejects_dirty_working_tree_by_default() {
        let att = BaselineAttestation::evaluate(
            SHA_A,
            SHA_A,
            None,
            TREE_A,
            vec![" M src/app.py".to_string()],
            false,
        );
        assert!(!att.attested);
        let (v, _, _) = evaluate_verdict(CaseKind::UpstreamBugVerification, &att, None, None, None);
        assert_eq!(v, Verdict::RejectedTampered);
        assert_eq!(v.exit_code(), 3);
    }

    #[test]
    fn rejects_tree_hash_and_commit_mismatch() {
        let att1 = BaselineAttestation::evaluate(SHA_A, SHA_A, Some(TREE_A), TREE_B, vec![], false);
        assert!(!att1.attested);
        let att2 = BaselineAttestation::evaluate(SHA_A, SHA_B, None, TREE_A, vec![], false);
        assert!(!att2.attested);
    }

    #[test]
    fn emits_verified_bug_fix_on_clean_fail_to_pass() {
        let att = clean_attestation();
        let treat = sample_treatment("--- a/src/app.py\n+++ b/src/app.py\n-a\n+b\n", true);
        let a = summary_from_cases(&[("t1", TestOutcome::Fail), ("t2", TestOutcome::Pass)], 1);
        let b = summary_from_cases(&[("t1", TestOutcome::Pass), ("t2", TestOutcome::Pass)], 0);

        let (v, cmp, _) = evaluate_verdict(
            CaseKind::UpstreamBugVerification,
            &att,
            Some(&treat),
            Some(&a),
            Some(&b),
        );
        assert_eq!(v, Verdict::VerifiedBugFix);
        assert_eq!(cmp.unwrap().fixed_tests, vec!["t1"]);
    }

    #[test]
    fn emits_verified_finops_or_cleanup_on_clean_pass_to_pass() {
        let att = clean_attestation();
        let treat = sample_treatment("--- a/src/app.ts\n+++ b/src/app.ts\n-old\n+new\n", true);
        let a = summary_from_cases(&[("t1", TestOutcome::Pass)], 0);
        let b = summary_from_cases(&[("t1", TestOutcome::Pass)], 0);

        let (v, _, _) = evaluate_verdict(
            CaseKind::FinopsOrCleanup,
            &att,
            Some(&treat),
            Some(&a),
            Some(&b),
        );
        assert_eq!(v, Verdict::VerifiedFinopsOrCleanup);
    }

    #[test]
    fn detects_test_level_regression_even_when_fail_count_drops() {
        let att = clean_attestation();
        let treat = sample_treatment("--- a/src/app.py\n+++ b/src/app.py\n-a\n+b\n", true);
        let a = summary_from_cases(
            &[
                ("t1", TestOutcome::Fail),
                ("t2", TestOutcome::Fail),
                ("t3", TestOutcome::Pass),
            ],
            1,
        );
        let b = summary_from_cases(
            &[
                ("t1", TestOutcome::Pass),
                ("t2", TestOutcome::Pass),
                ("t3", TestOutcome::Fail),
            ],
            1,
        );

        let (v, cmp, reason) = evaluate_verdict(
            CaseKind::UpstreamBugVerification,
            &att,
            Some(&treat),
            Some(&a),
            Some(&b),
        );
        assert_eq!(v, Verdict::RejectedRegression);
        assert_eq!(cmp.unwrap().regressed_tests, vec!["t3"]);
        assert!(reason.contains("t3"));
    }

    #[test]
    fn emits_no_op_when_diff_is_empty() {
        let att = clean_attestation();
        let treat = sample_treatment("", true);
        let (v, _, _) = evaluate_verdict(CaseKind::FinopsOrCleanup, &att, Some(&treat), None, None);
        assert_eq!(v, Verdict::NoOp);
    }

    #[test]
    fn rejects_non_idempotent_treatment() {
        let att = clean_attestation();
        let treat = sample_treatment("--- a/x\n+++ b/x\n-1\n+2\n", false);
        let a = summary_from_cases(&[("t1", TestOutcome::Pass)], 0);
        let b = summary_from_cases(&[("t1", TestOutcome::Pass)], 0);
        let (v, _, reason) = evaluate_verdict(
            CaseKind::FinopsOrCleanup,
            &att,
            Some(&treat),
            Some(&a),
            Some(&b),
        );
        assert_eq!(v, Verdict::RejectedRegression);
        assert!(reason.contains("non-idempotent"));
    }

    #[test]
    fn caps_synthetic_probe_at_verified_probe() {
        let att = BaselineAttestation::evaluate(
            SHA_A,
            SHA_A,
            None,
            TREE_A,
            vec![" M src/app.py".to_string()],
            true,
        );
        let treat = sample_treatment("--- a/src/app.py\n+++ b/src/app.py\n-a\n+b\n", true);
        let a = summary_from_cases(&[("t1", TestOutcome::Fail)], 1);
        let b = summary_from_cases(&[("t1", TestOutcome::Pass)], 0);
        let (v, _, _) = evaluate_verdict(
            CaseKind::FeatureOrSecurityProbe,
            &att,
            Some(&treat),
            Some(&a),
            Some(&b),
        );
        assert_eq!(v, Verdict::VerifiedProbe);
    }
}
