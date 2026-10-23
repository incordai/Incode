//! The final testing: whether the roadmap items a run was for are reached.
//!
//! Asked once, when the queue has drained — see `ledger_loop`'s `drained`.

use std::path::Path;

use crate::ledger::Ledger;

/// The roadmap items this run did not prove, or `None` if all of them passed.
///
/// # Why a missing brief is not a failure
///
/// A project with no brief has no roadmap, so there is no goal to be unmet and
/// nothing for this to say. Returning "unmet" there would stop every run in
/// every project that has not been through intake yet — a gate that fires when
/// it has no information is a gate people switch off.
///
/// The same reasoning covers an item with no check in `docs/VERIFICATION.md`:
/// `standings` reports it `Unproven`, which is honest, and it is reported
/// rather than treated as a pass.
/// What the final testing found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalVerdict {
    /// Every roadmap item proved reached, or there is no roadmap to prove.
    AllReached,
    /// Checked, and these are not proved.
    Unmet(Vec<String>),
    /// The roadmap could not be read, so nothing was checked.
    Unchecked(String),
}

/// The final testing: whether the roadmap items this run was for are reached.
///
/// A trait for the same reason [`OwnerChannel`] and [`WorkJournal`] are: the
/// real implementation fetches the brief over HTTP, and a runner whose loop can
/// only be exercised with a service running is a loop nobody tests. It also
/// keeps the network out of the scheduling tests, which would otherwise pass or
/// fail depending on whether the memory service happened to be up on the
/// machine running them.
pub trait RoadmapGoals {
    fn verdict(&mut self, workspace: &Path, ledger: &Ledger) -> GoalVerdict;
}

/// No roadmap source, so nothing was checked — and it says exactly that.
///
/// The default, and deliberately NOT "all reached". A gate whose default
/// certifies is not a gate, and this one exists because a drained queue was
/// already being reported as finished work.
pub struct NoGoals;

impl RoadmapGoals for NoGoals {
    fn verdict(&mut self, _workspace: &Path, _ledger: &Ledger) -> GoalVerdict {
        GoalVerdict::Unchecked("no roadmap source was given to this run".to_string())
    }
}

/// The real gate: the roadmap from the brief, with every item's check run.
pub struct BriefGoals;

impl RoadmapGoals for BriefGoals {
    fn verdict(&mut self, workspace: &Path, ledger: &Ledger) -> GoalVerdict {
        goal_verdict(workspace, ledger)
    }
}

fn goal_verdict(workspace: &Path, ledger: &Ledger) -> GoalVerdict {
    let items: Vec<(String, String)> = match crate::brief::check(workspace) {
        crate::brief::Gate::Ready(brief) => brief
            .roadmap
            .iter()
            .map(|item| (item.id.clone(), item.title.clone()))
            .collect(),
        // A project that never went through intake has no roadmap, so there is
        // no goal here to be unmet and nothing for this to say.
        crate::brief::Gate::NoBrief { .. } => return GoalVerdict::AllReached,
        // NOT A PASS, and not a failure either. The brief is the only place the
        // goals are written down, so when it cannot be read the honest answer
        // is "unchecked" — and unchecked must never be reported as reached,
        // because that is silent certification in exactly the conditions where
        // nobody is watching.
        crate::brief::Gate::Unreachable { url, reason } => {
            return GoalVerdict::Unchecked(format!("the brief at {url} is unreachable ({reason})"))
        }
        crate::brief::Gate::Incomplete { project, missing } => {
            return GoalVerdict::Unchecked(format!(
                "the brief for `{project}` is still missing {}",
                missing
                    .iter()
                    .map(|a| a.field())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
    };

    match goals_unreached(&items, workspace, ledger) {
        None => GoalVerdict::AllReached,
        Some(unreached) => GoalVerdict::Unmet(unreached),
    }
}

/// [`unreached_goals`] with the roadmap passed in rather than fetched.
///
/// Split out because the fetch is an HTTP call to the memory service, and a
/// test that needs one cannot run. Everything interesting — running each
/// item's check and deciding what counts as reached — is on this side of the
/// line.
fn goals_unreached(
    items: &[(String, String)],
    workspace: &Path,
    ledger: &Ledger,
) -> Option<Vec<String>> {
    if items.is_empty() {
        return None;
    }

    let checks = crate::goal::checks_from_docs(workspace);
    let standings = crate::goal::standings(items, &checks, workspace, &ledger.tasks);
    let unreached: Vec<String> = standings
        .iter()
        .filter(|s| !s.is_reached())
        .map(crate::goal::Standing::line)
        .collect();

    if unreached.is_empty() {
        None
    } else {
        Some(unreached)
    }
}
