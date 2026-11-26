# Tasks

A **task** is a named unit of work. tam creates a git worktree for it, on a branch named after the task, starts a Claude Code agent there, and `tam drop` cleans it up.

A **member** is a task's worktree, with at most one agent session. A task can hold several members, one per repository, for work spanning e.g. a backend and a frontend: `tam new feat -r api -r web` creates both worktrees, on branch `feat`, and starts an agent in each. Members have a stable id, which is also their agent's id (`$TAM_AGENT_ID`), and a short name within the task: the repository name. Name one as `TASK/MEMBER`; a bare task name means its only member.

## Status

**Status is always derived**, never stored. tam checks the daemon (is an agent running?), the filesystem (does the worktree exist?), git (does the branch exist?), and activity timestamps (is the task stale?) every time you look.

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

## Commands

```
tam new NAME                   Create task: branch NAME and its worktree, in the current repo,
                               and start its agent, attached
tam new NAME --no-start        ... without starting an agent
tam new NAME -s REF            Branch from a specific ref
tam new NAME -p TEXT           Start its agent on a prompt
tam new NAME -d                Don't attach to the agent (--json: print a receipt)
tam new NAME -r REPO -r REPO   ... in each of these repositories instead (one member each)
tam new NAME -r REPO --join    Add a worktree in another repository to the task NAME
tam new NAME --brief REF       Give the task a brief: a URL, a ticket id or a path (tam brief)

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

## Creating a task

`tam new` names a repository by a path to it (or to any directory in it), or by its project name as `tam ls` and `tam pick` show it (e.g. `api`, or `mnemo (work)` when two projects share a name). It is all or nothing: every repository is checked first (branch free, worktree directory free, `-s REF` present), and if a worktree still cannot be created, the ones created before it are removed again, with the branches created for them. What cannot be removed without losing work is kept, and the error says where. With several repositories, `tam new` starts an agent in each (`-p`: each gets the prompt) and attaches to the first one's (with `-d`, or without a terminal, it attaches to none and prints how to); `tam run`, `tam attach` and the dashboard's pane act on one member (`NAME/REPO`); `tam stop` and `tam drop` act on the whole task, or on one member given as `NAME/REPO`. Git runs in the environment of the `tam` command that asked (checkout hooks and filters such as git-lfs see your `PATH`), for `tam repair` too.

## Dropping a task

`tam drop` checks first: it refuses when uncommitted changes would be lost, or, with `-b`, commits that no other branch, tag or remote branch holds. It lists them and, on a terminal, asks before going on. If a worktree cannot be removed, the task is kept. The [shells](shells.md) open in its worktrees are closed first, never a reason to refuse: `tam drop` says how many.

## Worktrees deleted outside tam

The daemon checks every task's worktree against git (`git worktree list`) when it starts and every 10 seconds. A worktree deleted outside tam (`rm -rf`, `git worktree remove`) shows as `gone` in `tam ps` and the dashboard, with what can be done. `tam repair` recreates it on its branch: only if the branch still exists, is checked out nowhere else, and git's leftover registration of the directory (if any) has no staged changes. `tam drop` forgets the task. A worktree switched to another branch is not gone; `tam ps` notes the switch. tam only looks: nothing is recreated or removed unless you ask.

## Adopting a worktree

`tam adopt` takes over a worktree made by hand (`git worktree add`), from the worktree or any directory in it: it must be a linked worktree (not the main checkout) on a branch. From then on it is a task like any other: `tam drop` removes it.

## Merging and splitting tasks

`tam merge` and `tam split` change which task a member belongs to, and nothing else: worktrees, branches, sessions, run history, messages and running agents stay as they are. Started as two tasks, a backend and a frontend become one with `tam merge backend frontend`: the members of `frontend` join `backend` after its own, and `frontend` is gone. A member keeps its short name (its repository) unless the task has one by that name already; it is then `<repo>-<branch>`. Names through a task merged away (`frontend`, `frontend/web`) no longer resolve; member ids still do. `tam split backend/web` is the inverse: the member goes to a new task named after its branch, or NAME when a task has that name already. The only member of a task cannot be split out of it. Neither runs while one of the tasks is being dropped. Running agents of the members involved learn of it at their next prompt or tool call: what changed, then their task and peers as they are now (an idle agent is not woken for it). Other agents are not told; `tam peers` shows their peers. The merged task keeps its [brief](collaboration.md#brief), and a task split off starts with the local `brief.md`; the directories of the tasks merged away stay in `~/.local/share/tam/tasks/`.

## Seeing what runs

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

## The daemon and setup

The daemon owns the agents and shells. It starts on the first command that needs it and shuts down after 30 seconds with no agent, shell or client: there is nothing to manage.

```
tam init                       Remove the Claude hooks older versions installed globally
tam shutdown                   Stop all agents and kill the daemon
tam status                     Check if the daemon is running
```
