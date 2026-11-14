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

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::routine::learn::{check_phrase, check_record, learn_with, Cleaned, IN_WINDOW};
use crate::routine::params::{self, types_a_secret, Target, Values, Why};
use crate::routine::pictures::{Pictures, When};
use crate::routine::replay::{replay, Runner};
use crate::routine::source::{Call, Record, StepSource};
use crate::routine::store::Store;
use crate::routine::words::word_set;
use crate::routine::{now_ms, Routine, Step};

use crate::selector::Selector;

use super::tools::{resolve_selector, text, text_of, window_id_for};
use super::Server;

/// The most record ids one `routine_save` reads.
const MAX_RECORDS: usize = 20;

/// The longest phrase a routine may be named by, in characters.
const MAX_PHRASE_CHARS: usize = 200;

/// The routines file and where records come from, held by a [`Server`].
pub struct RoutineKit {
    store: Store,
    source: Box<dyn StepSource>,
    /// Takes the picture of a step; incu's own capture when `None`. Replaced in
    /// tests, so that what is kept and named is tested without photographing the
    /// screen of the machine running them.
    shooter: Option<Shooter>,
}

/// What takes the picture of the window a step is for: the image's bytes (JPEG).
pub type Shooter = Box<dyn Fn(&Step) -> Result<Vec<u8>, String> + Send>;

impl RoutineKit {
    /// A kit over `store`, reading records from `source`.
    #[must_use]
    pub fn new(store: Store, source: Box<dyn StepSource>) -> Self {
        Self {
            store,
            source,
            shooter: None,
        }
    }

    /// The same kit, taking its step pictures with `shooter` and not incu's own
    /// capture.
    #[must_use]
    pub fn with_shooter(mut self, shooter: Shooter) -> Self {
        self.shooter = Some(shooter);
        self
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
        // Being kept (never expiring) is the person's to give, by `incu routine
        // keep`. Said plainly, so an agent does not believe it did it.
        if args.get("taught").is_some() {
            return Err(
                "`taught` is not an argument: only the person keeps a routine \
                        from expiring, with `incu routine keep \"<phrase>\"` at a terminal"
                    .to_string(),
            );
        }
        let kit = self.kit()?;
        // What the desktop says is a password field, asked about each control a
        // typing step might have gone into, once each.
        let seen: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
        let is_secret = |raw: &str| {
            *seen
                .borrow_mut()
                .entry(raw.to_string())
                .or_insert_with(|| desktop_says_password(raw))
        };
        let calls: Vec<Call> = match (args.get("records"), args.get("steps")) {
            (Some(_), Some(_)) => {
                return Err("give `records` or `steps`, not both".to_string());
            }
            (None, None) => {
                return Err(
                    "give `records` (Watchman record ids) or `steps` to learn from".to_string(),
                );
            }
            (Some(ids), None) => {
                let records = records_of(kit.source.as_ref(), ids)?;
                // A run that failed is not kept, whatever calls it holds.
                for record in &records {
                    check_record(record).map_err(|r| r.to_string())?;
                }
                records.into_iter().flat_map(|r| r.calls).collect()
            }
            (None, Some(steps)) => steps_of(steps)?
                .iter()
                .map(|s| Call::ok(&s.tool, s.args.clone()))
                .collect(),
        };
        let cleaned = learn_with(&phrase, &calls, &is_secret).map_err(|r| r.to_string())?;
        let now = now_ms();
        let saved = kit
            .store
            .update(|file| file.learn(&phrase, cleaned.steps.clone(), false, now))?;
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
        // Every parameter must be given before anything runs: a secret the
        // routine does not keep is asked for here, not half way through.
        let values = Values::bind(&routine.steps, args.get("params"))?;
        let mut live = Live::new(self, values);
        let outcome = replay(&routine, &mut live);
        let pictures = live.pictures_note();
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
            "pictures": pictures,
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
        kit.store.update(|file| {
            if file.forget(&phrase, now_ms()) {
                Ok(())
            } else {
                Err(format!(
                    "no routine matches \"{phrase}\"; routine_list shows what is saved"
                ))
            }
        })?;
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
                json!({"matched": true, "words": words, "routine": describe(routine, now)})
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
        self.kit()?.store.update(|file| {
            file.used(phrase, now_ms());
            Ok(())
        })
    }

    /// One tool call through every gate, as `tools/call` runs it. The content it
    /// answered, or the words it was refused with.
    ///
    /// Called by `Server::call_tool` for the call itself, and by [`Live`] for
    /// each step of a replay: one path, so a step can never be let through by a
    /// door a live call does not have.
    pub(super) fn gated_call(&mut self, name: &str, args: &Value) -> Result<Vec<Value>, String> {
        let seen = hook_view(name, args);
        self.gated_call_as(name, args, &seen)
    }

    /// [`Server::gated_call`] with `seen` as the arguments the operator's hooks
    /// are shown, which may differ from `args` (the call's real arguments) only
    /// by having secrets taken out: see [`hook_view`] and the replay's parameters.
    pub(super) fn gated_call_as(
        &mut self,
        name: &str,
        args: &Value,
        seen: &Value,
    ) -> Result<Vec<Value>, String> {
        self.catch_stop_file();
        let permitted = self.authorise(name);
        let raw_args = seen.to_string();
        let vetoed = self.hook_veto(&permitted, name, &raw_args);
        let outcome = self.outcome(&permitted, vetoed, name, args);
        self.tell_post_hooks(name, &raw_args, &outcome);
        self.record_call(name, args, &outcome);
        outcome
    }
}

/// What the operator's hooks are shown of a call's arguments: all of them, except
/// that every value in `routine_run`'s `params` is `"<param>"`.
///
/// A hook sees the real arguments of a call (it is the operator's own, and a
/// pre-tool hook can only judge what it can read), but a hook is also how
/// Watchman and other recorders see calls, so nothing the caller passed to name
/// a secret is handed on. A `params` entry is only a reference to where the
/// person keeps the value, never the value, and still it is not repeated.
pub(super) fn hook_view(name: &str, args: &Value) -> Value {
    let mut seen = args.clone();
    if name == "routine_run" {
        if let Some(params) = seen.get_mut("params").and_then(Value::as_object_mut) {
            for value in params.values_mut() {
                *value = json!("<param>");
            }
        } else if seen.get("params").is_some_and(|p| !p.is_null()) {
            seen["params"] = json!("<param>");
        }
    }
    seen
}

/// The live desktop as a [`Runner`]: every step through the server's gates.
pub(super) struct Live<'a> {
    server: &'a mut Server,
    /// What the caller gave for the routine's parameters. In memory only.
    values: Values,
    /// The folder this run's pictures go to, or `None` when it could not be made.
    pictures: Option<Pictures>,
    /// Why there are no pictures, or fewer: the first thing that went wrong.
    picture_trouble: Option<String>,
    /// Where the typing target is as the steps run (see [`Target`]), and where it
    /// was before the step now running.
    pub(super) target: Target,
    pub(super) before_target: Target,
    /// The step just run typed a secret: the picture before the next is skipped.
    skip_next_before: bool,
    /// The pictures that were not taken because a secret was on screen.
    skipped: Vec<String>,
}

impl<'a> Live<'a> {
    /// A run over `server`, with the parameters `values` and a folder of its own
    /// for its pictures (the finished runs' are cleared).
    pub(super) fn new(server: &'a mut Server, values: Values) -> Self {
        let (pictures, picture_trouble) = match server
            .kit()
            .map(|kit| kit.store.pictures_dir())
            .and_then(Pictures::begin)
        {
            Ok(pictures) => (Some(pictures), None),
            Err(why) => (None, Some(format!("no pictures: {why}"))),
        };
        Live {
            server,
            values,
            pictures,
            picture_trouble,
            target: Target::Nothing,
            before_target: Target::Nothing,
            skip_next_before: false,
            skipped: Vec::new(),
        }
    }

    /// What the reply says of the pictures: where they are and how many, or why
    /// there are none, and which were left untaken because a secret was on screen.
    pub(super) fn pictures_note(&self) -> Value {
        let mut note = match &self.pictures {
            Some(pictures) => json!({
                "folder": pictures.dir().display().to_string(),
                "kept": pictures.count(),
                "note": "one picture before and one after each step, of the last run only",
            }),
            None => json!({"kept": 0}),
        };
        if let Some(trouble) = &self.picture_trouble {
            note["trouble"] = json!(trouble);
        }
        if !self.skipped.is_empty() {
            note["skipped"] = json!(self.skipped);
        }
        note
    }

    /// One step, as the live desktop runs it (see [`Runner::run`]). `stored` is
    /// the step as saved, `filled` the same with its parameters in.
    fn step(&mut self, stored: &Step, filled: &Step) -> Result<String, String> {
        // The client went away: the person who asked is not there to be told how
        // it ended, and nothing more is done for them.
        if self.server.client_gone() {
            return Err("the client went away; the replay stopped here".to_string());
        }
        let args = live_args(filled)?;
        // What the hooks are shown: the step's real arguments with each
        // parameter's value left as its `{{name}}`.
        let seen = hidden_params(&args, stored);
        // A link or a button is invoked through its own pattern when it has one:
        // that needs no mouse, works for a control scrolled out of view and takes
        // no focus. The recorded click runs INSTEAD only when the invoke was
        // refused before it did anything; see [`after_invoke`].
        if let Some(invoke) = invoke_for(filled) {
            match after_invoke(self.server.gated_call("element_action", &invoke)) {
                AfterInvoke::Done(answer) => return answer,
                AfterInvoke::ClickAsRecorded => {}
            }
        }
        self.server
            .gated_call_as(&filled.tool, &args, &seen)
            .map(|content| text_of(&content))
    }

    /// The picture of the window `step` is for (else the screen), taken THROUGH
    /// the gates a `screenshot` call passes: its tier, the operator's pre-tool
    /// hook (a hook that refuses `screenshot` refuses these), the
    /// blocked-application guards, the stop file, the post-tool hook and the
    /// journal. Not a side door to the screen. Tests replace it ([`Shooter`]).
    fn take(&mut self, step: &Step) -> Result<Vec<u8>, String> {
        if let Some(shooter) = self.server.kit().ok().and_then(|kit| kit.shooter.as_ref()) {
            return shooter(step);
        }
        let mut args = json!({"format": "jpeg"});
        if let Some(window) = step_window(step).and_then(|s| window_id_for(&s).ok()) {
            args["window"] = json!(window);
        }
        let content = self.server.gated_call("screenshot", &args)?;
        image_bytes(&content)
    }
}

impl Runner for Live<'_> {
    fn run(&mut self, _n: usize, step: &Step) -> Result<String, String> {
        // The parameters go into the arguments of this one call, in memory, and
        // come out of whatever it answers.
        let filled = self.values.fill(step);
        self.before_target = self.target.clone();
        let answer = self
            .step(step, &filled)
            .map(|answer| self.values.scrub(&answer))
            .map_err(|why| self.values.scrub(&why));
        self.target.after(step);
        answer
    }

    fn settle(&mut self) {
        std::thread::sleep(crate::limits::get().settle);
    }

    fn look(&mut self, n: usize, step: &Step, when: When) -> Option<PathBuf> {
        self.pictures.as_ref()?;
        // A picture is of a screen with a password on it: not when this step has
        // just typed a secret (its field is filled in, and may show), nor before
        // the one after it (it is still there).
        match when {
            When::Before if std::mem::take(&mut self.skip_next_before) => {
                self.skipped.push(format!(
                    "step {n}: no picture before: the step before it typed a secret"
                ));
                return None;
            }
            When::After if types_a_secret(step, &self.before_target) => {
                self.skip_next_before = true;
                self.skipped.push(format!(
                    "step {n}: no picture after: it typed a secret (a parameter, or text \
                     into a field that is, or may be, a password field)"
                ));
                return None;
            }
            _ => {}
        }
        let taken = self.take(step);
        let pictures = self.pictures.as_mut()?;
        match taken.and_then(|bytes| pictures.keep(n, when, "jpg", &bytes)) {
            Ok(path) => Some(path),
            Err(why) => {
                self.picture_trouble
                    .get_or_insert(format!("step {n}: no picture: {why}"));
                None
            }
        }
    }
}

/// `args` (a step's real arguments for the tool) with the typed text of a step
/// that has parameters put back to the stored `{{name}}` form, for the hooks.
pub(super) fn hidden_params(args: &Value, stored: &Step) -> Value {
    if params::names_of(stored).is_empty() {
        return args.clone();
    }
    let mut seen = args.clone();
    for key in ["text", "value"] {
        if let (Some(template), Some(slot)) = (stored.args.get(key), seen.get_mut(key)) {
            *slot = template.clone();
        }
    }
    seen
}

/// The bytes of the image in a tool's answer.
fn image_bytes(content: &[Value]) -> Result<Vec<u8>, String> {
    use base64::Engine;
    let data = content
        .iter()
        .find(|block| block.get("type").and_then(Value::as_str) == Some("image"))
        .and_then(|block| block.get("data"))
        .and_then(Value::as_str)
        .ok_or("the screenshot answered no image")?;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| format!("the screenshot's image is not base64: {e}"))
}

/// What to do after the `element_action invoke` tried in place of a recorded
/// click.
pub(super) enum AfterInvoke {
    /// That is the step's answer, good or bad.
    Done(Result<String, String>),
    /// The invoke was refused before it did anything: run the recorded click.
    ClickAsRecorded,
}

/// Decide from what the invoke answered.
///
/// The click runs after an error ONLY when the error says the control cannot be
/// invoked at all ([`refused_before_acting`]). Any other error (the pattern call
/// failed, the call timed out, the operator's hook or the stop refused it) may
/// have come after the control acted, and a second action on top would press a
/// button twice: the step fails with that error and the replay stops.
pub(super) fn after_invoke(answer: Result<Vec<Value>, String>) -> AfterInvoke {
    match answer {
        Ok(content) => AfterInvoke::Done(Ok(text_of(&content))),
        Err(why) if refused_before_acting(&why) => AfterInvoke::ClickAsRecorded,
        Err(why) => AfterInvoke::Done(Err(why)),
    }
}

/// Whether an `element_action invoke` error says the control does not offer the
/// action: nothing was done, as each platform words it (UI Automation: "does not
/// support", macOS and Linux: "does not offer", "offers no actions").
///
/// Narrow on purpose. It matches the whole of each platform's message (the
/// action's name in quotes AND the list of what the control does offer that
/// follows it), so a message that merely contains some of those words (an
/// operator's hook explaining a refusal) is not taken for one.
pub(super) fn refused_before_acting(why: &str) -> bool {
    let said = why.to_lowercase();
    said.contains("does not support \"invoke\"; it supports:")
        || (said.contains("does not offer \"invoke\"") && said.contains("; it offers:"))
        || said.contains("offers no actions; use click_element or set_value")
}

/// The window a step is for, as a selector of only its window part: the one its
/// own selector names, or its `in_window`.
fn step_window(step: &Step) -> Option<Selector> {
    let raw = step
        .args
        .get("selector")
        .or_else(|| step.args.get(IN_WINDOW))?
        .as_str()?;
    let whole = Selector::parse(raw).ok()?;
    whole
        .has_window_part()
        .then(|| Selector::parse(&whole.render_window_part()).ok())?
}

/// Whether the desktop says the control `raw` names is a password field, now.
/// Asked when a routine is saved, of a control that no name condemns; the control
/// may not be on screen any more, and then the answer is no.
fn desktop_says_password(raw: &str) -> bool {
    let Ok(selector) = Selector::parse(raw) else {
        return false;
    };
    // Only a selector that says which window: without one it would be looked for
    // in whatever is in front.
    if !selector.has_window_part() || !selector.has_element_part() {
        return false;
    }
    resolve_selector(None, &selector).is_ok_and(|found| found.element.secret)
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

/// The records `ids` names, oldest first.
fn records_of(source: &dyn StepSource, ids: &Value) -> Result<Vec<Record>, String> {
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
    Ok(records)
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
        "step_count": routine.steps.len(),
        // Everything each step types and every selector, in full: this is what
        // the person reads to see what the routine will do before it runs.
        "steps": routine.steps.iter().map(Step::review).collect::<Vec<_>>(),
        "learned": routine.date,
        "last_used": crate::routine::day_of(routine.last_used_ms),
        "taught": routine.taught,
    });
    let needs = params::names(&routine.steps);
    if !needs.is_empty() {
        shown["params_needed"] = json!(needs);
    }
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
        // Every step with the full text it types: read it, it is what will run.
        "steps": cleaned.steps.iter().map(Step::review).collect::<Vec<_>>(),
        "looking_steps_dropped": cleaned.dropped,
        "file": file.display().to_string(),
    });
    if saved.unkept {
        reply["kept_flag_dropped"] = json!(
            "the routine this replaced had been kept by the person, and these steps are not \
             the ones they kept, so it is NOT kept now and will expire after 30 days unused. \
             Only the person can keep it again, after reading it: `incu routine keep`"
        );
    }
    if !cleaned.parameters.is_empty() {
        reply["parameters"] = json!(cleaned
            .parameters
            .iter()
            .map(|p| json!({
                "name": p.name,
                "step": p.step,
                "why": match p.why {
                    Why::SecureField => "typed into a password or other secure field",
                    Why::Redacted => "the recording kept only a placeholder for the text",
                    Why::UnknownField => {
                        "typed after a key that can move focus (Tab, Enter, an arrow), so \
                         the field is not known and may be a password field"
                    }
                },
            }))
            .collect::<Vec<_>>());
        reply["parameters_note"] = json!(
            "those steps are NOT stored with their text. routine_run refuses to start \
             unless it is given a REFERENCE for every one, as params: {\"name\": {\"env\": \
             \"INCU_PARAM_NAME\"}}: the person sets that variable in the environment of the \
             incu process, so the value never passes through you or your transcript (a \
             literal value is refused). It is used in memory for that run only and is \
             never written to the routines file, the journal or what the hooks are shown"
        );
    }
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
