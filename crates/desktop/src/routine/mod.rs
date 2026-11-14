//! Routines: what an agent did once at the desktop, kept so it can be done
//! again with no model.
//!
//! # Where this sits
//!
//! A routine is a trigger phrase and the list of desktop tool calls that
//! carried it out. [`learn`] decides whether a run is fit to keep and cleans
//! it, [`store`] keeps the routines in one JSON file, [`words`] finds the
//! routine a phrase asks for, and [`replay`] runs one again, step by step, to
//! the first failure. The MCP tools in `mcp::routine_tool` are the only
//! callers; they are the seam where the live desktop and its gates enter.
//!
//! # What is reused and what is not built
//!
//! There is **no trace format and no trace store**. Watchman already records
//! every tool call, so a routine is made from Watchman's records
//! ([`source::StepSource`], with [`watchman::WatchmanSource`] reading the local
//! memory engine's read-only `/v1/memory/watchman/*` endpoints) or from an
//! explicit step list. The engine is optional: [`source::MemorySource`] is the
//! in-memory source tests use, and a crate with no engine at all still saves,
//! lists, matches and replays routines from an explicit list.
//!
//! The design is the learned-shortcut design of the Atticus project (MIT;
//! `atticus.py`, "learned shortcuts"), kept as simple as it is there:
//!
//! - per routine: the cleaned steps, the trigger phrase, the date, when it was
//!   last used, and whether the person kept it (`incu routine keep`: no tool can
//!   set it);
//! - a phrase matches when its WORD SET is the routine's, in any order, with
//!   the filler words ignored ([`words`]);
//! - nothing is learned when the phrase says "this" or "that", when the run
//!   failed, or when any step could not be replayed on another day: an `eN`
//!   id, a pixel coordinate or a window handle ([`learn`]);
//! - reading steps (`ui_tree`, `screenshot`, ...) are dropped, `wait_for` is
//!   kept;
//! - a replay that fails reports the step and stops. There is NO step repair:
//!   the caller redoes the whole task and saves the new run, which replaces
//!   the routine;
//! - a routine unused for [`EXPIRY_DAYS`] days is forgotten, unless the user
//!   taught it.
//!
//! # Not here
//!
//! Voice, scheduling, chats and the browser belong to Watchman. This module
//! never calls a model and never schedules anything.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod learn;
pub mod oslock;
pub mod params;
pub mod pictures;
pub mod replay;
pub mod source;
pub mod store;
pub mod watchman;
pub mod words;

/// A routine unused this long is forgotten, unless the user taught it.
pub const EXPIRY_DAYS: i64 = 30;

/// Milliseconds in a day, for expiry.
pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// The most steps one routine may hold. A run longer than this is not a
/// shortcut, and a bound keeps a hand-edited file from being replayed forever.
pub const MAX_STEPS: usize = 100;

/// The most routines the file holds; saving a new one past this is refused
/// rather than silently dropping an old one.
pub const MAX_ROUTINES: usize = 100;

/// The most one step's arguments may take as JSON, in bytes. Typed text is the
/// bulk of it; a routine that types a page is not a shortcut.
pub const MAX_STEP_BYTES: usize = 4 * 1024;

/// The most one routine may take, by [`Routine::size_bytes`]. With
/// [`MAX_ROUTINES`] of them the file stays under what it may be
/// ([`store::MAX_FILE_BYTES`]), with room for the indentation the file is
/// written with; a test writes the largest file the two allow and reads it back.
pub const MAX_ROUTINE_BYTES: usize = 24 * 1024;

/// What a step costs beyond its own bytes: the keys, quotes and indentation of
/// the readable file. Counted so that a routine of many tiny steps, which is
/// mostly structure, is not mistaken for a small one.
const STEP_OVERHEAD_BYTES: usize = 160;

/// One desktop tool call to run again: the tool and the arguments it was
/// given. The same names and arguments the live tools take, so one dispatcher
/// runs both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// A desktop tool's name, as `tools/list` offers it.
    pub tool: String,
    /// Its arguments.
    #[serde(default)]
    pub args: Value,
}

impl Step {
    /// A step.
    #[must_use]
    pub fn new(tool: &str, args: Value) -> Self {
        Self {
            tool: tool.to_string(),
            args,
        }
    }

    /// The step in one line, for the agent and the operator. Typed text and
    /// values are shown by length only, and a selector without a text field's
    /// name, as the journal shows them (`mcp::gate`, the redaction boundary): a
    /// line about a routine must not carry what a password field was given.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut shown = Vec::new();
        if let Some(map) = self.args.as_object() {
            for (key, value) in map {
                let text = match (key.as_str(), value.as_str()) {
                    ("text" | "value", Some(s)) => format!("{} characters", s.chars().count()),
                    // As the journal has a selector: the name of a text field
                    // is what was typed in it, so it is left out
                    // ([`crate::selector::Selector::render_redacted`]). This line
                    // is in the failure of a replay, which is journalled whole.
                    ("selector", Some(s)) => crate::selector::Selector::parse(s).map_or_else(
                        |_| "(unreadable)".to_string(),
                        |sel| format!("\"{}\"", sel.render_redacted()),
                    ),
                    _ => value.to_string(),
                };
                shown.push(format!("{key}={text}"));
            }
        }
        if shown.is_empty() {
            self.tool.clone()
        } else {
            format!("{} {}", self.tool, shown.join(" "))
        }
    }

    /// The step in one line with EVERYTHING it types and every selector in
    /// full: what `routine_save`, `routine_list` and `routine_match` show, so the
    /// person can read exactly what a routine will type and where before it runs.
    ///
    /// A parameter (`{{password}}`) is shown as that name, which is all the file
    /// holds of it. Not for the journal or a failure message: those take
    /// [`Step::summary`], which keeps typed text out.
    #[must_use]
    pub fn review(&self) -> String {
        let mut shown = Vec::new();
        if let Some(map) = self.args.as_object() {
            for (key, value) in map {
                shown.push(format!("{key}={value}"));
            }
        }
        if shown.is_empty() {
            self.tool.clone()
        } else {
            format!("{} {}", self.tool, shown.join(" "))
        }
    }
}

/// A learned routine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Routine {
    /// The phrase it was learned under, as the user said it.
    pub phrase: String,
    /// What to run, in order, already cleaned.
    pub steps: Vec<Step>,
    /// The day it was learned, `YYYY-MM-DD`. For people; expiry uses
    /// [`Routine::last_used_ms`].
    pub date: String,
    /// When it last ran to the end (or was learned), in Unix milliseconds.
    pub last_used_ms: i64,
    /// The person asked to keep it (`incu routine keep`). Such a routine never
    /// expires. Only the person sets this: no tool takes it as an argument, so an
    /// agent cannot exempt what it saved from expiry. A save that replaces a kept
    /// routine stays kept.
    #[serde(default)]
    pub taught: bool,
}

impl Routine {
    /// What this routine takes of the file, in bytes, as [`MAX_ROUTINE_BYTES`]
    /// counts it: its phrase, each step's tool and arguments, and
    /// [`STEP_OVERHEAD_BYTES`] for each step.
    #[must_use]
    pub fn size_bytes(&self) -> usize {
        self.phrase.len()
            + 64
            + self
                .steps
                .iter()
                .map(|s| s.tool.len() + s.args.to_string().len() + STEP_OVERHEAD_BYTES)
                .sum::<usize>()
    }

    /// Whether this routine has gone unused long enough to be forgotten at
    /// `now_ms`. A routine the user taught never is. A clock that went
    /// backwards (a `last_used_ms` after `now_ms`) does not expire anything.
    #[must_use]
    pub fn is_expired(&self, now_ms: i64) -> bool {
        !self.taught && now_ms.saturating_sub(self.last_used_ms) > EXPIRY_DAYS * DAY_MS
    }

    /// Whole days until it expires, or `None` for a routine that never does.
    #[must_use]
    pub fn days_left(&self, now_ms: i64) -> Option<i64> {
        if self.taught {
            return None;
        }
        let left = EXPIRY_DAYS * DAY_MS - now_ms.saturating_sub(self.last_used_ms);
        // Rounded up: a routine saved a moment ago has 30 days, not 29.
        Some(((left + DAY_MS - 1) / DAY_MS).max(0))
    }
}

/// `YYYY-MM-DD` of a Unix time in milliseconds (UTC).
#[must_use]
pub fn day_of(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Now, in Unix milliseconds.
#[must_use]
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn routine(taught: bool, last_used_ms: i64) -> Routine {
        Routine {
            phrase: "open notes".to_string(),
            steps: vec![Step::new("key", json!({"keys": ["ctrl", "n"]}))],
            date: "2026-01-01".to_string(),
            last_used_ms,
            taught,
        }
    }

    #[test]
    fn a_routine_expires_after_thirty_unused_days_and_not_before() {
        let r = routine(false, 0);
        assert!(
            !r.is_expired(30 * DAY_MS),
            "exactly thirty days is not over"
        );
        assert!(r.is_expired(30 * DAY_MS + 1));
        assert!(!r.is_expired(1));
    }

    #[test]
    fn a_routine_the_user_taught_never_expires() {
        let r = routine(true, 0);
        assert!(!r.is_expired(10_000 * DAY_MS));
        assert_eq!(r.days_left(10_000 * DAY_MS), None);
    }

    #[test]
    fn a_clock_that_went_backwards_expires_nothing() {
        assert!(!routine(false, 100 * DAY_MS).is_expired(0));
    }

    #[test]
    fn days_left_counts_down_and_stops_at_zero() {
        let r = routine(false, 0);
        assert_eq!(r.days_left(0), Some(30));
        assert_eq!(r.days_left(10 * DAY_MS), Some(20));
        assert_eq!(r.days_left(99 * DAY_MS), Some(0));
    }

    #[test]
    fn a_step_summary_never_carries_typed_text() {
        let s = Step::new("type_text", json!({"text": "hunter2 and more"}));
        let line = s.summary();
        assert!(line.contains("16 characters"), "{line}");
        assert!(!line.contains("hunter2"), "{line}");
        let s = Step::new(
            "set_value",
            json!({"selector": "role=edit", "value": "secret"}),
        );
        assert!(!s.summary().contains("secret"), "{}", s.summary());
        assert!(s.summary().contains("role=edit"));
    }

    #[test]
    fn the_day_is_a_utc_date() {
        assert_eq!(day_of(0), "1970-01-01");
        assert_eq!(day_of(1_790_107_969_427), "2026-09-22");
    }
}
