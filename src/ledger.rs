//! The task ledger — what is being worked on, and what it took to finish it.
//!
//! The roadmap says what *should* happen; it lives in the brief and belongs to
//! the proxy. This says what *happened to it*: which task is in hand, how many
//! attempts it has had, and the evidence that closed it. Those are execution
//! facts, and putting them in the brief would push them into a prompt that rides
//! every approval.
//!
//! So every task here names the roadmap item it implements. One list with an
//! execution record attached — not a second list that can drift from the first.
//!
//! # The law
//!
//! **A task reaches `Done` only with [`Verified`] proof.** [`Ledger::complete`]
//! is the single path into that state and cannot be called without a command
//! having exited zero. No amount of confidence from any model reaches it. This
//! is not caution: both autonomous loops built here failed by letting an
//! automated approver's verdict be recorded as verification.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::verify::Verified;

/// Where the ledger lives, relative to the workspace.
pub const LEDGER_DIR: &str = ".incord";
pub const LEDGER_FILE: &str = "ledger.json";

/// How many times a task may be attempted before it is set aside.
///
/// Two, then stop. A third attempt on an unchanged hypothesis is how a loop
/// burns a night, and the standing rule against thrashing says the same thing.
pub const MAX_ATTEMPTS: u32 = 2;

/// Where a task is in its life.
///
/// Seven, and no more. Anything that looks like an eighth is an attempt counter
/// or an evidence record wearing a state's clothes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// In the ledger; its dependencies are not all done yet.
    Pending,
    /// Eligible for pickup.
    Ready,
    /// Claimed by a runner.
    Running,
    /// An owner decision is outstanding.
    Blocked,
    /// Verified by a passing command. Terminal.
    Done,
    /// Out of attempts; will not be retried without a person.
    Abandoned,
    /// The owner dismissed it.
    Cancelled,
}

impl TaskState {
    #[must_use]
    pub fn is_open(self) -> bool {
        !matches!(self, Self::Done | Self::Abandoned | Self::Cancelled)
    }
}

/// One unit of work, and what has happened to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// The roadmap item this implements. A task with no roadmap behind it is
    /// scope that arrived without a decision.
    pub roadmap_id: String,
    pub title: String,
    #[serde(default = "default_state")]
    pub state: TaskState,
    /// The command that decides whether this is finished.
    ///
    /// Empty means nothing can prove it, and [`Ledger::complete`] will refuse —
    /// see [`Task::can_be_verified`].
    #[serde(default)]
    pub verify: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub attempts: u32,
    /// What the passing command actually said, kept with the task it closed.
    #[serde(default)]
    pub evidence: Option<String>,
    /// Why it is where it is, in whoever's words moved it.
    #[serde(default)]
    pub note: String,
    /// The contract names this task is built against.
    ///
    /// Written at PLANNING time, not discovered at run time: a task that knows
    /// which shapes it must honour can be checked against them, and a task that
    /// does not is where `{user_id}` meets `{userId}`.
    #[serde(default)]
    pub contracts: Vec<String>,
    /// How expensive this work is expected to be — the role it routes to.
    ///
    /// A plan decision rather than a runtime guess. Deciding at run time means
    /// deciding with the least information anyone will ever have about the task,
    /// while the planner had the whole roadmap in front of it.
    #[serde(default)]
    pub est_tier: String,
    /// Who did the work, and what they actually ran on.
    ///
    /// The AGENT is the identity — it chooses the model, and the model can
    /// change underneath it when a fallback fires. Recording the model as the
    /// worker would split one agent's history the first time that happened.
    /// See [`crate::identity`].
    #[serde(default)]
    pub worked_by: String,
    /// What the owner told the agent to do differently.
    ///
    /// Separate from `note`, which is a record of everything that has moved this
    /// task — failures, questions, verdicts. This is the one line the NEXT
    /// attempt has to follow, and feeding a failure message back to the agent as
    /// if it were an instruction would be worse than sending nothing.
    #[serde(default)]
    pub owner_note: String,
    /// The branch holding work from an earlier attempt that could not merge.
    ///
    /// Set when a lane's change conflicted with another lane's. That work is
    /// NOT lost — it is committed on the branch — but it is not in the tree
    /// either, so the task goes back to the queue. Carrying the branch name
    /// lets the next attempt be told to reconcile the change it already wrote
    /// instead of starting from nothing, which is the difference between
    /// paying for that work once and paying for it twice.
    ///
    /// Unlike `note`, this is not a failure message dressed up as an
    /// instruction: it is a FACT about where the code is. `None` means there
    /// is no such branch, never "the branch is gone".
    #[serde(default)]
    pub unmerged_branch: Option<String>,
    /// The file this task must write, for work that produces information
    /// rather than code.
    ///
    /// Fixed at planning time, before the agent runs, because a deliverable
    /// the agent chooses the location of is a deliverable nothing can check.
    /// `None` is the ordinary case: a task that changes code is judged by the
    /// project's own command. See [`crate::findings`].
    #[serde(default)]
    pub deliverable: Option<String>,
}

fn default_state() -> TaskState {
    TaskState::Pending
}

impl Task {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        roadmap_id: impl Into<String>,
        title: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            roadmap_id: roadmap_id.into(),
            title: title.into(),
            state: TaskState::Pending,
            verify: Vec::new(),
            depends_on: Vec::new(),
            attempts: 0,
            evidence: None,
            note: String::new(),
            contracts: Vec::new(),
            est_tier: String::new(),
            worked_by: String::new(),
            owner_note: String::new(),
            unmerged_branch: None,
            deliverable: None,
        }
    }

    #[must_use]
    pub fn with_verify(mut self, argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.verify = argv.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn with_dependencies(mut self, ids: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.depends_on = ids.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn with_contracts(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.contracts = names.into_iter().map(Into::into).collect();
        self
    }

    /// Make this task's deliverable a findings file, and its check the
    /// command that judges one.
    ///
    /// Sets both together on purpose. A deliverable with no check is a
    /// request nobody enforces, and a findings check with no deliverable
    /// names a path the agent was never told to write.
    #[must_use]
    pub fn delivering_findings(mut self, path: impl Into<String>) -> Self {
        let path = path.into();
        self.verify = vec![
            crate::verify::SELF_PROGRAM.to_string(),
            "--check-findings".to_string(),
            path.clone(),
        ];
        self.deliverable = Some(path);
        self
    }

    /// Assign this task to a role at planning time.
    #[must_use]
    pub fn with_tier(mut self, tier: impl Into<String>) -> Self {
        self.est_tier = tier.into();
        self
    }

    /// The role this task should run on, or `None` to use the session default.
    ///
    /// An unknown tier is NOT silently ignored: it returns `None` and the caller
    /// falls back to the session model, which is the safe direction — a typo in
    /// a plan should cost money, never correctness.
    #[must_use]
    pub fn role(&self) -> Option<&str> {
        let tier = self.est_tier.trim();
        crate::roles::KNOWN_ROLES
            .iter()
            .find(|known| known.eq_ignore_ascii_case(tier))
            .copied()
    }

    /// Whether anything could ever prove this finished.
    ///
    /// A task nobody can check is a decision for a person, not a chore to mark
    /// done — so it is sent to the owner rather than quietly completed.
    #[must_use]
    pub fn can_be_verified(&self) -> bool {
        !self.verify.is_empty()
    }

    #[must_use]
    pub fn attempts_left(&self) -> u32 {
        MAX_ATTEMPTS.saturating_sub(self.attempts)
    }

    /// Extract the 8-hex character worker fingerprint from `worked_by`, if one was recorded.
    #[must_use]
    pub fn agent_fingerprint(&self) -> Option<&str> {
        crate::identity::Attribution::extract_fingerprint(&self.worked_by)
    }

    /// Extract the instance id (`agent-...`) from `worked_by`, if one was recorded.
    #[must_use]
    pub fn agent_instance(&self) -> Option<&str> {
        crate::identity::Attribution::extract_instance(&self.worked_by)
    }

    /// Whether this task was attempted or finished by the given worker query.
    ///
    /// Matches on instance ID (`agent-...`), fingerprint (8 hex chars), or worker label.
    #[must_use]
    pub fn is_worked_by(&self, worker_query: &str) -> bool {
        let q = worker_query.trim();
        if q.is_empty() {
            return false;
        }
        if let Some(instance) = self.agent_instance() {
            if instance == q {
                return true;
            }
        }
        if let Some(fp) = self.agent_fingerprint() {
            if fp == q {
                return true;
            }
        }
        self.worked_by.contains(q)
    }
}

/// The ordered plan and its execution state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub tasks: Vec<Task>,
}

impl Ledger {
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: 1,
            tasks: Vec::new(),
        }
    }

    #[must_use]
    pub fn path_for(workspace: &Path) -> PathBuf {
        workspace.join(LEDGER_DIR).join(LEDGER_FILE)
    }

    /// Read the ledger, or an empty one when there is none yet.
    pub fn load(workspace: &Path) -> Result<Self, String> {
        let path = Self::path_for(workspace);
        if !path.exists() {
            return Ok(Self::new());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{} is not a ledger: {e}", path.display()))
    }

    /// Write the ledger back.
    ///
    /// Pretty-printed because a person reads this in a diff, and the task list
    /// is an ordered array — order is preserved, so a save produces the same
    /// bytes for the same state rather than a reshuffle.
    pub fn save(&self, workspace: &Path) -> Result<(), String> {
        let path = Self::path_for(workspace);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        let text =
            serde_json::to_string_pretty(self).map_err(|e| format!("could not encode: {e}"))?;
        std::fs::write(&path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// All tasks that were worked by or attempted by a given worker query
    /// (instance ID, fingerprint, or worker label).
    #[must_use]
    pub fn tasks_by_worker(&self, worker_query: &str) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|t| t.is_worked_by(worker_query))
            .collect()
    }

    /// All tasks worked by a specific worker instance (`agent-...`).
    #[must_use]
    pub fn tasks_by_instance(&self, instance: &str) -> Vec<&Task> {
        let instance = instance.trim();
        if instance.is_empty() {
            return Vec::new();
        }
        self.tasks
            .iter()
            .filter(|t| t.agent_instance() == Some(instance))
            .collect()
    }

    /// All tasks worked by a specific agent fingerprint.
    #[must_use]
    pub fn tasks_by_fingerprint(&self, fingerprint: &str) -> Vec<&Task> {
        let fingerprint = fingerprint.trim();
        if fingerprint.is_empty() {
            return Vec::new();
        }
        self.tasks
            .iter()
            .filter(|t| t.agent_fingerprint() == Some(fingerprint))
            .collect()
    }

    /// Mutable access for the runner, which records who attempted a task.
    ///
    /// Deliberately narrow in name: state transitions go through the methods
    /// below, which is what keeps the one edge into `Done` the only edge.
    pub fn get_mut_public(&mut self, id: &str) -> Option<&mut Task> {
        self.get_mut(id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    #[must_use]
    pub fn is_done(&self, id: &str) -> bool {
        self.get(id).is_some_and(|t| t.state == TaskState::Done)
    }

    /// Move every `Pending` task whose dependencies are all done to `Ready`.
    ///
    /// Called before picking work so a task unblocked by the previous one is
    /// eligible immediately, rather than on some later pass.
    pub fn refresh_ready(&mut self) {
        let satisfied: Vec<String> = self
            .tasks
            .iter()
            .filter(|t| t.state == TaskState::Done)
            .map(|t| t.id.clone())
            .collect();
        for task in &mut self.tasks {
            if task.state == TaskState::Pending
                && task.depends_on.iter().all(|d| satisfied.contains(d))
            {
                task.state = TaskState::Ready;
            }
        }
    }

    /// Tasks that can never become ready, and what is holding each one.
    ///
    /// A dependency reaches `Done` or it never does: `Abandoned` and `Cancelled`
    /// are terminal, and a task named by nobody was never in the ledger at all.
    /// Waiting on any of those is waiting forever, and a queue that reports such
    /// a task as merely "waiting" describes it exactly as it describes the ones
    /// a person is about to answer — so the two look alike right up until the
    /// person never comes.
    #[must_use]
    pub fn dead_dependencies(&self) -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        for task in &self.tasks {
            if !task.state.is_open() {
                continue;
            }
            let dead: Vec<String> = task
                .depends_on
                .iter()
                .filter_map(|id| match self.get(id) {
                    None => Some(format!("{id} (no such task)")),
                    Some(dep) => match dep.state {
                        TaskState::Abandoned => Some(format!("{id} (abandoned)")),
                        TaskState::Cancelled => Some(format!("{id} (cancelled)")),
                        _ => None,
                    },
                })
                .collect();
            if !dead.is_empty() {
                out.push((task.id.clone(), dead));
            }
        }
        out
    }

    /// The next task to work on, in ledger order.
    #[must_use]
    pub fn next_ready(&self) -> Option<&Task> {
        self.tasks.iter().find(|t| t.state == TaskState::Ready)
    }

    /// Whether any work remains that a runner could pick up or is holding.
    #[must_use]
    pub fn has_open_work(&self) -> bool {
        self.tasks.iter().any(|t| t.state.is_open())
    }

    /// Open tasks that no runner can pick up, by id.
    ///
    /// `Blocked` waits on a person; `Pending` waits on a dependency that may
    /// never land, because a dependency set aside or cancelled never becomes
    /// `Done` and so never releases what follows it. Both are work that is not
    /// finished and not moving, and a run that ends with either must say so.
    #[must_use]
    pub fn waiting_ids(&self) -> Vec<String> {
        self.tasks
            .iter()
            .filter(|t| matches!(t.state, TaskState::Blocked | TaskState::Pending))
            .map(|t| t.id.clone())
            .collect()
    }

    /// Claim a task for work.
    pub fn start(&mut self, id: &str) -> Result<(), String> {
        let task = self
            .get_mut(id)
            .ok_or_else(|| format!("no task `{id}` in the ledger"))?;
        if task.state != TaskState::Ready {
            return Err(format!(
                "task `{id}` is {:?}, not ready to start",
                task.state
            ));
        }
        task.state = TaskState::Running;
        Ok(())
    }

    /// **The only path to `Done`.**
    ///
    /// Demands [`Verified`], which cannot be constructed without a command that
    /// exited zero — so no caller can complete a task on the strength of a
    /// report, however confident. The evidence is stored with the task, because
    /// a completion nobody can inspect later is a claim again.
    pub fn complete(&mut self, id: &str, proof: &Verified) -> Result<(), String> {
        let task = self
            .get_mut(id)
            .ok_or_else(|| format!("no task `{id}` in the ledger"))?;
        task.state = TaskState::Done;
        task.evidence = Some(proof.evidence().summary());
        task.note = "verified".to_string();
        // The unmerged branch is history now: the tree on disk passed the
        // check, so whatever that branch held is either in it or superseded by
        // it. Keeping the name would point a later reader — or a later
        // attempt — at a branch that no longer describes anything.
        task.unmerged_branch = None;
        Ok(())
    }

    /// Record a failed attempt and decide what happens next.
    ///
    /// Back to `Ready` while attempts remain, `Abandoned` once they do not.
    /// There is deliberately no `Failed` state: a failure is a counter and a
    /// note, and a state nothing ever observes is not a state.
    pub fn record_failure(
        &mut self,
        id: &str,
        why: impl Into<String>,
    ) -> Result<TaskState, String> {
        let task = self
            .get_mut(id)
            .ok_or_else(|| format!("no task `{id}` in the ledger"))?;
        task.attempts = task.attempts.saturating_add(1);
        task.note = why.into();
        task.state = if task.attempts >= MAX_ATTEMPTS {
            TaskState::Abandoned
        } else {
            TaskState::Ready
        };
        Ok(task.state)
    }

    /// Hand a task to the owner.
    pub fn block(&mut self, id: &str, question: impl Into<String>) -> Result<(), String> {
        let task = self
            .get_mut(id)
            .ok_or_else(|| format!("no task `{id}` in the ledger"))?;
        task.state = TaskState::Blocked;
        task.note = question.into();
        Ok(())
    }

    /// A blocked task the owner answered with an instruction: try again, doing
    /// it the way they said.
    ///
    /// The attempt counter is deliberately NOT reset. A task the owner has
    /// redirected twice is still a task that has failed twice, and clearing the
    /// count would let a pair of redirections run forever.
    /// `instruction` is what the owner said to do DIFFERENTLY, carried separately
    /// so the next attempt can be told it. A verdict recorded only in the log is
    /// a verdict the agent never hears, which makes the whole exchange theatre.
    pub fn unblock(
        &mut self,
        id: &str,
        note: impl Into<String>,
        instruction: impl Into<String>,
    ) -> Result<(), String> {
        let task = self
            .get_mut(id)
            .ok_or_else(|| format!("no task `{id}` in the ledger"))?;
        if task.state != TaskState::Blocked {
            return Err(format!(
                "`{id}` is not blocked, so there is nothing to answer"
            ));
        }
        task.state = TaskState::Ready;
        task.note = note.into();
        let instruction = instruction.into();
        // An empty instruction leaves the previous one standing rather than
        // wiping it: the owner answering twice must not erase what they said the
        // first time.
        if !instruction.trim().is_empty() {
            if task.verify.is_empty() {
                let cmd = crate::verify::parse_command(&instruction);
                if !cmd.is_empty() {
                    task.verify = cmd;
                }
            }
            task.owner_note = instruction;
        }
        Ok(())
    }

    /// Assign a verify command to a task.
    pub fn set_verify(
        &mut self,
        id: &str,
        argv: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<(), String> {
        let task = self
            .get_mut(id)
            .ok_or_else(|| format!("no task `{id}` in the ledger"))?;
        task.verify = argv.into_iter().map(Into::into).collect();
        Ok(())
    }

    /// The owner dropped it.
    pub fn cancel(&mut self, id: &str, why: impl Into<String>) -> Result<(), String> {
        let task = self
            .get_mut(id)
            .ok_or_else(|| format!("no task `{id}` in the ledger"))?;
        task.state = TaskState::Cancelled;
        task.note = why.into();
        Ok(())
    }

    /// Out of the queue without being finished and without being dropped.
    ///
    /// This is what an owner's *approval* means on a task nothing can check:
    /// stop asking, do not pretend it passed. It is emphatically NOT `Done` —
    /// that edge takes proof and only proof — and the distinction is the entire
    /// reason both earlier autonomous loops are worth not repeating.
    pub fn set_aside(&mut self, id: &str, why: impl Into<String>) -> Result<(), String> {
        let task = self
            .get_mut(id)
            .ok_or_else(|| format!("no task `{id}` in the ledger"))?;
        task.state = TaskState::Abandoned;
        task.note = why.into();
        Ok(())
    }

    /// Swap one roadmap item's unfinished tasks for a fresh plan.
    ///
    /// # Wired to
    ///
    /// Called only by [`crate::runner`] when a task has used its attempts and a
    /// [`crate::runner::Replanner`] offers a different approach.
    ///
    /// # VERIFIED WORK IS NEVER DISCARDED
    ///
    /// `Done` tasks for the item stay exactly as they are, proof and all. A
    /// re-plan means "the rest of this approach was wrong", never "the part that
    /// already passed its check did not happen" — and a loop that could delete
    /// its own evidence would be able to launder a failure into a fresh start,
    /// which is the one thing the completion law exists to prevent.
    ///
    /// Cancelled tasks stay too: the owner dismissed those, and a machine must
    /// not reopen a decision a person made.
    ///
    /// # A RE-PLAN NEVER MAKES A PROVABLE ITEM UNPROVABLE
    ///
    /// The check travels with the item, not with the plan. A re-planner builds
    /// its tasks from whatever check the operator's settings resolve to NOW —
    /// and MEASURED live, that resolved to nothing on a machine whose ledger
    /// already carried `node --check game.js`: the replacement tasks came back
    /// with no check at all and were set aside as unprovable, so a run that had
    /// just recovered from an agent failure lost the item to a second, quieter
    /// one. The old tasks are about to be dropped, so this is the last moment
    /// their check exists; a fresh task that names no check of its own inherits
    /// it here.
    ///
    /// A fresh task that DOES name one keeps it: a re-planner with a better
    /// check for a different approach must be able to say so.
    ///
    /// Returns how many unfinished tasks were replaced.
    pub fn replan_item(&mut self, roadmap_id: &str, fresh: Vec<Task>) -> usize {
        // Read before the retain below drops the tasks that hold it.
        let inherited: Vec<String> = self
            .tasks
            .iter()
            .find(|task| task.roadmap_id == roadmap_id && !task.verify.is_empty())
            .map(|task| task.verify.clone())
            .unwrap_or_default();

        let before = self.tasks.len();
        self.tasks.retain(|task| {
            task.roadmap_id != roadmap_id
                || matches!(task.state, TaskState::Done | TaskState::Cancelled)
        });
        let removed = before - self.tasks.len();
        self.tasks.extend(fresh.into_iter().map(|mut task| {
            if task.verify.is_empty() {
                task.verify.clone_from(&inherited);
            }
            task
        }));
        removed
    }

    /// Build a ledger from the roadmap the owner approved.
    ///
    /// Only OPEN items become tasks: a roadmap entry already marked done is
    /// history, and re-running it would be work nobody asked for. Every task
    /// carries the roadmap id it came from, so the two can never disagree about
    /// what exists.
    #[must_use]
    pub fn from_roadmap(items: &[crate::brief::RoadmapItem]) -> Self {
        let tasks = items
            .iter()
            .filter(|item| item.is_open())
            .enumerate()
            .map(|(index, item)| {
                let id = if item.id.trim().is_empty() {
                    format!("t-{:03}", index + 1)
                } else {
                    item.id.clone()
                };
                Task::new(id, item.id.clone(), item.title.clone())
            })
            .collect();
        Self { version: 1, tasks }
    }

    /// Build the steps of a plan into tasks that carry their role.
    ///
    /// # The one call that makes routing real
    ///
    /// [`Task::with_tier`] existed, was tested, and had no production caller —
    /// so `role()` answered `None` for every real task and
    /// `agent::worker` never switched model. Every task ran on whatever the
    /// session opened with, including the ones whose whole point was that they
    /// were cheap. This is the call that was missing.
    ///
    /// `roadmap_item` is the item the plan decomposes, so each step still traces
    /// back to something the owner approved — a step is HOW an item gets done,
    /// never a new thing to build.
    /// `verify` is the command that decides whether each step is finished.
    ///
    /// # Why it is a parameter and not looked up here
    ///
    /// The check is the operator's, resolved through
    /// [`crate::verify::command_for`] — environment, then the admin file, then
    /// detection from the manifest. Reaching for it inside the ledger would put
    /// a second resolver beside that one, and the copy that gets read is never
    /// reliably the copy that gets fixed.
    ///
    /// **An empty `verify` is not a shortcut.** It produces tasks that
    /// [`Task::can_be_verified`] refuses, and the runner sets every one of them
    /// aside — which is the law working, not a bug. It was a bug when
    /// `from_plan` forgot to pass one at all: a live run planned thirteen steps
    /// correctly and abandoned all thirteen with `0 attempt(s) made`, because
    /// nothing could ever have proved them.
    #[must_use]
    pub fn from_plan(
        roadmap_item: &str,
        steps: &[crate::plan::Step],
        verify: &[String],
        workspace: &Path,
    ) -> Self {
        // THE FROZEN INTERFACES, ATTACHED HERE SO NO CALLER CAN FORGET.
        //
        // `Task::contracts` was read by `runner::task_instruction` and written
        // by nobody: `Task::with_contracts` existed, was tested, and had no
        // production caller, so the block telling an agent which shapes are
        // frozen never appeared in a single real prompt. Same shape as
        // `with_tier` and `with_dependencies` — built, documented, unreachable.
        //
        // Done in `from_plan` rather than at the two call sites because a
        // third caller is the way this comes back.
        let contracts = crate::contracts::Contracts::load(workspace)
            .map(|frozen| frozen.required_names())
            .unwrap_or_default();

        let mut tasks: Vec<Task> = Vec::new();
        let mut preparation: Vec<String> = Vec::new();
        // Which task each PLAN STEP became, 1-based as the planner numbers
        // them. A step that folded into preparation has no task of its own, so
        // a dependency naming it resolves to whichever task absorbed it — the
        // work is in there, and that is the task to wait for.
        let mut task_of_step: Vec<Option<String>> = Vec::new();

        // Research tasks created since the last code task, waiting to be
        // handed to the change they were done for. This is what the old fold
        // achieved by putting their text inside that task's title: the work
        // happens first, and its result reaches whoever needed it.
        let mut research_pending: Vec<(String, String)> = Vec::new();
        // The last task that changed code. Research placed AFTER a change was
        // research about that change — a closing `review` is the ordinary
        // case — and the fold guaranteed the ordering by putting it in the
        // same prompt. Nothing else carries that guarantee now.
        let mut last_change: Option<String> = None;

        for step in steps {
            // WHAT THE PLANNER SAID HAS TO WAIT.
            //
            // Resolved against the steps ALREADY SEEN. A dependency pointing
            // forward, or at a step that produced no task, names nothing this
            // ledger can wait for — and a task waiting on a dependency that
            // never arrives is `dead_dependencies`, a run that stops for a
            // reason nobody can act on. Dropping it is the safe direction: the
            // worst case is two lanes that collide, which the merge already
            // refuses and retries.
            let declared = |id: &str, seen: &[Option<String>]| -> Vec<String> {
                step.after
                    .iter()
                    .filter_map(|n| seen.get(n.checked_sub(1)?)?.clone())
                    .filter(|dep| dep != id)
                    .collect()
            };

            // A STEP THAT PRODUCES INFORMATION, WITH A JUDGE OF ITS OWN.
            //
            // It is still true that a build cannot prove this step did
            // anything — that is why it is not given the project's check.
            // It is given a deliverable instead, and the command that judges
            // one. What this buys is the routing: as a task it carries its
            // role, so retrieval runs on the cheap model and a design review
            // does not. See `crate::findings`.
            if crate::roles::produces_findings(&step.role) {
                let id = format!("{roadmap_item}.{}", tasks.len() + 1);
                let path = crate::findings::path_for(&id);
                let mut depends = declared(&id, &task_of_step);
                // WAITS FOR THE CHANGE IT COMES AFTER. Without this a closing
                // `review` is free to run beside the `implement` task it was
                // written to judge, and would review code that does not exist
                // yet — a verdict on nothing, delivered on time.
                if let Some(change) = &last_change {
                    if !depends.contains(change) {
                        depends.push(change.clone());
                    }
                }

                let mut task = Task::new(
                    &id,
                    roadmap_item.to_string(),
                    format!(
                        "{}\n\nThis task changes no code. Write what you find to \
                         `{path}` — the files, the line numbers, and what they \
                         mean. That file is the deliverable, and it is what will \
                         be checked.",
                        step.instruction
                    ),
                )
                .with_tier(&step.role)
                .delivering_findings(&path);
                if !depends.is_empty() {
                    task = task.with_dependencies(depends);
                }
                tasks.push(task);
                research_pending.push((id.clone(), path));
                task_of_step.push(Some(id));
                continue;
            }

            if !crate::roles::changes_code(&step.role) {
                // Still folded, and `plan` is the case that matters: these
                // steps come out of a plan, so a plan step inside one is the
                // planner narrating itself. See `roles::produces_findings`.
                preparation.push(format!("{}: {}", step.role, step.instruction));
                // Resolved once the task that absorbs it exists.
                task_of_step.push(None);
                continue;
            }
            let mut title = String::new();
            if !preparation.is_empty() {
                title.push_str("First, without changing anything:\n");
                for line in preparation.drain(..) {
                    title.push_str(&format!("  - {line}\n"));
                }
                title.push_str("Then: ");
            }
            title.push_str(&step.instruction);
            let id = format!("{roadmap_item}.{}", tasks.len() + 1);

            let mut depends = declared(&id, &task_of_step);

            // THE RESEARCH THIS CHANGE WAS ASKED FOR, and where to read it.
            //
            // Both halves matter. The dependency makes the work happen first,
            // which the fold used to guarantee by putting it in the same task.
            // The paths make it USEFUL: an agent that is not told where the
            // findings are will go and do the reading again, and the run pays
            // twice for one answer.
            if !research_pending.is_empty() {
                title.push_str("\n\nWork already done for this task, read it before you start:\n");
                for (from, path) in research_pending.drain(..) {
                    title.push_str(&format!("  - `{path}` (from {from})\n"));
                    if !depends.contains(&from) && from != id {
                        depends.push(from);
                    }
                }
            }

            // EVERY CODE TASK, not only the ones whose text happens to name a
            // contract. A task that must honour a shape often does not
            // mention it — and the cost of the two directions is not
            // symmetric: an agent told about an interface it never touches
            // reads one extra line, while an agent NOT told renames a field
            // and breaks a module nobody notices until integration.
            //
            // Research tasks are left out: they write no code, so there is no
            // shape for them to break.
            let mut task = Task::new(&id, roadmap_item.to_string(), title)
                .with_tier(&step.role)
                .with_contracts(contracts.clone())
                .with_verify(verify.to_vec());
            if !depends.is_empty() {
                task = task.with_dependencies(depends);
            }
            tasks.push(task);
            last_change = Some(id.clone());

            // Every preparation step folded into this task resolves to it, so
            // a later `[after N]` naming one of them waits for the right task.
            for slot in &mut task_of_step {
                if slot.is_none() {
                    *slot = Some(id.clone());
                }
            }
            task_of_step.push(Some(id));
        }

        // Anything left over trails the last change — a closing `review` is the
        // usual case. With no change to attach to, the plan plans nothing this
        // ledger can run, and the steps are kept as they were so the runner
        // reports them honestly rather than the item vanishing.
        if !preparation.is_empty() {
            match tasks.last_mut() {
                Some(last) => {
                    last.title
                        .push_str("\nFinally, and report rather than change:\n");
                    for line in preparation.drain(..) {
                        last.title.push_str(&format!("  - {line}\n"));
                    }
                }
                None => {
                    tasks = steps
                        .iter()
                        .enumerate()
                        .map(|(index, step)| {
                            Task::new(
                                format!("{roadmap_item}.{}", index + 1),
                                roadmap_item.to_string(),
                                step.instruction.clone(),
                            )
                            .with_tier(&step.role)
                        })
                        .collect();
                }
            }
        }
        crate::trace!(
            "plan",
            "{roadmap_item}: {} step(s) became {} task(s) [{}]",
            steps.len(),
            tasks.len(),
            tasks
                .iter()
                .map(|t| t.est_tier.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        Self { version: 1, tasks }
    }

    /// Where the work stands, for a person asking mid-run.
    ///
    /// # Why this exists separately from [`Self::summary`]
    ///
    /// `summary` lists every task and is printed once, when a run ENDS. That
    /// answers "what happened" after the fact. It does not answer the question
    /// a person actually asks while a loop is working — *how is it going* —
    /// and there was no way to ask that at all: the ledger is on disk, the
    /// runner holds it, and nothing showed it on demand.
    ///
    /// So this leads with the counts, names what is in hand, and says who did
    /// the finished work. Blocked tasks carry the reason, because "blocked" on
    /// its own tells a person nothing they can act on.
    #[must_use]
    pub fn progress(&self) -> String {
        if self.tasks.is_empty() {
            return "No work is planned yet — the ledger is empty.".to_string();
        }

        let count = |state: TaskState| self.tasks.iter().filter(|t| t.state == state).count();
        let done = count(TaskState::Done);
        let total = self.tasks.len();

        let mut out = format!(
            "  {done}/{total} done · {} ready · {} running · {} blocked · {} set aside · {} cancelled",
            count(TaskState::Ready),
            count(TaskState::Running),
            count(TaskState::Blocked),
            count(TaskState::Abandoned),
            count(TaskState::Cancelled),
        );

        // What is in hand right now, or what would be picked up next. A person
        // asking mid-run wants this line more than any other.
        if let Some(running) = self.tasks.iter().find(|t| t.state == TaskState::Running) {
            out.push_str(&format!(
                "\n\n  In hand:  {} — {}",
                running.id, running.title
            ));
            if !running.worked_by.is_empty() {
                out.push_str(&format!("\n            by {}", running.worked_by));
            }
        } else if let Some(next) = self.next_ready() {
            out.push_str(&format!("\n\n  Next up:  {} — {}", next.id, next.title));
        }

        // Blocked work, with the reason. "Blocked" alone is not actionable.
        for task in self.tasks.iter().filter(|t| t.state == TaskState::Blocked) {
            out.push_str(&format!("\n\n  Blocked:  {} — {}", task.id, task.title));
            if !task.note.trim().is_empty() {
                out.push_str(&format!("\n            {}", first_line_of(&task.note)));
            }
        }

        // Set aside, with what failed. These are the ones a person must decide
        // about; leaving them silent is how held work disappears.
        for task in self
            .tasks
            .iter()
            .filter(|t| t.state == TaskState::Abandoned)
        {
            out.push_str(&format!("\n\n  Set aside: {} — {}", task.id, task.title));
            if !task.note.trim().is_empty() {
                out.push_str(&format!("\n             {}", first_line_of(&task.note)));
            }
        }

        // Who did the finished work, and what proved it.
        let finished: Vec<&Task> = self
            .tasks
            .iter()
            .filter(|t| t.state == TaskState::Done)
            .collect();
        if !finished.is_empty() {
            out.push_str("\n\n  Finished:");
            for task in finished {
                out.push_str(&format!("\n    {} — {}", task.id, task.title));
                if !task.worked_by.is_empty() {
                    out.push_str(&format!("\n      by {}", task.worked_by));
                }
                if let Some(evidence) = &task.evidence {
                    out.push_str(&format!("\n      proved by {evidence}"));
                }
            }
        }

        out
    }

    /// A line per task, for a person reading what the run did.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.tasks.is_empty() {
            return "The ledger is empty.".to_string();
        }
        let mut out = String::new();
        for task in &self.tasks {
            out.push_str(&format!(
                "\n  [{:?}] {} — {}",
                task.state, task.id, task.title
            ));
            if let Some(evidence) = &task.evidence {
                out.push_str(&format!("\n      {evidence}"));
            }
        }
        out.trim_start_matches('\n').to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The first non-empty line of a note, so a status line stays one line.
fn first_line_of(text: &str) -> String {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {

    /// **A re-plan must not quietly make a provable item unprovable.**
    ///
    /// Measured live: `d2` was planned with `node --check game.js`, its attempt
    /// failed, and the re-planner rebuilt it from the operator's settings —
    /// which resolved to NO check on that machine. The replacement task landed
    /// with `verify: []` and was set aside as unprovable, so a run that had just
    /// recovered from an agent failure lost the item to a second, quieter one.
    ///
    /// Put the bug back by extending with `fresh` unchanged and the first
    /// assertion fails: the replacement carries no check at all.
    #[test]
    fn a_replanned_task_inherits_the_check_of_the_one_it_replaces() {
        use super::{Ledger, Task, TaskState};

        let mut proved = Task::new("d2.1", "d2", "the old approach");
        proved.verify = vec![
            "node".to_string(),
            "--check".to_string(),
            "game.js".to_string(),
        ];
        let mut ledger = Ledger {
            version: 1,
            tasks: vec![proved],
        };

        let replaced =
            ledger.replan_item("d2", vec![Task::new("d2.1", "d2", "a different approach")]);

        assert_eq!(replaced, 1);
        assert_eq!(
            ledger.tasks[0].verify,
            vec!["node", "--check", "game.js"],
            "the check travels with the ITEM; a re-plan that drops it cannot be proved and is set aside"
        );
        assert_ne!(
            ledger.tasks[0].state,
            TaskState::Blocked,
            "a task with a check must not be blocked for want of one"
        );
    }

    /// A re-planner that names its OWN check keeps it — a different approach may
    /// legitimately be proved a different way.
    #[test]
    fn a_replanned_task_that_names_its_own_check_keeps_it() {
        use super::{Ledger, Task};

        let mut proved = Task::new("d2.1", "d2", "the old approach");
        proved.verify = vec![
            "node".to_string(),
            "--check".to_string(),
            "game.js".to_string(),
        ];
        let mut ledger = Ledger {
            version: 1,
            tasks: vec![proved],
        };

        let mut fresh = Task::new("d2.1", "d2", "a different approach");
        fresh.verify = vec!["npm".to_string(), "test".to_string()];
        ledger.replan_item("d2", vec![fresh]);

        assert_eq!(ledger.tasks[0].verify, vec!["npm", "test"]);
    }

    /// **A person watching a run can ask how it is going.** The ledger was
    /// shown only when a run ENDED, which answers "what happened" and never
    /// "how is it going" — the question actually asked mid-flight.
    #[test]
    fn progress_leads_with_what_is_done_out_of_how_many() {
        let mut ledger = Ledger {
            version: 1,
            tasks: vec![
                Task::new("t-1", "r-1", "first"),
                Task::new("t-2", "r-2", "second"),
                Task::new("t-3", "r-3", "third"),
            ],
        };
        ledger.tasks[0].state = TaskState::Done;
        ledger.tasks[1].state = TaskState::Ready;

        let out = ledger.progress();
        assert!(out.contains("1/3 done"), "got: {out}");
        assert!(out.contains("Next up:  t-2"), "got: {out}");
    }

    /// **Who did the finished work, and what proved it.** A status that says
    /// "done" without either is a claim the reader cannot check.
    #[test]
    fn progress_names_the_worker_and_the_evidence() {
        let mut ledger = Ledger {
            version: 1,
            tasks: vec![Task::new("t-1", "r-1", "the work")],
        };
        ledger.tasks[0].state = TaskState::Done;
        ledger.tasks[0].worked_by = "some-backend:grunt (abc123) on some-model".to_string();
        ledger.tasks[0].evidence = Some("`cargo test` passed in 900ms".to_string());

        let out = ledger.progress();
        assert!(out.contains("some-backend:grunt"), "got: {out}");
        assert!(out.contains("cargo test"), "got: {out}");
    }

    /// **Blocked work carries its reason.** "Blocked" on its own tells a person
    /// nothing they can act on, and they are the only one who can unblock it.
    #[test]
    fn progress_says_why_something_is_blocked() {
        let mut ledger = Ledger {
            version: 1,
            tasks: vec![Task::new("t-1", "r-1", "the work")],
        };
        ledger.tasks[0].state = TaskState::Blocked;
        ledger.tasks[0].note = "nothing can prove this finished".to_string();

        let out = ledger.progress();
        assert!(out.contains("Blocked:  t-1"), "got: {out}");
        assert!(out.contains("nothing can prove"), "got: {out}");
    }

    /// Work set aside is never silent: it is the category a person must decide
    /// about, and hiding it is how held work disappears.
    #[test]
    fn progress_surfaces_work_that_was_set_aside() {
        let mut ledger = Ledger {
            version: 1,
            tasks: vec![Task::new("t-1", "r-1", "the work")],
        };
        ledger.tasks[0].state = TaskState::Abandoned;
        ledger.tasks[0].note = "failed twice: the check did not pass".to_string();

        let out = ledger.progress();
        assert!(out.contains("Set aside: t-1"), "got: {out}");
        assert!(out.contains("failed twice"), "got: {out}");
    }

    /// A task in hand is named ahead of everything else — it is what the person
    /// asking is actually asking about.
    #[test]
    fn progress_names_the_task_in_hand() {
        let mut ledger = Ledger {
            version: 1,
            tasks: vec![Task::new("t-1", "r-1", "the work")],
        };
        ledger.tasks[0].state = TaskState::Running;
        ledger.tasks[0].worked_by = "some-backend:implement (xyz789)".to_string();

        let out = ledger.progress();
        assert!(out.contains("In hand:  t-1"), "got: {out}");
        assert!(out.contains("some-backend:implement"), "got: {out}");
    }

    /// An empty ledger says no work is planned, rather than reporting 0/0.
    #[test]
    fn progress_on_an_empty_ledger_says_nothing_is_planned() {
        let out = Ledger::new().progress();
        assert!(out.contains("No work is planned"), "got: {out}");
    }
    use super::*;
    use crate::verify::CommandEvidence;

    /// Test-only evidence. Production code CANNOT build this — the fields are
    /// private and `fabricated` is `#[cfg(test)]`, so the only proof a shipped
    /// binary can hold is proof something really ran.
    fn proof() -> Verified {
        Verified::from_evidence(CommandEvidence::fabricated(
            vec!["cargo".to_string(), "test".to_string()],
            Some(0),
        ))
        .expect("a zero exit is proof")
    }

    fn ledger_with(tasks: Vec<Task>) -> Ledger {
        Ledger { version: 1, tasks }
    }

    /// The law. `complete` demands proof, and proof cannot be built from a
    /// failure — so no path exists from "a model said it works" to `Done`.
    #[test]
    fn a_task_reaches_done_only_with_proof() {
        let mut ledger = ledger_with(vec![Task::new("t-1", "r-1", "the work")]);
        ledger
            .complete("t-1", &proof())
            .expect("proof completes it");

        let task = ledger.get("t-1").expect("the task");
        assert_eq!(task.state, TaskState::Done);
        assert!(
            task.evidence
                .as_deref()
                .is_some_and(|e| e.contains("passed")),
            "the evidence must be kept with the task; got: {:?}",
            task.evidence
        );
    }

    /// Two failures and it stops. A third attempt on an unchanged hypothesis is
    /// how an unattended loop burns a night.
    #[test]
    fn a_task_is_set_aside_after_the_second_failure() {
        let mut ledger = ledger_with(vec![Task::new("t-1", "r-1", "the work")]);

        let after_first = ledger
            .record_failure("t-1", "the check failed")
            .expect("recorded");
        assert_eq!(after_first, TaskState::Ready, "one failure is retried");
        assert_eq!(ledger.get("t-1").expect("task").attempts, 1);

        let after_second = ledger
            .record_failure("t-1", "failed again")
            .expect("recorded");
        assert_eq!(
            after_second,
            TaskState::Abandoned,
            "the second failure stops it rather than trying a third time"
        );
        assert_eq!(ledger.get("t-1").expect("task").attempts_left(), 0);
    }

    /// A task nobody can check must not be completable. It is a decision for a
    /// person, and the ledger says so rather than marking it done.
    #[test]
    fn a_task_with_no_check_can_never_be_proven_finished() {
        let unverifiable = Task::new("t-1", "r-1", "tidy the docs");
        assert!(
            !unverifiable.can_be_verified(),
            "a task with no verify command cannot be proven"
        );

        let checkable = Task::new("t-2", "r-2", "fix the parser")
            .with_verify(["cargo", "test", "--", "parser"]);
        assert!(checkable.can_be_verified());
    }

    /// Dependencies gate readiness, and finishing one unblocks the next on the
    /// same pass rather than some later one.
    #[test]
    fn finishing_a_task_makes_its_dependents_ready_immediately() {
        let mut ledger = ledger_with(vec![
            Task::new("t-1", "r-1", "first"),
            Task::new("t-2", "r-2", "second").with_dependencies(["t-1"]),
        ]);

        ledger.refresh_ready();
        assert_eq!(ledger.next_ready().map(|t| t.id.as_str()), Some("t-1"));
        assert_eq!(
            ledger.get("t-2").expect("task").state,
            TaskState::Pending,
            "the dependent waits"
        );

        ledger.complete("t-1", &proof()).expect("completes");
        ledger.refresh_ready();
        assert_eq!(
            ledger.next_ready().map(|t| t.id.as_str()),
            Some("t-2"),
            "the dependent becomes ready as soon as its dependency is done"
        );
    }

    #[test]
    fn a_task_cannot_be_started_unless_it_is_ready() {
        let mut ledger = ledger_with(vec![Task::new("t-1", "r-1", "work")]);
        let error = ledger.start("t-1").expect_err("pending is not ready");
        assert!(error.contains("not ready"), "got: {error}");

        ledger.refresh_ready();
        ledger.start("t-1").expect("ready starts");
        assert_eq!(ledger.get("t-1").expect("task").state, TaskState::Running);
    }

    #[test]
    fn an_unknown_task_is_reported_rather_than_silently_ignored() {
        let mut ledger = Ledger::new();
        for error in [
            ledger.start("nope").unwrap_err(),
            ledger.complete("nope", &proof()).unwrap_err(),
            ledger.record_failure("nope", "x").unwrap_err(),
            ledger.block("nope", "?").unwrap_err(),
        ] {
            assert!(error.contains("no task `nope`"), "got: {error}");
        }
    }

    /// The ledger derives from the roadmap; an item already done is history and
    /// must not be scheduled again.
    #[test]
    fn only_open_roadmap_items_become_tasks() {
        use crate::brief::RoadmapItem;
        let items = vec![
            RoadmapItem {
                id: "r-1".to_string(),
                title: "already shipped".to_string(),
                status: "done".to_string(),
            },
            RoadmapItem {
                id: "r-2".to_string(),
                title: "still to do".to_string(),
                status: "todo".to_string(),
            },
        ];
        let ledger = Ledger::from_roadmap(&items);
        assert_eq!(ledger.tasks.len(), 1, "the done item is history");
        assert_eq!(ledger.tasks[0].roadmap_id, "r-2");
        assert_eq!(ledger.tasks[0].title, "still to do");
    }

    #[test]
    fn open_work_is_what_decides_whether_a_run_continues() {
        let mut ledger = ledger_with(vec![Task::new("t-1", "r-1", "work")]);
        assert!(ledger.has_open_work());

        ledger.complete("t-1", &proof()).expect("completes");
        assert!(
            !ledger.has_open_work(),
            "a ledger of finished work has nothing left to run"
        );
    }

    /// A ledger that cannot survive a restart is not a ledger.
    #[test]
    fn a_ledger_round_trips_through_a_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut ledger = ledger_with(vec![
            Task::new("t-1", "r-1", "first").with_verify(["cargo", "test"]),
            Task::new("t-2", "r-2", "second").with_dependencies(["t-1"]),
        ]);
        ledger.complete("t-1", &proof()).expect("completes");
        ledger.save(dir.path()).expect("saves");

        let reloaded = Ledger::load(dir.path()).expect("loads");
        assert_eq!(reloaded, ledger, "what was written must be what comes back");
        assert_eq!(
            reloaded.tasks[0].state,
            TaskState::Done,
            "including the state and the evidence that closed it"
        );
        assert!(reloaded.tasks[0].evidence.is_some());
    }

    #[test]
    fn a_missing_ledger_is_an_empty_one_rather_than_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ledger = Ledger::load(dir.path()).expect("a missing file is not a failure");
        assert!(ledger.tasks.is_empty());
    }

    #[test]
    fn a_corrupt_ledger_is_reported_rather_than_silently_replaced() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = Ledger::path_for(dir.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "{ not a ledger").expect("write");

        let error = Ledger::load(dir.path()).expect_err("must not be treated as empty");
        assert!(error.contains("is not a ledger"), "got: {error}");
    }

    #[test]
    fn ledger_filters_tasks_by_worker_instance_and_fingerprint() {
        let mut t1 = Task::new("t-1", "r-1", "task 1");
        t1.worked_by = "backend-a:grunt (11223344) [agent-first123456] on model-x".to_string();

        let mut t2 = Task::new("t-2", "r-1", "task 2");
        t2.worked_by = "backend-b:implement (aabbccdd) [agent-second6543] on model-y".to_string();

        let mut t3 = Task::new("t-3", "r-1", "task 3");
        t3.worked_by = "backend-a:grunt (11223344) [agent-first123456] on model-x".to_string();

        let ledger = ledger_with(vec![t1, t2, t3]);

        // Query by instance ID (isolated agent)
        let first_worker_tasks = ledger.tasks_by_instance("agent-first123456");
        assert_eq!(first_worker_tasks.len(), 2);
        assert_eq!(first_worker_tasks[0].id, "t-1");
        assert_eq!(first_worker_tasks[1].id, "t-3");

        let second_worker_tasks = ledger.tasks_by_instance("agent-second6543");
        assert_eq!(second_worker_tasks.len(), 1);
        assert_eq!(second_worker_tasks[0].id, "t-2");

        // Query by fingerprint
        let fp_tasks = ledger.tasks_by_fingerprint("11223344");
        assert_eq!(fp_tasks.len(), 2);

        // General worker query
        assert_eq!(ledger.tasks_by_worker("backend-b").len(), 1);
        assert_eq!(ledger.tasks_by_worker("non-existent").len(), 0);
    }

    fn step(role: &str, instruction: &str, after: Vec<usize>) -> crate::plan::Step {
        crate::plan::Step {
            role: role.to_string(),
            instruction: instruction.to_string(),
            after,
        }
    }

    /// **A DECLARED ORDERING BECOMES A REAL DEPENDENCY.**
    ///
    /// Until the planner could say so, nothing in production ever called
    /// `with_dependencies` — so every task in every plan was ready at once,
    /// whether or not the second one edited what the first created. With
    /// lanes that is not merely untidy: it is two agents in two checkouts
    /// building on a file only one of them has.
    #[test]
    fn a_step_that_waits_gets_a_dependency_the_runner_honours() {
        let ledger = Ledger::from_plan(
            "r-1",
            &[
                step("implement", "add the login page", vec![]),
                step("implement", "add the signup page", vec![]),
                step("implement", "share the validation", vec![1, 2]),
            ],
            &["true".to_string()],
            Path::new("."),
        );

        assert_eq!(ledger.tasks.len(), 3);
        assert!(ledger.tasks[0].depends_on.is_empty());
        assert!(ledger.tasks[1].depends_on.is_empty());
        assert_eq!(
            ledger.tasks[2].depends_on,
            vec!["r-1.1".to_string(), "r-1.2".to_string()]
        );

        // AND THE RUNNER HONOURS IT. Only the two independent steps are ready;
        // the third waits, which is what stops it being handed to a lane.
        let mut ledger = ledger;
        ledger.refresh_ready();
        let ready: Vec<&str> = ledger
            .tasks
            .iter()
            .filter(|t| t.state == TaskState::Ready)
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(ready, ["r-1.1", "r-1.2"], "the waiting step was made ready");
    }

    /// A plan that declares nothing keeps today's behaviour exactly.
    #[test]
    fn a_plan_with_no_ordering_leaves_every_task_independent() {
        let ledger = Ledger::from_plan(
            "r-1",
            &[
                step("implement", "one", vec![]),
                step("implement", "two", vec![]),
            ],
            &["true".to_string()],
            Path::new("."),
        );
        assert!(ledger.tasks.iter().all(|t| t.depends_on.is_empty()));
    }

    /// **A DEPENDENCY NOTHING CAN SATISFY IS DROPPED, NOT RECORDED.**
    ///
    /// A planner naming a step that produced no task — or pointing forward at
    /// one that does not exist yet — would otherwise create a task waiting
    /// for something that never arrives. That is `dead_dependencies`: a run
    /// that halts for a reason nobody can act on. Dropping it risks two lanes
    /// colliding, which the merge already refuses and retries.
    #[test]
    fn an_unsatisfiable_dependency_is_dropped_rather_than_deadlocking() {
        let ledger = Ledger::from_plan(
            "r-1",
            &[
                // Points FORWARD at a step that has not happened yet.
                step("implement", "first", vec![2]),
                step("implement", "second", vec![]),
            ],
            &["true".to_string()],
            Path::new("."),
        );

        assert!(
            ledger.tasks[0].depends_on.is_empty(),
            "a forward reference became a dependency nothing can satisfy: {:?}",
            ledger.tasks[0].depends_on
        );
        assert!(
            ledger.dead_dependencies().is_empty(),
            "the plan produced a task that can never become ready"
        );
    }

    /// A folded step is absorbed by a task, and a dependency naming it waits
    /// for THAT task — the work really is in there.
    ///
    /// `plan` is the role that still folds. See
    /// [`crate::roles::produces_findings`] for why it, and only it, does.
    #[test]
    fn a_dependency_on_a_folded_step_waits_for_the_task_that_absorbed_it() {
        let ledger = Ledger::from_plan(
            "r-1",
            &[
                step("plan", "decide the order", vec![]),
                step("implement", "fix the first", vec![]),
                step("implement", "fix the second", vec![1]),
            ],
            &["true".to_string()],
            Path::new("."),
        );

        // `plan` produced no task of its own; it folded into r-1.1.
        assert_eq!(ledger.tasks.len(), 2, "got {:?}", ledger.tasks);
        assert_eq!(
            ledger.tasks[1].depends_on,
            vec!["r-1.1".to_string()],
            "the dependency did not resolve to the task holding that work"
        );
    }

    /// **THE FROZEN INTERFACES MUST REACH THE AGENT THAT COULD BREAK THEM.**
    ///
    /// `Task::contracts` was read by `runner::task_instruction` — it builds
    /// the block telling an agent which names are agreed and not to be
    /// renamed — and written by nothing. `Task::with_contracts` existed, was
    /// tested, and had no production caller, so that block never appeared in
    /// a real prompt. The whole point of freezing a shape is that the person
    /// building against it is told, and nobody was.
    #[test]
    fn a_code_task_is_told_which_shapes_are_frozen() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join(".incord")).expect("mkdir");
        std::fs::write(
            dir.path().join(crate::contracts::CONTRACTS_FILE),
            r#"{"schemas":{"User":{"id":"string"}},
                "endpoints":[{"path":"POST /api/users","request":"User"}]}"#,
        )
        .expect("write");

        let ledger = Ledger::from_plan(
            "r-1",
            &[
                step("explore", "find the writers", vec![]),
                step("implement", "add the endpoint", vec![]),
            ],
            &["true".to_string()],
            dir.path(),
        );

        let change = ledger
            .tasks
            .iter()
            .find(|t| t.deliverable.is_none())
            .expect("the implement task");
        assert!(
            change.contracts.contains(&"User".to_string()),
            "the agent was not told about the frozen schema: {:?}",
            change.contracts
        );
        assert!(
            change.contracts.contains(&"POST /api/users".to_string()),
            "the agent was not told about the frozen endpoint: {:?}",
            change.contracts
        );

        // AND IT REACHES THE PROMPT. The field is worth nothing if the
        // instruction does not carry it.
        let told = crate::runner::task_instruction(change);
        assert!(told.contains("User"), "{told}");

        // A research task writes no code, so there is no shape for it to
        // break and no reason to spend prompt on one.
        let research = ledger
            .tasks
            .iter()
            .find(|t| t.deliverable.is_some())
            .expect("the explore task");
        assert!(research.contracts.is_empty(), "{:?}", research.contracts);
    }

    /// A project with no contracts file is the ordinary case and must produce
    /// ordinary tasks — not an error, and not a task claiming shapes exist.
    #[test]
    fn no_contracts_file_means_no_contracts_and_no_complaint() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ledger = Ledger::from_plan(
            "r-1",
            &[step("implement", "do it", vec![])],
            &["true".to_string()],
            dir.path(),
        );
        assert_eq!(ledger.tasks.len(), 1);
        assert!(ledger.tasks[0].contracts.is_empty());
    }

    /// **A RESEARCH STEP IS A TASK, AND IT IS NOT JUDGED BY THE BUILD.**
    ///
    /// Both halves are the point. It has to be a task or it is never routed
    /// to a model of its own — which was the whole reason `roles` exists and
    /// the reason the fold was a bad trade. And it must not inherit
    /// `["true"]`, the project's check, because a check that passes whatever
    /// the step did is what marked `explore: locate the ball entity` as done.
    #[test]
    fn a_research_step_is_its_own_task_judged_by_its_deliverable() {
        let ledger = Ledger::from_plan(
            "r-1",
            &[
                step("explore", "find the writers", vec![]),
                step("implement", "fix them", vec![]),
            ],
            &["true".to_string()],
            Path::new("."),
        );

        assert_eq!(ledger.tasks.len(), 2, "got {:?}", ledger.tasks);

        let research = &ledger.tasks[0];
        assert_eq!(research.est_tier, "explore");
        let path = research
            .deliverable
            .clone()
            .expect("a research task must have a deliverable");
        assert_ne!(
            research.verify,
            vec!["true".to_string()],
            "the research step inherited the build as its judge"
        );
        assert!(
            research.verify.contains(&path),
            "the check does not name the deliverable: {:?}",
            research.verify
        );
        // And it can still be finished. A task nothing can prove is set aside
        // by the runner, which would turn every research step into a stall.
        assert!(research.can_be_verified(), "{:?}", research.verify);

        // The change waits for it, and is told where to read it.
        let change = &ledger.tasks[1];
        assert_eq!(change.depends_on, vec![research.id.clone()]);
        assert!(change.title.contains(&path), "{}", change.title);
        assert_eq!(change.verify, vec!["true".to_string()]);
    }

    /// A trailing `review` is a task in its own right, waiting on the change.
    ///
    /// It used to be appended to the last task's title as "Finally, and
    /// report rather than change" — which asked the model that wrote the code
    /// to review the code it had just written, in the same turn, and then
    /// judged the whole thing by whether the build passed.
    #[test]
    fn a_closing_review_runs_after_the_change_and_on_its_own() {
        let ledger = Ledger::from_plan(
            "r-1",
            &[
                step("implement", "write it", vec![]),
                step("review", "judge the approach", vec![]),
            ],
            &["true".to_string()],
            Path::new("."),
        );

        assert_eq!(ledger.tasks.len(), 2, "got {:?}", ledger.tasks);
        let review = &ledger.tasks[1];
        assert_eq!(review.est_tier, "review");
        assert!(review.deliverable.is_some());
        assert_eq!(
            review.depends_on,
            vec!["r-1.1".to_string()],
            "the review can run beside the change it was written to judge"
        );
    }
}
