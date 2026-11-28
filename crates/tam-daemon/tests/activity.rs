//! The daemon's activity log (`log/` next to the ledger): what it says of
//! requests (the client's pid, how long, the reply), of a task's creation
//! and drop (their steps), of an agent's run, of messages, of a
//! connection's quiet requests; and never a message's text.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tam_daemon::activity::Activity;
use tam_daemon::daemon::Daemon;
use tam_proto::{NewMember, Request, Response, ServerMessage, SessionChoice};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const SECRET: &str = "SECRET-MESSAGE-TEXT";

struct Conn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl Conn {
    async fn connect(sock: &Path) -> Self {
        let (reader, writer) = UnixStream::connect(sock).await.unwrap().into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
        }
    }

    async fn send_line(&mut self, line: &str) -> Response {
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        loop {
            let mut text = String::new();
            assert!(self.reader.read_line(&mut text).await.unwrap() > 0);
            if let ServerMessage::Response(resp) = serde_json::from_str(text.trim()).unwrap() {
                return resp;
            }
        }
    }

    async fn request(&mut self, request: &Request) -> Response {
        self.send_line(&serde_json::to_string(request).unwrap())
            .await
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = tam_worktree::git::command()
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

fn repo(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["commit", "-q", "--allow-empty", "-m", "init"]);
    path.to_path_buf()
}

/// Every line of the log, in order.
fn lines(log: &Path) -> Vec<Value> {
    let mut files: Vec<_> = std::fs::read_dir(log)
        .map(|d| d.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    files.sort();
    files
        .iter()
        .flat_map(|f| {
            let text = std::fs::read_to_string(f).unwrap();
            assert!(
                !text.contains(SECRET),
                "a message's text in {}",
                f.display()
            );
            text.lines()
                .map(|l| serde_json::from_str::<Value>(l).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn find<'a>(lines: &'a [Value], kind: &str, test: impl Fn(&Value) -> bool) -> &'a Value {
    lines
        .iter()
        .find(|l| l["kind"] == kind && test(l))
        .unwrap_or_else(|| panic!("no {kind} line as expected in {lines:#?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_logs_what_it_did_and_how_long_it_took() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let sock = root.join("s.sock");
    let log = root.join("log");
    let daemon = Daemon::new(sock.clone(), root.join("ledger.jsonl"))
        .activity(Activity::to_dir(&log, "daemon"))
        .conn_stats_after(Duration::from_millis(300));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    let mut conn = None;
    for _ in 0..40 {
        if UnixStream::connect(&sock).await.is_ok() {
            conn = Some(Conn::connect(&sock).await);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut conn = conn.expect("daemon did not start");

    let api = repo(&root.join("src/api"));
    let created = conn
        .request(&Request::CreateMembers {
            task: "logs".into(),
            members: vec![NewMember {
                repo_root: api.clone(),
                branch: "logs".into(),
                dir: root.join("wt/api--logs"),
            }],
            source: None,
            join: false,
            env: Default::default(),
        })
        .await;
    let Response::Task { task, .. } = created else {
        panic!("expected Task, got {created:?}");
    };
    let member = task.members[0].id.clone();

    let started = conn
        .request(&Request::RunAgent {
            member: member.clone(),
            provider: Some("sh".into()),
            session: SessionChoice::New,
            args: vec!["-c".into(), "sleep 30".into()],
            prompt: None,
            env: Default::default(),
            wrapper: vec![],
        })
        .await;
    assert!(matches!(started, Response::Running { .. }), "{started:?}");

    // Quiet requests: counted, written when a connection that lasted
    // closes; not for a short one.
    let mut short = Conn::connect(&sock).await;
    short.request(&Request::ListTasks).await;
    drop(short);
    {
        let mut viewer = Conn::connect(&sock).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        for _ in 0..3 {
            let screen = viewer
                .request(&Request::Screen {
                    id: member.clone(),
                    from: None,
                })
                .await;
            assert!(matches!(screen, Response::Screen { .. }), "{screen:?}");
        }
        let gone = viewer
            .request(&Request::Screen {
                id: "m-nobody".into(),
                from: None,
            })
            .await;
        assert!(matches!(gone, Response::Error { .. }));
    }

    let sent = conn
        .request(&Request::SendMessage {
            to: "logs".into(),
            text: SECRET.into(),
            from: None,
        })
        .await;
    assert!(matches!(sent, Response::Sent { .. }), "{sent:?}");
    // A request the daemon cannot read: said without its text.
    let invalid = conn
        .send_line(&format!(
            r#"{{"type":"send_message","to":1,"text":"{SECRET}"}}"#
        ))
        .await;
    assert!(matches!(invalid, Response::Error { .. }));

    assert!(matches!(
        conn.request(&Request::Kill { id: member.clone() }).await,
        Response::Ok
    ));
    let dropped = conn
        .request(&Request::DropTask {
            name: "logs".into(),
            force: true,
            delete_branch: true,
        })
        .await;
    assert!(matches!(dropped, Response::Dropped { .. }), "{dropped:?}");
    assert!(matches!(
        conn.request(&Request::Shutdown).await,
        Response::Ok
    ));
    drop(conn);

    let mut found = Vec::new();
    for _ in 0..100 {
        found = lines(&log);
        if found.iter().any(|l| l["kind"] == "daemon.stop") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let lines = found;
    let pid = std::process::id();

    // Every line has its header.
    for line in &lines {
        assert!(line["ts"].as_i64().is_some(), "{line}");
        assert!(line["time"].as_str().is_some(), "{line}");
        assert_eq!(line["src"], "daemon", "{line}");
        assert_eq!(line["pid"], pid, "{line}");
    }
    find(&lines, "daemon.start", |l| l["version"].is_string());

    // A request: the client's pid, how long, the reply.
    let create = find(&lines, "request", |l| l["req"] == "create_members");
    assert_eq!(create["peer"], pid);
    assert_eq!(create["task"], "logs");
    assert_eq!(create["reply"], "task");
    assert!(create["ms"].as_f64().unwrap() > 0.0);
    let steps = find(&lines, "create.steps", |l| l["task"] == "logs");
    assert_eq!(steps["ok"], true);
    for step in ["lock_wait", "check", "create", "observe"] {
        assert!(steps["steps_ms"][step].is_number(), "{step}: {steps}");
    }

    let spawn = find(&lines, "agent.spawn", |l| l["id"] == member.as_str());
    assert_eq!(spawn["task"], "logs");
    assert_eq!(spawn["provider"], "sh");
    assert_eq!(spawn["resume"], false);
    let run = find(&lines, "request", |l| l["req"] == "run_agent");
    assert_eq!(run["session"], "new");
    assert_eq!(run["reply"], "running");

    // Three screens and a failed one, on the viewer's connection.
    let stats = find(&lines, "conn.stats", |l| {
        l["requests"]["screen"].is_object()
    });
    assert!(!lines
        .iter()
        .any(|l| l["kind"] == "conn.stats" && l["requests"]["list_tasks"].is_object()));
    assert_eq!(stats["requests"]["screen"]["n"], 4);
    assert_eq!(stats["requests"]["screen"]["errors"], 1);
    assert_eq!(stats["peer"], pid);
    assert!(!lines
        .iter()
        .any(|l| l["kind"] == "request" && l["req"] == "screen" && l["slow"].is_null()));

    let message = find(&lines, "message.sent", |l| l["to"] == member.as_str());
    assert_eq!(message["len"], SECRET.len());
    let send = find(&lines, "request", |l| l["req"] == "send_message");
    assert_eq!(send["len"], SECRET.len());
    let invalid = find(&lines, "request.invalid", |_| true);
    assert_eq!(invalid["req"], "send_message");

    let exit = find(&lines, "agent.exit", |l| l["id"] == member.as_str());
    assert_eq!(exit["exit_code"], -1);
    assert_eq!(exit["task"], "logs");
    assert!(exit["run_ms"].as_f64().unwrap() > 0.0);
    assert!(exit["states_ms"]["working"].is_number(), "{exit}");

    let drop = find(&lines, "drop.steps", |l| l["task"] == "logs");
    assert_eq!(drop["ok"], true);
    for step in ["lock_wait", "check", "remove"] {
        assert!(drop["steps_ms"][step].is_number(), "{step}: {drop}");
    }
    let drop_request = find(&lines, "request", |l| l["req"] == "drop_task");
    assert_eq!(drop_request["reply"], "dropped");
    assert_eq!(drop_request["force"], true);

    let stop = find(&lines, "daemon.stop", |_| true);
    assert_eq!(stop["reason"], "requested");
}
