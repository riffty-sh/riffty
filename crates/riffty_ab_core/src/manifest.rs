use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CaseKind {
    #[default]
    UpstreamBugVerification,
    FinopsOrCleanup,
    FeatureOrSecurityProbe,
}

impl fmt::Display for CaseKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UpstreamBugVerification => write!(f, "upstream_bug_verification"),
            Self::FinopsOrCleanup => write!(f, "finops_or_cleanup"),
            Self::FeatureOrSecurityProbe => write!(f, "feature_or_security_probe"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TestParserKind {
    JunitXml,
    Tap,
    #[default]
    ExitCode,
}

impl fmt::Display for TestParserKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::JunitXml => write!(f, "junit_xml"),
            Self::Tap => write!(f, "tap"),
            Self::ExitCode => write!(f, "exit_code"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbManifest {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub case_id: String,
    #[serde(default)]
    pub case_kind: CaseKind,
    #[serde(default)]
    pub description: Option<String>,
    pub repo: RepoConfig,
    #[serde(default)]
    pub riffty: RifftyConfig,
    pub test: TestConfig,
    #[serde(default)]
    pub perf: PerfConfig,
}

fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    /// Remote Git URL or local path (when `local_path` is not used)
    pub url: String,
    /// Full 40-character hex commit SHA
    pub commit_sha: String,
    /// Optional expected `git rev-parse HEAD^{tree}` 40-char hex SHA for double attestation
    #[serde(default)]
    pub tree_sha: Option<String>,
    /// Optional sparse-checkout paths for large monorepos
    #[serde(default)]
    pub sparse_paths: Vec<String>,
    /// Hard-blocked (`false`) by default to prevent fake/staged local bug edits
    #[serde(default)]
    pub allow_dirty_baseline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RifftyConfig {
    /// Paths relative to repository root to analyze/fix (default: `["."]`)
    #[serde(default = "default_target_paths")]
    pub target_paths: Vec<String>,
    /// Optional Refaster YAML rule pack path (relative to manifest directory or absolute)
    #[serde(default)]
    pub rules: Option<String>,
    #[serde(default)]
    pub unsafe_fixes: bool,
    #[serde(default)]
    pub require_tier2: bool,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

fn default_target_paths() -> Vec<String> {
    vec![".".to_string()]
}

impl Default for RifftyConfig {
    fn default() -> Self {
        Self {
            target_paths: default_target_paths(),
            rules: None,
            unsafe_fixes: false,
            require_tier2: false,
            extra_args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestConfig {
    /// Optional one-time setup command run after clean checkout and before Variant A & B
    #[serde(default)]
    pub setup_cmd: Option<String>,
    /// Test command run in both Variant A and Variant B
    pub command: String,
    #[serde(default)]
    pub parser: TestParserKind,
    /// Relative path (or glob) to JUnit XML report(s) when `parser = "junit_xml"`
    #[serde(default)]
    pub report_path: Option<String>,
    #[serde(default = "default_test_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn default_test_timeout_secs() -> u64 {
    300
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerfConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub command: String,
    #[serde(default = "default_warmup_runs")]
    pub warmup_runs: usize,
    #[serde(default = "default_measured_pairs")]
    pub measured_pairs: usize,
    #[serde(default = "default_perf_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub metric_name: Option<String>,
    /// Optional substring prefix or simple regex capture for stdout metric extraction
    #[serde(default)]
    pub metric_regex: Option<String>,
    /// Optional JSON key for stdout metric extraction
    #[serde(default)]
    pub metric_json_key: Option<String>,
    #[serde(default)]
    pub higher_is_better: bool,
}

fn default_warmup_runs() -> usize {
    1
}

fn default_measured_pairs() -> usize {
    5
}

fn default_perf_timeout_secs() -> u64 {
    120
}

impl Default for PerfConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: String::new(),
            warmup_runs: default_warmup_runs(),
            measured_pairs: default_measured_pairs(),
            timeout_secs: default_perf_timeout_secs(),
            metric_name: None,
            metric_regex: None,
            metric_json_key: None,
            higher_is_better: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestValidationError {
    pub messages: Vec<String>,
}

impl fmt::Display for ManifestValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid ab-case.toml: {}", self.messages.join("; "))
    }
}

impl std::error::Error for ManifestValidationError {}

pub fn is_valid_git_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

impl AbManifest {
    pub fn from_toml_str(input: &str) -> Result<Self, String> {
        let manifest: Self =
            toml::from_str(input).map_err(|e| format!("failed to parse ab-case.toml: {e}"))?;
        manifest.validate().map_err(|e| e.to_string())?;
        Ok(manifest)
    }

    pub fn to_toml_string(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| format!("failed to serialize ab-case.toml: {e}"))
    }

    pub fn validate(&self) -> Result<(), ManifestValidationError> {
        let mut errors = Vec::new();

        if self.schema_version != CURRENT_SCHEMA_VERSION {
            errors.push(format!(
                "unsupported schema_version {} (expected {})",
                self.schema_version, CURRENT_SCHEMA_VERSION
            ));
        }

        if self.case_id.trim().is_empty() {
            errors.push("case_id must not be empty".to_string());
        } else if !self
            .case_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            errors.push(format!(
                "case_id '{}' must contain only ASCII alphanumeric, '-', '_', or '.'",
                self.case_id
            ));
        }

        if self.repo.url.trim().is_empty() {
            errors.push("repo.url must not be empty".to_string());
        }

        if !is_valid_git_sha(&self.repo.commit_sha) {
            errors.push(format!(
                "repo.commit_sha '{}' must be a full 40-character hex commit SHA",
                self.repo.commit_sha
            ));
        }

        if let Some(ref tree_sha) = self.repo.tree_sha
            && !is_valid_git_sha(tree_sha)
        {
            errors.push(format!(
                "repo.tree_sha '{tree_sha}' must be a full 40-character hex git tree SHA"
            ));
        }

        // Anti-Potemkin policy: upstream_bug_verification and finops_or_cleanup
        // MUST use an unmodified upstream tree (allow_dirty_baseline = false).
        if self.repo.allow_dirty_baseline
            && matches!(
                self.case_kind,
                CaseKind::UpstreamBugVerification | CaseKind::FinopsOrCleanup
            )
        {
            errors.push(format!(
                "allow_dirty_baseline = true is forbidden for case_kind = '{}' (use 'feature_or_security_probe' for synthetic probes)",
                self.case_kind
            ));
        }

        if self.riffty.target_paths.is_empty() {
            errors.push("riffty.target_paths must contain at least one path".to_string());
        }
        for p in &self.riffty.target_paths {
            if p.trim().is_empty() || p.starts_with('/') || p.split('/').any(|seg| seg == "..") {
                errors.push(format!(
                    "riffty.target_paths entry '{p}' must be a clean repo-relative path without '..'"
                ));
            }
        }

        if self.test.command.trim().is_empty() {
            errors.push("test.command must not be empty".to_string());
        }
        if self.test.timeout_secs == 0 {
            errors.push("test.timeout_secs must be > 0".to_string());
        }
        if self.test.parser == TestParserKind::JunitXml
            && self
                .test
                .report_path
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            errors.push("test.report_path is required when test.parser = 'junit_xml'".to_string());
        }

        if self.perf.enabled {
            if self.perf.command.trim().is_empty() {
                errors.push("perf.command must not be empty when perf.enabled = true".to_string());
            }
            if self.perf.measured_pairs == 0 {
                errors
                    .push("perf.measured_pairs must be >= 1 when perf.enabled = true".to_string());
            }
            if self.perf.timeout_secs == 0 {
                errors.push("perf.timeout_secs must be > 0 when perf.enabled = true".to_string());
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ManifestValidationError { messages: errors })
        }
    }

    pub fn sample_template(case_id: &str, repo_url: &str, commit_sha: &str) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            case_id: case_id.to_string(),
            case_kind: CaseKind::UpstreamBugVerification,
            description: Some("Empirical A/B verification case for riffty".to_string()),
            repo: RepoConfig {
                url: repo_url.to_string(),
                commit_sha: commit_sha.to_string(),
                tree_sha: None,
                sparse_paths: Vec::new(),
                allow_dirty_baseline: false,
            },
            riffty: RifftyConfig::default(),
            test: TestConfig {
                setup_cmd: None,
                command: "pytest -q --junitxml=junit.xml".to_string(),
                parser: TestParserKind::JunitXml,
                report_path: Some("junit.xml".to_string()),
                timeout_secs: 300,
                env: BTreeMap::new(),
            },
            perf: PerfConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_SHA: &str = "a13bdc05aa23c7fce32ad1bd7033d33f84158397";

    #[test]
    fn parses_valid_manifest_with_secure_defaults() {
        let toml_str = format!(
            r#"
schema_version = 1
case_id = "langchainjs-clone-001"
case_kind = "finops_or_cleanup"

[repo]
url = "https://github.com/langchain-ai/langchainjs"
commit_sha = "{VALID_SHA}"

[test]
command = "node --test --test-reporter=tap test.mjs"
parser = "tap"
"#
        );

        let m = AbManifest::from_toml_str(&toml_str).expect("manifest should parse");
        assert!(!m.repo.allow_dirty_baseline);
        assert_eq!(m.riffty.target_paths, vec!["."]);
        assert_eq!(m.test.timeout_secs, 300);
        assert!(!m.perf.enabled);
    }

    #[test]
    fn rejects_short_or_non_hex_commit_sha() {
        let toml_str = r#"
schema_version = 1
case_id = "bad-sha"

[repo]
url = "https://github.com/example/repo"
commit_sha = "abc1234"

[test]
command = "pytest"
"#;
        let err = AbManifest::from_toml_str(toml_str).unwrap_err();
        assert!(err.contains("40-character hex commit SHA"), "got: {err}");
    }

    #[test]
    fn rejects_dirty_baseline_for_upstream_bug_verification() {
        let toml_str = format!(
            r#"
schema_version = 1
case_id = "fake-bug-attempt"
case_kind = "upstream_bug_verification"

[repo]
url = "https://github.com/example/repo"
commit_sha = "{VALID_SHA}"
allow_dirty_baseline = true

[test]
command = "pytest"
"#
        );
        let err = AbManifest::from_toml_str(&toml_str).unwrap_err();
        assert!(
            err.contains("allow_dirty_baseline = true is forbidden"),
            "got: {err}"
        );
    }

    #[test]
    fn allows_dirty_baseline_only_for_explicit_probe_kind() {
        let toml_str = format!(
            r#"
schema_version = 1
case_id = "synthetic-probe"
case_kind = "feature_or_security_probe"

[repo]
url = "https://github.com/example/repo"
commit_sha = "{VALID_SHA}"
allow_dirty_baseline = true

[test]
command = "pytest"
"#
        );
        let m = AbManifest::from_toml_str(&toml_str).expect("probe manifest should be allowed");
        assert_eq!(m.case_kind, CaseKind::FeatureOrSecurityProbe);
        assert!(m.repo.allow_dirty_baseline);
    }

    #[test]
    fn requires_report_path_for_junit_xml_parser() {
        let toml_str = format!(
            r#"
schema_version = 1
case_id = "junit-missing-path"

[repo]
url = "https://github.com/example/repo"
commit_sha = "{VALID_SHA}"

[test]
command = "pytest"
parser = "junit_xml"
"#
        );
        let err = AbManifest::from_toml_str(&toml_str).unwrap_err();
        assert!(err.contains("test.report_path is required"), "got: {err}");
    }

    #[test]
    fn rejects_path_traversal_in_target_paths() {
        let toml_str = format!(
            r#"
schema_version = 1
case_id = "traversal-check"

[repo]
url = "https://github.com/example/repo"
commit_sha = "{VALID_SHA}"

[riffty]
target_paths = ["../escape"]

[test]
command = "pytest"
"#
        );
        let err = AbManifest::from_toml_str(&toml_str).unwrap_err();
        assert!(err.contains("without '..'"), "got: {err}");
    }
}
