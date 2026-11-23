use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn make_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "riffty_ab_m2_{prefix}_{}_{}",
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

fn find_riffty_bin() -> PathBuf {
    let ab_bin = PathBuf::from(env!("CARGO_BIN_EXE_riffty-ab"));
    let sibling = ab_bin.parent().unwrap().join("riffty");
    assert!(sibling.exists());
    sibling
}

#[test]
fn e2e_scan_emit_manifest_interleaved_perf_and_verify_report() {
    let ab_bin = env!("CARGO_BIN_EXE_riffty-ab");
    let riffty_bin = find_riffty_bin();
    let root = make_temp_dir("scan_perf");
    let repo = root.join("origin");
    let out_dir = root.join("out");
    std::fs::create_dir_all(&repo).unwrap();

    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@riffty.sh"]);
    git(&repo, &["config", "user.name", "Riffty Test"]);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("vendor")).unwrap();

    // Real source with JSTS-REGEX-LOOP-001 (recompiling RegExp inside loop)
    std::fs::write(
        repo.join("src/matcher.mjs"),
        "export function countMatches(items) {\n  let n = 0;\n  for (const s of items) {\n    const re = new RegExp('^[a-z]+-[0-9]+$');\n    if (re.test(s)) n++;\n  }\n  return n;\n}\n",
    )
    .unwrap();
    // Vendored file that MUST be filtered out by scan
    std::fs::write(
        repo.join("vendor/thirdparty.mjs"),
        "export function bad(x) { return x === NaN; }\n",
    )
    .unwrap();
    std::fs::write(repo.join("package.json"), "{\"name\":\"demo\"}\n").unwrap();
    std::fs::write(
        repo.join("bench.mjs"),
        "import { countMatches } from './src/matcher.mjs';\nconst data = Array.from({ length: 50000 }, (_, i) => `item-${i}`);\nconst t0 = performance.now();\nconst c = countMatches(data);\nif (c !== 50000) process.exit(1);\nconst dt = performance.now() - t0;\nconsole.log(JSON.stringify({ elapsed_ms: dt }));\n",
    )
    .unwrap();

    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    // 1. Read-only discovery scan + auto-emit manifest
    let emitted_manifest = root.join("discovered.toml");
    let scan_out = Command::new(ab_bin)
        .args([
            "scan",
            repo.to_str().unwrap(),
            "--riffty-bin",
            riffty_bin.to_str().unwrap(),
            "--emit-manifest",
            emitted_manifest.to_str().unwrap(),
            "--case-id",
            "m2-regex-perf",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        scan_out.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&scan_out.stderr)
    );
    let scan_json: serde_json::Value = serde_json::from_slice(&scan_out.stdout).unwrap();
    assert_eq!(scan_json["filtered_vendored"].as_array().unwrap().len(), 1);
    assert_eq!(scan_json["candidates"][0]["file"], "src/matcher.mjs");
    assert_eq!(scan_json["candidates"][0]["rule_id"], "JSTS-REGEX-LOOP-001");
    assert!(emitted_manifest.is_file());

    // 2. Enable interleaved perf on the discovered manifest and run full A/B
    let mut toml_str = std::fs::read_to_string(&emitted_manifest).unwrap();
    toml_str = toml_str.replace("command = \"npm test\"", "command = \"node bench.mjs\"");
    toml_str = toml_str.replace(
        "[perf]\nenabled = false\ncommand = \"\"",
        "[perf]\nenabled = true\ncommand = \"node bench.mjs\"\nmetric_name = \"elapsed_ms\"\nmetric_json_key = \"elapsed_ms\"",
    );
    std::fs::write(&emitted_manifest, toml_str).unwrap();

    let run_out = Command::new(ab_bin)
        .args([
            "run",
            emitted_manifest.to_str().unwrap(),
            "--allow-exec",
            "--riffty-bin",
            riffty_bin.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(
        run_out.status.code(),
        Some(0),
        "run failed: {}",
        String::from_utf8_lossy(&run_out.stderr)
    );
    let run_json: serde_json::Value = serde_json::from_slice(&run_out.stdout).unwrap();
    assert_eq!(run_json["verdict"], "VERIFIED_FINOPS_OR_CLEANUP");
    assert_eq!(run_json["perf"]["metric_name"], "elapsed_ms");
    assert_eq!(run_json["perf"]["measured_pairs"], 5);
    assert_eq!(run_json["perf"]["samples_a"].as_array().unwrap().len(), 5);
    assert_eq!(run_json["perf"]["samples_b"].as_array().unwrap().len(), 5);

    // 3. Offline verify-report passes on authentic report
    let report_path = out_dir.join("ab_report.json");
    let verify_ok = Command::new(ab_bin)
        .args(["verify-report", report_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(verify_ok.status.code(), Some(0));

    // 4. Offline verify-report rejects forged verdict (exit 3)
    let forged_path = out_dir.join("forged_report.json");
    let forged = std::fs::read_to_string(&report_path)
        .unwrap()
        .replace("VERIFIED_FINOPS_OR_CLEANUP", "VERIFIED_BUG_FIX");
    std::fs::write(&forged_path, forged).unwrap();
    let verify_bad = Command::new(ab_bin)
        .args(["verify-report", forged_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(verify_bad.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&verify_bad.stderr).contains("verdict tampering"));

    let _ = std::fs::remove_dir_all(&root);
}
