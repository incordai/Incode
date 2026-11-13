//! The settings the crate owns: limits, the Linux display and the operator's
//! hooks.
//!
//! # One file for either binary
//!
//! incode keeps these three under `desktop_limits`, `desktop_display` and
//! `desktop_hooks` (also `hooks`) in `~/.incode/admin.json`. [`Config`] reads
//! those names as aliases of its own, and ignores every other key, so
//! `incu serve --config ~/.incode/admin.json` reads incode's file unchanged and
//! an operator's guard cannot end up written in one place and missing in the
//! other. There is deliberately no `deny_unknown_fields` here for that reason.
//! (`desktop_limits` itself keeps it, as ever: a misspelt limit is refused, not
//! ignored.)
//!
//! # Wired to
//!
//! - **Called by** [`crate::cli`] (`incu serve --config`), which hands the
//!   limits to [`crate::limits::set`], the display to
//!   [`crate::linux::attach`] and the hooks to [`crate::hooks::ShellHooks`].
//! - incode's `--desktop-mcp` does not read this: it already holds the same
//!   three values in its `AdminConfig` and passes them to
//!   [`crate::cli::serve`] itself.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::hooks::HookConfig;
use crate::limits::DesktopLimits;

/// The environment variable that names the config file when `--config` is not
/// given.
pub const CONFIG_ENV: &str = "INCU_CONFIG";

/// What `incu` is configured with. Every field is optional: an absent one is
/// the default, and a file of nothing behaves as no file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Every bound: rate cap, heartbeat, tree size, journal size, timeouts. The
    /// two safety limits may only be made stricter. See [`crate::limits`].
    #[serde(default, alias = "desktop_limits")]
    pub limits: DesktopLimits,
    /// Linux: the X display to drive. `":N"` attaches to one already running;
    /// `"xvfb"` (or `"xvfb:WIDTHxHEIGHT"`) starts a private one for the server's
    /// life. Unset is a refusal, never `$DISPLAY`. See [`crate::linux::attach`].
    #[serde(default, alias = "desktop_display")]
    pub display: Option<String>,
    /// The operator's commands around every call. See [`crate::hooks`].
    #[serde(default, alias = "desktop_hooks", alias = "hooks")]
    pub hooks: HookConfig,
}

impl Config {
    /// Where the file lives when `--config` is not given: `$INCU_CONFIG`, else
    /// `incord-desktop/config.json` under the platform's config folder
    /// (`%APPDATA%` on Windows, `$XDG_CONFIG_HOME` or `~/.config` elsewhere).
    /// `None` when none of those can be found.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os(CONFIG_ENV).filter(|p| !p.is_empty()) {
            return Some(PathBuf::from(path));
        }
        let base = if cfg!(windows) {
            std::env::var_os("APPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
                })
        };
        base.map(|dir| dir.join("incord-desktop").join("config.json"))
    }

    /// Read the file at `path`.
    ///
    /// # Errors
    ///
    /// The file cannot be read or is not valid. A malformed file is REFUSED, not
    /// read as empty: empty would start the desktop with no hooks and default
    /// limits, a guard the operator wrote silently gone.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        // A byte-order mark is what some Windows editors put in front of a file
        // a person wrote by hand.
        serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|e| format!("{} is not a valid config: {e}", path.display()))
    }

    /// The config in force when no `--config` was given: the file at
    /// [`Config::default_path`] if there is one, else the defaults. A file that
    /// exists but is malformed is still refused.
    ///
    /// # Errors
    ///
    /// The file exists and cannot be read or is not valid.
    pub fn load_default() -> Result<Self, String> {
        match Self::default_path() {
            Some(path) if path.is_file() => Self::load(&path),
            _ => Ok(Self::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The point of the aliases**: incode's own file, with all its other keys,
    /// is a valid config here and means the same.
    #[test]
    fn an_incode_admin_file_is_read_as_it_stands() {
        let admin = r#"{
            "model": "whatever",
            "notify": "bell",
            "desktop_limits": {"max_actions_per_minute": 30, "blocked_apps": ["vault"]},
            "desktop_display": "xvfb:1280x800",
            "desktop_hooks": {
                "pre_tool_use": ["check.sh", {"command": "ask.sh", "matcher": "type_text"}],
                "post_tool_use": ["log.sh"],
                "stop": ["not-a-desktop-event.sh"]
            }
        }"#;
        let config: Config = serde_json::from_str(admin).expect("incode's file");
        assert_eq!(config.limits.max_actions_per_minute, Some(30));
        assert_eq!(config.limits.blocked_apps, ["vault"]);
        assert_eq!(config.display.as_deref(), Some("xvfb:1280x800"));
        assert_eq!(config.hooks.pre_tool_use.len(), 2);
        assert_eq!(config.hooks.post_tool_use.len(), 1);
    }

    /// `hooks` is the same key as `desktop_hooks`, as incode's `admin.json`
    /// accepts it.
    #[test]
    fn hooks_is_accepted_for_desktop_hooks() {
        let config: Config =
            serde_json::from_str(r#"{"hooks": {"pre_tool_use": ["a"]}}"#).expect("config");
        assert_eq!(config.hooks.pre_tool_use.len(), 1);
    }

    #[test]
    fn the_crates_own_names_work_too() {
        let config: Config =
            serde_json::from_str(r#"{"limits": {"tree_depth": 4}, "display": ":99", "hooks": {}}"#)
                .expect("config");
        assert_eq!(config.limits.tree_depth, Some(4));
        assert_eq!(config.display.as_deref(), Some(":99"));
    }

    #[test]
    fn nothing_set_is_the_default() {
        let config: Config = serde_json::from_str("{}").expect("config");
        assert_eq!(config, Config::default());
        assert!(config.hooks.is_empty());
        assert!(config.limits.is_empty());
    }

    /// A misspelt limit is refused, not ignored: a stricter cap the operator
    /// wrote must not silently stay at its default.
    #[test]
    fn a_misspelt_limit_is_refused() {
        let bad = serde_json::from_str::<Config>(r#"{"limits": {"max_action_per_minute": 1}}"#);
        assert!(bad.is_err());
    }

    #[test]
    fn a_malformed_file_is_refused_not_read_as_empty() {
        let dir = std::env::temp_dir().join(format!("incu-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.json");
        std::fs::write(&path, "{not json").expect("write");
        let why = Config::load(&path).expect_err("malformed");
        assert!(why.contains("not a valid config"), "{why}");
        let missing = Config::load(&dir.join("absent.json")).expect_err("missing");
        assert!(missing.contains("cannot read"), "{missing}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
