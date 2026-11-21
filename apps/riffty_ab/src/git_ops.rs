use std::path::Path;
use std::process::Command;

use riffty_ab_core::BaselineAttestation;

fn run_git(repo_dir: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    if let Some(dir) = repo_dir {
        cmd.arg("-C").arg(dir);
    }
    let output = cmd
        .args(args)
        .output()
        .map_err(|e| format!("failed to execute git {:?}: {e}", args))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "git {:?} failed (exit {:?}): {}{}",
            args,
            output.status.code(),
            stderr.trim(),
            if stdout.trim().is_empty() {
                String::new()
            } else {
                format!(" | stdout: {}", stdout.trim())
            }
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Clones `repo_url` (shallowly, with optional sparse paths) into `dest_dir` and checks out `commit_sha`.
pub fn clone_and_checkout_pinned(
    repo_url: &str,
    commit_sha: &str,
    dest_dir: &Path,
    sparse_paths: &[String],
) -> Result<(), String> {
    if !dest_dir.join(".git").exists() {
        std::fs::create_dir_all(dest_dir)
            .map_err(|e| format!("failed to create {}: {e}", dest_dir.display()))?;
        run_git(Some(dest_dir), &["init", "-q"])?;
        run_git(Some(dest_dir), &["remote", "add", "origin", repo_url])?;
        if !sparse_paths.is_empty() {
            run_git(Some(dest_dir), &["config", "core.sparseCheckout", "true"])?;
            let mut sparse_args = vec!["sparse-checkout", "set", "--no-cone"];
            let refs: Vec<&str> = sparse_paths.iter().map(String::as_str).collect();
            sparse_args.extend(refs);
            run_git(Some(dest_dir), &sparse_args)?;
        }
    }

    let has_commit = Command::new("git")
        .arg("-C")
        .arg(dest_dir)
        .args(["cat-file", "-e", &format!("{commit_sha}^{{commit}}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !has_commit {
        let fetch_args = if sparse_paths.is_empty() {
            vec!["fetch", "--depth", "1", "origin", commit_sha]
        } else {
            vec![
                "fetch",
                "--depth",
                "1",
                "--filter=blob:none",
                "origin",
                commit_sha,
            ]
        };
        if run_git(Some(dest_dir), &fetch_args).is_err() {
            // Fallback for git servers that do not support fetching arbitrary SHAs with --depth 1
            run_git(Some(dest_dir), &["fetch", "origin"])?;
        }
    }

    run_git(
        Some(dest_dir),
        &["-c", "advice.detachedHead=false", "checkout", "--force", commit_sha],
    )?;
    run_git(Some(dest_dir), &["clean", "-fdx"])?;
    Ok(())
}

/// Cryptographically attests `HEAD`, `HEAD^{tree}`, and clean working tree status.
pub fn attest_baseline(
    repo_dir: &Path,
    expected_commit_sha: &str,
    expected_tree_sha: Option<&str>,
    allow_dirty_baseline: bool,
) -> Result<BaselineAttestation, String> {
    let actual_head_sha = run_git(Some(repo_dir), &["rev-parse", "HEAD"])?;
    let actual_tree_sha = run_git(Some(repo_dir), &["rev-parse", "HEAD^{tree}"])?;
    let status_out = run_git(Some(repo_dir), &["status", "--porcelain"])?;
    let dirty_entries: Vec<String> = status_out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();

    Ok(BaselineAttestation::evaluate(
        expected_commit_sha,
        &actual_head_sha,
        expected_tree_sha,
        &actual_tree_sha,
        dirty_entries,
        allow_dirty_baseline,
    ))
}

/// Captures exact unified diff of all tracked working-tree modifications.
pub fn git_diff_unified(repo_dir: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["diff", "--no-color"])
        .output()
        .map_err(|e| format!("failed to run git diff: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git diff failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Lists repo-relative paths of tracked files modified in the working tree.
pub fn git_modified_files(repo_dir: &Path) -> Result<Vec<String>, String> {
    let out = run_git(Some(repo_dir), &["diff", "--name-only"])?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Creates a detached git worktree at `worktree_dir` pinned to `commit_sha`.
pub fn create_detached_worktree(
    repo_dir: &Path,
    commit_sha: &str,
    worktree_dir: &Path,
) -> Result<(), String> {
    run_git(
        Some(repo_dir),
        &[
            "worktree",
            "add",
            "--detach",
            worktree_dir.to_str().unwrap_or(""),
            commit_sha,
        ],
    )?;
    Ok(())
}

/// Removes a detached git worktree cleanly (best-effort prune).
pub fn remove_worktree(repo_dir: &Path, worktree_dir: &Path) {
    let _ = run_git(
        Some(repo_dir),
        &[
            "worktree",
            "remove",
            "--force",
            worktree_dir.to_str().unwrap_or(""),
        ],
    );
    let _ = std::fs::remove_dir_all(worktree_dir);
    let _ = run_git(Some(repo_dir), &["worktree", "prune"]);
}

/// Applies a unified diff patch cleanly inside `worktree_dir` via `git apply`.
pub fn apply_patch_str(worktree_dir: &Path, unified_diff: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("git")
        .arg("-C")
        .arg(worktree_dir)
        .args(["apply", "--whitespace=nowarn", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn git apply: {e}"))?;

    if let Some(ref mut stdin) = child.stdin {
        stdin
            .write_all(unified_diff.as_bytes())
            .map_err(|e| format!("failed to write patch to git apply stdin: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("failed waiting for git apply: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git apply failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Shallow-clones a repository's HEAD (with optional history depth for `riffty-ab scan --commits N`).
pub fn clone_head_shallow(repo_url: &str, dest_dir: &Path, depth: usize) -> Result<(), String> {
    let depth_str = depth.max(1).to_string();
    run_git(
        None,
        &[
            "clone",
            "--depth",
            &depth_str,
            repo_url,
            dest_dir.to_str().unwrap_or(""),
        ],
    )?;
    Ok(())
}

/// Returns up to `limit` recent commit SHAs from `HEAD` (newest first).
pub fn recent_commit_shas(repo_dir: &Path, limit: usize) -> Result<Vec<String>, String> {
    let n_arg = format!("-{}", limit.max(1));
    let out = run_git(Some(repo_dir), &["rev-list", &n_arg, "HEAD"])?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

pub fn rev_parse_head_and_tree(repo_dir: &Path) -> Result<(String, String), String> {
    let head = run_git(Some(repo_dir), &["rev-parse", "HEAD"])?;
    let tree = run_git(Some(repo_dir), &["rev-parse", "HEAD^{tree}"])?;
    Ok((head, tree))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "riffty_ab_{prefix}_{}_{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn init_sample_repo(dir: &Path) -> (String, String) {
        run_git(Some(dir), &["init", "-q"]).unwrap();
        run_git(Some(dir), &["config", "user.email", "test@riffty.sh"]).unwrap();
        run_git(Some(dir), &["config", "user.name", "Riffty Test"]).unwrap();
        std::fs::write(dir.join("app.py"), "x = 1\n").unwrap();
        run_git(Some(dir), &["add", "app.py"]).unwrap();
        run_git(Some(dir), &["commit", "-q", "-m", "initial"]).unwrap();
        let head = run_git(Some(dir), &["rev-parse", "HEAD"]).unwrap();
        let tree = run_git(Some(dir), &["rev-parse", "HEAD^{tree}"]).unwrap();
        (head, tree)
    }

    #[test]
    fn attests_clean_repo_and_rejects_tampered_working_tree() {
        let repo = make_temp_dir("attest");
        let (head, tree) = init_sample_repo(&repo);

        let clean = attest_baseline(&repo, &head, Some(&tree), false).unwrap();
        assert!(clean.attested, "clean repo must attest: {clean:?}");
        assert!(clean.working_tree_clean);

        // Tamper with tracked file
        std::fs::write(repo.join("app.py"), "x = 999  # fake bug\n").unwrap();
        let tampered = attest_baseline(&repo, &head, Some(&tree), false).unwrap();
        assert!(!tampered.attested, "tampered repo must fail attestation");
        assert!(!tampered.working_tree_clean);
        assert!(tampered
            .failure_reason
            .as_deref()
            .unwrap()
            .contains("dirty baseline"));

        // Clean up tracked modification and test untracked file
        run_git(Some(&repo), &["checkout", "--", "app.py"]).unwrap();
        std::fs::write(repo.join("injected.py"), "fake = True\n").unwrap();
        let untracked = attest_baseline(&repo, &head, Some(&tree), false).unwrap();
        assert!(!untracked.attested, "untracked file must fail attestation");

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn clones_pinned_sha_from_local_origin() {
        let origin = make_temp_dir("origin");
        let (head, tree) = init_sample_repo(&origin);

        let clone_dest = make_temp_dir("clone_dest");
        clone_and_checkout_pinned(origin.to_str().unwrap(), &head, &clone_dest, &[]).unwrap();

        let att = attest_baseline(&clone_dest, &head, Some(&tree), false).unwrap();
        assert!(att.attested);
        assert_eq!(att.actual_head_sha, head);
        assert_eq!(att.actual_tree_sha, tree);

        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone_dest);
    }
}
