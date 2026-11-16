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

    /// Start an agent in a task, resuming its current session
    ///
    /// The current session is the last one the agent had a turn in (Claude
    /// reports it through its hooks). Agents without hooks (codex) start a
    /// new session unless given one with --session.
    Run {
        /// Task name, or TASK/MEMBER (resolved from current directory if omitted)
        name: Option<String>,

        /// Start a new session (it becomes current once you use it)
        #[arg(long, conflicts_with = "session")]
        new_session: bool,

        /// Resume this agent session instead of the current one
        #[arg(long, value_name = "ID")]
        session: Option<String>,

        /// Agent tool to use (e.g. claude, codex; default: the task's last
        /// agent, else the configured one)
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

    /// Make a worktree created outside tam a task (or a member of one)
    ///
    /// The task owns the worktree from then on: dropping it removes the
    /// worktree, as for any task.
    Adopt {
        /// The worktree, or a directory in it (default: the current directory)
        path: Option<PathBuf>,

        /// Task name (default: the worktree's branch)
        #[arg(short, long, conflicts_with = "join")]
        name: Option<String>,

        /// Add the worktree to this existing task instead
        #[arg(long)]
        join: Option<String>,
    },

    /// Recreate the worktree of a task (or of one member: TASK/MEMBER) that
    /// was deleted outside tam, on its branch
    Repair {
        /// Task name, or TASK/MEMBER
        name: String,
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

    /// Send a message to the agent of a task member
    ///
    /// A Claude agent gets it right away: when idle it wakes up, when busy it
    /// reads it at its next step. Other agents get it typed in once idle. A
    /// member with no running agent gets it when its next agent starts. Sent
    /// from an agent tam started, it comes from that agent's member; else
    /// from the user.
    Msg {
        /// Recipient: task name, TASK/MEMBER or member id
        target: String,

        /// Message text (read from stdin when omitted)
        text: Vec<String>,

        /// Wait for the reply and print it: the recipient's next message back,
        /// or its response at the end of the turn that got the message
        #[arg(short, long)]
        wait: bool,

        /// How long --wait waits for the reply, in seconds
        #[arg(long, default_value_t = 90, requires = "wait")]
        timeout: u64,

        /// Agent id of the sender (set by tam in the agents it starts)
        #[arg(long, env = "TAM_AGENT_ID", hide = true)]
        from: Option<String>,
    },

    /// Show a task's history: its agent runs and messages
    History {
        /// Task name (resolved from current directory if omitted)
        task: Option<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Wait for the agent's messages and print them (run in the background
    /// by the inbox hooks tam injects into the Claude agents it spawns)
    #[command(hide = true)]
    InboxWait {
        /// Agent ID (defaults to $TAM_AGENT_ID; exits quietly if absent)
        #[arg(short, long, env = "TAM_AGENT_ID")]
        agent: Option<String>,

        /// Daemon socket (defaults to $TAM_SOCKET)
        #[arg(long, env = "TAM_SOCKET")]
        socket: Option<PathBuf>,
    },
}
