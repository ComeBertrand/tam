//! Chats and Claude's session files, through a daemon: a repository's
//! history (newest first, the user's name for a session before Claude's
//! title, a session nobody typed in and one a chat holds left out), a chat
//! made from a session of that repository only, its title in its info and
//! label, read again as Claude rewrites it (and gone while its agent runs a
//! new session), there at once after a restart.
//!
//! The session files are written here by hand, under a `CLAUDE_CONFIG_DIR`
//! of the test's: kept to a single test, since it sets process environment
//! (with `HOME` and `XDG_CONFIG_HOME`: no real file is read).

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tam_daemon::daemon::Daemon;
use tam_daemon::sessions;
use tam_proto::{ChatInfo, Event, Request, Response, ServerMessage, SessionInfo};
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

    async fn chat(&mut self, request: &Request) -> ChatInfo {
        match self.request(request).await {
            Response::Chat { chat, .. } => *chat,
            other => panic!("expected Chat for {request:?}, got {other:?}"),
        }
    }

    async fn refused(&mut self, request: &Request) -> String {
        match self.request(request).await {
            Response::Error { message } => message,
            other => panic!("expected Error for {request:?}, got {other:?}"),
        }
    }

    async fn chats(&mut self) -> Vec<ChatInfo> {
        match self.request(&Request::ListChats).await {
            Response::Chats { chats } => chats,
            other => panic!("expected Chats, got {other:?}"),
        }
    }

    async fn history(&mut self, dir: &Path) -> Vec<SessionInfo> {
        let request = Request::ChatHistory {
            dir: dir.to_path_buf(),
        };
        match self.request(&request).await {
            Response::Sessions { sessions } => sessions,
            other => panic!("expected Sessions, got {other:?}"),
        }
    }

    /// The next `ChatUpdated` of chat `id` whose title is `title`.
    async fn titled(&mut self, id: &str, title: Option<&str>) -> ChatInfo {
        let wanted = |event: &Event| {
            matches!(event, Event::ChatUpdated { chat }
                if chat.id == id && chat.title.as_deref() == title)
        };
        if let Some(i) = self.events.iter().position(wanted) {
            self.events.remove(i);
        } else {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(ServerMessage::Event(event)) = self.read().await {
                        if wanted(&event) {
                            return;
                        }
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("chat {id} never titled {title:?}"));
        }
        self.chats().await.into_iter().find(|c| c.id == id).unwrap()
    }
}

async fn start_daemon(sock: &Path, ledger: &Path, chat_sessions_every: Duration) {
    let daemon = Daemon::new(sock.to_path_buf(), ledger.to_path_buf())
        .reconcile_every(Duration::from_millis(200))
        .chat_sessions_every(chat_sessions_every);
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

/// A repository `name` under `root` with one commit; its main checkout,
/// resolved, as Claude names its project.
fn repo(root: &Path, name: &str) -> PathBuf {
    let dir = root.join("src").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["commit", "-q", "--allow-empty", "-m", "init"]);
    dir.canonicalize().unwrap()
}

/// Lines of a session file, as Claude writes them.
fn ai(session: &str, title: &str) -> String {
    serde_json::json!({"type": "ai-title", "aiTitle": title, "sessionId": session}).to_string()
}

fn custom(session: &str, title: &str) -> String {
    serde_json::json!({"type": "custom-title", "customTitle": title, "sessionId": session})
        .to_string()
}

fn prompt(session: &str, text: &str) -> String {
    serde_json::json!({
        "type": "last-prompt",
        "lastPrompt": text,
        "leafUuid": "00000000-0000-4000-8000-0000000000aa",
        "sessionId": session,
    })
    .to_string()
}

fn mode(session: &str) -> String {
    serde_json::json!({"type": "permission-mode", "permissionMode": "default", "sessionId": session})
        .to_string()
}

/// Append `lines` to the file of `session` run in `dir`, last written at
/// `at` (Unix seconds).
fn session(claude: &Path, dir: &Path, session: &str, lines: &[String], at: u64) {
    let file = sessions::session_file(claude, dir, session).unwrap();
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let mut f = File::options()
        .create(true)
        .append(true)
        .open(&file)
        .unwrap();
    for line in lines {
        writeln!(f, "{line}").unwrap();
    }
    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(at))
        .unwrap();
}

fn ids(sessions: &[SessionInfo]) -> Vec<&str> {
    sessions.iter().map(|s| s.id.as_str()).collect()
}

/// Start the chat's agent as a plain program: it resumes no session.
fn run(chat: &str) -> Request {
    Request::RunAgent {
        member: chat.into(),
        provider: Some("sleep".into()),
        session: Default::default(),
        args: vec!["3600".into()],
        prompt: None,
        env: Default::default(),
        wrapper: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chats_take_titles_and_sessions_from_claudes_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let claude = root.join("claude");
    std::env::set_var("CLAUDE_CONFIG_DIR", &claude);
    std::env::set_var("HOME", root.join("home"));
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    let api = repo(root, "api");
    let web = repo(root, "web");

    session(
        &claude,
        &api,
        "s-old",
        &[prompt("s-old", "first try"), ai("s-old", "Old work")],
        1_000,
    );
    session(
        &claude,
        &api,
        "s-held",
        &[
            prompt("s-held", "find the leak"),
            ai("s-held", "Investigate a leak"),
        ],
        2_000,
    );
    // Named in Claude: the name wins over the title Claude generated.
    session(
        &claude,
        &api,
        "s-new",
        &[
            prompt("s-new", "ship it"),
            ai("s-new", "Deploy preparation"),
            custom("s-new", "friday deploy"),
            ai("s-new", "Deploy preparation, again"),
        ],
        3_000,
    );
    // Nobody typed in it: nothing to list.
    session(&claude, &api, "s-blank", &[mode("s-blank")], 4_000);
    // Another repository's.
    session(&claude, &web, "s-web", &[prompt("s-web", "web")], 5_000);

    let sock = root.join("tam.sock");
    let ledger = root.join("ledger.jsonl");
    start_daemon(&sock, &ledger, Duration::from_millis(100)).await;
    let mut conn = Conn::connect(&sock).await;
    let mut events = Conn::connect(&sock).await;
    events.request(&Request::Subscribe).await;

    // The history, newest first.
    let history = conn.history(&api).await;
    assert_eq!(ids(&history), ["s-new", "s-held", "s-old"]);
    assert_eq!(
        history[0],
        SessionInfo {
            id: "s-new".into(),
            title: Some("friday deploy".into()),
            last_prompt: Some("ship it".into()),
            modified_at: 3_000,
        }
    );
    assert_eq!(ids(&conn.history(&web).await), ["s-web"]);
    // A repository's main checkout only.
    let message = conn
        .refused(&Request::ChatHistory {
            dir: root.join("nowhere"),
        })
        .await;
    assert!(!message.is_empty());

    // Made from a session: its title at once, and as its label.
    let chat = conn
        .chat(&Request::CreateChat {
            dir: api.clone(),
            name: None,
            session: Some("s-held".into()),
        })
        .await;
    let id = chat.id.clone();
    assert_eq!(chat.title.as_deref(), Some("Investigate a leak"));
    assert_eq!(chat.last_prompt.as_deref(), Some("find the leak"));
    assert_eq!(chat.label(), "Investigate a leak");
    match events.request(&Request::GetChat { chat: id.clone() }).await {
        Response::Chat { chat, .. } => assert_eq!(chat.title, Some("Investigate a leak".into())),
        other => panic!("expected Chat, got {other:?}"),
    }
    // A session a chat holds is no longer offered.
    assert_eq!(ids(&conn.history(&api).await), ["s-new", "s-old"]);

    // Only a session of that repository.
    for (dir, missing) in [(&api, "s-nope"), (&api, "s-web"), (&web, "s-old")] {
        let message = conn
            .refused(&Request::CreateChat {
                dir: dir.to_path_buf(),
                name: None,
                session: Some(missing.into()),
            })
            .await;
        assert!(message.contains("no session"), "{message}");
        assert!(message.contains(missing), "{message}");
    }
    assert_eq!(conn.chats().await.len(), 1);

    // Claude rewrites its title as the conversation moves: read again.
    session(
        &claude,
        &api,
        "s-held",
        &[ai("s-held", "Fix the pool leak")],
        6_000,
    );
    let chat = events.titled(&id, Some("Fix the pool leak")).await;
    assert_eq!(chat.label(), "Fix the pool leak");
    // The user names it in Claude: that wins over Claude's title.
    session(
        &claude,
        &api,
        "s-held",
        &[
            custom("s-held", "pool"),
            prompt("s-held", "now the tests"),
            ai("s-held", "Fix the pool leak and its tests"),
        ],
        7_000,
    );
    let chat = events.titled(&id, Some("pool")).await;
    assert_eq!(chat.last_prompt.as_deref(), Some("now the tests"));
    // Named in tam: that name is its label.
    let named = conn
        .chat(&Request::RenameChat {
            chat: id.clone(),
            name: Some("mine".into()),
        })
        .await;
    assert_eq!(
        (named.label().as_str(), named.title.as_deref()),
        ("mine", Some("pool"))
    );
    conn.chat(&Request::RenameChat {
        chat: id.clone(),
        name: None,
    })
    .await;

    // Its agent in a new session (a plain program resumes none): not that
    // title any more; back when the agent is gone.
    match conn.request(&run(&id)).await {
        Response::Running { resumed, .. } => assert_eq!(resumed, None),
        other => panic!("expected Running, got {other:?}"),
    }
    let chat = events.titled(&id, None).await;
    assert_eq!(chat.label(), id);
    assert!(matches!(
        conn.request(&Request::Kill { id: id.clone() }).await,
        Response::Ok
    ));
    events.titled(&id, Some("pool")).await;

    // A restart: there at once, before the first periodic read.
    conn.request(&Request::Shutdown).await;
    drop((conn, events));
    wait_ledger_released(&ledger).await;
    start_daemon(&sock, &ledger, Duration::from_secs(3600)).await;
    let mut conn = Conn::connect(&sock).await;
    let chats = conn.chats().await;
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].title.as_deref(), Some("pool"));
    assert_eq!(chats[0].label(), "pool");

    // Closed, its session is offered again.
    let closed = conn.chat(&Request::CloseChat { chat: id.clone() }).await;
    assert_eq!(closed.title.as_deref(), Some("pool"));
    assert_eq!(ids(&conn.history(&api).await), ["s-held", "s-new", "s-old"]);
    conn.request(&Request::Shutdown).await;
}
