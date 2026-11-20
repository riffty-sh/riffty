use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn create_temp_git_repo(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "riffty_v2_{prefix}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    run_git(&dir, &["init", "-b", "main"]);
    run_git(&dir, &["config", "user.email", "ci@riffty.dev"]);
    run_git(&dir, &["config", "user.name", "Riffty CI"]);
    dir
}

fn run_git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git invocation");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cli_check_and_fix_heals_safe_rules_and_preserves_unsafe_on_temp_repo() {
    let repo = create_temp_git_repo("cli_fix");
    let stats_py = repo.join("stats.py");
    std::fs::write(
        &stats_py,
        concat!(
            "import warnings\n",
            "import numpy as np\n",
            "\n",
            "def calculate_average(values):\n",
            "    warnings.warn(\"empty slice\")\n",
            "    return 0.0\n",
            "\n",
            "def wrapper(*args, **kwargs):\n",
            "    return 0.0\n",
            "\n",
            "def test_stats():\n",
            "    # Keep comment\n",
            "    np.testing.assert_warns(RuntimeWarning, calculate_average, [], match=\"empty\")\n",
            "    np.testing.assert_warns(RuntimeWarning, wrapper, [], match=\"empty\")\n",
        ),
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_riffty");

    // Run `riffty check --fix --format json <repo>`
    let out = Command::new(bin)
        .args(["check", "--fix", "--format", "json", repo.to_str().unwrap()])
        .output()
        .expect("run riffty check --fix");

    // Exit code 1 because `wrapper(*args, **kwargs)` has an `Unsafe` RFT-301 remaining
    assert_eq!(out.status.code(), Some(1));

    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid JSON check report");
    assert_eq!(report["auto_healed_count"], 2);
    assert_eq!(report["diagnostics_count"], 1);
    assert_eq!(report["diagnostics"][0]["rule_id"], "RFT-301");
    assert_eq!(report["diagnostics"][0]["applicability"], "Unsafe");

    let disk_after = std::fs::read_to_string(&stats_py).unwrap();
    assert!(
        disk_after.contains("import pytest\n"),
        "disk:\n{disk_after}"
    );
    assert!(
        disk_after.contains("warnings.warn(\"empty slice\", stacklevel=2)"),
        "disk:\n{disk_after}"
    );
    assert!(
        disk_after.contains(
            "    # Keep comment\n    with pytest.warns(RuntimeWarning, match=\"empty\"):\n        calculate_average([])"
        ),
        "disk:\n{disk_after}"
    );
    assert!(
        disk_after
            .contains("np.testing.assert_warns(RuntimeWarning, wrapper, [], match=\"empty\")"),
        "Unsafe wrapper call must remain untouched:\n{disk_after}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn mcp_stdio_review_changes_diff_scoping_reverse_dependents_and_auto_heal() {
    let repo = create_temp_git_repo("mcp_stdio");

    // Baseline commit:
    // - `stats.py` has an UNTOUCHED legacy `warnings.warn("legacy baseline")` on line 4 (must be suppressed by R14!)
    //   and `def calculate_average(values, match=None):` which initially accepted `match`.
    // - `test_caller.py` imports `calculate_average` and calls `np.testing.assert_warns(..., calculate_average, [], match="empty")`.
    let stats_path = repo.join("stats.py");
    let caller_path = repo.join("test_caller.py");

    std::fs::write(
        &stats_path,
        concat!(
            "import warnings\n",
            "\n",
            "def untouched_legacy():\n",
            "    warnings.warn(\"legacy baseline\")\n",
            "\n",
            "def calculate_average(values, match=None):\n",
            "    return 0.0\n",
        ),
    )
    .unwrap();

    std::fs::write(
        &caller_path,
        concat!(
            "import numpy as np\n",
            "from stats import calculate_average\n",
            "\n",
            "def test_avg():\n",
            "    np.testing.assert_warns(RuntimeWarning, calculate_average, [], match=\"empty\")\n",
        ),
    )
    .unwrap();

    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "initial baseline"]);

    // Working-tree change ONLY in `stats.py`:
    // 1. Remove `match=None` from `calculate_average(values)` (breaking `test_caller.py` via `reverse_dependents`!)
    // 2. Add a new `warnings.warn("empty input")` missing `stacklevel=2` inside `calculate_average`!
    // Note: `untouched_legacy()` at lines 3-4 is NOT touched in `git diff`, and `test_caller.py` is NOT touched in `git diff`!
    std::fs::write(
        &stats_path,
        concat!(
            "import warnings\n",
            "\n",
            "def untouched_legacy():\n",
            "    warnings.warn(\"legacy baseline\")\n",
            "\n",
            "def calculate_average(values):\n",
            "    warnings.warn(\"empty input\")\n",
            "    return 0.0\n",
        ),
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_riffty");
    let mut child = Command::new(bin)
        .args(["mcp", "--workspace", repo.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `riffty mcp`");

    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));

    // 1. MCP `initialize` handshake
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test-agent", "version": "1.0.0" }
            }
        })
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut init_line = String::new();
    stdout.read_line(&mut init_line).unwrap();
    let init_resp: serde_json::Value = serde_json::from_str(&init_line).expect("init json");
    assert_eq!(init_resp["id"], 1);
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "riffty");

    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        })
    )
    .unwrap();
    stdin.flush().unwrap();

    // 2. Call `tools/list` and verify `riffty_review_changes` is advertised
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        })
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut list_line = String::new();
    stdout.read_line(&mut list_line).unwrap();
    let list_resp: serde_json::Value = serde_json::from_str(&list_line).expect("tools/list json");
    assert_eq!(
        list_resp["result"]["tools"][0]["name"],
        "riffty_review_changes"
    );

    // 3. Call `riffty_review_changes` with `auto_heal: false` first:
    //    - Untouched `untouched_legacy` RFT-118 MUST be suppressed (`R14`)!
    //    - Touched `calculate_average` RFT-118 AND reverse-dependent `test_caller.py` RFT-301 (`R15`) MUST be caught!
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "riffty_review_changes",
                "arguments": {
                    "target": "unstaged",
                    "base_ref": "main",
                    "auto_heal": false
                }
            }
        })
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut dry_line = String::new();
    stdout.read_line(&mut dry_line).unwrap();
    let dry_resp: serde_json::Value = serde_json::from_str(&dry_line).expect("tools/call dry json");
    let dry_text = dry_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("text verdict");
    let dry_verdict: serde_json::Value = serde_json::from_str(dry_text).expect("verdict json");

    assert_eq!(
        dry_verdict["verdict"], "REQUEST_CHANGES",
        "dry_verdict: {dry_verdict}"
    );
    assert_eq!(dry_verdict["summary"]["files_reviewed"], 2);
    assert_eq!(dry_verdict["summary"]["auto_healed_count"], 0);
    assert_eq!(dry_verdict["summary"]["blocking_issues"], 2);

    let comments = dry_verdict["blocking_comments"].as_array().unwrap();
    // Verify baseline line 4 (`legacy baseline`) was NOT reported!
    assert!(
        !comments
            .iter()
            .any(|c| c["line"] == 4 && c["file"] == "stats.py"),
        "untouched baseline leaked into blocking_comments: {comments:?}"
    );
    // Verify `reverse_dependents` caught `test_caller.py` RFT-301 even though `test_caller.py` had zero git diff!
    let caller_comment = comments
        .iter()
        .find(|c| c["file"] == "test_caller.py")
        .expect("reverse_dependents must audit test_caller.py");
    assert_eq!(caller_comment["rule"], "RFT-301");
    assert_eq!(caller_comment["severity"], "ERROR");
    assert_eq!(
        caller_comment["type_proof"]["target_callable"],
        "calculate_average"
    );
    assert_eq!(caller_comment["type_proof"]["unexpected_kwarg"], "match");

    // 4. Now call `riffty_review_changes` with `auto_heal: true`!
    //    Both `RFT-118` in `stats.py` AND `Safe` `RFT-301` in `test_caller.py` are `Applicability::Safe`,
    //    so both heal cleanly on disk and return `"verdict": "APPROVED"`!
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "riffty_review_changes",
                "arguments": {
                    "target": "unstaged",
                    "base_ref": "main",
                    "auto_heal": true
                }
            }
        })
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut heal_line = String::new();
    stdout.read_line(&mut heal_line).unwrap();
    let heal_resp: serde_json::Value =
        serde_json::from_str(&heal_line).expect("tools/call heal json");
    let heal_text = heal_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("heal text verdict");
    let heal_verdict: serde_json::Value = serde_json::from_str(heal_text).expect("heal verdict");

    assert_eq!(
        heal_verdict["verdict"], "APPROVED",
        "heal_verdict: {heal_verdict}"
    );
    assert_eq!(heal_verdict["summary"]["auto_healed_count"], 2);
    assert_eq!(heal_verdict["summary"]["blocking_issues"], 0);

    // Verify disk files were surgically patched while untouched baseline line 4 stayed untouched!
    let stats_after = std::fs::read_to_string(&stats_path).unwrap();
    assert!(
        stats_after.contains("warnings.warn(\"legacy baseline\")\n"),
        "untouched baseline must NOT be modified:\n{stats_after}"
    );
    assert!(
        stats_after.contains("warnings.warn(\"empty input\", stacklevel=2)"),
        "touched line must be healed:\n{stats_after}"
    );

    let caller_after = std::fs::read_to_string(&caller_path).unwrap();
    assert!(
        caller_after.contains("import pytest\n"),
        "caller:\n{caller_after}"
    );
    assert!(
        caller_after.contains(
            "with pytest.warns(RuntimeWarning, match=\"empty\"):\n        calculate_average([])"
        ),
        "caller:\n{caller_after}"
    );

    drop(stdin);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn cli_fix_dry_run_emits_unified_diff_without_modifying_disk() {
    let repo = create_temp_git_repo("cli_dry_run");
    let app_py = repo.join("app.py");
    let original = "import warnings\n\ndef f():\n    warnings.warn(\"oops\")\n";
    std::fs::write(&app_py, original).unwrap();

    let bin = env!("CARGO_BIN_EXE_riffty");
    let out = Command::new(bin)
        .args(["check", "--fix", "--dry-run", repo.to_str().unwrap()])
        .output()
        .expect("run riffty check --fix --dry-run");

    // Exit code 1 because fixes are pending in dry-run mode
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--- a/app.py"), "stdout:\n{stdout}");
    assert!(stdout.contains("+++ b/app.py"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("+    warnings.warn(\"oops\", stacklevel=2)"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("dry-run, no files written"), "stdout:\n{stdout}");

    // Disk file MUST remain untouched
    assert_eq!(std::fs::read_to_string(&app_py).unwrap(), original);

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn cli_java_tier1_fix_sarif_explain_doctor_and_require_tier2() {
    let repo = create_temp_git_repo("cli_java_tier1");
    let billing_java = repo.join("Billing.java");
    std::fs::write(
        &billing_java,
        concat!(
            "import java.math.BigDecimal;\n",
            "import java.util.List;\n",
            "import java.util.stream.Collectors;\n",
            "\n",
            "class Billing {\n",
            "    void run(List<String> items) {\n",
            "        BigDecimal fee = new BigDecimal(0.1);\n",
            "        Integer n = new Integer(7);\n",
            "        boolean none = items.stream().collect(Collectors.toList()).isEmpty();\n",
            "    }\n",
            "}\n",
        ),
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_riffty");

    // 1. SARIF 2.1.0 output (explicitly testing Tier 1 zero-JVM mode)
    let sarif_out = Command::new(bin)
        .args([
            "check",
            "--java-tier1-only",
            "--format",
            "sarif",
            repo.to_str().unwrap(),
        ])
        .output()
        .expect("run riffty check --format sarif");
    assert_eq!(sarif_out.status.code(), Some(1));
    let sarif: serde_json::Value = serde_json::from_slice(&sarif_out.stdout).expect("valid SARIF");
    assert_eq!(sarif["version"], "2.1.0");
    assert_eq!(sarif["runs"][0]["results"].as_array().unwrap().len(), 3);
    assert_eq!(
        sarif["runs"][0]["properties"]["provenance"][0]["tier"],
        "tier1_syntactic"
    );

    // 2. --require-tier2 MUST fail closed with exit code 2 on Java Tier 1
    let req_t2 = Command::new(bin)
        .args([
            "check",
            "--java-tier1-only",
            "--require-tier2",
            repo.to_str().unwrap(),
        ])
        .output()
        .expect("run riffty check --require-tier2");
    assert_eq!(req_t2.status.code(), Some(2));

    // 3. --fix heals all 3 Safe Tier 1 Java rules cleanly
    let fix_out = Command::new(bin)
        .args([
            "check",
            "--java-tier1-only",
            "--fix",
            "--summary",
            repo.to_str().unwrap(),
        ])
        .output()
        .expect("run riffty check --fix on Java");
    assert_eq!(fix_out.status.code(), Some(0));
    let after = std::fs::read_to_string(&billing_java).unwrap();
    assert!(after.contains("BigDecimal.valueOf(0.1)"), "after:\n{after}");
    assert!(after.contains("Integer.valueOf(7)"), "after:\n{after}");
    assert!(after.contains("items.stream().findAny().isEmpty()"), "after:\n{after}");

    // 4. `riffty explain JAVA-BIGDEC-001` & `riffty doctor`
    let explain_out = Command::new(bin)
        .args(["explain", "JAVA-BIGDEC-001"])
        .output()
        .expect("run riffty explain");
    assert_eq!(explain_out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&explain_out.stdout).contains("BigDecimal.valueOf(0.1)"));

    let doctor_out = Command::new(bin)
        .args(["doctor", repo.to_str().unwrap()])
        .output()
        .expect("run riffty doctor");
    assert_eq!(doctor_out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&doctor_out.stdout).contains("Java Tier 1:"));

    let _ = std::fs::remove_dir_all(&repo);
}
