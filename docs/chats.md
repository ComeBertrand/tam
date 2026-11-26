# Chats

A **chat** is a Claude session on a repository's main checkout, started from tam: `tam chat` in a repository, or `c` in the [dashboard](dashboard.md#chats). It owns nothing on disk (no worktree, no branch), and several can run on one repository at once: for debugging, a deployment, a discussion that may end in a task. A chat has an id (`c-1a2b3c4d`, also its agent's `$TAM_AGENT_ID`) and, if you give it one, a name; else tam shows it by the title of its Claude session.

## Commands

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

## Where a chat runs

A chat runs on the main checkout of its repository: REPO is a path to the repository (or to any directory in it, a worktree too), or its project name as `tam ls` shows it. Several chats can share a repository. `tam ps` lists the chats above the tasks, with their ids; a chat's status is its agent's, else `idle`, and CLOSES says when autoclean closes it (`in 3d`, rounded up; `due`; `kept`; or `-` when autoclean is off).

## How names resolve

A name means a task before it means a chat. `tam attach`, `tam stop`, `tam run`, `tam msg`, `tam wait`, `tam result` and `tam history` look for a task first (its name, `TASK/MEMBER`, a member id, from an agent a peer's short name), then for a chat's id, then for a chat's name. A chat cannot be named like an existing task (nor `TASK/…`); a task made later with a chat's name takes it, and the chat is still reached by its id. `tam close`, `tam rename` and `tam keep` take only chats. Without a name, the current directory names a task's worktree, never a chat.

## Sessions and closing

`tam run CHAT` always resumes the chat's own session: a chat is that session, so `--new-session` and `--session` are refused for it (`tam chat --session ID` makes another chat). Once Claude has deleted the session's transcript, `tam run CHAT` says to close the chat and start another. The task commands (`drop`, `repair`, `brief`, `split`, `merge`) say when a name is a chat's. `tam chat --session ID` takes a session Claude ran in that repository's main checkout, that no open chat holds. A chat whose agent cannot start is closed again at once. `tam close` asks before closing (pass `-y` without a terminal); nothing on disk changes, Claude keeps the session, and `tam close` prints the `tam chat --session` command that makes the chat again. Every hint tam prints names a chat by its id, which always resolves.

## Autoclean

A chat (a Claude session on a repository's main checkout, started from tam) is closed by itself after `autoclean` days without a turn: a prompt you type, or an answer to a permission request or a question. The days count from its last turn, its creation, or when you stopped keeping it. A chat is never closed while its agent works or waits on you, nor while you are attached to it or typing in it, and a kept chat never is. Claude's transcript stays (until Claude deletes old ones, after 30 days by default): the chat can be made again from its session. The setting is read again every minute: no restart needed.

Set the number of days with `autoclean` in the `[chat]` section of the [configuration](configuration.md) (3 by default; `0` or `false`: never).

## From inside a chat

From inside a chat, its agent uses `tam` as any shell does: `tam new` starts a task from it, `tam msg` reaches a task's member, and the reply comes back to the chat.
