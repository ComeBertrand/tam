//! Run as the real binary: no panic when stdout's reader went away (#38).
//!
//! The daemon listens where the CLI looks for it (`$XDG_RUNTIME_DIR/tam/sock`).
//! Every command runs with temporary XDG directories and HOME, and without
//! the caller's `TAM_AGENT_ID`/`TAM_SOCKET`: it can never reach a real daemon.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use tam_daemon::daemon::Daemon;
use tam_proto::{Request, Response, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// `tam`, isolated in `root`.
fn tam(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tam"));
    cmd.env("XDG_RUNTIME_DIR", root.join("rt"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOME", root.join("home"))
        .env_remove("TAM_AGENT_ID")
        .env_remove("TAM_SOCKET")
        .stdin(Stdio::null());
    cmd
}

/// Run a command whose stdout is a pipe nobody reads: its read end is
/// closed before the command starts, so its first write fails.
async fn output_to_closed_pipe(mut cmd: Command) -> Output {
    let (read, write) = nix::unistd::pipe().unwrap();
    drop(read);
    cmd.stdout(Stdio::from(write)).stderr(Stdio::piped());
    tokio::task::spawn_blocking(move || cmd.spawn().unwrap().wait_with_output().unwrap())
        .await
        .unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

async fn send(stream: &mut UnixStream, request: &Request) -> Response {
    let mut json = serde_json::to_string(request).unwrap();
    json.push('\n');
    stream.write_all(json.as_bytes()).await.unwrap();
    let mut reader = BufReader::new(&mut *stream);
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

/// A daemon where `tam(root)` finds it, with one task (`feat`).
async fn daemon_with_a_task(root: &Path) -> UnixStream {
    let sock = root.join("rt").join("tam").join("sock");
    let daemon = Daemon::new(sock.clone(), root.join("ledger.jsonl"));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    let mut stream = loop {
        if let Ok(s) = UnixStream::connect(&sock).await {
            break s;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let dir = root.join("work").join("feat");
    std::fs::create_dir_all(&dir).unwrap();
    let request = Request::CreateTask {
        name: "feat".into(),
        members: vec![tam_proto::NewMember {
            repo_root: PathBuf::from("/src/app"),
            branch: "feat".into(),
            dir,
        }],
    };
    assert!(matches!(
        send(&mut stream, &request).await,
        Response::Task { .. }
    ));
    stream
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn printing_to_a_closed_pipe_exits_quietly() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let _daemon = daemon_with_a_task(root).await;
    // Something for `tam ls` to find.
    let repo = root.join("projects").join("app");
    std::fs::create_dir_all(&repo).unwrap();
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(git.success());
    let projects = root.join("projects").display().to_string();

    for args in [
        vec!["ps"],
        vec!["ps", "--json"],
        vec!["history", "feat"],
        vec!["history", "feat", "--json"],
        vec!["peers", "--all"],
        vec!["peers", "--all", "--json"],
        vec!["ls", &projects],
        vec!["ls", &projects, "--raw"],
        vec!["ls", &projects, "--json"],
    ] {
        let mut cmd = tam(root);
        cmd.args(&args);
        let out = output_to_closed_pipe(cmd).await;
        assert!(
            out.status.success(),
            "tam {args:?}: {:?}\n{}",
            out.status,
            stderr(&out)
        );
        assert!(
            !stderr(&out).contains("panicked"),
            "tam {args:?}: {}",
            stderr(&out)
        );
    }
}
