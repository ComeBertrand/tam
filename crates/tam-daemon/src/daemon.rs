use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;
use tam_proto::{
    AgentInfo, AgentState, Event, MemberHealth, MemberOutcome, MemberState, MessageInfo, NewMember,
    Request, Response, SessionChoice, TaskInfo,
};
use tam_worktree::worktree;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch, Mutex};
use tracing::{error, info, warn};

use crate::activity::{self, Activity, ConnStats};
use crate::agent::Agent;
use crate::answer::{self, Delivery};
use crate::autoclean;
use crate::inbox::{self, Handover, Inbox, ReplyOutcome};
use crate::ledger::{ChatSession, Ledger, Owner};
use crate::notify::{self, Notification, NotifyConfig};
use crate::provider;
use crate::pty::{write_pty, Pty};
use crate::sessions;
use crate::shell::{self, Place, Shell};
use crate::worktrees::{self, Checked, Observation, Recorded, Removal};

pub struct Daemon {
    socket_path: PathBuf,
    ledger_path: PathBuf,
    reconcile_every: Duration,
    chat_sessions_every: Duration,
    autoclean: autoclean::Source,
    autoclean_every: Duration,
    idle_timeout: Duration,
    /// Where the activity log goes; `None`: next to the ledger, as the
    /// config says (see [`activity`]).
    activity: Option<Activity>,
    conn_stats_after: Duration,
}

struct DaemonState {
    agents: HashMap<String, Agent>,
    /// Shells, by id (`sh-xxxxxxxx`): apart from the agents, so that
    /// nothing about agents (hooks, state, notifications, runs, messages,
    /// autoclean) sees them. The PTY requests find either (`pty`).
    shells: HashMap<String, Shell>,
    /// The next shell's opening order.
    next_shell: u64,
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
    /// Why it shuts down: `idle` or `requested` (for the log).
    shutdown_reason: &'static str,
    event_tx: broadcast::Sender<Event>,
    socket_path: PathBuf,
    connected_clients: Arc<AtomicUsize>,
    /// When set, the daemon will shut down after this instant if still idle.
    idle_deadline: Option<Instant>,
    /// How long the daemon stays up with no agent, no shell and no client.
    idle_timeout: Duration,
    /// Message delivery state (the messages are in the ledger).
    inbox: Inbox,
    /// What changed in a member's task since its agent last heard (merge,
    /// split), by member id: told at its next prompt or tool call.
    peer_changes: HashMap<String, Vec<String>>,
    /// Permission requests agents' hooks wait on, for clients to answer.
    asks: answer::Asks,
    /// The activity log (see [`activity`]).
    activity: Activity,
}

/// How long the daemon stays up with no agent, no shell and no client.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Capacity of the event broadcast channel. A subscriber that falls this far
/// behind misses events.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// How often the daemon removes the activity log's old files (and when it
/// starts).
const PRUNE_INTERVAL: Duration = Duration::from_secs(3600);

/// How often the daemon checks the members' worktrees against git.
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

/// Least time between two `ScreenUpdated` events on a connection: a busy
/// agent redraws far more often than a viewer needs.
const SCREEN_UPDATE_INTERVAL: Duration = Duration::from_millis(100);

/// The same on a connection someone types on (`Request::Keys`): what they
/// type shows at once.
const TYPING_UPDATE_INTERVAL: Duration = Duration::from_millis(15);

/// How long after its last keys a connection counts as typed on.
const TYPING_FOR: Duration = Duration::from_secs(2);

impl Daemon {
    /// A daemon serving `socket_path` and owning the ledger at `ledger_path`
    /// (see [`crate::ledger::default_path`]).
    pub fn new(socket_path: PathBuf, ledger_path: PathBuf) -> Self {
        Self {
            socket_path,
            ledger_path,
            reconcile_every: RECONCILE_INTERVAL,
            chat_sessions_every: CHAT_SESSION_INTERVAL,
            autoclean: autoclean::Source::default(),
            autoclean_every: autoclean::INTERVAL,
            idle_timeout: IDLE_TIMEOUT,
            activity: None,
            conn_stats_after: CONN_STATS_AFTER,
        }
    }

    /// Log what a connection read when it closes if it lasted `after`
    /// instead of [`CONN_STATS_AFTER`].
    pub fn conn_stats_after(mut self, after: Duration) -> Self {
        self.conn_stats_after = after;
        self
    }

    /// Write the activity log to `activity` instead of next to the ledger
    /// (`log/`), and keep `[log] keep_days` days of it.
    pub fn activity(mut self, activity: Activity) -> Self {
        self.activity = Some(activity);
        self
    }

    /// Shut down after `timeout` with no agent, no shell and no client
    /// instead of [`IDLE_TIMEOUT`].
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Read what Claude's files say of the chats' sessions every
    /// `interval` instead of [`CHAT_SESSION_INTERVAL`] (and on start).
    pub fn chat_sessions_every(mut self, interval: Duration) -> Self {
        self.chat_sessions_every = interval;
        self
    }

    /// Check the members' worktrees against git every `interval` instead of
    /// [`RECONCILE_INTERVAL`] (and on start).
    pub fn reconcile_every(mut self, interval: Duration) -> Self {
        self.reconcile_every = interval;
        self
    }

    /// Look for chats past their time every `interval` instead of
    /// [`autoclean::INTERVAL`] (and on start).
    pub fn autoclean_every(mut self, interval: Duration) -> Self {
        self.autoclean_every = interval;
        self
    }

    /// Close chats after `limit` without a turn (`None`: never), whatever
    /// `[chat] autoclean` says.
    pub fn close_chats_after(mut self, limit: Option<Duration>) -> Self {
        self.autoclean = autoclean::Source::given(match limit {
            Some(limit) => autoclean::Setting::After(limit),
            None => autoclean::Setting::Off,
        });
        self
    }

    pub async fn run(&self) -> Result<()> {
        let started = Instant::now();
        let settings = activity::settings();
        let activity = self
            .activity
            .clone()
            .unwrap_or_else(|| match settings.enabled {
                true => Activity::to_dir(activity::dir_next_to(&self.ledger_path), "daemon"),
                false => Activity::off(),
            });
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

        let pruned = activity
            .dir()
            .map(|dir| activity::prune(dir, settings.keep_days));
        let (tasks, chats) = (ledger.tasks().len(), ledger.chats().len());
        let ledger_report = json!({
            "migrated": report.migrated,
            "compacted": report.compacted,
            "malformed": report.malformed.len(),
        });

        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let state = Arc::new(Mutex::new(DaemonState {
            agents: HashMap::new(),
            shells: HashMap::new(),
            next_shell: 0,
            ledger,
            dropping: HashSet::new(),
            worktrees: Arc::new(Mutex::new(())),
            next_id: 1,
            shutdown: false,
            shutdown_reason: "",
            event_tx,
            socket_path: self.socket_path.clone(),
            connected_clients: Arc::new(AtomicUsize::new(0)),
            idle_deadline: None,
            idle_timeout: self.idle_timeout,
            inbox: Inbox::default(),
            peer_changes: HashMap::new(),
            asks: answer::Asks::default(),
            activity: activity.clone(),
        }));

        // Chats past their time are closed before any client sees them.
        let mut autoclean = self.autoclean.clone();
        clean_chats(&state, &mut autoclean).await;

        // The chats' titles before the first client asks.
        refresh_chat_sessions(&state).await;

        // Create socket directory
        if let Some(parent) = self.socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Remove stale socket file
        let _ = tokio::fs::remove_file(&self.socket_path).await;

        let listener = UnixListener::bind(&self.socket_path)
            .with_context(|| format!("failed to bind socket at {:?}", self.socket_path))?;
        info!("tamd listening on {:?}", self.socket_path);
        activity.record(
            "daemon.start",
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "protocol": tam_proto::PROTOCOL_VERSION,
                "ms": activity::ms_since(started),
                "tasks": tasks,
                "chats": chats,
                "ledger": ledger_report,
                "log_files_pruned": pruned,
            }),
        );

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
        tokio::spawn(chat_session_loop(
            Arc::downgrade(&state),
            self.chat_sessions_every,
        ));

        // The activity log's old files, for a daemon up for days.
        if let Some(dir) = activity.dir() {
            let (dir, keep_days) = (dir.to_path_buf(), settings.keep_days);
            tokio::spawn(async move {
                let mut every = tokio::time::interval(PRUNE_INTERVAL);
                every.tick().await;
                loop {
                    every.tick().await;
                    let dir = dir.clone();
                    let _ =
                        tokio::task::spawn_blocking(move || activity::prune(&dir, keep_days)).await;
                }
            });
        }

        // And chats past their time, periodically.
        tokio::spawn(autoclean_loop(
            Arc::downgrade(&state),
            autoclean,
            self.autoclean_every,
        ));

        let mut connections: u64 = 0;
        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            let state = state.clone();
                            connections += 1;
                            let conn = Conn {
                                id: connections,
                                // The client's pid: its own lines carry it.
                                peer: stream.peer_cred().ok().and_then(|c| c.pid()),
                                activity: activity.clone(),
                                stats: ConnStats::default(),
                                opened: Instant::now(),
                                stats_after: self.conn_stats_after,
                            };
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, state, conn).await {
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
        let reason = state.lock().await.shutdown_reason;
        activity.record(
            "daemon.stop",
            json!({
                "reason": reason,
                "uptime_s": started.elapsed().as_secs(),
                "connections": connections,
            }),
        );

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
        let changes: Vec<(String, AgentState, AgentState, Duration)> = state
            .agents
            .iter_mut()
            .filter_map(|(id, agent)| {
                agent
                    .check_state_change()
                    .map(|(old, new, held)| (id.clone(), old, new, held))
            })
            .collect();
        for (id, old, new, held) in changes {
            info!(id = %id, old = %old, new = %new, "state changed");
            let Some(info) = state.agent_info(&id) else {
                continue;
            };
            let notified = notify_config.as_ref().is_some_and(|cfg| {
                // A member's task, a chat's label.
                let name = match state.ledger.chat(&id) {
                    Some(chat) => chat.label(),
                    None => info.task.clone().unwrap_or_else(|| id.clone()),
                };
                notify::fire_if_matching(
                    cfg,
                    &Notification {
                        id: &id,
                        task: &name,
                        old,
                        new,
                        detail: &info.detail,
                        viewers: info.viewers,
                    },
                )
            });
            let detail = &info.detail;
            state.activity.record(
                "agent.state",
                json!({
                    "id": id,
                    "task": info.task,
                    "from": old,
                    "to": new,
                    // How long it was `from`.
                    "prev_ms": activity::ms(held),
                    "viewers": info.viewers,
                    "notified": notified,
                    "tool": detail.pending.as_ref().and_then(|p| p.tool.clone()),
                    "error": detail.error.as_ref().map(|e| activity::cut(&e.kind, 100)),
                }),
            );
            let _ = state.event_tx.send(Event::StateChange { id, old, new });
        }

        // Auto-shutdown: if no agents, shells and clients, start countdown
        let has_agents = !state.agents.is_empty() || !state.shells.is_empty();
        let has_clients = state.connected_clients.load(Ordering::Relaxed) > 0;

        if has_agents || has_clients {
            state.idle_deadline = None;
        } else if state.idle_deadline.is_none() {
            let timeout = state.idle_timeout;
            info!("no agents, shells or clients, auto-shutdown in {timeout:?}");
            state.idle_deadline = Some(Instant::now() + timeout);
        } else if let Some(deadline) = state.idle_deadline {
            if Instant::now() >= deadline {
                info!("idle timeout reached, auto-shutting down");
                state.shutdown = true;
                state.shutdown_reason = "idle";
                break;
            }
        }

        // Check context usage every 10 seconds — two-phase to avoid disk IO under lock
        if context_counter >= 10 {
            context_counter = 0;

            // Phase 1: collect metadata under lock (cheap). Several chats
            // share a directory: a chat's transcript is known (its hooks',
            // else its session's), never guessed (the newest one there may
            // be another chat's).
            let jobs: Vec<_> = state
                .agents
                .iter()
                .map(|(id, agent)| {
                    let mut job = agent.context_refresh_job(id);
                    if state.ledger.is_chat(id) {
                        job.transcript = state.chat_transcript(id);
                    }
                    job
                })
                .filter(|job| job.transcript.is_some() || !state.ledger.is_chat(&job.id))
                .collect();

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

impl DaemonState {
    fn broadcast(&self, event: Event) {
        let _ = self.event_tx.send(event);
    }

    /// An agent's info, with the task of its member (none for a chat's).
    fn agent_info(&self, id: &str) -> Option<AgentInfo> {
        let agent = self.agents.get(id)?;
        let mut info = agent.info(id);
        info.task = agent
            .owner()
            .and_then(|m| self.ledger.member_task(m))
            .map(str::to_string);
        Some(info)
    }

    /// Remove agents whose process exited, telling clients and recording the
    /// end of their run; and shells whose process exited.
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
        self.reap_shells();
    }

    /// Remove the shells whose process exited, telling clients.
    fn reap_shells(&mut self) {
        let exited: Vec<String> = self
            .shells
            .iter_mut()
            .filter_map(|(id, shell)| shell.has_exited().then(|| id.clone()))
            .collect();
        for id in exited {
            info!(id = %id, "shell exited");
            if let Some(shell) = self.shells.remove(&id) {
                self.activity.record(
                    "shell.exit",
                    json!({
                        "id": id,
                        "member": shell.member(),
                        "run_ms": activity::ms(shell.pty().uptime()),
                    }),
                );
            }
            self.broadcast(Event::ShellClosed { id });
        }
    }

    /// The PTY of an agent or a shell: what the requests on terminals
    /// (attach, screen, keys, size, scrollback) act on.
    fn pty(&self, id: &str) -> Option<&Pty> {
        match self.agents.get(id) {
            Some(agent) => Some(agent.pty()),
            None => self.shells.get(id).map(Shell::pty),
        }
    }

    /// Take the shells of these task members out of the table, telling
    /// clients. The caller closes them.
    fn take_shells_of(&mut self, members: &[String]) -> Vec<Shell> {
        let ids: Vec<String> = self
            .shells
            .iter()
            .filter(|(_, s)| s.member().is_some_and(|m| members.iter().any(|id| id == m)))
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                let shell = self.shells.remove(&id)?;
                self.broadcast(Event::ShellClosed { id });
                Some(shell)
            })
            .collect()
    }

    /// An agent was removed from the table: emit `AgentExited` and record the
    /// end of its member's (or chat's) run.
    fn agent_gone(&mut self, id: &str, agent: &Agent, exit_code: i32) {
        let mut fields = agent.run_summary();
        fields["id"] = id.into();
        fields["task"] = agent
            .owner()
            .and_then(|m| self.ledger.member_task(m))
            .into();
        // -1: tam stopped it (stop, drop, close, shutdown).
        fields["exit_code"] = exit_code.into();
        self.activity.record("agent.exit", fields);
        self.broadcast(Event::AgentExited {
            id: id.to_string(),
            exit_code,
        });
        self.inbox.agent_gone(id, agent.owner());
        self.ask_ended(id, "agent_exit", None);
        self.asks.release(id);
        if let Some(member) = agent.owner() {
            self.peer_changes.remove(member);
        }
        let Some(member) = agent.owner() else {
            return;
        };
        match self
            .ledger
            .run_ended_with(member, exit_code, agent.response().cloned())
        {
            Ok(Some(owner)) => self.broadcast(owner.updated()),
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
            .filter(|(_, a)| a.owner().is_some_and(|m| members.iter().any(|id| id == m)))
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
    /// Keys came on this connection: updates come sooner until then.
    typing_until: Option<tokio::time::Instant>,
    /// The watch counts as a viewer of the agent (`WatchScreen.viewing`).
    _viewer: Option<Viewer>,
}

/// One viewer of an agent, for as long as this lives.
struct Viewer(Arc<AtomicUsize>);

impl Viewer {
    fn of(viewers: Arc<AtomicUsize>) -> Self {
        viewers.fetch_add(1, Ordering::Relaxed);
        Self(viewers)
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
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
    let now = tokio::time::Instant::now();
    let interval = match watch.typing_until {
        Some(until) if now < until => TYPING_UPDATE_INTERVAL,
        _ => SCREEN_UPDATE_INTERVAL,
    };
    watch.quiet_until = Some(now + interval);
    Some(watch.id.clone())
}

/// A client's connection, as the activity log names it: its number since
/// the daemon started and the client's pid (the client's own lines carry
/// it). Counts its quiet requests, written every `ConnStats::PERIOD` and
/// when it closes, if it lasted (a command's few reads say nothing its
/// own `cli` line does not).
struct Conn {
    id: u64,
    peer: Option<i32>,
    activity: Activity,
    stats: ConnStats,
    opened: Instant,
    /// Lasted this long, it writes its stats when it closes.
    stats_after: Duration,
}

/// A connection that lasted this long says what it read when it closes.
pub const CONN_STATS_AFTER: Duration = Duration::from_secs(10);

impl Conn {
    /// Log a request answered with `response` after `took`; a quiet one is
    /// counted instead, unless it was slow.
    fn answered(&mut self, described: activity::Described, took: Duration, response: &Response) {
        let (reply, error) = activity::reply(response);
        if described.quiet {
            self.stats.add(described.kind, took, error.is_some());
            if self.stats.due() {
                self.flush();
            }
            if took < activity::SLOW {
                return;
            }
        }
        let mut fields = described.fields;
        fields.insert("req".into(), described.kind.into());
        fields.insert("ms".into(), activity::ms(took).into());
        fields.insert("reply".into(), reply.into());
        fields.insert("error".into(), error.into());
        if described.quiet {
            fields.insert("slow".into(), true.into());
        }
        self.record("request", fields);
    }

    fn record(&self, kind: &str, mut fields: serde_json::Map<String, serde_json::Value>) {
        fields.insert("conn".into(), self.id.into());
        fields.insert("peer".into(), self.peer.into());
        self.activity.record(kind, fields.into());
    }

    /// Write what was counted (`conn.stats`), if anything.
    fn flush(&mut self) {
        if let Some(serde_json::Value::Object(fields)) = self.stats.take() {
            self.record("conn.stats", fields);
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if self.opened.elapsed() >= self.stats_after {
            self.flush();
        }
    }
}

/// Handle a single client connection.
/// Starts as newline-delimited JSON request/response.
/// If the client sends an Attach request, switches to raw byte streaming.
/// After a Subscribe request, daemon events are pushed to the client as JSON
/// lines between responses.
async fn handle_connection(
    stream: UnixStream,
    state: Arc<Mutex<DaemonState>>,
    mut conn: Conn,
) -> Result<()> {
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
                        // Not serde's message: it can quote the request.
                        let kind = serde_json::from_slice::<serde_json::Value>(line.trim_ascii())
                            .ok()
                            .and_then(|v| Some(activity::cut(v.get("type")?.as_str()?, 50)));
                        let mut fields = serde_json::Map::new();
                        fields.insert("req".into(), kind.into());
                        fields.insert("error".into(), format!("{:?}", e.classify()).into());
                        conn.record("request.invalid", fields);
                        write_response(&mut writer, &resp).await?;
                        line.clear();
                        continue;
                    }
                };
                let described = activity::describe(&request);
                let started = Instant::now();

                // Attach takes over the connection — handle it specially
                let response = if let Request::Attach { id, cols, rows } = request {
                    let attach_resources = {
                        let state_guard = state.lock().await;
                        match state_guard.pty(&id) {
                            Some(pty) => {
                                // Before the replay: it is drawn at this size.
                                pty.resize(cols, rows);
                                let (replay, output_rx) = pty.attach_output();
                                Ok((output_rx, replay, pty.master(), pty.viewers()))
                            }
                            None => Err(not_found(&id)),
                        }
                    };

                    match attach_resources {
                        Ok((output_rx, replay, master, viewers)) => {
                            write_response(&mut writer, &Response::Attached).await?;
                            let buffered = buf_reader.buffer().to_vec();
                            let reader = buf_reader.into_inner();
                            let typed = handle_attach_session(
                                reader, writer, buffered, output_rx, replay, master, viewers,
                            )
                            .await;
                            // Written when it ends: `ms` is how long it lasted.
                            let mut fields = described.fields;
                            fields.insert("ms".into(), activity::ms_since(started).into());
                            fields.insert("wait".into(), true.into());
                            fields.insert("typed_bytes".into(), (*typed.as_ref().unwrap_or(&0)).into());
                            conn.record("attach", fields);
                            return typed.map(|_| ());
                        }
                        Err(message) => Response::Error { message },
                    }
                } else if let Request::Subscribe = request {
                    // Subscribe before replying: no event after the Ok is missed.
                    event_rx = Some(state.lock().await.event_tx.subscribe());
                    Response::Ok
                } else if let Request::WatchScreen { id, viewing } = request {
                    match id {
                        None => {
                            screen_watch = None;
                            Response::Ok
                        }
                        Some(id) => match state.lock().await.pty(&id) {
                            Some(pty) => {
                                screen_watch = Some(ScreenWatch {
                                    changes: pty.screen_changes(),
                                    id,
                                    quiet_until: None,
                                    typing_until: None,
                                    _viewer: viewing.then(|| Viewer::of(pty.viewers())),
                                });
                                Response::Ok
                            }
                            None => Response::Error {
                                message: not_found(&id),
                            },
                        },
                    }
                } else if let Request::Keys { id, data } = request {
                    let master = state.lock().await.pty(&id).map(Pty::master);
                    match master {
                        Some(master) => {
                            // Off the runtime: a PTY whose agent reads nothing blocks.
                            let written = tokio::task::spawn_blocking(move || {
                                write_pty(&master, data.as_bytes())
                            })
                            .await
                            .unwrap_or(false);
                            // What was typed shows at once, and the next
                            // changes soon after.
                            if let Some(watch) = screen_watch.as_mut().filter(|w| w.id == id) {
                                let now = tokio::time::Instant::now();
                                watch.typing_until = Some(now + TYPING_FOR);
                                watch.quiet_until = None;
                            }
                            if written {
                                Response::Ok
                            } else {
                                Response::Error {
                                    message: format!("could not type into '{id}'"),
                                }
                            }
                        }
                        None => Response::Error {
                            message: not_found(&id),
                        },
                    }
                } else if let Request::WaitInbox { agent_id } = request {
                    // Takes over the connection until messages are handed
                    // over (the log has the messages, not the wait).
                    return handle_wait_inbox(buf_reader, writer, &state, &agent_id).await;
                } else if let Request::WaitAnswer { agent_id, payload } = request {
                    // Takes over the connection until the request is over
                    // (the log has the request: `ask.open`, `ask.end`).
                    return handle_wait_answer(buf_reader, writer, &state, &agent_id, &payload).await;
                } else {
                    dispatch(request, &state).await
                };
                // Before the reply: a client gone meanwhile (a `tam wait`
                // interrupted) fails the write.
                conn.answered(described, started.elapsed(), &response);
                write_response(&mut writer, &response).await?;

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
                        warn!(missed = n, conn = conn.id, "client lagging behind, events dropped");
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

/// Bidirectional raw byte relay between a client and an agent's PTY. How
/// many bytes the client typed (for the log: never what).
async fn handle_attach_session(
    mut reader: tokio::net::unix::OwnedReadHalf,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    buffered: Vec<u8>,
    mut output_rx: broadcast::Receiver<Vec<u8>>,
    replay: Vec<u8>,
    master: Arc<OwnedFd>,
    viewers: Arc<AtomicUsize>,
) -> Result<u64> {
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
    let typed = AtomicU64::new(buffered.len() as u64);
    let input_task = async {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if nix::unistd::write(&*write_master, &buf[..n]).is_err() {
                        break;
                    }
                    typed.fetch_add(n as u64, Ordering::Relaxed);
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
    Ok(typed.load(Ordering::Relaxed))
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
                    launch: crate::agent::Launch::default(),
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
            handle_answer_hook(state, &agent_id, &event, payload.as_ref()).await;
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
                    let runs = state.ledger.runs_of(&member.id);
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
        } => handle_drop(state, &name, force, delete_branch).await,
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
            session,
            args,
            prompt,
            env,
            wrapper,
        } => {
            handle_run_agent(
                state,
                RunSpec {
                    member,
                    provider,
                    session,
                    args,
                    prompt,
                    launch: crate::agent::Launch { env, wrapper },
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
        Request::RepairMember { member, env } => handle_repair_member(state, &member, env).await,
        Request::AdoptWorktree { dir, name, join } => {
            handle_adopt_worktree(state, dir, name, join).await
        }
        Request::Screen { id, from } => match state.lock().await.pty(&id) {
            Some(pty) => Response::Screen {
                screen: pty.screen(from),
            },
            None => Response::Error {
                message: not_found(&id),
            },
        },
        Request::WatchScreen { .. } => Response::Error {
            message: "watch_screen should be handled by connection handler".into(),
        },
        Request::Keys { .. } => Response::Error {
            message: "keys should be handled by connection handler".into(),
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
                // Not a task: a chat's.
                Some(name) if state.ledger.task(&name).is_none() => {
                    match state.ledger.resolve_chat(&name) {
                        Ok(chat) => Response::Messages {
                            messages: state.ledger.chat_messages(&chat.id),
                        },
                        Err(_) => Response::Error {
                            message: format!("task or chat '{}' not found", name),
                        },
                    }
                }
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
        Request::GetBrief { task } => handle_get_brief(state, &task).await,
        Request::SetBrief { task, brief } => handle_set_brief(state, &task, brief).await,
        Request::MergeTasks { into, from } => handle_merge_tasks(state, &into, &from).await,
        Request::SplitMember { member, name } => {
            handle_split_member(state, &member, name.as_deref()).await
        }
        Request::WaitAnswer { .. } => Response::Error {
            message: "wait_answer should be handled by connection handler".into(),
        },
        Request::Answer { agent, ask, answer } => handle_answer(state, &agent, &ask, answer).await,
        Request::CreateChat { dir, name, session } => {
            handle_create_chat(state, dir, name, session).await
        }
        Request::ListChats => Response::Chats {
            chats: state.lock().await.ledger.chats(),
        },
        Request::GetChat { chat } => {
            let state = state.lock().await;
            match state.ledger.resolve_chat(&chat) {
                Ok(chat) => Response::Chat {
                    runs: state.ledger.runs_of(&chat.id),
                    chat: Box::new(chat),
                },
                Err(e) => Response::Error {
                    message: format!("{e:#}"),
                },
            }
        }
        Request::RenameChat { chat, name } => {
            handle_change_chat(state, &chat, |ledger, id| {
                ledger.rename_chat(id, name.as_deref())
            })
            .await
        }
        Request::KeepChat { chat, keep } => {
            handle_change_chat(state, &chat, |ledger, id| ledger.keep_chat(id, keep)).await
        }
        Request::CloseChat { chat } => handle_close_chat(state, &chat).await,
        Request::ChatHistory { dir } => handle_chat_history(state, dir).await,
        Request::ResolveTarget { target, from } => {
            handle_resolve_target(state, &target, from.as_deref()).await
        }
        Request::OpenShell { on, env, wrapper } => {
            handle_open_shell(state, on, crate::agent::Launch { env, wrapper }).await
        }
        Request::ListShells => {
            let mut state = state.lock().await;
            state.reap_shells();
            Response::Shells {
                shells: state.shells_in_order(),
            }
        }
        Request::CloseShell { id } => handle_close_shell(state, &id).await,
    }
}

/// The error for an id that is no running agent's or open shell's.
fn not_found(id: &str) -> String {
    match shell::is_shell_id(id) {
        true => format!("shell '{id}' not found"),
        false => format!("agent '{id}' not found"),
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
    /// The client's environment (`RunAgent`); never kept.
    launch: crate::agent::Launch,
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
        launch,
    } = spec;

    // A member's agent runs in its worktree, under the member id; a chat's
    // in its repository's main checkout, under the chat id.
    let (id, dir) = match &member {
        Some(member) => {
            let Some(owner) = state.ledger.owner(member) else {
                return Response::Error {
                    message: format!("task member or chat '{}' not found", member),
                };
            };
            if id.as_ref().is_some_and(|id| id != member) {
                return Response::Error {
                    message: format!("the agent of '{}' runs under its id", member),
                };
            }
            if !dir.as_os_str().is_empty() && dir != owner.dir() {
                return Response::Error {
                    message: format!(
                        "'{}' runs in {}, not {}",
                        member,
                        owner.dir().display(),
                        dir.display()
                    ),
                };
            }
            if let Owner::Member(task, _) = &owner {
                if state.dropping.contains(&task.name) {
                    return Response::Error {
                        message: format!("task '{}' is being dropped", task.name),
                    };
                }
            }
            if state.agents.contains_key(member) {
                return Response::Error {
                    message: format!("'{}' already has a running agent", owner.label()),
                };
            }
            (Some(member.clone()), owner.dir().to_path_buf())
        }
        None => (id, dir),
    };

    let id = id.unwrap_or_else(|| {
        let id = format!("agent-{}", state.next_id);
        state.next_id += 1;
        id
    });

    if state.agents.contains_key(&id) || state.shells.contains_key(&id) {
        return Response::Error {
            message: format!("agent '{}' already exists", id),
        };
    }

    let socket_path = state.socket_path.to_string_lossy().to_string();
    let env_vars = [
        ("TAM_AGENT_ID", id.as_str()),
        ("TAM_SOCKET", socket_path.as_str()),
    ];

    // A member's task directory is there for any agent; a Claude agent
    // also reaches it and reads the other members' worktrees (see
    // `brief::access`).
    let access = member
        .as_deref()
        .and_then(|member| state.member_access(member))
        .filter(|_| provider::grants_access(&provider));
    let readable = access
        .as_ref()
        .map(|a| a.read_only.clone())
        .unwrap_or_default();
    let resolved = Arc::from(provider::resolve_with(&provider, access));
    let started = Instant::now();
    let spawned = Agent::spawn(
        resolved,
        &dir,
        &args,
        resume_session.as_deref(),
        prompt.as_deref(),
        &env_vars,
        &launch,
    );
    state.activity.record(
        "agent.spawn",
        json!({
            "id": id,
            "task": member.as_deref().and_then(|m| state.ledger.member_task(m)),
            "provider": provider,
            "dir": dir,
            "resume": resume_session.is_some(),
            "args": args.len(),
            "prompt_len": prompt.as_ref().map(|p| p.chars().count()),
            "wrapper": !launch.wrapper.is_empty(),
            "ms": activity::ms_since(started),
            "error": spawned.as_ref().err().map(|e| activity::cut(&format!("{e:#}"), 500)),
        }),
    );
    match spawned {
        Ok(mut agent) => {
            info!(id = %id, provider = %provider, dir = %dir.display(), "spawned agent");
            agent.set_owner(member.clone());
            agent.set_readable(readable);
            state.agents.insert(id.clone(), agent);
            let info = state.agent_info(&id).expect("agent just inserted");
            state.broadcast(Event::AgentSpawned {
                id: id.clone(),
                info,
            });
            if let Some(member) = member {
                match state.ledger.run_started(&member, &provider, resume_session) {
                    Ok(owner) => state.broadcast(owner.updated()),
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
    session: SessionChoice,
    args: Vec<String>,
    prompt: Option<String>,
    /// The client's environment; never kept.
    launch: crate::agent::Launch,
}

/// Start a member's (or a chat's) agent, in its current session unless told
/// otherwise.
async fn handle_run_agent(state: &Arc<Mutex<DaemonState>>, spec: RunSpec) -> Response {
    let RunSpec {
        member,
        provider,
        session,
        args,
        prompt,
        launch,
    } = spec;

    let (label, chat, provider, current) = {
        let state = state.lock().await;
        let Some(owner) = state.ledger.owner(&member) else {
            return Response::Error {
                message: format!("task member or chat '{}' not found", member),
            };
        };
        let label = owner.label();
        let chat = matches!(owner, Owner::Chat(_));
        // Before looking for the session: it would not be resumed anyway.
        if state.agents.contains_key(&member) {
            return Response::Error {
                message: format!("'{}' already has a running agent", label),
            };
        }
        // Claude, whatever the member's earlier runs were.
        let provider = provider.unwrap_or_else(|| "claude".into());
        let current = state.ledger.current_session(&member, &provider);
        (label, chat, provider, current)
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
                    let hint = if chat {
                        ""
                    } else {
                        " (R in the TUI, tam run --new-session)"
                    };
                    return Response::Error {
                        message: format!(
                            "the {} session of '{}' ({}) can no longer be resumed: start a \
                             new one{}",
                            provider, label, id, hint
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
            launch,
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
    if state.shells.contains_key(id) {
        return Response::Error {
            message: format!("'{id}' is a shell, not an agent: close it (CloseShell)"),
        };
    }

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
    match state.pty(id) {
        Some(pty) => Response::Scrollback { data: pty.text() },
        None => Response::Error {
            message: not_found(id),
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
    match state.pty(id) {
        Some(pty) => {
            pty.resize(cols, rows);
            Response::Ok
        }
        None => Response::Error {
            message: not_found(id),
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
    let was = agent.current_state();
    let Some(result) = agent.handle_hook_event(event, payload) else {
        return Response::Error {
            message: format!("unknown hook event '{}' for agent '{}'", event, agent_id),
        };
    };
    info!(id = %agent_id, event = %event, result = ?result, "hook event");
    // Out of a wait on the user (a permission, a question): answered.
    let answered = autoclean::waits_on_user(was) && agent.current_state() != was;

    let changed = agent.info(agent_id).detail != before;
    let session = agent
        .owner()
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
    if answered {
        state.user_answered(agent_id);
    }
    // After a merge or split: what changed, and its task now.
    if let Some(context) = state.take_peer_note(agent_id, event, payload) {
        return Response::HookContext { context };
    }
    // Whenever a session starts (resumed, cleared, compacted too): what its
    // task is now, and its peers.
    if event == "SessionStart" {
        let note = state.agents.get(agent_id).and_then(|agent| {
            crate::peers::session_note(&state.ledger, agent.owner()?, agent.readable())
        });
        if let Some(context) = note {
            return Response::HookContext { context };
        }
    }
    Response::Ok
}

async fn handle_shutdown(state: &Arc<Mutex<DaemonState>>) -> Response {
    let mut state = state.lock().await;

    // Remove all agents and shells, and stop them in background
    let agents: Vec<_> = state.agents.drain().collect();
    for (id, agent) in &agents {
        state.agent_gone(id, agent, -1);
    }
    let agents: Vec<_> = agents.into_iter().map(|(_, agent)| agent).collect();
    let shells: Vec<_> = state.shells.drain().collect();
    let shells: Vec<_> = shells
        .into_iter()
        .map(|(id, shell)| {
            state.broadcast(Event::ShellClosed { id });
            shell
        })
        .collect();
    state.shutdown = true;
    state.shutdown_reason = "requested";

    if !agents.is_empty() || !shells.is_empty() {
        tokio::task::spawn_blocking(move || stop_all(agents, shells));
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

    let (worktrees, activity) = {
        let state = state.lock().await;
        (state.worktrees.clone(), state.activity.clone())
    };
    let mut phases = activity::Phases::start();
    let held = worktrees.lock().await;
    phases.mark("lock_wait");
    let removal = stop_and_remove(state, name, dirs, force, delete_branch, &mut phases).await;
    drop(held);
    activity.record(
        "drop.steps",
        json!({ "task": name, "ok": removal.is_ok(), "steps_ms": phases.steps() }),
    );

    let mut state = state.lock().await;
    state.dropping.remove(name);
    let mut removal = match removal {
        Ok(removal) => removal,
        Err(response) => return response,
    };
    if let Err(e) = state.ledger.drop_task(name, removal.deleted) {
        return Response::Error {
            message: format!("cannot record the drop of '{name}': {e:#}; drop it again to finish"),
        };
    }
    removal.warnings.extend(state.retire_task_dir(name));
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
    phases: &mut activity::Phases,
) -> Result<Removal, Response> {
    let check = |dirs: Vec<PathBuf>| {
        blocking(move || dirs.into_iter().map(worktrees::check).collect::<Vec<_>>())
    };
    let mut checked = check(dirs.clone()).await?;
    phases.mark("check");
    if !force {
        refuse_lossy_drop(&checked, delete_branch)?;
    }

    let (agents, shells) = {
        let mut state = state.lock().await;
        let members = state.ledger.member_ids(name);
        (state.take_task_agents(name), state.take_shells_of(&members))
    };
    let stopped = Stopped::of(&agents, &shells);
    if stopped.any() {
        info!(
            task = %name,
            agents = agents.len(),
            shells = shells.len(),
            "stopping the agents and closing the shells of dropped task"
        );
        // Gone before the worktrees are removed.
        blocking(move || stop_all(agents, shells)).await?;
        phases.mark("stop");
        if !force {
            checked = check(dirs).await?;
            phases.mark("check");
            refuse_lossy_drop(&checked, delete_branch)?;
        }
    }

    let removed = blocking(move || worktrees::remove_all(&checked, delete_branch)).await?;
    phases.mark("remove");
    removed.map_err(|e| Response::Error {
        message: format!("Task '{name}' kept{}: {e}", stopped.note()),
    })
}

/// `DropTask`: a task, or one member of a task (`<task>/<member>`, a member
/// id). A task's last member takes the task with it.
async fn handle_drop(
    state: &Arc<Mutex<DaemonState>>,
    name: &str,
    force: bool,
    delete_branch: bool,
) -> Response {
    let target = {
        let state = state.lock().await;
        if state.ledger.task(name).is_some() {
            None
        } else {
            // Not `resolve_from`: no short name reaches a peer from here.
            match state.ledger.resolve(name) {
                Ok((task, member)) => Some((task, member)),
                Err(e) => {
                    return Response::Error {
                        message: format!("{e:#}"),
                    }
                }
            }
        }
    };
    match target {
        None => handle_drop_task(state, name, force, delete_branch).await,
        Some((task, member)) if task.members.len() > 1 => {
            handle_drop_member(state, &task, member, force, delete_branch).await
        }
        Some((task, _)) => {
            drop_with_last_member(state, name, &task.name, force, delete_branch).await
        }
    }
}

/// Drop the task whose last member is `member`, saying so.
async fn drop_with_last_member(
    state: &Arc<Mutex<DaemonState>>,
    member: &str,
    task: &str,
    force: bool,
    delete_branch: bool,
) -> Response {
    match handle_drop_task(state, task, force, delete_branch).await {
        Response::Dropped { mut warnings } => {
            warnings.push(format!(
                "'{member}' was the last member of task '{task}': the task is dropped"
            ));
            Response::Dropped { warnings }
        }
        other => other,
    }
}

/// Drop one member of a task that has others, as `handle_drop_task` drops
/// all of a task's: check its worktree, stop its agent and wait for it,
/// check again, remove the worktree (and branch), then forget the member.
/// The task is claimed meanwhile: no agent starts in it, no member joins it.
async fn handle_drop_member(
    state: &Arc<Mutex<DaemonState>>,
    task: &TaskInfo,
    member: tam_proto::MemberInfo,
    force: bool,
    delete_branch: bool,
) -> Response {
    let label = task.member_label(&member);
    let is_last = {
        let mut guard = state.lock().await;
        if !guard.dropping.insert(task.name.clone()) {
            return Response::Error {
                message: format!("task '{}' is already being dropped", task.name),
            };
        }
        // Again, under the claim: another drop may have taken the other
        // members since.
        let is_last = match guard.ledger.member(&member.id) {
            // A merge or split may have moved it since.
            Some((now, _)) if now.name == task.name => now.members.len() < 2,
            _ => {
                guard.dropping.remove(&task.name);
                return Response::Error {
                    message: format!("'{label}' is no longer a member of task '{}'", task.name),
                };
            }
        };
        if is_last {
            guard.dropping.remove(&task.name);
        }
        is_last
    };
    if is_last {
        return drop_with_last_member(state, &label, &task.name, force, delete_branch).await;
    }
    let (worktrees, activity) = {
        let state = state.lock().await;
        (state.worktrees.clone(), state.activity.clone())
    };
    let mut phases = activity::Phases::start();
    let held = worktrees.lock().await;
    phases.mark("lock_wait");
    let removal =
        stop_and_remove_member(state, &label, &member, force, delete_branch, &mut phases).await;
    drop(held);
    activity.record(
        "drop.steps",
        json!({
            "task": task.name,
            "member": member.id,
            "ok": removal.is_ok(),
            "steps_ms": phases.steps(),
        }),
    );

    let mut state = state.lock().await;
    state.dropping.remove(&task.name);
    let removal = match removal {
        Ok(removal) => removal,
        Err(response) => return response,
    };
    let info = match state.ledger.drop_member(&member.id) {
        Ok(info) => info,
        Err(e) => {
            return Response::Error {
                message: format!(
                    "cannot record the drop of '{label}': {e:#}; drop it again to finish"
                ),
            }
        }
    };
    for warning in &removal.warnings {
        warn!(member = %member.id, "{warning}");
    }
    info!(task = %task.name, member = %member.id, "member dropped");
    state.broadcast(Event::TaskUpdated { task: info });
    Response::Dropped {
        warnings: removal.warnings,
    }
}

/// [`stop_and_remove`] for one member: check its worktree (`Err` with
/// `DropRefused` if work would be lost), stop its agent and wait for it,
/// check again, remove the worktree.
async fn stop_and_remove_member(
    state: &Arc<Mutex<DaemonState>>,
    label: &str,
    member: &tam_proto::MemberInfo,
    force: bool,
    delete_branch: bool,
    phases: &mut activity::Phases,
) -> Result<Removal, Response> {
    let check = |dir: PathBuf| blocking(move || vec![worktrees::check(dir)]);
    let mut checked = check(member.dir.clone()).await?;
    phases.mark("check");
    if !force {
        refuse_lossy_drop(&checked, delete_branch)?;
    }
    let (agents, shells) = {
        let mut state = state.lock().await;
        let agent = state.take_member_agent(&member.id);
        let shells = state.take_shells_of(std::slice::from_ref(&member.id));
        (Vec::from_iter(agent), shells)
    };
    let stopped = Stopped::of(&agents, &shells);
    if stopped.any() {
        info!(
            member = %member.id,
            shells = shells.len(),
            "stopping the agent and closing the shells of dropped member"
        );
        // Gone before the worktree is removed.
        blocking(move || stop_all(agents, shells)).await?;
        phases.mark("stop");
        if !force {
            checked = check(member.dir.clone()).await?;
            phases.mark("check");
            refuse_lossy_drop(&checked, delete_branch)?;
        }
    }
    let label = label.to_string();
    let removed = blocking(move || worktrees::remove_all(&checked, delete_branch)).await?;
    phases.mark("remove");
    removed.map_err(|e| Response::Error {
        message: format!("'{label}' kept{}: {e}", stopped.note()),
    })
}

/// What a drop stopped before removing worktrees, for its error.
#[derive(Clone, Copy)]
struct Stopped {
    agents: bool,
    shells: bool,
}

impl Stopped {
    fn of(agents: &[Agent], shells: &[Shell]) -> Self {
        Self {
            agents: !agents.is_empty(),
            shells: !shells.is_empty(),
        }
    }

    fn any(self) -> bool {
        self.agents || self.shells
    }

    fn note(self) -> &'static str {
        match (self.agents, self.shells) {
            (false, false) => "",
            (true, false) => " (its agent was stopped)",
            (false, true) => " (its shells were closed)",
            (true, true) => " (its agent was stopped, its shells closed)",
        }
    }
}

/// Stop agents and close shells, all at once, and wait for them.
/// Blocking: off the async runtime, outside the state lock.
fn stop_all(agents: Vec<Agent>, shells: Vec<Shell>) {
    std::thread::scope(|scope| {
        for agent in agents {
            scope.spawn(move || agent.kill_and_drop());
        }
        for shell in shells {
            scope.spawn(move || shell.close());
        }
    });
}

impl DaemonState {
    /// Take a member's (or chat's) agent out of the table, telling clients
    /// and recording the end of its run. The caller kills it.
    fn take_member_agent(&mut self, member: &str) -> Option<Agent> {
        // A member's agent runs under the member id, a chat's under its id.
        let agent = self.agents.remove(member)?;
        self.agent_gone(member, &agent, -1);
        Some(agent)
    }
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
    let (worktrees, activity) = {
        let state = state.lock().await;
        (state.worktrees.clone(), state.activity.clone())
    };
    let mut phases = activity::Phases::start();
    let _held = worktrees.lock().await;
    phases.mark("lock_wait");
    let (ids, recorded): (Vec<String>, Vec<Recorded>) =
        state.lock().await.recorded_worktrees().into_iter().unzip();
    let members = ids.len();
    let observed = observe(recorded).await;
    phases.mark("git");
    state
        .lock()
        .await
        .record_health(ids.into_iter().zip(observed));
    // Every 10 s: only when slow (a change is `member.health`).
    if phases.total_ms() >= RECONCILE_SLOW_MS {
        activity.record(
            "reconcile.slow",
            json!({ "members": members, "ms": phases.total_ms(), "steps_ms": phases.steps() }),
        );
    }
}

/// A reconcile pass that took this long is logged.
const RECONCILE_SLOW_MS: f64 = 1000.0;

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
                self.activity.record(
                    "member.health",
                    json!({ "task": task, "member": member, "health": health }),
                );
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
/// [`worktree::recreate`] checks again right before acting. Git runs in the
/// client's environment `env` (the daemon's when empty).
async fn handle_repair_member(
    state: &Arc<Mutex<DaemonState>>,
    target: &str,
    env: std::collections::BTreeMap<String, String>,
) -> Response {
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
    let env = (!env.is_empty()).then_some(env);
    let recreated = run_git(move || {
        tam_worktree::git::with_env(env, || worktree::recreate(&at, &dir, &branch))
    });
    if let Err(e) = recreated.await {
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
            runs: state.ledger.runs_of(&member.id),
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
    let activity = state.lock().await.activity.clone();
    let mut phases = activity::Phases::start();
    let count = members.len();
    let response = create_members(state, task, members, source, join, env, &mut phases).await;
    activity.record(
        "create.steps",
        json!({
            "task": task,
            "join": join,
            "members": count,
            "ok": matches!(response, Response::Task { .. }),
            "steps_ms": phases.steps(),
        }),
    );
    response
}

/// [`handle_create_members`], its steps timed.
async fn create_members(
    state: &Arc<Mutex<DaemonState>>,
    task: &str,
    members: Vec<NewMember>,
    source: Option<String>,
    join: bool,
    env: std::collections::BTreeMap<String, String>,
    phases: &mut activity::Phases,
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
    phases.mark("lock_wait");
    if let Err(why) = state.lock().await.can_create_members(task, join) {
        return error(why);
    }

    let env = (!env.is_empty()).then_some(env);
    let (check, from, in_env) = (members.clone(), source.clone(), env.clone());
    let checked = run_git(move || {
        tam_worktree::git::with_env(in_env, || worktrees::check_new(&check, from.as_deref()))
    });
    let checked = checked.await;
    phases.mark("check");
    if let Err(e) = checked {
        return error(format!("{e:#}\nNothing was created."));
    }
    let (create, in_env) = (members.clone(), env.clone());
    let created = run_git(move || {
        Ok(tam_worktree::git::with_env(in_env, || {
            worktrees::create_all(&create, source.as_deref())
        }))
    });
    let created = created.await;
    phases.mark("create");
    let created = match created {
        Ok(Ok(created)) => created,
        Ok(Err(report)) => return error(report),
        Err(e) => return error(format!("{e:#} (worktrees may be left behind)")),
    };
    let observed = observe(members.iter().map(Recorded::from).collect()).await;
    phases.mark("observe");

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

impl DaemonState {
    /// The member (or chat) a caller's agent id stands for: the one its
    /// agent runs for, or the one with that id. `None`: the user.
    fn sender_member(&self, agent_id: Option<&str>) -> Option<String> {
        let id = agent_id?;
        match self.agents.get(id) {
            Some(agent) => agent.owner().map(str::to_string),
            None => self.ledger.owner(id).map(|o| o.id().to_string()),
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
            self.activity
                .record("message.handed", json!({ "to": member, "messages": ids }));
        }
    }

    /// A message reached its recipient's agent: record it and tell clients.
    /// `in_turn`: the agent reports the end of the turn that got it (whose
    /// last response is the reply).
    fn record_delivery(&mut self, id: &str, in_turn: bool) {
        match self.ledger.message_delivered(id) {
            Ok(Some(message)) => {
                info!(id = %id, to = %message.to, "message delivered");
                self.activity.record(
                    "message.delivered",
                    json!({
                        "message": id,
                        "to": message.to,
                        "after_s": crate::ledger::now().saturating_sub(message.sent_at),
                        "in_turn": in_turn,
                    }),
                );
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
            Some(_) => format!("the agent of '{to}' takes no messages: its next one gets it"),
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
    let recipient = state
        .ledger
        .resolve_recipient(sender.as_deref(), to)
        .map_err(|e| format!("{e:#}"))?;
    if let Owner::Member(task, _) = &recipient {
        if state.dropping.contains(&task.name) {
            return Err(format!("task '{}' is being dropped", task.name));
        }
    }
    let recipient = recipient.id().to_string();
    let (hop, reply_to) = match sender.as_deref() {
        None => (0, None),
        Some(sender) if sender == recipient => {
            return Err("cannot send a message to yourself".into())
        }
        Some(sender) => {
            let hop = state.inbox.next_hop(sender);
            if hop > inbox::MAX_HOPS {
                let label = state
                    .ledger
                    .message_label(sender)
                    .unwrap_or_else(|| sender.to_string());
                return Err(format!(
                    "loop guard: this would be message {hop} of an exchange between agents (at most {}); it starts over once the user prompts '{label}'",
                    inbox::MAX_HOPS
                ));
            }
            state.inbox.check_rate(sender)?;
            let since = crate::ledger::now().saturating_sub(inbox::REPLY_WINDOW_SECS);
            let answered = state.ledger.unanswered_message(&recipient, sender, since);
            (hop, answered)
        }
    };
    let message = state
        .ledger
        .send_message(
            sender.as_deref(),
            &recipient,
            text,
            hop,
            reply_to.as_deref(),
        )
        .map_err(|e| format!("cannot record the message: {e:#}"))?;
    info!(id = %message.id, from = %message.from_label, to = %message.to_label, hop, "message sent");
    state.activity.record(
        "message.sent",
        json!({
            "message": message.id,
            "from": message.from,
            "to": message.to,
            "hop": hop,
            "reply_to": message.reply_to,
            "len": text.chars().count(),
        }),
    );
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
    state.hand_over_messages(&recipient);
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

/// What the hooks of a member's (or chat's) agent tell about its messages:
/// the prompt that shows them to the model (delivered), a prompt the user
/// typed (its agent-to-agent exchanges start over; a chat's last turn), the
/// end of a turn (the reply to the messages it got).
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
        .and_then(Agent::owner)
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
                if state.ledger.is_chat(&member) {
                    state.chat_turn(&member);
                }
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
                .message_label(&member)
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
            .map(|a| a.owner().map(str::to_string))
        {
            None => Err(format!("agent '{}' not found", agent_id)),
            Some(None) => Err(format!(
                "agent '{}' runs for no task member or chat",
                agent_id
            )),
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

// ---------------------------------------------------------------------------
// Outcomes: what members' agents are doing (`tam wait`, `tam result`)
// ---------------------------------------------------------------------------

/// How often a wait looks at the agents again without an event: a safety
/// net, every change comes with one (hooks, the state monitor, exits).
const WAIT_RECHECK: Duration = Duration::from_secs(1);

impl DaemonState {
    /// The ids of the members (or chats) `targets` name (see
    /// `Ledger::resolve_recipient`: a chat after everything else).
    fn resolve_members(&self, targets: &[String]) -> Result<Vec<String>, String> {
        if targets.is_empty() {
            return Err("no task member given".into());
        }
        targets
            .iter()
            .map(|t| {
                self.ledger
                    .resolve_recipient(None, t)
                    .map(|owner| owner.id().to_string())
                    .map_err(|e| format!("{e:#}"))
            })
            .collect()
    }

    /// What a member's (or chat's) agent is doing, and its last response;
    /// `None` once the member is gone. Reap exited agents first: until then
    /// they look alive.
    fn outcome(&self, member: &str) -> Option<MemberOutcome> {
        let (task, label) = match self.ledger.owner(member)? {
            Owner::Member(task, info) => {
                let label = task.member_label(&info);
                (task.name, label)
            }
            Owner::Chat(chat) => (String::new(), chat.label()),
        };
        let last_run = self.ledger.runs_of(member).pop();
        let outcome = match self.agents.get(member) {
            Some(agent) => {
                let running = agent.info(member);
                let detail = *running.detail;
                MemberOutcome {
                    member: member.to_string(),
                    task,
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
                task,
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

// ---------------------------------------------------------------------------
// Briefs (see `brief.rs`)
// ---------------------------------------------------------------------------

impl DaemonState {
    fn tasks_root(&self) -> PathBuf {
        crate::brief::tasks_root(self.ledger.path())
    }

    /// The task a brief request names: a task, else a member's task.
    fn brief_task(&self, target: &str) -> Result<TaskInfo, String> {
        if let Some(task) = self.ledger.task(target) {
            return Ok(task);
        }
        match self.ledger.resolve(target) {
            Ok((task, _)) => Ok(task),
            Err(e) => Err(format!("{e:#}")),
        }
    }

    /// The task's brief, its directory made if missing.
    fn ensure_brief(&self, task: &TaskInfo) -> Result<tam_proto::TaskBrief> {
        let reference = self.ledger.brief(&task.name);
        crate::brief::ensure(&self.tasks_root(), task, reference)
    }

    /// What the agent of `member` may reach outside its worktree (see
    /// `brief::access`), its task's directory made if missing. `None` when
    /// that fails: the agent starts anyway, with its worktree only.
    fn member_access(&self, member: &str) -> Option<provider::TaskAccess> {
        let (task, _) = self.ledger.member(member)?;
        match self.ensure_brief(&task) {
            Ok(brief) => Some(crate::brief::access(&brief, &task, member)),
            Err(e) => {
                warn!(task = %task.name, "cannot make the task's directory: {e:#}");
                None
            }
        }
    }

    /// Remove the directory of a task just dropped (see `brief::retire`);
    /// what to tell the user.
    fn retire_task_dir(&self, name: &str) -> Option<String> {
        match crate::brief::retire(&self.tasks_root(), name, crate::ledger::now()) {
            Ok(Some(kept)) => Some(format!(
                "the brief of task '{name}' is kept at {}",
                kept.display()
            )),
            Ok(None) => None,
            Err(e) => Some(format!(
                "cannot remove the directory of task '{name}': {e:#}"
            )),
        }
    }
}

async fn handle_get_brief(state: &Arc<Mutex<DaemonState>>, target: &str) -> Response {
    let state = state.lock().await;
    let task = match state.brief_task(target) {
        Ok(task) => task,
        Err(message) => return Response::Error { message },
    };
    match state.ensure_brief(&task) {
        Ok(brief) => Response::Brief {
            task: task.name,
            brief,
        },
        Err(e) => Response::Error {
            message: format!("{e:#}"),
        },
    }
}

async fn handle_set_brief(
    state: &Arc<Mutex<DaemonState>>,
    target: &str,
    brief: Option<String>,
) -> Response {
    let mut state = state.lock().await;
    let task = match state.brief_task(target) {
        Ok(task) => task,
        Err(message) => return Response::Error { message },
    };
    if state.dropping.contains(&task.name) {
        return Response::Error {
            message: format!("task '{}' is being dropped", task.name),
        };
    }
    if let Err(e) = state.ledger.set_brief(&task.name, brief.as_deref()) {
        return Response::Error {
            message: format!("{e:#}"),
        };
    }
    info!(task = %task.name, brief = ?brief, "brief set");
    match state.ensure_brief(&task) {
        Ok(brief) => Response::Brief {
            task: task.name,
            brief,
        },
        Err(e) => Response::Error {
            message: format!("the brief is set, but {e:#}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Merge and split: members move between tasks (`Ledger::merge_tasks`,
// `Ledger::split_member`). One ledger append, no git, agents keep running.
// ---------------------------------------------------------------------------

/// Most changes a member's agent is told at once: the oldest go first (the
/// note that follows them tells its task as it is now).
const MAX_PEER_CHANGES: usize = 5;

impl DaemonState {
    /// Why the membership of `tasks` cannot change now: one is unknown or
    /// being dropped (a drop removes the worktrees it listed when claimed).
    fn refuse_move(&self, tasks: &[&str]) -> Option<String> {
        tasks.iter().find_map(|t| {
            if self.ledger.task(t).is_none() {
                Some(format!("task '{t}' not found"))
            } else if self.dropping.contains(*t) {
                Some(format!("task '{t}' is being dropped"))
            } else {
                None
            }
        })
    }

    /// The labels the members of `tasks` have now, by member id.
    fn labels(&self, tasks: &[&str]) -> BTreeMap<String, String> {
        tasks
            .iter()
            .filter_map(|t| self.ledger.task(t))
            .flat_map(|task| {
                let labels: Vec<(String, String)> = task
                    .members
                    .iter()
                    .map(|m| (m.id.clone(), task.member_label(m)))
                    .collect();
                labels
            })
            .collect()
    }

    /// Tell clients the task of moved members' running agents changed (a
    /// client lists an agent under its member, by member id).
    fn agents_moved(&self, members: &[String]) {
        let ids: Vec<&String> = self
            .agents
            .iter()
            .filter(|(_, a)| a.owner().is_some_and(|m| members.iter().any(|id| id == m)))
            .map(|(id, _)| id)
            .collect();
        for id in ids {
            if let Some(info) = self.agent_info(id) {
                self.broadcast(Event::AgentUpdated {
                    id: id.clone(),
                    info,
                });
            }
        }
    }

    /// Keep what changed for the running agents of members that have hooks
    /// (Claude): `(member, change)`. Returns the members told.
    fn note_peer_changes(&mut self, changes: Vec<(String, String)>) -> Vec<String> {
        let mut told = Vec::new();
        for (member, change) in changes {
            let hooked = self
                .agents
                .values()
                .any(|a| a.owner() == Some(member.as_str()) && a.inbox_hook());
            if !hooked {
                continue;
            }
            let kept = self.peer_changes.entry(member.clone()).or_default();
            kept.push(change);
            if kept.len() > MAX_PEER_CHANGES {
                kept.remove(0);
            }
            told.push(member);
        }
        told
    }

    /// What agent `agent_id` is told at this hook of changes to its task:
    /// at a prompt or a tool call (not a subagent's: its context is not the
    /// session's), what changed and its note now. A session start gets the
    /// whole note anyway: they are forgotten.
    fn take_peer_note(
        &mut self,
        agent_id: &str,
        event: &str,
        payload: Option<&serde_json::Value>,
    ) -> Option<String> {
        let member = self.agents.get(agent_id)?.owner()?.to_string();
        let subagent = payload
            .and_then(|p| p.get("agent_id"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| !id.is_empty());
        match event {
            "SessionStart" => {
                self.peer_changes.remove(&member);
                return None;
            }
            "UserPromptSubmit" => {}
            "PostToolUse" if !subagent => {}
            _ => return None,
        }
        let changes = self.peer_changes.remove(&member)?;
        let readable = self.agents.get(agent_id)?.readable();
        crate::peers::change_note(&self.ledger, &member, readable, &changes)
    }

    /// Make the directories of tasks members moved into, if missing: the
    /// note their agents get names the brief and scratch directory there.
    fn ensure_moved_dirs(&self, tasks: &[&TaskInfo]) {
        for task in tasks {
            if let Err(e) = self.ensure_brief(task) {
                warn!(task = %task.name, "cannot make the task's directory: {e:#}");
            }
        }
    }
}

/// Merge tasks `from` into task `into` (`Request::MergeTasks`): their
/// members join it, they are gone. Refused while one of them is being
/// dropped.
async fn handle_merge_tasks(
    state: &Arc<Mutex<DaemonState>>,
    into: &str,
    from: &[String],
) -> Response {
    let error = |why: String| Response::Error {
        message: format!("cannot merge into '{into}': {why}"),
    };
    let mut names = vec![into];
    names.extend(from.iter().map(String::as_str));
    // At once while a drop holds one of them; then after the creation,
    // repair or drop under way (a member joining one of them moves too).
    if let Some(why) = state.lock().await.refuse_move(&names) {
        return error(why);
    }
    let worktrees = state.lock().await.worktrees.clone();
    let _held = worktrees.lock().await;
    let mut state = state.lock().await;
    if let Some(why) = state.refuse_move(&names) {
        return error(why);
    }
    let previous = state.labels(&names);
    // Moving members, with the task each comes from.
    let origins: Vec<(String, String)> = from
        .iter()
        .flat_map(|t| {
            state
                .ledger
                .member_ids(t)
                .into_iter()
                .map(|id| (id, t.clone()))
        })
        .collect();
    let moved: Vec<String> = origins.iter().map(|(id, _)| id.clone()).collect();
    let task = match state.ledger.merge_tasks(into, from) {
        Ok(task) => task,
        Err(e) => return error(format!("{e:#}")),
    };
    info!(task = %into, from = ?from, "tasks merged");
    for name in from {
        state.broadcast(Event::TaskDropped { name: name.clone() });
    }
    state.broadcast(Event::TaskUpdated { task: task.clone() });
    state.agents_moved(&moved);
    state.ensure_moved_dirs(&[&task]);

    let changes = task
        .members
        .iter()
        .map(|m| {
            let change = match origins.iter().find(|(id, _)| *id == m.id) {
                Some((_, origin)) => crate::peers::merged_away(
                    origin,
                    into,
                    previous.get(&m.id).unwrap_or(&m.id),
                    &task.member_label(m),
                ),
                None => crate::peers::merged_in(into, from),
            };
            (m.id.clone(), change)
        })
        .collect();
    let notified = state.note_peer_changes(changes);
    Response::Moved {
        tasks: vec![task],
        moved,
        previous,
        notified,
    }
}

/// Split a member out of its task into a new task (`Request::SplitMember`):
/// `name`, else its branch. Refused for a task's only member, and while its
/// task is being dropped.
async fn handle_split_member(
    state: &Arc<Mutex<DaemonState>>,
    target: &str,
    name: Option<&str>,
) -> Response {
    let first = match state.lock().await.split_plan(target, name) {
        Ok(plan) => plan,
        Err(message) => return Response::Error { message },
    };
    let worktrees = state.lock().await.worktrees.clone();
    let _held = worktrees.lock().await;
    let mut state = state.lock().await;
    // Again, by id: it may have moved meanwhile.
    let (task, member, name) = match state.split_plan(&first.1.id, name) {
        Ok(plan) if plan.0.name == first.0.name => plan,
        Ok(_) => {
            return Response::Error {
                message: format!(
                    "cannot split '{}': it is no longer a member of task '{}'",
                    first.0.member_label(&first.1),
                    first.0.name
                ),
            }
        }
        Err(message) => return Response::Error { message },
    };
    let label = task.member_label(&member);
    let previous = state.labels(&[&task.name]);
    let created = match state.ledger.split_member(&member.id, &name) {
        Ok(created) => created,
        Err(e) => {
            return Response::Error {
                message: format!("cannot split '{label}': {e:#}"),
            }
        }
    };
    let rest = state
        .ledger
        .task(&task.name)
        .expect("the task keeps its other members");
    info!(task = %task.name, member = %member.id, into = %name, "member split");
    state.broadcast(Event::TaskUpdated { task: rest.clone() });
    state.broadcast(Event::TaskCreated {
        task: created.clone(),
    });
    let moved = vec![member.id.clone()];
    state.agents_moved(&moved);
    state.ensure_moved_dirs(&[&created]);

    let mut changes = vec![(
        member.id.clone(),
        crate::peers::split_away(&label, &task.name, &name),
    )];
    for m in &rest.members {
        changes.push((
            m.id.clone(),
            crate::peers::split_off(&label, &member.id, &name),
        ));
    }
    let notified = state.note_peer_changes(changes);
    Response::Moved {
        tasks: vec![created, rest],
        moved,
        previous,
        notified,
    }
}

impl DaemonState {
    /// The member `target` names (see `Ledger::resolve`), its task, and the
    /// name of the task it would be split into; else why it cannot be.
    fn split_plan(
        &self,
        target: &str,
        name: Option<&str>,
    ) -> Result<(TaskInfo, tam_proto::MemberInfo, String), String> {
        // Plain `resolve`, as for a drop. `tam split` sends the member id it
        // resolved as `tam run` does (from an agent, a peer's short name too).
        let (task, member) = self
            .ledger
            .resolve(target)
            .map_err(|e| format!("cannot split '{target}': {e:#}"))?;
        let label = task.member_label(&member);
        let refuse = |why: String| Err(format!("cannot split '{label}': {why}"));
        if self.dropping.contains(&task.name) {
            return refuse(format!("task '{}' is being dropped", task.name));
        }
        if task.members.len() < 2 {
            return refuse(format!(
                "it is the only member of task '{}': there is nothing to split it from",
                task.name
            ));
        }
        let name = match name {
            Some(name) if name.trim().is_empty() => return refuse("a task needs a name".into()),
            Some(name) => name.to_string(),
            None => member.branch.clone(),
        };
        if self.ledger.task(&name).is_some() {
            return refuse(match name == member.branch {
                true => format!("task '{name}' (its branch) already exists: name the new task"),
                false => format!("task '{name}' already exists"),
            });
        }
        Ok((task, member, name))
    }
}

// ---------------------------------------------------------------------------
// Answers: a client answers the permission request an agent's hook waits on
// (`answer.rs`).
// ---------------------------------------------------------------------------

/// How long an answer waits for the hook's connection to take it.
const ANSWER_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

impl DaemonState {
    /// Show clients `ask` as the request of agent `id` they can answer
    /// (`None`: none).
    /// Log that the request agent `id` waits on (if any) ends without an
    /// answer from a client: `how` (`agent_exit`, `hook_event` with the
    /// `event`). (`hook_gone`, no in the terminal or the hook's timeout, and
    /// `answered` are logged where they happen.)
    fn ask_ended(&self, id: &str, how: &str, event: Option<&str>) {
        if let Some((ask, open)) = self.asks.waiting(id) {
            self.activity.record(
                "ask.end",
                json!({
                    "id": id,
                    "ask": ask,
                    "how": how,
                    "event": event,
                    "ms": activity::ms(open),
                    "wait": true,
                }),
            );
        }
    }

    fn show_ask(&mut self, id: &str, ask: Option<tam_proto::Ask>) {
        let changed = self
            .agents
            .get_mut(id)
            .is_some_and(|agent| agent.set_ask(ask));
        if changed {
            if let Some(info) = self.agent_info(id) {
                self.broadcast(Event::AgentUpdated {
                    id: id.to_string(),
                    info,
                });
            }
        }
    }
}

/// A hook event that tells the request an agent waits on is over (its tool
/// ran, a turn ended or started, another request came) releases its hook.
async fn handle_answer_hook(
    state: &Arc<Mutex<DaemonState>>,
    agent_id: &str,
    event: &str,
    payload: Option<&serde_json::Value>,
) {
    let mut state = state.lock().await;
    if state.asks.ended_by(agent_id, event, payload) {
        state.ask_ended(agent_id, "hook_event", Some(event));
        state.asks.release(agent_id);
        state.show_ask(agent_id, None);
    }
}

/// An agent's `PermissionRequest` hook waits for the answer: show the
/// request to clients and hold the connection until one answers it
/// (`HookOutput`), or it is over otherwise (`Ok`: the hook prints nothing),
/// or the hook goes away.
async fn handle_wait_answer(
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    state: &Arc<Mutex<DaemonState>>,
    agent_id: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    let registered = {
        let mut state = state.lock().await;
        // Only a request its provider took as one: it waits on it.
        if state.agents.get(agent_id).is_some_and(Agent::is_pending) {
            state
                .asks
                .register(agent_id, payload)
                .map(|(ask, token, rx)| {
                    let (what, cut, questions) = match &ask.kind {
                        tam_proto::AskKind::Permission { cut, .. } => ("permission", *cut, None),
                        tam_proto::AskKind::Plan { cut, .. } => ("plan", *cut, None),
                        tam_proto::AskKind::Questions { questions } => {
                            ("questions", false, Some(questions.len()))
                        }
                    };
                    let task = state.ledger.member_task(agent_id).map(str::to_string);
                    state.activity.record(
                        "ask.open",
                        json!({
                            "id": agent_id,
                            "task": task,
                            "ask": ask.id,
                            "tool": activity::cut(&ask.tool, 100),
                            "what": what,
                            "cut": cut,
                            "questions": questions,
                        }),
                    );
                    state.show_ask(agent_id, Some(ask));
                    (token, rx)
                })
        } else {
            None
        }
    };
    let Some((token, mut rx)) = registered else {
        return write_response(&mut writer, &Response::Ok).await;
    };

    let mut buf = [0u8; 256];
    tokio::select! {
        delivery = &mut rx => match delivery {
            Ok(Delivery { output, written }) => {
                let response = Response::HookOutput { output };
                let _ = written.send(write_response(&mut writer, &response).await.is_ok());
            }
            // Over otherwise: Claude keeps to its own dialog.
            Err(_) => {
                let _ = write_response(&mut writer, &Response::Ok).await;
            }
        },
        _ = reader.read(&mut buf) => {
            // The hook went away: answered "no" in the terminal, or timed out.
            let mut state = state.lock().await;
            // Logged only if it is this hook's (a newer one may have
            // replaced it).
            let waiting = state
                .asks
                .waiting(agent_id)
                .map(|(ask, open)| (ask.to_string(), open));
            if state.asks.unregister(agent_id, token) {
                if let Some((ask, open)) = waiting {
                    state.activity.record(
                        "ask.end",
                        json!({
                            "id": agent_id,
                            "ask": ask,
                            "how": "hook_gone",
                            "ms": activity::ms(open),
                            "wait": true,
                        }),
                    );
                }
                state.show_ask(agent_id, None);
            }
        }
    }
    Ok(())
}

/// A client answers the request `ask` of agent `agent_id`: hand it to the
/// hook waiting on that request, and reply once the hook has it.
async fn handle_answer(
    state: &Arc<Mutex<DaemonState>>,
    agent_id: &str,
    ask: &str,
    answer: tam_proto::AskAnswer,
) -> Response {
    let (written, open, activity) = {
        let mut state = state.lock().await;
        let open = state.asks.waiting(agent_id).map(|(_, open)| open);
        match state.asks.answer(agent_id, ask, &answer) {
            Ok(written) => {
                if let Some(agent) = state.agents.get_mut(agent_id) {
                    agent.answered();
                }
                state.user_answered(agent_id);
                if let Some(info) = state.agent_info(agent_id) {
                    state.broadcast(Event::AgentUpdated {
                        id: agent_id.to_string(),
                        info,
                    });
                }
                (written, open, state.activity.clone())
            }
            Err(message) => return Response::Error { message },
        }
    };
    // Not the answer itself: a reason or an answer typed is the user's text.
    let kind = activity::answer_kind(&answer);
    info!(id = %agent_id, ask = %ask, answer = kind, "answered");
    let delivered = matches!(
        tokio::time::timeout(ANSWER_WRITE_TIMEOUT, written).await,
        Ok(Ok(true))
    );
    activity.record(
        "ask.end",
        json!({
            "id": agent_id,
            "ask": ask,
            "how": "answered",
            "answer": kind,
            "delivered": delivered,
            "ms": open.map(activity::ms),
            "wait": true,
        }),
    );
    match delivered {
        true => Response::Ok,
        false => Response::Error {
            message: "the agent's hook went away before it got the answer: answer in its terminal"
                .into(),
        },
    }
}

// ---------------------------------------------------------------------------
// Chats: Claude sessions on a repository's main checkout (see the ledger).
// Their agents start with `RunAgent` and are handled as members' are, under
// the chat id: hooks, screen, typing, messages, answers.
// ---------------------------------------------------------------------------

impl DaemonState {
    /// Close the open chat `id`: take its agent out of the table (the end of
    /// its run is recorded with it), forget the chat, tell clients. Returns
    /// the agent, for the caller to kill outside the lock, and the chat as
    /// it was, with its runs. `CloseChat` and autoclean both close this way.
    fn close_chat(
        &mut self,
        id: &str,
    ) -> (
        Option<Agent>,
        Result<(tam_proto::ChatInfo, Vec<tam_proto::RunInfo>)>,
    ) {
        let agent = self.take_member_agent(id);
        let chat = self.ledger.chat(id).expect("the chat is still open");
        let runs = self.ledger.runs_of(id);
        let closed = self.ledger.close_chat(id).map(|()| {
            self.broadcast(Event::ChatClosed { id: id.to_string() });
            (chat, runs)
        });
        (agent, closed)
    }

    /// Record that the user prompted the agent of chat `id`, telling clients.
    fn chat_turn(&mut self, id: &str) {
        match self.ledger.chat_turn(id) {
            Ok(()) => {
                if let Some(chat) = self.ledger.chat(id) {
                    self.broadcast(Event::ChatUpdated { chat });
                }
            }
            Err(e) => error!(chat = %id, "failed to record a turn: {e:#}"),
        }
    }
}

async fn handle_create_chat(
    state: &Arc<Mutex<DaemonState>>,
    dir: PathBuf,
    name: Option<String>,
    session: Option<String>,
) -> Response {
    // Git and Claude's files are read before the lock.
    let given = session.clone();
    let checked = run_git(move || {
        let dir = main_checkout(&dir)?;
        let said = match given {
            Some(session) => claude_session_of(&dir, &session)?,
            None => None,
        };
        Ok((dir, said))
    })
    .await;
    let (dir, said) = match checked {
        Ok(checked) => checked,
        Err(e) => {
            return Response::Error {
                message: format!("{e:#}"),
            }
        }
    };
    let mut state = state.lock().await;
    match state
        .ledger
        .create_chat(&dir, name.as_deref(), session.as_deref())
    {
        Ok(chat) => {
            let chat = match said {
                Some(said) => state
                    .ledger
                    .set_chat_session(&chat.id, said)
                    .unwrap_or(chat),
                None => chat,
            };
            info!(chat = %chat.id, dir = %chat.dir.display(), session = ?chat.session, "chat created");
            state.broadcast(Event::ChatCreated { chat: chat.clone() });
            Response::Chat {
                chat: Box::new(chat),
                runs: Vec::new(),
            }
        }
        Err(e) => Response::Error {
            message: format!("{e:#}"),
        },
    }
}

/// Apply `change` (which says whether anything changed) to the chat
/// `target` names; reply the chat, and tell clients when it changed.
async fn handle_change_chat(
    state: &Arc<Mutex<DaemonState>>,
    target: &str,
    change: impl FnOnce(&mut Ledger, &str) -> Result<bool>,
) -> Response {
    let mut state = state.lock().await;
    let id = match state.ledger.resolve_chat(target) {
        Ok(chat) => chat.id,
        Err(e) => {
            return Response::Error {
                message: format!("{e:#}"),
            }
        }
    };
    match change(&mut state.ledger, &id) {
        Ok(changed) => {
            let chat = state.ledger.chat(&id).expect("the chat is still open");
            if changed {
                info!(chat = %id, name = ?chat.name, keep = chat.keep, "chat changed");
                state.broadcast(Event::ChatUpdated { chat: chat.clone() });
            }
            Response::Chat {
                runs: state.ledger.runs_of(&id),
                chat: Box::new(chat),
            }
        }
        Err(e) => Response::Error {
            message: format!("{e:#}"),
        },
    }
}

/// Close a chat: stop its agent, forget the chat. Nothing on disk is
/// touched (its directory, branch, Claude's transcript). Replies the chat
/// as it was, once its agent is gone.
async fn handle_close_chat(state: &Arc<Mutex<DaemonState>>, target: &str) -> Response {
    let (agent, response) = {
        let mut state = state.lock().await;
        let id = match state.ledger.resolve_chat(target) {
            Ok(chat) => chat.id,
            Err(e) => {
                return Response::Error {
                    message: format!("{e:#}"),
                }
            }
        };
        let (agent, closed) = state.close_chat(&id);
        let response = match closed {
            Ok((chat, runs)) => {
                info!(chat = %id, "chat closed");
                Response::Chat {
                    chat: Box::new(chat),
                    runs,
                }
            }
            Err(e) => Response::Error {
                message: format!("its agent is stopped, but the chat is kept: {e:#}"),
            },
        };
        (agent, response)
    };
    if let Some(agent) = agent {
        // Gone before the reply: the session can be taken up again at once.
        let _ = tokio::task::spawn_blocking(move || agent.kill_and_drop()).await;
    }
    response
}

// ---------------------------------------------------------------------------
// Chats' sessions in Claude's files (`crate::sessions`): each open chat's
// title and last prompt, read again every 10s and kept in the ledger's
// memory; a repository's history, for `CreateChat` to take a session from.
// Read off the state lock.
// ---------------------------------------------------------------------------

/// How often the daemon reads again what Claude's files say of each open
/// chat's session (title, last prompt).
pub const CHAT_SESSION_INTERVAL: Duration = Duration::from_secs(10);

impl DaemonState {
    /// The file of the Claude session chat `id` is in: the transcript its
    /// agent's hooks reported, else (before its first hook) the session its
    /// agent resumed, else (no agent) its current session's, in its
    /// checkout's Claude project. `None`: no such chat, or a new session
    /// nobody typed in yet.
    fn chat_transcript(&self, id: &str) -> Option<PathBuf> {
        let chat = self.ledger.chat(id)?;
        let session = match self.agents.get(id) {
            Some(agent) => {
                if let Some(path) = agent.context_refresh_job(id).transcript {
                    return Some(path);
                }
                let run = self.ledger.runs_of(id).pop();
                run.filter(|r| r.ended_at.is_none())?.session_id?
            }
            None => chat.session?,
        };
        sessions::session_file(&sessions::claude_dir()?, &chat.dir, &session)
    }
}

/// `dir` as a chat's: an absolute path to a repository's main checkout,
/// resolved. Runs git.
fn main_checkout(dir: &Path) -> Result<PathBuf> {
    if !dir.is_absolute() {
        anyhow::bail!("{}: not an absolute path", dir.display());
    }
    worktrees::check_main_checkout(dir)?;
    Ok(worktree::resolve_path(dir))
}

/// What Claude's files say of `session`, a session run in `dir`; an error
/// when they hold no such session. Not looked for (`None`) when Claude's
/// directory is unknown; an empty id is the ledger's to refuse.
fn claude_session_of(dir: &Path, session: &str) -> Result<Option<ChatSession>> {
    let Some(claude) = sessions::claude_dir().filter(|_| !session.trim().is_empty()) else {
        return Ok(None);
    };
    match sessions::session_file(&claude, dir, session).filter(|f| f.is_file()) {
        Some(file) => Ok(Some(chat_session(sessions::read(&file)))),
        None => anyhow::bail!(
            "Claude has no session {} run in {} (looked in {})",
            session,
            dir.display(),
            sessions::project_dir(&claude, dir).display()
        ),
    }
}

fn chat_session(read: Option<tam_proto::SessionInfo>) -> ChatSession {
    read.map(|s| ChatSession {
        title: s.title,
        last_prompt: s.last_prompt,
    })
    .unwrap_or_default()
}

/// Holds the state only while it reads: a daemon shut down lets go of its
/// ledger at once (a test starts the next one in the same process).
async fn chat_session_loop(state: Weak<Mutex<DaemonState>>, every: Duration) {
    // The first pass ran before the daemon took clients.
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let Some(state) = state.upgrade() else {
            break;
        };
        if state.lock().await.shutdown {
            break;
        }
        refresh_chat_sessions(&state).await;
    }
}

/// Read again what Claude's files say of each open chat's session, and
/// push `ChatUpdated` for each chat where it changed.
async fn refresh_chat_sessions(state: &Arc<Mutex<DaemonState>>) {
    let files: Vec<(String, Option<PathBuf>)> = {
        let state = state.lock().await;
        let chats = state.ledger.chats();
        chats
            .into_iter()
            .map(|chat| {
                let file = state.chat_transcript(&chat.id);
                (chat.id, file)
            })
            .collect()
    };
    if files.is_empty() {
        return;
    }
    let read = tokio::task::spawn_blocking(move || {
        files
            .into_iter()
            .map(|(id, file)| {
                let said = chat_session(file.as_deref().and_then(sessions::read));
                (id, file, said)
            })
            .collect::<Vec<_>>()
    })
    .await;
    let Ok(read) = read else {
        return;
    };
    let mut state = state.lock().await;
    for (id, file, said) in read {
        // Its agent went to another session meanwhile: the next pass reads it.
        if state.chat_transcript(&id) != file {
            continue;
        }
        if let Some(chat) = state.ledger.set_chat_session(&id, said) {
            state.broadcast(Event::ChatUpdated { chat });
        }
    }
}

/// The Claude sessions of the repository whose main checkout is `dir`,
/// newest first, but those an open chat holds.
async fn handle_chat_history(state: &Arc<Mutex<DaemonState>>, dir: PathBuf) -> Response {
    let dir = match run_git(move || main_checkout(&dir)).await {
        Ok(dir) => dir,
        Err(e) => {
            return Response::Error {
                message: format!("{e:#}"),
            }
        }
    };
    let Some(claude) = sessions::claude_dir() else {
        return Response::Sessions {
            sessions: Vec::new(),
        };
    };
    // A chat holds its current session, and the one its agent is in (a
    // new session, before its first turn is recorded).
    let held: HashSet<String> = {
        let state = state.lock().await;
        let chats = state.ledger.chats();
        chats
            .into_iter()
            .flat_map(|chat| {
                let running = state
                    .chat_transcript(&chat.id)
                    .and_then(|f| Some(f.file_stem()?.to_string_lossy().into_owned()));
                chat.session.into_iter().chain(running)
            })
            .collect()
    };
    let history = tokio::task::spawn_blocking(move || sessions::history(&claude, &dir))
        .await
        .unwrap_or_default();
    Response::Sessions {
        sessions: history
            .into_iter()
            .filter(|s| !held.contains(&s.id))
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Autoclean (#65): chats past their time closed, at start and periodically
// (see `autoclean.rs`).
// ---------------------------------------------------------------------------

impl DaemonState {
    /// The user answered what agent `agent_id` waited on (a permission, a
    /// question): for a chat's agent, a turn.
    fn user_answered(&mut self, agent_id: &str) {
        let chat = self.agents.get(agent_id).and_then(Agent::owner);
        if let Some(chat) = chat.filter(|id| self.ledger.is_chat(id)) {
            let chat = chat.to_string();
            self.chat_turn(&chat);
        }
    }

    /// The open chats past their time at `now` that may be closed: no agent
    /// runs for them, or one at the prompt nobody is attached to or typing in.
    fn chats_due(&self, now: u64) -> Vec<tam_proto::ChatInfo> {
        self.ledger
            .chats()
            .into_iter()
            .filter(|chat| chat.closes_at.is_some_and(|at| at <= now))
            .filter(|chat| {
                let agent = self.agents.get(&chat.id).map(|a| a.info(&chat.id));
                autoclean::may_close(agent.map(|a| (a.state, a.viewers)))
            })
            .collect()
    }
}

/// Close chats past their time every `every` (the first pass is at start).
/// Holds the state only during a pass: a daemon shut down lets go of its
/// ledger at once (a test starts the next one in the same process).
async fn autoclean_loop(
    state: Weak<Mutex<DaemonState>>,
    mut source: autoclean::Source,
    every: Duration,
) {
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let Some(state) = state.upgrade() else {
            break;
        };
        if state.lock().await.shutdown {
            break;
        }
        clean_chats(&state, &mut source).await;
    }
}

/// Close the chats past their time, as `CloseChat` does: chosen and closed
/// in one hold of the lock (a turn cannot come in between), their agents
/// killed after. The setting is read again first: when it changed, every
/// chat's `closes_at` did.
async fn clean_chats(state: &Arc<Mutex<DaemonState>>, source: &mut autoclean::Source) {
    let limit = source.limit();
    let agents: Vec<Agent> = {
        let mut state = state.lock().await;
        if state.ledger.set_chat_limit(limit) {
            for chat in state.ledger.chats() {
                state.broadcast(Event::ChatUpdated { chat });
            }
        }
        let due = state.chats_due(crate::ledger::now());
        due.into_iter()
            .filter_map(|chat| {
                let (agent, closed) = state.close_chat(&chat.id);
                if closed.is_ok() {
                    let now = crate::ledger::now();
                    state.activity.record(
                        "chat.autoclean",
                        json!({
                            "chat": chat.id,
                            "agent_running": agent.is_some(),
                            "since_turn_s": chat.last_turn_at.map(|t| now.saturating_sub(t)),
                            "since_created_s": now.saturating_sub(chat.created_at),
                        }),
                    );
                }
                match closed {
                    Ok(_) => info!(
                        chat = %chat.id,
                        name = ?chat.name,
                        last_turn_at = ?chat.last_turn_at,
                        closes_at = ?chat.closes_at,
                        "chat closed by autoclean: no turn for too long"
                    ),
                    Err(e) => error!(
                        chat = %chat.id,
                        "autoclean stopped the agent, but the chat is kept: {e:#}"
                    ),
                }
                agent
            })
            .collect()
    };
    if !agents.is_empty() {
        let _ = tokio::task::spawn_blocking(move || {
            for agent in agents {
                agent.kill_and_drop();
            }
        })
        .await;
    }
}

// ---------------------------------------------------------------------------
// Names typed at the command line (#66)
// ---------------------------------------------------------------------------

/// Find the member or chat `target` names (`Request::ResolveTarget`): the
/// rule `SendMessage` applies to its recipient, so a name means the same
/// thing to every command.
async fn handle_resolve_target(
    state: &Arc<Mutex<DaemonState>>,
    target: &str,
    from: Option<&str>,
) -> Response {
    let state = state.lock().await;
    let caller = state.sender_member(from);
    match state.ledger.resolve_recipient(caller.as_deref(), target) {
        Ok(Owner::Member(task, member)) => Response::Member {
            runs: state.ledger.runs_of(&member.id),
            task,
            member: Box::new(member),
        },
        Ok(Owner::Chat(chat)) => Response::Chat {
            runs: state.ledger.runs_of(&chat.id),
            chat: Box::new(chat),
        },
        // After every other kind: a shell's id.
        Err(_) if state.shells.contains_key(target) => Response::Shell {
            shell: Box::new(state.shells[target].info(target)),
        },
        Err(e) => Response::Error {
            message: format!("{e:#}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Shells (#81): the user's `$SHELL` on a task member's worktree or a
// repository's main checkout (see `shell.rs`). In memory only.
// ---------------------------------------------------------------------------

impl DaemonState {
    /// The open shells, in the order they were opened.
    fn shells_in_order(&self) -> Vec<tam_proto::ShellInfo> {
        let mut shells: Vec<(&String, &Shell)> = self.shells.iter().collect();
        shells.sort_by_key(|(_, shell)| shell.seq());
        shells
            .into_iter()
            .map(|(id, shell)| shell.info(id))
            .collect()
    }

    /// Where a shell on task member `target` opens: its worktree, unless
    /// the worktree is gone or broken, or its task is being dropped.
    fn member_place(&self, target: &str) -> Result<(Place, PathBuf, String), String> {
        let (task, member) = self.ledger.resolve(target).map_err(|e| format!("{e:#}"))?;
        let label = task.member_label(&member);
        if self.dropping.contains(&task.name) {
            return Err(format!("task '{}' is being dropped", task.name));
        }
        match self.ledger.health(&member.id) {
            Some(MemberHealth::Gone { .. }) => {
                return Err(format!(
                    "the worktree of '{label}' is gone ({})",
                    member.dir.display()
                ))
            }
            Some(MemberHealth::Broken { problem }) => {
                return Err(format!("the worktree of '{label}' is broken: {problem}"))
            }
            Some(MemberHealth::Ok { .. }) | None => {}
        }
        Ok((Place::Member(member.id), member.dir, member.repo))
    }
}

/// Open a shell on a member's worktree or a main checkout, numbered in its
/// place: the highest number open there plus one.
async fn handle_open_shell(
    state: &Arc<Mutex<DaemonState>>,
    on: tam_proto::ShellPlace,
    launch: crate::agent::Launch,
) -> Response {
    let error = |message: String| Response::Error {
        message: format!("cannot open a shell: {message}"),
    };
    let place = match on {
        tam_proto::ShellPlace::Member(target) => {
            let state = state.lock().await;
            return match state.member_place(&target) {
                Ok(place) => open_shell(state, place, &launch),
                Err(why) => error(why),
            };
        }
        // Git runs before the lock.
        tam_proto::ShellPlace::Checkout(dir) => match run_git(move || main_checkout(&dir)).await {
            Ok(dir) => {
                let repo = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dir.display().to_string());
                (Place::Checkout(dir.clone()), dir, repo)
            }
            Err(e) => return error(format!("{e:#}")),
        },
    };
    open_shell(state.lock().await, place, &launch)
}

/// Open the shell, in one hold of the lock: its task cannot be claimed by a
/// drop in between, nor its number taken.
fn open_shell(
    mut state: tokio::sync::MutexGuard<'_, DaemonState>,
    (place, dir, repo): (Place, PathBuf, String),
    launch: &crate::agent::Launch,
) -> Response {
    // A shell that exited no longer holds its number.
    state.reap_shells();
    let number = shell::next_number(
        state
            .shells
            .values()
            .filter(|s| *s.place() == place)
            .map(Shell::number),
    );
    let id = shell::new_id(|id| state.shells.contains_key(id) || state.agents.contains_key(id));
    let seq = state.next_shell;
    let opening = shell::Opening {
        place,
        dir,
        repo,
        number,
        seq,
    };
    match Shell::open(opening, launch) {
        Ok(opened) => {
            let info = opened.info(&id);
            info!(
                id = %id,
                member = ?info.member,
                dir = %info.dir.display(),
                number,
                "shell opened"
            );
            state.next_shell += 1;
            state.shells.insert(id, opened);
            state.broadcast(Event::ShellOpened {
                shell: info.clone(),
            });
            Response::Shell {
                shell: Box::new(info),
            }
        }
        Err(e) => Response::Error {
            message: format!("cannot open a shell: {e:#}"),
        },
    }
}

/// Close a shell: take it out of the table (clients are told), hang it up
/// and wait for it outside the lock. Replies the shell as it was.
async fn handle_close_shell(state: &Arc<Mutex<DaemonState>>, id: &str) -> Response {
    let (shell, info) = {
        let mut state = state.lock().await;
        let Some(shell) = state.shells.remove(id) else {
            return Response::Error {
                message: not_found(id),
            };
        };
        let info = shell.info(id);
        state.broadcast(Event::ShellClosed { id: id.to_string() });
        (shell, info)
    };
    info!(id = %id, "closing shell");
    let _ = tokio::task::spawn_blocking(move || shell.close()).await;
    Response::Shell {
        shell: Box::new(info),
    }
}
