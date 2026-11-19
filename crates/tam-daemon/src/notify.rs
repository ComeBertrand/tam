use std::process::Command;

use serde::Deserialize;
use tam_proto::{AgentDetail, AgentState};
use tracing::{error, info};

/// Notification configuration from config.toml.
#[derive(Debug, Clone)]
pub struct NotifyConfig {
    /// Command template to run on state transitions, run with `sh -c`.
    /// Placeholders (escaped for the quoting they sit in):
    /// - `{task}`, `{id}` (agent id), `{state}` (alias `{status}`), `{old_state}`
    /// - `{kind}`: `question`, `permission`, `done`, `error`
    /// - `{message}`: question or permission asked, last response, or error
    /// - `{tool}`: tool waiting for permission; `{error}`: error kind
    ///
    /// Not fired while a viewer is attached to the agent.
    pub command: String,
    /// Which states trigger notifications.
    pub on_states: Vec<AgentState>,
}

/// Raw TOML shape for the [notify] section.
#[derive(Debug, Deserialize, Default)]
struct ConfigFile {
    notify: Option<NotifySection>,
}

#[derive(Debug, Deserialize)]
struct NotifySection {
    command: Option<String>,
    on_states: Option<Vec<String>>,
}

/// Load notify config from the standard config path.
/// Returns None if no [notify] section or no command configured.
pub fn load_notify_config() -> Option<NotifyConfig> {
    let config_dir = dirs::config_dir()?;

    let config_path = config_dir.join("tam").join("config.toml");

    if !config_path.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&config_path).ok()?;
    let file: ConfigFile = toml::from_str(&content).ok()?;
    let section = file.notify?;
    let command = section.command?;

    let on_states = section
        .on_states
        .unwrap_or_else(|| {
            ["ready", "blocked", "question", "error"]
                .map(String::from)
                .to_vec()
        })
        .into_iter()
        .filter_map(|s| parse_state(&s))
        .collect();

    Some(NotifyConfig { command, on_states })
}

fn parse_state(s: &str) -> Option<AgentState> {
    match s {
        "working" => Some(AgentState::Working),
        "blocked" => Some(AgentState::Blocked),
        // `input`: its name before, in configs written then.
        "ready" | "input" => Some(AgentState::Ready),
        "idle" => Some(AgentState::Idle),
        "question" => Some(AgentState::Question),
        "error" => Some(AgentState::Error),
        _ => None,
    }
}

/// Shell-quote a string for safe interpolation into `sh -c` commands.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.bytes().all(|b| {
        matches!(
            b,
            b'a'..=b'z'
                | b'A'..=b'Z'
                | b'0'..=b'9'
                | b'-'
                | b'_'
                | b'/'
                | b'.'
                | b':'
                | b'@'
                | b'='
                | b'+'
                | b','
        )
    }) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A state change to notify about, with what the agent reported.
pub struct Notification<'a> {
    /// Agent id.
    pub id: &'a str,
    /// Task the agent runs for (the agent id when it has none).
    pub task: &'a str,
    pub old: AgentState,
    pub new: AgentState,
    pub detail: &'a AgentDetail,
    /// Clients attached to the agent: the user is already looking at it.
    pub viewers: usize,
}

impl Notification<'_> {
    /// What the user is being told about: `question`, `permission`, `done`,
    /// `error` (or the state name for the other states).
    fn kind(&self) -> String {
        match self.new {
            AgentState::Question => "question".into(),
            AgentState::Blocked => "permission".into(),
            AgentState::Ready => "done".into(),
            AgentState::Error => "error".into(),
            other => other.to_string(),
        }
    }

    /// One-line text: the question or permission asked, the last response,
    /// or the error.
    fn message(&self) -> String {
        let d = self.detail;
        let text = match self.new {
            AgentState::Question | AgentState::Blocked => {
                d.pending.as_ref().map(|p| p.summary.clone())
            }
            AgentState::Ready => d.last_message.clone(),
            AgentState::Error => d
                .error
                .as_ref()
                .map(|e| e.message.clone().unwrap_or_else(|| e.kind.clone())),
            _ => None,
        };
        let text = text.unwrap_or_default();
        let line = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        match line.char_indices().nth(MAX_MESSAGE_CHARS) {
            Some((cut, _)) => format!("{}…", &line[..cut]),
            None => line.to_string(),
        }
    }

    fn tool(&self) -> String {
        match self.new {
            AgentState::Question | AgentState::Blocked => self
                .detail
                .pending
                .as_ref()
                .and_then(|p| p.tool.clone())
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn error(&self) -> String {
        match self.new {
            AgentState::Error => self
                .detail
                .error
                .as_ref()
                .map(|e| e.kind.clone())
                .unwrap_or_default(),
            _ => String::new(),
        }
    }
}

/// Longest `{message}` in a notification.
const MAX_MESSAGE_CHARS: usize = 200;

/// The command to run for a notification, or `None` if it shouldn't fire:
/// the new state isn't one of `on_states`, or a viewer is attached.
pub fn command_for(config: &NotifyConfig, n: &Notification) -> Option<String> {
    if !config.on_states.contains(&n.new) || n.viewers > 0 {
        return None;
    }
    let new = n.new.to_string();
    let values = [
        ("task", n.task.to_string()),
        ("id", n.id.to_string()),
        ("state", new.clone()),
        ("status", new),
        ("old_state", n.old.to_string()),
        ("kind", n.kind()),
        ("message", n.message()),
        ("tool", n.tool()),
        ("error", n.error()),
    ];
    Some(expand(&config.command, &values))
}

/// Fire the notification command for a state change, if it matches.
pub fn fire_if_matching(config: &NotifyConfig, n: &Notification) {
    let Some(cmd) = command_for(config, n) else {
        return;
    };

    info!(id = %n.id, state = %n.new, "firing notification");

    // Fire and forget — don't block the state monitor
    std::thread::spawn(move || {
        if let Err(e) = Command::new("sh").arg("-c").arg(&cmd).status() {
            error!(cmd = %cmd, error = %e, "notification command failed");
        }
    });
}

/// Replace `{name}` placeholders in a `sh -c` template, escaping each value
/// for where it sits: inside single quotes, inside double quotes, or bare.
/// Unknown `{…}` are left as they are.
fn expand(template: &str, values: &[(&str, String)]) -> String {
    #[derive(Clone, Copy, PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut out = String::with_capacity(template.len());
    let mut quote = Quote::None;
    let mut rest = template;
    while let Some(c) = rest.chars().next() {
        if c == '{' {
            let value = rest.find('}').and_then(|end| {
                let name = &rest[1..end];
                values
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| (end, v))
            });
            if let Some((end, value)) = value {
                out.push_str(&match quote {
                    Quote::None => shell_quote(value),
                    // Close the quote, add an escaped quote, reopen.
                    Quote::Single => value.replace('\'', "'\\''"),
                    Quote::Double => value
                        .chars()
                        .flat_map(|c| {
                            let escape = matches!(c, '"' | '\\' | '$' | '`');
                            escape.then_some('\\').into_iter().chain([c])
                        })
                        .collect(),
                });
                rest = &rest[end + 1..];
                continue;
            }
        }
        match (quote, c) {
            (Quote::None, '\'') => quote = Quote::Single,
            (Quote::None, '"') => quote = Quote::Double,
            (Quote::Single, '\'') | (Quote::Double, '"') => quote = Quote::None,
            // An escaped character keeps the quoting state
            (Quote::None | Quote::Double, '\\') => {
                out.push(c);
                rest = &rest[1..];
                match rest.chars().next() {
                    Some(next) => {
                        out.push(next);
                        rest = &rest[next.len_utf8()..];
                    }
                    None => break,
                }
                continue;
            }
            _ => {}
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tam_proto::{AgentError, Pending};

    #[test]
    fn parse_states() {
        assert_eq!(parse_state("working"), Some(AgentState::Working));
        assert_eq!(parse_state("blocked"), Some(AgentState::Blocked));
        assert_eq!(parse_state("ready"), Some(AgentState::Ready));
        assert_eq!(parse_state("idle"), Some(AgentState::Idle));
        assert_eq!(parse_state("unknown"), None);
    }

    fn n<'a>(new: AgentState, detail: &'a AgentDetail) -> Notification<'a> {
        Notification {
            id: "agent-1",
            task: "feat",
            old: AgentState::Working,
            new,
            detail,
            viewers: 0,
        }
    }

    fn config(command: &str) -> NotifyConfig {
        NotifyConfig {
            command: command.into(),
            on_states: vec![
                AgentState::Ready,
                AgentState::Blocked,
                AgentState::Question,
                AgentState::Error,
            ],
        }
    }

    /// Run a command through sh and return its stdout.
    fn sh(cmd: &str) -> String {
        let out = Command::new("sh").arg("-c").arg(cmd).output().unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn parse_new_states() {
        assert_eq!(parse_state("question"), Some(AgentState::Question));
        assert_eq!(parse_state("error"), Some(AgentState::Error));
    }

    #[test]
    fn fires_only_on_configured_states() {
        let d = AgentDetail::default();
        let mut cfg = config("echo {task} {state} {old_state}");
        cfg.on_states = vec![AgentState::Ready];
        assert_eq!(
            command_for(&cfg, &n(AgentState::Ready, &d)).as_deref(),
            Some("echo feat ready working")
        );
        assert!(command_for(&cfg, &n(AgentState::Working, &d)).is_none());
    }

    #[test]
    fn skipped_when_a_viewer_is_attached() {
        let d = AgentDetail::default();
        let mut notif = n(AgentState::Ready, &d);
        notif.viewers = 1;
        assert!(command_for(&config("echo"), &notif).is_none());
    }

    #[test]
    fn placeholders_for_each_kind() {
        let cfg = config("echo {kind}/{tool}/{error}/{message}/{id}/{status}");
        let mut d = AgentDetail {
            pending: Some(Pending {
                tool: Some("Bash".into()),
                summary: "touch x".into(),
            }),
            last_message: Some("All done.\nDetails…".into()),
            error: Some(AgentError {
                kind: "rate_limit".into(),
                message: Some("API Error: Rate limit reached".into()),
            }),
            ..Default::default()
        };
        let run = |state, d: &AgentDetail| sh(&command_for(&cfg, &n(state, d)).unwrap());
        assert_eq!(
            run(AgentState::Blocked, &d),
            "permission/Bash//touch x/agent-1/blocked\n"
        );
        assert_eq!(
            run(AgentState::Ready, &d),
            "done///All done./agent-1/ready\n"
        );
        assert_eq!(
            run(AgentState::Error, &d),
            "error//rate_limit/API Error: Rate limit reached/agent-1/error\n"
        );
        d.pending = Some(Pending {
            tool: Some("AskUserQuestion".into()),
            summary: "Tea or coffee?".into(),
        });
        assert_eq!(
            run(AgentState::Question, &d),
            "question/AskUserQuestion//Tea or coffee?/agent-1/question\n"
        );
    }

    #[test]
    fn values_are_escaped_for_their_quoting() {
        let nasty = r#"it's "$HOME" `id` \ $(echo pwned); a b"#;
        let d = AgentDetail {
            last_message: Some(nasty.into()),
            ..Default::default()
        };
        let notif = n(AgentState::Ready, &d);
        for template in [
            "printf '%s' {message}",
            "printf '%s' '{message}'",
            "printf '%s' \"{message}\"",
            "printf '%s' 'msg: {message}!'",
            "printf '%s' \"msg: {message}!\"",
        ] {
            let cmd = command_for(&config(template), &notif).unwrap();
            let expected = if template.contains("msg:") {
                format!("msg: {nasty}!")
            } else {
                nasty.to_string()
            };
            assert_eq!(sh(&cmd), expected, "template {template} -> {cmd}");
        }
    }

    #[test]
    fn user_style_template() {
        // Placeholders inside single quotes, as users tend to write them
        let d = AgentDetail::default();
        let cfg = config("printf '%s|%s' 'tam: {task}' '{status}'");
        assert_eq!(
            sh(&command_for(&cfg, &n(AgentState::Ready, &d)).unwrap()),
            "tam: feat|ready"
        );
    }

    #[test]
    fn unknown_placeholders_and_escapes_are_kept() {
        let d = AgentDetail::default();
        let cfg = config(r#"echo {nope} \' {task} "\"{task}""#);
        let cmd = command_for(&cfg, &n(AgentState::Ready, &d)).unwrap();
        assert_eq!(cmd, r#"echo {nope} \' feat "\"feat""#);
    }

    #[test]
    fn long_messages_are_cut() {
        let d = AgentDetail {
            last_message: Some("x".repeat(500)),
            ..Default::default()
        };
        let m = n(AgentState::Ready, &d).message();
        assert_eq!(m.chars().count(), MAX_MESSAGE_CHARS + 1);
    }
}
