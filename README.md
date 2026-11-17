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

**Member** — a task's worktree, with at most one agent session. A task can hold several members, one per repository, for work spanning e.g. a backend and a frontend: `tam new feat -r api -r web` creates both worktrees, on branch `feat`, and starts an agent in each. Members have a stable id, which is also their agent's id (`$TAM_AGENT_ID`), and a short name within the task: the repository name. Name one as `TASK/MEMBER`; a bare task name means its only member.

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
- **Messages** — `tam msg` sends a message to a running session without attaching, from you or from another agent, and can wait for the reply
- **Peers** — an agent knows the other members of its task (their names, repositories and worktrees) and reaches them with `tam msg`; `tam peers` shows what their agents are doing
- **Custom TUI commands** — bind keys to shell commands (open editor, run tests, etc.)

## Commands

### Task lifecycle

```
tam new NAME                   Create task: branch NAME and its worktree, in the current repo
tam new NAME -s REF            Branch from a specific ref
tam new NAME -p TEXT           Start its agent on a prompt
tam new NAME -d                Don't attach to the agent (--json: print a receipt)
tam new NAME -r REPO -r REPO   ... in each of these repositories instead (one member each)
tam new NAME -r REPO --join    Add a worktree in another repository to the task NAME

tam run NAME                   Start the task's agent, resuming its current session
tam run NAME --new-session     Start a new session instead
tam run NAME --session ID      Resume a given agent session
tam run NAME -p TEXT -d        On a prompt, without attaching (--json: print a receipt)

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

`tam new` names a repository by a path to it (or to any directory in it), or by its project name as `tam ls` and `tam pick` show it (e.g. `api`, or `mnemo (work)` when two projects share a name). It is all or nothing: every repository is checked first (branch free, worktree directory free, `-s REF` present), and if a worktree still cannot be created, the ones created before it are removed again, with the branches created for them. What cannot be removed without losing work is kept, and the error says where. With several repositories, `tam new` attaches to the first one's agent; `tam run`, `tam attach` and the preview act on one member (`NAME/REPO`), `tam stop` and `tam drop` on the whole task.

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

### Messages

```
tam msg NAME TEXT              Send a message to the member's agent (TEXT or stdin)
tam msg NAME TEXT --wait       ...and print its reply (--timeout SECS, default 90)

tam history NAME               The task's agent runs and messages
tam peers                      The other members of your task, and what their agents do
tam peers --all                ...of every task (--json: machine-readable)
```

A Claude agent gets a message right away: when idle it wakes up, when busy it reads it at its next step (after a pending permission prompt is answered). Other agents get it typed in once idle. A member with no running agent gets it when its next agent starts. Sent from inside an agent TAM started, a message comes from that agent, which the recipient can answer with `tam msg`; otherwise it comes from you. The reply `--wait` prints is the recipient's next message back, or its response at the end of the turn that got the message. Two agents answering each other are stopped after 6 messages, until you prompt one of them; an agent sends at most 20 messages per 10 minutes.

An agent TAM starts knows its task. A Claude agent is told when its session starts (again on resume, `/clear` and compaction): its task, its name, and the other members with their repositories and worktrees, so a backend and a frontend session can agree on an API without you relaying. Other agents have their member id in `$TAM_AGENT_ID` and can run `tam peers`. From an agent, `tam msg`, `tam run`, `tam stop` and `tam attach` take the short name of another member of its task (`tam msg frontend ...`), unless a task has that name; `tam peers` prints the name that works from where you run it (`TASK/MEMBER` from outside the task).

### Scripting

For agents and scripts: nothing waits on a terminal, exit statuses tell what happened, and `--json` output has a versioned shape.

```
tam new NAME -p TEXT -d --json Create a task, start its agent on a prompt, print a receipt
tam run NAME -p TEXT -d --json Start the agent on a prompt, print a receipt
tam wait NAME                  Wait until the task's agents stop working; print their states
tam wait NAME -u input -t 30m  ... until they wait for input, for at most 30 minutes
tam result NAME                Print the last response of the task's agents
tam ps --json                  Every task, as JSON
```

`tam new` and `tam run` attach only when stdin and stdout are terminals: otherwise they start the agent and say so. `tam wait` returns once each agent is in one of the `--until` states, comma-separated: `working`, `blocked`, `input`, `idle`, `question`, `error`, `exit` (no agent runs). The default is any state but `working`. `input` takes in `idle`, since agents without hooks never report `input`. It gives up at once when an agent exits or its turn fails, unless `--until` takes `exit` or `error`. `tam result` prints the response that ended the agent's last turn, in full; it is still there once the agent exited, or the daemon restarted. There is none while a turn runs, before the first turn of a run ends, after a failed or interrupted turn, and for agents other than Claude. For `wait` and `result`, NAME is a task (all its members), TASK/MEMBER or a member id. Both report what they find: right after `tam msg`, an agent that has not picked the message up yet is still `input`, with its previous response. `tam msg --wait` waits for the reply instead, and `response_at` tells when a response came.

Claude asks whether to trust a folder it does not know yet, and runs no hook until it is answered: a detached agent in a new worktree waits there (`tam wait` sees `input` after 10s, and `tam result` has nothing). Attach once to answer it.

| Exit status | Meaning | `error.kind` |
|---|---|---|
| 0 | Done | |
| 1 | Failed; `tam wait` gave up; `tam result` has no response | `failed`, `unreachable`, `no_result` |
| 2 | Invalid arguments (reported as text by the argument parser) | |
| 3 | No such task or member, or a task name where one member is needed | `not_found` |
| 4 | `tam wait` timed out | `timeout` |
| 5 | The daemon cannot be reached, or broke off | `daemon` |

With `--json`, stdout holds one JSON document and nothing else, also when the command fails: its `error` is then `{"kind": …, "message": …}`, next to what could still be reported (a command with nothing to report prints only `schema_version` and `error`). Every document has `"schema_version": 1`, raised only when a key is removed, renamed or retyped: new keys may come. Every key is always present, `null` when it has no value.

`tam ps --json`, tasks most urgent first:

```json
{
  "schema_version": 1,
  "tasks": [
    {
      "name": "feat",
      "status": "input",
      "dir": "/home/me/worktrees/app--feat",
      "agent": "claude",
      "context_percent": 34,
      "run_count": 2,
      "last_activity": 1763400000,
      "created_at": 1763300000,
      "members": [
        {
          "id": "m-1a2b3c4d",
          "name": "app",
          "repo": "app",
          "branch": "feat",
          "dir": "/home/me/worktrees/app--feat",
          "status": "input",
          "agent": "claude",
          "health": { "state": "ok", "branch": "feat" },
          "label": "feat",
          "repo_root": "/home/me/src/app",
          "state": "input",
          "pid": 4242,
          "viewers": 0,
          "context_percent": 34,
          "session_id": "8c1f2a90-…",
          "pending": null,
          "turn_error": null,
          "denied": null,
          "last_message": "Fixed the bug: …",
          "last_run": {
            "provider": "claude",
            "session_id": "8c1f2a90-…",
            "started_at": 1763390000,
            "ended_at": null,
            "exit_code": null
          }
        }
      ]
    }
  ]
}
```

- `status` (of a task: its most urgent member's) is `run`, `input`, `block`, `question`, `error`, `idle`, `stale` or `gone`. `state` is the running agent's: `working`, `blocked`, `input`, `idle`, `question` or `error`; `null` without one. A task's `dir`, `agent` and `context_percent` are its first member's.
- `pending` (while `blocked` or `question`) is `{"tool", "summary"}`, `turn_error` (while `error`) `{"kind", "message"}`, `denied` (the last tool call auto mode refused in this turn) `{"tool", "summary", "reason"}`.
- `session_id` is the running agent's session, else the last run's. `last_message` is the start of the agent's last response: `tam result` has all of it. A run's `exit_code` is -1 when it was stopped.
- `health` is what the daemon saw in git: `{"state": "ok", "branch"}`, `{"state": "gone", "cannot_repair"}` or `{"state": "broken", "problem"}`; `null` until checked.
- Before version 1, `tam ps --json` printed the `tasks` array alone, with the keys up to `last_activity` (tasks) and `health` (members).

`tam new --json` and `tam run --json`: the task's members (`new`) or the one started (`run`). `agent` is `null` when none started, `resumed_session` is `null` for a new session.

```json
{
  "schema_version": 1,
  "task": "feat",
  "members": [
    {
      "id": "m-1a2b3c4d",
      "name": "app",
      "label": "feat",
      "repo": "app",
      "branch": "feat",
      "dir": "/home/me/worktrees/app--feat",
      "agent": "claude",
      "resumed_session": null
    }
  ],
  "error": null
}
```

`tam wait --json` and `tam result --json`: `state` is the agent's, or `exit` when none runs; `exit_code` then tells how its last run ended. `response` is the full text that ended the last turn, `response_at` when.

```json
{
  "schema_version": 1,
  "task": "feat",
  "members": [
    {
      "id": "m-1a2b3c4d",
      "name": "app",
      "label": "feat",
      "state": "input",
      "agent": "claude",
      "session_id": "8c1f2a90-…",
      "exit_code": null,
      "pending": null,
      "turn_error": null,
      "response": "Fixed the bug: …",
      "response_at": 1763400000
    }
  ],
  "error": null
}
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

The pane `p` opens tells what the selected task needs from you, without attaching: its agent's state and since when, the permission it waits on (the tool and its input), its question, its last message, why its turn failed, what auto mode refused, its branch against the default branch with its diff stat, its latest messages, and the agent's screen below. `tab` shows the screen on the whole pane. In a task with several members it shows the one that needs you; `←`/`→` show another, and the member keys (`enter`, `s`, `r`, `R`) act on the member shown.

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

Messages (`tam msg`) reach a Claude agent through one more injected hook, which runs in the background and wakes the session (shown as `tam message` in its transcript). Other providers get them typed into their input once idle: as a paste for Codex, as a single line for any other program.

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
