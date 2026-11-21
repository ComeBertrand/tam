//! Autoclean (#65): a chat is closed by itself after days without a turn
//! (`[chat] autoclean` in the config, 3 days by default; `0` or `false`
//! turns it off). Its clock starts again at each turn (a prompt the user
//! typed, or an answer to what its agent waited on), when it is un-kept,
//! and starts at its creation. A kept chat is never closed.
//!
//! The daemon looks for chats past their time at start (no agent runs then)
//! and every [`INTERVAL`], and closes them as `CloseChat` does, if their
//! agent is not running or sits at the prompt with nobody attached or
//! typing: never while it works or waits on the user. Clients get the time
//! from which a chat is closed (`ChatInfo::closes_at`), not the setting.
//! The passes are at the end of `daemon.rs`.

use std::path::Path;
use std::time::Duration;

use tam_proto::AgentState;
use tracing::{info, warn};

/// `[chat] autoclean` when the config does not set it, in days.
pub const DEFAULT_DAYS: f64 = 3.0;

/// How often the daemon looks for chats past their time (and at start).
pub const INTERVAL: Duration = Duration::from_secs(60);

const DAY_SECS: f64 = 86_400.0;

/// What the config says about autoclean.
#[derive(Debug, Clone, PartialEq)]
pub enum Setting {
    /// Close a chat this long after its last turn.
    After(Duration),
    Off,
    /// The config cannot be read, or the value is not one: off (a typo next
    /// to `autoclean = false` must not start closing chats), said in the
    /// daemon's log.
    Invalid(String),
}

impl Setting {
    /// How long a chat may go without a turn; `None`: never closed.
    pub fn limit(&self) -> Option<Duration> {
        match self {
            Self::After(limit) => Some(*limit),
            Self::Off | Self::Invalid(_) => None,
        }
    }

    fn default_days() -> Self {
        Self::days(DEFAULT_DAYS)
    }

    fn days(days: f64) -> Self {
        if !days.is_finite() || days < 0.0 {
            return Self::Invalid(format!(
                "chat.autoclean is a number of days, false or true, not {days}"
            ));
        }
        if days == 0.0 {
            return Self::Off;
        }
        // Beyond what a `Duration` holds: never, as good as.
        Self::After(Duration::try_from_secs_f64(days * DAY_SECS).unwrap_or(Duration::MAX))
    }
}

/// The setting in the text of a config file.
pub fn parse(text: &str) -> Setting {
    let config: toml::Table = match toml::from_str(text) {
        Ok(config) => config,
        Err(e) => return Setting::Invalid(format!("the config cannot be read: {e}")),
    };
    let Some(chat) = config.get("chat") else {
        return Setting::default_days();
    };
    let Some(chat) = chat.as_table() else {
        return Setting::Invalid("[chat] is not a table".into());
    };
    match chat.get("autoclean") {
        None | Some(toml::Value::Boolean(true)) => Setting::default_days(),
        Some(toml::Value::Boolean(false)) => Setting::Off,
        Some(toml::Value::Integer(days)) => Setting::days(*days as f64),
        Some(toml::Value::Float(days)) => Setting::days(*days),
        Some(other) => Setting::Invalid(format!(
            "chat.autoclean is a number of days, false or true, not {other}"
        )),
    }
}

/// The setting in the config file at `path` (none: the default).
fn load_from(path: &Path) -> Setting {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Setting::default_days(),
        Err(e) => Setting::Invalid(format!("{}: {e}", path.display())),
    }
}

/// The setting in tam's config file
/// ([`tam_worktree::config::config_path`], where the rest of tam reads it).
pub fn load() -> Setting {
    match tam_worktree::config::config_path() {
        Some(path) => load_from(&path),
        None => Setting::default_days(),
    }
}

/// Where the daemon takes the setting from: the config, read again at every
/// pass (a change applies without a restart), or one given (tests).
#[derive(Debug, Clone, Default)]
pub struct Source {
    given: Option<Setting>,
    /// The setting of the last pass: a change is logged once.
    last: Option<Setting>,
}

impl Source {
    /// Always `setting`, whatever the config says.
    pub fn given(setting: Setting) -> Self {
        Self {
            given: Some(setting),
            last: None,
        }
    }

    /// How long a chat may go without a turn now (`None`: never closed).
    pub fn limit(&mut self) -> Option<Duration> {
        let setting = self.given.clone().unwrap_or_else(load);
        if self.last.as_ref() != Some(&setting) {
            match &setting {
                Setting::After(limit) => info!(
                    days = limit.as_secs_f64() / DAY_SECS,
                    "chats are closed after this many days without a turn"
                ),
                Setting::Off => info!("chats are never closed for want of turns"),
                Setting::Invalid(e) => warn!("chats are never closed for want of turns: {e}"),
            }
            self.last = Some(setting.clone());
        }
        setting.limit()
    }
}

/// Whether a chat past its time may be closed now, given its agent's state
/// and number of viewers (attached, or typing in a dashboard's pane) if one
/// runs: none, or one at the prompt that nobody is attached to or typing in.
pub fn may_close(agent: Option<(AgentState, usize)>) -> bool {
    match agent {
        None => true,
        Some((state, viewers)) => {
            viewers == 0
                && matches!(
                    state,
                    AgentState::Ready | AgentState::Idle | AgentState::Error
                )
        }
    }
}

/// Whether an agent in `state` waits on the user (a permission, a question):
/// only the user ends that, so a chat's agent leaving it is a turn.
pub fn waits_on_user(state: AgentState) -> bool {
    matches!(state, AgentState::Blocked | AgentState::Question)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn days(n: f64) -> Setting {
        Setting::After(Duration::from_secs_f64(n * DAY_SECS))
    }

    #[test]
    fn three_days_unless_the_config_says_otherwise() {
        assert_eq!(parse(""), days(3.0));
        assert_eq!(parse("[notify]\ncommand = \"x\"\n"), days(3.0));
        assert_eq!(parse("[chat]\n"), days(3.0));
        assert_eq!(parse("[chat]\nautoclean = true\n"), days(3.0));
        assert_eq!(parse("[chat]\nautoclean = 7\n"), days(7.0));
        assert_eq!(parse("[chat]\nautoclean = 0.5\n"), days(0.5));
        assert_eq!(days(0.5).limit(), Some(Duration::from_secs(43_200)));
        assert_eq!(
            parse("[chat]\nautoclean = 1e300\n").limit(),
            Some(Duration::MAX)
        );
    }

    #[test]
    fn zero_or_false_turns_it_off() {
        for text in [
            "[chat]\nautoclean = 0\n",
            "[chat]\nautoclean = 0.0\n",
            "[chat]\nautoclean = false\n",
        ] {
            assert_eq!(parse(text), Setting::Off, "{text}");
            assert_eq!(parse(text).limit(), None);
        }
    }

    #[test]
    fn what_cannot_be_read_turns_it_off() {
        for text in [
            "[chat]\nautoclean = -1\n",
            "[chat]\nautoclean = \"never\"\n",
            "[chat]\nautoclean = nan\n",
            "chat = 3\n",
            // A typo elsewhere: the file is not TOML.
            "[chat]\nautoclean = false\n[tui\n",
        ] {
            let setting = parse(text);
            assert!(
                matches!(setting, Setting::Invalid(_)),
                "{text}: {setting:?}"
            );
            assert_eq!(setting.limit(), None);
        }
    }

    #[test]
    fn the_file_is_read_at_each_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(load_from(&path), days(3.0));
        std::fs::write(&path, "[chat]\nautoclean = 1\n").unwrap();
        assert_eq!(load_from(&path), days(1.0));
        std::fs::write(&path, "[chat]\nautoclean = false\n").unwrap();
        assert_eq!(load_from(&path), Setting::Off);
        // A directory where the file should be: unreadable.
        let unreadable = dir.path().join("dir");
        std::fs::create_dir(&unreadable).unwrap();
        assert!(matches!(load_from(&unreadable), Setting::Invalid(_)));

        let mut given = Source::given(Setting::Off);
        assert_eq!(given.limit(), None);
        let mut given = Source::given(days(2.0));
        assert_eq!(given.limit(), Some(Duration::from_secs(172_800)));
    }

    #[test]
    fn only_an_agent_at_the_prompt_nobody_views_is_closed() {
        use AgentState::*;
        assert!(may_close(None));
        for state in [Ready, Idle, Error] {
            assert!(may_close(Some((state, 0))), "{state}");
            assert!(!may_close(Some((state, 1))), "{state} viewed");
        }
        for state in [Working, Blocked, Question] {
            assert!(!may_close(Some((state, 0))), "{state}");
        }
        assert!(waits_on_user(Blocked) && waits_on_user(Question));
        for state in [Working, Ready, Idle, Error] {
            assert!(!waits_on_user(state), "{state}");
        }
    }
}
