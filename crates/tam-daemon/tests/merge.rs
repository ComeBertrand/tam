//! Merging tasks and splitting a member out, through a daemon: the replies,
//! the events clients get, and what the members' running agents are told
//! (at their next prompt or tool call, not at once).
//!
//! Claude agents are a fake `claude` (a sleep); their hooks are played by
//! `HookEvent` requests. Kept to a single test: it sets process environment
//! (PATH, HOME, XDG_CONFIG_HOME), which must not race other tests in this
//! binary.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tam_daemon::daemon::Daemon;
use tam_proto::{Event, NewMember, Request, Response, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

struct Conn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    events: Vec<Event>,
}

impl Conn {
    async fn connect(sock: &Path) -> Self {
        let (reader, writer) = UnixStream::connect(sock).await.unwrap().into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
            events: Vec::new(),
        }
    }

    async fn read(&mut self) -> ServerMessage {
        let mut line = String::new();
        self.reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    async fn request(&mut self, request: &Request) -> Response {
        let mut json = serde_json::to_string(request).unwrap();
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await.unwrap();
        loop {
            match self.read().await {
                ServerMessage::Response(resp) => return resp,
                ServerMessage::Event(event) => self.events.push(event),
            }
        }
    }

    /// Events pushed until `last` matches (which ends the list).
    async fn events_until(&mut self, last: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut got = std::mem::take(&mut self.events);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !got.last().is_some_and(&last) {
                if let ServerMessage::Event(event) = self.read().await {
                    got.push(event);
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("events so far: {got:?}"));
        got
    }

    /// Create a single-member task; returns its member id.
    async fn task(&mut self, name: &str, root: &Path) -> String {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let request = Request::CreateTask {
            name: name.into(),
            members: vec![NewMember {
                repo_root: PathBuf::from("/src").join(name),
                branch: name.into(),
                dir,
            }],
        };
        match self.request(&request).await {
            Response::Task { task, .. } => task.members[0].id.clone(),
            other => panic!("expected Task, got {other:?}"),
        }
    }

    async fn spawn(&mut self, member: &str, provider: &str, args: &[&str]) {
        let request = Request::Spawn {
            provider: provider.into(),
            dir: PathBuf::new(),
            id: None,
            args: args.iter().map(|a| a.to_string()).collect(),
            resume_session: None,
            prompt: None,
            member: Some(member.into()),
        };
        let resp = self.request(&request).await;
        assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    }

    /// What a hook of the agent gets back: the context it adds, if any.
    async fn hook(&mut self, agent: &str, event: &str, extra: serde_json::Value) -> Option<String> {
        let mut payload = serde_json::json!({"hook_event_name": event});
        if let (Some(p), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
            p.extend(extra.clone());
        }
        let request = Request::HookEvent {
            agent_id: agent.into(),
            event: event.into(),
            payload: Some(payload),
        };
        match self.request(&request).await {
            Response::HookContext { context } => Some(context),
            Response::Ok => None,
            other => panic!("expected Ok or HookContext, got {other:?}"),
        }
    }

    async fn refused(&mut self, request: Request) -> String {
        match self.request(&request).await {
            Response::Error { message } => message,
            other => panic!("expected Error, got {other:?}"),
        }
    }
}

fn fake_claude(bin: &Path) {
    std::fs::create_dir_all(bin).unwrap();
    let fake = bin.join("claude");
    std::fs::write(&fake, "#!/bin/sh\nexec sleep 3600\n").unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

fn none() -> serde_json::Value {
    serde_json::json!({})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merge_and_split_move_members_and_tell_their_agents() {
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
    for _ in 0..40 {
        if UnixStream::connect(&sock).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut conn = Conn::connect(&sock).await;
    let back = conn.task("back", root).await;
    let front = conn.task("front", root).await;
    let docs = conn.task("docs", root).await;
    conn.spawn(&back, "claude", &[]).await;
    conn.spawn(&front, "claude", &[]).await;
    // No hooks: never told.
    conn.spawn(&docs, "sh", &["-c", "sleep 3600"]).await;
    let mut events = Conn::connect(&sock).await;
    assert!(matches!(
        events.request(&Request::Subscribe).await,
        Response::Ok
    ));

    // Refused: nothing changes.
    let into_itself = Request::MergeTasks {
        into: "back".into(),
        from: vec!["back".into()],
    };
    assert!(conn.refused(into_itself).await.contains("itself"));
    let unknown = Request::MergeTasks {
        into: "back".into(),
        from: vec!["nope".into()],
    };
    assert!(conn.refused(unknown).await.contains("'nope' not found"));
    let only = Request::SplitMember {
        member: "front".into(),
        name: Some("x".into()),
    };
    assert!(conn.refused(only).await.contains("only member"));

    // Merge: front's member joins back, front is gone; agents keep running.
    let merge = Request::MergeTasks {
        into: "back".into(),
        from: vec!["front".into(), "docs".into()],
    };
    let (tasks, moved, previous, notified) = match conn.request(&merge).await {
        Response::Moved {
            tasks,
            moved,
            previous,
            notified,
        } => (tasks, moved, previous, notified),
        other => panic!("expected Moved, got {other:?}"),
    };
    let names: Vec<&str> = tasks[0].members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["back", "front", "docs"]);
    assert_eq!(moved, [front.clone(), docs.clone()]);
    assert_eq!(previous[&back], "back");
    assert_eq!(previous[&front], "front");
    assert_eq!(notified, [back.clone(), front.clone()]);

    // Clients drop the merged tasks, update the one that stays, then learn
    // where the moved members' agents are (in no given order).
    let moved_to_back = |e: &Event, member: &str| matches!(e, Event::AgentUpdated { id, info } if id == member && info.task.as_deref() == Some("back"));
    let mut got = Vec::new();
    while ![&front, &docs]
        .iter()
        .all(|m| got.iter().any(|e| moved_to_back(e, m)))
    {
        let more = events
            .events_until(|e| moved_to_back(e, &front) || moved_to_back(e, &docs))
            .await;
        got.extend(more);
    }
    let dropped: Vec<&str> = got
        .iter()
        .filter_map(|e| match e {
            Event::TaskDropped { name } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(dropped, ["front", "docs"]);
    let updated = got
        .iter()
        .position(|e| matches!(e, Event::TaskUpdated { task } if task.name == "back" && task.members.len() == 3))
        .expect("back updated");
    let agent = got
        .iter()
        .position(|e| moved_to_back(e, &front) || moved_to_back(e, &docs))
        .expect("the agents updated");
    assert!(updated < agent, "{got:?}");
    // The directories of the tasks merged away stay where they are.
    let dirs = root.join("tasks");
    assert!(dirs.join("front").join("brief.md").is_file());
    assert!(dirs.join("docs").join("brief.md").is_file());

    // Not at a notification, nor a subagent's tool call: at the next
    // prompt or tool call of the session, once.
    assert_eq!(conn.hook(&back, "Notification", none()).await, None);
    let subagent = serde_json::json!({"agent_id": "sub-1", "tool_name": "Read"});
    assert_eq!(conn.hook(&back, "PostToolUse", subagent).await, None);
    let note = conn.hook(
        &back,
        "UserPromptSubmit",
        serde_json::json!({"prompt": "go"}),
    );
    let note = note.await.expect("a note at the next prompt");
    assert!(
        note.starts_with("[tam] Tasks front, docs were merged into this task (back)"),
        "{note}"
    );
    assert!(note.contains("in task back"), "{note}");
    assert!(note.contains(&format!("- front ({front})")), "{note}");
    let again = conn.hook(
        &back,
        "PostToolUse",
        serde_json::json!({"tool_name": "Read"}),
    );
    assert_eq!(again.await, None);
    // A session start tells it all anyway.
    let start = conn.hook(
        &front,
        "SessionStart",
        serde_json::json!({"source": "compact"}),
    );
    let start = start.await.expect("the start note");
    assert!(start.contains("in task back"), "{start}");
    let after = conn.hook(
        &front,
        "PostToolUse",
        serde_json::json!({"tool_name": "Read"}),
    );
    assert_eq!(after.await, None);

    // Split front back out, under its branch's name.
    let split = Request::SplitMember {
        member: "back/front".into(),
        name: None,
    };
    let (tasks, moved, previous, notified) = match conn.request(&split).await {
        Response::Moved {
            tasks,
            moved,
            previous,
            notified,
        } => (tasks, moved, previous, notified),
        other => panic!("expected Moved, got {other:?}"),
    };
    assert_eq!(tasks[0].name, "front");
    assert_eq!(tasks[1].name, "back");
    assert_eq!(tasks[1].members.len(), 2);
    assert_eq!(moved, vec![front.clone()]);
    assert_eq!(previous[&front], "back/front");
    assert_eq!(notified, [front.clone(), back.clone()]);
    events
        .events_until(|e| matches!(e, Event::AgentUpdated { id, info } if *id == front && info.task.as_deref() == Some("front")))
        .await;
    let note = conn.hook(
        &back,
        "PostToolUse",
        serde_json::json!({"tool_name": "Bash"}),
    );
    let note = note.await.expect("a note at the next tool call");
    assert!(
        note.starts_with(&format!(
            "[tam] Member back/front ({front}) was split out of this task into task front"
        )),
        "{note}"
    );
    let note = conn.hook(
        &front,
        "UserPromptSubmit",
        serde_json::json!({"prompt": "go"}),
    );
    let note = note.await.expect("a note at the next prompt");
    assert!(note.contains("its only member for now"), "{note}");
    // The new task's directory, which the note names, is there.
    let scratch = dirs.join("front").join("scratch");
    assert!(scratch.is_dir());
    assert!(note.contains(&scratch.display().to_string()), "{note}");

    // Its branch names a task now: a second split needs a name.
    conn.request(&Request::MergeTasks {
        into: "back".into(),
        from: vec!["front".into()],
    })
    .await;
    let other = root.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let request = Request::CreateTask {
        name: "front".into(),
        members: vec![NewMember {
            repo_root: PathBuf::from("/src/other"),
            branch: "front".into(),
            dir: other,
        }],
    };
    assert!(matches!(
        conn.request(&request).await,
        Response::Task { .. }
    ));
    let default = Request::SplitMember {
        member: front.clone(),
        name: None,
    };
    let message = conn.refused(default).await;
    assert!(
        message.contains("task 'front' (its branch) already exists: name the new task"),
        "{message}"
    );
    let taken = Request::SplitMember {
        member: front.clone(),
        name: Some("back".into()),
    };
    assert!(conn.refused(taken).await.contains("'back' already exists"));
    let blank = Request::SplitMember {
        member: front.clone(),
        name: Some(" ".into()),
    };
    assert!(conn.refused(blank).await.contains("needs a name"));

    // A task split off has its directory at once: its agent, already
    // running, is told the brief and scratch directory there.
    let named = Request::SplitMember {
        member: front.clone(),
        name: Some("solo".into()),
    };
    assert!(matches!(conn.request(&named).await, Response::Moved { .. }));
    assert!(dirs.join("solo").join("scratch").is_dir());
    assert!(dirs.join("solo").join("brief.md").is_file());
}
