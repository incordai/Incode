//! `/run`: the ledger, worked until there is a reason to stop.
//!
//! One function for both front ends. The REPL calls [`run`] with a status bar
//! on the terminal; `incode --acp` calls it with its own [`Progress`], which
//! sends the ledger to the client as a plan, inside a
//! [`crate::cli::console::stream`] that sends what the run prints as it
//! prints it. Both stop the same way: the session's cancel token — Ctrl-C,
//! `session/cancel`, or the interface's stop file — is looked at between
//! tasks, so a task already under way finishes first.
//!
//! [`work`] and [`report`] are the loop and what is said after it, apart from
//! the setup, so a test can drive them with a worker of its own.

use std::path::{Path, PathBuf};

mod lanes;

use crate::cli::render;
use crate::ledger::Ledger;
use crate::outln;
use crate::permissions::PermissionMode;
use crate::runner::Progress;
use crate::runtime::cancel::{CancelToken, StopWatch};
use lanes::CliLaneWorkers;

/// What a run needs from the session that starts it.
pub(crate) struct RunContext<'a> {
    pub(crate) workspace: &'a Path,
    /// Where the operator's settings live; the roles are read fresh from it.
    pub(crate) data_dir: &'a Path,
    pub(crate) admin: &'a crate::admin::AdminConfig,
    pub(crate) catalogue: &'a crate::agent::catalogue::Catalogue,
    pub(crate) fleet: &'a crate::fleet::Fleet,
    pub(crate) system_prompt: &'a [String],
    pub(crate) extra_dirs: &'a [PathBuf],
    pub(crate) env_filter: bool,
    /// The mode the session runs under now. A read-only session does not
    /// start a run: every task in it changes files.
    pub(crate) permission_mode: PermissionMode,
}

/// Who watches the run.
pub(crate) enum Watch<'w> {
    /// The REPL: a status bar on the terminal, when there is one.
    Terminal,
    /// A caller that presents the run itself — `incode --acp`, which sends
    /// the ledger as the client's plan. No status bar is drawn.
    By(&'w mut dyn Progress),
}

/// Work the ledger — derived from the roadmap when there is none yet — and
/// say how it ended.
pub(crate) fn run(ctx: &RunContext<'_>, cancel: &CancelToken, watch: Watch<'_>) {
    let workspace = ctx.workspace;
    let Some((mut ledger, freshly_derived, _stop_watch)) = ledger_for_run(ctx, cancel) else {
        return;
    };

    // The ceiling the operator set, from wherever they set it — not the
    // environment alone, which would ignore the admin file.
    let mut run_budget = ctx.admin.resolve_budget().value;
    // A question the runner must not answer itself goes to the owner's queue.
    // Switched off, the runner stops at a question instead of asking —
    // deliberately NOT "another model decides".
    let mut proxy_owner = crate::runner::MemoryOwner::default();
    let mut no_owner = crate::runner::NoOwner;
    let owner: &mut dyn crate::runner::OwnerChannel = if ctx.admin.plugins.owner {
        &mut proxy_owner
    } else {
        &mut no_owner
    };
    // Finished work is written to the shared store under the AGENT's id.
    let mut journal = crate::runner::MemoryJournal;
    let Some(agents) = RunAgents::resolve(ctx) else {
        return;
    };
    let mut backends = RunBackends::build(ctx, &agents);
    if freshly_derived {
        plan_ledger(ctx, &mut ledger, backends.planner(), &agents.roles);
    }

    // THE WORKER IS A MEMORY SESSION OF ITS OWN, as a person's session is:
    // its agents' calls are filed under its id, and the engine hears it
    // start, stop after each task, and end (`run_memory`). Named after the
    // planning pass, which is the lead's work, not the worker's.
    let engine = crate::hooks::engine::Engine::locate();
    let worker_session = uuid::Uuid::new_v4().to_string();
    let (worker, _lead_planner) = backends.into_worker(ctx, &agents.roles, &worker_session);
    // A bar the whole time the loop runs, on a terminal; and what the run is
    // changing, as it changes it, written through the bar's OWN lock.
    let draw =
        matches!(watch, Watch::Terminal) && std::io::IsTerminal::is_terminal(&std::io::stdout());
    // Who follows the lanes as they work, when the caller has anyone.
    let followed = match &watch {
        Watch::By(progress) => progress.watch(),
        Watch::Terminal => None,
    };
    let run_status = render::LiveStatus::start(draw);
    let edit_writer = run_status.writer();
    let worker = worker.showing_edits(Box::new(move |line: &str| {
        edit_writer.write_text(line);
    }));
    // The operator's hooks, read once for the run: each task's instruction
    // goes through their `user_prompt_submit`, on this worker and every lane.
    let operator_hooks = crate::hooks::HookRunner::from_settings();
    let mut worker = super::run_memory::Remembered::new(
        worker,
        crate::hooks::memory::MemoryHooks::new(engine.clone(), workspace, &worker_session),
    )
    .with_operator_hooks(operator_hooks.clone());
    let mut watcher = RunWatcher::new(&run_status, watch);
    let mut no_replan = crate::runner::NoReplan;
    let mut live_replan = LiveReplanner::for_run(ctx, &agents);
    let replanner: &mut dyn crate::runner::Replanner = if live_replan.titles.is_empty() {
        &mut no_replan
    } else {
        &mut live_replan
    };
    let lanes = CliLaneWorkers::for_run(ctx, &agents, followed, cancel, engine, operator_hooks);
    let outcome = work(
        &mut ledger,
        Crew {
            worker: &mut worker,
            owner,
            journal: &mut journal,
            replanner,
            // THE FINAL TESTING: every roadmap item's check, once the queue
            // has drained.
            goals: &mut crate::runner::BriefGoals,
            // MORE THAN ONE AGENT AT A TIME, when the operator asked for it.
            lanes: &lanes,
        },
        workspace,
        &mut run_budget,
        cancel,
        &mut watcher,
    );
    run_status.stop();
    report(&outcome, &ledger, workspace);
    // WHAT THE RUN ACTUALLY ACHIEVED, not what it attempted: the same call
    // `/roadmap` makes, so the board and the belief are told either way.
    super::slash_commands::handle_roadmap(workspace);
}

/// Whether this session may start a run at all. Why not is said.
///
/// Called by [`ledger_for_run`], before anything is read or built.
fn may_start(ctx: &RunContext<'_>) -> bool {
    if ctx.permission_mode == PermissionMode::ReadOnly {
        render::print_error(
            "The ledger run changes files, and this session is read-only. Switch to \
             workspace-write first.",
        );
        return false;
    }
    // The loop is where an unspecified project costs the most: it runs
    // unattended, so nobody is present to notice it building the wrong thing.
    // Refused outright rather than started and stopped at the first write.
    if let Some(note) = crate::spec::intake_note(ctx.workspace) {
        render::print_error(
            "The autonomous loop will not start on a project that is not specified yet.",
        );
        render::print_info(&note);
        return false;
    }
    true
}

/// The ledger a run works, whether it was derived just now, and the watch on
/// the interface's stop file — held for as long as the run lasts. `None` when
/// the run must not start or has nothing to do; why is said.
///
/// Called by [`run`], first.
fn ledger_for_run(ctx: &RunContext<'_>, cancel: &CancelToken) -> Option<(Ledger, bool, StopWatch)> {
    if !may_start(ctx) {
        return None;
    }
    // Built from the roadmap the owner approved, so the harness can only work
    // on what was actually planned. Planning is for a ledger that has just
    // been derived; one resumed from disk was planned on the run that made it.
    let (ledger, freshly_derived) = ledger_to_work(ctx.workspace)?;
    if ledger.tasks.is_empty() {
        render::print_info("Nothing to run: the roadmap has no open items.");
        return None;
    }
    render::print_info(&format!(
        "Running {} task(s). A stop lands at the next task.",
        ledger.tasks.len()
    ));
    let stop_watch = watch_for_stop(cancel, ctx.workspace);
    Some((ledger, freshly_derived, stop_watch))
}

/// The agents a run works with, and the settings they work under — every one
/// read as it stands NOW, so a change made in this session reaches the next
/// `/run`.
///
/// # Wired to
///
/// - **Resolved by** [`run`], once the ledger has work in it.
/// - **Read by** [`RunBackends::build`], [`LiveReplanner::for_run`] and
///   [`CliLaneWorkers::for_run`].
struct RunAgents {
    /// The lead: plans the ledger, and plans it again when a plan fails.
    lead: crate::agent::registry::DetectedAgent,
    /// The lead's sub-agent: works the tasks, here and in every lane.
    worker: crate::agent::registry::DetectedAgent,
    roles: crate::roles::RoleTable,
    guards: crate::guards::Guards,
}

impl RunAgents {
    /// Who leads and who works, the roles and the guards. `None` when no
    /// agent can lead; why is said.
    ///
    /// Called by [`run`].
    fn resolve(ctx: &RunContext<'_>) -> Option<Self> {
        // The lead by the choice as it stands NOW, so a `/agent` made in this
        // session applies from the next `/run`.
        let lead = match ctx
            .catalogue
            .lead(crate::agent::registry::preferred_agent().as_deref())
        {
            Ok(found) => found,
            Err(why) => {
                render::print_error(&why);
                return None;
            }
        };
        // THE AGENT THAT WORKS THE TASKS: the lead's sub-agent, read fresh for the
        // same reason.
        let sub = ctx.catalogue.sub_agent(
            &lead,
            crate::admin::AdminConfig::load_default()
                .sub_agent
                .as_deref(),
        );
        if let Some(note) = &sub.note {
            render::print_info(note);
        }
        let worker = sub.agent;
        // THE ROLES AS THEY STAND NOW: a `/roles` saved in this session must
        // reach the run. A file broken since startup is said out loud, and the
        // settings the session started with are used.
        let roles = match crate::admin::AdminConfig::load(ctx.data_dir) {
            Ok(fresh) => fresh.resolve_roles().value,
            Err(e) => {
                render::print_error(&e);
                ctx.admin.resolve_roles().value
            }
        };
        render::print_info(&format!(
            "Working the ledger with {} — its own loop, on your subscription. The check is \
             still run by this harness, in its own process.",
            worker.display_name
        ));
        // Defaulted rather than fatal HERE only because the worker loads the same
        // policy and refuses to delegate at all if it cannot be parsed.
        let guards = crate::guards::Guards::load(ctx.workspace).unwrap_or_default();
        Some(Self {
            lead,
            worker,
            roles,
            guards,
        })
    }
}

/// The backends the run's own thread drives: the worker's, the lead's when
/// it is a different agent, and a standby for every other installed agent.
///
/// # Wired to
///
/// - **Built by** [`run`], from [`RunAgents`].
/// - **Plans through** [`Self::planner`], in [`plan_ledger`].
/// - **Becomes the run's worker** in [`Self::into_worker`].
struct RunBackends {
    worker: Backend,
    lead_planner: Option<Backend>,
    standby: Vec<Backend>,
}

/// One agent backend, as the run holds it.
type Backend = Box<dyn crate::agent::AgentBackend>;

impl RunBackends {
    /// Every backend the run needs, built as the agents stand now.
    ///
    /// Called by [`run`], once the agents are resolved.
    fn build(ctx: &RunContext<'_>, agents: &RunAgents) -> Self {
        let workspace = ctx.workspace;
        // EVERYTHING THE SESSION KNOWS, ON THE RUN PATH TOO: the run backend was
        // once built without the system context, twice. See `run_backend`.
        let worker: Box<dyn crate::agent::AgentBackend> = super::route_backend(
            workspace,
            &agents.worker.id,
            &agents.worker.command,
            &agents.guards,
            ctx.system_prompt,
            ctx.env_filter,
        );
        // Planning is the lead's job; only a sub-agent other than the lead needs
        // a second backend for it.
        let lead_planner: Option<Box<dyn crate::agent::AgentBackend>> =
            (agents.worker.id != agents.lead.id).then(|| {
                super::route_backend(
                    workspace,
                    &agents.lead.id,
                    &agents.lead.command,
                    &agents.guards,
                    ctx.system_prompt,
                    ctx.env_filter,
                )
            });
        // Every OTHER installed agent is somewhere the work can move when this
        // one runs out of room: two subscriptions are two separate allowances.
        let standby: Vec<Box<dyn crate::agent::AgentBackend>> = ctx
            .fleet
            .agents()
            .iter()
            .filter(|a| a.id != agents.worker.id)
            .map(|other| {
                super::run_backend(
                    workspace,
                    &other.id,
                    &other.command,
                    &agents.guards,
                    ctx.system_prompt,
                )
            })
            .collect();
        if !standby.is_empty() {
            render::print_info(&format!(
                "{} standby agent(s) available if this one cannot do the work.",
                standby.len()
            ));
        }
        Self {
            worker,
            lead_planner,
            standby,
        }
    }

    /// The backend that plans: the lead's when it has its own, else the worker's.
    ///
    /// Called by [`run`] for [`plan_ledger`].
    fn planner(&mut self) -> &mut dyn crate::agent::AgentBackend {
        match self.lead_planner.as_mut() {
            Some(lead) => lead.as_mut(),
            None => self.worker.as_mut(),
        }
    }

    /// The run's own worker, its backends filed under the run's memory session,
    /// and the lead's planner — handed back so it is kept, unused, until the run
    /// ends, as it always was.
    ///
    /// Called by [`run`], after the planning pass.
    fn into_worker(
        mut self,
        ctx: &RunContext<'_>,
        roles: &crate::roles::RoleTable,
        session: &str,
    ) -> (
        crate::agent::worker::AgentBackendWorker<Backend>,
        Option<Backend>,
    ) {
        self.worker.set_memory_session(session);
        for other in &mut self.standby {
            other.set_memory_session(session);
        }
        // A protected write — the test a step writes first — is put to the owner
        // rather than refused, on the same switch as the worker's own channel.
        // Nobody is at a terminal during a run, so this is the only one to ask.
        if ctx.admin.plugins.owner {
            let owner: std::sync::Arc<dyn crate::hooks::engine::Prompter> =
                std::sync::Arc::new(crate::owner::OwnerPrompter::new(
                    Box::new(crate::runner::MemoryOwner::default()),
                    ctx.workspace,
                ));
            self.worker.set_owner(std::sync::Arc::clone(&owner));
            for other in &mut self.standby {
                other.set_owner(std::sync::Arc::clone(&owner));
            }
        }
        // The role table applies here too: the CLI takes `--model` per
        // invocation, so cheap work runs cheap.
        let worker = crate::agent::worker::AgentBackendWorker::in_workspace(
            self.worker,
            ctx.workspace.to_path_buf(),
        )
        .with_roles(roles.clone())
        .with_standby(self.standby)
        .with_extra_dirs(ctx.extra_dirs.to_vec())
        // Its OWN channel rather than the runner's, on the same switch.
        .with_owner(if ctx.admin.plugins.owner {
            Box::new(crate::runner::MemoryOwner::default())
        } else {
            Box::new(crate::runner::NoOwner) as Box<dyn crate::runner::OwnerChannel>
        });
        (worker, self.lead_planner)
    }
}

/// THE PLANNING PASS: a roadmap item is a thing to achieve, not a kind of
/// work, so it carries no role. One planning turn per open item, in `plan`
/// mode. An item the planner returns nothing for is KEPT AS IT WAS.
///
/// Called by [`run`], only for a ledger derived just now.
fn plan_ledger(
    ctx: &RunContext<'_>,
    ledger: &mut Ledger,
    planner: &mut dyn crate::agent::AgentBackend,
    roles: &crate::roles::RoleTable,
) {
    let workspace = ctx.workspace;
    // The operator's check, never named by the model.
    let verify = ctx
        .admin
        .resolve_verify(workspace)
        .map(|resolved| resolved.value)
        .unwrap_or_default();
    if verify.is_empty() {
        render::print_error(
            "No check is configured for this project, so no planned step can be proved \
             finished. Set one with INCODE_VERIFY_CMD or `verify_command` in admin.json.",
        );
    }
    let mut planned: Vec<crate::ledger::Task> = Vec::new();
    for task in &ledger.tasks {
        let steps = crate::plan::split(planner, roles, &task.title);
        if steps.is_empty() {
            planned.push(task.clone());
            continue;
        }
        render::print_info(&format!(
            "{} -> {} step(s): {}",
            task.roadmap_id,
            steps.len(),
            steps
                .iter()
                .map(|s| s.role.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        planned.extend(Ledger::from_plan(&task.roadmap_id, &steps, &verify, workspace).tasks);
    }
    ledger.tasks = planned;
}

/// The ledger to work, and whether it was derived just now (and so still
/// needs planning). `None` when there is nothing to work from; why is said.
fn ledger_to_work(workspace: &Path) -> Option<(Ledger, bool)> {
    match Ledger::load(workspace) {
        Ok(existing) if !existing.tasks.is_empty() => Some((existing, false)),
        Ok(_) => {
            // THE DOCUMENT ALREADY SAYS WHAT THIS IS. Only what it does not
            // cover is then asked for, and a brief a person wrote is never
            // overwritten. Asked as the QUESTION (`may_code`), not matched as
            // a shape.
            if !crate::brief::check(workspace).may_code() {
                match crate::brief::derive_from_docs(workspace) {
                    Ok(still_missing) if still_missing.is_empty() => {
                        render::print_info("Brief derived from this project's own documents.");
                    }
                    Ok(still_missing) => render::print_info(&format!(
                        "Derived what the documents cover; still missing: {}",
                        still_missing
                            .iter()
                            .map(|a| a.field())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                    // Not fatal: the gate below says what is missing.
                    Err(e) => crate::trace!("brief", "not derived: {e}"),
                }
            }
            match crate::brief::check(workspace) {
                crate::brief::Gate::Ready(brief) => {
                    Some((Ledger::from_roadmap(&brief.roadmap), true))
                }
                other => {
                    render::print_error(
                        &other
                            .intake_task()
                            .unwrap_or_else(|| "no roadmap to work from".to_string()),
                    );
                    None
                }
            }
        }
        Err(e) => {
            render::print_error(&e);
            None
        }
    }
}

/// Clear a stop left over from before, and watch the interface's stop file
/// for as long as the returned watch is held.
///
/// A Ctrl-C at the prompt, or a stop that ended an earlier run, left the
/// token set; without the reset this run would stop before its first task.
/// The interface's stop button is another process, so it writes a request and
/// this watches for it.
pub(crate) fn watch_for_stop(cancel: &CancelToken, workspace: &Path) -> StopWatch {
    cancel.reset();
    let watch = cancel.cancel_on_request(crate::runtime::cancel::stop_path_in(workspace));
    if watch.cleared_stale() {
        render::print_info(
            "An old stop request from the interface was cleared: it was made while nothing \
             was running.",
        );
    }
    watch
}

/// Who works a run, beside the ledger and the workspace.
pub(crate) struct Crew<'a> {
    pub(crate) worker: &'a mut dyn crate::runner::TaskWorker,
    pub(crate) owner: &'a mut dyn crate::runner::OwnerChannel,
    pub(crate) journal: &'a mut dyn crate::runner::WorkJournal,
    pub(crate) replanner: &'a mut dyn crate::runner::Replanner,
    pub(crate) goals: &'a mut dyn crate::runner::RoadmapGoals,
    pub(crate) lanes: &'a dyn crate::runner::LaneWorkers,
}

/// Work `ledger` until there is a reason to stop, polling `cancel` between
/// tasks. `progress` is told the ledger as it goes, and once more as it
/// stands at the end.
pub(crate) fn work(
    ledger: &mut Ledger,
    crew: Crew<'_>,
    workspace: &Path,
    budget: &mut crate::budget::Budget,
    cancel: &CancelToken,
    progress: &mut dyn Progress,
) -> crate::runner::RunOutcome {
    let outcome = crate::runner::run_watched(
        ledger,
        crew.worker,
        workspace,
        budget,
        &|| cancel.is_cancelled(),
        crew.owner,
        crew.journal,
        progress,
        crew.replanner,
        crew.goals,
        crew.lanes,
    );
    progress.ledger(ledger);
    outcome
}

/// What a run says once it has stopped. Always said, never summarised away:
/// a run that stops without saying why reads as one that crashed.
pub(crate) fn report(outcome: &crate::runner::RunOutcome, ledger: &Ledger, workspace: &Path) {
    render::print_info(&outcome.stop.message());
    render::print_info(&format!(
        "{} completed, {} attempt(s) made.",
        outcome.completed.len(),
        outcome.attempted
    ));
    // WHICH AGENT SPENT IT, with several models on several subscriptions.
    if let Some(report) = outcome.spend_report() {
        render::print_info(&report);
    }
    // WHAT ONE AGENT ASKED ANOTHER AND NOBODY ANSWERED: each is a guess
    // somebody made, and only safe if it is seen afterwards.
    let unanswered = crate::runner::unanswered_questions(workspace);
    if !unanswered.is_empty() {
        render::print_info(&format!(
            "{} question(s) between agents went unanswered — each one is a guess somebody \
             made:",
            unanswered.len()
        ));
        for question in &unanswered {
            outln!("  {question}");
        }
    }
    outln!("{}", ledger.summary());
}

/// Draws the ledger's progress while a run works, and hands it on to a caller
/// that presents the run itself.
///
/// Only for a run: `/run` has a plan to measure against, and an interactive
/// turn does not. A bar that invented a denominator would be a number the
/// reader could not use.
struct RunWatcher<'a, 'w> {
    status: &'a render::LiveStatus,
    width: usize,
    also: Option<&'w mut dyn Progress>,
}

impl<'a, 'w> RunWatcher<'a, 'w> {
    /// The bar on `status`, handing on to whoever `watch` names.
    ///
    /// Called by [`run`], once the status line is up.
    fn new(status: &'a render::LiveStatus, watch: Watch<'w>) -> Self {
        Self {
            status,
            width: crate::cli::activity::terminal_width(),
            also: match watch {
                Watch::By(progress) => Some(progress),
                Watch::Terminal => None,
            },
        }
    }
}

impl Progress for RunWatcher<'_, '_> {
    fn advanced(&mut self, done: usize, total: usize) {
        let snapshot = crate::cli::activity::Status {
            activity: crate::cli::activity::Activity::THINKING,
            elapsed: 0,
            tokens: 0,
            progress: Some((done, total)),
        };
        self.status
            .set(&snapshot.render(render::FRAME_SLOT, self.width));
        if let Some(also) = self.also.as_mut() {
            also.advanced(done, total);
        }
    }

    fn ledger(&mut self, ledger: &Ledger) {
        if let Some(also) = self.also.as_mut() {
            also.ledger(ledger);
        }
    }

    fn watch(&self) -> Option<std::sync::Arc<dyn crate::watch::Watch>> {
        self.also.as_ref().and_then(|also| also.watch())
    }
}

/// Plans a roadmap item again after its first plan failed.
///
/// # Wired to
///
/// - **Implements** [`crate::runner::Replanner`]; called from the run loop in
///   `runner::run_watched` when a task uses up its attempts.
/// - **Calls** [`crate::plan::split`] and [`crate::ledger::Ledger::from_plan`] —
///   the SAME two calls that build the first plan, so a re-plan cannot drift
///   into producing a different shape of task than the original.
///
/// # Why it owns a backend instead of borrowing the run's
///
/// The worker owns the run's backend for the whole loop. Lending it out would
/// mean handing the same `&mut` to two places, so this keeps its own — which is
/// affordable because a planning turn is ONE turn, and it is the cheap half of
/// the split by design: the planner reads no files and writes no code.
struct LiveReplanner {
    /// The tree this project lives in — the key its lessons are filed under.
    workspace: PathBuf,
    backend: Box<dyn crate::agent::AgentBackend>,
    roles: crate::roles::RoleTable,
    /// The check every re-planned step is judged by — the operator's, never the
    /// model's, for the same reason as the first plan.
    verify: Vec<String>,
    /// Roadmap id to its title, from the brief. A re-plan needs the GOAL, not
    /// the failed task's wording: planning against the task that just failed
    /// would inherit the framing that failed with it.
    titles: std::collections::HashMap<String, String>,
}

impl LiveReplanner {
    /// A SECOND OPINION WHEN A GOAL DEFEATS ITS FIRST PLAN, bounded at
    /// `runner::MAX_REPLANS`. Its own backend: the worker holds the run's.
    ///
    /// Called by [`run`]; used only when the brief has roadmap titles to plan
    /// against.
    fn for_run(ctx: &RunContext<'_>, agents: &RunAgents) -> Self {
        let workspace = ctx.workspace;
        let titles: std::collections::HashMap<String, String> = match crate::brief::check(workspace)
        {
            crate::brief::Gate::Ready(brief) => brief
                .roadmap
                .iter()
                .map(|item| (item.id.clone(), item.title.clone()))
                .collect(),
            // No roadmap to plan against means no second opinion worth having.
            _ => std::collections::HashMap::new(),
        };
        Self {
            workspace: workspace.to_path_buf(),
            // THE SAME CONSTRUCTOR THE RUN ITSELF USES: routing by id is
            // `run_backend`'s whole job.
            backend: super::run_backend(
                workspace,
                &agents.lead.id,
                &agents.lead.command,
                &agents.guards,
                ctx.system_prompt,
            ),
            roles: agents.roles.clone(),
            verify: ctx
                .admin
                .resolve_verify(workspace)
                .map(|resolved| resolved.value)
                .unwrap_or_default(),
            titles,
        }
    }
}

impl crate::runner::Replanner for LiveReplanner {
    /// Write the mistake down, whether or not a re-plan follows.
    ///
    /// A store that will not take it must NOT stop the run: losing the note
    /// costs the next run some context, losing the run costs the owner the work.
    fn failed(&mut self, roadmap_id: &str, why: &str) {
        match crate::lessons::record(&self.workspace, roadmap_id, why) {
            Ok(true) => crate::trace!("lessons", "{roadmap_id}: written down"),
            Ok(false) => crate::trace!("lessons", "{roadmap_id}: already known"),
            Err(e) => crate::trace!("lessons", "{roadmap_id}: not written: {e}"),
        }
    }

    fn replan(&mut self, roadmap_id: &str, why: &str) -> Vec<crate::ledger::Task> {
        // Read back from the STORE rather than from this run: another session
        // may have learned something about this project since, and a shared
        // memory that is only ever written is not shared.
        let lessons = crate::lessons::known(&self.workspace);

        // An item with no title in the brief is one this run cannot describe,
        // and planning against an id alone would produce noise.
        let Some(title) = self.titles.get(roadmap_id).cloned() else {
            crate::trace!("plan", "{roadmap_id}: no roadmap title, not re-planning");
            return Vec::new();
        };

        let mut instruction = format!(
            "{title}\n\nA previous plan for this was attempted and failed. What \
             went wrong, as an OBSERVATION rather than an instruction:\n{}\n\n\
             Plan a DIFFERENT approach. Repeating the same shape of plan spends \
             the owner's remaining attempts on the thing that already failed.",
            why.trim()
        );
        if let Some(block) = crate::lessons::prompt_block(&lessons) {
            instruction.push_str("\n\n");
            instruction.push_str(&block);
        }

        let steps = crate::plan::split(self.backend.as_mut(), &self.roles, &instruction);
        if steps.is_empty() {
            return Vec::new();
        }
        Ledger::from_plan(roadmap_id, &steps, &self.verify, &self.workspace).tasks
    }
}
