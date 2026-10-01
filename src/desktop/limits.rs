//! The desktop's limits, from admin.json `desktop_limits`.
//!
//! Every number the desktop server bounds itself with lives here and can be
//! set there; an unset field is the default below. Read once, when
//! `incode --desktop-mcp` starts ([`set`]), and the same for the life of the
//! process ([`get`]); anything that runs without that start (a test) sees
//! the defaults.
//!
//! Two of them are SAFETY limits — the runaway cap and the dead-man's switch
//! (`docs/COMPUTER_USE.md` 7b) — and may only be made stricter: a value above
//! the default is refused by name rather than quietly loosening the guard
//! that stops a looping agent.

use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Most actions in any rolling minute: the runaway guard. Stricter only.
pub const DEFAULT_MAX_ACTIONS_PER_MINUTE: u32 = 120;
/// How long the desktop keeps acting after the last heartbeat. Stricter only.
pub const DEFAULT_HEARTBEAT_DEADLINE_SECS: u64 = 30;
/// How deep `ui_tree` walks when the call names no depth.
pub const DEFAULT_TREE_DEPTH: u32 = 12;
/// Most elements one walk returns.
pub const DEFAULT_TREE_MAX_ELEMENTS: u32 = 400;
/// The desktop journal's size before it keeps only its newest half.
pub const DEFAULT_JOURNAL_MAX_BYTES: u64 = 256 * 1024;
/// How long a private Xvfb has to report it is ready (Linux).
pub const DEFAULT_XVFB_START_TIMEOUT_SECS: u64 = 10;
/// How long the clipboard program has to take the text (Linux).
pub const DEFAULT_CLIPBOARD_TIMEOUT_SECS: u64 = 5;
/// Each accessibility request (macOS AX, Linux AT-SPI): a hung application
/// costs this, not the whole walk.
pub const DEFAULT_ACCESSIBILITY_CALL_TIMEOUT_MS: u64 = 1000;

/// admin.json `desktop_limits`, as written. Every field optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_actions_per_minute: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_deadline_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_max_elements: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xvfb_start_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessibility_call_timeout_ms: Option<u64>,
}

impl DesktopLimits {
    /// Whether nothing is set, so admin.json does not carry an empty object.
    pub fn is_empty(&self) -> bool {
        *self == DesktopLimits::default()
    }

    /// The limits in force: each set value checked, each unset one the
    /// default. Zero is refused everywhere — a zero cap or timeout is a
    /// desktop that can do nothing — and the two safety limits are refused
    /// above their defaults.
    pub fn resolve(&self) -> Result<Limits, String> {
        fn positive<T: PartialOrd + Default + Copy + std::fmt::Display>(
            name: &str,
            value: Option<T>,
            default: T,
        ) -> Result<T, String> {
            match value {
                Some(v) if v <= T::default() => Err(format!(
                    "desktop_limits.{name} must be more than 0, not {v}"
                )),
                Some(v) => Ok(v),
                None => Ok(default),
            }
        }
        fn stricter<T: PartialOrd + Default + Copy + std::fmt::Display>(
            name: &str,
            value: Option<T>,
            default: T,
        ) -> Result<T, String> {
            let v = positive(name, value, default)?;
            if v > default {
                return Err(format!(
                    "desktop_limits.{name} is a safety limit and may only be made stricter: \
                     at most {default}, not {v}"
                ));
            }
            Ok(v)
        }
        Ok(Limits {
            max_actions_per_minute: stricter(
                "max_actions_per_minute",
                self.max_actions_per_minute,
                DEFAULT_MAX_ACTIONS_PER_MINUTE,
            )? as usize,
            heartbeat_deadline: Duration::from_secs(stricter(
                "heartbeat_deadline_secs",
                self.heartbeat_deadline_secs,
                DEFAULT_HEARTBEAT_DEADLINE_SECS,
            )?),
            tree_depth: positive("tree_depth", self.tree_depth, DEFAULT_TREE_DEPTH)?,
            tree_max_elements: positive(
                "tree_max_elements",
                self.tree_max_elements,
                DEFAULT_TREE_MAX_ELEMENTS,
            )? as usize,
            journal_max_bytes: positive(
                "journal_max_bytes",
                self.journal_max_bytes,
                DEFAULT_JOURNAL_MAX_BYTES,
            )?,
            xvfb_start_timeout: Duration::from_secs(positive(
                "xvfb_start_timeout_secs",
                self.xvfb_start_timeout_secs,
                DEFAULT_XVFB_START_TIMEOUT_SECS,
            )?),
            clipboard_timeout: Duration::from_secs(positive(
                "clipboard_timeout_secs",
                self.clipboard_timeout_secs,
                DEFAULT_CLIPBOARD_TIMEOUT_SECS,
            )?),
            accessibility_call_timeout: Duration::from_millis(positive(
                "accessibility_call_timeout_ms",
                self.accessibility_call_timeout_ms,
                DEFAULT_ACCESSIBILITY_CALL_TIMEOUT_MS,
            )?),
        })
    }
}

/// The limits in force.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub max_actions_per_minute: usize,
    pub heartbeat_deadline: Duration,
    pub tree_depth: u32,
    pub tree_max_elements: usize,
    pub journal_max_bytes: u64,
    pub xvfb_start_timeout: Duration,
    pub clipboard_timeout: Duration,
    pub accessibility_call_timeout: Duration,
}

static IN_FORCE: OnceLock<Limits> = OnceLock::new();

/// Fix the limits for this process. The first call wins; `incode
/// --desktop-mcp` makes it before anything acts.
pub fn set(limits: Limits) {
    let _ = IN_FORCE.set(limits);
}

/// The limits in force: those [`set`], else the defaults.
pub fn get() -> &'static Limits {
    IN_FORCE.get_or_init(|| {
        DesktopLimits::default()
            .resolve()
            .expect("the defaults are valid")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unset is the default, for every limit.
    #[test]
    fn nothing_set_is_every_default() {
        let limits = DesktopLimits::default().resolve().expect("valid");
        assert_eq!(limits.max_actions_per_minute, 120);
        assert_eq!(limits.heartbeat_deadline, Duration::from_secs(30));
        assert_eq!(limits.tree_depth, 12);
        assert_eq!(limits.tree_max_elements, 400);
        assert_eq!(limits.journal_max_bytes, 256 * 1024);
        assert_eq!(limits.xvfb_start_timeout, Duration::from_secs(10));
        assert_eq!(limits.clipboard_timeout, Duration::from_secs(5));
        assert_eq!(
            limits.accessibility_call_timeout,
            Duration::from_millis(1000)
        );
    }

    /// **THE SAFETY LIMITS ONLY TIGHTEN.** Stricter is taken; looser is
    /// refused by name with the ceiling; zero is refused everywhere.
    #[test]
    fn a_safety_limit_may_be_tightened_but_never_loosened() {
        let strict = DesktopLimits {
            max_actions_per_minute: Some(30),
            heartbeat_deadline_secs: Some(10),
            tree_depth: Some(40),
            journal_max_bytes: Some(1024 * 1024),
            ..DesktopLimits::default()
        }
        .resolve()
        .expect("stricter safety, larger tree and journal");
        assert_eq!(strict.max_actions_per_minute, 30);
        assert_eq!(strict.heartbeat_deadline, Duration::from_secs(10));
        assert_eq!(strict.tree_depth, 40);
        assert_eq!(strict.journal_max_bytes, 1024 * 1024);

        let why = DesktopLimits {
            max_actions_per_minute: Some(1000),
            ..DesktopLimits::default()
        }
        .resolve()
        .expect_err("looser");
        assert!(
            why.contains("max_actions_per_minute") && why.contains("120"),
            "{why}"
        );
        let why = DesktopLimits {
            heartbeat_deadline_secs: Some(300),
            ..DesktopLimits::default()
        }
        .resolve()
        .expect_err("looser");
        assert!(why.contains("heartbeat_deadline_secs"), "{why}");

        let why = DesktopLimits {
            tree_max_elements: Some(0),
            ..DesktopLimits::default()
        }
        .resolve()
        .expect_err("zero");
        assert!(why.contains("more than 0"), "{why}");
    }

    /// A misspelt key is an error, not a limit silently left at its default.
    #[test]
    fn an_unknown_key_is_refused() {
        let parsed: Result<DesktopLimits, _> =
            serde_json::from_str(r#"{"max_action_per_minute": 10}"#);
        assert!(parsed.is_err());
    }
}
