# How tam runs Claude Code

tam runs Claude Code: `claude` must be on your `PATH`. Nothing to set up.

## Hooks

Claude Code reports state changes through hooks that tam injects into each agent it spawns (`claude --settings`, nothing is written to your Claude settings): tam knows the moment an agent finishes, asks a question, needs a permission or fails on an API error. The permission hook also waits, up to a day, for an answer from the dashboard (`a`); Claude shows its own dialog meanwhile, and once the hook gives up the dialog is still there, nothing approved. Claude runs no hooks until a folder is trusted, so a new worktree shows `ready` while Claude's trust prompt waits. An interrupted turn fires no hook; tam takes a working agent silent for 10s as back at the prompt. Context use is read from the session's transcript. If you ran `tam init --agent claude` with an older version, run `tam init` to remove the global hooks it installed.

## Environment

An agent runs in the environment of what started it: the shell you ran `tam new` or `tam run` in, or the one the dashboard was launched from (its `PATH`, tools, SSH agent and credentials), not the daemon's, which may come from another shell started long ago. A resumed session gets yours too. It is passed to the agent only, never written down or logged. Left out are what belongs to you rather than the agent: an agent's own `TAM_AGENT_ID` and `TAM_SOCKET` when an agent starts another one, the variables a Claude Code session sets for the commands it runs (a session started with them is taken as nested in it), your terminal pane (`TMUX`, `ZELLIJ`, …), size (`COLUMNS`, `LINES`) and directory (`PWD`). The agent gets `TERM=xterm-256color` when you have no `TERM`, e.g. from a script.

## The dev environment: [spawn] wrapper

Your shell is rarely the worktree's dev environment, the dashboard's least of all: set `wrapper` in `[spawn]` to a command that loads it, and every agent starts through it, in its worktree, on top of your environment. `["direnv", "exec", "."]` loads the worktree's `.envrc` (and unloads one loaded where you started). Any command that takes the agent's command as its last arguments and `exec`s it fits, such as `nix develop -c` or `mise exec --`. It runs at every start and resume, and what it prints or fails on shows on the agent's screen. direnv runs only an `.envrc` you allowed, and a new worktree's copy is not: add `direnv allow` to the `.tam.toml` init commands, or the agent stops at once. The wrapper is read at each start: no restart needed after a change.

## Sessions

Each task member has a current session, which `tam run` and `r` resume: the last session the agent had a turn in, as its hooks report it (so a `/clear` followed by a prompt moves it). `tam run --new-session` and `R` start a new one, which becomes current once you use it. A session whose transcript Claude deleted is not resumed: tam says so and you start a new one. `tam run --session ID` resumes any other session.

## Messages

Messages (`tam msg`) reach the agent through one more injected hook, which runs in the background and wakes the session (shown as `tam message` in its transcript).
