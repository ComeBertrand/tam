# Scripting

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

## Waiting and results

`tam new` and `tam run` attach only when stdin and stdout are terminals: otherwise they start the agent and say so. `tam wait` returns once each agent is in one of the `--until` states, comma-separated: `working`, `blocked`, `ready`, `idle`, `question`, `error`, `exit` (no agent runs). The default is any state but `working`. `ready` (`input`, its former name, still works) takes in `idle`, since agents without hooks never report `ready`. It gives up at once when an agent exits or its turn fails, unless `--until` takes `exit` or `error`. `tam result` prints the response that ended the agent's last turn, in full; it is still there once the agent exited, or the daemon restarted. There is none while a turn runs, before the first turn of a run ends, after a failed or interrupted turn, and for agents other than Claude. For `wait` and `result`, NAME is a task (all its members), TASK/MEMBER, a member id, or a chat. Both report what they find: right after `tam msg`, an agent that has not picked the message up yet is still `ready`, with its previous response. `tam msg --wait` waits for the reply instead, and `response_at` tells when a response came.

Claude asks whether to trust a folder it does not know yet, and runs no hook until it is answered: a detached agent in a new worktree waits there (`tam wait` sees `ready` after 10s, and `tam result` has nothing). Attach once to answer it.

## Exit statuses

| Exit status | Meaning | `error.kind` |
|---|---|---|
| 0 | Done | |
| 1 | Failed; `tam wait` gave up; `tam result` has no response; a merge or split refused; `tam close` not confirmed | `failed`, `unreachable`, `no_result` |
| 2 | Invalid arguments (reported as text by the argument parser) | |
| 3 | No such task, member, chat or open shell; a task name where one member is needed; a chat or a shell where the command takes none | `not_found` |
| 4 | `tam wait` timed out | `timeout` |
| 5 | The daemon cannot be reached, or broke off | `daemon` |

## JSON output

With `--json`, stdout holds one JSON document and nothing else, also when the command fails: its `error` is then `{"kind": …, "message": …}`, next to what could still be reported (a command with nothing to report prints only `schema_version` and `error`). Every document has `"schema_version": 1`, raised only when a key is removed, renamed or retyped: new keys may come. Every key is always present, `null` when it has no value.

### tam ps

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

### tam new, tam run, tam chat

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

### tam brief

`tam brief --json`: `file` is the local brief while there is no `reference`. `tam peers --json` has the same document, without `schema_version`, under `brief` (`null` outside a task).

```json
{
  "schema_version": 1,
  "task": "feat",
  "reference": "https://tickets.example.com/SHOP-42",
  "file": null,
  "scratch": "/home/me/.local/share/tam/tasks/feat/scratch"
}
```

### tam wait, tam result

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

### tam close, tam rename, tam keep

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

### tam shell

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

### tam merge, tam split

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
