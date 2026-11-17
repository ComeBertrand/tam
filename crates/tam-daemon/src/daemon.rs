use std::collections::{BTreeSet, HashMap, HashSet};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tam_proto::{
    AgentInfo, AgentState, Event, MemberHealth, MemberOutcome, MemberState, MessageInfo, NewMember,
    Request, Response, SessionChoice, TaskInfo,
};
use tam_worktree::worktree;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch, Mutex};
use tracing::{error, info, warn};

use crate::agent::Agent;
use crate::inbox::{self, Handover, Inbox, ReplyOutcome};
use crate::ledger::Ledger;
use crate::notify::{self, Notification, NotifyConfig};
use crate::provider;
use crate::worktrees::{self, Checked, Observation, Recorded, Removal};

pub struct Daemon {
    socket_path: PathBuf,
    ledger_path: PathBuf,
    reconcile_every: Duration,
}

struct DaemonState {
    agents: HashMap<String, Agent>,
    /// Task ledger — the daemon is its only writer.
    ledger: Ledger,
    /// Tasks being dropped: no agent may start in them meanwhile.
    dropping: HashSet<String>,
    /// Held while git works on members' worktrees (reconcile, repair, drop):
    /// one at a time, so a reconcile pass never reports a state a repair or a
    /// drop changed under it. Never taken while holding the state lock.
    worktrees: Arc<Mutex<()>>,
    next_id: u64,
    shutdown: bool,
    event_tx: broadcast::Sender<Event>,
    socket_path: PathBuf,
    connected_clients: Arc<AtomicUsize>,
    /// When set, the daemon will shut down after this instant if still idle.
    idle_deadline: Option<Instant>,
    /// Message delivery state (the messages are in the ledger).
    inbox: Inbox,
}

/// Default idle timeout before daemon auto-shuts down (seconds).
const IDLE_TIMEOUT_SECS: u64 = 30;

/// Capacity of the event broadcast channel. A subscriber that falls this far
/// behind misses events.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// How often the daemon checks the members' worktrees against git.
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

/// Least time between two `ScreenUpdated` events on a connection: a busy
/// agent redraws far more often than a viewer needs.
const SCREEN_UPDATE_INTERVAL: Duration = Duration::from_millis(100);

impl Daemon {
    /// A daemon serving `socket_path` and owning the ledger at `ledger_path`
    /// (see [`crate::ledger::default_path`]).
    pub fn new(socket_path: PathBuf, ledger_path: PathBuf) -> Self {
        Self {
            socket_path,
            ledger_path,
            reconcile_every: RECONCILE_INTERVAL,
        }
    }

    /// Check the members' worktrees against git every `interval` instead of
    /// [`RECONCILE_INTERVAL`] (and on start).
    pub fn reconcile_every(mut self, interval: Duration) -> Self {
        self.reconcile_every = interval;
        self
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
            worktrees: Arc::new(Mutex::new(())),
            next_id: 1,
            shutdown: false,
            event_tx,
            socket_path: self.socket_path.clone(),
            connected_clients: Arc::new(AtomicUsize::new(0)),
            idle_deadline: None,
            inbox: Inbox::default(),
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

        // Check the members' worktrees against git now, then periodically.
        let reconcile_state = state.clone();
        let reconcile_every = self.reconcile_every;
        tokio::spawn(async move {
            reconcile_loop(reconcile_state, reconcile_every).await;
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

        // Type queued messages into the idle agents without an inbox hook.
        for delivery in state.pty_deliveries() {
            tokio::spawn(type_into_pty(state_arc.clone(), delivery));
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
        self.inbox.agent_gone(id, agent.member());
        let Some(member) = agent.member() else {
            return;
        };
        match self
            .ledger
            .run_ended_with(member, exit_code, agent.response().cloned())
        {
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
    /// Not those whose hooks report their sessions: they report one once it
    /// has a turn, and a session found earlier could not be resumed.
    fn session_jobs(&self) -> Vec<SessionJob> {
        self.agents
            .values()
            .filter(|agent| !agent.reports_sessions())
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

    /// Record the session an agent's hooks showed a turn in as its member's
    /// current run's session (it changes on e.g. `/clear`).
    fn record_hook_session(&mut self, member: &str, session_id: &str) {
        match self.ledger.session_reported(member, session_id) {
            Ok(true) => {
                info!(member = %member, session = %session_id, "session reported by hook")
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

/// The agent whose screen a connection watches (`Request::WatchScreen`).
struct ScreenWatch {
    id: String,
    changes: watch::Receiver<()>,
    /// No update before this instant (throttling).
    quiet_until: Option<tokio::time::Instant>,
}

/// Next update of the screen watched on a connection: at most one per
/// `SCREEN_UPDATE_INTERVAL`, a change during the interval comes at its end.
/// `None` once the agent is gone; never resolves without a watch.
/// Cancel-safe.
async fn next_screen_update(watch: &mut Option<ScreenWatch>) -> Option<String> {
    let Some(watch) = watch else {
        return std::future::pending().await;
    };
    if let Some(until) = watch.quiet_until {
        tokio::time::sleep_until(until).await;
        watch.quiet_until = None;
    }
    watch.changes.changed().await.ok()?;
    watch.quiet_until = Some(tokio::time::Instant::now() + SCREEN_UPDATE_INTERVAL);
    Some(watch.id.clone())
}

/// Handle a single client connection.
/// Starts as newline-delimited JSON request/response.
/// If the client sends an Attach request, switches to raw byte streaming.
/// After a Subscribe request, daemon events are pushed to the client as JSON
/// lines between responses.
async fn handle_connection(stream: UnixStream, state: Arc<Mutex<DaemonState>>) -> Result<()> {
    let mut event_rx: Option<broadcast::Receiver<Event>> = None;
    let mut screen_watch: Option<ScreenWatch> = None;
    // Track connected clients for auto-shutdown
    let client_counter = state.lock().await.connected_clients.clone();
    client_counter.fetch_add(1, Ordering::Relaxed);
    let _guard = ClientGuard {
        counter: client_counter,
    };

    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    // Bytes, not a String: `read_until` keeps a partly read line when an
    // event cancels it (`read_line` would lose it).
    let mut line = Vec::new();

    loop {
        tokio::select! {
            result = buf_reader.read_until(b'\n', &mut line) => {
                if result? == 0 {
                    break; // client disconnected
                }

                let request: Request = match serde_json::from_slice(line.trim_ascii()) {
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
                                // Before the replay: it is drawn at this size.
                                agent.resize(cols, rows);
                                let (replay, output_rx) = agent.attach_output();
                                Ok((output_rx, replay, agent.pty_master(), agent.viewers()))
                            }
                            None => Err(format!("agent '{}' not found", id)),
                        }
                    };

                    match attach_resources {
                        Ok((output_rx, replay, master, viewers)) => {
                            write_response(&mut writer, &Response::Attached).await?;
                            let buffered = buf_reader.buffer().to_vec();
                            let reader = buf_reader.into_inner();
                            return handle_attach_session(
                                reader, writer, buffered, output_rx, replay, master, viewers,
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
                } else if let Request::WatchScreen { id } = request {
                    let response = match id {
                        None => {
                            screen_watch = None;
                            Response::Ok
                        }
                        Some(id) => match state.lock().await.agents.get(&id) {
                            Some(agent) => {
                                screen_watch = Some(ScreenWatch {
                                    changes: agent.screen_changes(),
                                    id,
                                    quiet_until: None,
                                });
                                Response::Ok
                            }
                            None => Response::Error {
                                message: format!("agent '{}' not found", id),
                            },
                        },
                    };
                    write_response(&mut writer, &response).await?;
                } else if let Request::WaitInbox { agent_id } = request {
                    // Takes over the connection until messages are handed over.
                    return handle_wait_inbox(buf_reader, writer, &state, &agent_id).await;
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
            update = next_screen_update(&mut screen_watch) => {
                match update {
                    Some(id) => write_event(&mut writer, &Event::ScreenUpdated { id }).await?,
                    // The agent is gone.
                    None => screen_watch = None,
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
    replay: Vec<u8>,
    master: Arc<OwnedFd>,
    viewers: Arc<AtomicUsize>,
) -> Result<()> {
    viewers.fetch_add(1, Ordering::Relaxed);
    // Bring the client's terminal to the agent's: history, screen, modes.
    writer.write_all(&replay).await?;

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
        } => {
            handle_inbox_hook(state, &agent_id, &event, payload.as_ref()).await;
            handle_hook_event(state, &agent_id, &event, payload.as_ref()).await
        }
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
        Request::ResolveMember { target, dir, from } => {
            let state = state.lock().await;
            let caller = state.sender_member(from.as_deref());
            let found = match (target, dir) {
                (Some(target), _) => state.ledger.resolve_from(caller.as_deref(), &target),
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
        Request::RunAgent {
            member,
            provider,
            default_provider,
            session,
            args,
            prompt,
        } => {
            handle_run_agent(
                state,
                RunSpec {
                    member,
                    provider,
                    default_provider,
                    session,
                    args,
                    prompt,
                },
            )
            .await
        }
        Request::Reconcile => {
            reconcile(state).await;
            Response::Tasks {
                tasks: state.lock().await.ledger.tasks(),
            }
        }
        Request::RepairMember { member } => handle_repair_member(state, &member).await,
        Request::AdoptWorktree { dir, name, join } => {
            handle_adopt_worktree(state, dir, name, join).await
        }
        Request::Screen { id, from } => match state.lock().await.agents.get(&id) {
            Some(agent) => Response::Screen {
                screen: agent.screen(from),
            },
            None => Response::Error {
                message: format!("agent '{}' not found", id),
            },
        },
        Request::WatchScreen { .. } => Response::Error {
            message: "watch_screen should be handled by connection handler".into(),
        },
        Request::SendMessage { to, text, from } => {
            handle_send_message(state, &to, &text, from.as_deref()).await
        }
        Request::WaitInbox { .. } => Response::Error {
            message: "wait_inbox should be handled by connection handler".into(),
        },
        Request::WaitReply { id, timeout_secs } => {
            handle_wait_reply(state, &id, timeout_secs).await
        }
        Request::ListMessages { task } => {
            let state = state.lock().await;
            match task {
                Some(task) if state.ledger.task(&task).is_none() => Response::Error {
                    message: format!("task '{}' not found", task),
                },
                task => Response::Messages {
                    messages: state.ledger.messages(task.as_deref()),
                },
            }
        }
        Request::GetOutcomes { members } => handle_get_outcomes(state, &members).await,
        Request::WaitAgents {
            members,
            until,
            timeout_secs,
        } => handle_wait_agents(state, &members, until, timeout_secs).await,
        Request::CreateMembers {
            task,
            members,
            source,
            join,
            env,
        } => handle_create_members(state, &task, members, source, join, env).await,
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

struct RunSpec {
    member: String,
    provider: Option<String>,
    default_provider: String,
    session: SessionChoice,
    args: Vec<String>,
    prompt: Option<String>,
}

/// Start a member's agent: the requested agent, else the member's last one,
/// in the member's current session for that agent unless told otherwise.
async fn handle_run_agent(state: &Arc<Mutex<DaemonState>>, spec: RunSpec) -> Response {
    let RunSpec {
        member,
        provider,
        default_provider,
        session,
        args,
        prompt,
    } = spec;

    let (label, provider, current) = {
        let state = state.lock().await;
        let Some((task, info)) = state.ledger.member(&member) else {
            return Response::Error {
                message: format!("member '{}' not found", member),
            };
        };
        let label = task.member_label(&info);
        // Before looking for the session: it would not be resumed anyway.
        if state.agents.contains_key(&member) {
            return Response::Error {
                message: format!("'{}' already has a running agent", label),
            };
        }
        let provider = provider
            .or_else(|| state.ledger.last_provider(&member))
            .unwrap_or(default_provider);
        let current = state.ledger.current_session(&member, &provider);
        (label, provider, current)
    };

    let agent = provider::resolve(&provider);
    let resume_session = match session {
        SessionChoice::New => None,
        SessionChoice::Id(id) => {
            if !agent.can_resume() {
                return Response::Error {
                    message: format!("agent '{}' cannot resume a session", provider),
                };
            }
            Some(id)
        }
        // Agents that report no sessions have no current one: tam would
        // resume the one it started them in, not the one they ended in.
        SessionChoice::Current => match current {
            Some(id) if agent.reports_sessions() => {
                // Outside the lock: it reads the provider's files.
                if !agent.session_exists(&id) {
                    return Response::Error {
                        message: format!(
                            "the {} session of '{}' ({}) can no longer be resumed: start a \
                             new one (R in the TUI, tam run --new-session)",
                            provider, label, id
                        ),
                    };
                }
                Some(id)
            }
            _ => None,
        },
    };

    // The spawn checks the member again (e.g. no agent started meanwhile).
    let response = handle_spawn(
        state,
        SpawnSpec {
            provider,
            dir: PathBuf::new(),
            id: None,
            args,
            resume_session: resume_session.clone(),
            prompt,
            member: Some(member),
        },
    )
    .await;
    match response {
        Response::Spawned { id } => Response::Running {
            id,
            resumed: resume_session,
        },
        other => other,
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
        Some(agent) => Response::Scrollback { data: agent.text() },
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
        .zip(agent.resumable_session())
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
    // Whenever a session starts (resumed, cleared, compacted too): what its
    // task is now, and its peers.
    if event == "SessionStart" {
        let note = state
            .agents
            .get(agent_id)
            .and_then(Agent::member)
            .and_then(|member| crate::peers::session_note(&state.ledger, member));
        if let Some(context) = note {
            return Response::HookContext { context };
        }
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
    // Created with its worktrees' health known (git runs before the lock).
    let observed = observe(members.iter().map(Recorded::from).collect()).await;
    let mut state = state.lock().await;
    match state.ledger.create_task(name, members) {
        Ok(task) => {
            let task = state.observe_new(task, observed);
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

    let worktrees = state.lock().await.worktrees.clone();
    let held = worktrees.lock().await;
    let removal = stop_and_remove(state, name, dirs, force, delete_branch).await;
    drop(held);

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

// ---------------------------------------------------------------------------
// Reconciliation with git, and the actions on members' worktrees
// ---------------------------------------------------------------------------

/// Check the members' worktrees against git on start, then every `every`.
async fn reconcile_loop(state: Arc<Mutex<DaemonState>>, every: Duration) {
    let mut interval = tokio::time::interval(every);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if state.lock().await.shutdown {
            break;
        }
        reconcile(&state).await;
    }
}

/// Check every member's worktree against git and record its health,
/// pushing `TaskUpdated` for each task where it changed. Observes only:
/// nothing is created, removed, or written to the ledger file.
async fn reconcile(state: &Arc<Mutex<DaemonState>>) {
    let worktrees = state.lock().await.worktrees.clone();
    let _held = worktrees.lock().await;
    let (ids, recorded): (Vec<String>, Vec<Recorded>) =
        state.lock().await.recorded_worktrees().into_iter().unzip();
    let observed = observe(recorded).await;
    state
        .lock()
        .await
        .record_health(ids.into_iter().zip(observed));
}

/// [`worktrees::observe`], off the async runtime.
async fn observe(recorded: Vec<Recorded>) -> Vec<Observation> {
    run_git(move || Ok(worktrees::observe(&recorded)))
        .await
        .unwrap_or_else(|e| {
            error!("cannot check worktrees: {e:#}");
            Vec::new()
        })
}

/// Run git work off the async runtime.
async fn run_git<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow::anyhow!("git work failed: {e}"))?
}

impl From<&NewMember> for Recorded {
    fn from(m: &NewMember) -> Self {
        Recorded {
            dir: m.dir.clone(),
            branch: m.branch.clone(),
            repo: Some(m.repo_root.clone()),
        }
    }
}

impl DaemonState {
    /// The worktrees of the members of tasks not being dropped, with their
    /// member ids.
    fn recorded_worktrees(&self) -> Vec<(String, Recorded)> {
        self.ledger
            .tasks()
            .into_iter()
            .filter(|t| !self.dropping.contains(&t.name))
            .flat_map(|t| t.members)
            .map(|m| {
                let recorded = Recorded {
                    repo: self.ledger.repo_root(&m.id),
                    dir: m.dir,
                    branch: m.branch,
                };
                (m.id, recorded)
            })
            .collect()
    }

    /// Record observed health, pushing `TaskUpdated` for each task where it
    /// changed. Members dropped, or being dropped, meanwhile are skipped.
    fn record_health(&mut self, observed: impl IntoIterator<Item = (String, Observation)>) {
        let mut changed = BTreeSet::new();
        for (member, observation) in observed {
            let Some(task) = self.ledger.member_task(&member) else {
                continue;
            };
            if self.dropping.contains(task) {
                continue;
            }
            let was_unhealthy = self
                .ledger
                .health(&member)
                .is_some_and(MemberHealth::is_unhealthy);
            let health = observation.health.clone();
            if let Some(task) = self
                .ledger
                .observe(&member, observation.health, observation.repo)
            {
                if health.is_unhealthy() {
                    warn!(task = %task, member = %member, health = ?health, "member worktree unhealthy");
                } else if was_unhealthy {
                    info!(task = %task, member = %member, "member worktree back");
                }
                changed.insert(task);
            }
        }
        for name in changed {
            if let Some(task) = self.ledger.task(&name) {
                self.broadcast(Event::TaskUpdated { task });
            }
        }
    }

    /// Record the observed health of a task's new members (`observed` in
    /// member order) and return the task with it. Pushes nothing: the caller
    /// announces the task.
    fn observe_new(&mut self, task: TaskInfo, observed: Vec<Observation>) -> TaskInfo {
        for (member, o) in task.members.iter().zip(observed) {
            self.ledger.observe(&member.id, o.health, o.repo);
        }
        self.ledger.task(&task.name).unwrap_or(task)
    }
}

/// Recreate a gone member's worktree on its branch (`Request::RepairMember`).
/// Holds the worktree lock, so no reconcile pass or drop runs meanwhile;
/// [`worktree::recreate`] checks again right before acting.
async fn handle_repair_member(state: &Arc<Mutex<DaemonState>>, target: &str) -> Response {
    let error = |message: String| Response::Error { message };
    let worktrees = state.lock().await.worktrees.clone();
    let _held = worktrees.lock().await;

    let (label, member, repo) = {
        let state = state.lock().await;
        let (task, member) = match state.ledger.resolve(target) {
            Ok(found) => found,
            Err(e) => return error(format!("{e:#}")),
        };
        let label = task.member_label(&member);
        if state.dropping.contains(&task.name) {
            return error(format!("task '{}' is being dropped", task.name));
        }
        if state.agents.contains_key(&member.id) {
            return error(format!("'{label}' has a running agent: stop it first"));
        }
        let Some(repo) = state.ledger.repo_root(&member.id) else {
            return error(format!(
                "cannot recreate the worktree of '{label}': its repository is unknown \
                 (a task from an older tam)"
            ));
        };
        (label, member, repo)
    };

    let (dir, branch, at) = (member.dir.clone(), member.branch.clone(), repo.clone());
    if let Err(e) = run_git(move || worktree::recreate(&at, &dir, &branch)).await {
        return error(format!("cannot recreate the worktree of '{label}': {e:#}"));
    }
    info!(member = %member.id, dir = %member.dir.display(), "worktree recreated");

    let observed = observe(vec![Recorded {
        dir: member.dir.clone(),
        branch: member.branch.clone(),
        repo: Some(repo),
    }])
    .await;
    let mut state = state.lock().await;
    state.record_health(observed.into_iter().map(|o| (member.id.clone(), o)));
    match state.ledger.member(&member.id) {
        Some((task, member)) => Response::Member {
            runs: state.ledger.member_runs(&member.id),
            task,
            member: Box::new(member),
        },
        None => error(format!("'{label}' was dropped meanwhile")),
    }
}

/// Turn the existing worktree containing `dir` into a member: of a new task
/// (`name`, default its branch) or of the task `join`.
async fn handle_adopt_worktree(
    state: &Arc<Mutex<DaemonState>>,
    dir: PathBuf,
    name: Option<String>,
    join: Option<String>,
) -> Response {
    let error = |message: String| Response::Error { message };
    if name.is_some() && join.is_some() {
        return error("adopt into a new task (name) or an existing one (join), not both".into());
    }
    let member = match run_git(move || worktrees::adoptable(&dir)).await {
        Ok(member) => member,
        Err(e) => return error(format!("cannot adopt: {e:#}")),
    };
    let observed = observe(vec![Recorded::from(&member)]).await;
    // Only a worktree its repository lists (e.g. not one of a bare
    // repository, whose root tam cannot tell).
    let why_not = match observed.first().map(|o| &o.health) {
        Some(MemberHealth::Ok { .. }) => None,
        Some(MemberHealth::Broken { problem }) => Some(problem.clone()),
        Some(MemberHealth::Gone { .. }) => Some("it is gone".into()),
        None => Some("it could not be checked".into()),
    };
    if let Some(why) = why_not {
        return error(format!("cannot adopt {}: {why}", member.dir.display()));
    }

    let mut state = state.lock().await;
    // Recorded directories may go through symlinks: compare them resolved.
    let tasks = state.ledger.tasks();
    let existing = tasks.iter().find(|t| {
        t.members
            .iter()
            .any(|m| worktree::same_path(&m.dir, &member.dir))
    });
    if let Some(task) = existing {
        return error(format!(
            "{} is already a member of task '{}'",
            member.dir.display(),
            task.name
        ));
    }
    let added = match &join {
        Some(task) if state.dropping.contains(task) => {
            return error(format!("task '{task}' is being dropped"));
        }
        Some(task) => state.ledger.add_member(task, &member),
        None => {
            let name = name.unwrap_or_else(|| member.branch.clone());
            state
                .ledger
                .create_task(&name, std::slice::from_ref(&member))
        }
    };
    let task = match added {
        Ok(task) => task,
        Err(e) => return error(format!("{e:#}")),
    };
    // The adopted worktree joined last.
    let id = task
        .members
        .last()
        .map(|m| m.id.clone())
        .unwrap_or_default();
    for o in observed {
        state.ledger.observe(&id, o.health, o.repo);
    }
    let task = state.ledger.task(&task.name).unwrap_or(task);
    info!(task = %task.name, member = %id, dir = %member.dir.display(), "worktree adopted");
    if join.is_some() {
        state.broadcast(Event::TaskUpdated { task: task.clone() });
    } else {
        state.broadcast(Event::TaskCreated { task: task.clone() });
    }
    let runs = state.ledger.runs(&task.name);
    Response::Task { task, runs }
}

/// Create worktrees as the members of a new task, or with `join` as new
/// members of an existing one (`Request::CreateMembers`): check them all,
/// create them, record them. All or nothing: the worktrees are undone when
/// one fails or the task cannot be recorded.
///
/// Holds the worktree lock throughout (git runs outside the state lock), so
/// no reconcile pass, repair or drop runs meanwhile. A drop may still claim
/// the task while it waits for that lock: checked again before recording.
///
/// Git runs in the client's environment `env` (the daemon's when empty), as
/// it would have run in the client.
async fn handle_create_members(
    state: &Arc<Mutex<DaemonState>>,
    task: &str,
    members: Vec<NewMember>,
    source: Option<String>,
    join: bool,
    env: std::collections::BTreeMap<String, String>,
) -> Response {
    let error = |why: String| Response::Error {
        message: match join {
            true => format!("cannot add to task '{task}': {why}"),
            false => format!("cannot create task '{task}': {why}"),
        },
    };
    if members.is_empty() {
        return error("no repository given".into());
    }
    let worktrees = state.lock().await.worktrees.clone();
    let _held = worktrees.lock().await;
    if let Err(why) = state.lock().await.can_create_members(task, join) {
        return error(why);
    }

    let env = (!env.is_empty()).then_some(env);
    let (check, from, in_env) = (members.clone(), source.clone(), env.clone());
    let checked = run_git(move || {
        tam_worktree::git::with_env(in_env, || worktrees::check_new(&check, from.as_deref()))
    });
    if let Err(e) = checked.await {
        return error(format!("{e:#}\nNothing was created."));
    }
    let (create, in_env) = (members.clone(), env.clone());
    let created = run_git(move || {
        Ok(tam_worktree::git::with_env(in_env, || {
            worktrees::create_all(&create, source.as_deref())
        }))
    });
    let created = match created.await {
        Ok(Ok(created)) => created,
        Ok(Err(report)) => return error(report),
        Err(e) => return error(format!("{e:#} (worktrees may be left behind)")),
    };
    let observed = observe(members.iter().map(Recorded::from).collect()).await;

    let mut state = state.lock().await;
    let recorded = match state.can_create_members(task, join) {
        Err(why) => Err(anyhow::anyhow!(why)),
        Ok(()) if join => state.ledger.add_members(task, &members),
        Ok(()) => state.ledger.create_task(task, &members),
    };
    let info = match recorded {
        Ok(info) => info,
        Err(e) => {
            drop(state);
            let undone = run_git(move || {
                Ok(tam_worktree::git::with_env(env, || {
                    worktrees::undo_all(created)
                }))
            })
            .await
            .unwrap_or_else(|e| format!("{e:#} (worktrees may be left behind)"));
            return error(format!("{e:#}\n{undone}"));
        }
    };

    // The new members joined last, in order.
    let ids: Vec<String> = info.members[info.members.len() - members.len()..]
        .iter()
        .map(|m| m.id.clone())
        .collect();
    for (id, o) in ids.iter().zip(observed) {
        state.ledger.observe(id, o.health, o.repo);
    }
    let info = state.ledger.task(task).unwrap_or(info);
    for (id, m) in ids.iter().zip(&members) {
        info!(task = %task, member = %id, dir = %m.dir.display(), "worktree created");
    }
    if join {
        state.broadcast(Event::TaskUpdated { task: info.clone() });
    } else {
        info!(task = %task, "task created");
        state.broadcast(Event::TaskCreated { task: info.clone() });
    }
    Response::Task {
        runs: state.ledger.runs(task),
        task: info,
    }
}

impl DaemonState {
    /// Why members cannot be created for `task` (see `CreateMembers`): a new
    /// task needs a free name, `join` a task that exists and is not being
    /// dropped.
    fn can_create_members(&self, task: &str, join: bool) -> Result<(), String> {
        if task.trim().is_empty() {
            return Err("a task needs a name".into());
        }
        match (self.ledger.task(task).is_some(), join) {
            (true, false) => Err("it already exists (add repositories to it with --join)".into()),
            (false, true) => Err("it does not exist".into()),
            (true, true) if self.dropping.contains(task) => Err("it is being dropped".into()),
            _ => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Messages (see `inbox.rs`)
// ---------------------------------------------------------------------------

/// Queued messages to type into an agent's PTY, outside the state lock.
struct PtyDelivery {
    member: String,
    ids: Vec<String>,
    master: Arc<OwnedFd>,
    input: Vec<u8>,
}

impl DaemonState {
    /// The member a caller's agent id stands for: the member its agent runs
    /// for, or the member with that id. `None`: the user.
    fn sender_member(&self, agent_id: Option<&str>) -> Option<String> {
        let id = agent_id?;
        match self.agents.get(id) {
            Some(agent) => agent.member().map(str::to_string),
            None => self.ledger.member(id).map(|(_, m)| m.id),
        }
    }

    /// A member's messages waiting to be handed over (queued, not in flight).
    fn queued_messages(&self, member: &str) -> Vec<MessageInfo> {
        let flying = self.inbox.in_flight(member);
        self.ledger
            .pending_messages(member)
            .into_iter()
            .filter(|m| !flying.contains(&m.id))
            .collect()
    }

    /// Hand a member's queued messages to its agent's inbox hook, if one
    /// waits. (A member's agent runs under the member id.)
    fn hand_over_messages(&mut self, member: &str) {
        if !self.inbox.has_waiter(member) {
            return;
        }
        let queued = self.queued_messages(member);
        if queued.is_empty() {
            return;
        }
        let (text, ids) = inbox::batch(&queued);
        if self.inbox.hand_over(member, member, text, ids.clone()) {
            info!(member = %member, ids = ?ids, "messages handed to the inbox hook");
        }
    }

    /// Messages to type into the idle agents that have no inbox hook: the
    /// oldest queued one each.
    fn pty_deliveries(&mut self) -> Vec<PtyDelivery> {
        let ready: Vec<String> = self
            .agents
            .iter()
            .filter(|(id, agent)| {
                agent.member() == Some(id.as_str())
                    && !agent.inbox_hook()
                    && !self.inbox.has_waiter(id)
                    && matches!(agent.current_state(), AgentState::Idle | AgentState::Input)
            })
            .map(|(id, _)| id.clone())
            .collect();
        let mut deliveries = Vec::new();
        for member in ready {
            let Some(message) = self.queued_messages(&member).into_iter().next() else {
                continue;
            };
            let agent = &self.agents[&member];
            let input = agent.pty_input(&inbox::pty_text(std::slice::from_ref(&message)));
            let ids = vec![message.id];
            self.inbox.take_off(&member, &ids);
            deliveries.push(PtyDelivery {
                master: agent.pty_master(),
                member,
                ids,
                input,
            });
        }
        deliveries
    }

    /// A message reached its recipient's agent: record it and tell clients.
    /// `in_turn`: the agent reports the end of the turn that got it (whose
    /// last response is the reply).
    fn record_delivery(&mut self, id: &str, in_turn: bool) {
        match self.ledger.message_delivered(id) {
            Ok(Some(message)) => {
                info!(id = %id, to = %message.to, "message delivered");
                self.inbox.record_hop(&message.to, message.hop);
                if in_turn {
                    self.inbox.delivered_this_turn(&message.to, id);
                }
                self.broadcast(Event::MessageDelivered { message });
            }
            Ok(None) => {}
            Err(e) => error!(id = %id, "failed to record a message delivery: {e:#}"),
        }
    }

    /// Why a message waits; `None` once it reached its agent.
    fn pending_note(&self, message: &MessageInfo) -> Option<String> {
        if message.delivered_at.is_some() {
            return None;
        }
        let to = &message.to_label;
        Some(match self.agents.get(&message.to) {
            None => format!("no agent is running in '{to}': it gets the message when one starts"),
            Some(_) if self.inbox.in_flight(&message.to).contains(&message.id) => {
                format!("handed to the agent of '{to}', which reads it at its next step")
            }
            Some(agent) if agent.inbox_hook() => format!(
                "waiting for the inbox hook of '{to}' (armed when its session starts and after each turn)"
            ),
            Some(_) => format!("typed into the agent of '{to}' once it is idle"),
        })
    }
}

async fn handle_send_message(
    state: &Arc<Mutex<DaemonState>>,
    to: &str,
    text: &str,
    from: Option<&str>,
) -> Response {
    let mut state = state.lock().await;
    match send_message(&mut state, to, text, from) {
        Ok(message) => {
            let pending = state.pending_note(&message);
            Response::Sent {
                message: Box::new(message),
                pending,
            }
        }
        Err(message) => Response::Error { message },
    }
}

/// Check, record and route a message.
fn send_message(
    state: &mut DaemonState,
    to: &str,
    text: &str,
    from: Option<&str>,
) -> Result<MessageInfo, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("empty message".into());
    }
    let len = text.chars().count();
    if len > inbox::MAX_TEXT {
        return Err(format!(
            "message too long: {len} characters (at most {})",
            inbox::MAX_TEXT
        ));
    }
    let sender = state.sender_member(from);
    let (task, recipient) = state
        .ledger
        .resolve_from(sender.as_deref(), to)
        .map_err(|e| format!("{e:#}"))?;
    if state.dropping.contains(&task.name) {
        return Err(format!("task '{}' is being dropped", task.name));
    }
    let (hop, reply_to) = match sender.as_deref() {
        None => (0, None),
        Some(sender) if sender == recipient.id => {
            return Err("cannot send a message to yourself".into())
        }
        Some(sender) => {
            let hop = state.inbox.next_hop(sender);
            if hop > inbox::MAX_HOPS {
                let label = state
                    .ledger
                    .member_label(sender)
                    .unwrap_or_else(|| sender.to_string());
                return Err(format!(
                    "loop guard: this would be message {hop} of an exchange between agents (at most {}); it starts over once the user prompts '{label}'",
                    inbox::MAX_HOPS
                ));
            }
            state.inbox.check_rate(sender)?;
            let since = crate::ledger::now().saturating_sub(inbox::REPLY_WINDOW_SECS);
            let answered = state
                .ledger
                .unanswered_message(&recipient.id, sender, since);
            (hop, answered)
        }
    };
    let message = state
        .ledger
        .send_message(
            sender.as_deref(),
            &recipient.id,
            text,
            hop,
            reply_to.as_deref(),
        )
        .map_err(|e| format!("cannot record the message: {e:#}"))?;
    info!(id = %message.id, from = %message.from_label, to = %message.to_label, hop, "message sent");
    state.broadcast(Event::MessageSent {
        message: message.clone(),
    });
    if let Some(answered) = &message.reply_to {
        let outcome = ReplyOutcome::Reply {
            message: Some(message.clone()),
            text: message.text.clone(),
        };
        if state.inbox.replied(answered, outcome) > 0 {
            // A `tam msg --wait` of the recipient returned it: delivered,
            // in the recipient's current turn (whose response answers it in
            // turn), and not shown to its session a second time.
            state.record_delivery(&message.id, true);
            return Ok(state.ledger.message(&message.id).unwrap_or(message));
        }
    }
    state.hand_over_messages(&recipient.id);
    Ok(message)
}

async fn handle_wait_reply(
    state: &Arc<Mutex<DaemonState>>,
    id: &str,
    timeout_secs: u64,
) -> Response {
    let rx = {
        let mut state = state.lock().await;
        if state.ledger.message(id).is_none() {
            return Response::Error {
                message: format!("message '{id}' not found"),
            };
        }
        match state.inbox.wait_reply(id) {
            Ok(outcome) => return reply_response(outcome),
            Err(rx) => rx,
        }
    };
    match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await {
        Ok(Ok(outcome)) => reply_response(outcome),
        _ => {
            state.lock().await.inbox.prune_reply_waiters(id);
            Response::Error {
                message: format!("no reply to {id} within {timeout_secs}s"),
            }
        }
    }
}

fn reply_response(outcome: ReplyOutcome) -> Response {
    match outcome {
        ReplyOutcome::Reply { message, text } => Response::Reply {
            message: message.map(Box::new),
            text,
        },
        ReplyOutcome::Failed(message) => Response::Error { message },
    }
}

/// What the hooks of a member's agent tell about its messages: the prompt
/// that shows them to the model (delivered), a prompt the user typed (its
/// agent-to-agent exchanges start over), the end of a turn (the reply to
/// the messages it got).
async fn handle_inbox_hook(
    state: &Arc<Mutex<DaemonState>>,
    agent_id: &str,
    event: &str,
    payload: Option<&serde_json::Value>,
) {
    let mut state = state.lock().await;
    let Some(member) = state
        .agents
        .get(agent_id)
        .and_then(Agent::member)
        .map(str::to_string)
    else {
        return;
    };
    let field = |key: &str| {
        payload
            .and_then(|p| p.get(key))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
    };
    match event {
        "UserPromptSubmit" => {
            let prompt = field("prompt");
            let ids = inbox::ids_in(prompt, state.inbox.in_flight(&member));
            if !ids.is_empty() {
                state.inbox.land(&member, &ids);
                for id in &ids {
                    state.record_delivery(id, true);
                }
            } else if inbox::is_user_prompt(prompt) {
                state.inbox.reset_chain(&member);
            }
        }
        "Stop" => {
            let text = field("last_assistant_message").to_string();
            for id in state.inbox.turn_ended(&member) {
                state.inbox.replied(
                    &id,
                    ReplyOutcome::Reply {
                        message: None,
                        text: text.clone(),
                    },
                );
            }
        }
        "StopFailure" => {
            let error = match field("error") {
                "" => "unknown error",
                e => e,
            }
            .to_string();
            let label = state
                .ledger
                .member_label(&member)
                .unwrap_or_else(|| member.clone());
            for id in state.inbox.turn_ended(&member) {
                state.inbox.replied(
                    &id,
                    ReplyOutcome::Failed(format!(
                        "the turn of '{label}' that got {id} failed: {error}"
                    )),
                );
            }
        }
        _ => {}
    }
}

/// An agent's inbox hook waits for its messages: hold the connection until
/// messages are handed over (`Inbox`), a newer hook or the agent's exit
/// releases it (`Ok`), or the hook goes away (its messages stay queued).
async fn handle_wait_inbox(
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    state: &Arc<Mutex<DaemonState>>,
    agent_id: &str,
) -> Result<()> {
    let registered = {
        let mut state = state.lock().await;
        match state
            .agents
            .get(agent_id)
            .map(|a| a.member().map(str::to_string))
        {
            None => Err(format!("agent '{}' not found", agent_id)),
            Some(None) => Err(format!("agent '{}' runs for no task member", agent_id)),
            Some(Some(member)) => {
                let (token, rx) = state.inbox.register_waiter(agent_id);
                state.hand_over_messages(&member);
                Ok((member, token, rx))
            }
        }
    };
    let (member, token, mut rx) = match registered {
        Ok(registered) => registered,
        Err(message) => return write_response(&mut writer, &Response::Error { message }).await,
    };

    let mut buf = [0u8; 256];
    let handover = tokio::select! {
        handover = &mut rx => handover.ok(),
        _ = reader.read(&mut buf) => {
            // The hook went away: it takes nothing.
            let mut state = state.lock().await;
            state.inbox.unregister_waiter(agent_id, token);
            if let Ok(Handover::Messages { ids, .. }) = rx.try_recv() {
                state.inbox.land(&member, &ids);
            }
            return Ok(());
        }
    };
    match handover {
        Some(Handover::Messages { text, ids }) => {
            let response = Response::Inbox {
                text,
                ids: ids.clone(),
            };
            if write_response(&mut writer, &response).await.is_err() {
                state.lock().await.inbox.land(&member, &ids);
            }
        }
        Some(Handover::Released) | None => {
            let _ = write_response(&mut writer, &Response::Ok).await;
        }
    }
    Ok(())
}

/// Type messages into an agent's PTY, then press Enter, and record them as
/// delivered.
async fn type_into_pty(state: Arc<Mutex<DaemonState>>, delivery: PtyDelivery) {
    let PtyDelivery {
        member,
        ids,
        master,
        input,
    } = delivery;
    let fd = master.clone();
    let typed = tokio::task::spawn_blocking(move || write_pty(&fd, &input))
        .await
        .unwrap_or(false);
    if typed {
        tokio::time::sleep(inbox::SUBMIT_DELAY).await;
        let _ = tokio::task::spawn_blocking(move || write_pty(&master, b"\r")).await;
    }
    let mut state = state.lock().await;
    state.inbox.land(&member, &ids);
    if typed {
        for id in &ids {
            state.record_delivery(id, false);
        }
    } else {
        warn!(member = %member, "could not type a message into the agent's terminal");
    }
}

/// Write all of `data` to a PTY master.
fn write_pty(fd: &OwnedFd, mut data: &[u8]) -> bool {
    while !data.is_empty() {
        match nix::unistd::write(fd, data) {
            Ok(n) => data = &data[n..],
            Err(nix::errno::Errno::EINTR) | Err(nix::errno::Errno::EAGAIN) => continue,
            Err(_) => return false,
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Outcomes: what members' agents are doing (`tam wait`, `tam result`)
// ---------------------------------------------------------------------------

/// How often a wait looks at the agents again without an event: a safety
/// net, every change comes with one (hooks, the state monitor, exits).
const WAIT_RECHECK: Duration = Duration::from_secs(1);

impl DaemonState {
    /// The ids of the members `targets` name (see `Ledger::resolve`).
    fn resolve_members(&self, targets: &[String]) -> Result<Vec<String>, String> {
        if targets.is_empty() {
            return Err("no task member given".into());
        }
        targets
            .iter()
            .map(|t| {
                self.ledger
                    .resolve(t)
                    .map(|(_, m)| m.id)
                    .map_err(|e| format!("{e:#}"))
            })
            .collect()
    }

    /// What a member's agent is doing, and its last response; `None` once
    /// the member is gone. Reap exited agents first: until then they look
    /// alive.
    fn outcome(&self, member: &str) -> Option<MemberOutcome> {
        let (task, info) = self.ledger.member(member)?;
        let label = task.member_label(&info);
        let last_run = self.ledger.member_runs(member).pop();
        let outcome = match self.agents.get(member) {
            Some(agent) => {
                let running = agent.info(member);
                let detail = *running.detail;
                MemberOutcome {
                    member: member.to_string(),
                    task: task.name,
                    label,
                    state: running.state.into(),
                    provider: Some(running.provider),
                    session_id: detail
                        .session_id
                        .or_else(|| last_run.and_then(|r| r.session_id)),
                    exit_code: None,
                    pending: detail.pending,
                    error: detail.error,
                    response: agent.response().cloned(),
                }
            }
            None => MemberOutcome {
                member: member.to_string(),
                task: task.name,
                label,
                state: MemberState::Exit,
                provider: last_run.as_ref().map(|r| r.provider.clone()),
                session_id: last_run.as_ref().and_then(|r| r.session_id.clone()),
                exit_code: last_run.and_then(|r| r.exit_code),
                pending: None,
                error: None,
                response: self.ledger.last_response(member),
            },
        };
        Some(outcome)
    }
}

async fn handle_get_outcomes(state: &Arc<Mutex<DaemonState>>, targets: &[String]) -> Response {
    let mut state = state.lock().await;
    state.reap_exited();
    match state.resolve_members(targets) {
        Ok(ids) => Response::Outcomes {
            members: ids.iter().filter_map(|id| state.outcome(id)).collect(),
            timed_out: false,
        },
        Err(message) => Response::Error { message },
    }
}

/// Whether a wait for `until` is over: every agent got there, or one exited
/// or failed its turn without `until` taking that (it would not move on
/// without someone acting).
fn wait_over(outcomes: &[MemberOutcome], until: &[MemberState]) -> bool {
    let stuck = |o: &MemberOutcome| {
        matches!(o.state, MemberState::Exit | MemberState::Error) && !o.state.satisfies(until)
    };
    outcomes.iter().any(stuck) || outcomes.iter().all(|o| o.state.satisfies(until))
}

/// Wait until the members' agents settle (`Request::WaitAgents`).
async fn handle_wait_agents(
    state: &Arc<Mutex<DaemonState>>,
    targets: &[String],
    until: Vec<MemberState>,
    timeout_secs: Option<u64>,
) -> Response {
    let until = if until.is_empty() {
        MemberState::SETTLED.to_vec()
    } else {
        until
    };
    // Too far off to reckon with: no limit.
    let deadline = timeout_secs
        .and_then(|secs| tokio::time::Instant::now().checked_add(Duration::from_secs(secs)));
    // Subscribed before the first look: no change after it goes unseen.
    let (ids, mut events) = {
        let state = state.lock().await;
        match state.resolve_members(targets) {
            Ok(ids) => (ids, state.event_tx.subscribe()),
            Err(message) => return Response::Error { message },
        }
    };
    let mut outcomes: Vec<MemberOutcome> = Vec::new();
    loop {
        {
            let mut state = state.lock().await;
            state.reap_exited();
            outcomes = ids
                .iter()
                .enumerate()
                .filter_map(|(i, id)| {
                    // A member dropped meanwhile runs no agent any more.
                    state.outcome(id).or_else(|| {
                        let gone = outcomes.get(i)?.clone();
                        Some(MemberOutcome {
                            state: MemberState::Exit,
                            pending: None,
                            error: None,
                            ..gone
                        })
                    })
                })
                .collect();
        }
        if wait_over(&outcomes, &until) {
            return Response::Outcomes {
                members: outcomes,
                timed_out: false,
            };
        }
        // The deadline expression is evaluated even when the branch is
        // disabled, hence the fallback.
        let at = deadline.unwrap_or_else(tokio::time::Instant::now);
        tokio::select! {
            event = events.recv() => {
                if let Err(broadcast::error::RecvError::Closed) = event {
                    return Response::Error {
                        message: "the daemon is shutting down".into(),
                    };
                }
            }
            _ = tokio::time::sleep(WAIT_RECHECK) => {}
            _ = tokio::time::sleep_until(at), if deadline.is_some() => {
                return Response::Outcomes {
                    members: outcomes,
                    timed_out: true,
                };
            }
        }
    }
}
