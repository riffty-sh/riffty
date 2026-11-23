use std::fs;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use crate::{JavaSidecarBundle, discover_java_sidecar_bundle_at, verify_jar_sha256};

const DEFAULT_GITHUB_REPO: &str = "riffty-sh/riffty";

/// Resolves the root cache directory for lazily hydrated `riffty` language plugins.
/// Precedence: `RIFFTY_CACHE_DIR` -> `XDG_CACHE_HOME/riffty` -> `LOCALAPPDATA/riffty` -> `$HOME/.cache/riffty`.
pub fn riffty_cache_dir() -> Option<PathBuf> {
    if let Ok(custom) = std::env::var("RIFFTY_CACHE_DIR") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        let trimmed = xdg.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed).join("riffty"));
        }
    }
    if cfg!(windows)
        && let Ok(local_app_data) = std::env::var("LOCALAPPDATA")
    {
        let trimmed = local_app_data.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed).join("riffty"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(|home| PathBuf::from(home).join(".cache").join("riffty"))
}

/// Returns the version-pinned cache directory for the hydrated `riffty-java` sidecar bundle.
pub fn cached_java_sidecar_dir(version: &str) -> Option<PathBuf> {
    let clean_ver = version.trim().trim_start_matches('v');
    riffty_cache_dir().map(|root| {
        root.join("plugins")
            .join("riffty-java")
            .join(format!("v{clean_ver}"))
    })
}

/// Returns the compile-time target triple matching GitHub Release artifact naming.
pub fn current_target_triple() -> Result<&'static str, String> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Ok("x86_64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        Ok("aarch64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Ok("aarch64-apple-darwin")
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        Ok("x86_64-apple-darwin")
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        Ok("x86_64-pc-windows-msvc")
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "windows", target_arch = "x86_64"),
    )))]
    {
        Err(format!(
            "unsupported platform for on-demand riffty-java hydration ({}-{})",
            std::env::consts::ARCH,
            std::env::consts::OS
        ))
    }
}

/// Computes lowercase hex SHA-256 of `bytes` and verifies it matches `expected_hex`.
pub fn verify_bytes_sha256(bytes: &[u8], expected_hex: &str, label: &str) -> Result<(), String> {
    let expected = expected_hex
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "invalid SHA-256 manifest for `{label}` (expected 64 hex chars, got `{expected}`)"
        ));
    }
    let digest = Sha256::digest(bytes);
    let actual: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        return Err(format!(
            "security boundary violation: SHA-256 mismatch for `{label}` (expected {expected}, got {actual})"
        ));
    }
    Ok(())
}

/// Safely extracts a `.tar.gz` byte slice into `dest_dir`, rejecting any path-traversal entries.
pub fn extract_tar_gz_safe(archive_bytes: &[u8], dest_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dest_dir)
        .map_err(|e| format!("failed to create dir `{}`: {e}", dest_dir.display()))?;
    let gz = GzDecoder::new(Cursor::new(archive_bytes));
    let mut archive = tar::Archive::new(gz);
    archive.set_preserve_permissions(true);

    let entries = archive
        .entries()
        .map_err(|e| format!("failed to read tar.gz entries: {e}"))?;
    for entry_res in entries {
        let mut entry = entry_res.map_err(|e| format!("corrupt tar.gz entry: {e}"))?;
        let rel_path = entry
            .path()
            .map_err(|e| format!("invalid entry path in tar.gz: {e}"))?
            .into_owned();
        for comp in rel_path.components() {
            if matches!(
                comp,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            ) {
                return Err(format!(
                    "security boundary violation: tar.gz archive contains unsafe path `{}`",
                    rel_path.display()
                ));
            }
        }
        entry
            .unpack_in(dest_dir)
            .map_err(|e| format!("failed to unpack `{}`: {e}", rel_path.display()))?;
    }
    Ok(())
}

#[cfg(windows)]
fn extract_zip_safe(archive_bytes: &[u8], dest_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dest_dir)
        .map_err(|e| format!("failed to create dir `{}`: {e}", dest_dir.display()))?;
    let tmp_zip = dest_dir.with_extension("download.zip");
    fs::write(&tmp_zip, archive_bytes)
        .map_err(|e| format!("failed to stage zip archive `{}`: {e}", tmp_zip.display()))?;
    let status = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Expand-Archive -Path $args[0] -DestinationPath $args[1] -Force",
        ])
        .arg(&tmp_zip)
        .arg(dest_dir)
        .status()
        .map_err(|e| format!("failed to invoke PowerShell Expand-Archive: {e}"))?;
    let _ = fs::remove_file(&tmp_zip);
    if !status.success() {
        return Err(format!("Expand-Archive failed with status {status}"));
    }
    Ok(())
}

fn fetch_bytes(url: &str, timeout_secs: u64) -> Result<Vec<u8>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .build()
        .into();
    let mut resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("failed to download `{url}`: {e}"))?;
    let mut bytes = Vec::new();
    resp.body_mut()
        .as_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("failed to read response body from `{url}`: {e}"))?;
    Ok(bytes)
}

/// Downloads, SHA-256 verifies, and atomically installs the self-contained `riffty-java`
/// sidecar bundle (bundled `jlink` JDK 25 + Leyden AOT + shaded Error Prone/Picnic JAR)
/// into the user cache directory.
pub fn hydrate_java_sidecar(version: &str, force: bool) -> Result<JavaSidecarBundle, String> {
    let clean_ver = version.trim().trim_start_matches('v');
    let cache_dir = cached_java_sidecar_dir(clean_ver).ok_or_else(|| {
        "unable to determine cache directory (set HOME or RIFFTY_CACHE_DIR)".to_string()
    })?;

    if !force
        && let Some(existing) = discover_java_sidecar_bundle_at(&cache_dir)
        && verify_jar_sha256(&existing.runner_jar, &existing.expected_sha256).is_ok()
    {
        return Ok(existing);
    }

    let target = current_target_triple()?;
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let archive_name = format!("riffty-java-{target}.{ext}");
    let sha_name = format!("{archive_name}.sha256");

    let base_url = std::env::var("RIFFTY_PLUGIN_BASE_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let repo = std::env::var("RIFFTY_GITHUB_REPO")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_GITHUB_REPO.to_string());
            format!("https://github.com/{repo}/releases/download/v{clean_ver}")
        });

    let archive_url = format!("{base_url}/{archive_name}");
    let sha_url = format!("{base_url}/{sha_name}");

    let sha_bytes = fetch_bytes(&sha_url, 30)?;
    let expected_sha =
        String::from_utf8(sha_bytes).map_err(|e| format!("invalid UTF-8 in `{sha_url}`: {e}"))?;

    let archive_bytes = fetch_bytes(&archive_url, 300)?;
    verify_bytes_sha256(&archive_bytes, &expected_sha, &archive_name)?;

    let parent = cache_dir
        .parent()
        .ok_or_else(|| format!("invalid cache path `{}`", cache_dir.display()))?;
    fs::create_dir_all(parent).map_err(|e| {
        format!(
            "failed to create plugin cache root `{}`: {e}",
            parent.display()
        )
    })?;

    let staging_dir = parent.join(format!(".staging-v{clean_ver}-{}", std::process::id()));
    if staging_dir.exists() {
        let _ = fs::remove_dir_all(&staging_dir);
    }

    let extract_res = {
        #[cfg(windows)]
        {
            extract_zip_safe(&archive_bytes, &staging_dir)
        }
        #[cfg(not(windows))]
        {
            extract_tar_gz_safe(&archive_bytes, &staging_dir)
        }
    };
    if let Err(err) = extract_res {
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(err);
    }

    let bundle = discover_java_sidecar_bundle_at(&staging_dir).ok_or_else(|| {
        let _ = fs::remove_dir_all(&staging_dir);
        format!(
            "hydrated archive `{archive_name}` missing riffty-java-runner.jar, riffty-java.sha256, or bundled runtime"
        )
    })?;
    if let Err(err) = verify_jar_sha256(&bundle.runner_jar, &bundle.expected_sha256) {
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(err);
    }

    if cache_dir.exists() {
        fs::remove_dir_all(&cache_dir).map_err(|e| {
            format!(
                "failed to replace existing cache `{}`: {e}",
                cache_dir.display()
            )
        })?;
    }
    fs::rename(&staging_dir, &cache_dir).map_err(|e| {
        let _ = fs::remove_dir_all(&staging_dir);
        format!(
            "failed to promote hydrated sidecar into `{}`: {e}",
            cache_dir.display()
        )
    })?;

    discover_java_sidecar_bundle_at(&cache_dir).ok_or_else(|| {
        format!(
            "hydrated sidecar disappeared after promotion to `{}`",
            cache_dir.display()
        )
    })
}

/// Removes all cached `riffty-java` sidecar bundles under the user cache directory.
pub fn clean_java_sidecar_cache() -> Result<Option<PathBuf>, String> {
    let Some(root) = riffty_cache_dir() else {
        return Ok(None);
    };
    let plugins_dir = root.join("plugins");
    if plugins_dir.exists() {
        fs::remove_dir_all(&plugins_dir)
            .map_err(|e| format!("failed to remove `{}`: {e}", plugins_dir.display()))?;
        return Ok(Some(plugins_dir));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    #[test]
    fn verify_bytes_sha256_accepts_valid_and_rejects_tampered() {
        let payload = b"riffty-self-contained-plugin";
        let digest = Sha256::digest(payload);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        let manifest = format!("{hex}  riffty-java-test.tar.gz\n");

        verify_bytes_sha256(payload, &manifest, "riffty-java-test.tar.gz")
            .expect("matching sha256 must pass");

        let err = verify_bytes_sha256(b"tampered", &manifest, "riffty-java-test.tar.gz")
            .expect_err("tampered bytes must fail closed");
        assert!(err.contains("SHA-256 mismatch"));
    }

    #[test]
    fn extract_tar_gz_safe_unpacks_valid_archive() {
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut builder = tar::Builder::new(&mut gz);
            let data = b"hello-riffty-java";
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "riffty-java.sha256", &data[..])
                .expect("append tar entry");
            builder.finish().expect("finish tar");
        }
        let archive_bytes = gz.finish().expect("finish gzip");

        let tmp = std::env::temp_dir().join(format!("riffty-hydrate-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        extract_tar_gz_safe(&archive_bytes, &tmp).expect("safe extract succeeds");
        let content = fs::read_to_string(tmp.join("riffty-java.sha256")).expect("read extracted");
        assert_eq!(content, "hello-riffty-java");
        let _ = fs::remove_dir_all(&tmp);
    }
}
