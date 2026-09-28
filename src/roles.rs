//! Which model does which kind of work.
//!
//! Delegation already saved *context* — a subagent's exploration never enters
//! the parent's conversation. It saved no *money*, because the child borrowed
//! the parent's client and therefore ran on the parent's model. Reading a
//! directory to find where something lives does not need the model that decides
//! whether the design is right.
//!
//! A role names a kind of work. The operator decides which model serves it, so
//! nothing here knows or names a vendor: a fleet is a deployment choice, and a
//! harness that hardcoded one would have to be edited to change it.
//!
//! # The rule this module does not get to bend
//!
//! Routing decides *who does the work*. It decides nothing about whether the
//! work is finished. A cheap model's output and an expensive model's output are
//! both claims, and a claim is verified by a command that exits zero — never by
//! another model agreeing with it.

use std::collections::BTreeMap;

/// Prefix for the environment variables that assign models to a role.
///
/// `INCODE_ROLE_EXPLORE=first,second,third` assigns the explore role an ORDERED
/// list. An env var per role rather than a config file: there are few roles,
/// they change rarely, and an operator switching a fleet should not have to edit
/// JSON to do it.
pub const ROLE_ENV_PREFIX: &str = "INCODE_ROLE_";

/// How many models a role may list.
///
/// A ceiling rather than a target. Each candidate that fails costs a real
/// attempt before the next is tried, so an unbounded list turns one unlucky role
/// into a long silence.
pub const MAX_CANDIDATES: usize = 8;

/// The roles the harness knows how to ask for.
///
/// Deliberately few. Every extra role is another decision the orchestrator has
/// to make correctly, and a wrong choice costs more than the saving.
pub const KNOWN_ROLES: [&str; 7] = [
    "explore",
    "diagnose",
    "plan",
    "implement",
    "review",
    "frontend",
    "grunt",
];

/// What each role is for, shown to the orchestrator so it can choose.
#[must_use]
pub fn role_purpose(role: &str) -> Option<&'static str> {
    match role {
        "explore" => Some(
            "RETRIEVAL — finding where something is. Listing the files that \
             match, pulling the lines around a hit, fetching a definition by \
             name. High volume, no judgement, and a wrong answer shows up \
             immediately: the cheapest model that can follow instructions. \
             Reading a large file and reporting which branch decides a \
             behaviour is NOT this. That is a judgement about code and belongs \
             to `diagnose`, however much it looks like reading.",
        ),
        "diagnose" => Some(
            "working out WHY — which of several candidate causes the evidence \
             actually fits, which branch decides a behaviour, what a call path \
             does across every site that shares it. Low volume, and the most \
             expensive job here to get wrong: a wrong cause sends every hour \
             after it in the wrong direction, so the expensive model pays for \
             itself even though the work looks like reading.",
        ),
        "implement" => Some(
            "writing and changing code against a clear specification, where the \
             work is checkable by running something. A fix in code nobody has \
             read yet is NOT this — it carries the risk of `diagnose` and wants \
             that role first, or the one used for review.",
        ),
        "review" => Some(
            "judging a design, weighing a trade-off, or deciding whether an \
             approach is sound. The expensive model earns its cost here, and it \
             should not be from the same family as whoever wrote the code.",
        ),
        "plan" => Some(
            "deciding what to build and in what order. Low call volume, high \
             leverage: a wrong plan is paid for by every task after it.",
        ),
        "frontend" => Some(
            "building interfaces, judged by how they look and behave rather \
             than by a test passing — a different skill from fixing a backend \
             bug, and often a different model.",
        ),
        "grunt" => Some(
            "mechanical edits with no decision in them: a rename applied across \
             call sites already known, a file moved, a list sorted, items sent \
             to the right bucket. The test is whether a wrong answer is obvious \
             the moment it lands. Anything that has to be REASONED about — code \
             that ships included — is not this, however small it looks.",
        ),
        _ => None,
    }
}

/// Whether work in this role ends in a change a check can judge.
///
/// # Why this distinction had to be made
///
/// A live run planned an item into six steps and marked FOUR of them `Done` —
/// including `explore: locate the ball entity` and `plan: specify the falling
/// behaviour` — because each was judged by the project's own check,
/// `node --check game.js`, which passed for reasons that had nothing to do with
/// the step. Only the `implement` step had earned it. A check that says yes
/// whatever the step did is not a check.
///
/// The honest position is that `explore`, `diagnose`, `plan` and `review`
/// produce INFORMATION, and information is not provable by a build. This
/// function is the line between the two, and every task judged by the
/// project's own command is on the code side of it.
///
/// This does not weaken the completion law; it stops pretending the law was
/// satisfied.
///
/// # What is no longer true
///
/// All four used to be FOLDED — turned into prose inside the change they
/// served, with no task of their own. Three of them are tasks again, judged
/// by a deliverable instead of by a build; see [`produces_findings`], which
/// says what that bought and what it cost. Only `plan` still folds.
#[must_use]
pub fn changes_code(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "implement" | "frontend" | "grunt"
    )
}

/// Whether work in this role is worth running as a task of its own, judged by
/// the artifact it writes rather than by a build.
///
/// # Why the fold was not the end of the story
///
/// [`changes_code`] is still right: a build cannot judge information, and
/// marking `explore` done because `node --check` passed was the bug. But
/// folding those steps into the prose of the change they served had a cost
/// nobody priced at the time — a folded step is not a task, so it is never
/// ROUTED. The cheap model this module exists to send retrieval to never saw
/// any retrieval: it all ran on whichever model got the `implement` task.
///
/// A role listed here gets its task back, with a different judge. See
/// [`crate::findings`] for what that judge can and cannot prove.
///
/// # Why `plan` is not in this list
///
/// These steps come OUT of a plan. A `plan` step inside one is the planner
/// describing its own reasoning, and giving it a separate agent, a separate
/// turn and a separate deliverable would be paying a second time for the
/// decision that produced the list it appears in.
#[must_use]
pub fn produces_findings(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "explore" | "diagnose" | "review"
    )
}

/// The family a model belongs to, if the operator declared one.
///
/// Written as `model@family`. Nothing here knows any vendor — the tag is the
/// operator's word for "these share a lineage", which is the only thing the
/// harness needs and the only part it could not work out for itself.
#[must_use]
pub fn split_family(candidate: &str) -> (&str, Option<&str>) {
    match candidate.split_once('@') {
        Some((model, family)) if !family.trim().is_empty() => (model.trim(), Some(family.trim())),
        _ => (candidate.trim(), None),
    }
}

/// The model name with any family tag removed.
#[must_use]
pub fn model_name(candidate: &str) -> &str {
    split_family(candidate).0
}

/// Split an assignment into its ordered candidates.
///
/// Commas separate; whitespace around each is noise. Empty entries are dropped
/// rather than kept as a model named "", which a trailing comma would otherwise
/// produce.
#[must_use]
pub fn parse_candidates(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .take(MAX_CANDIDATES)
        .map(str::to_string)
        .collect()
}

/// Marks a candidate as a reference to a capability tier rather than a model.
///
/// `@top` means "whatever this operator considers a top-capability model", which
/// is the thing that actually stays true as models are replaced.
pub const TIER_PREFIX: char = '@';

/// Expand tier references in a candidate list against the operator's ladder.
///
/// # Why a ladder at all
///
/// Roles are about the KIND of work; tiers are about the CAPABILITY that work
/// needs. Without the second idea every role repeats the same model list, so
/// adding a model means editing seven rows and the day one is missed the fleet
/// is quietly inconsistent. With it, `plan`, `review` and `diagnose` all say
/// `@top` and a new top-capability model is added in exactly one place.
///
/// It is also what makes the ladder VENDOR-NEUTRAL. A tier is a list of things
/// that can do a job — `["opus", "some-agent:their-big-model", …]` — so the
/// order is a preference and the rest are fallbacks. A candidate naming an agent
/// that is not installed is skipped further down the line by
/// [`crate::fleet`], so one ladder works on every machine.
///
/// # The rules, chosen so this can never surprise anyone
///
/// - **One level.** A tier's own list must be literal models. A `@` inside a
///   tier is dropped and reported, which makes a cycle impossible by
///   construction rather than by a depth counter nobody tunes.
/// - **Order is preserved**, and a tier expands in place, so
///   `["@mid", "@top"]` means "try the mid models, then the top ones".
/// - **Duplicates collapse to their first appearance** — a model reachable
///   through two tiers must not consume two attempts.
/// - **An unknown reference is returned, not swallowed.** Dropping it silently
///   would leave a role with no candidates, which looks exactly like a role
///   nobody configured — and the operator would never learn their typo.
#[must_use]
pub fn expand_tiers(
    candidates: &[String],
    tiers: &BTreeMap<String, Vec<String>>,
) -> (Vec<String>, Vec<String>) {
    let mut out: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();

    let push = |model: &str, out: &mut Vec<String>| {
        let model = model.trim();
        if model.is_empty() || out.iter().any(|held| held == model) {
            return;
        }
        out.push(model.to_string());
    };

    for candidate in candidates {
        let candidate = candidate.trim();
        let Some(name) = candidate.strip_prefix(TIER_PREFIX) else {
            push(candidate, &mut out);
            continue;
        };
        let name = name.trim();
        match tiers.get(name) {
            Some(models) => {
                for model in models {
                    // A tier referring to another tier is the one shape this
                    // does not follow — see "One level" above.
                    if model.trim().starts_with(TIER_PREFIX) {
                        unknown.push(model.trim().to_string());
                        continue;
                    }
                    push(model, &mut out);
                }
            }
            None if !name.is_empty() => unknown.push(format!("{TIER_PREFIX}{name}")),
            // A bare `@` is a typo with no name to report; it is simply not a
            // candidate.
            None => {}
        }
    }

    out.truncate(MAX_CANDIDATES);
    (out, unknown)
}

/// Which models serve which role, in the order they should be tried.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleTable {
    assigned: BTreeMap<String, Vec<String>>,
}

impl RoleTable {
    /// Read role assignments from the environment.
    ///
    /// A role with no assignment is simply absent: the caller then runs the work
    /// on the model it already has, which is the behaviour before any of this
    /// existed. Nothing breaks by not configuring a fleet.
    #[must_use]
    pub fn from_env() -> Self {
        let mut assigned = BTreeMap::new();
        for role in KNOWN_ROLES {
            let key = format!("{ROLE_ENV_PREFIX}{}", role.to_ascii_uppercase());
            if let Ok(raw) = std::env::var(&key) {
                let candidates = parse_candidates(&raw);
                if !candidates.is_empty() {
                    assigned.insert(role.to_string(), candidates);
                }
            }
        }
        Self { assigned }
    }

    /// Assign one model to a role, replacing whatever was there.
    #[must_use]
    pub fn with_role(mut self, role: impl Into<String>, model: impl Into<String>) -> Self {
        self.assigned.insert(role.into(), vec![model.into()]);
        self
    }

    /// Assign an ordered list of models to a role.
    #[must_use]
    pub fn with_candidates(
        mut self,
        role: impl Into<String>,
        models: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let list: Vec<String> = models
            .into_iter()
            .map(Into::into)
            .filter(|m: &String| !m.trim().is_empty())
            .take(MAX_CANDIDATES)
            .collect();
        if list.is_empty() {
            self.assigned.remove(&role.into());
        } else {
            self.assigned.insert(role.into(), list);
        }
        self
    }

    /// The models for a role, in the order to try them.
    ///
    /// Empty for an unknown role rather than falling back silently: a typo
    /// should run on the model already in hand and be visible, not resolve to
    /// something that happens to be configured.
    #[must_use]
    pub fn candidates_for(&self, role: &str) -> &[String] {
        self.assigned.get(role).map_or(&[], Vec::as_slice)
    }

    /// The first model for a role, if any.
    #[must_use]
    pub fn model_for(&self, role: &str) -> Option<&str> {
        self.candidates_for(role).first().map(String::as_str)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.assigned.is_empty()
    }

    /// The roles that actually have a model behind them.
    #[must_use]
    pub fn configured_roles(&self) -> Vec<&str> {
        self.assigned.keys().map(String::as_str).collect()
    }

    /// Whether the reviewer shares a lineage with whoever writes the code.
    ///
    /// A model reviewing output from its own family agrees with itself for
    /// reasons that have nothing to do with correctness: shared training data
    /// means shared blind spots, so the review reads as independent and is not.
    /// This is the same failure as an automated approver in a judgement seat,
    /// one level up.
    ///
    /// Compares declared families when both are tagged, and falls back to the
    /// model name — an identical model is the worst case of the same problem.
    /// Returns `None` when there is nothing to warn about, including when the
    /// operator declared no families and the models simply differ.
    #[must_use]
    pub fn review_independence_warning(&self) -> Option<String> {
        let reviewer = self.candidates_for("review").first()?;
        let writer = self
            .candidates_for("implement")
            .first()
            .or_else(|| self.candidates_for("frontend").first())?;

        let (review_model, review_family) = split_family(reviewer);
        let (write_model, write_family) = split_family(writer);

        let same = match (review_family, write_family) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => review_model.eq_ignore_ascii_case(write_model),
        };
        if !same {
            return None;
        }
        Some(format!(
            "Review is served by `{review_model}`, the same lineage that writes \
             the code (`{write_model}`). A model reviewing its own family agrees \
             with itself for reasons unrelated to correctness. Assign a reviewer \
             from a different source, or tag families as `model@family` if these \
             differ."
        ))
    }

    /// A line for the operator, so what the fleet resolved to is visible at
    /// startup rather than inferred from a bill later.
    #[must_use]
    pub fn summary_line(&self) -> String {
        if self.assigned.is_empty() {
            return "Roles: none assigned — delegated work runs on the session model.".to_string();
        }
        let pairs: Vec<String> = self
            .assigned
            .iter()
            .map(|(role, models)| format!("{role}={}", models.join(" > ")))
            .collect();
        format!("Roles: {}", pairs.join(", "))
    }

    /// What to tell the orchestrator about the fleet it can call on.
    ///
    /// Returns `None` when nothing is assigned, so a session without a fleet
    /// carries no text about choosing between models it does not have.
    #[must_use]
    pub fn prompt_block(&self) -> Option<String> {
        if self.assigned.is_empty() {
            return None;
        }
        let mut out = String::from(
            "<delegation_roles>\nWhen delegating, name a `role` and the work runs on the \
             model assigned to it. Choose by what the work needs, not by what is cheapest \
             to type:",
        );
        for role in self.assigned.keys() {
            if let Some(purpose) = role_purpose(role) {
                out.push_str(&format!("\n  - {role}: {purpose}"));
            } else {
                out.push_str(&format!("\n  - {role}"));
            }
        }
        out.push_str(
            "\n\nA role decides WHO does the work. It decides nothing about whether the \
             work is done: that is settled by running the check, never by a model — \
             including you — judging its own or another's output.\n</delegation_roles>",
        );
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder() -> BTreeMap<String, Vec<String>> {
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "top".to_string(),
            vec![
                "opus".to_string(),
                "other-agent:their-big".to_string(),
                "third:pro".to_string(),
            ],
        );
        tiers.insert(
            "mid".to_string(),
            vec!["sonnet".to_string(), "other-agent:their-fast".to_string()],
        );
        tiers
    }

    /// **The point of the ladder: one place to add a model.** A role names a
    /// capability and gets every model the operator says can serve it, in
    /// order — across vendors, because a tier is not a vendor.
    #[test]
    fn a_tier_reference_expands_to_every_model_that_can_serve_it() {
        let (expanded, unknown) = expand_tiers(&["@top".to_string()], &ladder());
        assert_eq!(expanded, vec!["opus", "other-agent:their-big", "third:pro"]);
        assert!(unknown.is_empty(), "{unknown:?}");
    }

    /// Order is a preference and the rest are fallbacks, so a tier expands
    /// IN PLACE rather than being appended.
    #[test]
    fn tiers_expand_in_place_so_the_order_is_the_preference() {
        let (expanded, _) =
            expand_tiers(&["first-choice".to_string(), "@mid".to_string()], &ladder());
        assert_eq!(
            expanded,
            vec!["first-choice", "sonnet", "other-agent:their-fast"]
        );
    }

    /// A model reachable through two tiers must not consume two attempts —
    /// each failed candidate costs a real turn before the next is tried.
    #[test]
    fn a_model_in_two_tiers_is_tried_once() {
        let mut tiers = ladder();
        tiers.insert("also".to_string(), vec!["opus".to_string()]);
        let (expanded, _) = expand_tiers(&["@top".to_string(), "@also".to_string()], &tiers);
        assert_eq!(
            expanded.iter().filter(|m| *m == "opus").count(),
            1,
            "got {expanded:?}"
        );
    }

    /// **A typo must not read as "this role was never configured".** Dropping
    /// an unknown tier in silence leaves the role empty, which looks identical
    /// to one nobody set up — and the operator never learns why their expensive
    /// work ran on the session model.
    #[test]
    fn an_unknown_tier_is_reported_rather_than_swallowed() {
        let (expanded, unknown) =
            expand_tiers(&["@tpo".to_string(), "sonnet".to_string()], &ladder());
        assert_eq!(expanded, vec!["sonnet"], "the good candidate still stands");
        assert_eq!(unknown, vec!["@tpo"], "the typo must come back to be shown");
    }

    /// **A cycle is impossible by construction, not by a depth limit.** A tier
    /// list must be literal models; a reference inside one is refused and
    /// reported rather than followed.
    #[test]
    fn a_tier_naming_another_tier_cannot_loop() {
        let mut tiers = BTreeMap::new();
        tiers.insert("a".to_string(), vec!["@b".to_string(), "real".to_string()]);
        tiers.insert("b".to_string(), vec!["@a".to_string()]);
        let (expanded, unknown) = expand_tiers(&["@a".to_string()], &tiers);
        assert_eq!(expanded, vec!["real"], "it must still take the real model");
        assert_eq!(unknown, vec!["@b"], "and say which reference it refused");
    }

    /// A list with no references is returned as it was — every existing
    /// configuration keeps working untouched.
    #[test]
    fn a_list_of_plain_models_is_unchanged() {
        let (expanded, unknown) = expand_tiers(
            &["haiku".to_string(), "sonnet".to_string()],
            &BTreeMap::new(),
        );
        assert_eq!(expanded, vec!["haiku", "sonnet"]);
        assert!(unknown.is_empty());
    }

    /// The ceiling still holds after expansion: each failed candidate costs a
    /// real attempt, so an unbounded ladder is a long silence.
    #[test]
    fn expansion_is_still_bounded_by_the_candidate_ceiling() {
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "everything".to_string(),
            (0..MAX_CANDIDATES + 6)
                .map(|n| format!("model-{n}"))
                .collect(),
        );
        let (expanded, _) = expand_tiers(&["@everything".to_string()], &tiers);
        assert_eq!(expanded.len(), MAX_CANDIDATES);
    }

    #[test]
    fn a_role_with_no_assignment_is_absent_rather_than_guessed() {
        let table = RoleTable::default();
        assert_eq!(table.model_for("explore"), None);
        assert!(table.is_empty());
        assert!(
            table.prompt_block().is_none(),
            "a session with no fleet must carry no text about choosing one"
        );
    }

    /// A typo must not silently resolve to whatever happens to be configured —
    /// that would run expensive work on a cheap model without anyone noticing.
    #[test]
    fn an_unknown_role_does_not_fall_back_to_a_configured_one() {
        let table = RoleTable::default().with_role("explore", "some-small-model");
        assert_eq!(table.model_for("explore"), Some("some-small-model"));
        assert_eq!(table.model_for("explor"), None);
        assert_eq!(table.model_for(""), None);
    }

    #[test]
    fn assignments_are_read_from_the_environment() {
        let key = format!("{ROLE_ENV_PREFIX}EXPLORE");
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(&key, "a-configured-model");
        let table = RoleTable::from_env();
        std::env::remove_var(&key);

        assert_eq!(table.model_for("explore"), Some("a-configured-model"));
    }

    /// An operator who sets a variable to nothing has assigned nothing. Treating
    /// an empty string as a model name would produce a request for a model
    /// called "".
    #[test]
    fn a_blank_assignment_is_not_an_assignment() {
        let key = format!("{ROLE_ENV_PREFIX}REVIEW");
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(&key, "   ");
        let table = RoleTable::from_env();
        std::env::remove_var(&key);

        assert!(table.model_for("review").is_none());
    }

    #[test]
    fn the_summary_says_plainly_when_no_fleet_is_configured() {
        assert!(RoleTable::default()
            .summary_line()
            .contains("none assigned"));
        let table = RoleTable::default().with_role("review", "a-large-model");
        assert!(table.summary_line().contains("review=a-large-model"));
    }

    /// The block the orchestrator reads must carry the law with it. Routing text
    /// that only explained how to pick a cheap model would invite exactly the
    /// failure this design is built to avoid.
    #[test]
    fn the_prompt_block_states_that_a_role_never_settles_whether_work_is_done() {
        let table = RoleTable::default().with_role("explore", "m");
        let block = table
            .prompt_block()
            .expect("a configured fleet has a block");
        assert!(block.contains("explore"));
        assert!(
            block.contains("decides nothing about whether the work is done"),
            "got: {block}"
        );
        assert!(block.contains("running the check"), "got: {block}");
    }

    /// Every role offered must explain itself, or the orchestrator is choosing
    /// between names.
    #[test]
    fn every_known_role_states_what_it_is_for() {
        for role in KNOWN_ROLES {
            assert!(
                role_purpose(role).is_some(),
                "role `{role}` is offered with no purpose"
            );
        }
        assert_eq!(role_purpose("not-a-role"), None);
    }

    /// The rule the owner said they would break last: a reviewer from the same
    /// lineage as the writer agrees with itself for reasons unrelated to
    /// correctness.
    #[test]
    fn a_reviewer_from_the_writers_own_family_is_flagged() {
        let same = RoleTable::default()
            .with_role("implement", "writer-model@lab-a")
            .with_role("review", "reviewer-model@lab-a");
        let warning = same
            .review_independence_warning()
            .expect("same family must be flagged");
        assert!(warning.contains("same lineage"), "got: {warning}");
        assert!(warning.contains("reviewer-model"), "got: {warning}");

        let different = RoleTable::default()
            .with_role("implement", "writer-model@lab-a")
            .with_role("review", "reviewer-model@lab-b");
        assert_eq!(
            different.review_independence_warning(),
            None,
            "different families are the point of the rule, not a problem"
        );
    }

    /// With no family declared, an identical model is the worst case of the
    /// same problem and must still be caught.
    #[test]
    fn the_same_model_reviewing_itself_is_flagged_without_any_family_tag() {
        let table = RoleTable::default()
            .with_role("implement", "one-model")
            .with_role("review", "one-model");
        assert!(table.review_independence_warning().is_some());

        let differing = RoleTable::default()
            .with_role("implement", "one-model")
            .with_role("review", "another-model");
        assert_eq!(differing.review_independence_warning(), None);
    }

    /// The frontend role writes code too, so a reviewer sharing ITS lineage is
    /// the same defect.
    #[test]
    fn the_frontend_writer_is_also_compared_against_the_reviewer() {
        let table = RoleTable::default()
            .with_role("frontend", "ui-model@lab-c")
            .with_role("review", "judge-model@lab-c");
        assert!(
            table.review_independence_warning().is_some(),
            "whoever wrote the code is the comparison, not just `implement`"
        );
    }

    /// A family tag is a note for the harness, never part of the model name —
    /// passing it to a provider would ask for a model that does not exist.
    #[test]
    fn the_family_tag_is_stripped_from_the_model_name() {
        assert_eq!(model_name("some-model@lab-a"), "some-model");
        assert_eq!(model_name("  spaced-model @ lab-b "), "spaced-model");
        assert_eq!(model_name("no-tag-model"), "no-tag-model");
        assert_eq!(split_family("m@").1, None, "an empty tag is no tag");
    }

    /// Every role offered must explain itself, including the new ones.
    ///
    /// This used to assert `KNOWN_ROLES.len() == 6` — a count, under a doc
    /// comment promising something else entirely, and carrying the name of the
    /// vendor test below it. A count breaks the moment a role is added and
    /// says nothing about the roles that exist; what actually matters is that
    /// the orchestrator can read what each one is FOR, because a role with no
    /// purpose is one it will never route to on purpose.
    #[test]
    fn every_role_explains_what_it_is_for() {
        for role in KNOWN_ROLES {
            let purpose =
                role_purpose(role).unwrap_or_else(|| panic!("`{role}` is offered with no purpose"));
            assert!(
                purpose.len() > 40,
                "`{role}` needs a purpose an orchestrator can choose on: {purpose}"
            );
        }
        // The control: a role that does not exist has no purpose, so the loop
        // above is checking membership rather than passing on everything.
        assert_eq!(role_purpose("not-a-role"), None);
    }

    /// No vendor may appear in this crate's source: which model serves a role is
    /// the operator's decision, and a hardcoded one would have to be edited out.
    #[test]
    fn no_vendor_or_model_name_is_baked_into_the_source() {
        let source = include_str!("roles.rs");
        // The test's own assertion list would otherwise match itself, so check
        // the part of the file above the test module.
        let production = source
            .split("mod tests")
            .next()
            .expect("the file has a production half");
        for vendor in [
            "gemini", "grok", "gpt-", "claude-", "llama", "mistral", "glm-",
        ] {
            assert!(
                !production.to_ascii_lowercase().contains(vendor),
                "`{vendor}` is named in source; a fleet is configuration, not code"
            );
        }
    }
}
