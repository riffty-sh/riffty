use std::fmt;

use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests {
    use super::*;

    const SHA_A: &str = "a13bdc05aa23c7fce32ad1bd7033d33f84158397";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const TREE_A: &str = "1111111111111111111111111111111111111111";
    const TREE_B: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn attests_clean_matching_commit_and_tree() {
        let att = BaselineAttestation::evaluate(
            SHA_A,
            SHA_A,
            Some(TREE_A),
            TREE_A,
            vec![],
            false,
        );
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
        assert!(att
            .failure_reason
            .as_deref()
            .unwrap()
            .contains("dirty baseline"));
    }

    #[test]
    fn rejects_tree_hash_mismatch() {
        let att = BaselineAttestation::evaluate(
            SHA_A,
            SHA_A,
            Some(TREE_A),
            TREE_B,
            vec![],
            false,
        );
        assert!(!att.attested);
        assert!(att
            .failure_reason
            .as_deref()
            .unwrap()
            .contains("HEAD^{tree} SHA mismatch"));
    }

    #[test]
    fn rejects_commit_sha_mismatch() {
        let att = BaselineAttestation::evaluate(SHA_A, SHA_B, None, TREE_A, vec![], false);
        assert!(!att.attested);
        assert!(att
            .failure_reason
            .as_deref()
            .unwrap()
            .contains("HEAD SHA mismatch"));
    }
}
