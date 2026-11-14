//! Running a routine again: in order, to the first failure, with no model.
//!
//! # What a replay is
//!
//! The same tool calls, run in order through whatever [`Runner`] is given. The
//! server's runner sends each one through the SAME gates a live call passes (the
//! stop file, the permission tier, the operator's pre-tool hook, the
//! blocked-application guards, the action-rate cap, the post-tool hook and the
//! journal): a replay is not a way around any of them.
//!
//! # What it does when a step fails
//!
//! It stops there and says which step and why ([`Failure`]). It does not retry,
//! does not skip, does not guess and does not repair the step. The caller, an
//! agent, is told to do the whole task again from the screen as it now is and to
//! save that run, which replaces the routine (Atticus: "no step repair"). A
//! step has failed when the tool refused or errored, or when a `wait_for` came
//! back as a timeout or a stop, because that is what "the screen is not as the
//! routine expects" looks like.
//!
//! Before and after each step the runner is asked for a picture
//! ([`Runner::look`], kept by [`super::pictures`]); a failure names the failed
//! step's two. A picture that cannot be taken is no reason to fail a replay.
//!
//! Every step is checked with [`super::learn::check_step`] BEFORE the first one
//! runs, so a routines file edited by hand cannot get a coordinate click or an
//! `eN` id past the rule the learner applies, and a bad step means nothing ran.

use std::fmt;
use std::path::PathBuf;

use super::learn::{check_step, wait_failure};
use super::pictures::When;
use super::{Routine, Step};

/// What runs one step. The server's own implements it over its gates; tests
/// implement it over a script.
pub trait Runner {
    /// A picture of the screen, or of the window the step is for, before or
    /// after `step` (step `n`): the path it was kept at. `None` when there is no
    /// picture (the default, and a screen that could not be captured: a
    /// replay does not fail for want of one).
    fn look(&mut self, _n: usize, _step: &Step, _when: When) -> Option<PathBuf> {
        None
    }

    /// Run `step` as step `n`. `Ok` is what the tool answered, as text; `Err` is
    /// its refusal or error, in its own words.
    ///
    /// # Errors
    ///
    /// The step was refused or failed.
    fn run(&mut self, n: usize, step: &Step) -> Result<String, String>;

    /// A pause after a step that changed the screen, before the next one looks
    /// at it. The default does nothing; the live runner waits the desktop's
    /// settle time.
    fn settle(&mut self) {}
}

/// One step that ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    /// The step in one line ([`Step::summary`]).
    pub summary: String,
    /// The first line of what it answered.
    pub answer: String,
}

/// A replay that ran every step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The steps, in order.
    pub ran: Vec<Ran>,
}

/// A replay that stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The routine's phrase.
    pub phrase: String,
    /// The failed step, 1-based.
    pub step: usize,
    /// How many steps the routine has.
    pub total: usize,
    /// The failed step in one line.
    pub summary: String,
    /// Why it failed.
    pub why: String,
    /// The step was refused before anything ran (so no step ran).
    pub before_start: bool,
    /// Where the picture taken before the failed step is kept, when there is
    /// one.
    pub before_picture: Option<String>,
    /// Where the picture taken after it is kept: what the screen showed when it
    /// failed.
    pub after_picture: Option<String>,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Failure {
            phrase,
            step,
            total,
            summary,
            why,
            before_start,
            before_picture,
            after_picture,
        } = self;
        if *before_start {
            write!(
                f,
                "routine \"{phrase}\" was not run: step {step} of {total} ({summary}) cannot be \
                 replayed: {why}. Nothing was done."
            )?;
        } else {
            write!(
                f,
                "routine \"{phrase}\" failed at step {step} of {total} ({summary}): {why}. \
                 {} step(s) before it ran; nothing was retried or repaired.",
                step - 1
            )?;
        }
        match (before_picture, after_picture) {
            (None, None) => {}
            (before, after) => {
                write!(f, " Pictures of step {step}:")?;
                if let Some(path) = before {
                    write!(f, " before: {path}")?;
                }
                if let Some(path) = after {
                    write!(f, " after: {path}")?;
                }
                write!(f, ".")?;
            }
        }
        write!(
            f,
            " Do the whole task yourself from the screen as it is now, then routine_save it \
             under the same phrase to replace this routine, or routine_forget it."
        )
    }
}

/// Run `routine` through `runner`, stopping at the first failure.
///
/// # Errors
///
/// A [`Failure`] naming the step and why: a step the rules refuse (nothing ran),
/// a step the tool refused or errored on, or a `wait_for` that timed out.
pub fn replay(routine: &Routine, runner: &mut dyn Runner) -> Result<Report, Box<Failure>> {
    let total = routine.steps.len();
    type Pictures = (Option<PathBuf>, Option<PathBuf>);
    let failure = |step: usize, why: String, before_start: bool, pictures: &Pictures| {
        Box::new(Failure {
            phrase: routine.phrase.clone(),
            step,
            total,
            summary: routine.steps[step - 1].summary(),
            why,
            before_start,
            before_picture: pictures.0.as_ref().map(|p| p.display().to_string()),
            after_picture: pictures.1.as_ref().map(|p| p.display().to_string()),
        })
    };
    let none: Pictures = (None, None);
    for (i, step) in routine.steps.iter().enumerate() {
        check_step(step, i + 1).map_err(|why| failure(i + 1, why.to_string(), true, &none))?;
    }
    let mut ran = Vec::with_capacity(total);
    for (i, step) in routine.steps.iter().enumerate() {
        let n = i + 1;
        let before = runner.look(n, step, When::Before);
        let answered = runner.run(n, step);
        // The screen has had its moment before the picture of it: after every
        // step that changes it, the last included. A wait looks for itself.
        if step.tool != "wait_for" {
            runner.settle();
        }
        let pictures = (before, runner.look(n, step, When::After));
        let answer = answered.map_err(|why| failure(n, why, false, &pictures))?;
        if step.tool == "wait_for" {
            if let Some(why) = wait_failure(&answer) {
                return Err(failure(n, why, false, &pictures));
            }
        }
        if let Some(why) = unverified_before(step, &answer, routine.steps.get(n)) {
            return Err(failure(n, why, false, &pictures));
        }
        ran.push(Ran {
            summary: step.summary(),
            answer: first_line(&answer),
        });
    }
    Ok(Report { ran })
}

/// The steps whose "unverified" is a reason to stop: the ones that SET A VALUE,
/// where "unverified" means the value may not have taken (`set_value`, and the
/// `element_action`s that set one, [`sets_a_value`]).
///
/// NOT a click, an `invoke`, a `press` or a `scroll_into_view`: those always
/// answer "unverified" (live, on Windows, every one of them), because a button
/// press has no state to read back. Stopping on it would stop every routine that
/// presses a button; the next step's own selector, or a `wait_for`, is what
/// checks one.
fn reads_back(step: &Step) -> bool {
    step.tool == "set_value" || sets_a_value(step)
}

/// Whether `step` is an `element_action` that sets a value (a spinner's
/// `increment` or `decrement`), as opposed to pressing, invoking or scrolling.
fn sets_a_value(step: &Step) -> bool {
    step.tool == "element_action"
        && step
            .args
            .get("action")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|a| {
                matches!(
                    a.trim().to_ascii_lowercase().as_str(),
                    "increment" | "decrement"
                )
            })
}

/// The steps that act on what the step before them did.
const DEPENDS_ON_THE_SCREEN: &[&str] = &[
    "click_element",
    "set_value",
    "element_action",
    "type_text",
    "paste_text",
    "key",
];

/// Why the replay must stop after `step` answered `answer`, when the answer says
/// the effect could not be confirmed ("unverified") and `next` is a step that
/// acts on it. `None` when it was confirmed, when nothing follows, or when a
/// `wait_for` follows, because that checks the screen itself.
///
/// Live, `set_value` into a window that was not in front reported "unverified"
/// though nothing had changed; carrying on from there types or clicks into a
/// state the routine never reached.
fn unverified_before(step: &Step, answer: &str, next: Option<&Step>) -> Option<String> {
    let next = next.filter(|n| DEPENDS_ON_THE_SCREEN.contains(&n.tool.as_str()))?;
    let said = answer.to_lowercase();
    if !reads_back(step) || !(said.contains("unverified") || said.contains("tried to set")) {
        return None;
    }
    Some(format!(
        "it answered \"{}\", so its effect is not confirmed, and the next step ({}) depends on it",
        first_line(answer),
        next.tool
    ))
}

/// The first line of an answer, bounded.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    let mut out: String = line.chars().take(160).collect();
    if line.chars().count() > 160 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A scripted desktop: each step answers from a list, and every call is
    /// logged, so a test can say exactly what ran.
    struct Script {
        answers: Vec<Result<String, String>>,
        calls: Vec<String>,
        settles: usize,
    }

    impl Script {
        fn answering(answers: Vec<Result<String, String>>) -> Self {
            Self {
                answers,
                calls: Vec::new(),
                settles: 0,
            }
        }
    }

    impl Runner for Script {
        fn run(&mut self, n: usize, step: &Step) -> Result<String, String> {
            self.calls.push(format!("{n}:{}", step.tool));
            self.answers
                .get(n - 1)
                .cloned()
                .unwrap_or_else(|| Ok("ok".to_string()))
        }

        fn settle(&mut self) {
            self.settles += 1;
        }
    }

    fn routine(steps: Vec<Step>) -> Routine {
        Routine {
            phrase: "send the report".to_string(),
            steps,
            date: String::new(),
            last_used_ms: 0,
            taught: false,
        }
    }

    fn three() -> Routine {
        routine(vec![
            Step::new(
                "click_element",
                json!({"selector": "app=mail;window=Inbox;role=button;name=New"}),
            ),
            Step::new(
                "wait_for",
                json!({"condition": "window_appears", "selector": "window~=Compose"}),
            ),
            Step::new(
                "type_text",
                json!({"text": "hello", "in_window": "window~=Compose"}),
            ),
        ])
    }

    fn set_then_click(answer: &str) -> (Routine, Script) {
        let routine = routine(vec![
            Step::new(
                "set_value",
                json!({"selector": "app=mail;window~=Compose;role=edit;name=To", "value": "ann"}),
            ),
            Step::new(
                "click_element",
                json!({"selector": "app=mail;window~=Compose;role=button;name=Send"}),
            ),
        ]);
        (routine, Script::answering(vec![Ok(answer.to_string())]))
    }

    #[test]
    fn a_step_that_reports_unverified_stops_the_replay_when_the_next_step_depends_on_it() {
        let (routine, mut desk) = set_then_click(
            "set 3 chars (unverified: it reads back as 0 characters, not the 3 sent)",
        );
        let failed = replay(&routine, &mut desk).unwrap_err();
        assert_eq!(failed.step, 1);
        assert_eq!(desk.calls, ["1:set_value"], "the click never ran");
        assert!(failed.why.contains("unverified"), "{}", failed.why);
        assert!(failed.why.contains("click_element"), "{}", failed.why);
        assert!(
            failed.to_string().contains("failed at step 1 of 2"),
            "{failed}"
        );
    }

    #[test]
    fn a_verified_step_lets_the_replay_go_on() {
        let (routine, mut desk) = set_then_click("set 3 chars (verified)");
        assert_eq!(replay(&routine, &mut desk).unwrap().ran.len(), 2);
    }

    #[test]
    fn unverified_is_no_reason_to_stop_when_nothing_acts_on_it_next() {
        // Last step: nothing depends on it.
        let (mut routine, mut desk) = set_then_click("set 3 chars (verified)");
        routine.steps.pop();
        desk.answers = vec![Ok("set 3 chars (unverified: not read back)".to_string())];
        assert!(replay(&routine, &mut desk).is_ok());
        // A wait follows: it looks at the screen itself.
        let mut waits = routine.clone();
        waits.steps.push(Step::new(
            "wait_for",
            json!({"condition": "window_gone", "selector": "window~=Compose"}),
        ));
        let mut desk = Script::answering(vec![
            Ok("set 3 chars (unverified: not read back)".to_string()),
            Ok("{\"result\": \"found\"}".to_string()),
        ]);
        assert!(replay(&waits, &mut desk).is_ok());
    }

    #[test]
    fn a_click_always_says_unverified_and_never_stops_the_replay() {
        let mut desk = Script::answering(vec![
            Ok("clicked x (unverified: a click has no state to read back)".to_string()),
            Ok("{\"result\": \"found\"}".to_string()),
            Ok("typed".to_string()),
        ]);
        assert!(replay(&three(), &mut desk).is_ok());
    }

    #[test]
    fn every_step_runs_in_order_and_the_report_says_so() {
        let mut desk = Script::answering(vec![
            Ok("clicked\nmore".to_string()),
            Ok("{\n  \"result\": \"found\"\n}".to_string()),
            Ok("typed 5 characters".to_string()),
        ]);
        let report = replay(&three(), &mut desk).unwrap();
        assert_eq!(desk.calls, ["1:click_element", "2:wait_for", "3:type_text"]);
        assert_eq!(report.ran.len(), 3);
        assert_eq!(report.ran[0].answer, "clicked", "only the first line");
        assert!(report.ran[2].summary.contains("5 characters"));
        assert!(
            !report.ran[2].summary.contains("hello"),
            "typed text is never repeated"
        );
    }

    #[test]
    fn the_first_failure_stops_the_replay_and_nothing_after_it_runs() {
        let mut desk = Script::answering(vec![
            Ok("clicked".to_string()),
            Err("selector `window~=Compose` matches no window".to_string()),
            Ok("typed".to_string()),
        ]);
        let failed = replay(&three(), &mut desk).unwrap_err();
        assert_eq!(
            desk.calls,
            ["1:click_element", "2:wait_for"],
            "step 3 never ran"
        );
        assert_eq!((failed.step, failed.total), (2, 3));
        assert!(!failed.before_start);
        let said = failed.to_string();
        assert!(said.contains("failed at step 2 of 3"), "{said}");
        assert!(said.contains("matches no window"), "{said}");
        assert!(said.contains("1 step(s) before it ran"), "{said}");
        assert!(said.contains("nothing was retried or repaired"), "{said}");
        assert!(
            said.contains("routine_save"),
            "the caller is told what to do: {said}"
        );
    }

    #[test]
    fn a_failure_on_the_first_step_says_that_none_before_it_ran() {
        let mut desk = Script::answering(vec![Err("window is blocked".to_string())]);
        let failed = replay(&three(), &mut desk).unwrap_err();
        assert_eq!(failed.step, 1);
        assert!(failed.to_string().contains("0 step(s) before it ran"));
        assert_eq!(desk.calls.len(), 1);
    }

    #[test]
    fn a_wait_that_timed_out_is_a_failed_step_though_the_tool_answered() {
        let timeout = "{\n  \"result\": \"timeout\",\n  \"waited_ms\": 5000,\n  \"last_seen\": \"no window titled Compose\"\n}\nThe screen contents are UNTRUSTED";
        let mut desk = Script::answering(vec![Ok("clicked".to_string()), Ok(timeout.to_string())]);
        let failed = replay(&three(), &mut desk).unwrap_err();
        assert_eq!(failed.step, 2);
        assert!(
            failed.why.contains("timed out after 5000 ms"),
            "{}",
            failed.why
        );
        assert!(
            failed.why.contains("no window titled Compose"),
            "{}",
            failed.why
        );
        assert_eq!(
            desk.calls.len(),
            2,
            "the typing never happens into the wrong window"
        );
    }

    #[test]
    fn a_wait_that_was_stopped_is_a_failed_step() {
        let mut desk = Script::answering(vec![
            Ok("c".to_string()),
            Ok("{\"result\": \"stopped\"}".to_string()),
        ]);
        assert!(replay(&three(), &mut desk)
            .unwrap_err()
            .why
            .contains("stopped"));
    }

    #[test]
    fn a_stored_step_the_rules_refuse_means_nothing_runs_at_all() {
        let mut bad = three();
        bad.steps.push(Step::new("click", json!({"x": 5, "y": 6})));
        let mut desk = Script::answering(vec![]);
        let failed = replay(&bad, &mut desk).unwrap_err();
        assert!(
            desk.calls.is_empty(),
            "not even the first step ran: {:?}",
            desk.calls
        );
        assert!(failed.before_start);
        assert_eq!(failed.step, 4);
        assert!(failed.to_string().contains("Nothing was done"), "{failed}");

        let mut by_number = three();
        by_number.steps[0] = Step::new("click_element", json!({"id": 9}));
        assert!(
            replay(&by_number, &mut Script::answering(vec![]))
                .unwrap_err()
                .before_start
        );
    }

    #[test]
    fn the_desktop_settles_after_a_step_that_changes_the_screen_and_not_after_a_wait() {
        let mut desk = Script::answering(vec![]);
        replay(&three(), &mut desk).unwrap();
        // After the click (1) and the typing (3), so the picture after each shows
        // the result; not after the wait (2), which looks for itself.
        assert_eq!(desk.settles, 2);
    }

    /// A button press is not a reason to stop: it ALWAYS says "unverified".
    #[test]
    fn a_recorded_button_press_does_not_stop_the_replay() {
        let invoke = |action: &str| {
            Step::new(
                "element_action",
                json!({"selector": "app=mail;window=Inbox;role=button;name=New", "action": action}),
            )
        };
        let typing = Step::new(
            "type_text",
            json!({"text": "hello", "in_window": "window~=Compose"}),
        );
        for action in ["invoke", "press", "scroll_into_view", "click"] {
            let routine = routine(vec![invoke(action), typing.clone()]);
            let mut desk = Script::answering(vec![Ok(
                "invoked button (unverified: a press has no state to read back)".to_string(),
            )]);
            let report = replay(&routine, &mut desk)
                .unwrap_or_else(|f| panic!("{action} stopped the replay: {f}"));
            assert_eq!(report.ran.len(), 2, "{action}");
        }
    }

    #[test]
    fn a_value_setting_action_that_is_unverified_does_stop_the_replay() {
        let spin = Step::new(
            "element_action",
            json!({"selector": "app=calc;window=Calc;role=edit;name=Count", "action": "increment"}),
        );
        let typing = Step::new(
            "type_text",
            json!({"text": "1", "in_window": "app=calc;window=Calc"}),
        );
        let mut desk =
            Script::answering(vec![Ok("incremented (unverified: no state)".to_string())]);
        let failed = replay(&routine(vec![spin, typing]), &mut desk).unwrap_err();
        assert_eq!(failed.step, 1);
        assert_eq!(desk.calls, ["1:element_action"]);
        // A set_value that only "tried to set" is the same.
        let (routine, mut desk) =
            set_then_click("tried to set e49 (Edit) to 17 chars: it reads back as nothing");
        assert!(replay(&routine, &mut desk).is_err());
    }

    /// What the scripted desktop sees, for the pictures.
    struct Watching {
        inner: Script,
        looks: Vec<String>,
    }

    impl Runner for Watching {
        fn look(&mut self, n: usize, _step: &Step, when: When) -> Option<PathBuf> {
            self.looks.push(format!("{n}:{when:?}"));
            Some(PathBuf::from(format!("shots/step-{n}-{when:?}.jpg")))
        }
        fn run(&mut self, n: usize, step: &Step) -> Result<String, String> {
            self.looks.push(format!("{n}:run"));
            self.inner.run(n, step)
        }
        fn settle(&mut self) {
            self.inner.settle();
        }
    }

    #[test]
    fn a_picture_is_taken_before_and_after_each_step() {
        let mut desk = Watching {
            inner: Script::answering(vec![]),
            looks: Vec::new(),
        };
        replay(&three(), &mut desk).unwrap();
        assert_eq!(
            desk.looks,
            [
                "1:Before", "1:run", "1:After", "2:Before", "2:run", "2:After", "3:Before",
                "3:run", "3:After"
            ]
        );
    }

    #[test]
    fn a_failure_names_the_pictures_of_the_failed_step() {
        let mut desk = Watching {
            inner: Script::answering(vec![
                Ok("clicked".to_string()),
                Err("selector matches no window".to_string()),
            ]),
            looks: Vec::new(),
        };
        let failed = replay(&three(), &mut desk).unwrap_err();
        assert!(failed
            .before_picture
            .as_deref()
            .unwrap()
            .contains("step-2-Before"));
        assert!(failed
            .after_picture
            .as_deref()
            .unwrap()
            .contains("step-2-After"));
        let said = failed.to_string();
        assert!(said.contains("Pictures of step 2"), "{said}");
        assert!(said.contains("step-2-Before.jpg"), "{said}");
        assert!(said.contains("step-2-After.jpg"), "{said}");
        // A wait that timed out is a failure with pictures too.
        let timeout = "{\"result\": \"timeout\", \"waited_ms\": 1}";
        let mut desk = Watching {
            inner: Script::answering(vec![Ok("c".to_string()), Ok(timeout.to_string())]),
            looks: Vec::new(),
        };
        assert!(replay(&three(), &mut desk)
            .unwrap_err()
            .to_string()
            .contains("step-2-After"));
        // And with no pictures at all, the message has no picture sentence.
        let mut plain = Script::answering(vec![Err("nope".to_string())]);
        let said = replay(&three(), &mut plain).unwrap_err().to_string();
        assert!(!said.contains("Pictures of step"), "{said}");
    }

    #[test]
    fn a_routine_with_no_steps_runs_nothing_and_succeeds_empty() {
        let mut desk = Script::answering(vec![]);
        assert!(replay(&routine(vec![]), &mut desk).unwrap().ran.is_empty());
    }
}
