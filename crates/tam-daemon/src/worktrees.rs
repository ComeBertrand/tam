//! The git side of task members' worktrees: dropping a task (check its
//! worktrees, then remove them), observing their health against `git worktree
//! list`, and checking a worktree before it is adopted.
//!
//! Each step works on one worktree (its path and repository), so a task with
//! several worktrees drops each one the same way. Blocking (runs git): call
//! from `spawn_blocking`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tam_proto::{MemberHealth, NewMember, WorktreeReport};
use tam_worktree::git::{self, WorktreeEntry};
use tam_worktree::worktree::{self, WorktreeCheck};

/// A task's worktree, checked before the drop.
pub(crate) enum Checked {
    /// The directory no longer exists: nothing to remove.
    Gone { path: PathBuf },
    /// A worktree of `repo`, and what removing it would lose.
    Worktree {
        path: PathBuf,
        repo: PathBuf,
        check: WorktreeCheck,
    },
    /// The directory exists but is not a worktree tam can check (e.g. its
    /// repository moved). Reported as a problem: only a forced drop goes on,
    /// leaving it in place.
    Unknown { path: PathBuf, error: String },
}

/// Check the worktree at `path`. Read-only.
pub(crate) fn check(path: PathBuf) -> Checked {
    if !path.exists() {
        return Checked::Gone { path };
    }
    let checked = tam_worktree::git::repo_root(&path)
        .and_then(|repo| Ok((worktree::check(&repo, &path)?, repo)));
    match checked {
        Ok((check, repo)) => Checked::Worktree { path, repo, check },
        Err(e) => Checked::Unknown {
            path,
            error: format!("{e:#}"),
        },
    }
}

impl Checked {
    /// Why dropping it needs `force`: the work it would lose, or why it
    /// cannot be checked (`None`: nothing).
    pub(crate) fn report(&self, delete_branch: bool) -> Option<WorktreeReport> {
        match self {
            Checked::Gone { .. } => None,
            Checked::Unknown { path, error } => Some(WorktreeReport {
                path: path.clone(),
                branch: None,
                uncommitted: Vec::new(),
                unmerged: Vec::new(),
                problem: Some(error.clone()),
            }),
            Checked::Worktree { path, check, .. } => {
                check.loses_work(delete_branch).then(|| WorktreeReport {
                    path: path.clone(),
                    branch: check.branch.clone(),
                    uncommitted: check.uncommitted.clone(),
                    unmerged: check.lost_commits(delete_branch).to_vec(),
                    problem: None,
                })
            }
        }
    }
}

/// What removing a task's worktrees did.
#[derive(Debug, Default)]
pub(crate) struct Removal {
    /// The worktrees are gone (removed now or before).
    pub deleted: bool,
    /// What was left behind (a directory, a branch git refused to delete).
    pub warnings: Vec<String>,
}

/// Remove checked worktrees, deleting their branch with `delete_branch`.
/// `Err` when a worktree could not be removed: it is still there.
///
/// Callers check first (or were told to force): git's own refusal is
/// overridden, as it also refuses clean worktrees with submodules.
pub(crate) fn remove_all(checked: &[Checked], delete_branch: bool) -> Result<Removal, String> {
    let mut removal = Removal {
        deleted: true,
        warnings: Vec::new(),
    };
    for c in checked {
        remove(c, delete_branch, &mut removal)?;
    }
    Ok(removal)
}

fn remove(checked: &Checked, delete_branch: bool, removal: &mut Removal) -> Result<(), String> {
    match checked {
        Checked::Gone { path } => {
            if delete_branch {
                removal.warnings.push(format!(
                    "{} no longer exists, its branch was not deleted",
                    path.display()
                ));
            }
        }
        Checked::Unknown { path, error } => {
            removal.deleted = false;
            removal
                .warnings
                .push(format!("{} left in place: {error}", path.display()));
        }
        Checked::Worktree { path, repo, check } => {
            worktree::remove(repo, path, true).map_err(|e| format!("{e:#}"))?;
            if !delete_branch {
                return Ok(());
            }
            match &check.branch {
                Some(branch) => {
                    if let Err(e) = worktree::delete_branch(repo, branch) {
                        removal
                            .warnings
                            .push(format!("branch '{branch}' kept: {e:#}"));
                    }
                }
                None => removal.warnings.push(format!(
                    "{} had no branch checked out, no branch deleted",
                    path.display()
                )),
            }
        }
    }
    Ok(())
}

/// A member's worktree as the ledger records it, to observe.
pub(crate) struct Recorded {
    pub dir: PathBuf,
    pub branch: String,
    /// Its repository, when known (recorded, or observed before).
    pub repo: Option<PathBuf>,
}

/// What [`observe`] found of a member's worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Observation {
    pub health: MemberHealth,
    /// The worktree's repository, when found.
    pub repo: Option<PathBuf>,
}

/// Check members' worktrees against `git worktree list`, listing each
/// repository once. One observation per member, in order. Read-only.
///
/// An existing directory is looked up in its known repository's list, else
/// in the list of the repository git finds from the directory itself. A
/// missing one is gone, recreatable or not by [`worktree::plan_recreate`].
pub(crate) fn observe(members: &[Recorded]) -> Vec<Observation> {
    let mut lists = Lists::default();
    members
        .iter()
        .map(|m| {
            if m.dir.exists() {
                observe_present(m, &mut lists)
            } else {
                observe_missing(m, &mut lists)
            }
        })
        .collect()
}

/// `git worktree list` of each repository, run once per [`observe`].
#[derive(Default)]
struct Lists(HashMap<PathBuf, Result<Vec<WorktreeEntry>, String>>);

impl Lists {
    fn get(&mut self, repo: &Path) -> &Result<Vec<WorktreeEntry>, String> {
        self.0
            .entry(repo.to_path_buf())
            .or_insert_with(|| git::worktree_entries(repo).map_err(|e| format!("{e:#}")))
    }

    /// The entry of `repo`'s list for the worktree at `dir`, if listed.
    fn find(&mut self, repo: &Path, dir: &Path) -> Option<WorktreeEntry> {
        let entries = self.get(repo).as_ref().ok()?;
        entries
            .iter()
            .find(|e| worktree::same_path(&e.path, dir))
            .cloned()
    }
}

fn observe_present(m: &Recorded, lists: &mut Lists) -> Observation {
    let listed = |entry: WorktreeEntry, repo: PathBuf| Observation {
        health: if entry.prunable {
            MemberHealth::Broken {
                problem: format!("git would prune {}", m.dir.display()),
            }
        } else {
            MemberHealth::Ok {
                branch: entry.branch,
            }
        },
        repo: Some(repo),
    };
    if let Some(repo) = &m.repo {
        if let Some(entry) = lists.find(repo, &m.dir) {
            return listed(entry, repo.clone());
        }
    }
    // Not where the ledger says: ask git where the directory belongs.
    let broken = |problem: String, repo: Option<PathBuf>| Observation {
        health: MemberHealth::Broken { problem },
        repo,
    };
    let repo = match git::repo_root(&m.dir) {
        Ok(repo) => repo,
        Err(e) => return broken(format!("{e:#}"), None),
    };
    if let Err(e) = lists.get(&repo) {
        return broken(e.clone(), Some(repo));
    }
    match lists.find(&repo, &m.dir) {
        Some(entry) => listed(entry, repo),
        None => broken(
            format!(
                "{} is not a worktree of {}",
                m.dir.display(),
                repo.display()
            ),
            Some(repo),
        ),
    }
}

fn observe_missing(m: &Recorded, lists: &mut Lists) -> Observation {
    let gone = |why: Option<String>, repo: Option<PathBuf>| Observation {
        health: MemberHealth::Gone { cannot_repair: why },
        repo,
    };
    let Some(repo) = &m.repo else {
        return gone(
            Some("its repository is unknown (a task from an older tam)".into()),
            None,
        );
    };
    if !repo.exists() {
        return gone(
            Some(format!("repository {} not found", repo.display())),
            None,
        );
    }
    let why = match lists.get(repo) {
        Ok(entries) => worktree::plan_recreate(repo, &m.dir, &m.branch, entries)
            .err()
            .map(|e| format!("{e:#}")),
        Err(e) => Some(e.clone()),
    };
    gone(why, Some(repo.clone()))
}

/// The linked worktree containing `dir`, as a new task member on the branch
/// it has checked out (paths resolved). Refused for a directory outside any
/// worktree, a main checkout and a detached HEAD. Read-only.
pub(crate) fn adoptable(dir: &Path) -> Result<NewMember> {
    let resolve = |p: &Path| {
        p.canonicalize()
            .with_context(|| format!("cannot resolve {}", p.display()))
    };
    let dir = resolve(dir)?;
    let top = resolve(&git::toplevel(&dir)?)?;
    let repo = resolve(&git::repo_root(&top)?)?;
    // The checks a drop makes: a linked worktree of `repo`, its branch.
    let check = worktree::check(&repo, &top)?;
    let Some(branch) = check.branch else {
        bail!(
            "{} is on a detached HEAD (a rebase in progress?): check out a branch first",
            top.display()
        );
    };
    Ok(NewMember {
        repo_root: repo,
        branch,
        dir: top,
    })
}

/// Check the worktrees to create for new task members, before creating
/// any: each repository is the main checkout of a repository, each
/// directory is free and given once, each branch can be checked out (see
/// [`worktree::check_add`]). Read-only.
pub(crate) fn check_new(members: &[NewMember], source: Option<&str>) -> Result<()> {
    for (i, m) in members.iter().enumerate() {
        let repo = &m.repo_root;
        let at = || repo.display().to_string();
        if !repo.is_dir() {
            bail!("{}: no such directory", repo.display());
        }
        let main = git::toplevel(repo)
            .and_then(|top| Ok((top, git::repo_root(repo)?)))
            .with_context(at)?;
        if !worktree::same_path(&main.0, repo) || !worktree::same_path(&main.1, repo) {
            bail!(
                "{} is not the main checkout of a repository",
                repo.display()
            );
        }
        if let Some(other) = members[..i]
            .iter()
            .find(|o| worktree::same_path(&o.dir, &m.dir))
        {
            bail!(
                "{} and {} would both have their worktree at {}",
                other.repo_root.display(),
                repo.display(),
                m.dir.display()
            );
        }
        if members[..i]
            .iter()
            .any(|o| worktree::same_path(&o.repo_root, repo) && o.branch == m.branch)
        {
            bail!("{} is given twice on branch '{}'", repo.display(), m.branch);
        }
        worktree::check_add(repo, &m.dir, &m.branch, source).with_context(at)?;
    }
    Ok(())
}

/// A worktree [`create_all`] created, in its repository.
pub(crate) struct Created {
    pub repo: PathBuf,
    pub added: worktree::Added,
}

/// Create the worktrees of new task members, in order (see
/// [`worktree::add`]). All or nothing: when one fails, the ones created
/// before it (and that one, if git left it) are undone; `Err` says what
/// failed, then what [`undo_all`] reported.
pub(crate) fn create_all(
    members: &[NewMember],
    source: Option<&str>,
) -> Result<Vec<Created>, String> {
    let mut created = Vec::new();
    for m in members {
        match worktree::add(&m.repo_root, &m.dir, &m.branch, source) {
            Ok(added) => created.push(Created {
                repo: m.repo_root.clone(),
                added,
            }),
            Err(e) => {
                let failed = format!("{}: {:#}", m.repo_root.display(), e.error);
                created.extend(e.created.map(|added| Created {
                    repo: m.repo_root.clone(),
                    added,
                }));
                return Err(format!("{failed}\n{}", undo_all(created)));
            }
        }
    }
    Ok(created)
}

/// Undo worktrees [`create_all`] created, last first (see
/// [`worktree::undo_add`]). Says what was undone, and what is left behind.
pub(crate) fn undo_all(created: Vec<Created>) -> String {
    if created.is_empty() {
        return "Nothing was created.".into();
    }
    let mut removed = Vec::new();
    let mut left = Vec::new();
    for c in created.iter().rev() {
        let what = match (c.added.dir.exists(), c.added.branch_created) {
            (true, true) => format!(
                "{} (and its new branch '{}')",
                c.added.dir.display(),
                c.added.branch
            ),
            (true, false) => c.added.dir.display().to_string(),
            // Git created the branch, not the worktree.
            (false, _) => format!("branch '{}' of {}", c.added.branch, c.repo.display()),
        };
        match worktree::undo_add(&c.repo, &c.added) {
            Ok(()) => removed.push(what),
            Err(e) => left.push(format!("{what}: {e:#}")),
        }
    }
    let mut report = Vec::new();
    if !removed.is_empty() {
        report.push(format!(
            "Removed what was created meanwhile: {}.",
            removed.join(", ")
        ));
    }
    if !left.is_empty() {
        report.push(format!(
            "Left behind, in no task:\n  {}\nRemove them (git worktree remove, git branch -D) \
             or make them tasks (tam adopt).",
            left.join("\n  ")
        ));
    } else {
        report.push("Nothing was left behind.".into());
    }
    report.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn git_in(dir: &Path, args: &[&str]) {
        let out = tam_worktree::git::command()
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

    /// A repository `repo` under a temp dir, with one commit on `main`.
    fn repo() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git_in(&repo, &["init", "-q", "-b", "main"]);
        git_in(&repo, &["config", "user.email", "t@t"]);
        git_in(&repo, &["config", "user.name", "t"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        (tmp, repo)
    }

    /// Add a worktree on a new branch `branch`, next to the repository.
    fn add(repo: &Path, branch: &str) -> Recorded {
        let dir = repo.parent().unwrap().join(format!("repo--{branch}"));
        git_in(
            repo,
            &["worktree", "add", "-q", "-b", branch, dir.to_str().unwrap()],
        );
        Recorded {
            dir,
            branch: branch.into(),
            repo: Some(repo.to_path_buf()),
        }
    }

    fn health(m: &Recorded) -> MemberHealth {
        observe(std::slice::from_ref(m)).remove(0).health
    }

    #[test]
    fn worktree_in_place_is_ok_whatever_its_branch() {
        let (_tmp, repo) = repo();
        let feat = add(&repo, "feat");
        let observed = observe(std::slice::from_ref(&feat));
        assert_eq!(
            observed,
            [Observation {
                health: MemberHealth::Ok {
                    branch: Some("feat".into())
                },
                repo: Some(repo.clone()),
            }]
        );
        // Switched, or detached (e.g. mid-rebase): still usable.
        git_in(&feat.dir, &["checkout", "-q", "-b", "other"]);
        assert_eq!(
            health(&feat),
            MemberHealth::Ok {
                branch: Some("other".into())
            }
        );
        git_in(&feat.dir, &["checkout", "-q", "--detach"]);
        assert_eq!(health(&feat), MemberHealth::Ok { branch: None });
    }

    #[test]
    fn removed_or_deleted_worktree_is_gone_and_repairable() {
        let (_tmp, repo) = repo();
        let removed = add(&repo, "removed");
        let deleted = add(&repo, "deleted");
        git_in(
            &repo,
            &["worktree", "remove", removed.dir.to_str().unwrap()],
        );
        std::fs::remove_dir_all(&deleted.dir).unwrap();
        let repairable = MemberHealth::Gone {
            cannot_repair: None,
        };
        assert_eq!(health(&removed), repairable);
        assert_eq!(health(&deleted), repairable);
    }

    #[test]
    fn gone_worktree_that_cannot_be_recreated_says_why() {
        let (tmp, repo) = repo();
        let moved = add(&repo, "moved");
        let to = tmp.path().join("elsewhere");
        git_in(
            &repo,
            &[
                "worktree",
                "move",
                moved.dir.to_str().unwrap(),
                to.to_str().unwrap(),
            ],
        );
        let cannot = |m: &Recorded| match health(m) {
            MemberHealth::Gone {
                cannot_repair: Some(why),
            } => why,
            other => panic!("expected gone, unrepairable: {other:?}"),
        };
        assert!(cannot(&moved).contains("checked out at"));

        let deleted = add(&repo, "deleted");
        git_in(
            &repo,
            &["worktree", "remove", deleted.dir.to_str().unwrap()],
        );
        git_in(&repo, &["branch", "-D", "deleted"]);
        assert!(cannot(&deleted).contains("no longer exists"));

        let unknown = Recorded {
            repo: None,
            ..add(&repo, "unknown")
        };
        std::fs::remove_dir_all(&unknown.dir).unwrap();
        assert!(cannot(&unknown).contains("unknown"));

        let lost_repo = Recorded {
            dir: tmp.path().join("nope"),
            branch: "x".into(),
            repo: Some(tmp.path().join("no-repo")),
        };
        assert!(cannot(&lost_repo).contains("not found"));
    }

    #[test]
    fn directory_that_is_not_a_worktree_is_broken() {
        let (tmp, repo) = repo();
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let sub = add(&repo, "feat").dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        for dir in [plain, sub, repo.join("missing-but-not")] {
            std::fs::create_dir_all(&dir).unwrap();
            let m = Recorded {
                dir,
                branch: "feat".into(),
                repo: Some(repo.clone()),
            };
            assert!(
                matches!(health(&m), MemberHealth::Broken { .. }),
                "{}: {:?}",
                m.dir.display(),
                health(&m)
            );
        }
    }

    #[test]
    fn repository_is_found_from_the_directory_when_unknown() {
        let (_tmp, repo) = repo();
        let feat = Recorded {
            repo: None,
            ..add(&repo, "feat")
        };
        let observed = observe(std::slice::from_ref(&feat));
        assert!(matches!(observed[0].health, MemberHealth::Ok { .. }));
        assert_eq!(
            observed[0]
                .repo
                .as_deref()
                .map(Path::canonicalize)
                .transpose()
                .unwrap(),
            Some(repo.canonicalize().unwrap())
        );
    }

    #[test]
    fn paths_through_a_symlink_match_git_s() {
        let (tmp, repo) = repo();
        add(&repo, "feat");
        let links = TempDir::new().unwrap();
        let link = links.path().join("link");
        std::os::unix::fs::symlink(tmp.path(), &link).unwrap();
        let via_link = Recorded {
            dir: link.join("repo--feat"),
            branch: "feat".into(),
            repo: Some(link.join("repo")),
        };
        assert!(matches!(health(&via_link), MemberHealth::Ok { .. }));
        // Gone, through the symlink too: its stale registration is found.
        std::fs::remove_dir_all(tmp.path().join("repo--feat")).unwrap();
        assert_eq!(
            health(&via_link),
            MemberHealth::Gone {
                cannot_repair: None
            }
        );
    }

    #[test]
    fn adoptable_worktrees() {
        let (tmp, repo) = repo();
        let feat = add(&repo, "feat");
        std::fs::create_dir_all(feat.dir.join("src")).unwrap();
        // From the worktree or a directory in it.
        for dir in [feat.dir.clone(), feat.dir.join("src")] {
            let member = adoptable(&dir).unwrap();
            assert_eq!(member.dir, feat.dir.canonicalize().unwrap());
            assert_eq!(member.repo_root, repo.canonicalize().unwrap());
            assert_eq!(member.branch, "feat");
        }

        let err = |dir: &Path| format!("{:#}", adoptable(dir).unwrap_err());
        assert!(err(&repo).contains("main checkout"), "{}", err(&repo));
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(
            err(&plain).contains("not a git repository"),
            "{}",
            err(&plain)
        );
        assert!(err(&tmp.path().join("missing")).contains("cannot resolve"));
        git_in(&feat.dir, &["checkout", "-q", "--detach"]);
        assert!(
            err(&feat.dir).contains("detached HEAD"),
            "{}",
            err(&feat.dir)
        );
    }
}
