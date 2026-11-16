use std::path::PathBuf;

use tam_daemon::daemon::Daemon;
use tam_proto::{Request, Response, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Send a request and read the response over a Unix socket.
/// Skips any pushed events and returns only the response.
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
            ServerMessage::Event(_) => continue, // skip pushed events
        }
    }
}

/// Start a daemon on a temp socket and return the socket path.
/// The daemon runs in a background task and is dropped when the test ends.
async fn start_daemon(dir: &std::path::Path) -> PathBuf {
    let sock = dir.join("test.sock");
    let daemon = Daemon::new(sock.clone(), dir.join("ledger.jsonl"));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    // Wait for socket to appear
    for _ in 0..40 {
        if UnixStream::connect(&sock).await.is_ok() {
            return sock;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("daemon did not start");
}

#[tokio::test]
async fn list_empty() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => assert!(agents.is_empty()),
        other => panic!("expected Agents, got {:?}", other),
    }
}

#[tokio::test]
async fn spawn_and_list() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let resp = send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("test-agent".into()),
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;
    match &resp {
        Response::Spawned { id } => assert_eq!(id, "test-agent"),
        other => panic!("expected Spawned, got {:?}", other),
    }

    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => {
            assert_eq!(agents.len(), 1);
            assert_eq!(agents[0].id, "test-agent");
            assert_eq!(agents[0].provider, "sleep");
        }
        other => panic!("expected Agents, got {:?}", other),
    }
}

#[tokio::test]
async fn spawn_duplicate_id() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let req = Request::Spawn {
        provider: "sleep".into(),
        dir: PathBuf::from("/tmp"),
        id: Some("dupe".into()),
        args: vec!["3600".into()],
        resume_session: None,
        prompt: None,
        task: None,
    };

    let resp = send(&mut stream, &req).await;
    assert!(matches!(resp, Response::Spawned { .. }));

    let resp = send(&mut stream, &req).await;
    match resp {
        Response::Error { message } => assert!(message.contains("already exists")),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn spawn_invalid_directory() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let resp = send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/nonexistent/path/that/does/not/exist"),
            id: Some("bad-dir".into()),
            args: vec!["1".into()],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;
    match resp {
        Response::Error { message } => assert!(message.contains("directory"), "{}", message),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn kill_agent() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("to-kill".into()),
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;

    let resp = send(
        &mut stream,
        &Request::Kill {
            id: "to-kill".into(),
        },
    )
    .await;
    assert!(matches!(resp, Response::Ok));

    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => assert!(agents.is_empty()),
        other => panic!("expected empty Agents, got {:?}", other),
    }
}

#[tokio::test]
async fn kill_nonexistent() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let resp = send(&mut stream, &Request::Kill { id: "nope".into() }).await;
    match resp {
        Response::Error { message } => assert!(message.contains("not found")),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn auto_generated_ids() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let resp = send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: None,
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;
    match &resp {
        Response::Spawned { id } => assert_eq!(id, "agent-1"),
        other => panic!("expected Spawned, got {:?}", other),
    }

    let resp = send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: None,
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;
    match &resp {
        Response::Spawned { id } => assert_eq!(id, "agent-2"),
        other => panic!("expected Spawned, got {:?}", other),
    }
}

#[tokio::test]
async fn malformed_json() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    stream.write_all(b"this is not json\n").await.unwrap();

    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: Response = serde_json::from_str(line.trim()).unwrap();
    match resp {
        Response::Error { message } => assert!(message.contains("invalid request")),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn shutdown_kills_all() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("a".into()),
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;
    send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("b".into()),
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;

    let resp = send(&mut stream, &Request::Shutdown).await;
    assert!(matches!(resp, Response::Ok));
}

#[tokio::test]
async fn exited_process_is_cleaned_up() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    // 'true' exits immediately with code 0
    send(
        &mut stream,
        &Request::Spawn {
            provider: "true".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("quick".into()),
            args: vec![],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;

    // Give it a moment to exit
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Exited agents are removed — exit is an event, not a state
    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => assert!(agents.is_empty()),
        other => panic!("expected empty Agents, got {:?}", other),
    }
}

#[tokio::test]
async fn failed_process_is_cleaned_up() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    // 'false' exits immediately with code 1
    send(
        &mut stream,
        &Request::Spawn {
            provider: "false".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("fail".into()),
            args: vec![],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Exited agents are removed regardless of exit code
    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => assert!(agents.is_empty()),
        other => panic!("expected empty Agents, got {:?}", other),
    }
}

#[tokio::test]
async fn attach_nonexistent_agent() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let resp = send(
        &mut stream,
        &Request::Attach {
            id: "nope".into(),
            cols: 80,
            rows: 24,
        },
    )
    .await;
    match resp {
        Response::Error { message } => assert!(message.contains("not found")),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn attach_receives_scrollback() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    // Spawn 'echo hello' — it will produce output then exit
    send(
        &mut stream,
        &Request::Spawn {
            provider: "bash".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("echo-test".into()),
            args: vec!["-c".into(), "echo hello".into()],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;

    // Wait for output to be captured in scrollback
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Attach on a fresh connection (the current one will switch to raw mode)
    let mut attach_stream = UnixStream::connect(&sock).await.unwrap();
    let mut json = serde_json::to_string(&Request::Attach {
        id: "echo-test".into(),
        cols: 80,
        rows: 24,
    })
    .unwrap();
    json.push('\n');
    attach_stream.write_all(json.as_bytes()).await.unwrap();

    // Read the JSON response line
    let mut reader = BufReader::new(&mut attach_stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: Response = serde_json::from_str(line.trim()).unwrap();
    assert!(matches!(resp, Response::Attached));

    // Read scrollback — should contain "hello"
    let mut buf = [0u8; 4096];
    let n = tokio::time::timeout(std::time::Duration::from_secs(2), reader.read(&mut buf))
        .await
        .expect("timed out reading scrollback")
        .expect("read failed");

    let output = String::from_utf8_lossy(&buf[..n]);
    assert!(output.contains("hello"), "scrollback was: {:?}", output);
}

#[tokio::test]
async fn attach_relays_input_and_output() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    // Spawn 'cat' — it echoes stdin to stdout
    send(
        &mut stream,
        &Request::Spawn {
            provider: "cat".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("cat-test".into()),
            args: vec![],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Attach on a fresh connection
    let mut attach_stream = UnixStream::connect(&sock).await.unwrap();
    let mut json = serde_json::to_string(&Request::Attach {
        id: "cat-test".into(),
        cols: 80,
        rows: 24,
    })
    .unwrap();
    json.push('\n');
    attach_stream.write_all(json.as_bytes()).await.unwrap();

    // Read attached response
    let mut reader = BufReader::new(&mut attach_stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: Response = serde_json::from_str(line.trim()).unwrap();
    assert!(matches!(resp, Response::Attached));

    // Send some input (write directly to the underlying stream)
    let stream_ref = reader.get_mut();
    stream_ref.write_all(b"ping\n").await.unwrap();

    // Read back the echoed output (PTY echo + cat echo)
    let mut buf = [0u8; 4096];
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(500), reader.read(&mut buf))
            .await
        {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                collected.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&collected);
                if text.contains("ping") {
                    break;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => break, // timeout
        }
    }

    let output = String::from_utf8_lossy(&collected);
    assert!(
        output.contains("ping"),
        "expected 'ping' in output, got: {:?}",
        output
    );
}

#[tokio::test]
async fn generic_agent_transitions_to_idle() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    // Spawn a process that outputs once then sleeps (goes quiet)
    send(
        &mut stream,
        &Request::Spawn {
            provider: "bash".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("idle-test".into()),
            args: vec!["-c".into(), "echo hello; sleep 3600".into()],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;

    // Immediately after output, should be working
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let resp = send(&mut stream, &Request::List).await;
    match &resp {
        Response::Agents { agents } => {
            assert_eq!(agents.len(), 1);
            assert_eq!(agents[0].state, tam_proto::AgentState::Working);
        }
        other => panic!("expected Agents, got {:?}", other),
    }

    // After the generic provider's idle timeout (5s), should be idle
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => {
            assert_eq!(agents.len(), 1);
            assert_eq!(agents[0].state, tam_proto::AgentState::Idle);
        }
        other => panic!("expected Agents, got {:?}", other),
    }
}

#[tokio::test]
async fn events_pushed_on_agent_exit() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();
    assert!(matches!(
        send(&mut stream, &Request::Subscribe).await,
        Response::Ok
    ));

    // Spawn a process that exits quickly
    send(
        &mut stream,
        &Request::Spawn {
            provider: "true".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("ev-test".into()),
            args: vec![],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;

    // Read lines until we get an AgentExited event (monitor runs every 1s)
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut got_exit = false;

    while tokio::time::Instant::now() < deadline {
        line.clear();
        match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            reader.read_line(&mut line),
        )
        .await
        {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                if let Ok(ServerMessage::Event(tam_proto::Event::AgentExited { id, .. })) =
                    serde_json::from_str::<ServerMessage>(line.trim())
                {
                    if id == "ev-test" {
                        got_exit = true;
                        break;
                    }
                }
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    assert!(got_exit, "expected AgentExited event for ev-test");
}

#[tokio::test]
async fn hook_event_updates_agent_state() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    // Spawn a sleep process (generic provider)
    send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("hook-test".into()),
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;

    // Generic provider doesn't handle hooks — should return error
    let resp = send(
        &mut stream,
        &Request::HookEvent {
            agent_id: "hook-test".into(),
            event: "Stop".into(),
            payload: None,
        },
    )
    .await;
    match resp {
        Response::Error { message } => assert!(message.contains("unknown hook event")),
        other => panic!("expected Error for generic provider hook, got {:?}", other),
    }

    // Hook for nonexistent agent — should return error
    let resp = send(
        &mut stream,
        &Request::HookEvent {
            agent_id: "nonexistent".into(),
            event: "Stop".into(),
            payload: None,
        },
    )
    .await;
    match resp {
        Response::Error { message } => assert!(message.contains("not found")),
        other => panic!("expected Error for missing agent, got {:?}", other),
    }
}

#[tokio::test]
async fn spawn_emits_event() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;

    // Open two connections: one to spawn, one to listen for events
    let mut cmd_stream = UnixStream::connect(&sock).await.unwrap();
    let mut evt_stream = UnixStream::connect(&sock).await.unwrap();
    assert!(matches!(
        send(&mut evt_stream, &Request::Subscribe).await,
        Response::Ok
    ));

    // Spawn an agent via the command connection
    send(
        &mut cmd_stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("spawn-evt".into()),
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;

    // The event connection should receive an AgentSpawned event
    let mut reader = BufReader::new(&mut evt_stream);
    let mut line = String::new();
    let mut got_spawn = false;

    match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        reader.read_line(&mut line),
    )
    .await
    {
        Ok(Ok(_)) => {
            if let Ok(ServerMessage::Event(tam_proto::Event::AgentSpawned { id, info })) =
                serde_json::from_str::<ServerMessage>(line.trim())
            {
                assert_eq!(id, "spawn-evt");
                assert_eq!(info.id, "spawn-evt");
                assert_eq!(info.provider, "sleep");
                got_spawn = true;
            }
        }
        _ => {}
    }

    assert!(got_spawn, "expected AgentSpawned event for spawn-evt");
}

#[tokio::test]
async fn kill_emits_event() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;

    // Open two connections: one to command, one to listen for events
    let mut cmd_stream = UnixStream::connect(&sock).await.unwrap();
    let mut evt_stream = UnixStream::connect(&sock).await.unwrap();
    assert!(matches!(
        send(&mut evt_stream, &Request::Subscribe).await,
        Response::Ok
    ));

    // Spawn then kill
    send(
        &mut cmd_stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("kill-evt".into()),
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;

    // Drain the spawn event from the event stream
    let mut reader = BufReader::new(&mut evt_stream);
    let mut line = String::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        reader.read_line(&mut line),
    )
    .await;

    // Now kill
    line.clear();
    send(
        &mut cmd_stream,
        &Request::Kill {
            id: "kill-evt".into(),
        },
    )
    .await;

    // Should get an AgentExited event
    line.clear();
    let mut got_exit = false;

    match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        reader.read_line(&mut line),
    )
    .await
    {
        Ok(Ok(_)) => {
            if let Ok(ServerMessage::Event(tam_proto::Event::AgentExited { id, .. })) =
                serde_json::from_str::<ServerMessage>(line.trim())
            {
                assert_eq!(id, "kill-evt");
                got_exit = true;
            }
        }
        _ => {}
    }

    assert!(got_exit, "expected AgentExited event for kill-evt");
}

#[tokio::test]
async fn resize_nonexistent_agent() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let resp = send(
        &mut stream,
        &Request::Resize {
            id: "nope".into(),
            cols: 80,
            rows: 24,
        },
    )
    .await;
    match resp {
        Response::Error { message } => assert!(message.contains("not found")),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn resize_updates_pty_and_signals_agent() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    // The agent prints its PTY size whenever it receives SIGWINCH.
    send(
        &mut stream,
        &Request::Spawn {
            provider: "bash".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("resize-test".into()),
            args: vec![
                "-c".into(),
                "trap 'echo \"size=$(stty size)\"' WINCH; while :; do sleep 0.05; done".into(),
            ],
            resume_session: None,
            prompt: None,
            task: None,
        },
    )
    .await;

    // Let bash install the trap before signalling it.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let resp = send(
        &mut stream,
        &Request::Resize {
            id: "resize-test".into(),
            cols: 123,
            rows: 45,
        },
    )
    .await;
    assert!(matches!(resp, Response::Ok), "got {:?}", resp);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut last = String::new();
    while tokio::time::Instant::now() < deadline {
        if let Response::Scrollback { data } = send(
            &mut stream,
            &Request::Scrollback {
                id: "resize-test".into(),
            },
        )
        .await
        {
            if data.contains("size=45 123") {
                return;
            }
            last = data;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("agent never saw the new size; scrollback: {:?}", last);
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

/// A connection that keeps its read buffer across requests and queues pushed
/// events until asked for them.
struct Conn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    events: std::collections::VecDeque<tam_proto::Event>,
}

impl Conn {
    async fn connect(sock: &std::path::Path) -> Self {
        let (reader, writer) = UnixStream::connect(sock).await.unwrap().into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
            events: Default::default(),
        }
    }

    async fn subscribed(sock: &std::path::Path) -> Self {
        let mut conn = Self::connect(sock).await;
        assert!(matches!(
            conn.request(&Request::Subscribe).await,
            Response::Ok
        ));
        conn
    }

    async fn read(&mut self) -> Option<ServerMessage> {
        let mut line = String::new();
        match self.reader.read_line(&mut line).await {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(serde_json::from_str(line.trim()).unwrap()),
        }
    }

    async fn request(&mut self, request: &Request) -> Response {
        let mut json = serde_json::to_string(request).unwrap();
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await.unwrap();
        loop {
            match self.read().await.expect("daemon closed the connection") {
                ServerMessage::Response(resp) => return resp,
                ServerMessage::Event(event) => self.events.push_back(event),
            }
        }
    }

    /// Wait for the first queued or incoming event matching `pred`.
    async fn wait_event(
        &mut self,
        secs: u64,
        pred: impl Fn(&tam_proto::Event) -> bool,
    ) -> Option<tam_proto::Event> {
        while let Some(event) = self.events.pop_front() {
            if pred(&event) {
                return Some(event);
            }
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            let msg = tokio::time::timeout_at(deadline, self.read())
                .await
                .ok()??;
            if let ServerMessage::Event(event) = msg {
                if pred(&event) {
                    return Some(event);
                }
            }
        }
    }

    async fn create_task(&mut self, name: &str, dir: &std::path::Path) -> tam_proto::TaskInfo {
        match self
            .request(&Request::CreateTask {
                name: name.into(),
                dir: dir.into(),
                owned: true,
            })
            .await
        {
            Response::Task { task, runs } => {
                assert!(runs.is_empty());
                task
            }
            other => panic!("expected Task, got {other:?}"),
        }
    }

    async fn runs(&mut self, name: &str) -> Vec<tam_proto::RunInfo> {
        match self.request(&Request::GetTask { name: name.into() }).await {
            Response::Task { runs, .. } => runs,
            other => panic!("expected Task, got {other:?}"),
        }
    }

    async fn spawn_for_task(&mut self, task: &str, provider: &str, args: &[&str]) -> Response {
        self.request(&Request::Spawn {
            provider: provider.into(),
            dir: PathBuf::from("/tmp"),
            id: Some(task.into()),
            args: args.iter().map(|a| a.to_string()).collect(),
            resume_session: Some("sess-1".into()),
            prompt: None,
            task: Some(task.into()),
        })
        .await
    }
}

#[tokio::test]
async fn task_create_list_get_drop() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;

    let task = conn.create_task("feat", dir.path()).await;
    assert_eq!(task.name, "feat");
    assert!(task.owned);
    assert_eq!(task.run_count, 0);

    // Same name, or same directory: refused.
    let resp = conn
        .request(&Request::CreateTask {
            name: "feat".into(),
            dir: PathBuf::from("/elsewhere"),
            owned: false,
        })
        .await;
    assert!(matches!(resp, Response::Error { message } if message.contains("already exists")));

    match conn.request(&Request::ListTasks).await {
        Response::Tasks { tasks } => assert_eq!(tasks, vec![task.clone()]),
        other => panic!("expected Tasks, got {other:?}"),
    }
    assert!(conn.runs("feat").await.is_empty());

    let resp = conn
        .request(&Request::DropTask {
            name: "feat".into(),
            worktree_deleted: true,
        })
        .await;
    assert!(matches!(resp, Response::Ok), "{resp:?}");
    match conn.request(&Request::ListTasks).await {
        Response::Tasks { tasks } => assert!(tasks.is_empty()),
        other => panic!("expected Tasks, got {other:?}"),
    }
    let resp = conn
        .request(&Request::GetTask {
            name: "feat".into(),
        })
        .await;
    assert!(matches!(resp, Response::Error { .. }));

    // Unsubscribed connections get no pushed events.
    assert!(conn.events.is_empty(), "{:?}", conn.events);
}

#[tokio::test]
async fn task_events_reach_subscribers() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut watcher = Conn::subscribed(&sock).await;
    let mut cmd = Conn::connect(&sock).await;

    let task = cmd.create_task("feat", dir.path()).await;
    let event = watcher
        .wait_event(3, |e| matches!(e, tam_proto::Event::TaskCreated { .. }))
        .await;
    assert!(
        matches!(&event, Some(tam_proto::Event::TaskCreated { task: t }) if *t == task),
        "{event:?}"
    );

    cmd.request(&Request::DropTask {
        name: "feat".into(),
        worktree_deleted: false,
    })
    .await;
    let event = watcher
        .wait_event(3, |e| matches!(e, tam_proto::Event::TaskDropped { .. }))
        .await;
    assert!(
        matches!(&event, Some(tam_proto::Event::TaskDropped { name }) if name == "feat"),
        "{event:?}"
    );
}

#[tokio::test]
async fn agent_exiting_on_its_own_records_run_end() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut watcher = Conn::subscribed(&sock).await;
    let mut cmd = Conn::connect(&sock).await;
    cmd.create_task("feat", dir.path()).await;

    let resp = cmd.spawn_for_task("feat", "bash", &["-c", "exit 3"]).await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");

    // Run started: count goes up, the spawned agent carries its task.
    let started = watcher
        .wait_event(3, |e| matches!(e, tam_proto::Event::TaskUpdated { .. }))
        .await;
    assert!(
        matches!(&started, Some(tam_proto::Event::TaskUpdated { task }) if task.run_count == 1),
        "{started:?}"
    );

    // The process exits by itself; the monitor reaps it and records the end.
    let exited = watcher
        .wait_event(5, |e| matches!(e, tam_proto::Event::AgentExited { .. }))
        .await;
    assert!(
        matches!(
            exited,
            Some(tam_proto::Event::AgentExited { exit_code: 3, .. })
        ),
        "{exited:?}"
    );
    watcher
        .wait_event(3, |e| matches!(e, tam_proto::Event::TaskUpdated { .. }))
        .await
        .expect("TaskUpdated after exit");

    let runs = cmd.runs("feat").await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].provider, "bash");
    assert_eq!(runs[0].session_id.as_deref(), Some("sess-1"));
    assert_eq!(runs[0].exit_code, Some(3));
    assert!(runs[0].ended_at.is_some());
}

#[tokio::test]
async fn kill_and_drop_record_run_end() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut cmd = Conn::connect(&sock).await;
    cmd.create_task("feat", dir.path()).await;

    let resp = cmd.spawn_for_task("feat", "sleep", &["3600"]).await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");

    // One agent per task.
    let resp = cmd
        .request(&Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("other-id".into()),
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            task: Some("feat".into()),
        })
        .await;
    assert!(
        matches!(&resp, Response::Error { message } if message.contains("already has a running agent")),
        "{resp:?}"
    );

    match cmd.request(&Request::List).await {
        Response::Agents { agents } => {
            assert_eq!(agents.len(), 1);
            assert_eq!(agents[0].task.as_deref(), Some("feat"));
        }
        other => panic!("expected Agents, got {other:?}"),
    }

    cmd.request(&Request::Kill { id: "feat".into() }).await;
    let runs = cmd.runs("feat").await;
    assert_eq!(runs[0].exit_code, Some(-1));

    // A second run, ended by dropping the task.
    let resp = cmd.spawn_for_task("feat", "sleep", &["3600"]).await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    let resp = cmd
        .request(&Request::DropTask {
            name: "feat".into(),
            worktree_deleted: false,
        })
        .await;
    assert!(matches!(resp, Response::Ok), "{resp:?}");
    match cmd.request(&Request::List).await {
        Response::Agents { agents } => assert!(agents.is_empty(), "{agents:?}"),
        other => panic!("expected Agents, got {other:?}"),
    }

    let ledger = std::fs::read_to_string(dir.path().join("ledger.jsonl")).unwrap();
    let ends = ledger.matches("\"agent_run_ended\"").count();
    assert_eq!(ends, 2, "{ledger}");
}

#[tokio::test]
async fn spawn_for_unknown_task_fails() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut cmd = Conn::connect(&sock).await;
    let resp = cmd.spawn_for_task("nope", "sleep", &["3600"]).await;
    assert!(
        matches!(&resp, Response::Error { message } if message.contains("not found")),
        "{resp:?}"
    );
    match cmd.request(&Request::List).await {
        Response::Agents { agents } => assert!(agents.is_empty()),
        other => panic!("expected Agents, got {other:?}"),
    }
}

#[tokio::test]
async fn tasks_survive_daemon_restart() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut cmd = Conn::connect(&sock).await;
    cmd.create_task("feat", dir.path()).await;
    cmd.request(&Request::Shutdown).await;
    drop(cmd);

    // Wait for the old daemon to release the ledger.
    let ledger_path = dir.path().join("ledger.jsonl");
    let mut released = false;
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if tam_daemon::ledger::Ledger::open(&ledger_path).is_ok() {
            released = true;
            break;
        }
    }
    assert!(released, "old daemon kept the ledger locked");

    let sock = start_daemon(dir.path()).await;
    let mut cmd = Conn::connect(&sock).await;
    match cmd.request(&Request::ListTasks).await {
        Response::Tasks { tasks } => {
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].name, "feat");
        }
        other => panic!("expected Tasks, got {other:?}"),
    }
}

#[tokio::test]
async fn second_daemon_on_same_ledger_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;

    let other_sock = dir.path().join("other.sock");
    let second = Daemon::new(other_sock.clone(), dir.path().join("ledger.jsonl"));
    let err = second.run().await.unwrap_err();
    assert!(err.to_string().contains("locked"), "{err:#}");
    assert!(!other_sock.exists());
    // The first daemon is still serving.
    assert!(UnixStream::connect(&sock).await.is_ok());
}
