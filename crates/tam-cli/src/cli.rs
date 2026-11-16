use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "tam",
    about = "Terminal agent multiplexer — manage units of work, not just processes"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Create a new task in the current repository (a worktree on branch NAME)
    /// and start an agent
    New {
        /// Task name, also the name of its branch
        name: String,

        /// Branch from a specific ref (default: the repository's default branch)
        #[arg(short, long)]
        source: Option<String>,

        /// Don't start an agent after creating the task
        #[arg(long)]
        no_start: bool,
    },

    /// Start or resume an agent in a task
    Run {
        /// Task name, or TASK/MEMBER (resolved from current directory if omitted)
        name: Option<String>,

        /// Skip session picker, always start a new session
        #[arg(long)]
        new_session: bool,

        /// Agent tool to use (e.g. claude, codex)
        #[arg(short, long)]
        agent: Option<String>,

        /// Initial prompt text
        prompt: Option<String>,

        /// Extra arguments passed to the agent command
        #[arg(last = true)]
        args: Vec<String>,
    },

    /// Stop the agents of a task (or of one member: TASK/MEMBER)
    Stop {
        /// Task name, or TASK/MEMBER (resolved from current directory if omitted)
        name: Option<String>,
    },

    /// Attach to a running agent
    Attach {
        /// Task name, or TASK/MEMBER (resolved from current directory if omitted)
        name: Option<String>,
    },

    /// Stop a task's agents, remove its worktrees and forget it
    ///
    /// Refuses when uncommitted changes would be lost, or with -b commits no
    /// other branch holds, unless confirmed or forced.
    Drop {
        /// Task name
        name: String,

        /// Also delete the worktrees' git branches
        #[arg(short, long)]
        branch: bool,

        /// Drop even if uncommitted changes or unmerged commits are lost
        #[arg(short, long)]
        force: bool,
    },

    /// List tasks with computed status
    Ps {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Discover projects and worktrees
    Ls {
        /// Directory to search (default: discovery roots)
        path: Option<PathBuf>,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Output paths only, one per line
        #[arg(long)]
        raw: bool,

        /// Output pretty names only, one per line (same format as `tam pick`)
        #[arg(long)]
        porcelain: bool,
    },

    /// Fuzzy project picker
    Pick {
        /// Finder command (overrides config finder, e.g. fzf, "dmenu -l 20")
        #[arg(short = 'F', long)]
        finder: Option<String>,
    },

    /// Remove the agent hooks older versions of tam installed globally
    /// (hooks are now injected into each agent tam spawns)
    Init {
        /// Agent to clean up (e.g. claude)
        #[arg(long)]
        agent: String,
    },

    /// Stop all agents and shut down the daemon
    Shutdown,

    /// Check if the daemon is running
    Status,

    /// Run the daemon (used internally by auto-start)
    #[command(hide = true)]
    Daemon,

    /// Forward a hook event to the daemon (run by the hooks tam injects into
    /// the agents it spawns). Reads the hook's JSON input on stdin.
    #[command(hide = true)]
    HookNotify {
        /// Agent ID (defaults to $TAM_AGENT_ID; exits quietly if absent)
        #[arg(short, long, env = "TAM_AGENT_ID")]
        agent: Option<String>,

        /// Daemon socket (defaults to $TAM_SOCKET, set by the daemon that
        /// spawned the agent)
        #[arg(long, env = "TAM_SOCKET")]
        socket: Option<std::path::PathBuf>,

        /// Legacy event name, passed by the hooks older `tam init` wrote to
        /// the global Claude settings. Such calls are ignored: hooks are
        /// injected per spawn now.
        #[arg(short, long)]
        event: Option<String>,
    },
}
