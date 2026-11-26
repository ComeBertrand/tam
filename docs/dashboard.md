# The dashboard

Running `tam` with no arguments opens the dashboard:

```
 tam  4 chats  8 tasks  6 need you                                 by repository
 STATUS          TASK                                            BRANCH CTX
 chats ──────────────────────────────────────────────────────────────────────────
▌◆ ready      7s Release checklist for v2.4                      api     41% ━━━━
 ⠸ run        7s Why are the e2e tests flaky?                    web     12% ━━━━
 ? question   7s Postgres 17 upgrade plan ∞                      infra   63% ━━━━
 ○ idle       2d Pricing page copy ◔                             web
 shells ─────────────────────────────────────────────────────────────────────────
                 Shell #1                                        api
                 Shell #1                                        web
 api ────────────────────────────────────────────────────────────────────────────
 ▲ block     34s fix-rate-limit                                  ↑2 *1   67% ━━━━
                  ├ Shell #1
                  └ Shell #2
 ⠸ run       34s perf-cache                                      =       31% ━━━━
 ◌ stale     45d spike-graphql                                   ↑5 ↓12
 api + web ──────────────────────────────────────────────────────────────────────
                 checkout-v2
 ⠸ run        8s  ├ api                                          ↑1 *3   12% ━━━━
                  │ └ Shell #1
 ? question   7s  └ web                                          ↑1      40% ━━━━
 infra ──────────────────────────────────────────────────────────────────────────
 ✗ error      7s log-retention                                   ↑1      22% ━━━━
 ◆ ready      7s terraform-upgrade                               ↑3 ↓1   55% ━━━━
 web ────────────────────────────────────────────────────────────────────────────
 ○ idle       2d dark-mode                                       =
 ∅ gone       3d old-banner
 enter:type  x:stop  d:close  p:keep  n:new task  c:new chat  ?:help  q:quit
```

## The list

Tasks are grouped by repository (a task in several under one header naming them, e.g. `api + web`), the most urgent first in each (blocked/question → error → ready → running → idle → stale → gone), then by name. `t` lists chats and tasks together, most urgent first across repositories, with a REPO column (a chat's BRANCH says `chat`); a task's shells stay below it, and those on main checkouts come last (their BRANCH says `shell`). A task with several members has a row per member below its own, which shows its name alone: each member's row has its status. A working agent's mark is a spinner, and CTX is how full its context is; `▸` marks the member the keys act on. Next to the status, how long it has been in it (without an agent: since the task's last activity). BRANCH compares each worktree with the default branch: `↑` commits ahead, `↓` behind, `*` paths with uncommitted changes, `=` none of them (`…` until git was asked). Git is asked, read-only and one worktree at a time, about every worktree once a minute and after its agent's state changes, and every 10 seconds about the one the pane shows. When the list is above the pane and at least 100 columns wide, a last column says what each member waits on. Columns the width cannot hold go first (what members wait on, then CTX, BRANCH, REPO and the time in a status), task names last.

### Chats in the list

Chats come first, in a section of their own, in the order they were made, whatever their repository: the same row as a task's (status, time in it, name, context), the chat's repository where a task has its branch. A chat shows the name you gave it, else the title of its Claude session (its id until it has one). `◔` after the name (the name dimmed) means its last day before autoclean closes it, `∞` that it is kept.

### Shells in the list

Shells (your `$SHELL`, run by the daemon) have rows too, named `Shell #1`, `Shell #2`… in each place, with no status, time or context: a shell in a task's worktree hangs below its task (below its member in a task with several), and the shells on repositories' main checkouts have a section of their own after the chats, in the order they were opened, with their repository where a task has its branch.

## Keys

| Key | Action |
|---|---|
| `j` / `k` | Move (from the chats down to the tasks, through the shells) |
| `gg` / `G` | Go to the first / last entry |
| `enter` | Give the pane the keyboard: what you type goes to the agent, `ctrl-]` comes back to the list (see [The pane](#the-pane)) |
| `f`, `shift-enter` | Open the agent's terminal on the whole screen; `ctrl-]` detaches and returns to the dashboard. `shift-enter` works in terminals that tell it from `enter`: kitty, and others with its keyboard protocol |
| `h` / `l`, `←` / `→` | Show another member of the task in the pane |
| `v` | Scroll through the agent's history (see [Scrolling](#scrolling)) |
| `n` | Create a task: `tab` picks several repositories, `ctrl-n` / `ctrl-p` move in the list; an existing task's name adds them to it |
| `c` | Create a chat (see [Chats](#chats)) |
| `r` | Run the agent in its current session |
| `R` | Run the agent in a new session |
| `x` | Stop the agent |
| `a` | Answer what its agent asks (see [Answering without attaching](#answering-without-attaching)) |
| `!` | Open a shell here (see [Shells](#shells)) |
| `s` | Open a shell on a repository's main checkout |
| `d` | Drop: then `y` keeps the branch, `b` deletes it; in a task with several members, `m` drops the focused member only, `M` with its branch. Work that would be lost is shown for a second confirmation. On a `gone` task, `r` recreates the worktree instead |
| `/` | Filter (see [Filter and prompts](#filter-and-prompts)) |
| `t` | Switch between grouped by repository and attention first |
| `T` | Switch between the `dark` and `unicorn` themes, until you quit (`theme` in the config is the one it starts with) |
| `?` | List every key, custom commands included |
| `q` | Quit |

The footer shows the keys that fit.

## The pane

The pane, always shown, is the selected agent's screen, which takes the pane's size.

The pane is the terminal of the selected task's (or chat's) agent, or of the selected shell, as it is right now. `enter` gives it the keyboard: every key goes to the agent, the dashboard's own keys too (`q`, `d`…), a paste arrives as one paste, `shift-enter` or `alt-enter` is a new line in the agent's prompt, and the pane's border says so until `ctrl-]` (or a click in the list) gives the keyboard back. While you type to an agent, tam sends no notification about it. If the agent exits meanwhile, the keys you were still typing are dropped, not run as dashboard keys. `f` attaches on the whole screen instead, as `tam attach` does. Its bottom border keeps what the terminal may no longer show: why its turn failed, what auto mode refused, and messages still waiting for the agent. Without an agent there is no terminal, and the pane says instead how the last run ended or what is wrong with the worktree, the branch against the default branch with its diff stat, and the latest messages. In a task with several members it shows the one that needs you; `←`/`→` show another, and the member keys (`enter`, `f`, `x`, `r`, `R`, `a`, `!`) act on the member shown. From 136 columns the pane is beside the list (wide enough for an agent's 80-column screen), else below it.

### Scrolling

`v` scrolls through the history of the agent in the pane. The pane's border turns orange (yellow in a 16-colour terminal), where typing's is the accent colour.

| Key | Action |
|---|---|
| `j` / `k` | A line at a time |
| `g` / `G` | To the oldest line, and back to live |
| `enter` | Type into the agent, at the live screen |
| `ctrl-]` | Back to the list, on the live screen |

The wheel over the pane scrolls too, `Home` goes to the oldest line, `End` back to live.

## Answering without attaching

Without attaching, `a` answers what the agent asks: a permission (the tool's whole input is shown; `y` allows it once, `n` says no, with a reason the agent reads and goes on with, or without one, which asks it to stop and wait for you), a plan to approve (`y`: the agent leaves plan mode, and edits keep asking for permission), or its questions (`↑`/`↓` and `enter` pick an option, `space` ticks several, the last row takes an answer of your own; with several questions, `←`/`→` move between them and a last step sends them all). It answers the request it shows and no other: once that request is over (answered in the agent's terminal, or the agent moved on), the dialog says so and nothing is sent. A yes given in the terminal shows late, when the tool has run: an answer sent meanwhile goes to a request Claude has already answered, and changes nothing. Nothing is ever allowed for more than once, and a request too long to show in full (over 16,000 characters) can only be refused from the dashboard. Claude's own dialog stays in the agent's terminal meanwhile, and the first answer wins. Without the dashboard, the request waits in the terminal as usual.

What the dashboard cannot answer (an MCP server's input request, a request older than a day) says so: attach to answer it there.

## Chats

`c` makes a chat: pick its repository in the picker `n` uses (one repository), then what it starts from: `+ new chat`, or one of the repository's past Claude sessions (its title, its last prompt, how long ago), newest first, those a chat already holds left out; the filter matches titles and prompts. `enter` starts it and gives it the keyboard. No name is asked: the chat shows Claude's title until you name it. A session still open in a plain terminal would be written by two processes at once: open sessions from tam.

On a chat: `enter` types into it, starting its agent first when none runs (in the chat's own session: resuming is never a step of its own, and `r`/`R` only say so), `f` and `shift-enter` start it too and attach, `d` closes it after you confirm (its agent stops and tam forgets it; nothing on disk changes and Claude keeps the session, so the chat can be made again from the repository's history), `p` keeps it (never closed for want of turns) or stops keeping it, `N` names it (an empty name goes back to Claude's title), and `x`, `a`, `!` and custom commands work as on a task. The pane of a chat in its last day says on its bottom border when it closes and that `p` keeps it; without an agent, the pane says how its last run ended, what `enter` will do and what it was last asked.

## Shells

`!` opens a shell where the selection is: in the worktree of the member the pane shows, on a chat's main checkout, or where the selected shell runs (another one there); `s` opens one on the main checkout of a repository you pick (in the picker `n` uses, one repository). The new shell is selected and has the keyboard at once. It runs in the environment the dashboard was started from, through `[spawn] wrapper` as an agent does, without `TAM_AGENT_ID`: what you type there is yours. It keeps running when you quit the dashboard, until it exits (`exit`, ctrl-d) or you close it; dropping its task or member closes it, and the drop dialog says how many shells go with it. A daemon restart closes every shell. On a shell: `enter` types into it (`ctrl-]` comes back to the list), `f` and `shift-enter` attach, `v` scrolls its history, `!` opens another one in the same place, `x` and `d` close it after you confirm (what runs in it stops; nothing on disk changes); `r`, `R`, `a`, `p` and `N` do nothing. The pane is its screen, titled with where it runs (`Shell #2 · fix-rate-limit`). A shell that closes while selected leaves the selection on its task.

## Filter and prompts

The filter and the project picker match fuzzily: the letters typed, in order, not necessarily together (`frl` finds `fix-rate-limit`), the matched letters highlighted. The filter looks at task names and at each member's name, repository and branch, at a chat's name, title, id and repository, and at the repository of a shell on a main checkout (a shell in a worktree shows with its task) (`enter` keeps it, `esc` empties it with the task found still selected); the picker ranks the best matches first. In the filter, the picker and the name and path prompts, `←`/`→`, `home`/`end` and `ctrl-a`/`ctrl-e` move the cursor, `ctrl-←`/`ctrl-→` by word, `ctrl-w` deletes a word, `ctrl-u`/`ctrl-k` to the start/end. `n` opens the picker at once: the projects under `$HOME` are looked for in the background (those found last time show meanwhile).

## Mouse and copying

The mouse: a click selects a task, a chat or a shell (with the pane open, a member's row shows that member) or a project in the picker; the wheel moves the selection over the list and in the picker, and scrolls the agent's screen over the pane. Dragging over the pane selects the agent's text, and letting go copies it, to the clipboard and the primary selection: the dashboard asks the terminal to (OSC 52, which kitty, WezTerm, Alacritty, foot and others do; in tmux, with `set-clipboard on`; a terminal that does not still shows "Copied"). The selection stays on its text while the agent writes; if the output scrolls part of it out of the pane before you let go, the rest is copied and the footer says so. The next click or key takes it away. The dashboard takes the mouse while it is shown, and lets go of it while attached to an agent. Most terminals still select their own way with `shift` held, but the dashboard redraws while an agent works, and a terminal drops its selection when anything is drawn on its lines.
