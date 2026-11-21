//! Chats through a daemon: made on a repository's main checkout, listed in
//! creation order, renamed, kept, closed; their agents run there, several
//! on one repository at once; a restart keeps them; closing one leaves the
//! checkout exactly as it was; a chat's agent messages task members.
//!
//! The agents are plain programs (`sleep`): no hooks. What needs Claude's
//! hooks (resume, turns, answers) is in `chat_claude.rs`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tam_daemon::daemon::Daemon;
use tam_proto::{AgentInfo, ChatInfo, Event, NewMember, Request, Response, ServerMessage};
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

    async fn agents(&mut self) -> Vec<AgentInfo> {
        match self.request(&Request::List).await {
            Response::Agents { agents } => agents,
            other => panic!("expected Agents, got {other:?}"),
        }
    }

    /// Wait for events until `pred` takes one; returns it.
    async fn event(&mut self, pred: impl Fn(&Event) -> bool) -> Event {
        if let Some(i) = self.events.iter().position(&pred) {
            return self.events.remove(i);
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(ServerMessage::Event(event)) = self.read().await {
                    if pred(&event) {
                        return event;
                    }
                }
            }
        })
        .await
        .expect("no such event")
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

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repository `name` under `root` with one commit; its main checkout,
/// resolved as the daemon records it.
fn repo(root: &Path, name: &str) -> PathBuf {
    let dir = root.join("src").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("README"), "hello\n").unwrap();
    git(&dir, &["add", "README"]);
    git(&dir, &["commit", "-q", "-m", "init"]);
    dir.canonicalize().unwrap()
}

fn create(dir: &Path, name: Option<&str>) -> Request {
    Request::CreateChat {
        dir: dir.to_path_buf(),
        name: name.map(str::to_string),
        session: None,
    }
}

/// Start the chat's agent as a plain program that waits.
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
async fn chats_are_made_listed_renamed_kept_and_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let sock = root.join("tam.sock");
    start_daemon(&sock, &root.join("ledger.jsonl")).await;
    let mut conn = Conn::connect(&sock).await;
    let mut events = Conn::connect(&sock).await;
    assert!(matches!(
        events.request(&Request::Subscribe).await,
        Response::Ok
    ));
    let api = repo(root, "api");

    let first = conn.chat(&create(&api, Some("debug"))).await;
    assert!(first.id.starts_with("c-"), "{}", first.id);
    assert_eq!(first.name.as_deref(), Some("debug"));
    assert_eq!(first.repo, "api");
    assert_eq!(first.dir, api);
    assert!(!first.keep && first.session.is_none() && first.last_turn_at.is_none());
    let second = conn.chat(&create(&api, None)).await;
    let third = conn.chat(&create(&api, Some("design"))).await;
    for chat in [&first, &second, &third] {
        let Event::ChatCreated { chat: told } = events
            .event(|e| matches!(e, Event::ChatCreated { chat: c } if c.id == chat.id))
            .await
        else {
            unreachable!()
        };
        assert_eq!(&told, chat);
    }

    // Renamed and kept, by id or name; clients are told.
    let renamed = conn
        .chat(&Request::RenameChat {
            chat: second.id.clone(),
            name: Some("deploy".into()),
        })
        .await;
    assert_eq!(renamed.name.as_deref(), Some("deploy"));
    events
        .event(
            |e| matches!(e, Event::ChatUpdated { chat } if chat.name.as_deref() == Some("deploy")),
        )
        .await;
    let kept = conn
        .chat(&Request::KeepChat {
            chat: "deploy".into(),
            keep: true,
        })
        .await;
    assert!(kept.keep);
    events
        .event(|e| matches!(e, Event::ChatUpdated { chat } if chat.id == second.id && chat.keep))
        .await;
    let got = conn
        .chat(&Request::GetChat {
            chat: "deploy".into(),
        })
        .await;
    assert_eq!(got, kept);

    // Listed in creation order, never as tasks.
    let ids: Vec<String> = conn.chats().await.into_iter().map(|c| c.id).collect();
    assert_eq!(ids, [first.id.clone(), second.id.clone(), third.id.clone()]);
    assert!(matches!(
        conn.request(&Request::ListTasks).await,
        Response::Tasks { tasks } if tasks.is_empty()
    ));

    // Refused: a name taken, a linked worktree, a directory inside the
    // checkout, no repository, a relative path, an unknown chat.
    let why = conn.refused(&create(&api, Some("debug"))).await;
    assert!(why.contains("already"), "{why}");
    let linked = root.join("w").join("api--feat");
    git(
        &api,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            linked.to_str().unwrap(),
        ],
    );
    let inside = api.join("sub");
    std::fs::create_dir_all(&inside).unwrap();
    let plain = root.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    for dir in [&linked, &inside, &plain] {
        let why = conn.refused(&create(dir, None)).await;
        assert!(!why.is_empty(), "{}", dir.display());
    }
    let why = conn.refused(&create(Path::new("src/api"), None)).await;
    assert!(why.contains("absolute"), "{why}");
    let why = conn
        .refused(&Request::RenameChat {
            chat: "nope".into(),
            name: None,
        })
        .await;
    assert!(why.contains("not found"), "{why}");
    assert_eq!(conn.chats().await.len(), 3);

    // Closed: gone from the list, told to clients, not found any more.
    let closed = conn
        .chat(&Request::CloseChat {
            chat: "debug".into(),
        })
        .await;
    assert_eq!(closed.id, first.id);
    events
        .event(|e| matches!(e, Event::ChatClosed { id } if *id == first.id))
        .await;
    let ids: Vec<String> = conn.chats().await.into_iter().map(|c| c.id).collect();
    assert_eq!(ids, [second.id.clone(), third.id.clone()]);
    let why = conn
        .refused(&Request::GetChat {
            chat: first.id.clone(),
        })
        .await;
    assert!(why.contains("not found"), "{why}");
    // Its name is free again.
    conn.chat(&create(&api, Some("debug"))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_chats_run_at_once_on_one_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let sock = root.join("tam.sock");
    start_daemon(&sock, &root.join("ledger.jsonl")).await;
    let mut conn = Conn::connect(&sock).await;
    let mut events = Conn::connect(&sock).await;
    events.request(&Request::Subscribe).await;
    let api = repo(root, "api");
    let one = conn.chat(&create(&api, None)).await;
    let two = conn.chat(&create(&api, None)).await;

    for chat in [&one, &two] {
        match conn.request(&run(&chat.id)).await {
            Response::Running { id, resumed } => {
                assert_eq!(id, chat.id);
                assert_eq!(resumed, None);
            }
            other => panic!("expected Running, got {other:?}"),
        }
        events
            .event(|e| matches!(e, Event::ChatUpdated { chat: c } if c.id == chat.id && c.run_count == 1))
            .await;
    }
    // Both run, in the repository's main checkout, under the chat ids, in
    // no task.
    let agents = conn.agents().await;
    for chat in [&one, &two] {
        let agent = agents.iter().find(|a| a.id == chat.id).expect("running");
        assert_eq!(agent.dir, api);
        assert_eq!(agent.task, None);
    }
    let why = conn.refused(&run(&one.id)).await;
    assert!(why.contains("already has a running agent"), "{why}");
    // A chat is no task member.
    let why = conn
        .refused(&Request::ResolveMember {
            target: Some(one.id.clone()),
            dir: None,
            from: None,
        })
        .await;
    assert!(why.contains("is a chat"), "{why}");
    let why = conn
        .refused(&Request::ResolveMember {
            target: None,
            dir: Some(api.clone()),
            from: None,
        })
        .await;
    assert!(why.contains("no task member"), "{why}");

    // Stopping one leaves the other running; its run is recorded.
    assert!(matches!(
        conn.request(&Request::Kill { id: one.id.clone() }).await,
        Response::Ok
    ));
    let runs = match conn
        .request(&Request::GetChat {
            chat: one.id.clone(),
        })
        .await
    {
        Response::Chat { runs, .. } => runs,
        other => panic!("expected Chat, got {other:?}"),
    };
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].member, one.id);
    assert_eq!(runs[0].exit_code, Some(-1));
    let agents = conn.agents().await;
    assert!(!agents.iter().any(|a| a.id == one.id));
    assert!(agents.iter().any(|a| a.id == two.id));
    // And it starts again.
    assert!(matches!(
        conn.request(&run(&one.id)).await,
        Response::Running { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_keeps_chats() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let sock = root.join("tam.sock");
    let ledger = root.join("ledger.jsonl");
    start_daemon(&sock, &ledger).await;
    let mut conn = Conn::connect(&sock).await;
    let api = repo(root, "api");
    let web = repo(root, "web");
    let one = conn.chat(&create(&web, None)).await;
    let two = conn.chat(&create(&api, Some("debug"))).await;
    let three = conn.chat(&create(&api, None)).await;
    conn.request(&Request::RenameChat {
        chat: one.id.clone(),
        name: Some("design".into()),
    })
    .await;
    conn.request(&Request::KeepChat {
        chat: two.id.clone(),
        keep: true,
    })
    .await;
    assert!(matches!(
        conn.request(&run(&three.id)).await,
        Response::Running { .. }
    ));
    let before = conn.chats().await;

    assert!(matches!(
        conn.request(&Request::Shutdown).await,
        Response::Ok
    ));
    drop(conn);
    wait_ledger_released(&ledger).await;
    start_daemon(&sock, &ledger).await;
    let mut conn = Conn::connect(&sock).await;

    let after = conn.chats().await;
    assert_eq!(after, before);
    assert_eq!(after[0].name.as_deref(), Some("design"));
    assert!(after[1].keep);
    // The agent did not survive its daemon: its run ended with it.
    assert!(conn.agents().await.is_empty());
    match conn
        .request(&Request::GetChat {
            chat: three.id.clone(),
        })
        .await
    {
        Response::Chat { runs, .. } => {
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].exit_code, Some(-1));
        }
        other => panic!("expected Chat, got {other:?}"),
    }
}

/// What git says of a checkout, and its files.
fn snapshot(dir: &Path) -> Vec<String> {
    let mut out = vec![
        git(dir, &["status", "--porcelain=v1", "-uall"]),
        git(dir, &["rev-parse", "HEAD"]),
        git(dir, &["branch", "--show-current"]),
        git(dir, &["for-each-ref"]),
        git(dir, &["worktree", "list", "--porcelain"]),
        git(dir, &["diff", "--cached"]),
        git(dir, &["diff"]),
    ];
    for file in ["README", "staged.txt", "untracked.txt"] {
        out.push(std::fs::read_to_string(dir.join(file)).unwrap());
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_a_chat_leaves_its_checkout_as_it_was() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let sock = root.join("tam.sock");
    start_daemon(&sock, &root.join("ledger.jsonl")).await;
    let mut conn = Conn::connect(&sock).await;
    let api = repo(root, "api");
    // On a branch of its own, with work in every state.
    git(&api, &["switch", "-q", "-c", "work"]);
    std::fs::write(api.join("README"), "hello, modified\n").unwrap();
    std::fs::write(api.join("staged.txt"), "staged\n").unwrap();
    git(&api, &["add", "staged.txt"]);
    std::fs::write(api.join("untracked.txt"), "untracked\n").unwrap();
    let before = snapshot(&api);

    let chat = conn.chat(&create(&api, Some("debug"))).await;
    assert!(matches!(
        conn.request(&run(&chat.id)).await,
        Response::Running { .. }
    ));
    let closed = conn
        .chat(&Request::CloseChat {
            chat: chat.id.clone(),
        })
        .await;
    assert_eq!(closed.run_count, 1);
    // Its agent is gone when the reply comes.
    assert!(conn.agents().await.is_empty());
    assert!(conn.chats().await.is_empty());
    assert_eq!(snapshot(&api), before);
    assert_eq!(git(&api, &["branch", "--show-current"]).trim(), "work");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chat_messages_task_members_and_gets_their_replies() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let sock = root.join("tam.sock");
    start_daemon(&sock, &root.join("ledger.jsonl")).await;
    let mut conn = Conn::connect(&sock).await;
    let api = repo(root, "api");
    let worktree = root.join("w").join("api--feat");
    std::fs::create_dir_all(&worktree).unwrap();
    let member = match conn
        .request(&Request::CreateTask {
            name: "feat".into(),
            members: vec![NewMember {
                repo_root: api.clone(),
                branch: "feat".into(),
                dir: worktree,
            }],
        })
        .await
    {
        Response::Task { task, .. } => task.members[0].id.clone(),
        other => panic!("expected Task, got {other:?}"),
    };
    let chat = conn.chat(&create(&api, Some("debug"))).await;
    assert!(matches!(
        conn.request(&run(&chat.id)).await,
        Response::Running { .. }
    ));

    // From the chat's agent ($TAM_AGENT_ID), names resolve as from anywhere.
    match conn
        .request(&Request::ResolveMember {
            target: Some("feat".into()),
            dir: None,
            from: Some(chat.id.clone()),
        })
        .await
    {
        Response::Member { member: found, .. } => assert_eq!(found.id, member),
        other => panic!("expected Member, got {other:?}"),
    }
    let asked = match conn
        .request(&Request::SendMessage {
            to: "feat".into(),
            text: "status?".into(),
            from: Some(chat.id.clone()),
        })
        .await
    {
        Response::Sent { message, .. } => *message,
        other => panic!("expected Sent, got {other:?}"),
    };
    assert_eq!(asked.from.as_deref(), Some(chat.id.as_str()));
    assert_eq!(asked.from_label, chat.id);
    assert_eq!(asked.to, member);

    // The member answers to the label it was given: it reaches the chat.
    let reply = match conn
        .request(&Request::SendMessage {
            to: asked.from_label.clone(),
            text: "done".into(),
            from: Some(member.clone()),
        })
        .await
    {
        Response::Sent { message, pending } => {
            // The chat's agent takes no messages (no hooks): its next does.
            assert!(pending.is_some());
            *message
        }
        other => panic!("expected Sent, got {other:?}"),
    };
    assert_eq!(reply.to, chat.id);
    assert_eq!(reply.reply_to.as_deref(), Some(asked.id.as_str()));
    // The user reaches it by name too.
    match conn
        .request(&Request::SendMessage {
            to: "debug".into(),
            text: "hi".into(),
            from: None,
        })
        .await
    {
        Response::Sent { message, .. } => assert_eq!(message.to, chat.id),
        other => panic!("expected Sent, got {other:?}"),
    }
    let listed = |resp: Response| match resp {
        Response::Messages { messages } => messages.into_iter().map(|m| m.id).collect::<Vec<_>>(),
        other => panic!("expected Messages, got {other:?}"),
    };
    let of_chat = listed(
        conn.request(&Request::ListMessages {
            task: Some("debug".into()),
        })
        .await,
    );
    assert_eq!(of_chat.len(), 3);
    assert_eq!(&of_chat[..2], [asked.id.clone(), reply.id.clone()]);
    let of_task = listed(
        conn.request(&Request::ListMessages {
            task: Some("feat".into()),
        })
        .await,
    );
    assert_eq!(of_task, [asked.id.clone(), reply.id.clone()]);

    // Closed, its messages go with it; the one it sent stays.
    conn.chat(&Request::CloseChat {
        chat: chat.id.clone(),
    })
    .await;
    let left = listed(conn.request(&Request::ListMessages { task: None }).await);
    assert_eq!(left, [asked.id]);
}
