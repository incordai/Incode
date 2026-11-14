//! The five routine tools: `routine_save`, `routine_run`, `routine_forget`
//! (acting tier) and `routine_list`, `routine_match` (observing tier).
//!
//! The rules are in [`crate::routine`]; this file is where they meet the live
//! server. Two things are decided here and nowhere else:
//!
//! - **A replay runs through the server's own gates.** [`Live`] sends every
//!   step to [`Server::gated_call`], the function `tools/call` itself uses, so
//!   each step gets the stop file, the permission tier, the operator's pre-tool
//!   hook (which sees the real arguments), the blocked-application guards, the
//!   action-rate cap, the post-tool hook and a journal line of its own. The
//!   routine as a whole is one more journalled call. A replay cannot do what a
//!   live call would be refused.
//! - **The engine is optional.** A server with no step source still lists,
//!   matches, replays and forgets routines, and saves one from an explicit list
//!   of steps; only saving from Watchman record ids needs the engine.
//!
//! # Wired to
//!
//! - **Called by** `Server::run_tool` in [`super::tools`].
//! - **Calls** [`crate::routine`] for the rules and the file, and
//!   `Server::gated_call` for every replayed step.

use serde_json::{json, Value};

use crate::routine::learn::{check_phrase, learn_from_calls, learn_from_steps, Cleaned, IN_WINDOW};
use crate::routine::replay::{replay, Runner};
use crate::routine::source::{Call, StepSource};
use crate::routine::store::{Routines, Store};
use crate::routine::words::word_set;
use crate::routine::{now_ms, Routine, Step};

use crate::selector::Selector;

use super::tools::{text, text_of, window_id_for};
use super::Server;

/// The most record ids one `routine_save` reads.
const MAX_RECORDS: usize = 20;

/// The longest phrase a routine may be named by, in characters.
const MAX_PHRASE_CHARS: usize = 200;

/// The routines file and where records come from, held by a [`Server`].
pub struct RoutineKit {
    store: Store,
    source: Box<dyn StepSource>,
}

impl RoutineKit {
    /// A kit over `store`, reading records from `source`.
    #[must_use]
    pub fn new(store: Store, source: Box<dyn StepSource>) -> Self {
        Self { store, source }
    }

    /// Where the routines file is.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        self.store.path()
    }
}

impl Server {
    /// Offer the routine tools over `kit`. Without this they refuse by name:
    /// a server built for a protocol test has no routines file to write.
    #[must_use]
    pub fn with_routines(mut self, kit: RoutineKit) -> Self {
        self.routines = Some(kit);
        self
    }

    /// The kit, or the refusal saying there is none.
    fn kit(&self) -> Result<&RoutineKit, String> {
        self.routines
            .as_ref()
            .ok_or_else(|| "routines are not configured on this server".to_string())
    }

    /// One of the five routine tools. Called by `Server::run_tool`.
    pub(super) fn routine_tool(&mut self, name: &str, args: &Value) -> Result<Vec<Value>, String> {
        match name {
            "routine_save" => self.routine_save(args),
            "routine_run" => self.routine_run(args),
            "routine_forget" => self.routine_forget(args),
            "routine_list" => self.routine_list(),
            "routine_match" => self.routine_match(args),
            other => Err(format!("no tool named '{other}'")),
        }
    }

    /// `routine_save`: learn a routine from Watchman records or from steps.
    fn routine_save(&mut self, args: &Value) -> Result<Vec<Value>, String> {
        let phrase = phrase_arg(args)?;
        check_phrase(&phrase).map_err(|r| r.to_string())?;
        let taught = args.get("taught").and_then(Value::as_bool).unwrap_or(false);
        let kit = self.kit()?;
        let cleaned = match (args.get("records"), args.get("steps")) {
            (Some(_), Some(_)) => {
                return Err("give `records` or `steps`, not both".to_string());
            }
            (None, None) => {
                return Err(
                    "give `records` (Watchman record ids) or `steps` to learn from".to_string(),
                );
            }
            (Some(ids), None) => learn_from_calls(&phrase, &calls_of(kit.source.as_ref(), ids)?),
            (None, Some(steps)) => learn_from_steps(&phrase, &steps_of(steps)?),
        }
        .map_err(|r| r.to_string())?;
        let now = now_ms();
        let mut file = kit.store.load()?;
        let saved = file.learn(&phrase, cleaned.steps.clone(), taught, now)?;
        kit.store.save(&file)?;
        Ok(vec![text(saved_reply(
            &phrase,
            &cleaned,
            &saved,
            kit.store.path(),
        ))])
    }

    /// `routine_run`: replay the routine a phrase asks for.
    fn routine_run(&mut self, args: &Value) -> Result<Vec<Value>, String> {
        let phrase = phrase_arg(args)?;
        let routine = self.find_routine(&phrase)?;
        let outcome = replay(&routine, &mut Live { server: self });
        let report = outcome.map_err(|failure| failure.to_string())?;
        // Used, so its thirty days begin again. Not a reason to fail a routine
        // that has just run to the end.
        let remembered = self.remember_use(&routine.phrase);
        let steps: Vec<Value> = report
            .ran
            .iter()
            .enumerate()
            .map(|(i, r)| json!(format!("{}. {} -> {}", i + 1, r.summary, r.answer)))
            .collect();
        let mut reply = json!({
            "ran": routine.phrase,
            "steps_run": report.ran.len(),
            "steps": steps,
        });
        if let Err(why) = remembered {
            reply["note"] = json!(format!(
                "it ran, but its last-used time was not saved: {why}"
            ));
        }
        Ok(vec![text(pretty(&reply))])
    }

    /// `routine_forget`: delete the routine a phrase asks for.
    fn routine_forget(&mut self, args: &Value) -> Result<Vec<Value>, String> {
        let phrase = phrase_arg(args)?;
        let kit = self.kit()?;
        let mut file = kit.store.load()?;
        if !file.forget(&phrase, now_ms()) {
            return Err(format!(
                "no routine matches \"{phrase}\"; routine_list shows what is saved"
            ));
        }
        kit.store.save(&file)?;
        Ok(vec![text(format!("forgot the routine for \"{phrase}\""))])
    }

    /// `routine_list`: every routine that has not expired.
    fn routine_list(&self) -> Result<Vec<Value>, String> {
        let kit = self.kit()?;
        let file = kit.store.load()?;
        let now = now_ms();
        let routines: Vec<Value> = file.live(now).map(|r| describe(r, now)).collect();
        Ok(vec![text(pretty(&json!({
            "count": routines.len(),
            "routines": routines,
            "file": kit.store.path().display().to_string(),
        })))])
    }

    /// `routine_match`: which routine a phrase would run, without running it.
    fn routine_match(&self, args: &Value) -> Result<Vec<Value>, String> {
        let phrase = phrase_arg(args)?;
        let kit = self.kit()?;
        let file = kit.store.load()?;
        let now = now_ms();
        let words: Vec<String> = word_set(&phrase).into_iter().collect();
        let reply = match file.matching(&phrase, now) {
            Some(routine) => {
                let mut shown = describe(routine, now);
                shown["steps"] = json!(routine.steps.iter().map(Step::summary).collect::<Vec<_>>());
                json!({"matched": true, "words": words, "routine": shown})
            }
            None => json!({"matched": false, "words": words}),
        };
        Ok(vec![text(pretty(&reply))])
    }

    /// The live routine `phrase` asks for.
    fn find_routine(&self, phrase: &str) -> Result<Routine, String> {
        let file = self.kit()?.store.load()?;
        file.matching(phrase, now_ms()).cloned().ok_or_else(|| {
            format!("no routine matches \"{phrase}\"; routine_list shows what is saved")
        })
    }

    /// Record that the routine `phrase` ran to the end, reading the file again
    /// first so a routine saved meanwhile is not overwritten.
    fn remember_use(&self, phrase: &str) -> Result<(), String> {
        let kit = self.kit()?;
        let mut file: Routines = kit.store.load()?;
        file.used(phrase, now_ms());
        kit.store.save(&file)
    }

    /// One tool call through every gate, as `tools/call` runs it. The content it
    /// answered, or the words it was refused with.
    ///
    /// Called by `Server::call_tool` for the call itself, and by [`Live`] for
    /// each step of a replay: one path, so a step can never be let through by a
    /// door a live call does not have.
    pub(super) fn gated_call(&mut self, name: &str, args: &Value) -> Result<Vec<Value>, String> {
        self.catch_stop_file();
        let permitted = self.authorise(name);
        let raw_args = args.to_string();
        let vetoed = self.hook_veto(&permitted, name, &raw_args);
        let outcome = self.outcome(&permitted, vetoed, name, args);
        self.tell_post_hooks(name, &raw_args, &outcome);
        self.record_call(name, args, &outcome);
        outcome
    }
}

/// The live desktop as a [`Runner`]: every step through the server's gates.
struct Live<'a> {
    server: &'a mut Server,
}

impl Runner for Live<'_> {
    fn run(&mut self, _n: usize, step: &Step) -> Result<String, String> {
        let args = live_args(step)?;
        // A link or a button is invoked through its own pattern when it has one:
        // that needs no mouse, works for a control scrolled out of view and takes
        // no focus. A control that will not be invoked (or a refusal of the
        // invoke) leaves the recorded click to run, each through the same gates;
        // the journal shows both.
        if let Some(invoke) = invoke_for(step) {
            if let Ok(content) = self.server.gated_call("element_action", &invoke) {
                return Ok(text_of(&content));
            }
        }
        self.server
            .gated_call(&step.tool, &args)
            .map(|content| text_of(&content))
    }

    fn settle(&mut self) {
        std::thread::sleep(crate::limits::get().settle);
    }
}

/// The arguments the tool is called with for `step`. A keyboard step's
/// `in_window` (a window-only selector) becomes the `window` of the window it
/// names NOW; the tool then refuses unless that window has the focus, so keys
/// never go to whatever happens to be in front.
pub(super) fn live_args(step: &Step) -> Result<Value, String> {
    let mut args = step.args.clone();
    let Some(map) = args.as_object_mut() else {
        return Ok(args);
    };
    let Some(named) = map.remove(IN_WINDOW) else {
        return Ok(args);
    };
    let raw = named.as_str().ok_or("in_window is not text")?;
    let selector = Selector::parse(raw).map_err(|e| format!("in_window: {e}"))?;
    let window = window_id_for(&selector).map_err(|e| format!("in_window {raw}: {e}"))?;
    map.insert("window".to_string(), Value::String(window));
    Ok(args)
}

/// The `element_action invoke` that does what the left click of `step` would,
/// when the step is a click on a link or a button found by selector.
pub(super) fn invoke_for(step: &Step) -> Option<Value> {
    if step.tool != "click_element" {
        return None;
    }
    let left = step
        .args
        .get("button")
        .and_then(Value::as_str)
        .is_none_or(|b| b == "left");
    let raw = step.args.get("selector")?.as_str()?;
    let role = Selector::parse(raw).ok()?.role?;
    (left && matches!(role.word(), "button" | "link"))
        .then(|| json!({"selector": raw, "action": "invoke"}))
}

/// The phrase argument: required, text, not empty, not absurdly long.
fn phrase_arg(args: &Value) -> Result<String, String> {
    let phrase = args
        .get("phrase")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or("phrase is required: the words that ask for this routine")?;
    if phrase.chars().count() > MAX_PHRASE_CHARS {
        return Err(format!("phrase is over {MAX_PHRASE_CHARS} characters"));
    }
    Ok(phrase.to_string())
}

/// The recorded calls of the records `ids` names, oldest record first.
fn calls_of(source: &dyn StepSource, ids: &Value) -> Result<Vec<Call>, String> {
    let ids: Vec<&str> = ids
        .as_array()
        .ok_or("records must be a list of Watchman record ids")?
        .iter()
        .map(|id| id.as_str().ok_or("every record id must be text"))
        .collect::<Result<_, _>>()?;
    if ids.is_empty() || ids.len() > MAX_RECORDS {
        return Err(format!("give between 1 and {MAX_RECORDS} record ids"));
    }
    let mut records = ids
        .iter()
        .map(|id| source.record(id))
        .collect::<Result<Vec<_>, _>>()?;
    // A task spans several turns, one record each; they run in the order they
    // happened, not the order they were named. Stable, so equal times keep it.
    records.sort_by_key(|r| r.started_at_ms);
    Ok(records.into_iter().flat_map(|r| r.calls).collect())
}

/// An explicit step list: `[{"tool": ..., "args": {...}}, ...]`.
fn steps_of(steps: &Value) -> Result<Vec<Step>, String> {
    let list = steps
        .as_array()
        .ok_or("steps must be a list of {tool, args}")?;
    list.iter()
        .enumerate()
        .map(|(i, s)| {
            serde_json::from_value::<Step>(s.clone())
                .map_err(|e| format!("step {} is not {{tool, args}}: {e}", i + 1))
        })
        .collect()
}

/// A routine as the agent is shown it in a list.
fn describe(routine: &Routine, now: i64) -> Value {
    let mut shown = json!({
        "phrase": routine.phrase,
        "steps": routine.steps.len(),
        "learned": routine.date,
        "last_used": crate::routine::day_of(routine.last_used_ms),
        "taught": routine.taught,
    });
    if let Some(days) = routine.days_left(now) {
        shown["expires_in_days"] = json!(days);
    }
    shown
}

/// What `routine_save` answers.
fn saved_reply(
    phrase: &str,
    cleaned: &Cleaned,
    saved: &crate::routine::store::Saved,
    file: &std::path::Path,
) -> String {
    let mut reply = json!({
        "saved": phrase,
        "replaced_an_older_routine": saved.replaced,
        "taught": saved.taught,
        "steps": cleaned.steps.iter().map(Step::summary).collect::<Vec<_>>(),
        "looking_steps_dropped": cleaned.dropped,
        "file": file.display().to_string(),
    });
    if cleaned.inferred_window > 0 {
        reply["keyboard_steps_given_the_window_before_them"] = json!(cleaned.inferred_window);
        reply["window_note"] = json!(
            "typing and key steps name no window in a recording, so each was given the window \
             of the step before it (see `in_window` in the steps); a replay refuses to type \
             unless that window has the focus"
        );
    }
    if !cleaned.skipped.is_empty() {
        reply["left_out_not_desktop_tools"] = json!(cleaned.skipped);
        reply["warning"] = json!(
            "those calls are not in the routine; if the task depended on them, a replay \
             will not do them"
        );
    }
    pretty(&reply)
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}
