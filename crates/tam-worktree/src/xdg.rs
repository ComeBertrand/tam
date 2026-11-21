//! The XDG base directories, the same on every platform: macOS included.
//!
//! Not `dirs::config_dir()` and `dirs::data_local_dir()`: on macOS both are
//! `~/Library/Application Support`. On Linux they are what these return.

use std::ffi::OsString;
use std::path::PathBuf;

/// `$XDG_CONFIG_HOME`, else `~/.config`. `None`: no home directory.
pub fn config_home() -> Option<PathBuf> {
    base(
        std::env::var_os("XDG_CONFIG_HOME"),
        dirs::home_dir(),
        ".config",
    )
}

/// `$XDG_DATA_HOME`, else `~/.local/share`. `None`: no home directory.
pub fn data_home() -> Option<PathBuf> {
    base(
        std::env::var_os("XDG_DATA_HOME"),
        dirs::home_dir(),
        ".local/share",
    )
}

/// The variable's `value` when it is an absolute path (the specification
/// says to ignore a relative one, and an empty one is unset), else
/// `default` under `home`.
fn base(value: Option<OsString>, home: Option<PathBuf>, default: &str) -> Option<PathBuf> {
    value
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| Some(home?.join(default)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> Option<PathBuf> {
        Some(PathBuf::from("/home/me"))
    }

    #[test]
    fn the_variable_when_it_is_set() {
        assert_eq!(
            base(Some("/xdg/config".into()), home(), ".config"),
            Some(PathBuf::from("/xdg/config"))
        );
        assert_eq!(
            base(Some("/xdg/config".into()), None, ".config"),
            Some(PathBuf::from("/xdg/config"))
        );
    }

    #[test]
    fn else_under_home() {
        assert_eq!(
            base(None, home(), ".config"),
            Some(PathBuf::from("/home/me/.config"))
        );
        assert_eq!(
            base(None, home(), ".local/share"),
            Some(PathBuf::from("/home/me/.local/share"))
        );
        assert_eq!(base(None, None, ".config"), None);
    }

    #[test]
    fn an_empty_or_relative_value_is_ignored() {
        for value in ["", "config", "./config", "~/config"] {
            assert_eq!(
                base(Some(value.into()), home(), ".config"),
                Some(PathBuf::from("/home/me/.config")),
                "{value:?}"
            );
        }
    }
}
