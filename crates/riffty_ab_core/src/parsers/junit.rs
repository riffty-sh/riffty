use std::collections::BTreeMap;

use crate::TestParserKind;
use crate::parsers::{TestOutcome, TestRunSummary, excerpt};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedJunitSuite {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub errors: usize,
    pub skipped: usize,
    pub cases: BTreeMap<String, TestOutcome>,
}

/// Parses one or more JUnit XML documents (pytest, Maven Surefire, Gradle, Vitest, Jest).
pub fn parse_junit_xml_documents(xml_docs: &[String]) -> Result<ParsedJunitSuite, String> {
    let mut combined = ParsedJunitSuite::default();
    for doc in xml_docs {
        let suite = parse_junit_xml(doc)?;
        combined.total += suite.total;
        combined.passed += suite.passed;
        combined.failed += suite.failed;
        combined.errors += suite.errors;
        combined.skipped += suite.skipped;
        combined.cases.extend(suite.cases);
    }
    Ok(combined)
}

pub fn parse_junit_xml(xml: &str) -> Result<ParsedJunitSuite, String> {
    if !xml.contains("<testsuite") && !xml.contains("<testcase") {
        return Err("input does not appear to be a JUnit XML report".to_string());
    }

    let mut suite = ParsedJunitSuite::default();
    let bytes = xml.as_bytes();
    let mut pos = 0usize;

    while let Some(rel_idx) = xml[pos..].find("<testcase") {
        let tag_start = pos + rel_idx;
        let after_name = tag_start + "<testcase".len();
        if after_name < bytes.len()
            && !matches!(bytes[after_name], b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/')
        {
            pos = after_name;
            continue;
        }

        let Some(tag_end_rel) = xml[tag_start..].find('>') else {
            return Err("unterminated <testcase> start tag in JUnit XML".to_string());
        };
        let tag_end = tag_start + tag_end_rel;
        let open_tag = &xml[tag_start..=tag_end];
        let self_closing = open_tag.ends_with("/>");

        let classname = extract_attr(open_tag, "classname").unwrap_or_default();
        let name = extract_attr(open_tag, "name").unwrap_or_else(|| format!("unnamed_{}", suite.total + 1));
        let key = if classname.is_empty() {
            name
        } else {
            format!("{classname}::{name}")
        };

        let body = if self_closing {
            pos = tag_end + 1;
            ""
        } else {
            let Some(close_rel) = xml[tag_end + 1..].find("</testcase>") else {
                return Err(format!("missing </testcase> closing tag for '{key}'"));
            };
            let close_start = tag_end + 1 + close_rel;
            let inner = &xml[tag_end + 1..close_start];
            pos = close_start + "</testcase>".len();
            inner
        };

        let outcome = if contains_child_tag(body, "error") {
            suite.errors += 1;
            TestOutcome::Error
        } else if contains_child_tag(body, "failure") {
            suite.failed += 1;
            TestOutcome::Fail
        } else if contains_child_tag(body, "skipped") {
            suite.skipped += 1;
            TestOutcome::Skipped
        } else {
            suite.passed += 1;
            TestOutcome::Pass
        };

        suite.total += 1;
        suite.cases.insert(key, outcome);
    }

    Ok(suite)
}

pub fn build_junit_summary(
    xml_docs: &[String],
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u64,
    stdout: &str,
    stderr: &str,
) -> Result<TestRunSummary, String> {
    let parsed = parse_junit_xml_documents(xml_docs)?;
    Ok(TestRunSummary {
        parser: TestParserKind::JunitXml,
        exit_code,
        timed_out,
        duration_ms,
        total: parsed.total,
        passed: parsed.passed,
        failed: parsed.failed,
        errors: parsed.errors,
        skipped: parsed.skipped,
        cases: parsed.cases,
        stdout_excerpt: excerpt(stdout, 4000),
        stderr_excerpt: excerpt(stderr, 4000),
    })
}

fn contains_child_tag(body: &str, tag_name: &str) -> bool {
    let needle = format!("<{tag_name}");
    let mut start = 0;
    while let Some(idx) = body[start..].find(&needle) {
        let after = start + idx + needle.len();
        if after >= body.len()
            || matches!(
                body.as_bytes()[after],
                b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/'
            )
        {
            return true;
        }
        start = after;
    }
    false
}

fn extract_attr(tag: &str, attr_name: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let needle = format!("{attr_name}=");
    let mut search = 0;
    while let Some(idx) = tag[search..].find(&needle) {
        let abs = search + idx;
        if abs > 0 && !bytes[abs - 1].is_ascii_whitespace() {
            search = abs + needle.len();
            continue;
        }
        let val_start = abs + needle.len();
        if val_start >= bytes.len() {
            return None;
        }
        let quote = bytes[val_start];
        if quote != b'"' && quote != b'\'' {
            return None;
        }
        let rest = &tag[val_start + 1..];
        let end = rest.find(quote as char)?;
        return Some(unescape_xml(&rest[..end]));
    }
    None
}

fn unescape_xml(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pytest_and_surefire_junit_xml() {
        let pytest_xml = r#"<?xml version="1.0" encoding="utf-8"?>
<testsuites>
  <testsuite name="pytest" errors="1" failures="1" skipped="1" tests="4" time="0.42">
    <testcase classname="tests.test_time" name="test_utc_aware" time="0.01"/>
    <testcase classname="tests.test_time" name="test_naive_fails" time="0.02">
      <failure message="AssertionError">assert tzinfo is not None</failure>
    </testcase>
    <testcase classname="tests.test_io" name="test_crash" time="0.01">
      <error message="RuntimeError">boom</error>
    </testcase>
    <testcase classname="tests.test_io" name="test_optional" time="0.00">
      <skipped type="pytest.skip" message="not linux"/>
    </testcase>
  </testsuite>
</testsuites>"#;

        let parsed = parse_junit_xml(pytest_xml).expect("valid junit xml");
        assert_eq!(parsed.total, 4);
        assert_eq!(parsed.passed, 1);
        assert_eq!(parsed.failed, 1);
        assert_eq!(parsed.errors, 1);
        assert_eq!(parsed.skipped, 1);
        assert_eq!(
            parsed.cases.get("tests.test_time::test_utc_aware"),
            Some(&TestOutcome::Pass)
        );
        assert_eq!(
            parsed.cases.get("tests.test_time::test_naive_fails"),
            Some(&TestOutcome::Fail)
        );
        assert_eq!(
            parsed.cases.get("tests.test_io::test_crash"),
            Some(&TestOutcome::Error)
        );
        assert_eq!(
            parsed.cases.get("tests.test_io::test_optional"),
            Some(&TestOutcome::Skipped)
        );
    }

    #[test]
    fn merges_multiple_surefire_reports() {
        let doc1 = r#"<testsuite name="A"><testcase classname="com.A" name="t1"/></testsuite>"#;
        let doc2 = r#"<testsuite name="B"><testcase classname="com.B" name="t2"><failure/></testcase></testsuite>"#;
        let merged = parse_junit_xml_documents(&[doc1.to_string(), doc2.to_string()]).unwrap();
        assert_eq!(merged.total, 2);
        assert_eq!(merged.passed, 1);
        assert_eq!(merged.failed, 1);
    }
}
