use std::collections::BTreeMap;

use crate::TestParserKind;
use crate::parsers::{TestOutcome, TestRunSummary, excerpt};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedTap {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub cases: BTreeMap<String, TestOutcome>,
}

/// Parses TAP v13/v14 output (such as `node --test --test-reporter=tap`).
/// Top-level (non-indented) `ok` / `not ok` assertions are counted to avoid double-counting nested subtests.
pub fn parse_tap(output: &str) -> ParsedTap {
    let mut parsed = ParsedTap::default();

    for line in output.lines() {
        // Ignore indented subtests, YAML diagnostic blocks, comments, and plan lines.
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let trimmed = line.trim_end();
        let (is_ok, rest) = if let Some(r) = trimmed.strip_prefix("ok ") {
            (true, r)
        } else if trimmed == "ok" {
            (true, "")
        } else if let Some(r) = trimmed.strip_prefix("not ok ") {
            (false, r)
        } else if trimmed == "not ok" {
            (false, "")
        } else {
            continue;
        };

        // Strip optional test number and leading "- "
        let rest = rest.trim_start();
        let after_num = rest
            .split_once(char::is_whitespace)
            .and_then(|(first, tail)| {
                if first.bytes().all(|b| b.is_ascii_digit()) {
                    Some(tail.trim_start())
                } else {
                    None
                }
            })
            .unwrap_or(rest);
        let desc_and_dir = after_num.strip_prefix("- ").unwrap_or(after_num).trim();

        let (raw_name, directive) = match desc_and_dir.split_once('#') {
            Some((name, dir)) => (name.trim(), Some(dir.trim().to_ascii_uppercase())),
            None => (desc_and_dir, None),
        };

        let is_skip = directive
            .as_deref()
            .is_some_and(|d| d.starts_with("SKIP") || d.starts_with("TODO"));

        let case_name = if raw_name.is_empty() {
            format!("tap_case_{}", parsed.total + 1)
        } else {
            raw_name.to_string()
        };

        let outcome = if is_skip {
            parsed.skipped += 1;
            TestOutcome::Skipped
        } else if is_ok {
            parsed.passed += 1;
            TestOutcome::Pass
        } else {
            parsed.failed += 1;
            TestOutcome::Fail
        };

        parsed.total += 1;
        parsed.cases.insert(case_name, outcome);
    }

    parsed
}

pub fn build_tap_summary(
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u64,
    stdout: &str,
    stderr: &str,
) -> TestRunSummary {
    let parsed = parse_tap(stdout);
    TestRunSummary {
        parser: TestParserKind::Tap,
        exit_code,
        timed_out,
        duration_ms,
        total: parsed.total,
        passed: parsed.passed,
        failed: parsed.failed,
        errors: 0,
        skipped: parsed.skipped,
        cases: parsed.cases,
        stdout_excerpt: excerpt(stdout, 4000),
        stderr_excerpt: excerpt(stderr, 4000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_node_test_tap_v13_with_subtests_and_directives() {
        let tap = r#"TAP version 13
# Subtest: deepClone preserves Date objects
    ok 1 - nested assertion
    1..1
ok 1 - deepClone preserves Date objects
not ok 2 - deepClone handles circular ref
  ---
  duration_ms: 1.2
  failureType: 'testCodeFailure'
  ...
ok 3 - legacy mode # SKIP not enabled
1..3
"#;

        let parsed = parse_tap(tap);
        assert_eq!(parsed.total, 3);
        assert_eq!(parsed.passed, 1);
        assert_eq!(parsed.failed, 1);
        assert_eq!(parsed.skipped, 1);
        assert_eq!(
            parsed.cases.get("deepClone preserves Date objects"),
            Some(&TestOutcome::Pass)
        );
        assert_eq!(
            parsed.cases.get("deepClone handles circular ref"),
            Some(&TestOutcome::Fail)
        );
        assert_eq!(
            parsed.cases.get("legacy mode"),
            Some(&TestOutcome::Skipped)
        );
    }
}
