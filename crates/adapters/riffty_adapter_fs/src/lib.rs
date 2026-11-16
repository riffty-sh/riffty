use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ignore::WalkBuilder;
use riffty_core::{
    DiffTarget, FileDiffRanges, FileId, GitDiffPort, PatchApplierPort, TextRange, WorkspaceFile,
    WorkspaceVfsPort, compute_line_starts,
};

#[derive(Default, Debug, Clone, Copy)]
pub struct FsWorkspaceAdapter;

impl WorkspaceVfsPort for FsWorkspaceAdapter {
    fn discover_python_files(&self, root: &Path) -> Result<Vec<WorkspaceFile>, String> {
        if root.is_file() {
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
            return Err(format!("workspace path `{}` does not exist", root.display()));
        }

        let mut candidate_paths: Vec<PathBuf> = Vec::new();
        let walker = WalkBuilder::new(root)
            .standard_filters(true)
            .hidden(true)
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    ".git" | ".venv" | "venv" | "__pycache__" | "node_modules" | "target"
                )
            })
            .build();

        for entry in walker {
            let entry = entry.map_err(|e| format!("directory walk error: {e}"))?;
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "py") {
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
        fs::read_to_string(path)
            .map_err(|e| format!("failed to read `{}`: {e}", path.display()))
    }
}

impl PatchApplierPort for FsWorkspaceAdapter {
    fn write_patched_file(
        &self,
        workspace_root: &Path,
        target_path: &Path,
        updated_source: &str,
    ) -> Result<(), String> {
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
        let canonical_target = target_path
            .canonicalize()
            .map_err(|e| format!("failed to canonicalize target `{}`: {e}", target_path.display()))?;

        if !canonical_target.starts_with(&canonical_root) {
            return Err(format!(
                "security boundary violation: refusing to patch `{}` outside workspace `{}`",
                canonical_target.display(),
                canonical_root.display()
            ));
        }

        fs::write(&canonical_target, updated_source)
            .map_err(|e| format!("failed to write `{}`: {e}", canonical_target.display()))
    }
}

impl GitDiffPort for FsWorkspaceAdapter {
    fn touched_ranges(
        &self,
        repo_root: &Path,
        target: &DiffTarget,
    ) -> Result<Vec<FileDiffRanges>, String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo_root).arg("diff").arg("--unified=0").arg("--no-color");
        match target {
            DiffTarget::Unstaged => {}
            DiffTarget::Staged => {
                cmd.arg("--cached");
            }
            DiffTarget::BranchDiff { base_ref } => {
                cmd.arg(format!("{base_ref}...HEAD"));
            }
        }
        cmd.arg("--").arg("*.py");

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
                    let byte_pos = line_starts
                        .get(idx)
                        .copied()
                        .unwrap_or(source.len() as u32);
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
                    let (start_s, count_s) = plus_spec
                        .split_once(',')
                        .unwrap_or((plus_spec, "1"));
                    if let (Ok(start), Ok(count)) = (start_s.parse::<u32>(), count_s.parse::<u32>()) {
                        map.entry(file.clone()).or_default().push((start, count));
                    }
                }
            }
        }
    }
    map
}

