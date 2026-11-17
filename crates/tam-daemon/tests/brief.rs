//! A task's brief and directory: kept by the daemon, made when first needed,
//! told to the members' agents at session start, reachable by Claude
//! agents (their task's directory, their peers' worktrees read only), and
//! retired with the task. Against real git worktrees and a fake `claude`.
//!
//! Kept to a single test: it sets `PATH` (the fake `claude`), which must
//! not race other tests in this binary.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::Value;
use tam_daemon::daemon::Daemon;
use tam_proto::{NewMember, Request, Response, ServerMessage, TaskBrief};
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

async fn brief(stream: &mut UnixStream, request: Request) -> TaskBrief {
    match send(stream, &request).await {
        Response::Brief { brief, .. } => brief,
        other => panic!("expected Brief, got {other:?}"),
    }
}

fn get(task: &str) -> Request {
    Request::GetBrief { task: task.into() }
}

fn set(task: &str, brief: Option<&str>) -> Request {
    Request::SetBrief {
        task: task.into(),
        brief: brief.map(str::to_string),
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
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

/// A repository `name` with a worktree on branch `feat`; the new member.
fn member(root: &Path, name: &str) -> NewMember {
    let repo = root.join("src").join(name);
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let dir = root.join("w").join(format!("{name}--feat"));
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "feat", dir.to_str().unwrap()],
    );
    NewMember {
        repo_root: repo,
        branch: "feat".into(),
        dir,
    }
}

/// Wait for a file the fake agent writes.
async fn read_when_written(path: &Path) -> String {
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(path) {
            if !s.is_empty() {
                return s;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{} was never written", path.display());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_brief_from_creation_to_drop() {
    let tmp = tempfile::tempdir().unwrap();
    let root = &std::fs::canonicalize(tmp.path()).unwrap();

    // A fake claude that writes down its arguments, one per line, and
    // stays up like an idle session.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let args_file = root.join("claude-args");
    std::fs::write(
        bin.join("claude"),
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{0}.tmp'\n\
             mv '{0}.tmp' '{0}'\nexec sleep 3600\n",
            args_file.display()
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

    let sock = root.join("sock");
    let daemon = Daemon::new(sock.clone(), root.join("data").join("ledger.jsonl"));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    let mut stream = loop {
        if let Ok(s) = UnixStream::connect(&sock).await {
            break s;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let (api, web) = (member(root, "api"), member(root, "web"));
    let request = Request::CreateTask {
        name: "feat".into(),
        members: vec![api.clone(), web.clone()],
    };
    let task = match send(&mut stream, &request).await {
        Response::Task { task, .. } => task,
        other => panic!("expected Task, got {other:?}"),
    };
    let (api_id, web_id) = (task.members[0].id.clone(), task.members[1].id.clone());
    let task_dir = root.join("data").join("tasks").join("feat");

    // Any member's agent finds the task's directory there, even one that
    // gets no access to it (not Claude).
    let resp = send(
        &mut stream,
        &Request::Spawn {
            provider: "sh".into(),
            dir: PathBuf::new(),
            id: None,
            args: vec!["-c".into(), "exec sleep 3600".into()],
            resume_session: None,
            prompt: None,
            member: Some(web_id.clone()),
        },
    )
    .await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    assert!(task_dir.join("scratch").is_dir());

    // The local brief by default, made when first asked for; by a member
    // too.
    let local = brief(&mut stream, get("feat")).await;
    assert_eq!(local.reference, None);
    assert_eq!(local.dir, task_dir);
    let file = local.file().unwrap();
    let template = std::fs::read_to_string(&file).unwrap();
    assert!(
        template.contains(&format!("\n## api ({api_id})\n")),
        "{template}"
    );
    assert!(local.scratch().is_dir());
    assert_eq!(brief(&mut stream, get(&web_id)).await, local);
    assert!(matches!(
        send(&mut stream, &get("nope")).await,
        Response::Error { .. }
    ));

    // A reference instead, set through a member's name.
    let linked = brief(&mut stream, set("feat/web", Some("https://t/CS-1"))).await;
    assert_eq!(linked.reference.as_deref(), Some("https://t/CS-1"));
    assert_eq!(brief(&mut stream, get("feat")).await, linked);

    // A Claude agent reaches the task's directory and reads its peer's
    // worktree, through the settings tam gives it; the prompt stays last.
    let resp = send(
        &mut stream,
        &Request::Spawn {
            provider: "claude".into(),
            dir: PathBuf::new(),
            id: None,
            args: vec![],
            resume_session: None,
            prompt: Some("go".into()),
            member: Some(api_id.clone()),
        },
    )
    .await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    let args = read_when_written(&args_file).await;
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(args.len(), 3, "{args:?}");
    assert_eq!((args[0], args[2]), ("--settings", "go"));
    let settings: Value = serde_json::from_str(args[1]).unwrap();
    assert_eq!(
        settings["permissions"]["additionalDirectories"],
        serde_json::json!([task_dir, web.dir])
    );
    assert_eq!(
        settings["permissions"]["deny"],
        serde_json::json!([format!("Edit(/{}/**)", web.dir.display())])
    );

    // Its session starts knowing the brief, the scratch directory and
    // what it can read.
    let start = Request::HookEvent {
        agent_id: api_id.clone(),
        event: "SessionStart".into(),
        payload: Some(serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-1",
            "source": "startup",
        })),
    };
    let note = match send(&mut stream, &start).await {
        Response::HookContext { context } => context,
        other => panic!("expected HookContext, got {other:?}"),
    };
    assert!(
        note.contains("\nYou can read the other members' worktrees, not edit them.\n"),
        "{note}"
    );
    assert!(
        note.contains("brief, what its members work from: https://t/CS-1 (tam does not fetch"),
        "{note}"
    );
    assert!(note.contains(&format!("\"## api ({api_id})\"")), "{note}");
    assert!(
        note.contains(&format!(
            "shared by its members: {}.",
            local.scratch().display()
        )),
        "{note}"
    );

    // Back to the local file; the next session start says so.
    let local_again = brief(&mut stream, set("feat", None)).await;
    assert_eq!(local_again, local);
    let note = match send(&mut stream, &start).await {
        Response::HookContext { context } => context,
        other => panic!("expected HookContext, got {other:?}"),
    };
    assert!(
        note.contains(&format!("work from: the file {}.", file.display())),
        "{note}"
    );

    // Dropped: scratch goes, the brief its members wrote in is kept.
    std::fs::write(&file, format!("{template}\nPOST /items -> 201\n")).unwrap();
    std::fs::write(local.scratch().join("notes"), "x").unwrap();
    let drop = Request::DropTask {
        name: "feat".into(),
        force: true,
        delete_branch: false,
    };
    let warnings = match send(&mut stream, &drop).await {
        Response::Dropped { warnings } => warnings,
        other => panic!("expected Dropped, got {other:?}"),
    };
    assert!(!task_dir.exists());
    let dropped = root.join("data").join("tasks").join(".dropped");
    let kept: Vec<PathBuf> = std::fs::read_dir(&dropped)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    let name = kept[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("feat-") && name.ends_with(".md"), "{name}");
    assert!(std::fs::read_to_string(&kept[0])
        .unwrap()
        .contains("POST /items"));
    assert_eq!(
        warnings,
        [format!(
            "the brief of task 'feat' is kept at {}",
            kept[0].display()
        )]
    );
}
