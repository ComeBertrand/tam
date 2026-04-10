# TAM — Terminal Agent Multiplexer

[![CI](https://github.com/ComeBertrand/tam/actions/workflows/ci.yml/badge.svg)](https://github.com/ComeBertrand/tam/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/tam-cli.svg)](https://crates.io/crates/tam-cli)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

> Manage units of work, not just processes.

TAM manages **tasks** — named units of work that bind a directory to a series of AI agent runs. It unifies git worktree management and agent process supervision into a single tool.

## Quick start

```bash
# Set up Claude Code hooks for state detection
tam init --agent claude

# Create a task with its own worktree
tam new fix-auth -w

# Start an agent in the task (attaches immediately)
tam run fix-auth

# Detach with ctrl-], then check all tasks
tam ps

# Open the TUI dashboard
tam
```

## Key concepts

**Task** — a named unit of work binding a directory to agent runs. Tasks have two flavors:

- **Owned**: TAM creates a git worktree for the task. The task name becomes the branch name. `tam drop` cleans up both.
- **Borrowed**: TAM tracks agents in an existing directory without touching the filesystem.

**Status is always derived**, never stored. TAM checks the daemon (is an agent running?), the filesystem (does the worktree exist?), git (does the branch exist?), and activity timestamps (is the task stale?) every time you look.

| Status | Meaning |
|---|---|
| `● run` | Agent producing output |
| `▲ input` | Agent waiting for user prompt |
| `▲ block` | Agent waiting for permission |
| `○ idle` | No agent running, task exists |
| `◌ stale` | No activity for 30 days |
| `✗ gone` | Worktree or branch deleted externally |

## Commands

### Task lifecycle

```
tam new NAME                   Create task bound to current directory
tam new NAME -w                Create task with its own worktree
tam new NAME -w -s REF         Worktree branched from a specific ref

tam run NAME                   Start/resume an agent in the task
tam run NAME --new-session     Start a fresh session

tam stop NAME                  Kill the agent (task persists)
tam stop                       Resolve task from current directory

tam attach NAME                Full-screen attach to running agent
tam attach                     Resolve from current directory

tam drop NAME                  Kill agent + remove task (+ delete worktree if owned)
tam drop NAME -b               Also delete the git branch
```

### Observing

```
tam ps                         Task table with computed status
tam ps --json                  Machine-readable output

tam ls                         Discover projects and worktrees
tam ls PATH                    Discover under a specific directory

tam pick                       Fuzzy project picker (prints selected path)
```

### TUI

Running `tam` with no arguments opens the dashboard:

```
┌────────────────────────────────────────────────────────────────────────────┐
│  tam — 4 tasks (1 needs input)                                             │
├────────────────────────────────────────────────────────────────────────────┤
│  STATUS    REPO     TASK          AGENT   OWN  DIR                   CTX   │
│  ● run     myapp    feat          claude   ✔   ~/wt/myapp--feat      34%   │
│▸ ▲ input   myapp    fix-nav       claude   ✔   ~/wt/myapp--fix-nav   67%   │
│  ○ idle    myapp    refactor      -        ✔   ~/wt/myapp--refac     -     │
│  ◌ stale   other    old-thing     -        ✘   ~/projects/other      -     │
├────────────────────────────────────────────────────────────────────────────┤
│  enter:attach  n:new  r:run  s:stop  d:drop  p:peek  q:quit               │
└────────────────────────────────────────────────────────────────────────────┘
```

Keys: `j`/`k` navigate, `enter` attaches, `n` creates a task, `r` runs an agent, `s` stops, `d` drops, `p` toggles peek (scrollback preview), `/` filters, `q` quits.

### Setup

```
tam init --agent claude        Configure Claude Code hooks
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
command = "notify-send 'tam: {task}' '{status}'"
on_states = ["input", "blocked"]

[session]
finder = "fzf"

[[tui.commands]]
name = "open in editor"
key = "o"
command = "code {dir}"
```

Per-repo worktree initialization is configured via `.tam.toml` at the project root:

```toml
[init]
include = [".env", ".claude/**"]    # copy untracked files from main checkout
commands = ["npm install"]          # run in the new worktree
```

## Install

### From source

```bash
cargo install tam-cli
```

### GitHub Releases

Download a prebuilt binary from [Releases](https://github.com/ComeBertrand/tam/releases).

### Nix

```bash
nix run github:ComeBertrand/tam
```

## Architecture

Four crates:

| Crate | Role |
|---|---|
| `tam-proto` | Wire protocol types (daemon ↔ client, JSON over Unix socket) |
| `tam-daemon` | PTY management, agent lifecycle, state detection, scrollback |
| `tam-worktree` | Git operations, worktree CRUD, project discovery, pretty names |
| `tam-cli` | CLI/TUI, task ledger, bridges worktrees and agents |

See [`tam_manifesto.md`](tam_manifesto.md) for the full design.

## License

MIT
