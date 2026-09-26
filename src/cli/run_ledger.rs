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

use crate::cli::render;
use crate::ledger::Ledger;
use crate::outln;
use crate::permissions::PermissionMode;
use crate::runner::Progress;
use crate::runtime::cancel::{CancelToken, StopWatch};

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
    if ctx.permission_mode == PermissionMode::ReadOnly {
        render::print_error(
            "The ledger run changes files, and this session is read-only. Switch to \
             workspace-write first.",
        );
        return;
    }
    // The loop is where an unspecified project costs the most: it runs
    // unattended, so nobody is present to notice it building the wrong thing.
    // Refused outright rather than started and stopped at the first write.
    if let Some(note) = crate::spec::intake_note(workspace) {
        render::print_error(
            "The autonomous loop will not start on a project that is not specified yet.",
        );
        render::print_info(&note);
        return;
    }
    // Built from the roadmap the owner approved, so the harness can only work
    // on what was actually planned. Planning is for a ledger that has just
    // been derived; one resumed from disk was planned on the run that made it.
    let Some((mut ledger, freshly_derived)) = ledger_to_work(workspace) else {
        return;
    };
    if ledger.tasks.is_empty() {
        render::print_info("Nothing to run: the roadmap has no open items.");
        return;
    }
    render::print_info(&format!(
        "Running {} task(s). A stop lands at the next task.",
        ledger.tasks.len()
    ));
    let _stop_watch = watch_for_stop(cancel, workspace);

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
    // The lead by the choice as it stands NOW, so a `/agent` made in this
    // session applies from the next `/run`.
    let agent = match ctx
        .catalogue
        .lead(crate::agent::registry::preferred_agent().as_deref())
    {
        Ok(found) => found,
        Err(why) => {
            render::print_error(&why);
            return;
        }
    };
    // THE AGENT THAT WORKS THE TASKS: the lead's sub-agent, read fresh for the
    // same reason.
    let sub = ctx.catalogue.sub_agent(
        &agent,
        crate::admin::AdminConfig::load_default()
            .sub_agent
            .as_deref(),
    );
    if let Some(note) = &sub.note {
        render::print_info(note);
    }
    let worker_agent = sub.agent;
    // THE ROLES AS THEY STAND NOW: a `/roles` saved in this session must
    // reach the run. A file broken since startup is said out loud, and the
    // settings the session started with are used.
    let run_roles = match crate::admin::AdminConfig::load(ctx.data_dir) {
        Ok(fresh) => fresh.resolve_roles().value,
        Err(e) => {
            render::print_error(&e);
            ctx.admin.resolve_roles().value
        }
    };
    render::print_info(&format!(
        "Working the ledger with {} — its own loop, on your subscription. The check is \
         still run by this harness, in its own process.",
        worker_agent.display_name
    ));
    // Defaulted rather than fatal HERE only because the worker loads the same
    // policy and refuses to delegate at all if it cannot be parsed.
    let run_guards = crate::guards::Guards::load(workspace).unwrap_or_default();
    // EVERYTHING THE SESSION KNOWS, ON THE RUN PATH TOO: the run backend was
    // once built without the system context, twice. See `run_backend`.
    let mut backend: Box<dyn crate::agent::AgentBackend> = super::route_backend(
        workspace,
        &worker_agent.id,
        &worker_agent.command,
        &run_guards,
        ctx.system_prompt,
        ctx.env_filter,
    );
    // Planning is the lead's job; only a sub-agent other than the lead needs
    // a second backend for it.
    let mut lead_planner: Option<Box<dyn crate::agent::AgentBackend>> =
        (worker_agent.id != agent.id).then(|| {
            super::route_backend(
                workspace,
                &agent.id,
                &agent.command,
                &run_guards,
                ctx.system_prompt,
                ctx.env_filter,
            )
        });
    // Every OTHER installed agent is somewhere the work can move when this
    // one runs out of room: two subscriptions are two separate allowances.
    let mut standby: Vec<Box<dyn crate::agent::AgentBackend>> = ctx
        .fleet
        .agents()
        .iter()
        .filter(|a| a.id != worker_agent.id)
        .map(|other| {
            super::run_backend(
                workspace,
                &other.id,
                &other.command,
                &run_guards,
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
    // THE PLANNING PASS: a roadmap item is a thing to achieve, not a kind of
    // work, so it carries no role. One planning turn per open item, in `plan`
    // mode. An item the planner returns nothing for is KEPT AS IT WAS.
    if freshly_derived {
        // The operator's check, never named by the model.
        let verify = ctx
            .admin
            .resolve_verify(workspace)
            .map(|resolved| resolved.value)
            .unwrap_or_default();
        if verify.is_empty() {
            render::print_error(
                "No check is configured for this project, so no planned step can be proved \
                 finished. Set one with INCORD_VERIFY_CMD or `verify_command` in admin.json.",
            );
        }
        let mut planned: Vec<crate::ledger::Task> = Vec::new();
        for task in &ledger.tasks {
            let planner = match lead_planner.as_mut() {
                Some(lead) => lead.as_mut(),
                None => backend.as_mut(),
            };
            let steps = crate::plan::split(planner, &run_roles, &task.title);
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

    // THE WORKER IS A MEMORY SESSION OF ITS OWN, as a person's session is:
    // its agents' calls are filed under its id, and the engine hears it
    // start, stop after each task, and end (`run_memory`). Named after the
    // planning pass, which is the lead's work, not the worker's.
    let engine = crate::hooks::engine::Engine::locate();
    let worker_session = uuid::Uuid::new_v4().to_string();
    backend.set_memory_session(&worker_session);
    for other in &mut standby {
        other.set_memory_session(&worker_session);
    }
    // The role table applies here too: the CLI takes `--model` per
    // invocation, so cheap work runs cheap.
    let worker =
        crate::agent::worker::AgentBackendWorker::in_workspace(backend, workspace.to_path_buf())
            .with_roles(run_roles.clone())
            .with_standby(standby)
            .with_extra_dirs(ctx.extra_dirs.to_vec())
            // Its OWN channel rather than the runner's, on the same switch.
            .with_owner(if ctx.admin.plugins.owner {
                Box::new(crate::runner::MemoryOwner::default())
            } else {
                Box::new(crate::runner::NoOwner) as Box<dyn crate::runner::OwnerChannel>
            });
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
    let mut worker = super::run_memory::Remembered::new(
        worker,
        crate::hooks::memory::MemoryHooks::new(engine.clone(), workspace, &worker_session),
    );
    let mut watcher = RunWatcher {
        status: &run_status,
        width: crate::cli::activity::terminal_width(),
        also: match watch {
            Watch::By(progress) => Some(progress),
            Watch::Terminal => None,
        },
    };
    // A SECOND OPINION WHEN A GOAL DEFEATS ITS FIRST PLAN, bounded at
    // `runner::MAX_REPLANS`. Its own backend: the worker holds the run's.
    let titles: std::collections::HashMap<String, String> = match crate::brief::check(workspace) {
        crate::brief::Gate::Ready(brief) => brief
            .roadmap
            .iter()
            .map(|item| (item.id.clone(), item.title.clone()))
            .collect(),
        // No roadmap to plan against means no second opinion worth having.
        _ => std::collections::HashMap::new(),
    };
    let mut no_replan = crate::runner::NoReplan;
    let mut live_replan = LiveReplanner {
        workspace: workspace.to_path_buf(),
        // THE SAME CONSTRUCTOR THE RUN ITSELF USES: routing by id is
        // `run_backend`'s whole job.
        backend: super::run_backend(
            workspace,
            &agent.id,
            &agent.command,
            &run_guards,
            ctx.system_prompt,
        ),
        roles: run_roles.clone(),
        verify: ctx
            .admin
            .resolve_verify(workspace)
            .map(|resolved| resolved.value)
            .unwrap_or_default(),
        titles,
    };
    let replanner: &mut dyn crate::runner::Replanner = if live_replan.titles.is_empty() {
        &mut no_replan
    } else {
        &mut live_replan
    };
    let lanes = CliLaneWorkers {
        // Lanes work tasks, so they run the sub-agent.
        agent_id: worker_agent.id.clone(),
        command: worker_agent.command.clone(),
        guards: run_guards.clone(),
        system_prompt: ctx.system_prompt.to_vec(),
        roles: run_roles,
        extra_dirs: ctx.extra_dirs.to_vec(),
        desktop: super::desktop_enabled_from_env(),
        permission_mode: ctx.permission_mode,
        watch: followed,
        cancel: cancel.clone(),
        opened: std::sync::atomic::AtomicUsize::new(0),
        engine,
    };
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
}

impl crate::runner::LaneWorkers for CliLaneWorkers {
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
}

impl crate::runner::TaskWorker for LaneAgent {
    fn work(&mut self, task: &crate::ledger::Task) -> Result<String, String> {
        // Built HERE, on the lane's own thread, because the backend cannot
        // cross a thread boundary. Every argument is the one the sequential
        // path passes, except the workspace — which is the whole point.
        let backend = super::run_backend_with_desktop(
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
        let mut worker = super::run_memory::Remembered::new(
            agent,
            crate::hooks::memory::MemoryHooks::new(self.engine.clone(), &self.workspace, &session),
        );

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
