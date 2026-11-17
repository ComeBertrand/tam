//! The non-interactive CLI, what agents and scripts rely on: exit statuses,
//! errors as JSON with `--json`, starting agents without attaching, `tam ps
//! --json`, `tam wait` and `tam result`. The documents are in `schema.rs`.

use std::io::IsTerminal;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use tam_proto::{AgentInfo, MemberInfo, MemberOutcome, MemberState, TaskInfo};

use crate::cli::{Commands, Until};
use crate::client::Client;
use crate::schema::{self, ErrorInfo, SCHEMA_VERSION};

/// Why a command failed, as scripts tell it: its exit status and, with
/// `--json`, the `error.kind` of its document. Errors of no kind failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// It could not do what was asked.
    Failed,
    /// `tam wait`: an agent exited or failed its turn short of the states
    /// waited for.
    Unreachable,
    /// `tam result`: a member has no response to give.
    NoResult,
    /// No such task or member, or a task name where one member is needed.
    NotFound,
    /// `tam wait` ran out of time.
    Timeout,
    /// The daemon could not be reached, or broke off.
    Daemon,
}

impl ErrorKind {
    /// The exit status (2, usage errors, is clap's).
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Failed | Self::Unreachable | Self::NoResult => 1,
            Self::NotFound => 3,
            Self::Timeout => 4,
            Self::Daemon => 5,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Unreachable => "unreachable",
            Self::NoResult => "no_result",
            Self::NotFound => "not_found",
            Self::Timeout => "timeout",
            Self::Daemon => "daemon",
        }
    }
}

/// An error of a known kind, anywhere in an `anyhow::Error`'s chain.
#[derive(Debug)]
pub struct CliError {
    kind: ErrorKind,
    message: String,
    /// Printed already, in the command's JSON document: exit only.
    reported: bool,
}

impl CliError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            reported: false,
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

/// An error of this kind.
pub fn error(kind: ErrorKind, message: impl Into<String>) -> anyhow::Error {
    CliError::new(kind, message).into()
}

/// The kind of a failure (`Failed` when nothing in it tells).
pub fn kind_of(err: &anyhow::Error) -> ErrorKind {
    err.chain()
        .find_map(|e| e.downcast_ref::<CliError>())
        .map_or(ErrorKind::Failed, |e| e.kind)
}

impl ErrorInfo {
    fn new(err: &anyhow::Error) -> Self {
        Self {
            kind: kind_of(err).as_str(),
            message: format!("{err:#}"),
        }
    }
}

/// Whether a command was asked for JSON: its failure is reported as JSON.
pub fn json_output(command: &Commands) -> bool {
    match command {
        Commands::New { json, .. }
        | Commands::Run { json, .. }
        | Commands::Ps { json }
        | Commands::Ls { json, .. }
        | Commands::History { json, .. }
        | Commands::Peers { json, .. }
        | Commands::Wait { json, .. }
        | Commands::Result { json, .. }
        | Commands::Brief { json, .. } => *json,
        _ => false,
    }
}

/// Report a failed command and return its exit status: with `--json`, a
/// document on stdout (`schema_version` and `error`), else `Error: …` on
/// stderr. Nothing when the command printed it already.
pub fn fail(err: &anyhow::Error, json: bool) -> i32 {
    let found = err.chain().find_map(|e| e.downcast_ref::<CliError>());
    if !found.is_some_and(|e| e.reported) {
        if json {
            let doc = schema::Failure {
                schema_version: SCHEMA_VERSION,
                error: ErrorInfo::new(err),
            };
            if let Ok(doc) = serde_json::to_string_pretty(&doc) {
                println!("{doc}");
            }
        } else {
            eprintln!("Error: {err:?}");
        }
    }
    kind_of(err).exit_code()
}

/// End a command that printed its JSON document (with `error`): its
/// failure is only to exit with.
fn finish(error: Option<anyhow::Error>, json: bool) -> Result<()> {
    match error {
        None => Ok(()),
        Some(err) if json => Err(CliError {
            kind: kind_of(&err),
            message: format!("{err:#}"),
            reported: true,
        }
        .into()),
        Some(err) => Err(err),
    }
}

fn print_json<T: Serialize>(doc: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(doc)?);
    Ok(())
}

/// Whether `tam new`/`tam run` attach to the agent they start: unless told
/// not to (`--detach`, `--json`), or with no terminal to attach (stdin and
/// stdout both).
pub fn attaching(detach: bool) -> bool {
    !detach && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Start the agents of a task's `members` without attaching (`start`:
/// `None` starts none), then tell what started: the JSON receipt, or a line
/// per agent. Stops at the first agent that fails to start.
pub async fn start_detached(
    client: &mut Client,
    task: &TaskInfo,
    members: &[MemberInfo],
    config: &crate::config::Config,
    start: Option<crate::Start>,
    json: bool,
) -> Result<()> {
    let mut started: Vec<schema::Started> = members
        .iter()
        .map(|m| schema::Started::new(task, m))
        .collect();
    let mut failure = None;
    if let Some(start) = start {
        for (member, entry) in members.iter().zip(started.iter_mut()) {
            let run = client
                .run_agent(
                    &member.id,
                    start.agent.clone(),
                    &config.default_agent,
                    start.session.clone(),
                    start.prompt.clone(),
                    start.args.clone(),
                )
                .await;
            match run {
                Ok((_, resumed)) => {
                    // The agent the daemon picked is that of the run it
                    // just recorded.
                    let (_, _, runs) = client.resolve_member(Some(&member.id), None).await?;
                    entry.agent = runs.last().map(|r| r.provider.clone());
                    entry.resumed_session = resumed;
                }
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
    }

    if json {
        print_json(&schema::Receipt {
            schema_version: SCHEMA_VERSION,
            task: task.name.clone(),
            members: started,
            error: failure.as_ref().map(ErrorInfo::new),
        })?;
    } else {
        for entry in &started {
            let Some(agent) = &entry.agent else {
                continue;
            };
            let session = match &entry.resumed_session {
                Some(id) => format!("resumed session {id}"),
                None => "new session".into(),
            };
            println!(
                "Started {agent} in '{}' ({session}). Attach with: tam attach {}",
                entry.label, entry.label
            );
        }
    }
    finish(failure, json)
}

/// `tam ps --json` (see `schema::Ps`).
pub async fn ps(client: &mut Client, infos: Vec<TaskInfo>, agents: &[AgentInfo]) -> Result<()> {
    let mut tasks = Vec::with_capacity(infos.len());
    for info in infos {
        // The runs tell the members' last exit and session. A task dropped
        // meanwhile has none.
        let runs = match client.get_task(&info.name).await {
            Ok((_, runs)) => runs,
            Err(e) if kind_of(&e) == ErrorKind::Daemon => return Err(e),
            Err(_) => Vec::new(),
        };
        tasks.push((info, runs));
    }
    print_json(&schema::Ps::new(tasks, agents))
}

/// The members a command acts on: all of a task's, the one named as
/// TASK/MEMBER or by id, or the one whose worktree holds the current
/// directory.
async fn task_members(
    client: &mut Client,
    name: Option<&str>,
) -> Result<(TaskInfo, Vec<MemberInfo>)> {
    if let Some(name) = name {
        if let Ok((task, _)) = client.get_task(name).await {
            let members = task.members.clone();
            return Ok((task, members));
        }
    }
    let (task, member, _) = crate::resolve_member(client, name).await?;
    Ok((task, vec![member]))
}

fn member_state(until: Until) -> MemberState {
    match until {
        Until::Working => MemberState::Working,
        Until::Blocked => MemberState::Blocked,
        Until::Input => MemberState::Input,
        Until::Idle => MemberState::Idle,
        Until::Question => MemberState::Question,
        Until::Error => MemberState::Error,
        Until::Exit => MemberState::Exit,
    }
}

/// `tam wait`: wait until the agents settle in one of `until` (default:
/// any state but working), then print their states.
pub async fn wait(
    name: Option<String>,
    until: Vec<Until>,
    timeout: Option<Duration>,
    json: bool,
) -> Result<()> {
    let mut client = Client::connect().await?;
    let (task, members) = task_members(&mut client, name.as_deref()).await?;
    let until: Vec<MemberState> = if until.is_empty() {
        MemberState::SETTLED.to_vec()
    } else {
        until.into_iter().map(member_state).collect()
    };
    let ids = members.into_iter().map(|m| m.id).collect();
    let timeout_secs = timeout.map(|t| t.as_secs());
    let (outcomes, timed_out) = client.wait_agents(ids, until.clone(), timeout_secs).await?;

    let error = if timed_out {
        let waiting: Vec<String> = outcomes
            .iter()
            .filter(|o| !o.state.satisfies(&until))
            .map(|o| format!("'{}' is {}", o.label, o.state))
            .collect();
        Some(error(
            ErrorKind::Timeout,
            format!(
                "timed out after {}s: {}",
                timeout_secs.unwrap_or_default(),
                waiting.join(", ")
            ),
        ))
    } else {
        // Given up: on the agent that exited or failed, not on one still
        // going (another member's).
        let stuck = |o: &&MemberOutcome| {
            matches!(o.state, MemberState::Exit | MemberState::Error) && !o.state.satisfies(&until)
        };
        outcomes
            .iter()
            .find(stuck)
            .or_else(|| outcomes.iter().find(|o| !o.state.satisfies(&until)))
            .map(|o| error(ErrorKind::Unreachable, gave_up(o, &until)))
    };
    if json {
        let info = error.as_ref().map(ErrorInfo::new);
        print_json(&schema::Outcomes::new(&task, &outcomes, info))?;
    } else {
        for o in &outcomes {
            println!("{}", state_line(o));
        }
    }
    finish(error, json)
}

/// `tam result`: print the responses that ended the agents' last turns.
pub async fn result(name: Option<String>, json: bool) -> Result<()> {
    let mut client = Client::connect().await?;
    let (task, members) = task_members(&mut client, name.as_deref()).await?;
    let ids = members.into_iter().map(|m| m.id).collect();
    let outcomes = client.outcomes(ids).await?;

    let missing: Vec<String> = outcomes
        .iter()
        .filter(|o| o.response.is_none())
        .map(no_response)
        .collect();
    let error = (!missing.is_empty()).then(|| error(ErrorKind::NoResult, missing.join("; ")));
    if json {
        let info = error.as_ref().map(ErrorInfo::new);
        print_json(&schema::Outcomes::new(&task, &outcomes, info))?;
    } else {
        // Several members: each response under its member's name.
        let several = outcomes.len() > 1;
        let mut first = true;
        for o in &outcomes {
            let Some(response) = &o.response else {
                continue;
            };
            if several {
                if !first {
                    println!();
                }
                println!("── {} ──", o.label);
            }
            first = false;
            println!("{}", response.text.trim_end());
        }
    }
    finish(error, json)
}

/// What the agent waits on, or why its turn failed.
fn state_detail(o: &MemberOutcome) -> Option<String> {
    match o.state {
        MemberState::Blocked | MemberState::Question => o.pending.as_ref().map(|p| match &p.tool {
            Some(tool) => format!("{tool}: {}", p.summary),
            None => p.summary.clone(),
        }),
        MemberState::Error => o.error.as_ref().map(|e| match &e.message {
            Some(message) => format!("{}: {message}", e.kind),
            None => e.kind.clone(),
        }),
        MemberState::Exit => Some(match (&o.provider, o.exit_code) {
            (None, _) => "no agent ran".into(),
            (Some(_), Some(-1)) => "stopped".into(),
            (Some(_), Some(code)) => format!("exit {code}"),
            (Some(_), None) => "not running".into(),
        }),
        _ => None,
    }
}

/// `feat/api: question (AskUserQuestion: Tea or coffee?)`.
fn state_line(o: &MemberOutcome) -> String {
    match state_detail(o) {
        Some(detail) => format!("{}: {} ({detail})", o.label, o.state),
        None => format!("{}: {}", o.label, o.state),
    }
}

/// Why a wait gave up on a member short of `until`.
fn gave_up(o: &MemberOutcome, until: &[MemberState]) -> String {
    let wanted: Vec<String> = until.iter().map(ToString::to_string).collect();
    let why = match (o.state, &o.provider, o.exit_code) {
        (MemberState::Error, ..) => match state_detail(o) {
            Some(detail) => format!("its turn failed ({detail})"),
            None => "its turn failed".into(),
        },
        (_, None, _) => "no agent ran in it".into(),
        (_, Some(_), Some(-1)) => "its agent was stopped".into(),
        (_, Some(_), Some(code)) => format!("its agent exited (exit {code})"),
        (_, Some(_), None) => "its agent is not running".into(),
    };
    format!(
        "'{}' will not get to {} without someone acting: {why}",
        o.label,
        wanted.join(" or ")
    )
}

/// Why a member has no response.
fn no_response(o: &MemberOutcome) -> String {
    let why = match o.state {
        MemberState::Working => "its agent is in the middle of a turn (tam wait first)".into(),
        MemberState::Blocked | MemberState::Question | MemberState::Error => {
            let what = match o.state {
                MemberState::Error => "its last turn failed",
                MemberState::Blocked => "its agent waits on a permission",
                _ => "its agent asked a question",
            };
            match state_detail(o) {
                Some(detail) => format!("{what} ({detail})"),
                None => what.into(),
            }
        }
        MemberState::Exit if o.provider.is_none() => "no agent ran in it".into(),
        _ => "no turn of its latest run ended with one (not run yet, interrupted, or an agent \
              that reports none: only Claude does)"
            .into(),
    };
    format!("no response from '{}': {why}", o.label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tam_proto::{AgentResponse, Pending};

    fn outcome(state: MemberState) -> MemberOutcome {
        MemberOutcome {
            member: "m-1".into(),
            task: "feat".into(),
            label: "feat".into(),
            state,
            provider: Some("claude".into()),
            session_id: None,
            exit_code: None,
            pending: None,
            error: None,
            response: None,
        }
    }

    #[test]
    fn exit_statuses_tell_failures_apart() {
        let codes: Vec<(&str, i32)> = [
            ErrorKind::Failed,
            ErrorKind::Unreachable,
            ErrorKind::NoResult,
            ErrorKind::NotFound,
            ErrorKind::Timeout,
            ErrorKind::Daemon,
        ]
        .iter()
        .map(|k| (k.as_str(), k.exit_code()))
        .collect();
        assert_eq!(
            codes,
            [
                ("failed", 1),
                ("unreachable", 1),
                ("no_result", 1),
                ("not_found", 3),
                ("timeout", 4),
                ("daemon", 5)
            ]
        );
    }

    #[test]
    fn the_kind_is_found_under_context() {
        use anyhow::Context;
        let err = Err::<(), _>(error(ErrorKind::NotFound, "task 'x' not found"))
            .context("cannot run")
            .unwrap_err();
        assert_eq!(kind_of(&err), ErrorKind::NotFound);
        assert_eq!(kind_of(&anyhow::anyhow!("plain")), ErrorKind::Failed);
        let info = ErrorInfo::new(&err);
        assert_eq!(info.kind, "not_found");
        assert_eq!(info.message, "cannot run: task 'x' not found");
    }

    #[test]
    fn a_reported_failure_is_not_printed_again() {
        let err = finish(Some(error(ErrorKind::Timeout, "late")), true).unwrap_err();
        assert!(err.downcast_ref::<CliError>().unwrap().reported);
        assert_eq!(fail(&err, true), 4);
        let err = finish(Some(error(ErrorKind::Timeout, "late")), false).unwrap_err();
        assert!(!err.downcast_ref::<CliError>().unwrap().reported);
        assert!(finish(None, true).is_ok());
    }

    #[test]
    fn state_lines_say_what_the_agent_waits_on() {
        let mut o = outcome(MemberState::Question);
        o.pending = Some(Pending {
            tool: Some("AskUserQuestion".into()),
            summary: "Tea or coffee?".into(),
        });
        assert_eq!(
            state_line(&o),
            "feat: question (AskUserQuestion: Tea or coffee?)"
        );
        let mut o = outcome(MemberState::Exit);
        o.exit_code = Some(-1);
        assert_eq!(state_line(&o), "feat: exit (stopped)");
        o.exit_code = Some(2);
        assert_eq!(
            gave_up(&o, &[MemberState::Input, MemberState::Question]),
            "'feat' will not get to input or question without someone acting: its agent exited (exit 2)"
        );
        o.provider = None;
        assert_eq!(state_line(&o), "feat: exit (no agent ran)");
        assert_eq!(state_line(&outcome(MemberState::Input)), "feat: input");
        assert_eq!(
            gave_up(&o, &[MemberState::Input]),
            "'feat' will not get to input without someone acting: no agent ran in it"
        );
    }

    #[test]
    fn no_response_says_why() {
        assert!(no_response(&outcome(MemberState::Working)).contains("tam wait first"));
        let mut o = outcome(MemberState::Exit);
        o.provider = None;
        assert_eq!(
            no_response(&o),
            "no response from 'feat': no agent ran in it"
        );
        let o = outcome(MemberState::Input);
        assert!(no_response(&o).contains("only Claude"));
        // With one, the outcome renders it whole.
        let mut o = outcome(MemberState::Input);
        o.response = Some(AgentResponse {
            text: "done".into(),
            at: 3,
        });
        let task = TaskInfo {
            name: "feat".into(),
            members: vec![],
            created_at: 0,
            run_count: 0,
            last_activity: None,
        };
        let doc = serde_json::to_value(schema::Outcomes::new(&task, &[o], None)).unwrap();
        assert_eq!(doc["members"][0]["response"], "done");
        assert_eq!(doc["members"][0]["response_at"], 3);
        assert!(doc["error"].is_null());
    }

    #[test]
    fn wait_takes_states_and_durations() {
        use crate::cli::Cli;
        use clap::Parser;
        let parse = |args: &[&str]| Cli::try_parse_from(["tam"].iter().chain(args));
        match parse(&[
            "wait",
            "feat",
            "--until",
            "input,exit",
            "-t",
            "15m",
            "--json",
        ])
        .unwrap()
        .command
        {
            Some(Commands::Wait {
                name,
                until,
                timeout,
                json,
            }) => {
                assert_eq!(name.as_deref(), Some("feat"));
                assert_eq!(until, [Until::Input, Until::Exit]);
                assert_eq!(timeout, Some(Duration::from_secs(900)));
                assert!(json);
            }
            _ => panic!("not a wait"),
        }
        for (arg, secs) in [("90", 90), ("90s", 90), ("2h", 7200), ("0", 0)] {
            match parse(&["wait", "--timeout", arg]).unwrap().command {
                Some(Commands::Wait { timeout, .. }) => {
                    assert_eq!(timeout, Some(Duration::from_secs(secs)), "{arg}")
                }
                _ => panic!("not a wait"),
            }
        }
        for bad in ["", "m", "1d", "-5", "1.5h", "99999999999999999999h"] {
            assert!(parse(&["wait", "--timeout", bad]).is_err(), "{bad}");
        }
        assert!(parse(&["wait", "--until", "done"]).is_err());

        // One prompt, given either way; none with --no-start.
        assert!(parse(&["run", "feat", "--prompt", "a", "b"]).is_err());
        assert!(parse(&["run", "--prompt", "a", "-d"]).is_ok());
        assert!(parse(&["new", "feat", "--prompt", "a", "--no-start"]).is_err());
    }
}
