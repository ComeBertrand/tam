# Configuration

tam reads `$XDG_CONFIG_HOME/tam/config.toml`, by default `~/.config/tam/config.toml`, on every platform (macOS too). It keeps its data (the task ledger, the tasks' directories) in `$XDG_DATA_HOME/tam`, by default `~/.local/share/tam`, the same way.

## config.toml

Every key is optional. An example with all of them:

```toml
[spawn]
wrapper = ["direnv", "exec", "."]   # run every agent through it, in its worktree (see claude-code.md)

[worktree]
root = "~/worktrees"          # where worktrees are created (default)
auto_init = true              # run .tam.toml after worktree creation (default: false)

[discovery]
max_depth = 5
ignore = [".*", "node_modules", "target"]

[notify]
command = "notify-send 'tam: {task}' '{kind}: {message}'"
on_states = ["ready", "blocked", "question", "error"]   # default

[session]
finder = "fzf"                          # tam pick's finder

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

### Custom commands

A custom command's key is a single character the dashboard does not use: a config binding one of its keys does not load. `c`, `p`, `N`, `v`, `!` and `x` are the exception, being newer than custom commands: a command bound to one of them keeps it, and the dashboard's action (new chat, keep, rename a chat, scroll the pane, a shell here, stop or close a shell) then has no key (the help says so; `d` still closes a shell). On a shell in a worktree, a custom command gets its member's `{task}`, `{id}` and `{dir}`; on a shell on a main checkout, `{task}` is its name (`Shell #1`) and `{id}` its id.

### Brief patterns

A brief pattern turns what you type into the task's brief (`reference`: `{0}` is the match, `{1}`… its groups, `{brief}` what you typed; without `reference`, the brief stays as typed), and gives the agents of `tam new` their start prompt (`prompt`, where `{brief}` is the brief). A brief that matches no pattern and names an existing file is recorded as its absolute path.

### Chat autoclean

`autoclean` in `[chat]` is how many days a chat may go without a turn before it is closed: see [Chats](chats.md#autoclean).

### Notifications

The notify command runs with `sh -c` on state changes, except while you are attached to the agent. Placeholders are escaped for the quoting they sit in:

| Placeholder | Value |
|---|---|
| `{task}`, `{id}` | Task name, agent id (the member id); for a chat, its name (else the title of its Claude session, else its id) and its id |
| `{state}` (alias `{status}`), `{old_state}` | New and previous agent state |
| `{kind}` | `question`, `permission`, `done` or `error` |
| `{message}` | The question or permission asked, the agent's last response, or the error |
| `{tool}` | Tool waiting for permission (e.g. `Bash`) |
| `{error}` | Error kind (e.g. `rate_limit`) |

## Per-repository setup: .tam.toml

Per-repo worktree initialization is configured via `.tam.toml` at the project root. With `auto_init = true` in `[worktree]` (it is off by default), when `tam new NAME` creates a worktree (or `tam repair` recreates one), it copies files matching `include` globs from the main checkout (useful for untracked secrets or editor config) and runs `commands` inside the new worktree:

```toml
[init]
include = [".env", ".claude/**"]
commands = ["npm install"]
```
