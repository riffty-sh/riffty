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
    assert!(
        stdout.contains("dry-run, no files written"),
        "stdout:\n{stdout}"
    );

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
    assert!(
        after.contains("items.stream().findAny().isEmpty()"),
        "after:\n{after}"
    );

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

#[test]
fn mcp_stdio_review_changes_heals_java_diff_and_emits_provenance() {
    let repo = create_temp_git_repo("mcp_java");
    let billing_java = repo.join("Billing.java");
    std::fs::write(&billing_java, "class Billing {}\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "baseline"]);

    // Unstaged edit introducing a BigDecimal(double) bug
    std::fs::write(
        &billing_java,
        concat!(
            "import java.math.BigDecimal;\n",
            "class Billing {\n",
            "    BigDecimal fee(double rate) {\n",
            "        return new BigDecimal(0.1);\n",
            "    }\n",
            "}\n",
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

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

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
                "clientInfo": { "name": "java-agent", "version": "1.0.0" }
            }
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();

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

    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "riffty_review_changes",
                "arguments": { "target": "unstaged", "auto_heal": true }
            }
        })
    )
    .unwrap();
    stdin.flush().unwrap();

    line.clear();
    stdout.read_line(&mut line).unwrap();
    let resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    let verdict: serde_json::Value =
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(verdict["verdict"], "APPROVED", "verdict: {verdict}");
    assert_eq!(verdict["summary"]["auto_healed_count"], 1);
    assert_eq!(verdict["provenance"][0]["language"], "java");

    let after = std::fs::read_to_string(&billing_java).unwrap();
    assert!(after.contains("BigDecimal.valueOf(0.1)"), "after:\n{after}");

    drop(stdin);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn cli_jsts_tier1_fix_sarif_explain_ignore_and_require_tier2() {
    let repo = create_temp_git_repo("cli_jsts_tier1");
    let app_ts = repo.join("app.ts");
    std::fs::write(
        &app_ts,
        concat!(
            "export async function run(x: number, lines: string[]) {\n",
            "  const suppressed = x === NaN; // riffty: ignore[JSTS-NAN-EQ-001] -- intentional\n",
            "  const badNan = x === NaN;\n",
            "  const cloned = JSON.parse(JSON.stringify(lines));\n",
            "  let hits = 0;\n",
            "  for (const s of cloned) {\n",
            "    if (new RegExp(\"^[0-9]+$\").test(s)) hits++;\n",
            "  }\n",
            "  const p = new Promise(async (resolve) => resolve(hits));\n",
            "  return await p;\n",
            "}\n",
        ),
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_riffty");

    // 1. REQ-M1-05: `riffty explain` for all 6 Tier 1 JS/TS rule IDs
    for rule_id in [
        "JSTS-NAN-EQ-001",
        "JSTS-CLONE-001",
        "JSTS-REGEX-LOOP-001",
        "JSTS-INTL-LOOP-001",
        "JSTS-PROMISE-EXEC-001",
        "JSTS-DUP-AWAIT-001",
    ] {
        let exp = Command::new(bin)
            .args(["explain", rule_id])
            .output()
            .expect("riffty explain");
        assert!(exp.status.success(), "explain {rule_id} failed");
        let stdout = String::from_utf8_lossy(&exp.stdout);
        assert!(stdout.contains(rule_id), "missing {rule_id} in:\n{stdout}");
        assert!(stdout.contains("Before:"), "missing Before in:\n{stdout}");
        assert!(stdout.contains("After:"), "missing After in:\n{stdout}");
    }

    // 2. REQ-M1-06: `--require-tier2` fails closed with exit code 2
    let req2 = Command::new(bin)
        .args(["check", "--require-tier2", repo.to_str().unwrap()])
        .output()
        .expect("riffty check --require-tier2");
    assert_eq!(req2.status.code(), Some(2));

    // 3. REQ-M1-06: JSON provenance emits `tier1_syntactic` with `oxc_semantic`
    let json_out = Command::new(bin)
        .args(["check", "--format", "json", repo.to_str().unwrap()])
        .output()
        .expect("riffty check --format json");
    let report: serde_json::Value = serde_json::from_slice(&json_out.stdout).expect("valid json");
    assert_eq!(report["provenance"][0]["language"], "jsts");
    assert_eq!(report["provenance"][0]["tier"], "tier1_syntactic");
    assert!(
        report["provenance"][0]["runtime_detail"]
            .as_str()
            .unwrap()
            .contains("oxc_semantic")
    );
    // REQ-M1-04: Suppressed line 2 must not be reported; 5 unsuppressed findings remain
    assert_eq!(report["diagnostics_count"], 5, "report: {report}");

    // 4. REQ-M1-06: SARIF 2.1.0 schema check
    let sarif_out = Command::new(bin)
        .args(["check", "--format", "sarif", repo.to_str().unwrap()])
        .output()
        .expect("riffty check --format sarif");
    let sarif: serde_json::Value = serde_json::from_slice(&sarif_out.stdout).expect("valid sarif");
    assert_eq!(sarif["version"], "2.1.0");
    assert!(sarif["runs"][0]["results"].as_array().unwrap().len() == 5);

    // 5. Fix + idempotency
    let fix_out = Command::new(bin)
        .args(["check", "--fix", "--format", "json", repo.to_str().unwrap()])
        .output()
        .expect("riffty check --fix");
    let fix_report: serde_json::Value =
        serde_json::from_slice(&fix_out.stdout).expect("valid fix json");
    assert_eq!(
        fix_report["auto_healed_count"], 4,
        "fix_report: {fix_report}"
    );
    assert_eq!(
        fix_report["diagnostics_count"], 1,
        "only ManualOnly JSTS-PROMISE-EXEC-001 remains"
    );

    let after = std::fs::read_to_string(&app_ts).unwrap();
    assert!(after.contains("const suppressed = x === NaN; // riffty: ignore[JSTS-NAN-EQ-001]"));
    assert!(after.contains("const badNan = Number.isNaN(x);"));
    assert!(after.contains("const cloned = structuredClone(lines);"));
    assert!(after.contains("const __RIFFTY_RE_1 = new RegExp(\"^[0-9]+$\");"));
    assert!(after.contains("return p;"));

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn req_m2_04_cli_jsts_tier2_tsrs_require_tier2_pass_and_degraded_exit2() {
    let repo = create_temp_git_repo("cli_jsts_tier2");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("tsconfig.json"),
        r#"{"compilerOptions":{"target":"ES2022","module":"ESNext","strict":true,"noEmit":true},"include":["src/**/*"]}"#,
    )
    .unwrap();
    let app_ts = repo.join("src/sort.ts");
    std::fs::write(
        &app_ts,
        "export function orderPrices(prices: number[]): number[] {\n  return prices.sort();\n}\n",
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_riffty");

    // 1. Healthy Tier 2: --require-tier2 exits 1 on finding (NOT 2!) and emits `tier2_semantic`
    let check_out = Command::new(bin)
        .args([
            "check",
            "--require-tier2",
            "--format",
            "json",
            repo.to_str().unwrap(),
        ])
        .output()
        .expect("riffty check --require-tier2");
    assert_eq!(check_out.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&check_out.stdout).expect("valid json");
    assert_eq!(report["provenance"][0]["tier"], "tier2_semantic");
    assert!(
        report["provenance"][0]["runtime_detail"]
            .as_str()
            .unwrap()
            .contains("tsrs (TS7 Rust)")
    );
    assert_eq!(report["diagnostics"][0]["rule_id"], "JSTS-NUM-SORT-001");

    // 2. Healthy Tier 2 --fix --require-tier2 heals and exits 0!
    let fix_out = Command::new(bin)
        .args(["check", "--require-tier2", "--fix", repo.to_str().unwrap()])
        .output()
        .expect("riffty check --require-tier2 --fix");
    assert_eq!(fix_out.status.code(), Some(0));
    let fixed = std::fs::read_to_string(&app_ts).unwrap();
    assert!(
        fixed.contains("prices.sort((a, b) => a - b)"),
        "fixed:\n{fixed}"
    );

    // 3. Degraded when package.json declares dependencies without node_modules -> exit code 2!
    std::fs::write(
        repo.join("package.json"),
        r#"{"name":"app","dependencies":{"openai":"^4.0.0"}}"#,
    )
    .unwrap();
    let deg_out = Command::new(bin)
        .args(["check", "--require-tier2", repo.to_str().unwrap()])
        .output()
        .expect("riffty check --require-tier2 degraded");
    assert_eq!(deg_out.status.code(), Some(2));

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn req_m3_02_cli_sarif_and_mcp_enforce_jsts_architecture_forbid() {
    let repo = create_temp_git_repo("cli_jsts_arch");
    std::fs::create_dir_all(repo.join("src/domain")).unwrap();
    std::fs::create_dir_all(repo.join("src/infra")).unwrap();
    std::fs::write(
        repo.join("tsconfig.json"),
        r#"{"compilerOptions":{"target":"ES2023","module":"ESNext","baseUrl":".","paths":{"@/*":["src/*"]},"strict":true,"noEmit":true},"include":["src/**/*"]}"#,
    )
    .unwrap();
    std::fs::write(
        repo.join("riffty.toml"),
        r#"
[[jsts.architecture.forbid]]
id = "JSTS-ARCH-DOMAIN"
from = "src/domain/**"
target = ["src/infra/**", "next/**"]
"#,
    )
    .unwrap();
    std::fs::write(repo.join("src/infra/db.ts"), "export const db = {};\n").unwrap();
    std::fs::write(
        repo.join("src/domain/order.ts"),
        "export const clean = 1;\n",
    )
    .unwrap();

    // Commit clean baseline
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success());
    };
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "clean baseline"]);

    // Introduce forbidden import via tsconfig path alias `@/infra/db`
    std::fs::write(
        repo.join("src/domain/order.ts"),
        "import { db } from \"@/infra/db\";\nexport const leaked = db;\n",
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_riffty");

    // 1. CLI SARIF 2.1.0 emits Error-level JSTS-ARCH-DOMAIN at line 1
    let sarif_out = Command::new(bin)
        .args(["check", "--format", "sarif", repo.to_str().unwrap()])
        .output()
        .expect("riffty check --format sarif");
    assert_eq!(sarif_out.status.code(), Some(1));
    let sarif: serde_json::Value = serde_json::from_slice(&sarif_out.stdout).expect("valid sarif");
    assert_eq!(sarif["version"], "2.1.0");
    let results = sarif["runs"][0]["results"].as_array().unwrap();
    assert!(
        results
            .iter()
            .any(|r| r["ruleId"] == "JSTS-ARCH-DOMAIN" && r["level"] == "error"),
        "sarif results: {results:?}"
    );

    // 2. Live MCP blocks unstaged boundary leak with REQUEST_CHANGES!
    let mut child = Command::new(bin)
        .args(["mcp", "--workspace", repo.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn riffty mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

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
                "clientInfo": { "name": "test", "version": "1.0" }
            }
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();

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

    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "riffty_review_changes",
                "arguments": { "target": "unstaged", "auto_heal": true }
            }
        })
    )
    .unwrap();
    stdin.flush().unwrap();

    line.clear();
    stdout.read_line(&mut line).unwrap();
    let resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    let verdict: serde_json::Value =
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(verdict["verdict"], "REQUEST_CHANGES", "verdict: {verdict}");
    assert!(
        verdict["blocking_comments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["rule"] == "JSTS-ARCH-DOMAIN" && c["severity"] == "ERROR"),
        "verdict: {verdict}"
    );

    drop(stdin);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn e2e_rules_validate_detects_duplicates_and_accepts_clean_sugar_pack() {
    let dir = create_temp_git_repo("rules_validate");
    let base = dir.join("base.yml");
    let child = dir.join("child.yml");
    let dup = dir.join("dup.yml");

    std::fs::write(
        &base,
        r#"
pack: pydantic-v1-to-v2
defaults:
  language: python
  severity: warning
  applicability: safe
  target_type: pydantic.BaseModel
rename_methods:
  dict: model_dump
  json: model_dump_json
"#,
    )
    .unwrap();
    std::fs::write(
        &child,
        r#"
extends:
  - base.yml
rules:
  - id: CUSTOM-001
    name: custom-rule
    message: "custom"
    pattern: "old_fn($X)"
    replace: "new_fn($X)"
"#,
    )
    .unwrap();
    std::fs::write(
        &dup,
        r#"
rules:
  - id: DUP-A
    name: dup-a
    message: "m1"
    pattern: "foo($X)"
    replace: "bar($X)"
  - id: DUP-B
    name: dup-b
    message: "m2"
    pattern: "foo($X)"
    replace: "bar($X)"
"#,
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_riffty");

    let ok_out = Command::new(bin)
        .args(["rules", "validate", child.to_str().unwrap()])
        .output()
        .expect("riffty rules validate child");
    assert!(
        ok_out.status.success(),
        "expected exit 0, stderr: {}",
        String::from_utf8_lossy(&ok_out.stderr)
    );
    let stdout = String::from_utf8_lossy(&ok_out.stdout);
    assert!(
        stdout.contains("OK (3 rules checked, 0 issues)"),
        "got: {stdout}"
    );

    let fail_out = Command::new(bin)
        .args(["rules", "validate", dup.to_str().unwrap()])
        .output()
        .expect("riffty rules validate dup");
    assert_eq!(fail_out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&fail_out.stderr);
    assert!(stderr.contains("DuplicatePattern"), "got: {stderr}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn e2e_cross_language_architecture_layers_and_cycles() {
    let repo = create_temp_git_repo("cross_lang_arch");
    std::fs::create_dir_all(repo.join("app/domain")).unwrap();
    std::fs::create_dir_all(repo.join("app/infra")).unwrap();
    std::fs::create_dir_all(repo.join("src/domain")).unwrap();
    std::fs::create_dir_all(repo.join("src/infra")).unwrap();

    std::fs::write(
        repo.join("riffty.toml"),
        r#"
[architecture]
forbid_cycles = true

[[architecture.layers]]
name = "domain"
matches = ["app.domain.**", "src/domain/**"]
may_access = []

[[architecture.layers]]
name = "infra"
matches = ["app.infra.**", "src/infra/**"]
may_access = ["domain"]
"#,
    )
    .unwrap();

    std::fs::write(
        repo.join("app/domain/order.py"),
        "from app.infra.db import save\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("app/infra/db.py"),
        "from app.domain.order import Order\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("src/domain/user.ts"),
        "import { repo } from '../infra/repo';\nexport const x = repo;\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/infra/repo.ts"), "export const repo = 1;\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_riffty");
    let out = Command::new(bin)
        .args([
            "check",
            "--no-type-oracle",
            "--format",
            "json",
            repo.to_str().unwrap(),
        ])
        .output()
        .expect("riffty check --format json");
    assert_eq!(out.status.code(), Some(1));

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid json report");
    let diags = report["diagnostics"].as_array().unwrap();
    let rule_ids: Vec<&str> = diags.iter().filter_map(|d| d["rule_id"].as_str()).collect();
    assert!(
        rule_ids.contains(&"ARCH-LAYER-DOMAIN"),
        "expected ARCH-LAYER-DOMAIN, got: {diags:#?}"
    );
    assert!(
        rule_ids.contains(&"ARCH-CYCLE-001"),
        "expected ARCH-CYCLE-001, got: {diags:#?}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}
