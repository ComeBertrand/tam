//! The terminal half of what the daemon runs: a process on a PTY it owns,
//! with the screen (`Terminal`), the output relayed to viewers, and how the
//! process is stopped. Agents (`crate::agent`) and shells (`crate::shell`)
//! are each one of these plus what they are.

use std::ffi::{OsStr, OsString};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use nix::pty::{openpty, Winsize};
use nix::sys::signal::{kill, killpg, Signal};
use nix::unistd::Pid;
use tam_proto::{AgentEnv, ScreenInfo};
use tokio::sync::{broadcast, watch};
use tracing::error;

use crate::terminal::{self, Terminal};

/// What the client starting a process has it run in (`Request::RunAgent`,
/// `Request::OpenShell`). Held until the process starts, never kept: the
/// environment holds secrets.
#[derive(Default)]
pub struct Launch {
    /// The process's whole environment, the client's. Empty: the daemon's.
    pub env: AgentEnv,
    /// Command the process's command is given to as trailing arguments, in
    /// its directory (`direnv exec .`: the worktree's dev environment). It
    /// should `exec` the command, which then keeps its pid. Empty: none.
    pub wrapper: Vec<String>,
}

/// How long a process hung up ([`Pty::hang_up`]) has to exit before its
/// process group is killed.
const HANGUP_GRACE: Duration = Duration::from_secs(1);

/// How long, once a process hung up exited, to wait for whatever else held
/// its PTY (its jobs, the shell a wrapper forked) to let go of it.
const RELEASE_WAIT: Duration = Duration::from_secs(1);

/// `TERM` of a process whose caller has none (a script, cron): its
/// terminal is a PTY, drawn by tam's screen and the viewers' terminals.
const FALLBACK_TERM: &str = "xterm-256color";

impl Launch {
    /// `cmd` given to the wrapper, if any, which runs where `cmd` would,
    /// with what was set on it.
    fn wrap(&self, cmd: Command) -> Command {
        let Some((program, args)) = self.wrapper.split_first() else {
            return cmd;
        };
        let mut wrapped = Command::new(program);
        wrapped
            .args(args)
            .arg(cmd.get_program())
            .args(cmd.get_args());
        if let Some(dir) = cmd.get_current_dir() {
            wrapped.current_dir(dir);
        }
        for (key, val) in cmd.get_envs() {
            match val {
                Some(val) => wrapped.env(key, val),
                None => wrapped.env_remove(key),
            };
        }
        wrapped
    }

    /// Run `cmd` (in `dir`) with the client's environment instead of the
    /// daemon's, keeping what was set (or removed) on it. The program is
    /// looked up on the client's `PATH`.
    fn apply(&self, cmd: &mut Command, dir: &Path) {
        if !self.env.is_empty() {
            let set: Vec<(OsString, Option<OsString>)> = cmd
                .get_envs()
                .map(|(k, v)| (k.to_os_string(), v.map(OsStr::to_os_string)))
                .collect();
            cmd.env_clear().envs(&self.env.0);
            let term = self.env.0.get("TERM").map(String::as_str);
            if matches!(term, None | Some("" | "dumb")) {
                cmd.env("TERM", FALLBACK_TERM);
            }
            for (key, val) in set {
                match val {
                    Some(val) => cmd.env(key, val),
                    None => cmd.env_remove(key),
                };
            }
        }
        // Not the directory of the shell the environment comes from.
        cmd.env("PWD", dir);
    }
}

/// A process on a PTY the daemon owns, leading its own session, and its
/// screen.
pub struct Pty {
    child: Child,
    master: Arc<OwnedFd>,
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
    /// Ends once no process holds the PTY any more.
    reader: JoinHandle<()>,
}

impl Pty {
    /// Start `cmd` in `dir` on a new PTY, in the environment `launch`
    /// gives (through its wrapper, if any). What was set on `cmd` (or
    /// removed from it) wins over the client's environment. `what` names
    /// the program in errors.
    pub fn spawn(cmd: Command, dir: &Path, launch: &Launch, what: &str) -> Result<Self> {
        anyhow::ensure!(dir.is_dir(), "directory does not exist: {}", dir.display());

        let mut cmd = launch.wrap(cmd);
        launch.apply(&mut cmd, dir);

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
        // Neither end is for the processes the daemon starts (this one
        // gets the slave as its stdio, and other agents and shells, which
        // run what the user types, nothing of it); and a PTY is released
        // only once every process holding its slave lets go.
        for fd in [&master, &slave] {
            fcntl(fd.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
                .context("failed to set up PTY")?;
        }

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
                    // Create new session so the process is detached from our terminal
                    nix::unistd::setsid()
                        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                    // Set the slave PTY as the controlling terminal
                    if libc::ioctl(slave_raw_fd, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                })
                .spawn()
                .with_context(|| match launch.wrapper.first() {
                    Some(wrapper) => format!(
                        "failed to spawn '{}' through '{}' in {}",
                        what,
                        wrapper,
                        dir.display()
                    ),
                    None => format!("failed to spawn '{}' in {}", what, dir.display()),
                })?
        };
        // The slave's copies the command still holds: only the process may
        // keep the PTY open (the reader ends when it no longer does).
        drop(cmd);

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
        let reader = {
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
            child,
            master,
            terminal,
            screen_changed,
            output_tx,
            viewers: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
            last_output_at,
            reader,
        })
    }

    /// Check if the process has exited. Returns Some(exit_code) if so.
    pub fn check_exited(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            Ok(None) => None,
            Err(_) => Some(-1),
        }
    }

    /// Wait up to `limit` for the process to exit; whether it did.
    fn wait_exit(&mut self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if matches!(self.child.try_wait(), Ok(Some(_)) | Err(_)) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Hang the process up, as a terminal that closes does: SIGHUP, on
    /// which an interactive shell hangs up its jobs (each in a process
    /// group of its own) and exits; then, if it is still there after
    /// [`HANGUP_GRACE`], SIGKILL to its process group. Returns once it
    /// exited and nothing holds the PTY any more (its jobs are gone, and
    /// so is the shell when the process is a wrapper that forked it), or
    /// [`RELEASE_WAIT`] after it exited: a job that ignored the hangup is
    /// left. A shell that does not hang up its jobs (dash) leaves them.
    pub fn hang_up(&mut self) {
        let pid = Pid::from_raw(self.child.id() as i32);
        let _ = kill(pid, Signal::SIGHUP);
        if !self.wait_exit(HANGUP_GRACE) {
            // Not reaped: the pid is still the group's.
            let _ = killpg(pid, Signal::SIGKILL);
            if !self.wait_exit(Duration::from_secs(2)) {
                error!(pid = %self.child.id(), "process did not exit after SIGKILL, abandoning");
                return;
            }
        }
        let deadline = Instant::now() + RELEASE_WAIT;
        while !self.released() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Send SIGTERM, wait briefly, then SIGKILL if needed.
    pub fn kill(&mut self) {
        let pid = Pid::from_raw(self.child.id() as i32);

        // Try graceful shutdown first, for up to 200ms.
        let _ = kill(pid, Signal::SIGTERM);
        if self.wait_exit(Duration::from_millis(200)) {
            return;
        }

        // Force kill; wait up to 2s (handles slow process cleanup).
        let _ = kill(pid, Signal::SIGKILL);
        if !self.wait_exit(Duration::from_secs(2)) {
            error!(pid = %self.child.id(), "process did not exit after SIGKILL, abandoning");
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether no process holds the PTY any more: the process and all it
    /// started that kept the terminal (its jobs) are gone.
    pub fn released(&self) -> bool {
        self.reader.is_finished()
    }

    /// How long the process has run.
    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// How long the PTY has been silent.
    pub fn silent_for(&self) -> Duration {
        lock(&self.last_output_at).elapsed()
    }

    /// Count now as output (the silence starts again).
    pub fn mark_output(&self) {
        *lock(&self.last_output_at) = Instant::now();
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

    /// The text of the terminal: history, then screen.
    pub fn text(&self) -> String {
        lock(&self.terminal).text()
    }

    /// Get a clone of the PTY master fd (kept alive by Arc).
    pub fn master(&self) -> Arc<OwnedFd> {
        self.master.clone()
    }

    /// Resize the PTY (at least 1×1) and notify the process.
    pub fn resize(&self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let ws = libc::winsize {
            ws_col: cols,
            ws_row: rows,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        {
            // The terminal first, and no output in between: the process's
            // redraw must find it at the new size.
            let mut terminal = lock(&self.terminal);
            terminal.resize(cols, rows);
            unsafe {
                libc::ioctl(
                    self.master.as_raw_fd(),
                    libc::TIOCSWINSZ as libc::c_ulong,
                    &ws,
                );
            }
        }
        self.screen_changed.send_replace(());
        // Notify the process of the resize
        let _ = kill(Pid::from_raw(self.child.id() as i32), Signal::SIGWINCH);
    }

    /// The screen: live, or scrolled back to line `from`.
    pub fn screen(&self, from: Option<u64>) -> ScreenInfo {
        lock(&self.terminal).screen(from)
    }

    /// Notified on each change of the screen. Closed once the process and
    /// its PTY are gone.
    pub fn screen_changes(&self) -> watch::Receiver<()> {
        self.screen_changed.subscribe()
    }
}

impl Drop for Pty {
    /// Never leave the processes behind: every daemon path stops a process
    /// before dropping it, but a daemon torn down without that (a panic, an
    /// in-process test daemon going away) would orphan them. The process
    /// leads its own session (`setsid`), so its process group is the tree
    /// it started (but for the jobs of a shell, in groups of their own).
    fn drop(&mut self) {
        // `try_wait` keeps the status once reaped: no signal to a reused pid.
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = killpg(Pid::from_raw(self.child.id() as i32), Signal::SIGKILL);
            let _ = self.child.wait();
        }
    }
}

/// Lock a mutex, even one a panicking thread left poisoned: the PTY reader
/// must go on relaying output.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where the PTY reader takes the process's output.
struct PtyReader {
    terminal: Arc<Mutex<Terminal>>,
    screen_changed: watch::Sender<()>,
    output_tx: broadcast::Sender<Vec<u8>>,
    last_output_at: Arc<Mutex<Instant>>,
}

impl PtyReader {
    /// Blocking loop that reads PTY master output, tracks when output last
    /// arrived, feeds the terminal, and broadcasts it to any attached
    /// clients. Exits when the PTY slave side is closed (the process and
    /// all it started that kept the terminal are gone).
    fn run(self, master_fd: i32) {
        let mut buf = [0u8; 4096];
        loop {
            match nix::unistd::read(master_fd, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    *lock(&self.last_output_at) = Instant::now();
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

    #[test]
    fn the_wrapper_runs_the_command_where_it_would_run() {
        let mut cmd = Command::new("claude");
        cmd.current_dir("/w/app--feat")
            .args(["--resume", "s-1", "fix the bug"])
            .env("FROM_PROVIDER", "1")
            .env_remove("TAM_AGENT_ID");
        let launch = Launch {
            env: AgentEnv::default(),
            wrapper: vec!["direnv".into(), "exec".into(), ".".into()],
        };
        let wrapped = launch.wrap(cmd);
        assert_eq!(wrapped.get_program(), "direnv");
        let args: Vec<_> = wrapped.get_args().collect();
        assert_eq!(
            args,
            ["exec", ".", "claude", "--resume", "s-1", "fix the bug"]
        );
        assert_eq!(wrapped.get_current_dir(), Some(Path::new("/w/app--feat")));
        let mut envs: Vec<_> = wrapped.get_envs().collect();
        envs.sort();
        assert_eq!(
            envs,
            [
                (OsStr::new("FROM_PROVIDER"), Some(OsStr::new("1"))),
                (OsStr::new("TAM_AGENT_ID"), None),
            ]
        );

        // Without one, the command as it is.
        let cmd = Launch::default().wrap(Command::new("claude"));
        assert_eq!(cmd.get_program(), "claude");
        assert_eq!(cmd.get_args().count(), 0);
    }

    #[test]
    fn what_was_set_on_the_command_wins_over_the_clients_environment() {
        let mut cmd = Command::new("sh");
        cmd.env("TAM_AGENT_ID", "m-own").env_remove("TAM_SOCKET");
        let launch = Launch {
            env: AgentEnv(
                [
                    ("TAM_AGENT_ID", "m-caller"),
                    ("TAM_SOCKET", "/elsewhere"),
                    ("PATH", "/caller/bin"),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ),
            wrapper: Vec::new(),
        };
        launch.apply(&mut cmd, Path::new("/w/app--feat"));
        let envs: std::collections::BTreeMap<_, _> = cmd.get_envs().collect();
        assert_eq!(envs[OsStr::new("TAM_AGENT_ID")], Some(OsStr::new("m-own")));
        assert_eq!(envs.get(OsStr::new("TAM_SOCKET")).copied().flatten(), None);
        assert_eq!(envs[OsStr::new("PATH")], Some(OsStr::new("/caller/bin")));
        assert_eq!(envs[OsStr::new("TERM")], Some(OsStr::new(FALLBACK_TERM)));
        assert_eq!(envs[OsStr::new("PWD")], Some(OsStr::new("/w/app--feat")));
    }
}
