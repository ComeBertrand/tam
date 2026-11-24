//! Shells from the command line (#83): `tam shell` opens one (detached
//! here: no terminal) on a task member's worktree, on a main checkout with
//! `-r`, or where the current directory is; `tam ps` lists them (below
//! their member, or in a table of their own) and `tam ps --json` has them
//! under `shells`; `tam stop` closes one by its id; the commands that do not
//! take a shell say the id is a shell's; `tam drop` says how many it
//! closes.
//!
//! The real `tam` runs against an in-process daemon, on a real repository
//! and a worktree `tam new` made, with temporary XDG directories and HOME,
//! without `TAM_AGENT_ID`/`TAM_SOCKET`, and `/bin/sh` as `$SHELL`. Kept to a
//! single test: it sets process environment (HOME, XDG_*: the daemon reads
//! its config there), which must not race other tests.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;
use tam_daemon::daemon::Daemon;

struct Env {
    root: PathBuf,
}

impl Env {
    /// `tam`, isolated in `root`, run in `dir`.
    fn tam(&self, dir: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tam"));
        cmd.args(args)
            .env("XDG_RUNTIME_DIR", self.root.join("rt"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("HOME", self.root.join("home"))
            .env("SHELL", "/bin/sh")
            .env_remove("TAM_AGENT_ID")
            .env_remove("TAM_SOCKET")
            .current_dir(dir)
            .stdin(Stdio::null());
        tam_worktree::git::without_local_env(&mut cmd);
        cmd
    }

    async fn run(&self, dir: &Path, args: &[&str]) -> Output {
        let mut cmd = self.tam(dir, args);
        tokio::task::spawn_blocking(move || cmd.output().unwrap())
            .await
            .unwrap()
    }

    /// `tam … --json`, which must print one document.
    async fn json(&self, dir: &Path, args: &[&str]) -> (Option<i32>, Value) {
        let out = self.run(dir, args).await;
        let doc = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{args:?}: {e}: {} {}", stdout(&out), stderr(&out)));
        (out.status.code(), doc)
    }

    /// The shells `tam ps --json` lists.
    async fn shells(&self) -> Vec<Value> {
        let (code, doc) = self.json(&self.root, &["ps", "--json"]).await;
        assert_eq!(code, Some(0), "{doc}");
        doc["shells"].as_array().unwrap().clone()
    }

    /// Write tam's config where `tam` reads it: `$XDG_CONFIG_HOME/tam`.
    fn config(&self, extra: &str) {
        let dir = self.root.join("config").join("tam");
        std::fs::create_dir_all(&dir).unwrap();
        let text = format!(
            "[worktree]\nroot = \"{}\"\n{extra}",
            self.root.join("wt").display()
        );
        std::fs::write(dir.join("config.toml"), text).unwrap();
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
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

/// The id of the shell a `tam shell --json` receipt is about.
fn opened(code: Option<i32>, doc: &Value) -> String {
    assert_eq!(code, Some(0), "{doc}");
    doc["shell"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shells_from_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    // The in-process daemon's own environment: nothing of the real user's.
    std::env::set_var("HOME", root.join("home"));
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    std::env::set_var("XDG_DATA_HOME", root.join("data"));
    let env = Env { root: root.clone() };
    env.config("");

    let sock = root.join("rt").join("tam").join("sock");
    let daemon = Daemon::new(sock.clone(), root.join("ledger.jsonl"));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    while tokio::net::UnixStream::connect(&sock).await.is_err() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let src = root.join("home").join("src");
    let api = src.join("api");
    std::fs::create_dir_all(&api).unwrap();
    git(&api, &["init", "-q", "-b", "main"]);
    git(&api, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let out = env
        .run(&src, &["new", "feat", "-r", "api", "--no-start"])
        .await;
    assert!(out.status.success(), "{}", stderr(&out));
    let worktree = root.join("wt").join("api--feat");
    assert!(worktree.join(".git").is_file());

    // On the task's worktree, from it: a receipt.
    let (code, doc) = env.json(&worktree, &["shell", "-d", "--json"]).await;
    let first = opened(code, &doc);
    assert!(first.starts_with("sh-"), "{doc}");
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["name"], "Shell #1");
    assert_eq!(doc["number"], 1);
    assert_eq!(doc["task"], "feat");
    assert!(doc["member"].as_str().unwrap().starts_with("m-"), "{doc}");
    assert_eq!(doc["repo"], "api");
    assert_eq!(doc["dir"], worktree.display().to_string());
    assert!(doc["pid"].as_u64().is_some(), "{doc}");
    // Named, from elsewhere: the next number there.
    let (code, doc) = env.json(&root, &["shell", "feat", "-d", "--json"]).await;
    let second = opened(code, &doc);
    assert_eq!(doc["name"], "Shell #2");
    assert_eq!(doc["dir"], worktree.display().to_string());

    // On the main checkout, named as tam ls shows it (found under HOME): a
    // place of its own, numbered from 1.
    let home = root.join("home");
    let (code, doc) = env
        .json(&home, &["shell", "-r", "api", "-d", "--json"])
        .await;
    let on_checkout = opened(code, &doc);
    assert_eq!(doc["name"], "Shell #1");
    assert!(doc["task"].is_null() && doc["member"].is_null(), "{doc}");
    assert_eq!(doc["dir"], api.display().to_string());
    // From a directory in it, without -d's JSON: the id alone on stdout, how
    // to attach on stderr.
    std::fs::create_dir_all(api.join("lib")).unwrap();
    let out = env.run(&api.join("lib"), &["shell", "-d"]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    let here = stdout(&out).trim_end().to_string();
    assert!(here.starts_with("sh-") && !here.contains(' '), "{here}");
    assert!(
        stderr(&out).contains(&format!(
            "Opened Shell #2 of api in ~/src/api. Attach with: tam attach {here}"
        )),
        "{}",
        stderr(&out)
    );

    // A worktree no task has: refused, saying both ways out. Nowhere to
    // open one: refused too. An unknown task: not found.
    let loose = root.join("wt").join("api--loose");
    git(
        &api,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "loose",
            loose.to_str().unwrap(),
        ],
    );
    let out = env.run(&loose, &["shell", "-d"]).await;
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("tam adopt") && stderr(&out).contains("tam shell -r ."),
        "{}",
        stderr(&out)
    );
    let out = env.run(&root, &["shell", "-d"]).await;
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("no task or repository"),
        "{}",
        stderr(&out)
    );
    let (code, doc) = env.json(&root, &["shell", "nope", "-d", "--json"]).await;
    assert_eq!(code, Some(3), "{doc}");
    assert_eq!(doc["error"]["kind"], "not_found");
    let out = env.run(&root, &["shell", "-r", "nope", "-d"]).await;
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(env.shells().await.len(), 4, "nothing more opened");

    // Listed: the main checkout's in a table of their own before the tasks,
    // the worktree's below their task; each with its id.
    let out = env.run(&root, &["ps"]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    let shown = stdout(&out);
    let lines: Vec<&str> = shown.lines().collect();
    assert!(
        lines[0].starts_with("SHELL") && lines[0].contains("ID"),
        "{shown}"
    );
    assert!(lines[1].starts_with("Shell #1") && lines[1].contains(&on_checkout));
    assert!(lines[1].ends_with("~/src/api"), "{shown}");
    assert!(lines[2].starts_with("Shell #2") && lines[2].contains(&here));
    assert_eq!(lines[3], "", "{shown}");
    assert!(lines[4].contains("TASK"), "{shown}");
    assert!(lines[5].contains("feat"), "{shown}");
    assert!(
        lines[6].trim_start() == format!("├─ Shell #1 ({first})"),
        "{shown}"
    );
    assert!(
        lines[7].trim_start() == format!("└─ Shell #2 ({second})"),
        "{shown}"
    );
    let shells = env.shells().await;
    let ids: Vec<&str> = shells.iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(ids, [&first, &second, &on_checkout, &here], "opening order");
    assert_eq!(shells[0]["task"], "feat");
    assert_eq!(shells[0]["viewers"], 0);
    assert!(shells[2]["task"].is_null() && shells[2]["member"].is_null());

    // The commands that do not take a shell say it is one.
    for args in [
        &["run", &first, "-d"][..],
        &["msg", &first, "hello"][..],
        &["wait", &first, "-t", "1"][..],
        &["result", &first][..],
        &["history", &first][..],
        &["drop", &first][..],
        &["merge", "feat", &first][..],
        &["close", &first, "-y"][..],
        &["shell", &first, "-d"][..],
    ] {
        let out = env.run(&root, args).await;
        assert_eq!(out.status.code(), Some(3), "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains(&format!("'{first}' is a shell (Shell #1)")),
            "{args:?}: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains(&format!("tam stop {first} closes it")),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    for args in [
        &["run", &first, "-d", "--json"][..],
        &["wait", &first, "--json"][..],
        &["result", &first, "--json"][..],
    ] {
        let (code, doc) = env.json(&root, args).await;
        assert_eq!(code, Some(3), "{args:?}: {doc}");
        assert_eq!(doc["error"]["kind"], "not_found", "{args:?}: {doc}");
    }
    // Attach takes it, and needs a terminal for it like an agent's.
    let out = env.run(&root, &["attach", &first]).await;
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("stdin is not a terminal"),
        "{}",
        stderr(&out)
    );

    // Closed by its id; gone then.
    let out = env.run(&root, &["stop", &first]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), format!("Closed Shell #1 of feat ({first})\n"));
    let out = env.run(&root, &["stop", &first]).await;
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!("no open shell '{first}'")),
        "{}",
        stderr(&out)
    );
    let out = env.run(&root, &["attach", &first]).await;
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert_eq!(env.shells().await.len(), 3);

    // Through [spawn] wrapper, read at each open, in the worktree: the
    // highest number open there plus one.
    let wrap = root.join("wrap");
    let mark = root.join("wrapped");
    std::fs::write(
        &wrap,
        format!("#!/bin/sh\npwd > '{}'\nexec \"$@\"\n", mark.display()),
    )
    .unwrap();
    std::fs::set_permissions(&wrap, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    env.config(&format!("[spawn]\nwrapper = [\"{}\"]\n", wrap.display()));
    let (code, doc) = env.json(&root, &["shell", "feat", "-d", "--json"]).await;
    let third = opened(code, &doc);
    env.config("");
    assert_eq!(doc["name"], "Shell #3");
    for _ in 0..500 {
        if mark.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        std::fs::read_to_string(&mark).unwrap().trim_end(),
        worktree.display().to_string()
    );

    // A drop closes the task's shells, and says so.
    let out = env.run(&root, &["drop", "feat"]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "Dropped task 'feat' and closed its 2 shells\n"
    );
    let ids: Vec<String> = env
        .shells()
        .await
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, [on_checkout.clone(), here.clone()]);
    assert!(!ids.contains(&second) && !ids.contains(&third));

    for id in [on_checkout, here] {
        let out = env.run(&root, &["stop", &id]).await;
        assert!(out.status.success(), "{}", stderr(&out));
        assert!(
            stdout(&out).starts_with("Closed Shell #"),
            "{}",
            stdout(&out)
        );
        assert!(stdout(&out).contains(" of api "), "{}", stdout(&out));
    }
    assert!(env.shells().await.is_empty());
}
