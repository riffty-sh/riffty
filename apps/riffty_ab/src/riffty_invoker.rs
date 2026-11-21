use std::path::{Path, PathBuf};
use std::process::Command;

use riffty_ab_core::RifftyConfig;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct RifftyJsonReport {
    #[allow(dead_code)]
    pub files_checked: usize,
    pub auto_healed_count: usize,
    pub diagnostics_count: usize,
    #[serde(default)]
    pub provenance: serde_json::Value,
    #[serde(default)]
    #[allow(dead_code)]
    pub auto_healed: Vec<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    pub diagnostics: Vec<serde_json::Value>,
}

/// Resolves the `riffty` executable: explicit flag > `RIFFTY_BIN` > sibling of `riffty-ab` > `PATH`.
pub fn resolve_riffty_binary(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p.to_path_buf());
        }
        return Err(format!("--riffty-bin path does not exist: {}", p.display()));
    }

    if let Ok(env_bin) = std::env::var("RIFFTY_BIN")
        && !env_bin.trim().is_empty()
    {
        let p = PathBuf::from(env_bin);
        if p.exists() {
            return Ok(p);
        }
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("riffty");
        if sibling.exists() {
            return Ok(sibling);
        }
        // When running from `target/debug/deps/...` in cargo tests
        if let Some(parent) = dir.parent() {
            let parent_sibling = parent.join("riffty");
            if parent_sibling.exists() {
                return Ok(parent_sibling);
            }
        }
    }

    Ok(PathBuf::from("riffty"))
}

pub fn riffty_version(riffty_bin: &Path) -> Result<String, String> {
    let out = Command::new(riffty_bin)
        .arg("--version")
        .output()
        .map_err(|e| format!("failed to run {} --version: {e}", riffty_bin.display()))?;
    if !out.status.success() {
        return Err(format!(
            "riffty --version failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn run_riffty_check(
    riffty_bin: &Path,
    repo_dir: &Path,
    manifest_dir: &Path,
    cfg: &RifftyConfig,
    fix: bool,
) -> Result<RifftyJsonReport, String> {
    let mut combined_files = 0usize;
    let mut combined_healed = 0usize;
    let mut combined_diags = 0usize;
    let mut combined_prov = Vec::new();
    let mut combined_healed_vec = Vec::new();
    let mut combined_diags_vec = Vec::new();

    for rel_target in &cfg.target_paths {
        let target_abs = repo_dir.join(rel_target);
        let mut cmd = Command::new(riffty_bin);
        cmd.arg("check").arg("--format").arg("json");
        if fix {
            cmd.arg("--fix");
        }
        if cfg.unsafe_fixes {
            cmd.arg("--unsafe-fixes");
        }
        if cfg.require_tier2 {
            cmd.arg("--require-tier2");
        }
        if let Some(ref rules) = cfg.rules {
            let rules_path = Path::new(rules);
            let resolved_rules = if rules_path.is_absolute() {
                rules_path.to_path_buf()
            } else {
                manifest_dir.join(rules_path)
            };
            cmd.arg("--rules").arg(resolved_rules);
        }
        for extra in &cfg.extra_args {
            cmd.arg(extra);
        }
        cmd.arg(&target_abs);

        let output = cmd.output().map_err(|e| {
            format!(
                "failed to execute {} check on {}: {e}",
                riffty_bin.display(),
                target_abs.display()
            )
        })?;

        // Exit code 0 (clean) and 1 (diagnostics found) are normal; exit 2+ is a hard error.
        let code = output.status.code().unwrap_or(2);
        if code >= 2 {
            return Err(format!(
                "riffty check failed (exit {code}) on '{}': {}",
                rel_target,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        let parsed: RifftyJsonReport = serde_json::from_slice(&output.stdout).map_err(|e| {
            format!(
                "failed to parse riffty JSON output on '{}': {e}\nstdout: {}",
                rel_target,
                String::from_utf8_lossy(&output.stdout)
            )
        })?;

        combined_files += parsed.files_checked;
        combined_healed += parsed.auto_healed_count;
        combined_diags += parsed.diagnostics_count;
        if let Some(arr) = parsed.provenance.as_array() {
            combined_prov.extend(arr.iter().cloned());
        }
        combined_healed_vec.extend(parsed.auto_healed);
        combined_diags_vec.extend(parsed.diagnostics);
    }

    Ok(RifftyJsonReport {
        files_checked: combined_files,
        auto_healed_count: combined_healed,
        diagnostics_count: combined_diags,
        provenance: serde_json::Value::Array(combined_prov),
        auto_healed: combined_healed_vec,
        diagnostics: combined_diags_vec,
    })
}
