//! Creating a task's worktrees in several repositories (`CreateMembers`):
//! all or nothing; and dropping one of them (`DropTask` of a member). Against
//! real git repositories.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tam_daemon::daemon::Daemon;
use tam_proto::{Event, MemberHealth, NewMember, Request, Response, ServerMessage, TaskInfo};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Start a daemon on a temp socket; it runs until the test ends.
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
                ServerMessage::Event(event) => self.events.push(event),
            }
        }
    }

    /// The events pushed within `ms`, queued ones first.
    async fn events_within(&mut self, ms: u64) -> Vec<Event> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
        while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, self.read()).await {
            if let ServerMessage::Event(event) = msg {
                self.events.push(event);
            }
        }
        std::mem::take(&mut self.events)
    }

    async fn create(&mut self, task: &str, members: &[NewMember], join: bool) -> Response {
        self.request(&create_request(task, members, None, join))
            .await
    }

    async fn tasks(&mut self) -> Vec<TaskInfo> {
        match self.request(&Request::ListTasks).await {
            Response::Tasks { tasks } => tasks,
            other => panic!("expected Tasks, got {other:?}"),
        }
    }
}

fn create_request(task: &str, members: &[NewMember], source: Option<&str>, join: bool) -> Request {
    Request::CreateMembers {
        task: task.into(),
        members: members.to_vec(),
        source: source.map(str::to_string),
        join,
        env: Default::default(),
    }
}

fn created(resp: Response) -> TaskInfo {
    match resp {
        Response::Task { task, .. } => task,
        other => panic!("expected Task, got {other:?}"),
    }
}

fn error_message(resp: Response) -> String {
    match resp {
        Response::Error { message } => message,
        other => panic!("expected Error, got {other:?}"),
    }
}

/// Run git in `dir`, asserting success.
fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
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

/// A repository at `path`, with one commit on `main`.
fn repo(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.email", "test@test.com"]);
    git(path, &["config", "user.name", "Test"]);
    git(path, &["commit", "-q", "--allow-empty", "-m", "init"]);
    path.to_path_buf()
}

/// Give `repo` a `post-checkout` hook (it runs in each new worktree).
fn hook(repo: &Path, script: &str) {
    let hooks = repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    git(repo, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
    let hook = hooks.join("post-checkout");
    std::fs::write(&hook, format!("#!/bin/sh\n{script}\n")).unwrap();
    let mut perms = std::fs::metadata(&hook).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&hook, perms).unwrap();
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    std::process::Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(repo)
        .status()
        .unwrap()
        .success()
}

/// Repositories under `root/src`, worktrees under `root/wt`.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(dir: &Path) -> Self {
        Self {
            root: dir.canonicalize().unwrap(),
        }
    }

    fn repo(&self, name: &str) -> PathBuf {
        repo(&self.root.join("src").join(name))
    }

    fn worktree(&self, repo: &Path, branch: &str) -> PathBuf {
        let name = repo.file_name().unwrap().to_string_lossy();
        self.root.join("wt").join(format!("{name}--{branch}"))
    }

    fn member(&self, repo: &Path, branch: &str) -> NewMember {
        NewMember {
            repo_root: repo.to_path_buf(),
            branch: branch.into(),
            dir: self.worktree(repo, branch),
        }
    }
}

fn ok_on(branch: &str) -> Option<MemberHealth> {
    Some(MemberHealth::Ok {
        branch: Some(branch.into()),
    })
}

#[tokio::test]
async fn one_task_across_repositories_then_one_more() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let fx = Fixture::new(dir.path());
    let (api, web, docs) = (fx.repo("api"), fx.repo("web"), fx.repo("docs"));
    // An existing branch is checked out, a missing one created.
    git(&web, &["branch", "feat"]);
    let mut watcher = Conn::subscribed(&sock).await;
    let mut cmd = Conn::connect(&sock).await;

    let members = [fx.member(&api, "feat"), fx.member(&web, "feat")];
    let task = created(cmd.create("feat", &members, false).await);
    let names: Vec<&str> = task.members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["api", "web"]);
    for (m, new) in task.members.iter().zip(&members) {
        assert_eq!(m.dir, new.dir);
        assert_eq!(m.repo_root.as_deref(), Some(new.repo_root.as_path()));
        assert_eq!(m.health, ok_on("feat"));
        assert!(m.dir.join(".git").is_file());
    }
    let events = watcher.events_within(300).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TaskCreated { task: t } if *t == task)),
        "{events:?}"
    );

    // A third repository joins; the first two are untouched.
    let docs_member = fx.member(&docs, "feat");
    let task = created(
        cmd.create("feat", std::slice::from_ref(&docs_member), true)
            .await,
    );
    assert_eq!(task.members.len(), 3);
    assert_eq!(task.members[2].name, "docs");
    assert_eq!(task.members[2].health, ok_on("feat"));
    let events = watcher.events_within(300).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TaskUpdated { task: t } if t.members.len() == 3)),
        "{events:?}"
    );

    // A new task needs a free name; joining, a task that exists.
    let err = error_message(cmd.create("feat", &[fx.member(&api, "x")], false).await);
    assert!(err.contains("already exists"), "{err}");
    let err = error_message(cmd.create("nope", &[fx.member(&api, "x")], true).await);
    assert!(err.contains("does not exist"), "{err}");

    // Dropped together.
    let resp = cmd
        .request(&Request::DropTask {
            name: "feat".into(),
            force: false,
            delete_branch: false,
        })
        .await;
    assert!(matches!(resp, Response::Dropped { .. }), "{resp:?}");
    for m in &task.members {
        assert!(!m.dir.exists(), "{}", m.dir.display());
    }
}

#[tokio::test]
async fn git_runs_in_the_environment_of_the_request() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let fx = Fixture::new(dir.path());
    let api = fx.repo("api");
    let mark = fx.root.join("mark");
    hook(
        &api,
        &format!("echo \"${{TAM_TEST_MARK:-unset}}\" > '{}'", mark.display()),
    );
    let mut cmd = Conn::connect(&sock).await;

    // The client's environment: the hook sees it.
    let mut env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    env.insert("TAM_TEST_MARK".into(), "from the client".into());
    let request = Request::CreateMembers {
        task: "feat".into(),
        members: vec![fx.member(&api, "feat")],
        source: None,
        join: false,
        env,
    };
    created(cmd.request(&request).await);
    assert_eq!(std::fs::read_to_string(&mark).unwrap(), "from the client\n");

    // None: the daemon's own.
    created(
        cmd.create("other", &[fx.member(&api, "other")], false)
            .await,
    );
    assert_eq!(std::fs::read_to_string(&mark).unwrap(), "unset\n");
}

#[tokio::test]
async fn a_failure_undoes_the_worktrees_created_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let fx = Fixture::new(dir.path());
    let (api, web, docs) = (fx.repo("api"), fx.repo("web"), fx.repo("docs"));
    git(&web, &["branch", "feat"]);
    // Git creates the worktree, then fails on the hook.
    hook(&docs, "exit 1");
    let mut watcher = Conn::subscribed(&sock).await;
    let mut cmd = Conn::connect(&sock).await;

    let members = [
        fx.member(&api, "feat"),
        fx.member(&web, "feat"),
        fx.member(&docs, "feat"),
    ];
    let err = error_message(cmd.create("feat", &members, false).await);
    assert!(err.starts_with("cannot create task 'feat'"), "{err}");
    assert!(err.contains(&docs.display().to_string()), "{err}");
    assert!(err.contains("Nothing was left behind"), "{err}");

    for m in &members {
        assert!(!m.dir.exists(), "{}", m.dir.display());
    }
    // Branches created for the task go; the one that existed stays.
    assert!(!branch_exists(&api, "feat"));
    assert!(branch_exists(&web, "feat"));
    assert!(!branch_exists(&docs, "feat"));
    assert!(cmd.tasks().await.is_empty());
    let events = watcher.events_within(300).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::TaskCreated { .. })),
        "{events:?}"
    );

    // Nothing in the way of a second try.
    std::fs::remove_file(docs.join(".git/hooks/post-checkout")).unwrap();
    let task = created(cmd.create("feat", &members, false).await);
    assert_eq!(task.members.len(), 3);
}

#[tokio::test]
async fn a_branch_git_left_without_its_worktree_is_undone() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let fx = Fixture::new(dir.path());
    let (api, web) = (fx.repo("api"), fx.repo("web"));
    let mut cmd = Conn::connect(&sock).await;
    // Web's worktree goes where it cannot be created: git makes the
    // branch, then fails.
    let ro = fx.root.join("ro");
    std::fs::create_dir_all(&ro).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    let web_member = NewMember {
        dir: ro.join("web--feat"),
        ..fx.member(&web, "feat")
    };

    let members = [fx.member(&api, "feat"), web_member];
    let err = error_message(cmd.create("feat", &members, false).await);
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(err.contains("branch 'feat' of"), "{err}");
    assert!(err.contains("Nothing was left behind"), "{err}");
    assert!(!members[0].dir.exists());
    assert!(!branch_exists(&api, "feat"));
    assert!(!branch_exists(&web, "feat"));
    assert!(cmd.tasks().await.is_empty());
}

#[tokio::test]
async fn what_cannot_be_undone_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let fx = Fixture::new(dir.path());
    let (api, web) = (fx.repo("api"), fx.repo("web"));
    // Something writes into api's new worktree: removing it would lose that.
    hook(&api, "echo generated > notes.txt");
    hook(&web, "exit 1");
    let mut cmd = Conn::connect(&sock).await;

    let members = [fx.member(&api, "feat"), fx.member(&web, "feat")];
    let err = error_message(cmd.create("feat", &members, false).await);
    let api_wt = &members[0].dir;
    assert!(err.contains("Left behind, in no task"), "{err}");
    assert!(err.contains(&api_wt.display().to_string()), "{err}");
    assert!(err.contains("uncommitted changes"), "{err}");
    assert!(err.contains("tam adopt"), "{err}");
    assert!(api_wt.join("notes.txt").exists());
    assert!(branch_exists(&api, "feat"));
    assert!(!members[1].dir.exists());
    assert!(cmd.tasks().await.is_empty());
}

#[tokio::test]
async fn everything_is_checked_before_anything_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let fx = Fixture::new(dir.path());
    let (api, web) = (fx.repo("api"), fx.repo("web"));
    let mut cmd = Conn::connect(&sock).await;
    let api_feat = fx.member(&api, "feat");

    let refused = |err: String, expected: &str| {
        assert!(err.starts_with("cannot create task 'feat'"), "{err}");
        assert!(err.contains(expected), "{expected:?} in {err}");
        assert!(err.contains("Nothing was created"), "{err}");
    };

    // Two repositories with the same directory name: one worktree path.
    let other_web = repo(&fx.root.join("elsewhere/web"));
    let clash = [fx.member(&web, "feat"), fx.member(&other_web, "feat")];
    refused(
        error_message(
            cmd.create(
                "feat",
                &[api_feat.clone(), clash[0].clone(), clash[1].clone()],
                false,
            )
            .await,
        ),
        "would both have their worktree at",
    );

    // The branch is checked out elsewhere in the second repository.
    let elsewhere = fx.root.join("web-feat-by-hand");
    git(
        &web,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            elsewhere.to_str().unwrap(),
        ],
    );
    refused(
        error_message(
            cmd.create("feat", &[api_feat.clone(), fx.member(&web, "feat")], false)
                .await,
        ),
        "is checked out at",
    );

    // The start point exists in one repository only.
    git(&api, &["branch", "dev"]);
    let resp = cmd
        .request(&create_request(
            "x",
            &[fx.member(&api, "x"), fx.member(&web, "x")],
            Some("dev"),
            false,
        ))
        .await;
    let err = error_message(resp);
    assert!(err.contains("'dev' is not a commit"), "{err}");
    assert!(err.contains(&web.display().to_string()), "{err}");
    assert!(!branch_exists(&api, "x"));

    // Not a repository's main checkout; not a directory; nothing given.
    let as_repo = |repo: &Path| NewMember {
        repo_root: repo.to_path_buf(),
        branch: "feat".into(),
        dir: fx.root.join("wt/odd--feat"),
    };
    refused(
        error_message(
            cmd.create("feat", &[api_feat.clone(), as_repo(&elsewhere)], false)
                .await,
        ),
        "not the main checkout",
    );
    refused(
        error_message(
            cmd.create(
                "feat",
                &[api_feat.clone(), as_repo(&fx.root.join("nope"))],
                false,
            )
            .await,
        ),
        "no such directory",
    );
    let err = error_message(cmd.create("feat", &[], false).await);
    assert!(err.contains("no repository given"), "{err}");

    // None of it created anything.
    assert!(!api_feat.dir.exists());
    assert!(!branch_exists(&api, "feat"));
    assert!(
        !fx.root.join("wt").exists()
            || std::fs::read_dir(fx.root.join("wt"))
                .unwrap()
                .next()
                .is_none()
    );
    assert!(cmd.tasks().await.is_empty());
}

#[tokio::test]
async fn joining_a_task_being_dropped_is_undone() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let fx = Fixture::new(dir.path());
    let (api, web) = (fx.repo("api"), fx.repo("web"));
    let mut cmd = Conn::connect(&sock).await;
    created(cmd.create("feat", &[fx.member(&api, "feat")], false).await);

    // The worktree in web takes a while to create: the drop claims the
    // task meanwhile, and waits for it.
    hook(&web, "sleep 1");
    let web_feat = fx.member(&web, "feat");
    let joining = {
        let (sock, web_feat) = (sock.clone(), web_feat.clone());
        tokio::spawn(async move {
            let mut conn = Conn::connect(&sock).await;
            conn.create("feat", &[web_feat], true).await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let resp = cmd
        .request(&Request::DropTask {
            name: "feat".into(),
            force: false,
            delete_branch: false,
        })
        .await;
    assert!(matches!(resp, Response::Dropped { .. }), "{resp:?}");

    let err = error_message(joining.await.unwrap());
    assert!(err.starts_with("cannot add to task 'feat'"), "{err}");
    assert!(err.contains("being dropped"), "{err}");
    assert!(!web_feat.dir.exists());
    assert!(!branch_exists(&web, "feat"));
    assert!(!fx.worktree(&api, "feat").exists());
    assert!(cmd.tasks().await.is_empty());
}

#[tokio::test]
async fn one_member_is_dropped_alone() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let fx = Fixture::new(dir.path());
    let (api, web) = (fx.repo("api"), fx.repo("web"));
    let mut watcher = Conn::subscribed(&sock).await;
    let mut cmd = Conn::connect(&sock).await;
    let members = [fx.member(&api, "feat"), fx.member(&web, "feat")];
    let task = created(cmd.create("feat", &members, false).await);
    let web_id = task.members[1].id.clone();
    let resp = cmd
        .request(&Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::new(),
            id: None,
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            member: Some(web_id.clone()),
        })
        .await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    std::fs::write(members[1].dir.join("notes.txt"), "work").unwrap();
    watcher.events_within(100).await;

    let drop = |name: &str, force: bool| Request::DropTask {
        name: name.into(),
        force,
        delete_branch: false,
    };
    // Work would be lost: nothing happens.
    match cmd.request(&drop("feat/web", false)).await {
        Response::DropRefused { worktrees } => {
            assert_eq!(worktrees.len(), 1);
            assert_eq!(worktrees[0].path, members[1].dir);
        }
        other => panic!("expected DropRefused, got {other:?}"),
    }
    assert!(members[1].dir.exists());

    // Forced: web's agent and worktree go, api stays, the task too.
    let resp = cmd.request(&drop("feat/web", true)).await;
    assert!(matches!(resp, Response::Dropped { .. }), "{resp:?}");
    assert!(!members[1].dir.exists());
    assert!(branch_exists(&web, "feat"));
    assert!(members[0].dir.exists());
    let tasks = cmd.tasks().await;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].members.len(), 1);
    assert_eq!(tasks[0].members[0].name, "api");
    match cmd.request(&Request::List).await {
        Response::Agents { agents } => assert!(agents.iter().all(|a| a.id != web_id)),
        other => panic!("expected Agents, got {other:?}"),
    }
    let events = watcher.events_within(300).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TaskUpdated { task } if task.members.len() == 1)),
        "{events:?}"
    );
    assert!(!events
        .iter()
        .any(|e| matches!(e, Event::TaskDropped { .. })));

    let err = error_message(cmd.request(&drop("feat/nope", false)).await);
    assert!(err.contains("no member 'nope'"), "{err}");

    // The last member takes the task with it.
    match cmd.request(&drop("feat/api", false)).await {
        Response::Dropped { warnings } => {
            assert!(
                warnings.iter().any(|w| w.contains("last member")),
                "{warnings:?}"
            );
        }
        other => panic!("expected Dropped, got {other:?}"),
    }
    assert!(cmd.tasks().await.is_empty());
    assert!(!members[0].dir.exists());
}
