use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Protocol version. Increment on any breaking wire format change.
pub const PROTOCOL_VERSION: u32 = 2;

/// What an agent is doing right now.
/// Agents that exit (successfully or with error) are cleaned up immediately —
/// exit is an event, not a state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Actively producing output
    Working,
    /// Needs user action to continue (e.g. permission prompt)
    Blocked,
    /// Finished current task, waiting for new prompt
    Input,
    /// Running but inactive (heuristic fallback)
    Idle,
    /// Asked the user a question and waits for the answer
    Question,
    /// The last turn failed (e.g. API error); waits for a new prompt
    Error,
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Working => write!(f, "working"),
            Self::Blocked => write!(f, "blocked"),
            Self::Input => write!(f, "input"),
            Self::Idle => write!(f, "idle"),
            Self::Question => write!(f, "question"),
            Self::Error => write!(f, "error"),
        }
    }
}

/// Summary info about a running agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub provider: String,
    pub dir: PathBuf,
    pub state: AgentState,
    pub pid: Option<u32>,
    pub uptime_secs: u64,
    #[serde(default)]
    pub viewers: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// What the agent reported through its hooks (empty for providers
    /// without hooks).
    #[serde(default, skip_serializing_if = "AgentDetail::is_empty")]
    pub detail: Box<AgentDetail>,
}

/// Details an agent reports through its hooks, kept by the daemon per agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDetail {
    /// Agent session id (changes on e.g. `/clear`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// What the agent waits on while `blocked` or `question`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
    /// Text of the agent's last response, once a turn finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<String>,
    /// Why the last turn failed, while `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AgentError>,
    /// Last tool call refused by the agent's auto mode in the current turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied: Option<DeniedTool>,
}

impl AgentDetail {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A permission or a question the agent waits on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// Tool asking for permission (e.g. `Bash`), or the question tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// One-line summary: the command, file, or question text.
    pub summary: String,
}

/// Cause of a failed turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentError {
    /// Error kind as reported by the agent (e.g. `rate_limit`).
    pub kind: String,
    /// Human-readable message, when the agent gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A tool call refused without asking the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeniedTool {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Summary of a task, as derived by the daemon from its ledger.
///
/// New fields must be optional (`#[serde(default)]`) so older clients keep
/// decoding it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInfo {
    pub name: String,
    pub dir: PathBuf,
    /// The task owns its directory (a worktree created for it).
    pub owned: bool,
    /// Unix timestamp (seconds) of the task's creation.
    pub created_at: u64,
    /// Number of agent runs started in this task.
    #[serde(default)]
    pub run_count: usize,
    /// Unix timestamp (seconds) of the latest task or run event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<u64>,
}

/// One agent run in a task, from the daemon's ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunInfo {
    pub provider: String,
    /// Agent session id, once known (resumed session or reported by the agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub started_at: u64,
    /// `None` while the run is still going (or if the daemon died under it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    /// Exit code of the agent; -1 when it was killed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// Why the daemon refused to drop one worktree without `force`: the work the
/// drop would lose (only what is at risk), or why it cannot be checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeReport {
    pub path: PathBuf,
    /// Branch checked out in the worktree, `None` on a detached HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Uncommitted changes, as `git status --porcelain` lines.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncommitted: Vec<String>,
    /// Commits (`<short sha> <subject>`) no other branch, tag or remote
    /// branch holds, lost with the branch (or with a detached HEAD).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmerged: Vec<String>,
    /// Why the directory cannot be checked (e.g. not a git worktree). A
    /// forced drop forgets the task and leaves the directory in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// Client -> Daemon request.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Spawn {
        provider: String,
        dir: PathBuf,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_session: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        /// Task the agent runs for. The daemon records the run in the task's
        /// ledger (start, session, end). Must name an existing task.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
    },
    List,
    Kill {
        id: String,
    },
    Attach {
        id: String,
        cols: u16,
        rows: u16,
    },
    /// Fetch the tail of an agent's scrollback buffer.
    Scrollback {
        id: String,
    },
    /// Resize an agent's PTY (e.g. when a viewer's terminal changes size).
    Resize {
        id: String,
        cols: u16,
        rows: u16,
    },
    /// Hook callback from an agent process (e.g. Claude Code hooks).
    HookEvent {
        agent_id: String,
        /// Hook event name (e.g. `Stop`, `PermissionRequest`).
        event: String,
        /// The hook's JSON input, as the agent passed it (bulky fields such
        /// as tool output stripped by `tam hook-notify`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
    },
    Shutdown,
    /// Version handshake — sent by client on connect.
    Hello {
        protocol_version: u32,
    },
    /// Record a new task. Replies `Task`; pushes `TaskCreated`.
    CreateTask {
        name: String,
        dir: PathBuf,
        owned: bool,
    },
    /// All active tasks. Replies `Tasks`.
    ListTasks,
    /// One task with its run history. Replies `Task`.
    GetTask {
        name: String,
    },
    /// Drop a task: stop its agent, remove its worktree (owned tasks) and
    /// forget it. Replies `Dropped` and pushes `TaskDropped`; or replies
    /// `DropRefused`, changing nothing, when the drop would lose work and
    /// `force` is unset; or replies `Error` when the worktree cannot be
    /// removed (the agent is stopped then, the task kept).
    DropTask {
        name: String,
        /// Drop even if uncommitted changes or unmerged commits are lost.
        #[serde(default)]
        force: bool,
        /// Also delete the worktree's branch (`git branch -D`). Without
        /// `force`, refused while it has unmerged commits.
        #[serde(default)]
        delete_branch: bool,
    },
    /// Receive pushed `Event`s on this connection from now on. Replies `Ok`.
    Subscribe,
}

/// Daemon -> Client response.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Spawned {
        id: String,
    },
    Agents {
        agents: Vec<AgentInfo>,
    },
    Attached,
    Scrollback {
        data: String,
    },
    Ok,
    Error {
        message: String,
    },
    Hello {
        protocol_version: u32,
    },
    Tasks {
        tasks: Vec<TaskInfo>,
    },
    Task {
        task: TaskInfo,
        #[serde(default)]
        runs: Vec<RunInfo>,
    },
    /// The task was dropped. `warnings` tell what was left behind (e.g. a
    /// branch git refused to delete).
    Dropped {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
    },
    /// The drop would lose work: nothing was done. Retry with `force`.
    DropRefused {
        worktrees: Vec<WorktreeReport>,
    },
}

/// Daemon -> Client pushed event (unsolicited).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    AgentSpawned {
        id: String,
        info: AgentInfo,
    },
    StateChange {
        id: String,
        old: AgentState,
        new: AgentState,
    },
    AgentExited {
        id: String,
        exit_code: i32,
    },
    ContextUpdate {
        id: String,
        context_percent: u8,
    },
    /// An agent's hook details changed (pending action, last message, error…).
    AgentUpdated {
        id: String,
        info: AgentInfo,
    },
    TaskCreated {
        task: TaskInfo,
    },
    /// A task's summary changed (e.g. a run started or ended).
    TaskUpdated {
        task: TaskInfo,
    },
    TaskDropped {
        name: String,
    },
}

/// Any message sent from daemon to client (response or pushed event).
/// Uses untagged serde — tries Response first, then Event.
/// The type values don't overlap so deserialization is unambiguous.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Response(Response),
    Event(Event),
}

/// Default socket path for daemon communication.
pub fn default_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("tam").join("sock")
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".tam").join("sock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_state_display() {
        assert_eq!(AgentState::Working.to_string(), "working");
        assert_eq!(AgentState::Blocked.to_string(), "blocked");
        assert_eq!(AgentState::Input.to_string(), "input");
        assert_eq!(AgentState::Idle.to_string(), "idle");
        assert_eq!(AgentState::Question.to_string(), "question");
        assert_eq!(AgentState::Error.to_string(), "error");
    }

    #[test]
    fn agent_state_serde_roundtrip() {
        for state in [
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Input,
            AgentState::Idle,
            AgentState::Question,
            AgentState::Error,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let back: AgentState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, back);
        }
    }

    #[test]
    fn agent_state_serde_values() {
        assert_eq!(
            serde_json::to_string(&AgentState::Working).unwrap(),
            "\"working\""
        );
        assert_eq!(
            serde_json::to_string(&AgentState::Blocked).unwrap(),
            "\"blocked\""
        );
        assert_eq!(
            serde_json::to_string(&AgentState::Input).unwrap(),
            "\"input\""
        );
    }

    #[test]
    fn request_spawn_roundtrip() {
        let req = Request::Spawn {
            provider: "claude".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("fix-auth".into()),
            args: vec!["--verbose".into()],
            resume_session: None,
            prompt: None,
            task: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::Spawn {
                provider,
                dir,
                id,
                args,
                ..
            } => {
                assert_eq!(provider, "claude");
                assert_eq!(dir, PathBuf::from("/tmp"));
                assert_eq!(id, Some("fix-auth".into()));
                assert_eq!(args, vec!["--verbose"]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn request_spawn_omits_none_id() {
        let req = Request::Spawn {
            provider: "claude".into(),
            dir: PathBuf::from("/tmp"),
            id: None,
            args: vec![],
            resume_session: None,
            prompt: None,
            task: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            !json.contains("\"id\""),
            "id:None should be omitted: {}",
            json
        );
    }

    #[test]
    fn request_list_serde() {
        let json = serde_json::to_string(&Request::List).unwrap();
        assert_eq!(json, r#"{"type":"list"}"#);
        let back: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Request::List));
    }

    #[test]
    fn request_kill_serde() {
        let req = Request::Kill { id: "test".into() };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::Kill { id } => assert_eq!(id, "test"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn request_shutdown_serde() {
        let json = serde_json::to_string(&Request::Shutdown).unwrap();
        assert_eq!(json, r#"{"type":"shutdown"}"#);
    }

    #[test]
    fn request_hook_event_serde() {
        let req = Request::HookEvent {
            agent_id: "fix-auth".into(),
            event: "Stop".into(),
            payload: Some(serde_json::json!({"last_assistant_message": "done"})),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"hook_event\""));
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::HookEvent {
                agent_id,
                event,
                payload,
            } => {
                assert_eq!(agent_id, "fix-auth");
                assert_eq!(event, "Stop");
                assert_eq!(payload.unwrap()["last_assistant_message"], "done");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn request_hook_event_without_payload() {
        let json = r#"{"type":"hook_event","agent_id":"a","event":"stop"}"#;
        match serde_json::from_str::<Request>(json).unwrap() {
            Request::HookEvent { payload, .. } => assert!(payload.is_none()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn agent_detail_omitted_when_empty() {
        let mut info = AgentInfo {
            id: "a".into(),
            provider: "claude".into(),
            dir: PathBuf::from("/tmp"),
            state: AgentState::Question,
            pid: None,
            uptime_secs: 0,
            viewers: 0,
            context_percent: None,
            task: None,
            detail: Default::default(),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("detail"), "{json}");
        assert!(json.contains("\"state\":\"question\""));

        info.detail.pending = Some(Pending {
            tool: Some("AskUserQuestion".into()),
            summary: "Tea or coffee?".into(),
        });
        let json = serde_json::to_string(&info).unwrap();
        let back: AgentInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.detail, info.detail);
    }

    #[test]
    fn response_spawned_serde() {
        let resp = Response::Spawned {
            id: "fix-auth".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        match back {
            Response::Spawned { id } => assert_eq!(id, "fix-auth"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn response_agents_serde() {
        let resp = Response::Agents {
            agents: vec![AgentInfo {
                id: "test".into(),
                provider: "claude".into(),
                dir: PathBuf::from("/tmp"),
                state: AgentState::Working,
                pid: Some(1234),
                uptime_secs: 60,
                viewers: 0,
                context_percent: None,
                task: None,
                detail: Default::default(),
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        match back {
            Response::Agents { agents } => {
                assert_eq!(agents.len(), 1);
                assert_eq!(agents[0].id, "test");
                assert_eq!(agents[0].state, AgentState::Working);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn response_ok_serde() {
        let json = serde_json::to_string(&Response::Ok).unwrap();
        assert_eq!(json, r#"{"type":"ok"}"#);
    }

    #[test]
    fn response_error_serde() {
        let resp = Response::Error {
            message: "boom".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("boom"));
    }

    #[test]
    fn deserialize_from_wire_format() {
        // Pin the exact wire format expected by clients
        let json = r#"{"type":"spawn","provider":"claude","dir":"/tmp","args":[]}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::Spawn {
                provider,
                dir,
                id,
                args,
                resume_session,
                prompt,
                task,
            } => {
                assert_eq!(provider, "claude");
                assert_eq!(dir, PathBuf::from("/tmp"));
                assert_eq!(id, None);
                assert!(args.is_empty());
                // Backward compat: missing fields default correctly
                assert!(resume_session.is_none());
                assert!(prompt.is_none());
                assert!(task.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn request_attach_serde() {
        let req = Request::Attach {
            id: "test".into(),
            cols: 120,
            rows: 40,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::Attach { id, cols, rows } => {
                assert_eq!(id, "test");
                assert_eq!(cols, 120);
                assert_eq!(rows, 40);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn request_resize_serde() {
        let req = Request::Resize {
            id: "test".into(),
            cols: 100,
            rows: 30,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(
            json,
            r#"{"type":"resize","id":"test","cols":100,"rows":30}"#
        );
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::Resize { id, cols, rows } => {
                assert_eq!(id, "test");
                assert_eq!(cols, 100);
                assert_eq!(rows, 30);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn response_attached_serde() {
        let json = serde_json::to_string(&Response::Attached).unwrap();
        assert_eq!(json, r#"{"type":"attached"}"#);
    }

    #[test]
    fn event_state_change_serde() {
        let event = Event::StateChange {
            id: "fix-auth".into(),
            old: AgentState::Working,
            new: AgentState::Input,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"state_change\""));
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::StateChange { id, old, new } => {
                assert_eq!(id, "fix-auth");
                assert_eq!(old, AgentState::Working);
                assert_eq!(new, AgentState::Input);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn event_agent_exited_serde() {
        let event = Event::AgentExited {
            id: "test".into(),
            exit_code: 1,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"agent_exited\""));
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::AgentExited { id, exit_code } => {
                assert_eq!(id, "test");
                assert_eq!(exit_code, 1);
            }
            _ => panic!("wrong variant"),
        }
    }

    fn sample_task() -> TaskInfo {
        TaskInfo {
            name: "feat".into(),
            dir: PathBuf::from("/tmp/myapp--feat"),
            owned: true,
            created_at: 100,
            run_count: 2,
            last_activity: Some(200),
        }
    }

    #[test]
    fn event_task_created_serde() {
        let event = Event::TaskCreated {
            task: sample_task(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"task_created\""));
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::TaskCreated { task } => assert_eq!(task, sample_task()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn event_task_updated_serde() {
        let event = Event::TaskUpdated {
            task: sample_task(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"task_updated\""));
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            ServerMessage::Event(Event::TaskUpdated { .. })
        ));
    }

    #[test]
    fn task_requests_wire_format() {
        let json = serde_json::to_string(&Request::CreateTask {
            name: "feat".into(),
            dir: PathBuf::from("/tmp/feat"),
            owned: true,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"create_task","name":"feat","dir":"/tmp/feat","owned":true}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::ListTasks).unwrap(),
            r#"{"type":"list_tasks"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Subscribe).unwrap(),
            r#"{"type":"subscribe"}"#
        );
        let req: Request = serde_json::from_str(r#"{"type":"drop_task","name":"feat"}"#).unwrap();
        match req {
            Request::DropTask {
                name,
                force,
                delete_branch,
            } => {
                assert_eq!(name, "feat");
                assert!(!force);
                assert!(!delete_branch);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn drop_responses_wire_format() {
        assert_eq!(
            serde_json::to_string(&Response::Dropped { warnings: vec![] }).unwrap(),
            r#"{"type":"dropped"}"#
        );
        let refused = Response::DropRefused {
            worktrees: vec![WorktreeReport {
                path: PathBuf::from("/wt/app--feat"),
                branch: Some("feat".into()),
                uncommitted: vec!["?? notes.txt".into()],
                unmerged: vec![],
                problem: None,
            }],
        };
        let json = serde_json::to_string(&refused).unwrap();
        assert_eq!(
            json,
            r#"{"type":"drop_refused","worktrees":[{"path":"/wt/app--feat","branch":"feat","uncommitted":["?? notes.txt"]}]}"#
        );
        match serde_json::from_str::<ServerMessage>(&json).unwrap() {
            ServerMessage::Response(Response::DropRefused { worktrees }) => {
                assert_eq!(worktrees[0].uncommitted, vec!["?? notes.txt"]);
                assert!(worktrees[0].unmerged.is_empty());
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn response_task_is_not_mistaken_for_an_event() {
        let resp = Response::Task {
            task: sample_task(),
            runs: vec![RunInfo {
                provider: "claude".into(),
                session_id: Some("s1".into()),
                started_at: 150,
                ended_at: None,
                exit_code: None,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(
            !json.contains("ended_at"),
            "open run omits ended_at: {json}"
        );
        match serde_json::from_str::<ServerMessage>(&json).unwrap() {
            ServerMessage::Response(Response::Task { task, runs }) => {
                assert_eq!(task, sample_task());
                assert_eq!(runs.len(), 1);
                assert_eq!(runs[0].session_id.as_deref(), Some("s1"));
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn spawn_task_field_roundtrip() {
        let req = Request::Spawn {
            provider: "claude".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("feat".into()),
            args: vec![],
            resume_session: None,
            prompt: None,
            task: Some("feat".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"task\":\"feat\""));
        match serde_json::from_str::<Request>(&json).unwrap() {
            Request::Spawn { task, .. } => assert_eq!(task.as_deref(), Some("feat")),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn event_task_dropped_serde() {
        let event = Event::TaskDropped {
            name: "feat".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"task_dropped\""));
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::TaskDropped { name } => assert_eq!(name, "feat"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn deserialize_unknown_type_fails() {
        let json = r#"{"type":"explode"}"#;
        assert!(serde_json::from_str::<Request>(json).is_err());
    }

    #[test]
    fn deserialize_garbage_fails() {
        assert!(serde_json::from_str::<Request>("not json").is_err());
    }

    #[test]
    fn agent_info_task_field_optional() {
        // task field should be omitted when None
        let info = AgentInfo {
            id: "test".into(),
            provider: "claude".into(),
            dir: PathBuf::from("/tmp"),
            state: AgentState::Working,
            pid: Some(1234),
            uptime_secs: 60,
            viewers: 0,
            context_percent: None,
            task: None,
            detail: Default::default(),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("\"task\""));

        // task field should be present when Some
        let info_with_task = AgentInfo {
            task: Some("feat".into()),
            ..info
        };
        let json = serde_json::to_string(&info_with_task).unwrap();
        assert!(json.contains("\"task\":\"feat\""));
    }

    #[test]
    fn agent_info_backward_compat_no_task() {
        // Old wire format without task field should deserialize fine
        let json = r#"{"id":"test","provider":"claude","dir":"/tmp","state":"working","pid":1234,"uptime_secs":60,"viewers":0}"#;
        let info: AgentInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.id, "test");
        assert!(info.task.is_none());
    }
}
