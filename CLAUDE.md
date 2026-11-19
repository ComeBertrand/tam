# CLAUDE.md

## Project

Rust workspace. Terminal agent multiplexer merging yawn (worktree management)
and zinc (agent daemon) into a task-centric tool. Full design in `tam_manifesto.md`.

## Crate structure

- `tam-proto`    — wire protocol types (daemon <-> client)
- `tam-daemon`   — daemon: PTY management, agent screens, agent lifecycle, state detection, task ledger, task drop,
  worktree reconciliation (member health, repair, adopt)
- `tam-worktree` — library: git ops, worktree CRUD, project discovery, pretty names, init
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
  when the ledger is folded, not stored. Agents store only their member id;
  `AgentInfo.task` is looked up when asked, so moving a member to another task
  (merge, split) is one ledger event. Names typed by users (task, `task/member`,
  member id, or the cwd) are resolved by the daemon: `Ledger::resolve` /
  `member_at`, request `ResolveMember`.
- **Task status is always derived** — computed from agent state + the
  member health the daemon observed in git + activity timestamps, never
  stored. See `task.rs` `Task::status()`: each member has its own status, the
  task shows the most urgent. Unknown health (`None`) is not gone. Staleness
  is time-based (30 days without activity), not git-merge-based.
- **Daemon owns the ledger** — append-only JSONL at
  `$XDG_DATA_HOME/tam/ledger.jsonl` (`tam-daemon/src/ledger.rs`). Records events
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
  DropTask) and get TaskCreated/TaskUpdated/TaskDropped events after
  `Subscribe`; `tam-cli` never touches the ledger file.
- **Drop is the daemon's** — `DropTask { force, delete_branch }` (`handle_drop_task`;
  git side in `tam-daemon/src/worktrees.rs`, per worktree, over
  `tam_worktree::worktree::{check, remove, delete_branch}`). Without `force` it
  replies `DropRefused` with what would be lost — uncommitted changes, and with
  `delete_branch` commits no other branch/tag/remote holds — or why a directory
  can't be checked, and changes nothing. Otherwise: stop the members' agents
  and wait for them, check again, remove each member's worktree (`--force`:
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
- **Agent screen is the daemon's** — `tam-daemon/src/terminal.rs`: a `vt100`
  parser per agent at its PTY's size (80×24 until a viewer resizes it, both
  resized together in `Agent::resize`), with 1000 rows of history. The PTY
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
  daemon exits after 30s with no agents and no clients.
- **Provider trait** — `tam-daemon/src/provider.rs`. Claude uses hooks for state
  detection, generic/codex use PTY heuristic (5s idle timeout). Claude's hooks
  are injected per spawn (`claude --settings`) and run `tam hook-notify`, which
  forwards the hook's JSON input to the daemon (`$TAM_SOCKET`, never starting
  one); `ClaudeProvider::handle_hook` maps it to a state and per-agent
  `AgentDetail`. A working Claude agent silent for 10s falls back to input
  (interrupted turns fire no hook).
- **One session per member, resumed by default** — clients start a member's
  agent with `RunAgent { session: current | new | id }` (`handle_run_agent`):
  the daemon picks the agent (requested, else the member's last, else the
  client's default) and the session, then spawns as usual. The current session
  of a member for an agent is derived, not stored: the session of its latest
  run of that agent that has one (`Ledger::current_session`). A run's session
  is the one it resumed, or the last one the agent's hooks showed a turn in
  (`HookData::resumable_session` → `Ledger::session_reported`:
  `AgentSessionIdentified`, then `AgentSessionChanged` on e.g. `/clear`). A
  session nobody typed in has no transcript, so it never counts. `current`
  resumes only for providers that `reports_sessions()` (Claude), and only if
  `session_exists()` (Claude deletes old transcripts); codex resumes only an
  explicit id, generic agents never. No session picker, no transcript scanning
  in `tam-cli`.
- **Messages** — `tam msg` (`SendMessage`; sender = the member of the
  caller's `$TAM_AGENT_ID`, resolved by the daemon, else the user). Messages
  are ledger events kept with the recipient member (`MessageSent`,
  `MessageDelivered`): undelivered ones survive restarts and wait for the
  member's next agent. Delivery is in `inbox.rs` (daemon handlers at the end
  of `daemon.rs`). Claude agents get two more per-spawn hooks (`SessionStart`,
  `Stop`) running `tam inbox-wait` with `asyncRewake` (7-day timeout): it
  holds a `WaitInbox` connection (handled like Attach), the daemon hands it
  the queued messages, it prints them on stderr and exits 2, and Claude shows
  them to the model: waking it when idle, else at its next step (Claude's own
  queue, also held while a permission dialog is open). The newest waiter
  wins. Handed over is not delivered: a message is delivered when its id
  shows in the next `UserPromptSubmit` prompt, and goes back to the queue if
  the agent exits first. Agents without the hook (codex, generic) get it
  typed into the PTY once idle (bracketed paste, else one line), written
  outside the state lock. `WaitReply` (`--wait`): the recipient's next
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
  outlive it). MCP elicitation and codex/generic agents have nothing to
  answer from a client: attach, or `tam msg`. No CLI command answers (an
  approval path agents could call is #29's). The dashboard: `a`
  (`tui/answer.rs`, `Mode::Answer`) opens on the focused member's
  `detail.ask` as it is then and keeps it: the dialog answers that id only;
  `App::answer_stale` (the member's ask is another or none) makes any key
  close it, nothing sent; a `cut` request offers no `y`; keys before its first draw (`shown`, set in
  render: `a` and `y` in one input batch) do nothing; `enter` never
  allows. Permission/plan: `y`, `n` then an optional reason (`TextInput`).
  Questions: one at a time, a row to type one's own answer, a review step
  with several. A refused answer opens a notice. `m` (`Mode::Message`)
  sends `SendMessage` from the user to the focused member's id: the same
  path as `tam msg`, for every provider. `a` and `m` are reserved keys.
- **Peers and discovery** — an agent knows its task from its environment:
  `$TAM_AGENT_ID` (its member id, the only stable handle) and `$TAM_SOCKET`;
  never a task name or peer list, which change (a member joins; a merge,
  #27, moves members). Claude agents are also told at every `SessionStart`
  (startup, resume, `/clear`, compaction): the daemon replies
  `HookContext` to that `HookEvent` with `peers::session_note` (task, own
  name and id, each peer's address, id, repo, branch and worktree, how to
  reach them), and `tam hook-notify` prints it as `additionalContext`
  (nothing else ever goes to its stdout). The note is a snapshot that says
  so; what changes by the second (what peers' agents are doing) and later
  membership changes are for `tam peers [--all] [--json]` (`peers.rs` in
  `tam-cli`: the caller is the member of `$TAM_AGENT_ID`, else of the cwd;
  JSON has `schema_version`). Codex and generic agents get no note (nothing
  is typed into their PTY): the environment and `tam peers`. Short names:
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
- **Config** — `$XDG_CONFIG_HOME/tam/config.toml` (global), `.tam.toml` (per-repo init).
- **Triage pane** — the TUI's detail pane (always shown: `App::peek`;
  `h`/`l` like `←`/`→`, `ctrl-u`/`ctrl-d` like `pgup`/`pgdn`; `tui/triage.rs`,
  `ui::render_pane`) tells, for the focused member of the selected task,
  what it needs from the user without attaching (and `a` answers it when
  the daemon can: the whole request, `AgentDetail::ask`, shown whatever the
  state, which a parallel tool call can move on): its state and since when
  (`AgentInfo.state_since`, stamped by the daemon when it reports the change,
  "now" while a hook's change is not reported yet; the TUI stamps a
  `StateChange` itself), the permission it waits on (tool and one-line
  input: `Pending.summary`) or its question, its last message, why its turn
  failed, what auto mode refused; without an agent, how its last run ended
  (`GetTask` runs) or what is wrong with its worktree; its branch against
  the default branch and its diff stat (`tui/branch.rs`: read-only git with
  `GIT_OPTIONAL_LOCKS=0`, off the UI thread, one worktree at a time:
  `App::branch_due`, the focused member every 10s while shown, the others
  every 60s for the task list, any after its state changes); its latest messages
  (`ListMessages`, then `MessageSent`/`MessageDelivered`); its screen below.
  `tab` shows the screen on the whole pane. Member actions (enter, `s`, `r`,
  `R`, custom commands) apply to the focused member (`App::focused_member`):
  the one the pane shows: the most urgent one when the
  selection landed on the task, pinned while the task stays selected (a key
  never acts on a member the pane did not show), and `←`/`→` move it. The
  dashboard redraws every second (times in state).
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
  gives up on `exit`/`error` not waited for; `MemberState::satisfies`: `input`
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
  (`CreateMembers.env` from `repos::git_env`, without repo-local `GIT_*`
  variables; `git::with_env` scopes it to the thread running that git, so
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
- **Task list** — `tui/table.rs` draws `App::rows`: a header row per
  repository (`app::repo_group`: the set of a task's repositories, `api +
  web`), a row per task, and below a task with several members a row per
  member (`▸` marks `App::focused_member`); such a task's own row has
  its name only, no status (an agent's, so a member's). Only task rows are selected
  (`App::selected` indexes visible tasks); `App::table_offset` keeps the
  scroll between draws so the selected task's member rows (and its group's
  header) stay in view. `gg`/`G` select the first/last task
  (`App::pending_g`). `t` (`App::toggle_order`, `Order`) switches to
  attention first: no groups, a REPO column. Columns: status and time in it
  (`state_since`, else the task's `last_activity`), task, branch
  (`↑`/`↓`/`*` from `App::branches`), agent, context, and from 100 columns
  what the member waits on; `table::fit` drops columns (`DROP_ORDER`)
  before cutting task names. `ui::split`: the pane beside the list from
  136 columns (pane >= 82: an agent's 80-column screen), else below it,
  the list as tall as its rows up to half the height (so the
  what-it-waits-on column shows only stacked, from 100 columns). The footer keeps the hints that
  fit (`ui::fit_hints`, by priority). `tam ps` (`ps.rs`) prints the same
  rows as text: a row per task, most urgent first, and below a task with
  several members a row per member (status, agent, worktree, context).
- **Dashboard input** — prompts (the filter, the picker's filter, the task
  name, the path) are `tui/input.rs` `TextInput`: cursor movement and
  editing, drawn with the terminal's cursor. `tui/fuzzy.rs` matches the
  filter (task, member names, repositories, branches; the list's order
  kept) and ranks the picker, with positions to highlight. `?` is
  `Mode::Help` (custom commands included; any key or click closes). The
  mouse: `mouse_capture` turns on click and wheel reports only (`?1000`,
  SGR `?1006`: no motion), off before attaching (before the input reader
  pauses: a report must never reach the agent) and back after.
  `handle_mouse` uses `App::hits`, where the last draw put the list's rows
  (task index, member), the pane and the picker's items. Input events are
  handled in batches (all those queued, then one draw); the screen is
  fetched again once per batch (`screen_moved`). `n` opens the picker at
  once (`start_new_task_flow`, with `App::projects` from last time);
  `discover_projects` runs on a thread, `App::set_projects` merges its
  result, keeping the filter, picks and highlighted item.
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
  them. Codex and generic agents get nothing (`provider::grants_access`).
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
  `SessionStart` or the agent's exit forgets it. Codex and generic agents are not
  told. Names through a merged task stop resolving; member ids do not. Briefs
  (#25): the target keeps its brief, a source's `BriefSet` goes with its task,
  and a task split off starts with the local brief. The sources' directories
  stay in `tasks/` (a later task of that name moves one aside: `brief::ensure`).
  The target's or new task's directory is made at once (`ensure_moved_dirs`):
  the note names it. A moved Claude agent keeps what its spawn granted (task
  directory, readable worktrees: `Agent::readable`, which `change_note` passes
  on, so the note says which peers it can read) until its next start. A drop
  of one member checks under its claim that the member is still in the task it
  claimed.

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
  --brief` with a pattern, `tam brief`, a written brief kept on drop). The
  daemon's `tests/brief.rs` checks the settings a fake `claude` gets and
  the start note.
- Tests that shell out to git need a real git repo (use tempdir fixtures)

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
