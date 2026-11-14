//! What a run must be like to be kept, and how it is cleaned.
//!
//! The rules are Atticus's learned-shortcut rules (`atticus.py` `learn`,
//! `hands_replayable`), applied to a list of recorded tool calls:
//!
//! 1. **The phrase must not point at the screen.** "this" or "that" means
//!    whatever was on screen when it was said ([`super::words::is_deictic`]).
//! 2. **The run must not have failed.** ANY call that errored (a shell command,
//!    a refused `ui_tree`, a file that was not there: not only a desktop call),
//!    a record whose own outcome says it failed ([`check_record`]), or a
//!    `wait_for` that timed out or was stopped, is a run that did not do what it
//!    set out to; replaying it would repeat the failure.
//! 3. **Every kept step must be replayable on another day.** The only way to
//!    name a control that survives is a selector, so a step that uses an `eN`
//!    id (valid for one `ui_tree` only), a pixel coordinate (valid for one
//!    layout), or a window handle (valid for one window) is refused, and so is
//!    the whole routine: dropping that step would silently skip part of the
//!    task. `click`, `move_mouse`, `drag` and `focus_window` have no other form,
//!    so they are never replayable.
//!    A step that acts must also say WHERE: its selector carries an `app` or
//!    `window` part, and a keyboard step (`type_text`, `paste_text`, `key`)
//!    carries [`IN_WINDOW`]. Without one the step would act in whatever window
//!    is in front when it runs, which is how a live test typed into the wrong
//!    program. When the recorded keyboard step has none, the window of the step
//!    before it is used and the reply says so; the replay still refuses unless
//!    that window has the focus. `scroll` is never replayable: it takes no
//!    selector and scrolls under the pointer.
//! 4. **Reading steps are dropped.** `ui_tree`, `screenshot`, `list_windows`,
//!    `screen_info` and `cursor_position` are how the agent found its way; the
//!    replay needs only the actions. `wait_for` is kept, because it is what makes
//!    the replay wait for the screen instead of guessing how long it takes.
//!
//! Calls that are not desktop tools at all (a shell command, a file edit) are
//! left out and REPORTED in [`Cleaned::skipped`], so the agent knows that a
//! step it may have relied on is not in the routine.
//!
//! Last, the text of a step that types into a secure field, or that a recording
//! kept only as a placeholder, is taken out and made a parameter
//! ([`super::params`]): a routine never holds a secret.
//!
//! The same per-step check, [`check_step`], runs again before a replay, so a
//! hand-edited routines file cannot smuggle in a step the learner would have
//! refused.

use std::fmt;

use serde_json::Value;

use super::params::{names, parameterise, Made, Probe, MAX_PARAMS};
use super::source::{Call, Record};
use super::words::{is_deictic, word_set};
use super::{Step, MAX_STEPS, MAX_STEP_BYTES};
use crate::permission::{ACTING, OBSERVING};
use crate::selector::Selector;

/// The desktop tools that only look and take part in no replay.
pub const READING: &[&str] = &[
    "screen_info",
    "cursor_position",
    "screenshot",
    "ui_tree",
    "list_windows",
];

/// The tools a step may be, with the argument that names the control.
const REPLAYABLE: &[&str] = &[
    "wait_for",
    "click_element",
    "set_value",
    "element_action",
    "type_text",
    "paste_text",
    "key",
];

/// The tools whose control is named by a selector.
const NEEDS_SELECTOR: &[&str] = &["click_element", "set_value", "element_action"];

/// The tools that send keystrokes to whichever window has focus. They take no
/// selector, so a step names its window with [`IN_WINDOW`].
pub const KEYBOARD: &[&str] = &["type_text", "paste_text", "key"];

/// The step argument that names the window a keyboard step is for: a selector
/// with only an `app` and/or `window` part. It is the routine's own key, not the
/// tool's: the replay turns it into the tool's `window` (a handle, looked up
/// now), so the tool refuses unless that window really has the focus.
pub const IN_WINDOW: &str = "in_window";

/// Why a run or a step was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The phrase says "this" or "that".
    Deictic,
    /// The phrase has no words of its own (all filler).
    NoWords,
    /// A record's own outcome says the run failed, whatever its calls say.
    RecordFailed {
        /// The record.
        id: String,
        /// What it says.
        why: String,
    },
    /// A call in the run failed.
    RunFailed {
        /// Its 1-based position in the calls given.
        step: usize,
        /// The tool.
        tool: String,
        /// What went wrong.
        why: String,
    },
    /// A step cannot be replayed on another day.
    NotReplayable {
        /// Its 1-based position.
        step: usize,
        /// The tool.
        tool: String,
        /// Why not, and what to do instead.
        why: String,
    },
    /// Nothing is left once the reading steps are dropped.
    NoSteps,
    /// More steps than a routine may hold.
    TooLong(usize),
    /// More parameters than a routine may take.
    TooManyParams(usize),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Deictic => write!(
                f,
                "not learned: the phrase says \"this\" or \"that\", which means whatever was \
                 on screen when it was said. Give the routine a name that stands on its own"
            ),
            Refusal::NoWords => write!(
                f,
                "not learned: the phrase has no words of its own once filler words are \
                 ignored"
            ),
            Refusal::RecordFailed { id, why } => write!(
                f,
                "not learned: record {id} says its run failed ({why}). Only a run that worked \
                 is kept; do the task again, cleanly, and save that"
            ),
            Refusal::RunFailed { step, tool, why } => write!(
                f,
                "not learned: the run failed at call {step} ({tool}): {why}. Only a run \
                 that worked is kept; do the task again, cleanly, and save that"
            ),
            Refusal::NotReplayable { step, tool, why } => write!(
                f,
                "not learned: call {step} ({tool}) {why}. Only steps that name their control \
                 by selector can be replayed"
            ),
            Refusal::NoSteps => write!(
                f,
                "not learned: no desktop action is left once looking-only calls \
                 (ui_tree, screenshot, list_windows, ...) are dropped"
            ),
            Refusal::TooManyParams(n) => write!(
                f,
                "not learned: {n} parameters (secrets and redacted texts the routine would \
                 have to be given) is more than a routine may take ({MAX_PARAMS}); \
                 save a shorter task"
            ),
            Refusal::TooLong(n) => write!(
                f,
                "not learned: {n} steps is more than a routine may hold ({MAX_STEPS})"
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// What a run comes to once it is accepted.
#[derive(Debug, Clone, PartialEq)]
pub struct Cleaned {
    /// The steps to keep, in order.
    pub steps: Vec<Step>,
    /// How many looking-only calls were dropped.
    pub dropped: usize,
    /// The names of calls that are not desktop tools and were left out.
    pub skipped: Vec<String>,
    /// Keyboard steps that named no window and were given the window of the step
    /// before them.
    pub inferred_window: usize,
    /// The steps whose text was taken out and made a parameter the run must be
    /// given ([`super::params`]).
    pub parameters: Vec<Made>,
}

/// The names incu's tools are recorded under as `mcp__<server>__<tool>`: the key
/// incode's client configs register it by (`desktop`), the name `incu` itself
/// answers to, and its protocol name, with the underscore a client makes of the
/// hyphen. Nothing else is taken for this server.
pub const OWN_SERVERS: &[&str] = &["desktop", "incu", "incode-desktop", "incode_desktop"];

/// The desktop tool a recorded name is, if it is one.
///
/// A recorded name is the plain tool name (incode's own tools), or an MCP
/// name `mcp__<server>__<tool>`. For the second form the SERVER must be one of
/// incu's own names ([`OWN_SERVERS`]), exactly: other MCP servers have tools
/// called `key` or `scroll` too, and a server that merely has "desktop" in its
/// name (a remote-desktop tool, a screen sharer) is not this one. A routine must
/// never replay someone else's tool.
#[must_use]
pub fn desktop_tool(recorded: &str) -> Option<&str> {
    let known = |name: &str| OBSERVING.contains(&name) || ACTING.contains(&name);
    match recorded.strip_prefix("mcp__") {
        None => known(recorded).then_some(recorded),
        Some(rest) => {
            let (server, tool) = rest.split_once("__")?;
            (OWN_SERVERS.contains(&server) && known(tool)).then_some(tool)
        }
    }
}

/// Whether `phrase` may name a routine.
///
/// # Errors
///
/// It points at the screen, or has no words of its own.
pub fn check_phrase(phrase: &str) -> Result<(), Refusal> {
    if is_deictic(phrase) {
        return Err(Refusal::Deictic);
    }
    if word_set(phrase).is_empty() {
        return Err(Refusal::NoWords);
    }
    Ok(())
}

/// Whether `step` may be replayed, as step `n` (1-based, for the message).
///
/// # Errors
///
/// It is not a replayable tool, or it names a control or window in a way that
/// does not survive to another day, or its arguments are not usable.
pub fn check_step(step: &Step, n: usize) -> Result<(), Refusal> {
    let refuse = |why: String| Refusal::NotReplayable {
        step: n,
        tool: step.tool.clone(),
        why,
    };
    let tool = step.tool.as_str();
    if !REPLAYABLE.contains(&tool) {
        return Err(refuse(match tool {
            "click" | "move_mouse" | "drag" => {
                "uses raw coordinates, which are right for one layout only".to_string()
            }
            "focus_window" => {
                "names a window by its handle, which is right for one window only".to_string()
            }
            "scroll" => "scrolls under the pointer, in no window of its own (it takes no \
                         selector); use element_action scroll_into_view on a control, or a \
                         key such as Page Down"
                .to_string(),
            t if t.starts_with("routine_") => {
                "is a routine tool; a routine cannot run another".to_string()
            }
            "watch" => {
                "waits for hours, which a replay must not; use wait_for with a limit".to_string()
            }
            other => format!("is not a tool a routine can replay ({other})"),
        }));
    }
    let Some(args) = step.args.as_object() else {
        return Err(refuse("has arguments that are not an object".to_string()));
    };
    if step.args.to_string().len() > MAX_STEP_BYTES {
        return Err(refuse(format!("has arguments over {MAX_STEP_BYTES} bytes")));
    }
    let given = |key: &str| args.get(key).is_some_and(|v| !v.is_null());
    for key in ["id", "from_id", "to_id"] {
        if given(key) {
            return Err(refuse(format!(
                "uses an element number (`{key}`, an eN id), which is valid for one ui_tree only"
            )));
        }
    }
    for key in ["x", "y", "from_x", "from_y", "to_x", "to_y"] {
        if given(key) {
            return Err(refuse(format!("uses a raw coordinate (`{key}`)")));
        }
    }
    if given("window") {
        return Err(refuse(
            "names a window by its handle (`window`), which is valid for one window only; \
             name the window in the selector (`app=...;window=...`)"
                .to_string(),
        ));
    }
    if tool == "wait_for" && given("baseline") {
        return Err(refuse(
            "compares the screen with a `baseline` from one earlier moment".to_string(),
        ));
    }
    let looks_at_screen = tool == "wait_for"
        && args.get("condition").and_then(Value::as_str) == Some("screen_changes");
    // A typing step MAY name the control it types into (and only then is its
    // text kept literally: see `params`); when it does, the replay focuses that
    // control first, so it must say which window it is in.
    let names_its_field = matches!(tool, "type_text" | "paste_text") && given("selector");
    let needs_selector = NEEDS_SELECTOR.contains(&tool)
        || (tool == "wait_for" && !looks_at_screen)
        || names_its_field;
    check_selector(args.get("selector"), needs_selector).map_err(refuse)?;
    if names_its_field {
        let named = args.get("selector").and_then(Value::as_str);
        if named.is_some_and(|raw| Selector::parse(raw).is_ok_and(|s| !s.has_element_part())) {
            return Err(refuse(
                "names a window and no control in `selector`; name the field it types into \
                 (role=edit;name=...) or leave the selector out"
                    .to_string(),
            ));
        }
    }
    if KEYBOARD.contains(&tool) {
        check_in_window(args.get(IN_WINDOW)).map_err(refuse)?;
    }
    Ok(())
}

/// The selector argument. When the tool needs one it must be text that parses
/// AND say which window it is in (`app` or `window`): a selector with only a
/// control part is resolved in whatever window is in front.
fn check_selector(selector: Option<&Value>, needed: bool) -> Result<(), String> {
    match selector {
        None | Some(Value::Null) if needed => {
            Err("has no `selector` to find its control by".to_string())
        }
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(raw)) => {
            let parsed = Selector::parse(raw)
                .map_err(|e| format!("has a selector that does not parse: {e}"))?;
            if needed && !parsed.has_window_part() {
                return Err(
                    "has a selector with no window part, so it would act in whatever window is \
                     in front; add `app=...;window=...` (or `window~=...`) to it"
                        .to_string(),
                );
            }
            Ok(())
        }
        Some(_) => Err("has a `selector` that is not text".to_string()),
    }
}

/// A keyboard step's `in_window`: a window-only selector that names a window.
fn check_in_window(value: Option<&Value>) -> Result<(), String> {
    let Some(Value::String(raw)) = value.filter(|v| !v.is_null()) else {
        return Err(format!(
            "sends keys to whatever window has focus: it names no window (`{IN_WINDOW}`, e.g. \
             app=notepad;window~=Untitled)"
        ));
    };
    let parsed = Selector::parse(raw)
        .map_err(|e| format!("has an `{IN_WINDOW}` that does not parse: {e}"))?;
    parsed
        .require_window_only()
        .map_err(|e| format!("has an `{IN_WINDOW}` that is not a window: {e}"))?;
    if parsed.has_window_part() {
        Ok(())
    } else {
        Err(format!("has an `{IN_WINDOW}` that names no app or window"))
    }
}

/// Why a `wait_for` that answered `answer` did not find what it waited for, or
/// `None` when it did. A timeout is a reply and not a tool error, so it is not
/// in a span's `error` and has to be read from the answer.
///
/// The live answer is JSON followed by a note, which is parsed. A recorded one
/// may have been cut at 4 KiB, which is not valid JSON, so that case is read
/// from the text.
pub(super) fn wait_failure(answer: &str) -> Option<String> {
    let first = serde_json::Deserializer::from_str(answer.trim_start())
        .into_iter::<Value>()
        .next()
        .and_then(Result::ok);
    if let Some(reply) = first {
        return match reply.get("result").and_then(Value::as_str) {
            Some("timeout") => Some(format!(
                "the wait timed out after {} ms; last seen: {}",
                reply["waited_ms"],
                reply["last_seen"].as_str().unwrap_or("nothing recorded")
            )),
            Some("stopped") => Some("the wait was stopped".to_string()),
            _ => None,
        };
    }
    let said = |result: &str| {
        answer.contains(&format!("\"result\": \"{result}\""))
            || answer.contains(&format!("\"result\":\"{result}\""))
    };
    if said("timeout") {
        Some("the wait timed out".to_string())
    } else if said("stopped") {
        Some("the wait was stopped".to_string())
    } else {
        None
    }
}

/// Clean a recorded run into the steps of a routine, or say why it is not
/// one. `phrase` is the routine's trigger.
///
/// # Errors
///
/// A [`Refusal`]: the phrase, a failed call, a step that cannot be replayed, an
/// empty result or too many steps.
pub fn learn_from_calls(phrase: &str, calls: &[Call]) -> Result<Cleaned, Refusal> {
    // With nothing to ask the desktop, no control is known to be plain: every
    // typed text becomes a parameter (see [`super::params::why_not_kept`]).
    learn_with(phrase, calls, &|_| Probe::Unknown)
}

/// As [`learn_from_calls`], with `probe` asked about the raw selector of a control
/// a typing step names: the desktop's own report, at save time, of whether it is
/// a password field. A text is kept only when a step names a control that
/// `probe` says is plain (see [`super::params`]).
///
/// # Errors
///
/// As [`learn_from_calls`].
pub fn learn_with(
    phrase: &str,
    calls: &[Call],
    probe: &dyn Fn(&str) -> Probe,
) -> Result<Cleaned, Refusal> {
    check_phrase(phrase)?;
    // ANY call that failed is a run that failed: a shell command that errored, a
    // file that could not be read, a looking call that was refused. Not only the
    // desktop calls: what went wrong there may be why the desktop calls that
    // followed did what they did, and a routine made of them is not what the
    // task needed.
    for (index, call) in calls.iter().enumerate() {
        if let Some(why) = &call.error {
            return Err(Refusal::RunFailed {
                step: index + 1,
                tool: call.tool.clone(),
                why: first_line(why),
            });
        }
    }
    let mut steps = Vec::new();
    let mut dropped = 0;
    let mut skipped = Vec::new();
    let mut inferred_window = 0;
    // The window part of the last step that named one, for a keyboard step that
    // names none of its own.
    let mut last_window: Option<String> = None;
    for (index, call) in calls.iter().enumerate() {
        let n = index + 1;
        let Some(tool) = desktop_tool(&call.tool) else {
            skipped.push(call.tool.clone());
            continue;
        };
        if READING.contains(&tool) {
            dropped += 1;
            continue;
        }
        if let Some(why) = call
            .output
            .as_deref()
            .filter(|_| tool == "wait_for")
            .and_then(wait_failure)
        {
            return Err(Refusal::RunFailed {
                step: n,
                tool: tool.to_string(),
                why,
            });
        }
        let mut step = Step::new(tool, call.args.clone());
        // The picture asked for after an action is for the agent that asked.
        if let Some(map) = step.args.as_object_mut() {
            map.remove("screenshot_after");
            if KEYBOARD.contains(&tool) && !map.contains_key(IN_WINDOW) {
                if let Some(window) = &last_window {
                    map.insert(IN_WINDOW.to_string(), Value::String(window.clone()));
                    inferred_window += 1;
                }
            }
        }
        check_step(&step, n)?;
        last_window = window_part_of(&step).or(last_window);
        steps.push(step);
    }
    if steps.is_empty() {
        return Err(Refusal::NoSteps);
    }
    if steps.len() > MAX_STEPS {
        return Err(Refusal::TooLong(steps.len()));
    }
    // After every step is known replayable, and last: the text of a secure field
    // or a redacted recording is taken out of the steps before they go anywhere.
    let parameters = parameterise(&mut steps, probe);
    let taken = names(&steps).len();
    if taken > MAX_PARAMS {
        return Err(Refusal::TooManyParams(taken));
    }
    Ok(Cleaned {
        steps,
        dropped,
        skipped,
        inferred_window,
        parameters,
    })
}

/// Whether a record that was read says its own run did not work.
///
/// # Errors
///
/// [`Refusal::RecordFailed`] when it does.
pub fn check_record(record: &Record) -> Result<(), Refusal> {
    match &record.failed {
        Some(why) => Err(Refusal::RecordFailed {
            id: record.id.clone(),
            why: first_line(why),
        }),
        None => Ok(()),
    }
}

/// The window part of a step's selector (or its `in_window`), as text, when it
/// names a window.
fn window_part_of(step: &Step) -> Option<String> {
    let raw = step
        .args
        .get("selector")
        .or_else(|| step.args.get(IN_WINDOW))?
        .as_str()?;
    let selector = Selector::parse(raw).ok()?;
    selector
        .has_window_part()
        .then(|| selector.render_window_part())
}

/// Clean an explicit step list: the same rules, with nothing to say about
/// failure because nothing was run.
///
/// # Errors
///
/// As [`learn_from_calls`].
pub fn learn_from_steps(phrase: &str, steps: &[Step]) -> Result<Cleaned, Refusal> {
    let calls: Vec<Call> = steps
        .iter()
        .map(|s| Call::ok(&s.tool, s.args.clone()))
        .collect();
    learn_from_calls(phrase, &calls)
}

/// The first line of an error, bounded: the whole of a tool refusal can be a
/// paragraph.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    let mut out: String = line.chars().take(200).collect();
    if line.chars().count() > 200 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The window every test selector names: a step that acts must say where.
    const IN: &str = "app=settings;window~=Settings";

    /// A click on the control `selector` names, in the test window.
    fn sel_click(selector: &str) -> Call {
        Call::ok(
            "click_element",
            json!({"selector": format!("{IN};{selector}")}),
        )
    }

    fn learn(calls: &[Call]) -> Result<Cleaned, Refusal> {
        learn_from_calls("open the settings dialog", calls)
    }

    #[test]
    fn a_run_of_selector_steps_is_kept_in_order() {
        let cleaned = learn(&[
            sel_click("role=button;name=Settings"),
            Call::ok(
                "wait_for",
                json!({"condition": "window_appears", "selector": "window~=Settings"}),
            ),
            Call::ok("type_text", json!({"text": "dark"})),
            Call::ok("key", json!({"keys": ["enter"]})),
        ])
        .unwrap();
        let tools: Vec<_> = cleaned.steps.iter().map(|s| s.tool.as_str()).collect();
        assert_eq!(tools, ["click_element", "wait_for", "type_text", "key"]);
        assert_eq!(cleaned.dropped, 0);
        // The typing names no window of its own: it is given the one the wait
        // before it named, and the reply says so.
        assert_eq!(cleaned.inferred_window, 2);
        for typed in &cleaned.steps[2..] {
            assert_eq!(typed.args[IN_WINDOW], "window~=Settings", "{typed:?}");
        }
    }

    #[test]
    fn a_step_that_acts_must_name_its_window() {
        let bare = |args: Value| learn(&[Call::ok("click_element", args)]).unwrap_err();
        let why = bare(json!({"selector": "role=button;name=Save"})).to_string();
        assert!(why.contains("no window part"), "{why}");
        // Any of app, window or window~ is a window part.
        for selector in [
            "app=notepad;role=button;name=Save",
            "window=Untitled;role=button;name=Save",
            "window~=Untitled;role=button;name=Save",
        ] {
            assert!(
                learn(&[Call::ok("click_element", json!({"selector": selector}))]).is_ok(),
                "{selector}"
            );
        }
        for tool in ["set_value", "element_action"] {
            let args = json!({"selector": "role=edit;name=Body", "value": "x", "action": "press"});
            let why = learn(&[Call::ok(tool, args)]).unwrap_err().to_string();
            assert!(why.contains("no window part"), "{tool}: {why}");
        }
        // An element wait is held to it too; a screen wait looks at no control.
        let why = learn(&[Call::ok(
            "wait_for",
            json!({"condition": "element_appears", "selector": "role=edit;aid=search"}),
        )])
        .unwrap_err()
        .to_string();
        assert!(why.contains("no window part"), "{why}");
    }

    #[test]
    fn a_keyboard_step_with_no_window_to_borrow_is_refused() {
        for call in [
            Call::ok("type_text", json!({"text": "hi"})),
            Call::ok("paste_text", json!({"text": "hi"})),
            Call::ok("key", json!({"keys": ["enter"]})),
        ] {
            let why = learn(std::slice::from_ref(&call)).unwrap_err().to_string();
            assert!(
                why.contains("whatever window has focus"),
                "{}: {why}",
                call.tool
            );
        }
    }

    #[test]
    fn a_keyboard_step_may_name_its_window_and_that_window_must_parse_and_be_a_window() {
        let typing = |in_window: Value| {
            learn(&[Call::ok(
                "type_text",
                json!({"text": "hi", "in_window": in_window}),
            )])
        };
        assert!(typing(json!("app=notepad;window~=Untitled")).is_ok());
        assert!(typing(json!("colour=red")).is_err(), "does not parse");
        assert!(
            typing(json!("role=edit;name=Body")).is_err(),
            "a control, not a window"
        );
        assert!(typing(json!("")).is_err(), "names nothing");
        assert!(typing(json!(5)).is_err());
    }

    #[test]
    fn the_window_borrowed_is_the_last_one_named_not_the_first() {
        let cleaned = learn(&[
            Call::ok(
                "click_element",
                json!({"selector": "app=mail;window=Inbox;role=button;name=New"}),
            ),
            Call::ok(
                "click_element",
                json!({"selector": "app=mail;window=Compose;role=edit;name=Body"}),
            ),
            Call::ok("type_text", json!({"text": "hello"})),
        ])
        .unwrap();
        assert_eq!(cleaned.steps[2].args[IN_WINDOW], "app=mail;window=Compose");
    }

    #[test]
    fn looking_steps_are_dropped_and_wait_for_is_kept() {
        let cleaned = learn(&[
            Call::ok("list_windows", json!({})),
            Call::ok("ui_tree", json!({"depth": 4})),
            Call::ok("screenshot", json!({})),
            Call::ok("screen_info", json!({})),
            Call::ok("cursor_position", json!({})),
            sel_click("role=button;name=Settings"),
            Call::ok(
                "wait_for",
                json!({"condition": "element_appears", "selector": "app=settings;role=edit;aid=search"}),
            ),
        ])
        .unwrap();
        assert_eq!(cleaned.dropped, 5);
        assert_eq!(cleaned.steps.len(), 2);
        assert_eq!(cleaned.steps[1].tool, "wait_for");
    }

    #[test]
    fn an_element_number_is_refused_and_so_is_the_whole_routine() {
        let why = learn(&[
            sel_click("role=button;name=Settings"),
            Call::ok("click_element", json!({"id": 16})),
        ])
        .unwrap_err();
        assert!(
            matches!(&why, Refusal::NotReplayable { step: 2, tool, .. } if tool == "click_element"),
            "{why:?}"
        );
        assert!(why.to_string().contains("eN id"), "{why}");
        for tool in ["set_value", "element_action"] {
            assert!(
                learn(&[Call::ok(
                    tool,
                    json!({"id": 3, "value": "x", "action": "press"})
                )])
                .is_err(),
                "{tool}"
            );
        }
        assert!(learn(&[Call::ok("scroll", json!({"dx": 0, "dy": -3, "id": 4}))]).is_err());
    }

    #[test]
    fn raw_coordinates_are_refused_in_every_tool_that_takes_them() {
        for call in [
            Call::ok("click", json!({"x": 10, "y": 20})),
            Call::ok("move_mouse", json!({"x": 10, "y": 20})),
            Call::ok(
                "drag",
                json!({"from_x": 1, "from_y": 2, "to_x": 3, "to_y": 4}),
            ),
            Call::ok("drag", json!({"from_id": 1, "to_id": 2})),
        ] {
            let why = learn(std::slice::from_ref(&call)).unwrap_err();
            assert!(
                matches!(why, Refusal::NotReplayable { .. }),
                "{} gave {why:?}",
                call.tool
            );
        }
        // Scroll is refused before its arguments are read: it takes no selector.
        let why = learn(&[Call::ok("scroll", json!({"dx": 0, "dy": 1}))]).unwrap_err();
        assert!(why.to_string().contains("under the pointer"), "{why}");
    }

    #[test]
    fn a_window_handle_is_refused_and_focus_window_cannot_be_replayed() {
        let why = learn(&[Call::ok(
            "click_element",
            json!({"selector": "role=button;name=OK", "window": "197330"}),
        )])
        .unwrap_err();
        assert!(why.to_string().contains("window"), "{why}");
        assert!(learn(&[Call::ok("focus_window", json!({"window": "197330"}))]).is_err());
        assert!(learn(&[Call::ok("key", json!({"keys": ["a"], "window": "7"}))]).is_err());
    }

    #[test]
    fn a_step_with_no_usable_selector_is_refused() {
        assert!(learn(&[Call::ok("click_element", json!({}))]).is_err());
        assert!(
            learn(&[sel_click("colour=red")]).is_err(),
            "an unparseable selector"
        );
        assert!(learn(&[Call::ok("set_value", json!({"selector": 5, "value": "x"}))]).is_err());
        // A screen wait has no control to name.
        assert!(learn(&[Call::ok("wait_for", json!({"condition": "screen_changes"}))]).is_ok());
        assert!(learn(&[Call::ok(
            "wait_for",
            json!({"condition": "element_appears"})
        )])
        .is_err());
    }

    #[test]
    fn a_wait_compared_with_one_earlier_screen_is_refused() {
        let why = learn(&[Call::ok(
            "wait_for",
            json!({"condition": "screen_changes", "baseline": "00ff00ff00ff00ff00ff00ff00ff00ff"}),
        )])
        .unwrap_err();
        assert!(why.to_string().contains("baseline"), "{why}");
    }

    #[test]
    fn a_failed_call_is_a_failed_run() {
        let why = learn(&[
            sel_click("role=button;name=Settings"),
            sel_click("role=button;name=Missing").failed("matches no control\nsecond line"),
        ])
        .unwrap_err();
        match &why {
            Refusal::RunFailed { step, why, .. } => {
                assert_eq!(*step, 2);
                assert_eq!(why, "matches no control", "only the first line");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_wait_that_timed_out_or_was_stopped_is_a_failed_run() {
        let wait = |answer: &str| {
            Call::ok(
                "wait_for",
                json!({"condition": "window_appears", "selector": "window~=X"}),
            )
            .answering(answer)
        };
        assert!(matches!(
            learn(&[wait(
                "{\n  \"result\": \"timeout\",\n  \"waited_ms\": 5000\n}"
            )]),
            Err(Refusal::RunFailed { .. })
        ));
        assert!(matches!(
            learn(&[wait("{\"result\":\"stopped\"}")]),
            Err(Refusal::RunFailed { .. })
        ));
        assert!(learn(&[wait("{\n  \"result\": \"found\"\n}")]).is_ok());
    }

    /// A run is failed when ANY call in it failed: a looking call that was
    /// refused, a shell command that exited non-zero, a file that was not there.
    #[test]
    fn a_failed_call_of_any_kind_is_a_failed_run() {
        let ok = sel_click("role=button;name=Settings");
        // The case that was missed: [Bash fails, incu key ok].
        let key = Call::ok(
            "mcp__desktop__key",
            json!({"keys": ["enter"], "in_window": IN}),
        );
        let why = learn(&[
            Call::ok("Bash", json!({"command": "false"})).failed("exit code 1"),
            key.clone(),
        ])
        .unwrap_err();
        match &why {
            Refusal::RunFailed { step, tool, why } => {
                assert_eq!((*step, tool.as_str()), (1, "Bash"));
                assert_eq!(why, "exit code 1");
            }
            other => panic!("{other:?}"),
        }
        assert!(why.to_string().contains("call 1 (Bash)"), "{why}");
        // A looking call that failed, a call that is no desktop tool at all, and
        // an error after the last desktop step.
        for failing in [
            Call::ok("ui_tree", json!({})).failed("window has no tree"),
            Call::ok("mcp__memory__recall_index", json!({})).failed("unknown scope"),
            Call::ok("Read", json!({"file_path": "x"})).failed("no such file"),
        ] {
            assert!(
                matches!(
                    learn(&[ok.clone(), failing.clone()]),
                    Err(Refusal::RunFailed { .. })
                ),
                "{failing:?}"
            );
            assert!(matches!(
                learn(&[failing, ok.clone()]),
                Err(Refusal::RunFailed { .. })
            ));
        }
        // And the same calls that all worked are learned.
        assert!(learn(&[Call::ok("Bash", json!({"command": "true"})), key]).is_ok());
    }

    #[test]
    fn a_record_whose_own_outcome_says_it_failed_is_not_learned() {
        let record = Record::new("w-1", 1, vec![sel_click("role=button;name=Settings")]);
        assert!(check_record(&record).is_ok());
        let failed = check_record(&record.failing("status is \"failed\"\nmore")).unwrap_err();
        assert_eq!(
            failed,
            Refusal::RecordFailed {
                id: "w-1".to_string(),
                why: "status is \"failed\"".to_string()
            }
        );
        assert!(failed.to_string().contains("w-1"), "{failed}");
    }

    /// Only incu's own servers: a name with "desktop" in it is not enough.
    #[test]
    fn another_servers_tools_are_never_taken_for_incus() {
        for foreign in [
            "mcp__remote_desktop__key",
            "mcp__desktop_sharing__click_element",
            "mcp__my-incu-fork__key",
            "mcp__Desktop__key",
            "mcp__desktopx__wait_for",
            "mcp__incu2__wait_for",
        ] {
            assert_eq!(desktop_tool(foreign), None, "{foreign}");
        }
        for own in OWN_SERVERS {
            let name = format!("mcp__{own}__wait_for");
            assert_eq!(desktop_tool(&name), Some("wait_for"), "{name}");
        }
        // Left out of a routine and reported, as any other call that is not ours.
        let cleaned = learn(&[
            Call::ok("mcp__remote_desktop__key", json!({"keys": ["enter"]})),
            sel_click("role=button;name=New"),
        ])
        .unwrap();
        assert_eq!(cleaned.steps.len(), 1);
        assert_eq!(cleaned.skipped, ["mcp__remote_desktop__key"]);
    }

    /// click username, type "alice", Tab, type "hunter2", Enter.
    #[test]
    fn a_password_typed_after_tab_is_never_learned_as_text() {
        let cleaned = learn(&[
            sel_click("role=edit;name=Username"),
            Call::ok("type_text", json!({"text": "alice", "in_window": IN})),
            Call::ok("key", json!({"keys": ["Tab"], "in_window": IN})),
            Call::ok("type_text", json!({"text": "hunter2", "in_window": IN})),
            Call::ok("key", json!({"keys": ["Enter"], "in_window": IN})),
        ])
        .unwrap();
        // No step names a field, so no typed text is kept: not the password, and
        // not the user name either.
        assert_eq!(cleaned.parameters.len(), 2);
        assert_eq!(cleaned.steps[3].args["text"], "{{text_2}}");
        assert_eq!(cleaned.steps[1].args["text"], "{{text}}");
        let kept = format!("{cleaned:?}");
        assert!(
            !kept.contains("hunter2") && !kept.contains("alice"),
            "{kept}"
        );
    }

    /// Text is kept when the step itself names a plain text box that the desktop
    /// confirms is not a password field, and not otherwise.
    #[test]
    fn a_step_that_names_a_plain_field_keeps_its_text_when_the_desktop_confirms_it() {
        let named = |field: &str| {
            Call::ok(
                "type_text",
                json!({"text": "rust", "in_window": IN, "selector": format!("{IN};{field}")}),
            )
        };
        let plain = |_: &str| Probe::Plain;
        let kept =
            learn_with("search for rust", &[named("role=edit;name=Search")], &plain).unwrap();
        assert!(kept.parameters.is_empty());
        assert_eq!(kept.steps[0].args["text"], "rust");
        // The desktop could not check it: a parameter.
        let unchecked =
            learn_from_calls("search for rust", &[named("role=edit;name=Search")]).unwrap();
        assert_eq!(unchecked.steps[0].args["text"], "{{search}}");
        // Marked as a password by the desktop, whatever it is called.
        let secret = learn_with(
            "search for rust",
            &[named("role=edit;name=Search")],
            &|_: &str| Probe::Secret,
        )
        .unwrap();
        assert_eq!(secret.steps[0].args["text"], "{{search}}");
        assert_eq!(
            secret.parameters[0].why,
            crate::routine::params::Why::SecureField
        );
        // A selector that names a window and no control is refused outright: the
        // replay would have nothing to focus.
        let no_control = Call::ok(
            "type_text",
            json!({"text": "x", "in_window": IN, "selector": IN}),
        );
        assert!(matches!(
            learn_with("search for rust", &[no_control], &plain),
            Err(Refusal::NotReplayable { .. })
        ));
        // One with no window part would be focused in whatever is in front.
        let no_window = Call::ok(
            "type_text",
            json!({"text": "x", "in_window": IN, "selector": "role=edit;name=Search"}),
        );
        assert!(matches!(
            learn_with("search for rust", &[no_window], &plain),
            Err(Refusal::NotReplayable { .. })
        ));
    }

    #[test]
    fn a_routine_that_would_take_too_many_parameters_is_refused() {
        let mut calls = vec![sel_click("role=edit;name=Username")];
        for i in 0..=MAX_PARAMS {
            calls.push(Call::ok(
                "type_text",
                json!({"text": format!("{{{{mine{i}}}}}"), "in_window": IN}),
            ));
        }
        let why = learn(&calls).unwrap_err();
        assert_eq!(why, Refusal::TooManyParams(MAX_PARAMS + 1));
        assert!(
            why.to_string().contains("more than a routine may take"),
            "{why}"
        );
    }

    #[test]
    fn typing_into_a_password_field_is_learned_as_a_parameter() {
        let cleaned = learn(&[
            sel_click("role=edit;name=Password"),
            Call::ok(
                "type_text",
                json!({"text": "hunter2-secret", "in_window": IN}),
            ),
        ])
        .unwrap();
        assert_eq!(cleaned.parameters.len(), 1);
        assert_eq!(cleaned.steps[1].args["text"], "{{text}}");
        assert!(!format!("{cleaned:?}").contains("hunter2"));
        // A step that names the field itself, with the name saying it is secret.
        let named = learn_with(
            "sign in",
            &[Call::ok(
                "type_text",
                json!({"text": "hunter2-secret", "in_window": IN, "selector": format!("{IN};role=edit;name=Password")}),
            )],
            &|_: &str| Probe::Plain,
        )
        .unwrap();
        assert_eq!(named.steps[0].args["text"], "{{password}}");
        // The desktop's own word that a control is a password field counts too.
        let by_desktop = learn_with(
            "sign in",
            &[Call::ok(
                "set_value",
                json!({"selector": format!("{IN};role=edit;name=Code"), "value": "482913"}),
            )],
            &|raw: &str| {
                if raw.contains("name=Code") {
                    Probe::Secret
                } else {
                    Probe::Plain
                }
            },
        )
        .unwrap();
        assert_eq!(by_desktop.steps[0].args["value"], "{{code}}");
    }

    #[test]
    fn this_and_that_are_refused_before_anything_is_looked_at() {
        let calls = [sel_click("role=button;name=Settings")];
        assert_eq!(
            learn_from_calls("close this", &calls).unwrap_err(),
            Refusal::Deictic
        );
        assert_eq!(
            learn_from_calls("click that button", &calls).unwrap_err(),
            Refusal::Deictic
        );
        assert_eq!(
            learn_from_calls("please do the", &calls).unwrap_err(),
            Refusal::NoWords
        );
    }

    #[test]
    fn nothing_but_looking_is_not_a_routine() {
        assert_eq!(
            learn(&[
                Call::ok("ui_tree", json!({})),
                Call::ok("screenshot", json!({}))
            ])
            .unwrap_err(),
            Refusal::NoSteps
        );
        assert_eq!(learn(&[]).unwrap_err(), Refusal::NoSteps);
    }

    #[test]
    fn too_many_steps_are_refused() {
        let calls: Vec<Call> = (0..=MAX_STEPS)
            .map(|_| sel_click("role=button;name=Next"))
            .collect();
        assert_eq!(learn(&calls).unwrap_err(), Refusal::TooLong(MAX_STEPS + 1));
    }

    #[test]
    fn calls_that_are_not_desktop_tools_are_left_out_and_reported() {
        let cleaned = learn(&[
            Call::ok("Bash", json!({"command": "open -a Notes"})),
            sel_click("role=button;name=New"),
            Call::ok("mcp__memory__recall_index", json!({})),
        ])
        .unwrap();
        assert_eq!(cleaned.steps.len(), 1);
        assert_eq!(cleaned.skipped, ["Bash", "mcp__memory__recall_index"]);
    }

    #[test]
    fn an_mcp_name_counts_only_from_a_server_that_looks_like_this_one() {
        assert_eq!(desktop_tool("click_element"), Some("click_element"));
        assert_eq!(
            desktop_tool("mcp__desktop__click_element"),
            Some("click_element")
        );
        assert_eq!(desktop_tool("mcp__incode-desktop__key"), Some("key"));
        assert_eq!(desktop_tool("mcp__incu__wait_for"), Some("wait_for"));
        // Someone else's `key`, and a name that is not a desktop tool at all.
        assert_eq!(desktop_tool("mcp__computer-use__key"), None);
        assert_eq!(desktop_tool("mcp__desktop__format_disk"), None);
        assert_eq!(desktop_tool("Bash"), None);
    }

    #[test]
    fn the_picture_asked_for_after_an_action_is_not_kept() {
        let cleaned = learn(&[Call::ok(
            "click_element",
            json!({"selector": "app=dialog;role=button;name=OK", "screenshot_after": true}),
        )])
        .unwrap();
        assert_eq!(
            cleaned.steps[0].args,
            json!({"selector": "app=dialog;role=button;name=OK"})
        );
    }

    #[test]
    fn routine_tools_and_watch_are_not_steps() {
        for tool in ["routine_run", "routine_save", "watch"] {
            assert!(
                matches!(
                    learn(&[Call::ok(tool, json!({"phrase": "x"}))]),
                    Err(Refusal::NotReplayable { .. })
                ),
                "{tool}"
            );
        }
    }

    #[test]
    fn an_explicit_list_is_cleaned_by_the_same_rules() {
        let ok = learn_from_steps(
            "sign in to the portal",
            &[Step::new(
                "click_element",
                json!({"selector": "app=portal;role=button;name=Sign in"}),
            )],
        );
        assert!(ok.is_ok());
        let bad = learn_from_steps(
            "sign in to the portal",
            &[Step::new("click", json!({"x": 1, "y": 2}))],
        );
        assert!(bad.is_err());
    }

    #[test]
    fn check_step_is_what_a_replay_asks_of_a_stored_step() {
        let keys = json!({"keys": ["ctrl", "s"], "in_window": "app=notepad"});
        assert!(check_step(&Step::new("key", keys), 1).is_ok());
        let bare = Step::new("key", json!({"keys": ["ctrl", "s"]}));
        assert!(check_step(&bare, 1).is_err(), "no window named");
        assert!(check_step(&Step::new("click_element", json!({"id": 1})), 1).is_err());
        assert!(check_step(&Step::new("key", json!("ctrl+s")), 1).is_err());
    }
}
