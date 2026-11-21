use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn make_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "riffty_ab_m1_{prefix}_{}_{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git should execute");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_git_repo(dir: &Path, files: &[(&str, &str)]) -> String {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "test@riffty.sh"]);
    git(dir, &["config", "user.name", "Riffty Test"]);
    for (rel, content) in files {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "initial commit"]);
    git(dir, &["rev-parse", "HEAD"])
}

fn find_riffty_bin() -> PathBuf {
    let ab_bin = PathBuf::from(env!("CARGO_BIN_EXE_riffty-ab"));
    let sibling = ab_bin.parent().unwrap().join("riffty");
    assert!(
        sibling.exists(),
        "expected riffty binary next to riffty-ab at {}",
        sibling.display()
    );
    sibling
}

#[test]
fn e2e_verified_bug_fix_with_junit_xml_idempotency_and_pr_review() {
    let ab_bin = env!("CARGO_BIN_EXE_riffty-ab");
    let riffty_bin = find_riffty_bin();
    let root = make_temp_dir("bugfix");
    let repo = root.join("origin");
    let out_dir = root.join("out");
    std::fs::create_dir_all(&repo).unwrap();

    // Real JS/TS NaN comparison bug (JSTS-NAN-EQ-001): `x === NaN` is always false!
    let sha = init_git_repo(
        &repo,
        &[
            (
                "src/check.mjs",
                "export function isInvalid(x) {\n  return x === NaN;\n}\n",
            ),
            (
                "run_test.py",
                r#"import subprocess, sys
code = "import { isInvalid } from './src/check.mjs'; if (!isInvalid(NaN)) process.exit(1);"
res = subprocess.run(["node", "-e", code])
failed = res.returncode != 0
xml = '<testsuite name="nan"><testcase classname="nan" name="test_is_invalid">' + ('<failure/>' if failed else '') + '</testcase></testsuite>'
with open("junit.xml", "w") as f:
    f.write(xml)
sys.exit(1 if failed else 0)
"#,
            ),
        ],
    );

    let manifest = format!(
        r#"
schema_version = 1
case_id = "e2e-nan-bugfix"
case_kind = "upstream_bug_verification"

[repo]
url = "{}"
commit_sha = "{sha}"
allow_dirty_baseline = false

[riffty]
target_paths = ["src/check.mjs"]

[test]
command = "python3 run_test.py"
parser = "junit_xml"
report_path = "junit.xml"
timeout_secs = 60
"#,
        repo.display()
    );
    let manifest_path = root.join("ab-case.toml");
    std::fs::write(&manifest_path, manifest).unwrap();

    let output = Command::new(ab_bin)
        .args([
            "run",
            manifest_path.to_str().unwrap(),
            "--allow-exec",
            "--emit-pr-review",
            "--riffty-bin",
            riffty_bin.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .expect("riffty-ab should run");

    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["verdict"], "VERIFIED_BUG_FIX");
    assert_eq!(report["attestation"]["attested"], true);
    assert_eq!(report["treatment"]["idempotent"], true);
    assert_eq!(report["treatment"]["auto_healed_count"], 1);
    assert_eq!(report["treatment"]["pass2_auto_healed_count"], 0);
    assert_eq!(report["variant_a"]["failed"], 1);
    assert_eq!(report["variant_b"]["passed"], 1);
    assert_eq!(report["comparison"]["fixed_tests"][0], "nan::test_is_invalid");

    assert!(out_dir.join("ab_report.json").is_file());
    assert!(out_dir.join("AB_REPORT.md").is_file());
    assert!(out_dir.join("treatment.patch").is_file());
    assert!(out_dir.join("PR_FOR_REVIEW.md").is_file());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn e2e_detects_regression_and_no_op_and_tampered() {
    let ab_bin = env!("CARGO_BIN_EXE_riffty-ab");
    let riffty_bin = find_riffty_bin();
    let root = make_temp_dir("gates");

    // 1. Regression test: test explicitly expects old buggy behavior (PASS in A -> FAIL in B)
    let repo_reg = root.join("reg");
    std::fs::create_dir_all(&repo_reg).unwrap();
    let sha_reg = init_git_repo(
        &repo_reg,
        &[
            (
                "src/check.mjs",
                "export function isInvalid(x) {\n  return x === NaN;\n}\n",
            ),
            (
                "test_reg.mjs",
                "import test from 'node:test';\nimport assert from 'node:assert/strict';\nimport { isInvalid } from './src/check.mjs';\ntest('expects false on NaN', () => { assert.equal(isInvalid(NaN), false); });\n",
            ),
        ],
    );
    let reg_manifest = root.join("reg.toml");
    std::fs::write(
        &reg_manifest,
        format!(
            r#"
schema_version = 1
case_id = "e2e-regression"
case_kind = "finops_or_cleanup"

[repo]
url = "{}"
commit_sha = "{sha_reg}"
allow_dirty_baseline = false

[riffty]
target_paths = ["src/check.mjs"]

[test]
command = "node --test --test-reporter=tap test_reg.mjs"
parser = "tap"
"#,
            repo_reg.display()
        ),
    )
    .unwrap();

    let reg_out = Command::new(ab_bin)
        .args([
            "run",
            reg_manifest.to_str().unwrap(),
            "--allow-exec",
            "--riffty-bin",
            riffty_bin.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(reg_out.status.code(), Some(1), "regression must exit 1");
    let reg_json: serde_json::Value = serde_json::from_slice(&reg_out.stdout).unwrap();
    assert_eq!(reg_json["verdict"], "REJECTED_REGRESSION");
    assert_eq!(reg_json["comparison"]["regressed_tests"][0], "expects false on NaN");

    // 2. NO_OP test: clean code with 0 riffty fixes
    let repo_noop = root.join("noop");
    std::fs::create_dir_all(&repo_noop).unwrap();
    let sha_noop = init_git_repo(
        &repo_noop,
        &[("src/clean.mjs", "export function add(a, b) { return a + b; }\n")],
    );
    let noop_manifest = root.join("noop.toml");
    std::fs::write(
        &noop_manifest,
        format!(
            r#"
schema_version = 1
case_id = "e2e-noop"
case_kind = "finops_or_cleanup"

[repo]
url = "{}"
commit_sha = "{sha_noop}"

[riffty]
target_paths = ["src/clean.mjs"]

[test]
command = "node -e 'process.exit(0)'"
parser = "exit_code"
"#,
            repo_noop.display()
        ),
    )
    .unwrap();

    let noop_out = Command::new(ab_bin)
        .args([
            "run",
            noop_manifest.to_str().unwrap(),
            "--allow-exec",
            "--riffty-bin",
            riffty_bin.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(noop_out.status.code(), Some(0));
    let noop_json: serde_json::Value = serde_json::from_slice(&noop_out.stdout).unwrap();
    assert_eq!(noop_json["verdict"], "NO_OP");

    // 3. Tampered baseline test -> exit 3
    std::fs::write(repo_noop.join("src/clean.mjs"), "// tampered\n").unwrap();
    let tamp_out = Command::new(ab_bin)
        .args([
            "run",
            noop_manifest.to_str().unwrap(),
            "--allow-exec",
            "--repo-dir",
            repo_noop.to_str().unwrap(),
            "--riffty-bin",
            riffty_bin.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(tamp_out.status.code(), Some(3), "tampered must exit 3");
    let tamp_json: serde_json::Value = serde_json::from_slice(&tamp_out.stdout).unwrap();
    assert_eq!(tamp_json["verdict"], "REJECTED_TAMPERED");

    let _ = std::fs::remove_dir_all(&root);
}
