use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use riffty_core::Diagnostic;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::architecture::JsTsArchitectureForbidRule;

const CACHE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFileEntry {
    pub content_sha256: String,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsTsWorkspaceCache {
    pub schema_version: u32,
    pub env_fingerprint: String,
    pub files: BTreeMap<String, CachedFileEntry>,
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

impl JsTsWorkspaceCache {
    pub fn new(env_fingerprint: String) -> Self {
        Self {
            schema_version: CACHE_SCHEMA_VERSION,
            env_fingerprint,
            files: BTreeMap::new(),
        }
    }

    pub fn cache_file_path(workspace_root: &Path, tsconfig_path: &Path) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(tsconfig_path.to_string_lossy().as_bytes());
        let hex = hex_encode(&hasher.finalize());
        workspace_root
            .join(".riffty")
            .join("cache")
            .join(format!("jsts-{}.bin", &hex[..16]))
    }

    pub fn load(path: &Path, expected_env_fingerprint: &str) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let parsed: Self = serde_json::from_slice(&bytes).ok()?;
        if parsed.schema_version != CACHE_SCHEMA_VERSION {
            return None;
        }
        if parsed.env_fingerprint != expected_env_fingerprint {
            return None;
        }
        Some(parsed)
    }

    pub fn save(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec(self) {
            let tmp = path.with_extension("bin.tmp");
            if std::fs::write(&tmp, &bytes).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }
}

pub fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex_encode(&hasher.finalize())
}

/// Computes a deterministic environment fingerprint covering:
/// - `tsconfig.json` contents
/// - `package.json` contents (if present)
/// - All workspace `.d.ts` files (exact content hash -> editing ANY `.d.ts` invalidates dependents!)
/// - `node_modules/@types` package names & mtimes
/// - `[[jsts.architecture.forbid]]` rules
pub fn compute_env_fingerprint(
    workspace_root: &Path,
    tsconfig_path: &Path,
    arch_rules: &[JsTsArchitectureForbidRule],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"riffty-jsts-cache-v1\n");

    if let Ok(cfg) = std::fs::read(tsconfig_path) {
        hasher.update(b"tsconfig:");
        hasher.update(&cfg);
    }

    let project_dir = tsconfig_path.parent().unwrap_or(workspace_root);
    for pkg_dir in [project_dir, workspace_root] {
        let pkg = pkg_dir.join("package.json");
        if let Ok(raw) = std::fs::read(&pkg) {
            hasher.update(b"pkg:");
            hasher.update(&raw);
        }
    }

    if let Ok(arch_json) = serde_json::to_vec(arch_rules) {
        hasher.update(b"arch:");
        hasher.update(&arch_json);
    }

    // Hash all workspace `.d.ts` files (excluding `node_modules`, `.git`, `dist`, `build`, `.next`)
    let mut dts_files: Vec<PathBuf> = Vec::new();
    collect_workspace_dts(project_dir, &mut dts_files);
    dts_files.sort();
    for dts in dts_files {
        hasher.update(dts.to_string_lossy().as_bytes());
        hasher.update(b"=");
        if let Ok(raw) = std::fs::read(&dts) {
            hasher.update(&raw);
        }
        hasher.update(b"\n");
    }

    // Hash `node_modules/@types` directory entries (names + package.json versions/mtimes)
    for base in [project_dir, workspace_root] {
        let types_dir = base.join("node_modules").join("@types");
        if let Ok(entries) = std::fs::read_dir(&types_dir) {
            let mut names: Vec<String> = entries
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            names.sort();
            for name in names {
                hasher.update(b"@types/");
                hasher.update(name.as_bytes());
                let pkg = types_dir.join(&name).join("package.json");
                if let Ok(meta) = std::fs::metadata(&pkg) {
                    hasher.update(meta.len().to_le_bytes());
                }
            }
        }
    }

    hex_encode(&hasher.finalize())
}

fn collect_workspace_dts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() {
            if matches!(
                name_str.as_ref(),
                "node_modules"
                    | ".git"
                    | ".riffty"
                    | "dist"
                    | "build"
                    | ".next"
                    | "target"
                    | ".turbo"
            ) {
                continue;
            }
            collect_workspace_dts(&path, out);
        } else if name_str.ends_with(".d.ts")
            || name_str.ends_with(".d.mts")
            || name_str.ends_with(".d.cts")
        {
            out.push(path);
        }
    }
}
