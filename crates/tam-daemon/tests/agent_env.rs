//! An agent runs in its caller's environment (`RunAgent.env`): exactly that,
//! with the daemon's `TAM_AGENT_ID`/`TAM_SOCKET`, its program found on the
//! caller's `PATH`. And the daemon keeps nothing of it: not in the ledger or
//! anything next to it (after a restart, which compacts the ledger, too), not
//! in events, replies or errors.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tam_daemon::daemon::Daemon;
use tam_proto::{AgentEnv, Request, Response, ServerMessage, SessionChoice};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// A variable only the caller has, standing for a token: neither its name
/// nor its value may be found anywhere the daemon writes or answers.
const SECRET_NAME: &str = "TAM_LEAK_CHECK_7F3A9C";
const SECRET_VALUE: &str = "leak-check-value-5be1d2";

struct Conn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    /// Everything the daemon sent, as JSON lines.
    seen: Vec<String>,
}

impl Conn {
    async fn connect(sock: &Path) -> Self {
        let (reader, writer) = UnixStream::connect(sock).await.unwrap().into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
            seen: Vec::new(),
        }
    }

    async fn read(&mut self) -> Option<ServerMessage> {
        let mut line = String::new();
        match self.reader.read_line(&mut line).await {
            Ok(0) | Err(_) => None,
            Ok(_) => {
                let msg = serde_json::from_str(line.trim()).unwrap();
                self.seen.push(line);
                Some(msg)
            }
        }
    }

    async fn request(&mut self, request: &Request) -> Response {
        let mut json = serde_json::to_string(request).unwrap();
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await.unwrap();
        loop {
            match self.read().await.expect("daemon closed the connection") {
                ServerMessage::Response(resp) => return resp,
                ServerMessage::Event(_) => {}
            }
        }
    }

    /// Read the events pushed until none comes for `quiet`.
    async fn drain(&mut self, quiet: Duration) {
        while let Ok(Some(_)) = tokio::time::timeout(quiet, self.read()).await {}
    }
}

async fn start_daemon(sock: &Path, ledger: &Path) {
    // Reconciling often: a daemon shut down lets go of the ledger at its
    // next pass.
    let daemon = Daemon::new(sock.to_path_buf(), ledger.to_path_buf())
        .reconcile_every(Duration::from_millis(200));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    for _ in 0..60 {
        if UnixStream::connect(sock).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon did not start");
}

async fn wait_ledger_released(ledger: &Path) {
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if tam_daemon::ledger::Ledger::open(ledger).is_ok() {
            return;
        }
    }
    panic!("old daemon kept the ledger locked");
}

/// A fake agent that checks its environment and writes what it found to
/// `report` (never the secret itself: an agent's output is kept), then
/// waits. Its arguments: the member id, the daemon's socket, the worktree.
fn env_agent(bin: &Path, report: &Path) {
    let script = format!(
        r#"#!/bin/sh
{{
  [ "${SECRET_NAME}" = "{SECRET_VALUE}" ] && echo secret-ok
  [ -z "${{HOME+set}}" ] && echo no-daemon-home
  [ "$TAM_AGENT_ID" = "$1" ] && echo own-id
  [ "$TAM_SOCKET" = "$2" ] && echo own-socket
  [ "$PWD" = "$3" ] && echo pwd-ok
  echo "term=$TERM"
}} > '{r}.tmp'
mv '{r}.tmp' '{r}'
exec sleep 3600
"#,
        r = report.display()
    );
    let path = bin.join("env-agent");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

async fn read_report(report: &Path) -> Vec<String> {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(report) {
            return text.lines().map(str::to_string).collect();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the agent did not start");
}

fn assert_no_secret(what: &str, text: &str) {
    assert!(
        !text.contains(SECRET_NAME) && !text.contains(SECRET_VALUE),
        "the caller's environment leaked into {what}: {text}"
    );
}

/// Every file under `dir`, with its contents.
fn files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files(&path));
        } else if let Ok(bytes) = std::fs::read(&path) {
            found.push((path, bytes));
        }
    }
    found
}

fn run_agent(member: &str, provider: &str, args: Vec<String>, env: &AgentEnv) -> Request {
    Request::RunAgent {
        member: member.into(),
        provider: Some(provider.into()),
        default_provider: "claude".into(),
        session: SessionChoice::New,
        args,
        prompt: None,
        env: env.clone(),
    }
}

fn create_task(name: &str, dir: &Path) -> Request {
    Request::CreateTask {
        name: name.into(),
        members: vec![tam_proto::NewMember {
            repo_root: PathBuf::from("/src/app"),
            branch: name.into(),
            dir: dir.into(),
        }],
    }
}

#[tokio::test]
async fn an_agent_runs_in_its_callers_environment_and_nothing_of_it_is_kept() {
    let tmp = tempfile::tempdir().unwrap();
    // Resolved: the agent compares `$PWD` with it.
    let root = tmp.path().canonicalize().unwrap();
    let sock = root.join("d.sock");
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let ledger = data.join("ledger.jsonl");
    // The agent is only on the PATH the caller sends, not on the daemon's.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let report = root.join("report");
    env_agent(&bin, &report);
    let work = root.join("app--feat");
    std::fs::create_dir_all(&work).unwrap();

    start_daemon(&sock, &ledger).await;
    let mut watcher = Conn::connect(&sock).await;
    assert!(matches!(
        watcher.request(&Request::Subscribe).await,
        Response::Ok
    ));
    let mut cmd = Conn::connect(&sock).await;
    let member = match cmd.request(&create_task("feat", &work)).await {
        Response::Task { task, .. } => task.members[0].id.clone(),
        other => panic!("expected Task, got {other:?}"),
    };

    // The caller's environment: no HOME (the daemon has one), no TERM, and
    // TAM_* variables that are not this agent's.
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let env = AgentEnv(BTreeMap::from([
        ("PATH".to_string(), path),
        (SECRET_NAME.to_string(), SECRET_VALUE.to_string()),
        ("TAM_AGENT_ID".to_string(), "m-caller".to_string()),
        ("TAM_SOCKET".to_string(), "/elsewhere/sock".to_string()),
    ]));
    let args = vec![
        member.clone(),
        sock.display().to_string(),
        work.display().to_string(),
    ];
    match cmd
        .request(&run_agent(&member, "env-agent", args.clone(), &env))
        .await
    {
        Response::Running { id, .. } => assert_eq!(id, member),
        other => panic!("expected Running, got {other:?}"),
    }
    assert_eq!(
        read_report(&report).await,
        [
            "secret-ok",
            "no-daemon-home",
            "own-id",
            "own-socket",
            "pwd-ok",
            "term=xterm-256color"
        ]
    );

    // Refusals and failures say nothing of it.
    match cmd
        .request(&run_agent(&member, "env-agent", args, &env))
        .await
    {
        Response::Error { message } => assert!(message.contains("already has"), "{message}"),
        other => panic!("expected Error, got {other:?}"),
    }
    let other_work = root.join("app--other");
    std::fs::create_dir_all(&other_work).unwrap();
    let other = match cmd.request(&create_task("other", &other_work)).await {
        Response::Task { task, .. } => task.members[0].id.clone(),
        other => panic!("expected Task, got {other:?}"),
    };
    match cmd
        .request(&run_agent(&other, "no-such-agent-tam", vec![], &env))
        .await
    {
        Response::Error { message } => assert!(message.contains("failed to spawn"), "{message}"),
        other => panic!("expected Error, got {other:?}"),
    }

    // Nor what clients read.
    for request in [
        Request::List,
        Request::ListTasks,
        Request::GetTask {
            name: "feat".into(),
        },
        Request::ResolveMember {
            target: Some(member.clone()),
            dir: None,
            from: None,
        },
        Request::Scrollback { id: member.clone() },
    ] {
        cmd.request(&request).await;
    }
    cmd.request(&Request::Kill { id: member.clone() }).await;
    watcher.drain(Duration::from_millis(300)).await;
    assert_no_secret("replies", &cmd.seen.concat());
    assert_no_secret("events", &watcher.seen.concat());
    assert!(
        watcher.seen.iter().any(|e| e.contains("agent_spawned")),
        "{:?}",
        watcher.seen
    );

    // Nor what the daemon writes: the ledger as recorded, then as compacted
    // by the next daemon, and everything next to it (task directories).
    cmd.request(&Request::Shutdown).await;
    drop((cmd, watcher));
    wait_ledger_released(&ledger).await;
    for (path, bytes) in files(&data) {
        assert_no_secret(
            &path.display().to_string(),
            &String::from_utf8_lossy(&bytes),
        );
    }
    start_daemon(&sock, &ledger).await;
    let mut cmd = Conn::connect(&sock).await;
    match cmd
        .request(&Request::GetTask {
            name: "feat".into(),
        })
        .await
    {
        Response::Task { runs, .. } => assert_eq!(runs.len(), 1, "{runs:?}"),
        other => panic!("expected Task, got {other:?}"),
    }
    let recorded = files(&data);
    assert!(recorded.iter().any(|(p, _)| p == &ledger));
    for (path, bytes) in recorded {
        assert_no_secret(
            &path.display().to_string(),
            &String::from_utf8_lossy(&bytes),
        );
    }
    cmd.request(&Request::Shutdown).await;
}
