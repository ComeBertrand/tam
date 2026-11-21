//! tam finds its config in `$XDG_CONFIG_HOME/tam`, else `~/.config/tam`, and
//! its data in `$XDG_DATA_HOME/tam`, else `~/.local/share/tam`, on every
//! platform: never in `~/Library/Application Support`, where macOS keeps
//! what `dirs::config_dir()` and `dirs::data_local_dir()` name (#73).
//!
//! Through the loaders themselves: the worktree config (`tam-worktree`), the
//! notify and chat autoclean settings and the ledger's path (`tam-daemon`).
//! The CLI's own two (`[spawn]`, `[[brief.patterns]]`) are read by `tam` in
//! the CLI's `agent_env.rs` and `brief.rs`, from `$XDG_CONFIG_HOME` only.
//!
//! One test in this file: it sets the process environment.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tam_daemon::autoclean::{self, Setting};
use tam_daemon::{ledger, notify};
use tam_worktree::config;

/// A config naming where it was found, in `dir/tam`; its autoclean is `days`.
fn write_config(dir: &Path, name: &str, days: u64) {
    let dir = dir.join("tam");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[worktree]\nroot = \"/from/{name}\"\n\n[notify]\ncommand = \"{name}\"\n\n\
             [chat]\nautoclean = {days}\n"
        ),
    )
    .unwrap();
}

fn days(days: u64) -> Setting {
    Setting::After(Duration::from_secs(days * 86_400))
}

/// What each loader read: (worktree root, notify command, autoclean).
fn read() -> (PathBuf, Option<String>, Setting) {
    let worktree_root = config::load_config().unwrap().worktree_root;
    let notify = notify::load_notify_config().map(|n| n.command);
    (worktree_root, notify, autoclean::load())
}

fn found(name: &str, autoclean: u64) -> (PathBuf, Option<String>, Setting) {
    (
        PathBuf::from(format!("/from/{name}")),
        Some(name.to_string()),
        days(autoclean),
    )
}

#[test]
fn config_and_data_follow_xdg_on_every_platform() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    std::env::set_var("HOME", &home);
    write_config(&root.join("config"), "xdg", 1);
    write_config(&home.join(".config"), "dot-config", 2);
    // macOS's own place: never read.
    write_config(
        &home.join("Library").join("Application Support"),
        "library",
        4,
    );

    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    std::env::set_var("XDG_DATA_HOME", root.join("data"));
    assert_eq!(read(), found("xdg", 1));
    assert_eq!(ledger::default_path(), root.join("data/tam/ledger.jsonl"));

    // Unset, empty or relative (the specification says to ignore it): the
    // directories under HOME.
    for value in [None, Some(""), Some("config")] {
        match value {
            None => {
                std::env::remove_var("XDG_CONFIG_HOME");
                std::env::remove_var("XDG_DATA_HOME");
            }
            Some(value) => {
                std::env::set_var("XDG_CONFIG_HOME", value);
                std::env::set_var("XDG_DATA_HOME", value);
            }
        }
        assert_eq!(read(), found("dot-config", 2), "{value:?}");
        assert_eq!(
            ledger::default_path(),
            home.join(".local/share/tam/ledger.jsonl"),
            "{value:?}"
        );
    }

    // No config there: none at all, whatever ~/Library holds.
    std::env::remove_var("XDG_CONFIG_HOME");
    std::fs::remove_file(home.join(".config/tam/config.toml")).unwrap();
    assert_eq!(
        read(),
        (
            home.join("worktrees"),
            None,
            days(autoclean::DEFAULT_DAYS as u64)
        )
    );
}
