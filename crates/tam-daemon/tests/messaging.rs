//! Messages between task members, through a daemon: queueing, the inbox
//! hook hand-over and its confirmation, replies, the loop guard, and the
//! PTY fallback.
//!
//! The inbox hook (`tam inbox-wait`) is played by a plain `WaitInbox`
//! connection, and Claude's hooks by `HookEvent` requests. Agents are busy
//! shell loops (never idle, so the PTY fallback stays out of the way),
//! except where the fallback is tested.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tam_daemon::daemon::Daemon;
use tam_proto::{Event, MessageInfo, Request, Response, ServerMessage};
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

/// A connection that queues pushed events until asked for them.
struct Conn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    events: std::collections::VecDeque<Event>,
}

impl Conn {
    async fn connect(sock: &Path) -> Self {
        let (reader, writer) = UnixStream::connect(sock).await.unwrap().into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
            events: Default::default(),
        }
    }

    async fn subscribed(sock: &Path) -> Self {
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

    async fn wait_event(&mut self, secs: u64, pred: impl Fn(&Event) -> bool) -> Option<Event> {
        while let Some(event) = self.events.pop_front() {
            if pred(&event) {
                return Some(event);
            }
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
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

    /// Create a single-member task; returns its member id.
    async fn member(&mut self, name: &str, root: &Path) -> String {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let request = Request::CreateTask {
            name: name.into(),
            members: vec![tam_proto::NewMember {
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

    /// An agent that keeps printing: never idle.
    async fn spawn_busy(&mut self, member: &str) {
        self.spawn(member, "sh", &["-c", "while :; do echo .; sleep 1; done"])
            .await;
    }

    async fn send(&mut self, to: &str, text: &str, from: Option<&str>) -> Response {
        self.request(&Request::SendMessage {
            to: to.into(),
            text: text.into(),
            from: from.map(str::to_string),
        })
        .await
    }

    /// Send a message that must be accepted.
    async fn sent(
        &mut self,
        to: &str,
        text: &str,
        from: Option<&str>,
    ) -> (MessageInfo, Option<String>) {
        match self.send(to, text, from).await {
            Response::Sent { message, pending } => (*message, pending),
            other => panic!("expected Sent, got {other:?}"),
        }
    }

    async fn refused(&mut self, to: &str, text: &str, from: Option<&str>) -> String {
        match self.send(to, text, from).await {
            Response::Error { message } => message,
            other => panic!("expected Error, got {other:?}"),
        }
    }

    async fn hook(&mut self, agent: &str, event: &str, payload: serde_json::Value) {
        self.request(&Request::HookEvent {
            agent_id: agent.into(),
            event: event.into(),
            payload: Some(payload),
        })
        .await;
    }

    async fn message(&mut self, id: &str) -> MessageInfo {
        match self.request(&Request::ListMessages { task: None }).await {
            Response::Messages { messages } => messages.into_iter().find(|m| m.id == id).unwrap(),
            other => panic!("expected Messages, got {other:?}"),
        }
    }
}

/// Let a request just sent on another connection take effect: the daemon
/// serves each connection on its own task, so nothing orders them. (Local
/// round trips take well under a millisecond.)
async fn settle() {
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// Wait for the reply to a message on its own connection, as `tam msg
/// --wait` does.
fn wait_reply(sock: &Path, id: &str) -> tokio::task::JoinHandle<Response> {
    let sock = sock.to_path_buf();
    let id = id.to_string();
    tokio::spawn(async move {
        Conn::connect(&sock)
            .await
            .request(&Request::WaitReply {
                id,
                timeout_secs: 10,
            })
            .await
    })
}

/// The reply a `wait_reply` got.
async fn reply(waiting: tokio::task::JoinHandle<Response>) -> (Option<MessageInfo>, String) {
    match tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("a reply")
        .unwrap()
    {
        Response::Reply { message, text } => (message.map(|m| *m), text),
        other => panic!("expected Reply, got {other:?}"),
    }
}

/// Wait for an agent's messages as its inbox hook does.
async fn wait_inbox(sock: PathBuf, agent: String) -> Response {
    Conn::connect(&sock)
        .await
        .request(&Request::WaitInbox { agent_id: agent })
        .await
}

/// The messages an inbox hook gets right away.
async fn take_inbox(sock: &Path, agent: &str) -> (String, Vec<String>) {
    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        wait_inbox(sock.to_path_buf(), agent.to_string()),
    )
    .await
    .expect("messages handed over");
    match resp {
        Response::Inbox { text, ids } => (text, ids),
        other => panic!("expected Inbox, got {other:?}"),
    }
}

/// Hand an agent its messages, and have it show them to its model.
async fn deliver(conn: &mut Conn, sock: &Path, agent: &str) -> Vec<String> {
    let (text, ids) = take_inbox(sock, agent).await;
    let prompt = format!(
        "<task-notification>\n<summary>tam message</summary>\n</task-notification>\n<system-reminder>\n[tam] {text}\n</system-reminder>"
    );
    conn.hook(
        agent,
        "UserPromptSubmit",
        serde_json::json!({"hook_event_name": "UserPromptSubmit", "prompt": prompt}),
    )
    .await;
    ids
}

#[tokio::test]
async fn inbox_hook_gets_queued_messages_and_the_prompt_confirms_them() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    let mut events = Conn::subscribed(&sock).await;
    let member = conn.member("feat", dir.path()).await;

    let (first, pending) = conn.sent("feat", "  hello\n", None).await;
    assert_eq!(first.text, "hello");
    assert_eq!((first.from, first.hop), (None, 0));
    assert!(pending.unwrap().contains("no agent is running"));
    assert!(events
        .wait_event(
            5,
            |e| matches!(e, Event::MessageSent { message } if message.id == first.id)
        )
        .await
        .is_some());

    conn.spawn_busy(&member).await;
    let (text, ids) = take_inbox(&sock, &member).await;
    assert_eq!(ids, [first.id.as_str()]);
    assert_eq!(text, format!("Message {} from the user:\nhello", first.id));
    // Handed over is not delivered yet
    assert!(conn.message(&first.id).await.delivered_at.is_none());

    // Nothing waits: the next one is queued, and not handed over twice.
    let (second, pending) = conn.sent(&member, "again", None).await;
    assert!(pending.is_some());

    // The prompt that shows it to the model confirms the delivery.
    conn.hook(
        &member,
        "UserPromptSubmit",
        serde_json::json!({"prompt": format!("<task-notification>…{text}")}),
    )
    .await;
    let delivered = events
        .wait_event(5, |e| matches!(e, Event::MessageDelivered { .. }))
        .await
        .expect("MessageDelivered");
    match delivered {
        Event::MessageDelivered { message } => {
            assert_eq!(message.id, first.id);
            assert!(message.delivered_at.is_some());
        }
        _ => unreachable!(),
    }
    assert!(conn.message(&first.id).await.delivered_at.is_some());

    let (_, ids) = take_inbox(&sock, &member).await;
    assert_eq!(ids, [second.id]);
}

#[tokio::test]
async fn a_newer_waiter_releases_the_older_and_a_gone_one_takes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    let member = conn.member("feat", dir.path()).await;
    conn.spawn_busy(&member).await;

    let older = tokio::spawn(wait_inbox(sock.clone(), member.clone()));
    settle().await;
    let newer = tokio::spawn(wait_inbox(sock.clone(), member.clone()));
    let released = tokio::time::timeout(Duration::from_secs(5), older)
        .await
        .expect("older waiter released")
        .unwrap();
    assert!(matches!(released, Response::Ok), "{released:?}");
    newer.abort();
    let _ = newer.await;

    // The newer one is gone too: the message stays queued.
    let mut gone = Conn::connect(&sock).await;
    let json = serde_json::to_string(&Request::WaitInbox {
        agent_id: member.clone(),
    })
    .unwrap();
    gone.writer
        .write_all(format!("{json}\n").as_bytes())
        .await
        .unwrap();
    settle().await;
    drop(gone);
    settle().await;
    let (message, _) = conn.sent("feat", "hi", None).await;
    let (_, ids) = take_inbox(&sock, &member).await;
    assert_eq!(ids, [message.id]);

    // Waiting for an unknown agent, or one that is no task member, fails.
    let resp = wait_inbox(sock.clone(), "nobody".into()).await;
    assert!(matches!(resp, Response::Error { .. }), "{resp:?}");
}

#[tokio::test]
async fn messages_in_flight_go_back_to_the_queue_when_the_agent_exits() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    let member = conn.member("feat", dir.path()).await;
    conn.spawn_busy(&member).await;
    let (message, _) = conn.sent("feat", "hi", None).await;
    let (_, ids) = take_inbox(&sock, &member).await;
    assert_eq!(ids, [message.id.as_str()]);

    // The agent dies before showing it: its next agent gets it again.
    assert!(matches!(
        conn.request(&Request::Kill { id: member.clone() }).await,
        Response::Ok
    ));
    let (_, pending) = conn.sent("feat", "and this", None).await;
    assert!(pending.unwrap().contains("no agent is running"));
    conn.spawn_busy(&member).await;
    let (text, ids) = take_inbox(&sock, &member).await;
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], message.id);
    assert!(text.contains("hi\n\nMessage msg-"), "{text}");
}

#[tokio::test]
async fn the_reply_is_the_response_that_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    let member = conn.member("feat", dir.path()).await;
    conn.spawn_busy(&member).await;

    let (message, _) = conn.sent("feat", "status?", None).await;
    let waiting = {
        let sock = sock.clone();
        let id = message.id.clone();
        tokio::spawn(async move {
            Conn::connect(&sock)
                .await
                .request(&Request::WaitReply {
                    id,
                    timeout_secs: 10,
                })
                .await
        })
    };
    // A turn that ends before the message reaches the model is no reply.
    conn.hook(
        &member,
        "Stop",
        serde_json::json!({"last_assistant_message": "unrelated"}),
    )
    .await;
    deliver(&mut conn, &sock, &member).await;
    conn.hook(
        &member,
        "Stop",
        serde_json::json!({"last_assistant_message": "all green"}),
    )
    .await;
    let reply = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("reply")
        .unwrap();
    match reply {
        Response::Reply {
            message: None,
            text,
        } => assert_eq!(text, "all green"),
        other => panic!("expected Reply, got {other:?}"),
    }
    // Asked again after the fact: the same reply.
    let again = conn
        .request(&Request::WaitReply {
            id: message.id.clone(),
            timeout_secs: 1,
        })
        .await;
    assert!(matches!(again, Response::Reply { text, .. } if text == "all green"));

    // A failed turn fails the wait.
    let (failing, _) = conn.sent("feat", "deploy", None).await;
    deliver(&mut conn, &sock, &member).await;
    conn.hook(
        &member,
        "StopFailure",
        serde_json::json!({"error": "rate_limit"}),
    )
    .await;
    match conn
        .request(&Request::WaitReply {
            id: failing.id,
            timeout_secs: 1,
        })
        .await
    {
        Response::Error { message } => assert!(message.contains("rate_limit"), "{message}"),
        other => panic!("expected Error, got {other:?}"),
    }

    // No reply in time
    let (silent, _) = conn.sent("feat", "anyone?", None).await;
    match conn
        .request(&Request::WaitReply {
            id: silent.id,
            timeout_secs: 1,
        })
        .await
    {
        Response::Error { message } => assert!(message.contains("no reply"), "{message}"),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn an_answer_is_the_reply_and_is_not_shown_twice() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    let a = conn.member("api", dir.path()).await;
    let b = conn.member("web", dir.path()).await;
    conn.spawn_busy(&a).await;
    conn.spawn_busy(&b).await;

    let (question, _) = conn.sent("web", "which port?", Some(&a)).await;
    assert_eq!(question.from.as_deref(), Some(a.as_str()));
    assert_eq!((question.from_label.as_str(), question.hop), ("api", 1));
    let waiting = {
        let sock = sock.clone();
        let id = question.id.clone();
        tokio::spawn(async move {
            Conn::connect(&sock)
                .await
                .request(&Request::WaitReply {
                    id,
                    timeout_secs: 10,
                })
                .await
        })
    };
    settle().await;
    let (answer, pending) = conn.sent("api", "8080", Some(&b)).await;
    assert_eq!(answer.reply_to.as_deref(), Some(question.id.as_str()));
    // The waiting `tam msg --wait` of api got it: delivered.
    assert!(pending.is_none(), "{pending:?}");
    assert!(answer.delivered_at.is_some());
    match tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap()
    {
        Response::Reply {
            message: Some(m),
            text,
        } => {
            assert_eq!(text, "8080");
            assert_eq!(m.id, answer.id);
        }
        other => panic!("expected Reply, got {other:?}"),
    }
    // A sender unknown to the daemon is the user.
    let (from_user, _) = conn.sent("web", "hi", Some("agent-42")).await;
    assert_eq!(from_user.from, None);
}

/// api asks web and waits; web asks back and waits too. Its question
/// answers api's, and api's response at the end of its turn answers it.
#[tokio::test]
async fn two_agents_waiting_on_each_other_both_get_a_reply() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    let a = conn.member("api", dir.path()).await;
    let b = conn.member("web", dir.path()).await;
    conn.spawn_busy(&a).await;
    conn.spawn_busy(&b).await;

    let (first, _) = conn.sent("web", "which port?", Some(&a)).await;
    let a_waits = wait_reply(&sock, &first.id);
    assert_eq!(deliver(&mut conn, &sock, &b).await, [first.id.as_str()]);
    settle().await;

    let (second, pending) = conn.sent("api", "for which env?", Some(&b)).await;
    assert_eq!(second.reply_to.as_deref(), Some(first.id.as_str()));
    assert!(pending.is_none(), "{pending:?}");
    let b_waits = wait_reply(&sock, &second.id);
    let (message, text) = reply(a_waits).await;
    assert_eq!(text, "for which env?");
    assert_eq!(message.unwrap().id, second.id);

    // api got web's question through its `--wait`, in its current turn.
    conn.hook(
        &a,
        "Stop",
        serde_json::json!({"last_assistant_message": "dev: 8080"}),
    )
    .await;
    let (message, text) = reply(b_waits).await;
    assert_eq!((message, text.as_str()), (None, "dev: 8080"));
}

#[tokio::test]
async fn bad_messages_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    let a = conn.member("api", dir.path()).await;
    conn.spawn_busy(&a).await;
    assert!(conn
        .refused("api", "hi", Some(&a))
        .await
        .contains("yourself"));
    assert!(conn.refused("api", " \n ", None).await.contains("empty"));
    assert!(conn
        .refused("api", &"x".repeat(8001), None)
        .await
        .contains("too long"));
    assert!(conn.refused("nope", "hi", None).await.contains("not found"));
    let resp = conn
        .request(&Request::ListMessages {
            task: Some("nope".into()),
        })
        .await;
    assert!(matches!(resp, Response::Error { .. }), "{resp:?}");
}

#[tokio::test]
async fn the_loop_guard_stops_endless_exchanges_until_the_user_prompts() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    let a = conn.member("api", dir.path()).await;
    let b = conn.member("web", dir.path()).await;
    conn.spawn_busy(&a).await;
    conn.spawn_busy(&b).await;

    // a and b answer each other: each message is one more hop.
    let mut pair = [(&a, &b, "web"), (&b, &a, "api")].into_iter().cycle();
    for hop in 1..=6 {
        let (from, to, to_name) = pair.next().unwrap();
        let (message, _) = conn.sent(to_name, "ping", Some(from)).await;
        assert_eq!(message.hop, hop);
        assert_eq!(deliver(&mut conn, &sock, to).await, [message.id]);
    }
    let (from, _, to_name) = pair.next().unwrap();
    let refused = conn.refused(to_name, "ping", Some(from)).await;
    assert!(refused.contains("loop guard"), "{refused}");

    // The user prompts the agent: its exchanges start over.
    conn.hook(
        from,
        "UserPromptSubmit",
        serde_json::json!({"prompt": "carry on"}),
    )
    .await;
    let (message, _) = conn.sent(to_name, "ping", Some(from)).await;
    assert_eq!(message.hop, 1);
}

#[tokio::test]
async fn an_agent_without_inbox_hook_gets_messages_typed_in_once_idle() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    let mut events = Conn::subscribed(&sock).await;
    let member = conn.member("feat", dir.path()).await;
    conn.spawn(&member, "cat", &[]).await;

    let (message, pending) = conn.sent("feat", "hello\nworld", None).await;
    assert!(pending.unwrap().contains("once it is idle"));
    // The generic provider is idle after 5s of silence.
    assert!(events
        .wait_event(
            15,
            |e| matches!(e, Event::MessageDelivered { message: m } if m.id == message.id)
        )
        .await
        .is_some());
    // One line, submitted: the terminal echoes it, cat prints it back.
    let expected = format!("[tam] Message {} from the user: hello world", message.id);
    let mut scrollback = String::new();
    for _ in 0..50 {
        if let Response::Scrollback { data } = conn
            .request(&Request::Scrollback { id: member.clone() })
            .await
        {
            scrollback = data;
            if scrollback.matches(&expected).count() >= 2 {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("message not typed in: {scrollback:?}");
}

#[tokio::test]
async fn queued_messages_survive_a_daemon_restart() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    let member = conn.member("feat", dir.path()).await;
    let (message, _) = conn.sent("feat", "for later", None).await;
    conn.request(&Request::Shutdown).await;
    drop(conn);
    let ledger = dir.path().join("ledger.jsonl");
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if tam_daemon::ledger::Ledger::open(&ledger).is_ok() {
            break;
        }
    }

    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    match conn
        .request(&Request::ListMessages {
            task: Some("feat".into()),
        })
        .await
    {
        Response::Messages { messages } => {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].id, message.id);
            assert!(messages[0].delivered_at.is_none());
        }
        other => panic!("expected Messages, got {other:?}"),
    }
    conn.spawn_busy(&member).await;
    let (_, ids) = take_inbox(&sock, &member).await;
    assert_eq!(ids, [message.id]);
}
