# Activity log

tam writes down what it does, when, and how long it took, to read afterwards: what was slow, what failed, what you do over and over. It is in `$XDG_DATA_HOME/tam/log` (by default `~/.local/share/tam/log`), a file per local day (`2026-11-28.jsonl`), one JSON object per line. The daemon, each command and the dashboard append their own lines. Files older than `keep_days` are removed; `[log] enabled = false` turns it off (see [Configuration](configuration.md)).

Every line starts with `ts` (Unix time in milliseconds), `time` (local, readable), `src` (`daemon`, `cli` or `tui`), `pid` and `kind`. Durations are `ms`, in milliseconds; `wait: true` says one is mostly waiting, not work (attached to an agent, `tam wait`, a request waiting on you, a dashboard session). A line is written when what it tells ends: `ts - ms` is when it started. The daemon names a client by its `pid` (`peer`), which is the `pid` of that client's own lines.

| `kind` | Written by | When, and what it says |
|---|---|---|
| `cli` | a command | when it ends: `cmd` and its targets and flags, `exit`, `error_kind` and `error` if it failed, `signal` if interrupted (Ctrl-C, its terminal closed), `terminal` (run by hand or by a script), `agent` (the member or chat whose agent ran it, if one did) |
| `cli.usage` | a command | a command line tam refused: the command typed, clap's `error_kind` |
| `daemon.autostart` | a command, the dashboard | it had to start the daemon: how long until it answered |
| `tui.start`, `tui.stop` | the dashboard | its size and theme; at the end how long it ran, inputs, actions, frames |
| `tui.input` | the dashboard | a key (`key`: `j`, `enter`, `ctrl-]`), click or paste that did something: the `mode` it came in, the mode it left for (`to`, with `mode_ms` spent in the one left), the `action` it ran with its target and `ms`, the entry focused then (`id`, `on`), the `status` line or `notice` shown |
| `tui.mode` | the dashboard | a mode left without an input (the agent typed into exited) |
| `tui.slow` | the dashboard | a redraw that took 200 ms or more, by step |
| `daemon.start`, `daemon.stop` | the daemon | version, start time, tasks, chats; why it stopped (`idle`, `requested`) |
| `request` | the daemon | a request that does something: `req`, its targets, `ms`, `reply` (`error` with its message) |
| `conn.stats` | the daemon | the reads and terminal traffic of a connection that lasted (a dashboard): by request, how many, `ms` in all and at most, errors. Such a request taking 100 ms or more is a `request` too (`slow`) |
| `attach` | the daemon | an attach that ended: how long, how many bytes were typed |
| `create.steps`, `drop.steps` | the daemon | a task's creation or drop, step by step: waiting for other git work (`lock_wait`), checks, stopping its agents, creating or removing the worktrees |
| `reconcile.slow` | the daemon | a check of the worktrees that took a second or more |
| `member.health` | the daemon | a worktree's health changed (gone, broken, back, another branch) |
| `agent.spawn` | the daemon | an agent started: resumed or not, wrapper or not, how long it took |
| `agent.state` | the daemon | an agent's state changed: `from`, `to`, `prev_ms` (how long it was `from`), the tool it waits on, whether you were viewing it, whether a notification fired |
| `agent.exit` | the daemon | an agent's run ended: `exit_code` (-1: tam stopped it), `run_ms`, `states_ms` (time in each state), `hooks` (how many of each event) |
| `ask.open`, `ask.end` | the daemon | a permission request, plan or question an agent waits on, and how it ended: answered from the dashboard (`answer`: `allow`, `deny`, …), by a hook event (its tool ran: you said yes in the terminal), its hook gone (no in the terminal, or its timeout), the agent gone; `ms` it waited |
| `message.sent`, `message.handed`, `message.delivered` | the daemon | a message's way, by id, its length, `after_s` until delivered |
| `chat.autoclean` | the daemon | a chat closed for want of turns |
| `shell.exit` | the daemon | a shell that ended by itself |
| `daemon.warn`, `daemon.error` | the daemon | its warnings and errors, as it words them |

What it never holds: environments, prompts, messages, keys typed into an agent or a shell, what is typed into the dashboard's filter and pickers, answers' reasons and text, hook payloads, briefs. Their length or a count at most. Names (of tasks and chats, also those typed in the dashboard), ids, paths and branches are in it, and errors as tam words them (one may quote git, or name the wrapper that failed).

It is meant to be read by a program, or an agent, as much as by you: for example

```sh
cd ~/.local/share/tam/log
jq -c 'select(.kind == "cli") | {time, cmd, ms, exit}' *.jsonl                     # commands and how long they took
jq -c 'select(.ms > 1000 and .wait != true)' *.jsonl                                # whatever took more than a second
jq -r 'select(.kind == "tui.input") | .action // .key' *.jsonl | sort | uniq -c | sort -rn   # what you do most in the dashboard
jq -c 'select(.kind == "agent.state" and .to == "ready") | {id, prev_ms}' *.jsonl   # how long each turn worked
```
