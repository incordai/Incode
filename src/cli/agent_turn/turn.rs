//! One interactive turn, in named steps: the turn opened, the agent run (and
//! the work moved to a standby when it cannot do it), the connected files
//! followed up, the memory engine's continuations run, the operator's stop
//! hooks told, the reply ended and its questions held, the sentinel checked,
//! and the turn's usage, quota and checkpoint put on the books.
//!
//! # Why these are steps, in a file of their own
//!
//! Root cause: `turn_observed` in `cli/agent_turn.rs` spanned 526 lines, over
//! the code auditor's 100-line function threshold, because one body did every
//! job a turn has: it opened the turn (cancel, `allowed-tools`, the memory
//! session start, the snapshot), streamed every agent event to the screen
//! through a twelve-argument closure, failed over to the standby agents, ran
//! the follow-up for connected files, ran the memory engine's continuations,
//! told the operator's stop hooks, ended the reply, held its questions,
//! checked the sentinel and kept the usage and quota books. Each of those is
//! now a named step here, called by [`AgentSession::turn_observed`] in the
//! order the one body ran them, with no change in behaviour: the snapshot is
//! still taken before anything runs, the memory engine hears the same events
//! in the same order on the same paths, and every early return and error path
//! is where it was. What the closure captured — the sinks and the turn's
//! on-screen state — is [`TurnStream`]. The steps live in this file rather
//! than in `agent_turn.rs`, which was 2,784 lines with its tests and would
//! have passed the project's 3,000-line limit with them.
//!
//! # Wired to
//!
//! - **Called by** [`AgentSession::turn`] — the REPL's line and `/run`
//!   (`crate::cli`) — and by `crate::acp_agent::prompt::run_turn` for
//!   `incode --acp`, through [`AgentSession::turn_observed`].
//! - **Calls** [`AgentBackend::run_turn`] on the primary and the standby
//!   agents; [`crate::hooks::memory::MemoryHooks`] (`tools_ran`, `said`,
//!   `set_background`, `stop`) for the memory engine; [`crate::links::LinkMap`]
//!   for the follow-up; [`crate::guards::Sentinel`] for the breach check;
//!   [`crate::hooks::HookRunner::stop`] for the operator's stop hooks;
//!   [`crate::telemetry`]; and [`crate::tools::checkpoint::CheckpointStore`]
//!   to seal the turn's checkpoint.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::agent::{AgentBackend, AgentEvent, TurnOutcome};
use crate::cli::activity::{Activity, Status};
use crate::hooks::memory::{Said, Speaker};
use crate::tools::checkpoint::CheckpointStore;

use super::{assemble_prompt, AgentSession, Observer, TurnStart};

/// How wide a tool result panel is drawn in a conversation.
const RESULT_WIDTH: usize = 100;

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

impl<'a> TurnStart<'a> {
    /// The files as the turn's snapshot holds them. Built where an agent is
    /// run — by [`AgentSession::run_or_fail_over`], [`AgentSession::follow_up`]
    /// and [`AgentSession::carry_on`] — from the checkpoints alone, so the
    /// backends can still be driven while it is held. The snapshot is taken
    /// once, before the turn, so every one of these says the same thing.
    fn of(
        checkpoints: &'a CheckpointStore,
        snapshot_complete: Option<bool>,
        roots: &'a [PathBuf],
    ) -> Self {
        Self {
            checkpoints,
            complete: snapshot_complete == Some(true),
            roots,
        }
    }
}

/// What one turn shows while it runs, and to whom.
///
/// The sinks the caller handed [`AgentSession::turn_observed`], and what the
/// turn has drawn and counted so far. Made once per turn by it, carried
/// through every run of an agent in that turn — the first, a standby's, each
/// continuation — and read at the end for the summary, the questions and the
/// books.
struct TurnStream<'s, 'o> {
    on_text: &'s mut dyn FnMut(&str),
    on_status: &'s mut dyn FnMut(&Status),
    observer: Option<Observer<'o>>,
    // What the agent is doing, for a person watching a turn that can run
    // for minutes. These events were parsed from the first turn this
    // harness ever drove and thrown away by the interactive path, which is
    // why a long turn showed one word: "Thinking".
    started: Instant,
    activity: Activity,
    /// Which call each result belongs to. The result carries only an id.
    /// The name of each call and what it was acting on. A result carries
    /// only an id, so without this it is text with no author and no subject.
    tool_calls: HashMap<String, (String, Option<String>)>,
    /// FILE CHANGES, shown as the changes themselves. The same watcher the
    /// ledger run uses, so one rule decides what a change looks like on
    /// screen wherever it happens. See [`crate::cli::edits::Watcher`].
    ///
    /// The root is cloned rather than borrowed: this outlives any borrow of
    /// the session that drives the backends.
    edited: crate::cli::edits::Watcher,
    /// Hides the question block while it streams, and keeps the whole reply
    /// so it can be parsed once the turn ends.
    filter: crate::ask::StreamFilter,
    usage: crate::runtime::usage::TokenUsage,
    quota: Option<(String, i64)>,
    /// WHETHER THE LAST THING WRITTEN LEFT THE CURSOR MID-LINE.
    ///
    /// A reply streams in fragments and the last one does not end with a
    /// newline, because the model simply stopped talking. The tool-result
    /// panel below then writes its rule assuming it owns a fresh row — so the
    /// two collided and the screen read:
    ///
    ///   I'll read the project's brief and roadmap first.── ✗ ToolSearch …
    ///
    /// which is the agent's sentence and a tool header sharing one line, with
    /// no way to tell where one ends. Tracked here rather than asked of the
    /// console, because `on_text` is a callback the caller owns and only this
    /// turn knows what it has already sent.
    mid_line: bool,
    /// Whether this run of the agent streamed deltas, so its whole-message
    /// text is not shown a second time. Reset by every [`Self::attempt`].
    saw_deltas: bool,
}

impl<'s, 'o> TurnStream<'s, 'o> {
    /// A turn's stream, before anything is drawn. Made by
    /// [`AgentSession::turn_observed`] once the turn is counted.
    fn new(
        root: PathBuf,
        on_text: &'s mut dyn FnMut(&str),
        on_status: &'s mut dyn FnMut(&Status),
        observer: Option<Observer<'o>>,
    ) -> Self {
        Self {
            on_text,
            on_status,
            observer,
            started: Instant::now(),
            activity: Activity::THINKING,
            tool_calls: HashMap::new(),
            edited: crate::cli::edits::Watcher::watching(root),
            filter: crate::ask::StreamFilter::new(),
            usage: crate::runtime::usage::TokenUsage::default(),
            quota: None,
            mid_line: false,
            saw_deltas: false,
        }
    }

    /// The one place that ends a line before something structural is drawn.
    fn start_new_line(&mut self) {
        if self.mid_line {
            (self.on_text)("\n");
            self.mid_line = false;
        }
    }

    /// Reset stream filter and turn tracking for the standby agent attempt.
    /// Called by [`AgentSession::run_or_fail_over`] before a standby runs; the
    /// quota, the activity and the cursor are kept.
    fn restart(&mut self, root: PathBuf) {
        self.filter = crate::ask::StreamFilter::new();
        self.usage = crate::runtime::usage::TokenUsage::default();
        self.tool_calls.clear();
        self.edited = crate::cli::edits::Watcher::watching(root);
    }

    /// One run of `backend` on `prompt`: every event handed to the observer,
    /// heard into `said` (cleared first) and shown, then the outcome judged —
    /// and whether a failure moves the work to a standby. Called by
    /// [`AgentSession::run_or_fail_over`] for the primary and each standby,
    /// and by [`AgentSession::carry_on`] for each continuation.
    fn attempt(
        &mut self,
        backend: &mut dyn AgentBackend,
        prompt: &str,
        said: &mut Said,
        start: &TurnStart<'_>,
    ) -> (Result<TurnOutcome, String>, bool) {
        self.saw_deltas = false;
        *said = Said::default();
        let result = backend.run_turn(prompt, &mut |event: &AgentEvent| {
            if let Some(observe) = self.observer.as_mut() {
                observe(event, start);
            }
            // What the model said this run, for the transcript and the
            // stop.
            said.hear(event);
            self.heard(event);
        });
        judged(backend, result)
    }

    /// One agent event, drawn and counted. Called by [`Self::attempt`] for
    /// every event, after the observer and the transcript have it.
    fn heard(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TextDelta(fragment) => {
                self.saw_deltas = true;
                self.show(fragment);
            }
            AgentEvent::Text(text) => {
                // For backends that do not stream deltas (e.g. whole-message responses),
                // render the text unless streaming deltas have already been displayed.
                if !self.saw_deltas {
                    self.show(text);
                }
            }
            AgentEvent::ToolResult {
                id,
                output,
                is_error,
            } => self.show_result(id, output, *is_error),
            AgentEvent::ToolUse { id, name, input } => self.called(id, name, input),
            AgentEvent::Quota {
                status, resets_at, ..
            } => self.quota = Some((status.clone(), *resets_at)),
            // incode speaking mid-turn: a line of its own, never part
            // of the reply. An observer presents it itself (`--acp`,
            // as a thought), so it is not drawn into the reply too.
            AgentEvent::Notice(note) if self.observer.is_none() => {
                self.start_new_line();
                (self.on_text)(&format!("{note}\n"));
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
            } => {
                // Cumulative for the turn: the last event is the truth, so
                // this REPLACES rather than adds.
                self.usage.input_tokens = *input_tokens;
                self.usage.output_tokens = *output_tokens;
                self.usage.cache_creation_input_tokens = *cache_creation_tokens;
                self.usage.cache_read_input_tokens = *cache_read_tokens;
                self.report_status();
            }
            _ => {}
        }
    }

    /// Reply text, through the question filter, onto the screen. Called by
    /// [`Self::heard`] for a delta, and for whole text when no delta came.
    fn show(&mut self, text: &str) {
        let visible = self.filter.push(text);
        if !visible.is_empty() {
            self.mid_line = !visible.ends_with('\n');
            (self.on_text)(&visible);
        }
    }

    /// A tool's result: claimed by the edit watcher, then drawn as the change
    /// itself or as a panel — unless an observer presents it. Called by
    /// [`Self::heard`].
    fn show_result(&mut self, id: &str, output: &str, is_error: bool) {
        // Named by the call that asked for it: a result arrives with
        // only an id, and an unnamed result is a wall of text with
        // nothing saying which tool produced it.
        let (tool, on) = self
            .tool_calls
            .get(id)
            .cloned()
            .unwrap_or_else(|| ("tool".to_string(), None));
        let width = crate::cli::activity::terminal_width().min(RESULT_WIDTH);
        // Claimed before anything is drawn, so the turn's tally of
        // changed files is the same whoever presents the result.
        let edit_block = self.edited.returned(id, is_error, width);
        // An observer presents results itself, as structure; a
        // panel drawn into the reply as well would show it twice.
        if self.observer.is_some() {
            return;
        }
        // The panel owns whole rows. Without this its rule was
        // appended to whatever the reply had left unfinished.
        self.start_new_line();

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
                (self.on_text)(&format!("{line}\n"));
            }
            return;
        }

        for line in crate::cli::panel::result(&tool, on.as_deref(), output, is_error, width) {
            (self.on_text)(&format!("{line}\n"));
        }
    }

    /// A tool call: remembered for its result, kept for the edit watcher, and
    /// shown as what the agent is doing. Called by [`Self::heard`].
    fn called(&mut self, id: &str, name: &str, input: &str) {
        self.tool_calls.insert(
            id.to_string(),
            (name.to_string(), crate::cli::panel::target(input)),
        );
        // Kept for the result to claim. Parsed HERE because the
        // call is the only place both sides of the change exist.
        self.edited.called(id, input);
        self.activity = Activity::from_tool(name, input);
        self.report_status();
    }

    /// What the agent is doing, how long the turn has run and what it has
    /// used, handed to `on_status`. Called by [`Self::called`] and on every
    /// usage event.
    fn report_status(&mut self) {
        (self.on_status)(&Status {
            activity: self.activity.clone(),
            elapsed: self.started.elapsed().as_secs(),
            // Every token the turn used, cached input
            // included — the number the budget charges.
            tokens: u64::from(self.usage.total_tokens()),
            progress: None,
        });
    }

    /// The reply's last rows: how much the turn changed, what the question
    /// filter held back, and a closing newline. Called by
    /// [`AgentSession::turn_observed`] once every run of an agent is over.
    fn end(&mut self) {
        // HOW MUCH MOVED, in one line, once the turn is over.
        //
        // The per-change blocks answer "what changed here"; after a dozen of
        // them nobody can answer "how much changed in total", which is the
        // question that decides whether a turn is reviewable at a glance or
        // needs a diff tool. Files are counted once however often each was
        // edited — see [`crate::cli::edits::Tally`].
        if let Some(summary) = self.edited.summary() {
            self.start_new_line();
            (self.on_text)(&format!("{summary}\n"));
        }

        // Anything the agent put to the owner. Printed as ordinary output, not
        // offered as a selector: a printed menu is answered with one character
        // at the normal prompt, which is what makes it usable by voice.
        let tail = self.filter.finish();
        if !tail.is_empty() {
            self.mid_line = !tail.ends_with('\n');
            (self.on_text)(&tail);
        }
        // END THE TURN ON A ROW BOUNDARY. A reply stops wherever the model
        // stopped talking, so the last fragment rarely ends in a newline —
        // and then the next thing on screen continued that row: a notice, or
        // the echo of what the operator typed next. The transcript read as one
        // run-on line with the question and the answer inside it.
        //
        // Written out rather than through [`Self::start_new_line`] because
        // nothing reads the flag after this, and its reset would be a store
        // nobody reads.
        if self.mid_line {
            (self.on_text)("\n");
        }
    }
}

impl AgentSession {
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
    ///
    /// # The steps
    ///
    /// [`Self::open_turn`], then [`Self::run_or_fail_over`],
    /// [`Self::tell_memory`], [`Self::follow_up`], [`Self::carry_on`],
    /// [`Self::operator_stop`], [`TurnStream::end`], [`Self::hold_questions`],
    /// [`Self::check_sentinel`] and [`Self::account`], in that order.
    pub fn turn_observed(
        &mut self,
        workspace: &Path,
        instruction: &str,
        context: &[String],
        on_text: &mut dyn FnMut(&str),
        on_status: &mut dyn FnMut(&crate::cli::activity::Status),
        observer: Option<Observer<'_>>,
    ) -> Result<TurnOutcome, String> {
        // A custom command's `allowed-tools`, in force for this turn — its
        // follow-up and continuations included — and lifted when it ends.
        let held_to = self.open_turn(workspace, instruction);

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
        let mut stream = TurnStream::new(self.workspace.clone(), on_text, on_status, observer);
        let mut said = Said::default();
        let mut outcome =
            self.run_or_fail_over(&prompt, held_to.is_some(), &roots, &mut stream, &mut said);
        // What the tools did, whether or not the turn then succeeded: a tool
        // that ran, ran, and the engine's recorder learns from it.
        self.tell_memory(&said, outcome.is_ok());

        // What the change left behind, handed back to the agent once
        // ([`Self::follow_up`]).
        let changed = links.changed_since(&roots);
        let unfinished = links.unfinished(&changed);
        if outcome.is_ok() {
            self.follow_up(&unfinished, &links, &roots, &mut stream, &mut said);
        }

        outcome = self.carry_on(outcome, &roots, &mut stream, &mut said);
        if outcome.is_ok() {
            self.operator_stop(&said);
        }

        stream.end();
        self.hold_questions(stream.filter.text());
        self.check_sentinel(watched.as_ref(), &roots);
        self.account(&mut stream);

        // THE TURN'S CHECKPOINT IS SEALED: only what changed is kept, what
        // the turn created is added, and — when the session keeps its
        // checkpoints — it is written to disk for `/undo` after a restart.
        self.checkpoints.finish_turn();

        outcome
    }

    /// Ready the session for a turn: a stale cancel cleared, the next turn's
    /// `allowed-tools` put in force (returned, held for the turn), the memory
    /// session's start settled, and the tree snapshotted. Called by
    /// [`Self::turn_observed`] before anything else.
    fn open_turn(
        &mut self,
        workspace: &Path,
        instruction: &str,
    ) -> Option<crate::toolbox::TurnRestriction> {
        // Cleared first: a Ctrl-C pressed while the prompt was waiting belongs
        // to no turn, and letting it leak in would cancel the next one instantly.
        if let Some(token) = &self.cancel {
            token.reset();
        }
        let held_to = self.hold_turn_to_allowed();
        // A turn not opened by a person's line (a continuation, `/run`) still
        // carries the session-start block.
        self.settle_memory_start();
        // Taken before anything runs, because the agent's writes cannot be
        // intercepted as they happen.
        self.snapshot_complete = Some(self.snapshot(workspace, instruction));
        held_to
    }

    /// The prompt run by the primary agent and, while it fails in a way that
    /// moves the work, by each standby in turn — each told on screen who took
    /// over, and told when this turn's `allowed-tools` (`restricted`) cannot
    /// be enforced on it. The agent that finished becomes the primary. Called
    /// by [`Self::turn_observed`] once the turn is counted.
    fn run_or_fail_over(
        &mut self,
        prompt: &str,
        restricted: bool,
        roots: &[PathBuf],
        stream: &mut TurnStream<'_, '_>,
        said: &mut Said,
    ) -> Result<TurnOutcome, String> {
        let start = TurnStart::of(&self.checkpoints, self.snapshot_complete, roots);
        let (mut attempt, mut agent_failed) =
            stream.attempt(self.backend.as_mut(), prompt, said, &start);
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

            stream.start_new_line();
            (stream.on_text)(&format!("\n[{handover}]\n\n"));
            if restricted && !self.standby[index].runs_tools_here() {
                self.notices.push(crate::custom_commands::not_enforced_by(
                    &took_over,
                    &self
                        .session_tools
                        .turn_allowlist()
                        .map_or_else(Vec::new, |l| l.listed().to_vec()),
                ));
            }

            stream.restart(self.workspace.clone());
            (attempt, agent_failed) =
                stream.attempt(self.standby[index].as_mut(), prompt, said, &start);
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
        attempt
    }

    /// What one run of an agent did and said, told to the memory engine: the
    /// tools that ran always, the words only when the run succeeded (`ok`).
    /// Called by [`Self::turn_observed`] after the first run and by
    /// [`Self::carry_on`] after each continuation.
    fn tell_memory(&mut self, said: &Said, ok: bool) {
        self.memory.tools_ran(said);
        if ok {
            self.memory.said(Speaker::Agent, &said.whole());
        }
    }

    /// What the change left behind — handed BACK TO THE AGENT, which is
    /// still holding the context and is the thing that can act on it.
    /// Telling only the person means they return, re-explain the task and
    /// pay for another turn.
    ///
    /// ONE follow-up, never a loop: the second pass can itself change files
    /// and strand others, and a harness that kept chasing that would spend a
    /// subscription on a graph it cannot fully resolve. One pass catches the
    /// case this exists for — a backend edited without its callers — and the
    /// rest is reported to a person.
    ///
    /// When the follow-up says anything, it replaces `said`. Called by
    /// [`Self::turn_observed`] when the turn succeeded.
    fn follow_up(
        &mut self,
        unfinished: &[crate::links::Unfinished],
        links: &crate::links::LinkMap,
        roots: &[PathBuf],
        stream: &mut TurnStream<'_, '_>,
        said: &mut Said,
    ) {
        let Some(instruction) = crate::links::LinkMap::follow_up(unfinished) else {
            return;
        };
        self.notices.push(
            "Connected files were left untouched — asking the agent to check them.".to_string(),
        );
        self.memory.said(Speaker::Person, &instruction);
        let mut replied = Said::default();
        let start = TurnStart::of(&self.checkpoints, self.snapshot_complete, roots);
        let _ = self
            .backend
            .run_turn(&instruction, &mut |event: &AgentEvent| {
                // The follow-up is part of this turn, so an observer
                // sees its tool calls as it saw the first pass's.
                if let Some(observe) = stream.observer.as_mut() {
                    observe(event, &start);
                }
                replied.hear(event);
                match event {
                    AgentEvent::TextDelta(fragment) => (stream.on_text)(fragment),
                    AgentEvent::Notice(note) if stream.observer.is_none() => {
                        (stream.on_text)(&format!("\n{note}\n"));
                    }
                    _ => {}
                }
            });
        self.memory.tools_ran(&replied);
        if !replied.last().is_empty() {
            self.memory.said(Speaker::Agent, &replied.whole());
            *said = replied;
        }

        // What the follow-up itself left behind is REPORTED, not chased.
        let after = links.changed_since(roots);
        if let Some(note) = crate::links::LinkMap::report(&links.unfinished(&after)) {
            self.notices.push(note);
        }
    }

    /// THE TURN IS ENDING, and the memory engine is told — it records the
    /// turn, and may answer that the agent should carry on. Its answer is
    /// then the next thing the model is sent, in the same conversation, on
    /// every route: a CLI model resumes its session with it as the next
    /// prompt, the API route appends it to the conversation. Bounded by the
    /// engine's own brake and by `MAX_CONTINUATIONS`; not asked at all once
    /// the turn was stopped or while a background job runs.
    ///
    /// Returns the outcome of the last run. Called by [`Self::turn_observed`]
    /// after the follow-up, whatever `outcome` is: a failed turn is returned
    /// as it is, without asking the engine.
    fn carry_on(
        &mut self,
        mut outcome: Result<TurnOutcome, String>,
        roots: &[PathBuf],
        stream: &mut TurnStream<'_, '_>,
        said: &mut Said,
    ) -> Result<TurnOutcome, String> {
        while outcome.is_ok() {
            let last = said.last().to_string();
            // Counted NOW, as the turn ends: a job the model started during
            // it counts, and one that has finished does not.
            self.memory
                .set_background(self.session_tools.running_jobs());
            let Some(reason) = self.memory.stop(&last, self.cancel.as_ref()) else {
                break;
            };
            stream.start_new_line();
            (stream.on_text)(&format!("\n[memory engine: carry on — {reason}]\n\n"));
            let start = TurnStart::of(&self.checkpoints, self.snapshot_complete, roots);
            outcome = stream
                .attempt(self.backend.as_mut(), &reason, said, &start)
                .0;
            self.tell_memory(said, outcome.is_ok());
        }
        outcome
    }

    /// The operator's `stop` commands, once the turn is over — after any
    /// continuation (`crate::hooks`). Told what the agent said last; what
    /// they print joins the turn's notices. They cannot reopen the turn.
    /// Called by [`Self::turn_observed`] when the turn succeeded.
    fn operator_stop(&mut self, said: &Said) {
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

    /// The questions the agent put to the owner in `reply`, said as notices
    /// and held for the next line to answer. Called by
    /// [`Self::turn_observed`] once the reply has ended on screen.
    fn hold_questions(&mut self, reply: &str) {
        let asked = crate::ask::parse(reply);
        if !asked.is_empty() {
            for question in &asked {
                self.notices.push(question.render());
            }
            self.pending = asked;
        }
    }

    /// Checked before the turn is reported as fine. A protected file a
    /// person allowed this turn is theirs, not a breach; taken either way,
    /// so an allow never outlives the turn it was given in. Called by
    /// [`Self::turn_observed`] after the questions are held.
    fn check_sentinel(
        &mut self,
        watched: Option<&(crate::guards::Sentinel, crate::guards::Guards)>,
        roots: &[PathBuf],
    ) {
        let allowed = self.session_tools.allowed_writes().take();
        if let Some((sentinel, guards)) = watched {
            let breaches = sentinel.breaches_allowing(roots, guards, &allowed);
            if let Some(refusal) = crate::guards::Sentinel::refusal(&breaches) {
                // Said, not thrown: an interactive turn has no ledger to fail,
                // and the person is right there to decide what to do.
                self.notices.push(refusal);
            }
        }
    }

    /// The turn's quota, tokens and time put on the session's books and the
    /// operator's collector. Called by [`Self::turn_observed`] last, before
    /// the checkpoint is sealed.
    fn account(&mut self, stream: &mut TurnStream<'_, '_>) {
        // The latest the agent reported. Kept across turns so `/status` can show
        // it between turns, not only during one.
        if stream.quota.is_some() {
            self.quota = stream.quota.take();
        }

        // Added across turns, having been replaced within one.
        let turn_usage = stream.usage;
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
        crate::telemetry::turn_finished(
            &model,
            &turn_usage,
            stream.started.elapsed().as_secs_f64(),
        );
        crate::telemetry::export_in_background();
    }
}

#[cfg(test)]
mod tests;
