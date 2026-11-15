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

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
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

/// Most looks at ONE window (or one kind of look) that have been given up on
/// and are still blocked. At the limit no new look at it is started: another
/// would only join them. Counted per window, so one hung program does not stop
/// a wait on any other.
const MAX_ABANDONED_PER_KEY: usize = 4;

/// Most looks given up on and still blocked in the whole process: a safety net
/// above the per-window limit, against a desktop where very many windows hang.
const MAX_ABANDONED_IN_ALL: usize = 24;

/// How often a look being waited for asks whether the operator stopped the
/// wait. A stop is acted on within this, not at the look's limit.
const STOP_SLICE: Duration = Duration::from_millis(100);

/// The looks given up on that have not returned yet, by what they looked at
/// (a window's id, or the kind of look when it is not of one window).
struct Stuck {
    looks: Mutex<Vec<(String, usize)>>,
}

impl Stuck {
    const fn new() -> Stuck {
        Stuck {
            looks: Mutex::new(Vec::new()),
        }
    }

    fn held(&self) -> MutexGuard<'_, Vec<(String, usize)>> {
        self.looks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How many looks at `key` are still blocked.
    fn of(&self, key: &str) -> usize {
        self.held()
            .iter()
            .find(|(k, _)| k == key)
            .map_or(0, |(_, n)| *n)
    }

    /// How many are still blocked, whatever they looked at.
    fn all(&self) -> usize {
        self.held().iter().map(|(_, n)| *n).sum()
    }

    fn add(&self, key: &str) {
        let mut held = self.held();
        match held.iter_mut().find(|(k, _)| k == key) {
            Some((_, n)) => *n += 1,
            None => held.push((key.to_string(), 1)),
        }
    }

    fn remove(&self, key: &str) {
        let mut held = self.held();
        if let Some(at) = held.iter().position(|(k, _)| k == key) {
            held[at].1 = held[at].1.saturating_sub(1);
            if held[at].1 == 0 {
                held.remove(at);
            }
        }
    }
}

/// Looks given up on that have not returned yet.
static STUCK: Stuck = Stuck::new();

/// `work`, run on a thread of its own and waited for at most `limit`, or until
/// `stopped` says the operator stopped the wait (asked every [`STOP_SLICE`]).
///
/// On time it is `work`'s answer. Past `limit`, or at the stop, the thread is
/// left to finish by itself (it cannot be stopped from outside: it is inside a
/// call the operating system has not returned from) and the answer is
/// [`UNANSWERED`], counted against `key` in `stuck` until the thread does
/// return. The wait that asked ends as stopped, not as unanswered, because it
/// asks the stop first.
fn bounded<T: Send + 'static>(
    what: &str,
    key: &str,
    limit: Duration,
    stuck: &'static Stuck,
    stopped: &dyn Fn() -> bool,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    bounded_with(what, key, limit, stuck, stopped, &|_| {}, work)
}

/// A point in giving up on a look, where a test may let the look return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gap {
    /// The wait has decided to give up and has not yet settled with the look.
    Before,
    /// The look is settled as given up on and counted.
    After,
}

/// [`bounded`], with `gap` called at each [`Gap`]: production passes nothing;
/// a test makes the look return exactly there.
fn bounded_with<T: Send + 'static>(
    what: &str,
    key: &str,
    limit: Duration,
    stuck: &'static Stuck,
    stopped: &dyn Fn() -> bool,
    gap: &dyn Fn(Gap),
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (here, everywhere) = (stuck.of(key), stuck.all());
    if here >= MAX_ABANDONED_PER_KEY || everywhere >= MAX_ABANDONED_IN_ALL {
        return Err(format!(
            "{UNANSWERED}: {here} earlier looks at this are still blocked in the \
             accessibility layer ({everywhere} in all), so none was started to {what}"
        ));
    }
    // 0 running, 1 returned in time, 2 given up on.
    let state = Arc::new(AtomicU8::new(0));
    let (send, receive) = mpsc::channel();
    let theirs = Arc::clone(&state);
    let owned_key = key.to_string();
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
                stuck.remove(&owned_key);
            }
            let _ = send.send(answer);
        });
    if let Err(e) = spawned {
        return Err(format!("could not start a look to {what}: {e}"));
    }
    let end = Instant::now() + limit;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match receive.recv_timeout(left.min(STOP_SLICE)) {
            Ok(answer) => return answer,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(format!("the look to {what} ended without an answer"))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let why = if stopped() {
            "the wait was stopped".to_string()
        } else if Instant::now() >= end {
            format!(
                "within {} ms (a window that is closing or not responding can do this)",
                limit.as_millis()
            )
        } else {
            continue;
        };
        gap(Gap::Before);
        // COUNT FIRST, then settle with the look, and undo the count if the look
        // had returned. The look's thread takes itself off the count only
        // after it has seen the settlement, so its removal always follows this
        // addition: counting after the settlement let a thread returning in
        // between remove what was not yet added, and the entry then stayed
        // for the life of the process.
        stuck.add(key);
        return if state
            .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            gap(Gap::After);
            Err(format!("{UNANSWERED} to {what}: {why}"))
        } else {
            // It returned in the instant between the timeout and this, and
            // took no part in the count.
            stuck.remove(key);
            receive
                .recv()
                .unwrap_or_else(|_| Err(format!("the look to {what} ended without an answer")))
        };
    }
}

/// What a look at nothing in particular is counted against.
const ANYWHERE: &str = "(the desktop)";

/// The desktop as this process sees it, with a clock that starts now.
struct Live<'a> {
    started: Instant,
    /// When the wait's `timeout_ms` is up. A look may run to this and
    /// [`LOOK_GRACE`] beyond it.
    deadline: Instant,
    /// The operator's stop, asked while a look is waited for.
    stopped: &'a dyn Fn() -> bool,
}

impl<'a> Live<'a> {
    fn new(budget: &Budget, stopped: &'a dyn Fn() -> bool) -> Live<'a> {
        let started = Instant::now();
        Live {
            started,
            deadline: started + Duration::from_millis(budget.timeout_ms),
            stopped,
        }
    }

    /// `work` as one look at `key` (a window's id, or [`ANYWHERE`]), held to
    /// the deadline and the grace after it, and to the operator's stop.
    fn within<T: Send + 'static>(
        &self,
        what: &str,
        key: &str,
        work: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let limit = (self.deadline + LOOK_GRACE).saturating_duration_since(Instant::now());
        bounded(what, key, limit, &STUCK, self.stopped, work)
    }
}

fn text_of(e: impl ToString) -> String {
    e.to_string()
}

impl Env for Live<'_> {
    fn now_ms(&mut self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn sleep_ms(&mut self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }

    fn windows(&mut self) -> Result<Vec<WindowInfo>, String> {
        // Not the windows of a blocked application: they are not listed, and a
        // selector must not be able to find one by its title.
        let all = self.within("list the windows", ANYWHERE, listed)?;
        Ok(all
            .into_iter()
            .filter(|w| crate::limits::get().blocked(&w.app).is_none())
            .collect())
    }

    fn window_alive(&mut self, id: &str) -> Option<bool> {
        uia::window_exists(id)
    }

    fn focused_window(&mut self) -> Result<WindowInfo, String> {
        let window = self.within("find the focused window", ANYWHERE, || {
            uia::window_info(None).map_err(text_of)
        })?;
        refuse_blocked(&window)?;
        Ok(window)
    }

    fn tree(&mut self, window: &str, depth: Option<u32>) -> Result<Tree, String> {
        let id = window.to_string();
        // Counted against this window, so a window that hangs stops the looks
        // at it and at no other.
        let key = window.to_string();
        let (info, tree) = self.within("walk the window", &key, move || {
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
        let key = window.clone().unwrap_or_else(|| ANYWHERE.to_string());
        self.within("capture the screen", &key, move || {
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
    look_until(&condition, &budget, stopped, None)
}

/// `watch`: the same look, for hours, with a note for the operator's hook.
///
/// **Why a tool of its own and not an option of `wait_for`.** `wait_for` is held
/// to 60 seconds, polled every 50 ms to 2 s, and that bound is a safety property
/// its tests pin: a call that is supposed to return in moments. A watch is the
/// opposite promise (it holds the call open until something happens, up to a
/// day, and looks gently), so it takes its own limits ([`Budget::watch`]) and
/// its own description, which says plainly that it blocks. The look itself is
/// the same code ([`look_until`]).
///
/// `message` is carried into the reply and so into the operator's post-tool
/// hook, which is how a watch notifies: see `Server::tell_post_hooks`.
///
/// A watch of the SCREEN (`screen_changes`) looks every five seconds unless told
/// otherwise ([`Budget::watch_screen`]), not every second: each look is a capture
/// and a hash of the whole screen.
///
/// **A watch holds the server for its whole duration.** The server answers one
/// call at a time on one thread, so while a watch runs no other call is served;
/// it ends when the condition holds, its time runs out, or the operator stops
/// the desktop (`incu stop`). **Nothing else ends it**: end of input on stdin does
/// not (a watch that was the last request a client sent is still wanted), so a
/// watch whose client has gone lasts until its deadline (1 hour unless asked for
/// more, 24 at most) or `incu stop`.
///
/// Called by `Server::run_tool`.
pub(super) fn watch(args: &Value, stopped: &dyn Fn() -> bool) -> Result<Vec<Value>, String> {
    let condition = condition_of(args)?;
    let budget = watch_budget(
        &condition,
        whole(args, "timeout_ms")?,
        whole(args, "poll_ms")?,
    );
    let message = message_arg(args)?;
    look_until(&condition, &budget, stopped, message)
}

/// How long a `watch` of `condition` may run and how often it looks: a look at
/// the screen is the slow one, so it is the one given the slower default.
fn watch_budget(condition: &Condition, timeout: Option<u64>, poll: Option<u64>) -> Budget {
    match condition {
        Condition::Screen { .. } => Budget::watch_screen(timeout, poll),
        _ => Budget::watch(timeout, poll),
    }
}

/// The most of a `watch` message that is kept, in characters.
const MAX_MESSAGE_CHARS: usize = 200;

/// What the watch is to say when it fires, if the call gave one.
fn message_arg(args: &Value) -> Result<Option<String>, String> {
    match args.get("message") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(m)) if m.chars().count() <= MAX_MESSAGE_CHARS => Ok(Some(m.clone())),
        Some(Value::String(_)) => Err(format!("message is over {MAX_MESSAGE_CHARS} characters")),
        Some(_) => Err("message must be text".to_string()),
    }
}

/// Look for `condition` on the live desktop within `budget` and render the
/// reply. A stop is a refusal, not a reply.
fn look_until(
    condition: &Condition,
    budget: &Budget,
    stopped: &dyn Fn() -> bool,
    message: Option<String>,
) -> Result<Vec<Value>, String> {
    let mut env = Live::new(budget, stopped);
    let outcome = wait::run(&mut env, condition, budget, stopped)?;
    if matches!(outcome, Outcome::Stopped { .. }) {
        return Err(crate::safety::Refusal::Stopped.message());
    }
    let mut reply = outcome.to_json(condition, budget);
    if let Some(message) = message {
        reply["message"] = serde_json::json!(message);
    }
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

    /// A watch of the screen looks every five seconds, not every second, and
    /// anything else keeps its second; a poll the caller gives is still theirs.
    #[test]
    fn a_watch_of_the_screen_polls_every_five_seconds_by_default() {
        let screen = condition_of(&json!({"condition": "screen_changes"})).unwrap();
        let window =
            condition_of(&json!({"condition": "window_gone", "selector": "app=zz;window=none"}))
                .unwrap();
        assert_eq!(watch_budget(&screen, None, None).poll_ms, 5_000);
        assert_eq!(watch_budget(&window, None, None).poll_ms, 1_000);
        assert_eq!(watch_budget(&screen, None, Some(2_000)).poll_ms, 2_000);
        // The same bounds otherwise: an hour by default, a day at most.
        assert_eq!(
            watch_budget(&screen, None, None).timeout_ms,
            wait::DEFAULT_WATCH_MS
        );
        assert_eq!(
            watch_budget(&screen, Some(u64::MAX), None).timeout_ms,
            wait::MAX_WATCH_MS
        );
        // And wait_for is not touched: still 250 ms.
        assert_eq!(Budget::new(None, None).poll_ms, wait::DEFAULT_POLL_MS);
    }

    /// **A look that blocks cannot outlive its limit.** The live hang was a
    /// call that did not return for minutes; here the work sleeps far longer
    /// than the limit and `bounded` must hand back long before it returns.
    #[test]
    fn a_look_that_blocks_is_given_up_on_at_its_limit_not_waited_for() {
        static STUCK: Stuck = Stuck::new();
        let started = Instant::now();
        let answer = bounded(
            "walk the window",
            "w1",
            Duration::from_millis(150),
            &STUCK,
            &|| false,
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
        assert_eq!(STUCK.of("w1"), 1, "counted while it blocks");
    }

    /// **The operator's stop is acted on within a slice, not at the limit.** A
    /// 60 s look at a hung window, stopped at 300 ms, gives the caller back at
    /// about 300 ms (and the caller's wait asks the stop before its deadline,
    /// so it ends as stopped).
    #[test]
    fn a_stop_ends_the_wait_for_a_blocked_look_within_a_slice() {
        static STUCK: Stuck = Stuck::new();
        let stop_at = Instant::now() + Duration::from_millis(300);
        let started = Instant::now();
        let answer: Result<(), String> = bounded(
            "walk the window",
            "w1",
            Duration::from_secs(60),
            &STUCK,
            &|| Instant::now() >= stop_at,
            || {
                std::thread::sleep(Duration::from_secs(5));
                Ok(())
            },
        );
        let took = started.elapsed();
        let why = answer.unwrap_err();
        assert!(why.starts_with(UNANSWERED), "{why}");
        assert!(why.contains("stopped"), "{why}");
        assert!(
            took >= Duration::from_millis(300)
                && took < Duration::from_millis(300) + STOP_SLICE * 5,
            "stopped at 300 ms, returned at {took:?}"
        );
        assert_eq!(STUCK.of("w1"), 1);
    }

    #[test]
    fn a_look_given_up_on_is_taken_off_the_count_when_it_returns() {
        static STUCK: Stuck = Stuck::new();
        let answer = bounded(
            "walk",
            "w1",
            Duration::from_millis(50),
            &STUCK,
            &|| false,
            || {
                std::thread::sleep(Duration::from_millis(300));
                Ok(())
            },
        );
        assert!(answer.is_err());
        assert_eq!(STUCK.of("w1"), 1);
        let until = Instant::now() + Duration::from_secs(5);
        while STUCK.all() != 0 {
            assert!(Instant::now() < until, "the count was never given back");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Block (up to 5 s) until `flag` is set, then give the look's thread a
    /// moment to finish what it does after `work` returns.
    fn until_returned(flag: &std::sync::atomic::AtomicBool) {
        let by = Instant::now() + Duration::from_secs(5);
        while !flag.load(Ordering::SeqCst) {
            assert!(Instant::now() < by, "the look never returned");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    /// **A look that returns just as it is given up on leaves no entry behind.**
    /// The look returns AFTER the wait has settled with it as given up on (its
    /// thread then removes itself from the count) but before the wait would,
    /// in the old order, have added it: the removal then found nothing, the
    /// addition followed, and the entry stayed for good.
    #[test]
    fn a_look_returning_just_after_it_is_given_up_on_leaves_no_entry() {
        static STUCK: Stuck = Stuck::new();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (mine, theirs) = (Arc::clone(&done), Arc::clone(&done));
        let answer = bounded_with(
            "walk",
            "w1",
            Duration::from_millis(20),
            &STUCK,
            &|| false,
            &move |at| {
                if at == Gap::After {
                    until_returned(&mine);
                }
            },
            move || {
                std::thread::sleep(Duration::from_millis(100));
                theirs.store(true, Ordering::SeqCst);
                Ok(())
            },
        );
        assert!(answer.unwrap_err().starts_with(UNANSWERED));
        assert_eq!(STUCK.all(), 0, "{:?}", STUCK.held());
    }

    /// And a look that returns after the wait decided to give up but before it
    /// settled: its answer is returned, and nothing is left counted.
    #[test]
    fn a_look_returning_just_before_it_is_settled_gives_its_answer_and_no_entry() {
        static STUCK: Stuck = Stuck::new();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (mine, theirs) = (Arc::clone(&done), Arc::clone(&done));
        let answer = bounded_with(
            "walk",
            "w1",
            Duration::from_millis(20),
            &STUCK,
            &|| false,
            &move |at| {
                if at == Gap::Before {
                    until_returned(&mine);
                }
            },
            move || {
                std::thread::sleep(Duration::from_millis(100));
                theirs.store(true, Ordering::SeqCst);
                Ok(7)
            },
        );
        assert_eq!(answer.unwrap(), 7);
        assert_eq!(STUCK.all(), 0, "{:?}", STUCK.held());
    }

    /// Entries of one window are not taken off by another's look returning.
    #[test]
    fn a_returning_look_takes_off_only_its_own_windows_entry() {
        static STUCK: Stuck = Stuck::new();
        STUCK.add("other");
        let answer: Result<(), String> = bounded(
            "walk",
            "w1",
            Duration::from_secs(5),
            &STUCK,
            &|| false,
            || Ok(()),
        );
        assert!(answer.is_ok());
        assert_eq!((STUCK.of("other"), STUCK.of("w1")), (1, 0));
    }

    #[test]
    fn a_look_that_answers_in_time_gives_its_answer_or_its_error() {
        static STUCK: Stuck = Stuck::new();
        let ok = bounded(
            "list",
            "w1",
            Duration::from_secs(5),
            &STUCK,
            &|| false,
            || Ok(vec![1, 2]),
        );
        assert_eq!(ok.unwrap(), vec![1, 2]);
        let refused: Result<(), String> = bounded(
            "list",
            "w1",
            Duration::from_secs(5),
            &STUCK,
            &|| false,
            || Err("refused: blocked".to_string()),
        );
        assert_eq!(refused.unwrap_err(), "refused: blocked");
        assert_eq!(STUCK.all(), 0);
    }

    #[test]
    fn no_new_look_is_started_while_too_many_are_still_blocked_at_the_same_window() {
        static STUCK: Stuck = Stuck::new();
        for _ in 0..MAX_ABANDONED_PER_KEY {
            STUCK.add("hung");
        }
        let ran = Arc::new(AtomicU8::new(0));
        let flag = Arc::clone(&ran);
        let answer = bounded(
            "walk",
            "hung",
            Duration::from_secs(5),
            &STUCK,
            &|| false,
            move || {
                flag.store(1, Ordering::SeqCst);
                Ok(())
            },
        );
        let why = answer.unwrap_err();
        assert!(why.starts_with(UNANSWERED), "{why}");
        assert!(why.contains("still blocked"), "{why}");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(ran.load(Ordering::SeqCst), 0, "a look was started anyway");
    }

    /// **One hung program does not stop a wait on any other.** The limit on
    /// abandoned looks is per window: with a window's looks all stuck, a look
    /// at another window runs and answers.
    #[test]
    fn a_hung_window_does_not_stop_looks_at_another() {
        static STUCK: Stuck = Stuck::new();
        for _ in 0..MAX_ABANDONED_PER_KEY {
            STUCK.add("hung");
        }
        let other = bounded(
            "walk",
            "healthy",
            Duration::from_secs(5),
            &STUCK,
            &|| false,
            || Ok("walked"),
        );
        assert_eq!(other.unwrap(), "walked");
        // The hung one is still refused.
        assert!(bounded(
            "walk",
            "hung",
            Duration::from_secs(5),
            &STUCK,
            &|| false,
            || { Ok("walked") }
        )
        .is_err());
    }

    /// The process-wide net is above the per-window limit, and holds.
    #[test]
    fn the_net_over_all_windows_is_higher_than_one_windows_limit_and_holds() {
        const { assert!(MAX_ABANDONED_IN_ALL > MAX_ABANDONED_PER_KEY) };
        static STUCK: Stuck = Stuck::new();
        for at in 0..MAX_ABANDONED_IN_ALL {
            STUCK.add(&format!("window {at}"));
        }
        let why = bounded(
            "walk",
            "fresh",
            Duration::from_secs(5),
            &STUCK,
            &|| false,
            || Ok(()),
        )
        .unwrap_err();
        assert!(why.starts_with(UNANSWERED), "{why}");
        assert!(why.contains("in all"), "{why}");
    }

    #[test]
    fn the_live_deadline_is_the_budgets_timeout_from_now() {
        let never = || false;
        let live = Live::new(&Budget::new(Some(4_000), None), &never);
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
