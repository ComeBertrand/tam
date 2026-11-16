use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Check that git is available.
pub fn check_git_available() -> Result<()> {
    Command::new("git")
        .arg("--version")
        .output()
        .context("git is not installed or not in PATH")?;
    Ok(())
}

/// Run a git command in the given directory and return stdout.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .context("git is not installed or not in PATH")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run a git command and return its non-empty stdout lines, untrimmed
/// (porcelain formats carry meaning in leading spaces).
fn git_lines(dir: &Path, args: &[&str]) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("cannot run git in {}", dir.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Resolve the toplevel of the current working tree (the checkout root).
/// For the main repo this is the repo root; for a worktree it's the worktree directory.
pub fn toplevel(dir: &Path) -> Result<PathBuf> {
    let tl = git(dir, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(tl))
}

/// Resolve the main repository root (the common dir for worktrees).
pub fn repo_root(dir: &Path) -> Result<PathBuf> {
    let common_dir = git(dir, &["rev-parse", "--git-common-dir"])?;
    let git_common = PathBuf::from(&common_dir);

    // git-common-dir returns the .git dir; we want its parent
    let root = if git_common.is_absolute() {
        git_common
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(git_common)
    } else {
        let abs = dir.join(&git_common);
        abs.canonicalize()?
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(abs)
    };

    Ok(root)
}

/// Run `git fetch --quiet`.
pub fn fetch(dir: &Path) -> Result<()> {
    // fetch may fail if there's no remote, that's ok
    let _ = git(dir, &["fetch", "--quiet"]);
    Ok(())
}

/// Check if a local branch exists.
pub fn local_branch_exists(dir: &Path, name: &str) -> Result<bool> {
    let result = git(
        dir,
        &["show-ref", "--verify", &format!("refs/heads/{}", name)],
    );
    Ok(result.is_ok())
}

/// Check if a remote tracking branch exists (origin/<name>).
pub fn remote_branch_exists(dir: &Path, name: &str) -> Result<bool> {
    let result = git(
        dir,
        &[
            "show-ref",
            "--verify",
            &format!("refs/remotes/origin/{}", name),
        ],
    );
    Ok(result.is_ok())
}

/// Detect the default branch via origin/HEAD, falling back to main then master.
pub fn default_branch(dir: &Path) -> Result<String> {
    // Try origin/HEAD
    if let Ok(output) = git(dir, &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        if let Some(branch) = output.strip_prefix("refs/remotes/origin/") {
            return Ok(branch.to_string());
        }
    }

    // Fallback: check if main exists
    if remote_branch_exists(dir, "main")? || local_branch_exists(dir, "main")? {
        return Ok("main".to_string());
    }

    // Fallback: check if master exists
    if remote_branch_exists(dir, "master")? || local_branch_exists(dir, "master")? {
        return Ok("master".to_string());
    }

    bail!("could not detect default branch")
}

/// Add a worktree.
pub fn worktree_add(dir: &Path, target: &Path, branch: &str) -> Result<()> {
    git(dir, &["worktree", "add", &target.to_string_lossy(), branch])?;
    Ok(())
}

/// Add a worktree with a new branch.
pub fn worktree_add_new_branch(
    dir: &Path,
    target: &Path,
    branch: &str,
    start_point: &str,
) -> Result<()> {
    git(
        dir,
        &[
            "worktree",
            "add",
            "-b",
            branch,
            &target.to_string_lossy(),
            start_point,
        ],
    )?;
    Ok(())
}

/// Remove a worktree.
pub fn worktree_remove(dir: &Path, target: &Path) -> Result<()> {
    git(dir, &["worktree", "remove", &target.to_string_lossy()])?;
    Ok(())
}

/// Force-remove a worktree (allows dirty/locked worktrees).
pub fn worktree_remove_force(dir: &Path, target: &Path) -> Result<()> {
    git(
        dir,
        &["worktree", "remove", "--force", &target.to_string_lossy()],
    )?;
    Ok(())
}

/// Delete a local branch: `git branch -d` (refused unless merged), or
/// `git branch -D` with `force`.
pub fn delete_branch(dir: &Path, name: &str, force: bool) -> Result<()> {
    git(dir, &["branch", if force { "-D" } else { "-d" }, name])?;
    Ok(())
}

/// Branch checked out in the working tree at `dir`, `None` on a detached HEAD.
pub fn current_branch(dir: &Path) -> Result<Option<String>> {
    let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    Ok((branch != "HEAD").then_some(branch))
}

/// Uncommitted changes in the working tree at `dir`, as `git status
/// --porcelain` lines: modified, staged and untracked files (untracked
/// directories collapsed), ignored files left out.
pub fn uncommitted_changes(dir: &Path) -> Result<Vec<String>> {
    git_lines(dir, &["status", "--porcelain=v1"])
}

/// Commits (`<short sha> <subject>`, newest first) reachable from the HEAD of
/// the working tree at `dir` and from no branch other than `branch`, no tag
/// and no remote-tracking branch: the commits only `branch` (or a detached
/// HEAD) holds.
pub fn commits_only_on(dir: &Path, branch: Option<&str>) -> Result<Vec<String>> {
    let exclude = branch.map(|b| format!("--exclude={b}"));
    let mut args = vec!["log", "--format=%h %s", "HEAD", "--not"];
    args.extend(exclude.as_deref());
    args.extend(["--branches", "--tags", "--remotes"]);
    git_lines(dir, &args)
}

/// List worktrees. Returns list of worktree paths.
pub fn worktree_list(dir: &Path) -> Result<Vec<PathBuf>> {
    let output = git(dir, &["worktree", "list", "--porcelain"])?;
    let paths = output
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect();
    Ok(paths)
}

/// The repository's common git directory (`.git` of the main checkout, or the
/// bare repository), from any of its worktrees.
pub fn common_dir(dir: &Path) -> Result<PathBuf> {
    let common = PathBuf::from(git(dir, &["rev-parse", "--git-common-dir"])?);
    if common.is_absolute() {
        Ok(common)
    } else {
        Ok(dir.join(common).canonicalize()?)
    }
}

/// Whether the index of the worktree whose git directory is `git_dir` has
/// staged changes (differs from its HEAD). Needs no working tree: it also
/// reads the index of a worktree whose directory is gone.
pub fn has_staged_changes(git_dir: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["diff", "--cached", "--quiet"])
        .current_dir(git_dir)
        .output()
        .context("git is not installed or not in PATH")?;
    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => bail!(
            "cannot read the index of {}: {}",
            git_dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// A worktree of a repository, as `git worktree list --porcelain` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// Its directory, as git recorded it (absolute, symlinks resolved).
    pub path: PathBuf,
    /// Branch checked out there; `None` on a detached HEAD (also during a
    /// rebase) and for a bare repository.
    pub branch: Option<String>,
    /// The bare repository itself (no working tree).
    pub bare: bool,
    /// Locked (`git worktree lock`): git refuses to remove or prune it.
    pub locked: bool,
    /// Git would prune it, e.g. its directory is gone. Older git versions do
    /// not report it: check the directory instead.
    pub prunable: bool,
}

/// The worktrees of the repository containing `dir`, main checkout first.
pub fn worktree_entries(dir: &Path) -> Result<Vec<WorktreeEntry>> {
    let mut entries: Vec<WorktreeEntry> = Vec::new();
    for line in git_lines(dir, &["worktree", "list", "--porcelain"])? {
        let (key, value) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        if key == "worktree" {
            entries.push(WorktreeEntry {
                path: PathBuf::from(value),
                branch: None,
                bare: false,
                locked: false,
                prunable: false,
            });
            continue;
        }
        let Some(entry) = entries.last_mut() else {
            continue;
        };
        match key {
            "branch" => {
                entry.branch = Some(value.strip_prefix("refs/heads/").unwrap_or(value).into())
            }
            "bare" => entry.bare = true,
            "locked" => entry.locked = true,
            "prunable" => entry.prunable = true,
            _ => {} // HEAD <sha>, detached
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Create a real git repo with an initial commit.
    fn init_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git(path, &["init"]).unwrap();
        git(path, &["config", "user.email", "test@test.com"]).unwrap();
        git(path, &["config", "user.name", "Test"]).unwrap();
        // Create initial commit so we have a branch
        fs::write(path.join("README.md"), "# test").unwrap();
        git(path, &["add", "."]).unwrap();
        git(path, &["commit", "-m", "init"]).unwrap();
    }

    #[test]
    fn test_repo_root_regular_repo() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myrepo");
        init_repo(&repo);

        let root = repo_root(&repo).unwrap();
        assert_eq!(root.canonicalize().unwrap(), repo.canonicalize().unwrap());
    }

    #[test]
    fn test_repo_root_from_worktree() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myrepo");
        init_repo(&repo);

        let wt_path = tmp.path().join("myrepo--feature");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                &wt_path.to_string_lossy(),
            ],
        )
        .unwrap();

        let root = repo_root(&wt_path).unwrap();
        assert_eq!(root.canonicalize().unwrap(), repo.canonicalize().unwrap());
    }

    #[test]
    fn test_local_branch_exists() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);

        // The default branch (main or master) should exist
        let default = git(&repo, &["branch", "--show-current"]).unwrap();
        assert!(local_branch_exists(&repo, &default).unwrap());
        assert!(!local_branch_exists(&repo, "nonexistent-branch").unwrap());
    }

    #[test]
    fn test_local_branch_exists_after_create() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);

        git(&repo, &["branch", "feature-x"]).unwrap();
        assert!(local_branch_exists(&repo, "feature-x").unwrap());
    }

    #[test]
    fn test_default_branch_fallback_to_main() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        // Ensure we're on main
        let _ = git(&repo, &["branch", "-M", "main"]);

        let branch = default_branch(&repo).unwrap();
        assert!(branch == "main" || branch == "master");
    }

    #[test]
    fn test_worktree_add_and_list() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);

        let wt_path = tmp.path().join("repo--feature");
        worktree_add_new_branch(&repo, &wt_path, "feature", "HEAD").unwrap();

        let worktrees = worktree_list(&repo).unwrap();
        assert!(worktrees.len() >= 2); // main + the new worktree
        let wt_canonical = wt_path.canonicalize().unwrap();
        assert!(
            worktrees
                .iter()
                .any(|p| p.canonicalize().unwrap_or_default() == wt_canonical),
            "worktree list should contain the new worktree"
        );
    }

    #[test]
    fn test_worktree_add_existing_branch() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);

        git(&repo, &["branch", "existing-branch"]).unwrap();

        let wt_path = tmp.path().join("repo--existing");
        worktree_add(&repo, &wt_path, "existing-branch").unwrap();

        assert!(wt_path.exists());
        let worktrees = worktree_list(&repo).unwrap();
        assert!(worktrees.len() >= 2);
    }

    #[test]
    fn test_worktree_remove() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);

        let wt_path = tmp.path().join("repo--feature");
        worktree_add_new_branch(&repo, &wt_path, "feature", "HEAD").unwrap();
        assert!(wt_path.exists());

        worktree_remove(&repo, &wt_path).unwrap();
        assert!(!wt_path.exists());
    }

    #[test]
    fn test_worktree_entries() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let _ = git(&repo, &["branch", "-M", "main"]);
        let path = |name: &str| tmp.path().canonicalize().unwrap().join(name);
        for name in ["feat/x", "detached", "locked", "deleted"] {
            let wt = tmp.path().join(name.replace('/', "-"));
            worktree_add_new_branch(&repo, &wt, name, "HEAD").unwrap();
        }
        git(&path("detached"), &["checkout", "-q", "--detach"]).unwrap();
        git(
            &repo,
            &["worktree", "lock", "--reason", "usb disk", "../locked"],
        )
        .unwrap();
        fs::remove_dir_all(path("deleted")).unwrap();

        let entries = worktree_entries(&repo).unwrap();
        let summary: Vec<(PathBuf, Option<&str>, bool, bool)> = entries
            .iter()
            .map(|e| (e.path.clone(), e.branch.as_deref(), e.locked, e.bare))
            .collect();
        assert_eq!(
            summary,
            // Main checkout first, then by path.
            [
                (path("repo"), Some("main"), false, false),
                (path("deleted"), Some("deleted"), false, false),
                (path("detached"), None, false, false),
                (path("feat-x"), Some("feat/x"), false, false),
                (path("locked"), Some("locked"), true, false),
            ]
        );
        assert!(entries[1].prunable, "{:?}", entries[1]);
        assert!(!entries[3].prunable);
        // Asked from a linked worktree, the list is the same.
        assert_eq!(worktree_entries(&path("feat-x")).unwrap(), entries);
    }

    #[test]
    fn test_fetch_no_remote() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);

        // Should not error even without a remote
        fetch(&repo).unwrap();
    }
}
