//! The git side of dropping a task: check its worktrees, then remove them.
//!
//! Each step works on one worktree (its path and repository), so a task with
//! several worktrees drops each one the same way. Blocking (runs git): call
//! from `spawn_blocking`.

use std::path::PathBuf;

use tam_proto::WorktreeReport;
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
