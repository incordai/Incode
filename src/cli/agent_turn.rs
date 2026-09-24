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
use crate::tools::checkpoint::CheckpointStore;

/// How many files a pre-turn snapshot will hold.
///
/// Bounds the ACTION — the snapshot — and never the report: a tree over this is
/// told that undo is off for the turn, not quietly given half of one.
pub const MAX_SNAPSHOT_FILES: usize = 5_000;

/// Directories that never hold hand-written source and are enormous.
const SKIP_DIRS: [&str; 5] = ["target", "node_modules", "dist", ".git", ".incord"];

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
}

/// How wide a tool result panel is drawn in a conversation.
const RESULT_WIDTH: usize = 100;

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
        let backend: Box<dyn AgentBackend> =
            crate::cli::route_backend(&workspace, agent_id, &binary, guards, &system_context, true);
        Self {
            backend,
            standby: Vec::new(),
            workspace: workspace_for_self,
            checkpoints: CheckpointStore::new(),
            snapshot_complete: None,
            cancel: None,
            turns: 0,
            quota: None,
            notices: Vec::new(),
            pending: Vec::new(),
            answers: Vec::new(),
            extra_dirs: Vec::new(),
            usage: crate::runtime::usage::TokenUsage::default(),
        }
    }

    /// Configure standby agents to take over if the primary backend fails.
    #[must_use]
    pub fn with_standby(mut self, mut standby: Vec<Box<dyn AgentBackend>>) -> Self {
        for s in &mut standby {
            if let Some(token) = &self.cancel {
                s.set_cancel(token.clone());
            }
            s.set_extra_dirs(self.extra_dirs.clone());
        }
        self.standby = standby;
        self
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

    #[cfg(test)]
    #[must_use]
    pub fn with_backend(mut self, backend: Box<dyn AgentBackend>) -> Self {
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

    /// The model the next turn will ask for.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.backend.model()
    }

    /// Start a fresh agent session on the next turn.
    ///
    /// Really cleared: the id is dropped, so the next turn does not resume the
    /// conversation the user just asked to be rid of.
    pub fn clear(&mut self) {
        self.backend.clear_session();
        for s in &mut self.standby {
            s.clear_session();
        }
    }

    /// How much the agent may do on its own for the following turns.
    pub fn set_permission_mode(&mut self, mode: &str) {
        self.backend.set_permission_mode(mode);
        for s in &mut self.standby {
            s.set_permission_mode(mode);
        }
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
        // Cleared first: a Ctrl-C pressed while the prompt was waiting belongs
        // to no turn, and letting it leak in would cancel the next one instantly.
        if let Some(token) = &self.cancel {
            token.reset();
        }
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
                                       on_status: &mut dyn FnMut(&crate::cli::activity::Status)|
         -> Result<TurnOutcome, crate::agent::AgentError> {
            saw_deltas = false;
            backend.run_turn(&prompt, &mut |event: &AgentEvent| match event {
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
                    if let Some(shown) = edited.returned(id, *is_error, width) {
                        for line in shown {
                            on_text(&format!("{line}\n"));
                        }
                        return;
                    }

                    for line in
                        crate::cli::panel::result(&tool, on.as_deref(), output, *is_error, width)
                    {
                        on_text(&format!("{line}\n"));
                    }
                }
                AgentEvent::ToolUse { id, name, input } => {
                    tool_calls.insert(id.clone(), (name.clone(), crate::cli::panel::target(input)));
                    // Kept for the result to claim. Parsed HERE because the
                    // call is the only place both sides of the change exist.
                    edited.called(id, input);
                    activity = crate::cli::activity::Activity::from_tool(name, input);
                    on_status(&crate::cli::activity::Status {
                        activity: activity.clone(),
                        elapsed: started.elapsed().as_secs(),
                        tokens: u64::from(turn_usage.input_tokens)
                            + u64::from(turn_usage.output_tokens),
                        progress: None,
                    });
                }
                AgentEvent::Quota {
                    status, resets_at, ..
                } => *turn_quota = Some((status.clone(), *resets_at)),
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
                        tokens: u64::from(*input_tokens) + u64::from(*output_tokens),
                        progress: None,
                    });
                }
                _ => {}
            })
        };

        let first_result = run_backend_attempt(
            &mut self.backend,
            &mut filter,
            &mut turn_usage,
            &mut turn_quota,
            &mut tool_calls,
            &mut edited,
            &mut mid_line,
            on_text,
            on_status,
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

            // Reset stream filter and turn tracking for the standby agent attempt
            filter = crate::ask::StreamFilter::new();
            turn_usage = crate::runtime::usage::TokenUsage::default();
            tool_calls.clear();
            edited = crate::cli::edits::Watcher::watching(self.workspace.clone());

            let standby_result = run_backend_attempt(
                &mut self.standby[index],
                &mut filter,
                &mut turn_usage,
                &mut turn_quota,
                &mut tool_calls,
                &mut edited,
                &mut mid_line,
                on_text,
                on_status,
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

        let outcome = attempt;

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
                let _ = self
                    .backend
                    .run_turn(&instruction, &mut |event: &AgentEvent| {
                        if let AgentEvent::TextDelta(fragment) = event {
                            on_text(fragment);
                        }
                    });

                // What the follow-up itself left behind is REPORTED, not chased.
                let after = links.changed_since(&roots);
                if let Some(note) = crate::links::LinkMap::report(&links.unfinished(&after)) {
                    self.notices.push(note);
                }
            }
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

        // Checked before the turn is reported as fine.
        if let Some((sentinel, guards)) = &watched {
            let breaches = sentinel.breaches_across(&roots, guards);
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
        match self.snapshot_complete {
            None => return Err("there is nothing to undo yet — no turn has run.".to_string()),
            Some(false) => {
                return Err(format!(
                    "undo is not available for that turn: the workspace has more than {MAX_SNAPSHOT_FILES} files, so no complete snapshot was taken. A partial undo would restore some files and not others."
                ))
            }
            Some(true) => {}
        }
        self.checkpoints
            .undo_last(workspace)
            .map_err(|e| e.to_string())
    }

    /// Snapshot the tree. Returns whether it covered everything.
    fn snapshot(&mut self, workspace: &Path, label: &str) -> bool {
        self.checkpoints.begin(crate::text::head_bytes(label, 60));

        // Every root, not just the workspace: a change made in an added
        // directory has to be undoable too.
        let mut stack = vec![workspace.to_path_buf()];
        stack.extend(self.extra_dirs.iter().cloned());
        let mut seen = 0usize;
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                seen += 1;
                if seen > MAX_SNAPSHOT_FILES {
                    return false;
                }
                self.checkpoints.snapshot(&path);
            }
        }
        true
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
                },
                crate::ask::Choice {
                    label: "Second".to_string(),
                    detail: String::new(),
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

    /// **The chosen agent gets its own backend even with the API variables
    /// set.** Their presence used to replace whatever agent had been chosen,
    /// so the banner named one agent while another did the work. Only the API
    /// route's own id reaches the API backend now.
    #[test]
    fn the_api_variables_do_not_replace_the_chosen_local_agent() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let names = [
            crate::agent::api::API_URL_ENV,
            crate::agent::api::API_KEY_ENV,
        ];
        let before: Vec<Option<std::ffi::OsString>> = names.iter().map(std::env::var_os).collect();
        std::env::set_var(names[0], "http://127.0.0.1:9/v1/messages");
        std::env::set_var(names[1], "not-a-real-key");

        let dir = tempfile::tempdir().expect("temp dir");
        let local = session(dir.path()).backend_id().to_string();
        let api = AgentSession::new(
            dir.path().to_path_buf(),
            crate::agent::registry::API_ROUTE,
            String::new(),
            &crate::guards::Guards::default(),
            Vec::new(),
        )
        .backend_id()
        .to_string();

        for (name, value) in names.iter().zip(before) {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
        assert_eq!(
            local, "claude-code",
            "the API variables replaced the chosen local agent"
        );
        assert_eq!(api, crate::agent::registry::API_ROUTE);
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
}
