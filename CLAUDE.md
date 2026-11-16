# CLAUDE.md

## Project

Rust workspace. Terminal agent multiplexer merging yawn (worktree management)
and zinc (agent daemon) into a task-centric tool. Full design in `tam_manifesto.md`.

## Crate structure

- `tam-proto`    — wire protocol types (daemon <-> client)
- `tam-daemon`   — daemon: PTY management, agent lifecycle, state detection, task ledger, task drop
- `tam-worktree` — library: git ops, worktree CRUD, project discovery, pretty names, init
- `tam-cli`      — binary `tam`: CLI, TUI, bridges worktrees and agents

Dependency graph: `tam-worktree` is standalone. `tam-daemon` depends on `tam-proto`
and `tam-worktree` (dropping a task removes its worktree). `tam-cli` depends on all three.

## Key architecture patterns

- **Task status is always derived** — computed from daemon state + git state +
  filesystem + activity timestamps, never stored. See `task.rs`
  `GitBranchStatus` + `Task::status()`. Staleness is time-based (30 days
  without activity), not git-merge-based.
- **Daemon owns the ledger** — append-only JSONL at
  `$XDG_DATA_HOME/tam/ledger.jsonl` (`tam-daemon/src/ledger.rs`). Records events
  (TaskCreated, AgentRunStarted, etc.), never states. The daemon is the only
  writer (flock on `ledger.jsonl.lock`), records agent runs itself (Spawn with
  `task`), compacts on start and sets malformed lines aside in
  `ledger.jsonl.rejected`. Clients use task requests (CreateTask, ListTasks,
  GetTask, DropTask) and get TaskCreated/TaskUpdated/TaskDropped events after
  `Subscribe`; `tam-cli` never touches the ledger file.
- **Drop is the daemon's** — `DropTask { force, delete_branch }` (`handle_drop_task`;
  git side in `tam-daemon/src/worktrees.rs`, per worktree, over
  `tam_worktree::worktree::{check, remove, delete_branch}`). Without `force` it
  replies `DropRefused` with what would be lost — uncommitted changes, and with
  `delete_branch` commits no other branch/tag/remote holds — or why a directory
  can't be checked, and changes nothing. Otherwise: stop the agent and wait for
  it, check again, remove the worktree (`--force`: checked already, and git
  refuses clean worktrees with submodules otherwise), `git branch -D`, then the
  ledger. The task leaves the ledger only once its worktree is gone; a
  kept branch is a warning in `Dropped`. Git runs outside the state lock; the task
  sits in `dropping` meanwhile, so Spawn into it is refused.
- **Daemon auto-starts/auto-shuts-down** — client spawns it on first connect,
  daemon exits after 30s with no agents and no clients.
- **Provider trait** — `tam-daemon/src/provider.rs`. Claude uses hooks for state
  detection, generic/codex use PTY heuristic (5s idle timeout). Claude's hooks
  are injected per spawn (`claude --settings`) and run `tam hook-notify`, which
  forwards the hook's JSON input to the daemon (`$TAM_SOCKET`, never starting
  one); `ClaudeProvider::handle_hook` maps it to a state and per-agent
  `AgentDetail`. A working Claude agent silent for 10s falls back to input
  (interrupted turns fire no hook).
- **Testing hooks by hand** — run the dev daemon with temp `XDG_RUNTIME_DIR`
  (short path: Unix socket limit), `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and
  `TAM_AGENT_ID`/`TAM_SOCKET` unset. From inside a Claude session also unset the
  `CLAUDE*` variables, or nested Claude sessions write no transcript.
- **Config** — `$XDG_CONFIG_HOME/tam/config.toml` (global), `.tam.toml` (per-repo init).

## Build & check

```bash
cargo fmt
cargo clippy -- -D warnings
cargo test
```

## Testing

- Unit tests are inline in each module (~230 across all crates)
- Integration tests in `crates/tam-daemon/tests/` (~33 async tests, real sockets/PTYs/git repos)
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
