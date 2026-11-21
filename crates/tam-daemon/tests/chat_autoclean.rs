//! Autoclean (#65) through a daemon. At start, before any client connects,
//! the chats past `[chat] autoclean` (read from the config file) are
//! closed: a kept one, one with a recent turn or un-keep, a new one are not;
//! a change of the setting reaches clients without a restart. On the timer,
//! a chat past its time whose agent waits on the user (blocked, question)
//! or works is not closed, nor one at the prompt someone types in; an
//! answer is a turn; once at the prompt with nobody there, it is closed as
//! `CloseChat` closes it.
//!
//! The agent is a fake `claude` (it prints a dot a second: no silence
//! fallback), found on the `PATH` sent with `RunAgent`; its hooks are
//! played as `tam hook-notify` plays them. Kept to a single test: it sets
//! process environment (HOME, XDG_CONFIG_HOME, CLAUDE_CONFIG_DIR: the
//! test's config, no notify command or Claude files of whoever runs it),
//! which must not race other tests in this binary.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tam_daemon::daemon::Daemon;
use tam_proto::{AgentEnv, ChatInfo, Event, Request, Response, ServerMessage, SessionChoice};
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
        let mut line = Vec::new();
        match self.reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(serde_json::from_slice(&line).unwrap()),
        }
    }

    async fn request(&mut self, request: &Request) -> Response {
        let mut json = serde_json::to_string(request).unwrap();
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await.unwrap();
        loop {
            match self.read().await.expect("daemon closed the connection") {
                ServerMessage::Response(resp) => return resp,
                ServerMessage::Event(event) => self.events.push(event),
            }
        }
    }

    async fn ok(&mut self, request: &Request) {
        let resp = self.request(request).await;
        assert!(matches!(resp, Response::Ok), "{request:?}: {resp:?}");
    }

    async fn chat(&mut self, request: &Request) -> ChatInfo {
        match self.request(request).await {
            Response::Chat { chat, .. } => *chat,
            other => panic!("expected Chat for {request:?}, got {other:?}"),
        }
    }

    async fn chats(&mut self) -> Vec<ChatInfo> {
        match self.request(&Request::ListChats).await {
            Response::Chats { chats } => chats,
            other => panic!("expected Chats, got {other:?}"),
        }
    }

    async fn hook(&mut self, agent: &str, event: &str, extra: Value) {
        let mut payload = json!({ "hook_event_name": event, "session_id": "s-1" });
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        self.ok(&Request::HookEvent {
            agent_id: agent.into(),
            event: event.into(),
            payload: Some(payload),
        })
        .await;
    }

    /// Wait for events until `pred` takes one; returns what it gives.
    async fn event<T>(&mut self, pred: impl Fn(&Event) -> Option<T>) -> T {
        if let Some(i) = self.events.iter().position(|e| pred(e).is_some()) {
            let event = self.events.remove(i);
            return pred(&event).expect("taken above");
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(ServerMessage::Event(event)) = self.read().await {
                    if let Some(found) = pred(&event) {
                        return found;
                    }
                }
            }
        })
        .await
        .expect("no such event")
    }

    /// The next `ChatUpdated` of chat `id` that `pred` takes.
    async fn updated(&mut self, id: &str, pred: impl Fn(&ChatInfo) -> bool) -> ChatInfo {
        self.event(|e| match e {
            Event::ChatUpdated { chat } if chat.id == id && pred(chat) => Some(chat.clone()),
            _ => None,
        })
        .await
    }
}

async fn wait_for_socket(sock: &Path) {
    for _ in 0..60 {
        if UnixStream::connect(sock).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon did not start");
}

fn git(dir: &Path, args: &[&str]) {
    let out = tam_worktree::git::command()
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

const DAY: u64 = 86_400;

/// Write tam's config where tam reads it: `$XDG_CONFIG_HOME`, or on macOS
/// `~/Library/Application Support`.
fn write_config(root: &Path, text: &str) {
    let macos = root
        .join("home")
        .join("Library")
        .join("Application Support");
    for dir in [root.join("config"), macos] {
        let dir = dir.join("tam");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), text).unwrap();
    }
}

/// The ledger lines of chats made, turned and kept at these times.
fn chat_lines(chats: &[(&str, u64, Option<&str>)], others: &[Value]) -> String {
    let mut lines: Vec<Value> = chats
        .iter()
        .map(|(id, created, session)| {
            let mut line = json!({
                "type": "chat_created",
                "chat": id,
                "dir": "/src/api",
                "timestamp": created,
            });
            if let Some(session) = session {
                line["session"] = json!(session);
            }
            line
        })
        .collect();
    lines.extend(others.iter().cloned());
    lines.iter().map(|l| format!("{l}\n")).collect()
}

fn lines_of(ledger: &Path, kind: &str) -> Vec<String> {
    std::fs::read_to_string(ledger)
        .unwrap()
        .lines()
        .filter(|l| l.contains(&format!(r#""type":"{kind}""#)))
        .map(str::to_string)
        .collect()
}

/// A fake `claude` that prints a dot a second, whatever it is given.
fn fake_claude(bin: &Path) {
    std::fs::create_dir_all(bin).unwrap();
    let fake = bin.join("claude");
    std::fs::write(&fake, "#!/bin/sh\nwhile :; do printf .; sleep 1; done\n").unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_autoclean() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::env::set_var("HOME", root.join("home"));
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    std::env::set_var("CLAUDE_CONFIG_DIR", root.join("claude"));

    at_start_from_the_config(&root.join("start")).await;
    on_the_timer_once_at_the_prompt(&root.join("timer")).await;
}

/// Closed at start, after the days the config says; a change of the
/// setting reaches clients at the next pass.
async fn at_start_from_the_config(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    // Two days, not the default three: the file is read.
    write_config(dir.parent().unwrap(), "[chat]\nautoclean = 2\n");
    let now = now();
    let (old, made, kept, turned, unkept, new) = (
        "c-00000001",
        "c-00000002",
        "c-00000003",
        "c-00000004",
        "c-00000005",
        "c-00000006",
    );
    let ago = |secs: u64| now - secs;
    let kept_at = |chat: &str, keep: bool, at: u64| json!({ "type": "chat_kept", "chat": chat, "keep": keep, "timestamp": at });
    let ledger = dir.join("ledger.jsonl");
    std::fs::write(
        &ledger,
        chat_lines(
            &[
                (old, ago(DAY * 5 / 2), None),
                // Made from an old session: its clock started in tam.
                (made, ago(DAY * 5 / 2), Some("s-old")),
                (kept, ago(10 * DAY), None),
                (turned, ago(10 * DAY), None),
                (unkept, ago(10 * DAY), None),
                (new, ago(3600), None),
            ],
            &[
                kept_at(kept, true, ago(9 * DAY)),
                json!({ "type": "chat_turn", "chat": turned, "timestamp": ago(DAY) }),
                kept_at(unkept, true, ago(9 * DAY)),
                kept_at(unkept, false, ago(3600)),
            ],
        ),
    )
    .unwrap();

    let sock = dir.join("tam.sock");
    let daemon =
        Daemon::new(sock.clone(), ledger.clone()).autoclean_every(Duration::from_millis(100));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    wait_for_socket(&sock).await;
    let mut conn = Conn::connect(&sock).await;
    let mut events = Conn::connect(&sock).await;
    events.ok(&Request::Subscribe).await;

    // Gone before any client could see them, as a close by hand leaves them.
    let chats = conn.chats().await;
    let ids: Vec<&str> = chats.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, [kept, turned, unkept, new]);
    let closed = lines_of(&ledger, "chat_closed");
    assert_eq!(closed.len(), 2, "{closed:?}");
    assert!(
        closed[0].contains(old) && closed[1].contains(made),
        "{closed:?}"
    );
    match conn.request(&Request::GetChat { chat: old.into() }).await {
        Response::Error { message } => assert!(message.contains("not found"), "{message}"),
        other => panic!("expected an error, got {other:?}"),
    }

    // When each one is closed: from its latest turn, un-keep or creation.
    let closes: Vec<Option<u64>> = chats.iter().map(|c| c.closes_at).collect();
    assert_eq!(
        closes,
        [
            None,
            Some(ago(DAY) + 2 * DAY),
            Some(ago(3600) + 2 * DAY),
            Some(ago(3600) + 2 * DAY),
        ]
    );

    // Turned off in the config: at the next pass, no chat closes any more.
    write_config(dir.parent().unwrap(), "[chat]\nautoclean = false\n");
    for chat in [turned, unkept, new] {
        events.updated(chat, |c| c.closes_at.is_none()).await;
    }
    assert!(conn.chats().await.iter().all(|c| c.closes_at.is_none()));

    conn.ok(&Request::Shutdown).await;
}

/// On the timer: a chat past its time is closed only once its agent is at
/// the prompt with nobody typing in it; an answer is a turn.
async fn on_the_timer_once_at_the_prompt(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let bin = dir.join("bin");
    fake_claude(&bin);
    let env = AgentEnv(BTreeMap::from([(
        "PATH".to_string(),
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
    )]));
    let api = dir.join("src").join("api");
    std::fs::create_dir_all(&api).unwrap();
    git(&api, &["init", "-q"]);
    git(&api, &["commit", "-q", "--allow-empty", "-m", "init"]);

    let sock = dir.join("tam.sock");
    let ledger = dir.join("ledger.jsonl");
    // One second without a turn, whatever the config says.
    let daemon = Daemon::new(sock.clone(), ledger.clone())
        .close_chats_after(Some(Duration::from_secs(1)))
        .autoclean_every(Duration::from_millis(100));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    wait_for_socket(&sock).await;
    let mut conn = Conn::connect(&sock).await;
    let mut events = Conn::connect(&sock).await;
    events.ok(&Request::Subscribe).await;

    // Kept while its agent starts: a second goes by fast.
    let chat = conn
        .chat(&Request::CreateChat {
            dir: api.clone(),
            name: Some("deploy".into()),
            session: None,
        })
        .await;
    let id = chat.id.clone();
    assert_eq!(chat.closes_at, Some(chat.created_at + 1));
    let kept = conn
        .chat(&Request::KeepChat {
            chat: id.clone(),
            keep: true,
        })
        .await;
    assert_eq!(kept.closes_at, None);
    match conn
        .request(&Request::RunAgent {
            member: id.clone(),
            provider: None,
            session: SessionChoice::Current,
            args: Vec::new(),
            prompt: None,
            env,
            wrapper: Vec::new(),
        })
        .await
    {
        Response::Running { id: running, .. } => assert_eq!(running, id),
        other => panic!("expected Running, got {other:?}"),
    }

    // A prompt, then a permission request it waits on.
    conn.hook(&id, "UserPromptSubmit", json!({ "prompt": "deploy it" }))
        .await;
    events.updated(&id, |c| c.last_turn_at.is_some()).await;
    let bash = json!({ "tool_name": "Bash", "tool_input": { "command": "make deploy" } });
    conn.hook(&id, "PermissionRequest", bash.clone()).await;
    let unkept = conn
        .chat(&Request::KeepChat {
            chat: id.clone(),
            keep: false,
        })
        .await;
    assert!(unkept.closes_at.unwrap() > unkept.last_turn_at.unwrap());
    let still_open = |chats: Vec<ChatInfo>| chats.iter().any(|c| c.id == id);

    // Blocked past its time: kept open.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(still_open(conn.chats().await), "closed while blocked");

    // The answer is a turn: its clock starts again.
    let turns = lines_of(&ledger, "chat_turn").len();
    events.chats().await;
    events.events.clear();
    conn.hook(&id, "PostToolUse", bash.clone()).await;
    let answered = events.updated(&id, |_| true).await;
    assert!(answered.last_turn_at >= unkept.last_turn_at);
    assert_eq!(answered.closes_at, Some(answered.last_turn_at.unwrap() + 1));
    assert_eq!(lines_of(&ledger, "chat_turn").len(), turns + 1);

    // Working past its time: kept open.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(still_open(conn.chats().await), "closed while working");

    // A question past its time: kept open.
    let question = json!({ "tool_name": "AskUserQuestion", "tool_input": { "questions": [] } });
    conn.hook(&id, "PreToolUse", question.clone()).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(still_open(conn.chats().await), "closed while asking");

    // At the prompt, someone typing in it: kept open.
    let mut typing = Conn::connect(&sock).await;
    typing
        .ok(&Request::WatchScreen {
            id: Some(id.clone()),
            viewing: true,
        })
        .await;
    conn.hook(&id, "PostToolUse", question).await;
    conn.hook(&id, "Stop", json!({ "last_assistant_message": "deployed" }))
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        still_open(conn.chats().await),
        "closed under someone typing"
    );

    // Nobody there any more: closed at the next pass, as by hand: its agent
    // stopped, then the chat.
    drop(typing);
    events
        .event(|e| matches!(e, Event::AgentExited { id: of, .. } if *of == id).then_some(()))
        .await;
    events
        .event(|e| matches!(e, Event::ChatClosed { id: of } if *of == id).then_some(()))
        .await;
    assert!(conn.chats().await.is_empty());
    assert_eq!(lines_of(&ledger, "chat_closed").len(), 1);
    match conn.request(&Request::List).await {
        Response::Agents { agents } => assert!(agents.is_empty(), "{agents:?}"),
        other => panic!("expected Agents, got {other:?}"),
    }

    conn.ok(&Request::Shutdown).await;
}
