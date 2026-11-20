//! Git run by tam, and by the tests, acts on the repository it is pointed
//! at, never on the one an inherited `GIT_DIR` names (#57).
//!
//! `git rebase --exec` and hooks run from a linked worktree export `GIT_DIR`
//! (hooks also an absolute `GIT_INDEX_FILE`): `git rebase -x 'cargo test'`
//! once ran the tests' git on the repository they were run from. Here those
//! variables name a sentinel repository, a linked worktree like every tam
//! worktree, for this whole process (one test in this file: it sets the
//! process environment). The tests' own git, the real `tam` binary started
//! with that environment, and the daemon in this process create, check and
//! drop a task in another repository; the sentinel's files must be byte for
//! byte what they were.
//!
//! The daemon listens where the CLI looks for it (`$XDG_RUNTIME_DIR/tam/sock`).
//! Every command runs with temporary XDG directories and HOME, and without
//! the caller's `TAM_AGENT_ID`/`TAM_SOCKET`: it can never reach a real daemon.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use tam_daemon::daemon::Daemon;
use tokio::net::UnixStream;

/// `tam` in `cwd`, isolated in `root`, with this process's `GIT_*`
/// variables: tam itself must not follow them.
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

/// Run a command to completion off the async runtime.
async fn output(mut cmd: Command) -> Output {
    tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The tests' git: as every test helper builds it.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = tam_worktree::git::command()
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
    text(&out.stdout).trim().to_string()
}

/// A repository with one commit on `main`, set up with the commands that
/// once went to the wrong repository (`git init` there made it bare, `git
/// config user.name` gave it a `[user]` section).
fn repo(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.name", "Test"]);
    git(path, &["config", "user.email", "t@t"]);
    std::fs::write(path.join("README"), path.display().to_string()).unwrap();
    git(path, &["add", "README"]);
    git(path, &["commit", "-q", "-m", "init"]);
    path.canonicalize().unwrap()
}

/// Every file under `dirs`, by path, with its contents: read directly, as
/// git in it could rewrite its index.
fn snapshot(dirs: &[&Path]) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, files);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                files.insert(path, bytes);
            }
        }
    }
    let mut files = BTreeMap::new();
    for dir in dirs {
        walk(dir, &mut files);
    }
    files
}

/// What differs between two snapshots, one line per file (text files with
/// their contents before and after).
fn changes(before: &BTreeMap<PathBuf, Vec<u8>>, after: &BTreeMap<PathBuf, Vec<u8>>) -> String {
    let mut lines = Vec::new();
    let show = |bytes: &[u8]| match std::str::from_utf8(bytes) {
        Ok(s) if s.len() < 2000 => format!("{s:?}"),
        _ => format!("{} bytes", bytes.len()),
    };
    for (path, old) in before {
        match after.get(path) {
            None => lines.push(format!("removed {}", path.display())),
            Some(new) if new != old => lines.push(format!(
                "changed {}: {} -> {}",
                path.display(),
                show(old),
                show(new)
            )),
            Some(_) => {}
        }
    }
    for (path, new) in after {
        if !before.contains_key(path) {
            lines.push(format!("added {}: {}", path.display(), show(new)));
        }
    }
    lines.join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_never_follows_an_inherited_git_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();

    // The sentinel: a repository with a remote (a stray `git fetch` would
    // bring its branch), and a linked worktree whose git directory is what
    // git exports from there.
    let upstream = repo(&root.join("upstream"));
    git(&upstream, &["branch", "upstream-only"]);
    let sentinel = repo(&root.join("sentinel"));
    git(
        &sentinel,
        &["remote", "add", "origin", upstream.to_str().unwrap()],
    );
    let sentinel_wt = root.join("sentinel-wt");
    git(
        &sentinel,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "wt",
            sentinel_wt.to_str().unwrap(),
        ],
    );
    let sentinel_git_dir = PathBuf::from(git(&sentinel_wt, &["rev-parse", "--absolute-git-dir"]));
    let before = snapshot(&[&sentinel, &sentinel_wt]);

    std::env::set_var("GIT_DIR", &sentinel_git_dir);
    std::env::set_var("GIT_INDEX_FILE", sentinel_git_dir.join("index"));

    let sock = root.join("rt").join("tam").join("sock");
    let daemon = Daemon::new(sock.clone(), root.join("ledger.jsonl"));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    while UnixStream::connect(&sock).await.is_err() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The tests' own git: a repository under HOME, with a remote.
    let home = root.join("home");
    let api = repo(&home.join("src/api"));
    let api_origin = root.join("api-origin.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            api.to_str().unwrap(),
            api_origin.to_str().unwrap(),
        ],
    );
    git(&api_origin, &["branch", "upstream-only"]);
    git(
        &api,
        &["remote", "add", "origin", api_origin.to_str().unwrap()],
    );
    let config = std::fs::read_to_string(api.join(".git/config")).unwrap();
    assert!(config.contains("name = Test"), "{config}");
    assert!(config.contains("bare = false"), "{config}");

    // `tam new`, the repository named as `tam ls` shows it: the client
    // finds and fetches it, the daemon creates the worktree and branch.
    let out = output(tam(
        &root,
        &home,
        &["new", "feat", "-r", "api", "--no-start"],
    ))
    .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    let worktree = home.join("worktrees/api--feat");
    assert!(worktree.join(".git").is_file(), "{}", text(&out.stdout));
    assert_eq!(git(&worktree, &["branch", "--show-current"]), "feat");
    git(
        &api,
        &["show-ref", "--verify", "refs/remotes/origin/upstream-only"],
    );

    // `tam ps` has the daemon check the worktree against git.
    let out = output(tam(&root, &home, &["ps", "--json"])).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    let ps: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let member = &ps["tasks"][0]["members"][0];
    assert_eq!(member["health"]["state"], "ok", "{ps}");
    assert_eq!(member["health"]["branch"], "feat", "{ps}");

    // `tam drop -b`: the daemon removes the worktree and deletes the branch.
    let out = output(tam(&root, &home, &["drop", "feat", "-b", "--force"])).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!worktree.exists());
    assert_eq!(git(&api, &["branch", "--list", "feat"]), "");

    let after = snapshot(&[&sentinel, &sentinel_wt]);
    assert!(
        before == after,
        "the sentinel repository changed:\n{}",
        changes(&before, &after)
    );
}
