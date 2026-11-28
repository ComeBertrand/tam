//! The CLI's and the dashboard's lines in the activity log (see
//! `tam_daemon::activity`): a `cli` line per command when it ends (`ms`,
//! exit status, error), `cli.usage` for a command line clap refused,
//! `daemon.autostart` when a client had to start the daemon, and the
//! dashboard's (`tui::log`).
//!
//! What a command line says is chosen here, command by command: names,
//! targets and flags; of a prompt, a message or a brief, the length only.

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde_json::{json, Map, Value};
use tam_daemon::activity::{self, Activity};

use crate::cli::Commands;
use crate::script;

/// This process's log, once [`start`] ran (else off).
static LOG: OnceLock<Activity> = OnceLock::new();

/// The command running, until its line is written.
static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

struct Running {
    cmd: &'static str,
    fields: Map<String, Value>,
    started: Instant,
}

/// This process's log.
pub fn log() -> &'static Activity {
    LOG.get_or_init(Activity::off)
}

/// A command starts: open the log as the config says (`[log]`) and keep
/// what its line will say. The dashboard's lines are its own (`src`
/// `tui`); `hook-notify` and `inbox-wait` log nothing (the daemon logs what
/// they bring), nor does `tam daemon` here (it is the daemon).
pub fn start(command: Option<&Commands>) {
    let Some(command) = command else {
        let _ = LOG.set(activity::for_process("tui"));
        return;
    };
    let Some((cmd, fields)) = describe(command) else {
        return;
    };
    let _ = LOG.set(activity::for_process("cli"));
    if !log().is_on() {
        return;
    }
    if let Ok(mut running) = RUNNING.lock() {
        *running = Some(Running {
            cmd,
            fields,
            started: Instant::now(),
        });
    }
    on_signals();
}

/// A command interrupted (Ctrl-C, its terminal closed, killed) still
/// writes its line, with the signal, and exits as a shell says it was
/// (128 + the signal). SIGKILL leaves no line.
fn on_signals() {
    use tokio::signal::unix::{signal, SignalKind};
    for (kind, number, name) in [
        (SignalKind::interrupt(), 2, "SIGINT"),
        (SignalKind::terminate(), 15, "SIGTERM"),
        (SignalKind::hangup(), 1, "SIGHUP"),
    ] {
        let Ok(mut signals) = signal(kind) else {
            continue;
        };
        tokio::spawn(async move {
            if signals.recv().await.is_some() {
                finish(128 + number, None, Some(name));
                std::process::exit(128 + number);
            }
        });
    }
}

/// The command ends with exit status `code` (`error`: why): its line.
/// Once; later calls do nothing.
pub fn end(code: i32, error: Option<&anyhow::Error>) {
    finish(code, error, None);
}

fn finish(code: i32, error: Option<&anyhow::Error>, signal: Option<&str>) {
    let Some(Running {
        cmd,
        mut fields,
        started,
    }) = RUNNING.lock().ok().and_then(|mut r| r.take())
    else {
        return;
    };
    fields.insert("cmd".into(), cmd.into());
    fields.insert("ms".into(), activity::ms_since(started).into());
    fields.insert("exit".into(), code.into());
    if let Some(signal) = signal {
        fields.insert("signal".into(), signal.into());
    }
    if let Some(error) = error {
        fields.insert("error_kind".into(), script::kind_of(error).as_str().into());
        fields.insert(
            "error".into(),
            activity::cut(&format!("{error:#}"), 500).into(),
        );
    }
    // Called by an agent (its member or chat), or by the user.
    fields.insert(
        "agent".into(),
        std::env::var("TAM_AGENT_ID")
            .ok()
            .filter(|id| !id.is_empty())
            .into(),
    );
    log().record("cli", Value::Object(fields));
}

/// Exit with `code`, the command's line written first.
pub fn exit(code: i32) -> ! {
    end(code, None);
    std::process::exit(code)
}

/// A command line clap refused (`kind`, e.g. `UnknownArgument`): which
/// command, not the arguments.
pub fn usage_error(kind: clap::error::ErrorKind) {
    use clap::error::ErrorKind;
    if matches!(kind, ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
        return;
    }
    // The first word, when it looks like a command (a typo is one too).
    let cmd = std::env::args().nth(1).filter(|a| {
        a.len() <= 20
            && a.chars().all(|c| c.is_ascii_lowercase() || c == '-')
            && !a.starts_with('-')
    });
    activity::for_process("cli").record(
        "cli.usage",
        json!({ "cmd": cmd, "error_kind": format!("{kind:?}") }),
    );
}

fn len(text: &Option<String>) -> Option<usize> {
    text.as_ref().map(|t| t.chars().count())
}

/// The command's name and what its line says of it; `None`: not logged.
fn describe(command: &Commands) -> Option<(&'static str, Map<String, Value>)> {
    let (cmd, fields) = match command {
        Commands::New {
            name,
            source,
            no_start,
            prompt,
            detach,
            json,
            repo,
            join,
            brief,
        } => (
            "new",
            json!({
                "task": name,
                "source": source,
                "no_start": no_start,
                "prompt_len": len(prompt),
                "detach": detach,
                "json": json,
                "repos": repo,
                "join": join,
                "brief": brief.is_some(),
            }),
        ),
        Commands::Run {
            name,
            new_session,
            session,
            prompt,
            args,
            prompt_option,
            detach,
            json,
        } => (
            "run",
            json!({
                "target": name,
                "new_session": new_session,
                "session": session.is_some(),
                "prompt_len": len(prompt).or(len(prompt_option)),
                "args": args.len(),
                "detach": detach,
                "json": json,
            }),
        ),
        Commands::Stop { name } => ("stop", json!({ "target": name })),
        Commands::Attach { name } => ("attach", json!({ "target": name })),
        Commands::Drop {
            name,
            branch,
            force,
        } => (
            "drop",
            json!({ "target": name, "branch": branch, "force": force }),
        ),
        Commands::Adopt { path, name, join } => {
            ("adopt", json!({ "path": path, "task": name, "join": join }))
        }
        Commands::Repair { name } => ("repair", json!({ "target": name })),
        Commands::Ps { json } => ("ps", json!({ "json": json })),
        Commands::Ls {
            path,
            json,
            raw,
            porcelain,
        } => (
            "ls",
            json!({ "path": path, "json": json, "raw": raw, "porcelain": porcelain }),
        ),
        Commands::Pick { finder } => ("pick", json!({ "finder": finder })),
        Commands::Init { agent } => ("init", json!({ "agent": agent })),
        Commands::Shutdown => ("shutdown", json!({})),
        Commands::Status => ("status", json!({})),
        Commands::Daemon | Commands::HookNotify { .. } | Commands::InboxWait { .. } => return None,
        Commands::Msg {
            target,
            text,
            wait,
            timeout,
            from,
        } => (
            "msg",
            json!({
                "target": target,
                "len": text.iter().map(|t| t.chars().count()).sum::<usize>() + text.len().saturating_sub(1),
                "wait": wait,
                "timeout_s": wait.then_some(timeout),
                "from": from,
            }),
        ),
        Commands::History { task, json } => ("history", json!({ "target": task, "json": json })),
        Commands::Peers { all, json, from } => {
            ("peers", json!({ "all": all, "json": json, "from": from }))
        }
        Commands::Wait {
            name,
            until,
            timeout,
            json,
        } => (
            "wait",
            json!({
                "target": name,
                "until": until.iter().map(|u| format!("{u:?}").to_lowercase()).collect::<Vec<_>>(),
                "timeout_s": timeout.map(|t| t.as_secs()),
                "json": json,
            }),
        ),
        Commands::Result { name, json } => ("result", json!({ "target": name, "json": json })),
        Commands::Brief {
            name,
            set,
            local,
            json,
            from,
        } => (
            "brief",
            json!({
                "target": name,
                "set": set.is_some(),
                "local": local,
                "json": json,
                "from": from,
            }),
        ),
        Commands::Merge { into, from, json } => {
            ("merge", json!({ "into": into, "from": from, "json": json }))
        }
        Commands::Split { member, name, json } => (
            "split",
            json!({ "member": member, "task": name, "json": json }),
        ),
        Commands::Chat {
            repo,
            name,
            session,
            keep,
            prompt,
            detach,
            json,
        } => (
            "chat",
            json!({
                "repo": repo,
                "name": name,
                "session": session.is_some(),
                "keep": keep,
                "prompt_len": len(prompt),
                "detach": detach,
                "json": json,
            }),
        ),
        Commands::Close { chat, yes, json } => {
            ("close", json!({ "chat": chat, "yes": yes, "json": json }))
        }
        Commands::Rename { chat, name, json } => (
            "rename",
            json!({ "chat": chat, "name": name, "json": json }),
        ),
        Commands::Keep { chat, off, json } => {
            ("keep", json!({ "chat": chat, "off": off, "json": json }))
        }
        Commands::Shell {
            target,
            repo,
            detach,
            json,
        } => (
            "shell",
            json!({ "target": target, "repo": repo, "detach": detach, "json": json }),
        ),
    };
    let mut fields = match fields {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    if waits(command) {
        fields.insert("wait".into(), true.into());
    }
    // Interactive, or a script (another program, an agent).
    let terminal = {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    };
    fields.insert("terminal".into(), terminal.into());
    Some((cmd, fields))
}

/// Whether the command's `ms` is mostly waiting: attached to an agent or
/// a shell, waiting for agents or a reply, or picking.
fn waits(command: &Commands) -> bool {
    match command {
        Commands::Attach { .. } | Commands::Wait { .. } | Commands::Pick { .. } => true,
        Commands::Msg { wait, .. } => *wait,
        Commands::New {
            no_start,
            detach,
            json,
            ..
        } => !no_start && script::attaching(*detach || *json),
        Commands::Run { detach, json, .. }
        | Commands::Chat { detach, json, .. }
        | Commands::Shell { detach, json, .. } => script::attaching(*detach || *json),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn described(args: &[&str]) -> Value {
        let cli = crate::cli::Cli::parse_from(args);
        let (cmd, mut fields) = describe(cli.command.as_ref().unwrap()).unwrap();
        fields.insert("cmd".into(), cmd.into());
        Value::Object(fields)
    }

    #[test]
    fn prompts_and_messages_are_lengths() {
        let new = described(&["tam", "new", "fix", "-r", "api", "-p", "SECRET PROMPT"]);
        assert_eq!(new["cmd"], "new");
        assert_eq!(new["task"], "fix");
        assert_eq!(new["repos"], json!(["api"]));
        assert_eq!(new["prompt_len"], 13);
        let msg = described(&["tam", "msg", "fix", "SECRET", "TEXT"]);
        assert_eq!(msg["len"], 11);
        let run = described(&["tam", "run", "fix", "SECRET PROMPT"]);
        assert_eq!(run["prompt_len"], 13);
        let brief = described(&["tam", "brief", "fix", "--set", "SECRET-1"]);
        assert_eq!(brief["set"], true);
        for line in [new, msg, run, brief] {
            assert!(!line.to_string().contains("SECRET"), "{line}");
        }
    }

    #[test]
    fn hooks_and_the_daemon_are_not_commands_logged() {
        for args in [
            &["tam", "daemon"][..],
            &["tam", "hook-notify"],
            &["tam", "inbox-wait"],
        ] {
            let cli = crate::cli::Cli::parse_from(args);
            assert!(
                describe(cli.command.as_ref().unwrap()).is_none(),
                "{args:?}"
            );
        }
    }
}
