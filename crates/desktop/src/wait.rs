//! `wait_for`: wait, bounded, until the screen is in a state the caller names.
//!
//! # Why this is the missing primitive
//!
//! `screenshot_after` sleeps a fixed `settle` time and hopes. That is a guess
//! at how long an application takes, and it is the guess a flow replayed without
//! a model cannot make (`docs/COMPUTER_USE.md`, replay): too short and the next
//! step runs against a dialog that is not there yet, too long and every step
//! pays for the slowest one. This waits for the thing itself: a control that
//! matches a selector appears or goes, a window with a title appears or goes,
//! or the picture of the screen changes. It reports what it found, or that it
//! timed out and what it saw last.
//!
//! # The bounds
//!
//! The caller gives `timeout_ms`; it is held to [`MAX_TIMEOUT_MS`] and the reply
//! says so. The polling interval is held to [`MIN_POLL_MS`]..=[`MAX_POLL_MS`].
//! No poll STARTS after the deadline, and the last one starts exactly at it, so
//! a condition that comes true at the deadline is not missed; a poll already
//! under way is not interrupted, so the call can end later than the timeout by
//! the length of one poll (a walk of a big window is the slow one). The stop
//! switch is asked between every poll and every [`SLICE_MS`] of sleeping.
//!
//! # Why a trait
//!
//! [`Env`] is everything the wait touches: the clock, the windows, a tree, a
//! picture. The live one is `mcp::wait_tool`; the tests here run the same code
//! against a scripted desktop with a virtual clock, so the timing rules are
//! tested to the millisecond and without sleeping.

use serde_json::{json, Value};

use crate::fingerprint::{self, Fingerprint};
use crate::selector::{selector_for, Selector};
use crate::uia::{Element, Tree, WindowInfo, NAMES_ARE_VALUES};

/// The timeout when the call gives none.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// The longest one call may wait. A longer request is held to this and says so.
pub const MAX_TIMEOUT_MS: u64 = 60_000;
/// How often the condition is checked when the call does not say.
pub const DEFAULT_POLL_MS: u64 = 250;
/// The fastest and slowest checking interval a call may ask for.
pub const MIN_POLL_MS: u64 = 50;
pub const MAX_POLL_MS: u64 = 2_000;
/// How long a `watch` waits when the call gives no time: one hour.
pub const DEFAULT_WATCH_MS: u64 = 60 * 60 * 1000;
/// The longest one `watch` may run: 24 hours. Longer is several watches.
pub const MAX_WATCH_MS: u64 = 24 * 60 * 60 * 1000;
/// How often a `watch` looks when the call does not say.
pub const DEFAULT_WATCH_POLL_MS: u64 = 1_000;
/// The fastest and slowest a `watch` may look. A watch is not waiting for a
/// dialog to settle, so it is held to a slower floor than `wait_for`'s.
pub const MIN_WATCH_POLL_MS: u64 = 250;
pub const MAX_WATCH_POLL_MS: u64 = 10_000;
/// Bits of the 128 that may differ before the screen counts as changed.
pub const DEFAULT_TOLERANCE: u32 = 4;
/// Sleeps are cut into slices this long so a stop is noticed within one.
const SLICE_MS: u64 = 100;
/// Look-alikes listed beside a match.
const OTHERS: usize = 4;

/// How long to wait and how often to look, after the bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    pub timeout_ms: u64,
    pub poll_ms: u64,
    /// What was changed from what was asked, for the reply.
    pub notes: Vec<String>,
}

/// The limits a kind of wait holds a request to.
struct Bounds {
    default_timeout: u64,
    max_timeout: u64,
    default_poll: u64,
    min_poll: u64,
    max_poll: u64,
}

/// `wait_for`: a few seconds, polled often.
const WAIT_BOUNDS: Bounds = Bounds {
    default_timeout: DEFAULT_TIMEOUT_MS,
    max_timeout: MAX_TIMEOUT_MS,
    default_poll: DEFAULT_POLL_MS,
    min_poll: MIN_POLL_MS,
    max_poll: MAX_POLL_MS,
};

/// `watch`: hours, polled gently, because nothing is waiting on the answer but
/// the person who asked to be told.
const WATCH_BOUNDS: Bounds = Bounds {
    default_timeout: DEFAULT_WATCH_MS,
    max_timeout: MAX_WATCH_MS,
    default_poll: DEFAULT_WATCH_POLL_MS,
    min_poll: MIN_WATCH_POLL_MS,
    max_poll: MAX_WATCH_POLL_MS,
};

impl Budget {
    /// `timeout_ms` and `poll_ms` as the call gave them (or did not), held to
    /// the bounds.
    #[must_use]
    pub fn new(timeout_ms: Option<u64>, poll_ms: Option<u64>) -> Budget {
        Self::within(&WAIT_BOUNDS, timeout_ms, poll_ms)
    }

    /// The same for a `watch`: up to [`MAX_WATCH_MS`], looking every
    /// [`DEFAULT_WATCH_POLL_MS`] unless told otherwise.
    #[must_use]
    pub fn watch(timeout_ms: Option<u64>, poll_ms: Option<u64>) -> Budget {
        Self::within(&WATCH_BOUNDS, timeout_ms, poll_ms)
    }

    fn within(bounds: &Bounds, timeout_ms: Option<u64>, poll_ms: Option<u64>) -> Budget {
        let (max, min_poll, max_poll) = (bounds.max_timeout, bounds.min_poll, bounds.max_poll);
        let mut notes = Vec::new();
        let asked = timeout_ms.unwrap_or(bounds.default_timeout);
        let timeout_ms = if asked > max {
            notes.push(format!(
                "timeout_ms {asked} is over the cap; waited at most {max} ms"
            ));
            max
        } else {
            asked
        };
        let wanted = poll_ms.unwrap_or(bounds.default_poll);
        let poll = wanted.clamp(min_poll, max_poll);
        if poll != wanted {
            notes.push(format!(
                "poll_ms {wanted} is outside {min_poll}..={max_poll}; polled every {poll} ms"
            ));
        }
        Budget {
            timeout_ms,
            poll_ms: poll,
            notes,
        }
    }
}

/// What an [`Env`] call's error begins with when the desktop did not answer in
/// time (a look given up on at its limit, see `mcp::wait_tool`).
///
/// It is not a failure of the wait: it is a look that saw nothing. The wait
/// goes on to its deadline, or ends there, as `timeout` with this as
/// `last_seen`, instead of ending as an error the moment one look is lost.
pub const UNANSWERED: &str = "the desktop did not answer";

/// What a wait touches. See the module header.
pub trait Env {
    /// Milliseconds since some fixed moment of this wait's own.
    fn now_ms(&mut self) -> u64;
    fn sleep_ms(&mut self, ms: u64);
    /// The windows there are to look at: those of blocked applications are not
    /// among them.
    fn windows(&mut self) -> Result<Vec<WindowInfo>, String>;
    /// Whether the window with this id still exists, when that can be told
    /// without asking the window anything (Windows: `IsWindow`): `Some(false)`
    /// is a window that has been destroyed, which no later look is made of.
    /// `None` when it cannot be told that way.
    ///
    /// A window that is closing is the one a look can block on: the
    /// accessibility layer asks the program that owns it, and that program is
    /// going away. So this is asked first and costs no such question.
    fn window_alive(&mut self, _id: &str) -> Option<bool> {
        None
    }
    /// The window keystrokes go to now.
    fn focused_window(&mut self) -> Result<WindowInfo, String>;
    /// A fresh walk of one window.
    ///
    /// `depth` is how deep, when the selector gave one; else the default one.
    /// The selector's `nth` and `path` count the controls of that walk
    /// (`uia::resolve`).
    fn tree(&mut self, window: &str, depth: Option<u32>) -> Result<Tree, String>;
    /// The fingerprint of the screen, or of one window's area.
    fn screen(&mut self, window: Option<&str>) -> Result<Fingerprint, String>;
}

/// What to wait for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// A control that matches `selector` is there (`appears`) or is not
    /// (`gone`). `window` is a window id; without it the selector's own
    /// `app`/`window` part names the window, and without that the window that
    /// has focus when the wait begins, which is then the one watched.
    Element {
        selector: Selector,
        window: Option<String>,
        appears: bool,
    },
    /// A window matching the selector's `app`/`window` is there, or is not.
    Window { selector: Selector, appears: bool },
    /// The screen (or `window`'s area) differs from `baseline` by more than
    /// `tolerance` bits; with no baseline, from how it is when the wait begins.
    Screen {
        baseline: Option<Fingerprint>,
        tolerance: u32,
        window: Option<String>,
    },
}

impl Condition {
    /// The word the call used.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Condition::Element { appears: true, .. } => "element_appears",
            Condition::Element { appears: false, .. } => "element_gone",
            Condition::Window { appears: true, .. } => "window_appears",
            Condition::Window { appears: false, .. } => "window_gone",
            Condition::Screen { .. } => "screen_changes",
        }
    }
}

/// What a satisfied wait found.
#[derive(Debug, Clone)]
pub enum Found {
    Element {
        window: WindowInfo,
        element: Box<Element>,
        /// The control part of a selector that picks it out of its window.
        selector: String,
        matches: usize,
        /// Others that fit, as selectors, when there are several.
        others: Vec<String>,
        /// The walk was cut, so more may lie beyond.
        truncated: bool,
    },
    ElementGone {
        why: String,
    },
    Windows(Vec<WindowInfo>),
    WindowGone,
    Screen {
        hash: Fingerprint,
        baseline: Fingerprint,
        distance: u32,
        tolerance: u32,
    },
}

/// How a wait ended.
#[derive(Debug, Clone)]
pub enum Outcome {
    Met {
        found: Found,
        waited_ms: u64,
        polls: u32,
    },
    TimedOut {
        waited_ms: u64,
        polls: u32,
        /// What the last look saw, in words.
        last: String,
        /// The last picture's fingerprint, for `screen_changes`: pass it back
        /// as `baseline` to go on waiting from where this left off.
        hash: Option<Fingerprint>,
    },
    Stopped {
        waited_ms: u64,
    },
}

/// One look at the condition.
enum Poll<T> {
    Ready(T),
    /// Not yet, and what was seen.
    Pending(String),
}

enum Waited<T> {
    Met(T, u64, u32),
    TimedOut(u64, u32, String),
    Stopped(u64),
}

/// Check, and while the answer is "not yet" and time is left, sleep and check
/// again: the loop every condition shares. The first check is at once.
fn wait_until<E: Env, T>(
    env: &mut E,
    budget: &Budget,
    stopped: &dyn Fn() -> bool,
    mut check: impl FnMut(&mut E) -> Result<Poll<T>, String>,
) -> Result<Waited<T>, String> {
    let start = env.now_ms();
    let mut polls = 0u32;
    loop {
        if stopped() {
            return Ok(Waited::Stopped(env.now_ms() - start));
        }
        polls += 1;
        let last = match check(env) {
            Ok(Poll::Ready(found)) => return Ok(Waited::Met(found, env.now_ms() - start, polls)),
            Ok(Poll::Pending(seen)) => seen,
            // A look the desktop did not answer is a look that saw nothing.
            Err(why) if why.starts_with(UNANSWERED) => why,
            Err(why) => return Err(why),
        };
        // The stop is asked BEFORE the deadline: a look that was waited for
        // until the deadline (or abandoned because the stop came) must end the
        // wait as stopped, not as a timeout the operator did not ask for.
        if stopped() {
            return Ok(Waited::Stopped(env.now_ms() - start));
        }
        let elapsed = env.now_ms() - start;
        if elapsed >= budget.timeout_ms {
            return Ok(Waited::TimedOut(elapsed, polls, last));
        }
        // The next look is at the interval, or at the deadline when that is
        // sooner: the last look is AT the deadline, not after it.
        let mut nap = budget.poll_ms.min(budget.timeout_ms - elapsed);
        while nap > 0 {
            if stopped() {
                return Ok(Waited::Stopped(env.now_ms() - start));
            }
            let slice = nap.min(SLICE_MS);
            env.sleep_ms(slice);
            nap -= slice;
        }
    }
}

/// Which window an element wait watches.
enum Target {
    /// This one, by id, whatever happens to focus.
    Pinned(String),
    /// Whichever the selector's `app`/`window` names, looked for again each time.
    Named,
}

/// What one look at an element condition saw.
enum Look {
    NoWindow,
    Seen(Box<Seen>),
}

/// A window that was there, its tree, and where in it the selector matched.
struct Seen {
    window: WindowInfo,
    tree: Tree,
    hits: Vec<usize>,
}

fn windows_named(env: &mut impl Env, selector: &Selector) -> Result<Vec<WindowInfo>, String> {
    let all = env.windows()?;
    Ok(selector.windows_named(&all).into_iter().cloned().collect())
}

/// The window the wait watches, decided once at the start.
///
/// A window named by id that is not there is an error for `appears` (nothing
/// can appear in a window that does not exist) and an answer for `gone`: a
/// pinned window that has closed is exactly what its control being gone looks
/// like, and the id of one that closed a moment before the call is the usual
/// case (the click that closed it was the last call).
fn target_of(
    env: &mut impl Env,
    selector: &Selector,
    window: Option<&str>,
    appears: bool,
) -> Result<Target, String> {
    if let Some(id) = window {
        let alive = env.window_alive(id);
        // Asked of nothing when the window is known to be destroyed.
        let listed = if alive == Some(false) {
            None
        } else {
            env.windows()?.into_iter().find(|w| w.id == id)
        };
        let there = listed.is_some() || alive == Some(true);
        if !there {
            if appears {
                return Err(format!(
                    "no window has the id {id}; take one from list_windows"
                ));
            }
            return Ok(Target::Pinned(id.to_string()));
        }
        if let Some(w) = &listed {
            if selector.has_window_part() && !selector.matches_window(w) {
                return Err(format!(
                    "window {id} is \"{}\" ({}), which the selector's window part `{}` does not name",
                    w.title,
                    w.app,
                    selector.render_window_part()
                ));
            }
        }
        return Ok(Target::Pinned(id.to_string()));
    }
    if selector.has_window_part() {
        return Ok(Target::Named);
    }
    // No window said: the one in front NOW. Watched by id from here on, so a
    // change of focus mid-wait is not taken for the control going away.
    Ok(Target::Pinned(env.focused_window()?.id))
}

fn look(env: &mut impl Env, target: &Target, selector: &Selector) -> Result<Look, String> {
    let window = match target {
        Target::Pinned(id) => {
            // Destroyed: nothing to ask. This is the cheap, certain answer
            // that ends an `element_gone` wait on a window that closed.
            if env.window_alive(id) == Some(false) {
                return Ok(Look::NoWindow);
            }
            env.windows()?.into_iter().find(|w| &w.id == id)
        }
        Target::Named => {
            let mut named = windows_named(env, selector)?;
            match named.len() {
                0 => None,
                1 => Some(named.remove(0)),
                n => {
                    return Err(format!(
                        "`{}` matches {n} windows, so it does not say which; pass `window` \
                         (an id from list_windows) or make the app/window more specific",
                        selector.render_window_part()
                    ))
                }
            }
        }
    };
    let Some(window) = window else {
        return Ok(Look::NoWindow);
    };
    let tree = match env.tree(&window.id, selector.depth) {
        Ok(tree) => tree,
        // Nothing was seen, and asking again would only wait for the same
        // answer: let the wait decide (`wait_until`).
        Err(why) if why.starts_with(UNANSWERED) => return Err(why),
        Err(why) => {
            // A window that closes while it is being walked fails the walk:
            // that is the window going away, which is an answer, not a
            // failure. Only a window that is still there makes it an error.
            if env.window_alive(&window.id) == Some(false) {
                return Ok(Look::NoWindow);
            }
            let still_there = env.windows()?.iter().any(|w| w.id == window.id);
            if still_there {
                return Err(why);
            }
            return Ok(Look::NoWindow);
        }
    };
    let mut hits = selector.candidates(&tree.elements);
    if let Some(nth) = selector.nth {
        hits = hits.get(nth as usize).copied().into_iter().collect();
    }
    Ok(Look::Seen(Box::new(Seen { window, tree, hits })))
}

fn cut_note(tree: &Tree) -> String {
    if tree.truncated {
        " (the tree was cut, so more may lie beyond)".to_string()
    } else {
        String::new()
    }
}

/// `depth` is the walk's, as the wait's selector gave it: the selectors written
/// for what was found are counted in that walk, so they carry it (see
/// [`Selector::depth`]).
fn found_element(window: WindowInfo, tree: Tree, hits: &[usize], depth: Option<u32>) -> Found {
    let first = hits[0];
    let written = |i: usize| {
        Selector {
            depth,
            ..selector_for(&tree.elements, i, NAMES_ARE_VALUES)
        }
        .render_element_part()
    };
    let others = hits
        .iter()
        .skip(1)
        .take(OTHERS)
        .map(|&i| written(i))
        .collect();
    Found::Element {
        selector: written(first),
        element: Box::new(tree.elements[first].clone()),
        matches: hits.len(),
        others,
        truncated: tree.truncated,
        window,
    }
}

fn element_check(
    env: &mut impl Env,
    target: &Target,
    selector: &Selector,
    appears: bool,
) -> Result<Poll<Found>, String> {
    Ok(match (look(env, target, selector)?, appears) {
        (Look::NoWindow, true) => Poll::Pending("the window is not there".to_string()),
        (Look::NoWindow, false) => Poll::Ready(Found::ElementGone {
            why: "its window is not there".to_string(),
        }),
        (Look::Seen(seen), true) => {
            let Seen { window, tree, hits } = *seen;
            if hits.is_empty() {
                Poll::Pending(format!(
                    "window \"{}\" has {} elements, none matches{}",
                    window.title,
                    tree.elements.len(),
                    cut_note(&tree)
                ))
            } else {
                Poll::Ready(found_element(window, tree, &hits, selector.depth))
            }
        }
        (Look::Seen(seen), false) => {
            let Seen { window, tree, hits } = *seen;
            if !hits.is_empty() {
                Poll::Pending(format!(
                    "{} element(s) in window \"{}\" still match",
                    hits.len(),
                    window.title
                ))
            } else if tree.truncated {
                // Absent from the part that was walked is not absent.
                Poll::Pending(format!(
                    "no match in window \"{}\", but the tree was cut, so its absence cannot be \
                     told",
                    window.title
                ))
            } else {
                Poll::Ready(Found::ElementGone {
                    why: "no element matches".to_string(),
                })
            }
        }
    })
}

fn window_check(
    env: &mut impl Env,
    selector: &Selector,
    appears: bool,
) -> Result<Poll<Found>, String> {
    let named = windows_named(env, selector)?;
    Ok(match (named.is_empty(), appears) {
        (false, true) => Poll::Ready(Found::Windows(named)),
        (true, true) => Poll::Pending("no window matches yet".to_string()),
        (true, false) => Poll::Ready(Found::WindowGone),
        (false, false) => Poll::Pending(format!("{} window(s) still match", named.len())),
    })
}

fn into_outcome(waited: Waited<Found>, hash: Option<Fingerprint>) -> Outcome {
    match waited {
        Waited::Met(found, waited_ms, polls) => Outcome::Met {
            found,
            waited_ms,
            polls,
        },
        Waited::TimedOut(waited_ms, polls, last) => Outcome::TimedOut {
            waited_ms,
            polls,
            last,
            hash,
        },
        Waited::Stopped(waited_ms) => Outcome::Stopped { waited_ms },
    }
}

/// Wait for `condition`. `Err` is a refusal or a failure of the desktop (a
/// selector that names two windows, a window that cannot be walked); a wait
/// that ran out of time is [`Outcome::TimedOut`], not an error.
pub fn run<E: Env>(
    env: &mut E,
    condition: &Condition,
    budget: &Budget,
    stopped: &dyn Fn() -> bool,
) -> Result<Outcome, String> {
    match condition {
        Condition::Element {
            selector,
            window,
            appears,
        } => {
            selector.require_element()?;
            let began = env.now_ms();
            let target = match target_of(env, selector, window.as_deref(), *appears) {
                Ok(target) => target,
                // A stop that ended the look which picks the window is the
                // stop, the same refusal as one during the polling.
                Err(why) if why.starts_with(UNANSWERED) && stopped() => {
                    return Ok(Outcome::Stopped {
                        waited_ms: env.now_ms() - began,
                    })
                }
                Err(why) => return Err(why),
            };
            let waited = wait_until(env, budget, stopped, |env| {
                element_check(env, &target, selector, *appears)
            })?;
            Ok(into_outcome(waited, None))
        }
        Condition::Window { selector, appears } => {
            selector.require_window_only()?;
            let waited = wait_until(env, budget, stopped, |env| {
                window_check(env, selector, *appears)
            })?;
            Ok(into_outcome(waited, None))
        }
        Condition::Screen {
            baseline,
            tolerance,
            window,
        } => {
            let mut from = *baseline;
            let mut last = None;
            let waited = wait_until(env, budget, stopped, |env| {
                let now = env.screen(window.as_deref())?;
                last = Some(now);
                let Some(base) = from else {
                    // The baseline is how it is as the wait begins.
                    from = Some(now);
                    return Ok(Poll::Pending("took the baseline".to_string()));
                };
                let distance = fingerprint::distance(base, now);
                Ok(if distance > *tolerance {
                    Poll::Ready(Found::Screen {
                        hash: now,
                        baseline: base,
                        distance,
                        tolerance: *tolerance,
                    })
                } else {
                    Poll::Pending(format!(
                        "the screen is {distance} bit(s) from the baseline; it counts as \
                         changed above {tolerance}"
                    ))
                })
            })?;
            Ok(into_outcome(waited, last))
        }
    }
}

fn window_json(w: &WindowInfo) -> Value {
    json!({"id": w.id, "title": w.title, "app": w.app, "focused": w.focused})
}

impl Found {
    fn fields(&self) -> Vec<(&'static str, Value)> {
        match self {
            Found::Element {
                window,
                element,
                selector,
                matches,
                others,
                truncated,
            } => {
                let mut e = json!({
                    "role": element.role.word(),
                    "name": element.name,
                    "rect": element.rect,
                    "enabled": element.enabled,
                    "selector": selector,
                });
                if !element.automation_id.is_empty() {
                    e["automation_id"] = json!(element.automation_id);
                }
                let mut w = window_json(window);
                let window_part = Selector {
                    app: Some(window.app.clone()).filter(|a| !a.is_empty()),
                    window: Some(crate::selector::Text::Exact(window.title.clone())),
                    ..Selector::default()
                };
                w["selector"] = json!(window_part.render_window_part());
                let mut out = vec![
                    ("window", w),
                    ("element", e),
                    ("matches", json!(matches)),
                    ("unique", json!(*matches == 1)),
                ];
                if !others.is_empty() {
                    out.push(("others", json!(others)));
                }
                if *truncated {
                    out.push(("truncated", json!(true)));
                }
                out
            }
            Found::ElementGone { why } => vec![("why", json!(why))],
            Found::Windows(ws) => vec![(
                "windows",
                json!(ws.iter().map(window_json).collect::<Vec<_>>()),
            )],
            Found::WindowGone => vec![("why", json!("no window matches"))],
            Found::Screen {
                hash,
                baseline,
                distance,
                tolerance,
            } => vec![
                ("hash", json!(hash.hex())),
                ("baseline", json!(baseline.hex())),
                ("distance", json!(distance)),
                ("tolerance", json!(tolerance)),
            ],
        }
    }
}

impl Outcome {
    /// The reply: a JSON object whose `result` is `found` or `timeout`.
    ///
    /// A timeout is a REPLY, not a tool error: the desktop worked, and what the
    /// caller wanted did not happen in time. It is no refusal either, which is
    /// what the journal would call an error.
    #[must_use]
    pub fn to_json(&self, condition: &Condition, budget: &Budget) -> Value {
        let mut out = serde_json::Map::new();
        let mut put = |k: &str, v: Value| {
            out.insert(k.to_string(), v);
        };
        match self {
            Outcome::Met {
                found,
                waited_ms,
                polls,
            } => {
                put("result", json!("found"));
                put("condition", json!(condition.name()));
                put("waited_ms", json!(waited_ms));
                put("polls", json!(polls));
                for (k, v) in found.fields() {
                    put(k, v);
                }
            }
            Outcome::TimedOut {
                waited_ms,
                polls,
                last,
                hash,
            } => {
                put("result", json!("timeout"));
                put("condition", json!(condition.name()));
                put("waited_ms", json!(waited_ms));
                put("polls", json!(polls));
                put("last_seen", json!(last));
                if let Some(h) = hash {
                    put("hash", json!(h.hex()));
                }
            }
            Outcome::Stopped { waited_ms } => {
                put("result", json!("stopped"));
                put("condition", json!(condition.name()));
                put("waited_ms", json!(waited_ms));
            }
        }
        put("timeout_ms", json!(budget.timeout_ms));
        put("poll_ms", json!(budget.poll_ms));
        if !budget.notes.is_empty() {
            put("notes", json!(budget.notes));
        }
        Value::Object(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uia::Role;
    use crate::Rect;
    use std::cell::Cell;

    fn rect() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        }
    }

    fn window(id: &str, app: &str, title: &str, focused: bool) -> WindowInfo {
        WindowInfo {
            id: id.to_string(),
            title: title.to_string(),
            app: app.to_string(),
            pid: 1,
            focused,
            rect: rect(),
            has_tree: true,
        }
    }

    fn button(id: u32, name: &str) -> Element {
        Element {
            id,
            depth: 1,
            role: Role::Button,
            name: name.to_string(),
            rect: rect(),
            enabled: true,
            secret: false,
            automation_id: String::new(),
        }
    }

    type At<T> = Box<dyn Fn(u64) -> T>;

    /// A desktop that is a function of time, on a clock that only moves when
    /// something sleeps or something slow is asked.
    struct Fake {
        now: u64,
        /// What each tree or picture costs, in virtual milliseconds.
        cost: u64,
        windows: At<Vec<WindowInfo>>,
        /// window id -> its tree at a time.
        elements: At<Vec<Element>>,
        truncated: bool,
        /// From this time on a walk fails, as one does of a closing window.
        tree_fails_from: Option<u64>,
        /// Whether the window exists, by time, for a desktop that can tell.
        alive: Option<At<bool>>,
        /// A walk that blocks this many virtual ms and is then given up on.
        tree_blocks_ms: Option<u64>,
        /// Set when a walk blocks: the operator's stop arrives during it.
        stop_during_block: Option<std::rc::Rc<std::cell::Cell<bool>>>,
        /// The window list is given up on, with the stop arriving during it.
        listing_stopped: Option<std::rc::Rc<std::cell::Cell<bool>>>,
        windows_calls: u32,
        screen: At<u128>,
        sleeps: Vec<u64>,
        trees: u32,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                now: 0,
                cost: 0,
                windows: Box::new(|_| vec![window("1", "app", "Doc", true)]),
                elements: Box::new(|_| Vec::new()),
                truncated: false,
                tree_fails_from: None,
                alive: None,
                tree_blocks_ms: None,
                stop_during_block: None,
                listing_stopped: None,
                windows_calls: 0,
                screen: Box::new(|_| 0),
                sleeps: Vec::new(),
                trees: 0,
            }
        }
    }

    impl Env for Fake {
        fn now_ms(&mut self) -> u64 {
            self.now
        }
        fn sleep_ms(&mut self, ms: u64) {
            self.sleeps.push(ms);
            self.now += ms;
        }
        fn windows(&mut self) -> Result<Vec<WindowInfo>, String> {
            self.windows_calls += 1;
            if let Some(stop) = &self.listing_stopped {
                stop.set(true);
                return Err(format!(
                    "{UNANSWERED} to list the windows: the wait was stopped"
                ));
            }
            Ok((self.windows)(self.now))
        }
        fn window_alive(&mut self, _: &str) -> Option<bool> {
            self.alive.as_ref().map(|alive| alive(self.now))
        }
        fn focused_window(&mut self) -> Result<WindowInfo, String> {
            (self.windows)(self.now)
                .into_iter()
                .find(|w| w.focused)
                .ok_or_else(|| "nothing has focus".to_string())
        }
        fn tree(&mut self, window: &str, _: Option<u32>) -> Result<Tree, String> {
            self.trees += 1;
            if let Some(blocked) = self.tree_blocks_ms {
                if let Some(stop) = &self.stop_during_block {
                    stop.set(true);
                }
                self.now += blocked;
                return Err(format!(
                    "{UNANSWERED} to walk the window within {blocked} ms"
                ));
            }
            if self.tree_fails_from.is_some_and(|t| self.now >= t) {
                return Err("no desktop element: Unspecified error".to_string());
            }
            let at = self.now;
            self.now += self.cost;
            Ok(Tree {
                root_name: "t".to_string(),
                window: window.to_string(),
                app: "app".to_string(),
                no_positions: None,
                elements: (self.elements)(at),
                truncated: self.truncated,
                offscreen: 0,
            })
        }
        fn screen(&mut self, _: Option<&str>) -> Result<Fingerprint, String> {
            let at = self.now;
            self.now += self.cost;
            Ok(Fingerprint((self.screen)(at)))
        }
    }

    fn never() -> bool {
        false
    }

    fn sel(text: &str) -> Selector {
        Selector::parse(text).unwrap()
    }

    fn appears(text: &str) -> Condition {
        Condition::Element {
            selector: sel(text),
            window: None,
            appears: true,
        }
    }

    fn gone(text: &str) -> Condition {
        Condition::Element {
            selector: sel(text),
            window: None,
            appears: false,
        }
    }

    fn budget(timeout: u64, poll: u64) -> Budget {
        Budget::new(Some(timeout), Some(poll))
    }

    // ---- the bounds ----

    #[test]
    fn the_defaults_are_five_seconds_every_quarter_second() {
        let b = Budget::new(None, None);
        assert_eq!((b.timeout_ms, b.poll_ms), (5_000, 250));
        assert!(b.notes.is_empty());
    }

    #[test]
    fn a_timeout_over_the_cap_is_held_to_it_and_says_so() {
        let b = Budget::new(Some(10 * MAX_TIMEOUT_MS), None);
        assert_eq!(b.timeout_ms, MAX_TIMEOUT_MS);
        assert_eq!(b.notes.len(), 1);
        assert!(b.notes[0].contains("over the cap"), "{:?}", b.notes);
        assert_eq!(Budget::new(Some(MAX_TIMEOUT_MS), None).notes.len(), 0);
        assert_eq!(Budget::new(Some(u64::MAX), None).timeout_ms, MAX_TIMEOUT_MS);
    }

    #[test]
    fn the_polling_interval_is_held_between_its_bounds() {
        assert_eq!(Budget::new(None, Some(1)).poll_ms, MIN_POLL_MS);
        assert_eq!(Budget::new(None, Some(0)).poll_ms, MIN_POLL_MS);
        assert_eq!(Budget::new(None, Some(1_000_000)).poll_ms, MAX_POLL_MS);
        assert_eq!(Budget::new(None, Some(500)).poll_ms, 500);
        assert_eq!(Budget::new(None, Some(1)).notes.len(), 1);
    }

    #[test]
    fn a_watch_defaults_to_an_hour_every_second_and_is_held_to_a_day() {
        let b = Budget::watch(None, None);
        assert_eq!(
            (b.timeout_ms, b.poll_ms),
            (DEFAULT_WATCH_MS, DEFAULT_WATCH_POLL_MS)
        );
        assert!(b.notes.is_empty());
        let long = Budget::watch(Some(MAX_WATCH_MS * 7), None);
        assert_eq!(long.timeout_ms, MAX_WATCH_MS);
        assert!(long.notes[0].contains("over the cap"), "{:?}", long.notes);
        assert_eq!(Budget::watch(Some(MAX_WATCH_MS), None).notes.len(), 0);
        assert_eq!(Budget::watch(None, Some(1)).poll_ms, MIN_WATCH_POLL_MS);
        assert_eq!(
            Budget::watch(None, Some(u64::MAX)).poll_ms,
            MAX_WATCH_POLL_MS
        );
        // The one-shot wait is untouched by any of it.
        assert_eq!(Budget::new(Some(u64::MAX), None).timeout_ms, MAX_TIMEOUT_MS);
    }

    #[test]
    fn a_long_watch_catches_a_window_that_appears_hours_later() {
        let two_hours = 2 * 60 * 60 * 1000;
        let mut env = Fake::new();
        env.windows = Box::new(move |t| {
            let mut all = vec![window("1", "app", "Doc", true)];
            if t >= two_hours {
                all.push(window("2", "mail", "Reminder", false));
            }
            all
        });
        let condition = Condition::Window {
            selector: sel("window~=Reminder"),
            appears: true,
        };
        let budget = Budget::watch(Some(3 * 60 * 60 * 1000), None);
        let outcome = run(&mut env, &condition, &budget, &never).unwrap();
        match outcome {
            Outcome::Met {
                waited_ms, polls, ..
            } => {
                assert_eq!(waited_ms, two_hours);
                // Once a second, not every 250 ms: 7201 looks, not 28801.
                assert!((7_000..7_400).contains(&polls), "{polls} looks");
            }
            other => panic!("{other:?}"),
        }
        assert!(
            env.sleeps.iter().all(|ms| *ms <= SLICE_MS),
            "a stop is still noticed within one slice"
        );
    }

    #[test]
    fn a_stop_ends_a_long_watch_at_once() {
        let mut env = Fake::new();
        let condition = Condition::Window {
            selector: sel("window~=Never"),
            appears: true,
        };
        let polled = Cell::new(0u32);
        let stopped = || {
            polled.set(polled.get() + 1);
            polled.get() > 50
        };
        let budget = Budget::watch(Some(MAX_WATCH_MS), None);
        let outcome = run(&mut env, &condition, &budget, &stopped).unwrap();
        assert!(matches!(outcome, Outcome::Stopped { .. }), "{outcome:?}");
        assert!(
            env.now < 60_000,
            "stopped within a minute of virtual time: {}",
            env.now
        );
    }

    // ---- the loop ----

    /// Checks at 0, 250, 500, 750 ...; true from `t` on.
    fn true_from(t: u64) -> impl FnMut(&mut Fake) -> Result<Poll<u64>, String> {
        move |env: &mut Fake| {
            let now = env.now_ms();
            Ok(if now >= t {
                Poll::Ready(now)
            } else {
                Poll::Pending(format!("not yet at {now}"))
            })
        }
    }

    #[test]
    fn a_condition_already_true_returns_at_once_without_sleeping() {
        let mut env = Fake::new();
        let got = wait_until(&mut env, &budget(5_000, 250), &never, true_from(0)).unwrap();
        assert!(matches!(got, Waited::Met(0, 0, 1)));
        assert!(env.sleeps.is_empty());
    }

    #[test]
    fn a_condition_that_comes_true_is_caught_at_the_next_poll() {
        let mut env = Fake::new();
        let got = wait_until(&mut env, &budget(5_000, 250), &never, true_from(700)).unwrap();
        // Looked at 0, 250, 500, 750.
        assert!(matches!(got, Waited::Met(750, 750, 4)), "{:?}", {
            match got {
                Waited::Met(a, b, c) => (a, b, c),
                _ => (0, 0, 0),
            }
        });
    }

    #[test]
    fn a_condition_that_never_holds_times_out_at_exactly_the_timeout() {
        let mut env = Fake::new();
        let got = wait_until(&mut env, &budget(1_000, 300), &never, true_from(u64::MAX)).unwrap();
        match got {
            Waited::TimedOut(waited, polls, last) => {
                assert_eq!(waited, 1_000);
                // 0, 300, 600, 900, then the last look AT 1000.
                assert_eq!(polls, 5);
                assert_eq!(last, "not yet at 1000");
            }
            _ => panic!("must time out"),
        }
    }

    #[test]
    fn a_condition_true_exactly_at_the_deadline_is_not_missed() {
        let mut env = Fake::new();
        let got = wait_until(&mut env, &budget(1_000, 300), &never, true_from(1_000)).unwrap();
        assert!(matches!(got, Waited::Met(1_000, 1_000, 5)));
    }

    #[test]
    fn a_zero_timeout_looks_once() {
        let mut env = Fake::new();
        let got = wait_until(&mut env, &budget(0, 250), &never, true_from(1)).unwrap();
        assert!(matches!(got, Waited::TimedOut(0, 1, _)));
        assert!(env.sleeps.is_empty());
        let mut env = Fake::new();
        let got = wait_until(&mut env, &budget(0, 250), &never, true_from(0)).unwrap();
        assert!(matches!(got, Waited::Met(0, 0, 1)));
    }

    #[test]
    fn no_look_starts_after_the_deadline_even_when_looking_is_slow() {
        // Each look takes 400 ms. Looks start at 0 and 650; the second ends at
        // 1050, past the 1000 deadline, so there is no third.
        let mut env = Fake::new();
        let got = wait_until(&mut env, &budget(1_000, 250), &never, |env: &mut Fake| {
            env.now += 400;
            Ok(Poll::<()>::Pending("slow".to_string()))
        })
        .unwrap();
        match got {
            Waited::TimedOut(waited, polls, _) => {
                assert_eq!(polls, 2);
                assert_eq!(waited, 1_050, "ends late by at most the look in flight");
            }
            _ => panic!("must time out"),
        }
    }

    #[test]
    fn sleeping_is_cut_into_slices_so_a_stop_is_noticed_within_one() {
        let mut env = Fake::new();
        let _ = wait_until(&mut env, &budget(2_000, 1_000), &never, true_from(u64::MAX));
        assert!(!env.sleeps.is_empty());
        assert!(
            env.sleeps.iter().all(|&s| s <= SLICE_MS),
            "{:?}",
            env.sleeps
        );
        assert_eq!(env.sleeps.iter().sum::<u64>(), 2_000);
    }

    #[test]
    fn a_stop_ends_the_wait_between_two_slices() {
        let mut env = Fake::new();
        let asked = Cell::new(0u32);
        let stop_after_a_few = || {
            asked.set(asked.get() + 1);
            asked.get() > 4
        };
        let got = wait_until(
            &mut env,
            &budget(60_000, 2_000),
            &stop_after_a_few,
            true_from(u64::MAX),
        )
        .unwrap();
        assert!(matches!(got, Waited::Stopped(_)));
        assert!(env.now < 1_000, "stopped promptly, not after the interval");
    }

    #[test]
    fn a_stop_before_the_first_look_looks_at_nothing() {
        let mut env = Fake::new();
        let mut looked = false;
        let got = wait_until(&mut env, &budget(1_000, 250), &|| true, |_: &mut Fake| {
            looked = true;
            Ok(Poll::<()>::Pending(String::new()))
        })
        .unwrap();
        assert!(matches!(got, Waited::Stopped(0)));
        assert!(!looked);
    }

    #[test]
    fn a_failing_look_ends_the_wait_with_its_error() {
        let mut env = Fake::new();
        let got = wait_until(&mut env, &budget(1_000, 250), &never, |_: &mut Fake| {
            Err::<Poll<()>, _>("the window cannot be walked".to_string())
        });
        assert_eq!(got.err().as_deref(), Some("the window cannot be walked"));
    }

    // ---- elements ----

    #[test]
    fn an_element_that_shows_up_later_is_found_with_a_selector_for_it() {
        let mut env = Fake::new();
        env.elements = Box::new(|t| {
            let mut e = vec![button(1, "Open")];
            if t >= 600 {
                e.push(button(2, "Save"));
            }
            e
        });
        let out = run(
            &mut env,
            &appears("role=button;name=Save"),
            &budget(5_000, 250),
            &never,
        )
        .unwrap();
        match out {
            Outcome::Met {
                found:
                    Found::Element {
                        selector,
                        matches,
                        window,
                        ..
                    },
                waited_ms,
                polls,
            } => {
                assert_eq!(selector, "role=button;name=Save");
                assert_eq!(matches, 1);
                assert_eq!(window.id, "1");
                assert_eq!(waited_ms, 750);
                assert_eq!(polls, 4);
            }
            other => panic!("expected found, got {other:?}"),
        }
    }

    #[test]
    fn an_element_that_never_comes_times_out_and_says_what_it_saw() {
        let mut env = Fake::new();
        env.elements = Box::new(|_| vec![button(1, "Open")]);
        let c = appears("name=Save");
        let out = run(&mut env, &c, &budget(1_000, 250), &never).unwrap();
        match &out {
            Outcome::TimedOut {
                waited_ms, last, ..
            } => {
                assert_eq!(*waited_ms, 1_000);
                assert!(last.contains("none matches"), "{last}");
            }
            other => panic!("expected timeout, got {other:?}"),
        }
        let json = out.to_json(&c, &budget(1_000, 250));
        assert_eq!(json["result"], "timeout");
        assert_eq!(json["timeout_ms"], 1_000);
    }

    #[test]
    fn several_matches_are_reported_as_several_and_not_resolved() {
        let mut env = Fake::new();
        env.elements = Box::new(|_| vec![button(1, "Save"), button(2, "Save")]);
        let out = run(&mut env, &appears("name=Save"), &budget(1_000, 250), &never).unwrap();
        let json = out.to_json(&appears("name=Save"), &budget(1_000, 250));
        assert_eq!(json["result"], "found");
        assert_eq!(json["matches"], 2);
        assert_eq!(json["unique"], false);
        assert_eq!(json["others"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn nth_waits_for_that_many_matches() {
        let mut env = Fake::new();
        env.elements = Box::new(|t| {
            let mut e = vec![button(1, "Row")];
            if t >= 500 {
                e.push(button(2, "Row"));
            }
            e
        });
        let out = run(
            &mut env,
            &appears("name=Row;nth=1"),
            &budget(5_000, 250),
            &never,
        )
        .unwrap();
        match out {
            Outcome::Met { waited_ms, .. } => assert_eq!(waited_ms, 500),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_element_that_goes_away_is_waited_for() {
        let mut env = Fake::new();
        env.elements = Box::new(|t| {
            if t < 400 {
                vec![button(1, "Loading")]
            } else {
                vec![button(2, "Done")]
            }
        });
        let out = run(&mut env, &gone("name=Loading"), &budget(5_000, 250), &never).unwrap();
        match out {
            Outcome::Met {
                found: Found::ElementGone { .. },
                waited_ms,
                ..
            } => assert_eq!(waited_ms, 500),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_element_that_stays_times_out_of_a_gone_wait() {
        let mut env = Fake::new();
        env.elements = Box::new(|_| vec![button(1, "Loading")]);
        let out = run(&mut env, &gone("name=Loading"), &budget(500, 250), &never).unwrap();
        match out {
            Outcome::TimedOut { last, .. } => assert!(last.contains("still match"), "{last}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_window_that_closes_takes_its_elements_with_it() {
        let mut env = Fake::new();
        env.windows = Box::new(|t| {
            if t < 300 {
                vec![window("1", "app", "Doc", true)]
            } else {
                vec![window("2", "app", "Other", true)]
            }
        });
        env.elements = Box::new(|_| vec![button(1, "OK")]);
        // Pinned to window 1 at the start (the focused one).
        let out = run(&mut env, &gone("name=OK"), &budget(5_000, 250), &never).unwrap();
        match out {
            Outcome::Met {
                found: Found::ElementGone { why },
                ..
            } => assert!(why.contains("window"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_walk_that_fails_because_the_window_closed_is_the_window_going_away() {
        let mut env = Fake::new();
        env.windows = Box::new(|t| {
            if t < 300 {
                vec![window("1", "app", "Doc", true)]
            } else {
                vec![]
            }
        });
        env.elements = Box::new(|_| vec![button(1, "OK")]);
        env.tree_fails_from = Some(300);
        let out = run(&mut env, &gone("name=OK"), &budget(5_000, 250), &never).unwrap();
        assert!(
            matches!(
                out,
                Outcome::Met {
                    found: Found::ElementGone { .. },
                    ..
                }
            ),
            "{out:?}"
        );
    }

    #[test]
    fn a_walk_that_fails_of_a_window_that_is_still_there_is_an_error() {
        let mut env = Fake::new();
        env.elements = Box::new(|_| vec![button(1, "OK")]);
        env.tree_fails_from = Some(0);
        let err = run(&mut env, &gone("name=OK"), &budget(5_000, 250), &never).unwrap_err();
        assert!(err.contains("Unspecified error"), "{err}");
    }

    /// **The window the caller pinned is destroyed mid-wait: `element_gone`
    /// is found at once.** The look that met a closing window is where the
    /// live wait hung: no look is made of a window known to be destroyed.
    #[test]
    fn a_pinned_window_destroyed_mid_wait_ends_element_gone_without_another_look() {
        let mut env = Fake::new();
        env.elements = Box::new(|_| vec![button(1, "Don't save")]);
        env.alive = Some(Box::new(|t| t < 300));
        let c = Condition::Element {
            selector: sel("role=button;name=Don't save"),
            window: Some("1".to_string()),
            appears: false,
        };
        let out = run(&mut env, &c, &budget(4_000, 250), &never).unwrap();
        match out {
            Outcome::Met {
                found: Found::ElementGone { why },
                waited_ms,
                ..
            } => {
                assert!(why.contains("window"), "{why}");
                assert_eq!(waited_ms, 500, "found at the first look after it closed");
            }
            other => panic!("{other:?}"),
        }
        let trees_before_it_closed = env.trees;
        assert!(trees_before_it_closed <= 2, "{trees_before_it_closed}");
    }

    #[test]
    fn a_pinned_window_already_destroyed_is_gone_at_once_and_nothing_can_appear_in_it() {
        let mut env = Fake::new();
        env.alive = Some(Box::new(|_| false));
        env.windows = Box::new(|_| vec![]);
        let gone_c = Condition::Element {
            selector: sel("role=button;name=OK"),
            window: Some("1".to_string()),
            appears: false,
        };
        let out = run(&mut env, &gone_c, &budget(4_000, 250), &never).unwrap();
        assert!(
            matches!(
                out,
                Outcome::Met {
                    found: Found::ElementGone { .. },
                    waited_ms: 0,
                    polls: 1,
                    ..
                }
            ),
            "{out:?}"
        );
        assert_eq!(env.trees, 0, "no walk of a window that is not there");
        assert_eq!(env.windows_calls, 0, "and not even a listing");
        // The same id, waiting for something to appear in it, is an error.
        let appears_c = Condition::Element {
            selector: sel("role=button;name=OK"),
            window: Some("1".to_string()),
            appears: true,
        };
        let err = run(&mut env, &appears_c, &budget(4_000, 250), &never).unwrap_err();
        assert!(err.contains("no window has the id"), "{err}");
    }

    /// An id that is not listed, on a desktop that cannot tell, is gone for
    /// `element_gone` as well: a closed window is not in the list.
    #[test]
    fn an_unlisted_pinned_window_is_gone_for_element_gone() {
        let mut env = Fake::new();
        env.windows = Box::new(|_| vec![window("2", "app", "Other", true)]);
        let c = Condition::Element {
            selector: sel("role=button;name=OK"),
            window: Some("1".to_string()),
            appears: false,
        };
        let out = run(&mut env, &c, &budget(4_000, 250), &never).unwrap();
        assert!(matches!(
            out,
            Outcome::Met {
                found: Found::ElementGone { .. },
                ..
            }
        ));
    }

    /// **A stop that comes during a blocked look ends the wait as stopped.**
    /// The look is given up on (by the stop, or at its limit) and the wait
    /// returns to its loop past the deadline: the stop is asked first, so the
    /// operator is told it stopped, not that it timed out.
    #[test]
    fn a_stop_during_a_blocked_look_is_a_stop_not_a_timeout() {
        let stop = std::rc::Rc::new(std::cell::Cell::new(false));
        let mut env = Fake::new();
        env.elements = Box::new(|_| vec![button(1, "OK")]);
        env.tree_blocks_ms = Some(3_000);
        env.stop_during_block = Some(std::rc::Rc::clone(&stop));
        let flag = std::rc::Rc::clone(&stop);
        let c = appears("name=OK");
        // The deadline (2 s) is past by the time the look is given up on.
        let out = run(&mut env, &c, &budget(2_000, 250), &move || flag.get()).unwrap();
        assert!(matches!(out, Outcome::Stopped { .. }), "{out:?}");
        assert_eq!(env.trees, 1, "no look after the stop");
    }

    /// A stop during the look that picks the window is the stop, not a plain
    /// error; an unanswered look with no stop is still not one.
    #[test]
    fn a_stop_during_the_window_picking_look_is_stopped_not_an_error() {
        let stop = std::rc::Rc::new(std::cell::Cell::new(false));
        let mut env = Fake::new();
        env.listing_stopped = Some(std::rc::Rc::clone(&stop));
        let flag = std::rc::Rc::clone(&stop);
        // Pinned to a window id: picking it lists the windows.
        let c = Condition::Element {
            selector: sel("name=OK"),
            window: Some("1".to_string()),
            appears: true,
        };
        let out = run(&mut env, &c, &budget(5_000, 250), &move || flag.get()).unwrap();
        assert!(matches!(out, Outcome::Stopped { .. }), "{out:?}");

        // The same look unanswered, with no stop: an error, as before.
        let mut env = Fake::new();
        env.listing_stopped = Some(std::rc::Rc::new(std::cell::Cell::new(false)));
        let err = run(&mut env, &c, &budget(5_000, 250), &never).unwrap_err();
        assert!(err.starts_with(UNANSWERED), "{err}");
    }

    /// **A look that blocks cannot keep the wait past its deadline.** The
    /// desktop gives up on such a look at its limit (`mcp::wait_tool`) and the
    /// wait treats it as a look that saw nothing: no second look is started
    /// after the deadline, and the reply is a timeout that says why.
    #[test]
    fn a_look_the_desktop_did_not_answer_is_a_timeout_not_a_hang_or_an_error() {
        let mut env = Fake::new();
        env.elements = Box::new(|_| vec![button(1, "OK")]);
        // Every walk takes 3 s of virtual time and is given up on.
        env.tree_blocks_ms = Some(3_000);
        let c = appears("name=OK");
        let out = run(&mut env, &c, &budget(4_000, 250), &never).unwrap();
        match out {
            Outcome::TimedOut {
                waited_ms,
                polls,
                last,
                ..
            } => {
                assert!(last.starts_with(UNANSWERED), "{last}");
                // Looks start at 0, 3250 (after its poll interval) and no
                // later: the one at 3250 ends at 6250, past the deadline,
                // and no third is begun.
                assert_eq!(polls, 2, "no look started after the deadline");
                assert_eq!(env.trees, 2);
                assert!(waited_ms < 4_000 + 3_000, "{waited_ms}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_look_that_is_never_answered_does_not_turn_an_element_gone_wait_into_gone() {
        let mut env = Fake::new();
        env.elements = Box::new(|_| vec![]);
        env.tree_blocks_ms = Some(500);
        let out = run(&mut env, &gone("name=OK"), &budget(1_000, 250), &never).unwrap();
        assert!(
            matches!(out, Outcome::TimedOut { .. }),
            "an unanswered look proves nothing: {out:?}"
        );
    }

    #[test]
    fn a_cut_tree_cannot_prove_an_element_gone() {
        let mut env = Fake::new();
        env.truncated = true;
        env.elements = Box::new(|_| vec![button(1, "Other")]);
        let out = run(&mut env, &gone("name=Loading"), &budget(500, 250), &never).unwrap();
        match out {
            Outcome::TimedOut { last, .. } => assert!(last.contains("cut"), "{last}"),
            other => panic!("a cut tree must not satisfy `gone`: {other:?}"),
        }
    }

    #[test]
    fn the_watched_window_is_the_one_in_front_at_the_start_not_whichever_is_later() {
        let mut env = Fake::new();
        // Focus moves to window 2 at once; the control exists "everywhere".
        env.windows = Box::new(|t| {
            vec![
                window("1", "app", "A", t < 100),
                window("2", "app", "B", t >= 100),
            ]
        });
        env.elements = Box::new(|_| vec![button(1, "OK")]);
        let out = run(&mut env, &appears("name=OK"), &budget(1_000, 250), &never).unwrap();
        match out {
            Outcome::Met {
                found: Found::Element { window, .. },
                ..
            } => assert_eq!(window.id, "1", "pinned at the start"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_window_id_must_exist_and_agree_with_the_selectors_window_part() {
        let mut env = Fake::new();
        let c = Condition::Element {
            selector: sel("name=OK"),
            window: Some("99".to_string()),
            appears: true,
        };
        let err = run(&mut env, &c, &budget(100, 50), &never).unwrap_err();
        assert!(err.contains("no window has the id 99"), "{err}");

        let c = Condition::Element {
            selector: sel("app=other;name=OK"),
            window: Some("1".to_string()),
            appears: true,
        };
        let err = run(&mut env, &c, &budget(100, 50), &never).unwrap_err();
        assert!(err.contains("does not name"), "{err}");
    }

    #[test]
    fn a_selector_naming_two_windows_is_refused_not_guessed() {
        let mut env = Fake::new();
        env.windows = Box::new(|_| {
            vec![
                window("1", "notepad", "a", true),
                window("2", "notepad", "b", false),
            ]
        });
        let c = appears("app=notepad;name=OK");
        let err = run(&mut env, &c, &budget(100, 50), &never).unwrap_err();
        assert!(err.contains("matches 2 windows"), "{err}");
    }

    #[test]
    fn a_selector_with_no_control_part_is_not_an_element_wait() {
        let mut env = Fake::new();
        let err = run(&mut env, &appears("app=notepad"), &budget(100, 50), &never).unwrap_err();
        assert!(err.contains("names no control"), "{err}");
        assert_eq!(env.trees, 0);
    }

    // ---- windows ----

    #[test]
    fn a_window_with_the_title_is_waited_for_and_listed_when_it_comes() {
        let mut env = Fake::new();
        env.windows = Box::new(|t| {
            let mut w = vec![window("1", "app", "Doc", true)];
            if t >= 300 {
                w.push(window("2", "app", "Save As", false));
            }
            w
        });
        let c = Condition::Window {
            selector: sel("window~=save as"),
            appears: true,
        };
        let out = run(&mut env, &c, &budget(5_000, 250), &never).unwrap();
        match out {
            Outcome::Met {
                found: Found::Windows(ws),
                waited_ms,
                ..
            } => {
                assert_eq!(ws.len(), 1);
                assert_eq!(ws[0].id, "2");
                assert_eq!(waited_ms, 500);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_window_that_closes_is_waited_for_and_one_already_gone_returns_at_once() {
        let mut env = Fake::new();
        env.windows = Box::new(|t| {
            if t < 400 {
                vec![window("2", "app", "Save As", false)]
            } else {
                vec![]
            }
        });
        let c = Condition::Window {
            selector: sel("window=Save As"),
            appears: false,
        };
        let out = run(&mut env, &c, &budget(5_000, 250), &never).unwrap();
        assert!(matches!(
            out,
            Outcome::Met {
                found: Found::WindowGone,
                waited_ms: 500,
                ..
            }
        ));
        let out = run(&mut env, &c, &budget(5_000, 250), &never).unwrap();
        assert!(matches!(
            out,
            Outcome::Met {
                found: Found::WindowGone,
                polls: 1,
                ..
            }
        ));
    }

    #[test]
    fn a_window_wait_takes_only_window_keys() {
        let mut env = Fake::new();
        let c = Condition::Window {
            selector: sel("app=x;role=button"),
            appears: true,
        };
        assert!(run(&mut env, &c, &budget(100, 50), &never)
            .unwrap_err()
            .contains("asks about a window"));
    }

    // ---- the screen ----

    fn screen(baseline: Option<u128>, tolerance: u32) -> Condition {
        Condition::Screen {
            baseline: baseline.map(Fingerprint),
            tolerance,
            window: None,
        }
    }

    #[test]
    fn the_screen_is_compared_with_how_it_was_when_the_wait_began() {
        let mut env = Fake::new();
        // Changes by 8 bits at 800.
        env.screen = Box::new(|t| if t >= 800 { 0xff } else { 0 });
        let c = screen(None, 4);
        let out = run(&mut env, &c, &budget(5_000, 250), &never).unwrap();
        match out {
            Outcome::Met {
                found:
                    Found::Screen {
                        distance,
                        baseline,
                        hash,
                        ..
                    },
                waited_ms,
                ..
            } => {
                assert_eq!(distance, 8);
                assert_eq!(baseline, Fingerprint(0));
                assert_eq!(hash, Fingerprint(0xff));
                assert_eq!(waited_ms, 1_000);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_change_within_the_tolerance_is_not_a_change() {
        let mut env = Fake::new();
        env.screen = Box::new(|t| if t >= 300 { 0b1111 } else { 0 });
        let c = screen(None, 4);
        let out = run(&mut env, &c, &budget(1_000, 250), &never).unwrap();
        match out {
            Outcome::TimedOut { hash, last, .. } => {
                assert_eq!(
                    hash,
                    Some(Fingerprint(0b1111)),
                    "the last picture is returned"
                );
                assert!(last.contains("4 bit(s)"), "{last}");
            }
            other => panic!("4 bits is within a tolerance of 4: {other:?}"),
        }
    }

    #[test]
    fn a_baseline_from_an_earlier_call_is_compared_at_the_first_look() {
        let mut env = Fake::new();
        env.screen = Box::new(|_| 0xffff);
        let out = run(&mut env, &screen(Some(0), 4), &budget(5_000, 250), &never).unwrap();
        assert!(matches!(
            out,
            Outcome::Met {
                waited_ms: 0,
                polls: 1,
                ..
            }
        ));
    }

    #[test]
    fn a_picture_that_takes_time_to_take_counts_against_the_timeout() {
        let mut env = Fake::new();
        env.cost = 300;
        env.screen = Box::new(|_| 0);
        let out = run(&mut env, &screen(None, 4), &budget(1_000, 250), &never).unwrap();
        match out {
            Outcome::TimedOut { waited_ms, .. } => {
                // The last look starts at the deadline and takes its 300.
                assert!((1_000..=1_300).contains(&waited_ms), "{waited_ms}")
            }
            other => panic!("{other:?}"),
        }
    }

    // ---- the reply ----

    #[test]
    fn a_found_element_reports_its_window_its_selector_and_the_bounds_used() {
        let mut env = Fake::new();
        env.elements = Box::new(|_| vec![button(7, "Save")]);
        let c = appears("name=Save");
        let b = Budget::new(Some(10 * MAX_TIMEOUT_MS), Some(1));
        let out = run(&mut env, &c, &b, &never).unwrap();
        let json = out.to_json(&c, &b);
        assert_eq!(json["result"], "found");
        assert_eq!(json["condition"], "element_appears");
        assert_eq!(json["element"]["role"], "button");
        assert_eq!(json["element"]["name"], "Save");
        assert_eq!(json["element"]["selector"], "role=button;name=Save");
        assert_eq!(json["window"]["id"], "1");
        assert_eq!(json["window"]["selector"], "app=app;window=Doc");
        assert_eq!(json["unique"], true);
        assert_eq!(json["timeout_ms"], MAX_TIMEOUT_MS);
        assert_eq!(json["poll_ms"], MIN_POLL_MS);
        assert_eq!(json["notes"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_stopped_wait_says_so() {
        let mut env = Fake::new();
        let c = appears("name=Save");
        let out = run(&mut env, &c, &budget(1_000, 250), &|| true).unwrap();
        assert!(matches!(out, Outcome::Stopped { waited_ms: 0 }));
        assert_eq!(out.to_json(&c, &budget(1_000, 250))["result"], "stopped");
    }
}
