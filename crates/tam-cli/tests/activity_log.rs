//! The activity log of the real binary (`$XDG_DATA_HOME/tam/log`): a `cli`
//! line per command (exit status, error, who called it, a signal that
//! interrupted it), a command line clap refused, the daemon a command had
//! to start (its own lines, as `tam daemon` writes them), and nothing at
//! all with `[log] enabled = false`.
//!
//! Every command runs with temporary XDG directories and HOME, and without
//! the caller's `TAM_AGENT_ID`/`TAM_SOCKET`: it can never reach a real
//! daemon.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;

/// `tam`, isolated in `root`.
fn tam(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tam"));
    cmd.env("XDG_RUNTIME_DIR", root.join("rt"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOME", root.join("home"))
        .env_remove("TAM_AGENT_ID")
        .env_remove("TAM_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    tam_worktree::git::without_local_env(&mut cmd);
    cmd
}

fn run(mut cmd: Command) -> Output {
    cmd.output().unwrap()
}

/// Every line of the log under `root`, in order.
fn lines(root: &Path) -> Vec<Value> {
    let dir = root.join("data/tam/log");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .map(|d| d.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    files.sort();
    files
        .iter()
        .flat_map(|f| {
            std::fs::read_to_string(f)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str::<Value>(l).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn find<'a>(lines: &'a [Value], kind: &str, test: impl Fn(&Value) -> bool) -> &'a Value {
    lines
        .iter()
        .find(|l| l["kind"] == kind && test(l))
        .unwrap_or_else(|| panic!("no {kind} line as expected in {lines:#?}"))
}

/// The lines once the daemon wrote that it stopped.
fn after_daemon_stopped(root: &Path) -> Vec<Value> {
    for _ in 0..100 {
        let found = lines(root);
        if found.iter().any(|l| l["kind"] == "daemon.stop") {
            return found;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the daemon did not stop: {:#?}", lines(root));
}

#[test]
fn commands_and_the_daemon_they_start_are_logged() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Starts the daemon.
    let ps = run({
        let mut cmd = tam(root);
        cmd.arg("ps");
        cmd
    });
    assert!(ps.status.success(), "{ps:?}");
    let usage = run({
        let mut cmd = tam(root);
        cmd.arg("frobnicate");
        cmd
    });
    assert_eq!(usage.status.code(), Some(2));
    let missing = run({
        let mut cmd = tam(root);
        cmd.args(["run", "nosuch", "-d", "SECRET PROMPT"]);
        cmd
    });
    assert_eq!(missing.status.code(), Some(3), "{missing:?}");
    // From an agent.
    let status = run({
        let mut cmd = tam(root);
        cmd.arg("status").env("TAM_AGENT_ID", "m-caller");
        cmd
    });
    assert!(status.status.success(), "{status:?}");
    let shutdown = run({
        let mut cmd = tam(root);
        cmd.arg("shutdown");
        cmd
    });
    assert!(shutdown.status.success(), "{shutdown:?}");

    let lines = after_daemon_stopped(root);
    let text = serde_json::to_string(&lines).unwrap();
    assert!(!text.contains("SECRET"), "{text}");

    let ps = find(&lines, "cli", |l| l["cmd"] == "ps");
    assert_eq!(ps["src"], "cli");
    assert_eq!(ps["exit"], 0);
    assert_eq!(ps["terminal"], false);
    assert!(ps.get("agent").is_none(), "{ps}");
    assert!(ps["ms"].as_f64().unwrap() > 0.0);
    let autostart = find(&lines, "daemon.autostart", |_| true);
    assert_eq!(autostart["ok"], true);
    assert_eq!(autostart["pid"], ps["pid"]);

    // The daemon it started: its own lines, the client named by its pid.
    let start = find(&lines, "daemon.start", |_| true);
    assert_eq!(start["src"], "daemon");
    let reconcile = find(&lines, "request", |l| l["req"] == "reconcile");
    assert_eq!(reconcile["peer"], ps["pid"]);
    assert_eq!(reconcile["pid"], start["pid"]);

    let usage = find(&lines, "cli.usage", |_| true);
    assert_eq!(usage["cmd"], "frobnicate");
    assert_eq!(usage["error_kind"], "InvalidSubcommand");

    let missing = find(&lines, "cli", |l| l["cmd"] == "run");
    assert_eq!(missing["exit"], 3);
    assert_eq!(missing["error_kind"], "not_found");
    assert_eq!(missing["target"], "nosuch");
    assert_eq!(missing["prompt_len"], 13);
    assert!(missing["error"].as_str().unwrap().contains("nosuch"));

    let status = find(&lines, "cli", |l| l["cmd"] == "status");
    assert_eq!(status["agent"], "m-caller");

    let stop = find(&lines, "daemon.stop", |_| true);
    assert_eq!(stop["reason"], "requested");
}

fn git(dir: &Path, args: &[&str]) {
    let out = tam_worktree::git::command()
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

#[test]
fn an_interrupted_command_still_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let repo = root.join("src/api");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let new = run({
        let mut cmd = tam(root);
        cmd.args(["new", "feat", "--no-start", "--json", "-r"])
            .arg(&repo);
        cmd
    });
    assert!(new.status.success(), "{new:?}");

    // No agent: it waits to see the message delivered, until interrupted.
    let msg = tam(root).args(["msg", "feat", "hello"]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(msg.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    let out = msg.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(130), "{out:?}");

    let shutdown = run({
        let mut cmd = tam(root);
        cmd.arg("shutdown");
        cmd
    });
    assert!(shutdown.status.success(), "{shutdown:?}");
    let lines = after_daemon_stopped(root);
    let msg = find(&lines, "cli", |l| l["cmd"] == "msg");
    assert_eq!(msg["signal"], "SIGINT");
    assert_eq!(msg["exit"], 130);
    assert_eq!(msg["len"], 5);
    let new = find(&lines, "cli", |l| l["cmd"] == "new");
    assert!(new.get("wait").is_none(), "{new}");
}

#[test]
fn nothing_is_logged_when_the_config_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("config/tam")).unwrap();
    std::fs::write(
        root.join("config/tam/config.toml"),
        "[log]\nenabled = false\n",
    )
    .unwrap();
    for args in [&["ps"][..], &["frobnicate"], &["shutdown"]] {
        let mut cmd = tam(root);
        cmd.args(args);
        run(cmd);
    }
    // The daemon stops a moment after `shutdown` replies.
    std::thread::sleep(Duration::from_millis(500));
    assert!(!root.join("data/tam/log").exists(), "{:#?}", lines(root));
}
