//! Answering an agent's permission request from a client, through a
//! daemon: the request shown to clients, the answer handed to the hook
//! waiting on that request only, and every way a request ends without one
//! (stale id, the hook gone, its tool run from the terminal, a newer
//! request, the end of the turn, the agent's exit).
//!
//! The agent is a fake `claude` (a sleep); its `PermissionRequest` hook is
//! played as `tam hook-notify` does it: a `HookEvent`, then `WaitAnswer` on
//! the same connection. Kept to a single test: it sets process environment
//! (PATH, HOME, XDG_CONFIG_HOME), which must not race other tests in this
//! binary.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tam_daemon::daemon::Daemon;
use tam_proto::{
    AgentInfo, AgentState, Ask, AskAnswer, AskKind, Event, NewMember, QuestionAnswer, Request,
    Response, ServerMessage,
};
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

    async fn read(&mut self) -> Option<ServerMessage> {
        let mut line = String::new();
        match self.reader.read_line(&mut line).await {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(serde_json::from_str(line.trim()).unwrap()),
        }
    }

    async fn write(&mut self, request: &Request) {
        let mut json = serde_json::to_string(request).unwrap();
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await.unwrap();
    }

    async fn response(&mut self) -> Response {
        loop {
            match self.read().await.expect("daemon closed the connection") {
                ServerMessage::Response(resp) => return resp,
                ServerMessage::Event(event) => self.events.push(event),
            }
        }
    }

    async fn request(&mut self, request: &Request) -> Response {
        self.write(request).await;
        self.response().await
    }

    /// The next `AgentUpdated` of `id` that `pred` takes.
    async fn updated(&mut self, id: &str, pred: impl Fn(&AgentInfo) -> bool) -> AgentInfo {
        let found = |event: &Event| match event {
            Event::AgentUpdated { id: of, info } if of == id && pred(info) => Some(info.clone()),
            _ => None,
        };
        if let Some(info) = self.events.iter().find_map(found) {
            self.events.clear();
            return info;
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(ServerMessage::Event(event)) = self.read().await {
                    if let Some(info) = found(&event) {
                        return info;
                    }
                }
            }
        })
        .await
        .expect("no such AgentUpdated")
    }

    async fn hook(&mut self, agent: &str, event: &str, extra: Value) -> Response {
        self.request(&hook_event(agent, event, extra)).await
    }

    async fn agent(&mut self, id: &str) -> AgentInfo {
        match self.request(&Request::List).await {
            Response::Agents { agents } => agents.into_iter().find(|a| a.id == id).unwrap(),
            other => panic!("expected Agents, got {other:?}"),
        }
    }

    async fn answer(&mut self, agent: &str, ask: &str, answer: AskAnswer) -> Response {
        let request = Request::Answer {
            agent: agent.into(),
            ask: ask.into(),
            answer,
        };
        self.request(&request).await
    }
}

fn hook_event(agent: &str, event: &str, extra: Value) -> Request {
    let mut payload = json!({ "hook_event_name": event, "session_id": "s-1" });
    payload
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    Request::HookEvent {
        agent_id: agent.into(),
        event: event.into(),
        payload: Some(payload),
    }
}

fn tool(name: &str, input: Value) -> Value {
    json!({ "tool_name": name, "tool_input": input })
}

/// A `PermissionRequest` hook waiting for its answer, as `tam hook-notify`
/// waits: the hook event, then `WaitAnswer` on the same connection.
struct Hook(Conn);

impl Hook {
    async fn wait(sock: &Path, agent: &str, payload: Value) -> Self {
        let mut conn = Conn::connect(sock).await;
        let resp = conn.hook(agent, "PermissionRequest", payload.clone()).await;
        assert!(matches!(resp, Response::Ok), "{resp:?}");
        let Request::HookEvent {
            payload: Some(payload),
            ..
        } = hook_event(agent, "PermissionRequest", payload)
        else {
            unreachable!()
        };
        conn.write(&Request::WaitAnswer {
            agent_id: agent.into(),
            payload,
        })
        .await;
        Self(conn)
    }

    /// What the hook is told, once it is told.
    async fn told(mut self) -> Response {
        tokio::time::timeout(Duration::from_secs(5), self.0.response())
            .await
            .expect("the hook is still waiting")
    }

    /// The decision the hook prints for Claude.
    async fn decision(self) -> Value {
        match self.told().await {
            Response::HookOutput { output } => {
                let output: Value = serde_json::from_str(&output).unwrap();
                output["hookSpecificOutput"]["decision"].clone()
            }
            other => panic!("expected HookOutput, got {other:?}"),
        }
    }

    /// Still waiting: told nothing yet.
    async fn still_waiting(&mut self) {
        let read = tokio::time::timeout(Duration::from_millis(300), self.0.read()).await;
        assert!(read.is_err(), "the hook was told {read:?}");
    }
}

fn ask_of(info: &AgentInfo) -> Ask {
    info.detail.ask.clone().expect("a request to answer")
}

fn fake_claude(bin: &Path) {
    std::fs::create_dir_all(bin).unwrap();
    let fake = bin.join("claude");
    std::fs::write(&fake, "#!/bin/sh\nexec sleep 3600\n").unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_answers_the_request_the_hook_waits_on() {
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
    let dir = root.join("feat");
    std::fs::create_dir_all(&dir).unwrap();
    let created = conn
        .request(&Request::CreateTask {
            name: "feat".into(),
            members: vec![NewMember {
                repo_root: PathBuf::from("/src/app"),
                branch: "feat".into(),
                dir,
            }],
        })
        .await;
    let Response::Task { task, .. } = created else {
        panic!("expected Task, got {created:?}");
    };
    let id = task.members[0].id.clone();
    let spawned = conn
        .request(&Request::Spawn {
            provider: "claude".into(),
            dir: PathBuf::new(),
            id: None,
            args: vec![],
            resume_session: None,
            prompt: None,
            member: Some(id.clone()),
        })
        .await;
    assert!(matches!(spawned, Response::Spawned { .. }), "{spawned:?}");
    let mut events = Conn::connect(&sock).await;
    assert!(matches!(
        events.request(&Request::Subscribe).await,
        Response::Ok
    ));

    // A permission: clients see the whole input, under an id of its own.
    let bash = tool(
        "Bash",
        json!({"command": "rm -rf build", "description": "Clean"}),
    );
    let hook = Hook::wait(&sock, &id, bash.clone()).await;
    let info = events.updated(&id, |i| i.detail.ask.is_some()).await;
    assert_eq!(info.state, AgentState::Blocked);
    let ask = ask_of(&info);
    assert_eq!(ask.tool, "Bash");
    assert_eq!(
        ask.kind,
        AskKind::Permission {
            input: "command: rm -rf build\ndescription: Clean".into(),
            cut: false,
        }
    );
    // An answer to another request, or one that does not fit, sends nothing.
    let stale = conn.answer(&id, "ask-00000000", AskAnswer::Allow).await;
    assert!(
        matches!(&stale, Response::Error { message } if message.contains("nothing was sent")),
        "{stale:?}"
    );
    let wrong = AskAnswer::Answers { answers: vec![] };
    assert!(matches!(
        conn.answer(&id, &ask.id, wrong).await,
        Response::Error { .. }
    ));
    let mut hook = hook;
    hook.still_waiting().await;

    // Allowed: the hook prints the decision, the agent goes on.
    let resp = conn.answer(&id, &ask.id, AskAnswer::Allow).await;
    assert!(matches!(resp, Response::Ok), "{resp:?}");
    assert_eq!(hook.decision().await, json!({ "behavior": "allow" }));
    let info = conn.agent(&id).await;
    assert_eq!(info.state, AgentState::Working);
    assert!(info.detail.ask.is_none() && info.detail.pending.is_none());
    // Once only.
    assert!(matches!(
        conn.answer(&id, &ask.id, AskAnswer::Allow).await,
        Response::Error { .. }
    ));

    // Denied with a reason: the agent is told who and why.
    let hook = Hook::wait(&sock, &id, bash.clone()).await;
    let ask = ask_of(&events.updated(&id, |i| i.detail.ask.is_some()).await);
    let deny = AskAnswer::Deny {
        reason: Some("use make clean".into()),
    };
    assert!(matches!(
        conn.answer(&id, &ask.id, deny).await,
        Response::Ok
    ));
    let decision = hook.decision().await;
    assert_eq!(decision["behavior"], "deny");
    assert!(decision.get("interrupt").is_none(), "{decision}");
    assert!(decision["message"]
        .as_str()
        .unwrap()
        .ends_with("tam dashboard, with this reason: use make clean"));

    // The hook goes away (the user said no in the terminal): nothing to
    // answer any more.
    let hook = Hook::wait(&sock, &id, bash.clone()).await;
    let ask = ask_of(&events.updated(&id, |i| i.detail.ask.is_some()).await);
    drop(hook);
    events.updated(&id, |i| i.detail.ask.is_none()).await;
    assert!(matches!(
        conn.answer(&id, &ask.id, AskAnswer::Allow).await,
        Response::Error { .. }
    ));

    // Questions: answered as Claude's own dialog answers them.
    let questions = json!({ "questions": [
        { "question": "Which colors?", "header": "Colors", "multiSelect": true,
          "options": [{ "label": "Red" }, { "label": "Blue" }] },
        { "question": "Which size?", "multiSelect": false,
          "options": [{ "label": "Small" }, { "label": "Large" }] },
    ]});
    let hook = Hook::wait(&sock, &id, tool("AskUserQuestion", questions.clone())).await;
    let info = events.updated(&id, |i| i.detail.ask.is_some()).await;
    assert_eq!(info.state, AgentState::Question);
    let ask = ask_of(&info);
    let AskKind::Questions { questions: shown } = &ask.kind else {
        panic!("expected questions, got {:?}", ask.kind);
    };
    assert_eq!(shown.len(), 2);
    assert!(matches!(
        conn.answer(&id, &ask.id, AskAnswer::Allow).await,
        Response::Error { .. }
    ));
    let answers = AskAnswer::Answers {
        answers: vec![
            QuestionAnswer {
                chosen: vec!["Red".into(), "Blue".into()],
                other: None,
            },
            QuestionAnswer {
                chosen: vec![],
                other: Some("Medium".into()),
            },
        ],
    };
    assert!(matches!(
        conn.answer(&id, &ask.id, answers).await,
        Response::Ok
    ));
    let decision = hook.decision().await;
    assert_eq!(decision["behavior"], "allow");
    assert_eq!(
        decision["updatedInput"]["answers"],
        json!({ "Which colors?": "Red, Blue", "Which size?": "Medium" })
    );

    // Answered in the terminal, the hook keeps running: the tool's
    // PostToolUse ends the request, and the hook is told nothing to print.
    let hook = Hook::wait(&sock, &id, tool("AskUserQuestion", questions.clone())).await;
    let ask = ask_of(&events.updated(&id, |i| i.detail.ask.is_some()).await);
    let mut ran = questions.clone();
    ran["answers"] = json!({ "Which colors?": "Red" });
    let resp = conn
        .hook(&id, "PostToolUse", tool("AskUserQuestion", ran))
        .await;
    assert!(matches!(resp, Response::Ok), "{resp:?}");
    assert!(matches!(hook.told().await, Response::Ok));
    events.updated(&id, |i| i.detail.ask.is_none()).await;
    assert!(matches!(
        conn.answer(&id, &ask.id, AskAnswer::Allow).await,
        Response::Error { .. }
    ));

    // Another tool running meanwhile (a parallel call) leaves it waiting;
    // a newer request ends it.
    let mut first = Hook::wait(&sock, &id, bash.clone()).await;
    let ask = ask_of(&events.updated(&id, |i| i.detail.ask.is_some()).await);
    conn.hook(&id, "PostToolUse", tool("Read", json!({"file_path": "/a"})))
        .await;
    first.still_waiting().await;
    let other = tool("Bash", json!({"command": "make"}));
    let second = Hook::wait(&sock, &id, other).await;
    assert!(matches!(first.told().await, Response::Ok));
    let newer = ask_of(
        &events
            .updated(&id, |i| {
                i.detail.ask.as_ref().is_some_and(|a| a.id != ask.id)
            })
            .await,
    );
    assert!(matches!(
        conn.answer(&id, &ask.id, AskAnswer::Allow).await,
        Response::Error { .. }
    ));
    // The end of the turn ends it too.
    conn.hook(&id, "Stop", json!({"last_assistant_message": "done"}))
        .await;
    assert!(matches!(second.told().await, Response::Ok));
    assert!(matches!(
        conn.answer(&id, &newer.id, AskAnswer::Allow).await,
        Response::Error { .. }
    ));

    // Only a request the agent waits on is held: none after the turn.
    let mut late = Conn::connect(&sock).await;
    late.write(&Request::WaitAnswer {
        agent_id: id.clone(),
        payload: tool("Bash", json!({"command": "ls"})),
    })
    .await;
    assert!(matches!(late.response().await, Response::Ok));

    // The agent exits: its hook is released (hooks outlive their agent).
    let hook = Hook::wait(&sock, &id, bash).await;
    events.updated(&id, |i| i.detail.ask.is_some()).await;
    assert!(matches!(
        conn.request(&Request::Kill { id: id.clone() }).await,
        Response::Ok
    ));
    assert!(matches!(hook.told().await, Response::Ok));
}
