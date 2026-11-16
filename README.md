<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/logo.svg">
    <img alt="TAM" src="assets/logo.svg" width="243">
  </picture>
  <br>
  <strong>Terminal Agent Multiplexer</strong>
</p>

<p align="center">
  <a href="https://github.com/ComeBertrand/tam/actions/workflows/ci.yml"><img src="https://github.com/ComeBertrand/tam/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://crates.io/crates/tam-cli"><img src="https://img.shields.io/crates/v/tam-cli.svg" alt="crates.io"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
</p>

<p align="center">
  <a href="#features">Features</a> ·
  <a href="#install">Install</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#commands">Commands</a> ·
  <a href="#configuration">Configuration</a> ·
  <a href="#agent-providers">Providers</a>
</p>

> Manage units of work, not just processes.

TAM manages **tasks** — named units of work, each with its own git worktree and a series of AI agent runs. It unifies git worktree management and agent process supervision into a single tool.

Running multiple AI coding agents means juggling tmux panes, manual worktree creation, and remembering which session to resume. TAM replaces that with a single abstraction: create a task, and it gets a worktree, a branch, and an agent — all under one name. Status is always derived from reality (daemon, git, filesystem), never stored, so it can't drift.

## Quick start

```bash
# Create a task — this creates branch "fix-auth" and worktree "myapp--fix-auth"
tam new fix-auth

# Start an agent and attach to it (full-screen, like tmux)
tam run fix-auth

# Detach with ctrl-] — the agent keeps running in the background
# Check all tasks at a glance
tam ps

# Or open the interactive TUI dashboard
tam
```

## Key concepts

**Task** — a named unit of work. TAM creates a git worktree for it, on a branch named after the task, and `tam drop` cleans it up.

**Member** — a task's worktree, with at most one agent session. A task is built to hold several members (one per repository, for work spanning e.g. a backend and a frontend); for now `tam new` creates one. Members have a stable id, which is also their agent's id (`$TAM_AGENT_ID`), and a short name within the task: the repository name. Name one as `TASK/MEMBER`; a bare task name means its only member.

**Status is always derived**, never stored. TAM checks the daemon (is an agent running?), the filesystem (does the worktree exist?), git (does the branch exist?), and activity timestamps (is the task stale?) every time you look.

| Status | Meaning |
|---|---|
| `● run` | Agent producing output |
| `▲ input` | Agent waiting for user prompt |
| `▲ block` | Agent waiting for permission (or plan approval) |
| `? question` | Agent asked the user a question |
| `✗ error` | Agent's last turn failed (API error, rate limit…) |
| `○ idle` | No agent running, task exists |
| `◌ stale` | No activity for 30 days |
| `✗ gone` | Worktree deleted outside tam, or no longer a worktree |

## Features

- **Task = worktree + agent** — `tam new feat` creates a branch, a worktree, and optionally starts an agent, all under one name
- **Derived status** — never stores lifecycle state; computes it from the daemon, git, and filesystem every time you look
- **TUI dashboard** — real-time task table with peek mode (the agent's live screen and its history, without attaching)
- **Session resume** — a task picks up its current session with no question asked, or starts a fresh one on request; session history is tracked in an append-only ledger
- **Auto-daemon** — the daemon starts on first command and shuts down after 30s idle; no manual management
- **Per-repo init** — `.tam.toml` copies untracked files and runs setup commands in new worktrees
- **Desktop notifications** — configurable alerts when agents need input or hit permission prompts
- **Custom TUI commands** — bind keys to shell commands (open editor, run tests, etc.)

## Commands

### Task lifecycle

```
tam new NAME                   Create task: branch NAME and its worktree, in the current repo
tam new NAME -s REF            Branch from a specific ref

tam run NAME                   Start the task's agent, resuming its current session
tam run NAME --new-session     Start a new session instead
tam run NAME --session ID      Resume a given agent session

tam stop NAME                  Kill the task's agents (task persists)
tam stop                       Resolve the member from the current directory

tam attach NAME                Full-screen attach to running agent
tam attach                     Resolve from current directory

tam drop NAME                  Stop agents + remove worktrees + forget task
tam drop NAME -b               Also delete the git branches
tam drop NAME -f               Drop even if work would be lost

tam adopt [PATH]               Make a worktree created outside tam a task
tam adopt PATH -n NAME         ... named NAME (default: its branch)
tam adopt PATH --join TASK     ... or a new member of an existing task
tam repair NAME                Recreate a worktree deleted outside tam

NAME is a task name, or TASK/MEMBER for one member of the task.
```

`tam drop` checks first: it refuses when uncommitted changes would be lost, or, with `-b`, commits that no other branch, tag or remote branch holds. It lists them and, on a terminal, asks before going on. If a worktree cannot be removed, the task is kept.

The daemon checks every task's worktree against git (`git worktree list`) when it starts and every 10 seconds. A worktree deleted outside tam (`rm -rf`, `git worktree remove`) shows as `gone` in `tam ps` and the dashboard, with what can be done. `tam repair` recreates it on its branch: only if the branch still exists, is checked out nowhere else, and git's leftover registration of the directory (if any) has no staged changes. `tam drop` forgets the task. A worktree switched to another branch is not gone; `tam ps` notes the switch. TAM only looks: nothing is recreated or removed unless you ask.

`tam adopt` takes over a worktree made by hand (`git worktree add`), from the worktree or any directory in it: it must be a linked worktree (not the main checkout) on a branch. From then on it is a task like any other: `tam drop` removes it.

### Observing

```
tam ps                         Task table with computed status
tam ps --json                  Machine-readable output

tam ls                         Discover projects and worktrees
tam ls PATH                    Discover under a specific directory

tam pick                       Fuzzy project picker (prints selected path)
tam pick -F "dmenu -l 20"     Use a custom finder instead of fzf
```

### TUI

Running `tam` with no arguments opens the dashboard:

```
┌────────────────────────────────────────────────────────────────────────────┐
│  tam — 4 tasks (1 needs input)                                             │
├────────────────────────────────────────────────────────────────────────────┤
│  STATUS    REPO     TASK          AGENT   DIR                        CTX   │
│  ● run     myapp    feat          claude  ~/wt/myapp--feat           34%   │
│▸ ▲ input   myapp    fix-nav       claude  ~/wt/myapp--fix-nav        67%   │
│  ○ idle    myapp    refactor      -       ~/wt/myapp--refactor       -     │
│  ◌ stale   other    old-thing     -       ~/wt/other--old-thing      -     │
├────────────────────────────────────────────────────────────────────────────┤
│  enter:attach  n:new  r:run  R:new session  s:stop  d:drop  p:peek  q:quit │
└────────────────────────────────────────────────────────────────────────────┘
```

Keys: `j`/`k` navigate, `enter` attaches, `n` creates a task, `r` runs the agent in its current session, `R` in a new session, `s` stops, `d` drops (then `y` keeps the branch, `b` deletes it; work that would be lost is shown for a second confirmation; on a `gone` task, `r` recreates the worktree instead), `p` toggles peek (the agent's screen, kept by the daemon at the agent's terminal size; `PgUp`/`PgDn` scroll through its history, `Home` goes to the oldest line, `End` back to live), `/` filters, `q` quits. When attached to an agent, `ctrl-]` detaches and returns to the dashboard.

Tasks are sorted by repository name, then by status priority (blocked/question → error → input → running → idle → stale → gone), then by task name.

### Setup

```
tam init --agent claude        Remove the Claude hooks older versions installed globally
tam shutdown                   Stop all agents and kill the daemon
tam status                     Check if the daemon is running
```

## Configuration

TAM reads `~/.config/tam/config.toml`:

```toml
[spawn]
default_agent = "claude"

[worktree]
root = "~/worktrees"
auto_init = true              # run .tam.toml after worktree creation

[discovery]
max_depth = 5
ignore = [".*", "node_modules", "target"]

[daemon]
scrollback = 1048576

[notify]
command = "notify-send 'tam: {task}' '{kind}: {message}'"
on_states = ["input", "blocked", "question", "error"]   # default

[session]
finder = "fzf"

[[tui.commands]]
name = "open in editor"
key = "o"
command = "code {dir}"
```

The notify command runs with `sh -c` on state changes, except while you are attached to the agent. Placeholders are escaped for the quoting they sit in:

| Placeholder | Value |
|---|---|
| `{task}`, `{id}` | Task name, agent id (the member id) |
| `{state}` (alias `{status}`), `{old_state}` | New and previous agent state |
| `{kind}` | `question`, `permission`, `done` or `error` |
| `{message}` | The question or permission asked, the agent's last response, or the error |
| `{tool}` | Tool waiting for permission (e.g. `Bash`) |
| `{error}` | Error kind (e.g. `rate_limit`) |

Per-repo worktree initialization is configured via `.tam.toml` at the project root. When `tam new NAME` creates a worktree, it copies files matching `include` globs from the main checkout (useful for untracked secrets or editor config) and runs `commands` inside the new worktree:

```toml
[init]
include = [".env", ".claude/**"]
commands = ["npm install"]
```

## Agent providers

| Provider | State detection | Context tracking | Setup |
|---|---|---|---|
| **Claude Code** | Hook-based (immediate) | Reads the session transcript for token usage | None |
| **Codex** | PTY heuristic (5s idle) | Reads session JSONL | None |
| **Any CLI** | PTY heuristic (5s idle) | None | None |

Claude Code reports state changes through hooks that TAM injects into each agent it spawns (`claude --settings`, nothing is written to your Claude settings): TAM knows the moment an agent finishes, asks a question, needs a permission or fails on an API error. Claude runs no hooks until a folder is trusted, so a new worktree shows `input` while Claude's trust prompt waits. An interrupted turn fires no hook; TAM takes a working agent silent for 10s as back at the prompt. If you ran `tam init --agent claude` with an older version, run it again to remove the global hooks it installed. Other providers fall back to a PTY idle-time heuristic. Any command-line program can be used as a provider: `tam run feat -a my-tool`.

Each task member has a current session per agent, which `tam run` and `r` resume: for Claude Code, the last session the agent had a turn in, as its hooks report it (so a `/clear` followed by a prompt moves it). `tam run --new-session` and `R` start a new one, which becomes current once you use it. A session whose transcript Claude deleted is not resumed: TAM says so and you start a new one. Codex and other agents report no sessions: they start a new session unless given one (`tam run --session ID`, codex only).

## Install

**Requirements**: git is required at runtime for all worktree and branch operations.

### Pre-built binaries

Download a binary for your platform from [GitHub Releases](https://github.com/ComeBertrand/tam/releases) (Linux x86_64/aarch64, macOS x86_64/aarch64).

### Cargo

```bash
cargo install tam-cli
```

### Nix

```bash
nix run github:ComeBertrand/tam
```

### Shell completions and man page

The build generates completions for bash, zsh, and fish, plus a man page (`tam.1`). When installing via `cargo install`, these are not placed automatically. After building from source, find them under the build output directory:

```bash
cargo build --release
find target/release/build/tam-cli-* -name 'completions' -o -name 'man'
```

## Contributing

Contributions are welcome — open an issue or submit a pull request. For the full design rationale and architecture, see [`tam_manifesto.md`](tam_manifesto.md).

## License

MIT
