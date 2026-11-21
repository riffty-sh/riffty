pub mod junit;
pub mod tap;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::TestParserKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    Pass,
    Fail,
    Error,
    Skipped,
}

impl TestOutcome {
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Fail | Self::Error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRunSummary {
    pub parser: TestParserKind,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub errors: usize,
    pub skipped: usize,
    pub cases: BTreeMap<String, TestOutcome>,
    pub stdout_excerpt: String,
    pub stderr_excerpt: String,
}

impl TestRunSummary {
    pub fn from_exit_code(
        exit_code: Option<i32>,
        timed_out: bool,
        duration_ms: u64,
        stdout: &str,
        stderr: &str,
    ) -> Self {
        let ok = !timed_out && exit_code == Some(0);
        Self {
            parser: TestParserKind::ExitCode,
            exit_code,
            timed_out,
            duration_ms,
            total: 1,
            passed: usize::from(ok),
            failed: usize::from(!ok),
            errors: 0,
            skipped: 0,
            cases: BTreeMap::new(),
            stdout_excerpt: excerpt(stdout, 4000),
            stderr_excerpt: excerpt(stderr, 4000),
        }
    }

    pub fn passed_overall(&self) -> bool {
        if self.timed_out {
            return false;
        }
        match self.parser {
            TestParserKind::ExitCode => self.exit_code == Some(0),
            TestParserKind::JunitXml | TestParserKind::Tap => {
                self.exit_code == Some(0) && self.failed == 0 && self.errors == 0
            }
        }
    }
}

pub fn excerpt(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...\n[truncated {} bytes]", &text[..end], text.len() - end)
}
