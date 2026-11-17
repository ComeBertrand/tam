//! What git says of a member's worktree for the task list and the detail
//! pane: how its branch compares with the repository's default branch, and
//! what it changed.
//!
//! Read-only and lock-free (`GIT_OPTIONAL_LOCKS=0`: the agent may be running
//! git in the same worktree). Run off the UI thread: git can be slow on a
//! large repository.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// How often the focused member's worktree is looked at again while shown.
pub const REFRESH: Duration = Duration::from_secs(10);
/// How often the other members' worktrees are, for the task list (and after
/// their agent's state changes).
pub const BACKGROUND: Duration = Duration::from_secs(60);

/// A member's branch and changes, as git had them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchState {
    /// The ref compared with (e.g. `origin/main`), when one was found.
    pub base: Option<String>,
    /// Commits on HEAD and not on `base`, and the other way round.
    pub ahead: u32,
    pub behind: u32,
    /// Changes from where HEAD left `base` (from HEAD without one) to the
    /// working tree, committed or not; untracked files left out.
    pub files: u32,
    pub insertions: u32,
    pub deletions: u32,
    /// Paths with uncommitted changes, untracked ones included.
    pub uncommitted: usize,
}

/// The last look at a member's worktree.
#[derive(Debug, Default)]
pub struct Observation {
    /// `Err` holds why git could not tell.
    pub state: Option<Result<BranchState, String>>,
    /// When it was taken; `None` asks for a new one.
    pub at: Option<Instant>,
    /// A look is under way.
    pub running: bool,
}

impl Observation {
    /// Whether to look again, looking `every` so often.
    pub fn due(&self, every: Duration) -> bool {
        !self.running && self.at.is_none_or(|at| at.elapsed() >= every)
    }
}

/// Look at the worktree in `dir`.
pub fn observe(dir: &Path) -> Result<BranchState, String> {
    if !dir.is_dir() {
        return Err(format!("{} not found", dir.display()));
    }
    let base = default_base(dir);
    let mut state = BranchState {
        base: base.clone(),
        ..Default::default()
    };
    let from = match &base {
        Some(base) => {
            let counts = git(
                dir,
                &[
                    "rev-list",
                    "--left-right",
                    "--count",
                    &format!("{base}...HEAD"),
                ],
            )?;
            let mut counts = counts.split_whitespace().map(str::parse::<u32>);
            if let (Some(Ok(behind)), Some(Ok(ahead))) = (counts.next(), counts.next()) {
                state.behind = behind;
                state.ahead = ahead;
            }
            git(dir, &["merge-base", base, "HEAD"])?
        }
        None => "HEAD".to_string(),
    };
    let stat = git(dir, &["diff", "--shortstat", from.trim()])?;
    (state.files, state.insertions, state.deletions) = parse_shortstat(&stat);
    state.uncommitted = git(dir, &["status", "--porcelain=v1"])?
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    Ok(state)
}

/// The repository's default branch, as `origin/<name>` when the remote has
/// it (kept up to date by fetches), else the local branch.
fn default_base(dir: &Path) -> Option<String> {
    let remote_head = git(
        dir,
        &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"],
    )
    .ok()
    .map(|r| r.trim().to_string())
    .filter(|r| !r.is_empty());
    remote_head
        .into_iter()
        .chain(["origin/main", "main", "origin/master", "master"].map(str::to_string))
        .find(|r| {
            git(
                dir,
                &["rev-parse", "--verify", "-q", &format!("{r}^{{commit}}")],
            )
            .is_ok()
        })
}

/// `(files, insertions, deletions)` from `git diff --shortstat`, e.g.
/// ` 3 files changed, 10 insertions(+), 2 deletions(-)`.
fn parse_shortstat(stat: &str) -> (u32, u32, u32) {
    let mut out = (0, 0, 0);
    for part in stat.split(',') {
        let mut words = part.split_whitespace();
        let (Some(n), Some(what)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(n) = n.parse() else { continue };
        if what.starts_with("file") {
            out.0 = n;
        } else if what.starts_with("insertion") {
            out.1 = n;
        } else if what.starts_with("deletion") {
            out.2 = n;
        }
    }
    out
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let why = stderr.lines().next().unwrap_or("").trim();
        return Err(format!("git {} failed: {why}", args[0]));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn commit(dir: &Path, file: &str, text: &str) {
        std::fs::write(dir.join(file), text).unwrap();
        run(dir, &["add", file]);
        run(dir, &["commit", "-q", "-m", file]);
    }

    #[test]
    fn shortstat_parsing() {
        assert_eq!(
            parse_shortstat(" 3 files changed, 10 insertions(+), 2 deletions(-)\n"),
            (3, 10, 2)
        );
        assert_eq!(parse_shortstat(" 1 file changed, 1 deletion(-)"), (1, 0, 1));
        assert_eq!(parse_shortstat(""), (0, 0, 0));
    }

    #[test]
    fn compares_the_worktree_with_the_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "test@test.com"]);
        run(&repo, &["config", "user.name", "Test"]);
        commit(&repo, "a.txt", "a\n");
        let wt = tmp.path().join("repo--feat");
        run(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );

        // Nothing done yet.
        let state = observe(&wt).unwrap();
        assert_eq!(
            state,
            BranchState {
                base: Some("main".into()),
                ..Default::default()
            }
        );

        // Two commits, one more on main, an edit and a new file.
        commit(&wt, "b.txt", "b\nb\n");
        commit(&wt, "c.txt", "c\n");
        commit(&repo, "d.txt", "d\n");
        std::fs::write(wt.join("a.txt"), "changed\n").unwrap();
        std::fs::write(wt.join("new.txt"), "new\n").unwrap();
        let state = observe(&wt).unwrap();
        assert_eq!(state.ahead, 2);
        assert_eq!(state.behind, 1);
        // b, c and a: from the merge base, the uncommitted edit included.
        assert_eq!((state.files, state.insertions, state.deletions), (3, 4, 1));
        assert_eq!(state.uncommitted, 2);
    }

    #[test]
    fn without_a_default_branch_only_uncommitted_changes_count() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        run(repo, &["init", "-q", "-b", "trunk"]);
        run(repo, &["config", "user.email", "test@test.com"]);
        run(repo, &["config", "user.name", "Test"]);
        commit(repo, "a.txt", "a\n");
        std::fs::write(repo.join("a.txt"), "b\n").unwrap();
        let state = observe(repo).unwrap();
        assert_eq!(state.base, None);
        assert_eq!((state.files, state.insertions, state.deletions), (1, 1, 1));
        assert_eq!(state.uncommitted, 1);

        // No worktree there (any more): why, not a panic.
        assert!(observe(&repo.join("gone")).is_err());
    }
}
