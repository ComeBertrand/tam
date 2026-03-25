use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Default)]
pub struct ConfigFile {
    pub discovery: Option<DiscoveryConfig>,
    pub worktree: Option<WorktreeConfig>,
}

#[derive(Debug, Deserialize)]
pub struct DiscoveryConfig {
    pub max_depth: Option<usize>,
    pub ignore: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct WorktreeConfig {
    pub root: Option<String>,
    pub auto_init: Option<bool>,
}

#[derive(Debug)]
pub struct Config {
    pub max_depth: usize,
    pub ignore: Vec<String>,
    pub worktree_root: PathBuf,
    pub auto_init: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_depth: 5,
            ignore: vec![".*".to_string(), "node_modules".to_string()],
            worktree_root: dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/"))
                .join("worktrees"),
            auto_init: false,
        }
    }
}

/// Expand a leading `~` to the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(rest)
    } else if path == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
    } else {
        PathBuf::from(path)
    }
}

/// Parse a TOML string into a resolved Config.
pub fn parse_config(toml_str: &str) -> Result<Config> {
    let file: ConfigFile = toml::from_str(toml_str)?;
    let defaults = Config::default();

    let max_depth = file
        .discovery
        .as_ref()
        .and_then(|d| d.max_depth)
        .unwrap_or(defaults.max_depth);

    let ignore = file
        .discovery
        .as_ref()
        .and_then(|d| d.ignore.clone())
        .unwrap_or(defaults.ignore);

    let worktree_root = file
        .worktree
        .as_ref()
        .and_then(|w| w.root.as_ref())
        .map(|r| expand_tilde(r))
        .unwrap_or(defaults.worktree_root);

    let auto_init = file
        .worktree
        .as_ref()
        .and_then(|w| w.auto_init)
        .unwrap_or(defaults.auto_init);

    Ok(Config {
        max_depth,
        ignore,
        worktree_root,
        auto_init,
    })
}

/// Load config from the standard path, with fallback to yawn config.
pub fn load_config() -> Result<Config> {
    let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("/"));

    let tam_config = config_dir.join("tam").join("config.toml");
    let yawn_config = config_dir.join("yawn").join("config.toml");

    if tam_config.exists() {
        let content = std::fs::read_to_string(&tam_config)?;
        parse_config(&content)
    } else if yawn_config.exists() {
        let content = std::fs::read_to_string(&yawn_config)?;
        parse_config(&content)
    } else {
        Ok(Config::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults() {
        let config = Config::default();
        assert_eq!(config.max_depth, 5);
        assert_eq!(config.ignore, vec![".*", "node_modules"]);
        assert!(config.worktree_root.ends_with("worktrees"));
    }

    #[test]
    fn test_parse_empty_toml() {
        let config = parse_config("").unwrap();
        assert_eq!(config.max_depth, 5);
        assert_eq!(config.ignore, vec![".*", "node_modules"]);
    }

    #[test]
    fn test_parse_partial_discovery() {
        let toml = r#"
[discovery]
max_depth = 3
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.max_depth, 3);
        // ignore should still be default since it wasn't specified
        assert_eq!(config.ignore, vec![".*", "node_modules"]);
    }

    #[test]
    fn test_parse_custom_ignore() {
        let toml = r#"
[discovery]
ignore = [".*", "node_modules", "target", "vendor"]
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(
            config.ignore,
            vec![".*", "node_modules", "target", "vendor"]
        );
    }

    #[test]
    fn test_parse_worktree_root_tilde() {
        let toml = r#"
[worktree]
root = "~/my-worktrees"
"#;
        let config = parse_config(toml).unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(config.worktree_root, home.join("my-worktrees"));
    }

    #[test]
    fn test_parse_worktree_root_absolute() {
        let toml = r#"
[worktree]
root = "/tmp/worktrees"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.worktree_root, PathBuf::from("/tmp/worktrees"));
    }

    #[test]
    fn test_parse_full_config() {
        let toml = r#"
[discovery]
max_depth = 10
ignore = [".*"]

[worktree]
root = "/opt/worktrees"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.max_depth, 10);
        assert_eq!(config.ignore, vec![".*"]);
        assert_eq!(config.worktree_root, PathBuf::from("/opt/worktrees"));
    }

    #[test]
    fn test_parse_auto_init() {
        let toml = r#"
[worktree]
auto_init = true
"#;
        let config = parse_config(toml).unwrap();
        assert!(config.auto_init);
    }

    #[test]
    fn test_parse_auto_init_default() {
        let config = parse_config("").unwrap();
        assert!(!config.auto_init);
    }

    #[test]
    fn test_parse_invalid_toml() {
        let result = parse_config("this is not valid toml {{{}}}");
        assert!(result.is_err());
    }

    #[test]
    fn test_expand_tilde_with_path() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~/foo/bar"), home.join("foo/bar"));
    }

    #[test]
    fn test_expand_tilde_bare() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~"), home);
    }

    #[test]
    fn test_expand_tilde_absolute_passthrough() {
        assert_eq!(expand_tilde("/tmp/foo"), PathBuf::from("/tmp/foo"));
    }

    #[test]
    fn test_expand_tilde_relative_passthrough() {
        assert_eq!(expand_tilde("foo/bar"), PathBuf::from("foo/bar"));
    }
}
