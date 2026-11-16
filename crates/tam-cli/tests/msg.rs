//! End to end: `tam msg`, `tam history` and `tam inbox-wait`, run as the real
//! binary against a daemon.
//!
//! The daemon listens where the CLI looks for it (`$XDG_RUNTIME_DIR/tam/sock`).
//! Every command runs with temporary XDG directories and HOME, and without
//! the caller's `TAM_AGENT_ID`/`TAM_SOCKET`: it can never reach a real daemon.

use std::io::Write;
use std::path::{Path, PathBuf};
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

async fn member(stream: &mut UnixStream, root: &Path, name: &str) -> String {
    let dir = root.join("work").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let request = Request::CreateTask {
        name: name.into(),
        members: vec![tam_proto::NewMember {
            repo_root: PathBuf::from("/src").join(name),
            branch: name.into(),
            dir,
        }],
    };
    match send(stream, &request).await {
        Response::Task { task, .. } => task.members[0].id.clone(),
        other => panic!("expected Task, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_through_the_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
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

    let api = member(&mut stream, root, "api").await;
    let web = member(&mut stream, root, "web").await;
    // A busy agent (never idle: no typing into its terminal).
    let resp = send(
        &mut stream,
        &Request::Spawn {
            provider: "sh".into(),
            dir: PathBuf::new(),
            id: None,
            args: vec!["-c".into(), "while :; do echo .; sleep 1; done".into()],
            resume_session: None,
            prompt: None,
            member: Some(web.clone()),
        },
    )
    .await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");

    // web's inbox hook waits in the background, as Claude runs it.
    let mut hook = tam(root);
    hook.arg("inbox-wait")
        .env("TAM_AGENT_ID", &web)
        .env("TAM_SOCKET", &sock)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped());
    let mut hook = hook.spawn().unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(br#"{"hook_event_name":"Stop"}"#)
        .unwrap();
    let hook = tokio::task::spawn_blocking(move || hook.wait_with_output().unwrap());
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Sent from api's agent: from api. Handed to the hook, but no prompt
    // shows it to a model here: not delivered yet.
    let mut cmd = tam(root);
    cmd.args(["msg", "web", "which", "port?"])
        .env("TAM_AGENT_ID", &api);
    let out = output(cmd).await;
    assert!(out.status.success(), "{}", stderr(&out));
    let line = stdout(&out);
    assert!(
        line.starts_with("Sent to 'web' (msg-") && line.contains("not delivered yet"),
        "{line}"
    );
    let id = line.split(['(', ')']).nth(1).unwrap().to_string();

    // The hook prints it for the model and exits 2.
    let hooked = tokio::time::timeout(Duration::from_secs(10), hook)
        .await
        .expect("inbox hook done")
        .unwrap();
    assert_eq!(hooked.status.code(), Some(2), "{}", stderr(&hooked));
    assert_eq!(
        stderr(&hooked),
        format!("Message {id} from api:\nwhich port?\nTo reply, run: tam msg api \"<reply>\"\n")
    );

    // From the user, text on stdin; nobody answers in time.
    let mut cmd = tam(root);
    cmd.args(["msg", "web", "--wait", "--timeout", "1"])
        .stdin(Stdio::piped());
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"status?\n").unwrap();
    let out = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no reply to msg-"),
        "{}",
        stderr(&out)
    );
    assert!(stdout(&out).is_empty());

    // Refused: unknown recipient.
    let mut cmd = tam(root);
    cmd.args(["msg", "nope", "hi"]);
    let out = output(cmd).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not found"), "{}", stderr(&out));

    // Both show in web's history.
    let mut cmd = tam(root);
    cmd.args(["history", "web"]);
    let out = output(cmd).await;
    assert!(out.status.success(), "{}", stderr(&out));
    let history = stdout(&out);
    assert!(
        history.contains(&format!("api → web  {id}  queued  which port?")),
        "{history}"
    );
    assert!(history.contains("user → web  msg-"), "{history}");
    assert!(history.contains("web  sh started"), "{history}");
    let mut cmd = tam(root);
    cmd.args(["history", "api", "--json"]);
    let out = output(cmd).await;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["messages"].as_array().unwrap().len(), 1);
    assert_eq!(json["messages"][0]["from_label"], "api");

    // The inbox hook exits 0 quietly with nothing to wait for.
    let missing = root.join("missing.sock");
    for (agent, socket) in [
        (None, &sock),
        (Some("nobody"), &sock),
        (Some(web.as_str()), &missing),
    ] {
        let mut cmd = tam(root);
        cmd.arg("inbox-wait").env("TAM_SOCKET", socket);
        if let Some(agent) = agent {
            cmd.env("TAM_AGENT_ID", agent);
        }
        let out = output(cmd).await;
        assert_eq!(out.status.code(), Some(0), "{agent:?}: {}", stderr(&out));
        assert!(stderr(&out).is_empty());
    }
    assert!(!missing.exists());

    let _ = send(&mut stream, &Request::Kill { id: web }).await;
}
