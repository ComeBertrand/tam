//! End to end: after a merge, the real `tam hook-notify` of a (fake) Claude
//! agent prints what changed as the hook's `additionalContext`, once, at
//! its next tool call or prompt.
//!
//! Kept to a single test: it sets process environment (PATH, HOME,
//! XDG_CONFIG_HOME), which must not race other tests in this binary.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

/// Run the real `tam hook-notify` as an injected hook would; its stdout.
async fn hook_notify(sock: &Path, agent: &str, payload: serde_json::Value) -> String {
    let (sock, agent) = (sock.to_path_buf(), agent.to_string());
    tokio::task::spawn_blocking(move || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tam"))
            .arg("hook-notify")
            .env("TAM_AGENT_ID", agent)
            .env("TAM_SOCKET", sock)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    })
    .await
    .unwrap()
}

/// The context a hook's output adds, checking it names its event.
fn context(out: &str, event: &str) -> String {
    let out: serde_json::Value =
        serde_json::from_str(out).unwrap_or_else(|e| panic!("not JSON ({e}): {out}"));
    let specific = &out["hookSpecificOutput"];
    assert_eq!(specific["hookEventName"], event, "{out}");
    specific["additionalContext"].as_str().unwrap().to_string()
}

/// Create a single-member task and start a (fake) Claude agent in it;
/// returns the member id.
async fn task_with_agent(stream: &mut UnixStream, name: &str, root: &Path) -> String {
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
    let member = match send(stream, &request).await {
        Response::Task { task, .. } => task.members[0].id.clone(),
        other => panic!("expected Task, got {other:?}"),
    };
    let request = Request::Spawn {
        provider: "claude".into(),
        dir: PathBuf::new(),
        id: None,
        args: vec![],
        resume_session: None,
        prompt: None,
        member: Some(member.clone()),
    };
    let resp = send(stream, &request).await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    member
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_notify_prints_the_change_at_the_next_step() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // Isolate from the user's environment: no real notify command, no real
    // Claude files.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::env::set_var("HOME", root.join("home"));
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    std::env::set_var("PATH", path);
    let fake = bin.join("claude");
    std::fs::write(&fake, "#!/bin/sh\nexec sleep 3600\n").unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let sock = root.join("tam.sock");
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
    let back = task_with_agent(&mut stream, "back", root).await;
    let front = task_with_agent(&mut stream, "front", root).await;

    // Nothing to tell yet.
    let tool = serde_json::json!({
        "hook_event_name": "PostToolUse",
        "tool_name": "Read",
        "tool_response": {"content": "…"},
    });
    assert_eq!(hook_notify(&sock, &back, tool.clone()).await, "");

    let merge = Request::MergeTasks {
        into: "back".into(),
        from: vec!["front".into()],
    };
    assert!(matches!(
        send(&mut stream, &merge).await,
        Response::Moved { .. }
    ));

    // At the next tool call, once.
    let out = hook_notify(&sock, &back, tool.clone()).await;
    let note = context(&out, "PostToolUse");
    assert!(
        note.starts_with(
            "[tam] Task front was merged into this task (back): its members joined it.\n"
        ),
        "{note}"
    );
    assert!(note.contains(&format!("- front ({front})")), "{note}");
    assert_eq!(hook_notify(&sock, &back, tool).await, "");

    // At the next prompt.
    let prompt = serde_json::json!({"hook_event_name": "UserPromptSubmit", "prompt": "go on"});
    let out = hook_notify(&sock, &front, prompt).await;
    let note = context(&out, "UserPromptSubmit");
    assert!(
        note.starts_with("[tam] Task front was merged into task back: this member, front until now, is back/front."),
        "{note}"
    );
    assert!(note.contains("in task back"), "{note}");
}
