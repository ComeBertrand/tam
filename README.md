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
  <a href="#the-agent">The agent</a>
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

**Chat** — a Claude session on a repository's main checkout, started from tam: `tam chat` in a repository. It owns nothing on disk (no worktree, no branch), and several can run on one repository at once: for debugging, a deployment, a discussion that may end in a task. A chat has an id (`c-1a2b3c4d`, also its agent's `$TAM_AGENT_ID`) and, if you give it one, a name; else tam shows it by the title of its Claude session.

**Status is always derived**, never stored. TAM checks the daemon (is an agent running?), the filesystem (does the worktree exist?), git (does the branch exist?), and activity timestamps (is the task stale?) every time you look.

| Status | Meaning |
|---|---|
| `● run` | Agent producing output |
| `◆ ready` | Agent waiting for user prompt |
| `▲ block` | Agent waiting for permission (or plan approval) |
| `? question` | Agent asked the user a question |
| `✗ error` | Agent's last turn failed (API error, rate limit…) |
| `○ idle` | No agent running, task exists |
| `◌ stale` | No activity for 30 days |
| `∅ gone` | Worktree deleted outside tam, or no longer a worktree |

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
- **Chats** — `tam chat` (or `c` in the dashboard) starts a Claude session on a repository's main checkout, new or from the repository's Claude history, listed by `tam ps` and the dashboard and reached by `tam attach`, `tam msg`, `tam wait`… like a task, then closed when done
- **Shells** — `tam shell` (or `!` in the dashboard) opens your shell in a task's worktree (`-r`, or `s`: on a repository's main checkout), kept running by the daemon: its screen in the pane, typed into or attached like an agent's, closed with its task
- **Merge and split** — `tam merge` makes tasks started separately one task, so their running agents become peers; `tam split` moves a member back out. Nothing stops or moves on disk
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
tam new NAME --brief REF       Give the task a brief: a URL, a ticket id or a path (see Brief)

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
tam drop TASK/MEMBER           Drop one member only (its agent, its worktree)

tam adopt [PATH]               Make a worktree created outside tam a task
tam adopt PATH -n NAME         ... named NAME (default: its branch)
tam adopt PATH --join TASK     ... or a new member of an existing task
tam repair NAME                Recreate a worktree deleted outside tam

tam merge INTO FROM...         Move the members of tasks FROM into task INTO (FROM tasks go)
tam split TASK/MEMBER [NAME]   Move one member out into a task of its own (default: its branch)

NAME is a task name, or TASK/MEMBER for one member of the task.
```

`tam drop` checks first: it refuses when uncommitted changes would be lost, or, with `-b`, commits that no other branch, tag or remote branch holds. It lists them and, on a terminal, asks before going on. If a worktree cannot be removed, the task is kept. The shells open in its worktrees (see Shells) are closed first, never a reason to refuse: `tam drop` says how many.

The daemon checks every task's worktree against git (`git worktree list`) when it starts and every 10 seconds. A worktree deleted outside tam (`rm -rf`, `git worktree remove`) shows as `gone` in `tam ps` and the dashboard, with what can be done. `tam repair` recreates it on its branch: only if the branch still exists, is checked out nowhere else, and git's leftover registration of the directory (if any) has no staged changes. `tam drop` forgets the task. A worktree switched to another branch is not gone; `tam ps` notes the switch. TAM only looks: nothing is recreated or removed unless you ask.

`tam new` names a repository by a path to it (or to any directory in it), or by its project name as `tam ls` and `tam pick` show it (e.g. `api`, or `mnemo (work)` when two projects share a name). It is all or nothing: every repository is checked first (branch free, worktree directory free, `-s REF` present), and if a worktree still cannot be created, the ones created before it are removed again, with the branches created for them. What cannot be removed without losing work is kept, and the error says where. With several repositories, `tam new` starts an agent in each (`-p`: each gets the prompt) and attaches to the first one's (with `-d`, or without a terminal, it attaches to none and prints how to); `tam run`, `tam attach` and the preview act on one member (`NAME/REPO`); `tam stop` and `tam drop` act on the whole task, or on one member given as `NAME/REPO`. Git runs in the environment of the `tam` command that asked (checkout hooks and filters such as git-lfs see your `PATH`), for `tam repair` too.

`tam adopt` takes over a worktree made by hand (`git worktree add`), from the worktree or any directory in it: it must be a linked worktree (not the main checkout) on a branch. From then on it is a task like any other: `tam drop` removes it.

`tam merge` and `tam split` change which task a member belongs to, and nothing else: worktrees, branches, sessions, run history, messages and running agents stay as they are. Started as two tasks, a backend and a frontend become one with `tam merge backend frontend`: the members of `frontend` join `backend` after its own, and `frontend` is gone. A member keeps its short name (its repository) unless the task has one by that name already; it is then `<repo>-<branch>`. Names through a task merged away (`frontend`, `frontend/web`) no longer resolve; member ids still do. `tam split backend/web` is the inverse: the member goes to a new task named after its branch, or NAME when a task has that name already. The only member of a task cannot be split out of it. Neither runs while one of the tasks is being dropped. Running agents of the members involved learn of it at their next prompt or tool call: what changed, then their task and peers as they are now (an idle agent is not woken for it). Other agents are not told; `tam peers` shows their peers. The merged task keeps its brief (see Brief), and a task split off starts with the local `brief.md`; the directories of the tasks merged away stay in `~/.local/share/tam/tasks/`.

### Chats

```
tam chat [REPO]                Start a chat on the repository (default: the current one) and attach
tam chat -n NAME               ... named NAME (default: shown by its Claude title, else its id)
tam chat --session ID          ... resuming an existing Claude session instead of a new one
tam chat --keep                ... kept: never closed for want of turns
tam chat -p TEXT -d            On a prompt, without attaching (--json: print a receipt)

tam close CHAT                 Stop the chat's agent and forget the chat (asks first; -y: don't)
tam rename CHAT [NAME]         Name the chat, or take its name away
tam keep CHAT [--off]          Keep the chat, or stop keeping it

tam attach CHAT                Attach to the chat's agent
tam stop CHAT                  Stop its agent (the chat stays)
tam run CHAT                   Start its agent again, in its session
tam msg CHAT TEXT              Message it; tam wait, tam result, tam history take a chat too

CHAT is a chat's id, or its name.
```

A chat runs on the main checkout of its repository: REPO is a path to the repository (or to any directory in it, a worktree too), or its project name as `tam ls` shows it. Several chats can share a repository. `tam ps` lists the chats above the tasks, with their ids; a chat's status is its agent's, else `idle`, and CLOSES says when autoclean closes it (`in 3d`, rounded up; `due`; `kept`; or `-` when autoclean is off).

A name means a task before it means a chat. `tam attach`, `tam stop`, `tam run`, `tam msg`, `tam wait`, `tam result` and `tam history` look for a task first (its name, `TASK/MEMBER`, a member id, from an agent a peer's short name), then for a chat's id, then for a chat's name. A chat cannot be named like an existing task (nor `TASK/…`); a task made later with a chat's name takes it, and the chat is still reached by its id. `tam close`, `tam rename` and `tam keep` take only chats. Without a name, the current directory names a task's worktree, never a chat.

`tam run CHAT` always resumes the chat's own session: a chat is that session, so `--new-session` and `--session` are refused for it (`tam chat --session ID` makes another chat). Once Claude has deleted the session's transcript, `tam run CHAT` says to close the chat and start another. The task commands (`drop`, `repair`, `brief`, `split`, `merge`) say when a name is a chat's. `tam chat --session ID` takes a session Claude ran in that repository's main checkout, that no open chat holds. A chat whose agent cannot start is closed again at once. `tam close` asks before closing (pass `-y` without a terminal); nothing on disk changes, Claude keeps the session, and `tam close` prints the `tam chat --session` command that makes the chat again. Every hint tam prints names a chat by its id, which always resolves.

From inside a chat, its agent uses `tam` as any shell does: `tam new` starts a task from it, `tam msg` reaches a task's member, and the reply comes back to the chat.

### Shells

```
tam shell [NAME]               Open a shell in the task's worktree (default: where you are) and attach
tam shell -r REPO              ... on the repository's main checkout instead
tam shell -d                   Don't attach: print its id (--json: print a receipt)

tam attach SHELL               Attach to the shell
tam stop SHELL                 Close it: what runs in it stops

SHELL is a shell's id (sh-…), as tam ps shows it.
```

A shell is your `$SHELL`, run by the daemon in a task's worktree or on a repository's main checkout: it keeps running once you detach (`ctrl-]`), until it exits (`exit`, `ctrl-d`) or `tam stop` closes it, and dropping its task (or its member) closes it too, as `tam drop` says. It runs in the environment you ran `tam shell` from, through `[spawn] wrapper` as an agent does (see The agent), without `TAM_AGENT_ID`: what you type there is yours, `tam msg` from it comes from you. A daemon restart closes every shell. Shells are named `Shell #1`, `Shell #2`… in each place (the number after the highest one open there), and by their id on the command line.

NAME is what `tam run` takes (a task, `TASK/MEMBER`, a member id), or a chat (its main checkout); REPO what `tam chat` takes (a path, or a project name as `tam ls` shows it; from a worktree, its main checkout). With neither, the shell opens in the task worktree you are in, else on the main checkout you are in; in a worktree no task has, `tam shell` refuses (`tam adopt` makes it a task's, `tam shell -r .` opens one on its main checkout). Without a terminal, or with `-d`, it opens the shell and prints its id alone on stdout (how to attach goes to stderr). `tam ps` lists the shells on main checkouts in a table of their own after the chats, and a task's shells below its row (below its member's, in a task with several), each with its id. The commands that act on agents or tasks (`run`, `msg`, `wait`, `result`, `history`, `drop`, `merge`, `close`) say when a name is a shell's id. The dashboard has them too (see TUI).

### Observing

```
tam ps                         Chats, shells on main checkouts, then the task table
                               with computed status, a row per member of a task
                               with several, and its shells below it
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

An agent gets a message right away: when idle it wakes up, when busy it reads it at its next step (after a pending permission prompt is answered). A member with no running agent gets it when its next agent starts. Sent from inside an agent TAM started, a message comes from that agent, which the recipient can answer with `tam msg`; otherwise it comes from you. The reply `--wait` prints is the recipient's next message back, or its response at the end of the turn that got the message. Two agents answering each other are stopped after 6 messages, until you prompt one of them; an agent sends at most 20 messages per 10 minutes.

An agent TAM starts knows its task. It is told when its session starts (again on resume, `/clear` and compaction): its task, its name, the other members with their repositories and worktrees, and the task's brief and scratch directory (see Brief), so a backend and a frontend session can agree on an API without you relaying. After `tam merge` or `tam split` changed its task, it is told again at its next prompt or tool call. It also has its member id in `$TAM_AGENT_ID`, and `tam peers` for what changed since. From an agent, `tam msg`, `tam run`, `tam stop`, `tam attach` and `tam split` take the short name of another member of its task (`tam msg frontend ...`), unless a task has that name; `tam peers` prints the name that works from where you run it (`TASK/MEMBER` from outside the task).

### Brief

```
tam brief [NAME]               The task's brief and scratch directory
tam brief NAME --set REF       Make REF the brief: a URL, a ticket id, a path
tam brief NAME --local         Make the local file brief.md the brief again
```

The members of a task work from one brief: the reference you give with `tam new --brief REF` or `tam brief --set REF` (a URL, a ticket id, a path; TAM neither fetches nor reads it), else the file `brief.md` in the task's directory (`~/.local/share/tam/tasks/NAME/`, or under `$XDG_DATA_HOME/tam` when it is set), which TAM starts with a heading per member. Next to it, `scratch/` is a directory the members share for transient notes. An agent is told both when its session starts, and how to write in the brief: under its own heading, and what the members agree on (an API shape, a name) under `Shared`, so that two agents do not overwrite each other's part. A changed brief reaches running agents at their next session start (resume, `/clear`, compaction); `tam peers` shows the current one. A pattern in the configuration can turn a ticket id into its URL and into the agents' start prompt (see Configuration). Dropping the task removes its directory: a `brief.md` the members wrote in is kept in `~/.local/share/tam/tasks/.dropped/`, and `tam drop` says where.

An agent can also read the other members' worktrees without asking, but not edit them: TAM adds them to the session's directories with a rule denying edits there, which covers Claude's file tools and the file commands it recognises in shell commands, not every program a command may run. A member that joined after the agent started is readable from the agent's next start. The task's directory is open to the agent as its worktree is: edits there follow its permission mode.

With several repositories, every agent starts on the same prompt (`-p`, or the pattern's): each one is told its own repository and its peers, and takes its part of the brief. To tell one member more, `tam msg TASK/MEMBER`.

### Scripting

For agents and scripts: nothing waits on a terminal, exit statuses tell what happened, and `--json` output has a versioned shape.

```
tam new NAME -p TEXT -d --json Create a task, start its agent on a prompt, print a receipt
tam run NAME -p TEXT -d --json Start the agent on a prompt, print a receipt
tam wait NAME                  Wait until the task's agents stop working; print their states
tam wait NAME -u ready -t 30m  ... until they wait for input, for at most 30 minutes
tam result NAME                Print the last response of the task's agents
tam ps --json                  Every task, chat and shell, as JSON
tam chat -d --json             Start a chat, print a receipt
tam shell NAME -d --json       Open a shell, print a receipt
tam close CHAT -y --json       Close a chat; print it as it was (rename, keep: as it is)
tam merge INTO FROM --json     Merge tasks; print who moved where
tam split TASK/MEMBER --json   Split a member out; print who moved where
```

`tam new` and `tam run` attach only when stdin and stdout are terminals: otherwise they start the agent and say so. `tam wait` returns once each agent is in one of the `--until` states, comma-separated: `working`, `blocked`, `ready`, `idle`, `question`, `error`, `exit` (no agent runs). The default is any state but `working`. `ready` (`input`, its former name, still works) takes in `idle`, since agents without hooks never report `ready`. It gives up at once when an agent exits or its turn fails, unless `--until` takes `exit` or `error`. `tam result` prints the response that ended the agent's last turn, in full; it is still there once the agent exited, or the daemon restarted. There is none while a turn runs, before the first turn of a run ends, after a failed or interrupted turn, and for agents other than Claude. For `wait` and `result`, NAME is a task (all its members), TASK/MEMBER, a member id, or a chat. Both report what they find: right after `tam msg`, an agent that has not picked the message up yet is still `ready`, with its previous response. `tam msg --wait` waits for the reply instead, and `response_at` tells when a response came.

Claude asks whether to trust a folder it does not know yet, and runs no hook until it is answered: a detached agent in a new worktree waits there (`tam wait` sees `ready` after 10s, and `tam result` has nothing). Attach once to answer it.

| Exit status | Meaning | `error.kind` |
|---|---|---|
| 0 | Done | |
| 1 | Failed; `tam wait` gave up; `tam result` has no response; a merge or split refused; `tam close` not confirmed | `failed`, `unreachable`, `no_result` |
| 2 | Invalid arguments (reported as text by the argument parser) | |
| 3 | No such task, member, chat or open shell; a task name where one member is needed; a chat or a shell where the command takes none | `not_found` |
| 4 | `tam wait` timed out | `timeout` |
| 5 | The daemon cannot be reached, or broke off | `daemon` |

With `--json`, stdout holds one JSON document and nothing else, also when the command fails: its `error` is then `{"kind": …, "message": …}`, next to what could still be reported (a command with nothing to report prints only `schema_version` and `error`). Every document has `"schema_version": 1`, raised only when a key is removed, renamed or retyped: new keys may come. Every key is always present, `null` when it has no value.

`tam ps --json`, tasks most urgent first, then chats (most urgent first, else in creation order), then shells (in the order they were opened):

```json
{
  "schema_version": 1,
  "tasks": [
    {
      "name": "feat",
      "status": "ready",
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
          "status": "ready",
          "agent": "claude",
          "health": { "state": "ok", "branch": "feat" },
          "label": "feat",
          "repo_root": "/home/me/src/app",
          "state": "ready",
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
  ],
  "chats": [
    {
      "id": "c-1a2b3c4d",
      "name": "login-bug",
      "label": "login-bug",
      "title": "Flaky login test",
      "last_prompt": "why is CI red?",
      "repo": "app",
      "dir": "/home/me/src/app",
      "status": "ready",
      "keep": false,
      "closes_at": 1763659200,
      "created_at": 1763390000,
      "last_turn_at": 1763400000,
      "run_count": 1,
      "agent": "claude",
      "state": "ready",
      "pid": 4243,
      "viewers": 0,
      "context_percent": 12,
      "session_id": "3e9b7c21-…",
      "pending": null,
      "turn_error": null,
      "denied": null,
      "last_message": "The login test is flaky: …",
      "last_run": {
        "provider": "claude",
        "session_id": null,
        "started_at": 1763390000,
        "ended_at": null,
        "exit_code": null
      }
    }
  ],
  "shells": [
    {
      "id": "sh-1a2b3c4d",
      "name": "Shell #1",
      "number": 1,
      "task": "feat",
      "member": "m-1a2b3c4d",
      "repo": "app",
      "dir": "/home/me/worktrees/app--feat",
      "pid": 4250,
      "viewers": 0,
      "opened_at": 1763400100
    }
  ]
}
```

- `status` (of a task: its most urgent member's) is `run`, `ready`, `block`, `question`, `error`, `idle`, `stale` or `gone`. `state` is the running agent's: `working`, `blocked`, `ready`, `idle`, `question` or `error`; `null` without one. A task's `dir`, `agent` and `context_percent` are its first member's.
- `pending` (while `blocked` or `question`) is `{"tool", "summary"}`, `turn_error` (while `error`) `{"kind", "message"}`, `denied` (the last tool call auto mode refused in this turn) `{"tool", "summary", "reason"}`.
- `session_id` is the running agent's session, else the last run's. `last_message` is the start of the agent's last response: `tam result` has all of it. A run's `exit_code` is -1 when it was stopped.
- `health` is what the daemon saw in git: `{"state": "ok", "branch"}`, `{"state": "gone", "cannot_repair"}` or `{"state": "broken", "problem"}`; `null` until checked.
- A chat's `id` is how to name it to tam; `label` is how tam shows it (its `name`, else its `title`, else its id) and may not be a name tam takes back. `title` (the user's `/rename`, else Claude's title) and `last_prompt` come from Claude's files of the session it is in. `status` is its agent's (`run`, `ready`, `block`, `question`, `error`), else `idle`. `last_turn_at` is when the user last prompted its agent; `closes_at` when autoclean closes it (`null` when kept, or autoclean is off); `session_id` the running agent's session, else the one it resumes next. The keys from `agent` on are those of a task member.
- A shell's `id` is how to name it to tam (`tam attach`, `tam stop`); `name` is how tam shows it, `Shell #` and its `number` among the shells of its place. `task` and `member` are those of the worktree it runs in, `null` on a main checkout; `pid` its process's (the wrapper's, which `exec`s the shell); `viewers` the clients attached to it or typing into it. A shell has no status.
- Before version 1, `tam ps --json` printed the `tasks` array alone, with the keys up to `last_activity` (tasks) and `health` (members).

`tam new --json` and `tam run --json`: the task's members (`new`) or the one started (`run`). `agent` is `null` when none started, `resumed_session` is `null` for a new session. For a chat (`tam chat --json`, `tam run CHAT --json`, and `tam wait`/`tam result` of a chat), the document has `chat` (its id) where a task's has `task`, and the chat as the one entry of `members`: `name` is `null` when unnamed, and there is no `branch`.

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

`tam brief --json`: `file` is the local brief while there is no `reference`. `tam peers --json` has the same document, without `schema_version`, under `brief` (`null` outside a task).

```json
{
  "schema_version": 1,
  "task": "feat",
  "reference": "https://tickets.example.com/CS-42",
  "file": null,
  "scratch": "/home/me/.local/share/tam/tasks/feat/scratch"
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
      "state": "ready",
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

`tam close --json`, `tam rename --json` and `tam keep --json`: the chat as it is now, or as it was for `close` (`open` is then `false`): `session_id` makes it again with `tam chat REPO --session ID`.

```json
{
  "schema_version": 1,
  "chat": "c-1a2b3c4d",
  "name": "login-bug",
  "label": "login-bug",
  "repo": "app",
  "dir": "/home/me/src/app",
  "keep": false,
  "session_id": "3e9b7c21-…",
  "created_at": 1763390000,
  "last_turn_at": 1763400000,
  "run_count": 1,
  "open": false
}
```

`tam shell --json`: the shell opened, with the keys a shell has in `tam ps --json` (`shell` is its id), but `viewers`. A failure (nothing opened) is the error document alone.

```json
{
  "schema_version": 1,
  "shell": "sh-1a2b3c4d",
  "name": "Shell #1",
  "number": 1,
  "task": "feat",
  "member": "m-1a2b3c4d",
  "repo": "app",
  "dir": "/home/me/worktrees/app--feat",
  "pid": 4250,
  "opened_at": 1763400100
}
```

`tam merge --json` and `tam split --json`: `task` is the merged task, or the new one; `from` the tasks merged into it (gone now), or the task split from. `members` are the merged task's members, or the member split out followed by those left in its task. `previous_label` is how a member was named before (names that went with it no longer resolve), `moved` whether it changed task, `agent` its running agent (it keeps running), `notified` whether that agent is told at its next prompt or tool call.

```json
{
  "schema_version": 1,
  "task": "backend",
  "from": ["frontend"],
  "members": [
    {
      "id": "m-1a2b3c4d",
      "task": "backend",
      "name": "api",
      "label": "backend/api",
      "previous_label": "backend",
      "moved": false,
      "repo": "api",
      "branch": "backend",
      "dir": "/home/me/worktrees/api--backend",
      "agent": "claude",
      "notified": true
    },
    {
      "id": "m-5e6f7a8b",
      "task": "backend",
      "name": "web",
      "label": "backend/web",
      "previous_label": "frontend",
      "moved": true,
      "repo": "web",
      "branch": "frontend",
      "dir": "/home/me/worktrees/web--frontend",
      "agent": null,
      "notified": false
    }
  ],
  "error": null
}
```

### TUI

Running `tam` with no arguments opens the dashboard:

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
 enter:type  f:full screen  x:stop  d:close  p:keep  N:rename  !:shell here  n:new task  c:new chat  ?:help
```

Chats come first, in a section of their own, in the order they were made, whatever their repository: the same row as a task's (status, time in it, name, context), the chat's repository where a task has its branch. A chat shows the name you gave it, else the title of its Claude session (its id until it has one). `◔` after the name (the name dimmed) means its last day before autoclean closes it, `∞` that it is kept.

Shells (your `$SHELL`, run by the daemon) have rows too, named `Shell #1`, `Shell #2`… in each place, with no status, time or context: a shell in a task's worktree hangs below its task (below its member in a task with several), and the shells on repositories' main checkouts have a section of their own after the chats, in the order they were opened, with their repository where a task has its branch.

Tasks are grouped by repository (a task in several under one header naming them, e.g. `backend + frontend`), the most urgent first in each (blocked/question → error → ready → running → idle → stale → gone), then by name. `t` lists chats and tasks together, most urgent first across repositories, with a REPO column (a chat's BRANCH says `chat`); a task's shells stay below it, and those on main checkouts come last (their BRANCH says `shell`). A task with several members has a row per member below its own, which shows its name alone: each member's row has its status. A working agent's mark is a spinner, and CTX is how full its context is; `▸` marks the member the keys act on. Next to the status, how long it has been in it (without an agent: since the task's last activity). BRANCH compares each worktree with the default branch: `↑` commits ahead, `↓` behind, `*` paths with uncommitted changes, `=` none of them (`…` until git was asked). Git is asked, read-only and one worktree at a time, about every worktree once a minute and after its agent's state changes, and every 10 seconds about the one the pane shows. When the list is above the pane and at least 100 columns wide, a last column says what each member waits on. Columns the width cannot hold go first (AGENT, then CTX), task names last.

Keys: `j`/`k` navigate, `enter` gives the pane the keyboard (what you type goes to the agent, `ctrl-]` comes back to the list), `f` (or `shift-enter`, in terminals that tell it from `enter`: kitty, and others with its keyboard protocol) opens the agent's terminal on the whole screen, `n` creates a task (`tab` picks several repositories, `ctrl-n`/`ctrl-p` move in the list; an existing task's name adds them to it), `r` runs the agent in its current session, `R` in a new session, `x` stops, `a` answers what its agent asks, `!` opens a shell here (see below), `s` a shell on a repository's main checkout, `d` drops (then `y` keeps the branch, `b` deletes it; in a task with several members, `m` drops the focused member only, `M` with its branch; work that would be lost is shown for a second confirmation; on a `gone` task, `r` recreates the worktree instead), the pane, always shown, is its agent's screen, which takes the pane's size (`h`/`l` or `←`/`→` move to another member of the task, `v` scrolls through its history, the pane's border orange (yellow in a 16-colour terminal) where typing's is the accent colour: `j`/`k` a line at a time, `g`/`G` to the oldest line and back to live, `enter` types into the agent at the live screen, `ctrl-]` back to the list on the live screen; the wheel over it scrolls too, `Home` goes to the oldest line, `End` back to live), `gg`/`G` go to the first/last entry (`j`/`k` go from the chats down to the tasks, through the shells), `/` filters, `t` switches between grouped by repository and attention first, `T` switches between the `dark` and `unicorn` themes (until you quit: `theme` in the config is the one it starts with), `?` lists every key (custom commands included), `q` quits. When attached to an agent, `ctrl-]` detaches and returns to the dashboard. The footer shows the keys that fit.

`c` makes a chat: pick its repository in the picker `n` uses (one repository), then what it starts from: `+ new chat`, or one of the repository's past Claude sessions (its title, its last prompt, how long ago), newest first, those a chat already holds left out; the filter matches titles and prompts. `enter` starts it and gives it the keyboard. No name is asked: the chat shows Claude's title until you name it. A session still open in a plain terminal would be written by two processes at once: open sessions from tam.

On a chat: `enter` types into it, starting its agent first when none runs (in the chat's own session: resuming is never a step of its own, and `r`/`R` only say so), `f` and `shift-enter` start it too and attach, `d` closes it after you confirm (its agent stops and tam forgets it; nothing on disk changes and Claude keeps the session, so the chat can be made again from the repository's history), `p` keeps it (never closed for want of turns) or stops keeping it, `N` names it (an empty name goes back to Claude's title), and `x`, `a`, `!` and custom commands work as on a task. The pane of a chat in its last day says on its bottom border when it closes and that `p` keeps it; without an agent, the pane says how its last run ended, what `enter` will do and what it was last asked.

`!` opens a shell where the selection is: in the worktree of the member the pane shows, on a chat's main checkout, or where the selected shell runs (another one there); `s` opens one on the main checkout of a repository you pick (in the picker `n` uses, one repository). The new shell is selected and has the keyboard at once. It runs in the environment the dashboard was started from, through `[spawn] wrapper` as an agent does, without `TAM_AGENT_ID`: what you type there is yours. It keeps running when you quit the dashboard, until it exits (`exit`, ctrl-d) or you close it; dropping its task or member closes it, and the drop dialog says how many shells go with it. A daemon restart closes every shell. On a shell: `enter` types into it (`ctrl-]` comes back to the list), `f` and `shift-enter` attach, `v` scrolls its history, `!` opens another one in the same place, `x` and `d` close it after you confirm (what runs in it stops; nothing on disk changes); `r`, `R`, `a`, `p` and `N` do nothing. The pane is its screen, titled with where it runs (`Shell #3 · fix-allotment-rounding`). A shell that closes while selected leaves the selection on its task.

The filter and the project picker match fuzzily: the letters typed, in order, not necessarily together (`unpr` finds `unit-pricing`), the matched letters highlighted. The filter looks at task names and at each member's name, repository and branch, at a chat's name, title, id and repository, and at the repository of a shell on a main checkout (a shell in a worktree shows with its task) (`enter` keeps it, `esc` empties it with the task found still selected); the picker ranks the best matches first. In the filter, the picker and the name and path prompts, `←`/`→`, `home`/`end` and `ctrl-a`/`ctrl-e` move the cursor, `ctrl-←`/`ctrl-→` by word, `ctrl-w` deletes a word, `ctrl-u`/`ctrl-k` to the start/end. `n` opens the picker at once: the projects under `$HOME` are looked for in the background (those found last time show meanwhile).

The mouse: a click selects a task, a chat or a shell (with the pane open, a member's row shows that member) or a project in the picker; the wheel moves the selection over the list and in the picker, and scrolls the agent's screen over the pane. The dashboard takes the mouse while it is shown (most terminals select text with `shift` held), and lets go of it while attached to an agent.

The pane is the terminal of the selected task's (or chat's) agent, or of the selected shell, as it is right now. `enter` gives it the keyboard: every key goes to the agent, the dashboard's own keys too (`q`, `d`…), a paste arrives as one paste, `shift-enter` or `alt-enter` is a new line in the agent's prompt, and the pane's border says so until `ctrl-]` (or a click in the list) gives the keyboard back. While you type to an agent, tam sends no notification about it. If the agent exits meanwhile, the keys you were still typing are dropped, not run as dashboard keys. `f` attaches on the whole screen instead, as `tam attach` does. Its bottom border keeps what the terminal may no longer show: why its turn failed, what auto mode refused, and messages still waiting for the agent. Without an agent there is no terminal, and the pane says instead how the last run ended or what is wrong with the worktree, the branch against the default branch with its diff stat, and the latest messages. In a task with several members it shows the one that needs you; `←`/`→` show another, and the member keys (`enter`, `f`, `x`, `r`, `R`, `a`, `!`) act on the member shown. From 136 columns the pane is beside the list (wide enough for an agent's 80-column screen), else below it.

Without attaching, `a` answers what the agent asks: a permission (the tool's whole input is shown; `y` allows it once, `n` says no, with a reason the agent reads and goes on with, or without one, which asks it to stop and wait for you), a plan to approve (`y`: the agent leaves plan mode, and edits keep asking for permission), or its questions (`↑`/`↓` and `enter` pick an option, `space` ticks several, the last row takes an answer of your own; with several questions, `←`/`→` move between them and a last step sends them all). It answers the request it shows and no other: once that request is over (answered in the agent's terminal, or the agent moved on), the dialog says so and nothing is sent. A yes given in the terminal shows late, when the tool has run: an answer sent meanwhile goes to a request Claude has already answered, and changes nothing. Nothing is ever allowed for more than once, and a request too long to show in full (over 16,000 characters) can only be refused from the dashboard. Claude's own dialog stays in the agent's terminal meanwhile, and the first answer wins. Without the dashboard, the request waits in the terminal as usual.

What the dashboard cannot answer (an MCP server's input request, a request older than a day) says so: attach to answer it there.

### Setup

```
tam init                       Remove the Claude hooks older versions installed globally
tam shutdown                   Stop all agents and kill the daemon
tam status                     Check if the daemon is running
```

## Configuration

TAM reads `$XDG_CONFIG_HOME/tam/config.toml`, by default `~/.config/tam/config.toml`, on every platform (macOS too). It keeps its data (the task ledger, the tasks' directories) in `$XDG_DATA_HOME/tam`, by default `~/.local/share/tam`, the same way.

```toml
[spawn]
wrapper = ["direnv", "exec", "."]   # run every agent through it, in its worktree

[worktree]
root = "~/worktrees"
auto_init = true              # run .tam.toml after worktree creation

[discovery]
max_depth = 5
ignore = [".*", "node_modules", "target"]

[notify]
command = "notify-send 'tam: {task}' '{kind}: {message}'"
on_states = ["ready", "blocked", "question", "error"]   # default

[session]
finder = "fzf"

[chat]
autoclean = 3                           # days without a turn before a chat is closed (fractions too); 0 or false: never

[tui]
theme = "dark"                          # 24-bit palette; "unicorn": brighter, with rainbows; "ansi": the terminal's 16 colours. Default: dark when COLORTERM says truecolor
animate = true                          # unicorn: the rainbows move (the logo, the selection's bar, the spinners, the rules, the pane's border)

[[tui.commands]]                        # on the selected task's focused member, or the selected chat
name = "open in editor"
key = "o"
command = "code {dir}"                  # also {task}, {id}, {provider}: as for notify; {dir} is the worktree, or a chat's or shell's main checkout

[[brief.patterns]]                      # tam new --brief CS-42, tam brief --set CS-42
match = '^CS-(\d+)$'                    # a regular expression: the first one that matches applies
reference = "https://tickets.example.com/CS-{1}"
prompt = "/implement-task {brief}"      # the agents' start prompt for tam new without -p
```

A custom command's key is a single character the dashboard does not use: a config binding one of its keys does not load. `c`, `p`, `N`, `v`, `!` and `x` are the exception, being newer than custom commands: a command bound to one of them keeps it, and the dashboard's action (new chat, keep, rename a chat, scroll the pane, a shell here, stop or close a shell) then has no key (the help says so; `d` still closes a shell). On a shell in a worktree, a custom command gets its member's `{task}`, `{id}` and `{dir}`; on a shell on a main checkout, `{task}` is its name (`Shell #1`) and `{id}` its id.

A brief pattern turns what you type into the task's brief (`reference`: `{0}` is the match, `{1}`… its groups, `{brief}` what you typed; without `reference`, the brief stays as typed), and gives the agents of `tam new` their start prompt (`prompt`, where `{brief}` is the brief). A brief that matches no pattern and names an existing file is recorded as its absolute path.

A chat (a Claude session on a repository's main checkout, started from tam) is closed by itself after `autoclean` days without a turn: a prompt you type, or an answer to a permission request or a question. The days count from its last turn, its creation, or when you stopped keeping it. A chat is never closed while its agent works or waits on you, nor while you are attached to it or typing in it, and a kept chat never is. Claude's transcript stays (until Claude deletes old ones, after 30 days by default): the chat can be made again from its session. The setting is read again every minute: no restart needed.

The notify command runs with `sh -c` on state changes, except while you are attached to the agent. Placeholders are escaped for the quoting they sit in:

| Placeholder | Value |
|---|---|
| `{task}`, `{id}` | Task name, agent id (the member id); for a chat, its name (else the title of its Claude session, else its id) and its id |
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

## The agent

TAM runs Claude Code: `claude` must be on your `PATH`. Nothing to set up.

Claude Code reports state changes through hooks that TAM injects into each agent it spawns (`claude --settings`, nothing is written to your Claude settings): TAM knows the moment an agent finishes, asks a question, needs a permission or fails on an API error. The permission hook also waits, up to a day, for an answer from the dashboard (`a`); Claude shows its own dialog meanwhile, and once the hook gives up the dialog is still there, nothing approved. Claude runs no hooks until a folder is trusted, so a new worktree shows `ready` while Claude's trust prompt waits. An interrupted turn fires no hook; TAM takes a working agent silent for 10s as back at the prompt. Context use is read from the session's transcript. If you ran `tam init --agent claude` with an older version, run `tam init` to remove the global hooks it installed.

An agent runs in the environment of what started it: the shell you ran `tam new` or `tam run` in, or the one the dashboard was launched from (its `PATH`, tools, SSH agent and credentials), not the daemon's, which may come from another shell started long ago. A resumed session gets yours too. It is passed to the agent only, never written down or logged. Left out are what belongs to you rather than the agent: an agent's own `TAM_AGENT_ID` and `TAM_SOCKET` when an agent starts another one, the variables a Claude Code session sets for the commands it runs (a session started with them is taken as nested in it), your terminal pane (`TMUX`, `ZELLIJ`, …), size (`COLUMNS`, `LINES`) and directory (`PWD`). The agent gets `TERM=xterm-256color` when you have no `TERM`, e.g. from a script.

Your shell is rarely the worktree's dev environment, the dashboard's least of all: set `wrapper` in `[spawn]` to a command that loads it, and every agent starts through it, in its worktree, on top of your environment. `["direnv", "exec", "."]` loads the worktree's `.envrc` (and unloads one loaded where you started). Any command that takes the agent's command as its last arguments and `exec`s it fits, such as `nix develop -c` or `mise exec --`. It runs at every start and resume, and what it prints or fails on shows on the agent's screen. direnv runs only an `.envrc` you allowed, and a new worktree's copy is not: add `direnv allow` to the `.tam.toml` init commands, or the agent stops at once. The wrapper is read at each start: no restart needed after a change.

Each task member has a current session, which `tam run` and `r` resume: the last session the agent had a turn in, as its hooks report it (so a `/clear` followed by a prompt moves it). `tam run --new-session` and `R` start a new one, which becomes current once you use it. A session whose transcript Claude deleted is not resumed: TAM says so and you start a new one. `tam run --session ID` resumes any other session.

Messages (`tam msg`) reach the agent through one more injected hook, which runs in the background and wakes the session (shown as `tam message` in its transcript).

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

Contributions are welcome — open an issue or submit a pull request. For the architecture, see [`CLAUDE.md`](CLAUDE.md); design decisions are recorded in the issues.

## License

MIT
