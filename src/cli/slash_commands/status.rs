//! Where things stand: `/status` (the session and the ledger) and `/roadmap`
//! (the goals, and which a command has proven).

use crate::cli::render;
use crate::{out, outln};

/// Save a quick memory (defaults to static/high).
/// What this session is and what it has spent.
///
/// Reports the AGENT's numbers, because the agent is what ran — a count of
/// messages this harness held would be a count of nothing.
pub fn handle_agent_status(
    model: &str,
    workspace: &std::path::Path,
    permission: &str,
    session_id: Option<&str>,
    turns: usize,
    usage: &crate::runtime::usage::TokenUsage,
    quota: Option<(&str, i64)>,
) {
    render::print_header("Session");
    outln!("  Model:            {model}");
    outln!("  Workspace:        {}", workspace.display());
    outln!("  Permission:       {permission}");
    outln!(
        "  Agent session:    {}",
        session_id.unwrap_or("not opened yet")
    );
    outln!("  Turns:            {turns}");
    outln!("  {}", usage.summary_line(None));
    // The number that decides whether work MOVES to another agent. Shown so a
    // failover is something an operator can see coming rather than discover.
    if let Some((status, resets_at)) = quota {
        let room = if crate::agent::is_limit_status(status) {
            "OUT OF ROOM — work will move to a standby agent"
        } else {
            "room to work"
        };
        outln!("  Quota:            {status} ({room})");
        if resets_at > 0 {
            outln!("  Resets at:        {resets_at} (unix time)");
        }
    }

    // The work, not just the session.
    //
    // The ledger is the answer to "how is it going", and it used to be shown
    // only once — after a run ENDED. A person watching a loop had no way to ask
    // mid-flight, which is exactly when they want to know.
    //
    // Read from disk rather than from the runner: the runner owns it during a
    // run and this is the same file it saves after every task, so what is shown
    // is what was actually recorded.
    render::print_header("Work");
    match crate::ledger::Ledger::load(workspace) {
        Ok(ledger) => outln!("{}", ledger.progress()),
        // A ledger that cannot be read is said out loud rather than shown as
        // "no work planned" — those are different facts.
        Err(e) => render::print_error(&format!("the ledger could not be read: {e}")),
    }
}

/// The roadmap, with what a command has actually proven.
///
/// # Wired to
///
/// - **Called by** `crate::cli::run`'s dispatch for [`super::SlashCommand::Roadmap`],
///   and after a run ends.
/// - **Calls** [`crate::brief::check`] for the items and
///   [`crate::goal::standings`] for what is proven.
/// - **Sibling to** [`handle_agent_status`], which shows the LEDGER — the tasks.
///   This shows the ROADMAP — the goals those tasks were meant to reach. They
///   disagree exactly when a plan was wrong, which is the case worth seeing.
///
/// Running the checks costs a command per item, which is why it is a command the
/// owner types rather than something on every prompt.
pub fn handle_roadmap(workspace: &std::path::Path) {
    render::print_header("Roadmap");
    let Some(brief) = ready_brief(workspace) else {
        return;
    };

    let items: Vec<(String, String)> = brief
        .roadmap
        .iter()
        .map(|item| (item.id.clone(), item.title.clone()))
        .collect();
    let checks = crate::goal::checks_from_docs(workspace);
    // The ledger is the other half of the cross-check: a goal may not advance on
    // its own command alone while the tasks planned for it are unfinished. An
    // unreadable ledger is an EMPTY one here, which is the conservative reading
    // — no task claims to be done, so nothing is ticked on its say-so.
    let ledger = crate::ledger::Ledger::load(workspace).unwrap_or_default();
    let standings = crate::goal::standings(&items, &checks, workspace, &ledger.tasks);
    out!("{}", crate::goal::board(&items, &standings));

    mark_proven_done(&brief.project, &items, &standings);
    say_what_cannot_prove(&items, &checks);
}

/// The brief, when it is complete — or the reason there is none, said.
///
/// FOUR OUTCOMES, FOUR MESSAGES. A first cut printed "no brief covers this
/// project" for all of them, and the very first live run hit it with the
/// service mid-restart: the board reported an absent brief for a project
/// whose brief was fine. That is the same conflation of "could not ask" with
/// "the answer is nothing" that cost this codebase a day elsewhere, and
/// `Gate` already draws the distinction — the handler was throwing it away.
///
/// Called by [`handle_roadmap`].
fn ready_brief(workspace: &std::path::Path) -> Option<Box<crate::brief::ProjectBrief>> {
    match crate::brief::check(workspace) {
        crate::brief::Gate::Ready(brief) => Some(brief),
        crate::brief::Gate::Unreachable { url, reason } => {
            render::print_error(&format!(
                "The roadmap could not be read because the memory service did not \
                 answer ({url}): {reason}\nThis is NOT an empty roadmap — start \
                 the service and ask again."
            ));
            None
        }
        crate::brief::Gate::Incomplete { project, missing } => {
            render::print_error(&format!(
                "The brief for {project} is incomplete, so its roadmap is not \
                 settled yet. Missing: {}",
                missing
                    .iter()
                    .map(|a| crate::brief::Artifact::field(*a))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            None
        }
        crate::brief::Gate::NoBrief { workspace } => {
            render::print_error(&format!(
                "No brief covers {} yet, so there is no roadmap to show. The \
                 goals live in the brief.",
                workspace.display()
            ));
            None
        }
    }
}

/// TELL THE BELIEF WHAT THE COMMANDS PROVED.
///
/// Until this existed the roadmap status was written once and never touched
/// by execution, so the proxy answered "todo" for goals that had been passing
/// for days — and the proxy is what reaches the owner's phone. Verified on
/// the live service: all three ball-game items read `status: 'todo'` while
/// `/roadmap` proved all three passed.
///
/// Only a REACHED standing ticks an item, and a standing is only reached when
/// a command exited zero. Nothing here can move an item back — see
/// `brief::mark_done_in_config` for why that asymmetry is deliberate.
///
/// Called by [`handle_roadmap`], after the board is printed.
fn mark_proven_done(
    project: &str,
    items: &[(String, String)],
    standings: &[crate::goal::Standing],
) {
    let proven: Vec<String> = items
        .iter()
        .zip(standings.iter())
        .filter(|(_, standing)| standing.is_reached())
        .map(|((id, _), _)| id.clone())
        .collect();
    match crate::brief::mark_roadmap_done(project, &proven) {
        Ok(0) => {}
        Ok(n) => render::print_info(&format!(
            "{n} roadmap item(s) marked done in the brief — the proxy and \
             anything reading it now agree with the checks."
        )),
        // A belief that cannot be updated is worth saying out loud: the board
        // above is right and the API is stale, which is exactly the drift this
        // is meant to end.
        Err(e) => render::print_error(&format!(
            "The board is correct but the brief could not be updated, so the \
             API still shows the old status: {e}"
        )),
    }
}

/// What the checks cannot prove: a check naming no item, and a roadmap with
/// no checks at all.
///
/// Called by [`handle_roadmap`], last.
fn say_what_cannot_prove(items: &[(String, String)], checks: &[crate::goal::GoalCheck]) {
    // A check that matches no item never runs and never complains. Named here
    // because it is indistinguishable from "covered" everywhere else.
    let orphans = crate::goal::orphans(items, checks);
    if !orphans.is_empty() {
        outln!(
            "\n  {} check(s) in {} name an item this roadmap does not have, so \
             they never run:",
            orphans.len(),
            crate::goal::VERIFICATION_PATH
        );
        for orphan in orphans {
            outln!("    - {} ({})", orphan.title, orphan.roadmap_id);
        }
    }

    // Said once, here, rather than repeated per line: an owner who has written
    // no checks needs to know HOW, not that thirty items are unproven.
    if checks.is_empty() && !items.is_empty() {
        outln!(
            "  Nothing here can be settled by a machine yet. Give an item a \
             command in {} and it will be proven instead of assumed:",
            crate::goal::VERIFICATION_PATH
        );
        outln!("    - **{}** — `the command that proves it`", items[0].0);
    }
}
