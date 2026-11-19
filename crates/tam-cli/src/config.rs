use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Raw TOML shape — all fields optional.
#[derive(Debug, Deserialize, Default)]
pub struct ConfigFile {
    pub spawn: Option<SpawnConfig>,
    pub session: Option<SessionConfig>,
    pub tui: Option<TuiConfig>,
}

#[derive(Debug, Deserialize)]
pub struct SpawnConfig {
    #[serde(alias = "agent")]
    pub default_agent: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SessionConfig {
    pub finder: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TuiConfig {
    #[serde(default)]
    pub commands: Vec<CustomCommand>,
    /// `dark` (24-bit palette) or `ansi` (the terminal's 16 colours).
    pub theme: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CustomCommand {
    pub name: String,
    pub key: String,
    pub command: String,
}

impl CustomCommand {
    pub fn key_char(&self) -> char {
        self.key.chars().next().unwrap_or('\0')
    }
}

/// Resolved config with defaults applied.
#[derive(Debug)]
pub struct Config {
    pub default_agent: String,
    pub finder: Option<String>,
    pub commands: Vec<CustomCommand>,
    pub theme: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_agent: "claude".into(),
            finder: None,
            commands: Vec::new(),
            theme: None,
        }
    }
}

pub fn parse_config(toml_str: &str) -> Result<Config> {
    let file: ConfigFile = toml::from_str(toml_str)?;
    let defaults = Config::default();

    let default_agent = file
        .spawn
        .as_ref()
        .and_then(|s| s.default_agent.clone())
        .unwrap_or(defaults.default_agent);

    let finder = file.session.as_ref().and_then(|s| s.finder.clone());

    let commands = file
        .tui
        .as_ref()
        .map(|t| t.commands.clone())
        .unwrap_or_default();
    validate_custom_commands(&commands)?;

    let theme = file.tui.as_ref().and_then(|t| t.theme.clone());
    if let Some(theme) = &theme {
        anyhow::ensure!(
            matches!(theme.as_str(), "dark" | "ansi"),
            "tui.theme is 'dark' or 'ansi', not '{theme}'"
        );
    }

    Ok(Config {
        default_agent,
        finder,
        commands,
        theme,
    })
}

pub fn load_config() -> Result<Config> {
    let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("/"));

    let tam_path = config_dir.join("tam").join("config.toml");

    if tam_path.exists() {
        let content = std::fs::read_to_string(&tam_path)?;
        parse_config(&content)
    } else {
        Ok(Config::default())
    }
}

pub(crate) fn shell_quote(s: &str) -> String {
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

pub fn run_custom_command(
    template: &str,
    task: &str,
    dir: &std::path::Path,
    provider: &str,
) -> Result<()> {
    let dir_str = dir.to_string_lossy();
    let cmd = template
        .replace("{task}", &shell_quote(task))
        .replace("{id}", &shell_quote(task))
        .replace("{dir}", &shell_quote(&dir_str))
        .replace("{provider}", &shell_quote(provider));

    unsafe {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .pre_exec(|| {
                nix::unistd::setsid().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                Ok(())
            })
            .spawn()
            .with_context(|| format!("failed to run command: {cmd}"))?;
    }
    Ok(())
}

const RESERVED_KEYS: &[char] = &[
    'q', 'j', 'k', 'h', 'l', 'n', 'r', 'R', 's', 'd', '/', 'g', 'G', 't', '?', 'a', 'm',
];

fn validate_custom_commands(commands: &[CustomCommand]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for cmd in commands {
        if cmd.key.chars().count() != 1 {
            anyhow::bail!(
                "custom command '{}': key must be a single character, got '{}'",
                cmd.name,
                cmd.key
            );
        }
        let c = cmd.key_char();
        if RESERVED_KEYS.contains(&c) {
            anyhow::bail!(
                "custom command '{}': key '{}' is reserved by a built-in action",
                cmd.name,
                cmd.key
            );
        }
        if !seen.insert(c) {
            anyhow::bail!("custom command '{}': duplicate key '{}'", cmd.name, cmd.key);
        }
    }
    Ok(())
}

pub const KNOWN_PROVIDERS: &[&str] = &["claude", "codex"];

pub fn validate_provider(name: &str) -> anyhow::Result<()> {
    if KNOWN_PROVIDERS.contains(&name) {
        Ok(())
    } else {
        anyhow::bail!(
            "unknown agent '{}'. Known agents: {}",
            name,
            KNOWN_PROVIDERS.join(", ")
        );
    }
}

/// Remove the hooks older versions of `tam init` wrote into the agent's
/// global settings. tam now injects its hooks into each agent it spawns, so
/// the global ones only fired for every session outside tam too.
pub fn remove_legacy_hooks(agent: &str) -> Result<()> {
    match agent {
        "claude" => {
            let settings_path = dirs::home_dir()
                .context("cannot determine home directory")?
                .join(".claude")
                .join("settings.json");
            remove_legacy_claude_hooks(&settings_path)
        }
        _ => anyhow::bail!("init not supported for agent '{agent}'"),
    }
}

fn remove_legacy_claude_hooks(settings_path: &Path) -> Result<()> {
    println!("Claude hooks are now injected into each agent tam spawns; nothing to install.");
    if !settings_path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(settings_path)
        .with_context(|| format!("failed to read {}", settings_path.display()))?;
    let mut settings: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", settings_path.display()))?;

    let removed = strip_legacy_hooks(&mut settings);
    if removed.is_empty() {
        println!("No old tam hooks in {}.", settings_path.display());
        return Ok(());
    }
    let formatted = serde_json::to_string_pretty(&settings)?;
    std::fs::write(settings_path, formatted.as_bytes())
        .with_context(|| format!("failed to write {}", settings_path.display()))?;
    println!(
        "Removed old tam hooks from {}: {}",
        settings_path.display(),
        removed.join(", ")
    );
    Ok(())
}

/// Whether a hook command is one written by an older `tam init` (or zinc).
fn is_legacy_hook_command(command: &str) -> bool {
    let mut words = command.split_whitespace();
    let program = words.next().unwrap_or("");
    let program = program.rsplit('/').next().unwrap_or(program);
    matches!(program, "tam" | "zinc")
        && words.next() == Some("hook-notify")
        && words.next() == Some("--event")
}

/// Remove legacy tam hook handlers from a Claude settings object, dropping
/// matcher groups and events left empty. Returns the removed events.
fn strip_legacy_hooks(settings: &mut serde_json::Value) -> Vec<String> {
    let mut removed = Vec::new();
    let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return removed;
    };
    for (event, groups) in hooks.iter_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        groups.retain_mut(|group| {
            let Some(handlers) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
                return true;
            };
            let before = handlers.len();
            handlers.retain(|h| !h["command"].as_str().is_some_and(is_legacy_hook_command));
            if handlers.len() == before {
                return true;
            }
            let label = match group.get("matcher").and_then(|m| m.as_str()) {
                Some(m) if !m.is_empty() => format!("{event}({m})"),
                _ => event.clone(),
            };
            removed.push(label);
            !group["hooks"].as_array().is_some_and(|h| h.is_empty())
        });
    }
    hooks.retain(|_, groups| !groups.as_array().is_some_and(|g| g.is_empty()));
    if hooks.is_empty() {
        if let Some(obj) = settings.as_object_mut() {
            obj.remove("hooks");
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let config = Config::default();
        assert_eq!(config.default_agent, "claude");
    }

    #[test]
    fn retired_daemon_scrollback_setting_is_ignored() {
        // Its byte buffer is gone: the daemon keeps the agent's terminal.
        let config = parse_config("[daemon]\nscrollback = 1048576\n").unwrap();
        assert_eq!(config.default_agent, "claude");
    }

    #[test]
    fn parse_empty_toml() {
        let config = parse_config("").unwrap();
        assert_eq!(config.default_agent, "claude");
    }

    #[test]
    fn parse_spawn_section() {
        let toml = r#"
[spawn]
default_agent = "codex"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.default_agent, "codex");
    }

    #[test]
    fn parse_session_finder() {
        let toml = r#"
[session]
finder = "fzf"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.finder.unwrap(), "fzf");
    }

    #[test]
    fn run_and_reply_keys_are_reserved() {
        for key in ["r", "R", "a", "m"] {
            let toml =
                format!("[[tui.commands]]\nname = \"x\"\nkey = \"{key}\"\ncommand = \"true\"\n");
            let err = parse_config(&toml).unwrap_err().to_string();
            assert!(err.contains("reserved"), "{key}: {err}");
        }
    }

    #[test]
    fn validate_known_provider() {
        assert!(validate_provider("claude").is_ok());
    }

    #[test]
    fn validate_unknown_provider() {
        let err = validate_provider("bash").unwrap_err();
        assert!(err.to_string().contains("unknown agent 'bash'"));
    }

    #[test]
    fn shell_quote_safe_string() {
        assert_eq!(shell_quote("/tmp/foo-bar"), "/tmp/foo-bar");
    }

    #[test]
    fn shell_quote_spaces() {
        assert_eq!(shell_quote("/tmp/my project"), "'/tmp/my project'");
    }

    #[test]
    fn shell_quote_injection() {
        assert_eq!(shell_quote("/tmp/foo; rm -rf /"), "'/tmp/foo; rm -rf /'");
    }

    #[test]
    fn legacy_hook_commands() {
        assert!(is_legacy_hook_command("tam hook-notify --event stop"));
        assert!(is_legacy_hook_command("zinc hook-notify --event stop"));
        assert!(is_legacy_hook_command(
            "/usr/bin/tam hook-notify --event notification:idle_prompt"
        ));
        assert!(!is_legacy_hook_command(
            "'/nix/store/x/bin/tam' hook-notify"
        ));
        assert!(!is_legacy_hook_command("tam hook-notify"));
        assert!(!is_legacy_hook_command("notify-send done"));
    }

    #[test]
    fn strips_only_legacy_hooks() {
        let legacy = |e: &str| serde_json::json!({"type": "command", "command": format!("tam hook-notify --event {e}"), "timeout": 5});
        let mine = serde_json::json!({"type": "command", "command": "notify-send stop"});
        let mut settings = serde_json::json!({
            "model": "opus",
            "hooks": {
                "UserPromptSubmit": [{"hooks": [legacy("user_prompt_submit")]}],
                "Stop": [{"hooks": [legacy("stop"), mine.clone()]}],
                "Notification": [
                    {"matcher": "idle_prompt", "hooks": [legacy("notification:idle_prompt")]},
                    {"matcher": "permission_prompt", "hooks": [legacy("notification:permission_prompt")]}
                ],
                "PreToolUse": [{"matcher": "Bash", "hooks": [mine.clone()]}]
            }
        });
        let removed = strip_legacy_hooks(&mut settings);
        assert_eq!(
            removed,
            vec![
                "Notification(idle_prompt)",
                "Notification(permission_prompt)",
                "Stop",
                "UserPromptSubmit"
            ]
        );
        assert_eq!(
            settings,
            serde_json::json!({
                "model": "opus",
                "hooks": {
                    "Stop": [{"hooks": [mine.clone()]}],
                    "PreToolUse": [{"matcher": "Bash", "hooks": [mine]}]
                }
            })
        );
        // Idempotent
        assert!(strip_legacy_hooks(&mut settings).is_empty());
    }

    #[test]
    fn drops_hooks_key_when_only_legacy_hooks() {
        let mut settings = serde_json::json!({
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "tam hook-notify --event stop"}]}]}
        });
        assert_eq!(strip_legacy_hooks(&mut settings), vec!["Stop"]);
        assert_eq!(settings, serde_json::json!({}));
    }

    #[test]
    fn removes_legacy_hooks_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"tam hook-notify --event stop"}]}]},"theme":"dark"}"#,
        )
        .unwrap();
        remove_legacy_claude_hooks(&path).unwrap();
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after, serde_json::json!({"theme": "dark"}));
        // Missing file is fine
        remove_legacy_claude_hooks(&dir.path().join("none.json")).unwrap();
    }
}
