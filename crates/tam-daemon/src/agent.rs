use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tam_proto::{AgentInfo, AgentResponse, AgentState, Ask};

use crate::provider::{HookData, HookResult, Provider};
pub use crate::pty::Launch;
use crate::pty::Pty;

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
    /// The process on its PTY, and its screen.
    pty: Pty,
    provider: Arc<dyn Provider>,
    dir: PathBuf,
    /// Stored state — used as fallback when provider doesn't do output detection (e.g. Claude with hooks).
    state: AgentState,
    /// Last state reported via events. Used to detect transitions.
    reported_state: AgentState,
    /// When the agent entered `reported_state`.
    reported_since: SystemTime,
    /// Cached context usage percentage, updated periodically.
    context_percent: Option<u8>,
    /// Id of the task member or chat this agent runs for, if any (its runs
    /// are recorded in the ledger): the agent's own id. A member's task is
    /// looked up when needed: it can change.
    owner: Option<String>,
    /// What the agent's hooks reported (session, pending action, …).
    hook_data: HookData,
    /// Other members' worktrees the agent was given read access to when
    /// it started (see `provider::TaskAccess`).
    readable: Vec<PathBuf>,
    /// For the activity log: how long it was in each state, how many hooks
    /// of each event it ran.
    states: BTreeMap<String, Duration>,
    hooks: BTreeMap<String, u64>,
}

impl Agent {
    /// Spawn a new agent process attached to a PTY, in the environment
    /// `launch` gives. `env_vars` are set on the child process on top of it
    /// (TAM_AGENT_ID, TAM_SOCKET: whatever the client's environment had).
    pub fn spawn(
        provider: Arc<dyn Provider>,
        dir: &Path,
        args: &[String],
        resume_session: Option<&str>,
        prompt: Option<&str>,
        env_vars: &[(&str, &str)],
        launch: &Launch,
    ) -> Result<Self> {
        let mut cmd = provider.build_command(dir, args, resume_session, prompt);
        for (key, val) in env_vars {
            cmd.env(key, val);
        }
        let pty = Pty::spawn(cmd, dir, launch, provider.name())?;
        Ok(Self {
            pty,
            provider,
            dir: dir.to_path_buf(),
            state: AgentState::Working,
            reported_state: AgentState::Working,
            reported_since: SystemTime::now(),
            context_percent: None,
            owner: None,
            hook_data: HookData::default(),
            readable: Vec::new(),
            states: BTreeMap::new(),
            hooks: BTreeMap::new(),
        })
    }

    /// The agent's process on its PTY, and its screen.
    pub fn pty(&self) -> &Pty {
        &self.pty
    }

    /// Check if the child process has exited. Returns Some(exit_code) if so.
    /// Agents that exit are cleaned up by the daemon — exit is an event, not a state.
    pub fn check_exited(&mut self) -> Option<i32> {
        self.pty.check_exited()
    }

    /// Send SIGTERM, wait briefly, then SIGKILL if needed.
    pub fn kill(&mut self) {
        self.pty.kill();
    }

    /// Kill the agent and drop it. Intended for use in background tasks
    /// where ownership is transferred (e.g. `spawn_blocking`).
    pub fn kill_and_drop(mut self) {
        self.kill();
    }

    /// Bind the agent to a task member or a chat.
    pub fn set_owner(&mut self, owner: Option<String>) {
        self.owner = owner;
    }

    /// The task member or chat the agent runs for.
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
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

    /// What the activity log says of the run so far: how long it ran, in
    /// each state (milliseconds, the current one until now), and how many
    /// hooks of each event it ran.
    pub fn run_summary(&self) -> serde_json::Value {
        let mut states = self.states.clone();
        *states.entry(self.reported_state.to_string()).or_default() +=
            self.reported_since.elapsed().unwrap_or_default();
        let states: serde_json::Map<_, _> = states
            .into_iter()
            .map(|(state, held)| (state, crate::activity::ms(held).into()))
            .collect();
        serde_json::json!({
            "provider": self.provider.name(),
            "run_ms": crate::activity::ms(self.pty.uptime()),
            "states_ms": states,
            "hooks": self.hooks,
        })
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
        *self.hooks.entry(event.to_string()).or_default() += 1;
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

    /// Check for a state transition. Returns Some((old, new, how long it was
    /// in old)) if state changed. Updates reported_state so the same
    /// transition isn't reported twice.
    pub fn check_state_change(&mut self) -> Option<(AgentState, AgentState, Duration)> {
        let current = self.current_state();
        // Commit a silence-derived state: output that follows (e.g. the
        // prompt echoing the user's typing) must not bring back `working`.
        if self.provider.silent_working_timeout().is_some() {
            self.state = current;
        }
        if current != self.reported_state {
            let old = self.reported_state;
            let held = self.reported_since.elapsed().unwrap_or_default();
            *self.states.entry(old.to_string()).or_default() += held;
            self.reported_state = current;
            self.reported_since = SystemTime::now();
            Some((old, current, held))
        } else {
            None
        }
    }

    /// Compute current state: ask the provider first (output heuristic),
    /// fall back to the stored state (set by hooks or default).
    pub fn current_state(&self) -> AgentState {
        let idle_duration = self.pty.silent_for();
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
    /// left for the caller, who knows the member's task (a chat has none).
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
            pid: Some(self.pty.pid()),
            uptime_secs: self.pty.uptime().as_secs(),
            viewers: self.pty.viewers().load(Ordering::Relaxed),
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
            pid: self.pty.pid(),
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

    /// Whether messages reach the agent through its inbox hook.
    pub fn inbox_hook(&self) -> bool {
        self.provider.inbox_hook()
    }

    /// The response that ended the agent's last turn, in full, as its hooks
    /// reported it; `None` while a turn runs or before the first one ends.
    pub fn response(&self) -> Option<&AgentResponse> {
        self.hook_data.response.as_ref()
    }

    /// Whether the agent's hooks report something it waits on (a
    /// permission, a question).
    pub fn is_pending(&self) -> bool {
        self.hook_data.detail.pending.is_some()
    }

    /// Show `ask` to clients as the request they can answer (`None`: no
    /// longer). Returns whether it changed.
    pub fn set_ask(&mut self, ask: Option<Ask>) -> bool {
        let changed = self.hook_data.detail.ask != ask;
        self.hook_data.detail.ask = ask;
        changed
    }

    /// A client answered the request the agent waited on: it goes on
    /// working (its next hook tells how). The answer counts as output, so
    /// the silence fallback leaves it the usual time to show some.
    pub fn answered(&mut self) {
        self.hook_data.detail.pending = None;
        self.hook_data.detail.ask = None;
        self.set_state(AgentState::Working);
        self.pty.mark_output();
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
        Some(t) if stored == AgentState::Working && silent_for >= t => AgentState::Ready,
        _ => stored,
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
        assert_eq!(s(AgentState::Working, 10), AgentState::Ready);
        // Only `working` is subject to the fallback
        for state in [
            AgentState::Blocked,
            AgentState::Question,
            AgentState::Error,
            AgentState::Ready,
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
