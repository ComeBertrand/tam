//! The activity log: what tam did, when, and how long it took, to read
//! afterwards (slow steps, failures, the same actions over and over).
//!
//! One JSON object per line, in a file per local day
//! (`<dir>/2026-11-28.jsonl`) next to the ledger (`log/`, see [`dir_next_to`]):
//! every process of tam appends its own lines (`src`: `daemon`, `cli`, `tui`),
//! each line one `write` on a file opened for appending, so lines from
//! several processes never mix. Each line starts with `ts` (Unix time in
//! milliseconds), `time` (local, readable), `src`, `pid` and `kind`; the
//! other fields depend on `kind` (documented in the README, Activity log).
//! Durations are `ms`, in milliseconds.
//!
//! What it never holds: environments, wrappers, prompts, messages, keys
//! typed, answers' text, hook payloads, briefs. Their length at most. Every
//! field the daemon writes about a request is chosen here
//! ([`describe`]), never a serialised request or response.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};
use tam_proto::{AskAnswer, Request, Response, SessionChoice, ShellPlace};

/// How many days of files are kept by default (`[log] keep_days`).
pub const KEEP_DAYS: u32 = 30;

/// A request quick to answer that took this long is logged anyway.
pub const SLOW: Duration = Duration::from_millis(100);

/// Where the lines go: nowhere (`off`), a directory, or memory (tests).
#[derive(Clone, Default)]
pub struct Activity(Option<Arc<Sink>>);

struct Sink {
    src: &'static str,
    pid: u32,
    out: Out,
}

enum Out {
    /// The file of the day, opened for each line: a log removed meanwhile
    /// is made again, and the day changes at midnight.
    Dir(PathBuf),
    Memory(Mutex<Vec<String>>),
}

impl Activity {
    /// Logs nothing.
    pub fn off() -> Self {
        Self(None)
    }

    /// Lines appended to a file per day in `dir` (made at the first line),
    /// as process kind `src`.
    pub fn to_dir(dir: impl Into<PathBuf>, src: &'static str) -> Self {
        Self::with(src, Out::Dir(dir.into()))
    }

    /// Lines kept in memory, for tests ([`Activity::lines`]).
    pub fn in_memory(src: &'static str) -> Self {
        Self::with(src, Out::Memory(Mutex::default()))
    }

    fn with(src: &'static str, out: Out) -> Self {
        Self(Some(Arc::new(Sink {
            src,
            pid: std::process::id(),
            out,
        })))
    }

    /// Whether lines go anywhere.
    pub fn is_on(&self) -> bool {
        self.0.is_some()
    }

    /// The directory the files are in, if any.
    pub fn dir(&self) -> Option<&Path> {
        match &self.0.as_ref()?.out {
            Out::Dir(dir) => Some(dir),
            Out::Memory(_) => None,
        }
    }

    /// The lines written so far, of an [`Activity::in_memory`].
    pub fn lines(&self) -> Vec<String> {
        match self.0.as_deref().map(|s| &s.out) {
            Some(Out::Memory(lines)) => lines.lock().map(|l| l.clone()).unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// Append one line of kind `kind` with `fields` (an object; its null
    /// fields left out). Never fails: a log must not get in the way.
    pub fn record(&self, kind: &str, fields: Value) {
        let Some(sink) = &self.0 else {
            return;
        };
        let now = SystemTime::now();
        let stamp = Stamp::of(now);
        let mut line = format!(
            "{{\"ts\":{},\"time\":\"{}\",\"src\":\"{}\",\"pid\":{},\"kind\":{}",
            stamp.millis,
            stamp.readable(),
            sink.src,
            sink.pid,
            Value::from(kind),
        );
        let fields = match fields {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            other => Map::from_iter([("value".to_string(), other)]),
        };
        for (key, value) in fields {
            if value.is_null() {
                continue;
            }
            line.push(',');
            line.push_str(&Value::from(key).to_string());
            line.push(':');
            line.push_str(&value.to_string());
        }
        line.push_str("}\n");
        match &sink.out {
            Out::Memory(lines) => {
                if let Ok(mut lines) = lines.lock() {
                    lines.push(line.trim_end().to_string());
                }
            }
            Out::Dir(dir) => {
                if let Some(mut file) = open_day(dir, &stamp.day()) {
                    // One write: lines of other processes never fall in it.
                    let _ = file.write_all(line.as_bytes());
                }
            }
        }
    }
}

fn open_day(dir: &Path, day: &str) -> Option<File> {
    std::fs::create_dir_all(dir).ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{day}.jsonl")))
        .ok()
}

/// The log's directory for the ledger at `ledger`: `log/` next to it, so
/// whatever keeps a ledger apart (tests) keeps its log apart too.
pub fn dir_next_to(ledger: &Path) -> PathBuf {
    ledger
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join("log")
}

/// The log's directory for the default ledger.
pub fn default_dir() -> PathBuf {
    dir_next_to(&crate::ledger::default_path())
}

/// Milliseconds, to the hundredth.
pub fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 100_000.0).round() / 100.0
}

/// Milliseconds since `start`.
pub fn ms_since(start: Instant) -> f64 {
    ms(start.elapsed())
}

/// `text` cut to `max` characters (an error, say: they quote what git said).
pub fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// How long each step of some work took: `mark` ends a step.
pub struct Phases {
    started: Instant,
    last: Instant,
    steps: Map<String, Value>,
}

impl Phases {
    pub fn start() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last: now,
            steps: Map::new(),
        }
    }

    /// Milliseconds since the start.
    pub fn total_ms(&self) -> f64 {
        ms_since(self.started)
    }

    /// The step since the last mark (or the start) is `step` (added to a
    /// step of that name already there).
    pub fn mark(&mut self, step: &str) {
        let now = Instant::now();
        let took =
            ms(now - self.last) + self.steps.get(step).and_then(Value::as_f64).unwrap_or(0.0);
        self.steps
            .insert(step.into(), ((took * 100.0).round() / 100.0).into());
        self.last = now;
    }

    /// The steps, in milliseconds, by name.
    pub fn steps(&self) -> Value {
        Value::Object(self.steps.clone())
    }
}

/// An instant as the log writes it.
struct Stamp {
    millis: i64,
    /// Seconds east of UTC, there and then.
    offset: i64,
}

impl Stamp {
    fn of(time: SystemTime) -> Self {
        let millis = match time.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_millis() as i64,
            Err(e) => -(e.duration().as_millis() as i64),
        };
        Self {
            millis,
            offset: local_offset(millis.div_euclid(1000)),
        }
    }

    fn local_secs(&self) -> i64 {
        self.millis.div_euclid(1000) + self.offset
    }

    /// The local day, `2026-11-28`: the file's name.
    fn day(&self) -> String {
        let (y, m, d) = civil(self.local_secs().div_euclid(86_400));
        format!("{y:04}-{m:02}-{d:02}")
    }

    /// `2026-11-28T15:04:05.123+01:00`.
    fn readable(&self) -> String {
        let secs = self.local_secs();
        let of_day = secs.rem_euclid(86_400);
        let (sign, offset) = match self.offset {
            o if o < 0 => ('-', -o),
            o => ('+', o),
        };
        format!(
            "{}T{:02}:{:02}:{:02}.{:03}{}{:02}:{:02}",
            self.day(),
            of_day / 3600,
            of_day / 60 % 60,
            of_day % 60,
            self.millis.rem_euclid(1000),
            sign,
            offset / 3600,
            offset / 60 % 60,
        )
    }
}

/// Seconds east of UTC of the local time zone at `secs` (Unix time).
fn local_offset(secs: i64) -> i64 {
    let time = secs as libc::time_t;
    // SAFETY: localtime_r writes the zeroed `tm` it is given, nothing else.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            0
        } else {
            tm.tm_gmtoff as i64
        }
    }
}

/// The date of day `days` since 1970-01-01 (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Days since 1970-01-01 of a date (the inverse of [`civil`]).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The day of a file of the log (`2026-11-28.jsonl`), in days since
/// 1970-01-01; `None` for any other file.
fn file_day(name: &str) -> Option<i64> {
    let date = name.strip_suffix(".jsonl")?;
    let mut parts = date.splitn(3, '-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (y, m, d) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then(|| days_from_civil(y, m, d))
}

/// Remove the files of `dir` older than the last `keep_days` days (today
/// counts); other files stay. How many went.
pub fn prune(dir: &Path, keep_days: u32) -> usize {
    let today = Stamp::of(SystemTime::now()).local_secs().div_euclid(86_400);
    prune_before(dir, today - i64::from(keep_days.max(1)) + 1)
}

fn prune_before(dir: &Path, first_kept: i64) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            name.to_str()
                .and_then(file_day)
                .is_some_and(|day| day < first_kept)
        })
        .filter(|entry| std::fs::remove_file(entry.path()).is_ok())
        .count()
}

/// The `[log]` settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// `enabled`: on unless `false`.
    pub enabled: bool,
    /// `keep_days`: files older than this many days are removed when the
    /// daemon starts.
    pub keep_days: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            keep_days: KEEP_DAYS,
        }
    }
}

/// The `[log]` settings of the config file; the defaults for what it does
/// not say or cannot be read.
pub fn settings() -> Settings {
    let text = tam_worktree::config::config_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    parse_settings(&text)
}

fn parse_settings(text: &str) -> Settings {
    let mut settings = Settings::default();
    let Ok(file) = toml::from_str::<toml::Table>(text) else {
        return settings;
    };
    let Some(section) = file.get("log").and_then(toml::Value::as_table) else {
        return settings;
    };
    if let Some(enabled) = section.get("enabled").and_then(toml::Value::as_bool) {
        settings.enabled = enabled;
    }
    if let Some(days) = section.get("keep_days").and_then(toml::Value::as_integer) {
        settings.keep_days = u32::try_from(days.max(1)).unwrap_or(u32::MAX);
    }
    settings
}

/// The log of a process of kind `src`, in the default directory, as the
/// config says.
pub fn for_process(src: &'static str) -> Activity {
    if settings().enabled {
        Activity::to_dir(default_dir(), src)
    } else {
        Activity::off()
    }
}

/// Puts the daemon's warnings and errors (`tracing`'s) in the log as
/// `daemon.warn` and `daemon.error`: they went nowhere when a client started
/// the daemon.
pub struct WarningLayer(pub Activity);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarningLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let level = *event.metadata().level();
        // More verbose levels are greater.
        if level > tracing::Level::WARN {
            return;
        }
        let mut fields = FieldText(Map::new());
        event.record(&mut fields);
        let kind = match level {
            tracing::Level::ERROR => "daemon.error",
            _ => "daemon.warn",
        };
        self.0.record(kind, Value::Object(fields.0));
    }
}

struct FieldText(Map<String, Value>);

impl tracing::field::Visit for FieldText {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().into(), cut(value, 500).into());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().into(), cut(&format!("{value:?}"), 500).into());
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// What the log says of a request: its kind, ids and names, counts and
/// lengths. `quiet`: a read or a terminal's traffic, counted per connection
/// ([`ConnStats`]) and logged alone only when slow.
pub struct Described {
    pub kind: &'static str,
    pub quiet: bool,
    pub fields: Map<String, Value>,
}

/// What the log says of `request` (see [`Described`]). Every request is
/// here, field by field: a new one is a decision about what it may write.
pub fn describe(request: &Request) -> Described {
    let (kind, quiet, fields) = match request {
        Request::Spawn {
            provider,
            dir,
            id,
            args,
            resume_session,
            prompt,
            member,
        } => (
            "spawn",
            false,
            json!({
                "provider": provider,
                "dir": dir,
                "id": id,
                "member": member,
                "args": args.len(),
                "resume": resume_session.is_some(),
                "prompt_len": prompt.as_ref().map(|p| p.chars().count()),
            }),
        ),
        Request::List => ("list", true, json!({})),
        Request::Kill { id } => ("kill", false, json!({ "id": id })),
        Request::Attach { id, cols, rows } => (
            "attach",
            false,
            json!({ "id": id, "cols": cols, "rows": rows }),
        ),
        Request::Scrollback { id } => ("scrollback", true, json!({ "id": id })),
        Request::Resize { id, cols, rows } => (
            "resize",
            true,
            json!({ "id": id, "cols": cols, "rows": rows }),
        ),
        Request::HookEvent {
            agent_id,
            event,
            payload,
        } => (
            "hook_event",
            true,
            json!({ "agent": agent_id, "event": event, "tool": tool_name(payload.as_ref()) }),
        ),
        Request::Shutdown => ("shutdown", false, json!({})),
        Request::Hello { protocol_version } => {
            ("hello", true, json!({ "protocol": protocol_version }))
        }
        Request::CreateTask { name, members } => (
            "create_task",
            false,
            json!({ "task": name, "members": members.len() }),
        ),
        Request::ListTasks => ("list_tasks", true, json!({})),
        Request::GetTask { name } => ("get_task", true, json!({ "task": name })),
        Request::ResolveMember { target, dir, from } => (
            "resolve_member",
            true,
            json!({ "target": target, "dir": dir, "from": from }),
        ),
        Request::DropTask {
            name,
            force,
            delete_branch,
        } => (
            "drop_task",
            false,
            json!({ "target": name, "force": force, "delete_branch": delete_branch }),
        ),
        Request::Subscribe => ("subscribe", true, json!({})),
        Request::RunAgent {
            member,
            provider,
            session,
            args,
            prompt,
            env: _,
            wrapper,
        } => (
            "run_agent",
            false,
            json!({
                "member": member,
                "provider": provider,
                "session": session_choice(session),
                "args": args.len(),
                "prompt_len": prompt.as_ref().map(|p| p.chars().count()),
                "wrapper": !wrapper.is_empty(),
            }),
        ),
        Request::Reconcile => ("reconcile", false, json!({})),
        Request::RepairMember { member, env: _ } => {
            ("repair_member", false, json!({ "member": member }))
        }
        Request::AdoptWorktree { dir, name, join } => (
            "adopt_worktree",
            false,
            json!({ "dir": dir, "task": name, "join": join }),
        ),
        Request::Screen { id, from } => ("screen", true, json!({ "id": id, "from": from })),
        Request::WatchScreen { id, viewing } => (
            "watch_screen",
            true,
            json!({ "id": id, "viewing": viewing }),
        ),
        Request::Keys { id, data } => ("keys", true, json!({ "id": id, "bytes": data.len() })),
        Request::SendMessage { to, text, from } => (
            "send_message",
            false,
            json!({ "to": to, "from": from, "len": text.chars().count() }),
        ),
        Request::WaitInbox { agent_id } => ("wait_inbox", false, json!({ "agent": agent_id })),
        Request::WaitReply { id, timeout_secs } => (
            "wait_reply",
            false,
            json!({ "message": id, "timeout_s": timeout_secs, "wait": true }),
        ),
        Request::ListMessages { task } => ("list_messages", true, json!({ "task": task })),
        Request::GetOutcomes { members } => ("get_outcomes", true, json!({ "targets": members })),
        Request::WaitAgents {
            members,
            until,
            timeout_secs,
        } => (
            "wait_agents",
            false,
            json!({ "targets": members, "until": until, "timeout_s": timeout_secs, "wait": true }),
        ),
        Request::CreateMembers {
            task,
            members,
            source,
            join,
            env: _,
        } => (
            "create_members",
            false,
            json!({
                "task": task,
                "repos": members.iter().map(|m| &m.repo_root).collect::<Vec<_>>(),
                "source": source,
                "join": join,
            }),
        ),
        Request::GetBrief { task } => ("get_brief", true, json!({ "task": task })),
        Request::SetBrief { task, brief } => (
            "set_brief",
            false,
            json!({ "task": task, "local": brief.is_none() }),
        ),
        Request::MergeTasks { into, from } => {
            ("merge_tasks", false, json!({ "into": into, "from": from }))
        }
        Request::SplitMember { member, name } => (
            "split_member",
            false,
            json!({ "member": member, "task": name }),
        ),
        Request::WaitAnswer { agent_id, payload } => (
            "wait_answer",
            false,
            json!({ "agent": agent_id, "tool": tool_name(Some(payload)) }),
        ),
        Request::Answer { agent, ask, answer } => (
            "answer",
            false,
            json!({ "agent": agent, "ask": ask, "answer": answer_kind(answer) }),
        ),
        Request::CreateChat { dir, name, session } => (
            "create_chat",
            false,
            json!({ "dir": dir, "name": name, "session": session }),
        ),
        Request::ListChats => ("list_chats", true, json!({})),
        Request::GetChat { chat } => ("get_chat", true, json!({ "chat": chat })),
        Request::RenameChat { chat, name } => {
            ("rename_chat", false, json!({ "chat": chat, "name": name }))
        }
        Request::KeepChat { chat, keep } => {
            ("keep_chat", false, json!({ "chat": chat, "keep": keep }))
        }
        Request::CloseChat { chat } => ("close_chat", false, json!({ "chat": chat })),
        Request::ChatHistory { dir } => ("chat_history", false, json!({ "dir": dir })),
        Request::ResolveTarget { target, from } => (
            "resolve_target",
            true,
            json!({ "target": target, "from": from }),
        ),
        Request::OpenShell {
            on,
            env: _,
            wrapper,
        } => {
            let (member, dir) = match on {
                ShellPlace::Member(target) => (Some(target), None),
                ShellPlace::Checkout(dir) => (None, Some(dir)),
            };
            (
                "open_shell",
                false,
                json!({ "member": member, "dir": dir, "wrapper": !wrapper.is_empty() }),
            )
        }
        Request::ListShells => ("list_shells", true, json!({})),
        Request::CloseShell { id } => ("close_shell", false, json!({ "id": id })),
    };
    let fields = match fields {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    Described {
        kind,
        quiet,
        fields,
    }
}

/// The tool a hook's payload is about (`tool_name`), if any.
fn tool_name(payload: Option<&Value>) -> Option<String> {
    let name = payload?.get("tool_name")?.as_str()?;
    Some(cut(name, 100))
}

fn session_choice(session: &SessionChoice) -> &'static str {
    match session {
        SessionChoice::Current => "current",
        SessionChoice::New => "new",
        SessionChoice::Id(_) => "id",
    }
}

/// What an answer says, without its text.
pub fn answer_kind(answer: &AskAnswer) -> &'static str {
    match answer {
        AskAnswer::Allow => "allow",
        AskAnswer::Deny { reason: None } => "deny",
        AskAnswer::Deny { reason: Some(_) } => "deny_with_reason",
        AskAnswer::Answers { .. } => "answers",
    }
}

/// The kind of a reply, and its error's message.
pub fn reply(response: &Response) -> (&'static str, Option<String>) {
    let kind = match response {
        Response::Spawned { .. } => "spawned",
        Response::Agents { .. } => "agents",
        Response::Attached => "attached",
        Response::Scrollback { .. } => "scrollback",
        Response::Ok => "ok",
        Response::Error { message } => return ("error", Some(cut(message, 500))),
        Response::Hello { .. } => "hello",
        Response::Tasks { .. } => "tasks",
        Response::Task { .. } => "task",
        Response::Member { .. } => "member",
        Response::Dropped { .. } => "dropped",
        Response::DropRefused { .. } => "drop_refused",
        Response::Running { .. } => "running",
        Response::Screen { .. } => "screen",
        Response::Sent { .. } => "sent",
        Response::Inbox { .. } => "inbox",
        Response::Reply { .. } => "reply",
        Response::Messages { .. } => "messages",
        Response::HookContext { .. } => "hook_context",
        Response::Outcomes { .. } => "outcomes",
        Response::Brief { .. } => "brief",
        Response::Moved { .. } => "moved",
        Response::HookOutput { .. } => "hook_output",
        Response::Chats { .. } => "chats",
        Response::Chat { .. } => "chat",
        Response::Sessions { .. } => "sessions",
        Response::Shell { .. } => "shell",
        Response::Shells { .. } => "shells",
    };
    (kind, None)
}

/// The quiet requests of a connection, by kind: how many, how long in all
/// and at most, how many failed. Written as `conn.stats` every
/// [`ConnStats::PERIOD`] it is used, and when it closes.
pub struct ConnStats {
    kinds: BTreeMap<&'static str, KindStats>,
    since: Instant,
}

#[derive(Default)]
struct KindStats {
    n: u64,
    ms: f64,
    max_ms: f64,
    errors: u64,
}

impl Default for ConnStats {
    fn default() -> Self {
        Self {
            kinds: BTreeMap::new(),
            since: Instant::now(),
        }
    }
}

impl ConnStats {
    pub const PERIOD: Duration = Duration::from_secs(300);

    pub fn add(&mut self, kind: &'static str, took: Duration, failed: bool) {
        let stats = self.kinds.entry(kind).or_default();
        let took = ms(took);
        stats.n += 1;
        stats.ms += took;
        stats.max_ms = stats.max_ms.max(took);
        stats.errors += u64::from(failed);
    }

    /// Whether it is time to write them.
    pub fn due(&self) -> bool {
        self.since.elapsed() >= Self::PERIOD
    }

    /// What was counted since the last time, as fields (`None`: nothing),
    /// and start counting again.
    pub fn take(&mut self) -> Option<Value> {
        let period = ms_since(self.since);
        self.since = Instant::now();
        if self.kinds.is_empty() {
            return None;
        }
        let requests: Map<String, Value> = std::mem::take(&mut self.kinds)
            .into_iter()
            .map(|(kind, s)| {
                let stats = json!({
                    "n": s.n,
                    "ms": (s.ms * 100.0).round() / 100.0,
                    "max_ms": s.max_ms,
                    "errors": s.errors,
                });
                (kind.to_string(), stats)
            })
            .collect();
        Some(json!({ "period_ms": period, "requests": requests }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Value {
        serde_json::from_str(line).unwrap()
    }

    #[test]
    fn a_line_starts_with_its_header_and_drops_nulls() {
        let log = Activity::in_memory("daemon");
        log.record(
            "request",
            json!({ "req": "kill", "id": "m-1", "none": null }),
        );
        let lines = log.lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("{\"ts\":"), "{}", lines[0]);
        let v = parse(&lines[0]);
        assert_eq!(v["src"], "daemon");
        assert_eq!(v["pid"], std::process::id());
        assert_eq!(v["kind"], "request");
        assert_eq!(v["req"], "kill");
        assert!(v.get("none").is_none());
        assert!(v["ts"].as_i64().unwrap() > 1_700_000_000_000);
    }

    #[test]
    fn off_writes_nothing() {
        let log = Activity::off();
        log.record("x", json!({}));
        assert!(!log.is_on());
        assert!(log.lines().is_empty());
    }

    #[test]
    fn lines_go_to_the_file_of_the_day() {
        let dir = tempfile::tempdir().unwrap();
        let log = Activity::to_dir(dir.path().join("log"), "cli");
        log.record("cli", json!({ "cmd": "ps" }));
        log.record("cli", json!({ "cmd": "ls" }));
        let day = Stamp::of(SystemTime::now()).day();
        let text =
            std::fs::read_to_string(dir.path().join("log").join(format!("{day}.jsonl"))).unwrap();
        let lines: Vec<_> = text.lines().map(parse).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["cmd"], "ls");
        // Removed meanwhile: made again.
        std::fs::remove_dir_all(dir.path().join("log")).unwrap();
        log.record("cli", json!({ "cmd": "x" }));
        log.record("cli", json!({ "cmd": "y" }));
        let text =
            std::fs::read_to_string(dir.path().join("log").join(format!("{day}.jsonl"))).unwrap();
        assert!(text.contains("\"y\""), "{text}");
    }

    #[test]
    fn dates_round_trip() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil(days_from_civil(2026, 11, 28)), (2026, 11, 28));
        assert_eq!(civil(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(
            days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 28),
            2
        );
    }

    #[test]
    fn readable_time_has_milliseconds_and_offset() {
        let stamp = Stamp {
            millis: days_from_civil(2026, 11, 28) * 86_400_000 + 3_723_045,
            offset: 3600,
        };
        assert_eq!(stamp.readable(), "2026-11-28T02:02:03.045+01:00");
        assert_eq!(stamp.day(), "2026-11-28");
        let west = Stamp {
            millis: days_from_civil(2026, 11, 28) * 86_400_000,
            offset: -5 * 3600 - 1800,
        };
        assert_eq!(west.readable(), "2026-11-27T18:30:00.000-05:30");
        assert_eq!(west.day(), "2026-11-27");
    }

    #[test]
    fn prune_removes_old_days_only() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "2026-10-01.jsonl",
            "2026-11-27.jsonl",
            "2026-11-28.jsonl",
            "notes.txt",
            "2026-1-1.jsonl",
        ] {
            std::fs::write(dir.path().join(name), "x").unwrap();
        }
        let today = days_from_civil(2026, 11, 28);
        assert_eq!(prune_before(dir.path(), today - 1), 1);
        let mut left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "2026-1-1.jsonl",
                "2026-11-27.jsonl",
                "2026-11-28.jsonl",
                "notes.txt"
            ]
        );
    }

    #[test]
    fn settings_default_and_from_the_config() {
        assert_eq!(parse_settings(""), Settings::default());
        assert_eq!(
            parse_settings("[log]\nenabled = false\nkeep_days = 7\n"),
            Settings {
                enabled: false,
                keep_days: 7
            }
        );
        assert_eq!(parse_settings("[log]\nkeep_days = 0\n").keep_days, 1);
        // What cannot be read: the defaults.
        assert_eq!(
            parse_settings("[log]\nenabled = 'no'\n"),
            Settings::default()
        );
        assert_eq!(parse_settings("not toml ["), Settings::default());
    }

    #[test]
    fn cut_keeps_whole_characters() {
        assert_eq!(cut("abc", 5), "abc");
        assert_eq!(cut("abcdef", 3), "abc…");
        assert_eq!(cut("ééé", 2), "éé…");
    }

    #[test]
    fn requests_say_lengths_never_text() {
        let secret = "SECRET-TEXT";
        let env = tam_proto::AgentEnv(BTreeMap::from([("SECRET_VAR".into(), secret.into())]));
        for request in [
            Request::SendMessage {
                to: "t".into(),
                text: secret.into(),
                from: None,
            },
            Request::Keys {
                id: "m-1".into(),
                data: secret.into(),
            },
            Request::RunAgent {
                member: "m-1".into(),
                provider: None,
                session: SessionChoice::New,
                args: vec![],
                prompt: Some(secret.into()),
                env: env.clone(),
                wrapper: vec![secret.into()],
            },
            Request::OpenShell {
                on: ShellPlace::Member("t".into()),
                env,
                wrapper: vec![secret.into()],
            },
            Request::HookEvent {
                agent_id: "m-1".into(),
                event: "PreToolUse".into(),
                payload: Some(json!({ "tool_name": "Bash", "tool_input": secret })),
            },
            Request::Answer {
                agent: "m-1".into(),
                ask: "ask-1".into(),
                answer: AskAnswer::Deny {
                    reason: Some(secret.into()),
                },
            },
            Request::SetBrief {
                task: "t".into(),
                brief: Some(secret.into()),
            },
            Request::CreateMembers {
                task: "t".into(),
                members: vec![],
                source: None,
                join: false,
                env: BTreeMap::from([("SECRET_VAR".into(), secret.into())]),
            },
        ] {
            let described = describe(&request);
            let text = Value::Object(described.fields).to_string();
            assert!(!text.contains("SECRET"), "{}: {text}", described.kind);
        }
        let hook = describe(&Request::HookEvent {
            agent_id: "m-1".into(),
            event: "PreToolUse".into(),
            payload: Some(json!({ "tool_name": "Bash" })),
        });
        assert_eq!(hook.fields["tool"], "Bash");
        assert!(hook.quiet);
    }

    #[test]
    fn conn_stats_sum_by_kind() {
        let mut stats = ConnStats::default();
        assert!(stats.take().is_none());
        stats.add("screen", Duration::from_millis(2), false);
        stats.add("screen", Duration::from_millis(4), true);
        stats.add("keys", Duration::from_millis(1), false);
        let taken = stats.take().unwrap();
        assert_eq!(taken["requests"]["screen"]["n"], 2);
        assert_eq!(taken["requests"]["screen"]["ms"], 6.0);
        assert_eq!(taken["requests"]["screen"]["max_ms"], 4.0);
        assert_eq!(taken["requests"]["screen"]["errors"], 1);
        assert_eq!(taken["requests"]["keys"]["errors"], 0);
        assert!(stats.take().is_none());
    }
}
