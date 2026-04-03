# CLAUDE.md

## Project

Rust workspace. Terminal agent multiplexer merging yawn (worktree management)
and zinc (agent daemon) into a task-centric tool. Full design in `tam_manifesto.md`.

## Crate structure

- `tam-proto`    — wire protocol types (daemon <-> client)
- `tam-daemon`   — daemon: PTY management, agent lifecycle, state detection
- `tam-worktree` — library: git ops, worktree CRUD, project discovery, pretty names, init
- `tam-cli`      — binary `tam`: CLI, TUI, task ledger, bridges worktrees and agents

Dependency graph: `tam-worktree` is standalone. `tam-daemon` depends on `tam-proto`.
`tam-cli` depends on all three.

## Key architecture patterns

- **Task status is always derived** — computed from daemon state + git state +
  filesystem + activity timestamps, never stored. See `task.rs`
  `GitBranchStatus` + `Task::status()`. Staleness is time-based (30 days
  without activity), not git-merge-based.
- **Ledger is append-only JSONL** — `~/.local/share/tam/ledger.jsonl`. Records
  events (TaskCreated, AgentRunStarted, etc.), never states.
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
