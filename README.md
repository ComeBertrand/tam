<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/logo.svg">
    <img alt="TAM" src="assets/logo.svg" width="243">
  </picture>
  <br>
  <strong>Terminal Agent Multiplexer</strong>
  <br>
  Run Claude Code agents in parallel, each in its own git worktree, from one terminal dashboard.
</p>

<p align="center">
  <a href="https://github.com/ComeBertrand/tam/actions/workflows/ci.yml"><img src="https://github.com/ComeBertrand/tam/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://crates.io/crates/tam-cli"><img src="https://img.shields.io/crates/v/tam-cli.svg" alt="crates.io"></a>
  <img src="https://img.shields.io/badge/platform-Linux%20%7C%20macOS-lightgrey.svg" alt="Platform: Linux | macOS">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
</p>

<p align="center">
  <a href="#install">Install</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#the-dashboard">Dashboard</a> ·
  <a href="#concepts">Concepts</a> ·
  <a href="#commands">Commands</a> ·
  <a href="docs/README.md">Documentation</a>
</p>

---

tam gives every piece of work you hand to an AI agent a name: a **task**. A task is a git branch, its worktree and a [Claude Code](https://github.com/anthropics/claude-code) session, created, resumed and cleaned up together. A small daemon keeps the agents running while you come and go, and a dashboard shows at a glance which ones need you.

```
 tam  4 chats  7 tasks  5 need you                                by repository
 STATUS          TASK                                          BRANCH  CTX
 chats ─────────────────────────────────────────────────────────────────────────
▌◆ ready      2m Release deployment on staging                 backend  41% ━━━━
 ⠹ run       10s Why rounding differs between runs             backend  12% ━━━━
 ? question   5m Answering the support thread ∞                backend  63% ━━━━
 ○ idle       2d Chat design ◔                                 tam
 shells ────────────────────────────────────────────────────────────────────────
                 Shell #1                                      backend
                 Shell #1                                      frontend
 backend ───────────────────────────────────────────────────────────────────────
 ▲ block      3m fix-allotment-rounding                         ↑2 *1   67% ━━━━
                  ├ Shell #1
                  └ Shell #3
 ⠹ run       20s perf                                           =       31% ━━━━
 ◌ stale     45d spike-cache                                    ↑5 ↓12
 backend + frontend ────────────────────────────────────────────────────────────
                 unit-pricing
 ⠹ run        1m  ├ backend                                     ↑1 *3   12% ━━━━
                  │ └ Shell #1
 ? question   5m  └▸frontend                                    ↑1      40% ━━━━
 frontend ──────────────────────────────────────────────────────────────────────
 ○ idle       2d nav                                            =
 ∅ gone       3d old-banner
 rustpallet ────────────────────────────────────────────────────────────────────
 ◆ ready     12m offload-explain                                ↑3 ↓1   55% ━━━━
 enter:type  f:full screen  x:stop  d:close  p:keep  N:rename  c:new chat  ?:help
```

## Why tam

Running several coding agents at once usually means juggling tmux panes, creating worktrees by hand and remembering which session went with which branch. With tam:

- **One name per piece of work.** `tam new fix-auth` creates the branch, the worktree and the agent. `tam drop fix-auth` removes them, after checking that no work would be lost.
- **Agents outlive your terminal.** Detach, close the terminal, come back from anywhere: the daemon keeps every agent running.
- **Know who needs you.** Each agent's state (working, waiting for a prompt, blocked on a permission, asking a question, failed) comes live from Claude Code's own hooks, with optional desktop notifications.
- **Act without switching.** From the dashboard, watch any agent's screen, type into it, or answer a permission, a plan or a question.
- **Span repositories.** One task can hold a worktree and an agent in each of several repositories. The agents know about each other and message each other.
- **Script it.** Every command runs without a terminal, with `--json` output and meaningful exit statuses, so scripts and agents can drive tam too.
- **Nothing to set up.** No hooks to install, no configuration required, no daemon to manage: it starts on first use and exits when idle.

## Install

tam runs on **Linux** and **macOS**. It needs:

- [git](https://git-scm.com/)
- [Claude Code](https://github.com/anthropics/claude-code), installed and logged in: `claude` on your `PATH`

Then install tam one of these ways:

| Method | |
|---|---|
| **Pre-built binary** | Download the archive for your platform (x86_64 or aarch64) from the [latest release](https://github.com/ComeBertrand/tam/releases/latest), extract it, and put `tam` on your `PATH`. |
| **Cargo** | `cargo install tam-cli` (Rust 1.88 or later) |
| **Nix** | `nix profile install github:ComeBertrand/tam`, or try it first with `nix run github:ComeBertrand/tam` |

`tam --help` lists the commands. The Nix package also installs shell completions (bash, zsh, fish) and a man page; a build from source generates them under `target/release/build/tam-cli-*/out/`.

## Quick start

**1. Start a task.** In any git repository:

```bash
cd ~/src/myapp
tam new fix-auth
```

tam creates the branch `fix-auth` from the default branch, a worktree for it in `~/worktrees/myapp--fix-auth`, and starts Claude Code there, full screen. Claude Code may first ask whether you trust the new folder: say yes, then tell it what to do. You can also give the prompt up front: `tam new fix-auth -p "Fix the login redirect loop"`.

**2. Detach.** Press `ctrl-]`. The agent keeps working in the background.

**3. Watch.** Run `tam` to open the dashboard: every agent, its status and its live screen. Select one and press `enter` to type into it, `ctrl-]` to come back to the list, `q` to quit. `tam ps` prints the same list as text.

**4. Come back.** `tam attach fix-auth` returns to a running agent. If it was stopped (or you rebooted), `tam run fix-auth` starts it again and resumes its session.

**5. Clean up.** Once the work is merged:

```bash
tam drop fix-auth       # stop the agent and remove the worktree (-b: delete the branch too)
```

`tam drop` refuses when uncommitted changes would be lost (with `-b`, also commits no other branch holds), lists them, and asks before going on.

## The dashboard

`tam` with no arguments opens it. Chats and shells come first, then the tasks, grouped by repository, the ones that need you first. The pane, beside or below the list, shows the selected agent's screen, live.

| Key | Action |
|---|---|
| `j` / `k` | Move through the list |
| `enter` | Type into the agent shown in the pane (`ctrl-]` gives the keyboard back) |
| `f` | Attach full screen (`ctrl-]` comes back) |
| `a` | Answer what the agent asks: a permission, a plan, a question |
| `n` | New task, in one or several repositories |
| `c` | New chat, fresh or from the repository's Claude history |
| `!` | Open a shell in the selected worktree |
| `r` / `R` | Start the agent: resume its session / start a new one |
| `x` | Stop the agent |
| `d` | Drop the task |
| `v` | Scroll back through the agent's screen |
| `/` | Filter |
| `t` | Group by repository, or most urgent first |
| `?` | Every key |
| `q` | Quit (the agents keep running) |

The mouse works too: click to select, scroll the pane, drag over it to copy. The full tour is in [docs/dashboard.md](docs/dashboard.md).

## Concepts

| | |
|---|---|
| **Task** | A named unit of work: a branch, its worktree and its Claude Code session. `tam new NAME` makes one, `tam drop NAME` cleans it up. |
| **Member** | One worktree of a task, with at most one agent. A task has one member per repository: `tam new feat -r api -r web` makes `feat/api` and `feat/web`. |
| **Chat** | A Claude session on a repository's main checkout, with no branch or worktree of its own: for questions, debugging, a deployment. `tam chat`, or `c` in the dashboard. Closed after 3 days without use, unless kept. |
| **Shell** | Your `$SHELL`, kept running by the daemon in a worktree or a main checkout. `tam shell`, or `!` in the dashboard. |
| **Daemon** | Owns the agents and shells. Starts with your first command and exits 30 seconds after the last agent, shell and client are gone. |

### Status

tam never stores a status. It derives it every time from the agent, git and recent activity, so what you see cannot drift from reality.

| Status | Meaning |
|---|---|
| `● run` | Agent producing output |
| `◆ ready` | Agent waiting for your prompt |
| `▲ block` | Agent waiting for a permission (or a plan approval) |
| `? question` | Agent asked you a question |
| `✗ error` | Agent's last turn failed (API error, rate limit…) |
| `○ idle` | No agent running |
| `◌ stale` | No activity for 30 days |
| `∅ gone` | Worktree deleted outside tam, or no longer a worktree |

## Going further

### One task, several repositories

```bash
tam new checkout-v2 -r api -r web -p "Add the checkout v2 endpoint and its page"
```

Each repository gets a worktree on branch `checkout-v2` and an agent of its own. Each agent is told about the others when it starts (their repository, their worktree, how to reach them), can read their worktrees, and can message them, so a backend agent and a frontend agent agree on an API without you relaying. `-r` takes a path, or a project name as `tam ls` shows it. tam attaches you to the first agent; reach the other from the dashboard or with `tam attach checkout-v2/web`. See [Tasks](docs/tasks.md) and [Agents working together](docs/collaboration.md).

### Messages

```bash
tam msg fix-auth "Also add a regression test"
tam msg fix-auth "Is the migration reversible?" --wait    # and print its reply
```

An idle agent wakes up to read it; a busy one reads it at its next step. Agents use `tam msg` to reach each other too.

### Chats and shells

```bash
tam chat              # a Claude session on this repository's main checkout
tam shell fix-auth    # a shell in the task's worktree, kept running by the daemon
```

See [Chats](docs/chats.md) and [Shells](docs/shells.md).

### Scripting

Every command works without a terminal: `tam new` and `tam run` start the agent and return, exit statuses say what happened, and `--json` prints a versioned document.

```bash
tam new fix-auth -p "Fix the login redirect loop" -d --json
tam wait fix-auth      # until the agent stops working
tam result fix-auth    # its last response, in full
```

One catch: Claude Code may ask whether to trust a new worktree, and a detached agent waits on that question (it shows as `ready`, with no result). Attach once to answer it.

Exit statuses and every JSON document are described in [docs/scripting.md](docs/scripting.md).

### Configuration

None is required. When you want some, tam reads `~/.config/tam/config.toml` (or `$XDG_CONFIG_HOME/tam/config.toml`), and a `.tam.toml` at the root of each repository:

```toml
# ~/.config/tam/config.toml
[worktree]
root = "~/worktrees"                # where worktrees are created (the default)
auto_init = true                    # run each repository's .tam.toml in its new worktrees

[spawn]
wrapper = ["direnv", "exec", "."]   # start agents in the worktree's dev environment

[notify]
command = "notify-send 'tam: {task}' '{kind}: {message}'"

[[tui.commands]]                    # your own dashboard keys
name = "open in editor"
key = "o"
command = "code {dir}"
```

```toml
# .tam.toml, at the root of a repository
[init]
include = [".env", ".claude/**"]    # untracked files copied into each new worktree
commands = ["npm install"]          # then run there
```

Every option is in [docs/configuration.md](docs/configuration.md).

## Commands

| Command | |
|---|---|
| `tam` | Open the dashboard |
| `tam new NAME [-r REPO]... [-p TEXT] [-d]` | Create a task and start its agents |
| `tam run [NAME] [--new-session]` | Start a task's agent, resuming its session |
| `tam attach [NAME]` | Attach to a running agent or shell (`ctrl-]` detaches) |
| `tam stop [NAME]` | Stop a task's agents (the task stays) |
| `tam drop NAME [-b]` | Stop the agents, remove the worktrees (`-b`: and the branches), forget the task |
| `tam ps` | List chats, shells and tasks with their status |
| `tam msg NAME TEXT [--wait]` | Send a message to an agent |
| `tam chat [REPO]` | Start a chat on a repository's main checkout |
| `tam shell [NAME]` | Open a shell in a task's worktree |
| `tam wait NAME` / `tam result NAME` | Wait until agents stop working / print their last response |
| `tam adopt`, `tam repair` | Make a worktree created by hand a task / recreate a deleted one |
| `tam merge`, `tam split` | Move members between tasks |

NAME is a task, `TASK/MEMBER` for one member of a task, or a chat. Without it, `run`, `stop` and `attach` act on the task whose worktree you are in. `tam help COMMAND` gives the details.

## Documentation

| Guide | |
|---|---|
| [Tasks](docs/tasks.md) | Creating, dropping, adopting, repairing, merging and splitting tasks |
| [Dashboard](docs/dashboard.md) | Every key, the pane, answering requests, the mouse |
| [Chats](docs/chats.md) | Claude sessions on a repository's main checkout |
| [Shells](docs/shells.md) | Shells the daemon keeps running |
| [Agents working together](docs/collaboration.md) | Messages, peers, and the task's brief |
| [Scripting](docs/scripting.md) | Running without a terminal, exit statuses, JSON output |
| [Configuration](docs/configuration.md) | Every option, notifications, per-repository setup |
| [How tam runs Claude Code](docs/claude-code.md) | Hooks, environment, dev-environment wrapper, sessions |

## Contributing

Contributions are welcome: open an issue or a pull request. The architecture is described in [CLAUDE.md](CLAUDE.md), and design decisions are recorded in the [issues](https://github.com/ComeBertrand/tam/issues). Before sending a change:

```bash
cargo fmt
cargo clippy -- -D warnings
cargo test
```

## License

[MIT](LICENSE)
