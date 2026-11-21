use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::manifest::{
    AbManifest, CURRENT_SCHEMA_VERSION, CaseKind, PerfConfig, RepoConfig, RifftyConfig, TestConfig,
    TestParserKind,
};
use crate::vendored::{VendoredMatch, classify_vendored_or_generated};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawDiagnostic {
    pub file: String,
    pub line: u32,
    pub rule_id: String,
    pub severity: String,
    pub message: String,
    #[serde(default)]
    pub applicability: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanCandidate {
    pub file: String,
    pub line: u32,
    pub rule_id: String,
    pub severity: String,
    pub applicability: Option<String>,
    pub message: String,
    pub score: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedTestRunner {
    pub ecosystem: String,
    pub command: String,
    pub parser: TestParserKind,
    pub report_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanReport {
    pub repo_url: String,
    pub commit_sha: String,
    pub tree_sha: String,
    pub commits_scanned: usize,
    pub total_diagnostics: usize,
    pub filtered_vendored: Vec<VendoredMatch>,
    pub candidates: Vec<ScanCandidate>,
    pub detected_runner: DetectedTestRunner,
}

/// Scores and ranks raw `riffty` diagnostics, filtering vendored/generated files by default.
pub fn rank_diagnostics<F>(
    diagnostics: &[RawDiagnostic],
    include_vendored: bool,
    mut read_file_content: F,
) -> (Vec<ScanCandidate>, Vec<VendoredMatch>)
where
    F: FnMut(&str) -> Option<String>,
{
    let mut candidates = Vec::new();
    let mut filtered_vendored = Vec::new();
    let mut seen_vendored = std::collections::BTreeSet::new();

    for d in diagnostics {
        let content = read_file_content(&d.file);
        if let Some(vm) = classify_vendored_or_generated(&d.file, content.as_deref()) {
            if seen_vendored.insert(vm.path.clone()) {
                filtered_vendored.push(vm);
            }
            if !include_vendored {
                continue;
            }
        }

        let mut score = 0u32;
        if d.applicability.as_deref() == Some("Safe") {
            score += 100;
        } else if d.applicability.as_deref() == Some("Unsafe") {
            score += 40;
        } else {
            score += 10;
        }

        match d.severity.to_ascii_uppercase().as_str() {
            "ERROR" => score += 50,
            "WARNING" => score += 25,
            _ => score += 5,
        }

        candidates.push(ScanCandidate {
            file: d.file.clone(),
            line: d.line,
            rule_id: d.rule_id.clone(),
            severity: d.severity.clone(),
            applicability: d.applicability.clone(),
            message: d.message.clone(),
            score,
        });
    }

    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.rule_id.cmp(&b.rule_id))
    });

    (candidates, filtered_vendored)
}

/// Read-only test runner detection from root marker filenames.
pub fn detect_test_runner(root_entries: &[String]) -> DetectedTestRunner {
    let has = |name: &str| root_entries.iter().any(|e| e == name);

    if has("pom.xml") {
        return DetectedTestRunner {
            ecosystem: "java-maven".to_string(),
            command: "mvn test".to_string(),
            parser: TestParserKind::JunitXml,
            report_path: Some("target/surefire-reports/TEST-*.xml".to_string()),
        };
    }
    if has("build.gradle") || has("build.gradle.kts") {
        return DetectedTestRunner {
            ecosystem: "java-gradle".to_string(),
            command: "./gradlew test".to_string(),
            parser: TestParserKind::JunitXml,
            report_path: Some("build/test-results/test/TEST-*.xml".to_string()),
        };
    }
    if has("pyproject.toml") || has("pytest.ini") || has("setup.py") || has("requirements.txt") {
        return DetectedTestRunner {
            ecosystem: "python-pytest".to_string(),
            command: "pytest -q --junitxml=junit.xml".to_string(),
            parser: TestParserKind::JunitXml,
            report_path: Some("junit.xml".to_string()),
        };
    }
    if has("package.json") {
        return DetectedTestRunner {
            ecosystem: "jsts-node".to_string(),
            command: "npm test".to_string(),
            parser: TestParserKind::ExitCode,
            report_path: None,
        };
    }

    DetectedTestRunner {
        ecosystem: "generic".to_string(),
        command: "true".to_string(),
        parser: TestParserKind::ExitCode,
        report_path: None,
    }
}

/// Generates a declarative `ab-case.toml` manifest from a `ScanReport`.
pub fn build_manifest_from_scan(
    scan: &ScanReport,
    case_id: &str,
    rules: Option<String>,
    require_tier2: bool,
    max_targets: usize,
) -> AbManifest {
    let mut target_paths = Vec::new();
    for c in &scan.candidates {
        if !target_paths.contains(&c.file) {
            target_paths.push(c.file.clone());
            if target_paths.len() >= max_targets.max(1) {
                break;
            }
        }
    }
    if target_paths.is_empty() {
        target_paths.push(".".to_string());
    }

    AbManifest {
        schema_version: CURRENT_SCHEMA_VERSION,
        case_id: case_id.to_string(),
        case_kind: CaseKind::FinopsOrCleanup,
        description: Some(format!(
            "Auto-discovered by riffty-ab scan at {} ({} candidate finding(s))",
            &scan.commit_sha[..8.min(scan.commit_sha.len())],
            scan.candidates.len()
        )),
        repo: RepoConfig {
            url: scan.repo_url.clone(),
            commit_sha: scan.commit_sha.clone(),
            tree_sha: Some(scan.tree_sha.clone()),
            sparse_paths: Vec::new(),
            allow_dirty_baseline: false,
        },
        riffty: RifftyConfig {
            target_paths,
            rules,
            unsafe_fixes: false,
            require_tier2,
            extra_args: Vec::new(),
        },
        test: TestConfig {
            setup_cmd: None,
            command: scan.detected_runner.command.clone(),
            parser: scan.detected_runner.parser,
            report_path: scan.detected_runner.report_path.clone(),
            timeout_secs: 300,
            env: BTreeMap::new(),
        },
        perf: PerfConfig::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_vendored_and_ranks_safe_fixes_first() {
        let diags = vec![
            RawDiagnostic {
                file: "vendor/lib/a.py".to_string(),
                line: 10,
                rule_id: "RFT-118".to_string(),
                severity: "ERROR".to_string(),
                message: "vendored".to_string(),
                applicability: Some("Safe".to_string()),
            },
            RawDiagnostic {
                file: "src/manual.ts".to_string(),
                line: 5,
                rule_id: "JSTS-PROMISE-EXEC-001".to_string(),
                severity: "ERROR".to_string(),
                message: "manual".to_string(),
                applicability: None,
            },
            RawDiagnostic {
                file: "src/fixable.ts".to_string(),
                line: 20,
                rule_id: "JSTS-NAN-EQ-001".to_string(),
                severity: "WARNING".to_string(),
                message: "safe fix".to_string(),
                applicability: Some("Safe".to_string()),
            },
        ];

        let (ranked, vendored) = rank_diagnostics(&diags, false, |_| None);
        assert_eq!(vendored.len(), 1);
        assert_eq!(vendored[0].path, "vendor/lib/a.py");
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].file, "src/fixable.ts");
        assert_eq!(ranked[1].file, "src/manual.ts");
    }

    #[test]
    fn detects_test_runners_from_root_markers() {
        assert_eq!(
            detect_test_runner(&["pyproject.toml".to_string()]).ecosystem,
            "python-pytest"
        );
        assert_eq!(
            detect_test_runner(&["pom.xml".to_string()]).ecosystem,
            "java-maven"
        );
        assert_eq!(
            detect_test_runner(&["build.gradle.kts".to_string()]).ecosystem,
            "java-gradle"
        );
        assert_eq!(
            detect_test_runner(&["package.json".to_string()]).ecosystem,
            "jsts-node"
        );
    }
}
