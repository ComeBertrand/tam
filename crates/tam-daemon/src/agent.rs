use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use nix::pty::{openpty, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tam_proto::{AgentInfo, AgentResponse, AgentState, ScreenInfo};
use tokio::sync::{broadcast, watch};
use tracing::error;

use crate::provider::{HookData, HookResult, Provider};
use crate::terminal::{self, Terminal};

/// Lightweight metadata for context refresh — collected under lock, IO done outside.
pub struct ContextRefreshJob {
    pub id: String,
    pub pid: u32,
    pub dir: PathBuf,
    pub provider: String,
    /// Session transcript reported by the agent's hooks.
    pub transcript: Option<PathBuf>,
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    dir: PathBuf,
    /// Stored state — used as fallback when provider doesn't do output detection (e.g. Claude with hooks).
    state: AgentState,
    child: Child,
    pty_master: Arc<OwnedFd>,
    /// The screen of the PTY, at its size. Locked by the reader while it
    /// takes in and relays a chunk of output.
    terminal: Arc<Mutex<Terminal>>,
    /// Bumped on every change of the screen.
    screen_changed: watch::Sender<()>,
    output_tx: broadcast::Sender<Vec<u8>>,
    viewers: Arc<AtomicUsize>,
    started_at: Instant,
    /// Updated by the PTY reader thread on every output chunk.
    last_output_at: Arc<Mutex<Instant>>,
    /// Last state reported via events. Used to detect transitions.
    reported_state: AgentState,
    /// When the agent entered `reported_state`.
    reported_since: SystemTime,
    /// Cached context usage percentage, updated periodically.
    context_percent: Option<u8>,
    /// Id of the task member this agent runs for, if any (its runs are
    /// recorded in the ledger). The member's task is looked up when needed:
    /// it can change.
    member: Option<String>,
    /// What the agent's hooks reported (session, pending action, …).
    hook_data: HookData,
    _reader_handle: JoinHandle<()>,
    /// Other members' worktrees the agent was given read access to when
    /// it started (see `provider::TaskAccess`).
    readable: Vec<PathBuf>,
}

impl Agent {
    /// Spawn a new agent process attached to a PTY.
    /// `env_vars` are set on the child process (e.g. TAM_AGENT_ID, TAM_SOCKET).
    pub fn spawn(
        provider: Arc<dyn Provider>,
        dir: &Path,
        args: &[String],
        resume_session: Option<&str>,
        prompt: Option<&str>,
        env_vars: &[(&str, &str)],
    ) -> Result<Self> {
        // Verify directory exists
        anyhow::ensure!(dir.is_dir(), "directory does not exist: {}", dir.display());

        let mut cmd = provider.build_command(dir, args, resume_session, prompt);
        for (key, val) in env_vars {
            cmd.env(key, val);
        }

        // Create PTY pair, at the terminal's size: a PTY without one
        // reports 0×0 and each program falls back on its own default.
        let size = Winsize {
            ws_row: terminal::DEFAULT_ROWS,
            ws_col: terminal::DEFAULT_COLS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(&size, None).context("failed to create PTY")?;
        let master = pty.master;
        let slave = pty.slave;

        // Grab raw fd before slave is consumed (valid in child after fork)
        let slave_raw_fd = slave.as_raw_fd();

        // Create stdio from slave PTY
        let stdin_fd = slave.try_clone().context("failed to clone slave fd")?;
        let stdout_fd = slave.try_clone().context("failed to clone slave fd")?;
        let stderr_fd = slave; // consumes original

        let child = unsafe {
            cmd.stdin(Stdio::from(stdin_fd))
                .stdout(Stdio::from(stdout_fd))
                .stderr(Stdio::from(stderr_fd))
                .pre_exec(move || {
                    // Create new session so the agent is detached from our terminal
                    nix::unistd::setsid()
                        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                    // Set the slave PTY as the controlling terminal
                    if libc::ioctl(slave_raw_fd, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                })
                .spawn()
                .with_context(|| {
                    format!("failed to spawn '{}' in {}", provider.name(), dir.display())
                })?
        };

        let master = Arc::new(master);
        let terminal = Arc::new(Mutex::new(Terminal::new(
            terminal::DEFAULT_COLS,
            terminal::DEFAULT_ROWS,
        )));
        let (screen_changed, _) = watch::channel(());
        let (output_tx, _) = broadcast::channel(64);
        let last_output_at = Arc::new(Mutex::new(Instant::now()));

        // Spawn a reader thread that drains PTY output into the terminal +
        // broadcast
        let reader_handle = {
            let reader = PtyReader {
                terminal: terminal.clone(),
                screen_changed: screen_changed.clone(),
                output_tx: output_tx.clone(),
                last_output_at: last_output_at.clone(),
            };
            let master = master.clone();
            std::thread::Builder::new()
                .name("pty-reader".to_string())
                .spawn(move || reader.run(master.as_raw_fd()))
                .context("failed to spawn PTY reader thread")?
        };

        Ok(Self {
            provider,
            dir: dir.to_path_buf(),
            state: AgentState::Working,
            child,
            pty_master: master,
            terminal,
            screen_changed,
            output_tx,
            viewers: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
            last_output_at,
            reported_state: AgentState::Working,
            reported_since: SystemTime::now(),
            context_percent: None,
            member: None,
            hook_data: HookData::default(),
            _reader_handle: reader_handle,
            readable: Vec::new(),
        })
    }

    /// Check if the child process has exited. Returns Some(exit_code) if so.
    /// Agents that exit are cleaned up by the daemon — exit is an event, not a state.
    pub fn check_exited(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            Ok(None) => None,
            Err(_) => Some(-1),
        }
    }

    /// Send SIGTERM, wait briefly, then SIGKILL if needed.
    pub fn kill(&mut self) -> Result<()> {
        let pid = Pid::from_raw(self.child.id() as i32);

        // Try graceful shutdown first
        let _ = kill(pid, Signal::SIGTERM);

        // Wait up to 200ms for graceful exit
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return Ok(());
            }
        }

        // Force kill
        let _ = kill(pid, Signal::SIGKILL);

        // Wait up to 2s for forced exit (handles slow process cleanup)
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return Ok(());
            }
        }

        error!(pid = %self.child.id(), "process did not exit after SIGKILL, abandoning");
        Ok(())
    }

    /// Kill the agent and drop it. Intended for use in background tasks
    /// where ownership is transferred (e.g. `spawn_blocking`).
    pub fn kill_and_drop(mut self) {
        let _ = self.kill();
    }

    /// Bind the agent to a task member.
    pub fn set_member(&mut self, member: Option<String>) {
        self.member = member;
    }

    pub fn member(&self) -> Option<&str> {
        self.member.as_deref()
    }

    /// Record the other members' worktrees the agent can read.
    pub fn set_readable(&mut self, dirs: Vec<PathBuf>) {
        self.readable = dirs;
    }

    /// The other members' worktrees the agent can read, as granted when it
    /// started: members that joined since are not among them.
    pub fn readable(&self) -> &[PathBuf] {
        &self.readable
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn provider_name(&self) -> &str {
        self.provider.name()
    }

    /// Whether the agent's hooks report its sessions.
    pub fn reports_sessions(&self) -> bool {
        self.provider.reports_sessions()
    }

    /// Set the stored state directly (used by hook-based providers).
    pub fn set_state(&mut self, state: AgentState) {
        self.state = state;
    }

    /// Apply a hook event via the provider: update the hook data and the
    /// stored state. Returns `None` if the provider doesn't know the event.
    pub fn handle_hook_event(
        &mut self,
        event: &str,
        payload: Option<&serde_json::Value>,
    ) -> Option<HookResult> {
        let current = self.current_state();
        let result = self
            .provider
            .handle_hook(event, payload, current, &mut self.hook_data);
        match result {
            HookResult::Unknown => return None,
            HookResult::Handled { state: Some(state) } => self.set_state(state),
            // Pin a silence-derived state, so that a hook that changes no
            // state can't bring back a stale `working`.
            HookResult::Handled { state: None } => self.set_state(current),
        }
        Some(result)
    }

    /// Session id reported by the agent's hooks.
    pub fn session_id(&self) -> Option<&str> {
        self.hook_data.detail.session_id.as_deref()
    }

    /// Last session the agent's hooks showed a turn in (or a resume of).
    pub fn resumable_session(&self) -> Option<&str> {
        self.hook_data.resumable_session.as_deref()
    }

    /// Check for a state transition. Returns Some((old, new)) if state changed.
    /// Updates reported_state so the same transition isn't reported twice.
    pub fn check_state_change(&mut self) -> Option<(AgentState, AgentState)> {
        let current = self.current_state();
        // Commit a silence-derived state: output that follows (e.g. the
        // prompt echoing the user's typing) must not bring back `working`.
        if self.provider.silent_working_timeout().is_some() {
            self.state = current;
        }
        if current != self.reported_state {
            let old = self.reported_state;
            self.reported_state = current;
            self.reported_since = SystemTime::now();
            Some((old, current))
        } else {
            None
        }
    }

    /// Compute current state: ask the provider first (output heuristic),
    /// fall back to the stored state (set by hooks or default).
    pub fn current_state(&self) -> AgentState {
        let idle_duration = self.last_output_at.lock().unwrap().elapsed();
        if let Some(state) = self.provider.detect_state_from_output(&[], idle_duration) {
            return state;
        }
        silence_fallback(
            self.state,
            idle_duration,
            self.provider.silent_working_timeout(),
        )
    }

    /// Build an AgentInfo snapshot for reporting to clients. Its `task` is
    /// left for the caller, who knows the member's task.
    pub fn info(&self, id: &str) -> AgentInfo {
        let state = self.current_state();
        // A change not reported yet (e.g. by the hook being handled) is now.
        let since = if state == self.reported_state {
            self.reported_since
        } else {
            SystemTime::now()
        };
        AgentInfo {
            id: id.to_string(),
            provider: self.provider.name().to_string(),
            dir: self.dir.clone(),
            state,
            pid: Some(self.child.id()),
            uptime_secs: self.started_at.elapsed().as_secs(),
            viewers: self.viewers.load(Ordering::Relaxed),
            context_percent: self.context_percent,
            task: None,
            detail: Box::new(self.hook_data.detail.clone()),
            state_since: Some(
                since
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            ),
        }
    }

    /// Collect lightweight metadata for two-phase context refresh.
    /// This is cheap (no IO) and can be called under the lock.
    pub fn context_refresh_job(&self, id: &str) -> ContextRefreshJob {
        ContextRefreshJob {
            id: id.to_string(),
            pid: self.child.id(),
            dir: self.dir.clone(),
            provider: self.provider.name().to_string(),
            transcript: self.hook_data.transcript_path.clone(),
        }
    }

    /// Set context percent. Returns true if the value changed.
    pub fn set_context_percent(&mut self, pct: Option<u8>) -> bool {
        let changed = self.context_percent != pct;
        self.context_percent = pct;
        changed
    }

    pub fn context_percent(&self) -> Option<u8> {
        self.context_percent
    }

    /// Get the viewer count handle for increment/decrement by attach sessions.
    pub fn viewers(&self) -> Arc<AtomicUsize> {
        self.viewers.clone()
    }

    /// For a viewer attaching (its terminal at the PTY's size): what to
    /// show it first, the terminal's state (see `Terminal::replay`), and
    /// the live PTY output after it. Taken under the terminal's lock: each
    /// byte of output is in exactly one of them.
    pub fn attach_output(&self) -> (Vec<u8>, broadcast::Receiver<Vec<u8>>) {
        let terminal = lock(&self.terminal);
        (terminal.replay().into_bytes(), self.output_tx.subscribe())
    }

    /// The text of the agent's terminal: history, then screen.
    pub fn text(&self) -> String {
        lock(&self.terminal).text()
    }

    /// Get a clone of the PTY master fd (kept alive by Arc).
    pub fn pty_master(&self) -> Arc<OwnedFd> {
        self.pty_master.clone()
    }

    /// Resize the agent's PTY (at least 1×1) and notify the agent process.
    pub fn resize(&self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let ws = libc::winsize {
            ws_col: cols,
            ws_row: rows,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        {
            // The terminal first, and no output in between: the agent's
            // redraw must find it at the new size.
            let mut terminal = lock(&self.terminal);
            terminal.resize(cols, rows);
            unsafe {
                libc::ioctl(
                    self.pty_master.as_raw_fd(),
                    libc::TIOCSWINSZ as libc::c_ulong,
                    &ws,
                );
            }
        }
        self.screen_changed.send_replace(());
        // Notify the agent process of the resize
        let _ = kill(Pid::from_raw(self.child.id() as i32), Signal::SIGWINCH);
    }

    /// The agent's screen: live, or scrolled back to line `from`.
    pub fn screen(&self, from: Option<u64>) -> ScreenInfo {
        lock(&self.terminal).screen(from)
    }

    /// Notified on each change of the agent's screen. Closed once the agent
    /// and its PTY are gone.
    pub fn screen_changes(&self) -> watch::Receiver<()> {
        self.screen_changed.subscribe()
    }

    /// Whether messages reach the agent through its inbox hook.
    pub fn inbox_hook(&self) -> bool {
        self.provider.inbox_hook()
    }

    /// Bytes that type `text` into the agent's input (Enter not included).
    pub fn pty_input(&self, text: &str) -> Vec<u8> {
        self.provider.pty_input(text)
    }

    /// The response that ended the agent's last turn, in full, as its hooks
    /// reported it; `None` while a turn runs or before the first one ends.
    pub fn response(&self) -> Option<&AgentResponse> {
        self.hook_data.response.as_ref()
    }
}

impl Drop for Agent {
    /// Never leave the agent's processes behind: every daemon path kills an
    /// agent before dropping it, but a daemon torn down without that (a
    /// panic, an in-process test daemon going away) would orphan them. The
    /// agent leads its own session (`setsid`), so its process group is the
    /// whole tree it started.
    fn drop(&mut self) {
        // `try_wait` keeps the status once reaped: no signal to a reused pid.
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ =
                nix::sys::signal::killpg(Pid::from_raw(self.child.id() as i32), Signal::SIGKILL);
            let _ = self.child.wait();
        }
    }
}

/// State of a hook-driven agent given how long its PTY has been silent: a
/// turn interrupted by the user ends without any hook, leaving `working`
/// stored, so a `working` agent silent for `timeout` is back at the prompt.
/// (`timeout` is the provider's `silent_working_timeout`; `None` disables
/// the fallback.)
///
/// Only `working` is corrected. Nothing fires either when a permission or
/// question is answered (the next hook is the tool's `PostToolUse`), but
/// the output can't tell: Claude's waiting dialogs may redraw as often as
/// its working spinner.
fn silence_fallback(
    stored: AgentState,
    silent_for: Duration,
    timeout: Option<Duration>,
) -> AgentState {
    match timeout {
        Some(t) if stored == AgentState::Working && silent_for >= t => AgentState::Input,
        _ => stored,
    }
}

/// Lock a mutex, even one a panicking thread left poisoned: the PTY reader
/// must go on relaying output.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where the PTY reader takes the agent's output.
struct PtyReader {
    terminal: Arc<Mutex<Terminal>>,
    screen_changed: watch::Sender<()>,
    output_tx: broadcast::Sender<Vec<u8>>,
    last_output_at: Arc<Mutex<Instant>>,
}

impl PtyReader {
    /// Blocking loop that reads PTY master output, tracks when output last
    /// arrived, feeds the terminal, and broadcasts it to any attached
    /// clients. Exits when the PTY slave side is closed
    /// (agent exits).
    fn run(self, master_fd: i32) {
        let mut buf = [0u8; 4096];
        loop {
            match nix::unistd::read(master_fd, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    if let Ok(mut ts) = self.last_output_at.lock() {
                        *ts = Instant::now();
                    }
                    {
                        let mut terminal = lock(&self.terminal);
                        terminal.feed(&data);
                        // Ignore send errors (no receivers is fine)
                        let _ = self.output_tx.send(data);
                    }
                    self.screen_changed.send_replace(());
                }
                Err(nix::errno::Errno::EIO) => break, // PTY closed
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => {
                    error!("PTY read error: {}", e);
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Option<Duration> = Some(Duration::from_secs(10));

    #[test]
    fn silent_working_agent_falls_back_to_input() {
        let s = |stored, secs| silence_fallback(stored, Duration::from_secs(secs), T);
        assert_eq!(s(AgentState::Working, 3), AgentState::Working);
        assert_eq!(s(AgentState::Working, 10), AgentState::Input);
        // Only `working` is subject to the fallback
        for state in [
            AgentState::Blocked,
            AgentState::Question,
            AgentState::Error,
            AgentState::Input,
        ] {
            assert_eq!(s(state, 60), state);
        }
    }

    #[test]
    fn no_fallback_without_timeout() {
        assert_eq!(
            silence_fallback(AgentState::Working, Duration::from_secs(600), None),
            AgentState::Working
        );
    }
}
