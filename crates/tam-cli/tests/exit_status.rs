//! What scripts rely on, run as the real binary: no panic when stdout's
//! reader went away (#38), and exit statuses (with `--json`, the error on
//! stdout) for a missing task and an unreachable daemon.
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

/// Run a command to completion off the async runtime.
async fn output(mut cmd: Command) -> Output {
    tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap()
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

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The JSON document a `--json` command printed.
fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}{}", stdout(out), stderr(out)))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_tasks_exit_with_status_3() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let _daemon = daemon_with_a_task(root).await;

    for args in [
        vec!["wait", "nope", "--json"],
        vec!["result", "nope", "--json"],
        vec!["run", "nope", "--json"],
    ] {
        let mut cmd = tam(root);
        cmd.args(&args);
        let out = output(cmd).await;
        assert_eq!(out.status.code(), Some(3), "tam {args:?}: {}", stderr(&out));
        let doc = json(&out);
        assert_eq!(doc["schema_version"], 1);
        assert_eq!(doc["error"]["kind"], "not_found", "{doc}");
        assert!(
            doc["error"]["message"].as_str().unwrap().contains("nope"),
            "{doc}"
        );
        assert_eq!(stderr(&out), "", "tam {args:?}");
    }

    // Without --json: the error on stderr, stdout left alone.
    let mut cmd = tam(root);
    cmd.args(["stop", "nope"]);
    let out = output(cmd).await;
    assert_eq!(out.status.code(), Some(3));
    assert!(stderr(&out).starts_with("Error: "), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    // Outside any task's worktree, and with no name: not found either.
    let mut cmd = tam(root);
    cmd.arg("result").current_dir(root);
    let out = output(cmd).await;
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(stderr(&out).contains("no task in current directory"));

    // No terminal: no dashboard.
    let out = output(tam(root)).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("needs a terminal"),
        "{}",
        stderr(&out)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_on_a_task_gives_up_on_the_member_that_exited() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut stream = daemon_with_a_task(root).await;
    // A task of two members, api busy and web soon gone.
    let members = ["api", "web"]
        .iter()
        .map(|repo| {
            let dir = root.join("work").join(format!("{repo}--duo"));
            std::fs::create_dir_all(&dir).unwrap();
            tam_proto::NewMember {
                repo_root: PathBuf::from("/src").join(repo),
                branch: "duo".into(),
                dir,
            }
        })
        .collect();
    let create = Request::CreateTask {
        name: "duo".into(),
        members,
    };
    let ids: Vec<String> = match send(&mut stream, &create).await {
        Response::Task { task, .. } => task.members.into_iter().map(|m| m.id).collect(),
        other => panic!("expected Task, got {other:?}"),
    };
    for (id, script) in ids
        .iter()
        .zip(["while :; do echo .; sleep 1; done", "sleep 1; exit 3"])
    {
        let spawn = Request::Spawn {
            provider: "sh".into(),
            dir: PathBuf::new(),
            id: None,
            args: vec!["-c".into(), script.into()],
            resume_session: None,
            prompt: None,
            member: Some(id.clone()),
        };
        let resp = send(&mut stream, &spawn).await;
        assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    }

    let mut cmd = tam(root);
    cmd.args([
        "wait",
        "duo",
        "--until",
        "ready",
        "--timeout",
        "20",
        "--json",
    ]);
    let out = output(cmd).await;
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let doc = json(&out);
    assert_eq!(doc["error"]["kind"], "unreachable", "{doc}");
    let message = doc["error"]["message"].as_str().unwrap();
    assert_eq!(
        message,
        "'duo/web' will not get to ready without someone acting: its agent exited (exit 3)"
    );
    assert_eq!(doc["members"][0]["label"], "duo/api");
    assert_eq!(doc["members"][0]["state"], "working");
    assert_eq!(doc["members"][1]["state"], "exit");
    assert_eq!(doc["members"][1]["exit_code"], 3);

    // No response from either (no hooks): nothing on stdout, why on stderr.
    let mut cmd = tam(root);
    cmd.args(["result", "duo"]);
    let out = output(cmd).await;
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), "");
    assert!(
        stderr(&out).contains("no response from 'duo/api': its agent is in the middle of a turn"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("no response from 'duo/web'"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_daemon_exits_with_status_5() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // A socket path longer than a Unix socket address takes: no daemon can
    // listen there, not even the one the CLI starts.
    let runtime = root.join("r".repeat(120));
    let mut cmd = tam(root);
    cmd.env("XDG_RUNTIME_DIR", &runtime)
        .args(["result", "feat", "--json"]);
    let out = output(cmd).await;
    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
    let doc = json(&out);
    assert_eq!(doc["error"]["kind"], "daemon", "{doc}");
    assert!(
        doc["error"]["message"]
            .as_str()
            .unwrap()
            .contains("failed to connect to daemon"),
        "{doc}"
    );
}
