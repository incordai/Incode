//! The lanes of a `/run`: one agent worker per lane, each built on the lane's
//! own thread, in the lane's own checkout.
//!
//! # Wired to
//!
//! - **Built by** `run_ledger::run`, through [`CliLaneWorkers::for_run`].
//! - **Implements** [`crate::runner::LaneWorkers`], which the runner asks for a
//!   worker per lane (`runner::lanes`).

use std::path::{Path, PathBuf};

use super::{RunAgents, RunContext};
use crate::permissions::PermissionMode;
use crate::runtime::cancel::CancelToken;

/// Builds one agent worker per lane, each in its own checkout.
///
/// # Why it holds settings and not a backend
///
/// [`crate::agent::AgentBackend`] is not `Send` — it owns callbacks into the
/// terminal — so a backend cannot be handed to a lane's thread. What CAN
/// cross is the settings it is built from: strings, paths and tables. So the
/// backend is constructed inside the lane, from the same
/// `run_backend_with_desktop` the sequential path uses, and no second place
/// decides which agent serves the work.
///
/// A process per lane is not waste: the worker already drops the agent's
/// conversation between tasks, so a reused one would start from nothing
/// anyway.
pub(crate) struct CliLaneWorkers {
    pub agent_id: String,
    pub command: String,
    pub guards: crate::guards::Guards,
    pub system_prompt: Vec<String>,
    pub roles: crate::roles::RoleTable,
    pub extra_dirs: Vec<PathBuf>,
    pub desktop: bool,
    /// Carried rather than read from the environment, because a lane runs on
    /// its own thread and a capability decision should not depend on what a
    /// process-wide variable happens to hold when that thread gets there.
    pub permission_mode: PermissionMode,
    /// Who follows each lane as it works, when anyone does: `incode --acp`
    /// shows every lane as a sub-agent with its own conversation.
    pub watch: Option<std::sync::Arc<dyn crate::watch::Watch>>,
    /// The run's stop. A lane's agent stops with it at once; what it had done
    /// is sealed onto its branch when the lane is closed, and not merged.
    pub cancel: CancelToken,
    /// How many lanes this run has opened, so each is named apart.
    pub opened: std::sync::atomic::AtomicUsize,
    /// The memory engine each lane's session is filed with, when there is
    /// one: every lane is a session of its own.
    pub engine: Option<crate::hooks::engine::Engine>,
    /// Lanes are opened as pull requests on GitHub rather than merged here
    /// (`INCODE_LANES_AS=pr`), read once when the run starts.
    pub review: bool,
    /// The operator's hooks, handed to every lane's worker.
    pub operator_hooks: Option<crate::hooks::HookRunner>,
}

impl CliLaneWorkers {
    /// The lanes of one run: its sub-agent and settings, and what the run
    /// shares with every lane — who watches, the stop, the memory engine and
    /// the operator's hooks.
    ///
    /// Called by `run_ledger::run`, once the run's own worker is built.
    pub(super) fn for_run(
        ctx: &RunContext<'_>,
        agents: &RunAgents,
        watch: Option<std::sync::Arc<dyn crate::watch::Watch>>,
        cancel: &CancelToken,
        engine: Option<crate::hooks::engine::Engine>,
        operator_hooks: Option<crate::hooks::HookRunner>,
    ) -> Self {
        Self {
            // Lanes work tasks, so they run the sub-agent.
            agent_id: agents.worker.id.clone(),
            command: agents.worker.command.clone(),
            guards: agents.guards.clone(),
            system_prompt: ctx.system_prompt.to_vec(),
            roles: agents.roles.clone(),
            extra_dirs: ctx.extra_dirs.to_vec(),
            desktop: crate::cli::desktop_enabled_from_env(),
            permission_mode: ctx.permission_mode,
            watch,
            cancel: cancel.clone(),
            opened: std::sync::atomic::AtomicUsize::new(0),
            engine,
            review: crate::parallel::lanes_as_pull_requests(),
            operator_hooks,
        }
    }
}

impl crate::runner::LaneWorkers for CliLaneWorkers {
    /// GitHub through `gh`, always: a task waiting on a pull request is
    /// settled whether or not this run opens new ones.
    fn forge(&self) -> Option<&dyn crate::tools::pull_request::Forge> {
        Some(&crate::tools::pull_request::GitHubCli)
    }

    fn open_for_review(&self) -> bool {
        self.review
    }

    fn spawn_for(&self, workspace: &Path) -> Option<Box<dyn crate::runner::TaskWorker + Send>> {
        let lane = self
            .opened
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        Some(Box::new(LaneAgent {
            workspace: workspace.to_path_buf(),
            agent_id: self.agent_id.clone(),
            command: self.command.clone(),
            guards: self.guards.clone(),
            system_prompt: self.system_prompt.clone(),
            roles: self.roles.clone(),
            extra_dirs: self.extra_dirs.clone(),
            desktop: self.desktop,
            permission_mode: self.permission_mode,
            watch: self.watch.clone(),
            cancel: self.cancel.clone(),
            lane,
            engine: self.engine.clone(),
            operator_hooks: self.operator_hooks.clone(),
        }))
    }
}

/// One lane's agent, built where it runs.
struct LaneAgent {
    workspace: PathBuf,
    agent_id: String,
    command: String,
    guards: crate::guards::Guards,
    system_prompt: Vec<String>,
    roles: crate::roles::RoleTable,
    extra_dirs: Vec<PathBuf>,
    desktop: bool,
    permission_mode: PermissionMode,
    watch: Option<std::sync::Arc<dyn crate::watch::Watch>>,
    cancel: CancelToken,
    /// This lane's number in the run, from 1.
    lane: usize,
    /// The memory engine its session is filed with, when there is one.
    engine: Option<crate::hooks::engine::Engine>,
    /// The operator's hooks, for each task's instruction.
    operator_hooks: Option<crate::hooks::HookRunner>,
}

impl crate::runner::TaskWorker for LaneAgent {
    fn work(&mut self, task: &crate::ledger::Task) -> Result<String, String> {
        // Built HERE, on the lane's own thread, because the backend cannot
        // cross a thread boundary. Every argument is the one the sequential
        // path passes, except the workspace — which is the whole point.
        let backend = crate::cli::run_backend_with_desktop(
            &self.workspace,
            &self.agent_id,
            &self.command,
            &self.guards,
            &self.system_prompt,
            self.desktop,
            self.permission_mode,
        );
        self.work_on(backend, task)
    }
}

impl LaneAgent {
    /// Work `task` on `backend`, followed by the watch when there is one, and
    /// stopped with the run.
    fn work_on(
        &self,
        mut backend: Box<dyn crate::agent::AgentBackend>,
        task: &crate::ledger::Task,
    ) -> Result<String, String> {
        backend.set_cancel(self.cancel.clone());
        // Announced before it starts; its tools report under it.
        let lane = format!("lane {}", self.lane);
        let tap = self.watch.as_ref().and_then(|watch| {
            watch.worker(&crate::watch::WorkerSpec {
                kind: "lane",
                name: format!("{} · {lane}", task.role().unwrap_or("task")),
                task: task.title.clone(),
                role: task.role().map(str::to_string),
                agent: Some(self.agent_id.clone()),
                // Routed by the worker as the task starts, so not known here.
                model: None,
                lane: Some(lane.clone()),
                workspace: self.workspace.clone(),
            })
        });
        if let Some(tap) = &tap {
            backend.set_watch(tap.watch());
        }
        // A session of its own with the memory engine, like every worker.
        let session = uuid::Uuid::new_v4().to_string();
        backend.set_memory_session(&session);
        let mut agent =
            crate::agent::worker::AgentBackendWorker::in_workspace(backend, self.workspace.clone())
                .with_roles(self.roles.clone())
                .with_extra_dirs(self.extra_dirs.clone());
        if let Some(tap) = tap {
            agent = agent.with_tap(tap);
        }
        let mut worker = crate::cli::run_memory::Remembered::new(
            agent,
            crate::hooks::memory::MemoryHooks::new(self.engine.clone(), &self.workspace, &session),
        )
        .with_operator_hooks(self.operator_hooks.clone());

        let report = crate::runner::TaskWorker::work(&mut worker, task);
        // The conversation this lane opened is on the vendor's side and is
        // billed on every later turn. Nothing else will clear it: the worker
        // is dropped here.
        crate::runner::TaskWorker::release(&mut worker);
        if let Some(mut tap) = worker.inner_mut().take_tap() {
            tap.ended(if self.cancel.is_cancelled() {
                crate::watch::WorkerEnd::Cancelled
            } else if report.is_ok() {
                crate::watch::WorkerEnd::Completed
            } else {
                crate::watch::WorkerEnd::Failed
            });
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::{Decision, Watch, WorkerEnd, WorkerSpec, WorkerTap};
    use std::sync::{Arc, Mutex};

    /// A watch that keeps what the lanes told it.
    #[derive(Default)]
    struct Kept {
        spawned: Mutex<Vec<WorkerSpec>>,
        said: Arc<Mutex<Vec<String>>>,
        ended: Arc<Mutex<Vec<WorkerEnd>>>,
    }

    struct KeptTap {
        said: Arc<Mutex<Vec<String>>>,
        ended: Arc<Mutex<Vec<WorkerEnd>>>,
    }

    impl Watch for Kept {
        fn output(&self, _: &str, _: &str) {}
        fn decided(&self, _: &str, _: &Decision) {}
        fn recorded(&self, _: &str, _: bool, _: Option<u64>) {}
        fn worker(&self, spec: &WorkerSpec) -> Option<Box<dyn WorkerTap>> {
            self.spawned.lock().expect("spawned").push(spec.clone());
            Some(Box::new(KeptTap {
                said: Arc::clone(&self.said),
                ended: Arc::clone(&self.ended),
            }))
        }
    }

    impl WorkerTap for KeptTap {
        fn event(&mut self, event: &crate::agent::AgentEvent) {
            if let crate::agent::AgentEvent::TextDelta(text) = event {
                self.said.lock().expect("said").push(text.clone());
            }
        }
        fn ended(&mut self, end: WorkerEnd) {
            self.ended.lock().expect("ended").push(end);
        }
        fn watch(&self) -> Arc<dyn Watch> {
            Arc::new(Kept::default())
        }
    }

    /// An agent that says one thing and finishes.
    struct Says(&'static str);

    impl crate::agent::AgentBackend for Says {
        fn id(&self) -> &str {
            "claude-code"
        }
        fn run_turn(
            &mut self,
            _prompt: &str,
            on_event: &mut dyn FnMut(&crate::agent::AgentEvent),
        ) -> Result<crate::agent::TurnOutcome, crate::agent::AgentError> {
            on_event(&crate::agent::AgentEvent::TextDelta(self.0.to_string()));
            on_event(&crate::agent::AgentEvent::Text(self.0.to_string()));
            Ok(crate::agent::TurnOutcome {
                session_id: "lane-conv".to_string(),
                stop_reason: "end_turn".to_string(),
                is_error: false,
                detail: None,
                served_by: Vec::new(),
            })
        }
        fn session_id(&self) -> Option<&str> {
            None
        }
    }

    /// **EACH LANE IS A WORKER OF ITS OWN.** Its agent is announced as
    /// `<role> · lane <n>` with its task, every event of its turn reaches the
    /// tap it was given, and it is said to have ended once it has.
    #[test]
    fn a_lane_is_announced_followed_and_ended() {
        let ws = tempfile::tempdir().expect("ws");
        let kept = Arc::new(Kept::default());
        let lanes = CliLaneWorkers {
            agent_id: "claude-code".to_string(),
            command: "claude".to_string(),
            guards: crate::guards::Guards::default(),
            system_prompt: Vec::new(),
            roles: crate::roles::RoleTable::default(),
            extra_dirs: Vec::new(),
            desktop: false,
            permission_mode: PermissionMode::WorkspaceWrite,
            watch: Some(Arc::clone(&kept) as Arc<dyn Watch>),
            cancel: CancelToken::new(),
            opened: std::sync::atomic::AtomicUsize::new(0),
            engine: None,
            review: false,
            operator_hooks: None,
        };
        let task = crate::ledger::Task::new("t-1", "r-1", "count the files").with_tier("implement");
        // The lane's own agent, as `spawn_for` makes it, on a stand-in
        // backend: a batch-file stand-in CLI cannot take the worker's
        // multi-line prompt on Windows.
        let _ = crate::runner::LaneWorkers::spawn_for(&lanes, ws.path()).expect("a lane");
        let lane = LaneAgent {
            workspace: ws.path().to_path_buf(),
            agent_id: lanes.agent_id.clone(),
            command: lanes.command.clone(),
            guards: lanes.guards.clone(),
            system_prompt: Vec::new(),
            roles: lanes.roles.clone(),
            extra_dirs: Vec::new(),
            desktop: false,
            permission_mode: lanes.permission_mode,
            watch: lanes.watch.clone(),
            cancel: lanes.cancel.clone(),
            lane: lanes.opened.load(std::sync::atomic::Ordering::Relaxed),
            engine: None,
            operator_hooks: None,
        };
        let report = lane.work_on(Box::new(Says("three files")), &task);
        assert!(report.is_ok(), "{report:?}");

        let spawned = kept.spawned.lock().expect("spawned").clone();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].name, "implement · lane 1");
        assert_eq!(spawned[0].task, "count the files");
        assert_eq!(spawned[0].lane.as_deref(), Some("lane 1"));
        assert_eq!(spawned[0].agent.as_deref(), Some("claude-code"));
        assert_eq!(kept.said.lock().expect("said").concat(), "three files");
        assert_eq!(
            kept.ended.lock().expect("ended").clone(),
            [WorkerEnd::Completed]
        );
    }
}
