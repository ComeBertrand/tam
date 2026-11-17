//! End to end: what an agent tam starts knows of its task and peers. The
//! note its `SessionStart` hook prints (`tam hook-notify`), `tam peers`,
//! and short names for `tam msg`, run as the real binary against a daemon,
//! on real git worktrees.
//!
//! Kept to a single test: it sets process environment (PATH for a fake
//! `claude`, the XDG directories the agents inherit), which must not race
//! other tests in this binary. Every command runs with temporary XDG
//! directories and HOME, and without the caller's `TAM_AGENT_ID` and
//! `TAM_SOCKET` unless a test sets them: it can never reach a real daemon.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;
use tam_daemon::daemon::Daemon;
use tam_proto::{NewMember, Request, Response, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

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

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// `tam` run by the agent of `member`.
fn tam_as(root: &Path, member: &str) -> Command {
    let mut cmd = tam(root);
    cmd.env("TAM_AGENT_ID", member);
    cmd
}

/// Run a command that must succeed; its stdout.
async fn ok(cmd: Command) -> String {
    let out = output(cmd).await;
    assert!(out.status.success(), "{}", stderr(&out));
    stdout(&out)
}

/// `tam peers --json` (and `extra` arguments) as JSON.
async fn peers_json(mut cmd: Command, extra: &[&str]) -> Value {
    cmd.args(["peers", "--json"]).args(extra);
    serde_json::from_str(&ok(cmd).await).unwrap()
}

/// `(address, task)` of each peer in `tam peers --json` output.
fn addresses(json: &Value) -> Vec<(String, String)> {
    json["peers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["address"].as_str().unwrap().to_string(),
                p["task"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// Run `tam hook-notify` as Claude runs the hooks tam injects; its stdout.
async fn hook_notify(root: &Path, sock: &Path, agent: &str, payload: Value) -> String {
    let mut cmd = tam(root);
    cmd.arg("hook-notify")
        .env("TAM_AGENT_ID", agent)
        .env("TAM_SOCKET", sock)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    stdout(&out)
}

/// The note a `SessionStart` hook prints for the agent of `agent`.
async fn session_note(root: &Path, sock: &Path, agent: &str, source: &str) -> String {
    let payload = serde_json::json!({
        "hook_event_name": "SessionStart",
        "session_id": "sess-1",
        "source": source,
    });
    let out = hook_notify(root, sock, agent, payload).await;
    let out: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out:?}"));
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "SessionStart");
    out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_string()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", stderr(&out));
}

/// A repository `name` with one commit, and a worktree of it on a new
/// branch `branch`; returns the worktree.
fn worktree(root: &Path, name: &str, branch: &str) -> PathBuf {
    let repo = root.join("src").join(name);
    if !repo.exists() {
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    }
    let dir = root.join("w").join(format!("{name}--{branch}"));
    git(
        &repo,
        &["worktree", "add", "-q", "-b", branch, dir.to_str().unwrap()],
    );
    dir
}

fn new_member(root: &Path, dir: &Path, branch: &str) -> NewMember {
    let repo = dir.file_name().unwrap().to_str().unwrap();
    let repo = repo.split("--").next().unwrap();
    NewMember {
        repo_root: root.join("src").join(repo),
        branch: branch.into(),
        dir: dir.to_path_buf(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_know_their_task_and_peers() {
    let tmp = tempfile::tempdir().unwrap();
    let root = &std::fs::canonicalize(tmp.path()).unwrap();

    // Agents inherit this environment: only temporary directories, and a
    // fake claude that stays up like an idle session.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("claude");
    std::fs::write(&fake, "#!/bin/sh\nexec sleep 3600\n").unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::env::set_var(
        "PATH",
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
    );
    std::env::set_var("HOME", root.join("home"));
    std::env::set_var("XDG_RUNTIME_DIR", root.join("rt"));
    std::env::set_var("XDG_DATA_HOME", root.join("data"));
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    std::env::remove_var("TAM_AGENT_ID");
    std::env::remove_var("TAM_SOCKET");

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

    // One task over two repositories.
    let backend_dir = worktree(root, "backend", "feat");
    let frontend_dir = worktree(root, "frontend", "feat");
    let request = Request::CreateTask {
        name: "feat".into(),
        members: vec![
            new_member(root, &backend_dir, "feat"),
            new_member(root, &frontend_dir, "feat"),
        ],
    };
    let (backend, frontend) = match send(&mut stream, &request).await {
        Response::Task { task, .. } => (task.members[0].id.clone(), task.members[1].id.clone()),
        other => panic!("expected Task, got {other:?}"),
    };
    let resp = send(
        &mut stream,
        &Request::Spawn {
            provider: "claude".into(),
            dir: PathBuf::new(),
            id: None,
            args: vec![],
            resume_session: None,
            prompt: None,
            member: Some(backend.clone()),
        },
    )
    .await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");

    // The session starts knowing its task and its peer.
    let note = session_note(root, &sock, &backend, "startup").await;
    assert!(
        note.starts_with(&format!(
            "[tam] This session is the agent of backend (member {backend}) in task feat."
        )),
        "{note}"
    );
    assert!(
        note.contains(&format!(
            "\n- frontend ({frontend}): repo frontend, branch feat, worktree {}\n",
            frontend_dir.display()
        )),
        "{note}"
    );
    assert!(note.contains("tam msg frontend \"<text>\""), "{note}");
    // ... its brief (the local file by default) and scratch directory, and
    // that it can read its peer's worktree.
    let task_dir = root.join("tasks").join("feat");
    assert!(
        note.contains(&format!(
            "brief, what its members work from: the file {}.",
            task_dir.join("brief.md").display()
        )),
        "{note}"
    );
    assert!(
        note.contains(&format!("\"## backend ({backend})\"")),
        "{note}"
    );
    assert!(
        note.contains(&format!(
            "shared by its members: {}.",
            task_dir.join("scratch").display()
        )),
        "{note}"
    );
    assert!(
        note.contains("\nYou can read the other members' worktrees, not edit them.\n"),
        "{note}"
    );
    // Other events print nothing: Claude reads a hook's stdout.
    let quiet = hook_notify(
        root,
        &sock,
        &backend,
        serde_json::json!({"hook_event_name": "Stop", "last_assistant_message": "ok"}),
    )
    .await;
    assert_eq!(quiet, "");
    let quiet = hook_notify(
        root,
        &sock,
        &backend,
        serde_json::json!({"hook_event_name": "UserPromptSubmit", "prompt": "go"}),
    )
    .await;
    assert_eq!(quiet, "");

    // A member joins later: the next session start (a resume here) says so,
    // and so does tam peers right away.
    let docs_dir = worktree(root, "docs", "feat");
    let request = Request::AdoptWorktree {
        dir: docs_dir.clone(),
        name: None,
        join: Some("feat".into()),
    };
    let docs = match send(&mut stream, &request).await {
        Response::Task { task, .. } => task.members[2].id.clone(),
        other => panic!("expected Task, got {other:?}"),
    };
    let note = session_note(root, &sock, &backend, "resume").await;
    assert!(
        note.contains(&format!("\n- docs ({docs}): repo docs")),
        "{note}"
    );
    // Its agent started before docs joined: it was not given docs' worktree.
    assert!(
        note.contains("\nYou can read the worktrees of frontend, not edit them.\n"),
        "{note}"
    );

    let out = ok({
        let mut cmd = tam_as(root, &backend);
        cmd.arg("peers");
        cmd
    })
    .await;
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[0],
        format!("You are backend ({backend}) in task feat."),
        "{out}"
    );
    assert_eq!(
        lines[1],
        format!(
            "Brief of task feat: {}",
            task_dir.join("brief.md").display()
        ),
        "{out}"
    );
    assert_eq!(
        lines[2],
        format!("Scratch: {}", task_dir.join("scratch").display()),
        "{out}"
    );
    assert!(lines[3].starts_with("NAME "), "{out}");
    assert!(
        lines[4].starts_with(&format!("frontend  {frontend}  idle ")),
        "{out}"
    );
    assert!(
        lines[5].starts_with(&format!("docs      {docs}  idle ")),
        "{out}"
    );
    assert_eq!(lines[6], "To message one: tam msg NAME \"<text>\"", "{out}");

    let json = peers_json(tam_as(root, &backend), &[]).await;
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["self"]["id"], backend.as_str());
    assert_eq!(json["self"]["name"], "backend");
    assert_eq!(json["self"]["agent"], "claude");
    assert_eq!(json["peers"][0]["id"], frontend.as_str());
    assert_eq!(json["peers"][0]["label"], "feat/frontend");
    assert_eq!(json["peers"][0]["repo"], "frontend");
    assert_eq!(json["peers"][0]["dir"], frontend_dir.to_str().unwrap());
    assert_eq!(json["peers"][0]["status"], "idle");
    assert_eq!(json["peers"][0]["agent"], Value::Null);
    assert_eq!(json["brief"]["task"], "feat");
    assert_eq!(json["brief"]["reference"], Value::Null);
    assert_eq!(
        json["brief"]["file"],
        task_dir.join("brief.md").to_str().unwrap()
    );
    assert_eq!(
        json["brief"]["scratch"],
        task_dir.join("scratch").to_str().unwrap()
    );
    assert_eq!(
        addresses(&json),
        [
            ("frontend".into(), "feat".into()),
            ("docs".into(), "feat".into())
        ]
    );

    // A peer's short name reaches it.
    let mut cmd = tam_as(root, &backend);
    cmd.args(["msg", "docs", "the API is POST /items"]);
    let out = ok(cmd).await;
    assert!(out.starts_with("Sent to 'feat/docs' (msg-"), "{out}");

    // A task named like a peer keeps its name: the peer goes by its label.
    let other_dir = worktree(root, "docs", "frontend");
    let request = Request::CreateTask {
        name: "frontend".into(),
        members: vec![new_member(root, &other_dir, "frontend")],
    };
    assert!(matches!(
        send(&mut stream, &request).await,
        Response::Task { .. }
    ));
    let mut cmd = tam_as(root, &backend);
    cmd.args(["msg", "frontend", "hi"]);
    let out = ok(cmd).await;
    assert!(out.starts_with("Sent to 'frontend' (msg-"), "{out}");
    let json = peers_json(tam_as(root, &backend), &[]).await;
    assert_eq!(
        addresses(&json),
        [
            ("feat/frontend".into(), "feat".into()),
            ("docs".into(), "feat".into())
        ]
    );
    let note = session_note(root, &sock, &backend, "compact").await;
    assert!(
        note.contains(&format!("\n- feat/frontend ({frontend}):")),
        "{note}"
    );
    assert!(note.contains("tam msg feat/frontend \"<text>\""), "{note}");
    let json = peers_json(tam_as(root, &backend), &["--all"]).await;
    assert_eq!(
        addresses(&json),
        [
            ("feat/frontend".into(), "feat".into()),
            ("docs".into(), "feat".into()),
            ("frontend".into(), "frontend".into())
        ]
    );

    // An agent without hooks has its member id in its environment, and tam
    // peers works from there.
    let out_file = root.join("peers.json");
    let script = format!(
        "export XDG_RUNTIME_DIR='{rt}' XDG_DATA_HOME='{data}' XDG_CONFIG_HOME='{config}' \
         HOME='{home}'\n'{tam}' peers --json > '{out}.tmp' 2>&1; mv '{out}.tmp' '{out}'\n\
         exec sleep 3600\n",
        rt = root.join("rt").display(),
        data = root.join("data").display(),
        config = root.join("config").display(),
        home = root.join("home").display(),
        tam = env!("CARGO_BIN_EXE_tam"),
        out = out_file.display(),
    );
    let resp = send(
        &mut stream,
        &Request::Spawn {
            provider: "sh".into(),
            dir: PathBuf::new(),
            id: None,
            args: vec!["-c".into(), script],
            resume_session: None,
            prompt: None,
            member: Some(docs.clone()),
        },
    )
    .await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    let mut written = String::new();
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(&out_file) {
            written = s;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let json: Value = serde_json::from_str(&written).unwrap_or_else(|e| panic!("{e}: {written}"));
    assert_eq!(json["self"]["id"], docs.as_str());
    assert_eq!(json["self"]["agent"], "sh");
    assert_eq!(
        addresses(&json),
        [
            ("backend".into(), "feat".into()),
            ("feat/frontend".into(), "feat".into())
        ]
    );

    // The user, in a member's worktree: labels, which work from anywhere.
    let mut cmd = tam(root);
    cmd.current_dir(&frontend_dir);
    let json = peers_json(cmd, &[]).await;
    assert_eq!(json["self"]["id"], frontend.as_str());
    assert_eq!(
        addresses(&json),
        [
            ("feat/backend".into(), "feat".into()),
            ("feat/docs".into(), "feat".into())
        ]
    );
    // Outside any task, only --all.
    let mut cmd = tam(root);
    cmd.current_dir(root).arg("peers");
    let out = output(cmd).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not in a task"), "{}", stderr(&out));
    let mut cmd = tam(root);
    cmd.current_dir(root);
    let json = peers_json(cmd, &["--all"]).await;
    assert_eq!(json["self"], Value::Null);
    assert_eq!(json["peers"].as_array().unwrap().len(), 4);

    for id in [backend, docs] {
        let _ = send(&mut stream, &Request::Kill { id }).await;
    }
}
