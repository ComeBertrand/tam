//! An agent runs in the environment of the `tam` command that starts it, not
//! the daemon's: `tam new` from one shell, `tam run` from another, each
//! agent gets its caller's variables and `PATH` (the fake `claude` is only
//! on the caller's), without what was the caller's own: a calling agent's
//! `TAM_AGENT_ID`, a calling Claude session's variables, its pane. Then the
//! configured wrapper (`[spawn] wrapper`), read at each start.
//!
//! Kept to a single test: it sets process environment (HOME,
//! XDG_CONFIG_HOME, a variable only the daemon has) for the daemon it runs
//! in this process.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;
use tam_daemon::daemon::Daemon;
use tam_proto::{Request, Response, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

async fn send(sock: &Path, request: &Request) -> Response {
    let mut stream = UnixStream::connect(sock).await.unwrap();
    let mut json = serde_json::to_string(request).unwrap();
    json.push('\n');
    stream.write_all(json.as_bytes()).await.unwrap();
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        match serde_json::from_str::<ServerMessage>(line.trim()).unwrap() {
            ServerMessage::Response(resp) => return resp,
            ServerMessage::Event(_) => continue,
        }
    }
}

async fn start_daemon(sock: &Path, ledger: &Path) {
    let daemon = Daemon::new(sock.to_path_buf(), ledger.to_path_buf())
        .reconcile_every(Duration::from_millis(200));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    for _ in 0..60 {
        if UnixStream::connect(sock).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon did not start");
}

/// `tam`, isolated in `root`, run from `dir`, with no terminal, in a shell
/// whose `PATH` has `bin` (the fake `claude`) and that is an agent's: its
/// Claude session's variables, in a tmux pane.
fn tam(root: &Path, dir: &Path, args: &[&str]) -> Command {
    let path = format!(
        "{}:{}",
        root.join("bin").display(),
        std::env::var("PATH").unwrap()
    );
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tam"));
    cmd.args(args)
        .current_dir(dir)
        .env("XDG_RUNTIME_DIR", root.join("rt"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOME", root.join("home"))
        .env("PATH", path)
        .env_remove("DAEMON_ONLY")
        .env("TAM_AGENT_ID", "m-caller")
        .env("TAM_SOCKET", root.join("elsewhere.sock"))
        .env("CLAUDECODE", "1")
        .env("CLAUDE_CODE_SESSION_ID", "caller-session")
        .env("CLAUDE_CONFIG_DIR", root.join("claude"))
        .env("TMUX", "/tmp/tmux-1000/default,1,0")
        .stdin(Stdio::null());
    cmd
}

async fn output(mut cmd: Command) -> Output {
    tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap()
}

fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "{:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A fake `claude` that writes what it found in its environment to
/// `report`, then waits like an idle session.
fn fake_claude(bin: &Path, report: &Path) {
    let script = format!(
        r#"#!/bin/sh
{{
  echo "caller=$CALLER"
  echo "daemon_only=$DAEMON_ONLY"
  echo "id=$TAM_AGENT_ID"
  echo "socket=$TAM_SOCKET"
  echo "claude_session=$CLAUDECODE$CLAUDE_CODE_SESSION_ID"
  echo "claude_config=$CLAUDE_CONFIG_DIR"
  echo "tmux=$TMUX"
  echo "pwd=$PWD"
  echo "wrapped=$WRAPPED"
}} > '{r}.tmp'
mv '{r}.tmp' '{r}'
exec sleep 3600
"#,
        r = report.display()
    );
    let path = bin.join("claude");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

/// What the agent started last found, by variable.
async fn report(file: &Path) -> std::collections::BTreeMap<String, String> {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(file) {
            let _ = std::fs::remove_file(file);
            return text
                .lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the agent did not start");
}

/// Write tam's config where it reads it: `$XDG_CONFIG_HOME`, or on macOS
/// `~/Library/Application Support`.
fn write_config(root: &Path, text: &str) {
    let macos = root
        .join("home")
        .join("Library")
        .join("Application Support");
    for dir in [root.join("config"), macos] {
        let dir = dir.join("tam");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), text).unwrap();
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_run_in_the_environment_of_the_command_that_starts_them() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let root = root.as_path();

    // The daemon runs here, in this process, with an environment of its
    // own: no `claude` on its PATH.
    std::env::remove_var("TAM_AGENT_ID");
    std::env::remove_var("TAM_SOCKET");
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    std::env::set_var("HOME", root.join("home"));
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    std::env::set_var("DAEMON_ONLY", "1");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let report_file = root.join("agent-env");
    fake_claude(&bin, &report_file);

    let worktrees = root.join("wt");
    write_config(
        root,
        &format!("[worktree]\nroot = \"{}\"\n", worktrees.display()),
    );
    let repo = root.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);

    let sock = root.join("rt").join("tam").join("sock");
    let ledger = root.join("data").join("tam").join("ledger.jsonl");
    start_daemon(&sock, &ledger).await;

    // `tam new` from one shell.
    let mut new = tam(root, &repo, &["new", "feat", "--detach", "--json"]);
    new.env("CALLER", "new");
    let out = output(new).await;
    ok(&out);
    let receipt: Value = serde_json::from_slice(&out.stdout).unwrap();
    let member = receipt["members"][0]["id"].as_str().unwrap().to_string();
    let dir = receipt["members"][0]["dir"].as_str().unwrap().to_string();
    let found = report(&report_file).await;
    assert_eq!(found["caller"], "new");
    assert_eq!(found["daemon_only"], "");
    assert_eq!(found["id"], member);
    assert_eq!(found["socket"], sock.display().to_string());
    assert_eq!(found["claude_session"], "");
    assert_eq!(
        found["claude_config"],
        root.join("claude").display().to_string()
    );
    assert_eq!(found["tmux"], "");
    assert_eq!(found["pwd"], dir);

    // `tam run` from another: the new run takes its caller's, not the last
    // run's.
    ok(&output(tam(root, &repo, &["stop", "feat"])).await);
    let mut run = tam(root, &repo, &["run", "feat", "--detach"]);
    run.env("CALLER", "run");
    ok(&output(run).await);
    let found = report(&report_file).await;
    assert_eq!(found["caller"], "run");
    assert_eq!(found["id"], member);
    assert_eq!(found["wrapped"], "");

    // A wrapper set meanwhile (no restart of anything): the agent runs
    // through it, in its worktree, with its caller's environment.
    let wrap = bin.join("wrap");
    std::fs::write(
        &wrap,
        "#!/bin/sh\nexport WRAPPED=\"$CALLER\"\nexec \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&wrap, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    write_config(
        root,
        &format!(
            "[spawn]\nwrapper = [\"wrap\"]\n\n[worktree]\nroot = \"{}\"\n",
            worktrees.display()
        ),
    );
    ok(&output(tam(root, &repo, &["stop", "feat"])).await);
    let mut run = tam(root, &repo, &["run", "feat", "--detach"]);
    run.env("CALLER", "wrapped run");
    ok(&output(run).await);
    let found = report(&report_file).await;
    assert_eq!(found["wrapped"], "wrapped run");
    assert_eq!(found["id"], member);
    assert_eq!(found["pwd"], dir);

    ok(&output(tam(root, &repo, &["stop", "feat"])).await);
    assert!(matches!(
        send(&sock, &Request::Shutdown).await,
        Response::Ok
    ));
}
