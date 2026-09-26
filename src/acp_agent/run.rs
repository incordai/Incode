//! `/run` over ACP: the ledger worked by the REPL's own
//! [`crate::cli::run_ledger::run`], its words streamed as message chunks and
//! the ledger sent as the client's plan — one entry per task, the running one
//! `in_progress`, re-sent as tasks finish.
//!
//! A `session/cancel` trips the session's token, which the run looks at
//! between tasks — the same point Ctrl-C and the interface's stop file land —
//! so the task already running finishes first and the prompt then answers
//! `cancelled`.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use agent_client_protocol_schema::v1 as acp;

use super::transport::Outbox;
use super::Shared;
use crate::ledger::{Ledger, TaskState};
use crate::permissions::PermissionMode;
use crate::runner::Progress;
use crate::runtime::cancel::CancelToken;

/// How `/run` is run for a session.
///
/// A trait so a test can work a ledger with a worker of its own; the
/// production one is [`CliRuns`].
pub(crate) trait LedgerRuns {
    /// Work the ledger in `workspace` under `mode`, stopping between tasks
    /// once `cancel` is tripped, telling `progress` as it goes.
    fn run(
        &mut self,
        workspace: &Path,
        cancel: &CancelToken,
        mode: PermissionMode,
        progress: &mut dyn Progress,
    );
}

/// What the REPL's `/run` is given, held for the session.
pub(crate) struct CliRuns {
    pub(crate) data_dir: std::path::PathBuf,
    pub(crate) admin: crate::admin::AdminConfig,
    pub(crate) catalogue: crate::agent::catalogue::Catalogue,
    pub(crate) fleet: crate::fleet::Fleet,
    pub(crate) system_prompt: Vec<String>,
    pub(crate) extra_dirs: Vec<std::path::PathBuf>,
    pub(crate) env_filter: bool,
}

impl LedgerRuns for CliRuns {
    fn run(
        &mut self,
        workspace: &Path,
        cancel: &CancelToken,
        mode: PermissionMode,
        progress: &mut dyn Progress,
    ) {
        crate::cli::run_ledger::run(
            &crate::cli::run_ledger::RunContext {
                workspace,
                data_dir: &self.data_dir,
                admin: &self.admin,
                catalogue: &self.catalogue,
                fleet: &self.fleet,
                system_prompt: &self.system_prompt,
                extra_dirs: &self.extra_dirs,
                env_filter: self.env_filter,
                permission_mode: mode,
            },
            cancel,
            crate::cli::run_ledger::Watch::By(progress),
        );
    }
}

/// The ledger as an ACP plan: a task that is done is `completed`, the one
/// claimed is `in_progress`, every other is `pending` — and one that will not
/// run again without a person says so in its text. The ledger has no
/// priority, so none is invented: every entry is `medium`.
pub(super) fn plan_of(ledger: &Ledger) -> acp::Plan {
    let entries = ledger
        .tasks
        .iter()
        .map(|task| {
            let status = match task.state {
                TaskState::Done => acp::PlanEntryStatus::Completed,
                TaskState::Running => acp::PlanEntryStatus::InProgress,
                _ => acp::PlanEntryStatus::Pending,
            };
            let content = match task.state {
                TaskState::Blocked => format!("{} (waiting on the owner)", task.title),
                TaskState::Abandoned => format!("{} (out of attempts)", task.title),
                TaskState::Cancelled => format!("{} (dismissed)", task.title),
                _ => task.title.clone(),
            };
            acp::PlanEntry::new(content, acp::PlanEntryPriority::Medium, status)
        })
        .collect();
    acp::Plan::new(entries)
}

/// Sends the ledger as the plan whenever it changes.
struct PlanFeed<'a> {
    outbox: &'a Outbox,
    session_id: &'a acp::SessionId,
    shared: &'a Shared,
    last: Option<acp::Plan>,
    /// Text the run printed that has not ended its line yet; sent before a
    /// plan, so the two arrive in the order they happened.
    pending: Rc<RefCell<String>>,
    /// The session's watch: each lane of the run is shown as a sub-agent.
    watch: &'a super::workers::SessionWatch,
}

impl Progress for PlanFeed<'_> {
    fn watch(&self) -> Option<Arc<dyn crate::watch::Watch>> {
        Some(Arc::new(self.watch.clone()))
    }

    fn advanced(&mut self, _done: usize, _total: usize) {
        self.shared.hold_cancel();
    }

    fn ledger(&mut self, ledger: &Ledger) {
        self.shared.hold_cancel();
        let plan = plan_of(ledger);
        if self.last.as_ref() == Some(&plan) {
            return;
        }
        flush(self.outbox, self.session_id, &self.pending);
        self.outbox.notify(
            acp::CLIENT_METHOD_NAMES.session_update,
            acp::SessionNotification::new(
                self.session_id.clone(),
                acp::SessionUpdate::Plan(plan.clone()),
            ),
        );
        self.last = Some(plan);
    }
}

/// Send what is held as one message chunk, and hold nothing.
fn flush(outbox: &Outbox, session_id: &acp::SessionId, pending: &Rc<RefCell<String>>) {
    let text = std::mem::take(&mut *pending.borrow_mut());
    say(outbox, session_id, &text);
}

fn say(outbox: &Outbox, session_id: &acp::SessionId, text: &str) {
    if text.is_empty() {
        return;
    }
    outbox.notify(
        acp::CLIENT_METHOD_NAMES.session_update,
        acp::SessionNotification::new(
            session_id.clone(),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::from(
                text,
            ))),
        ),
    );
}

/// Run `/run` for a session: its words as they are printed, a line at a
/// time, and its ledger as the plan. Answers `cancelled` when the client
/// cancelled it.
pub(super) fn run(
    runs: &mut dyn LedgerRuns,
    workspace: &Path,
    mode: PermissionMode,
    outbox: &Arc<Outbox>,
    session_id: &acp::SessionId,
    shared: &Shared,
    watch: &super::workers::SessionWatch,
) -> acp::StopReason {
    let pending: Rc<RefCell<String>> = Rc::default();
    let sink = {
        let outbox = Arc::clone(outbox);
        let session_id = session_id.clone();
        let pending = Rc::clone(&pending);
        Box::new(move |text: &str| {
            let mut held = pending.borrow_mut();
            held.push_str(text);
            // Whole lines go out as they are finished; a part-line waits.
            if let Some(end) = held.rfind('\n') {
                let lines: String = held.drain(..=end).collect();
                say(&outbox, &session_id, &lines);
            }
        })
    };
    let mut feed = PlanFeed {
        outbox,
        session_id,
        shared,
        last: None,
        pending: Rc::clone(&pending),
        watch,
    };
    crate::cli::console::stream(sink, || {
        runs.run(workspace, &shared.cancel, mode, &mut feed);
    });
    flush(outbox, session_id, &pending);
    if shared.cancelled() {
        acp::StopReason::Cancelled
    } else {
        acp::StopReason::EndTurn
    }
}
