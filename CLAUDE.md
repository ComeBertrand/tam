# CLAUDE.md

## Project

Rust workspace. Terminal agent multiplexer merging yawn (worktree management)
and zinc (agent daemon) into a task-centric tool. Full design in `tam_manifesto.md`.

## Crate structure

- `tam-proto`    — wire protocol types (daemon <-> client)
- `tam-daemon`   — daemon: PTY management, agent lifecycle, state detection, task ledger
- `tam-worktree` — library: git ops, worktree CRUD, project discovery, pretty names, init
- `tam-cli`      — binary `tam`: CLI, TUI, bridges worktrees and agents

Dependency graph: `tam-worktree` is standalone. `tam-daemon` depends on `tam-proto`.
`tam-cli` depends on all three.

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
- **Daemon auto-starts/auto-shuts-down** — client spawns it on first connect,
  daemon exits after 30s with no agents and no clients.
- **Provider trait** — `tam-daemon/src/provider.rs`. Claude uses hooks for state
  detection, generic/codex use PTY heuristic (5s idle timeout).
- **Config** — `$XDG_CONFIG_HOME/tam/config.toml` (global), `.tam.toml` (per-repo init).

## Build & check

```bash
cargo fmt
cargo clippy -- -D warnings
cargo test
```

## Testing

- Unit tests are inline in each module (~230 across all crates)
- Integration tests in `crates/tam-daemon/tests/` (19 async tests, real sockets/PTYs)
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
