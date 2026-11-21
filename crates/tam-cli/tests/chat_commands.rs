//! Chats from the command line (#66): `tam chat` starts one (detached here:
//! no terminal), `tam ps` lists it, `tam rename`/`keep`/`close` change it,
//! and `run`, `stop`, `wait`, `result`, `history`, `msg` take it by its id
//! or its name. A name a task has names the task; `tam chat --session`
//! takes up a closed chat's session (its file in the repository's project
//! directory), and a chat whose agent cannot start is closed again.
//!
//! The real `tam` runs against an in-process daemon, with temporary XDG
//! directories and HOME, and a fake `claude` on its PATH (it writes its
//! arguments, then waits); the agent's hooks are played by the test. Kept
//! to a single test: it sets process environment (HOME, XDG_*,
//! CLAUDE_CONFIG_DIR: the daemon reads its notify config and looks for
//! Claude's transcripts there), which must not race other tests.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use tam_daemon::daemon::Daemon;
use tam_daemon::sessions;
use tam_proto::{Request, Response, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// The session the fake transcript is for.
const SESSION: &str = "5d2e8f4a-1b3c-4d5e-8f90-a1b2c3d4e5f6";

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

/// Play one of Claude's hooks for agent `id`, in session `SESSION`.
async fn hook(stream: &mut UnixStream, id: &str, event: &str, extra: Value) {
    let mut payload = json!({ "hook_event_name": event, "session_id": SESSION });
    payload
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let resp = send(
        stream,
        &Request::HookEvent {
            agent_id: id.into(),
            event: event.into(),
            payload: Some(payload),
        },
    )
    .await;
    assert!(
        matches!(resp, Response::Ok | Response::HookContext { .. }),
        "{event}: {resp:?}"
    );
}

struct Env {
    root: PathBuf,
    bin: PathBuf,
    args: PathBuf,
}

impl Env {
    /// `tam`, isolated in `root`, run in `dir`, the fake `claude` on its
    /// PATH.
    fn tam(&self, dir: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tam"));
        let path = format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap());
        cmd.args(args)
            .env("XDG_RUNTIME_DIR", self.root.join("rt"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("HOME", self.root.join("home"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("PATH", path)
            .env_remove("TAM_AGENT_ID")
            .env_remove("TAM_SOCKET")
            .current_dir(dir)
            .stdin(Stdio::null());
        tam_worktree::git::without_local_env(&mut cmd);
        cmd
    }

    async fn run(&self, dir: &Path, args: &[&str]) -> Output {
        output(self.tam(dir, args)).await
    }

    /// `tam … --json`, which must print one document.
    async fn json(&self, dir: &Path, args: &[&str]) -> (Option<i32>, Value) {
        let out = self.run(dir, args).await;
        let doc = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{args:?}: {e}: {} {}", stdout(&out), stderr(&out)));
        (out.status.code(), doc)
    }

    /// The arguments the fake `claude` of agent `id` was started with (a
    /// previous start's are removed first by the caller).
    async fn started_with(&self, id: &str) -> Vec<String> {
        let file = self.args.join(id);
        for _ in 0..500 {
            if let Ok(text) = std::fs::read_to_string(&file) {
                return text.lines().map(str::to_string).collect();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the agent of {id} did not start");
    }
}

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

/// The value after `flag` in a command line.
fn after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let at = args.iter().position(|a| a == flag)?;
    args.get(at + 1).map(String::as_str)
}

/// Write tam's config where `tam` reads it: `$XDG_CONFIG_HOME/tam`.
fn write_config(root: &Path, text: &str) {
    let dir = root.join("config").join("tam");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), text).unwrap();
}

/// The chats `tam ps --json` lists.
async fn chats(env: &Env, dir: &Path) -> Vec<Value> {
    let (code, doc) = env.json(dir, &["ps", "--json"]).await;
    assert_eq!(code, Some(0), "{doc}");
    doc["chats"].as_array().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chats_from_the_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    // The in-process daemon's own environment: nothing of the real user's.
    std::env::set_var("HOME", root.join("home"));
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    std::env::set_var("XDG_DATA_HOME", root.join("data"));
    std::env::set_var("CLAUDE_CONFIG_DIR", root.join("claude"));
    let env = Env {
        bin: root.join("bin"),
        args: root.join("args"),
        root: root.clone(),
    };
    fake_claude(&env.bin, &env.args);

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

    let api = root.join("src").join("api");
    std::fs::create_dir_all(&api).unwrap();
    git(&api, &["init", "-q"]);
    git(&api, &["commit", "-q", "--allow-empty", "-m", "init"]);
    // Claude's file of a session run in the repository's main checkout.
    let transcript = sessions::session_file(&root.join("claude"), &api, SESSION).unwrap();
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{}\n").unwrap();
    let worktree = root.join("w").join("api--feat");
    std::fs::create_dir_all(&worktree).unwrap();
    let member = match send(
        &mut stream,
        &Request::CreateTask {
            name: "feat".into(),
            members: vec![tam_proto::NewMember {
                repo_root: api.clone(),
                branch: "feat".into(),
                dir: worktree,
            }],
        },
    )
    .await
    {
        Response::Task { task, .. } => task.members[0].id.clone(),
        other => panic!("expected Task, got {other:?}"),
    };

    // Not in a repository: nothing to start a chat on.
    let out = env.run(&root, &["chat", "-d"]).await;
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("not inside a git repository"));

    // Started, detached: a receipt; the agent is Claude, in a new session,
    // in the main checkout (named by a path from elsewhere).
    let (code, receipt) = env.json(&root, &["chat", "src/api", "-d", "--json"]).await;
    assert_eq!(code, Some(0), "{receipt}");
    let id = receipt["chat"].as_str().unwrap().to_string();
    assert!(id.starts_with("c-"), "{receipt}");
    assert!(receipt.get("task").is_none(), "{receipt}");
    let started = &receipt["members"][0];
    assert_eq!(started["id"], id.as_str());
    assert!(started["name"].is_null());
    assert_eq!(started["label"], id.as_str());
    assert_eq!(started["repo"], "api");
    assert_eq!(started["dir"], api.display().to_string());
    assert_eq!(started["agent"], "claude");
    assert!(started["resumed_session"].is_null());
    assert!(started.get("branch").is_none());
    assert!(receipt["error"].is_null());
    let args = env.started_with(&id).await;
    assert!(after(&args, "--resume").is_none(), "{args:?}");

    // Listed: its own table, before the tasks; in JSON, `chats`.
    let out = env.run(&api, &["ps"]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    let shown = stdout(&out);
    let lines: Vec<&str> = shown.lines().collect();
    assert!(
        lines[0].starts_with("STATUS") && lines[0].contains("CHAT"),
        "{shown}"
    );
    assert!(lines[1].contains(&id), "{shown}");
    // Autoclean's default: three days without a turn.
    assert!(lines[0].contains("CLOSES"), "{shown}");
    assert!(lines[1].contains("in 3d"), "{shown}");
    assert_eq!(lines[2], "", "{shown}");
    assert!(lines[3].contains("TASK"), "{shown}");
    assert!(lines[4].contains("feat"), "{shown}");
    let listed = chats(&env, &api).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], id.as_str());
    assert_eq!(listed[0]["agent"], "claude");
    assert_eq!(listed[0]["keep"], false);
    assert!(listed[0]["closes_at"].as_u64().is_some(), "{}", listed[0]);
    assert!(listed[0]["title"].is_null());

    // Named; never like a task, and only a chat is renamed.
    let out = env.run(&api, &["rename", &id, "debug"]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), format!("Chat {id} is named 'debug'.\n"));
    let out = env.run(&api, &["rename", "debug", "feat"]).await;
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("would name task 'feat'"),
        "{}",
        stderr(&out)
    );
    let out = env.run(&api, &["rename", "feat", "x"]).await;
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("'feat' is a task, not a chat"),
        "{}",
        stderr(&out)
    );
    let (code, doc) = env.json(&api, &["rename", "nope", "x", "--json"]).await;
    assert_eq!(code, Some(3));
    assert_eq!(doc["error"]["kind"], "not_found");
    let out = env.run(&api, &["chat", "-n", "feat", "-d"]).await;
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("would name task 'feat'"),
        "{}",
        stderr(&out)
    );
    assert_eq!(chats(&env, &api).await.len(), 1, "no chat made");

    // A turn of the user's, then its response.
    hook(
        &mut stream,
        &id,
        "SessionStart",
        json!({ "source": "startup" }),
    )
    .await;
    hook(
        &mut stream,
        &id,
        "UserPromptSubmit",
        json!({ "prompt": "why is CI red?" }),
    )
    .await;
    hook(
        &mut stream,
        &id,
        "Stop",
        json!({ "last_assistant_message": "a flaky test" }),
    )
    .await;
    let (code, doc) = env
        .json(&api, &["wait", "debug", "-t", "10", "--json"])
        .await;
    assert_eq!(code, Some(0), "{doc}");
    assert_eq!(doc["chat"], id.as_str());
    assert_eq!(doc["members"][0]["state"], "ready");
    assert_eq!(doc["members"][0]["name"], "debug");
    let out = env.run(&api, &["result", &id]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "a flaky test\n");
    let (code, doc) = env.json(&api, &["result", "debug", "--json"]).await;
    assert_eq!(code, Some(0), "{doc}");
    assert_eq!(doc["members"][0]["response"], "a flaky test");
    assert_eq!(doc["members"][0]["session_id"], SESSION);

    // Messages: from the chat to the task member, and back.
    let mut cmd = env.tam(&api, &["msg", "feat", "status?"]);
    cmd.env("TAM_AGENT_ID", &id);
    let out = output(cmd).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("to 'feat'"), "{}", stdout(&out));
    let mut cmd = env.tam(&root, &["msg", &id, "green now"]);
    cmd.env("TAM_AGENT_ID", &member);
    let out = output(cmd).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains(&format!("to '{id}'")),
        "{}",
        stdout(&out)
    );

    // Its history: the run, both messages.
    let out = env.run(&api, &["history", "debug"]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    let history = stdout(&out);
    assert!(history.contains("debug  claude started"), "{history}");
    assert!(history.contains(&format!("{id} → feat")), "{history}");
    assert!(history.contains(&format!("feat → {id}")), "{history}");
    let (_, doc) = env.json(&api, &["history", &id, "--json"]).await;
    assert_eq!(doc["chat"], id.as_str());
    assert_eq!(doc["messages"].as_array().unwrap().len(), 2);

    // Stopped: the chat stays; then resumed in its session, as it was.
    let out = env.run(&api, &["stop", "debug"]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        format!("Stopped the agent of chat 'debug' ({id})\n")
    );
    let out = env.run(&api, &["stop", "debug"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("no agent running in chat"),
        "{}",
        stderr(&out)
    );
    let out = env.run(&api, &["attach", "debug"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("no agent running in chat"),
        "{}",
        stderr(&out)
    );
    // Its last response outlives its agent.
    let out = env.run(&api, &["result", "debug"]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "a flaky test\n");
    for flags in [&["--new-session"][..], &["--session", "other"][..]] {
        let mut args = vec!["run", "debug", "-d"];
        args.extend(flags);
        let out = env.run(&api, &args).await;
        assert_eq!(out.status.code(), Some(1), "{flags:?}");
        assert!(
            stderr(&out).contains("resumes its own session"),
            "{flags:?}: {}",
            stderr(&out)
        );
    }
    std::fs::remove_file(env.args.join(&id)).unwrap();
    let (code, receipt) = env.json(&api, &["run", "debug", "-d", "--json"]).await;
    assert_eq!(code, Some(0), "{receipt}");
    assert_eq!(receipt["chat"], id.as_str());
    assert_eq!(receipt["members"][0]["resumed_session"], SESSION);
    let args = env.started_with(&id).await;
    assert_eq!(after(&args, "--resume"), Some(SESSION), "{args:?}");

    // Its session gone (Claude deleted the transcript): the way out is
    // another chat.
    let out = env.run(&api, &["stop", "debug"]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    let saved = std::fs::read(&transcript).unwrap();
    std::fs::remove_file(&transcript).unwrap();
    let out = env.run(&api, &["run", "debug", "-d"]).await;
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!(
            "close this one (tam close {id}) and start another"
        )),
        "{}",
        stderr(&out)
    );
    std::fs::write(&transcript, saved).unwrap();

    // The task commands say it is a chat.
    for args in [
        &["repair", "debug"][..],
        &["brief", "debug"][..],
        &["split", "debug"][..],
        &["merge", "feat", "debug"][..],
        &["drop", &id][..],
    ] {
        let out = env.run(&api, args).await;
        assert_eq!(out.status.code(), Some(3), "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("is a chat") || stderr(&out).contains("is chat"),
            "{args:?}: {}",
            stderr(&out)
        );
    }

    // Kept, then not.
    let out = env.run(&api, &["keep", "debug"]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    let kept = &chats(&env, &api).await[0];
    assert_eq!(kept["keep"], true);
    assert!(kept["closes_at"].is_null(), "{kept}");
    let out = env.run(&api, &["ps"]).await;
    assert!(stdout(&out).lines().nth(1).unwrap().contains("kept"));
    let (code, doc) = env.json(&api, &["keep", &id, "--off", "--json"]).await;
    assert_eq!(code, Some(0));
    assert_eq!(doc["keep"], false);
    assert_eq!(doc["open"], true);

    // Not a task: drop says what to do instead.
    let out = env.run(&api, &["drop", "debug"]).await;
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(stderr(&out).contains("tam close"), "{}", stderr(&out));

    // A task named like a chat later: the task comes first, the chat is
    // reached by its id.
    let (_, made) = env
        .json(&api, &["chat", "-n", "later", "-d", "--json"])
        .await;
    let later = made["chat"].as_str().unwrap().to_string();
    let worktree = root.join("w").join("api--later");
    std::fs::create_dir_all(&worktree).unwrap();
    let resp = send(
        &mut stream,
        &Request::CreateTask {
            name: "later".into(),
            members: vec![tam_proto::NewMember {
                repo_root: api.clone(),
                branch: "later".into(),
                dir: worktree,
            }],
        },
    )
    .await;
    assert!(matches!(resp, Response::Task { .. }), "{resp:?}");
    let (_, doc) = env.json(&api, &["result", "later", "--json"]).await;
    assert_eq!(doc["task"], "later", "{doc}");
    let (_, doc) = env.json(&api, &["result", &later, "--json"]).await;
    assert_eq!(doc["chat"], later.as_str(), "{doc}");

    // Closing asks first: refused without a terminal, unless --yes.
    let out = env.run(&api, &["close", "debug"]).await;
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("not closed"), "{}", stderr(&out));
    assert_eq!(chats(&env, &api).await.len(), 2);
    let out = env.run(&api, &["close", "feat", "-y"]).await;
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let out = env.run(&api, &["close", "debug", "--yes"]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).starts_with(&format!(
            "Closed chat 'debug' ({id}). To make it again: tam chat "
        )),
        "{}",
        stdout(&out)
    );
    assert!(stdout(&out).ends_with(&format!("--session {SESSION}\n")));
    let listed = chats(&env, &api).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], later.as_str());
    let (code, doc) = env.json(&api, &["close", &later, "-y", "--json"]).await;
    assert_eq!(code, Some(0));
    assert_eq!(doc["chat"], later.as_str());
    assert_eq!(doc["open"], false);

    // Made again from its session: its agent resumes it.
    let (code, receipt) = env
        .json(
            &api,
            &["chat", "--session", SESSION, "-n", "again", "-d", "--json"],
        )
        .await;
    assert_eq!(code, Some(0), "{receipt}");
    let again = receipt["chat"].as_str().unwrap().to_string();
    assert_ne!(again, id);
    assert_eq!(receipt["members"][0]["resumed_session"], SESSION);
    let args = env.started_with(&again).await;
    assert_eq!(after(&args, "--resume"), Some(SESSION), "{args:?}");

    // A session Claude has no file of in this repository: no chat.
    let (code, doc) = env
        .json(
            &api,
            &["chat", "--session", "no-such-session", "-d", "--json"],
        )
        .await;
    assert_eq!(code, Some(1));
    assert_eq!(doc["error"]["kind"], "failed");
    assert!(
        doc["error"]["message"]
            .as_str()
            .unwrap()
            .contains("has no session no-such-session"),
        "{doc}"
    );
    assert_eq!(chats(&env, &api).await.len(), 1);

    // An agent that cannot start (its wrapper is missing): the chat is
    // closed again, it never ran.
    write_config(&root, "[spawn]\nwrapper = [\"/nonexistent/tam-wrapper\"]\n");
    let out = env.run(&api, &["chat", "-n", "broken", "-d"]).await;
    write_config(&root, "");
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("closed again"), "{}", stderr(&out));
    let listed = chats(&env, &api).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], again.as_str());
    // Its name is free again.
    let (code, made) = env
        .json(&api, &["chat", "-n", "broken", "-d", "--json"])
        .await;
    assert_eq!(code, Some(0), "{made}");
    let broken = made["chat"].as_str().unwrap().to_string();

    for id in [again, broken] {
        let _ = send(&mut stream, &Request::Kill { id }).await;
    }
}
