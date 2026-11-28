//! Shells through a daemon (#81): the user's `$SHELL` on a PTY, in a task
//! member's worktree or a repository's main checkout, on real git
//! repositories. Numbered per place; their terminal goes through the
//! requests an agent's does; closing one hangs it up and leaves no job
//! behind; a drop closes a member's shells before removing its worktree;
//! a merge keeps them; the daemon stays up for them; they run in the
//! caller's environment and keep nothing of it.
//!
//! The shells are `sh`, or scripts standing for one; the test of jobs uses
//! bash, which hangs up its jobs as interactive shells do (dash, `sh` on
//! Debian, does not).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tam_daemon::daemon::Daemon;
use tam_proto::{
    AgentEnv, Event, NewMember, Request, Response, ServerMessage, ShellInfo, ShellPlace,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// A variable only the caller has, standing for a token: neither its name
/// nor its value may be found anywhere the daemon writes or answers.
const SECRET_NAME: &str = "TAM_SHELL_LEAK_CHECK_41C7";
const SECRET_VALUE: &str = "shell-leak-check-value-9d2e";

struct Conn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    events: Vec<Event>,
    /// Everything the daemon sent, as JSON lines.
    seen: Vec<String>,
}

impl Conn {
    async fn connect(sock: &Path) -> Self {
        let (reader, writer) = UnixStream::connect(sock).await.unwrap().into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
            events: Vec::new(),
            seen: Vec::new(),
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
            Ok(_) => {
                let msg = serde_json::from_str(line.trim()).unwrap();
                self.seen.push(line);
                Some(msg)
            }
        }
    }

    async fn send(&mut self, request: &Request) {
        let mut json = serde_json::to_string(request).unwrap();
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await.unwrap();
    }

    async fn request(&mut self, request: &Request) -> Response {
        self.send(request).await;
        loop {
            match self.read().await.expect("daemon closed the connection") {
                ServerMessage::Response(resp) => return resp,
                ServerMessage::Event(event) => self.events.push(event),
            }
        }
    }

    async fn shell(&mut self, request: &Request) -> ShellInfo {
        match self.request(request).await {
            Response::Shell { shell } => *shell,
            other => panic!("expected Shell for {request:?}, got {other:?}"),
        }
    }

    async fn refused(&mut self, request: &Request) -> String {
        match self.request(request).await {
            Response::Error { message } => message,
            other => panic!("expected Error for {request:?}, got {other:?}"),
        }
    }

    async fn shells(&mut self) -> Vec<ShellInfo> {
        match self.request(&Request::ListShells).await {
            Response::Shells { shells } => shells,
            other => panic!("expected Shells, got {other:?}"),
        }
    }

    async fn task(&mut self, request: &Request) -> tam_proto::TaskInfo {
        match self.request(request).await {
            Response::Task { task, .. } => task,
            other => panic!("expected Task for {request:?}, got {other:?}"),
        }
    }

    /// Wait for events until `pred` takes one; returns it.
    async fn event(&mut self, pred: impl Fn(&Event) -> bool) -> Event {
        if let Some(i) = self.events.iter().position(&pred) {
            return self.events.remove(i);
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match self.read().await {
                    Some(ServerMessage::Event(event)) if pred(&event) => return event,
                    Some(ServerMessage::Event(event)) => self.events.push(event),
                    Some(_) => {}
                    None => panic!("daemon closed the connection"),
                }
            }
        })
        .await
        .expect("no such event")
    }

    /// Read the events pushed until none comes for `quiet`.
    async fn drain(&mut self, quiet: Duration) {
        while let Ok(Some(msg)) = tokio::time::timeout(quiet, self.read()).await {
            if let ServerMessage::Event(event) = msg {
                self.events.push(event);
            }
        }
    }
}

async fn start(daemon: Daemon, sock: &Path) {
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

async fn start_daemon(sock: &Path, ledger: &Path) {
    let daemon = Daemon::new(sock.to_path_buf(), ledger.to_path_buf())
        .reconcile_every(Duration::from_millis(200));
    start(daemon, sock).await;
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

/// A new task `name` with a worktree in each of `repos`, on branch `name`,
/// created by the daemon.
fn create(root: &Path, name: &str, repos: &[&Path]) -> Request {
    let members = repos
        .iter()
        .map(|repo| {
            let repo_name = repo.file_name().unwrap().to_str().unwrap();
            NewMember {
                repo_root: repo.to_path_buf(),
                branch: name.into(),
                dir: root.join("w").join(format!("{repo_name}--{name}")),
            }
        })
        .collect();
    Request::CreateMembers {
        task: name.into(),
        members,
        source: None,
        join: false,
        env: BTreeMap::new(),
    }
}

/// The caller's environment for a shell: `PATH` with `bin` first, `HOME`
/// (no one's rc files), `SHELL`, and `extra`.
fn env(home: &Path, bin: Option<&Path>, shell: &str, extra: &[(&str, &str)]) -> AgentEnv {
    let path = std::env::var("PATH").unwrap();
    let path = match bin {
        Some(bin) => format!("{}:{path}", bin.display()),
        None => path,
    };
    let mut vars = BTreeMap::from([
        ("PATH".to_string(), path),
        ("HOME".to_string(), home.display().to_string()),
        ("SHELL".to_string(), shell.to_string()),
    ]);
    for (k, v) in extra {
        vars.insert(k.to_string(), v.to_string());
    }
    AgentEnv(vars)
}

fn open(on: ShellPlace, env: &AgentEnv) -> Request {
    Request::OpenShell {
        on,
        env: env.clone(),
        wrapper: Vec::new(),
    }
}

fn member(target: &str) -> ShellPlace {
    ShellPlace::Member(target.into())
}

fn checkout(dir: &Path) -> ShellPlace {
    ShellPlace::Checkout(dir.to_path_buf())
}

fn close(id: &str) -> Request {
    Request::CloseShell { id: id.into() }
}

fn keys(id: &str, data: &str) -> Request {
    Request::Keys {
        id: id.into(),
        data: data.into(),
    }
}

fn script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

/// The contents of `path` once a command wrote it (written whole: to a
/// temporary file, then moved).
async fn written(path: &Path) -> String {
    for _ in 0..500 {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.is_empty() {
                return text.trim().to_string();
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{} was never written", path.display());
}

/// Type `command` into the shell so that it writes its output to `out`
/// whole; returns that output.
async fn run_in(conn: &mut Conn, id: &str, command: &str, out: &Path) -> String {
    let typed = format!(
        "{command} > '{o}.tmp' && mv '{o}.tmp' '{o}'\n",
        o = out.display()
    );
    assert!(matches!(
        conn.request(&keys(id, &typed)).await,
        Response::Ok
    ));
    written(out).await
}

fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// Wait until process `pid` is gone (reaped too: a zombie still answers).
async fn gone(pid: i32, what: &str) {
    for _ in 0..250 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Never leave it behind, then fail.
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    );
    panic!("{what} ({pid}) outlived its shell");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shells_open_on_members_and_checkouts_and_are_numbered() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let sock = root.join("d.sock");
    start_daemon(&sock, &root.join("ledger.jsonl")).await;
    let mut events = Conn::subscribed(&sock).await;
    let mut conn = Conn::connect(&sock).await;
    let api = repo(&root, "api");
    let task = conn.task(&create(&root, "feat", &[&api])).await;
    let m = task.members[0].clone();
    let sh = env(&root, None, "sh", &[]);

    // On the member (by task name, by id) and on the main checkout: each
    // place numbers its own.
    let one = conn.shell(&open(member("feat"), &sh)).await;
    assert!(one.id.starts_with("sh-"), "{}", one.id);
    assert_eq!(
        (
            one.member.as_deref(),
            &one.dir,
            one.repo.as_str(),
            one.number
        ),
        (Some(m.id.as_str()), &m.dir, "api", 1)
    );
    assert_eq!(one.name(), "Shell #1");
    let two = conn.shell(&open(member(&m.id), &sh)).await;
    assert_eq!(
        (two.member.as_deref(), two.number),
        (Some(m.id.as_str()), 2)
    );
    let main = conn.shell(&open(checkout(&api), &sh)).await;
    assert_eq!(
        (
            main.member.as_deref(),
            &main.dir,
            main.repo.as_str(),
            main.number
        ),
        (None, &api, "api", 1)
    );
    for shell in [&one, &two, &main] {
        let Event::ShellOpened { shell: told } = events
            .event(|e| matches!(e, Event::ShellOpened { shell: s } if s.id == shell.id))
            .await
        else {
            unreachable!()
        };
        assert_eq!(&told, shell);
    }
    // Listed in the order they were opened.
    let ids: Vec<String> = conn.shells().await.into_iter().map(|s| s.id).collect();
    assert_eq!(ids, [one.id.clone(), two.id.clone(), main.id.clone()]);

    // Each runs in its place.
    let pwd = run_in(&mut conn, &one.id, "pwd -P", &root.join("pwd-one")).await;
    assert_eq!(Path::new(&pwd), m.dir.canonicalize().unwrap());
    let pwd = run_in(&mut conn, &main.id, "pwd -P", &root.join("pwd-main")).await;
    assert_eq!(Path::new(&pwd), api);

    // Not agents: no agent, no run, no state.
    assert!(matches!(
        conn.request(&Request::List).await,
        Response::Agents { agents } if agents.is_empty()
    ));
    match conn
        .request(&Request::GetTask {
            name: "feat".into(),
        })
        .await
    {
        Response::Task { task, runs } => {
            assert!(runs.is_empty(), "{runs:?}");
            assert_eq!(task.run_count, 0);
        }
        other => panic!("expected Task, got {other:?}"),
    }

    // #1 closed, #2 still open: the next is #3, never #1 while #2 shows.
    let closed = conn.shell(&close(&one.id)).await;
    assert_eq!((closed.id.as_str(), closed.number), (one.id.as_str(), 1));
    events
        .event(|e| matches!(e, Event::ShellClosed { id } if *id == one.id))
        .await;
    let three = conn.shell(&open(member("feat"), &sh)).await;
    assert_eq!(three.number, 3);
    // All closed: #1 again.
    conn.shell(&close(&two.id)).await;
    conn.shell(&close(&three.id)).await;
    let again = conn.shell(&open(member("feat"), &sh)).await;
    assert_eq!(again.number, 1);

    // A shell that exits is gone, and its number free at once.
    assert!(matches!(
        conn.request(&keys(&main.id, "exit\n")).await,
        Response::Ok
    ));
    events
        .event(|e| matches!(e, Event::ShellClosed { id } if *id == main.id))
        .await;
    let ids: Vec<String> = conn.shells().await.into_iter().map(|s| s.id).collect();
    assert_eq!(ids, [again.id.as_str()]);
    let main_again = conn.shell(&open(checkout(&api), &sh)).await;
    assert_eq!(main_again.number, 1);

    // A target names a shell after everything else; closed, it is gone.
    match conn
        .request(&Request::ResolveTarget {
            target: again.id.clone(),
            from: None,
        })
        .await
    {
        Response::Shell { shell } => assert_eq!(shell.id, again.id),
        other => panic!("expected Shell, got {other:?}"),
    }
    let why = conn.refused(&close(&one.id)).await;
    assert!(why.contains("not found"), "{why}");
    let why = conn
        .refused(&Request::ResolveTarget {
            target: one.id.clone(),
            from: None,
        })
        .await;
    assert!(why.contains("not found"), "{why}");
    // Not an agent to kill.
    let why = conn
        .refused(&Request::Kill {
            id: again.id.clone(),
        })
        .await;
    assert!(why.contains("is a shell"), "{why}");

    conn.request(&Request::Shutdown).await;
    events
        .event(|e| matches!(e, Event::ShellClosed { id } if *id == again.id))
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shells_terminal_takes_screen_keys_attach_and_resize() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let sock = root.join("d.sock");
    start_daemon(&sock, &root.join("ledger.jsonl")).await;
    let mut conn = Conn::connect(&sock).await;
    let api = repo(&root, "api");
    let shell = conn
        .shell(&open(checkout(&api), &env(&root, None, "sh", &[])))
        .await;
    let id = shell.id.clone();

    // Keys, then the screen and the scrollback.
    assert!(matches!(
        conn.request(&keys(&id, "echo hello-$((6*7))\n")).await,
        Response::Ok
    ));
    let screen = async |conn: &mut Conn| match conn
        .request(&Request::Screen {
            id: id.clone(),
            from: None,
        })
        .await
    {
        Response::Screen { screen } => screen,
        other => panic!("expected Screen, got {other:?}"),
    };
    let mut found = false;
    for _ in 0..250 {
        if screen(&mut conn).await.contents.contains("hello-42") {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(found, "never on the screen");
    match conn.request(&Request::Scrollback { id: id.clone() }).await {
        Response::Scrollback { data } => assert!(data.contains("hello-42"), "{data}"),
        other => panic!("expected Scrollback, got {other:?}"),
    }

    // Resized: the screen, and the size the shell sees.
    assert!(matches!(
        conn.request(&Request::Resize {
            id: id.clone(),
            cols: 100,
            rows: 30,
        })
        .await,
        Response::Ok
    ));
    let at = screen(&mut conn).await;
    assert_eq!((at.cols, at.rows), (100, 30));
    let size = run_in(&mut conn, &id, "stty size", &root.join("size")).await;
    assert_eq!(size, "30 100");

    // Watched while typed into: updates come, and it counts a viewer.
    let mut watcher = Conn::connect(&sock).await;
    assert!(matches!(
        watcher
            .request(&Request::WatchScreen {
                id: Some(id.clone()),
                viewing: true,
            })
            .await,
        Response::Ok
    ));
    assert!(matches!(
        watcher.request(&keys(&id, "echo watched\n")).await,
        Response::Ok
    ));
    watcher
        .event(|e| matches!(e, Event::ScreenUpdated { id: of } if *of == id))
        .await;
    assert_eq!(conn.shells().await[0].viewers, 1);
    drop(watcher);

    // Attached: the replay, then output and input both ways, at its size.
    let (reader, mut writer) = UnixStream::connect(&sock).await.unwrap().into_split();
    let mut reader = BufReader::new(reader);
    let mut attach = serde_json::to_string(&Request::Attach {
        id: id.clone(),
        cols: 90,
        rows: 20,
    })
    .unwrap();
    attach.push('\n');
    writer.write_all(attach.as_bytes()).await.unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(
        matches!(
            serde_json::from_str::<ServerMessage>(line.trim()).unwrap(),
            ServerMessage::Response(Response::Attached)
        ),
        "{line}"
    );
    writer.write_all(b"echo attached-$((1+1))\n").await.unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    tokio::time::timeout(Duration::from_secs(5), async {
        while !String::from_utf8_lossy(&out).contains("attached-2") {
            let n = reader.read(&mut buf).await.unwrap();
            assert!(n > 0, "the attach ended");
            out.extend_from_slice(&buf[..n]);
        }
    })
    .await
    .expect("nothing came back through the attach");
    let at = screen(&mut conn).await;
    assert_eq!((at.cols, at.rows), (90, 20));
    assert_eq!(conn.shells().await[0].viewers, 1);
    drop((reader, writer));

    // Closed: none of it takes its id any more.
    conn.shell(&close(&id)).await;
    let why = conn
        .refused(&Request::Screen {
            id: id.clone(),
            from: None,
        })
        .await;
    assert!(why.contains(&format!("shell '{id}' not found")), "{why}");
    let why = conn.refused(&keys(&id, "x")).await;
    assert!(why.contains("not found"), "{why}");
    conn.request(&Request::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shells_terminal_answers_where_the_cursor_is_unless_a_terminal_is_attached() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let sock = root.join("d.sock");
    start_daemon(&sock, &root.join("ledger.jsonl")).await;
    let mut conn = Conn::connect(&sock).await;
    let api = repo(&root, "api");
    // bash: `read -d` (dash, `sh` on Debian, has none).
    let shell = conn
        .shell(&open(checkout(&api), &env(&root, None, "bash", &[])))
        .await;
    let id = shell.id.clone();
    // Ask where the cursor is, as `fzf --height` does before it draws
    // anything, once it is somewhere known; the answer goes to a file.
    let ask = r#"printf '\033[2J\033[7;12H\033[6n'; read -rs -t 5 -d R at; echo "${at#*[}""#;

    // Typed into from a pane (a viewing watch: a viewer, with no terminal
    // to answer), no terminal attached: the daemon's answers.
    let mut pane = Conn::connect(&sock).await;
    assert!(matches!(
        pane.request(&Request::WatchScreen {
            id: Some(id.clone()),
            viewing: true,
        })
        .await,
        Response::Ok
    ));
    assert_eq!(conn.shells().await[0].viewers, 1);
    assert_eq!(
        run_in(&mut pane, &id, ask, &root.join("alone")).await,
        "7;12"
    );
    drop(pane);

    // Attached: the viewer's terminal answers what it is relayed (its
    // input comes back to the PTY), the daemon does not: one answer.
    let (reader, mut writer) = UnixStream::connect(&sock).await.unwrap().into_split();
    let mut reader = BufReader::new(reader);
    let mut attach = serde_json::to_string(&Request::Attach {
        id: id.clone(),
        cols: 90,
        rows: 20,
    })
    .unwrap();
    attach.push('\n');
    writer.write_all(attach.as_bytes()).await.unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.contains("attached"), "{line}");
    let out = root.join("attached");
    let typed = format!(
        "{ask} > '{o}.tmp' && mv '{o}.tmp' '{o}'\n",
        o = out.display()
    );
    assert!(matches!(
        conn.request(&keys(&id, &typed)).await,
        Response::Ok
    ));
    let mut relayed = Vec::new();
    let mut buf = [0u8; 4096];
    tokio::time::timeout(Duration::from_secs(5), async {
        while !relayed.windows(4).any(|w| w == b"\x1b[6n") {
            let n = reader.read(&mut buf).await.unwrap();
            assert!(n > 0, "the attach ended");
            relayed.extend_from_slice(&buf[..n]);
        }
    })
    .await
    .expect("the query was not relayed");
    writer.write_all(b"\x1b[3;4R").await.unwrap();
    assert_eq!(written(&out).await, "3;4");

    // Detached: the daemon's again.
    drop((reader, writer));
    for _ in 0..250 {
        if conn.shells().await[0].viewers == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(conn.shells().await[0].viewers, 0);
    assert_eq!(
        run_in(&mut conn, &id, ask, &root.join("again")).await,
        "7;12"
    );
    conn.request(&Request::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_a_shell_leaves_no_job_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let sock = root.join("d.sock");
    start_daemon(&sock, &root.join("ledger.jsonl")).await;
    let mut conn = Conn::connect(&sock).await;
    let api = repo(&root, "api");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // A wrapper that forks the shell instead of becoming it: the daemon
    // knows only the wrapper's pid. Hung up, the wrapper dies, and the
    // kernel hangs up the terminal's foreground group: the shell at its
    // prompt (running a foreground job, the job alone: wrappers are to
    // `exec` the shell).
    script(&bin.join("forking-wrap"), "#!/bin/sh\n\"$@\"\n");
    let bash = env(&root, Some(&bin), "bash", &[]);

    for wrapper in [vec![], vec!["forking-wrap".to_string()]] {
        let label = if wrapper.is_empty() {
            "shell"
        } else {
            "wrapped shell"
        };
        let out = root.join(format!("out-{}", wrapper.len()));
        std::fs::create_dir_all(&out).unwrap();
        let shell = conn
            .shell(&Request::OpenShell {
                on: checkout(&api),
                env: bash.clone(),
                wrapper: wrapper.clone(),
            })
            .await;
        let pid: i32 = run_in(&mut conn, &shell.id, "echo $$", &out.join("pid"))
            .await
            .parse()
            .unwrap();
        if wrapper.is_empty() {
            assert_eq!(pid as u32, shell.pid);
        } else {
            assert_ne!(pid as u32, shell.pid, "the wrapper forked the shell");
        }
        // A job in the background, then one in the foreground.
        let bg: i32 = run_in(&mut conn, &shell.id, "sleep 120 & echo $!", &out.join("bg"))
            .await
            .parse()
            .unwrap();
        let fg = if wrapper.is_empty() {
            let typed = format!(
                "sh -c 'echo $$ > {o}/fg.tmp && mv {o}/fg.tmp {o}/fg && exec sleep 120'\n",
                o = out.display()
            );
            conn.request(&keys(&shell.id, &typed)).await;
            Some(written(&out.join("fg")).await.parse::<i32>().unwrap())
        } else {
            None
        };
        assert!(alive(pid) && alive(bg) && fg.is_none_or(alive));

        conn.shell(&close(&shell.id)).await;
        gone(pid, label).await;
        gone(bg, &format!("the background job of the {label}")).await;
        if let Some(fg) = fg {
            gone(fg, &format!("the foreground job of the {label}")).await;
        }
        gone(shell.pid as i32, label).await;
    }
    conn.request(&Request::Shutdown).await;
}

/// Where [`hangup_recorder`] says it started.
fn started(report: &Path) -> PathBuf {
    PathBuf::from(format!("{}.started", report.display()))
}

/// A shell for drop tests: when hung up, it takes a moment, then records
/// whether its worktree is still there, and exits.
fn hangup_recorder(bin: &Path) {
    script(
        &bin.join("hup-shell"),
        "#!/bin/sh\n\
         trap 'sleep 0.3; if [ -d \"$PWD\" ]; then echo present; else echo absent; fi \
         > \"$REPORT.tmp\"; mv \"$REPORT.tmp\" \"$REPORT\"; exit 0' HUP\n\
         echo started > \"$REPORT.started\"\n\
         while :; do sleep 0.1; done\n",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drop_closes_the_shells_before_removing_the_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let sock = root.join("d.sock");
    start_daemon(&sock, &root.join("ledger.jsonl")).await;
    let mut events = Conn::subscribed(&sock).await;
    let mut conn = Conn::connect(&sock).await;
    let api = repo(&root, "api");
    let web = repo(&root, "web");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    hangup_recorder(&bin);
    let recorder = |report: &Path| {
        env(
            &root,
            Some(&bin),
            "hup-shell",
            &[("REPORT", report.to_str().unwrap())],
        )
    };
    let opened = async |conn: &mut Conn, target: &str, report: &Path| {
        let shell = conn.shell(&open(member(target), &recorder(report))).await;
        written(&started(report)).await;
        shell
    };

    // A task drop.
    let task = conn.task(&create(&root, "feat", &[&api])).await;
    let dir = task.members[0].dir.clone();
    let report = root.join("feat.report");
    let shell = opened(&mut conn, "feat", &report).await;
    // An uncommitted change refuses the drop, which changes nothing: the
    // shell stays open.
    std::fs::write(dir.join("wip"), "wip\n").unwrap();
    assert!(matches!(
        conn.request(&Request::DropTask {
            name: "feat".into(),
            force: false,
            delete_branch: false,
        })
        .await,
        Response::DropRefused { .. }
    ));
    assert_eq!(conn.shells().await.len(), 1);
    std::fs::remove_file(dir.join("wip")).unwrap();
    // A shell open is no reason to refuse: the drop closes it, then removes
    // the worktree.
    match conn
        .request(&Request::DropTask {
            name: "feat".into(),
            force: false,
            delete_branch: false,
        })
        .await
    {
        Response::Dropped { .. } => {}
        other => panic!("expected Dropped, got {other:?}"),
    }
    assert_eq!(written(&report).await, "present", "removed under its shell");
    assert!(!dir.exists());
    assert!(conn.shells().await.is_empty());
    events
        .event(|e| matches!(e, Event::ShellClosed { id } if *id == shell.id))
        .await;
    assert!(
        !events
            .events
            .iter()
            .any(|e| matches!(e, Event::TaskDropped { .. })),
        "the task went before its shell: {:?}",
        events.events
    );
    events
        .event(|e| matches!(e, Event::TaskDropped { name } if name == "feat"))
        .await;

    // A member drop: its shells only.
    let task = conn.task(&create(&root, "both", &[&api, &web])).await;
    let (api_dir, web_dir) = (task.members[0].dir.clone(), task.members[1].dir.clone());
    let kept_report = root.join("both-api.report");
    let kept = opened(&mut conn, "both/api", &kept_report).await;
    let report = root.join("both-web.report");
    let dropped = opened(&mut conn, "both/web", &report).await;
    match conn
        .request(&Request::DropTask {
            name: "both/web".into(),
            force: false,
            delete_branch: false,
        })
        .await
    {
        Response::Dropped { .. } => {}
        other => panic!("expected Dropped, got {other:?}"),
    }
    assert_eq!(written(&report).await, "present", "removed under its shell");
    assert!(!web_dir.exists() && api_dir.exists());
    events
        .event(|e| matches!(e, Event::ShellClosed { id } if *id == dropped.id))
        .await;
    let left: Vec<String> = conn.shells().await.into_iter().map(|s| s.id).collect();
    assert_eq!(left, [kept.id.as_str()]);

    // Its last member takes the task, and the shell.
    match conn
        .request(&Request::DropTask {
            name: "both/api".into(),
            force: false,
            delete_branch: false,
        })
        .await
    {
        Response::Dropped { .. } => {}
        other => panic!("expected Dropped, got {other:?}"),
    }
    assert_eq!(written(&kept_report).await, "present");
    assert!(!api_dir.exists());
    assert!(conn.shells().await.is_empty());
    conn.request(&Request::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shell_runs_in_its_callers_environment_and_nothing_of_it_is_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let sock = root.join("d.sock");
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let ledger = data.join("ledger.jsonl");
    start_daemon(&sock, &ledger).await;
    let mut watcher = Conn::subscribed(&sock).await;
    let mut conn = Conn::connect(&sock).await;
    let api = repo(&root, "api");
    let task = conn.task(&create(&root, "feat", &[&api])).await;
    let dir = task.members[0].dir.clone();

    // The shell and the wrapper are only on the caller's PATH. The shell
    // reports what it found (never the secret itself: its screen is
    // kept), then waits.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let report = root.join("report");
    script(
        &bin.join("env-shell"),
        &format!(
            r#"#!/bin/sh
{{
  [ "${SECRET_NAME}" = "{SECRET_VALUE}" ] && echo secret-ok
  [ -z "${{TAM_AGENT_ID+set}}" ] && echo no-agent-id
  [ -z "${{TAM_SOCKET+set}}" ] && echo no-socket
  [ "$PWD" = "$(pwd)" ] && echo "pwd=$PWD"
  echo "wrapped_in=$WRAPPED_IN"
  echo "term=$TERM"
}} > "$REPORT.tmp"
mv "$REPORT.tmp" "$REPORT"
exec sleep 120
"#
        ),
    );
    script(
        &bin.join("wrap"),
        "#!/bin/sh\nexport WRAPPED_IN=\"$(pwd)\"\nexec \"$@\"\n",
    );
    // TAM_* the caller has (another agent's): not the shell's.
    let caller = env(
        &root,
        Some(&bin),
        "env-shell",
        &[
            (SECRET_NAME, SECRET_VALUE),
            ("TAM_AGENT_ID", "m-caller"),
            ("TAM_SOCKET", "/elsewhere/sock"),
            ("REPORT", report.to_str().unwrap()),
        ],
    );
    let plain = conn.shell(&open(member("feat"), &caller)).await;
    assert_eq!(
        written(&report).await.lines().collect::<Vec<_>>(),
        [
            "secret-ok",
            "no-agent-id",
            "no-socket",
            &format!("pwd={}", dir.display()),
            "wrapped_in=",
            "term=xterm-256color",
        ]
    );
    std::fs::remove_file(&report).unwrap();
    conn.shell(&close(&plain.id)).await;

    // Through the wrapper, in the shell's directory.
    let wrapped = conn
        .shell(&Request::OpenShell {
            on: checkout(&api),
            env: caller.clone(),
            wrapper: vec!["wrap".into()],
        })
        .await;
    let found = written(&report).await;
    assert!(
        found.contains(&format!("wrapped_in={}", api.display())),
        "{found}"
    );
    assert!(found.contains("no-agent-id"), "{found}");

    // Failures name the program, never the environment.
    let mut missing = caller.clone();
    missing.0.insert("SHELL".into(), "no-such-shell-tam".into());
    let why = conn.refused(&open(member("feat"), &missing)).await;
    assert!(why.contains("failed to spawn 'no-such-shell-tam'"), "{why}");
    let why = conn
        .refused(&Request::OpenShell {
            on: member("feat"),
            env: caller.clone(),
            wrapper: vec!["no-such-wrapper-tam".into()],
        })
        .await;
    assert!(why.contains("through 'no-such-wrapper-tam'"), "{why}");

    // Nor in what clients read.
    for request in [
        Request::ListShells,
        Request::Scrollback {
            id: wrapped.id.clone(),
        },
        Request::Screen {
            id: wrapped.id.clone(),
            from: None,
        },
        Request::ListTasks,
    ] {
        conn.request(&request).await;
    }
    watcher.drain(Duration::from_millis(300)).await;
    let assert_no_secret = |what: &str, text: &str| {
        assert!(
            !text.contains(SECRET_NAME) && !text.contains(SECRET_VALUE),
            "the caller's environment leaked into {what}: {text}"
        );
    };
    assert_no_secret("replies", &conn.seen.concat());
    assert_no_secret("events", &watcher.seen.concat());
    assert!(
        watcher.seen.iter().any(|e| e.contains("shell_opened")),
        "{:?}",
        watcher.seen
    );

    // Nor in what the daemon writes; and no shell is in the ledger (the
    // activity log, `log/`, says what was done with them).
    conn.request(&Request::Shutdown).await;
    drop((conn, watcher));
    wait_ledger_released(&ledger).await;
    let log = data.join("log");
    assert!(log.is_dir(), "no activity log in {}", data.display());
    let mut files = vec![data.clone()];
    while let Some(path) = files.pop() {
        if path.is_dir() {
            files.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
        } else if let Ok(bytes) = std::fs::read(&path) {
            let text = String::from_utf8_lossy(&bytes);
            assert_no_secret(&path.display().to_string(), &text);
            if path.starts_with(&log) {
                continue;
            }
            for id in [&plain.id, &wrapped.id] {
                assert!(!text.contains(id.as_str()), "{id} in {}", path.display());
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_merge_keeps_a_shell_with_its_member() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let sock = root.join("d.sock");
    start_daemon(&sock, &root.join("ledger.jsonl")).await;
    let mut conn = Conn::connect(&sock).await;
    let api = repo(&root, "api");
    let web = repo(&root, "web");
    let from = conn.task(&create(&root, "api-fix", &[&api])).await;
    conn.task(&create(&root, "web-fix", &[&web])).await;
    let moving = from.members[0].clone();
    let sh = env(&root, None, "sh", &[]);
    let shell = conn.shell(&open(member("api-fix"), &sh)).await;

    match conn
        .request(&Request::MergeTasks {
            into: "web-fix".into(),
            from: vec!["api-fix".into()],
        })
        .await
    {
        Response::Moved { moved, .. } => assert_eq!(moved, [moving.id.as_str()]),
        other => panic!("expected Moved, got {other:?}"),
    }
    // The same shell, still its member's, alive.
    assert_eq!(conn.shells().await, std::slice::from_ref(&shell));
    let pwd = run_in(&mut conn, &shell.id, "pwd -P", &root.join("pwd")).await;
    assert_eq!(Path::new(&pwd), moving.dir.canonicalize().unwrap());
    // Its member numbers on, under the name it has now.
    let next = conn.shell(&open(member("web-fix/api"), &sh)).await;
    assert_eq!(
        (next.member.as_deref(), next.number),
        (Some(moving.id.as_str()), 2)
    );
    // And the merged task's drop closes them.
    match conn
        .request(&Request::DropTask {
            name: "web-fix".into(),
            force: true,
            delete_branch: false,
        })
        .await
    {
        Response::Dropped { .. } => {}
        other => panic!("expected Dropped, got {other:?}"),
    }
    assert!(conn.shells().await.is_empty());
    gone(shell.pid as i32, "the moved member's shell").await;
    conn.request(&Request::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_stays_up_while_a_shell_is_open() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let sock = root.join("d.sock");
    let daemon = Daemon::new(sock.clone(), root.join("ledger.jsonl"))
        .idle_timeout(Duration::from_millis(500));
    start(daemon, &sock).await;
    let api = repo(&root, "api");
    let shell = {
        let mut conn = Conn::connect(&sock).await;
        conn.shell(&open(checkout(&api), &env(&root, None, "sh", &[])))
            .await
    };

    // No agent, no client: the shell keeps it up.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut conn = Conn::connect(&sock).await;
    assert_eq!(conn.shells().await, std::slice::from_ref(&shell));
    conn.shell(&close(&shell.id)).await;
    drop(conn);

    // Then nothing does: it removes its socket as it goes (watched without
    // connecting, which would keep it up).
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if !sock.exists() {
            return;
        }
    }
    panic!("the daemon did not shut down once its shell closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shells_are_refused_where_they_cannot_run() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let sock = root.join("d.sock");
    start_daemon(&sock, &root.join("ledger.jsonl")).await;
    let mut events = Conn::subscribed(&sock).await;
    let mut conn = Conn::connect(&sock).await;
    let api = repo(&root, "api");
    let sh = env(&root, None, "sh", &[]);

    // A worktree gone.
    let gone_task = conn.task(&create(&root, "gone", &[&api])).await;
    let gone_dir = gone_task.members[0].dir.clone();
    git(
        &api,
        &["worktree", "remove", "--force", gone_dir.to_str().unwrap()],
    );
    conn.request(&Request::Reconcile).await;
    let why = conn.refused(&open(member("gone"), &sh)).await;
    assert!(why.contains("is gone"), "{why}");

    // A directory that is not a worktree of its repository.
    let plain = root.join("w").join("api--plain");
    std::fs::create_dir_all(&plain).unwrap();
    conn.task(&Request::CreateTask {
        name: "broken".into(),
        members: vec![NewMember {
            repo_root: api.clone(),
            branch: "plain".into(),
            dir: plain.clone(),
        }],
    })
    .await;
    let why = conn.refused(&open(member("broken"), &sh)).await;
    assert!(why.contains("broken"), "{why}");

    // A task being dropped: its shells close (slowly), and none opens.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    hangup_recorder(&bin);
    conn.task(&create(&root, "going", &[&api])).await;
    let report = root.join("going.report");
    let slow = env(
        &root,
        Some(&bin),
        "hup-shell",
        &[("REPORT", report.to_str().unwrap())],
    );
    let shell = conn.shell(&open(member("going"), &slow)).await;
    written(&started(&report)).await;
    let mut dropper = Conn::connect(&sock).await;
    dropper
        .send(&Request::DropTask {
            name: "going".into(),
            force: false,
            delete_branch: false,
        })
        .await;
    events
        .event(|e| matches!(e, Event::ShellClosed { id } if *id == shell.id))
        .await;
    let why = conn.refused(&open(member("going"), &sh)).await;
    assert!(why.contains("being dropped"), "{why}");
    loop {
        match dropper.read().await.expect("daemon closed the connection") {
            ServerMessage::Response(Response::Dropped { .. }) => break,
            ServerMessage::Response(other) => panic!("expected Dropped, got {other:?}"),
            ServerMessage::Event(_) => {}
        }
    }

    // Not a main checkout: a linked worktree, a directory inside the
    // checkout, a relative path, nothing there.
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
    for dir in [
        linked.clone(),
        inside,
        PathBuf::from("src/api"),
        root.join("nowhere"),
    ] {
        let why = conn.refused(&open(checkout(&dir), &sh)).await;
        assert!(why.starts_with("cannot open a shell: "), "{why}");
    }
    let why = conn.refused(&open(checkout(&linked), &sh)).await;
    assert!(why.contains("not the main checkout"), "{why}");

    // No such member; a chat is no member.
    let why = conn.refused(&open(member("nothing"), &sh)).await;
    assert!(why.contains("not found"), "{why}");
    let chat = match conn
        .request(&Request::CreateChat {
            dir: api.clone(),
            name: Some("talk".into()),
            session: None,
        })
        .await
    {
        Response::Chat { chat, .. } => chat,
        other => panic!("expected Chat, got {other:?}"),
    };
    let why = conn.refused(&open(member(&chat.id), &sh)).await;
    assert!(why.contains("is a chat"), "{why}");

    // None of it opened anything.
    assert!(conn.shells().await.is_empty());
    conn.request(&Request::Shutdown).await;
}
