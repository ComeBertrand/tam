# CLAUDE.md

## Project

Rust workspace. Terminal agent multiplexer merging yawn (worktree management)
and zinc (agent daemon) into a task-centric tool. The design is below (Key
architecture patterns); decisions and what is left are in the GitHub issues.

## Crate structure

- `tam-proto`    — wire protocol types (daemon <-> client)
- `tam-daemon`   — daemon: PTY management, agent screens, agent lifecycle, state detection, task ledger, task drop,
  worktree reconciliation (member health, repair, adopt), chats, shells
- `tam-worktree` — library: git ops, worktree CRUD, project discovery, pretty names, init,
  where tam's config and data live (`xdg`, `config::config_path`)
- `tam-cli`      — binary `tam`: CLI, TUI, bridges worktrees and agents

Dependency graph: `tam-worktree` is standalone. `tam-daemon` depends on `tam-proto`
and `tam-worktree` (it runs git on members' worktrees: drop, reconcile, repair,
adopt). `tam-cli` depends on all three.

## Key architecture patterns

- **Task model** — a task is a name plus 1..N members; a member is one
  worktree in one repo (repo, branch, dir) with at most one agent session.
  Every task owns its worktrees (no borrowed tasks). Members have a stable id
  (`m-xxxxxxxx`) independent of task and branch names: it is the member's
  agent id in the daemon and `$TAM_AGENT_ID`. Short names (repo name, else
  `<repo>-<branch>`, assigned in join order, never containing `/`) are derived
  when the ledger is folded, not stored. Agents store only their member id
  (`Agent::owner`, or a chat's id);
  `AgentInfo.task` is looked up when asked, so moving a member to another task
  (merge, split) is one ledger event. Names typed by users (task, `task/member`,
  member id, or the cwd) are resolved by the daemon: `Ledger::resolve` /
  `member_at`, request `ResolveMember`. Chats are the other kind of object
  (see Chats).
- **`ready`** — the state of an agent waiting for a prompt (`AgentState::Ready`,
  `TaskStatus::Ready`) was `input`: that name is still read on the wire, in
  `[notify] on_states` and by `tam wait -u`, never written.
- **Task status is always derived** — computed from agent state + the
  member health the daemon observed in git + activity timestamps, never
  stored. See `task.rs` `Task::status()`: each member has its own status, the
  task shows the most urgent. Unknown health (`None`) is not gone. Staleness
  is time-based (30 days without activity), not git-merge-based.
- **Daemon owns the ledger** — append-only JSONL at
  `$XDG_DATA_HOME/tam/ledger.jsonl`, else `~/.local/share/tam/ledger.jsonl`, on
  macOS too (`tam-daemon/src/ledger.rs`: `default_path`, from
  `tam_worktree::xdg::data_home`; see Config). Records events
  (TaskCreated, MemberAdded, AgentRunStarted keyed by member id, etc.), never
  states. The daemon is the only writer (flock on `ledger.jsonl.lock`), records
  agent runs itself (Spawn with `member`: the daemon supplies the agent id and
  dir), compacts on start and sets malformed lines aside in
  `ledger.jsonl.rejected`. Ledgers from before members (tam <= 0.3.6:
  `task_created` with `dir`/`owned`, runs keyed by task name) are migrated on
  open, line by line (tried as `LegacyEvent` first: serde would otherwise
  accept them as current events): owned tasks become single-member tasks,
  borrowed ones are dropped, the old file is kept as `ledger.jsonl.pre-members`
  (never overwritten: `.pre-members.1`, …).
  Clients use task requests (CreateTask, ListTasks, GetTask, ResolveMember,
  ResolveTarget, DropTask) and get TaskCreated/TaskUpdated/TaskDropped events after
  `Subscribe`; `tam-cli` never touches the ledger file.
- **Git acts where tam points it** — every git tam runs is
  `tam_worktree::git::command()` (the daemon's through `tam-worktree`,
  `tui/branch.rs` too), and every git the tests run. It removes
  `git::LOCAL_ENV_VARS` (`git rev-parse --local-env-vars`: `GIT_DIR`,
  `GIT_INDEX_FILE`, `GIT_WORK_TREE`, …): git exports some of them to `git
  rebase --exec` and to hooks run from a linked worktree (every tam
  worktree), and tam always gives git the directory to act on. `repos::git_env`
  (the caller's environment, for `git::with_env`, and agents' through
  `repos::agent_env`) leaves them out too.
- **Drop is the daemon's** — `DropTask { force, delete_branch }` (`handle_drop_task`;
  git side in `tam-daemon/src/worktrees.rs`, per worktree, over
  `tam_worktree::worktree::{check, remove, delete_branch}`). Without `force` it
  replies `DropRefused` with what would be lost — uncommitted changes, and with
  `delete_branch` commits no other branch/tag/remote holds — or why a directory
  can't be checked, and changes nothing. Otherwise: stop the members' agents
  and close their shells (never a reason to refuse) and wait for them, check
  again, remove each member's worktree (`--force`:
  checked already, and git refuses clean worktrees with submodules otherwise),
  `git branch -D`, then the ledger. The task leaves the ledger only once its
  worktrees are gone; a kept branch is a warning in `Dropped`. Git runs outside the state lock; the task
  sits in `dropping` meanwhile, so Spawn into it is refused.
- **Reconciliation is the daemon's, and only observes** — each member's
  worktree is checked against `git worktree list`, one listing per repository
  (`worktrees::observe`): on start, every 10s (`RECONCILE_INTERVAL`,
  `Daemon::reconcile_every` in tests), at `CreateTask`/adopt/repair, and on
  request `Reconcile` (`tam ps` and the TUI send it first). The result is
  `MemberInfo.health`: `ok` (any branch, or detached, e.g. mid-rebase),
  `gone` (with `cannot_repair` when it cannot be recreated) or `broken` (the
  directory is not a worktree of its repository). The ledger keeps it in memory
  next to the folded tasks (`Ledger::observe`), never in the file; a change is
  pushed as `TaskUpdated`. Reconciliation never creates, removes or records
  anything; it skips tasks being dropped. Paths are compared resolved
  (`worktree::same_path`), as git reports them. The repository of a member
  migrated without `repo_root` is learned from its directory and kept in memory
  only, so after a daemon restart such a gone member can only be dropped.
- **Repair and adopt are explicit** — `RepairMember` (`tam repair`, TUI `d`
  then `r`) recreates a gone worktree on the member's branch
  (`worktree::recreate`, a plain `git worktree add`). It clears only a stale
  registration at the same path on the same branch. `worktree::plan_recreate`
  is the one place that decides whether this is possible, for both health and
  repair. It refuses a locked, detached or other-branch registration, a
  branch that is gone or checked out elsewhere, and a running agent.
  `AdoptWorktree` (`tam adopt`) makes an existing linked worktree a member:
  `worktrees::adoptable` reuses drop's `worktree::check`, and refuses a main
  checkout and a detached HEAD. The new task is named after the branch, or
  the worktree joins an existing task (`--join`, one `MemberAdded`). A lock
  (`DaemonState::worktrees`) serialises reconcile passes, repairs and drops.
- **Agent screen is the daemon's** — `tam-daemon/src/pty.rs` (`Pty`) is the
  terminal half of an agent, shared with shells: the process (leading its own
  session), the PTY master (its fds close-on-exec: no process tam starts
  holds another's terminal), the screen, the output broadcast, the viewers,
  the last output, the reader thread, and how the process is stopped (`kill`:
  SIGTERM then SIGKILL, for agents; `hang_up`, for shells; its process group
  SIGKILLed if dropped alive). `Agent` is a `Pty` plus provider, hooks and
  state. `tam-daemon/src/terminal.rs`: a `vt100`
  parser per agent at its PTY's size (80×24 until a viewer resizes it, both
  resized together in `Pty::resize`), with 1000 rows of history. The PTY
  reader feeds it under its lock, then relays the bytes to attach; it also
  stamps `last_output_at` (silence fallback). `Screen { id, from }` returns
  a window as escape sequences: the live screen, or history from a line
  number (lines keep their number as they scroll into history, so a
  scrolled-back viewer stays put). `WatchScreen { id }` makes one connection
  receive `ScreenUpdated` for one agent, at most one per 100 ms (throttled in
  the connection loop, last change always sent). The TUI preview
  (`tui/peek.rs`) draws the cells itself (tui-term pins vt100 0.16), follows
  `ScreenUpdated`, and unwatches while attached (it reads nothing then).
  Attach replays from it (`Terminal::replay`, taken with the output
  subscription under the terminal lock): history as lines into the viewer's
  own scrollback, the screen row by row (erase-line only: rows below, the
  TUI's status bar, stay), cursor, attributes, input modes (focus reporting
  tracked in the parser callbacks). `Scrollback` returns its plain text.
  Line reads on both ends of the socket use `read_until`: `read_line` loses
  a partly read line when `select!` cancels it.
- **Daemon auto-starts/auto-shuts-down** — client spawns it on first connect,
  daemon exits after 30s (`IDLE_TIMEOUT`, `Daemon::idle_timeout` in tests)
  with no agents, no shells and no clients. `run` checks the shutdown flag
  100 ms after its last accepted connection: a client connecting more often
  keeps it up (a test watches the socket file instead).
- **Agents run in their caller's environment (#50)** — not the daemon's
  (inherited from whichever shell started it, maybe days ago).
  `RunAgent.env` (`tam_proto::AgentEnv`: its `Debug` shows a count only) is
  filled by `Client::run_agent` itself with `repos::agent_env()`, so `tam
  new`, `tam run` and the dashboard all send theirs: `git_env` without
  `repos::NOT_FOR_AGENTS` (a calling agent's `TAM_AGENT_ID`/`TAM_SOCKET`;
  the variables a Claude session sets for its commands, by name, since its
  settings such as `CLAUDE_CONFIG_DIR` must pass: a session started with
  them counts as nested and writes no transcript, and the messaging token
  is a secret; the caller's pane or window, `TMUX`, `ZELLIJ*`, …;
  `COLUMNS`/`LINES`; `PWD`/`OLDPWD`). `Agent::spawn` applies it
  (`pty::Launch`, in `Pty::spawn`; shells too): `env_clear`, the client's variables (the program is
  looked up on its `PATH`), `TERM=xterm-256color` if it has none or `dumb`,
  `PWD` the worktree, then `TAM_AGENT_ID`/`TAM_SOCKET`, whatever the client
  sent. Empty (`Spawn`, tests): the daemon's. Held until the process starts,
  never kept: not in `Agent`, the ledger, logs, events or errors. A resumed
  session takes its caller's too: nothing is remembered. An agent's own
  `tam` commands reach this daemon through the caller's `XDG_RUNTIME_DIR`
  (as the caller did); its hooks through `$TAM_SOCKET`. Known limit: the
  daemon still looks for Claude transcripts (resume check, context) under
  its own `HOME`/`CLAUDE_CONFIG_DIR`.
  The caller's environment is rarely the worktree's dev shell (a dashboard
  launched elsewhere): `[spawn] wrapper` (e.g. `["direnv", "exec", "."]`)
  is read by `Client::run_agent` at each start (a running dashboard sees a
  change) and sent as `RunAgent.wrapper`; `Launch::wrap` gives it the
  provider's command as trailing arguments, in the worktree, and it should
  `exec` the agent (same pid). Evaluated at every start and resume, in the
  worktree, on top of the caller's environment (direnv unloads an `.envrc`
  loaded elsewhere through `DIRENV_DIFF`, which is kept); its output and
  errors are on the agent's screen. A spawn error names it. Not recorded.
- **The agent is Claude** — the only agent a client starts: no `-a`, no
  `default_agent` (an old config's is ignored), no AGENT column.
  `tam-daemon/src/provider.rs` keeps the `Provider` trait for what tests
  run in Claude's place: `Spawn`/`RunAgent` with `provider` set to a
  program (`sleep`, `sh`) give a `GenericProvider`: state guessed from PTY
  silence (5s), no hooks, so no note, no access, no session, and messages
  wait for the member's next agent (nothing is typed into a PTY for them).
  Old ledgers' runs of other programs still load; `handle_run_agent`
  starts Claude whatever the member last ran, in Claude's own current
  session. Claude's state comes from hooks, which
  are injected per spawn (`claude --settings`) and run `tam hook-notify`, which
  forwards the hook's JSON input to the daemon (`$TAM_SOCKET`, never starting
  one); `ClaudeProvider::handle_hook` maps it to a state and per-agent
  `AgentDetail`. A working Claude agent silent for 10s falls back to ready
  (interrupted turns fire no hook).
- **One session per member, resumed by default** — clients start a member's
  agent with `RunAgent { session: current | new | id }` (`handle_run_agent`):
  the daemon picks the session, then spawns as usual. The current session
  of a member is derived, not stored: the session of its latest
  Claude run that has one (`Ledger::current_session`). A run's session
  is the one it resumed, or the last one the agent's hooks showed a turn in
  (`HookData::resumable_session` → `Ledger::session_reported`:
  `AgentSessionIdentified`, then `AgentSessionChanged` on e.g. `/clear`). A
  session nobody typed in has no transcript, so it never counts. `current`
  resumes only if `session_exists()` (Claude deletes old transcripts); an
  explicit id is not checked. No session picker, no transcript scanning
  in `tam-cli`, for a member; a chat is made from its repository's
  history, which the daemon reads (see Chats).
- **Messages** — `tam msg` (`SendMessage`; sender = the member of the
  caller's `$TAM_AGENT_ID`, resolved by the daemon, else the user). Messages
  are ledger events kept with the recipient member (`MessageSent`,
  `MessageDelivered`): undelivered ones survive restarts and wait for the
  member's next agent. Delivery is in `inbox.rs` (daemon handlers at the end
  of `daemon.rs`). Agents get two more per-spawn hooks (`SessionStart`,
  `Stop`) running `tam inbox-wait` with `asyncRewake` (7-day timeout): it
  holds a `WaitInbox` connection (handled like Attach), the daemon hands it
  the queued messages, it prints them on stderr and exits 2, and Claude shows
  them to the model: waking it when idle, else at its next step (Claude's own
  queue, also held while a permission dialog is open). The newest waiter
  wins. Handed over is not delivered: a message is delivered when its id
  shows in the next `UserPromptSubmit` prompt, and goes back to the queue if
  the agent exits first. `WaitReply` (`--wait`): the recipient's next
  message to the sender, or its last response at the end of the turn that
  got it (a message a `--wait` returned counts as delivered in the waiting
  agent's turn, never shown again: two agents waiting on each other each
  get a reply). Loop guard: hop count (at most 6, reset when the user prompts the
  agent) and 20 messages per 10 minutes per agent.
- **Answering requests from a client (#33)** — `tam-daemon/src/answer.rs`.
  Claude's `PermissionRequest` hook is `tam hook-notify` like the others,
  with a 24h timeout (`CLAUDE_PERMISSION_TIMEOUT`): after its `HookEvent` it
  sends `WaitAnswer` on the same connection (handled like `WaitInbox`:
  `handle_wait_answer`) and waits. Claude shows its own dialog meanwhile and
  takes the first answer. Only a request the provider took as pending is
  held. The daemon keeps it (`answer::Asks`: one per agent, a random
  `ask-xxxxxxxx` id) and shows it as `AgentDetail::ask`: the tool's whole
  input as text, the plan, or the questions with their options (16k chars
  at most, `cut`). `Answer { agent, ask, answer }` is taken only for that id
  while its hook is connected; the decision is built from the hook's own
  payload (`answer::decision`), written to that connection only (an answer
  never reaches another request), and the reply waits for the write. Allow
  is once: no rule is added, `permission_suggestions` never echoed. Plan
  approval passes the plan back (`updatedInput`, required) with `setMode
  default` (Claude would restore the mode from before plan mode, maybe
  accepting edits). Questions: `updatedInput.answers`, question text to the
  chosen labels then the typed answer joined by `, `, as Claude's own dialog
  gives them. Deny tells the agent it came from the dashboard, with the
  reason (it goes on) or without (it is asked to stop and wait); never
  `interrupt`: Claude applies it even to a request the user already said
  yes to in the terminal (the hook runs until the tool ends), stopping the
  turn. A late deny without it is ignored. A request `cut` cannot be
  allowed (what was not shown). An answered agent is `working`
  (`Agent::answered`). A request ends without an answer
  (the hook is told `Ok` and prints nothing: the dialog stays, nothing
  approved) when its connection closes (no in the terminal: Claude kills the
  hook; the timeout), its tool runs (`PostToolUse`/`PostToolUseFailure` of
  the same tool whose input agrees on the fields both have: Claude adds a
  question's answers and annotations, drops a plan's text; yes in the
  terminal leaves the hook running), a newer `PermissionRequest` or a turn
  ending or starting comes
  (`Asks::ended_by`, `handle_answer_hook`), or the agent exits (its hooks
  outlive it). MCP elicitation has nothing to
  answer from a client: attach, or `tam msg`. No CLI command answers (an
  approval path agents could call is #29's). The dashboard: `a`
  (`tui/answer.rs`, `Mode::Answer`) opens on the focused member's
  `detail.ask` as it is then and keeps it: the dialog answers that id only;
  `App::answer_stale` (the member's ask is another or none) makes any key
  close it, nothing sent; a `cut` request offers no `y`; keys before its first draw (`shown`, set in
  render: `a` and `y` in one input batch) do nothing; `enter` never
  allows. Permission/plan: `y`, `n` then an optional reason (`TextInput`).
  Questions: one at a time, a row to type one's own answer, a review step
  with several. A refused answer opens a notice. `a` is a reserved key.
  The dashboard sends no message itself: attach and type, or `tam msg`.
- **Peers and discovery** — an agent knows its task from its environment:
  `$TAM_AGENT_ID` (its member id, the only stable handle) and `$TAM_SOCKET`;
  never a task name or peer list, which change (a member joins; a merge,
  #27, moves members). Agents are also told at every `SessionStart`
  (startup, resume, `/clear`, compaction): the daemon replies
  `HookContext` to that `HookEvent` with `peers::session_note` (task, own
  name and id, each peer's address, id, repo, branch and worktree, how to
  reach them), and `tam hook-notify` prints it as `additionalContext`
  (nothing else ever goes to its stdout). The note is a snapshot that says
  so; what changes by the second (what peers' agents are doing) and later
  membership changes are for `tam peers [--all] [--json]` (`peers.rs` in
  `tam-cli`: the caller is the member of `$TAM_AGENT_ID`, else of the cwd;
  JSON has `schema_version`). Short names:
  `Ledger::resolve_from` (`SendMessage`, `ResolveMember { from }`, which
  `Client::resolve_member` fills from `$TAM_AGENT_ID`) takes a member's
  short name from an agent of its task, after everything `resolve` takes,
  so no existing name changes meaning (reply hints use labels). Printed
  addresses are checked: `Ledger::address` (and `tam peers`) give the short
  name only when it resolves to that member from the reader, else the label.
- **Testing hooks by hand** — run the dev daemon with temp `XDG_RUNTIME_DIR`
  (short path: Unix socket limit), `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and
  `TAM_AGENT_ID`/`TAM_SOCKET` unset. From inside a Claude session also unset the
  `CLAUDE*` variables, or nested Claude sessions write no transcript.
- **Config** — `$XDG_CONFIG_HOME/tam/config.toml`, else `~/.config/tam/config.toml`
  (global), `.tam.toml` (per-repo init). The same on every platform (#73):
  `tam_worktree::config::config_path` is the one place that says where, for
  every loader (`tam_worktree::config`, `tam-cli`'s `config` and `brief`,
  `tam-daemon`'s `notify` and `autoclean`). Never `dirs::config_dir()` or
  `dirs::data_local_dir()`: `~/Library/Application Support` on macOS.
  `tam_worktree::xdg` ignores an empty or relative `XDG_*` value, as the
  specification says. Tests write the config once, under `$XDG_CONFIG_HOME`.
- **The pane** — always shown (`App::peek`, `ui::render_pane`): the screen
  of the focused member's agent (or the selected chat's, or the selected
  shell: see Shells in the dashboard), on the whole pane, and nothing said twice
  (`a` answers a request without attaching). `enter` gives the pane the
  keyboard (`Mode::Terminal { agent }`, drawn as `Normal`): every key but
  ctrl-] (`keys::is_leave`; crossterm reads it as ctrl-5) is encoded as a
  terminal sends it (`tui/keys.rs`: `encode`, with the agent's cursor-key
  mode; `paste`, with its bracketed-paste mode: both come with `Screen`,
  whose contents end with `input_mode_formatted`) and typed with
  `Request::Keys { id, data }`, one write per input batch (`type_keys`).
  The daemon writes it to the PTY and, for 2s after, reports the screen
  that connection watches every 15 ms instead of 100 (`ScreenWatch::
  typing_until`): the daemon stays the only terminal emulator, and typing
  echoes at once. The agent's cursor is shown, the border is
  thick. Typing counts as viewing (`WatchScreen.viewing`, a `Viewer` in the
  daemon's `ScreenWatch`): no notification about that agent meanwhile.
  The mode ends on ctrl-] or a click in the list; and, unasked, when the
  pane shows another agent or none or a write fails (`App::check_typing`,
  `App::stop_typing`; not while the agent of the chat it shows starts:
  `App::starting`, see Chats in the dashboard):
  the keys still coming are then dropped until typing pauses
  (`App::drops_key`), never run as the dashboard's. For the same reason a
  re-sort keeps the selection on its entry (`update_state`, `update_agent`,
  `remove_agent` reselect), the wheel over the list does nothing while
  typing, the keys of the batch that brought the focusing `enter` are
  dropped, and the dashboard's terminal always reports pastes as such
  (`bracketed_paste`): outside the pane a paste goes into the prompt being
  typed in, else nowhere (`paste_into_prompt`). `f` is the full-screen attach
  (`Action::Attach`, a reserved key like `enter`'s others), `shift-enter`
  too where the terminal tells it from `enter`: the dashboard asks for the
  kitty keyboard protocol's first flag when the terminal has it
  (`keyboard_protocol`, off while attached). Typed into the pane,
  `shift-enter` is `ESC CR`, a new line in Claude's prompt.
  Its bottom border keeps what the screen may no longer show: why the turn
  failed, what auto mode refused (`ui::agent_detail_line`, not a request it
  waits on), how many messages wait for the agent, and when a chat in its
  last day closes. Without an agent
  there is no screen: `tui/triage.rs` says how its last run ended (`GetTask`
  runs) or what is wrong with its worktree, its branch against the default
  branch with its diff stat, and its latest messages (`ListMessages`, then
  `MessageSent`/`MessageDelivered`). `tui/branch.rs` asks git, read-only
  with `GIT_OPTIONAL_LOCKS=0`, off the UI thread, one worktree at a time
  (`App::branch_due`: the focused member every 10s, the others every 60s for
  the task list, any after its state changes). `h`/`l` like `←`/`→`.
  `v` scrolls the pane (`Mode::Scroll { agent }`, `handle_scroll_key`):
  `j`/`k` (`↓`/`↑`) a line (`Scroll::Down(1)`/`Up(1)`: `Screen { from }`
  again), `g`/`G` (`home`/`end`) the oldest line kept or live; `enter`
  types (`Mode::Terminal`), at the live screen at once (`peek.from`
  cleared), not at the first key; every other key does nothing (neither
  the agent's nor the list's: `shift-enter` is no attach). It ends
  as typing does (`Mode::pane_agent` names the agent of either): ctrl-]
  or a click in the list (`App::leave_pane`: back to the live screen), or
  unasked through `check_typing`/`stop_typing`, the keys still coming
  dropped (a held `j` would move the selection). Thick border, a chip
  saying how to leave, both in the theme's `scrolled` colour (typing's
  are `accent`), the wheel over the list does nothing, and it
  counts as viewing too. No page keys: `pgup`/`pgdn` do nothing, and
  ctrl-u/d/f/b neither (never `d`'s drop or `f`'s attach); the wheel over
  the pane and `home`/`end` scroll in the list's mode too. Member actions
  (enter, `x`, `r`, `R`, `a`, `!`, custom commands) apply to the focused
  member, chat or shell (`App::focused`, an `app::Focused`; `focused_member` for
  task-only code): the one the pane shows: the most urgent one when
  the selection landed on the task, pinned while the task stays selected (a
  key never acts on a member the pane did not show), and `←`/`→` move it.
  The dashboard redraws every second (times in state).
- **Non-interactive CLI** (`tam-cli/src/script.rs`, `schema.rs`) — `tam new`/`run`
  attach only when stdin and stdout are terminals (`script::attaching`), else start
  the agents and print what started (`--detach`; `--json`, a receipt). Failures carry
  a `script::ErrorKind` in the anyhow chain (`CliError`; `client.rs` marks daemon and
  not-found errors): exit 1 failed, 2 usage (clap), 3 not found, 4 timeout, 5 daemon.
  `main` wraps `run` and reports the error, as a JSON document on stdout with
  `--json` unless the command printed it inside its own (`reported`). The JSON
  documents are view structs in `schema.rs`, never wire types: `schema_version` 1,
  every key present (`null`), documented in the README (Scripting); a key removed
  or retyped bumps the version, `ps --json` keeps its pre-version keys. `tam wait`
  is `WaitAgents` (a `dispatch` arm like `WaitReply`: events plus a 1s recheck,
  gives up on `exit`/`error` not waited for; `MemberState::satisfies`: `ready`
  takes `idle`); `tam result` is `GetOutcomes`. A task name means all its members
  there. `MemberOutcome.response` is the Stop hook's `last_assistant_message` in
  full (`HookData::response`, up to 100k chars, cleared at `UserPromptSubmit`;
  `AgentDetail::last_message` stays the 2000-char excerpt every `AgentInfo`
  carries), recorded in `AgentRunEnded.response` when the run ends
  (`Ledger::last_response`): it outlives the agent and the daemon, and a new run
  starts with none (never a stale one). `.tam.toml` init commands print to
  stderr (stdout is the caller's).
- **Printing** — every `tam-cli` module prints through `println!`/`eprintln!`
  redefined at the crate root (`output.rs`, #38): a closed stdout (`tam ps |
  head -1`) exits 0 quietly instead of panicking. Not `SIGPIPE`'s default: it
  would kill a client on a closed daemon socket, silently.
- **Creation is the daemon's, all or nothing** — `CreateMembers { task, members,
  source, join }` (`handle_create_members`, git side in `worktrees.rs`:
  `check_new`, `create_all`, `undo_all`) creates one worktree per member and
  records them as a new task, or with `join` as new members of an existing one
  (`Ledger::add_members`, one append). Under the worktree lock: check every
  member first (main checkout, directory free and not shared, branch not checked
  out elsewhere, start point present: `worktree::check_add`), then create them
  in order (`worktree::add`: local branch, else `origin/<branch>`, else a new
  branch from `source` or the default one; it reports whether it created the
  branch, and what git leaves when it fails: the worktree on a failing
  `post-checkout` hook, the branch alone when the directory cannot be
  created). A failure, or a ledger write that fails (a drop may
  claim a joined task meanwhile: checked again), undoes what was created
  (`worktree::undo_add`): the worktree unless it has uncommitted changes, the
  branch only if this request created it and no other ref holds its commits.
  The error names what is left behind. The daemon's environment is the shell
  that started it, maybe long ago: git runs in the client's instead
  (`CreateMembers.env` from `repos::git_env`, without `git::LOCAL_ENV_VARS`;
  `git::with_env` scopes it to the thread running that git, so
  hooks and filters such as git-lfs see the caller's `PATH`; `RepairMember`
  too). Never recorded or logged. The daemon never fetches: the client does first. `tam new NAME [-r REPO]...
  [--join]` resolves each REPO in `tam-cli/src/repos.rs`: an existing path
  (any directory in the repository), else a project name exactly as `tam ls
  --porcelain` prints it (discovery under `$HOME`, only then, once). Worktree
  directories come from `worktree::target_dir` (`<root>/<repo>--<name>`);
  `.tam.toml` init runs after the task is recorded. With several members `tam
  new` starts one agent each (`-p`: the same prompt) and, when attaching
  (`script::attaching`), attaches to the first (`start_members`); else
  `script::start_detached` gets the new members only (`--json`: no text from
  `new_members`, the receipt lists them). `tam run`/`attach` of a
  multi-member task name refuse and list `TASK/MEMBER` names. The TUI's `n`
  flow does the same (`do_new_task`): `tab` picks several projects
  (`PickerState::marked`, in order), and the name of an existing task adds
  them to it (the name popup says so). `CreateTask` (record worktrees made
  otherwise) is only for tests now. Tests: `crates/tam-daemon/tests/create.rs`,
  `crates/tam-cli/tests/new_task.rs`.
- **Dropping one member** — `DropTask` of `<task>/<member>` or a member id
  (`handle_drop` routes; plain `Ledger::resolve`, never a peer's short name)
  drops that member alone as a task drop does for all (`handle_drop_member`,
  `stop_and_remove_member`: check, stop its agent, check again, remove; the
  task is claimed in `dropping` meanwhile), then `Ledger::drop_member`
  (`MemberDropped`: the member leaves the folded state, so compaction drops
  its history; remaining members' short names are derived again). A task's
  last member takes the task (a warning says so), also when another drop took
  the others meanwhile (counted again under the claim). In the dashboard, the
  drop dialogs of a task with several members offer the focused member alone
  (`m`, `M` with its branch; `MemberChoice`), sent by member id: `<task>/<member>`
  could name another task.
- **Task list** — `tui/table.rs` draws `App::rows`: the chats' section
  (`Row::Chats`, then a `Row::Chat` each, in creation order), the section
  of the shells on main checkouts (`Row::Shells`, then a `Row::Shell`
  each), then a header row per
  repository (`app::repo_group`: the set of a task's repositories, `api +
  web`), a row per task, and below a task with several members a row per
  member (`▸` marks `App::focused_member`); such a task's own row has
  its name only, no status (an agent's, so a member's). Below a member (or
  a task of one), a row per shell in its worktree (see Shells in the
  dashboard). Task, chat and shell
  rows are selected: `App::selected` indexes `App::visible_entries` (an
  `app::Entry` each), found again after a change by `app::Pick` (a chat's
  or shell's id, a task's name: never a label); `App::table_offset` keeps the
  scroll between draws so the selected task's member and shell rows (and its group's
  header) stay in view. `gg`/`G` select the first/last entry
  (`App::pending_g`); `j`/`k` go from the chats to the tasks, through the
  shells; the dashboard starts on the top entry (`set_chats`, `set_shells`,
  then `set_tasks`). `t`
  (`App::toggle_order`, `Order`) switches to attention first: no groups,
  chats and tasks in one list (`Entry::urgency`), a REPO column. Columns: status and time in it
  (`state_since`, else the task's `last_activity`), task, branch
  (`↑`/`↓`/`*` from `App::branches`), context, and from 100 columns
  what the member waits on; `table::fit` drops columns (`DROP_ORDER`)
  before cutting task names. `ui::split`: the pane beside the list from
  136 columns (pane >= 82: an agent's 80-column screen), else below it,
  the list as tall as its rows up to half the height (so the
  what-it-waits-on column shows only stacked, from 100 columns). The footer keeps the hints that
  fit (`ui::fit_hints`, by priority). `tam ps` (`ps.rs`) prints the same
  rows as text: a row per task, most urgent first, and below a task with
  several members a row per member (status, worktree, context), below a
  member (or a task of one) its shells with their ids; the shells on main
  checkouts in a table after the chats' (see Shells).
- **Chats in the dashboard (#67, #68)** — `App::chats` (`chat::Chat`: the info
  and its running agent), from `ListChats` at start, then `ChatCreated`/
  `ChatUpdated` (`App::upsert_chat`, keeping the agent) and `ChatClosed`;
  agent events find a chat's agent as a member's (`App::agent_slot`,
  `find_agent`). A chat's row (`table::chat_cell`): its agent's status,
  else idle; time in state, else since its last turn (`Chat::last`); its
  label (dimmed when neither named nor titled: its id); its repository
  (dim) where a task has its branch, `chat` there attention first (the
  repository in REPO); context; what it waits on, else its last prompt.
  After the name, one cell: `◔` (warn, the name dimmed) in its last day
  (`chat::Closes::last_day`: less than a day before autoclean, or due), `∞`
  (dim) when kept; `table::chat_name` cuts the name before it, never the
  mark. The filter takes its name, title, id and repository. The header
  counts chats, and "need you" and the spinner take them. The pane: its
  screen; without an agent, `triage::chat` (how its last run ended,
  `GetChat` runs; what enter does; its last prompt; its main checkout;
  messages, `ListMessages` with its id); in its last day, the bottom border
  says `closes in 5h · p keeps it` (`ui::closing`; `closes when idle` once
  due). Keys on a chat: `enter` types into it, starting its agent first
  when none runs (`Action::Resume`: the pane takes the keyboard at once,
  `App::starting` keeps it until `AgentSpawned`; a failure gives it back
  with a notice, the way out when its session is gone:
  `chat::session_gone`); `f`/`shift-enter` start it too, then attach; `r`/`R` only say
  that enter resumes it (a chat is its session: never `SessionChoice::New`);
  `d` asks (`Mode::ConfirmCloseChat`, `y`/`enter`) then `CloseChat`; `p`
  keeps or unkeeps (`KeepChat`; the status line says when it closes then);
  `N` names it (`Mode::RenameChat`, empty: Claude's title again; a refusal
  opens a notice); `x`, `a` and custom commands as on a task. `c` (#68)
  makes one: the repository picker (`Purpose::Chat`: the highlighted
  repository, a worktree standing for its main checkout; `tab` does
  nothing), then `ChatHistory` (`Action::ChatHistory`, `open_chat_history`)
  opens `Mode::PickSession` (`app::SessionPicker`: `+ new chat` first, id
  `NEW_SESSION`, then the sessions as the daemon lists them, newest first,
  those a chat holds left out; an item's display, what the filter matches,
  is the title then the last prompt, `SessionPicker::TITLE_GAP` apart,
  drawn as columns with the age by `ui::render_session_popup`; `+ new chat`
  drops out when the filter does not match it, so enter never makes a new
  chat for a filter typed to find a session). `enter` is
  `Action::CreateChat`: `create_chat` sends `CreateChat` (no name: Claude's
  title shows until the user names it), takes the chat at once
  (`upsert_chat`, the filter emptied if it would hide it), selects it,
  starts its agent (`RunAgent`, `Current`) and gives it the keyboard
  (`Mode::Terminal`, `App::starting`); an agent that cannot start closes
  the chat again, as `tam chat` does. A session picked that is still open
  in a plain terminal is not detected (two processes would write it):
  sessions are to be opened from tam. Custom
  commands (`config::CommandValues`): `{task}` the task's name or the
  chat's label, `{id}` the member's or chat's id, `{dir}` the worktree or
  the main checkout, `{provider}`. `c`, `p`, `N`, `v`, `!` and `x` came
  after custom commands could bind them (`config::YIELDING_KEYS`): a config that binds
  one still loads (every command reads it, hooks too), the key runs the
  custom command (`App::key_taken`, checked first in `handle_normal_key`),
  the footer leaves the dashboard's action out and the help marks it
  taken. The other reserved keys still refuse the config
  (`RESERVED_KEYS`).
- **Shells in the dashboard (#82)** — `App::shells` (`ShellInfo`s, in
  opening order), from `ListShells` at start (none from a daemon older than
  shells), then `ShellOpened` (`App::upsert_shell`: the reply to `OpenShell`
  and its event are one shell) and `ShellClosed` (`App::remove_shell`). A
  shell's row (`table::shell_cell`) is `Shell #N` alone: no status, time or
  context (a shell has none, so never "need you", the spinner or the
  attention sort), and no mark (its name, its branch or its section tell it
  apart). A shell in a worktree is a `Row::Shell { tree: Some(app::Branch) }`
  below its member (`App::member_shells`, by number): below the task's own
  row for a task of one (` ├`/` └`), below the member's row in a task with
  several (` │ └` under a member that is not the last, `   └` under the
  last). It shows with its task, wherever the task sorts or the filter
  takes it. Those on main checkouts have the section after the chats, in
  opening order, their repository (dim) where a task has its branch;
  attention first, they come last, the repository in REPO and `shell` in
  BRANCH. The filter matches their repository. Each is an `Entry::Shell`
  (`Pick::Shell(id)`): `visible_entries` puts a worktree's after its task's
  entry and the others after the chats (last, attention first), so
  `table::render` maps a member's row to its task's entry past the shells
  between members. On a shell `App::focused` is `Focused::Shell(shell, its
  member)`; `Focused::terminal` (an agent's id, else the shell's) is what
  the pane shows (`peek_agent`, so `WatchScreen`, `Screen`, `Resize` by the
  shell's id), what `enter` types into (`Mode::Terminal { agent }` then
  holds the shell's id), what `f`/`shift-enter` attach to (no provider in
  the status bar) and `v` scrolls. The pane's title is `Shell #3 · fix-api`
  (its member's label, else its repository). `focused_member` is `None` on a
  shell: `r`, `R`, `←`/`→` and the drop do nothing there, and `a`, `p`, `N`
  neither. `!` (`shell_here`) opens a shell in the focused member's
  worktree, a chat's main checkout, or where the selected shell runs; `s`
  opens the repository picker (`Purpose::Shell`, as `c`'s), then one on the
  checkout picked. Both are `Action::OpenShell`: `open_shell` sends
  `OpenShell` (`Client::open_shell`: `repos::agent_env` and `[spawn]
  wrapper`, as `run_agent`), then `App::take_shell` lists it, selects it
  (the filter emptied if it would hide it) and gives it the keyboard, the
  keys queued behind dropped; a refusal opens a notice. Stop moved from `s`
  to `x`; on a shell `x` and `d` ask (`Mode::ConfirmCloseShell`: `Shell #3
  of fix-api`, `y`/`enter`), then `CloseShell`. A shell that closes while
  selected gives the selection to its task, its member focused (one on a
  main checkout: the entry now in its place, as a closed chat does); typed
  into or scrolled, the pane gives the keyboard back first
  (`stop_typing("the shell closed")`, the keys still coming dropped).
  Custom commands on a shell in a worktree get its member's `{task}`, `{id}`
  and `{dir}`; on a main checkout's, its name, its id and the checkout
  (`Focused::command_values`). The drop dialogs say how many shells a drop
  closes (`ui::drop_closes`). Footer: `!:shell here`, and `s:new shell`, the
  first hint to go. `!` and `x` are in `config::YIELDING_KEYS`, `s` stays
  reserved.
- **Dashboard colours** — `tui/theme.rs`: colours by role (`theme::get()`),
  never a `Color::` in the drawing code. `DARK` (24-bit) when `COLORTERM`
  says truecolor or `[tui] theme = "dark"`, else `ANSI` (the terminal's 16
  colours; what tests draw with). `UNICORN` (`theme = "unicorn"`, Dracula's
  colours) has `Theme::rainbow`: `theme::rainbow(cell, tick)` gives the
  hues; the logo, the selection's bar, the spinners, the group rules
  (along them) and the pane's border move with `App::spin` (the loop then
  ticks all the time: `theme::animated`, off with `[tui] animate =
  false`). The border (`ui::rainbow_border`, over the drawn block) goes
  clockwise round `ui::border_path` with `theme::rainbow_around`: a
  whole number of turns of the hues round it, so no seam. `T`
  (`Action::NextTheme`, `theme::next`) switches theme while the dashboard
  runs, never written to the config. Scrolling the pane (`v`: its thick
  border, its chip, its title) is `scrolled` (orange, `ANSI`'s yellow:
  `warn`'s), never typing's `accent`, nor `warn` where the palette has
  another. Tests draw in another theme on their own thread only
  (`theme::in_theme`: tests run side by side). Backgrounds only on the
  header and footer bars, the selected row, chips and dialogs: never
  behind the agent's screen (`peek.rs` keeps the agent's colours). Dialogs draw over a dimmed dashboard
  (`ui::render`, not the footer: it has their keys). A working agent's mark
  is a spinner (`App::spin`, `None` in tests; the loop ticks every 120 ms
  only while one works). What needs the user (block, question, error, ready) is in
  bold. The agent the pane
  shows is given a terminal of the pane's size (`fit_agent`,
  `Peek::wanted_size`: asked once per agent and size, again after an
  attach). `ui::tests::dump_the_dashboard_in_colour` (ignored)
  writes the fixture dashboard as ANSI text to `$TAM_UI_DUMP`.
- **Dashboard input** — prompts (the filter, the picker's filter, the task
  name, a chat's name, the path) are `tui/input.rs` `TextInput`: cursor movement and
  editing, drawn with the terminal's cursor. `tui/fuzzy.rs` matches the
  filter (task, member names, repositories, branches; the list's order
  kept) and ranks the picker, with positions to highlight. `?` is
  `Mode::Help` (custom commands included; any key or click closes). The
  mouse: `mouse_capture` turns on click and wheel reports only (`?1000`,
  SGR `?1006`: no motion), off before attaching (before the input reader
  pauses: a report must never reach the agent) and back after.
  `handle_mouse` uses `App::hits`, where the last draw put the list's rows
  (entry index, member), the pane and the picker's items. Input events are
  handled in batches (all those queued, then one draw); the screen is
  fetched again once per batch (`screen_moved`). `n` opens the picker at
  once (`start_pick_project`, with `App::projects` from last time);
  `discover_projects` runs on a thread, `App::set_projects` merges its
  result, keeping the filter, picks and highlighted item. The picker is
  `Mode::PickProject(picker, Purpose)` (and the path prompt
  `SpawnEnterPath(_, Purpose)`): `n`'s for a task, `c`'s for a chat (see
  Chats in the dashboard), `s`'s for a shell (see Shells in the
  dashboard), one mode so that pastes, the mouse and the projects found
  reach all three.
- **Brief and task directory (#25)** — one brief per task: a reference the
  user gave (URL, ticket id, path; tam never fetches or reads it), recorded
  as `BriefSet` (kept with the task's events, latest only: a re-created
  name starts without one), else the local `brief.md`. Each task has a
  directory `tasks/<name>` next to the ledger (`tam-daemon/src/brief.rs`;
  the name percent-encoded, a leading `.` too, so `tasks/.dropped` is no
  task's) holding `brief.md` and `scratch/`. It is made when first needed
  (`brief::ensure`: member spawn, `GetBrief`, `SetBrief`), so older tasks
  get one; its `.task` marker holds the task's `created_at`, and a directory
  an earlier task of that name left is moved to `.dropped/`. The template
  has `## Shared` and `## <name> (<member id>)` per member (ids: short names
  can change). `handle_drop_task` retires it once the ledger has the drop
  (`brief::retire`): scratch goes, a `brief.md` with more than the template
  (`is_written`) moves to `.dropped/<dir>-<date>.md`, said in `Dropped`
  warnings. `GetBrief`/`SetBrief` take a task name, else a member target,
  and reply `Brief { task, brief: TaskBrief }`; `TaskInfo` does not carry
  it. The `SessionStart` note (`peers::session_note`) adds the brief, the
  heading to write under, the scratch directory, and the peers' worktrees
  the agent can read: those granted at its spawn (`Agent::readable`), not
  later joiners. Claude agents get `provider::TaskAccess` in the
  `--settings` JSON (`claude_access_settings`): `additionalDirectories` (the
  task directory, the peers' existing worktrees) and `deny` of
  `Edit(//<peer>/**)`. Not `--add-dir`: variadic, it would take the prompt
  as a directory. Claude restores neither on `--resume`: every spawn passes
  them (`provider::grants_access`: not to a test's plain program).
  CLI (`tam-cli/src/brief.rs`): `tam new --brief REF` (set after
  `CreateMembers`, before the agents start), `tam brief [NAME] [--set REF |
  --local] [--json]`, and `tam peers` shows it. `[[brief.patterns]]`
  (`match`, `reference`, `prompt`; `{0}`…`{9}`, `{brief}`), read by that
  module only, expand a typed brief and give `tam new` its start prompt
  unless `-p`; a typed path that exists is made absolute. Every member gets
  the same start prompt: its own part comes from the note (its repo, its
  peers) and the brief, or `tam msg`. Merging tasks (#27): the target keeps
  its brief; the sources' references go with them, their directories stay
  in `tasks/` (see Merge and split). Still open: moving a source's
  directory to the target's `scratch/from-<source>/`, a differing reference
  written there and reported, nothing deleted.
- **Merge and split** — `MergeTasks { into, from }` (`tam merge INTO FROM...`)
  and `SplitMember { member, name }` (`tam split TASK/MEMBER [NAME]`, by default
  named after the member's branch) move members between tasks: one ledger
  append (`Ledger::merge_tasks`: a `MemberMoved` per member of each `from` task,
  then its `TaskDropped`; `Ledger::split_member`: `TaskCreated`, `MemberMoved`),
  no git, agents keep running. A moved member joins last: no member already in
  the task is renamed. Compaction must fold a member whose first task is gone:
  `State::move_member` rewrites its kept `MemberAdded` to name its current task,
  keeps only its latest `MemberMoved` (a move into its own task, as a compacted
  ledger has, puts it last again: join order, so short names), and lowers the
  order of the target's `TaskCreated` to the member's `MemberAdded` (task events
  sort first at equal order). The handlers (end of `daemon.rs`) refuse an
  unknown task, a task in `dropping` (at once, then again under the worktree
  lock, taken so that a creation or drop under way ends first), a task's only
  member, a taken or empty name. Events: the source's `TaskDropped` (split: its
  `TaskUpdated`), the target's `TaskUpdated` (`TaskCreated`), then `AgentUpdated`
  for each moved running agent (clients rebuild a member new to a task without
  its agent). Reply `Moved` (tasks, moved ids, labels before, members told; JSON
  `schema::Moved`). Running Claude agents of the members involved are told
  without being woken: `DaemonState::peer_changes` keeps what changed
  (`peers::merged_in`, `merged_away`, `split_away`, `split_off`), and their next
  `UserPromptSubmit` or `PostToolUse` (not a subagent's: payload `agent_id`) gets
  `HookContext` with it and `peers::session_note` (`peers::change_note`); a
  `SessionStart` or the agent's exit forgets it. Names through a merged task stop resolving; member ids do not. Briefs
  (#25): the target keeps its brief, a source's `BriefSet` goes with its task,
  and a task split off starts with the local brief. The sources' directories
  stay in `tasks/` (a later task of that name moves one aside: `brief::ensure`).
  The target's or new task's directory is made at once (`ensure_moved_dirs`):
  the note names it. A moved Claude agent keeps what its spawn granted (task
  directory, readable worktrees: `Agent::readable`, which `change_note` passes
  on, so the note says which peers it can read) until its next start. A drop
  of one member checks under its claim that the member is still in the task it
  claimed.
- **Chats (#63, tracking #70)** — a Claude session on a repository's main
  checkout, started from tam: it owns nothing on disk (no worktree, no
  branch), several run on one repository at once. Its own kind of object,
  not a task with a flag. Ledger: `ChatCreated { chat, dir, name, session }`,
  `ChatRenamed`, `ChatKept`, `ChatTurn`, `ChatClosed`; the id (`c-xxxxxxxx`) is
  its agent's id and `$TAM_AGENT_ID`. Its agent's runs, sessions and messages
  are the member events with the chat id in `member`/`to`/`from`:
  `ledger::History` (runs, response, messages, events) is shared by
  `MemberRecord` and `ChatRecord`, `State::history(id)` finds either, so
  `run_started`, `session_reported`, `current_session`, `send_message`… take
  either id; `run_started`/`run_ended` return an `Owner` (`Member(task,
  member)` or `Chat`) whose `updated()` is `TaskUpdated` or `ChatUpdated`.
  Compaction drops a closed chat's events, keeps only the latest rename, keep
  and turn. Listed in creation order (`ChatRecord::order`). Requests (handlers
  at the end of `daemon.rs`): `CreateChat { dir, name, session }` records only
  (`worktrees::check_main_checkout`, the same check as `check_new`; the dir
  resolved; a session is refused unless Claude's files have it in that
  checkout's project (`claude_session_of`), and when another chat holds it;
  checked again at the first start); `RunAgent { member: <chat id> }`
  starts its agent in the checkout, `Current` being the latest session it
  had a turn in, else the one it was made from (`ChatInfo::session`); `ListChats`, `GetChat`,
  `RenameChat`, `KeepChat`, `CloseChat` (agent taken out, `ChatClosed`, then
  killed and waited for outside the lock; replies the chat as it was, with
  its session, to make it again; never touches the checkout, its branch or
  Claude's transcript). Chat targets: an id or a name (`resolve_chat`; names
  unique among chats, trimmed, never id-shaped). Events `ChatCreated`,
  `ChatUpdated`, `ChatClosed`. `ChatInfo::label()`: the name, else the
  title, else the id (only the name and the id resolve). Last turn (#65):
  `ChatTurn` at a `UserPromptSubmit` the user typed, neither a tam message being delivered
  nor a `<task-notification>` (`handle_inbox_hook`), `ChatInfo::last_turn_at`.
  What does not apply: worktree creation, reconciliation and health, repair,
  adopt, drop, brief and task directory, `TaskAccess` (`member_access` is
  members only), merge and split, the start note (`session_note` is members
  only). Answers (`a`), screen, keys, attach and the inbox go by agent id:
  unchanged. Messages: `SendMessage.to` takes a chat's id or name after every
  other kind of target (`Ledger::resolve_recipient`: a target naming a task
  never reaches a chat); messages label a chat by its id (`message_label`:
  reply hints must resolve back); `ListMessages` takes a chat. A chat's
  agent is a caller like any shell: `sender_member` is the chat,
  `ResolveMember { from: <chat> }` resolves as from no task. `resolve` says
  a chat (id or name) is no task member, and the cwd never names a chat
  (`member_at` stays members only): `tam run`/`attach` with no name in a
  main checkout find no task, `tam peers` says "not in a task". Notify:
  `{task}` is a chat's label, `{id}` its id; custom commands give the same, `{dir}`
  being the main checkout (see Chats in the dashboard). Titles and history (#64): the daemon's
  `sessions.rs` is the only reader of Claude's session files
  (`<claude dir>/projects/<project>/<session>.jsonl`; `project_name` is Claude's
  encoding: every UTF-16 unit but ASCII letters and digits a `-`, past 200
  cut and hashed), an undocumented format: a file it cannot read gives
  nothing. From the last 64 KiB, then the first (`WINDOW`: what Claude reads
  for its own list, and Claude writes these lines again every 32 KiB): the
  latest `custom-title` (the user's `/rename`; an empty one takes it back),
  else the latest `ai-title`; the latest `last-prompt`; the file's mtime.
  `ChatInfo::title`/`last_prompt` are those of the session the chat is in
  (`DaemonState::chat_transcript`: its agent's hooks' transcript, else the
  session its agent resumed, else its current session's file; none for a new
  session nobody typed in), kept in the ledger's memory
  (`Ledger::set_chat_session`, never written), read off the lock on start
  before the socket is bound, then every 10s (`CHAT_SESSION_INTERVAL`,
  `Daemon::chat_sessions_every` in tests), `ChatUpdated` when they change.
  `ChatHistory { dir }` (a main checkout) replies `Sessions` (`SessionInfo`:
  id, title, last prompt, `modified_at`), newest first, without the sessions
  with neither title nor prompt and those a chat holds (its `session`, and
  the one its agent is in). Context use reads a chat's transcript from
  `chat_transcript`, never guesses it (several sessions share its
  directory). Known limit: under the daemon's `HOME`/`CLAUDE_CONFIG_DIR`.
  The dashboard: see Chats in the dashboard.
  CLI (#66, `tam-cli/src/chat.rs`): `tam chat [REPO]` (`repos::resolve`,
  as `new -r`: from a worktree, its main checkout) is `CreateChat`, then
  `KeepChat` with `--keep`, then `RunAgent` (`Current`); a failure after
  the create closes the chat again (never ran, and it would hold
  `--session`). `tam close CHAT [-y]` (confirms on a terminal: refused,
  exit 1, without one and without `-y`; "n" keeps it; prints the `tam chat
  REPO --session S` that makes it again), `tam rename CHAT [NAME]`, `tam
  keep CHAT [--off]` take chats only (`GetChat` first: 3 not found, then 1
  refused); a task's name gets a hint. The commands that take a member
  take a chat through `ResolveTarget` (`Client::resolve_target`,
  `main::resolve_target`): `Ledger::resolve_recipient`, the rule of `tam
  msg`, so a task (name, `TASK/MEMBER`, member id, a peer's short name)
  always comes before a chat's id, then its name; `stop`, `wait`, `result`,
  `history` try a task name first (all its members). `check_chat_name`
  refuses a task's name and `<task>/…`; a task made later takes the name,
  the chat stays reachable by id. `GetOutcomes`/`WaitAgents` take chats
  (`resolve_members`, `outcome`: `MemberOutcome.task` empty). `tam run
  CHAT` refuses `--new-session`/`--session` (the daemon would not check
  another chat holds it); a session that can no longer be resumed gets
  the way out (`chat::cannot_resume`: `tam close`, then `tam chat`).
  `drop` and `merge` say a name is a chat's (`chat::not_a_task`; the
  others through `Ledger::resolve`, which says so for a chat's id or
  name). `tam history TASK/MEMBER` (or a member id) shows its task's.
  Shown by `ChatInfo::label()` (name, else Claude's title, else id), named
  by id in every hint (`chat::describe`, `tam attach c-…`): a title does
  not resolve. `tam ps`: a chats table above the tasks (`ps::chat_lines`,
  an ID column, its own header), status the agent's else idle, LAST the
  last turn else creation, CLOSES `closes_at` as time left rounded up
  (`ps::closes`: `in 3d`, `due`, `kept`, `-` off); `ps --json` a new
  `chats` key (`schema::PsChat`: `id`, `name`, `label`, `title`,
  `last_prompt`, `keep`, `closes_at`, …, then a member's agent keys).
  Documents for a chat (`schema::Subject`, flattened): `chat` where a
  task's have `task`, the chat the one entry of `members` (`name` null when
  unnamed, no `branch`); `close`/`rename`/`keep --json` print
  `schema::ChatDoc`. Schema version stays 1.
- **Chat autoclean (#65)** — `tam-daemon/src/autoclean.rs`, passes at the
  end of `daemon.rs`. A chat is closed by itself after `[chat] autoclean`
  days without a turn (3 by default, fractions allowed; `0`/`false` off,
  `true` 3; a config or value that cannot be read: off, warned in the
  daemon's log, so a typo never starts closing chats). Elapsed time from
  the latest of its creation (also for one made from an existing session),
  its last `ChatTurn` and its last un-keep (`ChatRecord::unkept_at`, from
  the `ChatKept` compaction keeps). A `ChatTurn` is also recorded when the
  user answers what a chat's agent waited on (`DaemonState::user_answered`:
  a hook takes it out of `blocked`/`question`, or `Answer`): only the user
  ends that. A kept chat is exempt; starting its agent is no turn.
  `clean_chats` runs at start, before the socket listens (no agent runs
  then: every chat past its time goes), then every `autoclean::INTERVAL`
  (60s; `Daemon::autoclean_every` in tests), reading the config again each
  time (`autoclean::Source`; `Daemon::close_chats_after` gives the limit
  instead, for tests). In one hold of the lock it closes the chats past
  `closes_at` whose agent is not running, or is `ready`/`idle`/`error` with
  no viewer (attached, or typing in a pane: `autoclean::may_close`), through
  `DaemonState::close_chat` as `CloseChat` does (`AgentExited`, `ChatClosed`,
  the agent killed outside the lock); only the log says it was autoclean.
  Clients get `ChatInfo::closes_at` (from when it is closed, once at the
  prompt; `None`: kept or off), computed in `State::chat_info` from the
  limit the daemon gives the ledger (`Ledger::set_chat_limit`, in memory; a
  change pushes `ChatUpdated` for every chat): #67's last day and time left.
- **Shells (#81, tracking #80)** — the user's `$SHELL` on a PTY the daemon
  owns, in a task member's worktree or a repository's main checkout
  (`tam-daemon/src/shell.rs`, handlers at the end of `daemon.rs`): a `Pty`
  plus where it runs and its number. Not an agent: `DaemonState::shells`,
  apart from `agents`, under ids of their own (`sh-xxxxxxxx`), so hooks,
  state, notify, ledger runs, the inbox, wait/result, autoclean and `List`
  never see one; nothing in the ledger (a daemon restart closes them, none
  is resumed). `DaemonState::pty` finds an agent's or a shell's PTY for
  `Attach`, `Screen`, `WatchScreen`, `Keys`, `Resize` and `Scrollback`
  (`not_found` says shell or agent by the id's shape); `Kill` refuses a
  shell's id, `Spawn` an id a shell has. Requests: `OpenShell { on:
  ShellPlace::Member(target) | Checkout(dir), env, wrapper }` (a member as
  `Ledger::resolve` takes it, refused while its task is in `dropping` or its
  health is gone or broken, checked, numbered and inserted in one hold of
  the lock; a checkout through `main_checkout`, as `CreateChat`, git before
  the lock), `ListShells` (in opening order, `Shell::seq`), `CloseShell {
  id }` (replies the shell as it was, once it is gone); events
  `ShellOpened`, `ShellClosed`; `ResolveTarget` replies `Shell` for an open
  shell's id, after every other kind. `ShellInfo`: id, member (`None` on a
  checkout), dir, repo, number, pid, opened_at, viewers; `name()` is
  `Shell #3`. The number counts per place (`shell::Place`: a member by id,
  so a merge or split keeps its shells and their numbers; a checkout
  resolved): the highest open there plus one, 1 when none
  (`shell::next_number`; exited shells reaped first). Program: `$SHELL` of
  the client's env (the daemon's when it sent none), else `/bin/sh`, no
  arguments (no login shell: it would redo the caller's environment),
  through `[spawn] wrapper` (`Launch`, as `RunAgent`); `TAM_AGENT_ID` and
  `TAM_SOCKET` removed whatever the env (`NOT_FOR_SHELLS`): what is typed
  there is the user's (`tam msg` from a shell is from the user). Closing
  (`Pty::hang_up`): SIGHUP, which bash and zsh pass to their jobs (each in
  a process group of its own: the group SIGKILL alone, `Pty::kill`'s
  fallback, orphans them); SIGKILL of its process group if it is still
  there after 1s; then, for at most 1s, until nothing holds the PTY
  (`Pty::released`: the reader ended), so the shell a forking wrapper
  started and its jobs are gone too. Known limits: dash (`sh` on Debian)
  does not hang up its background jobs, they stay; a wrapper that forks the
  shell instead of `exec`ing it leaves the shell while it runs a
  foreground job (the kernel hangs up that job only). Exit: polled with the
  agents (`reap_exited` → `reap_shells`), also before `ListShells` and a
  number is given. Drop: `stop_and_remove`/`stop_and_remove_member` take
  the members' shells with their agents (`take_shells_of`, `ShellClosed`)
  and `stop_all` waits for them all before any worktree goes; never a
  `DropRefused` reason, and `Dropped` does not count them (a client counts
  its `ShellInfo`s). Lifetime: an open shell keeps the daemon up;
  `Shutdown` closes them. Clients: the dashboard (#82, see Shells in the
  dashboard) and the CLI (#83, `tam-cli/src/shell.rs`). `tam shell
  [TARGET] [-r REPO] [-d] [--json]` (`shell::open`) opens one through
  `Client::open_shell` (as the dashboard: `repos::agent_env`, `[spawn]
  wrapper` read at each open), on: TARGET resolved by the client
  (`resolve_target`, as `tam run`: a member, then opened by its id; a
  chat's main checkout; a shell's id refused); `-r` a main checkout
  (`repos::named`, shared with `tam chat REPO`); else the member at the
  cwd, else the main checkout holding it, never a linked worktree no task
  has (refused, exit 3, naming `tam adopt` and `tam shell -r .`: it would
  open elsewhere than asked). It attaches when `script::attaching`; else
  its id alone on stdout, how to attach on stderr; `--json`
  `schema::ShellDoc` (`shell`: its id). `Target::Shell` (`ResolveTarget`'s
  `Shell`): `tam attach` and `tam stop` (`CloseShell`, no confirmation: an
  id is typed on purpose, as `tam stop` of an agent) take it; `run`,
  `msg` (after `SendMessage` fails), `wait`, `result`, `history`, `drop`,
  `merge`, `close`, `shell` say it is one (`shell::misnamed`, exit 3, as
  "is a chat"); a shell-shaped id no longer open is "no open shell". A
  shell is named by its id in every hint; `shell::named`/`place` (`Shell
  #3 of fix-api`, the member's label else the repository) and the drop
  wording (`count`, `closes_with_it`) are the dashboard's too. `tam ps`:
  main-checkout shells in a table after the chats (`ps::shell_lines`:
  SHELL, ID, DIR), a worktree's below its member by number (`ps::lines`,
  `└─ Shell #2 (sh-…)`, nothing else on the row); `ps --json` a `shells`
  key (`schema::PsShell`, in opening order, `task`/`member` null on a
  checkout). `tam drop` counts the shells of the members it drops
  (`ListShells` first) and says so: before `Drop anyway?`, and `Dropped
  task 'x' and closed its 2 shells`.

## Build & check

```bash
cargo fmt
cargo clippy -- -D warnings
cargo test
```

## Testing

- Unit tests are inline in each module (~340 across all crates)
- Integration tests in `crates/tam-daemon/tests/` (~60 async tests, real
  sockets/PTYs/git repos; `merge.rs`: merge/split replies, events and what
  fake Claude agents are told at their next prompt or tool call; `answer.rs`:
  a fake Claude's waiting permission hook answered, refused when stale,
  released when the request ends otherwise) and `crates/tam-cli/tests/` (`tam hook-notify`
  against a daemon, a permission request's hook printing the decision or
  nothing, session resume with a fake `claude`; one test per file:
  they set process environment; `tam msg`/`inbox-wait`/`history` against a
  daemon, child processes with temp XDG dirs and no
  `TAM_AGENT_ID`/`TAM_SOCKET`; `tam peers` and the start note on real git
  worktrees, with a fake `claude` and an `sh` agent running `tam peers`;
  `merge_split.rs`: `tam merge`/`tam split` text, `--json` and exit statuses;
  `merge_note.rs`: after a merge, the real `tam hook-notify` prints the note
  once, at the next tool call or prompt;
  `noninteractive.rs`:
  new/wait/result/ps --json with no terminal and a fake `claude`;
  `exit_status.rs`: exit statuses and a closed stdout; `brief.rs`: `tam new
  --brief` with a pattern, `tam brief`, a written brief kept on drop;
  `chat_cli.rs`: `tam msg` from a chat's agent and the reply back, `tam
  result` from it, `tam attach`/`run`/`peers` in a main checkout). The
  daemon's `tests/brief.rs` checks the settings a fake `claude` gets and
  the start note. Chats: the daemon's `chats.rs` (sleep agents on real
  repositories: create, list order, rename, keep, close and its events,
  refusals, two chats at once, a restart, close leaving a dirty checkout as
  it was, messages with a member) and `chat_claude.rs` (one test, a fake
  `claude` on the `PATH` sent with `RunAgent`, `CLAUDE_CONFIG_DIR` set:
  resume from a session of its repository, its title from the transcript
  its hooks report, no start note or task access,
  which prompts are turns, inbox delivery, an answer, a restart, close
  keeping the transcript and making the chat again) and `chat_history.rs`
  (one test, `CLAUDE_CONFIG_DIR` set, session files written by hand: the
  history, a chat made from a session of its repository only, its title
  read again as Claude rewrites it, gone while its agent runs a new
  session, there at once after a restart); `sessions.rs` unit tests write
  fixture lines by hand (never copied from real files).
  The CLI's `chat_commands.rs` (one test, the real `tam` with a fake
  `claude` on its `PATH`, hooks played by the test, the process's
  HOME/XDG/`CLAUDE_CONFIG_DIR` temporary): `tam chat -d --json`, `ps` and
  `ps --json`, rename (refused like a task), wait, result and history on a
  chat, `tam msg` both ways, stop and `tam run` resuming its session, a
  response after its agent exited, a session gone, `--session`/
  `--new-session` refused, the task commands saying it is a chat, keep, a
  task named later winning, close refused without `-y` then done, made
  again from its session (its file at `sessions::session_file`), a
  session of no file refused, a missing wrapper closing the chat again.
- Chat autoclean: the daemon's `chat_autoclean.rs` (one test: it sets
  `HOME`, `XDG_CONFIG_HOME` and `CLAUDE_CONFIG_DIR`, so `[chat] autoclean`
  is read from the test's config and no one's `[notify]` runs): pre-dated ledger lines
  closed at start or not, a change of the setting reaching clients; on the
  timer, a fake `claude` with played hooks kept open while blocked,
  working, asking or typed in, an answer recorded as a turn, then closed.
- The dashboard's chats (synchronous, no daemon): `ui.rs` render tests on
  `chat_dashboard()` (the day's `dashboard()` fixture, left as it is, plus
  four chats): the section first and its order, the marks and their
  colours, a cut name keeping its mark, attention first mixing chats and
  tasks, the filter, the pane (screen, `triage::chat`, the closing border,
  `p` taken), the footer and help, the dialogs; `mod.rs` key and event
  tests on `chat_app()`: selection across the sections, enter resuming then
  typing (`App::check_typing` while starting), `f`, `r`/`R`, close with its
  confirmation, keep, rename, custom commands and the keys they took, an
  answer, chat and agent events; `c`: the picker for one repository (a
  worktree standing for its repository, a path typed), the session list
  from a `Sessions` reply (`+ new chat` first, the filter, enter's
  `CreateChat`), its drawing, and `c` taken by a custom command. Resuming
  or creating against a live daemon (the dashboard always starts Claude) is
  not covered by a test: run the dev dashboard by hand in a sandbox (see
  Testing hooks by hand) with a fake `claude` on its `PATH`.
- The dashboard's shells (synchronous, no daemon): `ui.rs` render tests on
  `shell_dashboard()` (`chat_dashboard()` plus two shells below a task of
  one, one below each member of a task with two, three on main checkouts):
  the rows and their branches, the section, attention first, the filter,
  the pane's title and screen, the close dialog and what a drop closes, the
  footer and help (`!` and `x` taken), every mode at any size on a shell;
  `mod.rs` tests on `shell_app()`: the entries' order, `j`/`k`, the mouse,
  events, `!` on each kind of row and `App::take_shell`, `s` and its picker,
  `x` stopping or closing, `d`, the keys that do nothing, `enter`/`f`/`v`, a
  shell closing while selected and while typed in, custom commands and the
  keys they took. Opening, typing into and closing one against a live
  daemon: by hand in a sandbox (see Testing hooks by hand).
- Shells: the daemon's `shells.rs` (real repositories; `sh`, or scripts
  standing for a shell, on the caller's `PATH`): open on a member and a
  checkout, list order, numbers (#1 closed with #2 open: #3; all closed:
  #1; one that exits frees its number), not agents (no agent, no run),
  `ResolveTarget`, `Kill` refused; screen, keys, scrollback, resize (`stty
  size`), a viewing watch, attach by id; closing leaves no job (bash, which
  both CI runners have: a background and a foreground job, then through a
  forking wrapper the daemon knows only by pid); a refused drop leaving the
  shell, a task drop, a member drop and its last member, each closing the
  shells before the worktree goes (`hup-shell`: its HUP trap waits, then
  records whether its worktree is still there; a drop that does not wait
  fails it); the caller's environment (a sentinel, no `TAM_*`, the shell and
  the wrapper only on its `PATH`) in no reply, event, error or file, and no
  shell id in the ledger; a merge keeping a shell and its numbering; the
  daemon kept up by a shell, then shutting down; refusals (gone, broken,
  being dropped, not a main checkout, no member, a chat). The CLI's
  `shells.rs` (one test, the real `tam` with `SHELL=/bin/sh`, a worktree
  `tam new` made): `tam shell -d --json` from a worktree, on a task, with
  `-r` (a project name), from a directory in a main checkout (the id on
  stdout); refused in a worktree no task has, outside any repository, for
  an unknown task or repository; `ps` (the table, the rows below the
  task) and `ps --json`; `run`, `msg`, `wait`, `result`, `history`,
  `drop`, `merge`, `close`, `shell` refusing a shell's id (exit 3, JSON
  `not_found`); `attach` taking it; `stop` by id, then not found; the
  wrapper run in the worktree; `tam drop` saying it closed two.
- The daemon's `xdg.rs`: the worktree config, the notify and autoclean
  settings and the ledger's path follow `$XDG_CONFIG_HOME`/`$XDG_DATA_HOME`, else `~/.config` and
  `~/.local/share`, an empty or relative value ignored, a config in
  `~/Library/Application Support` never read (it tells on macOS CI only).
- `agent_env.rs`, in both crates: the daemon's runs an agent found only on
  the `PATH` sent with `RunAgent` and looks for a sentinel variable (name and
  value) in replies, events, errors and every file next to the ledger,
  before and after a restart, and runs one through a wrapper; the CLI's has
  `tam new` and `tam run` from shells other than the daemon's (a fake
  `claude` only on theirs), then a wrapper configured meanwhile.
- Tests that shell out to git need a real git repo (use tempdir fixtures).
  They build git with `tam_worktree::git::command()`, never
  `Command::new("git")`, and their `tam()` helpers start `tam` without the
  same variables (`git::without_local_env`): run from `git rebase --exec` or a hook in a
  linked worktree, they would otherwise act on this repository (#57).
  `crates/tam-cli/tests/git_dir.rs` sets `GIT_DIR`/`GIT_INDEX_FILE` to a
  sentinel repository for its process, runs the tests' git, the real `tam`
  and the daemon (`new`, `ps`, `drop -b`), and checks the sentinel's files are
  unchanged.

## Commits

Feature-consistent commits. Avoid small fragmented changes — group related
work into coherent commits.

## Releasing

1. Bump `version` in root `Cargo.toml` (workspace.package) and `flake.nix`
2. `cargo check` to regenerate `Cargo.lock`
3. Commit all three files
4. Tag `v<version>`, push commit + tag

## GitHub CLI

`gh` is installed and authenticated.
