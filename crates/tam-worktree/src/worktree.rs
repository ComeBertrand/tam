use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::git;

/// Create a worktree for the current project.
///
/// Returns the path to the created worktree.
pub fn create(name: &str, source: Option<&str>, config: &Config, cwd: &Path) -> Result<PathBuf> {
    let main_root = git::repo_root(cwd)
        .context("not inside a git repository — run this from within a project")?;
    let project_name = main_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let target = config
        .worktree_root
        .join(format!("{}--{}", project_name, name));

    if target.exists() {
        bail!("worktree directory already exists: {}", target.display());
    }

    // Auto-create worktree root if it doesn't exist
    if !config.worktree_root.exists() {
        fs::create_dir_all(&config.worktree_root).with_context(|| {
            format!(
                "failed to create worktree root: {}",
                config.worktree_root.display()
            )
        })?;
    }

    git::fetch(cwd)?;

    // Branch resolution
    if git::local_branch_exists(cwd, name)? {
        // a. Local branch exists — check it out
        git::worktree_add(cwd, &target, name)?;
    } else if git::remote_branch_exists(cwd, name)? {
        // b. Remote branch exists — track it
        git::worktree_add(cwd, &target, name)?;
    } else if let Some(base) = source {
        // c. --source provided — create new branch from base
        git::worktree_add_new_branch(cwd, &target, name, base)?;
    } else {
        // d. Create new branch from default branch
        let default = git::default_branch(cwd)?;
        git::worktree_add_new_branch(cwd, &target, name, &default)?;
    }

    Ok(target)
}

/// What removing a worktree, and possibly its branch, would lose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCheck {
    /// Branch checked out in the worktree, `None` on a detached HEAD.
    pub branch: Option<String>,
    /// Uncommitted changes (`git status --porcelain` lines, untracked files
    /// included, ignored ones not). Removing the worktree loses them.
    pub uncommitted: Vec<String>,
    /// Commits (`<short sha> <subject>`) that no other branch, tag or
    /// remote-tracking branch holds. Deleting the branch loses them (removing
    /// the worktree does too on a detached HEAD).
    pub unmerged: Vec<String>,
}

impl WorktreeCheck {
    /// Commits that dropping the worktree loses: its unmerged commits when
    /// its branch goes too (`delete_branch`) or when it has no branch.
    pub fn lost_commits(&self, delete_branch: bool) -> &[String] {
        if delete_branch || self.branch.is_none() {
            &self.unmerged
        } else {
            &[]
        }
    }

    /// Whether dropping the worktree (and its branch with `delete_branch`)
    /// loses work.
    pub fn loses_work(&self, delete_branch: bool) -> bool {
        !self.uncommitted.is_empty() || !self.lost_commits(delete_branch).is_empty()
    }
}

/// Check what removing `worktree`, a linked worktree of the repository whose
/// main checkout is `repo`, would lose. Read-only.
///
/// Errors if `worktree` is not the root of a linked worktree of `repo`.
pub fn check(repo: &Path, worktree: &Path) -> Result<WorktreeCheck> {
    let canonical = |p: &Path| {
        p.canonicalize()
            .with_context(|| format!("cannot resolve {}", p.display()))
    };
    let path = canonical(worktree)?;
    if canonical(&git::toplevel(worktree)?)? != path {
        bail!("{} is not the root of a git worktree", worktree.display());
    }
    if canonical(&git::repo_root(worktree)?)? != canonical(repo)? {
        bail!(
            "{} is not a worktree of {}",
            worktree.display(),
            repo.display()
        );
    }
    if path == canonical(repo)? {
        bail!("{} is the main checkout of its repository", repo.display());
    }

    let branch = git::current_branch(worktree)?;
    Ok(WorktreeCheck {
        uncommitted: git::uncommitted_changes(worktree)?,
        unmerged: git::commits_only_on(worktree, branch.as_deref())?,
        branch,
    })
}

/// Remove `worktree` from the repository whose main checkout is `repo`.
/// Without `force`, git refuses a worktree with uncommitted changes.
pub fn remove(repo: &Path, worktree: &Path, force: bool) -> Result<()> {
    if force {
        git::worktree_remove_force(repo, worktree)
    } else {
        git::worktree_remove(repo, worktree)
    }
}

/// Delete `branch` from `repo` whether merged or not (`git branch -D`):
/// use [`check`] first to know what it loses. Git refuses a branch checked
/// out in a worktree.
pub fn delete_branch(repo: &Path, branch: &str) -> Result<()> {
    git::delete_branch(repo, branch, true)
}

/// `path` with symlinks resolved, also when it no longer exists: its nearest
/// existing ancestor is resolved and the rest appended. Git reports worktree
/// paths resolved, while a path from a user or a config may go through a
/// symlink (e.g. `/var` on macOS).
pub fn resolve_path(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut base = path;
    loop {
        if let Ok(resolved) = base.canonicalize() {
            return missing.iter().rev().fold(resolved, |p, name| p.join(name));
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                base = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Whether `a` and `b` name the same directory (see [`resolve_path`]).
pub fn same_path(a: &Path, b: &Path) -> bool {
    a == b || resolve_path(a) == resolve_path(b)
}

/// How [`recreate`] brings back a missing worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecreatePlan {
    /// Nothing in the way: add the worktree.
    Add,
    /// Git still registers the worktree at this path (its directory was
    /// deleted without `git worktree remove`): clear that registration, then
    /// add the worktree.
    ReplaceStale(PathBuf),
}

/// Whether the missing worktree `dir` of the repository `repo` can be
/// recreated on `branch`; `Err` says why not. `entries` are the repository's
/// worktrees ([`git::worktree_entries`]). Read-only.
///
/// Refused when `dir` exists, when `branch` no longer exists locally, when
/// another worktree has `branch` checked out, and when git still registers
/// `dir` locked, on a detached HEAD, on another branch or with staged changes
/// in its index: recreating would override or lose that state.
pub fn plan_recreate(
    repo: &Path,
    dir: &Path,
    branch: &str,
    entries: &[git::WorktreeEntry],
) -> Result<RecreatePlan> {
    if dir.symlink_metadata().is_ok() {
        bail!("{} exists", dir.display());
    }
    if !git::local_branch_exists(repo, branch)? {
        bail!("branch '{branch}' no longer exists");
    }
    let mut plan = RecreatePlan::Add;
    for entry in entries {
        if same_path(&entry.path, dir) {
            if entry.locked {
                bail!(
                    "git still has {} registered, locked: unlock it first (git worktree unlock)",
                    dir.display()
                );
            }
            match &entry.branch {
                Some(b) if b == branch => {
                    // Its index outlives the directory: staged changes would
                    // go with the registration.
                    if let Some(admin) = admin_dir(repo, &entry.path)? {
                        if git::has_staged_changes(&admin)? {
                            bail!(
                                "git still has {} registered with staged changes, which \
                                 recreating it would lose (git --git-dir={} diff --cached)",
                                dir.display(),
                                admin.display()
                            );
                        }
                    }
                }
                Some(b) => bail!(
                    "git still has {} registered, on branch '{b}' rather than '{branch}'",
                    dir.display()
                ),
                None => bail!(
                    "git still has {} registered, on a detached HEAD: its commits may be on no branch",
                    dir.display()
                ),
            }
            plan = RecreatePlan::ReplaceStale(entry.path.clone());
        } else if entry.branch.as_deref() == Some(branch) {
            bail!(
                "branch '{branch}' is checked out at {}",
                entry.path.display()
            );
        }
    }
    Ok(plan)
}

/// Recreate the missing worktree `dir` of the repository `repo` on `branch`,
/// if [`plan_recreate`] allows it. Git's own checks still apply (e.g. to a
/// branch being rebased in another worktree).
pub fn recreate(repo: &Path, dir: &Path, branch: &str) -> Result<()> {
    let entries = git::worktree_entries(repo)?;
    if let RecreatePlan::ReplaceStale(stale) = plan_recreate(repo, dir, branch, &entries)? {
        // That registration only. Its directory is gone, its HEAD is
        // `branch` and its index stages nothing: removing it loses at most
        // its HEAD reflog.
        git::worktree_remove(repo, &stale)?;
    }
    git::worktree_add(repo, dir, branch)
}

/// The administrative directory git keeps for the linked worktree registered
/// at `path` (`<common dir>/worktrees/<id>`), if any.
fn admin_dir(repo: &Path, path: &Path) -> Result<Option<PathBuf>> {
    let Ok(admins) = fs::read_dir(git::common_dir(repo)?.join("worktrees")) else {
        return Ok(None);
    };
    for admin in admins.flatten().map(|a| a.path()) {
        // `gitdir` holds the worktree's `.git` file path (relative to the
        // admin directory with `worktree.useRelativePaths`).
        let Ok(gitdir) = fs::read_to_string(admin.join("gitdir")) else {
            continue;
        };
        let gitdir = admin.join(gitdir.trim());
        if gitdir.parent().is_some_and(|wt| same_path(wt, path)) {
            return Ok(Some(admin));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn git_cmd(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git_cmd(path, &["init"]);
        git_cmd(path, &["config", "user.email", "test@test.com"]);
        git_cmd(path, &["config", "user.name", "Test"]);
        fs::write(path.join("README.md"), "# test").unwrap();
        git_cmd(path, &["add", "."]);
        git_cmd(path, &["commit", "-m", "init"]);
        // Ensure we're on main
        let _ = git_cmd(path, &["branch", "-M", "main"]);
    }

    fn test_config(worktree_root: PathBuf) -> Config {
        Config {
            max_depth: 5,
            ignore: vec![".*".to_string(), "node_modules".to_string()],
            worktree_root,
            auto_init: false,
        }
    }

    // --- create tests ---

    #[test]
    fn test_create_new_branch_from_default() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);

        let wt_root = tmp.path().join("worktrees");
        fs::create_dir_all(&wt_root).unwrap();
        let config = test_config(wt_root.clone());

        let result = create("feature-x", None, &config, &repo).unwrap();
        assert_eq!(result, wt_root.join("myproject--feature-x"));
        assert!(result.exists());

        // The worktree should have the README from the repo
        assert!(result.join("README.md").exists());
    }

    #[test]
    fn test_create_with_source() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);

        let wt_root = tmp.path().join("worktrees");
        fs::create_dir_all(&wt_root).unwrap();
        let config = test_config(wt_root.clone());

        let result = create("feature-y", Some("main"), &config, &repo).unwrap();
        assert!(result.exists());
    }

    #[test]
    fn test_create_existing_local_branch() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);
        git_cmd(&repo, &["branch", "existing-branch"]);

        let wt_root = tmp.path().join("worktrees");
        fs::create_dir_all(&wt_root).unwrap();
        let config = test_config(wt_root.clone());

        let result = create("existing-branch", None, &config, &repo).unwrap();
        assert!(result.exists());
    }

    #[test]
    fn test_create_target_already_exists() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);

        let wt_root = tmp.path().join("worktrees");
        let target = wt_root.join("myproject--feature-x");
        fs::create_dir_all(&target).unwrap();
        let config = test_config(wt_root);

        let result = create("feature-x", None, &config, &repo);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"),);
    }

    // --- drop tests (check / remove / delete_branch) ---

    /// A repo with a worktree for `feature-x`; returns (tmp, repo, worktree).
    fn repo_with_worktree() -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);
        let config = test_config(tmp.path().join("worktrees"));
        let wt = create("feature-x", None, &config, &repo).unwrap();
        (tmp, repo, wt)
    }

    fn commit(dir: &Path, file: &str, msg: &str) {
        fs::write(dir.join(file), msg).unwrap();
        git_cmd(dir, &["add", "."]);
        git_cmd(dir, &["commit", "-m", msg]);
    }

    #[test]
    fn test_check_clean_worktree() {
        let (_tmp, repo, wt) = repo_with_worktree();
        let check = check(&repo, &wt).unwrap();
        assert_eq!(check.branch.as_deref(), Some("feature-x"));
        assert!(check.uncommitted.is_empty());
        assert!(check.unmerged.is_empty(), "{:?}", check.unmerged);
        assert!(!check.loses_work(true));
    }

    #[test]
    fn test_check_reports_uncommitted_changes() {
        let (_tmp, repo, wt) = repo_with_worktree();
        fs::write(wt.join("README.md"), "changed").unwrap();
        fs::write(wt.join("new.txt"), "untracked").unwrap();
        fs::write(wt.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(wt.join("ignored.txt"), "ignored").unwrap();

        let check = check(&repo, &wt).unwrap();
        assert_eq!(
            check.uncommitted,
            vec![" M README.md", "?? .gitignore", "?? new.txt"],
        );
        assert!(check.loses_work(false), "uncommitted work is always lost");
    }

    #[test]
    fn test_check_reports_commits_on_no_other_branch() {
        let (_tmp, repo, wt) = repo_with_worktree();
        commit(&wt, "a.txt", "first");
        commit(&wt, "b.txt", "second");

        let check = check(&repo, &wt).unwrap();
        let subjects: Vec<&str> = check
            .unmerged
            .iter()
            .map(|c| c.split_once(' ').unwrap().1)
            .collect();
        assert_eq!(subjects, vec!["second", "first"]);
        // Keeping the branch keeps the commits.
        assert!(!check.loses_work(false));
        assert!(check.lost_commits(false).is_empty());
        assert!(check.loses_work(true));

        // Once another branch holds them, they are no longer at risk.
        git_cmd(&repo, &["branch", "backup", "feature-x"]);
        assert!(check_unmerged(&repo, &wt).is_empty());
        git_cmd(&repo, &["branch", "-D", "backup"]);
        // Same for a remote-tracking branch (a pushed branch).
        git_cmd(
            &repo,
            &["update-ref", "refs/remotes/origin/feature-x", "feature-x"],
        );
        assert!(check_unmerged(&repo, &wt).is_empty());
    }

    fn check_unmerged(repo: &Path, wt: &Path) -> Vec<String> {
        check(repo, wt).unwrap().unmerged
    }

    #[test]
    fn test_check_branch_with_slash() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);
        let wt = tmp.path().join("wt");
        git::worktree_add_new_branch(&repo, &wt, "feat/x", "main").unwrap();
        commit(&wt, "a.txt", "work");

        let check = check(&repo, &wt).unwrap();
        assert_eq!(check.branch.as_deref(), Some("feat/x"));
        assert_eq!(check.unmerged.len(), 1, "{:?}", check.unmerged);
    }

    #[test]
    fn test_check_detached_head_loses_its_commits() {
        let (_tmp, repo, wt) = repo_with_worktree();
        git_cmd(&wt, &["checkout", "--detach"]);
        commit(&wt, "a.txt", "detached work");

        let check = check(&repo, &wt).unwrap();
        assert_eq!(check.branch, None);
        assert_eq!(check.unmerged.len(), 1);
        assert!(check.loses_work(false), "no branch keeps these commits");
    }

    #[test]
    fn test_check_refuses_non_worktrees() {
        let (tmp, repo, wt) = repo_with_worktree();
        // The main checkout.
        let err = check(&repo, &repo).unwrap_err().to_string();
        assert!(err.contains("main checkout"), "{err}");
        // A subdirectory of the worktree.
        fs::create_dir_all(wt.join("sub")).unwrap();
        let err = check(&repo, &wt.join("sub")).unwrap_err().to_string();
        assert!(err.contains("not the root"), "{err}");
        // A worktree of another repository.
        let other = tmp.path().join("other");
        init_repo(&other);
        let err = check(&other, &wt).unwrap_err().to_string();
        assert!(err.contains("not a worktree of"), "{err}");
        // Not a repository at all.
        let plain = tmp.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert!(check(&repo, &plain).is_err());
    }

    #[test]
    fn test_remove_worktree_keeps_branch() {
        let (_tmp, repo, wt) = repo_with_worktree();
        remove(&repo, &wt, false).unwrap();
        assert!(!wt.exists());
        assert!(git::local_branch_exists(&repo, "feature-x").unwrap());
    }

    #[test]
    fn test_remove_dirty_worktree_requires_force() {
        let (_tmp, repo, wt) = repo_with_worktree();
        fs::write(wt.join("dirty.txt"), "uncommitted").unwrap();

        assert!(remove(&repo, &wt, false).is_err());
        assert!(wt.exists(), "dirty worktree should not be removed");

        remove(&repo, &wt, true).unwrap();
        assert!(!wt.exists());
    }

    #[test]
    fn test_delete_unmerged_branch() {
        let (_tmp, repo, wt) = repo_with_worktree();
        commit(&wt, "a.txt", "work");

        // Checked out in the worktree: git refuses.
        assert!(delete_branch(&repo, "feature-x").is_err());

        remove(&repo, &wt, false).unwrap();
        delete_branch(&repo, "feature-x").unwrap();
        assert!(!git::local_branch_exists(&repo, "feature-x").unwrap());
    }

    // --- recreate tests ---

    fn plan(repo: &Path, wt: &Path, branch: &str) -> Result<RecreatePlan> {
        plan_recreate(repo, wt, branch, &git::worktree_entries(repo).unwrap())
    }

    fn plan_err(repo: &Path, wt: &Path, branch: &str) -> String {
        format!("{:#}", plan(repo, wt, branch).unwrap_err())
    }

    #[test]
    fn test_recreate_after_git_worktree_remove() {
        let (_tmp, repo, wt) = repo_with_worktree();
        commit(&wt, "a.txt", "work");
        remove(&repo, &wt, false).unwrap();

        assert_eq!(plan(&repo, &wt, "feature-x").unwrap(), RecreatePlan::Add);
        recreate(&repo, &wt, "feature-x").unwrap();
        assert!(wt.join("a.txt").exists(), "the branch's work is back");
        assert_eq!(
            check(&repo, &wt).unwrap().branch.as_deref(),
            Some("feature-x")
        );
    }

    #[test]
    fn test_recreate_after_deleting_the_directory() {
        let (_tmp, repo, wt) = repo_with_worktree();
        fs::remove_dir_all(&wt).unwrap();

        // Still registered: that registration is replaced.
        let stale = wt.canonicalize().unwrap_or_else(|_| resolve_path(&wt));
        assert_eq!(
            plan(&repo, &wt, "feature-x").unwrap(),
            RecreatePlan::ReplaceStale(stale)
        );
        recreate(&repo, &wt, "feature-x").unwrap();
        assert_eq!(
            check(&repo, &wt).unwrap().branch.as_deref(),
            Some("feature-x")
        );
        let entries = git::worktree_entries(&repo).unwrap();
        assert_eq!(entries.len(), 2, "{entries:?}");
    }

    #[test]
    fn test_recreate_refusals() {
        let (tmp, repo, wt) = repo_with_worktree();
        // In place: nothing to recreate.
        assert!(plan_err(&repo, &wt, "feature-x").contains("exists"));

        // Moved elsewhere: the branch is checked out there.
        let moved = tmp.path().join("moved");
        git_cmd(
            &repo,
            &[
                "worktree",
                "move",
                wt.to_str().unwrap(),
                moved.to_str().unwrap(),
            ],
        );
        let err = plan_err(&repo, &wt, "feature-x");
        assert!(err.contains("checked out at"), "{err}");
        assert!(recreate(&repo, &wt, "feature-x").is_err());
        assert!(!wt.exists());

        // Branch deleted.
        remove(&repo, &moved, false).unwrap();
        delete_branch(&repo, "feature-x").unwrap();
        let err = plan_err(&repo, &wt, "feature-x");
        assert!(err.contains("no longer exists"), "{err}");
    }

    #[test]
    fn test_recreate_keeps_stale_registrations_it_cannot_judge() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);
        let wt = |name: &str| tmp.path().join(name);
        for name in ["locked", "detached", "switched"] {
            git::worktree_add_new_branch(&repo, &wt(name), name, "main").unwrap();
        }
        git_cmd(&repo, &["worktree", "lock", wt("locked").to_str().unwrap()]);
        git_cmd(&wt("detached"), &["checkout", "--detach"]);
        git_cmd(&wt("switched"), &["checkout", "-b", "other"]);
        for name in ["locked", "detached", "switched"] {
            fs::remove_dir_all(wt(name)).unwrap();
        }

        let err = plan_err(&repo, &wt("locked"), "locked");
        assert!(err.contains("locked"), "{err}");
        let err = plan_err(&repo, &wt("detached"), "detached");
        assert!(err.contains("detached HEAD"), "{err}");
        let err = plan_err(&repo, &wt("switched"), "switched");
        assert!(err.contains("on branch 'other'"), "{err}");
        // Nothing was touched.
        assert_eq!(git::worktree_entries(&repo).unwrap().len(), 4);
    }

    #[test]
    fn test_recreate_keeps_a_stale_registration_with_staged_changes() {
        let (_tmp, repo, wt) = repo_with_worktree();
        fs::write(wt.join("new.txt"), "staged, never committed").unwrap();
        git_cmd(&wt, &["add", "new.txt"]);
        fs::remove_dir_all(&wt).unwrap();

        let err = plan_err(&repo, &wt, "feature-x");
        assert!(err.contains("staged changes"), "{err}");
        assert!(recreate(&repo, &wt, "feature-x").is_err());
        assert!(!wt.exists());

        // Once unstaged (here: thrown away), it can be.
        let admin = admin_dir(&repo, &wt).unwrap().expect("registered");
        git_cmd(
            &repo,
            &["--git-dir", admin.to_str().unwrap(), "reset", "-q"],
        );
        recreate(&repo, &wt, "feature-x").unwrap();
        assert!(wt.join("README.md").exists());
    }

    #[test]
    fn test_same_path_through_a_symlink() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir_all(real.join("wt")).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert!(same_path(&link.join("wt"), &real.join("wt")));
        // Missing directories below the symlink too.
        assert!(same_path(
            &link.join("gone/deeper"),
            &real.join("gone/deeper")
        ));
        assert!(!same_path(&link.join("wt"), &real.join("other")));
        assert_eq!(
            resolve_path(&link.join("gone")),
            real.canonicalize().unwrap().join("gone")
        );
    }
}
