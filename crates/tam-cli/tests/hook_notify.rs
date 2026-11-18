//! End to end: a daemon spawns a (fake) Claude agent with its hooks
//! injected, and `tam hook-notify` forwards hook payloads to it.
//!
//! Kept to a single test: it sets process environment (PATH, HOME,
//! XDG_CONFIG_HOME), which must not race other tests in this binary.

use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tam_daemon::daemon::Daemon;
use tam_proto::{AgentInfo, AgentState, Event, Request, Response, ServerMessage};
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

/// Run the real `tam hook-notify` as an injected hook would.
fn hook_notify(sock: &Path, agent: &str, payload: &serde_json::Value) {
    let out = start_hook_notify(sock, agent, payload)
        .wait_with_output()
        .unwrap();
    assert!(out.status.success());
}

/// Start the real `tam hook-notify` as an injected hook would, its output
/// kept: a permission request's waits for an answer.
fn start_hook_notify(sock: &Path, agent: &str, payload: &serde_json::Value) -> Child {
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
    child
}

/// What a waiting `tam hook-notify` printed once it exited (it must, soon).
async fn printed(child: Child) -> String {
    let out = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap());
    let out = tokio::time::timeout(Duration::from_secs(10), out)
        .await
        .expect("hook-notify still waiting")
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

/// The agent's info once the request it waits on can be answered.
async fn asking(stream: &mut UnixStream, id: &str) -> AgentInfo {
    for _ in 0..100 {
        let info = agent(stream, id).await;
        if info.detail.ask.is_some() {
            return info;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no request to answer");
}

/// Session of the latest run of task `feat`.
async fn run_session(stream: &mut UnixStream) -> Option<String> {
    let request = Request::GetTask {
        name: "feat".into(),
    };
    match send(stream, &request).await {
        Response::Task { runs, .. } => runs.last().unwrap().session_id.clone(),
        other => panic!("expected Task, got {other:?}"),
    }
}

async fn agent(stream: &mut UnixStream, id: &str) -> AgentInfo {
    match send(stream, &Request::List).await {
        Response::Agents { agents } => agents.into_iter().find(|a| a.id == id).unwrap(),
        other => panic!("expected Agents, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hooks_reach_the_daemon_through_hook_notify() {
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

    // Fake claude: record its arguments (written aside then renamed, so the
    // test never reads a partial list), then wait like an idle session.
    let args_file = root.join("claude-args");
    let script = format!(
        "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{f}.tmp'\nmv '{f}.tmp' '{f}'\nexec sleep 3600\n",
        f = args_file.display()
    );
    let fake = bin.join("claude");
    std::fs::write(&fake, script).unwrap();
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

    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let resp = send(
        &mut stream,
        &Request::CreateTask {
            name: "feat".into(),
            members: vec![tam_proto::NewMember {
                repo_root: root.join("app"),
                branch: "feat".into(),
                dir: work.clone(),
            }],
        },
    )
    .await;
    let member = match resp {
        Response::Task { task, .. } => task.members[0].id.clone(),
        other => panic!("expected Task, got {other:?}"),
    };
    // The agent runs under the member id: its hooks report with it.
    let resp = send(
        &mut stream,
        &Request::Spawn {
            provider: "claude".into(),
            dir: work.clone(),
            id: None,
            args: vec![],
            resume_session: None,
            prompt: Some("go".into()),
            member: Some(member.clone()),
        },
    )
    .await;
    assert!(
        matches!(&resp, Response::Spawned { id } if *id == member),
        "{resp:?}"
    );

    // Events, to check AgentUpdated
    let mut events = UnixStream::connect(&sock).await.unwrap();
    assert!(matches!(
        send(&mut events, &Request::Subscribe).await,
        Response::Ok
    ));

    // The hooks are injected with --settings, before the prompt
    let mut args = String::new();
    for _ in 0..100 {
        args = std::fs::read_to_string(&args_file).unwrap_or_default();
        if !args.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(args[0], "--settings", "{args:?}");
    assert_eq!(args.last(), Some(&"go"));
    let settings: serde_json::Value = serde_json::from_str(args[1]).unwrap();
    let command = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(command.ends_with("' hook-notify"), "{command}");

    let transcript = root.join("session.jsonl");
    hook_notify(
        &sock,
        &member,
        &serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-1",
            "transcript_path": transcript,
            "source": "startup",
        }),
    );
    // A session nobody had a turn in yet is not the run's session: it could
    // not be resumed.
    assert_eq!(run_session(&mut stream).await, None);

    // A permission request: hook-notify waits for an answer from the
    // dashboard while Claude shows its own dialog.
    let permission = serde_json::json!({
        "hook_event_name": "PermissionRequest",
        "session_id": "sess-1",
        "tool_name": "Bash",
        "tool_input": {"command": "cargo test"},
    });
    let waiting = start_hook_notify(&sock, &member, &permission);
    let info = asking(&mut stream, &member).await;
    assert_eq!(info.state, AgentState::Blocked);
    assert_eq!(info.detail.session_id.as_deref(), Some("sess-1"));
    assert_eq!(info.detail.pending.as_ref().unwrap().summary, "cargo test");
    // A turn: the session is recorded for the task's run.
    assert_eq!(run_session(&mut stream).await.as_deref(), Some("sess-1"));

    // AgentUpdated carries the detail
    let mut reader = BufReader::new(&mut events);
    let mut line = String::new();
    let updated = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            if let Ok(ServerMessage::Event(Event::AgentUpdated { info, .. })) =
                serde_json::from_str::<ServerMessage>(line.trim())
            {
                if info.detail.pending.is_some() {
                    return info;
                }
            }
        }
    })
    .await
    .expect("AgentUpdated with the pending permission");
    assert_eq!(
        updated.detail.pending.unwrap().tool.as_deref(),
        Some("Bash")
    );

    // Allowed in the terminal: the tool runs, and the waiting hook exits
    // printing nothing (Claude has its answer).
    hook_notify(
        &sock,
        &member,
        &serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "cargo test"},
            "tool_response": {"stdout": "x".repeat(100_000)},
        }),
    );
    let info = agent(&mut stream, &member).await;
    assert_eq!(info.state, AgentState::Working);
    assert!(info.detail.pending.is_none());
    assert!(info.detail.ask.is_none());
    assert_eq!(printed(waiting).await, "");

    // Answered from the dashboard: the hook prints the decision, nothing
    // else, on one line.
    let waiting = start_hook_notify(&sock, &member, &permission);
    let ask = asking(&mut stream, &member).await.detail.ask.unwrap();
    let answer = Request::Answer {
        agent: member.clone(),
        ask: ask.id,
        answer: tam_proto::AskAnswer::Allow,
    };
    assert!(matches!(send(&mut stream, &answer).await, Response::Ok));
    let decision = serde_json::json!({"hookSpecificOutput": {
        "hookEventName": "PermissionRequest",
        "decision": {"behavior": "allow"},
    }});
    assert_eq!(printed(waiting).await, format!("{decision}\n"));
    assert_eq!(agent(&mut stream, &member).await.state, AgentState::Working);

    hook_notify(
        &sock,
        &member,
        &serde_json::json!({
            "hook_event_name": "StopFailure",
            "error": "rate_limit",
            "last_assistant_message": "API Error: Rate limit reached",
        }),
    );
    let info = agent(&mut stream, &member).await;
    assert_eq!(info.state, AgentState::Error);
    assert_eq!(info.detail.error.as_ref().unwrap().kind, "rate_limit");

    // Legacy global hooks are ignored, and so is anything without an agent
    // or a payload; hook-notify always exits 0.
    let status = Command::new(env!("CARGO_BIN_EXE_tam"))
        .args(["hook-notify", "--event", "stop"])
        .env("TAM_AGENT_ID", &member)
        .env("TAM_SOCKET", &sock)
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(agent(&mut stream, &member).await.state, AgentState::Error);
    hook_notify(
        &sock,
        "nobody",
        &serde_json::json!({"hook_event_name": "Stop"}),
    );
    // No daemon behind the socket: no daemon gets started
    let missing = root.join("missing.sock");
    hook_notify(
        &missing,
        &member,
        &serde_json::json!({"hook_event_name": "Stop"}),
    );
    assert!(!missing.exists());

    let _ = send(&mut stream, &Request::Kill { id: member }).await;
}
