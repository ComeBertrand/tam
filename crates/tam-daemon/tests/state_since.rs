//! When an agent entered its state (`AgentInfo::state_since`), for a state a
//! hook sets: the event pushed while the hook is handled carries it already.
//!
//! Kept to a single test: it sets process environment (PATH, HOME,
//! XDG_CONFIG_HOME), which must not race other tests in this binary.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

async fn agent(stream: &mut UnixStream, id: &str) -> AgentInfo {
    match send(stream, &Request::List).await {
        Response::Agents { agents } => agents.into_iter().find(|a| a.id == id).unwrap(),
        other => panic!("expected Agents, got {other:?}"),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The next `AgentUpdated` on a subscribed connection.
async fn next_update(events: &mut UnixStream) -> AgentInfo {
    let mut reader = BufReader::new(events);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            if let Ok(ServerMessage::Event(Event::AgentUpdated { info, .. })) =
                serde_json::from_str::<ServerMessage>(line.trim())
            {
                return info;
            }
        }
    })
    .await
    .expect("AgentUpdated")
}

fn fake_claude(bin: &Path) {
    std::fs::create_dir_all(bin).unwrap();
    let fake = bin.join("claude");
    std::fs::write(&fake, "#!/bin/sh\nexec sleep 3600\n").unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_since_is_when_the_hook_changed_the_state() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // No real notify command, no real Claude files.
    std::env::set_var("HOME", root.join("home"));
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    let bin = root.join("bin");
    fake_claude(&bin);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    std::env::set_var("PATH", path);

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

    let spawned_at = now();
    let resp = send(
        &mut stream,
        &Request::Spawn {
            provider: "claude".into(),
            dir: root.to_path_buf(),
            id: Some("cl".into()),
            args: vec![],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    let info = agent(&mut stream, "cl").await;
    assert_eq!(info.state, AgentState::Working);
    let since = info.state_since.expect("state_since");
    assert!(since >= spawned_at && since <= now(), "{since}");

    let mut events = UnixStream::connect(&sock).await.unwrap();
    assert!(matches!(
        send(&mut events, &Request::Subscribe).await,
        Response::Ok
    ));

    // A later second: a stale timestamp would be the spawn's.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let asked_at = now();
    let resp = send(
        &mut stream,
        &Request::HookEvent {
            agent_id: "cl".into(),
            event: "PermissionRequest".into(),
            payload: Some(serde_json::json!({
                "hook_event_name": "PermissionRequest",
                "tool_name": "Bash",
                "tool_input": {"command": "cargo test"},
            })),
        },
    )
    .await;
    assert!(matches!(resp, Response::Ok), "{resp:?}");

    // Pushed before the daemon's state check reports the change.
    let updated = next_update(&mut events).await;
    assert_eq!(updated.state, AgentState::Blocked);
    assert!(updated.state_since.unwrap() >= asked_at, "{updated:?}");

    // And it stays the hook's time once reported.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let info = agent(&mut stream, "cl").await;
    assert_eq!(info.state, AgentState::Blocked);
    let blocked_since = info.state_since.unwrap();
    assert!(
        (asked_at..=asked_at + 2).contains(&blocked_since),
        "{blocked_since} vs {asked_at}"
    );
}
