pub mod manifest;
pub mod parsers;
pub mod report;
pub mod scanner;
pub mod stats;
pub mod vendored;
pub mod verdict;

pub use manifest::{
    AbManifest, CURRENT_SCHEMA_VERSION, CaseKind, PerfConfig, RepoConfig, RifftyConfig, TestConfig,
    TestParserKind,
};
pub use parsers::{TestOutcome, TestRunSummary};
pub use report::{AbReport, render_ab_report_md, render_pr_for_review_md, verify_report_integrity};
pub use scanner::{
    DetectedTestRunner, RawDiagnostic, ScanCandidate, ScanReport, build_manifest_from_scan,
    detect_test_runner, rank_diagnostics,
};
pub use stats::{PerfComparison, SampleStats, compute_perf_comparison, extract_metric_value};
pub use vendored::{VendoredMatch, classify_vendored_or_generated};
pub use verdict::{
    BaselineAttestation, TestComparison, TreatmentSummary, Verdict, compare_test_runs,
    compute_sha256_hex, evaluate_verdict,
};
