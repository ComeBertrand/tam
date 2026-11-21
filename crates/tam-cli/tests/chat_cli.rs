//! A chat's agent runs `tam` as any shell does: `tam msg` from it reaches a
//! task member, whose reply comes back to the chat; `tam result` works on a
//! task. In a main checkout, the current directory names no chat: `tam
//! attach` and `tam run` without a name find nothing, and `tam peers` says
//! it is not in a task.
//!
//! The chat is made with a request (its commands come later); the real
//! `tam` binary runs against an in-process daemon, with temporary XDG
//! directories and HOME: it can never reach a real daemon.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use tam_daemon::daemon::Daemon;
use tam_proto::{Request, Response, ServerMessage};
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

/// `tam`, isolated in `root`, run in `dir`.
fn tam(root: &Path, dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tam"));
    cmd.env("XDG_RUNTIME_DIR", root.join("rt"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOME", root.join("home"))
        .env_remove("TAM_AGENT_ID")
        .env_remove("TAM_SOCKET")
        .current_dir(dir)
        .stdin(Stdio::null());
    tam_worktree::git::without_local_env(&mut cmd);
    cmd
}

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

fn git(dir: &Path, args: &[&str]) {
    let out = tam_worktree::git::command()
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chats_agent_uses_tam_as_any_shell() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
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

    let api = root.join("src").join("api");
    std::fs::create_dir_all(&api).unwrap();
    git(&api, &["init", "-q"]);
    git(&api, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let worktree = root.join("w").join("api--feat");
    std::fs::create_dir_all(&worktree).unwrap();
    let member = match send(
        &mut stream,
        &Request::CreateTask {
            name: "feat".into(),
            members: vec![tam_proto::NewMember {
                repo_root: api.clone(),
                branch: "feat".into(),
                dir: worktree,
            }],
        },
    )
    .await
    {
        Response::Task { task, .. } => task.members[0].id.clone(),
        other => panic!("expected Task, got {other:?}"),
    };
    let chat = match send(
        &mut stream,
        &Request::CreateChat {
            dir: api.clone(),
            name: Some("debug".into()),
            session: None,
        },
    )
    .await
    {
        Response::Chat { chat, .. } => chat,
        other => panic!("expected Chat, got {other:?}"),
    };
    let resp = send(
        &mut stream,
        &Request::RunAgent {
            member: chat.id.clone(),
            provider: Some("sleep".into()),
            session: Default::default(),
            args: vec!["3600".into()],
            prompt: None,
            env: Default::default(),
            wrapper: Vec::new(),
        },
    )
    .await;
    assert!(matches!(resp, Response::Running { .. }), "{resp:?}");

    // From the chat's agent: to the task member, from the chat.
    let mut cmd = tam(&root, &api);
    cmd.args(["msg", "feat", "status?"])
        .env("TAM_AGENT_ID", &chat.id);
    let out = output(cmd).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).starts_with("Sent to 'feat' (msg-"),
        "{}",
        stdout(&out)
    );
    // The member replies to the label it was given: the chat's id.
    let mut cmd = tam(&root, &root);
    cmd.args(["msg", &chat.id, "done"])
        .env("TAM_AGENT_ID", &member);
    let out = output(cmd).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).starts_with(&format!("Sent to '{}' (msg-", chat.id)),
        "{}",
        stdout(&out)
    );
    let messages = match send(
        &mut stream,
        &Request::ListMessages {
            task: Some(chat.id.clone()),
        },
    )
    .await
    {
        Response::Messages { messages } => messages,
        other => panic!("expected Messages, got {other:?}"),
    };
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].from.as_deref(), Some(chat.id.as_str()));
    assert_eq!(messages[0].to, member);
    assert_eq!(messages[1].from.as_deref(), Some(member.as_str()));
    assert_eq!(messages[1].to, chat.id);
    assert_eq!(
        messages[1].reply_to.as_deref(),
        Some(messages[0].id.as_str())
    );

    // What a task's agents did, from the chat (none responded yet).
    let mut cmd = tam(&root, &api);
    cmd.args(["result", "feat", "--json"])
        .env("TAM_AGENT_ID", &chat.id);
    let out = output(cmd).await;
    let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["task"], "feat", "{}", stderr(&out));
    assert_eq!(result["members"][0]["id"], member.as_str());
    // As from any shell.
    let mut cmd = tam(&root, &api);
    cmd.args(["result", "feat", "--json"]);
    let shell = output(cmd).await;
    assert_eq!(shell.status.code(), out.status.code());
    assert_eq!(stdout(&shell), stdout(&out));

    // The main checkout names no chat.
    for args in [&["attach"][..], &["run", "--detach"][..]] {
        let mut cmd = tam(&root, &api);
        cmd.args(args).env("TAM_AGENT_ID", &chat.id);
        let out = output(cmd).await;
        assert_eq!(out.status.code(), Some(3), "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("no task in current directory"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    let mut cmd = tam(&root, &api);
    cmd.arg("peers").env("TAM_AGENT_ID", &chat.id);
    let out = output(cmd).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not in a task"), "{}", stderr(&out));

    let _ = send(&mut stream, &Request::Kill { id: chat.id }).await;
}
