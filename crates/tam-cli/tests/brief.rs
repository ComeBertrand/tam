//! End to end: a task's brief from the command line. `tam new --brief`
//! with a config pattern (a ticket id into its URL and the agents' start
//! prompt), `tam brief` to show and change it, and `tam drop` keeping a
//! brief the members wrote in. The real binary against a daemon, on
//! throwaway git repositories, with a fake `claude`.
//!
//! Kept to a single test: it sets `PATH` (the fake `claude` the daemon
//! starts), which must not race other tests in this binary. Every command
//! runs with temporary XDG directories and HOME, and without the caller's
//! `TAM_AGENT_ID`/`TAM_SOCKET`: it can never reach a real daemon.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;
use tam_daemon::daemon::Daemon;
use tokio::net::UnixStream;

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
    cmd
}

async fn output(mut cmd: Command) -> Output {
    tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Run `tam` to success; its stdout.
async fn ok(root: &Path, cwd: &Path, args: &[&str]) -> String {
    let out = output(tam(root, cwd, args)).await;
    assert!(out.status.success(), "tam {args:?}: {}", text(&out.stderr));
    text(&out.stdout)
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
}

fn repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["commit", "-q", "--allow-empty", "-m", "init"]);
}

/// The arguments the fake claude of member `id` was started with.
async fn claude_args(root: &Path, id: &str) -> Vec<String> {
    let path = root.join(format!("args-{id}"));
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(&path) {
            return s.lines().map(str::to_string).collect();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the agent of {id} never started");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_brief_from_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let root = &tmp.path().canonicalize().unwrap();

    // A fake claude that writes down its arguments and stays up.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(
        bin.join("claude"),
        format!(
            "#!/bin/sh\nf='{}/args-'\"$TAM_AGENT_ID\"\n\
             for a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$f.tmp\"\n\
             mv \"$f.tmp\" \"$f\"\nexec sleep 3600\n",
            root.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        bin.join("claude"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    std::env::set_var(
        "PATH",
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
    );

    let sock = root.join("rt").join("tam").join("sock");
    let daemon = Daemon::new(sock.clone(), root.join("ledger.jsonl"));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    while UnixStream::connect(&sock).await.is_err() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let tasks_dir = root.join("tasks");

    // Where tam reads its config: $XDG_CONFIG_HOME, or on macOS
    // ~/Library/Application Support.
    let macos = root
        .join("home")
        .join("Library")
        .join("Application Support");
    for config in [root.join("config"), macos] {
        let config = config.join("tam");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("config.toml"),
            r#"
[[brief.patterns]]
match = '^CS-(\d+)$'
reference = "https://tickets.example/CS-{1}"
prompt = "/implement-task {brief}"
"#,
        )
        .unwrap();
    }
    let src = root.join("home").join("src");
    repo(&src.join("api"));
    repo(&src.join("web"));

    // A ticket id: its URL is the brief, and each agent starts on the
    // pattern's prompt, knowing the brief.
    let out = ok(
        root,
        &src,
        &[
            "new", "feat", "-r", "api", "-r", "web", "--brief", "CS-42", "-d",
        ],
    )
    .await;
    assert!(out.contains("Started claude in 'feat/api'"), "{out}");
    let url = "https://tickets.example/CS-42";
    let json: Value =
        serde_json::from_str(&ok(root, &src, &["brief", "feat", "--json"]).await).unwrap();
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["task"], "feat");
    assert_eq!(json["reference"], url);
    assert_eq!(json["file"], Value::Null);
    let scratch = tasks_dir.join("feat").join("scratch");
    assert_eq!(json["scratch"], scratch.to_str().unwrap());
    assert!(scratch.is_dir());
    let ps: Value = serde_json::from_str(&ok(root, &src, &["ps", "--json"]).await).unwrap();
    let members = ps["tasks"][0]["members"].as_array().unwrap();
    let ids: Vec<&str> = members.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 2, "{ps}");
    for id in &ids {
        let args = claude_args(root, id).await;
        assert_eq!(args.len(), 3, "{args:?}");
        assert_eq!(args[0], "--settings");
        assert_eq!(args[2], format!("/implement-task {url}"));
        let settings: Value = serde_json::from_str(&args[1]).unwrap();
        let dirs = settings["permissions"]["additionalDirectories"].clone();
        assert_eq!(dirs[0], tasks_dir.join("feat").to_str().unwrap());
        assert_eq!(dirs.as_array().unwrap().len(), 2, "{settings}");
    }

    // From a worktree, the task's brief; changed to a path, made absolute.
    let api_wt = root.join("home").join("worktrees").join("api--feat");
    let out = ok(root, &api_wt, &["brief"]).await;
    assert_eq!(
        out,
        format!(
            "Brief of task feat: {url}\nScratch: {}\n",
            scratch.display()
        )
    );
    std::fs::write(api_wt.join("spec.md"), "the spec").unwrap();
    let out = ok(root, &api_wt, &["brief", "--set", "spec.md"]).await;
    assert!(
        out.starts_with(&format!(
            "Brief of task feat: {}\n",
            api_wt.join("spec.md").display()
        )),
        "{out}"
    );

    // Back to the local file, made from the template.
    let out = ok(root, &src, &["brief", "feat/web", "--local"]).await;
    let file = tasks_dir.join("feat").join("brief.md");
    assert!(
        out.starts_with(&format!("Brief of task feat: {}\n", file.display())),
        "{out}"
    );
    let template = std::fs::read_to_string(&file).unwrap();
    assert!(template.contains("\n## Shared\n"), "{template}");

    // Unknown tasks; --brief is for a new task only.
    for name in ["nope", "feat/nope"] {
        let out = output(tam(root, &src, &["brief", name, "--set", "x"])).await;
        assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    }
    let out = output(tam(
        root,
        &src,
        &["new", "feat", "-r", "api", "--join", "--brief", "x"],
    ))
    .await;
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));

    // Dropped: a brief the members wrote in is kept, and tam says where.
    std::fs::write(&file, format!("{template}\nAgreed: POST /items\n")).unwrap();
    let out = output(tam(root, &src, &["drop", "feat", "-f"])).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    let kept: Vec<PathBuf> = std::fs::read_dir(tasks_dir.join(".dropped"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(kept.len(), 1);
    assert!(
        stderr.contains(&format!(
            "warning: the brief of task 'feat' is kept at {}",
            kept[0].display()
        )),
        "{stderr}"
    );
    assert!(!tasks_dir.join("feat").exists());
}
