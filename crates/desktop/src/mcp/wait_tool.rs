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

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::capture::{self, CaptureOptions, ShotFormat};
use crate::fingerprint::{self, Fingerprint};
use crate::selector::Selector;
use crate::uia::{self, Tree, WindowInfo};
use crate::wait::{self, Budget, Condition, Env, Outcome, DEFAULT_TOLERANCE, UNANSWERED};

use super::guard::{refuse_app, refuse_blocked};
use super::tools::{guard_capture, text, window_arg};

/// Longest edge of the picture a fingerprint is made from: the fingerprint
/// is 128 bits, so more pixels only cost.
const FINGERPRINT_EDGE: u32 = 256;

/// How far past the wait's deadline a look may run before it is given up on.
///
/// **No look may outlive the deadline by more than this.** A look is a call
/// into the accessibility layer, which asks the program that owns the window;
/// when that program is closing or not responding the call blocks for as long
/// as the provider takes to give up (measured on Windows: a pinned window that
/// had just closed kept a wait with `timeout_ms: 4000` from returning for more
/// than five minutes). A look therefore runs on a thread of its own, and the
/// wait stops waiting for it at its deadline plus this much.
const LOOK_GRACE: Duration = Duration::from_millis(1000);

/// Most looks that have been given up on and are still blocked, in this
/// process. At the limit no new look is started: another would only join them.
const MAX_ABANDONED: usize = 4;

/// Looks given up on that have not returned yet.
static ABANDONED: AtomicUsize = AtomicUsize::new(0);

/// `work`, run on a thread of its own and waited for at most `limit`.
///
/// On time it is `work`'s answer. Past `limit` the thread is left to finish
/// by itself (it cannot be stopped from outside: it is inside a call the
/// operating system has not returned from) and the answer is
/// [`UNANSWERED`], counted in `abandoned` until the thread does return.
fn bounded<T: Send + 'static>(
    what: &str,
    limit: Duration,
    abandoned: &'static AtomicUsize,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let still_blocked = abandoned.load(Ordering::SeqCst);
    if still_blocked >= MAX_ABANDONED {
        return Err(format!(
            "{UNANSWERED}: {still_blocked} earlier looks are still blocked in the \
             accessibility layer, so none was started to {what}"
        ));
    }
    // 0 running, 1 returned in time, 2 given up on.
    let state = Arc::new(AtomicU8::new(0));
    let (send, receive) = mpsc::channel();
    let theirs = Arc::clone(&state);
    let spawned = std::thread::Builder::new()
        .name("wait-look".to_string())
        .spawn(move || {
            let answer = work();
            // If the wait has given up on this look, the thread is the one
            // left to take it off the count.
            if theirs
                .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                abandoned.fetch_sub(1, Ordering::SeqCst);
            }
            let _ = send.send(answer);
        });
    if let Err(e) = spawned {
        return Err(format!("could not start a look to {what}: {e}"));
    }
    match receive.recv_timeout(limit) {
        Ok(answer) => answer,
        Err(_) => {
            if state
                .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                abandoned.fetch_add(1, Ordering::SeqCst);
                Err(format!(
                    "{UNANSWERED} to {what} within {} ms (a window that is closing or not \
                     responding can do this)",
                    limit.as_millis()
                ))
            } else {
                // It returned in the instant between the timeout and this.
                receive
                    .recv()
                    .unwrap_or_else(|_| Err(format!("the look to {what} ended without an answer")))
            }
        }
    }
}

/// The desktop as this process sees it, with a clock that starts now.
struct Live {
    started: Instant,
    /// When the wait's `timeout_ms` is up. A look may run to this and
    /// [`LOOK_GRACE`] beyond it.
    deadline: Instant,
}

impl Live {
    fn new(budget: &Budget) -> Live {
        let started = Instant::now();
        Live {
            started,
            deadline: started + Duration::from_millis(budget.timeout_ms),
        }
    }

    /// `work` as one look, held to the deadline and the grace after it.
    fn within<T: Send + 'static>(
        &self,
        what: &str,
        work: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let limit = (self.deadline + LOOK_GRACE).saturating_duration_since(Instant::now());
        bounded(what, limit, &ABANDONED, work)
    }
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
        let all = self.within("list the windows", listed)?;
        Ok(all
            .into_iter()
            .filter(|w| crate::limits::get().blocked(&w.app).is_none())
            .collect())
    }

    fn window_alive(&mut self, id: &str) -> Option<bool> {
        uia::window_exists(id)
    }

    fn focused_window(&mut self) -> Result<WindowInfo, String> {
        let window = self.within("find the focused window", || {
            uia::window_info(None).map_err(text_of)
        })?;
        refuse_blocked(&window)?;
        Ok(window)
    }

    fn tree(&mut self, window: &str, depth: Option<u32>) -> Result<Tree, String> {
        let id = window.to_string();
        let (info, tree) = self.within("walk the window", move || {
            let info = uia::window_info(Some(&id)).map_err(text_of)?;
            // Refused before the window is walked: a blocked application's
            // window is never read.
            refuse_blocked(&info)?;
            let tree = uia::tree_now(&id, depth).map_err(text_of)?;
            Ok((info, tree))
        })?;
        // Walked in this process, and discarded unread when blocked.
        refuse_blocked(&info)?;
        refuse_app(&tree.app)?;
        Ok(tree)
    }

    fn screen(&mut self, window: Option<&str>) -> Result<Fingerprint, String> {
        let window = window.map(str::to_string);
        self.within("capture the screen", move || {
            let of = guard_capture(window.as_deref())?;
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
        })
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
    let mut env = Live::new(&budget);
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

    /// **A look that blocks cannot outlive its limit.** The live hang was a
    /// call that did not return for minutes; here the work sleeps far longer
    /// than the limit and `bounded` must hand back long before it returns.
    #[test]
    fn a_look_that_blocks_is_given_up_on_at_its_limit_not_waited_for() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let started = Instant::now();
        let answer = bounded(
            "walk the window",
            Duration::from_millis(150),
            &COUNT,
            || {
                std::thread::sleep(Duration::from_secs(5));
                Ok(1)
            },
        );
        let took = started.elapsed();
        let why = answer.unwrap_err();
        assert!(why.starts_with(UNANSWERED), "{why}");
        assert!(why.contains("walk the window"), "{why}");
        assert!(
            took < Duration::from_millis(150) + LOOK_GRACE,
            "a 150 ms limit held the caller for {took:?}"
        );
        assert_eq!(COUNT.load(Ordering::SeqCst), 1, "counted while it blocks");
    }

    #[test]
    fn a_look_given_up_on_is_taken_off_the_count_when_it_returns() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let answer = bounded("walk", Duration::from_millis(50), &COUNT, || {
            std::thread::sleep(Duration::from_millis(300));
            Ok(())
        });
        assert!(answer.is_err());
        assert_eq!(COUNT.load(Ordering::SeqCst), 1);
        let until = Instant::now() + Duration::from_secs(5);
        while COUNT.load(Ordering::SeqCst) != 0 {
            assert!(Instant::now() < until, "the count was never given back");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_look_that_answers_in_time_gives_its_answer_or_its_error() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let ok = bounded("list", Duration::from_secs(5), &COUNT, || Ok(vec![1, 2]));
        assert_eq!(ok.unwrap(), vec![1, 2]);
        let refused: Result<(), String> = bounded("list", Duration::from_secs(5), &COUNT, || {
            Err("refused: blocked".to_string())
        });
        assert_eq!(refused.unwrap_err(), "refused: blocked");
        assert_eq!(COUNT.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn no_new_look_is_started_while_too_many_are_still_blocked() {
        static COUNT: AtomicUsize = AtomicUsize::new(MAX_ABANDONED);
        let ran = Arc::new(AtomicU8::new(0));
        let flag = Arc::clone(&ran);
        let answer = bounded("walk", Duration::from_secs(5), &COUNT, move || {
            flag.store(1, Ordering::SeqCst);
            Ok(())
        });
        let why = answer.unwrap_err();
        assert!(why.starts_with(UNANSWERED), "{why}");
        assert!(why.contains("still blocked"), "{why}");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(ran.load(Ordering::SeqCst), 0, "a look was started anyway");
    }

    #[test]
    fn the_live_deadline_is_the_budgets_timeout_from_now() {
        let live = Live::new(&Budget::new(Some(4_000), None));
        let left = live.deadline.saturating_duration_since(Instant::now());
        assert!(left <= Duration::from_millis(4_000));
        assert!(left > Duration::from_millis(3_000), "{left:?}");
    }

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
