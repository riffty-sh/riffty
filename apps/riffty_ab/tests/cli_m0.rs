use std::process::Command;

#[test]
fn cli_blocks_execution_without_allow_exec_and_supports_plan_only() {
    let bin = env!("CARGO_BIN_EXE_riffty-ab");
    let tmp = std::env::temp_dir().join(format!("riffty_ab_cli_m0_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let manifest_path = tmp.join("ab-case.toml");

    let init_out = Command::new(bin)
        .args([
            "init",
            manifest_path.to_str().unwrap(),
            "--case-id",
            "m0-cli-gate",
            "--repo-url",
            "https://github.com/pallets/flask",
            "--commit-sha",
            "a13bdc05aa23c7fce32ad1bd7033d33f84158397",
        ])
        .output()
        .expect("init should run");
    assert!(init_out.status.success(), "init failed: {init_out:?}");

    // Without --allow-exec -> exit 4
    let blocked = Command::new(bin)
        .args(["run", manifest_path.to_str().unwrap()])
        .output()
        .expect("run should execute");
    assert_eq!(blocked.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("--allow-exec"));

    // With --plan-only -> exit 0 and valid JSON
    let plan = Command::new(bin)
        .args([
            "run",
            manifest_path.to_str().unwrap(),
            "--plan-only",
            "--format",
            "json",
        ])
        .output()
        .expect("plan-only should execute");
    assert!(plan.status.success(), "plan-only failed: {plan:?}");
    let parsed: serde_json::Value = serde_json::from_slice(&plan.stdout).expect("valid JSON plan");
    assert_eq!(parsed["case_id"], "m0-cli-gate");
    assert_eq!(parsed["allow_dirty_baseline"], false);

    let _ = std::fs::remove_dir_all(&tmp);
}
