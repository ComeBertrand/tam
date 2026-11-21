//! End to end: `tam new` across several repositories, run as the real binary
//! against a daemon, on throwaway git repositories.
//!
//! The daemon listens where the CLI looks for it (`$XDG_RUNTIME_DIR/tam/sock`).
//! Every command runs with temporary XDG directories and HOME, and without
//! the caller's `TAM_AGENT_ID`/`TAM_SOCKET`: it can never reach a real daemon.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use tam_daemon::daemon::Daemon;
use tam_proto::{Request, Response, ServerMessage, TaskInfo};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

async fn send(stream: &mut UnixStream, request: &Request) -> Response {
    let mut json = serde_json::to_string(request).unwrap();
    json.push('\n');
    stream.write_all(json.as_bytes()).await.unwrap();
    let mut reader = BufReader::new(&mut *stream);
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        match serde_json::from_str::<ServerMessage>(line.trim()).unwrap() {
            ServerMessage::Response(resp) => return resp,
            ServerMessage::Event(_) => continue,
        }
    }
}

async fn tasks(stream: &mut UnixStream) -> Vec<TaskInfo> {
    match send(stream, &Request::ListTasks).await {
        Response::Tasks { tasks } => tasks,
        other => panic!("expected Tasks, got {other:?}"),
    }
}

/// `tam` in `cwd`, isolated in `root`.
fn tam(root: &Path, cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tam"));
    cmd.args(args)
        .current_dir(cwd)
        .env("XDG_RUNTIME_DIR", root.join("rt"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOME", root.join("home"))
        .env_remove("TAM_AGENT_ID")
        .env_remove("TAM_SOCKET")
        .stdin(Stdio::null());
    tam_worktree::git::without_local_env(&mut cmd);
    cmd
}

/// Run a command to completion off the async runtime.
async fn output(mut cmd: Command) -> Output {
    tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn git(dir: &Path, args: &[&str]) {
    let out = tam_worktree::git::command()
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
}

fn repo(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "-b", "main"]);
    git(
        path,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    );
    path.canonicalize().unwrap()
}

/// Give `repo` a `post-checkout` hook (it runs in each new worktree).
fn hook(repo: &Path, script: &str) {
    let hooks = repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    git(repo, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
    let hook = hooks.join("post-checkout");
    std::fs::write(&hook, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_task_across_repositories() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let sock = root.join("rt").join("tam").join("sock");
    let daemon = Daemon::new(sock.clone(), root.join("ledger.jsonl"));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    let mut stream = loop {
        if let Ok(s) = UnixStream::connect(&sock).await {
            break s;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    // The worktree root, in tam's config: $XDG_CONFIG_HOME/tam.
    let wt = root.join("wt");
    std::fs::create_dir_all(root.join("config/tam")).unwrap();
    std::fs::write(
        root.join("config/tam/config.toml"),
        format!("[worktree]\nroot = \"{}\"\n", wt.display()),
    )
    .unwrap();
    let home = root.join("home");
    let src = home.join("src");
    let (api, web, docs) = (
        repo(&src.join("api")),
        repo(&src.join("web")),
        repo(&src.join("docs")),
    );

    // Two repositories, named by path. Git runs in this command's
    // environment, not the daemon's: web's checkout hook sees it.
    let mark = root.join("mark");
    hook(
        &web,
        &format!("echo \"${{TAM_TEST_MARK:-unset}}\" > '{}'", mark.display()),
    );
    let mut new = tam(
        &root,
        &src,
        &["new", "feat", "-r", "api", "-r", "./web", "--no-start"],
    );
    new.env("TAM_TEST_MARK", "from tam new");
    let out = output(new).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&mark).unwrap(), "from tam new\n");
    let stdout = text(&out.stdout);
    assert!(stdout.contains("Created task 'feat':"), "{stdout}");
    assert!(
        stdout.contains("feat/api") && stdout.contains("feat/web"),
        "{stdout}"
    );
    assert!(wt.join("api--feat/.git").is_file());
    assert!(wt.join("web--feat/.git").is_file());

    // A third joins, named as `tam ls` shows it (found under HOME).
    let out = tam(
        &root,
        &home,
        &["new", "feat", "-r", "docs", "--join", "--no-start"],
    );
    let out = output(out).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("Added to task 'feat':"));
    let task = tasks(&mut stream).await.remove(0);
    let names: Vec<&str> = task.members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["api", "web", "docs"]);
    let roots: Vec<_> = task
        .members
        .iter()
        .map(|m| m.repo_root.clone().unwrap())
        .collect();
    assert_eq!(roots, [api.clone(), web, docs.clone()]);

    // An existing name needs --join; a task name alone is not a member.
    let out = output(tam(
        &root,
        &src,
        &["new", "feat", "-r", "api", "--no-start"],
    ))
    .await;
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("--join"),
        "{}",
        text(&out.stderr)
    );
    let out = output(tam(&root, &src, &["run", "feat"])).await;
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("name one as feat/<member>"),
        "{}",
        text(&out.stderr)
    );

    // The second repository fails: the first one's worktree goes too.
    hook(&docs, "exit 1");
    let out = output(tam(
        &root,
        &src,
        &["new", "half", "-r", "api", "-r", "docs", "--no-start"],
    ))
    .await;
    assert!(!out.status.success());
    let stderr = text(&out.stderr);
    assert!(stderr.contains("cannot create task 'half'"), "{stderr}");
    assert!(stderr.contains("Nothing was left behind"), "{stderr}");
    assert!(!wt.join("api--half").exists() && !wt.join("docs--half").exists());
    assert_eq!(tasks(&mut stream).await.len(), 1);

    // One member dropped alone; the last one takes the task.
    let out = output(tam(&root, &src, &["drop", "feat/docs"])).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "Dropped 'feat/docs'\n");
    assert!(!wt.join("docs--feat").exists() && wt.join("api--feat").exists());
    let names: Vec<String> = tasks(&mut stream).await[0]
        .members
        .iter()
        .map(|m| m.name.clone())
        .collect();
    assert_eq!(names, ["api", "web"]);
    let out = output(tam(&root, &src, &["drop", "feat"])).await;
    assert_eq!(text(&out.stdout), "Dropped task 'feat'\n");
    assert!(tasks(&mut stream).await.is_empty());
}
