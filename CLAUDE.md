# CLAUDE.md

## Project

Rust workspace. Terminal agent multiplexer merging yawn (worktree management)
and zinc (agent daemon) into a task-centric tool. Full design in `tam_manifesto.md`.

## Crate structure

- `tam-proto`    — wire protocol types (daemon <-> client)
- `tam-daemon`   — daemon: PTY management, agent lifecycle, state detection
- `tam-worktree` — library: git ops, worktree CRUD, project discovery, pretty names, init
- `tam-cli`      — binary `tam`: CLI, TUI, task ledger, bridges worktrees and agents

## Build & check

```bash
cargo fmt
cargo clippy -- -D warnings
cargo test
```

## Testing

- Unit tests are inline in each module
- Integration tests in `crates/tam-daemon/tests/`
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
