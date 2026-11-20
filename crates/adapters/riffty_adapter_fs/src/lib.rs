use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use ignore::WalkBuilder;
use riffty_core::{
    DiffTarget, FileDiffRanges, FileId, GitDiffPort, PatchApplierPort, TextRange, WorkspaceFile,
    WorkspaceVfsPort, compute_line_starts,
};

static TMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceMarker {
    RifftyToml,
    JavaBuildRoot,
    JsTsWorkspaceRoot,
    GitRoot,
    TargetFallback,
}

fn package_json_has_workspaces(path: &Path) -> bool {
    let Ok(raw) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    val.get("workspaces").is_some_and(|w| !w.is_null())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWorkspace {
    /// Resolved workspace root directory (for config discovery, git diff, and containment).
    pub root: PathBuf,
    /// Original target path requested by the user (file or directory).
    pub target: PathBuf,
    /// Which marker anchored `root`.
    pub marker: WorkspaceMarker,
}

pub struct WorkspaceResolver;

impl WorkspaceResolver {
    /// Walks upward from `target_path` (or its parent directory if it is a file) to find the
    /// highest-priority workspace root marker:
    /// 1. `riffty.toml`
    /// 2. `settings.gradle` / `settings.gradle.kts` / outermost `pom.xml` (Java)
    ///    or `pnpm-workspace.yaml` / `turbo.json` / root `package.json` (`workspaces`) / `tsconfig.json` / `jsconfig.json` (JS/TS)
    /// 3. `.git`
    /// 4. Fallback: `target_path` itself (or parent dir for a file).
    pub fn resolve(target_path: &Path) -> ResolvedWorkspace {
        let abs_target = if target_path.is_absolute() {
            target_path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(target_path))
                .unwrap_or_else(|_| target_path.to_path_buf())
        };

        let start_dir = if abs_target.is_file() {
            abs_target
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        } else {
            abs_target.clone()
        };

        let mut best_java_root: Option<PathBuf> = None;
        let mut best_jsts_monorepo_root: Option<PathBuf> = None;
        let mut best_jsts_config_root: Option<PathBuf> = None;
        let mut best_git_root: Option<PathBuf> = None;

        for ancestor in start_dir.ancestors() {
            if ancestor.join("riffty.toml").is_file() {
                return ResolvedWorkspace {
                    root: ancestor.to_path_buf(),
                    target: abs_target,
                    marker: WorkspaceMarker::RifftyToml,
                };
            }

            if ancestor.join("settings.gradle").is_file()
                || ancestor.join("settings.gradle.kts").is_file()
                || ancestor.join("pom.xml").is_file()
            {
                // Keep walking upward so multi-module Maven/Gradle roots win over sub-modules.
                best_java_root = Some(ancestor.to_path_buf());
            }

            let pkg_json = ancestor.join("package.json");
            if ancestor.join("pnpm-workspace.yaml").is_file()
                || ancestor.join("turbo.json").is_file()
                || (pkg_json.is_file() && package_json_has_workspaces(&pkg_json))
            {
                best_jsts_monorepo_root = Some(ancestor.to_path_buf());
            } else if ancestor.join("tsconfig.json").is_file()
                || ancestor.join("jsconfig.json").is_file()
            {
                best_jsts_config_root = Some(ancestor.to_path_buf());
            }

            if best_git_root.is_none() && ancestor.join(".git").exists() {
                best_git_root = Some(ancestor.to_path_buf());
                // Do not walk past the git repository root for build markers.
                break;
            }
        }

        if let Some(java_root) = best_java_root {
            return ResolvedWorkspace {
                root: java_root,
                target: abs_target,
                marker: WorkspaceMarker::JavaBuildRoot,
            };
        }

        if let Some(jsts_root) = best_jsts_monorepo_root.or(best_jsts_config_root) {
            return ResolvedWorkspace {
                root: jsts_root,
                target: abs_target,
                marker: WorkspaceMarker::JsTsWorkspaceRoot,
            };
        }

        if let Some(git_root) = best_git_root {
            return ResolvedWorkspace {
                root: git_root,
                target: abs_target,
                marker: WorkspaceMarker::GitRoot,
            };
        }

        ResolvedWorkspace {
            root: start_dir,
            target: abs_target,
            marker: WorkspaceMarker::TargetFallback,
        }
    }
}

/// Validates a git ref passed to `git diff <base_ref>...HEAD` to prevent flag injection
/// or invalid ref syntax.
pub fn validate_git_ref(base_ref: &str) -> Result<(), String> {
    if base_ref.is_empty() {
        return Err("invalid git ref: ref must not be empty".to_string());
    }
    if base_ref.starts_with('-') {
        return Err(format!(
            "invalid git ref `{base_ref}`: ref must not start with `-`"
        ));
    }
    if base_ref.contains("..") {
        return Err(format!(
            "invalid git ref `{base_ref}`: ref must not contain `..`"
        ));
    }
    if base_ref.bytes().any(|b| b == 0) {
        return Err("invalid git ref: ref must not contain NUL bytes".to_string());
    }
    if base_ref.chars().any(|c| {
        c.is_whitespace()
            || c.is_control()
            || matches!(c, '\\' | '?' | '*' | '[' | ';' | '|' | '&' | '$' | '`')
    }) {
        return Err(format!(
            "invalid git ref `{base_ref}`: contains disallowed characters"
        ));
    }
    Ok(())
}

#[derive(Default, Debug, Clone, Copy)]
pub struct FsWorkspaceAdapter;

fn matches_extension(path: &Path, extensions: &[&str]) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    extensions.iter().any(|allowed| ext.eq_ignore_ascii_case(allowed))
}

impl WorkspaceVfsPort for FsWorkspaceAdapter {
    fn discover_files(
        &self,
        root: &Path,
        extensions: &[&str],
    ) -> Result<Vec<WorkspaceFile>, String> {
        if root.is_file() {
            if !matches_extension(root, extensions) {
                return Ok(Vec::new());
            }
            let source = self.read_file(root)?;
            let rel = root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| root.to_string_lossy().to_string());
            return Ok(vec![WorkspaceFile {
                file_id: FileId(1),
                path: root.to_path_buf(),
                relative_path: rel,
                source,
            }]);
        }

        if !root.exists() {
            return Err(format!(
                "workspace path `{}` does not exist",
                root.display()
            ));
        }

        let mut candidate_paths: Vec<PathBuf> = Vec::new();
        let walker = WalkBuilder::new(root)
            .standard_filters(true)
            .hidden(true)
            .filter_entry(|entry| {
                if entry.depth() == 0 {
                    return true;
                }
                let name = entry.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    ".git"
                        | ".venv"
                        | "venv"
                        | "__pycache__"
                        | "node_modules"
                        | ".next"
                        | "dist"
                        | "build"
                        | ".turbo"
                        | "coverage"
                        | "target"
                        | ".gradle"
                        | ".riffty"
                )
            })
            .build();

        for entry in walker {
            let entry = entry.map_err(|e| format!("directory walk error: {e}"))?;
            let path = entry.path();
            if path.is_file() && matches_extension(path, extensions) {
                candidate_paths.push(path.to_path_buf());
            }
        }
        candidate_paths.sort();

        let mut files = Vec::with_capacity(candidate_paths.len());
        for (idx, path) in candidate_paths.into_iter().enumerate() {
            let source = self.read_file(&path)?;
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            files.push(WorkspaceFile {
                file_id: FileId((idx + 1) as u32),
                path,
                relative_path: rel,
                source,
            });
        }
        Ok(files)
    }

    fn read_file(&self, path: &Path) -> Result<String, String> {
        fs::read_to_string(path).map_err(|e| format!("failed to read `{}`: {e}", path.display()))
    }
}

impl PatchApplierPort for FsWorkspaceAdapter {
    fn write_patched_file(
        &self,
        workspace_root: &Path,
        target_path: &Path,
        updated_source: &str,
    ) -> Result<(), String> {
        // Reject symlinks before canonicalization to prevent symlink-following TOCTOU attacks.
        let symlink_meta = fs::symlink_metadata(target_path).map_err(|e| {
            format!(
                "failed to stat target `{}`: {e}",
                target_path.display()
            )
        })?;
        if symlink_meta.file_type().is_symlink() {
            return Err(format!(
                "security boundary violation: refusing to patch symlink `{}`",
                target_path.display()
            ));
        }
        if !symlink_meta.file_type().is_file() {
            return Err(format!(
                "refusing to patch non-regular file `{}`",
                target_path.display()
            ));
        }

        let canonical_root = if workspace_root.is_file() {
            workspace_root
                .parent()
                .unwrap_or(workspace_root)
                .canonicalize()
                .map_err(|e| format!("failed to canonicalize root: {e}"))?
        } else {
            workspace_root
                .canonicalize()
                .map_err(|e| format!("failed to canonicalize workspace root: {e}"))?
        };
        let canonical_target = target_path.canonicalize().map_err(|e| {
            format!(
                "failed to canonicalize target `{}`: {e}",
                target_path.display()
            )
        })?;

        if !canonical_target.starts_with(&canonical_root) {
            return Err(format!(
                "security boundary violation: refusing to patch `{}` outside workspace `{}`",
                canonical_target.display(),
                canonical_root.display()
            ));
        }

        let parent_dir = canonical_target.parent().ok_or_else(|| {
            format!(
                "cannot determine parent directory for `{}`",
                canonical_target.display()
            )
        })?;

        let pid = std::process::id();
        let seq = TMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let file_name = canonical_target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("patch");
        let tmp_path = parent_dir.join(format!(".{file_name}.riffty-tmp-{pid}-{seq}"));

        let write_result = (|| -> Result<(), String> {
            let mut tmp_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)
                .map_err(|e| {
                    format!(
                        "failed to create atomic temp file `{}`: {e}",
                        tmp_path.display()
                    )
                })?;

            tmp_file
                .write_all(updated_source.as_bytes())
                .map_err(|e| format!("failed to write temp file `{}`: {e}", tmp_path.display()))?;
            tmp_file
                .sync_all()
                .map_err(|e| format!("failed to sync temp file `{}`: {e}", tmp_path.display()))?;
            drop(tmp_file);

            let _ = fs::set_permissions(&tmp_path, symlink_meta.permissions());

            fs::rename(&tmp_path, &canonical_target).map_err(|e| {
                format!(
                    "failed to atomically replace `{}`: {e}",
                    canonical_target.display()
                )
            })?;
            Ok(())
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&tmp_path);
        }
        write_result
    }
}

impl GitDiffPort for FsWorkspaceAdapter {
    fn touched_ranges(
        &self,
        repo_root: &Path,
        target: &DiffTarget,
    ) -> Result<Vec<FileDiffRanges>, String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(repo_root)
            .arg("diff")
            .arg("--unified=0")
            .arg("--no-color");
        match target {
            DiffTarget::Unstaged => {}
            DiffTarget::Staged => {
                cmd.arg("--cached");
            }
            DiffTarget::BranchDiff { base_ref } => {
                validate_git_ref(base_ref)?;
                cmd.arg(format!("{base_ref}...HEAD"));
            }
        }
        cmd.arg("--")
            .arg("*.py")
            .arg("*.java")
            .arg("*.ts")
            .arg("*.tsx")
            .arg("*.mts")
            .arg("*.cts")
            .arg("*.js")
            .arg("*.jsx")
            .arg("*.mjs")
            .arg("*.cjs");

        let output = cmd
            .output()
            .map_err(|e| format!("failed to invoke `git diff`: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("`git diff` failed: {}", stderr.trim()));
        }

        let diff_text = String::from_utf8_lossy(&output.stdout);
        let hunks_by_file = parse_unified_zero_diff(&diff_text);

        let mut results = Vec::new();
        for (rel_path, line_spans) in hunks_by_file {
            let abs_path = repo_root.join(&rel_path);
            if !abs_path.is_file() {
                continue;
            }
            let source = fs::read_to_string(&abs_path)
                .map_err(|e| format!("failed to read `{}`: {e}", abs_path.display()))?;
            let line_starts = compute_line_starts(&source);
            let mut ranges = Vec::with_capacity(line_spans.len());

            for (start_line, line_count) in line_spans {
                if line_count == 0 {
                    // Pure deletion hunk: anchor a 1-line neighborhood at `start_line`
                    let idx = start_line.saturating_sub(1) as usize;
                    let byte_pos = line_starts.get(idx).copied().unwrap_or(source.len() as u32);
                    ranges.push(TextRange::new(byte_pos, byte_pos));
                } else {
                    let start_idx = start_line.saturating_sub(1) as usize;
                    let end_idx = start_idx + (line_count as usize);
                    let start_byte = line_starts.get(start_idx).copied().unwrap_or(0);
                    let end_byte = line_starts
                        .get(end_idx)
                        .copied()
                        .unwrap_or(source.len() as u32);
                    ranges.push(TextRange::new(start_byte, end_byte));
                }
            }

            if !ranges.is_empty() {
                results.push(FileDiffRanges {
                    relative_path: PathBuf::from(rel_path),
                    touched_ranges: ranges,
                });
            }
        }
        Ok(results)
    }
}

/// Parses `git diff --unified=0` output into `relative_path -> Vec<(new_start_line, new_line_count)>`.
pub fn parse_unified_zero_diff(diff_text: &str) -> BTreeMap<String, Vec<(u32, u32)>> {
    let mut map: BTreeMap<String, Vec<(u32, u32)>> = BTreeMap::new();
    let mut current_file: Option<String> = None;

    for line in diff_text.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let trimmed = rest.trim();
            if trimmed == "/dev/null" {
                current_file = None;
            } else {
                let rel = trimmed.strip_prefix("b/").unwrap_or(trimmed).to_string();
                current_file = Some(rel);
            }
        } else if let Some(after_at) = line.strip_prefix("@@ ")
            && let Some(ref file) = current_file
            && let Some((header, _)) = after_at.split_once(" @@")
        {
            for part in header.split_whitespace() {
                if let Some(plus_spec) = part.strip_prefix('+') {
                    let (start_s, count_s) = plus_spec.split_once(',').unwrap_or((plus_spec, "1"));
                    if let (Ok(start), Ok(count)) = (start_s.parse::<u32>(), count_s.parse::<u32>())
                    {
                        map.entry(file.clone()).or_default().push((start, count));
                    }
                }
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("riffty_fs_test_{name}_{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn validate_git_ref_blocks_injection_and_allows_valid_refs() {
        assert!(validate_git_ref("main").is_ok());
        assert!(validate_git_ref("origin/main").is_ok());
        assert!(validate_git_ref("release/v1.2.3").is_ok());
        assert!(validate_git_ref("HEAD~1").is_ok());

        assert!(validate_git_ref("").is_err());
        assert!(validate_git_ref("--output=/tmp/pwn").is_err());
        assert!(validate_git_ref("-n").is_err());
        assert!(validate_git_ref("main..HEAD").is_err());
        assert!(validate_git_ref("main --cached").is_err());
        assert!(validate_git_ref("main\0evil").is_err());
        assert!(validate_git_ref("main;rm -rf /").is_err());
    }

    #[test]
    fn patch_applier_writes_atomically_and_rejects_symlinks_and_escape() {
        let root = make_temp_dir("atomic_patch");
        let outside = make_temp_dir("atomic_outside");
        let target = root.join("app.py");
        fs::write(&target, "x = 1\n").unwrap();

        let adapter = FsWorkspaceAdapter;
        adapter
            .write_patched_file(&root, &target, "x = 2\n")
            .expect("atomic patch should succeed");
        assert_eq!(fs::read_to_string(&target).unwrap(), "x = 2\n");

        // Outside workspace must be rejected
        let outside_file = outside.join("secret.py");
        fs::write(&outside_file, "secret = 1\n").unwrap();
        let err = adapter
            .write_patched_file(&root, &outside_file, "pwned\n")
            .unwrap_err();
        assert!(err.contains("security boundary violation"));

        // Symlink inside workspace must be rejected
        #[cfg(unix)]
        {
            let link_path = root.join("link.py");
            std::os::unix::fs::symlink(&target, &link_path).unwrap();
            let sym_err = adapter
                .write_patched_file(&root, &link_path, "x = 3\n")
                .unwrap_err();
            assert!(sym_err.contains("symlink"));
        }

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn workspace_resolver_walks_upward_by_marker_priority() {
        let root = make_temp_dir("resolver");
        let nested = root.join("services/orders/src/main/java/com/acme");
        fs::create_dir_all(&nested).unwrap();
        let java_file = nested.join("OrderController.java");
        fs::write(&java_file, "class OrderController {}\n").unwrap();

        // Fallback when no markers exist
        let r0 = WorkspaceResolver::resolve(&java_file);
        assert_eq!(r0.marker, WorkspaceMarker::TargetFallback);
        assert_eq!(r0.root, nested);

        // .git marker
        fs::create_dir_all(root.join(".git")).unwrap();
        let r1 = WorkspaceResolver::resolve(&java_file);
        assert_eq!(r1.marker, WorkspaceMarker::GitRoot);
        assert_eq!(r1.root, root);

        // Multi-module pom.xml beats .git and prefers outermost pom.xml
        fs::write(root.join("services/orders/pom.xml"), "<project/>").unwrap();
        fs::write(root.join("pom.xml"), "<project/>").unwrap();
        let r2 = WorkspaceResolver::resolve(&java_file);
        assert_eq!(r2.marker, WorkspaceMarker::JavaBuildRoot);
        assert_eq!(r2.root, root);

        // riffty.toml has highest priority
        let svc_dir = root.join("services/orders");
        fs::write(svc_dir.join("riffty.toml"), "exclude = []\n").unwrap();
        let r3 = WorkspaceResolver::resolve(&java_file);
        assert_eq!(r3.marker, WorkspaceMarker::RifftyToml);
        assert_eq!(r3.root, svc_dir);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn req_m0_02_workspace_resolver_jsts_monorepo_priority_5_levels_deep() {
        let root = make_temp_dir("jsts_resolver");
        // 5 levels nested: packages/ai/src/core/util/client.ts
        let nested = root.join("packages/ai/src/core/util");
        fs::create_dir_all(&nested).unwrap();
        let ts_file = nested.join("client.ts");
        fs::write(&ts_file, "export const x = 1;\n").unwrap();

        // 1. .git only
        fs::create_dir_all(root.join(".git")).unwrap();
        let r_git = WorkspaceResolver::resolve(&ts_file);
        assert_eq!(r_git.marker, WorkspaceMarker::GitRoot);
        assert_eq!(r_git.root, root);

        // 2. Local package tsconfig.json beats .git
        let pkg_dir = root.join("packages/ai");
        fs::write(pkg_dir.join("tsconfig.json"), "{}\n").unwrap();
        let r_tsc = WorkspaceResolver::resolve(&ts_file);
        assert_eq!(r_tsc.marker, WorkspaceMarker::JsTsWorkspaceRoot);
        assert_eq!(r_tsc.root, pkg_dir);

        // 3. Root package.json with `workspaces` beats nested tsconfig.json
        fs::write(
            root.join("package.json"),
            r#"{"name":"mono","workspaces":["packages/*"]}"#,
        )
        .unwrap();
        let r_pkg_ws = WorkspaceResolver::resolve(&ts_file);
        assert_eq!(r_pkg_ws.marker, WorkspaceMarker::JsTsWorkspaceRoot);
        assert_eq!(r_pkg_ws.root, root);

        // 4. turbo.json / pnpm-workspace.yaml also anchor monorepo root
        fs::write(root.join("package.json"), r#"{"name":"mono"}"#).unwrap();
        fs::write(root.join("pnpm-workspace.yaml"), "packages:\n  - packages/*\n").unwrap();
        let r_pnpm = WorkspaceResolver::resolve(&ts_file);
        assert_eq!(r_pnpm.marker, WorkspaceMarker::JsTsWorkspaceRoot);
        assert_eq!(r_pnpm.root, root);

        // 5. riffty.toml wins over everything
        fs::write(pkg_dir.join("riffty.toml"), "exclude = []\n").unwrap();
        let r_toml = WorkspaceResolver::resolve(&ts_file);
        assert_eq!(r_toml.marker, WorkspaceMarker::RifftyToml);
        assert_eq!(r_toml.root, pkg_dir);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn req_m0_03_discover_files_excludes_node_modules_and_jsts_build_dirs() {
        let root = make_temp_dir("jsts_discover");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/app.ts"), "export const a = 1;\n").unwrap();
        fs::write(root.join("src/view.tsx"), "export const v = <div />;\n").unwrap();

        for excluded in [
            "node_modules/pkg",
            ".next/static",
            "dist/esm",
            "build/out",
            ".turbo/cache",
            "coverage/lcov",
            ".git/hooks",
        ] {
            let dir = root.join(excluded);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("index.ts"), "export const ignored = true;\n").unwrap();
        }

        let adapter = FsWorkspaceAdapter;
        let files = adapter
            .discover_files(&root, riffty_core::Language::JSTS_EXTENSIONS)
            .unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.relative_path.as_str()).collect();
        assert_eq!(rels, vec!["src/app.ts", "src/view.tsx"]);

        let _ = fs::remove_dir_all(&root);
    }
}
