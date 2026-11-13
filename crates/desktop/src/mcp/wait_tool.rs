//! The `wait_for` tool: its arguments read, the live desktop handed to
//! [`crate::wait`], and what that found rendered as the reply.
//!
//! The waiting itself, and every rule about time, is in `crate::wait` and is
//! tested there against a scripted desktop. This file is the part that touches
//! the real one, and it holds each look to the same guards the one-shot tools
//! keep: a blocked application's window is never listed, walked or captured,
//! and a full capture is refused while one is on screen.
//!
//! # Wired to
//!
//! - **Called by** `Server::run_tool` in [`super::tools`].
//! - **Calls** [`crate::wait::run`] with [`Live`], which reads
//!   [`crate::uia`] and [`crate::capture`] through [`super::guard`].

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::capture::{self, CaptureOptions, ShotFormat};
use crate::fingerprint::{self, Fingerprint};
use crate::selector::Selector;
use crate::uia::{self, Tree, WindowInfo};
use crate::wait::{self, Budget, Condition, Env, Outcome, DEFAULT_TOLERANCE};

use super::guard::{refuse_app, refuse_blocked};
use super::tools::{guard_capture, text, window_arg};

/// Longest edge of the picture a fingerprint is made from: the fingerprint
/// is 128 bits, so more pixels only cost.
const FINGERPRINT_EDGE: u32 = 256;

/// The desktop as this process sees it, with a clock that starts now.
struct Live {
    started: Instant,
}

fn text_of(e: impl ToString) -> String {
    e.to_string()
}

impl Env for Live {
    fn now_ms(&mut self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn sleep_ms(&mut self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }

    fn windows(&mut self) -> Result<Vec<WindowInfo>, String> {
        // Not the windows of a blocked application: they are not listed, and a
        // selector must not be able to find one by its title.
        Ok(listed()?
            .into_iter()
            .filter(|w| crate::limits::get().blocked(&w.app).is_none())
            .collect())
    }

    fn focused_window(&mut self) -> Result<WindowInfo, String> {
        let window = uia::window_info(None).map_err(text_of)?;
        refuse_blocked(&window)?;
        Ok(window)
    }

    fn tree(&mut self, window: &str, depth: Option<u32>) -> Result<Tree, String> {
        let info = uia::window_info(Some(window)).map_err(text_of)?;
        refuse_blocked(&info)?;
        let tree = uia::tree_now(window, depth).map_err(text_of)?;
        // Walked in this process, and discarded unread when blocked.
        refuse_app(&tree.app)?;
        Ok(tree)
    }

    fn screen(&mut self, window: Option<&str>) -> Result<Fingerprint, String> {
        let of = guard_capture(window)?;
        let options = CaptureOptions {
            max_edge: FINGERPRINT_EDGE,
            format: ShotFormat::Png,
            ..CaptureOptions::default()
        };
        let shot = match &of {
            Some(info) => capture::capture_region(info.rect, options),
            None => capture::capture_primary(options),
        }
        .map_err(text_of)?;
        let picture = image::load_from_memory(&shot.bytes)
            .map_err(|e| format!("could not read the capture back to fingerprint it: {e}"))?
            .to_rgba8();
        Ok(fingerprint::of(&picture))
    }
}

/// The windows, asked again a few times when the desktop will not say.
///
/// A wait is exactly what meets a window closing in the middle of the
/// enumeration, and UI Automation answers that with an "unspecified error" for
/// that one call (measured: it failed the live wait tests on a busy Windows
/// desktop). One failed listing must not end a wait that is for the very thing
/// happening; a listing that keeps failing does.
fn listed() -> Result<Vec<WindowInfo>, String> {
    const TRIES: u32 = 4;
    let mut last = String::new();
    for attempt in 0..TRIES {
        match uia::list_windows() {
            Ok(windows) => return Ok(windows),
            Err(e) => last = e.to_string(),
        }
        if attempt + 1 < TRIES {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Err(format!("{last} (after {TRIES} tries)"))
}

/// A non-negative whole number argument, when it was given.
fn whole(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{key} must be a whole number of milliseconds, 0 or more")),
    }
}

fn selector_arg(args: &Value, condition: &str) -> Result<Selector, String> {
    let raw = args
        .get("selector")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{condition} needs a `selector`, e.g. role=button;name=Save"))?;
    let selector = Selector::parse(raw).map_err(|e| format!("selector: {e}"))?;
    // A blocked application is refused by name before anything looks for it.
    if let Some(app) = &selector.app {
        refuse_app(app)?;
    }
    Ok(selector)
}

/// What the call waits for.
fn condition_of(args: &Value) -> Result<Condition, String> {
    let which = args.get("condition").and_then(Value::as_str).ok_or(
        "condition is required: element_appears, element_gone, window_appears, \
             window_gone or screen_changes",
    )?;
    match which {
        "element_appears" | "element_gone" => Ok(Condition::Element {
            selector: selector_arg(args, which)?,
            window: window_arg(args),
            appears: which == "element_appears",
        }),
        "window_appears" | "window_gone" => Ok(Condition::Window {
            selector: selector_arg(args, which)?,
            appears: which == "window_appears",
        }),
        "screen_changes" => {
            let baseline = match args.get("baseline").and_then(Value::as_str) {
                None => None,
                Some(hex) => Some(Fingerprint::from_hex(hex).ok_or(
                    "baseline must be a `hash` an earlier wait_for returned (32 hex digits)",
                )?),
            };
            let tolerance = match args.get("tolerance") {
                None | Some(Value::Null) => DEFAULT_TOLERANCE,
                Some(v) => v
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| *n <= 128)
                    .ok_or("tolerance must be a whole number of bits, 0 to 128")?,
            };
            Ok(Condition::Screen {
                baseline,
                tolerance,
                window: window_arg(args),
            })
        }
        other => Err(format!(
            "unknown condition '{other}': use element_appears, element_gone, window_appears, \
             window_gone or screen_changes"
        )),
    }
}

/// `wait_for`: wait, bounded, until the condition holds. `stopped` is the
/// operator's stop, asked between polls. Called by `Server::run_tool`.
pub(super) fn wait_for(args: &Value, stopped: &dyn Fn() -> bool) -> Result<Vec<Value>, String> {
    let condition = condition_of(args)?;
    let budget = Budget::new(whole(args, "timeout_ms")?, whole(args, "poll_ms")?);
    let mut env = Live {
        started: Instant::now(),
    };
    let outcome = wait::run(&mut env, &condition, &budget, stopped)?;
    if matches!(outcome, Outcome::Stopped { .. }) {
        return Err(crate::safety::Refusal::Stopped.message());
    }
    let reply = outcome.to_json(&condition, &budget);
    Ok(vec![text(format!(
        "{}\n{}",
        serde_json::to_string_pretty(&reply).unwrap_or_default(),
        super::UNTRUSTED
    ))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_conditions_are_read_from_the_arguments() {
        let c = condition_of(&json!({"condition": "element_gone", "selector": "role=button;name=Save", "window": "7"}))
            .unwrap();
        match c {
            Condition::Element {
                selector,
                window,
                appears,
            } => {
                assert_eq!(selector.render(), "role=button;name=Save");
                assert_eq!(window.as_deref(), Some("7"));
                assert!(!appears);
            }
            other => panic!("{other:?}"),
        }
        let c =
            condition_of(&json!({"condition": "window_appears", "selector": "window~=Save As"}))
                .unwrap();
        assert!(matches!(c, Condition::Window { appears: true, .. }));
        let c = condition_of(&json!({"condition": "screen_changes"})).unwrap();
        assert!(matches!(
            c,
            Condition::Screen {
                baseline: None,
                tolerance: DEFAULT_TOLERANCE,
                window: None
            }
        ));
    }

    #[test]
    fn a_bad_condition_is_refused_by_name() {
        let bad = |v: Value| condition_of(&v).unwrap_err();
        assert!(bad(json!({})).contains("condition is required"));
        assert!(bad(json!({"condition": "soon"})).contains("unknown condition 'soon'"));
        assert!(bad(json!({"condition": "element_appears"})).contains("needs a `selector`"));
        assert!(
            bad(json!({"condition": "element_appears", "selector": "colour=red"}))
                .contains("unknown key")
        );
        assert!(
            bad(json!({"condition": "screen_changes", "baseline": "xyz"})).contains("baseline")
        );
        assert!(
            bad(json!({"condition": "screen_changes", "tolerance": 129})).contains("tolerance")
        );
        assert!(bad(json!({"condition": "screen_changes", "tolerance": -1})).contains("tolerance"));
    }

    #[test]
    fn a_blocked_application_is_refused_before_anything_looks_for_it() {
        let why =
            condition_of(&json!({"condition": "window_appears", "selector": "app=KeePassXC"}))
                .unwrap_err();
        assert!(why.contains("blocked"), "{why}");
    }

    #[test]
    fn a_timeout_must_be_a_whole_non_negative_number() {
        assert_eq!(whole(&json!({}), "timeout_ms"), Ok(None));
        assert_eq!(whole(&json!({"timeout_ms": 0}), "timeout_ms"), Ok(Some(0)));
        assert_eq!(
            whole(&json!({"timeout_ms": 1500}), "timeout_ms"),
            Ok(Some(1500))
        );
        assert!(whole(&json!({"timeout_ms": -5}), "timeout_ms").is_err());
        assert!(whole(&json!({"timeout_ms": 1.5}), "timeout_ms").is_err());
        assert!(whole(&json!({"timeout_ms": "soon"}), "timeout_ms").is_err());
    }
}
