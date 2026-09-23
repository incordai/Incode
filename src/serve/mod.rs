//! What a watching interface needs, read from what the run already wrote.
//!
//! # Read-only, on purpose
//!
//! Nothing in here starts work, stops work, or speaks to an agent. It reads the
//! ledger and the transcripts and says what it found. That is not a first
//! instalment with the interesting half missing — it is the half that CANNOT
//! break a run, and it is therefore the half that can ship while a run is
//! working.
//!
//! Sending an instruction back the other way is a different thing entirely: it
//! needs something to own the run loop, and a chat box wired to a working agent
//! would arrive mid-turn, where most of these CLIs either drop it or restart
//! and re-bill the whole context. That belongs behind its own door.

pub mod http;
pub mod project;
pub mod state;

use state::{Item, RunState, WorkState, Worker};

/// Every task in the ledger, in plan order — queued, blocked and finished
/// alike, with the branch a conflicted lane left its work on.
///
/// [`snapshot_at`] shows only tasks an agent has touched, and shows an
/// unreadable ledger as an empty board. A task list must not: a corrupt
/// ledger is an error a person needs to see, not "nothing to do".
///
/// # Errors
///
/// The ledger exists and cannot be read or parsed.
pub fn tasks_of(workspace: &std::path::Path) -> Result<Vec<crate::ledger::Task>, String> {
    crate::ledger::Ledger::load(workspace).map(|ledger| ledger.tasks)
}

/// The team channel as an interface shows it.
#[derive(Debug, serde::Serialize)]
pub struct Team {
    /// Everything said, oldest first.
    pub messages: Vec<crate::team::Message>,
    /// Questions nobody has answered — what an answer box is built from.
    pub open_questions: Vec<crate::team::Message>,
}

/// What the agents said to each other, and what still waits for an answer.
#[must_use]
pub fn team_of(workspace: &std::path::Path) -> Team {
    let channel = crate::team::Channel::in_workspace(workspace);
    Team {
        messages: channel.all(),
        open_questions: channel.open_questions(),
    }
}

/// Everything the interface shows, as of now.
///
/// `now` is passed in rather than read here so the staleness rule can be tested
/// without waiting ninety seconds for it.
#[must_use]
pub fn snapshot_at(workspace: &std::path::Path, now: &str) -> RunState {
    let ledger = crate::ledger::Ledger::load(workspace).unwrap_or_else(|_| {
        // A project with no ledger is not an error: it is a workspace nobody
        // has run yet, and the interface should show an empty board rather
        // than an error card.
        crate::ledger::Ledger::new()
    });

    let mut workers = Vec::new();
    for task in &ledger.tasks {
        let path = workspace
            .join(crate::transcript::DIR)
            .join(format!("{}.jsonl", task.id));
        let Ok(text) = std::fs::read_to_string(&path) else {
            // No transcript means this task has never been picked up. It is
            // still real work — it belongs to the roadmap below — but there is
            // no agent to show for it.
            continue;
        };
        let Some(line) = state::last_line(&text) else {
            continue;
        };

        let (mut work_state, mut status) =
            state::from_line(&line, state::seconds_between(&line.at, now));

        // THE LEDGER OVERRULES THE TRANSCRIPT, because it is the only one that
        // knows whether the work was PROVED. A transcript ends with the agent
        // saying it finished; that is a claim, and this harness does not take
        // claims. `Done` here means a command exited zero.
        match task.state {
            crate::ledger::TaskState::Done => {
                work_state = WorkState::Done;
                status = task
                    .evidence
                    .clone()
                    .unwrap_or_else(|| "verified".to_string());
            }
            crate::ledger::TaskState::Blocked => {
                work_state = WorkState::Blocked;
                status = "waiting on a person".to_string();
            }
            crate::ledger::TaskState::Abandoned => {
                work_state = WorkState::Idle;
                status = "set aside".to_string();
            }
            _ => {}
        }

        let (name, model) = state::attributed(&task.worked_by);
        workers.push(Worker {
            // The transcript's agent, not the ledger's: the ledger records who
            // finished, and mid-task those differ — which is exactly when
            // someone is watching.
            id: line.agent.clone(),
            name: if name.is_empty() {
                line.role.clone()
            } else {
                name
            },
            model,
            task: task.id.clone(),
            title: first_line(&task.title),
            state: work_state,
            status,
            at: line.at.clone(),
        });
    }

    // Most recent first: the thing that just happened is the thing being
    // watched, and a stable id order would bury it under yesterday's work.
    workers.sort_by(|a, b| b.at.cmp(&a.at));

    RunState {
        workspace: workspace.to_string_lossy().to_string(),
        workers,
        roadmap: roadmap_of(&ledger, workspace),
        at: now.to_string(),
    }
}

/// The snapshot, timed by this machine's clock.
#[must_use]
pub fn snapshot(workspace: &std::path::Path) -> RunState {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    snapshot_at(workspace, &now)
}

/// Progress per roadmap item.
///
/// Counted from the ledger's own states, so "done" carries the meaning it
/// carries everywhere else in this harness: proved by a command that exited
/// zero. A percentage assembled from anything softer would be the number an
/// operator trusts and the one that lies.
fn roadmap_of(ledger: &crate::ledger::Ledger, workspace: &std::path::Path) -> Vec<Item> {
    // Titles come from the brief when there is one, so an item reads as the
    // GOAL rather than as whichever task happened to be planned first.
    let titles: std::collections::HashMap<String, String> = match crate::brief::check(workspace) {
        crate::brief::Gate::Ready(brief) => brief
            .roadmap
            .iter()
            .map(|item| (item.id.clone(), item.title.clone()))
            .collect(),
        _ => std::collections::HashMap::new(),
    };

    let mut order: Vec<String> = Vec::new();
    let mut items: std::collections::HashMap<String, Item> = std::collections::HashMap::new();
    for task in &ledger.tasks {
        let id = task.roadmap_id.clone();
        if !items.contains_key(&id) {
            order.push(id.clone());
            items.insert(
                id.clone(),
                Item {
                    title: titles.get(&id).cloned().unwrap_or_else(|| id.clone()),
                    id: id.clone(),
                    done: 0,
                    total: 0,
                    blocked: false,
                },
            );
        }
        let Some(item) = items.get_mut(&id) else {
            continue;
        };
        item.total += 1;
        match task.state {
            crate::ledger::TaskState::Done => item.done += 1,
            // Blocked and Abandoned both mean a person is needed — one waiting
            // for a decision, one out of attempts. The interface shows the same
            // thing for both: this will not finish on its own.
            crate::ledger::TaskState::Blocked | crate::ledger::TaskState::Abandoned => {
                item.blocked = true;
            }
            _ => {}
        }
    }
    order
        .into_iter()
        .filter_map(|id| items.remove(&id))
        .collect()
}

/// The first line of a title, for a card that has one line to spare.
fn first_line(title: &str) -> String {
    title.lines().next().unwrap_or(title).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{Ledger, Task, TaskState};

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().expect("a scratch workspace")
    }

    fn write_transcript(root: &std::path::Path, task: &str, lines: &[&str]) {
        let dir = root.join(crate::transcript::DIR);
        std::fs::create_dir_all(&dir).expect("the transcript dir");
        std::fs::write(dir.join(format!("{task}.jsonl")), lines.join("\n"))
            .expect("write the transcript");
    }

    /// **The snapshot answers who is working, on what, and where.**
    ///
    /// The three questions the left column exists for, from files the run
    /// already wrote — no status store, nothing pushed by the runner.
    #[test]
    fn a_snapshot_names_the_worker_the_task_and_the_file() {
        let dir = workspace();
        let mut task = Task::new("d2.1", "d2", "add the jump\nsecond line ignored");
        task.state = TaskState::Running;
        task.worked_by =
            "claude-code:implement (1a534c20) [agent-9ac96a08d0f8] on claude-sonnet-5".to_string();
        let ledger = Ledger {
            version: 1,
            tasks: vec![task],
        };
        ledger.save(dir.path()).expect("save the ledger");

        write_transcript(
            dir.path(),
            "d2.1",
            &[
                r#"{"kind":"briefed","agent":"agent-9ac96a08d0f8","role":"implement","at":"2026-09-05T10:00:00.000Z"}"#,
                r#"{"kind":"tool_use","agent":"agent-9ac96a08d0f8","role":"implement","at":"2026-09-05T10:00:05.000Z","tool":"Edit","input":"{\"file_path\":\"game.js\",\"old_string\":\"a\",\"new_string\":\"b\"}"}"#,
            ],
        );

        let snap = snapshot_at(dir.path(), "2026-09-05T10:00:06.000Z");

        assert_eq!(snap.workers.len(), 1, "{:?}", snap.workers);
        let worker = &snap.workers[0];
        assert_eq!(
            worker.id, "agent-9ac96a08d0f8",
            "the INSTANCE identifies it"
        );
        assert_eq!(worker.name, "claude-code:implement");
        assert_eq!(worker.model, "claude-sonnet-5");
        assert_eq!(worker.task, "d2.1");
        assert_eq!(worker.title, "add the jump", "one line for a one-line card");
        assert_eq!(worker.state, WorkState::Writing);
        assert_eq!(worker.status, "writing · game.js");
    }

    /// **A finished task shows its EVIDENCE, not a tick.**
    ///
    /// The transcript ends with the agent saying it is done. That is a claim,
    /// and this harness does not take claims — so the ledger overrules it, and
    /// what the interface shows is the command that exited zero.
    #[test]
    fn a_proved_task_reports_the_command_that_proved_it() {
        let dir = workspace();
        let mut task = Task::new("d1.1", "d1", "make the ball fall");
        task.state = TaskState::Done;
        task.evidence = Some("`node --check game.js` passed in 64ms".to_string());
        task.worked_by = "claude-code:implement (1a534c20) [agent-1] on m".to_string();
        Ledger {
            version: 1,
            tasks: vec![task],
        }
        .save(dir.path())
        .expect("save");

        // The agent's last word is a CLAIM that it finished.
        write_transcript(
            dir.path(),
            "d1.1",
            &[
                r#"{"kind":"said","agent":"agent-1","role":"implement","at":"2026-09-05T10:00:00.000Z","text":"All done!"}"#,
            ],
        );

        let snap = snapshot_at(dir.path(), "2026-09-05T10:00:01.000Z");
        assert_eq!(snap.workers[0].state, WorkState::Done);
        assert!(
            snap.workers[0].status.contains("node --check"),
            "the proof, not a tick: {}",
            snap.workers[0].status
        );
    }

    /// **Progress counts only what was PROVED, and flags what needs a person.**
    #[test]
    fn roadmap_progress_counts_proved_work_and_marks_blocked_items() {
        let dir = workspace();
        let done = |id: &str, road: &str| {
            let mut t = Task::new(id, road, "t");
            t.state = TaskState::Done;
            t
        };
        let mut blocked = Task::new("d2.1", "d2", "t");
        blocked.state = TaskState::Blocked;
        let running = Task::new("d2.2", "d2", "t");

        Ledger {
            version: 1,
            tasks: vec![done("d1.1", "d1"), done("d1.2", "d1"), blocked, running],
        }
        .save(dir.path())
        .expect("save");

        let snap = snapshot_at(dir.path(), "2026-09-05T10:00:00.000Z");

        assert_eq!(snap.roadmap.len(), 2, "one entry per roadmap item");
        let d1 = &snap.roadmap[0];
        assert_eq!((d1.done, d1.total, d1.blocked), (2, 2, false));
        let d2 = &snap.roadmap[1];
        assert_eq!(
            (d2.done, d2.total, d2.blocked),
            (0, 2, true),
            "a blocked task marks its ITEM as needing a person"
        );
    }

    /// A task nobody has picked up yet contributes to progress and shows no
    /// worker — rather than inventing an idle one that never existed.
    #[test]
    fn an_unstarted_task_is_progress_without_a_worker() {
        let dir = workspace();
        Ledger {
            version: 1,
            tasks: vec![Task::new("d1.1", "d1", "not started")],
        }
        .save(dir.path())
        .expect("save");

        let snap = snapshot_at(dir.path(), "2026-09-05T10:00:00.000Z");
        assert!(snap.workers.is_empty(), "{:?}", snap.workers);
        assert_eq!(snap.roadmap[0].total, 1, "still counted as work to do");
    }

    /// A workspace nobody has run shows an empty board, never an error.
    #[test]
    fn a_workspace_with_no_ledger_reports_an_empty_board() {
        let dir = workspace();
        let snap = snapshot_at(dir.path(), "2026-09-05T10:00:00.000Z");
        assert!(snap.workers.is_empty());
        assert!(snap.roadmap.is_empty());
    }

    /// **The most recent worker comes first.**
    ///
    /// The one that just did something is the one being watched; ordering by id
    /// would bury it under whatever ran yesterday.
    #[test]
    fn workers_are_ordered_by_what_happened_most_recently() {
        let dir = workspace();
        let task = |id: &str| {
            let mut t = Task::new(id, "d1", "t");
            t.state = TaskState::Running;
            t.worked_by = format!("claude-code:implement (x) [{id}] on m");
            t
        };
        Ledger {
            version: 1,
            tasks: vec![task("d1.1"), task("d1.2")],
        }
        .save(dir.path())
        .expect("save");

        write_transcript(
            dir.path(),
            "d1.1",
            &[r#"{"kind":"said","agent":"older","at":"2026-09-05T10:00:00.000Z"}"#],
        );
        write_transcript(
            dir.path(),
            "d1.2",
            &[r#"{"kind":"said","agent":"newer","at":"2026-09-05T10:00:30.000Z"}"#],
        );

        let snap = snapshot_at(dir.path(), "2026-09-05T10:00:31.000Z");
        assert_eq!(
            snap.workers
                .iter()
                .map(|w| w.id.as_str())
                .collect::<Vec<_>>(),
            vec!["newer", "older"]
        );
    }
}
