//! What members' agents are doing, through a daemon: `GetOutcomes` and
//! `WaitAgents` (`tam result`, `tam wait`).
//!
//! Agents are shell commands (no hooks: their state comes from their
//! output, `idle` after 5s of silence). Claude's responses are covered end
//! to end in `tam-cli/tests/script.rs`, with a fake `claude`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tam_daemon::daemon::Daemon;
use tam_proto::{MemberOutcome, MemberState, NewMember, Request, Response, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

async fn start_daemon(dir: &Path) -> PathBuf {
    let sock = dir.join("test.sock");
    let daemon = Daemon::new(sock.clone(), dir.join("ledger.jsonl"));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    for _ in 0..40 {
        if UnixStream::connect(&sock).await.is_ok() {
            return sock;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon did not start");
}

/// One request on a connection of its own (a wait holds it).
async fn request(sock: &Path, request: &Request) -> Response {
    let stream = UnixStream::connect(sock).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut json = serde_json::to_string(request).unwrap();
    json.push('\n');
    writer.write_all(json.as_bytes()).await.unwrap();
    let mut reader = BufReader::new(reader);
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        match serde_json::from_str::<ServerMessage>(line.trim()).unwrap() {
            ServerMessage::Response(resp) => return resp,
            ServerMessage::Event(_) => continue,
        }
    }
}

/// A task with one member per name (each in its own repository); returns
/// the member ids.
async fn task(sock: &Path, root: &Path, name: &str, repos: &[&str]) -> Vec<String> {
    let members = repos
        .iter()
        .map(|repo| {
            let dir = root.join(format!("{repo}--{name}"));
            std::fs::create_dir_all(&dir).unwrap();
            NewMember {
                repo_root: PathBuf::from("/src").join(repo),
                branch: name.into(),
                dir,
            }
        })
        .collect();
    let create = Request::CreateTask {
        name: name.into(),
        members,
    };
    match request(sock, &create).await {
        Response::Task { task, .. } => task.members.into_iter().map(|m| m.id).collect(),
        other => panic!("expected Task, got {other:?}"),
    }
}

async fn spawn(sock: &Path, member: &str, script: &str) {
    let spawn = Request::Spawn {
        provider: "sh".into(),
        dir: PathBuf::new(),
        id: None,
        args: vec!["-c".into(), script.into()],
        resume_session: None,
        prompt: None,
        member: Some(member.into()),
    };
    let resp = request(sock, &spawn).await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
}

async fn outcomes(sock: &Path, members: &[&str]) -> Vec<MemberOutcome> {
    let get = Request::GetOutcomes {
        members: members.iter().map(|m| m.to_string()).collect(),
    };
    match request(sock, &get).await {
        Response::Outcomes { members, timed_out } => {
            assert!(!timed_out);
            members
        }
        other => panic!("expected Outcomes, got {other:?}"),
    }
}

/// Wait on `members`; returns the outcomes, whether it timed out, and how
/// long it took.
async fn wait(
    sock: &Path,
    members: &[&str],
    until: &[MemberState],
    timeout_secs: u64,
) -> (Vec<MemberOutcome>, bool, Duration) {
    let started = Instant::now();
    let wait = Request::WaitAgents {
        members: members.iter().map(|m| m.to_string()).collect(),
        until: until.to_vec(),
        timeout_secs: Some(timeout_secs),
    };
    let resp = tokio::time::timeout(Duration::from_secs(timeout_secs + 5), request(sock, &wait))
        .await
        .expect("the wait never answered");
    match resp {
        Response::Outcomes { members, timed_out } => (members, timed_out, started.elapsed()),
        other => panic!("expected Outcomes, got {other:?}"),
    }
}

const BUSY: &str = "while :; do echo .; sleep 1; done";

#[tokio::test]
async fn outcomes_of_members_with_and_without_agents() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let ids = task(&sock, dir.path(), "feat", &["api"]).await;

    // Never ran an agent.
    let got = outcomes(&sock, &["feat"]).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].member, ids[0]);
    assert_eq!(
        (got[0].task.as_str(), got[0].label.as_str()),
        ("feat", "feat")
    );
    assert_eq!(got[0].state, MemberState::Exit);
    assert_eq!((got[0].provider.as_deref(), got[0].exit_code), (None, None));
    assert!(got[0].response.is_none());

    // Running, then stopped.
    spawn(&sock, &ids[0], BUSY).await;
    let got = outcomes(&sock, &[&ids[0]]).await;
    assert_eq!(got[0].state, MemberState::Working);
    assert_eq!(got[0].provider.as_deref(), Some("sh"));
    let kill = Request::Kill { id: ids[0].clone() };
    assert!(matches!(request(&sock, &kill).await, Response::Ok));
    let got = outcomes(&sock, &["feat"]).await;
    assert_eq!(got[0].state, MemberState::Exit);
    assert_eq!(got[0].exit_code, Some(-1));
    assert_eq!(got[0].provider.as_deref(), Some("sh"));

    // Nobody to ask about.
    let get = Request::GetOutcomes {
        members: vec!["nope".into()],
    };
    match request(&sock, &get).await {
        Response::Error { message } => assert!(message.contains("not found"), "{message}"),
        other => panic!("expected Error, got {other:?}"),
    }
    let get = Request::GetOutcomes { members: vec![] };
    assert!(matches!(request(&sock, &get).await, Response::Error { .. }));
}

#[tokio::test]
async fn wait_ends_once_the_agent_settles() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let ids = task(&sock, dir.path(), "feat", &["api"]).await;
    // Prints, then stays quiet: idle after 5s, which is input enough for an
    // agent without hooks.
    spawn(&sock, &ids[0], "echo started; exec sleep 60").await;

    let (got, timed_out, _) = wait(&sock, &["feat"], &[MemberState::Input], 20).await;
    assert!(!timed_out);
    assert_eq!(got[0].state, MemberState::Idle);
}

#[tokio::test]
async fn wait_times_out_on_a_busy_agent() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let ids = task(&sock, dir.path(), "feat", &["api"]).await;
    spawn(&sock, &ids[0], BUSY).await;

    let (got, timed_out, took) = wait(&sock, &["feat"], &[], 1).await;
    assert!(timed_out);
    assert_eq!(got[0].state, MemberState::Working);
    assert!(took < Duration::from_secs(4), "{took:?}");

    // No time at all: a look, and the answer.
    let (got, timed_out, _) = wait(&sock, &["feat"], &[MemberState::Working], 0).await;
    assert!(!timed_out);
    assert_eq!(got[0].state, MemberState::Working);
}

#[tokio::test]
async fn wait_gives_up_when_an_agent_exits_short_of_the_state() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let ids = task(&sock, dir.path(), "feat", &["api", "web"]).await;
    spawn(&sock, &ids[0], BUSY).await;
    spawn(&sock, &ids[1], "sleep 1; exit 3").await;

    // web exits: it will never be input, whatever api does.
    let members = [ids[0].as_str(), ids[1].as_str()];
    let (got, timed_out, took) = wait(&sock, &members, &[MemberState::Input], 30).await;
    assert!(!timed_out);
    assert!(took < Duration::from_secs(10), "{took:?}");
    assert_eq!(got[0].state, MemberState::Working);
    assert_eq!(got[1].state, MemberState::Exit);
    assert_eq!(got[1].exit_code, Some(3));
    assert_eq!(got[1].label, "feat/web");

    // Waiting for the exit itself: done once api is stopped too.
    let waiting = {
        let sock = sock.clone();
        let api = ids[0].clone();
        tokio::spawn(async move { wait(&sock, &[&api], &[MemberState::Exit], 30).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let kill = Request::Kill { id: ids[0].clone() };
    assert!(matches!(request(&sock, &kill).await, Response::Ok));
    let (got, timed_out, _) = waiting.await.unwrap();
    assert!(!timed_out);
    assert_eq!(
        (got[0].state, got[0].exit_code),
        (MemberState::Exit, Some(-1))
    );
}
