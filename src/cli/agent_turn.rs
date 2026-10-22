//! One interactive turn, run by an installed agent on the operator's
//! subscription.
//!
//! # What this replaces, and why
//!
//! The harness used to own the agentic loop: it held a provider client, sent
//! messages, dispatched tools and streamed tokens back. That loop needed an API
//! credential and re-implemented, less well, what the installed agent already
//! does — its own tools, its own subagents, its own compaction, its own caching.
//!
//! So the loop is gone and the agent does the doing. What stays here is the part
//! only this harness does: the guard rules, the memory context, the checkpoint
//! that makes a turn undoable, and the budget.
//!
//! # Undo, on a path where the harness cannot see the writes
//!
//! [`crate::tools::checkpoint::CheckpointStore`] snapshots a file just before it
//! is written — which works when the harness does the writing. A delegated agent
//! writes through its own tools, so there is nothing to intercept.
//!
//! The snapshot is therefore taken BEFORE the turn, over the whole workspace.
//! That is more expensive than snapshotting the files actually touched, and it
//! is the only thing that can work without seeing them.
//!
//! **A capped snapshot is refused rather than offered.** If the tree is larger
//! than [`MAX_SNAPSHOT_FILES`], undo is reported as unavailable for that turn.
//! A partial undo would restore some files and not others, leaving a tree that
//! matches neither state — worse than no undo at all.

use std::path::{Path, PathBuf};

use crate::agent::{AgentBackend, AgentEvent, TurnOutcome};
use crate::tools::checkpoint::{CheckpointStore, SKIP_DIRS};

pub use crate::tools::checkpoint::MAX_SNAPSHOT_FILES;

/// An interactive session driven by an installed agent.
pub struct AgentSession {
    /// Boxed because the route is chosen at run time: an installed CLI, or a
    /// model over an API that this harness supplies the tools to. The session
    /// asks the same things of either and takes the default answer where a
    /// route genuinely cannot do something.
    backend: Box<dyn AgentBackend>,
    /// Standby backends that take over if the primary backend hits a quota limit
    /// or fails with an agent-side error.
    standby: Vec<Box<dyn AgentBackend>>,
    /// The primary root, kept so the guards can be re-read when roots change.
    workspace: PathBuf,
    checkpoints: CheckpointStore,
    /// Whether the last turn's snapshot covered the whole tree.
    ///
    /// `None` until a turn has run at all. A plain `bool` conflated "no turn
    /// yet" with "the snapshot hit its cap", so `/undo` in a fresh session
    /// reported that a one-file workspace had more than [`MAX_SNAPSHOT_FILES`]
    /// files — a false statement about the user's own tree.
    ///
    /// `Some(false)` means the snapshot was incomplete, and [`Self::undo`]
    /// refuses rather than restoring part of a change.
    snapshot_complete: Option<bool>,
    /// Set when the user asks to stop; cleared at the start of each turn so a
    /// press at the prompt cannot leak into the next one.
    cancel: Option<crate::runtime::cancel::CancelToken>,
    /// The operator's own hook commands (`crate::hooks`), for the events this
    /// session fires itself — a person's line, before it is sent. The tool
    /// events are the toolbox's.
    operator_hooks: Option<crate::hooks::HookRunner>,
    /// How many turns this session has run.
    turns: usize,
    /// Directories beyond the workspace this session may reach.
    ///
    /// Snapshotted with the workspace, so `/undo` puts back a change made in an
    /// added directory too. An undo that covered only the first root would
    /// restore half a change, which is worse than none.
    extra_dirs: Vec<PathBuf>,
    /// The last quota the agent reported, and when it resets.
    ///
    /// Parsed from the agent's own rate-limit event since the first turn this
    /// harness ever drove, and shown nowhere until now. It is the number that
    /// decides whether work MOVES to another agent, so an operator who cannot
    /// see it cannot see a failover coming.
    quota: Option<(String, i64)>,
    /// Things to tell the person, once the status line has stopped.
    ///
    /// Printed by the CALLER rather than here. Writing to the terminal while
    /// the status line is drawing puts two writers on one line, which is how a
    /// reply came out as "Thinking · 2m 36st by getting oriented".
    notices: Vec<String>,
    /// Answers collected so far, held until the last question is answered.
    answers: Vec<(crate::ask::Question, crate::ask::Answer)>,
    /// Questions the agent asked that nobody has answered yet.
    ///
    /// Held rather than answered on the spot: a modal read would stop the
    /// session dead, and the natural way to answer is simply to type the next
    /// line. So the menu is PRINTED and the next input is read as the answer.
    pending: Vec<crate::ask::Question>,
    /// Tokens the agent has reported, summed across turns.
    ///
    /// The agent reports CUMULATIVE totals per turn, so each turn REPLACES the
    /// turn's contribution rather than adding to it — adding would count the
    /// same tokens once per event and show a total that never matches the bill.
    usage: crate::runtime::usage::TokenUsage,
    /// The same spend, split by the model each turn asked for, for `/cost`.
    usage_by_model: Vec<(String, crate::runtime::usage::TokenUsage)>,
    /// The memory engine's session lifecycle, fired by this session: the
    /// session's start and end, each person's line, and each turn's stop.
    /// Fires nothing until [`Self::start_memory`] gives it an engine.
    memory: crate::hooks::memory::MemoryHooks,
    /// The standing context, less the blocks the engine's session-start
    /// replaces.
    base_context: Vec<String>,
    /// incode's own blocks that the engine's session-start block replaces when
    /// it answers — the rules, the recorder's handoff, the auditor's findings —
    /// so the two are never both sent.
    native_context: Vec<String>,
    /// What the engine's session-start handed this session, when it answered.
    start_block: Option<String>,
    /// The session-start still being answered, on a thread of its own: the
    /// session opens without waiting for it, and the first turn does.
    pending_start: Option<crate::hooks::memory::PendingStart>,
    /// The session's tool state: its background jobs — the model's and the
    /// REPL's `/bg` alike — and the model's task list, shared by every
    /// backend. A stop while a job runs is a wait, not an end.
    session_tools: std::sync::Arc<crate::toolbox::SessionTools>,
    /// The agent mode last set, so a backend switched in runs under it too.
    mode: Option<String>,
    /// Who is asked mid-turn, so a backend switched in asks them too.
    prompter: Option<std::sync::Arc<dyn crate::hooks::engine::Prompter>>,
    /// Who is told what the tools and workers do while they run, so a
    /// backend switched in tells them too.
    watch: Option<std::sync::Arc<dyn crate::watch::Watch>>,
    /// The `allowed-tools` of the custom command the next turn runs, taken
    /// by that turn and held for it alone. Empty: the turn is not narrowed.
    next_allowed: Vec<String>,
}

/// How wide a tool result panel is drawn in a conversation.
const RESULT_WIDTH: usize = 100;

/// The files as they stood when the running turn began.
///
/// Handed to a turn's observer beside every event, so a front end that shows
/// an edit as its before and after — `incode --acp` — can name the "before"
/// from the snapshot this turn took anyway. Reading the file when the call is
/// SEEN would race the agent: an agent CLI writes on its own, and the read can
/// land after the write.
pub struct TurnStart<'a> {
    checkpoints: &'a CheckpointStore,
    /// Whether the snapshot walked the whole tree. When it did not, a file it
    /// never saw may still have existed.
    complete: bool,
    roots: &'a [PathBuf],
}

/// What [`AgentSession::turn_observed`] hands each agent event to, with the
/// [`TurnStart`] beside it.
type Observer<'o> = &'o mut dyn FnMut(&AgentEvent, &TurnStart<'_>);

/// What a file held when the turn began.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Before {
    /// These contents.
    Held(String),
    /// Nothing: the file did not exist.
    Absent,
    /// Not known — the snapshot could not hold it, did not look there, or it
    /// is not text. Never reported as absent: that would call an existing
    /// file new.
    Unknown,
}

impl TurnStart<'_> {
    /// What `path` held when the turn began. `path` is spelled as the
    /// snapshot spelled it: a root joined with the names on disk.
    #[must_use]
    pub fn before(&self, path: &Path) -> Before {
        use crate::tools::checkpoint::Recorded;
        match self.checkpoints.recorded(path) {
            Recorded::Contents(bytes) => {
                String::from_utf8(bytes.to_vec()).map_or(Before::Unknown, Before::Held)
            }
            Recorded::Absent => Before::Absent,
            Recorded::Unheld => Before::Unknown,
            // The snapshot records the files that EXIST. Not among them means
            // absent only when the walk was complete and would have gone there.
            Recorded::NotSeen => {
                let walked = self.roots.iter().any(|root| {
                    path.strip_prefix(root).is_ok_and(|inside| {
                        !inside.components().any(|part| {
                            SKIP_DIRS.contains(&part.as_os_str().to_string_lossy().as_ref())
                        })
                    })
                });
                if self.complete && walked {
                    Before::Absent
                } else {
                    Before::Unknown
                }
            }
        }
    }
}

impl AgentSession {
    /// Start a session against a discovered agent.
    #[must_use]
    pub fn new(
        workspace: PathBuf,
        agent_id: &str,
        binary: String,
        guards: &crate::guards::Guards,
        system_context: Vec<String>,
    ) -> Self {
        let workspace_for_self = workspace.clone();
        // WHICH ROUTE is the agent's id, never whether API variables happen to
        // be set: those used to replace the chosen agent outright.
        let mut backend: Box<dyn AgentBackend> =
            crate::cli::route_backend(&workspace, agent_id, &binary, guards, &system_context, true);
        let session_tools = crate::toolbox::SessionTools::new(true);
        backend.share_session_tools(std::sync::Arc::clone(&session_tools));
        let offered = backend.offered_tools();
        if !offered.is_empty() {
            crate::trace!(
                "session",
                "incode runs {} tools for {}: {}",
                offered.len(),
                backend.id(),
                offered.join(", ")
            );
        }
        Self {
            session_tools,
            backend,
            standby: Vec::new(),
            memory: crate::hooks::memory::MemoryHooks::off(&workspace),
            base_context: system_context,
            native_context: Vec::new(),
            start_block: None,
            pending_start: None,
            workspace: workspace_for_self,
            checkpoints: CheckpointStore::new(),
            snapshot_complete: None,
            cancel: None,
            operator_hooks: None,
            turns: 0,
            quota: None,
            notices: Vec::new(),
            pending: Vec::new(),
            answers: Vec::new(),
            extra_dirs: Vec::new(),
            usage: crate::runtime::usage::TokenUsage::default(),
            usage_by_model: Vec::new(),
            mode: None,
            prompter: None,
            watch: None,
            next_allowed: Vec::new(),
        }
    }

    /// Hold the next turn — that one, not the session — to `allowed`, a custom
    /// command's `allowed-tools` in the permission rules' grammar. Empty lifts
    /// a restriction set and not yet used.
    pub fn restrict_next_turn(&mut self, allowed: Vec<String>) {
        self.next_allowed = allowed;
    }

    /// The next turn's restriction, put in force on the session's tools:
    /// the guard that lifts it, or `None` when the turn is not narrowed. Said
    /// in the notices when a rule does not parse, and when the agent serving
    /// the turn runs its own tools, where incode cannot refuse a call.
    fn hold_turn_to_allowed(&mut self) -> Option<crate::toolbox::TurnRestriction> {
        let allowed = std::mem::take(&mut self.next_allowed);
        if allowed.is_empty() {
            return None;
        }
        let (list, warnings) = crate::permissions::rules::Allowlist::parse(
            &allowed,
            crate::admin::home_dir().as_deref(),
        );
        self.notices.extend(warnings);
        if !self.backend.runs_tools_here() {
            self.notices.push(crate::custom_commands::not_enforced_by(
                self.backend.id(),
                &allowed,
            ));
        }
        crate::trace!("session", "this turn's tools: {}", allowed.join(", "));
        Some(self.session_tools.restrict_turn(list))
    }

    /// Put `backend` in as the agent that serves the next turn, carrying
    /// everything this session has already given the one it replaces: its
    /// tools, roots, stop, mode, prompter, memory id and standing context.
    ///
    /// A different agent cannot continue the conversation the last one had,
    /// so the next turn starts a fresh one there. The agent replaced becomes
    /// a standby — two installed agents are two allowances — unless it is
    /// the configured API route, which is never a standby; a standby of the
    /// agent now leading is dropped.
    pub fn switch_backend(&mut self, mut backend: Box<dyn AgentBackend>) {
        backend.share_session_tools(std::sync::Arc::clone(&self.session_tools));
        backend.set_extra_dirs(self.extra_dirs.clone());
        if let Some(token) = &self.cancel {
            backend.set_cancel(token.clone());
        }
        if let Some(mode) = &self.mode {
            backend.set_permission_mode(mode);
        }
        if let Some(prompter) = &self.prompter {
            backend.set_prompter(std::sync::Arc::clone(prompter));
        }
        if let Some(watch) = &self.watch {
            backend.set_watch(std::sync::Arc::clone(watch));
        }
        let mut previous = std::mem::replace(&mut self.backend, backend);
        let now = self.backend.id().to_string();
        self.standby.retain(|standby| standby.id() != now);
        if previous.id() != now && !crate::agent::models::is_api_route(previous.id()) {
            previous.clear_session();
            self.standby.push(previous);
        }
        self.name_memory_session();
        self.carry_context();
    }

    /// Ask the agent for the model the role table gives `role` — the
    /// operator's choice first wherever this agent can serve it, then the
    /// agent's own ladder for the role, as the ledger's worker routes a task.
    /// Nothing to offer leaves the model as it was. Returns the model the
    /// next turn asks for, if one was chosen.
    pub fn use_role(&mut self, table: &crate::roles::RoleTable, role: &str) -> Option<&str> {
        let mut ladder: Vec<String> = table.candidates_for(role).to_vec();
        ladder.extend(self.backend.ladder_for(role));
        if !ladder.is_empty() {
            self.backend.set_models(&ladder);
        }
        self.backend.model()
    }

    /// Write this session's transcript even when no memory engine will read
    /// it: a session a client can load again is replayed from it.
    pub fn keep_transcript(&mut self) {
        self.memory.keep_transcript();
    }

    /// Configure standby agents to take over if the primary backend fails.
    #[must_use]
    pub fn with_standby(mut self, mut standby: Vec<Box<dyn AgentBackend>>) -> Self {
        for s in &mut standby {
            if let Some(token) = &self.cancel {
                s.set_cancel(token.clone());
            }
            s.set_extra_dirs(self.extra_dirs.clone());
            s.share_session_tools(std::sync::Arc::clone(&self.session_tools));
            if let Some(watch) = &self.watch {
                s.set_watch(std::sync::Arc::clone(watch));
            }
        }
        self.standby = standby;
        self
    }

    /// The session's background jobs and task list.
    #[must_use]
    pub fn session_tools(&self) -> std::sync::Arc<crate::toolbox::SessionTools> {
        std::sync::Arc::clone(&self.session_tools)
    }

    /// Access configured standby backends.
    #[must_use]
    pub fn standby(&self) -> &[Box<dyn AgentBackend>] {
        &self.standby
    }

    /// The id of the current active primary backend.
    #[must_use]
    pub fn backend_id(&self) -> &str {
        self.backend.id()
    }

    /// The tools incode runs itself for the current backend — empty for a
    /// backend whose CLI still uses its own (see `docs/ARCHITECTURE.md` §5a).
    #[must_use]
    pub fn offered_tools(&self) -> Vec<String> {
        self.backend.offered_tools()
    }

    #[cfg(test)]
    #[must_use]
    pub fn with_backend(mut self, mut backend: Box<dyn AgentBackend>) -> Self {
        backend.share_session_tools(std::sync::Arc::clone(&self.session_tools));
        self.backend = backend;
        self
    }

    /// Every root this session may touch, workspace first.
    #[must_use]
    pub fn roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.workspace.clone()];
        roots.extend(self.extra_dirs.iter().cloned());
        roots
    }

    /// Extend the session — and its snapshot — to more directories.
    #[must_use]
    pub fn with_extra_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.backend.set_extra_dirs(dirs.clone());
        for s in &mut self.standby {
            s.set_extra_dirs(dirs.clone());
        }
        self.extra_dirs = dirs;
        self
    }

    /// Take what should be shown to the person after the turn.
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    /// The questions waiting on an answer.
    #[must_use]
    pub fn pending(&self) -> &[crate::ask::Question] {
        &self.pending
    }

    /// Read this input as the answer to the question at the front of the queue.
    ///
    /// # Why a queue and not one pass
    ///
    /// An agent can ask two things in one reply — measured, on the first live
    /// run of this feature. Applying one input to both would answer a question
    /// the person never read: "b" means the second option of the FIRST question
    /// and nothing at all about the second.
    ///
    /// So each input answers one question. Only when the last is answered does
    /// anything go back to the agent, because a turn per answer would pay for
    /// the same conversation several times over.
    pub fn take_answer(&mut self, input: &str) -> Answered {
        if self.pending.is_empty() {
            return Answered::NothingPending;
        }
        let question = self.pending.remove(0);
        let answer = question.answer_for(input);
        self.answers.push((question, answer));

        if let Some(next) = self.pending.first() {
            return Answered::AskedTheNext(next.render());
        }
        match crate::ask::reply(&std::mem::take(&mut self.answers)) {
            Some(instruction) => Answered::Ready(instruction),
            None => Answered::NothingPending,
        }
    }

    /// The deny rules the agent will be given.
    #[must_use]
    pub fn deny_rules(&self) -> &[String] {
        self.backend.deny_rules()
    }

    /// The directories this session works in beyond the workspace.
    #[must_use]
    pub fn extra_dirs(&self) -> &[PathBuf] {
        &self.extra_dirs
    }

    /// Give the session the flag that says the user asked to stop.
    #[must_use]
    pub fn with_cancel(mut self, token: crate::runtime::cancel::CancelToken) -> Self {
        self.backend.set_cancel(token.clone());
        for s in &mut self.standby {
            s.set_cancel(token.clone());
        }
        self.cancel = Some(token);
        self
    }

    /// The cancellation token, if one is configured.
    #[must_use]
    pub fn cancel_token(&self) -> Option<crate::runtime::cancel::CancelToken> {
        self.cancel.clone()
    }

    /// Point the next turn at a named model, falling back through the rest.
    pub fn set_models(&mut self, candidates: &[String]) {
        self.backend.set_models(candidates);
    }

    /// Point the next turn at a model the operator named outright.
    pub fn choose_model(&mut self, model: &str) {
        self.backend.choose_model(model);
    }

    /// The model the next turn will ask for.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.backend.model()
    }

    /// The API route the lead is driven over, when it is.
    #[must_use]
    pub fn api_route(&self) -> Option<&crate::agent::api::Settings> {
        self.backend.api_route()
    }

    /// Compact the lead's conversation now — see
    /// [`crate::agent::AgentBackend::compact`]. `None` when the lead is an
    /// agent CLI, which compacts its own.
    pub fn compact(&mut self, instructions: &str) -> Option<Result<String, String>> {
        // The operator's `pre_compact` commands first (`crate::hooks`): told,
        // never asked — what they print is shown with the result.
        let said: Vec<String> = match &self.operator_hooks {
            Some(hooks) => hooks
                .pre_compact(
                    "manual",
                    instructions,
                    self.session_id().unwrap_or(""),
                    Some(&self.workspace),
                )
                .messages()
                .to_vec(),
            None => Vec::new(),
        };
        match self.backend.compact(instructions) {
            Some(Ok(done)) if !said.is_empty() => {
                let lines: Vec<String> = said
                    .iter()
                    .map(|s| format!("[pre_compact hook] {s}"))
                    .collect();
                Some(Ok(format!("{}\n{done}", lines.join("\n"))))
            }
            other => other,
        }
    }

    /// Start a fresh agent session on the next turn.
    ///
    /// Really cleared: the id is dropped, so the next turn does not resume the
    /// conversation the user just asked to be rid of.
    ///
    /// For the memory engine this is one session ending and another opening:
    /// the end names the OLD id, and the new session's context replaces the
    /// old one's in what every backend carries.
    pub fn clear(&mut self) {
        // What the old conversation claimed, read and announced is let go
        // while every backend still names it.
        self.end_code_edits();
        self.backend.clear_session();
        for s in &mut self.standby {
            s.clear_session();
        }
        let next = uuid::Uuid::new_v4().to_string();
        // The old session's start is answered before it is ended.
        self.settle_memory_start();
        self.start_block = self.memory.restart(&next, "clear");
        self.name_memory_session();
        self.carry_context();
    }

    /// Open the memory engine's lifecycle for this session: every backend
    /// files its calls under the session's id, and the engine's session-start
    /// is fired with `source` (`startup`, `resume`).
    ///
    /// `native` are the blocks of the standing context that the engine's
    /// session-start block replaces: when it answers they are dropped and its
    /// block carried instead, so the same rules are never sent twice. When it
    /// does not, they stay.
    pub fn start_memory(
        &mut self,
        memory: crate::hooks::memory::MemoryHooks,
        native: Vec<String>,
        source: &str,
    ) {
        self.base_context.retain(|block| !native.contains(block));
        self.native_context = native;
        self.memory = memory;
        self.name_memory_session();
        // Asked now and answered in the background: the session opens without
        // it (it was 8.2 s of `session/new`), and the first turn waits for it,
        // because its context belongs in that turn.
        self.pending_start = self.memory.start_in_background(source);
    }

    /// Collect the session-start still being answered, if there is one, and
    /// hand its block to every backend. Waits at most the engine's own bound
    /// for the hook, so a block the engine gives is never dropped for being
    /// late; a hook past that bound was killed by the engine runner anyway.
    fn settle_memory_start(&mut self) {
        let Some(pending) = self.pending_start.take() else {
            return;
        };
        self.start_block = pending.wait(crate::hooks::engine::CONTEXT_BOUND);
        if self.start_block.is_some() {
            self.carry_context();
        }
    }

    /// The operator's own hook commands for the events this session fires.
    pub fn set_operator_hooks(&mut self, hooks: Option<crate::hooks::HookRunner>) {
        self.operator_hooks = hooks;
    }

    /// The operator's `user_prompt_submit` commands for a person's `line`,
    /// before anything is sent: the context their stdout adds to the turn, or
    /// `Err` with the reason one refused it (exit 2). The line is not changed.
    ///
    /// Called by `cli::plan_turn`, so the REPL and `--acp` run it the same way.
    pub fn prompt_hooks(&self, line: &str) -> Result<Vec<String>, String> {
        let Some(hooks) = &self.operator_hooks else {
            return Ok(Vec::new());
        };
        let ran =
            hooks.user_prompt_submit(line, self.session_id().unwrap_or(""), Some(&self.workspace));
        if ran.is_denied() {
            return Err(ran.messages().join("\n"));
        }
        Ok(ran
            .messages()
            .iter()
            .map(|said| crate::hooks::prompt_context_block(said))
            .collect())
    }

    /// The person's line is about to go to the model: written to the session's
    /// transcript, and the engine's context for it returned. The first one
    /// waits for the session-start first, so the engine hears the session open
    /// before the line, and the block rides this turn.
    pub fn memory_prompt(&mut self, line: &str) -> Option<String> {
        self.settle_memory_start();
        self.memory.prompt(line)
    }

    /// The operator's `session_start` commands (`crate::hooks`): what they
    /// printed, for the caller to show. `source` is `startup`.
    ///
    /// Called by `cli::prepare_session` (REPL and `--acp`); `task_run` fires
    /// its own, as it has no `AgentSession`.
    #[must_use]
    pub fn operator_session_start(&self, source: &str) -> Vec<String> {
        self.operator_hooks.as_ref().map_or_else(Vec::new, |hooks| {
            hooks
                .session_start(
                    source,
                    self.session_id().unwrap_or(""),
                    Some(&self.workspace),
                )
                .messages()
                .to_vec()
        })
    }

    /// The operator's `session_end` commands (`crate::hooks`): what they
    /// printed, for the caller to show if it still can.
    ///
    /// Called beside `end_memory` by the REPL (`cli::mod`) and `--acp`
    /// (`acp_agent`); `task_run` fires its own.
    #[must_use]
    pub fn operator_session_end(&self, reason: &str) -> Vec<String> {
        self.operator_hooks.as_ref().map_or_else(Vec::new, |hooks| {
            hooks
                .session_end(
                    reason,
                    self.session_id().unwrap_or(""),
                    Some(&self.workspace),
                )
                .messages()
                .to_vec()
        })
    }

    /// The session is over; `reason` says why. A session-start still being
    /// answered is collected first, so the engine hears the start before the
    /// end.
    pub fn end_memory(&mut self, reason: &str) {
        self.settle_memory_start();
        self.end_code_edits();
        self.memory.end(reason);
    }

    /// The engine's edit check hears the session end from every backend that
    /// announced writes in it (each ends the same session; a second end finds
    /// nothing left).
    fn end_code_edits(&mut self) {
        self.backend.end_code_edits();
        for s in &mut self.standby {
            s.end_code_edits();
        }
    }

    /// Tell every backend the session's id for the engine.
    fn name_memory_session(&mut self) {
        let id = self.memory.session().to_string();
        if id.is_empty() {
            return;
        }
        self.backend.set_memory_session(&id);
        for s in &mut self.standby {
            s.set_memory_session(&id);
        }
    }

    /// Hand every backend the standing context as it now stands: the base,
    /// then the engine's session-start block — or, without one, incode's own
    /// blocks it would have replaced.
    fn carry_context(&mut self) {
        let mut blocks = self.base_context.clone();
        match &self.start_block {
            Some(block) => blocks.push(block.clone()),
            None => blocks.extend(self.native_context.iter().cloned()),
        }
        self.backend.set_system_context(blocks.clone());
        for s in &mut self.standby {
            s.set_system_context(blocks.clone());
        }
    }

    /// The output style for the following turns (`crate::styles`): the old
    /// block out of the standing context, `block` in — last, as the session
    /// started with it — and every backend told. `None` leaves no style.
    pub fn set_output_style(&mut self, block: Option<String>) {
        self.base_context
            .retain(|existing| !crate::styles::is_block(existing));
        self.base_context.extend(block);
        self.carry_context();
    }

    /// How much the agent may do on its own for the following turns.
    pub fn set_permission_mode(&mut self, mode: &str) {
        self.mode = Some(mode.to_string());
        self.backend.set_permission_mode(mode);
        for s in &mut self.standby {
            s.set_permission_mode(mode);
        }
    }

    /// Who is asked, mid-turn, when a tool call needs a person or a question
    /// needs the owner: the terminal for the REPL, the client under ACP.
    /// Standbys get the same one, since a failover keeps the same person.
    pub fn set_prompter(&mut self, prompter: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {
        self.prompter = Some(std::sync::Arc::clone(&prompter));
        self.backend.set_prompter(std::sync::Arc::clone(&prompter));
        for s in &mut self.standby {
            s.set_prompter(std::sync::Arc::clone(&prompter));
        }
    }

    /// Who is told, as calls run, what they print, what the guards and the
    /// engine decided about them, and which workers they start — see
    /// [`crate::watch`]. Standbys get the same one.
    pub fn set_watch(&mut self, watch: std::sync::Arc<dyn crate::watch::Watch>) {
        self.watch = Some(std::sync::Arc::clone(&watch));
        self.backend.set_watch(std::sync::Arc::clone(&watch));
        for s in &mut self.standby {
            s.set_watch(std::sync::Arc::clone(&watch));
        }
    }

    /// Carry these pictures with the next turn's prompt, on the agent that
    /// leads now. A standby that takes the turn over does not get them.
    ///
    /// # Errors
    ///
    /// The agent cannot take pictures headless; the reason names it.
    pub fn attach_images(&mut self, images: Vec<crate::toolbox::Image>) -> Result<(), String> {
        self.backend.attach_images(images)
    }

    /// The conversations subagents opened, as `(session id, what it was asked)`.
    ///
    /// Empty on a route whose subagents live inside another process — there is
    /// nothing to report there rather than nothing that happened.
    #[must_use]
    pub fn children(&self) -> Vec<(String, String)> {
        self.backend.children()
    }

    /// Continue an agent session that already exists.
    ///
    /// The agent owns the transcript, so resuming is handing it the id back —
    /// there is nothing for this harness to replay.
    pub fn resume(&mut self, session_id: &str) {
        self.backend.set_session_id(session_id);
    }

    /// The agent's session, once it has one.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.backend.session_id()
    }

    /// How many turns this session has run.
    #[must_use]
    pub fn turns(&self) -> usize {
        self.turns
    }

    /// The last quota status the agent reported, and its reset time.
    #[must_use]
    pub fn quota(&self) -> Option<(&str, i64)> {
        self.quota.as_ref().map(|(s, at)| (s.as_str(), *at))
    }

    /// What the agent has reported spending across the session.
    #[must_use]
    pub fn usage(&self) -> crate::runtime::usage::TokenUsage {
        self.usage
    }

    /// The session's spend per model, in the order each was first used.
    #[must_use]
    pub fn usage_by_model(&self) -> &[(String, crate::runtime::usage::TokenUsage)] {
        &self.usage_by_model
    }

    /// Whether there is a change to put back.
    #[must_use]
    pub fn has_undo(&self) -> bool {
        self.checkpoints.has_undo()
    }

    /// What the pending undo would put back.
    #[must_use]
    pub fn pending_label(&self) -> Option<&str> {
        self.checkpoints.pending_label()
    }
}

/// Whether an outcome is an agent-side failure that moves the work to a standby.
fn judged(
    backend: &dyn AgentBackend,
    result: Result<TurnOutcome, crate::agent::AgentError>,
) -> (Result<TurnOutcome, String>, bool) {
    match result {
        Ok(outcome) if outcome.is_error => {
            let moves = backend.agent_failed(&outcome);
            (Err(outcome.failure()), moves)
        }
        Ok(outcome) => (Ok(outcome), false),
        Err(err) => {
            let reason = err.to_string();
            let moves = crate::agent::moves_the_work(&reason);
            (Err(reason), moves)
        }
    }
}

impl AgentSession {
    /// Run one turn, streaming text to `on_text` as it arrives.
    ///
    /// `context` is what the harness knows and the agent does not — guard rules
    /// and recalled memory. It goes BEFORE the instruction, because context
    /// after an instruction reads as an afterthought.
    pub fn turn(
        &mut self,
        workspace: &Path,
        instruction: &str,
        context: &[String],
        on_text: &mut dyn FnMut(&str),
        on_status: &mut dyn FnMut(&crate::cli::activity::Status),
    ) -> Result<TurnOutcome, String> {
        self.turn_observed(workspace, instruction, context, on_text, on_status, None)
    }

    /// [`Self::turn`], with every agent event also handed to `observer`.
    ///
    /// # Why a second entry point and not a second turn
    ///
    /// A front end that is not a terminal — `incode --acp`, where an editor
    /// draws the turn — needs the tool calls and their results as STRUCTURE,
    /// not as the panels this session draws into the text for a terminal. Both
    /// front ends must run the same turn (snapshot, guards, sentinel, standby,
    /// follow-up, questions), so the turn is shared and only the presentation
    /// differs: with an observer, tool result panels and edit blocks are left
    /// out of `on_text`, because the observer presents them itself.
    ///
    /// Each event comes with the [`TurnStart`]: what the files held when this
    /// turn began, for an observer that shows an edit as before and after.
    pub fn turn_observed(
        &mut self,
        workspace: &Path,
        instruction: &str,
        context: &[String],
        on_text: &mut dyn FnMut(&str),
        on_status: &mut dyn FnMut(&crate::cli::activity::Status),
        mut observer: Option<Observer<'_>>,
    ) -> Result<TurnOutcome, String> {
        // Cleared first: a Ctrl-C pressed while the prompt was waiting belongs
        // to no turn, and letting it leak in would cancel the next one instantly.
        if let Some(token) = &self.cancel {
            token.reset();
        }
        // A custom command's `allowed-tools`, in force for this turn — its
        // follow-up and continuations included — and lifted when it ends.
        let held_to = self.hold_turn_to_allowed();
        // A turn not opened by a person's line (a continuation, `/run`) still
        // carries the session-start block.
        self.settle_memory_start();
        // Taken before anything runs, because the agent's writes cannot be
        // intercepted as they happen.
        self.snapshot_complete = Some(self.snapshot(workspace, instruction));

        // The deny rules PREVENT a protected write; this catches whatever they
        // did not. The ledger path has had this since the guards work landed —
        // the interactive path had only the rules, and a rule that failed to
        // match left nothing behind it. Measured: a bare glob does not reach an
        // added root, and the write went through unnoticed.
        let roots = self.roots();
        // Which files use what, taken BEFORE the turn so the same index can
        // answer "who else touches this" afterwards. A change that redefines a
        // name three other files use, and leaves them alone, is the most common
        // way work looks finished and is not.
        let links = crate::links::LinkMap::build(&roots);
        let watched = crate::guards::Guards::load(&self.workspace)
            .ok()
            .map(|guards| (crate::guards::Sentinel::over_roots(&roots, &guards), guards));
        // What the snapshot above holds, for an observer showing edits as
        // before and after. Borrows only the checkpoints, so the backends can
        // still be driven while it is held.
        let start = TurnStart {
            checkpoints: &self.checkpoints,
            complete: self.snapshot_complete == Some(true),
            roots: &roots,
        };

        let prompt = assemble_prompt(instruction, context);

        self.turns += 1;
        // What the agent is doing, for a person watching a turn that can run
        // for minutes. These events were parsed from the first turn this
        // harness ever drove and thrown away by the interactive path, which is
        // why a long turn showed one word: "Thinking".
        let started = std::time::Instant::now();
        let mut activity = crate::cli::activity::Activity::THINKING;
        // Which call each result belongs to. The result carries only an id.
        // The name of each call and what it was acting on. A result carries
        // only an id, so without this it is text with no author and no subject.
        let mut tool_calls: std::collections::HashMap<String, (String, Option<String>)> =
            std::collections::HashMap::new();
        // FILE CHANGES, shown as the changes themselves. The same watcher the
        // ledger run uses, so one rule decides what a change looks like on
        // screen wherever it happens. See [`crate::cli::edits::Watcher`].
        //
        // The root is cloned rather than borrowed: the event closure below
        // outlives this borrow of `self`.
        let mut edited = crate::cli::edits::Watcher::watching(self.workspace.clone());
        // Hides the question block while it streams, and keeps the whole reply
        // so it can be parsed once the turn ends.
        let mut filter = crate::ask::StreamFilter::new();
        let mut turn_usage = crate::runtime::usage::TokenUsage::default();
        let mut turn_quota: Option<(String, i64)> = None;
        // WHETHER THE LAST THING WRITTEN LEFT THE CURSOR MID-LINE.
        //
        // A reply streams in fragments and the last one does not end with a
        // newline, because the model simply stopped talking. The tool-result
        // panel below then writes its rule assuming it owns a fresh row — so the
        // two collided and the screen read:
        //
        //   I'll read the project's brief and roadmap first.── ✗ ToolSearch …
        //
        // which is the agent's sentence and a tool header sharing one line, with
        // no way to tell where one ends. Tracked here rather than asked of the
        // console, because `on_text` is a callback the caller owns and only this
        // loop knows what it has already sent.
        let mut mid_line = false;
        // The one place that ends a line before something structural is drawn.
        macro_rules! start_new_line {
            ($sink:expr) => {
                if mid_line {
                    $sink("\n");
                    mid_line = false;
                }
            };
        }
        let mut saw_deltas = false;
        let mut run_backend_attempt = |backend: &mut Box<dyn AgentBackend>,
                                       prompt: &str,
                                       said: &mut crate::hooks::memory::Said,
                                       filter: &mut crate::ask::StreamFilter,
                                       turn_usage: &mut crate::runtime::usage::TokenUsage,
                                       turn_quota: &mut Option<(String, i64)>,
                                       tool_calls: &mut std::collections::HashMap<
            String,
            (String, Option<String>),
        >,
                                       edited: &mut crate::cli::edits::Watcher,
                                       mid_line: &mut bool,
                                       on_text: &mut dyn FnMut(&str),
                                       on_status: &mut dyn FnMut(&crate::cli::activity::Status),
                                       observer: &mut Option<Observer<'_>>|
         -> Result<TurnOutcome, crate::agent::AgentError> {
            saw_deltas = false;
            *said = crate::hooks::memory::Said::default();
            backend.run_turn(prompt, &mut |event: &AgentEvent| {
                if let Some(observe) = observer.as_mut() {
                    observe(event, &start);
                }
                // What the model said this run, for the transcript and the
                // stop.
                said.hear(event);
                match event {
                    AgentEvent::TextDelta(fragment) => {
                        saw_deltas = true;
                        let visible = filter.push(fragment);
                        if !visible.is_empty() {
                            *mid_line = !visible.ends_with('\n');
                            on_text(&visible);
                        }
                    }
                    AgentEvent::Text(text) => {
                        // For backends that do not stream deltas (e.g. whole-message responses),
                        // render the text unless streaming deltas have already been displayed.
                        if !saw_deltas {
                            let visible = filter.push(text);
                            if !visible.is_empty() {
                                *mid_line = !visible.ends_with('\n');
                                on_text(&visible);
                            }
                        }
                    }
                    AgentEvent::ToolResult {
                        id,
                        output,
                        is_error,
                    } => {
                        // Named by the call that asked for it: a result arrives with
                        // only an id, and an unnamed result is a wall of text with
                        // nothing saying which tool produced it.
                        let (tool, on) = tool_calls
                            .get(id.as_str())
                            .cloned()
                            .unwrap_or_else(|| ("tool".to_string(), None));
                        let width = crate::cli::activity::terminal_width().min(RESULT_WIDTH);
                        // Claimed before anything is drawn, so the turn's tally of
                        // changed files is the same whoever presents the result.
                        let edit_block = edited.returned(id, *is_error, width);
                        // An observer presents results itself, as structure; a
                        // panel drawn into the reply as well would show it twice.
                        if observer.is_some() {
                            return;
                        }
                        // The panel owns whole rows. Without this its rule was
                        // appended to whatever the reply had left unfinished.
                        if *mid_line {
                            on_text("\n");
                            *mid_line = false;
                        }

                        // A SUCCESSFUL edit is shown as the change itself.
                        //
                        // Only on success, and only here: a write can be refused by
                        // the guards, by the agent's own permission mode, or by the
                        // file system, and a diff printed when the call was MADE
                        // would claim a change that never landed. A failure keeps
                        // the ordinary panel, because the error text is the part
                        // worth reading.
                        if let Some(shown) = edit_block {
                            for line in shown {
                                on_text(&format!("{line}\n"));
                            }
                            return;
                        }

                        for line in crate::cli::panel::result(
                            &tool,
                            on.as_deref(),
                            output,
                            *is_error,
                            width,
                        ) {
                            on_text(&format!("{line}\n"));
                        }
                    }
                    AgentEvent::ToolUse { id, name, input } => {
                        tool_calls
                            .insert(id.clone(), (name.clone(), crate::cli::panel::target(input)));
                        // Kept for the result to claim. Parsed HERE because the
                        // call is the only place both sides of the change exist.
                        edited.called(id, input);
                        activity = crate::cli::activity::Activity::from_tool(name, input);
                        on_status(&crate::cli::activity::Status {
                            activity: activity.clone(),
                            elapsed: started.elapsed().as_secs(),
                            tokens: u64::from(turn_usage.total_tokens()),
                            progress: None,
                        });
                    }
                    AgentEvent::Quota {
                        status, resets_at, ..
                    } => *turn_quota = Some((status.clone(), *resets_at)),
                    // incode speaking mid-turn: a line of its own, never part
                    // of the reply. An observer presents it itself (`--acp`,
                    // as a thought), so it is not drawn into the reply too.
                    AgentEvent::Notice(note) if observer.is_none() => {
                        if *mid_line {
                            on_text("\n");
                            *mid_line = false;
                        }
                        on_text(&format!("{note}\n"));
                    }
                    AgentEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_creation_tokens,
                        cache_read_tokens,
                    } => {
                        // Cumulative for the turn: the last event is the truth, so
                        // this REPLACES rather than adds.
                        turn_usage.input_tokens = *input_tokens;
                        turn_usage.output_tokens = *output_tokens;
                        turn_usage.cache_creation_input_tokens = *cache_creation_tokens;
                        turn_usage.cache_read_input_tokens = *cache_read_tokens;
                        on_status(&crate::cli::activity::Status {
                            activity: activity.clone(),
                            elapsed: started.elapsed().as_secs(),
                            // Every token the turn used, cached input
                            // included — the number the budget charges.
                            tokens: u64::from(turn_usage.total_tokens()),
                            progress: None,
                        });
                    }
                    _ => {}
                }
            })
        };

        let mut said = crate::hooks::memory::Said::default();
        let first_result = run_backend_attempt(
            &mut self.backend,
            &prompt,
            &mut said,
            &mut filter,
            &mut turn_usage,
            &mut turn_quota,
            &mut tool_calls,
            &mut edited,
            &mut mid_line,
            on_text,
            on_status,
            &mut observer,
        );
        let (mut attempt, mut agent_failed) = judged(self.backend.as_ref(), first_result);
        let mut failed_agent = self.backend.id().to_string();
        let mut moved_to_index: Option<usize> = None;

        for index in 0..self.standby.len() {
            let Err(ref reason) = attempt else { break };
            if !agent_failed {
                break;
            }
            let took_over = self.standby[index].id().to_string();
            let handover = format!(
                "{failed_agent} could not do it ({}) — {took_over} took over",
                reason.lines().next().unwrap_or(reason).trim()
            );
            crate::trace!("agent", "{handover}");

            start_new_line!(on_text);
            on_text(&format!("\n[{handover}]\n\n"));
            if held_to.is_some() && !self.standby[index].runs_tools_here() {
                self.notices.push(crate::custom_commands::not_enforced_by(
                    &took_over,
                    &self
                        .session_tools
                        .turn_allowlist()
                        .map_or_else(Vec::new, |l| l.listed().to_vec()),
                ));
            }

            // Reset stream filter and turn tracking for the standby agent attempt
            filter = crate::ask::StreamFilter::new();
            turn_usage = crate::runtime::usage::TokenUsage::default();
            tool_calls.clear();
            edited = crate::cli::edits::Watcher::watching(self.workspace.clone());

            let standby_result = run_backend_attempt(
                &mut self.standby[index],
                &prompt,
                &mut said,
                &mut filter,
                &mut turn_usage,
                &mut turn_quota,
                &mut tool_calls,
                &mut edited,
                &mut mid_line,
                on_text,
                on_status,
                &mut observer,
            );
            (attempt, agent_failed) = judged(self.standby[index].as_ref(), standby_result);
            failed_agent = took_over;
            if !agent_failed {
                moved_to_index = Some(index);
                break;
            }
        }

        if let Some(index) = moved_to_index {
            let failed_primary = std::mem::replace(&mut self.backend, self.standby.remove(index));
            self.standby.push(failed_primary);
        }

        let mut outcome = attempt;
        if outcome.is_ok() {
            self.memory
                .said(crate::hooks::memory::Speaker::Agent, &said.whole());
        }

        // What the change left behind — handed BACK TO THE AGENT, which is
        // still holding the context and is the thing that can act on it.
        // Telling only the person means they return, re-explain the task and
        // pay for another turn.
        //
        // ONE follow-up, never a loop: the second pass can itself change files
        // and strand others, and a harness that kept chasing that would spend a
        // subscription on a graph it cannot fully resolve. One pass catches the
        // case this exists for — a backend edited without its callers — and the
        // rest is reported to a person.
        let changed = links.changed_since(&roots);
        let unfinished = links.unfinished(&changed);
        if outcome.is_ok() {
            if let Some(instruction) = crate::links::LinkMap::follow_up(&unfinished) {
                self.notices.push(
                    "Connected files were left untouched — asking the agent to check them."
                        .to_string(),
                );
                self.memory
                    .said(crate::hooks::memory::Speaker::Person, &instruction);
                let mut replied = crate::hooks::memory::Said::default();
                let _ = self
                    .backend
                    .run_turn(&instruction, &mut |event: &AgentEvent| {
                        // The follow-up is part of this turn, so an observer
                        // sees its tool calls as it saw the first pass's.
                        if let Some(observe) = observer.as_mut() {
                            observe(event, &start);
                        }
                        replied.hear(event);
                        match event {
                            AgentEvent::TextDelta(fragment) => on_text(fragment),
                            AgentEvent::Notice(note) if observer.is_none() => {
                                on_text(&format!("\n{note}\n"));
                            }
                            _ => {}
                        }
                    });
                if !replied.last().is_empty() {
                    self.memory
                        .said(crate::hooks::memory::Speaker::Agent, &replied.whole());
                    said = replied;
                }

                // What the follow-up itself left behind is REPORTED, not chased.
                let after = links.changed_since(&roots);
                if let Some(note) = crate::links::LinkMap::report(&links.unfinished(&after)) {
                    self.notices.push(note);
                }
            }
        }

        // THE TURN IS ENDING, and the memory engine is told — it records the
        // turn, and may answer that the agent should carry on. Its answer is
        // then the next thing the model is sent, in the same conversation, on
        // every route: a CLI model resumes its session with it as the next
        // prompt, the API route appends it to the conversation. Bounded by the
        // engine's own brake and by `MAX_CONTINUATIONS`; not asked at all once
        // the turn was stopped or while a background job runs.
        while outcome.is_ok() {
            let last = said.last().to_string();
            // Counted NOW, as the turn ends: a job the model started during
            // it counts, and one that has finished does not.
            self.memory
                .set_background(self.session_tools.running_jobs());
            let Some(reason) = self.memory.stop(&last, self.cancel.as_ref()) else {
                break;
            };
            start_new_line!(on_text);
            on_text(&format!("\n[memory engine: carry on — {reason}]\n\n"));
            let next = run_backend_attempt(
                &mut self.backend,
                &reason,
                &mut said,
                &mut filter,
                &mut turn_usage,
                &mut turn_quota,
                &mut tool_calls,
                &mut edited,
                &mut mid_line,
                on_text,
                on_status,
                &mut observer,
            );
            outcome = judged(self.backend.as_ref(), next).0;
            if outcome.is_ok() {
                self.memory
                    .said(crate::hooks::memory::Speaker::Agent, &said.whole());
            }
        }

        // The operator's `stop` commands, once the turn is over — after any
        // continuation above (`crate::hooks`). Told what the agent said
        // last; what they print joins the turn's notices. They cannot reopen
        // the turn.
        if outcome.is_ok() {
            let told: Vec<String> = match &self.operator_hooks {
                Some(hooks) => hooks
                    .stop(
                        said.last(),
                        self.session_id().unwrap_or(""),
                        Some(&self.workspace),
                    )
                    .messages()
                    .to_vec(),
                None => Vec::new(),
            };
            self.notices
                .extend(told.into_iter().map(|m| format!("[stop hook] {m}")));
        }

        // HOW MUCH MOVED, in one line, once the turn is over.
        //
        // The per-change blocks answer "what changed here"; after a dozen of
        // them nobody can answer "how much changed in total", which is the
        // question that decides whether a turn is reviewable at a glance or
        // needs a diff tool. Files are counted once however often each was
        // edited — see [`crate::cli::edits::Tally`].
        if let Some(summary) = edited.summary() {
            start_new_line!(on_text);
            on_text(&format!("{summary}\n"));
        }

        // Anything the agent put to the owner. Printed as ordinary output, not
        // offered as a selector: a printed menu is answered with one character
        // at the normal prompt, which is what makes it usable by voice.
        let tail = filter.finish();
        if !tail.is_empty() {
            mid_line = !tail.ends_with('\n');
            on_text(&tail);
        }
        // END THE TURN ON A ROW BOUNDARY. A reply stops wherever the model
        // stopped talking, so the last fragment rarely ends in a newline —
        // and then the next thing on screen continued that row: a notice, or
        // the echo of what the operator typed next. The transcript read as one
        // run-on line with the question and the answer inside it.
        //
        // Written out rather than through `start_new_line!` because nothing
        // reads the flag after this, and the macro's reset would be a dead
        // store — which the compiler is right to point at.
        if mid_line {
            on_text("\n");
        }
        let asked = crate::ask::parse(filter.text());
        if !asked.is_empty() {
            for question in &asked {
                self.notices.push(question.render());
            }
            self.pending = asked;
        }

        // Checked before the turn is reported as fine. A protected file a
        // person allowed this turn is theirs, not a breach; taken either way,
        // so an allow never outlives the turn it was given in.
        let allowed = self.session_tools.allowed_writes().take();
        if let Some((sentinel, guards)) = &watched {
            let breaches = sentinel.breaches_allowing(&roots, guards, &allowed);
            if let Some(refusal) = crate::guards::Sentinel::refusal(&breaches) {
                // Said, not thrown: an interactive turn has no ledger to fail,
                // and the person is right there to decide what to do.
                self.notices.push(refusal);
            }
        }

        // The latest the agent reported. Kept across turns so `/status` can show
        // it between turns, not only during one.
        if turn_quota.is_some() {
            self.quota = turn_quota;
        }

        // Added across turns, having been replaced within one.
        self.usage.input_tokens += turn_usage.input_tokens;
        self.usage.output_tokens += turn_usage.output_tokens;
        self.usage.cache_creation_input_tokens += turn_usage.cache_creation_input_tokens;
        self.usage.cache_read_input_tokens += turn_usage.cache_read_input_tokens;
        let model = self
            .model()
            .map_or_else(|| self.backend_id().to_string(), str::to_string);
        if turn_usage.total_tokens() > 0 {
            match self.usage_by_model.iter_mut().find(|(m, _)| *m == model) {
                Some((_, used)) => *used = used.plus(turn_usage),
                None => self.usage_by_model.push((model.clone(), turn_usage)),
            }
        }
        // The operator's collector, when telemetry is on (`crate::telemetry`):
        // the turn's usage and time, exported without anyone waiting.
        crate::telemetry::turn_finished(&model, &turn_usage, started.elapsed().as_secs_f64());
        crate::telemetry::export_in_background();

        // THE TURN'S CHECKPOINT IS SEALED: only what changed is kept, what
        // the turn created is added, and — when the session keeps its
        // checkpoints — it is written to disk for `/undo` after a restart.
        self.checkpoints.finish_turn();

        outcome
    }

    /// Put the files from the last turn back as they were.
    ///
    /// Refuses when the snapshot was incomplete: restoring part of a change
    /// leaves a tree matching neither state.
    pub fn undo(
        &mut self,
        workspace: &Path,
    ) -> Result<crate::tools::checkpoint::UndoOutcome, String> {
        // A turn whose snapshot hit its cap is refused by the store itself:
        // its checkpoint is kept, marked incomplete, for exactly that.
        self.checkpoints.undo_last(workspace).map_err(|e| {
            let said = e.to_string();
            if said.contains("nothing to undo") && self.snapshot_complete.is_none() {
                "there is nothing to undo yet — no turn has changed anything.".to_string()
            } else {
                said
            }
        })
    }

    /// Put the files back as they were `turns` changed turns ago, newest
    /// first: what was undone, and why it stopped early when it did.
    pub fn rewind(
        &mut self,
        workspace: &Path,
        turns: usize,
    ) -> (Vec<crate::tools::checkpoint::UndoOutcome>, Option<String>) {
        self.checkpoints.rewind(workspace, turns)
    }

    /// Keep this session's checkpoints on disk under `session`, so `/undo`
    /// and `/rewind` outlive the process — and start from the turns earlier
    /// sessions in this workspace kept.
    pub fn keep_checkpoints(&mut self, session: &str) {
        self.checkpoints = CheckpointStore::persisted(&self.workspace, &self.roots(), session);
    }

    /// Snapshot the tree. Returns whether it covered everything.
    fn snapshot(&mut self, workspace: &Path, label: &str) -> bool {
        // Every root, not just the workspace: a change made in an added
        // directory has to be undoable too.
        let mut roots = vec![workspace.to_path_buf()];
        roots.extend(self.extra_dirs.iter().cloned());
        self.checkpoints
            .begin_tree(crate::text::head_bytes(label, 60), &roots)
    }
}

/// What an input meant, when a question was waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answered {
    /// Nothing was waiting; the input is an ordinary instruction.
    NothingPending,
    /// Answered, and another question is now showing. No turn is run.
    AskedTheNext(String),
    /// Every question is answered; send this to the agent.
    Ready(String),
}

/// Propose, do not apply. The agent's own name for it.
pub const PLAN_MODE: &str = "plan";
/// Apply changes, within whatever the deny rules allow.
pub const ACT_MODE: &str = "acceptEdits";

/// What the agent is sent: what the harness knows, then what was asked.
///
/// The instruction goes LAST because it is what the agent acts on, and context
/// placed after an instruction reads as an afterthought.
/// The separator between one context block and the next.
const NEWLINES: &str = "

";

#[must_use]
pub fn assemble_prompt(instruction: &str, context: &[String]) -> String {
    let mut prompt = String::new();
    for part in context {
        if part.trim().is_empty() {
            continue;
        }
        prompt.push_str(part);
        prompt.push_str(NEWLINES);
    }
    prompt.push_str(instruction);
    prompt
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    fn asked(prompt: &str) -> crate::ask::Question {
        crate::ask::Question {
            prompt: prompt.to_string(),
            options: vec![
                crate::ask::Choice {
                    label: "First".to_string(),
                    detail: String::new(),
                    recommended: false,
                },
                crate::ask::Choice {
                    label: "Second".to_string(),
                    detail: String::new(),
                    recommended: false,
                },
            ],
        }
    }

    /// **Two questions get two answers.** Measured on the first live run: the
    /// agent asked two things in one reply, and applying one input to both
    /// would answer a question the person never read — "b" is the second option
    /// of the FIRST question and says nothing about the second.
    #[test]
    fn each_question_is_answered_separately() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut s = session(dir.path());
        s.pending = vec![asked("Which database?"), asked("Which auth?")];

        match s.take_answer("a") {
            Answered::AskedTheNext(menu) => assert!(menu.contains("Which auth?"), "got: {menu}"),
            other => panic!("expected the next question, got {other:?}"),
        }

        match s.take_answer("b") {
            Answered::Ready(instruction) => {
                assert!(
                    instruction.contains("Which database?"),
                    "got: {instruction}"
                );
                assert!(instruction.contains("First"), "got: {instruction}");
                assert!(instruction.contains("Which auth?"), "got: {instruction}");
                assert!(instruction.contains("Second"), "got: {instruction}");
            }
            other => panic!("expected a ready instruction, got {other:?}"),
        }
    }

    /// With nothing waiting, a line is an ordinary instruction — a session that
    /// swallowed input as an answer would be unusable.
    #[test]
    fn an_ordinary_line_is_not_treated_as_an_answer() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut s = session(dir.path());
        assert_eq!(s.take_answer("add a login page"), Answered::NothingPending);
    }

    /// Answering clears the queue, so the next ordinary line is not eaten.
    #[test]
    fn the_queue_empties_once_answered() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut s = session(dir.path());
        s.pending = vec![asked("Which?")];
        assert!(matches!(s.take_answer("a"), Answered::Ready(_)));
        assert_eq!(s.take_answer("anything"), Answered::NothingPending);
        assert!(s.pending().is_empty());
    }

    /// **The discriminator.** When an added root's write went through, either
    /// the rules never reached the agent or the glob did not match. This settles
    /// which: it asserts the SESSION hands the backend a rule naming that root.
    #[test]
    fn adding_a_root_gives_the_backend_rules_that_name_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let extra = tempfile::tempdir().expect("extra");
        let session = AgentSession::new(
            dir.path().to_path_buf(),
            "claude-code",
            "claude".to_string(),
            &crate::guards::Guards::default(),
            Vec::new(),
        )
        .with_extra_dirs(vec![extra.path().to_path_buf()]);

        let wanted = extra.path().to_string_lossy().replace(char::from(92), "/");
        let rules = session.deny_rules();
        assert!(
            rules.iter().any(|r| r.contains(&wanted)),
            "no rule names the added root.\nwanted: {wanted}\ngot: {rules:?}"
        );
    }

    /// **A fresh session says there is nothing to undo, not that the tree is
    /// too big.** Measured: `/undo` before any turn reported "the workspace has
    /// more than 5000 files" about a workspace holding one file — a false
    /// statement about the user's own tree, from a flag that meant two things.
    #[test]
    fn undo_before_any_turn_says_there_is_nothing_to_undo() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("only.txt"), "x").expect("one file");
        let mut s = session(dir.path());

        let err = s.undo(dir.path()).expect_err("nothing has run");
        assert!(err.contains("nothing to undo"), "got: {err}");
        assert!(
            !err.contains("5000") && !err.contains("more than"),
            "it must not invent a size for a one-file tree: {err}"
        );
    }
    use super::*;

    fn session(dir: &Path) -> AgentSession {
        AgentSession::new(
            dir.to_path_buf(),
            "claude-code",
            "claude".to_string(),
            &crate::guards::Guards::default(),
            Vec::new(),
        )
    }

    /// **The chosen agent gets its own backend even with the old API
    /// variables set.** Their presence once replaced whatever agent had been
    /// chosen, so the banner named one agent while another did the work; then
    /// they configured a route of their own. A route is an admin.json entry
    /// now, and these variables configure nothing.
    #[test]
    fn the_old_api_variables_do_not_replace_the_chosen_local_agent() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let names = ["INCODE_API_URL", "INCODE_API_KEY"];
        let before: Vec<Option<std::ffi::OsString>> = names.iter().map(std::env::var_os).collect();
        std::env::set_var(names[0], "http://127.0.0.1:9/v1/messages");
        std::env::set_var(names[1], "not-a-real-key");

        let dir = tempfile::tempdir().expect("temp dir");
        let local = session(dir.path()).backend_id().to_string();

        for (name, value) in names.iter().zip(before) {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
        assert_eq!(
            local, "claude-code",
            "the old API variables replaced the chosen local agent"
        );
    }

    /// **Context goes before the instruction.** Context placed after an
    /// instruction reads as an afterthought, and the instruction is the thing
    /// being acted on.
    #[test]
    fn the_instruction_comes_last() {
        let prompt = assemble_prompt(
            "fix the failing test",
            &["<guards>do not edit tests</guards>".to_string()],
        );
        assert!(prompt.ends_with("fix the failing test"), "got: {prompt}");
        assert!(
            prompt.find("<guards>").unwrap() < prompt.find("fix the failing").unwrap(),
            "got: {prompt}"
        );
    }

    /// An empty context contributes nothing — not a run of blank lines before
    /// the instruction.
    #[test]
    fn empty_context_does_not_pad_the_prompt() {
        assert_eq!(assemble_prompt("do it", &[]), "do it");
        assert_eq!(assemble_prompt("do it", &["   ".to_string()]), "do it");
    }

    /// The snapshot is labelled with what was asked, so `/undo` can say which
    /// change it is putting back. A checkpoint that captured no files has no
    /// label — so the fixture must contain something to capture.
    #[test]
    fn a_snapshot_is_labelled_with_what_was_asked() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("code.txt"), "x").expect("a file to capture");
        let mut s = session(dir.path());
        assert!(s.snapshot(dir.path(), "do the thing"));
        assert_eq!(s.pending_label(), Some("do the thing"));
    }

    /// **A tree too big to snapshot switches undo OFF rather than offering
    /// half of one.** A partial restore leaves a tree matching neither state.
    #[test]
    fn an_oversized_tree_refuses_undo_instead_of_half_doing_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        for i in 0..(MAX_SNAPSHOT_FILES + 10) {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "x").expect("write");
        }
        let mut s = session(dir.path());
        assert!(
            !s.snapshot(dir.path(), "big"),
            "the walk must report that it did not finish"
        );
        s.snapshot_complete = Some(false);
        let err = s.undo(dir.path()).expect_err("undo must refuse");
        assert!(err.contains("not available"), "got: {err}");
        assert!(
            err.contains("partial"),
            "it must say WHY refusing beats half-doing it: {err}"
        );
    }

    /// The snapshot skips build output — a tree with a large `target/` is
    /// ordinary, and walking it would make every turn slow for nothing.
    #[test]
    fn build_output_is_not_snapshotted() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("target")).expect("target");
        for i in 0..(MAX_SNAPSHOT_FILES + 10) {
            std::fs::write(dir.path().join("target").join(format!("o{i}.bin")), "x")
                .expect("write");
        }
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").expect("source");

        let mut s = session(dir.path());
        assert!(
            s.snapshot(dir.path(), "work"),
            "build output must not push the walk past its cap"
        );
    }

    /// An undoable turn restores what the agent changed.
    #[test]
    fn a_snapshotted_file_is_restored() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("code.txt");
        std::fs::write(&file, "original").expect("write");

        let mut s = session(dir.path());
        assert!(s.snapshot(dir.path(), "change it"));
        s.snapshot_complete = Some(true);

        // What a delegated agent would do, out of the harness's sight.
        std::fs::write(&file, "changed by the agent").expect("agent writes");

        s.undo(dir.path()).expect("undo");
        assert_eq!(
            std::fs::read_to_string(&file).expect("read"),
            "original",
            "the turn must be undoable even though the harness never saw the write"
        );
    }

    struct MockExhaustedBackend(&'static str);

    impl AgentBackend for MockExhaustedBackend {
        fn id(&self) -> &str {
            self.0
        }
        fn run_turn(
            &mut self,
            _prompt: &str,
            _on_event: &mut dyn FnMut(&AgentEvent),
        ) -> Result<TurnOutcome, crate::agent::AgentError> {
            Err(crate::agent::AgentError::new(
                "You've reached your weekly limit for Claude Code",
            ))
        }
        fn session_id(&self) -> Option<&str> {
            None
        }
    }

    struct MockRescuerBackend(&'static str, String);

    impl AgentBackend for MockRescuerBackend {
        fn id(&self) -> &str {
            self.0
        }
        fn run_turn(
            &mut self,
            _prompt: &str,
            on_event: &mut dyn FnMut(&AgentEvent),
        ) -> Result<TurnOutcome, crate::agent::AgentError> {
            on_event(&AgentEvent::TextDelta(self.1.clone()));
            Ok(TurnOutcome {
                session_id: "rescued-123".to_string(),
                stop_reason: "end_turn".to_string(),
                is_error: false,
                detail: None,
                served_by: vec![self.0.to_string()],
            })
        }
        fn session_id(&self) -> Option<&str> {
            Some("rescued-123")
        }
    }

    struct MockTaskFailureBackend(&'static str);

    impl AgentBackend for MockTaskFailureBackend {
        fn id(&self) -> &str {
            self.0
        }
        fn run_turn(
            &mut self,
            _prompt: &str,
            _on_event: &mut dyn FnMut(&AgentEvent),
        ) -> Result<TurnOutcome, crate::agent::AgentError> {
            Err(crate::agent::AgentError::new(
                "`cargo test` exited with code 1",
            ))
        }
        fn session_id(&self) -> Option<&str> {
            None
        }
    }

    #[test]
    fn interactive_session_fails_over_to_standby_on_weekly_limit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut s = session(dir.path())
            .with_backend(Box::new(MockExhaustedBackend("claude-code")))
            .with_standby(vec![Box::new(MockRescuerBackend(
                "antigravity",
                "rescued by gemini".to_string(),
            ))]);

        let mut output = String::new();
        let outcome = s.turn(
            dir.path(),
            "do something",
            &[],
            &mut |text| output.push_str(text),
            &mut |_| {},
        );

        assert!(
            outcome.is_ok(),
            "turn must succeed via standby: {outcome:?}"
        );
        assert!(output.contains("claude-code could not do it"));
        assert!(output.contains("antigravity took over"));
        assert!(output.contains("rescued by gemini"));
        assert_eq!(
            s.backend_id(),
            "antigravity",
            "standby must be promoted to primary backend"
        );
    }

    #[test]
    fn interactive_session_does_not_failover_on_task_side_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut s = session(dir.path())
            .with_backend(Box::new(MockTaskFailureBackend("claude-code")))
            .with_standby(vec![Box::new(MockRescuerBackend(
                "antigravity",
                "should not be called".to_string(),
            ))]);

        let mut output = String::new();
        let outcome = s.turn(
            dir.path(),
            "do something",
            &[],
            &mut |text| output.push_str(text),
            &mut |_| {},
        );

        assert!(
            outcome.is_err(),
            "task failure must fail without moving to standby"
        );
        assert_eq!(
            s.backend_id(),
            "claude-code",
            "primary backend must remain in place"
        );
        assert!(!output.contains("antigravity took over"));
    }

    #[test]
    fn interactive_session_without_standby_returns_error_on_limit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut s = session(dir.path()).with_backend(Box::new(MockExhaustedBackend("claude-code")));

        let mut output = String::new();
        let outcome = s.turn(
            dir.path(),
            "do something",
            &[],
            &mut |text| output.push_str(text),
            &mut |_| {},
        );

        let err = outcome.expect_err("must error when no standby exists");
        assert!(err.contains("weekly limit"), "got: {err}");
        assert!(!output.contains("took over"));
    }

    // -----------------------------------------------------------------------
    // The memory engine's session lifecycle, fired by the session.
    // -----------------------------------------------------------------------

    use crate::hooks::engine::fake::{self, Reply};
    use crate::hooks::memory::{MemoryHooks, MAX_CONTINUATIONS};
    use std::sync::{Arc, Mutex};

    const CARRY_ON: &str = r#"{"decision":"block","reason":"now run the tests"}"#;

    /// What a [`Recording`] backend was sent and told.
    #[derive(Default)]
    struct Seen {
        prompts: Vec<String>,
        context: Vec<String>,
        memory_sessions: Vec<String>,
    }

    /// A backend that writes down every prompt and answers each with text.
    /// `on_run` is called with the run's number before it answers.
    struct Recording {
        seen: Arc<Mutex<Seen>>,
        on_run: Box<dyn FnMut(usize)>,
    }

    impl AgentBackend for Recording {
        fn id(&self) -> &str {
            "claude-code"
        }
        fn run_turn(
            &mut self,
            prompt: &str,
            on_event: &mut dyn FnMut(&AgentEvent),
        ) -> Result<TurnOutcome, crate::agent::AgentError> {
            let n = {
                let mut seen = self.seen.lock().expect("seen");
                seen.prompts.push(prompt.to_string());
                seen.prompts.len()
            };
            (self.on_run)(n);
            on_event(&AgentEvent::Text(format!("reply {n}")));
            Ok(TurnOutcome {
                session_id: String::new(),
                stop_reason: "end_turn".to_string(),
                is_error: false,
                detail: None,
                served_by: Vec::new(),
            })
        }
        fn session_id(&self) -> Option<&str> {
            None
        }
        fn set_system_context(&mut self, blocks: Vec<String>) {
            self.seen.lock().expect("seen").context = blocks;
        }
        fn set_memory_session(&mut self, session: &str) {
            self.seen
                .lock()
                .expect("seen")
                .memory_sessions
                .push(session.to_string());
        }
    }

    /// A backend that starts its reply, has incode say something mid-turn,
    /// and finishes the reply.
    struct Noticing;

    impl AgentBackend for Noticing {
        fn id(&self) -> &str {
            "claude-code"
        }
        fn run_turn(
            &mut self,
            _prompt: &str,
            on_event: &mut dyn FnMut(&AgentEvent),
        ) -> Result<TurnOutcome, crate::agent::AgentError> {
            on_event(&AgentEvent::TextDelta("half a ".to_string()));
            on_event(&AgentEvent::Notice("[pre_compact hook] noted".to_string()));
            on_event(&AgentEvent::TextDelta("reply".to_string()));
            on_event(&AgentEvent::Text("half a reply".to_string()));
            Ok(TurnOutcome {
                session_id: String::new(),
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

    /// A backend whose turn uses 100 tokens in and 20 out.
    struct Spending;

    impl AgentBackend for Spending {
        fn id(&self) -> &str {
            "claude-code"
        }
        fn run_turn(
            &mut self,
            _prompt: &str,
            on_event: &mut dyn FnMut(&AgentEvent),
        ) -> Result<TurnOutcome, crate::agent::AgentError> {
            on_event(&AgentEvent::Usage {
                input_tokens: 100,
                output_tokens: 20,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
            });
            on_event(&AgentEvent::Text("done".to_string()));
            Ok(TurnOutcome {
                session_id: String::new(),
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

    /// **A FINISHED TURN'S USAGE GOES TO THE OPERATOR'S COLLECTOR**
    /// (`crate::telemetry`), under the model that served it, as one turn.
    #[test]
    fn a_finished_turn_records_its_tokens_and_one_turn() {
        let ws = tempfile::tempdir().expect("ws");
        let mut s = AgentSession::new(
            ws.path().to_path_buf(),
            "claude-code",
            "claude".to_string(),
            &crate::guards::Guards::default(),
            Vec::new(),
        )
        .with_backend(Box::new(Spending));
        let (outcome, metrics, _) = crate::telemetry::tests::captured(|| {
            s.turn(ws.path(), "go", &[], &mut |_| {}, &mut |_| {})
        });
        assert!(outcome.is_ok(), "{outcome:?}");
        let metrics = metrics.expect("the turn was recorded");
        let sum =
            |name, pairs: &[(&str, &str)]| crate::telemetry::tests::sum_of(&metrics, name, pairs);
        assert_eq!(
            sum(
                "incode.token.usage",
                &[("type", "input"), ("model", "claude-code")]
            ),
            100.0,
            "{metrics}"
        );
        assert_eq!(sum("incode.token.usage", &[("type", "output")]), 20.0);
        assert_eq!(sum("incode.turn.count", &[]), 1.0);
    }

    /// **incode's NOTICE IS SHOWN ON A LINE OF ITS OWN**, even when it lands
    /// mid-reply, and is not part of the reply.
    #[test]
    fn a_notice_mid_turn_is_shown_on_its_own_line() {
        let ws = tempfile::tempdir().expect("ws");
        let mut s = AgentSession::new(
            ws.path().to_path_buf(),
            "claude-code",
            "claude".to_string(),
            &crate::guards::Guards::default(),
            Vec::new(),
        )
        .with_backend(Box::new(Noticing));
        let mut shown = String::new();
        let outcome = s.turn(
            ws.path(),
            "go",
            &[],
            &mut |t| shown.push_str(t),
            &mut |_| {},
        );
        assert!(outcome.is_ok(), "{outcome:?}");
        assert!(
            shown.contains("half a \n[pre_compact hook] noted\nreply"),
            "{shown:?}"
        );
    }

    /// A session over `ws` whose backend records into `seen`, with the
    /// memory lifecycle opened on the fake engine in `engine`.
    fn with_memory(
        ws: &Path,
        engine: &Path,
        replies: &[(&str, Reply)],
        seen: &Arc<Mutex<Seen>>,
        on_run: Box<dyn FnMut(usize)>,
    ) -> AgentSession {
        let bin = fake::write(engine, replies);
        let engine = fake::engine(bin).capped(std::time::Duration::from_secs(20));
        let mut s = AgentSession::new(
            ws.to_path_buf(),
            "claude-code",
            "claude".to_string(),
            &crate::guards::Guards::default(),
            vec!["BASE".to_string(), "NATIVE-RULES".to_string()],
        )
        .with_backend(Box::new(Recording {
            seen: Arc::clone(seen),
            on_run,
        }));
        s.start_memory(
            MemoryHooks::new(Some(engine), ws, "sess-old"),
            vec!["NATIVE-RULES".to_string()],
            "startup",
        );
        s
    }

    /// **`/style` REACHES THE AGENT.** The old style's block goes, the new one
    /// comes, the rest of the standing context stays, and there is never more
    /// than one style.
    #[test]
    fn an_output_style_swap_reaches_the_backend_and_keeps_the_rest() {
        let ws = tempfile::tempdir().expect("ws");
        let seen = Arc::new(Mutex::new(Seen::default()));
        let style = |name: &str, body: &str| {
            crate::styles::block(&crate::styles::Style {
                name: name.into(),
                description: None,
                body: body.into(),
                source: PathBuf::from(format!("{name}.md")),
                origin: crate::styles::Origin::Operator,
            })
        };
        let mut s = AgentSession::new(
            ws.path().to_path_buf(),
            "claude-code",
            "claude".to_string(),
            &crate::guards::Guards::default(),
            vec!["BASE".to_string(), style("concise", "Be brief.")],
        )
        .with_backend(Box::new(Recording {
            seen: Arc::clone(&seen),
            on_run: Box::new(|_| {}),
        }));
        let styles_in = |seen: &Arc<Mutex<Seen>>| {
            seen.lock()
                .expect("seen")
                .context
                .iter()
                .filter(|b| crate::styles::is_block(b))
                .cloned()
                .collect::<Vec<_>>()
        };

        s.set_output_style(Some(style("teacher", "Explain why.")));
        let found = styles_in(&seen);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("Explain why."), "{found:?}");
        assert_eq!(seen.lock().expect("seen").context[0], "BASE");

        s.set_output_style(None);
        assert!(styles_in(&seen).is_empty());
        assert_eq!(seen.lock().expect("seen").context, ["BASE"]);
    }

    /// An operator hook command that prints `out`, writes `why` to stderr and
    /// exits with `code`, as the shell runs it.
    fn operator_hook(out: &str, why: &str, code: i32) -> String {
        if cfg!(windows) {
            format!("echo {out}& echo {why} 1>&2& exit /b {code}")
        } else {
            format!("echo '{out}'; echo '{why}' >&2; exit {code}")
        }
    }

    /// **A REFUSED LINE REACHES NOTHING.** The operator's `user_prompt_submit`
    /// hook exits 2: the plan is a refusal with its reason, and the memory
    /// engine never hears the line. Allowed, the hook's output rides the turn
    /// and the engine does hear it.
    #[test]
    fn a_prompt_refused_by_the_operators_hook_reaches_neither_engine_nor_agent() {
        for (code, refused) in [(2, true), (0, false)] {
            let ws = tempfile::tempdir().expect("ws");
            let engine = tempfile::tempdir().expect("engine");
            let seen = Arc::new(Mutex::new(Seen::default()));
            let mut s = with_memory(ws.path(), engine.path(), &[], &seen, Box::new(|_| {}));
            s.set_operator_hooks(Some(crate::hooks::HookRunner::new(
                crate::hooks::HookConfig {
                    user_prompt_submit: vec![operator_hook("checked", "no secrets", code)],
                    ..crate::hooks::HookConfig::default()
                },
            )));
            let plan = crate::cli::plan_turn(&mut s, ws.path(), "fix it", "fix it");
            let heard = fake::spawned(engine.path(), "user-prompt");
            if refused {
                match plan {
                    crate::cli::TurnPlan::Refused(why) => assert_eq!(why, "no secrets"),
                    _ => panic!("exit 2 did not refuse the line"),
                }
                assert_eq!(heard, 0, "the engine heard a refused line");
            } else {
                match plan {
                    crate::cli::TurnPlan::Agent { context, .. } => {
                        assert!(context.iter().any(|c| c.contains("checked")), "{context:?}")
                    }
                    _ => panic!("an allowing hook did not let the line through"),
                }
                assert_eq!(heard, 1, "the engine did not hear an allowed line");
            }
            assert!(seen.lock().expect("seen").prompts.is_empty());
        }
    }

    /// A repository with one commit and one untracked file: a change to
    /// review.
    fn repo_with_a_change() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("ws");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .status()
                .expect("git runs");
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "first"]);
        std::fs::write(dir.path().join("new.rs"), "fn new() {}\n").expect("write");
        dir
    }

    /// **`/review` GOES THROUGH THE OPERATOR'S PROMPT HOOK.** Exit 2 sends
    /// nothing to the agent and says why; allowed, the hook's output rides
    /// the review turn.
    #[test]
    fn a_review_refused_by_the_operators_hook_reaches_no_agent() {
        for (code, refused) in [(2, true), (0, false)] {
            let ws = repo_with_a_change();
            let engine = tempfile::tempdir().expect("engine");
            let seen = Arc::new(Mutex::new(Seen::default()));
            let mut s = with_memory(ws.path(), engine.path(), &[], &seen, Box::new(|_| {}));
            s.set_operator_hooks(Some(crate::hooks::HookRunner::new(
                crate::hooks::HookConfig {
                    user_prompt_submit: vec![operator_hook("checked", "no secrets", code)],
                    ..crate::hooks::HookConfig::default()
                },
            )));
            let ran = crate::cli::review_turn(&mut s, ws.path(), None);
            let prompts = seen.lock().expect("seen").prompts.clone();
            if refused {
                let why = ran.expect_err("exit 2 did not refuse the review");
                assert!(why.contains("no secrets"), "{why}");
                assert!(prompts.is_empty(), "a refused review reached the agent");
            } else {
                assert!(ran.is_ok(), "{ran:?}");
                assert_eq!(prompts.len(), 1, "{prompts:?}");
                assert!(
                    prompts[0].contains("new.rs") && prompts[0].contains("checked"),
                    "{}",
                    prompts[0]
                );
            }
        }
    }

    /// **A STOP HOOK RUNS WHEN THE TURN ENDS**, and what it prints is shown
    /// with the turn's notices; without one, no such notice appears.
    #[test]
    fn a_stop_hook_runs_after_the_turn_and_its_output_is_shown() {
        let ws = tempfile::tempdir().expect("ws");
        let engine = tempfile::tempdir().expect("engine");
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut s = with_memory(ws.path(), engine.path(), &[], &seen, Box::new(|_| {}));
        s.set_operator_hooks(Some(crate::hooks::HookRunner::new(
            crate::hooks::HookConfig {
                stop: vec![operator_hook("turn noted", "", 0)],
                ..crate::hooks::HookConfig::default()
            },
        )));
        one_turn(&mut s, ws.path(), "do the work");
        let notices = s.take_notices();
        assert!(
            notices.iter().any(|n| n == "[stop hook] turn noted"),
            "{notices:?}"
        );

        s.set_operator_hooks(None);
        one_turn(&mut s, ws.path(), "again");
        assert!(!s
            .take_notices()
            .iter()
            .any(|n| n.starts_with("[stop hook]")));
    }

    /// **THE SESSION'S START AND END HOOKS RUN IN ITS WORKSPACE**, and what
    /// they print comes back for the caller to show; without hooks, nothing.
    #[test]
    fn session_start_and_end_hooks_run_in_the_workspace_and_are_shown() {
        let ws = tempfile::tempdir().expect("ws");
        let engine = tempfile::tempdir().expect("engine");
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut s = with_memory(ws.path(), engine.path(), &[], &seen, Box::new(|_| {}));
        // Prints the directory it ran in: the workspace, or the hook was
        // given the wrong `cwd`.
        let here = if cfg!(windows) { "cd" } else { "pwd" };
        s.set_operator_hooks(Some(crate::hooks::HookRunner::new(
            crate::hooks::HookConfig {
                session_start: vec![here.to_string()],
                session_end: vec![operator_hook("bye", "", 2)],
                ..crate::hooks::HookConfig::default()
            },
        )));
        let started = s.operator_session_start("startup");
        let ran_in = std::fs::canonicalize(started.first().expect("start printed nothing"))
            .expect("the hook printed a directory");
        assert_eq!(
            ran_in,
            std::fs::canonicalize(ws.path()).expect("ws"),
            "{started:?}"
        );
        assert_eq!(s.operator_session_end("prompt_input_exit"), ["bye"]);

        s.set_operator_hooks(None);
        assert!(s.operator_session_start("startup").is_empty());
        assert!(s.operator_session_end("other").is_empty());
    }

    fn start_context(text: &str) -> String {
        serde_json::json!({
            "hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": text}
        })
        .to_string()
    }

    /// One turn, as the REPL and ACP run it: the person's line is handed to
    /// the engine first, then the turn.
    fn one_turn(s: &mut AgentSession, ws: &Path, line: &str) -> String {
        let _ = s.memory_prompt(line);
        let mut shown = String::new();
        let outcome = s.turn(ws, line, &[], &mut |t| shown.push_str(t), &mut |_| {});
        assert!(outcome.is_ok(), "{outcome:?}");
        shown
    }

    /// **THE SESSION-START BLOCK REACHES WHAT THE MODEL IS SENT, IN PLACE OF
    /// THE NATIVE BLOCKS IT OVERLAPS.** Every backend is told the session's id.
    #[test]
    fn the_session_start_context_replaces_the_native_blocks() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("e"),
        );
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut s = with_memory(
            ws.path(),
            engine.path(),
            &[(
                "session-start",
                Reply::exit(0).stdout(&start_context("START-X")),
            )],
            &seen,
            Box::new(|_| {}),
        );
        // The block rides from the first turn: the person's line waits for it.
        let _ = s.memory_prompt("go");
        let seen = seen.lock().expect("seen");
        assert_eq!(seen.context.len(), 2, "{:?}", seen.context);
        assert_eq!(seen.context[0], "BASE");
        assert!(seen.context[1].ends_with("START-X"), "{:?}", seen.context);
        assert!(
            seen.context[1].contains("`sess-old`"),
            "the model is told the session's id: {:?}",
            seen.context
        );
        assert_eq!(seen.memory_sessions, vec!["sess-old".to_string()]);
        let start = &fake::events(engine.path(), "session-start")[0];
        assert_eq!(start["source"], "startup");
    }

    /// And when the engine says nothing, the native blocks stay where they
    /// were: nothing is replaced by nothing.
    #[test]
    fn without_a_session_start_answer_the_native_blocks_stay() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("e"),
        );
        let seen = Arc::new(Mutex::new(Seen::default()));
        let _s = with_memory(
            ws.path(),
            engine.path(),
            &[("session-start", Reply::exit(0))],
            &seen,
            Box::new(|_| {}),
        );
        assert!(
            seen.lock().expect("seen").context.is_empty(),
            "the backend's context was not touched"
        );
    }

    /// **A SLOW SESSION-START DOES NOT HOLD THE SESSION OPEN; THE FIRST TURN
    /// CARRIES ITS BLOCK.** Opening the memory session returns while the
    /// engine is still answering (`session/new` is answered from here); the
    /// first person's line waits for the answer, so the block is in what the
    /// model is sent from that turn on, and the engine hears session-start
    /// before user-prompt.
    #[test]
    fn a_slow_session_start_is_waited_for_by_the_first_turn_not_the_opening() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("e"),
        );
        let seen = Arc::new(Mutex::new(Seen::default()));
        let began = std::time::Instant::now();
        let mut s = with_memory(
            ws.path(),
            engine.path(),
            &[
                (
                    "session-start",
                    Reply::exit(0).stdout(&start_context("START-SLOW")).after(3),
                ),
                ("user-prompt", Reply::exit(0)),
            ],
            &seen,
            Box::new(|_| {}),
        );
        let opened = began.elapsed();
        assert!(
            opened < std::time::Duration::from_secs(2),
            "opening waited for session-start: {opened:?}"
        );
        assert!(
            seen.lock().expect("seen").context.is_empty(),
            "a block arrived before the engine answered"
        );
        let _ = s.memory_prompt("do the work");
        let context = seen.lock().expect("seen").context.clone();
        assert!(
            context.last().is_some_and(|c| c.ends_with("START-SLOW")),
            "{context:?}"
        );
        assert_eq!(fake::events(engine.path(), "session-start").len(), 1);
        assert_eq!(fake::events(engine.path(), "user-prompt").len(), 1);
    }

    /// **A STOP THAT SAYS CARRY ON CONTINUES THE TURN ONCE, THEN ENDS.** The
    /// engine's words are the next prompt to the same backend; the second
    /// stop carries the brake; when the engine says nothing the turn ends.
    #[test]
    fn a_stop_block_continues_the_turn_once_then_ends() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("e"),
        );
        let seen = Arc::new(Mutex::new(Seen::default()));
        let answers = engine.path().join("capture.out");
        let mut s = with_memory(
            ws.path(),
            engine.path(),
            &[
                ("user-prompt", Reply::exit(0)),
                ("capture", Reply::exit(0).stdout(CARRY_ON)),
            ],
            &seen,
            // The continuation empties the engine's answer: its stop ends it.
            Box::new(move |n| {
                if n == 2 {
                    std::fs::write(&answers, "").expect("quiet the engine");
                }
            }),
        );
        let shown = one_turn(&mut s, ws.path(), "go");

        let prompts = seen.lock().expect("seen").prompts.clone();
        assert_eq!(
            prompts,
            vec!["go".to_string(), "now run the tests".to_string()]
        );
        let stops = fake::events(engine.path(), "capture");
        assert_eq!(stops.len(), 2, "{stops:?}");
        assert_eq!(stops[0]["stop_hook_active"], false);
        assert_eq!(stops[0]["last_assistant_message"], "reply 1");
        assert_eq!(stops[1]["stop_hook_active"], true);
        assert_eq!(stops[1]["last_assistant_message"], "reply 2");
        assert!(shown.contains("now run the tests"), "{shown}");
    }

    /// **THE CAP HOLDS.** An engine that always says carry on gets
    /// `MAX_CONTINUATIONS` more runs and no more.
    #[test]
    fn the_continuation_cap_holds_in_a_turn() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("e"),
        );
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut s = with_memory(
            ws.path(),
            engine.path(),
            &[
                ("user-prompt", Reply::exit(0)),
                ("capture", Reply::exit(0).stdout(CARRY_ON)),
            ],
            &seen,
            Box::new(|_| {}),
        );
        let _ = one_turn(&mut s, ws.path(), "go");
        let runs = seen.lock().expect("seen").prompts.len();
        assert_eq!(runs, 1 + usize::from(MAX_CONTINUATIONS));
    }

    /// **A TURN STOPPED BEFORE ITS STOP SPAWNS NOTHING.**
    #[test]
    fn a_cancelled_turn_never_starts_the_stop_hook() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("e"),
        );
        let seen = Arc::new(Mutex::new(Seen::default()));
        let token = crate::runtime::cancel::CancelToken::new();
        let pressed = token.clone();
        let mut s = with_memory(
            ws.path(),
            engine.path(),
            &[
                ("user-prompt", Reply::exit(0)),
                ("capture", Reply::exit(0).stdout(CARRY_ON)),
            ],
            &seen,
            // The person presses stop while the model is working.
            Box::new(move |_| pressed.cancel()),
        )
        .with_cancel(token);
        let _ = one_turn(&mut s, ws.path(), "go");
        // Counted at the spawn: a process killed before its first line would
        // never have counted itself.
        assert_eq!(fake::spawned(engine.path(), "capture"), 0);
        assert_eq!(seen.lock().expect("seen").prompts.len(), 1);
    }

    /// **WHILE A BACKGROUND JOB RUNS THE STOP IS NOT SENT**; with none
    /// running it is. A real job, in the session's own job table — the one
    /// the model's `Bash` with `run_in_background` and the REPL's `/bg` use.
    #[test]
    fn a_running_background_job_holds_the_stop() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("e"),
        );
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut s = with_memory(
            ws.path(),
            engine.path(),
            &[("user-prompt", Reply::exit(0)), ("capture", Reply::exit(0))],
            &seen,
            Box::new(|_| {}),
        );
        let long = if cfg!(windows) {
            "ping -n 31 127.0.0.1 > nul"
        } else {
            "sleep 30"
        };
        let job = s
            .session_tools()
            .jobs()
            .spawn(long, ws.path(), &crate::sandbox::Sandbox::off())
            .expect("the job starts");
        let _ = one_turn(&mut s, ws.path(), "start the build");
        assert_eq!(fake::calls(engine.path(), "capture"), 0);
        assert!(s.session_tools().jobs().kill(job), "the job was running");
        let _ = one_turn(&mut s, ws.path(), "is it done");
        assert_eq!(fake::calls(engine.path(), "capture"), 1);
    }

    /// **`/clear` ENDS THE OLD SESSION BY ITS OWN ID**, opens the next as a
    /// clear, and every backend files what follows under the new id — with
    /// the new session's context in place of the old one's.
    #[test]
    fn clearing_ends_the_old_id_and_starts_a_new_one() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("e"),
        );
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut s = with_memory(
            ws.path(),
            engine.path(),
            &[
                ("session-end", Reply::exit(0)),
                (
                    "session-start",
                    Reply::exit(0).stdout(&start_context("START-X")),
                ),
            ],
            &seen,
            Box::new(|_| {}),
        );
        s.clear();

        let end = &fake::events(engine.path(), "session-end")[0];
        assert_eq!(end["session_id"], "sess-old");
        assert_eq!(end["reason"], "clear");
        let starts = fake::events(engine.path(), "session-start");
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[1]["source"], "clear");
        let fresh = starts[1]["session_id"].as_str().expect("an id").to_string();
        assert_ne!(fresh, "sess-old");
        let seen = seen.lock().expect("seen");
        assert_eq!(seen.memory_sessions.last(), Some(&fresh));
        assert_eq!(seen.context.len(), 2, "{:?}", seen.context);
        assert_eq!(seen.context[0], "BASE");
        assert!(
            seen.context[1].ends_with("START-X") && seen.context[1].contains(&fresh),
            "{:?}",
            seen.context
        );
    }

    // -----------------------------------------------------------------------
    // A custom command's `allowed-tools`, held for one turn.
    // -----------------------------------------------------------------------

    /// The allowlist each turn ran under, as written; `None` for a turn with
    /// none.
    type HeldTo = Arc<Mutex<Vec<Option<Vec<String>>>>>;

    /// A backend that writes down, each turn, the allowlist in force on the
    /// session tools it was handed.
    struct ToolsSpy {
        runs_tools_here: bool,
        state: Option<Arc<crate::toolbox::SessionTools>>,
        seen: HeldTo,
    }

    impl AgentBackend for ToolsSpy {
        fn id(&self) -> &str {
            "spy"
        }
        fn run_turn(
            &mut self,
            _prompt: &str,
            _on_event: &mut dyn FnMut(&AgentEvent),
        ) -> Result<TurnOutcome, crate::agent::AgentError> {
            let held = self
                .state
                .as_ref()
                .and_then(|s| s.turn_allowlist())
                .map(|list| list.listed().to_vec());
            self.seen.lock().expect("seen").push(held);
            Ok(TurnOutcome {
                session_id: String::new(),
                stop_reason: "end_turn".to_string(),
                is_error: false,
                detail: None,
                served_by: Vec::new(),
            })
        }
        fn session_id(&self) -> Option<&str> {
            None
        }
        fn share_session_tools(&mut self, state: Arc<crate::toolbox::SessionTools>) {
            self.state = Some(state);
        }
        fn runs_tools_here(&self) -> bool {
            self.runs_tools_here
        }
    }

    fn spied(dir: &Path, runs_tools_here: bool) -> (AgentSession, HeldTo) {
        let seen = Arc::default();
        let s = session(dir).with_backend(Box::new(ToolsSpy {
            runs_tools_here,
            state: None,
            seen: Arc::clone(&seen),
        }));
        (s, seen)
    }

    /// **ONE TURN, NOT THE SESSION.** The restriction is in force while the
    /// turn it was set for runs, lifted when it ends, and the next turn runs
    /// with none.
    #[test]
    fn a_command_restriction_holds_its_turn_and_no_other() {
        let dir = tempfile::tempdir().expect("ws");
        let (mut s, seen) = spied(dir.path(), true);
        s.restrict_next_turn(vec!["Read".to_string(), "Bash(npm test:*)".to_string()]);
        s.turn(dir.path(), "first", &[], &mut |_| {}, &mut |_| {})
            .expect("turn");
        assert!(s.session_tools().turn_allowlist().is_none(), "lifted");
        s.turn(dir.path(), "second", &[], &mut |_| {}, &mut |_| {})
            .expect("turn");
        assert_eq!(
            *seen.lock().expect("seen"),
            vec![
                Some(vec!["Read".to_string(), "Bash(npm test:*)".to_string()]),
                None
            ]
        );
        assert!(
            !s.take_notices().iter().any(|n| n.contains("NOT enforced")),
            "enforced where incode runs the tools"
        );
    }

    /// An agent that runs its own tools is not held to it, and the turn says
    /// so rather than letting the list read as enforced.
    #[test]
    fn a_command_restriction_on_an_agent_with_its_own_tools_is_said_unenforced() {
        let dir = tempfile::tempdir().expect("ws");
        let (mut s, _seen) = spied(dir.path(), false);
        s.restrict_next_turn(vec!["Read".to_string()]);
        s.turn(dir.path(), "first", &[], &mut |_| {}, &mut |_| {})
            .expect("turn");
        let notices = s.take_notices();
        assert!(
            notices
                .iter()
                .any(|n| n.contains("allowed-tools (Read) was NOT enforced") && n.contains("spy")),
            "{notices:?}"
        );
    }
}
