# TAM Implementation Plan

## Context

TAM (Terminal Agent Multiplexer) merges two existing tools — **yawn** (git worktree manager) and **zinc** (agent multiplexer daemon) — into a single four-crate Rust workspace. The design is fully specified in `tam_manifesto.md`. The current repo contains only the manifesto, a license, and a gitignore.

The goal: a single `tam` command that manages **tasks** — named units of work binding a directory (optionally an owned worktree) to a series of agent runs, with an append-only ledger for persistence and always-derived status.

~79% of the code migrates from yawn/zinc with renames. ~21% is net new (ledger, task model, new CLI dispatch, TUI rework).

---

## Phase 0 — Project Scaffolding & Infrastructure [DONE]

Everything needed so `cargo check` succeeds before any source code lands.

### 0.1 Cargo workspace

Root `Cargo.toml` with four members, resolver 2, workspace-level metadata:
- `version = "0.1.0"`, `edition = "2021"`, `rust-version = "1.85.0"` (yawn's MSRV, higher than zinc's 1.80.0)
- `repository`/`homepage` pointing to `github.com/ComeBertrand/tam`
- Workspace dependencies: union of both projects' deps (serde, tokio, nix, clap, ratatui, crossterm, colored, globset, vt100, etc.)

Crate skeleton:
```
crates/
  tam-proto/     Cargo.toml + src/lib.rs (placeholder)
  tam-daemon/    Cargo.toml + src/lib.rs + tests/
  tam-worktree/  Cargo.toml + src/lib.rs
  tam-cli/       Cargo.toml + src/main.rs + build.rs
```

Key dep graph: `tam-worktree` is standalone (no proto/daemon deps). `tam-daemon` depends on `tam-proto`. `tam-cli` depends on all three.

### 0.2 GitHub Actions

**`.github/workflows/ci.yml`** — best of both projects:
- `fmt`: `cargo fmt --check`
- `clippy`: `cargo clippy -- -D warnings` (with rust-cache)
- `test`: matrix on `ubuntu-latest` + `macos-latest` (cross-platform worktree tests need macOS)
- `msrv`: `cargo check` with toolchain `1.85.0`

**`.github/workflows/release.yml`** — triggered on `v*` tags:
- `create-release`: draft GitHub release with auto-generated notes
- `build`: matrix (x86_64-linux, aarch64-linux, x86_64-darwin, aarch64-darwin), packages `tam` binary as tarball
- `publish`: ordered crates.io publish with 30s delays between each:
  `tam-proto` → `tam-worktree` → `tam-daemon` → `tam-cli`

### 0.3 CLAUDE.md

```markdown
# CLAUDE.md

## Project
Rust workspace. Terminal agent multiplexer merging yawn (worktree management)
and zinc (agent daemon) into a task-centric tool. Full design in `tam_manifesto.md`.

## Crate structure
- `tam-proto`    — wire protocol types (daemon ↔ client)
- `tam-daemon`   — daemon: PTY management, agent lifecycle, state detection
- `tam-worktree` — library: git ops, worktree CRUD, project discovery, pretty names, init
- `tam-cli`      — binary `tam`: CLI, TUI, task ledger, bridges worktrees and agents

## Build & check
cargo fmt
cargo clippy -- -D warnings
cargo test

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
```

### 0.4 Nix

**`flake.nix`**: Based on zinc's flake (nixos-25.11). Builds `tam-cli` crate, installs shell completions (bash/zsh/fish) and man page in `postInstall`.

### 0.5 Other infrastructure files

**`dist-workspace.toml`**: cargo-dist config targeting the `tam-cli` crate, same four platform targets.

**`.worktree-init.toml`**: For TAM's own dev worktrees — `include = [".envrc", "shell.nix"]`.

**`.gitignore`**: Add `result` and `.direnv/` to existing entries.

**`build.rs`** (in tam-cli): Generate shell completions for `tam` and man page `tam.1` via clap_complete/clap_mangen. Include `src/cli.rs` for command definitions. Based on zinc's build.rs.

**`README.md`**: Skeleton with project description, badge placeholders, install section, link to manifesto. Flesh out in Phase 7.

---

## Phase 1 — tam-proto [DONE]

**Source**: `~/Workspace/zinc/crates/zinc-proto/src/lib.rs` (~430 lines)
**Depends on**: Phase 0

Almost entirely mechanical rename. One file.

Changes from zinc-proto:
- `default_socket_path()`: `"zinc"` → `"tam"` in XDG and fallback paths
- `AgentInfo`: add `task: Option<String>` field
- New `Event` variants: `TaskCreated { name, dir, owned }`, `TaskDropped { name }`
- All existing serde roundtrip tests migrate unchanged

---

## Phase 2 — tam-worktree [DONE]

**Source**: `~/Workspace/yawn/src/` (6 modules, ~2,490 lines + ~1,200 lines of tests)
**Depends on**: Phase 0 (no dependency on Phase 1 — fully independent)

Extract yawn's library modules. No dependency on proto/daemon/agents.

```rust
// lib.rs
pub mod config;
pub mod discovery;
pub mod git;
pub mod init;
pub mod pretty;
pub mod worktree;
```

Module-by-module:
| Module | Lines | Changes |
|---|---|---|
| `git.rs` | 314 | Verbatim. Pure `&Path` → `Result` functions. |
| `worktree.rs` | 348 | Verbatim. Uses `crate::config` and `crate::git`. |
| `discovery.rs` | 260 | Verbatim. Only uses `globset` + std. |
| `pretty.rs` | 759 | Verbatim. Keep colored tree output (used by `tam ls`). |
| `init.rs` | 514 | Config file lookup: try `.worktree-init.toml` first, fall back to `.yawn.toml`. Log deprecation if only `.yawn.toml` found. |
| `config.rs` | 295 | Path: `~/.config/tam/config.toml` primary, `~/.config/yawn/config.toml` fallback. Drop `opener`/`finder` fields (yawn session-specific). |

All existing tests (~80 across modules) migrate with minimal changes (config file name references).

---

## Phase 3 — tam-daemon [DONE]

**Source**: `~/Workspace/zinc/crates/zinc-daemon/src/` (~2,030 lines + ~300 lines integration tests)
**Depends on**: Phase 1

Mostly mechanical rename from `zinc_proto` → `tam_proto`.

| File | Lines | Changes |
|---|---|---|
| `daemon.rs` | 623 | Env vars: `ZINC_AGENT_ID` → `TAM_AGENT_ID`, `ZINC_SOCKET` → `TAM_SOCKET`. Log messages: `zincd` → `tamd`. |
| `agent.rs` | 327 | Use `tam_proto`. Add `task` field to `AgentInfo` construction (= agent id). |
| `provider.rs` | 784 | Use `tam_proto`. No logic changes. |
| `scrollback.rs` | 149 | Verbatim. No external deps. |
| `notify.rs` | 145 | Config dir: `"zinc"` → `"tam"`. Template variable `{id}` → `{task}` (keep `{id}` as alias). |
| `tests/integration.rs` | ~300 | Use `tam_daemon`/`tam_proto`. No logic changes. |

The daemon is minimally task-aware — the agent `id` field already serves as the task name. The task abstraction lives in tam-cli's ledger.

---

## Phase 4 — tam-cli: Core [DONE]

**Depends on**: Phases 1, 2, 3

The largest phase. Mix of migration and new code.

### 4.1 New: `cli.rs` — TAM command definitions

Full rewrite (zinc's was 93 lines, TAM's is ~150). Commands from the manifesto:

```
tam                     # TUI (no subcommand)
tam new NAME [-w] [-s REF]
tam run NAME [--new-session]
tam stop [NAME]
tam attach [NAME]
tam drop NAME [-b]
tam gc [--dry-run]
tam ps [--json]
tam ls [PATH] [--json|--raw]
tam pick
tam init --agent NAME
tam shutdown / tam status
tam daemon              # hidden
tam hook-notify         # hidden
```

### 4.2 New: `ledger.rs` (~250 lines)

Append-only JSONL at `~/.local/share/tam/ledger.jsonl`.

Event types: `TaskCreated`, `AgentRunStarted`, `AgentRunEnded`, `WorktreeDeleted`, `TaskDropped` — each with timestamp.

`Ledger` struct: `load()` reads on startup, `append()` writes + flushes, `active_tasks()` derives current state, `task_runs(name)` returns history for session picker, `find_task_by_dir(path)` for cwd resolution.

### 4.3 New: `task.rs` (~150 lines)

`TaskStatus` enum: `Run`, `Input`, `Block`, `Idle`, `Merged`, `Orphan`, `Gone`.

`Task` struct: `name`, `dir`, `owned`, `agent_info: Option<AgentInfo>`, `run_count`, `last_activity`.

`Task::status()` — derived from daemon state + git state:
- Agent running → map `AgentState` to `Run`/`Input`/`Block`
- No agent + owned + branch merged → `Merged`
- No agent + owned + worktree gone → `Gone`
- No agent + owned + branch gone → `Orphan`
- Otherwise → `Idle`

### 4.4 Migrated: `client.rs` (~564 lines from zinc)

`zinc_proto` → `tam_proto`. Daemon auto-start command becomes `tam daemon`. Error messages reference `tam`. All terminal handling (raw mode, KbdProtoFilter, detach on ctrl-]) copies verbatim.

### 4.5 Reworked: `config.rs` (~900 lines)

Merge zinc + yawn config concerns into unified `~/.config/tam/config.toml`.

Sections: `[spawn]`, `[worktree]`, `[discovery]`, `[daemon]`, `[notify]`, `[session]`, `[[tui.commands]]`.

Config fallback chain:
1. `~/.config/tam/config.toml`
2. Worktree settings: fall back to `~/.config/yawn/config.toml`
3. Daemon/agent settings: fall back to `~/.config/zinc/config.toml`
4. Print migration hint on fallback use

`init_agent_hooks()`: writes `tam hook-notify` (not `zinc hook-notify`) to Claude settings. Detection checks both for migration.

### 4.6 Migrated: `sessions.rs` (~400 lines from zinc)

Session discovery (`list_claude_sessions`, `list_codex_sessions`) migrates unchanged. Cross-referenced with ledger data for the session picker.

### 4.7 New: `main.rs` dispatch (~400 lines)

Implements all commands listed in 4.1. Key flows:

- **`tam new NAME`**: check uniqueness (name + dir), create worktree if `-w`, run init if `auto_init`, append `TaskCreated` to ledger
- **`tam run NAME`**: look up task, resolve session (picker or `--new-session`), send `Spawn` to daemon, append `AgentRunStarted`
- **`tam stop`**: resolve name (explicit or from cwd), send `Kill`, append `AgentRunEnded`
- **`tam drop NAME`**: kill agent, delete worktree if owned, delete branch if `-b`, append `TaskDropped`
- **`tam gc`**: iterate owned tasks, check merged status via git, drop merged ones
- **`tam ps`**: build task list from ledger + daemon + git, render table
- **`tam ls`/`tam pick`**: delegate to `tam_worktree` discovery + pretty

---

## Phase 5 — tam-cli: TUI Rework [DONE]

**Depends on**: Phase 4

Transform zinc's agent-centric TUI (~1,370 lines) into TAM's task-centric TUI.

### 5.1 `tui/app.rs` — State model

Replace `agents: Vec<AgentInfo>` with `tasks: Vec<Task>`. New `Mode` enum:
- `Normal`, `FilterActive`, `Peek`
- `NewTaskPickProject`, `NewTaskEnterName { project_dir, name, worktree_toggle, agent_toggle }`
- `RunPickSession { task_name, sessions }`

Sort priority: Block → Input → Run → Idle → Merged → Orphan → Gone.

### 5.2 `tui/mod.rs` — Event loop

New actions: `NewTask`, `RunAgent`, `StopAgent`, `DropTask`, `TogglePeek`, `RefreshPeek`.

Keybindings: `n` (new), `r` (run), `s` (stop), `d` (drop), `p` (peek), `enter` (attach), `/` (filter), `q` (quit). Footer updates dynamically based on selected task state.

Data flow: load ledger → fetch agents from daemon → build task list → subscribe to daemon events → periodically refresh git state (~30s).

### 5.3 `tui/ui.rs` — Rendering

**Task table**: 5 columns — `STATUS`, `TASK`, `AGENT`, `DIR`, `CTX`. Status indicators with color (manifesto spec).

**Header**: `tam — N tasks (M needs input)`.

**New task flow** (two-step modal): project picker → name input with toggles.

**Session picker**: shown on `r` for idle tasks with history.

**Peek panel**: split view with compressed task list on left, scrollback on right.

**Attach status bar**: `tam > {task} ({provider}, {ctx}% ctx, {uptime}) — ctrl-]:detach`

---

## Phase 6 — Integration & Testing [DONE]

### 6.1 Test migration
- Daemon integration tests from zinc → tam-daemon (rename imports)
- All unit tests across all modules (already migrated with their code)

### 6.2 New integration tests
- Ledger: append, reload, derive active tasks
- Task lifecycle: new → run → stop → drop
- Worktree flow: new -w → verify directory → drop → verify cleanup
- GC: create owned task, merge branch, verify gc catches it
- Cwd resolution: `tam stop` / `tam attach` without name

### 6.3 Cross-cutting migration concerns

**Environment variables**: `ZINC_AGENT_ID` → `TAM_AGENT_ID`, `ZINC_SOCKET` → `TAM_SOCKET`. In hook-notify, also accept `ZINC_AGENT_ID` as fallback for users who haven't re-run `tam init`.

**Socket path**: `$XDG_RUNTIME_DIR/tam/sock` (new directory, no migration needed).

**PID file**: `$XDG_RUNTIME_DIR/tam/pid`.

**Ledger path**: `~/.local/share/tam/ledger.jsonl` (new, no migration).

**Config migration**: tam config primary → zinc config fallback for daemon settings → yawn config fallback for worktree settings. Print one-time hint.

**Init file rename**: `.worktree-init.toml` primary, `.yawn.toml` fallback with deprecation log.

---

## Phase 7 — Polish & Ship [TODO]

- Fill out README with usage examples, TUI screenshots, install instructions
- Doc comments on all public API items in tam-worktree
- Final CLAUDE.md review with architecture notes
- Manual end-to-end testing (the 11-step checklist from the manifesto)
- Tag `v0.1.0`, push, verify CI + release workflow

---

## Dependency Graph

```
Phase 0 (scaffolding)
  ├──→ Phase 1 (tam-proto)     ──┐
  ├──→ Phase 2 (tam-worktree)  ──┼──→ Phase 4 (cli core) ──→ Phase 5 (TUI) ──→ Phase 6 (test) ──→ Phase 7 (polish)
  └──→ Phase 3 (tam-daemon) ────┘
       [depends on Phase 1]
```

Phases 1 and 2 are fully independent — can be done in parallel.
Phase 3 depends only on Phase 1.
Phase 4 depends on 1 + 2 + 3.

---

## Estimates

| Phase | Migrated | New | Total |
|---|---|---|---|
| 0 — Scaffolding | 0 | ~500 | ~500 |
| 1 — tam-proto | ~430 | ~20 | ~450 |
| 2 — tam-worktree | ~2,490 | ~30 | ~2,520 |
| 3 — tam-daemon | ~2,330 | ~20 | ~2,350 |
| 4 — cli core | ~1,960 | ~800 | ~2,760 |
| 5 — TUI rework | ~800 | ~700 | ~1,500 |
| 6 — Testing | ~300 | ~400 | ~700 |
| 7 — Polish | — | ~200 | ~200 |
| **Total** | **~8,310** | **~2,670** | **~10,980** |
