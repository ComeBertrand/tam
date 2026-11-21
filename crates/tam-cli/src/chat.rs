//! Chats from the command line (#66): `tam chat` starts one, `tam close`,
//! `tam rename` and `tam keep` change one, and the commands that take a task
//! member take a chat too (`main.rs`, `script.rs`, `msg.rs`).
//!
//! A chat is shown by its label (`ChatInfo::label`), but every command tam
//! tells the user to type names it by its id: the label may be a title no
//! command takes back.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tam_proto::{AgentInfo, ChatInfo, SessionChoice};

use crate::client::Client;
use crate::schema::{self, ErrorInfo, Subject, SCHEMA_VERSION};
use crate::script::{self, ErrorKind};
use crate::task::{agent_status, TaskStatus};
use crate::Start;

/// A chat with its running agent, if any.
#[derive(Debug, Clone)]
pub struct Chat {
    pub info: ChatInfo,
    pub agent: Option<AgentInfo>,
}

impl Chat {
    pub fn new(info: ChatInfo, agents: &[AgentInfo]) -> Self {
        let agent = agents.iter().find(|a| a.id == info.id).cloned();
        Self { info, agent }
    }

    /// Its agent's state, else idle: a chat owns no worktree to be gone,
    /// and is closed rather than stale.
    pub fn status(&self) -> TaskStatus {
        self.agent
            .as_ref()
            .map_or(TaskStatus::Idle, |a| agent_status(a.state))
    }

    /// When the user last prompted its agent, else when it was made.
    pub fn last(&self) -> u64 {
        self.info.last_turn_at.unwrap_or(self.info.created_at)
    }
}

/// When autoclean closes a chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closes {
    /// Kept: never closed for want of turns.
    Kept,
    /// Autoclean is off.
    Off,
    /// Its time is past: closed as soon as its agent is idle with nobody
    /// attached or typing.
    Due,
    /// In this many seconds.
    In(u64),
}

impl Closes {
    /// When autoclean closes `chat`, from `now` (a Unix timestamp).
    pub fn of(chat: &ChatInfo, now: u64) -> Self {
        match chat.closes_at {
            _ if chat.keep => Self::Kept,
            None => Self::Off,
            Some(at) if at <= now => Self::Due,
            Some(at) => Self::In(at - now),
        }
    }

    /// Less than a day left (or none): the chat is in its last day.
    pub fn last_day(self) -> bool {
        match self {
            Self::Due => true,
            Self::In(left) => left < 86400,
            Self::Kept | Self::Off => false,
        }
    }
}

/// A time left, rounded up: `10m`, `5h`, `3d` (`3d` from a turn just now).
pub fn time_left(secs: u64) -> String {
    let minutes = secs.div_ceil(60);
    let hours = secs.div_ceil(3600);
    if minutes < 60 {
        format!("{minutes}m")
    } else if hours < 24 {
        format!("{hours}h")
    } else {
        format!("{}d", secs.div_ceil(86400))
    }
}

/// How messages to the user name a chat: `'name' (c-…)`, or its id alone.
pub fn describe(chat: &ChatInfo) -> String {
    let label = chat.label();
    if label == chat.id {
        chat.id.clone()
    } else {
        format!("'{label}' ({})", chat.id)
    }
}

/// What `tam chat` was asked for.
pub struct NewChat {
    pub repo: Option<String>,
    pub name: Option<String>,
    pub session: Option<String>,
    pub keep: bool,
    pub prompt: Option<String>,
    pub detach: bool,
    pub json: bool,
}

/// `tam chat`: make a chat on a repository's main checkout and start its
/// agent, then attach. A chat whose agent cannot start is closed again: it
/// never ran, and it would hold the session asked for.
pub async fn start(new: NewChat) -> Result<()> {
    let dir = repository(new.repo.as_deref())?;
    let mut client = Client::connect().await?;
    let chat = client
        .create_chat(&dir, new.name.as_deref(), new.session.as_deref())
        .await?;
    let start = Start {
        session: SessionChoice::Current,
        prompt: new.prompt,
        args: vec![],
    };
    let started = async {
        if new.keep {
            client.keep_chat(&chat.id, true).await?;
        }
        client
            .run_agent(&chat.id, start.session, start.prompt, start.args)
            .await
    }
    .await;
    let (id, resumed) = match started {
        Ok(started) => started,
        Err(e) => {
            let closed = match client.close_chat(&chat.id).await {
                Ok(_) => "the chat is closed again".to_string(),
                Err(close) => format!("closing the chat failed too: {close:#}"),
            };
            return Err(e.context(format!(
                "cannot start the agent of chat {} ({closed})",
                describe(&chat)
            )));
        }
    };

    if script::attaching(new.detach || new.json) {
        println!(
            "Started chat {} on {}",
            describe(&chat),
            crate::shorten_home(&chat.dir.display().to_string())
        );
        let client = Client::connect().await?;
        return client.attach(&id, &chat.label()).await;
    }
    report_started(&mut client, &chat, resumed, new.json).await
}

/// The main checkout of the repository `repo` names (as `tam new -r` takes
/// it), or of the current one.
fn repository(repo: Option<&str>) -> Result<PathBuf> {
    let cwd = std::fs::canonicalize(".")?;
    match repo {
        None => crate::repos::main_checkout(&cwd).map_err(|_| {
            anyhow::anyhow!(
                "not inside a git repository: run this from within a project, or name one \
                 (tam chat REPO)"
            )
        }),
        Some(repo) => {
            let wt_config = tam_worktree::config::load_config()?;
            let roots = crate::repos::resolve(&[repo.to_string()], &cwd, || {
                crate::repos::discover(&wt_config)
            })?;
            roots.into_iter().next().context("no repository found")
        }
    }
}

/// `tam run CHAT`, attaching: start the chat's agent in its session.
pub async fn run_and_attach(client: &mut Client, chat: &ChatInfo, start: Start) -> Result<()> {
    let (id, _) = client
        .run_agent(&chat.id, start.session, start.prompt, start.args)
        .await
        .map_err(|e| cannot_resume(chat, e))?;
    let client = Client::connect().await?;
    client.attach(&id, &chat.label()).await
}

/// `tam run CHAT` without attaching: start the chat's agent, then tell what
/// started (the JSON receipt, or a line).
pub async fn start_detached(
    client: &mut Client,
    chat: &ChatInfo,
    start: Start,
    json: bool,
) -> Result<()> {
    match client
        .run_agent(&chat.id, start.session, start.prompt, start.args)
        .await
        .map_err(|e| cannot_resume(chat, e))
    {
        Ok((_, resumed)) => report_started(client, chat, resumed, json).await,
        Err(e) if json => {
            print_json(&schema::Receipt {
                schema_version: SCHEMA_VERSION,
                of: Subject::Chat(chat.id.clone()),
                members: vec![schema::Started::chat(chat)],
                error: Some(ErrorInfo::new(&e)),
            })?;
            script::finish(Some(e), true)
        }
        Err(e) => Err(e),
    }
}

/// Why a chat's agent did not start, with the way out when its session can
/// no longer be resumed (Claude deleted the transcript): a chat is its
/// session, so another session is another chat.
fn cannot_resume(chat: &ChatInfo, e: anyhow::Error) -> anyhow::Error {
    let Some(message) = session_gone(&e) else {
        return e;
    };
    anyhow::anyhow!(
        "{message}. A chat is its session: close this one (tam close {}) and start another \
         (tam chat {})",
        chat.id,
        crate::shorten_home(&chat.dir.display().to_string())
    )
}

/// Why a chat's agent did not start, when it is that its session can no
/// longer be resumed (Claude deleted the transcript), without the daemon's
/// advice to start a new session: a chat cannot.
pub fn session_gone(e: &anyhow::Error) -> Option<String> {
    let message = format!("{e:#}");
    message
        .contains("can no longer be resumed")
        .then(|| message.trim_end_matches(": start a new one").to_string())
}

/// When `name`, which named no task, names a chat: an error saying so,
/// for the task commands (`drop`, `merge`).
pub async fn not_a_task(client: &mut Client, name: &str) -> Option<anyhow::Error> {
    match client.resolve_target(name).await {
        Ok(crate::client::Target::Chat(chat, _)) => Some(script::error(
            ErrorKind::NotFound,
            format!(
                "'{name}' is chat {}, not a task (tam close closes a chat)",
                chat.id
            ),
        )),
        _ => None,
    }
}

/// Tell that the chat's agent started: the receipt, or a line.
async fn report_started(
    client: &mut Client,
    chat: &ChatInfo,
    resumed: Option<String>,
    json: bool,
) -> Result<()> {
    // The agent the daemon picked is that of the run it just recorded.
    let (_, runs) = client.get_chat(&chat.id).await?;
    let agent = runs.last().map(|r| r.provider.clone());
    if json {
        let mut started = schema::Started::chat(chat);
        started.agent = agent;
        started.resumed_session = resumed;
        return print_json(&schema::Receipt {
            schema_version: SCHEMA_VERSION,
            of: Subject::Chat(chat.id.clone()),
            members: vec![started],
            error: None,
        });
    }
    let session = match &resumed {
        Some(id) => format!("resumed session {id}"),
        None => "new session".into(),
    };
    println!(
        "Started chat {} on {} ({session}). Attach with: tam attach {}",
        describe(chat),
        crate::shorten_home(&chat.dir.display().to_string()),
        chat.id
    );
    Ok(())
}

/// The chat `target` names (an id or a name: only chats here). A task's
/// name gets a hint.
async fn find(client: &mut Client, target: &str) -> Result<ChatInfo> {
    match client.get_chat(target).await {
        Ok((chat, _)) => Ok(chat),
        Err(e) if script::kind_of(&e) == ErrorKind::NotFound => {
            match client.get_task(target).await {
                Ok(_) => Err(script::error(
                    ErrorKind::NotFound,
                    format!("'{target}' is a task, not a chat (tam drop drops a task)"),
                )),
                Err(_) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// `tam close`: stop the chat's agent and forget the chat, once confirmed
/// (`yes`, or on the terminal). Nothing on disk changes: Claude keeps its
/// transcript, and `tam chat --session` makes the chat again.
pub async fn close(target: &str, yes: bool, json: bool) -> Result<()> {
    let mut client = Client::connect().await?;
    let chat = find(&mut client, target).await?;
    if !yes {
        if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
            return Err(script::error(
                ErrorKind::Failed,
                format!(
                    "chat {} not closed: no terminal to confirm on (pass --yes)",
                    describe(&chat)
                ),
            ));
        }
        let running = client.list_agents().await?.iter().any(|a| a.id == chat.id);
        let stops = if running {
            " Its agent will be stopped."
        } else {
            ""
        };
        let question = format!("Close chat {}?{stops} [y/N] ", describe(&chat));
        if !crate::confirm(&question)? {
            if json {
                return Err(script::error(
                    ErrorKind::Failed,
                    format!("chat {} kept: not confirmed", describe(&chat)),
                ));
            }
            eprintln!("Chat {} kept.", describe(&chat));
            std::process::exit(1);
        }
    }
    let (closed, _) = client.close_chat(&chat.id).await?;
    if json {
        return print_json(&schema::ChatDoc::new(&closed, false));
    }
    match &closed.session {
        Some(session) => println!(
            "Closed chat {}. To make it again: tam chat {} --session {session}",
            describe(&closed),
            crate::shorten_home(&closed.dir.display().to_string())
        ),
        None => println!("Closed chat {}.", describe(&closed)),
    }
    Ok(())
}

/// `tam rename`: name a chat, or with no name take its name away.
pub async fn rename(target: &str, name: Option<&str>, json: bool) -> Result<()> {
    let mut client = Client::connect().await?;
    let chat = find(&mut client, target).await?;
    let renamed = client.rename_chat(&chat.id, name).await?;
    if json {
        return print_json(&schema::ChatDoc::new(&renamed, true));
    }
    match &renamed.name {
        Some(name) => println!("Chat {} is named '{name}'.", renamed.id),
        None => println!("Chat {} has no name.", renamed.id),
    }
    Ok(())
}

/// `tam keep`: keep a chat (never closed for want of turns), or with `off`
/// stop keeping it.
pub async fn keep(target: &str, off: bool, json: bool) -> Result<()> {
    let mut client = Client::connect().await?;
    let chat = find(&mut client, target).await?;
    let kept = client.keep_chat(&chat.id, !off).await?;
    if json {
        return print_json(&schema::ChatDoc::new(&kept, true));
    }
    if kept.keep {
        println!("Chat {} is kept.", describe(&kept));
    } else {
        println!("Chat {} is no longer kept.", describe(&kept));
    }
    Ok(())
}

fn print_json<T: serde::Serialize>(doc: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(doc)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Commands};
    use clap::Parser;
    use tam_proto::{
        AgentDetail, AgentResponse, AgentState, MemberOutcome, MemberState, RunInfo, TaskInfo,
    };

    fn info(name: Option<&str>) -> ChatInfo {
        ChatInfo {
            id: "c-0000abcd".into(),
            name: name.map(str::to_string),
            repo: "api".into(),
            dir: "/src/api".into(),
            created_at: 10,
            keep: true,
            session: Some("s-1".into()),
            last_turn_at: Some(20),
            run_count: 1,
            title: None,
            last_prompt: None,
            closes_at: None,
        }
    }

    fn outcome(member: &str, label: &str) -> MemberOutcome {
        MemberOutcome {
            member: member.into(),
            task: String::new(),
            label: label.into(),
            state: MemberState::Ready,
            provider: Some("claude".into()),
            session_id: Some("s-1".into()),
            exit_code: None,
            pending: None,
            error: None,
            response: Some(AgentResponse {
                text: "done".into(),
                at: 30,
            }),
        }
    }

    fn keys(doc: &serde_json::Value) -> Vec<&str> {
        doc.as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn a_chats_documents_have_chat_where_a_tasks_have_task() {
        let chat = info(None);
        let doc = serde_json::to_value(schema::Outcomes::chat(
            &chat,
            &[outcome(&chat.id, &chat.label())],
            None,
        ))
        .unwrap();
        assert_eq!(doc["chat"], "c-0000abcd");
        assert!(doc.get("task").is_none());
        let entry = &doc["members"][0];
        assert_eq!(entry["id"], "c-0000abcd");
        assert!(entry["name"].is_null());
        assert_eq!(entry["label"], "c-0000abcd");
        assert_eq!(entry["response"], "done");

        // A task's are as they were: `task`, and every member's name.
        let task = TaskInfo {
            name: "feat".into(),
            members: vec![],
            created_at: 0,
            run_count: 0,
            last_activity: None,
        };
        let doc = serde_json::to_value(schema::Outcomes::new(
            &task,
            &[outcome("m-1", "feat")],
            None,
        ))
        .unwrap();
        assert_eq!(keys(&doc), ["error", "members", "schema_version", "task"]);
        assert_eq!(doc["members"][0]["name"], "feat");

        // A chat's receipt: no branch.
        let mut started = schema::Started::chat(&info(Some("debug")));
        started.agent = Some("claude".into());
        let doc = serde_json::to_value(schema::Receipt {
            schema_version: SCHEMA_VERSION,
            of: Subject::Chat("c-0000abcd".into()),
            members: vec![started],
            error: None,
        })
        .unwrap();
        assert_eq!(doc["chat"], "c-0000abcd");
        let entry = &doc["members"][0];
        assert_eq!(entry["name"], "debug");
        assert_eq!(entry["label"], "debug");
        assert!(entry.get("branch").is_none());
        assert!(entry["resumed_session"].is_null());
    }

    #[test]
    fn ps_lists_chats_after_the_tasks_key() {
        let agent = AgentInfo {
            id: "c-0000abcd".into(),
            provider: "claude".into(),
            dir: "/src/api".into(),
            state: AgentState::Question,
            pid: Some(7),
            uptime_secs: 1,
            viewers: 0,
            context_percent: Some(40),
            task: None,
            detail: Box::new(AgentDetail::default()),
            state_since: None,
        };
        let idle = ChatInfo {
            id: "c-1111beef".into(),
            session: None,
            ..info(None)
        };
        let run = RunInfo {
            member: "c-0000abcd".into(),
            provider: "claude".into(),
            session_id: Some("s-1".into()),
            started_at: 15,
            ended_at: None,
            exit_code: None,
        };
        let doc = serde_json::to_value(schema::Ps::new(
            vec![],
            vec![(idle, vec![]), (info(Some("debug")), vec![run])],
            &[agent],
        ))
        .unwrap();
        assert_eq!(keys(&doc), ["chats", "schema_version", "tasks"]);
        // The one waiting on the user first.
        let chats = doc["chats"].as_array().unwrap();
        assert_eq!(chats[0]["id"], "c-0000abcd");
        assert_eq!(chats[0]["status"], "question");
        assert_eq!(chats[0]["state"], "question");
        assert_eq!(chats[0]["label"], "debug");
        assert_eq!(chats[0]["session_id"], "s-1");
        assert_eq!(chats[0]["last_run"]["started_at"], 15);
        assert_eq!(chats[1]["status"], "idle");
        assert!(chats[1]["name"].is_null() && chats[1]["agent"].is_null());
        assert!(chats[1]["session_id"].is_null());
        // Every key present, null without a value.
        assert_eq!(keys(&chats[0]), keys(&chats[1]));
        assert!(chats[1]["last_run"].is_null());
    }

    #[test]
    fn a_closed_chats_document_has_its_session() {
        let doc = serde_json::to_value(schema::ChatDoc::new(&info(Some("debug")), false)).unwrap();
        assert_eq!(doc["chat"], "c-0000abcd");
        assert_eq!(doc["name"], "debug");
        assert_eq!(doc["session_id"], "s-1");
        assert_eq!(doc["dir"], "/src/api");
        assert_eq!(doc["open"], false);
        assert_eq!(doc["keep"], true);
    }

    #[test]
    fn chats_are_described_by_label_and_id() {
        assert_eq!(describe(&info(None)), "c-0000abcd");
        assert_eq!(describe(&info(Some("debug"))), "'debug' (c-0000abcd)");
        // Claude's title, when the user gave no name.
        let titled = ChatInfo {
            title: Some("Fix the login".into()),
            ..info(None)
        };
        assert_eq!(describe(&titled), "'Fix the login' (c-0000abcd)");
    }

    #[test]
    fn the_chat_commands_parse() {
        let parse = |args: &[&str]| Cli::try_parse_from(["tam"].iter().chain(args));
        match parse(&[
            "chat",
            "api",
            "-n",
            "debug",
            "--session",
            "s-1",
            "--keep",
            "-p",
            "hi",
            "-d",
        ])
        .unwrap()
        .command
        {
            Some(Commands::Chat {
                repo,
                name,
                session,
                keep,
                prompt,
                detach,
                json,
            }) => {
                assert_eq!(repo.as_deref(), Some("api"));
                assert_eq!(name.as_deref(), Some("debug"));
                assert_eq!(session.as_deref(), Some("s-1"));
                assert!(keep && detach && !json);
                assert_eq!(prompt.as_deref(), Some("hi"));
            }
            _ => panic!("not a chat"),
        }
        // A repository named like a command is still a repository.
        match parse(&["chat", "close"]).unwrap().command {
            Some(Commands::Chat { repo, .. }) => assert_eq!(repo.as_deref(), Some("close")),
            _ => panic!("not a chat"),
        }
        match parse(&["close", "debug", "-y", "--json"]).unwrap().command {
            Some(Commands::Close { chat, yes, json }) => {
                assert_eq!(chat, "debug");
                assert!(yes && json);
            }
            _ => panic!("not a close"),
        }
        match parse(&["rename", "c-0000abcd"]).unwrap().command {
            Some(Commands::Rename { chat, name, .. }) => {
                assert_eq!(chat, "c-0000abcd");
                assert!(name.is_none());
            }
            _ => panic!("not a rename"),
        }
        match parse(&["keep", "debug", "--off"]).unwrap().command {
            Some(Commands::Keep { chat, off, .. }) => assert!(chat == "debug" && off),
            _ => panic!("not a keep"),
        }
        assert!(parse(&["close"]).is_err());
        assert!(parse(&["rename", "a", "b", "c"]).is_err());
    }
}
