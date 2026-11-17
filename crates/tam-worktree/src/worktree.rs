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
    let target = target_dir(config, &main_root, name);

    if target.exists() {
        bail!("worktree directory already exists: {}", target.display());
    }

    git::fetch(cwd)?;
    add(cwd, &target, name, source).map_err(|e| e.error)?;
    Ok(target)
}

/// Where a task's worktree on branch `name` of the repository whose main
/// checkout is `repo_root` goes: `<worktree root>/<repo>--<name>`.
pub fn target_dir(config: &Config, repo_root: &Path, name: &str) -> PathBuf {
    let project_name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    config
        .worktree_root
        .join(format!("{}--{}", project_name, name))
}

/// A worktree [`add`] created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    pub dir: PathBuf,
    pub branch: String,
    /// The local branch did not exist before: it was created with the
    /// worktree (from a remote branch, or a start point).
    pub branch_created: bool,
}

/// Why [`add`] failed.
#[derive(Debug)]
pub struct AddError {
    pub error: anyhow::Error,
    /// The worktree, when git created it before failing (e.g. a failing
    /// `post-checkout` hook): undo it with [`undo_add`].
    pub created: Option<Added>,
}

/// How [`add`] gets its branch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Checkout {
    /// The local branch exists: check it out.
    Local,
    /// `origin/<branch>` exists: a local branch tracking it.
    Remote,
    /// A new branch from this start point.
    New(String),
}

/// Whether [`add`] can add a worktree at `dir` on `branch` (see there);
/// `Err` says why not. Read-only.
pub fn check_add(repo: &Path, dir: &Path, branch: &str, source: Option<&str>) -> Result<()> {
    plan_add(repo, dir, branch, source).map(|_| ())
}

fn plan_add(repo: &Path, dir: &Path, branch: &str, source: Option<&str>) -> Result<Checkout> {
    if dir.symlink_metadata().is_ok() {
        bail!("worktree directory already exists: {}", dir.display());
    }
    for entry in git::worktree_entries(repo)? {
        if entry.branch.as_deref() == Some(branch) {
            bail!(
                "branch '{branch}' is checked out at {}",
                entry.path.display()
            );
        }
    }
    if git::local_branch_exists(repo, branch)? {
        return Ok(Checkout::Local);
    }
    if git::remote_branch_exists(repo, branch)? {
        return Ok(Checkout::Remote);
    }
    let base = match source {
        Some(source) => source.to_string(),
        None => git::default_branch(repo)?,
    };
    if !git::resolves(repo, &base) {
        bail!("'{base}' is not a commit of this repository: cannot branch '{branch}' from it");
    }
    Ok(Checkout::New(base))
}

/// Add a worktree of the repository containing `repo` (its main checkout,
/// or any of its worktrees) at `dir`, which must not exist, on `branch`:
/// the local branch if it exists, else one tracking `origin/<branch>`, else
/// a new branch from `source` (default: the repository's default branch).
/// Refused when the branch is checked out elsewhere. Does not fetch: run
/// [`git::fetch`] first to see the latest remote branches.
pub fn add(repo: &Path, dir: &Path, branch: &str, source: Option<&str>) -> Result<Added, AddError> {
    let failed = |error| AddError {
        error,
        created: None,
    };
    let checkout = plan_add(repo, dir, branch, source).map_err(failed)?;
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))
            .map_err(failed)?;
    }
    let added = Added {
        dir: dir.to_path_buf(),
        branch: branch.to_string(),
        branch_created: checkout != Checkout::Local,
    };
    let result = match &checkout {
        Checkout::Local | Checkout::Remote => git::worktree_add(repo, dir, branch),
        Checkout::New(base) => git::worktree_add_new_branch(repo, dir, branch, base),
    };
    let Err(error) = result else {
        return Ok(added);
    };
    // Git may fail after doing part of the work: after creating the
    // worktree (a failing hook), or after creating the branch only (the
    // directory cannot be created).
    let left = dir.exists()
        || (added.branch_created && git::local_branch_exists(repo, branch).unwrap_or(true));
    Err(AddError {
        error,
        created: left.then_some(added),
    })
}

/// Undo [`add`]: remove the worktree, and its branch if it was created with
/// it, unless that would lose work. `repo` is the repository's main
/// checkout. `Err` says what was kept, and why: the worktree when it has
/// uncommitted changes (or cannot be checked), the branch when it holds
/// commits no other branch, tag or remote branch has.
///
/// Without its directory (git failed before creating it), only the branch.
pub fn undo_add(repo: &Path, added: &Added) -> Result<()> {
    if added.dir.symlink_metadata().is_err() {
        if !added.branch_created || !git::local_branch_exists(repo, &added.branch)? {
            return Ok(());
        }
        if !git::branch_commits_only_on(repo, &added.branch)?.is_empty() {
            bail!(
                "kept branch '{}': it holds commits no other branch has",
                added.branch
            );
        }
        return delete_branch(repo, &added.branch)
            .with_context(|| format!("kept branch '{}'", added.branch));
    }
    let check = check(repo, &added.dir).context("kept the worktree")?;
    if !check.uncommitted.is_empty() {
        bail!(
            "kept the worktree: it has uncommitted changes ({})",
            check.uncommitted.join(", ")
        );
    }
    // Forced: git refuses clean worktrees with submodules otherwise.
    remove(repo, &added.dir, true).context("kept the worktree")?;
    if !added.branch_created {
        return Ok(());
    }
    if check.branch.as_deref() != Some(added.branch.as_str()) {
        bail!(
            "removed the worktree, kept branch '{}': the worktree was on another branch",
            added.branch
        );
    }
    if !check.unmerged.is_empty() {
        bail!(
            "removed the worktree, kept branch '{}': it holds commits no other branch has",
            added.branch
        );
    }
    delete_branch(repo, &added.branch)
        .with_context(|| format!("removed the worktree, kept branch '{}'", added.branch))
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

    // --- add / undo_add ---

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        git::local_branch_exists(repo, branch).unwrap()
    }

    #[test]
    fn test_target_dir_is_named_after_the_repository() {
        let config = test_config(PathBuf::from("/wt"));
        assert_eq!(
            target_dir(&config, Path::new("/src/api"), "feat"),
            PathBuf::from("/wt/api--feat")
        );
    }

    #[test]
    fn test_add_tells_whether_it_created_the_branch() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        git_cmd(&repo, &["branch", "old"]);
        // A clone, whose origin has a branch it lacks locally.
        git_cmd(&repo, &["branch", "remote-only"]);
        let clone = tmp.path().join("clone");
        git_cmd(
            tmp.path(),
            &[
                "clone",
                "-q",
                repo.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );

        let new = add(&repo, &tmp.path().join("wt/new"), "new", None).unwrap();
        assert!(new.branch_created);
        assert!(new.dir.join("README.md").exists());
        let old = add(&repo, &tmp.path().join("wt/old"), "old", None).unwrap();
        assert!(!old.branch_created);
        let from = add(&repo, &tmp.path().join("wt/from"), "from", Some("old")).unwrap();
        assert!(from.branch_created);
        let tracking = add(&clone, &tmp.path().join("wt/r"), "remote-only", None).unwrap();
        assert!(tracking.branch_created);
        assert!(branch_exists(&clone, "remote-only"));
    }

    #[test]
    fn test_add_refusals_create_nothing() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let taken = tmp.path().join("taken");
        fs::create_dir_all(&taken).unwrap();
        add(&repo, &tmp.path().join("a"), "feat", None).unwrap();

        let err = |dir: &str, branch: &str, source: Option<&str>| {
            let dir = tmp.path().join(dir);
            assert!(check_add(&repo, &dir, branch, source).is_err());
            let e = add(&repo, &dir, branch, source).unwrap_err();
            assert!(e.created.is_none());
            format!("{:#}", e.error)
        };
        assert!(err("taken", "x", None).contains("already exists"));
        assert!(err("b", "feat", None).contains("is checked out at"));
        assert!(err("c", "y", Some("nope")).contains("'nope' is not a commit"));
        assert!(!tmp.path().join("b").exists() && !tmp.path().join("c").exists());
        assert!(!branch_exists(&repo, "y"));
        assert!(check_add(&repo, &tmp.path().join("d"), "z", Some("main")).is_ok());
    }

    #[test]
    fn test_add_reports_the_worktree_git_left_when_it_failed() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let hooks = repo.join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        git_cmd(
            &repo,
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        );
        let hook = hooks.join("post-checkout");
        fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

        let e = add(&repo, &tmp.path().join("wt"), "feat", None).unwrap_err();
        let created = e.created.expect("git created the worktree");
        assert!(created.dir.exists() && created.branch_created);
        fs::remove_file(&hook).unwrap();
        undo_add(&repo, &created).unwrap();
        assert!(!created.dir.exists());
        assert!(!branch_exists(&repo, "feat"));
    }

    #[test]
    fn test_add_reports_the_branch_git_left_without_a_worktree() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        git_cmd(&repo, &["branch", "remote-only"]);
        let clone = tmp.path().join("clone");
        git_cmd(
            tmp.path(),
            &[
                "clone",
                "-q",
                repo.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        // Git creates the branch, then cannot create the directory.
        let ro = tmp.path().join("ro");
        fs::create_dir_all(&ro).unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();

        for (repo, branch) in [(&repo, "new"), (&clone, "remote-only")] {
            let e = add(repo, &ro.join(branch), branch, None).unwrap_err();
            let created = e.created.expect("git created the branch");
            assert!(!created.dir.exists() && created.branch_created);
            assert!(branch_exists(repo, branch));
            undo_add(repo, &created).unwrap();
            assert!(!branch_exists(repo, branch), "{branch}");
        }
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn test_undo_add_keeps_what_would_be_lost() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        git_cmd(&repo, &["branch", "old"]);
        let wt = |name: &str| tmp.path().join(name);

        // A new branch goes with its worktree; an existing one stays.
        let new = add(&repo, &wt("new"), "new", None).unwrap();
        undo_add(&repo, &new).unwrap();
        assert!(!wt("new").exists() && !branch_exists(&repo, "new"));
        let old = add(&repo, &wt("old"), "old", None).unwrap();
        undo_add(&repo, &old).unwrap();
        assert!(!wt("old").exists() && branch_exists(&repo, "old"));

        // Uncommitted changes: nothing removed.
        let dirty = add(&repo, &wt("dirty"), "dirty", None).unwrap();
        fs::write(wt("dirty").join("notes.txt"), "x").unwrap();
        let err = format!("{:#}", undo_add(&repo, &dirty).unwrap_err());
        assert!(err.contains("uncommitted changes"), "{err}");
        assert!(wt("dirty").exists() && branch_exists(&repo, "dirty"));

        // Commits only the new branch has: the branch stays.
        let ahead = add(&repo, &wt("ahead"), "ahead", None).unwrap();
        commit(&wt("ahead"), "f.txt", "work");
        let err = format!("{:#}", undo_add(&repo, &ahead).unwrap_err());
        assert!(err.contains("kept branch 'ahead'"), "{err}");
        assert!(!wt("ahead").exists() && branch_exists(&repo, "ahead"));
    }
}
