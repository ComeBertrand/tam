//! End to end: `RunAgent` resumes a member's current session, the one the
//! (fake) Claude agent's hooks last showed a turn in.
//!
//! Kept to a single test: it sets process environment (PATH, HOME,
//! XDG_CONFIG_HOME), which must not race other tests in this binary.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use tam_daemon::daemon::Daemon;
use tam_proto::{Request, Response, ServerMessage, SessionChoice};
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

/// Run the real `tam hook-notify` as an injected hook would.
fn hook(sock: &Path, agent: &str, event: &str, session: &str, extra: serde_json::Value) {
    let mut payload = serde_json::json!({"hook_event_name": event, "session_id": session});
    payload
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let mut child = Command::new(env!("CARGO_BIN_EXE_tam"))
        .arg("hook-notify")
        .env("TAM_AGENT_ID", agent)
        .env("TAM_SOCKET", sock)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

/// A fake agent: records its name and arguments in `args_file` (written
/// aside then renamed, so a partial list is never read), then waits like an
/// idle session.
fn fake_agent(bin: &Path, name: &str, args_file: &Path) {
    let script = format!(
        "#!/bin/sh\n{{ echo {name}; for a in \"$@\"; do printf '%s\\n' \"$a\"; done; }} > '{f}.tmp'\nmv '{f}.tmp' '{f}'\nexec sleep 3600\n",
        f = args_file.display()
    );
    let path = bin.join(name);
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

struct Harness {
    stream: UnixStream,
    sock: PathBuf,
    member: String,
    args_file: PathBuf,
    /// Claude's transcripts of the member's worktree.
    project: PathBuf,
}

impl Harness {
    /// Start the member's agent; returns the session it resumed and the
    /// command line it was started with (program first).
    async fn run(
        &mut self,
        provider: Option<&str>,
        session: SessionChoice,
    ) -> Result<(Option<String>, Vec<String>), String> {
        let _ = std::fs::remove_file(&self.args_file);
        let request = Request::RunAgent {
            member: self.member.clone(),
            provider: provider.map(str::to_string),
            // A member that ran an agent runs it again: never this one.
            default_provider: "codex".into(),
            session,
            args: vec![],
            prompt: None,
            env: Default::default(),
            wrapper: vec![],
        };
        let resumed = match send(&mut self.stream, &request).await {
            Response::Running { id, resumed } => {
                assert_eq!(id, self.member);
                resumed
            }
            Response::Error { message } => return Err(message),
            other => panic!("expected Running, got {other:?}"),
        };
        for _ in 0..200 {
            if let Ok(args) = std::fs::read_to_string(&self.args_file) {
                return Ok((resumed, args.lines().map(str::to_string).collect()));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the agent did not start");
    }

    async fn stop(&mut self) {
        let request = Request::Kill {
            id: self.member.clone(),
        };
        assert!(matches!(
            send(&mut self.stream, &request).await,
            Response::Ok
        ));
    }

    fn hook(&self, event: &str, session: &str, extra: serde_json::Value) {
        hook(&self.sock, &self.member, event, session, extra);
    }

    /// A turn in `session`.
    fn turn(&self, session: &str) {
        self.hook("UserPromptSubmit", session, serde_json::json!({}));
        self.hook(
            "Stop",
            session,
            serde_json::json!({"last_assistant_message": "done"}),
        );
    }

    /// Claude wrote the session's transcript.
    fn transcript(&self, session: &str) {
        std::fs::write(self.project.join(format!("{session}.jsonl")), "{}\n").unwrap();
    }
}

fn resume_arg(args: &[String]) -> Option<&str> {
    let i = args.iter().position(|a| a == "--resume")?;
    args.get(i + 1).map(String::as_str)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_resumes_the_members_current_session() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Isolate from the user's environment: no real agents, notify command
    // or Claude files.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let home = root.join("home");
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    std::env::set_var("PATH", path);
    let args_file = root.join("agent-args");
    fake_agent(&bin, "claude", &args_file);
    fake_agent(&bin, "codex", &args_file);
    let project = home.join(".claude").join("projects").join("-work");
    std::fs::create_dir_all(&project).unwrap();

    let sock = root.join("tam.sock");
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

    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let request = Request::CreateTask {
        name: "feat".into(),
        members: vec![tam_proto::NewMember {
            repo_root: root.join("app"),
            branch: "feat".into(),
            dir: work.clone(),
        }],
    };
    let member = match send(&mut stream, &request).await {
        Response::Task { task, .. } => task.members[0].id.clone(),
        other => panic!("expected Task, got {other:?}"),
    };
    let mut h = Harness {
        stream,
        sock,
        member,
        args_file,
        project,
    };

    // No session yet: a new one.
    let (resumed, args) = h.run(Some("claude"), SessionChoice::Current).await.unwrap();
    assert_eq!((resumed, resume_arg(&args)), (None, None));
    h.hook(
        "SessionStart",
        "s1",
        serde_json::json!({"source": "startup"}),
    );
    h.turn("s1");
    // Running already: said so, whatever the session.
    let err = h.run(None, SessionChoice::Current).await.unwrap_err();
    assert!(err.contains("already has a running agent"), "{err}");
    h.stop().await;

    // Its transcript is gone (or was never written): refused, not an agent
    // exiting at once.
    let err = h.run(None, SessionChoice::Current).await.unwrap_err();
    assert!(err.contains("can no longer be resumed"), "{err}");
    assert!(err.contains("s1"), "{err}");

    // Resumed with no question asked, by the member's last agent.
    h.transcript("s1");
    let (resumed, args) = h.run(None, SessionChoice::Current).await.unwrap();
    assert_eq!(args[0], "claude");
    assert_eq!(resumed.as_deref(), Some("s1"));
    assert_eq!(resume_arg(&args), Some("s1"));
    h.hook(
        "SessionStart",
        "s1",
        serde_json::json!({"source": "resume"}),
    );
    // `/clear`, then a turn: the new session is the current one.
    h.hook("SessionStart", "s2", serde_json::json!({"source": "clear"}));
    h.turn("s2");
    h.stop().await;
    h.transcript("s2");
    let (resumed, _) = h.run(None, SessionChoice::Current).await.unwrap();
    assert_eq!(resumed.as_deref(), Some("s2"));
    h.stop().await;

    // A new session left before any turn does not replace it.
    let (resumed, args) = h.run(None, SessionChoice::New).await.unwrap();
    assert_eq!((resumed, resume_arg(&args)), (None, None));
    h.hook(
        "SessionStart",
        "s3",
        serde_json::json!({"source": "startup"}),
    );
    h.stop().await;
    let (resumed, _) = h.run(None, SessionChoice::Current).await.unwrap();
    assert_eq!(resumed.as_deref(), Some("s2"));
    h.stop().await;

    // One used does.
    h.run(None, SessionChoice::New).await.unwrap();
    h.hook(
        "SessionStart",
        "s4",
        serde_json::json!({"source": "startup"}),
    );
    h.turn("s4");
    h.stop().await;
    h.transcript("s4");
    let (resumed, _) = h.run(None, SessionChoice::Current).await.unwrap();
    assert_eq!(resumed.as_deref(), Some("s4"));
    h.stop().await;

    // The escape hatch: any session, unchecked.
    let (resumed, args) = h.run(None, SessionChoice::Id("s0".into())).await.unwrap();
    assert_eq!(resumed.as_deref(), Some("s0"));
    assert_eq!(resume_arg(&args), Some("s0"));
    h.stop().await;

    // Codex reports no sessions: a new one unless given one.
    let (resumed, args) = h.run(Some("codex"), SessionChoice::Current).await.unwrap();
    assert_eq!(args, ["codex", "-C", &work.display().to_string()]);
    assert_eq!(resumed, None);
    h.stop().await;
    let (resumed, args) = h
        .run(Some("codex"), SessionChoice::Id("x1".into()))
        .await
        .unwrap();
    assert_eq!(resumed.as_deref(), Some("x1"));
    assert_eq!(args[..3], ["codex", "resume", "x1"]);
    h.stop().await;
    let (resumed, _) = h.run(None, SessionChoice::Current).await.unwrap();
    assert_eq!(resumed, None, "codex resumes only on request");
    h.stop().await;

    // Claude's current session is still there for it: the one it was last
    // started in.
    h.transcript("s0");
    let (resumed, _) = h.run(Some("claude"), SessionChoice::Current).await.unwrap();
    assert_eq!(resumed.as_deref(), Some("s0"));
    h.stop().await;

    // An agent that cannot resume refuses a session.
    let err = h
        .run(Some("sh"), SessionChoice::Id("s1".into()))
        .await
        .unwrap_err();
    assert!(err.contains("cannot resume"), "{err}");
}
