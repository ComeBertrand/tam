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
  (a future merge) is one ledger event. Names typed by users (task, `task/member`,
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
- **Triage pane** — the TUI's detail pane (`p`; `tui/triage.rs`,
  `ui::render_pane`) tells, for the focused member of the selected task,
  what it needs from the user without attaching: its state and since when
  (`AgentInfo.state_since`, stamped by the daemon when it reports the change,
  "now" while a hook's change is not reported yet; the TUI stamps a
  `StateChange` itself), the permission it waits on (tool and one-line
  input: `Pending.summary`) or its question, its last message, why its turn
  failed, what auto mode refused; without an agent, how its last run ended
  (`GetTask` runs) or what is wrong with its worktree; its branch against
  the default branch and its diff stat (`tui/branch.rs`: read-only git with
  `GIT_OPTIONAL_LOCKS=0`, off the UI thread, for the focused member only,
  every 10s while shown and after its state changes); its latest messages
  (`ListMessages`, then `MessageSent`/`MessageDelivered`); its screen below.
  `tab` shows the screen on the whole pane. Member actions (enter, `s`, `r`,
  `R`, custom commands) apply to the focused member (`App::focused_member`):
  the most urgent one; while the pane is open, the one it showed when the
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
  new` starts one agent each and attaches to the first; `tam run`/`attach` of a
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
  the others meanwhile (counted again under the claim).

## Build & check

```bash
cargo fmt
cargo clippy -- -D warnings
cargo test
```

## Testing

- Unit tests are inline in each module (~340 across all crates)
- Integration tests in `crates/tam-daemon/tests/` (~60 async tests, real
  sockets/PTYs/git repos) and `crates/tam-cli/tests/` (`tam hook-notify`
  against a daemon, session resume with a fake `claude`; one test per file:
  they set process environment; `tam msg`/`inbox-wait`/`history` against a
  daemon, child processes with temp XDG dirs and no
  `TAM_AGENT_ID`/`TAM_SOCKET`; `tam peers` and the start note on real git
  worktrees, with a fake `claude` and an `sh` agent running `tam peers`;
  `noninteractive.rs`:
  new/wait/result/ps --json with no terminal and a fake `claude`;
  `exit_status.rs`: exit statuses and a closed stdout)
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
