//! Splitting one instruction into role-tagged steps.
//!
//! # The gap this closes
//!
//! [`crate::roles`] decides which model serves which kind of work, and
//! [`crate::agent::worker`] switches the model when a task names a role. Both
//! were finished and tested, and neither had ever run: nothing in production
//! ever put a role on a task. `Task::with_tier` had no caller outside tests, so
//! `task.role()` was always `None`, the routing branch never fired, and every
//! task — reading a directory or designing an interface — ran on whichever
//! model the session happened to open with.
//!
//! The missing piece was never the routing. It was the DECISION: who looks at a
//! job and says "this part is retrieval, that part is judgement". That decision
//! is what this module collects.
//!
//! # The shape
//!
//! The planner writes a block, exactly as [`crate::ask`] does for a question,
//! because the agent runs as a child process and can call nothing:
//!
//! ```text
//! <plan>
//! explore: find every file that mentions the box row
//! diagnose: work out which writer clears it, with line numbers
//! implement: fix it so the box survives a repaint
//! </plan>
//! ```
//!
//! One step per line, `role: what to do`. [`prompt_block`] teaches the format
//! and rides the planning turn.
//!
//! # The rule the planner does not get to bend
//!
//! **The planner plans. It does not do the work.** Its whole value is that the
//! expensive model reads the job once, decides the shape, and then never sees a
//! raw file again — the cheap models read those and report back. A planner that
//! starts implementing has spent the expensive tokens on exactly the work the
//! split existed to move somewhere cheaper, and it does it while looking
//! productive.
//!
//! And, as everywhere else here: a role decides WHO does the work. It decides
//! nothing about whether the work is finished — that is settled by a command
//! exiting zero, never by a model's opinion of its own output.

/// The permission mode a planning turn runs in.
///
/// # Why this is a flag and not a sentence in the prompt
///
/// [`prompt_block`] tells the planner not to read files or write code, and that
/// was the ONLY thing stopping it — a sentence, which a model under pressure
/// can talk itself past while looking productive. The moment it does, the
/// expensive model has read the files itself and the split has cost more than
/// it saved.
///
/// The agent CLI has a mode for exactly this, so the rule is handed to the
/// thing that enforces it instead of being asked for. Verified present on the
/// installed CLI: `--permission-mode` lists `plan` among its choices.
///
/// The prompt keeps saying it too. The flag stops the planner acting; the
/// sentence tells it what to produce instead, and neither does the other's job.
pub const PLAN_MODE: &str = "plan";

/// The marker the planner writes around its steps.
pub const OPEN: &str = "<plan>";
/// Closes the block.
pub const CLOSE: &str = "</plan>";

/// How many steps one plan may hold.
///
/// A bound rather than a target. Each step is a real turn against a real model,
/// so an unbounded plan is an unbounded run — and a plan long enough to need
/// more than this is a plan that should have been an item on the roadmap.
pub const MAX_STEPS: usize = 12;

/// One piece of work, and the kind of work it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The role, lower-cased, and always one of [`crate::roles::KNOWN_ROLES`].
    ///
    /// Guaranteed by the parser, which is what lets a caller hand it straight
    /// to [`crate::ledger::Task::with_tier`] and know it will route. A name
    /// outside the list never reaches here: it is reported by
    /// [`parse_detailed`] instead, because most of the time it is prose and the
    /// rest of the time it is a typo the operator needs to see.
    pub role: String,
    /// What that step is to do.
    pub instruction: String,
    /// Step numbers, 1-based, that must finish before this one starts.
    ///
    /// # Why the planner declares this and the harness does not infer it
    ///
    /// Two steps in one plan may be genuinely independent — "add the login
    /// page" and "add the signup page" — or strictly ordered, where the second
    /// edits what the first created. Nothing the harness can see tells those
    /// apart. The model that wrote the plan is the only participant that knows,
    /// and it knew at the moment it wrote them down.
    ///
    /// # Empty means independent, and the failure is caught rather than feared
    ///
    /// Defaulting to "depends on the previous step" would be safer in the
    /// abstract and would also make [`crate::parallel`] dead on arrival: a
    /// chain has exactly one ready task at a time, forever.
    ///
    /// Defaulting to independent risks a planner that forgets to declare an
    /// ordering — and that failure is self-correcting. Two lanes editing the
    /// same lines conflict, the merge is refused, and the task is retried
    /// sequentially where it can see the other change. The cost of the wrong
    /// default is a wasted lane; the cost of the safe default is the feature.
    pub after: Vec<usize>,
}

/// The steps in what the planner said.
///
/// Tolerant in the same way [`crate::ask::parse`] is: an unclosed marker is read
/// to the end rather than dropped, because a plan the harness silently swallowed
/// is worse than one read slightly wrong.
///
/// A line without a colon is prose, not a step. Models write a sentence before
/// and after a block routinely, and treating those as work would spend a turn
/// asking a model to do a sentence.
#[must_use]
pub fn parse(text: &str) -> Vec<Step> {
    parse_detailed(text).0
}

/// Finds `needle` in `haystack` ignoring ASCII case.
fn find_tag_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if haystack.len() < needle.len() {
        return None;
    }
    // Compared as BYTES. Slicing the `str` `needle.len()` bytes on from a
    // character boundary panics whenever a multi-byte character straddles the
    // end — found when a reply with an em dash after its `</plan>` crashed the
    // parse. A byte window cannot split anything, and an ASCII tag can only
    // match ASCII bytes.
    let (hay, want) = (haystack.as_bytes(), needle.as_bytes());
    haystack.char_indices().find_map(|(i, _)| {
        hay.get(i..i + want.len())
            .filter(|window| window.eq_ignore_ascii_case(want))
            .map(|_| i)
    })
}

/// Strips list bullets, numbers, and step labels from the beginning of a line.
///
/// Handles:
/// - Bullets: `-`, `*`, `+`, `•`
/// - Numbered markers: `1.`, `1)`, `(1)`, `[1]`
/// - Prefixes: `Step 1:`, `Step 1.`, `Step 1 -`, `Phase 1:`, `Task 1:`
/// - Combinations: `- 1.`, `* Step 1:`
fn strip_list_prefix(mut line: &str) -> &str {
    line = line.trim();

    loop {
        let prev = line;

        // 1. Strip bullet characters (+, -, *, •)
        line = line
            .trim_start_matches(['-', '*', '+', '\u{2022}'])
            .trim_start();

        // 2. Strip bracketed numbers: (1) or [1]
        if let Some(rest) = line.strip_prefix('(').or_else(|| line.strip_prefix('[')) {
            if let Some(close_idx) = rest.find([')', ']']) {
                let inner = &rest[..close_idx];
                if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit()) {
                    line = rest[close_idx + 1..].trim_start();
                }
            }
        }

        // 3. Strip standard numbers: 1. or 1) or 1: or 1-
        let digits_len = line.chars().take_while(char::is_ascii_digit).count();
        if digits_len > 0 {
            let after_digits = &line[digits_len..];
            if let Some(rest) = after_digits
                .strip_prefix('.')
                .or_else(|| after_digits.strip_prefix(')'))
                .or_else(|| after_digits.strip_prefix(':'))
                .or_else(|| after_digits.strip_prefix('-'))
            {
                line = rest.trim_start();
            }
        }

        // 4. Strip "Step 1:", "Step 1.", "Task 1:", "Phase 1:" prefixes
        for prefix in ["step", "phase", "task"] {
            if line.len() >= prefix.len() && line[..prefix.len()].eq_ignore_ascii_case(prefix) {
                let after_prefix = line[prefix.len()..].trim_start();
                let digits_len = after_prefix
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .count();
                let after_digits = after_prefix[digits_len..].trim_start();
                if let Some(rest) = after_digits
                    .strip_prefix(':')
                    .or_else(|| after_digits.strip_prefix('.'))
                    .or_else(|| after_digits.strip_prefix('-'))
                {
                    line = rest.trim_start();
                } else if digits_len > 0
                    && (after_digits.starts_with(char::is_whitespace) || after_digits.is_empty())
                {
                    line = after_digits.trim_start();
                }
            }
        }

        if line == prev {
            break;
        }
    }

    line
}

/// The steps, and the names that looked like roles and were not.
///
/// The second list is what stops a mistyped role from being work that vanishes
/// without a sound. A caller shows it; nothing here decides what to do about it.
#[must_use]
pub fn parse_detailed(text: &str) -> (Vec<Step>, Vec<String>) {
    let mut out = Vec::new();
    let mut unknown = Vec::new();
    let mut rest = text;

    while let Some(start) = find_tag_ignore_case(rest, OPEN) {
        let after = &rest[start + OPEN.len()..];
        let (body, next) = match find_tag_ignore_case(after, CLOSE) {
            Some(end) => (&after[..end], &after[end + CLOSE.len()..]),
            None => (after, ""),
        };
        for line in body.lines() {
            if out.len() >= MAX_STEPS {
                return (out, unknown);
            }
            match parse_step(line) {
                Ok(step) => out.push(step),
                Err(Some(name)) => {
                    if !unknown.contains(&name) {
                        unknown.push(name);
                    }
                }
                Err(None) => {}
            }
        }
        if next.is_empty() {
            break;
        }
        rest = next;
    }
    (out, unknown)
}

/// One line as a step, or the word before its colon when that is not a role.
///
/// # Why this must match against the KNOWN roles
///
/// A first attempt accepted any single word before a colon, on the reasoning
/// that a typo should still carry its work. Its own test caught the hole
/// immediately: `Note: the box is drawn in footer.rs` is one word and a colon,
/// so a sentence became a task, and a turn would have been spent asking a model
/// to do a note. `Summary:`, `Warning:` and `Step 1:` all land the same way, and
/// prose around a block is what models write as a matter of course.
///
/// So a step names a role the harness actually has. What that costs is a typo'd
/// role, and that cost is paid by REPORTING it rather than by dropping it in
/// silence — see [`parse_detailed`]. The planner is handed the exact list of
/// role names in [`prompt_block`], so a name outside it is far more often prose
/// than intent.
fn parse_step(line: &str) -> Result<Step, Option<String>> {
    let line = strip_list_prefix(line);

    // No colon at all is prose, and prose is not worth reporting: it is the
    // normal shape of a reply, not a mistake anyone should be told about.
    let Some((role_raw, instruction)) = line.split_once(':') else {
        return Err(None);
    };

    // Strip leading colons in instruction (e.g. from "role:: instruction")
    let instruction = instruction.trim_start_matches(':').trim();

    // Extract base role before any parenthetical explanation, e.g. "explore (fast)" -> "explore"
    let base_role = role_raw.split(['(', '[']).next().unwrap_or("").trim();

    // Clean role: strip markdown styling (`*`, `_`, '`') and lower-case
    let role = base_role
        .trim_matches(['*', '_', '`'])
        .trim()
        .to_ascii_lowercase();

    if role.is_empty() || instruction.is_empty() || role.contains(char::is_whitespace) {
        return Err(None);
    }
    if !crate::roles::KNOWN_ROLES.iter().any(|known| *known == role) {
        // ONE WORD, A COLON, AND NOT A ROLE. Usually `Note:` or `Summary:`;
        // occasionally a misspelled role. Reported either way, because the
        // second case is work that would otherwise vanish without a sound.
        return Err(Some(role));
    }
    // `[after 1]` or `[after 1,2]`, anywhere in the instruction. Taken out of
    // the text, because it is an instruction to the HARNESS and handing it to
    // the agent as part of the work would have it trying to interpret one.
    let (instruction, after) = split_after(instruction);
    if instruction.is_empty() {
        return Err(None);
    }

    Some(Step {
        role,
        instruction,
        after,
    })
    .ok_or(None)
}

/// An instruction with its `[after N]` marker removed, and the numbers it named.
///
/// Tolerant on purpose: `[after 1]`, `[after 1,2]`, `[after 1, 2]` and
/// `[AFTER 1]` all read the same. A planner that gets the punctuation slightly
/// wrong should still have its ORDERING understood — the alternative is a
/// declared dependency silently becoming part of the prose, which is the one
/// outcome worse than not supporting the syntax at all.
fn split_after(instruction: &str) -> (String, Vec<usize>) {
    let lower = instruction.to_ascii_lowercase();
    let Some(open) = lower.find("[after") else {
        return (instruction.trim().to_string(), Vec::new());
    };
    let Some(close_offset) = instruction[open..].find(']') else {
        return (instruction.trim().to_string(), Vec::new());
    };
    let close = open + close_offset;

    let inside = &instruction[open + "[after".len()..close];
    let after: Vec<usize> = inside
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|piece| piece.parse::<usize>().ok())
        // Step numbers are 1-based in the plan a human reads. Zero is a
        // planner mistake, not a step.
        .filter(|n| *n > 0)
        .collect();

    let mut text = String::from(&instruction[..open]);
    text.push_str(&instruction[close + 1..]);
    (text.split_whitespace().collect::<Vec<_>>().join(" "), after)
}

/// What to tell the planner, so it produces a plan rather than a start.
///
/// Returns `None` when no role has a model assigned: with nothing to route to,
/// a planning turn is a turn spent producing a split nobody can act on, and the
/// work is better done directly.
#[must_use]
pub fn prompt_block(roles: &crate::roles::RoleTable) -> Option<String> {
    if roles.is_empty() {
        return None;
    }
    let mut out = String::from(
        "<planning>\nSplit the work below into steps, and write NOTHING else. Do not \
         read files, do not write code, do not start: this turn exists so the \
         expensive model decides the SHAPE once and the cheap models do the doing.\n\n\
         Answer with exactly this block, one step per line, `role: what to do`:\n\n\
         <plan>\nexplore: find every file that mentions the status row\n\
         diagnose: work out which writer clears it, with line numbers\n\
         implement: fix it so the status row survives a repaint\n</plan>\n\n\
         Roles available, chosen by what the work NEEDS:",
    );
    for role in roles.configured_roles() {
        match crate::roles::role_purpose(role) {
            Some(purpose) => out.push_str(&format!("\n  - {role}: {purpose}")),
            None => out.push_str(&format!("\n  - {role}")),
        }
    }
    out.push_str(&format!(
        "\n\nAt most {MAX_STEPS} steps. A step names a role and one job; a step that \
         says \"and then\" is two steps.\n\n\
         SAY WHICH STEPS MUST WAIT. End a step with `[after N]` — or `[after 1,2]` — \
         when it cannot start until step N is finished, counting from 1. Steps with \
         no `[after]` may run AT THE SAME TIME, on different agents, in separate \
         checkouts.\n\n\
         <plan>\nimplement: add the login page\n\
         implement: add the signup page\n\
         implement: share the form validation between them [after 1,2]\n</plan>\n\n\
         Mark it when the second step EDITS what the first created, or needs to read \
         it. Do not mark it merely because you wrote them in that order — two steps \
         that touch different files are independent, and saying otherwise makes the \
         work take twice as long for nothing.\n\n\
         HOW TO CUT THE STEPS:\n\
         - Each step is a THIN VERTICAL SLICE — a tracer bullet through every layer \
         it needs, leaving something the check can prove on its own. \"add the \
         table\", \"add the endpoint\", \"add the page\" is three steps nobody can \
         verify until the last; \"a user can sign up, end to end\" is one that can.\n\
         - PREFACTOR FIRST. When a change is hard because of how the code is shaped, \
         the first step reshapes it WITHOUT changing behaviour, and the change comes \
         after it.\n\
         - A WIDE REFACTOR is three phases, never one sweep: EXPAND (add the new \
         shape beside the old), MIGRATE the callers in batches, one step per batch, \
         then CONTRACT (remove the old) once nothing uses it. Every step leaves the \
         check green.\n\n\
         SOME FILES ARE THE PROJECT'S OWN CHECK — its tests, specs, CI config and \
         migrations. Nothing lands in one until the owner allows it: an attempt that \
         writes one is stopped for the owner, because a test rewritten by the thing \
         it judges is not a test that was passed.\n\n\
         TESTS FIRST, FOR A BEHAVIOUR CHANGE. The test is its own step and comes \
         FIRST; the change is the next step, `[after N]`:\n\n\
         <plan>\nimplement: add a test that the status row survives a repaint, marked \
         pending the stack's own way (`#[ignore]`, `it.todo`) so the check still passes\n\
         implement: make the repaint keep the status row, and turn that test on [after 1]\n\
         </plan>\n\n\
         The owner allowing the first step is the owner agreeing what \"done\" means, \
         before anything is built. The second step needs the same when it turns the \
         test on, and it is done only when the check passes with the test running. Never tack \"and add a test\" onto the step that makes the change; a \
         refactor that keeps behaviour needs no new test.\n\n\
         A role decides WHO does the work. It decides \
         nothing about whether the work is done: that is settled by running the \
         check, never by a model — including you — judging its own or another's \
         output.\n</planning>"
    ));
    Some(out)
}

/// Run one planning turn: split `instruction` into role-tagged steps.
///
/// # What this does to the backend, and why it is left that way
///
/// It sets [`PLAN_MODE`] before the turn and **does not restore the previous
/// mode afterwards**. That is deliberate rather than forgotten: restoring it
/// here would need this function to know what the caller's mode was, and a
/// planning turn is followed by the caller building a ledger and running each
/// step on a worker that sets its own mode per task. Silently putting a
/// permissive mode back would be the surprising behaviour, not this.
///
/// # Failure is a plan of one step, never an error
///
/// A planner that answers with prose, or with a block naming no role the
/// harness has, produces no steps — and the honest response to that is to do
/// the work as it was asked, on the session model, exactly as before any of
/// this existed. Returning an error would mean a planner having a bad turn
/// stops work that a person asked for.
pub fn split(
    backend: &mut dyn crate::agent::AgentBackend,
    roles: &crate::roles::RoleTable,
    instruction: &str,
) -> Vec<Step> {
    let Some(block) = prompt_block(roles) else {
        return Vec::new();
    };
    backend.set_permission_mode(PLAN_MODE);

    let prompt = format!("{block}\n\nThe work to split:\n{instruction}");
    let mut text_blocks = String::new();
    let mut deltas = String::new();
    let outcome = backend.run_turn(&prompt, &mut |event| match event {
        crate::agent::AgentEvent::Text(text) => text_blocks.push_str(text),
        crate::agent::AgentEvent::TextDelta(fragment) => deltas.push_str(fragment),
        _ => {}
    });
    if let Err(e) = &outcome {
        crate::trace!("plan", "planning turn failed: {e}");
        return Vec::new();
    }
    let said = if !text_blocks.is_empty() {
        text_blocks
    } else {
        deltas
    };
    let (steps, unknown) = parse_detailed(&said);
    crate::trace!(
        "plan",
        "{} step(s) from {} chars: {:?}{}",
        steps.len(),
        said.len(),
        steps.iter().map(|s| s.role.as_str()).collect::<Vec<_>>(),
        if unknown.is_empty() {
            String::new()
        } else {
            format!(" | named no known role: {unknown:?}")
        }
    );
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend that answers with a fixed reply and records how it was driven.
    struct Planner {
        reply: String,
        modes: Vec<String>,
        prompts: Vec<String>,
        fail: bool,
    }

    impl Planner {
        fn saying(reply: &str) -> Self {
            Self {
                reply: reply.to_string(),
                modes: Vec::new(),
                prompts: Vec::new(),
                fail: false,
            }
        }
    }

    impl crate::agent::AgentBackend for Planner {
        fn id(&self) -> &str {
            "planner"
        }
        fn session_id(&self) -> Option<&str> {
            Some("s-plan")
        }
        fn set_permission_mode(&mut self, mode: &str) {
            self.modes.push(mode.to_string());
        }
        fn run_turn(
            &mut self,
            prompt: &str,
            on_event: &mut dyn FnMut(&crate::agent::AgentEvent),
        ) -> Result<crate::agent::TurnOutcome, crate::agent::AgentError> {
            self.prompts.push(prompt.to_string());
            if self.fail {
                return Err(crate::agent::AgentError::new("the planner could not run"));
            }
            on_event(&crate::agent::AgentEvent::Text(self.reply.clone()));
            Ok(crate::agent::TurnOutcome {
                session_id: "s-plan".to_string(),
                stop_reason: "end_turn".to_string(),
                is_error: false,
                detail: None,
                served_by: vec!["a-model".to_string()],
            })
        }
    }

    fn routable() -> crate::roles::RoleTable {
        crate::roles::RoleTable::default()
            .with_candidates("explore", vec!["cheap".to_string()])
            .with_candidates("implement", vec!["mid".to_string()])
    }

    /// **THE RULE IS ENFORCED, NOT REQUESTED.** Before this, "do not write code"
    /// was a sentence in the prompt and nothing else — a model that talked
    /// itself past it would read the files on the expensive model, which is the
    /// whole cost the split exists to avoid.
    ///
    /// `plan` is a real choice of the installed CLI's `--permission-mode`, so
    /// this asserts the backend was actually PUT in it.
    #[test]
    fn a_planning_turn_runs_in_plan_mode() {
        let mut backend = Planner::saying("<plan>\nexplore: find it\n</plan>");
        let steps = split(&mut backend, &routable(), "fix the box");

        assert_eq!(
            backend.modes,
            vec![PLAN_MODE.to_string()],
            "the planner must be put in plan mode before it is asked anything"
        );
        assert_eq!(steps.len(), 1, "and it still returns the plan: {steps:?}");
    }

    /// The instruction and the format both have to reach the planner, or it is
    /// splitting something it cannot see, or answering in a shape nobody reads.
    #[test]
    fn the_planner_is_given_the_work_and_the_format() {
        let mut backend = Planner::saying("<plan>\nexplore: find it\n</plan>");
        split(&mut backend, &routable(), "fix the vanishing box");

        let prompt = &backend.prompts[0];
        assert!(prompt.contains("fix the vanishing box"), "{prompt}");
        assert!(
            prompt.contains(OPEN),
            "the format must ride along: {prompt}"
        );
        assert!(prompt.contains("explore"), "and the roles: {prompt}");
    }

    /// **A planner having a bad turn must not stop work a person asked for.**
    /// No steps means do the job as it came, on the session model — which is
    /// exactly what happened before any of this existed.
    #[test]
    fn a_planner_that_answers_with_prose_yields_no_steps_rather_than_an_error() {
        let mut backend = Planner::saying("I think we should start by reading footer.rs.");
        assert!(split(&mut backend, &routable(), "fix it").is_empty());
    }

    /// A backend that fails is the same case: no plan, not a lost instruction.
    #[test]
    fn a_failed_planning_turn_yields_no_steps() {
        let mut backend = Planner::saying("<plan>\nexplore: find it\n</plan>");
        backend.fail = true;
        assert!(split(&mut backend, &routable(), "fix it").is_empty());
    }

    /// With nothing to route to there is nothing to split, and the turn is not
    /// spent at all — not even to be told the answer is unusable.
    #[test]
    fn no_routable_roles_means_the_planning_turn_never_runs() {
        let mut backend = Planner::saying("<plan>\nexplore: find it\n</plan>");
        let steps = split(&mut backend, &crate::roles::RoleTable::default(), "fix it");

        assert!(steps.is_empty());
        assert!(
            backend.prompts.is_empty(),
            "a turn was spent on a split nobody could act on"
        );
        assert!(backend.modes.is_empty(), "and the mode was changed for it");
    }

    #[test]
    fn a_plan_becomes_role_tagged_steps() {
        let steps = parse(
            "Here is how I would do it.\n\
             <plan>\n\
             explore: find every file that mentions the status row\n\
             diagnose: work out which writer clears it\n\
             implement: fix it and add a test\n\
             </plan>\n\
             Shall I start?",
        );
        assert_eq!(steps.len(), 3, "got {steps:?}");
        assert_eq!(steps[0].role, "explore");
        assert_eq!(
            steps[0].instruction,
            "find every file that mentions the status row"
        );
        assert_eq!(steps[2].role, "implement");
    }

    /// **Prose is not work.** A model writes a sentence around a block as a
    /// matter of course, and spending a turn asking a model to do a sentence is
    /// the cheapest possible way to waste the saving this exists to make.
    #[test]
    fn prose_around_and_inside_the_block_is_not_a_step() {
        let steps = parse(
            "<plan>\n\
             This is what I suggest.\n\
             explore: find the parser\n\
             Note: the box is drawn in footer.rs\n\
             </plan>",
        );
        assert_eq!(steps.len(), 1, "got {steps:?}");
        assert_eq!(steps[0].role, "explore");
    }

    /// A model writing a list writes dashes. Refusing them would make the
    /// format harder to obey than to ignore.
    #[test]
    fn a_bulleted_plan_is_read_the_same_way() {
        let steps = parse("<plan>\n- explore: find it\n* grunt: rename it\n</plan>");
        assert_eq!(steps.len(), 2, "got {steps:?}");
        assert_eq!(steps[0].role, "explore");
        assert_eq!(steps[1].role, "grunt");
    }

    /// **A swallowed plan is worse than one read slightly wrong** — the same
    /// rule `ask::parse` follows for the same reason.
    #[test]
    fn an_unclosed_block_is_still_read() {
        let steps = parse("<plan>\nexplore: find it\nimplement: fix it");
        assert_eq!(steps.len(), 2, "got {steps:?}");
    }

    /// An unbounded plan is an unbounded run: every step is a real turn against
    /// a real model.
    #[test]
    fn a_plan_is_bounded() {
        let body: String = (0..MAX_STEPS + 5)
            .map(|n| format!("grunt: step {n}\n"))
            .collect();
        let steps = parse(&format!("{OPEN}\n{body}{CLOSE}"));
        assert_eq!(steps.len(), MAX_STEPS);
    }

    /// **A mistyped role is REPORTED, never dropped in silence.** This is the
    /// cost of matching against the known roles, and it is paid here: a step
    /// that vanished without a sound would be work nobody knows was skipped.
    #[test]
    fn a_role_nobody_recognises_is_reported() {
        let (steps, unknown) = parse_detailed("<plan>\nchepa: tidy the imports\n</plan>");
        assert!(steps.is_empty(), "got {steps:?}");
        assert_eq!(
            unknown,
            vec!["chepa".to_string()],
            "the operator has to be able to see what was skipped and why"
        );
    }

    /// The same mechanism keeps ordinary prose out, and prose is NOT reported —
    /// a model writing "Note:" has made no mistake, and a warning about it every
    /// turn would train the operator to ignore the list that matters.
    #[test]
    fn a_prose_prefix_is_skipped_without_being_reported_as_a_typo() {
        let (steps, unknown) = parse_detailed(
            "<plan>\nexplore: find it\nNote: the box is drawn in footer.rs\n</plan>",
        );
        assert_eq!(steps.len(), 1, "got {steps:?}");
        assert_eq!(
            unknown,
            vec!["note".to_string()],
            "it is indistinguishable from a typo, so it is listed rather than guessed about"
        );
    }

    /// Every role the parser accepts must be one the router can act on, or a
    /// step routes to nothing and runs on the session model unnoticed.
    #[test]
    fn every_accepted_role_is_one_the_ledger_can_route() {
        for role in crate::roles::KNOWN_ROLES {
            let steps = parse(&format!("{OPEN}\n{role}: do the thing\n{CLOSE}"));
            assert_eq!(steps.len(), 1, "`{role}` was not accepted");
            assert_eq!(
                crate::ledger::Task::new("t", "r", "x")
                    .with_tier(&steps[0].role)
                    .role(),
                Some(role),
                "`{role}` parsed but the ledger could not route it"
            );
        }
    }

    /// Case is not meaning — a planner writing `Explore` means `explore`.
    #[test]
    fn a_role_is_matched_regardless_of_case() {
        let steps = parse("<plan>\nEXPLORE: find it\n</plan>");
        assert_eq!(steps[0].role, "explore");
    }

    /// No block at all is no plan, not an empty one to act on.
    #[test]
    fn text_with_no_block_yields_no_steps() {
        assert!(parse("I'll just get on with it.").is_empty());
    }

    /// **The planner is told not to do the work.** Without that line the
    /// expensive model reads the files itself and the split has cost more than
    /// it saved, while looking busy.
    /// **THE HARNESS MUST NOT ASK FOR WHAT ITS OWN GUARD REFUSES.**
    ///
    /// The prompt used to teach `implement: fix it and add a test that fails
    /// without the fix`, while [`crate::guards::DEFAULT_PROTECTED`] protects
    /// `tests/`, `test/`, `_test.`, `.test.` and `spec/`. So the harness
    /// instructed the agent to write a test and then failed the attempt for
    /// doing it — observed live on `b1.1`, abandoned after both attempts, and
    /// the agent worked around it by writing `check-rest.js` and
    /// `jump-behavior-check.js`, names no test-path rule matches.
    ///
    /// Put "and add a test that fails without the fix" back in the example and
    /// the first assertion fails.
    #[test]
    fn the_prompt_never_teaches_a_step_the_guard_will_refuse() {
        let mut roles = crate::roles::RoleTable::default();
        roles = roles.with_candidates("implement", vec!["mid".to_string()]);
        let block = prompt_block(&roles).expect("a table with a role produces a block");

        // The EXAMPLE only — the prose below it must be free to say the word
        // "test" in order to explain the constraint, and a blunter assertion
        // caught that explanation instead of the thing that regressed.
        let open = block.find(OPEN).expect("the block shows the format") + OPEN.len();
        let close = block.find(CLOSE).expect("and closes it");
        let example = &block[open..close];

        assert!(
            !example.to_ascii_lowercase().contains("test"),
            "the worked example must not model a protected write — the planner \
             copies its shape: {example}"
        );
        assert!(
            block.contains("until the owner allows it"),
            "and the planner must be TOLD the constraint, not left to discover it \
             by having an attempt fail: {block}"
        );
    }

    #[test]
    fn the_planner_is_told_to_plan_and_not_to_start() {
        let mut roles = crate::roles::RoleTable::default();
        roles = roles.with_candidates("explore", vec!["cheap".to_string()]);
        let block = prompt_block(&roles).expect("a table with a role produces a block");

        assert!(block.contains("do not write code"), "{block}");
        assert!(block.contains(OPEN), "it must show the format: {block}");
        assert!(
            block.contains("explore"),
            "and offer the roles it may use: {block}"
        );
        assert!(
            block.contains("never by a model"),
            "the completion law rides with it: {block}"
        );
    }

    /// With nothing to route to, a planning turn buys nothing and costs a turn.
    #[test]
    fn no_configured_roles_means_no_planning_turn() {
        assert_eq!(prompt_block(&crate::roles::RoleTable::default()), None);
    }

    #[test]
    fn numbered_lists_are_parsed_as_steps() {
        let steps = parse(
            "<plan>\n\
             1. explore: find every file\n\
             2) diagnose: locate the fault\n\
             (3) implement: fix the bug\n\
             [4] grunt: clean up imports\n\
             </plan>",
        );
        assert_eq!(steps.len(), 4, "got {steps:?}");
        assert_eq!(steps[0].role, "explore");
        assert_eq!(steps[0].instruction, "find every file");
        assert_eq!(steps[1].role, "diagnose");
        assert_eq!(steps[1].instruction, "locate the fault");
        assert_eq!(steps[2].role, "implement");
        assert_eq!(steps[2].instruction, "fix the bug");
        assert_eq!(steps[3].role, "grunt");
        assert_eq!(steps[3].instruction, "clean up imports");
    }

    #[test]
    fn bullets_with_plus_and_combinations_are_parsed() {
        let steps = parse(
            "<plan>\n\
             + explore: find files\n\
             - 1. diagnose: analyze logs\n\
             * 2. implement: write code\n\
             </plan>",
        );
        assert_eq!(steps.len(), 3, "got {steps:?}");
        assert_eq!(steps[0].role, "explore");
        assert_eq!(steps[1].role, "diagnose");
        assert_eq!(steps[2].role, "implement");
    }

    #[test]
    fn step_and_phase_and_task_prefixes_are_parsed() {
        let steps = parse(
            "<plan>\n\
             Step 1: explore: find files\n\
             Step 2. diagnose: check the stacktrace\n\
             Task 3: implement: fix the null pointer\n\
             Phase 4 - review: verify security invariants\n\
             </plan>",
        );
        assert_eq!(steps.len(), 4, "got {steps:?}");
        assert_eq!(steps[0].role, "explore");
        assert_eq!(steps[0].instruction, "find files");
        assert_eq!(steps[1].role, "diagnose");
        assert_eq!(steps[1].instruction, "check the stacktrace");
        assert_eq!(steps[2].role, "implement");
        assert_eq!(steps[2].instruction, "fix the null pointer");
        assert_eq!(steps[3].role, "review");
        assert_eq!(steps[3].instruction, "verify security invariants");
    }

    #[test]
    fn formatted_roles_are_cleaned_and_matched() {
        let steps = parse(
            "<plan>\n\
             - **explore**: find endpoints\n\
             - `diagnose`: check latency\n\
             - *implement*: apply database index\n\
             - _grunt_: format code\n\
             </plan>",
        );
        assert_eq!(steps.len(), 4, "got {steps:?}");
        assert_eq!(steps[0].role, "explore");
        assert_eq!(steps[1].role, "diagnose");
        assert_eq!(steps[2].role, "implement");
        assert_eq!(steps[3].role, "grunt");
    }

    #[test]
    fn roles_with_parenthetical_tier_notes_are_matched() {
        let steps = parse(
            "<plan>\n\
             explore (fast retrieval): list schema migrations\n\
             diagnose [deep reasoning]: verify lock order\n\
             implement (Sonnet 3.5): update query\n\
             </plan>",
        );
        assert_eq!(steps.len(), 3, "got {steps:?}");
        assert_eq!(steps[0].role, "explore");
        assert_eq!(steps[0].instruction, "list schema migrations");
        assert_eq!(steps[1].role, "diagnose");
        assert_eq!(steps[1].instruction, "verify lock order");
        assert_eq!(steps[2].role, "implement");
        assert_eq!(steps[2].instruction, "update query");
    }

    #[test]
    fn case_insensitive_plan_tags_are_parsed() {
        let steps = parse(
            "<Plan>\n\
             explore: find files\n\
             implement: make change\n\
             </Plan>",
        );
        assert_eq!(steps.len(), 2, "got {steps:?}");
        assert_eq!(steps[0].role, "explore");
        assert_eq!(steps[1].role, "implement");

        let steps_upper = parse(
            "<PLAN>\n\
             diagnose: analyze root cause\n\
             </PLAN>",
        );
        assert_eq!(steps_upper.len(), 1, "got {steps_upper:?}");
        assert_eq!(steps_upper[0].role, "diagnose");
    }

    #[test]
    fn double_colons_and_extra_whitespace_are_handled() {
        let steps = parse(
            "<plan>\n\
             explore:: find all controllers\n\
             implement : fix the response format\n\
             </plan>",
        );
        assert_eq!(steps.len(), 2, "got {steps:?}");
        assert_eq!(steps[0].role, "explore");
        assert_eq!(steps[0].instruction, "find all controllers");
        assert_eq!(steps[1].role, "implement");
        assert_eq!(steps[1].instruction, "fix the response format");
    }

    #[test]
    fn streaming_delta_and_text_are_not_duplicated_in_split() {
        struct StreamingAndBlockBackend {
            reply: String,
        }
        impl crate::agent::AgentBackend for StreamingAndBlockBackend {
            fn id(&self) -> &str {
                "streaming-backend"
            }
            fn session_id(&self) -> Option<&str> {
                Some("s-streaming")
            }
            fn set_permission_mode(&mut self, _mode: &str) {}
            fn run_turn(
                &mut self,
                _prompt: &str,
                on_event: &mut dyn FnMut(&crate::agent::AgentEvent),
            ) -> Result<crate::agent::TurnOutcome, crate::agent::AgentError> {
                // Emit streaming deltas first:
                for chunk in self.reply.as_bytes().chunks(10) {
                    let s = std::str::from_utf8(chunk).unwrap();
                    on_event(&crate::agent::AgentEvent::TextDelta(s.to_string()));
                }
                // Then emit the full assembled block at turn completion:
                on_event(&crate::agent::AgentEvent::Text(self.reply.clone()));
                Ok(crate::agent::TurnOutcome {
                    session_id: "s-streaming".to_string(),
                    stop_reason: "end_turn".to_string(),
                    is_error: false,
                    detail: None,
                    served_by: vec!["model".to_string()],
                })
            }
        }

        let mut backend = StreamingAndBlockBackend {
            reply: "<plan>\nexplore: find it\nimplement: fix it\n</plan>".to_string(),
        };
        let steps = split(&mut backend, &routable(), "fix it");
        assert_eq!(
            steps.len(),
            2,
            "steps must not be duplicated when backend emits deltas and text: {steps:?}"
        );
        assert_eq!(steps[0].role, "explore");
        assert_eq!(steps[1].role, "implement");
    }

    /// **A DECLARED ORDERING IS READ, AND LEAVES THE INSTRUCTION.**
    ///
    /// The marker is an instruction to the HARNESS. Left in the text it would
    /// be handed to the agent as part of the work, which would have a model
    /// trying to interpret `[after 1,2]` as something to do.
    #[test]
    fn a_step_can_say_what_it_waits_for() {
        let steps = parse(
            "<plan>\n\
             implement: add the login page\n\
             implement: add the signup page\n\
             implement: share the validation [after 1,2]\n\
             </plan>",
        );

        assert_eq!(steps.len(), 3);
        assert!(steps[0].after.is_empty(), "no marker means independent");
        assert!(steps[1].after.is_empty());
        assert_eq!(steps[2].after, vec![1, 2]);
        assert_eq!(
            steps[2].instruction, "share the validation",
            "the marker was left in the work the agent is given"
        );
    }

    /// Punctuation a planner gets slightly wrong must not silently become prose.
    #[test]
    fn the_ordering_marker_is_read_generously() {
        for line in [
            "implement: do it [after 1]",
            "implement: do it [after 1,2]",
            "implement: do it [after 1, 2]",
            "implement: do it [AFTER 1 2]",
        ] {
            let steps = parse(&format!("<plan>\n{line}\n</plan>"));
            assert_eq!(steps.len(), 1, "on: {line}");
            assert!(!steps[0].after.is_empty(), "ordering was lost on: {line}");
            assert_eq!(steps[0].instruction, "do it", "on: {line}");
        }
    }

    /// **A reply with an em dash near a tag parses instead of panicking.**
    /// The tag search sliced the text a tag's length on from each character,
    /// and a multi-byte character across that end took down the planning
    /// turn. Models write em dashes constantly, before and after the block.
    #[test]
    fn multi_byte_text_around_the_block_does_not_panic() {
        let reply = "Plan — here:\n<plan>\nimplement: add it — carefully\n</plan>\nDone — é.";
        let steps = parse(reply);
        assert_eq!(steps.len(), 1, "{steps:?}");
        assert_eq!(steps[0].instruction, "add it — carefully");
        assert!(parse("— — — no block at all — —").is_empty());
    }

    /// **Tests first, as their own step, and the change waits for it.** The
    /// example the planner copies is read with the real parser: two steps, the
    /// first writes the test, the second says `[after 1]`. The test is written
    /// pending, because the step is verified by the project's check like any
    /// other and a red test would fail it; the change turns it on.
    #[test]
    fn the_prompt_teaches_tests_first_as_their_own_step() {
        let roles = crate::roles::RoleTable::default().with_role("implement", "some-model");
        let block = prompt_block(&roles).expect("a planner block");
        let at = block.find("TESTS FIRST").expect("the rule is stated");
        let steps = parse(&block[at..]);

        assert_eq!(steps.len(), 2, "{steps:?}");
        assert!(steps[0].instruction.contains("test"), "{steps:?}");
        assert!(steps[0].instruction.contains("pending"), "{steps:?}");
        assert!(steps[0].after.is_empty(), "the test waits for nothing");
        assert_eq!(steps[1].after, [1], "the change waits for the test");
        assert!(
            steps[1].instruction.contains("turn that test on"),
            "{steps:?}"
        );
        for step in &steps {
            assert!(
                !step.instruction.contains("and add a test"),
                "a test folded into the change is the shape this replaces: {step:?}"
            );
        }
        assert!(block.contains("until the owner allows it"), "{block}");
    }

    /// The three ways a plan is cut, each stated where the planner reads it.
    #[test]
    fn the_prompt_teaches_slices_prefactoring_and_expand_migrate_contract() {
        let roles = crate::roles::RoleTable::default().with_role("implement", "some-model");
        let block = prompt_block(&roles).expect("a planner block");
        for rule in [
            "THIN VERTICAL SLICE",
            "PREFACTOR FIRST",
            "EXPAND",
            "MIGRATE the callers in batches",
            "CONTRACT",
        ] {
            assert!(block.contains(rule), "`{rule}` is not taught:\n{block}");
        }
        assert!(
            block.contains(&format!("At most {MAX_STEPS} steps")),
            "the cap still rides with it"
        );
    }

    /// The planner is told the syntax, or no model will ever emit it.
    #[test]
    fn the_prompt_teaches_the_ordering_marker() {
        let roles = crate::roles::RoleTable::default().with_role("implement", "some-model");
        let block = prompt_block(&roles).expect("a planner block");
        assert!(block.contains("[after"), "got: {block}");
        assert!(
            block.contains("AT THE SAME TIME"),
            "a planner that is not told what NOT marking means will mark everything: {block}"
        );
    }
}
