//! `tam merge` and `tam split` as scripts and people see them, run as the
//! real binary against a daemon: the text, the `--json` document and the
//! exit statuses.
//!
//! The daemon listens where the CLI looks for it (`$XDG_RUNTIME_DIR/tam/sock`).
//! Every command runs with temporary XDG directories and HOME, and without
//! the caller's `TAM_AGENT_ID`/`TAM_SOCKET`: it can never reach a real daemon.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use tam_daemon::daemon::Daemon;
use tam_proto::{Request, Response, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// `tam`, isolated in `root`.
fn tam(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tam"));
    cmd.env("XDG_RUNTIME_DIR", root.join("rt"))
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

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The JSON document a `--json` command printed.
fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}{}", stdout(out), stderr(out)))
}

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

/// A daemon where `tam(root)` finds it, with single-member tasks `names`
/// (each in the repository of its name, on the branch of its name).
async fn daemon_with_tasks(root: &Path, names: &[&str]) -> UnixStream {
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
    for name in names {
        let dir = root.join("work").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let request = Request::CreateTask {
            name: name.to_string(),
            members: vec![tam_proto::NewMember {
                repo_root: PathBuf::from("/src").join(name),
                branch: name.to_string(),
                dir,
            }],
        };
        assert!(matches!(
            send(&mut stream, &request).await,
            Response::Task { .. }
        ));
    }
    stream
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merge_and_split_report_what_moved() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let _daemon = daemon_with_tasks(root, &["back", "front"]).await;

    // Unknown tasks exit 3; a refused merge 1. Nothing changes.
    let out = output({
        let mut cmd = tam(root);
        cmd.args(["merge", "back", "nope", "--json"]);
        cmd
    })
    .await;
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert_eq!(json(&out)["error"]["kind"], "not_found");
    let out = output({
        let mut cmd = tam(root);
        cmd.args(["merge", "back", "back"]);
        cmd
    })
    .await;
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("into itself"), "{}", stderr(&out));

    let out = output({
        let mut cmd = tam(root);
        cmd.args(["merge", "back", "front", "--json"]);
        cmd
    })
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let doc = json(&out);
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["task"], "back");
    assert_eq!(doc["from"], serde_json::json!(["front"]));
    assert!(doc["error"].is_null());
    let members = doc["members"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["label"], "back/back");
    assert_eq!(members[0]["previous_label"], "back");
    assert_eq!(members[0]["moved"], false);
    assert_eq!(members[1]["task"], "back");
    assert_eq!(members[1]["name"], "front");
    assert_eq!(members[1]["label"], "back/front");
    assert_eq!(members[1]["previous_label"], "front");
    assert_eq!(members[1]["moved"], true);
    assert_eq!(members[1]["repo"], "front");
    assert_eq!(members[1]["branch"], "front");
    assert!(members[1]["agent"].is_null());
    assert_eq!(members[1]["notified"], false);
    for key in ["id", "dir"] {
        assert!(members[1][key].is_string(), "{key}: {doc}");
    }

    // The only member of a task cannot be split out of it; an unknown
    // member exits 3.
    let out = output({
        let mut cmd = tam(root);
        cmd.args(["split", "nope/x"]);
        cmd
    })
    .await;
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));

    let out = output({
        let mut cmd = tam(root);
        cmd.args(["split", "back/front"]);
        cmd
    })
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "Split back/front out of back into task front:\n  front  was back/front\n  back   was back/back\n"
    );

    let out = output({
        let mut cmd = tam(root);
        cmd.args(["split", "back", "solo", "--json"]);
        cmd
    })
    .await;
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    let doc = json(&out);
    let message = doc["error"]["message"].as_str().unwrap();
    assert!(message.contains("only member of task 'back'"), "{message}");
    assert_eq!(doc["error"]["kind"], "failed");
}
