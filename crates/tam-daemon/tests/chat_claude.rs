//! A chat's Claude agent, through a daemon: made from an existing session,
//! it resumes it, with tam's hooks but no start note and no task access;
//! the user's prompts are its turns (a tam message or a background
//! notification is none); `[notify]` names it by its name and id; messages
//! reach it through its inbox hook; a client answers its permission
//! requests; a restart keeps its session and last turn; closing it keeps
//! Claude's transcript, and the chat can be made again from that session.
//!
//! The agent is a fake `claude` (it writes its arguments, then sleeps),
//! found on the `PATH` sent with `RunAgent`; its hooks are played as `tam
//! hook-notify` plays them. Kept to a single test: it sets process
//! environment (HOME, XDG_CONFIG_HOME, CLAUDE_CONFIG_DIR: the test's notify
//! command, no real Claude files), which must not race other tests in this
//! binary.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};
use tam_daemon::daemon::Daemon;
use tam_proto::{
    AgentEnv, AgentInfo, AskAnswer, ChatInfo, Event, Request, Response, ServerMessage,
    SessionChoice,
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

    async fn chat(&mut self, request: &Request) -> ChatInfo {
        match self.request(request).await {
            Response::Chat { chat, .. } => *chat,
            other => panic!("expected Chat for {request:?}, got {other:?}"),
        }
    }

    async fn hook(&mut self, agent: &str, event: &str, extra: Value) -> Response {
        self.request(&hook_event(agent, event, extra)).await
    }

    /// Wait for events until `pred` takes one; returns it.
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

    async fn updated(&mut self, id: &str, pred: impl Fn(&AgentInfo) -> bool) -> AgentInfo {
        self.event(|e| match e {
            Event::AgentUpdated { id: of, info } if of == id && pred(info) => Some(info.clone()),
            _ => None,
        })
        .await
    }
}

/// The session the fake transcript is for.
const SESSION: &str = "0b7d4c1e-5a1f-4c37-9d0e-6f2a8b9c0d11";

fn hook_event(agent: &str, event: &str, extra: Value) -> Request {
    let mut payload = json!({ "hook_event_name": event, "session_id": SESSION });
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

async fn start_daemon(sock: &Path, ledger: &Path) {
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

/// A fake `claude` writing its arguments to `<args>/<agent id>`, a line
/// each, then waiting.
fn fake_claude(bin: &Path, args: &Path) {
    std::fs::create_dir_all(bin).unwrap();
    std::fs::create_dir_all(args).unwrap();
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{a}'/\"$TAM_AGENT_ID.tmp\"\nmv '{a}'/\"$TAM_AGENT_ID.tmp\" '{a}'/\"$TAM_AGENT_ID\"\nexec sleep 3600\n",
        a = args.display()
    );
    let fake = bin.join("claude");
    std::fs::write(&fake, script).unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

/// The arguments the fake `claude` of agent `id` was started with (waits
/// for them; a previous start's are removed by the caller).
async fn started_with(args: &Path, id: &str) -> Vec<String> {
    let file = args.join(id);
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(&file) {
            return text.lines().map(str::to_string).collect();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the agent of {id} did not start");
}

/// Start the chat's agent as a client does: Claude, its current session,
/// the client's environment.
fn run(chat: &str, env: &AgentEnv) -> Request {
    Request::RunAgent {
        member: chat.into(),
        provider: None,
        session: SessionChoice::Current,
        args: Vec::new(),
        prompt: None,
        env: env.clone(),
        wrapper: Vec::new(),
    }
}

/// How many turns of the chat the ledger file holds now.
fn turns_recorded(ledger: &Path) -> usize {
    std::fs::read_to_string(ledger)
        .unwrap()
        .lines()
        .filter(|l| l.contains(r#""type":"chat_turn""#))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chats_claude_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::env::set_var("HOME", root.join("home"));
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    // A notify command writing what it was given, in the config written
    // where tam reads it: `$XDG_CONFIG_HOME`, or on macOS `~/Library/
    // Application Support`.
    let notified = root.join("notified");
    let config = format!(
        "[notify]\ncommand = \"echo {{task}} {{id}} {{kind}} >> '{}'\"\non_states = [\"ready\"]\n",
        notified.display()
    );
    let macos = root
        .join("home")
        .join("Library")
        .join("Application Support");
    for dir in [root.join("config"), macos] {
        let dir = dir.join("tam");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), &config).unwrap();
    }
    let claude_dir = root.join("claude");
    std::env::set_var("CLAUDE_CONFIG_DIR", &claude_dir);
    let transcript = claude_dir
        .join("projects")
        .join("-src-api")
        .join(format!("{SESSION}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{}\n").unwrap();
    let bin = root.join("bin");
    let args = root.join("args");
    fake_claude(&bin, &args);
    let env = AgentEnv(BTreeMap::from([
        (
            "PATH".to_string(),
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        ),
        ("HOME".to_string(), root.join("home").display().to_string()),
    ]));

    let api = root.join("src").join("api");
    std::fs::create_dir_all(&api).unwrap();
    git(&api, &["init", "-q"]);
    git(&api, &["commit", "-q", "--allow-empty", "-m", "init"]);

    let sock = root.join("tam.sock");
    let ledger = root.join("ledger.jsonl");
    start_daemon(&sock, &ledger).await;
    let mut conn = Conn::connect(&sock).await;
    let mut events = Conn::connect(&sock).await;
    events.request(&Request::Subscribe).await;

    // Made from an existing session: its first agent resumes it.
    let chat = conn
        .chat(&Request::CreateChat {
            dir: api.clone(),
            name: Some("debug".into()),
            session: Some(SESSION.into()),
        })
        .await;
    assert_eq!(chat.session.as_deref(), Some(SESSION));
    let id = chat.id.clone();
    match conn.request(&run(&id, &env)).await {
        Response::Running {
            id: running,
            resumed,
        } => {
            assert_eq!(running, id);
            assert_eq!(resumed.as_deref(), Some(SESSION));
        }
        other => panic!("expected Running, got {other:?}"),
    }
    let started = started_with(&args, &id).await;
    let at = started
        .iter()
        .position(|a| a == "--resume")
        .expect("resumed");
    assert_eq!(started[at + 1], SESSION);
    // tam's hooks, and nothing of a task's: no extra directory.
    let at = started
        .iter()
        .position(|a| a == "--settings")
        .expect("hooks");
    let settings: Value = serde_json::from_str(&started[at + 1]).unwrap();
    assert!(settings["hooks"]["SessionStart"].is_array(), "{settings}");
    assert!(settings.get("permissions").is_none(), "{settings}");

    // No start note.
    let resp = conn
        .hook(&id, "SessionStart", json!({ "source": "resume" }))
        .await;
    assert!(matches!(resp, Response::Ok), "{resp:?}");

    // The user's prompt is a turn; clients are told.
    assert_eq!(turns_recorded(&ledger), 0);
    let resp = conn
        .hook(
            &id,
            "UserPromptSubmit",
            json!({ "prompt": "why is CI red?" }),
        )
        .await;
    assert!(matches!(resp, Response::Ok), "{resp:?}");
    let turned = events
        .event(|e| match e {
            Event::ChatUpdated { chat } if chat.id == id && chat.last_turn_at.is_some() => {
                Some(chat.clone())
            }
            _ => None,
        })
        .await;
    assert_eq!(turned.session.as_deref(), Some(SESSION));
    assert_eq!(turns_recorded(&ledger), 1);
    conn.hook(
        &id,
        "Stop",
        json!({ "last_assistant_message": "a flaky test" }),
    )
    .await;
    // Notified as the chat: its name, its id.
    let line = format!("debug {id} done");
    let mut told = String::new();
    for _ in 0..50 {
        told = std::fs::read_to_string(&notified).unwrap_or_default();
        if !told.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(told.lines().next(), Some(line.as_str()), "{told}");
    // A background notification is no turn of the user's.
    conn.hook(
        &id,
        "UserPromptSubmit",
        json!({ "prompt": "<task-notification>build done</task-notification>" }),
    )
    .await;
    conn.hook(&id, "Stop", json!({})).await;
    assert_eq!(turns_recorded(&ledger), 1);

    // A message reaches it through its inbox hook; the prompt that shows
    // it delivers it, and is no turn of the user's either.
    let sent = match conn
        .request(&Request::SendMessage {
            to: "debug".into(),
            text: "the deploy is done".into(),
            from: None,
        })
        .await
    {
        Response::Sent { message, .. } => *message,
        other => panic!("expected Sent, got {other:?}"),
    };
    assert_eq!(sent.to, id);
    assert_eq!(sent.to_label, id);
    let mut inbox = Conn::connect(&sock).await;
    let handed = inbox
        .request(&Request::WaitInbox {
            agent_id: id.clone(),
        })
        .await;
    let Response::Inbox { text, ids } = handed else {
        panic!("expected Inbox, got {handed:?}");
    };
    assert_eq!(ids, std::slice::from_ref(&sent.id));
    conn.hook(&id, "UserPromptSubmit", json!({ "prompt": text }))
        .await;
    events
        .event(|e| match e {
            Event::MessageDelivered { message } if message.id == sent.id => Some(()),
            _ => None,
        })
        .await;
    conn.hook(&id, "Stop", json!({})).await;
    assert_eq!(turns_recorded(&ledger), 1);

    // A client answers its permission request, as for a task member's.
    let mut hook = Conn::connect(&sock).await;
    let bash = json!({ "tool_name": "Bash", "tool_input": { "command": "make deploy" } });
    let resp = hook.hook(&id, "PermissionRequest", bash.clone()).await;
    assert!(matches!(resp, Response::Ok), "{resp:?}");
    let Request::HookEvent {
        payload: Some(payload),
        ..
    } = hook_event(&id, "PermissionRequest", bash)
    else {
        unreachable!()
    };
    hook.write(&Request::WaitAnswer {
        agent_id: id.clone(),
        payload,
    })
    .await;
    let info = events.updated(&id, |i| i.detail.ask.is_some()).await;
    let ask = info.detail.ask.unwrap();
    assert_eq!(ask.tool, "Bash");
    let answered = conn
        .request(&Request::Answer {
            agent: id.clone(),
            ask: ask.id,
            answer: AskAnswer::Allow,
        })
        .await;
    assert!(matches!(answered, Response::Ok), "{answered:?}");
    let told = tokio::time::timeout(Duration::from_secs(5), hook.response())
        .await
        .expect("the hook is still waiting");
    let Response::HookOutput { output } = told else {
        panic!("expected HookOutput, got {told:?}");
    };
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        output["hookSpecificOutput"]["decision"]["behavior"],
        "allow"
    );

    // A restart keeps its session and last turn; its agent starts again
    // in that session.
    let before = match conn.request(&Request::ListChats).await {
        Response::Chats { chats } => chats,
        other => panic!("expected Chats, got {other:?}"),
    };
    assert!(matches!(
        conn.request(&Request::Shutdown).await,
        Response::Ok
    ));
    drop((conn, events, inbox, hook));
    wait_ledger_released(&ledger).await;
    start_daemon(&sock, &ledger).await;
    let mut conn = Conn::connect(&sock).await;
    let after = match conn.request(&Request::ListChats).await {
        Response::Chats { chats } => chats,
        other => panic!("expected Chats, got {other:?}"),
    };
    assert_eq!(after, before);
    assert_eq!(after[0].last_turn_at, turned.last_turn_at);
    std::fs::remove_file(args.join(&id)).unwrap();
    match conn.request(&run(&id, &env)).await {
        Response::Running { resumed, .. } => assert_eq!(resumed.as_deref(), Some(SESSION)),
        other => panic!("expected Running, got {other:?}"),
    }
    started_with(&args, &id).await;

    // Closed: Claude's transcript stays, and the chat can be made again
    // from its session, which no chat holds any more.
    let closed = conn
        .chat(&Request::CloseChat {
            chat: "debug".into(),
        })
        .await;
    assert_eq!(closed.session.as_deref(), Some(SESSION));
    assert!(transcript.is_file());
    let again = conn
        .chat(&Request::CreateChat {
            dir: api.clone(),
            name: None,
            session: closed.session.clone(),
        })
        .await;
    assert_ne!(again.id, id);
    match conn.request(&run(&again.id, &env)).await {
        Response::Running { resumed, .. } => assert_eq!(resumed.as_deref(), Some(SESSION)),
        other => panic!("expected Running, got {other:?}"),
    }
    let started = started_with(&args, &again.id).await;
    assert!(started.iter().any(|a| a == SESSION), "{started:?}");
    // While it holds that session, no other chat is made from it.
    let refused = conn
        .request(&Request::CreateChat {
            dir: api.clone(),
            name: None,
            session: Some(SESSION.into()),
        })
        .await;
    assert!(matches!(refused, Response::Error { .. }), "{refused:?}");

    // A session with no transcript is refused when the agent starts.
    let missing = conn
        .chat(&Request::CreateChat {
            dir: api.clone(),
            name: None,
            session: Some("no-such-session".into()),
        })
        .await;
    match conn.request(&run(&missing.id, &env)).await {
        Response::Error { message } => {
            assert!(message.contains("can no longer be resumed"), "{message}");
            assert!(!message.contains("tam run"), "{message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}
