use std::collections::{HashMap, HashSet};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tam_proto::{AgentInfo, AgentState, Event, NewMember, Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Mutex};
use tracing::{error, info, warn};

use crate::agent::Agent;
use crate::ledger::Ledger;
use crate::notify::{self, Notification, NotifyConfig};
use crate::provider;
use crate::worktrees::{self, Checked, Removal};

pub struct Daemon {
    socket_path: PathBuf,
    ledger_path: PathBuf,
}

struct DaemonState {
    agents: HashMap<String, Agent>,
    /// Task ledger — the daemon is its only writer.
    ledger: Ledger,
    /// Tasks being dropped: no agent may start in them meanwhile.
    dropping: HashSet<String>,
    next_id: u64,
    shutdown: bool,
    event_tx: broadcast::Sender<Event>,
    socket_path: PathBuf,
    connected_clients: Arc<AtomicUsize>,
    /// When set, the daemon will shut down after this instant if still idle.
    idle_deadline: Option<Instant>,
}

/// Default idle timeout before daemon auto-shuts down (seconds).
const IDLE_TIMEOUT_SECS: u64 = 30;

/// Capacity of the event broadcast channel. A subscriber that falls this far
/// behind misses events.
const EVENT_CHANNEL_CAPACITY: usize = 256;

impl Daemon {
    /// A daemon serving `socket_path` and owning the ledger at `ledger_path`
    /// (see [`crate::ledger::default_path`]).
    pub fn new(socket_path: PathBuf, ledger_path: PathBuf) -> Self {
        Self {
            socket_path,
            ledger_path,
        }
    }

    pub async fn run(&self) -> Result<()> {
        // Lock the ledger before touching the socket: a second daemon must
        // fail here rather than unlink the running daemon's socket.
        let ledger_path = self.ledger_path.clone();
        let (ledger, report) =
            tokio::task::spawn_blocking(move || Ledger::open(&ledger_path)).await??;
        for m in &report.malformed {
            warn!(
                ledger = %ledger.path().display(),
                line = m.line,
                error = %m.error,
                "malformed ledger line set aside in .rejected"
            );
        }
        if report.migrated > 0 {
            info!(
                lines = report.migrated,
                backup = ?report.backup,
                "migrated ledger to task members"
            );
        }
        for task in &report.dropped_borrowed {
            warn!(
                task = %task,
                "dropped borrowed task: tasks own their worktrees now (its directory is untouched)"
            );
        }
        if report.compacted > 0 {
            info!(removed = report.compacted, "compacted ledger");
        }

        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let state = Arc::new(Mutex::new(DaemonState {
            agents: HashMap::new(),
            ledger,
            dropping: HashSet::new(),
            next_id: 1,
            shutdown: false,
            event_tx,
            socket_path: self.socket_path.clone(),
            connected_clients: Arc::new(AtomicUsize::new(0)),
            idle_deadline: None,
        }));

        // Create socket directory
        if let Some(parent) = self.socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Remove stale socket file
        let _ = tokio::fs::remove_file(&self.socket_path).await;

        let listener = UnixListener::bind(&self.socket_path)
            .with_context(|| format!("failed to bind socket at {:?}", self.socket_path))?;
        info!("tamd listening on {:?}", self.socket_path);

        // Write PID file next to socket
        let pid_path = self.socket_path.with_extension("pid");
        tokio::fs::write(&pid_path, std::process::id().to_string()).await?;

        // Load notification config
        let notify_config = notify::load_notify_config();
        if let Some(ref cfg) = notify_config {
            info!(command = %cfg.command, "notifications enabled");
        }

        // Spawn background task that monitors agents for state changes and exits
        let monitor_state = state.clone();
        tokio::spawn(async move {
            state_monitor(monitor_state, notify_config).await;
        });

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            let state = state.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, state).await {
                                    error!("client error: {}", e);
                                }
                            });
                        }
                        Err(e) => error!("accept error: {}", e),
                    }
                }
                _ = shutdown_signal(state.clone()) => {
                    info!("shutting down");
                    break;
                }
            }
        }

        // Cleanup
        let _ = tokio::fs::remove_file(&self.socket_path).await;
        let _ = tokio::fs::remove_file(&pid_path).await;

        Ok(())
    }
}

/// Poll until the shutdown flag is set.
async fn shutdown_signal(state: Arc<Mutex<DaemonState>>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if state.lock().await.shutdown {
            return;
        }
    }
}

/// Background task that periodically checks agents for state changes and exits.
/// Emits events on the daemon's broadcast channel.
async fn state_monitor(state_arc: Arc<Mutex<DaemonState>>, notify_config: Option<NotifyConfig>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut context_counter: u32 = 0;

    loop {
        interval.tick().await;
        context_counter += 1;

        let mut state = state_arc.lock().await;
        if state.shutdown {
            break;
        }

        // Check for exits
        state.reap_exited();

        // Check for state changes
        let changes: Vec<(String, AgentState, AgentState)> = state
            .agents
            .iter_mut()
            .filter_map(|(id, agent)| {
                agent
                    .check_state_change()
                    .map(|(old, new)| (id.clone(), old, new))
            })
            .collect();
        for (id, old, new) in changes {
            info!(id = %id, old = %old, new = %new, "state changed");
            if let (Some(cfg), Some(info)) = (&notify_config, state.agent_info(&id)) {
                notify::fire_if_matching(
                    cfg,
                    &Notification {
                        id: &id,
                        task: info.task.as_deref().unwrap_or(&id),
                        old,
                        new,
                        detail: &info.detail,
                        viewers: info.viewers,
                    },
                );
            }
            let _ = state.event_tx.send(Event::StateChange { id, old, new });
        }

        // Auto-shutdown: if no agents and no clients, start countdown
        let has_agents = !state.agents.is_empty();
        let has_clients = state.connected_clients.load(Ordering::Relaxed) > 0;

        if has_agents || has_clients {
            state.idle_deadline = None;
        } else if state.idle_deadline.is_none() {
            let deadline = Instant::now() + std::time::Duration::from_secs(IDLE_TIMEOUT_SECS);
            info!("no agents or clients, auto-shutdown in {IDLE_TIMEOUT_SECS}s");
            state.idle_deadline = Some(deadline);
        } else if let Some(deadline) = state.idle_deadline {
            if Instant::now() >= deadline {
                info!("idle timeout reached, auto-shutting down");
                state.shutdown = true;
                break;
            }
        }

        // Check context usage every 10 seconds — two-phase to avoid disk IO under lock
        if context_counter >= 10 {
            context_counter = 0;

            // Phase 1: collect metadata under lock (cheap)
            let jobs: Vec<_> = state
                .agents
                .iter()
                .map(|(id, agent)| agent.context_refresh_job(id))
                .collect();
            let session_jobs = state.session_jobs();

            let event_tx = state.event_tx.clone();
            // Release lock before disk IO
            drop(state);

            // Phase 2: do file IO outside the lock
            let results: Vec<_> = jobs
                .into_iter()
                .filter_map(|job| {
                    let usage = provider::resolve(&job.provider).context_usage(
                        job.pid,
                        &job.dir,
                        job.transcript.as_deref(),
                    );
                    usage.map(|u| (job.id, u.percent()))
                })
                .collect();

            let sessions: Vec<_> = session_jobs
                .into_iter()
                .filter_map(|job| {
                    let id = provider::resolve(&job.provider).session_id(job.pid)?;
                    Some((job, id))
                })
                .collect();

            // Phase 3: write back under lock
            if !sessions.is_empty() {
                let mut state = state_arc.lock().await;
                for (job, session_id) in &sessions {
                    state.record_session(job, session_id);
                }
            }
            if !results.is_empty() {
                let mut state = state_arc.lock().await;
                for (id, pct) in &results {
                    if let Some(agent) = state.agents.get_mut(id) {
                        if agent.set_context_percent(Some(*pct)) {
                            let _ = event_tx.send(Event::ContextUpdate {
                                id: id.clone(),
                                context_percent: *pct,
                            });
                        }
                    }
                }
            }
        }
    }
}

/// An agent whose run has no session id recorded yet.
struct SessionJob {
    member: String,
    pid: u32,
    provider: String,
}

impl DaemonState {
    fn broadcast(&self, event: Event) {
        let _ = self.event_tx.send(event);
    }

    /// An agent's info, with the task of its member.
    fn agent_info(&self, id: &str) -> Option<AgentInfo> {
        let agent = self.agents.get(id)?;
        let mut info = agent.info(id);
        info.task = agent
            .member()
            .and_then(|m| self.ledger.member_task(m))
            .map(str::to_string);
        Some(info)
    }

    /// Remove agents whose process exited, telling clients and recording the
    /// end of their run.
    fn reap_exited(&mut self) {
        let exited: Vec<(String, i32)> = self
            .agents
            .iter_mut()
            .filter_map(|(id, agent)| agent.check_exited().map(|code| (id.clone(), code)))
            .collect();
        for (id, exit_code) in exited {
            info!(id = %id, exit_code = %exit_code, "agent exited");
            if let Some(agent) = self.agents.remove(&id) {
                self.agent_gone(&id, &agent, exit_code);
            }
        }
    }

    /// An agent was removed from the table: emit `AgentExited` and record the
    /// end of its member's run.
    fn agent_gone(&mut self, id: &str, agent: &Agent, exit_code: i32) {
        self.broadcast(Event::AgentExited {
            id: id.to_string(),
            exit_code,
        });
        let Some(member) = agent.member() else {
            return;
        };
        match self.ledger.run_ended(member, exit_code) {
            Ok(Some(info)) => self.broadcast(Event::TaskUpdated { task: info }),
            Ok(None) => {}
            Err(e) => error!(member = %member, "failed to record run end: {e:#}"),
        }
    }

    /// Take the agents of the task's members out of the table, telling
    /// clients and recording the end of their runs. The caller kills them.
    fn take_task_agents(&mut self, task: &str) -> Vec<Agent> {
        let members = self.ledger.member_ids(task);
        let ids: Vec<String> = self
            .agents
            .iter()
            .filter(|(_, a)| a.member().is_some_and(|m| members.iter().any(|id| id == m)))
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                let agent = self.agents.remove(&id)?;
                self.agent_gone(&id, &agent, -1);
                Some(agent)
            })
            .collect()
    }

    /// Agents bound to a member whose current run has no session id yet.
    fn session_jobs(&self) -> Vec<SessionJob> {
        self.agents
            .values()
            .filter_map(|agent| {
                let member = agent.member()?;
                let run = self.ledger.member_runs(member).pop()?;
                (run.ended_at.is_none() && run.session_id.is_none()).then(|| SessionJob {
                    member: member.to_string(),
                    pid: agent.pid(),
                    provider: agent.provider_name().to_string(),
                })
            })
            .collect()
    }

    /// Record the session id reported by an agent's hooks for its member's
    /// current run (first one only; later ones are e.g. `/clear`).
    fn record_hook_session(&mut self, member: &str, session_id: &str) {
        match self.ledger.session_identified(member, session_id) {
            Ok(true) => {
                info!(member = %member, session = %session_id, "session identified by hook")
            }
            Ok(false) => {}
            Err(e) => error!(member = %member, "failed to record session: {e:#}"),
        }
    }

    /// Record a discovered session id, if the agent is still the one running
    /// for the member.
    fn record_session(&mut self, job: &SessionJob, session_id: &str) {
        let still_running = self
            .agents
            .values()
            .any(|a| a.member() == Some(job.member.as_str()) && a.pid() == job.pid);
        if !still_running {
            return;
        }
        match self.ledger.session_identified(&job.member, session_id) {
            Ok(true) => info!(member = %job.member, session = %session_id, "session identified"),
            Ok(false) => {}
            Err(e) => error!(member = %job.member, "failed to record session: {e:#}"),
        }
    }
}

/// Guard that decrements connected_clients on drop.
struct ClientGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Next event for a subscribed connection; never resolves otherwise.
async fn next_event(
    rx: &mut Option<broadcast::Receiver<Event>>,
) -> Result<Event, broadcast::error::RecvError> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Handle a single client connection.
/// Starts as newline-delimited JSON request/response.
/// If the client sends an Attach request, switches to raw byte streaming.
/// After a Subscribe request, daemon events are pushed to the client as JSON
/// lines between responses.
async fn handle_connection(stream: UnixStream, state: Arc<Mutex<DaemonState>>) -> Result<()> {
    let mut event_rx: Option<broadcast::Receiver<Event>> = None;
    // Track connected clients for auto-shutdown
    let client_counter = state.lock().await.connected_clients.clone();
    client_counter.fetch_add(1, Ordering::Relaxed);
    let _guard = ClientGuard {
        counter: client_counter,
    };

    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        tokio::select! {
            result = buf_reader.read_line(&mut line) => {
                if result? == 0 {
                    break; // client disconnected
                }

                let request: Request = match serde_json::from_str(line.trim()) {
                    Ok(r) => r,
                    Err(e) => {
                        let resp = Response::Error {
                            message: format!("invalid request: {}", e),
                        };
                        write_response(&mut writer, &resp).await?;
                        line.clear();
                        continue;
                    }
                };

                // Attach takes over the connection — handle it specially
                if let Request::Attach { id, cols, rows } = request {
                    let attach_resources = {
                        let state_guard = state.lock().await;
                        match state_guard.agents.get(&id) {
                            Some(agent) => {
                                agent.resize(cols, rows);
                                Ok((
                                    agent.subscribe(),
                                    agent.scrollback_contents(),
                                    agent.pty_master(),
                                    agent.viewers(),
                                ))
                            }
                            None => Err(format!("agent '{}' not found", id)),
                        }
                    };

                    match attach_resources {
                        Ok((output_rx, scrollback, master, viewers)) => {
                            write_response(&mut writer, &Response::Attached).await?;
                            let buffered = buf_reader.buffer().to_vec();
                            let reader = buf_reader.into_inner();
                            return handle_attach_session(
                                reader, writer, buffered, output_rx, scrollback, master, viewers,
                            )
                            .await;
                        }
                        Err(msg) => {
                            write_response(&mut writer, &Response::Error { message: msg }).await?;
                        }
                    }
                } else if let Request::Subscribe = request {
                    // Subscribe before replying: no event after the Ok is missed.
                    event_rx = Some(state.lock().await.event_tx.subscribe());
                    write_response(&mut writer, &Response::Ok).await?;
                } else {
                    let response = dispatch(request, &state).await;
                    write_response(&mut writer, &response).await?;
                }

                line.clear();

                if state.lock().await.shutdown {
                    break;
                }
            }
            event = next_event(&mut event_rx) => {
                match event {
                    Ok(event) => {
                        write_event(&mut writer, &event).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(missed = n, "client lagging behind, events dropped");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    Ok(())
}

/// Bidirectional raw byte relay between a client and an agent's PTY.
async fn handle_attach_session(
    mut reader: tokio::net::unix::OwnedReadHalf,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    buffered: Vec<u8>,
    mut output_rx: broadcast::Receiver<Vec<u8>>,
    scrollback: Vec<u8>,
    master: Arc<OwnedFd>,
    viewers: Arc<AtomicUsize>,
) -> Result<()> {
    viewers.fetch_add(1, Ordering::Relaxed);
    // Send scrollback so the client sees recent context
    if !scrollback.is_empty() {
        writer.write_all(&scrollback).await?;
    }

    // Forward any data buffered during the JSON handshake
    if !buffered.is_empty() {
        nix::unistd::write(&*master, &buffered)
            .map_err(|e| anyhow::anyhow!("PTY write error: {}", e))?;
    }

    let write_master = master.clone();

    // PTY output → client
    let output_task = async move {
        loop {
            match output_rx.recv().await {
                Ok(data) => {
                    if writer.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    // Client input → PTY
    let input_task = async move {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if nix::unistd::write(&*write_master, &buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    };

    tokio::select! {
        _ = output_task => {}
        _ = input_task => {}
    }

    viewers.fetch_sub(1, Ordering::Relaxed);
    Ok(())
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &Response,
) -> Result<()> {
    let mut json = serde_json::to_string(response)?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;
    Ok(())
}

async fn write_event(writer: &mut tokio::net::unix::OwnedWriteHalf, event: &Event) -> Result<()> {
    let mut json = serde_json::to_string(event)?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;
    Ok(())
}

async fn dispatch(request: Request, state: &Arc<Mutex<DaemonState>>) -> Response {
    match request {
        Request::Spawn {
            provider,
            dir,
            id,
            args,
            resume_session,
            prompt,
            member,
        } => {
            handle_spawn(
                state,
                SpawnSpec {
                    provider,
                    dir,
                    id,
                    args,
                    resume_session,
                    prompt,
                    member,
                },
            )
            .await
        }
        Request::List => handle_list(state).await,
        Request::Kill { id } => handle_kill(state, &id).await,
        Request::Attach { id, .. } => Response::Error {
            message: format!(
                "attach for '{}' should be handled by connection handler",
                id
            ),
        },
        Request::Scrollback { id } => handle_scrollback(state, &id).await,
        Request::Resize { id, cols, rows } => handle_resize(state, &id, cols, rows).await,
        Request::HookEvent {
            agent_id,
            event,
            payload,
        } => handle_hook_event(state, &agent_id, &event, payload.as_ref()).await,
        Request::Shutdown => handle_shutdown(state).await,
        Request::CreateTask { name, members } => handle_create_task(state, &name, &members).await,
        Request::ListTasks => Response::Tasks {
            tasks: state.lock().await.ledger.tasks(),
        },
        Request::GetTask { name } => {
            let state = state.lock().await;
            match state.ledger.task(&name) {
                Some(task) => Response::Task {
                    task,
                    runs: state.ledger.runs(&name),
                },
                None => Response::Error {
                    message: format!("task '{}' not found", name),
                },
            }
        }
        Request::ResolveMember { target, dir } => {
            let state = state.lock().await;
            let found = match (target, dir) {
                (Some(target), _) => state.ledger.resolve(&target),
                (None, Some(dir)) => state
                    .ledger
                    .member_at(&dir)
                    .ok_or_else(|| anyhow::anyhow!("no task member in {}", dir.display())),
                (None, None) => Err(anyhow::anyhow!("resolve_member needs a target or a dir")),
            };
            match found {
                Ok((task, member)) => {
                    let runs = state.ledger.member_runs(&member.id);
                    Response::Member {
                        task,
                        member: Box::new(member),
                        runs,
                    }
                }
                Err(e) => Response::Error {
                    message: format!("{e:#}"),
                },
            }
        }
        Request::DropTask {
            name,
            force,
            delete_branch,
        } => handle_drop_task(state, &name, force, delete_branch).await,
        Request::Subscribe => Response::Error {
            message: "subscribe should be handled by connection handler".into(),
        },
        Request::Hello { protocol_version } => {
            if protocol_version == tam_proto::PROTOCOL_VERSION {
                Response::Hello {
                    protocol_version: tam_proto::PROTOCOL_VERSION,
                }
            } else {
                Response::Error {
                    message: format!(
                        "protocol version mismatch: client={}, daemon={}. Run 'tam shutdown' to restart the daemon.",
                        protocol_version,
                        tam_proto::PROTOCOL_VERSION,
                    ),
                }
            }
        }
    }
}

struct SpawnSpec {
    provider: String,
    dir: PathBuf,
    id: Option<String>,
    args: Vec<String>,
    resume_session: Option<String>,
    prompt: Option<String>,
    member: Option<String>,
}

async fn handle_spawn(state: &Arc<Mutex<DaemonState>>, spec: SpawnSpec) -> Response {
    let mut state = state.lock().await;
    let SpawnSpec {
        provider,
        dir,
        id,
        args,
        resume_session,
        prompt,
        member,
    } = spec;

    // A member's agent runs in its worktree, under the member id.
    let (id, dir) = match &member {
        Some(member) => {
            let Some((task, info)) = state.ledger.member(member) else {
                return Response::Error {
                    message: format!("member '{}' not found", member),
                };
            };
            if id.as_ref().is_some_and(|id| id != member) {
                return Response::Error {
                    message: format!("the agent of member '{}' runs under its id", member),
                };
            }
            if !dir.as_os_str().is_empty() && dir != info.dir {
                return Response::Error {
                    message: format!(
                        "member '{}' runs in {}, not {}",
                        member,
                        info.dir.display(),
                        dir.display()
                    ),
                };
            }
            if state.dropping.contains(&task.name) {
                return Response::Error {
                    message: format!("task '{}' is being dropped", task.name),
                };
            }
            if state.agents.contains_key(member) {
                return Response::Error {
                    message: format!("'{}' already has a running agent", task.member_label(&info)),
                };
            }
            (Some(member.clone()), info.dir)
        }
        None => (id, dir),
    };

    let id = id.unwrap_or_else(|| {
        let id = format!("agent-{}", state.next_id);
        state.next_id += 1;
        id
    });

    if state.agents.contains_key(&id) {
        return Response::Error {
            message: format!("agent '{}' already exists", id),
        };
    }

    let socket_path = state.socket_path.to_string_lossy().to_string();
    let env_vars = [
        ("TAM_AGENT_ID", id.as_str()),
        ("TAM_SOCKET", socket_path.as_str()),
    ];

    let resolved = Arc::from(provider::resolve(&provider));
    match Agent::spawn(
        resolved,
        &dir,
        &args,
        resume_session.as_deref(),
        prompt.as_deref(),
        &env_vars,
    ) {
        Ok(mut agent) => {
            info!(id = %id, provider = %provider, dir = %dir.display(), "spawned agent");
            agent.set_member(member.clone());
            state.agents.insert(id.clone(), agent);
            let info = state.agent_info(&id).expect("agent just inserted");
            state.broadcast(Event::AgentSpawned {
                id: id.clone(),
                info,
            });
            if let Some(member) = member {
                match state.ledger.run_started(&member, &provider, resume_session) {
                    Ok(info) => state.broadcast(Event::TaskUpdated { task: info }),
                    Err(e) => error!(member = %member, "failed to record run start: {e:#}"),
                }
            }
            Response::Spawned { id }
        }
        Err(e) => Response::Error {
            message: format!("failed to spawn agent: {}", e),
        },
    }
}

async fn handle_list(state: &Arc<Mutex<DaemonState>>) -> Response {
    let mut state = state.lock().await;

    // Remove exited agents — exit is an event, not a state
    state.reap_exited();

    // Use cached context values — background monitor refreshes every 10s
    let agents = state
        .agents
        .keys()
        .filter_map(|id| state.agent_info(id))
        .collect();

    Response::Agents { agents }
}

async fn handle_kill(state: &Arc<Mutex<DaemonState>>, id: &str) -> Response {
    let mut state = state.lock().await;

    match state.agents.remove(id) {
        Some(agent) => {
            info!(id = %id, "killing agent");
            state.agent_gone(id, &agent, -1);
            // Kill process in background — don't block the lock
            tokio::task::spawn_blocking(move || {
                agent.kill_and_drop();
            });
            Response::Ok
        }
        None => Response::Error {
            message: format!("agent '{}' not found", id),
        },
    }
}

async fn handle_scrollback(state: &Arc<Mutex<DaemonState>>, id: &str) -> Response {
    let state = state.lock().await;
    match state.agents.get(id) {
        Some(agent) => {
            let raw = agent.scrollback_contents();
            // Return last 64KB as lossy UTF-8 (enough for vt100 to render a full screen)
            let tail = if raw.len() > 65536 {
                &raw[raw.len() - 65536..]
            } else {
                &raw
            };
            Response::Scrollback {
                data: String::from_utf8_lossy(tail).into_owned(),
            }
        }
        None => Response::Error {
            message: format!("agent '{}' not found", id),
        },
    }
}

async fn handle_resize(
    state: &Arc<Mutex<DaemonState>>,
    id: &str,
    cols: u16,
    rows: u16,
) -> Response {
    let state = state.lock().await;
    match state.agents.get(id) {
        Some(agent) => {
            agent.resize(cols, rows);
            Response::Ok
        }
        None => Response::Error {
            message: format!("agent '{}' not found", id),
        },
    }
}

async fn handle_hook_event(
    state: &Arc<Mutex<DaemonState>>,
    agent_id: &str,
    event: &str,
    payload: Option<&serde_json::Value>,
) -> Response {
    let mut state = state.lock().await;

    let Some(agent) = state.agents.get_mut(agent_id) else {
        return Response::Error {
            message: format!("agent '{}' not found", agent_id),
        };
    };
    let before = agent.info(agent_id).detail;
    let Some(result) = agent.handle_hook_event(event, payload) else {
        return Response::Error {
            message: format!("unknown hook event '{}' for agent '{}'", event, agent_id),
        };
    };
    info!(id = %agent_id, event = %event, result = ?result, "hook event");

    let changed = agent.info(agent_id).detail != before;
    let session = agent
        .member()
        .zip(agent.session_id())
        .map(|(member, session)| (member.to_string(), session.to_string()));
    if changed {
        if let Some(info) = state.agent_info(agent_id) {
            state.broadcast(Event::AgentUpdated {
                id: agent_id.to_string(),
                info,
            });
        }
    }
    if let Some((member, session_id)) = session {
        state.record_hook_session(&member, &session_id);
    }
    Response::Ok
}

async fn handle_shutdown(state: &Arc<Mutex<DaemonState>>) -> Response {
    let mut state = state.lock().await;

    // Remove all agents and kill in background
    let agents: Vec<_> = state.agents.drain().collect();
    for (id, agent) in &agents {
        state.agent_gone(id, agent, -1);
    }
    let agents: Vec<_> = agents.into_iter().map(|(_, agent)| agent).collect();
    state.shutdown = true;

    if !agents.is_empty() {
        tokio::task::spawn_blocking(move || {
            for agent in agents {
                agent.kill_and_drop();
            }
        });
    }

    info!("shutdown requested");
    Response::Ok
}

async fn handle_create_task(
    state: &Arc<Mutex<DaemonState>>,
    name: &str,
    members: &[NewMember],
) -> Response {
    let mut state = state.lock().await;
    match state.ledger.create_task(name, members) {
        Ok(task) => {
            for m in &task.members {
                info!(task = %name, member = %m.id, dir = %m.dir.display(), "task member added");
            }
            info!(task = %name, "task created");
            state.broadcast(Event::TaskCreated { task: task.clone() });
            Response::Task {
                task,
                runs: Vec::new(),
            }
        }
        Err(e) => Response::Error {
            message: format!("{e:#}"),
        },
    }
}

/// Drop a task: check its members' worktrees, stop their agents, remove the
/// worktrees (and branches), then forget the task. Git runs and the agents
/// are waited for without holding the state lock; the task is claimed
/// meanwhile so no agent starts in it.
///
/// Refuses (changing nothing) when the drop would lose work and `force` is
/// unset. The task leaves the ledger only once its worktrees are gone: if git
/// cannot remove one, the agents are stopped but the task kept.
async fn handle_drop_task(
    state: &Arc<Mutex<DaemonState>>,
    name: &str,
    force: bool,
    delete_branch: bool,
) -> Response {
    let dirs = {
        let mut state = state.lock().await;
        let Some(task) = state.ledger.task(name) else {
            return Response::Error {
                message: format!("task '{}' not found", name),
            };
        };
        let dirs: Vec<PathBuf> = task.members.iter().map(|m| m.dir.clone()).collect();
        if !state.dropping.insert(name.to_string()) {
            return Response::Error {
                message: format!("task '{}' is already being dropped", name),
            };
        }
        dirs
    };

    let removal = stop_and_remove(state, name, dirs, force, delete_branch).await;

    let mut state = state.lock().await;
    state.dropping.remove(name);
    let removal = match removal {
        Ok(removal) => removal,
        Err(response) => return response,
    };
    if let Err(e) = state.ledger.drop_task(name, removal.deleted) {
        return Response::Error {
            message: format!("cannot record the drop of '{name}': {e:#}; drop it again to finish"),
        };
    }
    for warning in &removal.warnings {
        warn!(task = %name, "{warning}");
    }
    info!(task = %name, "task dropped");
    state.broadcast(Event::TaskDropped { name: name.into() });
    Response::Dropped {
        warnings: removal.warnings,
    }
}

/// The work of a drop before the ledger: check the worktrees (`Err` with
/// `DropRefused` if work would be lost), stop the task's agents and wait for
/// them, check again (the agent may have written until it stopped), remove
/// the worktrees.
async fn stop_and_remove(
    state: &Arc<Mutex<DaemonState>>,
    name: &str,
    dirs: Vec<PathBuf>,
    force: bool,
    delete_branch: bool,
) -> Result<Removal, Response> {
    let check = |dirs: Vec<PathBuf>| {
        blocking(move || dirs.into_iter().map(worktrees::check).collect::<Vec<_>>())
    };
    let mut checked = check(dirs.clone()).await?;
    if !force {
        refuse_lossy_drop(&checked, delete_branch)?;
    }

    let agents = state.lock().await.take_task_agents(name);
    let stopped = !agents.is_empty();
    if stopped {
        info!(task = %name, "stopping agent of dropped task");
        blocking(move || agents.into_iter().for_each(Agent::kill_and_drop)).await?;
        if !force {
            checked = check(dirs).await?;
            refuse_lossy_drop(&checked, delete_branch)?;
        }
    }

    blocking(move || worktrees::remove_all(&checked, delete_branch))
        .await?
        .map_err(|e| {
            let agent = if stopped {
                " (its agent was stopped)"
            } else {
                ""
            };
            Response::Error {
                message: format!("Task '{name}' kept{agent}: {e}"),
            }
        })
}

/// Refuse a drop that would lose work, or whose worktree cannot be checked.
fn refuse_lossy_drop(checked: &[Checked], delete_branch: bool) -> Result<(), Response> {
    let reports: Vec<_> = checked
        .iter()
        .filter_map(|c| c.report(delete_branch))
        .collect();
    if reports.is_empty() {
        Ok(())
    } else {
        Err(Response::DropRefused { worktrees: reports })
    }
}

/// Run blocking work (git, waiting for a process) off the async runtime.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Response> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Response::Error {
            message: format!("drop failed: {e}"),
        })
}
