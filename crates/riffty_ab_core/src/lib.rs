pub mod manifest;
pub mod parsers;
pub mod report;
pub mod vendored;
pub mod verdict;

pub use manifest::{
    AbManifest, CURRENT_SCHEMA_VERSION, CaseKind, PerfConfig, RepoConfig, RifftyConfig, TestConfig,
    TestParserKind,
};
pub use parsers::{TestOutcome, TestRunSummary};
pub use report::{AbReport, render_ab_report_md, render_pr_for_review_md};
pub use vendored::{VendoredMatch, classify_vendored_or_generated};
pub use verdict::{
    BaselineAttestation, TestComparison, TreatmentSummary, Verdict, compare_test_runs,
    compute_sha256_hex, evaluate_verdict,
};
