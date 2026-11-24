//! Shells (#81): the user's `$SHELL` on a PTY the daemon owns, in a task
//! member's worktree or a repository's main checkout. A [`Pty`] plus where
//! it runs and its number: no provider, hooks, state, ledger runs,
//! messages or notifications, and nothing in the ledger. The daemon keeps
//! them apart from its agents (`DaemonState::shells`), under ids of their
//! own (`sh-xxxxxxxx`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;

use anyhow::Result;
use tam_proto::{AgentEnv, ShellInfo};

use crate::pty::{Launch, Pty};

/// What a shell's number counts in: its member (by id, so a merge or a
/// split keeps it), or its main checkout (resolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    Member(String),
    Checkout(PathBuf),
}

/// The shell run when the environment has no `$SHELL`.
const FALLBACK_SHELL: &str = "/bin/sh";

/// Variables a shell never gets, whatever environment it runs in: what is
/// typed there is the user's, not an agent's (`tam msg` from a shell is
/// from the user; the cwd still tells `tam peers` the task).
const NOT_FOR_SHELLS: [&str; 2] = ["TAM_AGENT_ID", "TAM_SOCKET"];

pub struct Shell {
    pty: Pty,
    place: Place,
    dir: PathBuf,
    repo: String,
    number: u32,
    opened_at: u64,
    /// Opening order, for `ListShells`.
    seq: u64,
}

/// Where and how a shell opens (see [`Shell::open`]).
pub struct Opening {
    pub place: Place,
    /// The member's worktree, or the main checkout.
    pub dir: PathBuf,
    pub repo: String,
    pub number: u32,
    pub seq: u64,
}

impl Shell {
    /// Start the shell (`$SHELL` of the environment `launch` gives, see
    /// [`program`]) on a new PTY in `opening.dir`, through the wrapper.
    pub fn open(opening: Opening, launch: &Launch) -> Result<Self> {
        let program = program(&launch.env);
        let mut cmd = Command::new(&program);
        cmd.current_dir(&opening.dir);
        for name in NOT_FOR_SHELLS {
            cmd.env_remove(name);
        }
        let pty = Pty::spawn(cmd, &opening.dir, launch, &program)?;
        Ok(Self {
            pty,
            place: opening.place,
            dir: opening.dir,
            repo: opening.repo,
            number: opening.number,
            opened_at: crate::ledger::now(),
            seq: opening.seq,
        })
    }

    pub fn info(&self, id: &str) -> ShellInfo {
        ShellInfo {
            id: id.to_string(),
            member: self.member().map(str::to_string),
            dir: self.dir.clone(),
            repo: self.repo.clone(),
            number: self.number,
            pid: self.pty.pid(),
            opened_at: self.opened_at,
            viewers: self.pty.viewers().load(Ordering::Relaxed),
        }
    }

    /// Its process on its PTY, and its screen.
    pub fn pty(&self) -> &Pty {
        &self.pty
    }

    pub fn place(&self) -> &Place {
        &self.place
    }

    /// The task member whose worktree it runs in.
    pub fn member(&self) -> Option<&str> {
        match &self.place {
            Place::Member(id) => Some(id),
            Place::Checkout(_) => None,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn number(&self) -> u32 {
        self.number
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Whether its process exited (`exit`, ctrl-d).
    pub fn has_exited(&mut self) -> bool {
        self.pty.check_exited().is_some()
    }

    /// Hang it up and wait for it (see [`Pty::hang_up`]). Blocking: off
    /// the async runtime, outside the state lock.
    pub fn close(mut self) {
        self.pty.hang_up();
    }
}

/// The program a shell runs: `$SHELL` of the environment it runs in (the
/// client's; the daemon's when the client sent none), else `/bin/sh`.
/// Found on that environment's `PATH` when it is a bare name.
pub fn program(env: &AgentEnv) -> String {
    let shell = match env.is_empty() {
        true => std::env::var("SHELL").ok(),
        false => env.0.get("SHELL").cloned(),
    };
    shell
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| FALLBACK_SHELL.to_string())
}

/// The number of a new shell in a place where `open` are the numbers of
/// the shells open: the highest plus one, 1 when none is open. A number
/// never moves to another shell while one has it.
pub fn next_number(open: impl IntoIterator<Item = u32>) -> u32 {
    open.into_iter().max().map_or(1, |n| n + 1)
}

/// A fresh shell id (`sh-` and 8 hex digits) for which `taken` is false.
pub fn new_id(taken: impl Fn(&str) -> bool) -> String {
    loop {
        let id = format!("sh-{:08x}", crate::ledger::random_u32());
        if !taken(&id) {
            return id;
        }
    }
}

/// Whether `s` has the shape of a shell id.
pub fn is_shell_id(s: &str) -> bool {
    s.strip_prefix("sh-")
        .is_some_and(|hex| hex.len() == 8 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env(vars: &[(&str, &str)]) -> AgentEnv {
        AgentEnv(
            vars.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    #[test]
    fn the_shell_is_the_clients_shell_else_sh() {
        assert_eq!(program(&env(&[("SHELL", "/usr/bin/zsh")])), "/usr/bin/zsh");
        assert_eq!(program(&env(&[("SHELL", "fish")])), "fish");
        assert_eq!(program(&env(&[("PATH", "/bin")])), "/bin/sh");
        assert_eq!(program(&env(&[("SHELL", " ")])), "/bin/sh");
    }

    #[test]
    fn a_shell_takes_the_number_after_the_highest_open() {
        assert_eq!(next_number([]), 1);
        assert_eq!(next_number([1]), 2);
        // #1 closed, #2 open: #3, never #1 again while #2 shows.
        assert_eq!(next_number([2]), 3);
        assert_eq!(next_number([3, 1]), 4);
    }

    #[test]
    fn shell_ids() {
        let id = new_id(|_| false);
        assert!(is_shell_id(&id), "{id}");
        assert!(!is_shell_id("sh-1234"));
        assert!(!is_shell_id("c-0000abcd"));
        assert!(!is_shell_id("sh-0000abcg"));
        // Taken ids are skipped.
        let first = new_id(|_| false);
        let other = new_id(|id| id == first);
        assert_ne!(first, other);
    }
}
