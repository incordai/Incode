//! Whether a ROADMAP ITEM is finished — not whether its tasks ran.
//!
//! # Wired to
//!
//! - **Called by** [`crate::runner`] when the queue drains, and by
//!   `crate::cli` to report what a run actually achieved.
//! - **Calls** [`crate::verify::verify`], which is the only thing in this crate
//!   that can mint a [`crate::verify::Verified`].
//! - **Sibling to** [`crate::verify`], which answers the same question one level
//!   down, for a single task. The two must never be collapsed: a task's check
//!   proves a change compiles, a goal's check proves the thing the owner asked
//!   for exists.
//! - **Reads** `docs/VERIFICATION.md`, the file [`crate::spec`] already requires.
//!
//! # The hole this closes
//!
//! [`crate::ledger::Task`] carries a `roadmap_id`, and **nothing ever read it**
//! — it was written by the constructor and touched by one test, nowhere else. So
//! the runner's success case, `LedgerEmpty`, meant "the queue drained", and it
//! was being reported as if it meant "the work is done". Those are different
//! facts. A run whose every task was verified can still leave the roadmap item
//! unmet, because the tasks were a PLAN and a plan can be wrong.
//!
//! Observed: a run finished 7 tasks against a ball-game roadmap, one task
//! abandoned, and nothing anywhere asked whether a ball game now existed.
//!
//! # The law is the same law
//!
//! A goal is reached when a command exits zero. Not when the tasks are done, not
//! when a model says so, not when the queue is empty. This module cannot mint
//! proof of its own — it asks [`crate::verify::verify`] like everything else, so
//! there is exactly one definition of "proven" in the crate.
//!
//! # Where a goal's check comes from
//!
//! `docs/VERIFICATION.md`, which already lists the roadmap items — as prose the
//! owner reads. An item that additionally names a command in backticks gets a
//! check a machine can run:
//!
//! ```text
//! - **b1 falls and lands** — `node check-rest.js`
//! ```
//!
//! Prose-only items are left alone and reported as UNPROVEN rather than failed:
//! the owner wrote a sentence, not a promise, and treating it as a broken check
//! would make writing documentation hazardous.
//!
//! The item is matched by the leading token of the bullet (`b1`), which is the
//! `roadmap_id` the planner already stamps on every task.

use std::path::Path;

use crate::verify::Verified;

/// The document a goal's check is read from.
pub const VERIFICATION_PATH: &str = "docs/VERIFICATION.md";

/// One roadmap item's runnable check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalCheck {
    /// The roadmap item this proves, matching [`crate::ledger::Task::roadmap_id`].
    pub roadmap_id: String,
    /// What the item is called, for saying which goal was reached.
    pub title: String,
    /// The command, already split. Empty is impossible: a bullet with no
    /// backticked command produces no `GoalCheck` at all.
    pub argv: Vec<String>,
}

/// What a goal's check said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// A command exited zero. The only way a goal is reached.
    Reached { title: String, evidence: String },
    /// A command ran and did not pass.
    Failed { title: String, why: String },
    /// Nobody wrote a runnable check for this item.
    ///
    /// Deliberately NOT a failure. Most roadmap items in a real project are
    /// checked by a person, and reporting those as broken would train the owner
    /// to ignore this whole report.
    Unproven { title: String },
    /// The goal's own check passed, but the work planned for it did not.
    ///
    /// # Why this cannot be a tick
    ///
    /// Measured on a live run: a session that refused EVERY task — "No check is
    /// configured, so no planned step can be proved finished" — still marked
    /// three roadmap items done, because the goal checks come from
    /// [`VERIFICATION_PATH`] and were run without ever asking what the ledger
    /// said. The roadmap advanced while nothing was built.
    ///
    /// A goal check passing over unfinished work means one of two things, and
    /// both need the owner rather than a tick: the check is too weak to notice
    /// the work is missing, or the work was never needed. Silently ticking
    /// picks the second without evidence.
    Contested { title: String, why: String },
}

impl Standing {
    #[must_use]
    pub fn is_reached(&self) -> bool {
        matches!(self, Self::Reached { .. })
    }

    /// One line for the operator.
    #[must_use]
    pub fn line(&self) -> String {
        match self {
            Self::Reached { title, .. } => format!("reached: {title}"),
            Self::Failed { title, why } => format!("NOT reached: {title} — {why}"),
            Self::Unproven { title } => {
                format!("unproven (no runnable check): {title}")
            }
            Self::Contested { title, why } => {
                format!("NOT reached: {title} — {why}")
            }
        }
    }
}

/// Every runnable goal check the project has written down.
///
/// A missing or unreadable document is an empty list, not an error: a project
/// that has not written goal checks is the ordinary case, and refusing to run
/// over it would make this feature a gate rather than a report.
#[must_use]
pub fn checks_from_docs(workspace: &Path) -> Vec<GoalCheck> {
    let Ok(text) = std::fs::read_to_string(workspace.join(VERIFICATION_PATH)) else {
        return Vec::new();
    };
    parse_checks(&text)
}

/// The checks in a verification document.
#[must_use]
pub fn parse_checks(text: &str) -> Vec<GoalCheck> {
    let mut out: Vec<GoalCheck> = Vec::new();
    for line in text.lines() {
        let Some(check) = parse_check(line) else {
            continue;
        };
        // First writing wins. A document that names an item twice is a document
        // being edited, and silently preferring the later line would make the
        // check depend on where in the file the owner happened to type.
        if !out.iter().any(|c| c.roadmap_id == check.roadmap_id) {
            out.push(check);
        }
    }
    out
}

/// One bullet as a goal check, or `None` when it is prose.
fn parse_check(line: &str) -> Option<GoalCheck> {
    let line = line.trim();
    let body = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("\u{2022} "))?;

    // The command is the LAST backticked span on the line. Last rather than
    // first because a title may itself quote a filename — `index.html` in
    // "opening `index.html` plays the game" is not the check.
    let (before, argv) = split_command(body)?;
    if !is_invocation(&argv) {
        return None;
    }

    let id = leading_id(before)?;
    Some(GoalCheck {
        roadmap_id: id,
        title: title_of(before),
        argv,
    })
}

/// Whether a backticked span is something to RUN, or a filename in prose.
///
/// # Found by running it, not by reading it
///
/// The first cut took any backticked span as the command. Live on the real
/// ball-game document, `- **b3 playable** — opening \`index.html\` plays the
/// game` became a check, and the board showed `✗ b3 — could not run
/// index.html: program not found`. A red cross against a goal nobody ever wrote
/// a check for is worse than the honest `○`: it reports a break that does not
/// exist, and the owner goes looking for it.
///
/// The rule: a check is an INVOCATION.
/// - More than one token is an invocation — `node check-rest.js`.
/// - A single bare word is a command — `make`, `pytest`, `just`.
/// - A single token with a file extension is a filename someone quoted, unless
///   it is written as an explicit path — `./build.sh` says "run me",
///   `index.html` does not.
fn is_invocation(argv: &[String]) -> bool {
    let [only] = argv else {
        return !argv.is_empty();
    };
    let explicit_path = only.starts_with("./")
        || only.starts_with('/')
        || only.starts_with(".\\")
        || only.starts_with('\\');
    explicit_path || std::path::Path::new(only).extension().is_none()
}

/// The text before the final backticked span, and that span split into argv.
fn split_command(body: &str) -> Option<(&str, Vec<String>)> {
    let close = body.rfind('`')?;
    let open = body[..close].rfind('`')?;
    let command = &body[open + 1..close];
    Some((
        &body[..open],
        command.split_whitespace().map(str::to_string).collect(),
    ))
}

/// The roadmap id a bullet opens with — `b1` in `**b1 falls and lands** — ...`.
///
/// Markdown emphasis is stripped first, because the owner writes the id inside
/// bold as often as not and an id of `**b1` matches nothing.
fn leading_id(before: &str) -> Option<String> {
    let cleaned = before.trim().trim_start_matches(['*', '_']).trim_start();
    let token = cleaned.split_whitespace().next()?;
    let token = token.trim_end_matches(['*', '_', ':', '.', ',']);
    // An id has to look like one. Requiring a digit keeps an ordinary English
    // sentence — "the ball falls" — from being read as an item called "the".
    if token.is_empty() || !token.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(token.to_ascii_lowercase())
}

/// The bullet's words, with the markdown and the trailing dash taken off.
fn title_of(before: &str) -> String {
    before
        .trim()
        .trim_matches(|c| c == '*' || c == '_')
        .trim()
        .trim_end_matches(['—', '-', ':'])
        .trim()
        .to_string()
}

/// Run one goal's check.
///
/// Delegates to [`crate::verify::verify`] rather than running the command here,
/// so a goal and a task are proven by the same code and the contract check
/// applies to both.
pub fn reached(check: &GoalCheck, workspace: &Path) -> Result<Verified, String> {
    crate::verify::verify(&check.argv, workspace)
}

/// Where every roadmap item stands, in the roadmap's own order.
///
/// `items` is `(roadmap_id, title)` — taken from the brief rather than from this
/// document, so an item the owner never wrote a check for still appears in the
/// report instead of quietly not existing.
/// The tasks planned for one roadmap item that have NOT finished.
///
/// The sub-goal half of the cross-check: every task carries its own `verify`,
/// and [`crate::ledger::TaskState::Done`] is reachable only through it, so a
/// task sitting in any other state is work this item was told to do and did not
/// prove.
fn unfinished_for<'a>(id: &str, tasks: &'a [crate::ledger::Task]) -> Vec<&'a crate::ledger::Task> {
    tasks
        .iter()
        .filter(|t| t.roadmap_id.eq_ignore_ascii_case(id))
        .filter(|t| t.state != crate::ledger::TaskState::Done)
        .collect()
}

pub fn standings(
    items: &[(String, String)],
    checks: &[GoalCheck],
    workspace: &Path,
    tasks: &[crate::ledger::Task],
) -> Vec<Standing> {
    items
        .iter()
        .map(|(id, title)| {
            let Some(check) = checks
                .iter()
                .find(|c| c.roadmap_id.eq_ignore_ascii_case(id))
            else {
                return Standing::Unproven {
                    title: title.clone(),
                };
            };
            crate::trace!("goal", "{id}: running {:?}", check.argv);
            match reached(check, workspace) {
                Ok(proof) => {
                    // THE CROSS-CHECK. The goal's command exiting zero is not
                    // enough on its own: the orchestrator also asks every task
                    // planned for this item whether it finished. Both halves
                    // must agree before the goal moves.
                    let open = unfinished_for(id, tasks);
                    if open.is_empty() {
                        return Standing::Reached {
                            title: title.clone(),
                            evidence: proof.evidence().summary(),
                        };
                    }
                    let names: Vec<String> = open
                        .iter()
                        .map(|t| format!("{} ({:?})", t.id, t.state))
                        .collect();
                    crate::trace!(
                        "goal",
                        "{id}: check passed but {} task(s) unfinished: {}",
                        open.len(),
                        names.join(", ")
                    );
                    Standing::Contested {
                        title: title.clone(),
                        why: format!(
                            "its check passes, but {} planned task(s) never finished: {}",
                            open.len(),
                            names.join(", ")
                        ),
                    }
                }
                Err(why) => Standing::Failed {
                    title: title.clone(),
                    why: why.lines().next().unwrap_or(&why).to_string(),
                },
            }
        })
        .collect()
}

/// The mark a standing wears on the board.
///
/// Four states, not two. A tick and a blank would force the failures —
/// "the check ran and did not pass", "the check passed over unfinished work"
/// and "nobody wrote a check" — into the same symbol, and those need different
/// actions from the owner: one is a bug, one is a disagreement between the
/// check and the ledger, the last is a sentence somebody has to write.
#[must_use]
pub fn mark(standing: &Standing) -> &'static str {
    match standing {
        Standing::Reached { .. } => "\u{2713}", // ✓ proven by a command
        Standing::Failed { .. } => "\u{2717}",  // ✗ the check ran and failed
        Standing::Unproven { .. } => "\u{25cb}", // ○ nothing can settle it
        Standing::Contested { .. } => "\u{26a0}", // ⚠ check and ledger disagree
    }
}

/// The roadmap as a board the owner can read at a glance.
///
/// # Wired to
///
/// Rendered by `crate::cli::slash_commands` for `/roadmap`, and printed by
/// `crate::cli` when a run ends.
///
/// # Why the tick cannot come from the tasks
///
/// A tick here means a command exited zero — nothing else. It was tempting to
/// mark an item done once its tasks were all `Done`, and that would have been
/// the same lie the completion law exists to stop, moved up one level: the tasks
/// are a PLAN, and a plan that was wrong finishes perfectly while the goal it
/// was meant to reach stays unmet. A green tick that means "we ran some tasks"
/// is worse than no tick, because the owner stops looking.
#[must_use]
pub fn board(items: &[(String, String)], standings: &[Standing]) -> String {
    if items.is_empty() {
        return "No roadmap. The brief carries the goals; without them a run has \
                nothing to aim at."
            .to_string();
    }
    let reached = standings.iter().filter(|s| s.is_reached()).count();
    let width = items.iter().map(|(id, _)| id.len()).max().unwrap_or(2);

    let mut out = String::new();
    for (index, (id, title)) in items.iter().enumerate() {
        let standing = standings.get(index);
        let mark = standing.map_or("\u{25cb}", mark);
        let note = match standing {
            Some(Standing::Failed { why, .. }) => format!("  — {why}"),
            Some(Standing::Unproven { .. }) => {
                format!("  — no runnable check in {VERIFICATION_PATH}")
            }
            _ => String::new(),
        };
        out.push_str(&format!("  {mark} {id:<width$}  {title}{note}\n"));
    }
    out.push_str(&format!(
        "\n  {reached} of {} proven by a command that exited zero.\n",
        items.len()
    ));
    out
}

/// Checks written for an item the roadmap does not have.
///
/// # Why this is worth its own report
///
/// A check whose id matches nothing never runs and says nothing — the owner
/// wrote a real command, believes the goal is covered, and it is silently
/// inert. That is the same shape as every other defect found in this codebase
/// today: not a crash, an absence that looks like success. A typo'd `b4` for
/// `b3` costs nothing to make and, unreported, is invisible forever.
#[must_use]
pub fn orphans<'a>(items: &[(String, String)], checks: &'a [GoalCheck]) -> Vec<&'a GoalCheck> {
    checks
        .iter()
        .filter(|check| {
            !items
                .iter()
                .any(|(id, _)| id.eq_ignore_ascii_case(&check.roadmap_id))
        })
        .collect()
}

/// The report an operator reads at the end of a run, or `None` when there is
/// no roadmap to report on.
#[must_use]
pub fn report(standings: &[Standing]) -> Option<String> {
    if standings.is_empty() {
        return None;
    }
    let reached = standings.iter().filter(|s| s.is_reached()).count();
    let mut out = format!(
        "Goals: {reached} of {} reached by a command that exited zero.\n",
        standings.len()
    );
    for standing in standings {
        out.push_str(&format!("  - {}\n", standing.line()));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real document from the live ball-game project, which is prose only.
    const PROSE_ONLY: &str = "# Verification\n\
        \n\
        The check that decides whether a task is done:\n\
        \n\
            node --check game.js\n\
        \n\
        - **b1 falls and lands** — the ball descends and stops at the floor.\n\
        - **b2 jumps** — space moves it up only when resting on the floor.\n\
        - **b3 playable** — opening `index.html` plays the game with no build step.\n";

    /// **Prose is not a broken check.** Every item here is a sentence a person
    /// confirms, and reporting three failures against a well-written document
    /// would teach the owner to ignore the report.
    ///
    /// `b3` is the trap: it contains backticks (`index.html`) but names no
    /// command. A first cut that took "the last backticked span" without
    /// requiring an id AND a plausible command turned a filename into a goal
    /// check and reported the goal as failing because `index.html` is not an
    /// executable.
    /// **A FILENAME IN PROSE IS NOT A CHECK.** Caught live rather than by
    /// reading: against the real ball-game document the board printed
    /// `✗ b3 — could not run index.html: program not found`, a red cross
    /// against a goal nobody had written a check for. That is worse than the
    /// honest `○`, because it reports a break the owner then goes hunting for.
    ///
    /// Revert `is_invocation` to `!argv.is_empty()` and this fails.
    #[test]
    fn a_prose_document_yields_no_checks_at_all() {
        let checks = parse_checks(PROSE_ONLY);
        assert!(
            checks.is_empty(),
            "every bullet here is prose; `index.html` is a filename the owner \
             quoted, not a command to run: {checks:?}"
        );
    }

    /// The three shapes a single backticked token can take.
    #[test]
    fn a_bare_word_runs_an_explicit_path_runs_a_filename_does_not() {
        let bare = parse_checks("- **b1 builds** — `make`\n");
        assert_eq!(bare.len(), 1, "a bare word is a command: {bare:?}");

        let path = parse_checks("- **b2 builds** — `./build.sh`\n");
        assert_eq!(
            path.len(),
            1,
            "an explicit path says 'run me' even with an extension: {path:?}"
        );

        let filename = parse_checks("- **b3 opens** — `report.pdf`\n");
        assert!(
            filename.is_empty(),
            "a lone token with an extension is a filename: {filename:?}"
        );
    }

    #[test]
    fn an_item_that_names_a_command_becomes_a_runnable_check() {
        let checks = parse_checks(
            "- **b1 falls and lands** — `node check-rest.js`\n\
             - **b2 jumps** — `node check-jump.js`\n",
        );
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].roadmap_id, "b1");
        assert_eq!(checks[0].argv, vec!["node", "check-rest.js"]);
        assert_eq!(checks[1].roadmap_id, "b2");
        assert_eq!(checks[1].argv, vec!["node", "check-jump.js"]);
    }

    /// An ordinary sentence must not become a goal called "the".
    #[test]
    fn a_bullet_with_no_item_id_is_not_a_check() {
        assert!(parse_checks("- the ball falls under `gravity`\n").is_empty());
    }

    #[test]
    fn the_first_writing_of_an_item_wins() {
        let checks = parse_checks(
            "- **b1 first** — `node one.js`\n\
             - **b1 second** — `node two.js`\n",
        );
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].argv, vec!["node", "one.js"]);
    }

    /// **An item with no check is UNPROVEN, never reached.** This is the whole
    /// point of the module: `LedgerEmpty` used to be reported as success, and a
    /// goal nobody can check must not inherit that.
    #[test]
    fn an_item_with_no_check_is_unproven_rather_than_reached() {
        let items = vec![("b9".to_string(), "something nobody checks".to_string())];
        let standings = standings(&items, &[], Path::new("."), &[]);
        assert_eq!(standings.len(), 1);
        assert!(!standings[0].is_reached(), "it was never proven");
        assert!(
            matches!(standings[0], Standing::Unproven { .. }),
            "and it is not a FAILURE either: {:?}",
            standings[0]
        );
    }

    #[test]
    fn the_report_counts_only_what_a_command_proved() {
        let standings = vec![
            Standing::Reached {
                title: "b1".into(),
                evidence: "ok".into(),
            },
            Standing::Unproven { title: "b2".into() },
            Standing::Failed {
                title: "b3".into(),
                why: "exit 1".into(),
            },
        ];
        let report = report(&standings).expect("three standings render");
        assert!(
            report.contains("1 of 3 reached"),
            "unproven and failed must not be counted as reached: {report}"
        );
    }

    #[test]
    fn no_roadmap_means_no_report_rather_than_an_empty_one() {
        assert_eq!(report(&[]), None);
    }

    /// **A check nobody's roadmap claims is INERT, and silence about it is the
    /// bug.** The owner wrote a real command and believes the goal is covered;
    /// a mistyped id means it never runs and never says so.
    #[test]
    fn a_check_matching_no_roadmap_item_is_reported_rather_than_ignored() {
        let items = vec![("b1".to_string(), "the real one".to_string())];
        let checks = parse_checks(
            "- **b1 the real one** — `node one.js`\n\
             - **b4 typo'd id** — `node four.js`\n",
        );
        assert_eq!(checks.len(), 2, "both parse as checks: {checks:?}");

        let orphans = orphans(&items, &checks);
        assert_eq!(orphans.len(), 1, "only b4 matches nothing: {orphans:?}");
        assert_eq!(orphans[0].roadmap_id, "b4");
        assert!(
            orphans[0].title.contains("typo"),
            "the title is what lets the owner find the line: {:?}",
            orphans[0].title
        );
    }

    #[test]
    fn every_check_matching_an_item_leaves_no_orphans() {
        let items = vec![("b1".to_string(), "one".to_string())];
        let checks = parse_checks("- **b1 one** — `node one.js`\n");
        assert!(orphans(&items, &checks).is_empty());
    }

    /// **The board must never tick an item a command did not prove.** This is
    /// the completion law at roadmap level: marking an item done because its
    /// tasks finished would be the same failure `Verified` exists to stop, one
    /// level up — the tasks are a plan, and a wrong plan finishes cleanly while
    /// the goal stays unmet.
    #[test]
    fn only_a_passing_command_earns_a_tick_on_the_board() {
        let items = vec![
            ("b1".to_string(), "falls and lands".to_string()),
            ("b2".to_string(), "jumps".to_string()),
            ("b3".to_string(), "playable".to_string()),
        ];
        let standings = vec![
            Standing::Reached {
                title: "falls and lands".into(),
                evidence: "exit 0".into(),
            },
            Standing::Failed {
                title: "jumps".into(),
                why: "exit 1".into(),
            },
            Standing::Unproven {
                title: "playable".into(),
            },
        ];
        let board = board(&items, &standings);

        assert!(board.contains("\u{2713} b1"), "b1 was proven: {board}");
        assert!(
            board.contains("\u{2717} b2"),
            "b2 ran and failed — a different fact from unproven: {board}"
        );
        assert!(
            board.contains("\u{25cb} b3"),
            "b3 has no check at all: {board}"
        );
        assert!(
            board.contains("1 of 3 proven"),
            "only the passing command counts: {board}"
        );
    }

    /// A run with no goals is worth saying out loud, not rendering as an empty
    /// board that reads like "nothing left to do".
    #[test]
    fn an_empty_roadmap_says_so_rather_than_showing_a_blank_board() {
        let board = board(&[], &[]);
        assert!(board.contains("No roadmap"), "{board}");
    }

    /// Three states need three marks: a failed check and a missing one call for
    /// different actions from the owner.
    #[test]
    fn the_three_standings_wear_three_different_marks() {
        let reached = Standing::Reached {
            title: "t".into(),
            evidence: "e".into(),
        };
        let failed = Standing::Failed {
            title: "t".into(),
            why: "w".into(),
        };
        let unproven = Standing::Unproven { title: "t".into() };
        let contested = Standing::Contested {
            title: "t".into(),
            why: "w".into(),
        };
        assert_ne!(mark(&reached), mark(&failed));
        assert_ne!(mark(&failed), mark(&unproven));
        assert_ne!(mark(&reached), mark(&unproven));
        assert_ne!(mark(&reached), mark(&contested));
        assert_ne!(mark(&failed), mark(&contested));
        assert_ne!(mark(&unproven), mark(&contested));
    }

    /// A command that always succeeds, so the goal check is not the variable —
    /// one every machine has, rather than a runtime the test host may lack.
    fn always_passes() -> GoalCheck {
        let argv: &[&str] = if cfg!(windows) {
            &["cmd", "/C", "exit 0"]
        } else {
            &["true"]
        };
        GoalCheck {
            roadmap_id: "b1".to_string(),
            title: "the ball falls".to_string(),
            argv: argv.iter().map(|a| (*a).to_string()).collect(),
        }
    }

    /// **A goal cannot be reached while its own tasks are unfinished.**
    ///
    /// This is the live failure, reproduced. On D:/ball-game a run refused every
    /// task — "No check is configured, so no planned step can be proved
    /// finished" — and still marked three roadmap items done, because the goal
    /// checks were run without ever asking the ledger. The roadmap advanced
    /// while nothing had been built.
    #[test]
    fn a_passing_goal_check_does_not_tick_an_item_whose_tasks_never_finished() {
        let dir = tempfile::tempdir().unwrap();
        let items = vec![("b1".to_string(), "the ball falls".to_string())];
        let checks = vec![always_passes()];

        // One task for b1 that never reached Done — exactly what a refused
        // attempt leaves behind.
        let mut task = crate::ledger::Task::new("b1.1", "b1", "make it fall");
        task.state = crate::ledger::TaskState::Ready;

        let standings = standings(&items, &checks, dir.path(), std::slice::from_ref(&task));

        assert!(
            !standings[0].is_reached(),
            "the goal ticked over unfinished work: {:?}",
            standings[0]
        );
        assert!(
            matches!(standings[0], Standing::Contested { .. }),
            "an unfinished task must be reported as a disagreement, not hidden: {:?}",
            standings[0]
        );
        assert!(
            standings[0].line().contains("b1.1"),
            "the report must name the task that did not finish: {}",
            standings[0].line()
        );
    }

    /// The control for the test above: the SAME check and item DO reach the goal
    /// once the task is done. Without this, the assertion could be passing
    /// because the check never succeeds.
    #[test]
    fn the_same_goal_is_reached_once_its_tasks_are_done() {
        let dir = tempfile::tempdir().unwrap();
        let items = vec![("b1".to_string(), "the ball falls".to_string())];
        let checks = vec![always_passes()];

        let mut task = crate::ledger::Task::new("b1.1", "b1", "make it fall");
        task.state = crate::ledger::TaskState::Done;

        let standings = standings(&items, &checks, dir.path(), std::slice::from_ref(&task));

        assert!(
            standings[0].is_reached(),
            "a finished item must still reach its goal: {:?}",
            standings[0]
        );
    }

    /// An item with no tasks planned for it is still governed by its check.
    ///
    /// The cross-check must not turn "nobody planned work for this" into a
    /// permanent block — a roadmap item proved entirely by an existing command
    /// is a legitimate case.
    #[test]
    fn an_item_with_no_tasks_is_decided_by_its_check_alone() {
        let dir = tempfile::tempdir().unwrap();
        let items = vec![("b1".to_string(), "the ball falls".to_string())];
        let standings = standings(&items, &[always_passes()], dir.path(), &[]);
        assert!(standings[0].is_reached(), "got {:?}", standings[0]);
    }
}
