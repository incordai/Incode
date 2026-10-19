//! Antigravity (`agy`) agent backend, driven as a subprocess.
//!
//! # Where this came from
//!
//! [`crate::agent::registry`] has listed `agy` for a while with
//! `Protocol::Unverified` and the note "Present, not driven. Its headless
//! contract has NOT been read here." This module is that reading: every flag
//! and every wire field below was taken from `agy --help` and from a real
//! `--output-format stream-json` run of v1.1.23, not from documentation.
//!
//! # What it proves about the harness
//!
//! This is the second backend, and it exists partly to keep the first one
//! honest. [`crate::guards`] states policy in neutral terms and each backend
//! renders it; a rule that only one agent can express would have shown up here
//! as something impossible to translate.
//!
//! It showed up as something worse, and more useful: **this agent cannot
//! express the policy at all.** See [`AgyBackend::enforces`].

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Deserialize;

use super::{AgentBackend, AgentError, AgentEvent, TurnOutcome};

/// How long one turn may take before this CLI abandons it.
///
/// Its own default is 5 minutes, which is shorter than a real implement task —
/// measured, as a greenfield task abandoned with `timeout waiting for response`
/// after doing real work first. Set generously rather than unbounded: the
/// harness's budget and attempt count are the intended limits, and a turn that
/// truly hangs still ends.
const PRINT_TIMEOUT: &str = "30m";

// ---------------------------------------------------------------------------
// Wire types — measured from a real run, v1.1.23
// ---------------------------------------------------------------------------

/// One line of `--output-format stream-json`.
///
/// Tagged on `event`, NOT on `type` — the other CLI this harness drives uses
/// `type`, and assuming the two agree is exactly the guess this module exists
/// to avoid.
#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
enum Wire {
    #[serde(rename = "init")]
    Init(InitEvent),
    #[serde(rename = "step_update")]
    StepUpdate { step_update: StepUpdate },
    /// NESTED under `result`, like `step_update` and unlike `init` — whose
    /// `conversation_id` sits at the top level. The two are not consistent, and
    /// a test copied from a real line caught this being modelled wrongly.
    #[serde(rename = "result")]
    Result { result: Box<ResultEvent> },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct InitEvent {
    conversation_id: String,
    #[serde(default)]
    init: InitBody,
}

#[derive(Debug, Default, Deserialize)]
struct InitBody {
    /// Every tool the agent has. Recorded because it is the only place this CLI
    /// says what it can do — and it is where the sub-agent tools were found.
    #[serde(default)]
    tools: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct StepUpdate {
    /// Which step of the turn this update belongs to: every fragment of one
    /// reply carries the same index.
    #[serde(default)]
    step_index: u64,
    #[serde(default)]
    step_type: String,
    /// A FRAGMENT of the assistant's text for this step, to be appended to
    /// the ones before it. Measured on a long reply: dozens of `ACTIVE`
    /// updates of one step, each a few words, the last one `DONE` and itself
    /// only a fragment. A short reply fits in one update, which is how it was
    /// once mistaken for the whole text of a finished step ([`StepText`]).
    #[serde(default)]
    text_delta: String,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct ResultEvent {
    conversation_id: String,
    /// `SUCCESS` on a clean turn. Anything else is treated as a failure, and
    /// the value is reported rather than flattened to a boolean.
    #[serde(default)]
    status: String,
    /// WHY the turn failed, in this CLI's own words.
    ///
    /// Read, not discarded. The first live run of this backend reported every
    /// failure as "the agent gave no reason" while the agent was in fact saying
    /// `invalid model selection (--model "sonnet"): model sonnet is not
    /// recognized` on this very field. The other backend carries the identical
    /// warning for the identical mistake — a reason the program received and
    /// threw away costs a day of guessing.
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

/// This CLI's usage shape. It reports `thinking_tokens` and a `total_tokens`
/// the other does not, and has NO cache-creation figure — so the mapping below
/// leaves that at zero rather than inventing one.
#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_read_tokens: u32,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Drives Antigravity over its non-interactive interface.
pub struct AgyBackend {
    binary: String,
    workspace: PathBuf,
    /// This CLI's word for a session. Resumed with `--conversation`.
    conversation_id: Option<String>,
    model: Option<String>,
    extra_dirs: Vec<PathBuf>,
    /// `accept-edits` or `plan`, this CLI's two execution modes.
    mode: Option<String>,
    /// What the harness knows and the agent does not.
    ///
    /// # Delivered in the PROMPT, because there is nowhere else
    ///
    /// This CLI has no flag to append to its system prompt. The other backend
    /// passes the same material on `--append-system-prompt`, and dropping it
    /// here because the flag is missing would repeat, on a new agent, exactly
    /// the bug [`crate::cli::run_backend`] was written to prevent: an agent
    /// working the ledger while told nothing of the guards, the findings or the
    /// role table.
    ///
    /// So it is prepended to the turn's prompt instead. That is weaker — it is
    /// ordinary prompt text the model may weigh differently from a system
    /// instruction — and weaker is the honest trade against not sending it.
    system_context: Vec<String>,
    cancel: Option<crate::runtime::cancel::CancelToken>,
}

impl AgyBackend {
    #[must_use]
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            binary: "agy".to_string(),
            workspace,
            conversation_id: None,
            model: None,
            extra_dirs: Vec::new(),
            // DEFAULTED, unlike the other backend, because this CLI's own
            // default is `request-review` — measured on its `init` line. A
            // headless run has nobody to answer a review prompt, so the first
            // greenfield run through this backend spent two full attempts and
            // created no file at all: the agent was waiting for an approval
            // that could never arrive.
            //
            // This is not a loosening of the guards. What the agent may write
            // is decided by `guards`, checked by `Sentinel` before and after
            // every attempt, and proved by the task's own command. Asking it
            // for interactive consent as well only guarantees it does nothing.
            mode: Some("accept-edits".to_string()),
            system_context: Vec::new(),
            cancel: None,
        }
    }

    /// Carry the harness's context on every turn.
    ///
    /// Blank blocks are DROPPED, matching the other backend: an empty block
    /// reads to a model as an instruction with nothing in it, and a project
    /// with no brief and no findings should send nothing rather than nothing
    /// wrapped in a heading.
    #[must_use]
    pub fn with_system_context(mut self, context: Vec<String>) -> Self {
        self.system_context = context
            .into_iter()
            .filter(|block| !block.trim().is_empty())
            .collect();
        self
    }

    /// Whether this agent recognises a model id as one of its own.
    ///
    /// Matched on the vendor prefixes this CLI's `models` subcommand reports.
    /// Deliberately a PREFIX test rather than a fixed list: the list changes
    /// with every release, and a stale allowlist would start rejecting models
    /// that had just become valid. A wrong answer here costs one un-routed
    /// turn; forwarding a foreign name costs the entire run.
    fn serves(&self, model: &str) -> bool {
        const KNOWN_FAMILIES: [&str; 3] = ["gemini-", "claude-", "gpt-"];
        let id = model.trim().to_ascii_lowercase();
        KNOWN_FAMILIES.iter().any(|f| id.starts_with(f))
    }

    /// The prompt actually sent: the harness's context, then the task, budgeted for OS limits.
    fn full_prompt(&self, prompt: &str) -> Result<String, AgentError> {
        let (budgeted_prompt, budgeted_context) =
            crate::agent::budget_for_spawn(prompt, &self.system_context, &self.workspace)?;
        Ok(if budgeted_context.is_empty() {
            budgeted_prompt
        } else {
            format!("{budgeted_context}\n\n{budgeted_prompt}")
        })
    }

    /// Override the binary, e.g. an absolute path from agent discovery.
    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self
    }

    fn command(&self, prompt: &str) -> Command {
        let mut cmd = Command::new(&self.binary);
        crate::agent::parent_session::detach_from_parent_session(&mut cmd);
        cmd.current_dir(&self.workspace);
        cmd.arg("--print").arg(prompt);
        cmd.arg("--output-format").arg("stream-json");
        // This CLI gives a print-mode turn 5 minutes by default and then gives
        // up. Measured: a greenfield implement task was abandoned with
        // `timeout waiting for response` at exactly that boundary, having done
        // real work first. The harness already bounds a run its own way — the
        // budget, the attempt count and the operator's own timeout — so the
        // agent's private deadline only truncates work the harness was still
        // willing to pay for.
        cmd.arg("--print-timeout").arg(PRINT_TIMEOUT);
        if let Some(model) = &self.model {
            cmd.arg("--model").arg(model);
        }
        if let Some(mode) = &self.mode {
            cmd.arg("--mode").arg(mode);
        }
        for dir in &self.extra_dirs {
            cmd.arg("--add-dir").arg(dir);
        }
        // Resume rather than start fresh: a new conversation re-pays the whole
        // prompt prefix, which is the same reason the other backend resumes.
        if let Some(id) = &self.conversation_id {
            cmd.arg("--conversation").arg(id);
        }
        cmd
    }
}

impl AgentBackend for AgyBackend {
    fn id(&self) -> &str {
        "agy"
    }

    /// **Neither half.** Measured, and this is the finding that made
    /// [`crate::guards::Enforcement`] necessary.
    ///
    /// `agy --help` offers no per-tool deny list — nothing equivalent to the
    /// other CLI's `--disallowedTools` — and no way to append to the system
    /// prompt. Its `init` line advertises `define_subagent`, `invoke_subagent`,
    /// `manage_subagents` and `browser_subagent`, so it both CAN delegate and
    /// cannot be told not to.
    ///
    /// Declaring `NOTHING` is what keeps the harness honest: the protected
    /// paths still hold, but only through [`crate::guards::Sentinel`], which
    /// detects a write after the fact and needs no cooperation. The no-subagent
    /// rule has no enforcement path here at all, and the worker reports that
    /// gap on every task rather than letting it pass unsaid.
    fn enforces(&self) -> crate::guards::Enforcement {
        crate::guards::Enforcement::NOTHING
    }

    /// `status: ERROR` is this agent's OWN channel, never a verdict on the work.
    ///
    /// MEASURED, and this is the whole reason the judgement is structural: a
    /// turn where the agent worked and got the task wrong still comes back
    /// `SUCCESS` — it is the harness's own check that decides such a turn
    /// failed. So `ERROR` can only be agy's side of the line: a busy provider,
    /// a subscription that has lapsed, a model the account cannot reach, a
    /// stream that broke. Every one of those succeeds on an agent that
    /// authenticates separately, and none of them says anything about the task.
    ///
    /// A live run supplied the case the phrase list could not: `Our servers are
    /// experiencing high traffic right now, please try again in a minute.` It
    /// matched no needle, so the task was retried on the same busy agent and
    /// then abandoned while a standby sat idle. Reading `status` instead of the
    /// message makes the next unseen wording free.
    fn agent_failed(&self, outcome: &crate::agent::TurnOutcome) -> bool {
        outcome.stop_reason.eq_ignore_ascii_case("ERROR")
            || crate::agent::moves_the_work(&outcome.failure())
    }

    fn system_context(&self) -> &[String] {
        &self.system_context
    }

    fn set_system_context(&mut self, blocks: Vec<String>) {
        self.system_context = blocks
            .into_iter()
            .filter(|b| !b.trim().is_empty())
            .collect();
    }

    /// This agent's own ladder, in ITS namespace.
    ///
    /// The ids are taken from `agy models` on v1.1.23, not guessed. The shape
    /// mirrors the harness's own tiering — cheap models for reading, strong
    /// ones for deciding and writing — so a role means the same thing here as
    /// it does on any other agent even though the names share nothing.
    ///
    /// Two candidates per role wherever there are two: this CLI has no fallback
    /// flag, so only the first is ever sent, but the list documents the
    /// intended order and costs nothing if that changes.
    fn ladder_for(&self, role: &str) -> Vec<String> {
        let ids: &[&str] = match role {
            // Reading and locating: the cheapest model that can follow a file.
            "explore" | "grunt" => &["gemini-3.8-flash-low", "gemini-3.8-flash-medium"],
            // Judgement about code that already exists.
            "diagnose" | "review" | "plan" => &["gemini-3.1-pro-high", "gemini-3.8-flash-high"],
            // Writing code, where being wrong is most expensive.
            "implement" | "frontend" => &["gemini-3.1-pro-high", "gemini-3.8-flash-high"],
            // An unknown role gets nothing rather than a guess — the agent's
            // own default then stands, which is the documented behaviour.
            _ => &[],
        };
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    fn session_id(&self) -> Option<&str> {
        self.conversation_id.as_deref()
    }

    fn set_session_id(&mut self, session_id: &str) {
        self.conversation_id = Some(session_id.to_string());
    }

    fn clear_session(&mut self) {
        self.conversation_id = None;
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Takes the first candidate this agent could actually SERVE.
    ///
    /// # Why the role ladder cannot be passed through as-is
    ///
    /// The ladder in `admin.json` holds one vendor's model ids, and this agent
    /// has its own namespace — `gemini-3.8-flash-high`, `gemini-3.1-pro-high`,
    /// and so on. Measured: `--model sonnet` returns
    /// `status: ERROR, error: model sonnet is not recognized`, so a run that
    /// forwarded the ladder failed EVERY task on arrival, before the agent had
    /// read a line of the project.
    ///
    /// A name this agent does not know is therefore dropped and its own default
    /// is left standing — which is exactly what [`AgentBackend::set_models`]
    /// says an agent that cannot be told should do. Routing is lost until the
    /// operator gives this agent its own ladder; losing routing costs money,
    /// while forwarding a foreign name costs the whole run.
    ///
    /// This is a REPORTED limitation, not a solved problem: per-agent role
    /// ladders are the real fix and they do not exist yet.
    fn set_models(&mut self, candidates: &[String]) {
        self.model = candidates.iter().find(|c| self.serves(c)).cloned();
    }

    fn set_extra_dirs(&mut self, dirs: Vec<PathBuf>) {
        self.extra_dirs = dirs;
    }

    fn set_permission_mode(&mut self, mode: &str) {
        // Only two values exist on this CLI. An unrecognised mode leaves the
        // agent's own default standing rather than passing a word it will
        // reject and failing the whole turn over a setting.
        self.mode = match mode {
            "plan" | "read-only" => Some("plan".to_string()),
            "acceptEdits" | "accept-edits" | "workspace-write" => Some("accept-edits".to_string()),
            _ => None,
        };
    }

    fn set_cancel(&mut self, token: crate::runtime::cancel::CancelToken) {
        self.cancel = Some(token);
    }

    fn run_turn(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError> {
        let sent = self.full_prompt(prompt)?;
        let mut command = self.command(&sent);
        crate::agent::process_tree::prepare(&mut command);
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| AgentError::new(format!("could not start `{}`: {e}", self.binary)))?;
        let tree = crate::agent::process_tree::Tree::adopt(&child);

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::new("the agent produced no output stream"))?;

        let stderr = crate::agent::stderr_tail::StderrTail::capture(child.stderr.take());

        let mut outcome: Option<TurnOutcome> = None;
        let mut usage = crate::runtime::usage::TokenUsage::default();
        let mut reply = StepText::default();

        for line in BufReader::new(stdout).lines() {
            if let Some(token) = &self.cancel {
                if token.is_cancelled() {
                    tree.kill(&mut child);
                    return Err(AgentError::new("the turn was cancelled"));
                }
            }
            let Ok(line) = line else { break };
            // A line this module does not understand is SKIPPED, never fatal:
            // the CLI is free to add events, and a turn that died because of an
            // unrecognised one would be a self-inflicted outage on upgrade.
            let Ok(wire) = serde_json::from_str::<Wire>(&line) else {
                continue;
            };
            match wire {
                Wire::Init(init) => {
                    self.conversation_id = Some(init.conversation_id.clone());
                    on_event(&AgentEvent::SessionStarted {
                        session_id: init.conversation_id,
                        model: self.model.clone().unwrap_or_default(),
                    });
                    // Reported rather than acted on: the harness cannot remove
                    // a tool from this agent, so the honest thing is to record
                    // that the sub-agent tools were on offer.
                    if init.init.tools.iter().any(|t| t.contains("subagent")) {
                        crate::trace!(
                            "guards",
                            "agy offers sub-agent tools and cannot be told not to: {}",
                            init.init
                                .tools
                                .iter()
                                .filter(|t| t.contains("subagent"))
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                }
                Wire::StepUpdate { step_update } => {
                    reply.feed(&step_update, on_event);
                    if let Some(u) = step_update.usage {
                        usage = to_usage(&u);
                    }
                }
                Wire::Result { result } => {
                    if let Some(u) = &result.usage {
                        usage = to_usage(u);
                    }
                    let is_error = !result.status.eq_ignore_ascii_case("SUCCESS");
                    self.conversation_id = Some(result.conversation_id.clone());
                    outcome = Some(TurnOutcome {
                        session_id: result.conversation_id.clone(),
                        stop_reason: result.status.clone(),
                        is_error,
                        // This CLI reports no per-model billing record, so the
                        // model that was ASKED for is the only answer available
                        // — and it is labelled as such by being the requested
                        // one, never presented as a billing fact.
                        detail: result.error.clone().filter(|e| !e.trim().is_empty()),
                        served_by: self.model.clone().into_iter().collect(),
                    });
                }
                Wire::Other => {}
            }
        }
        reply.finish(on_event);

        on_event(&AgentEvent::Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_tokens: usage.cache_creation_input_tokens,
            cache_read_tokens: usage.cache_read_input_tokens,
        });

        let status = child
            .wait()
            .map_err(|e| AgentError::new(format!("the agent did not exit cleanly: {e}")))?;

        outcome.ok_or_else(|| {
            let stderr_output = stderr.finish();
            let detail = if stderr_output.is_empty() {
                String::new()
            } else {
                format!(": {stderr_output}")
            };
            AgentError::new(format!(
                "the agent produced no result line (exit {}){detail}",
                status.code().unwrap_or(-1)
            ))
        })
    }
}

/// The assistant's words for a turn, rebuilt from step updates: each
/// fragment is shown as it arrives (`TextDelta`), and each step's whole text
/// is said once (`Text`) when the next step begins or the stream ends.
///
/// Why once: a caller keeps every `Text` as a separate thing said and joins
/// them with newlines (src/task_run.rs `show`, which builds `-t`'s result).
/// Measured: sending each fragment as a `Text` put a newline every few words —
/// `"issue\n": "… anim\nation …"` — and a JSON answer no longer parsed.
#[derive(Default)]
struct StepText {
    step: Option<u64>,
    text: String,
}

impl StepText {
    fn feed(&mut self, update: &StepUpdate, on_event: &mut dyn FnMut(&AgentEvent)) {
        if update.step_type != "agent_response" || update.text_delta.is_empty() {
            return;
        }
        if self.step != Some(update.step_index) {
            self.finish(on_event);
            self.step = Some(update.step_index);
        }
        on_event(&AgentEvent::TextDelta(update.text_delta.clone()));
        self.text.push_str(&update.text_delta);
    }

    fn finish(&mut self, on_event: &mut dyn FnMut(&AgentEvent)) {
        let text = std::mem::take(&mut self.text);
        if !text.trim().is_empty() {
            on_event(&AgentEvent::Text(text));
        }
    }
}

/// This CLI's usage figures, mapped to the harness's.
///
/// `cache_creation_input_tokens` stays ZERO because this CLI does not report
/// one. Folding `thinking_tokens` in there to make the shape match would put a
/// number in a field that means something else, and the budget would then be
/// counting a fiction.
fn to_usage(wire: &WireUsage) -> crate::runtime::usage::TokenUsage {
    crate::runtime::usage::TokenUsage {
        input_tokens: wire.input_tokens,
        output_tokens: wire.output_tokens,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: wire.cache_read_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #3: launched from inside another agent session, a child that
    /// inherits its variables reports that session as its own.
    #[test]
    fn the_cli_never_joins_the_session_that_launched_incode() {
        let cmd = AgyBackend::new(PathBuf::from(".")).command("p");
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == "CLAUDECODE" && v.is_none()),
            "CLAUDECODE is inherited, so the child joins the parent's session"
        );
    }

    /// **AN AGENT THAT CANNOT TAKE A PICTURE HEADLESS SAYS SO, BY NAME.**
    /// `agy --help` (1.2.11) has no image flag, and its stream-json input
    /// shape is not measured, so pictures are refused rather than dropped.
    #[test]
    fn pictures_are_refused_by_name() {
        let picture = crate::toolbox::Image::from_base64(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC",
        )
        .expect("a 1x1 png");
        let mut backend = AgyBackend::new(PathBuf::from("."));
        let why = backend
            .attach_images(vec![picture])
            .expect_err("agy takes no pictures");
        assert!(why.starts_with("agy cannot take pictures"), "{why}");
    }

    fn parse(line: &str) -> Wire {
        serde_json::from_str(line).expect("a wire line")
    }

    /// **A REPLY STREAMED IN FRAGMENTS IS SAID ONCE, WHOLE.** Lines shaped as
    /// a real long reply (measured on agy, Insi's lesson writer): fragments of
    /// one step, `ACTIVE` then `DONE`, each a few words and split inside words
    /// and inside a JSON key. The step is one `Text`, the fragments joined byte
    /// for byte, so its JSON parses; every fragment is still shown live as a
    /// `TextDelta`; a later step is a `Text` of its own.
    #[test]
    fn a_reply_streamed_in_fragments_is_said_once_whole() {
        let line = |index: u64, state: &str, kind: &str, text: &str| {
            format!(
                r#"{{"event":"step_update","step_update":{{"step_index":{index},"state":"{state}","step_type":"{kind}","text_delta":{}}}}}"#,
                serde_json::to_string(text).expect("json")
            )
        };
        let stream = [
            line(0, "DONE", "user_input", ""),
            line(2, "ACTIVE", "agent_response", "{\n  \"issue"),
            line(2, "ACTIVE", "agent_response", "\": \"The agent added anim"),
            line(
                2,
                "ACTIVE",
                "agent_response",
                "ation math\",\n  \"use\": \"Wire it",
            ),
            line(2, "DONE", "agent_response", " end to end.\"\n}"),
            line(4, "DONE", "agent_response", "Done."),
        ];
        let mut said = Vec::new();
        let mut deltas = 0;
        {
            let mut on_event = |event: &AgentEvent| match event {
                AgentEvent::Text(text) => said.push(text.clone()),
                AgentEvent::TextDelta(_) => deltas += 1,
                _ => {}
            };
            let mut reply = StepText::default();
            for wire in stream.iter().map(|l| parse(l)) {
                if let Wire::StepUpdate { step_update } = wire {
                    reply.feed(&step_update, &mut on_event);
                }
            }
            reply.finish(&mut on_event);
        }
        assert_eq!(said.len(), 2, "{said:?}");
        let answer: serde_json::Value =
            serde_json::from_str(&said[0]).expect("the whole reply parses");
        assert_eq!(answer["issue"], "The agent added animation math");
        assert_eq!(answer["use"], "Wire it end to end.");
        assert_eq!(said[1], "Done.");
        assert_eq!(deltas, 5, "every fragment shown as it came");
    }

    /// The envelope is keyed on `event`, not `type`.
    ///
    /// Copied from a real v1.1.23 run. If this ever parses as `Other` the CLI
    /// changed its envelope, and every other test here would still pass while
    /// the backend silently read nothing.
    #[test]
    fn the_init_line_from_a_real_run_is_understood() {
        let wire = parse(
            r#"{"event":"init","conversation_id":"1b343cea","init":{"cwd":"C:\\tmp","tools":["view_file","invoke_subagent"],"permission_mode":"request-review"}}"#,
        );
        match wire {
            Wire::Init(init) => {
                assert_eq!(init.conversation_id, "1b343cea");
                assert!(init.init.tools.iter().any(|t| t == "invoke_subagent"));
            }
            other => panic!("the init envelope was not recognised: {other:?}"),
        }
    }

    /// Assistant text arrives on a step update, not a content block.
    #[test]
    fn the_agents_reply_is_read_from_a_step_update() {
        let wire = parse(
            r#"{"event":"step_update","step_update":{"step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"PONG\n","usage":{"input_tokens":16317,"output_tokens":183,"cache_read_tokens":0}}}"#,
        );
        match wire {
            Wire::StepUpdate { step_update } => {
                assert_eq!(step_update.step_type, "agent_response");
                assert_eq!(step_update.text_delta, "PONG\n");
                let u = to_usage(&step_update.usage.expect("usage"));
                assert_eq!(u.input_tokens, 16317);
                assert_eq!(u.output_tokens, 183);
            }
            other => panic!("a step update was not recognised: {other:?}"),
        }
    }

    /// **Every `status: ERROR` moves the work, whatever the wording.**
    ///
    /// The three the owner named — a lapsed subscription, a model the account
    /// cannot reach, a model that is busy — arrive with three unrelated
    /// messages and one identical status. Only the middle one is in the phrase
    /// list; the first and third are asserted below to MISS it, which is what
    /// makes reading the status the fix rather than a longer list.
    #[test]
    fn any_error_status_moves_the_work_however_it_is_worded() {
        let backend = AgyBackend::new(std::path::PathBuf::from("."));
        let failed = |detail: &str| crate::agent::TurnOutcome {
            session_id: "c1".to_string(),
            stop_reason: "ERROR".to_string(),
            is_error: true,
            detail: Some(detail.to_string()),
            served_by: Vec::new(),
        };

        // VERBATIM from the live run.
        let busy =
            "Our servers are experiencing high traffic right now, please try again in a minute.";
        let lapsed = "Your plan does not include this model.";
        let missing = "model sonnet is not recognized";

        assert!(
            !crate::agent::moves_the_work(busy) && !crate::agent::moves_the_work(lapsed),
            "the control holds only while the phrase list really does miss these"
        );
        for detail in [busy, lapsed, missing] {
            assert!(
                backend.agent_failed(&failed(detail)),
                "an ERROR status is the agent's own failure: {detail}"
            );
        }

        // A turn the agent COMPLETED is never moved, however it reads — that is
        // the harness's own check to judge, and walking a wrong task through
        // every subscription spends them all to learn one thing.
        let finished = crate::agent::TurnOutcome {
            session_id: "c1".to_string(),
            stop_reason: "SUCCESS".to_string(),
            is_error: false,
            detail: None,
            served_by: Vec::new(),
        };
        assert!(!backend.agent_failed(&finished));
    }

    /// A non-SUCCESS status is a failed turn, and says which status it was.
    #[test]
    fn a_failed_turn_is_reported_with_the_status_that_caused_it() {
        let ok =
            parse(r#"{"event":"result","result":{"conversation_id":"c1","status":"SUCCESS"}}"#);
        let bad =
            parse(r#"{"event":"result","result":{"conversation_id":"c1","status":"CANCELLED"}}"#);
        let status_of = |w: Wire| match w {
            Wire::Result { result } => (
                result.status.clone(),
                !result.status.eq_ignore_ascii_case("SUCCESS"),
            ),
            other => panic!("a result line was not recognised: {other:?}"),
        };
        assert_eq!(status_of(ok), ("SUCCESS".to_string(), false));
        assert_eq!(status_of(bad), ("CANCELLED".to_string(), true));
    }

    /// An unknown event is ignored, never fatal — the CLI may add events.
    #[test]
    fn an_unrecognised_event_does_not_break_the_turn() {
        assert!(matches!(
            parse(r#"{"event":"something_new_in_a_later_release"}"#),
            Wire::Other
        ));
    }

    /// **This backend enforces nothing, and says so.**
    ///
    /// The measured reason: no per-tool deny flag exists on this CLI, and its
    /// own init line advertises four sub-agent tools. Claiming enforcement it
    /// does not have would be the exact failure the guard docs warn about.
    #[test]
    fn this_backend_claims_no_enforcement_it_does_not_have() {
        let backend = AgyBackend::new(PathBuf::from("."));
        assert_eq!(
            backend.enforces(),
            crate::guards::Enforcement::NOTHING,
            "claiming enforcement this CLI has no flag for would report a guard \
             that is not there"
        );
    }

    /// The harness's policy becomes a REPORTED GAP here, not a silent drop.
    #[test]
    fn the_whole_policy_is_reported_as_unenforced_on_this_agent() {
        let policy = crate::guards::restrictions(&crate::guards::Guards::default());
        let gaps = policy.unenforced_by(&AgyBackend::new(PathBuf::from(".")).enforces());
        assert_eq!(gaps.len(), 2, "both halves must be reported: {gaps:?}");
        assert!(gaps.iter().any(|g| g.contains("protected path")));
        assert!(gaps.iter().any(|g| g.contains("sub-agents")));
    }

    /// Only the first SERVEABLE candidate survives: no fallback chain here.
    #[test]
    fn a_fallback_chain_collapses_to_the_first_choice() {
        let mut backend = AgyBackend::new(PathBuf::from("."));
        backend.set_models(&[
            "gemini-3.8-flash-high".to_string(),
            "gemini-3.1-pro-high".to_string(),
        ]);
        assert_eq!(backend.model(), Some("gemini-3.8-flash-high"));
    }

    /// **Another vendor's model id is NOT forwarded.**
    ///
    /// The live failure: the role ladder holds `sonnet`/`opus`, this agent
    /// answered `status: ERROR, error: model sonnet is not recognized`, and
    /// every task in the run died before any work began. Leaving the agent's
    /// own default standing loses routing; forwarding the name loses the run.
    #[test]
    fn a_foreign_model_id_is_dropped_rather_than_sent() {
        let mut backend = AgyBackend::new(PathBuf::from("."));
        backend.set_models(&["sonnet".to_string(), "opus".to_string()]);
        assert_eq!(
            backend.model(),
            None,
            "a model this agent cannot serve must not reach its command line"
        );

        // Control: a ladder it CAN serve is still honoured, so the assertion
        // above is not passing because nothing is ever accepted.
        backend.set_models(&["sonnet".to_string(), "gemini-3.1-pro-high".to_string()]);
        assert_eq!(backend.model(), Some("gemini-3.1-pro-high"));
    }

    /// **This agent routes on its OWN ladder, not the operator's foreign one.**
    ///
    /// Before this, a ladder of `sonnet, opus` left the agent un-routed and
    /// every role ran on its default — the expensive one. The ids below come
    /// from `agy models`, and a cheap role must not draw the strong model.
    #[test]
    fn each_role_draws_a_model_this_agent_actually_has() {
        let backend = AgyBackend::new(PathBuf::from("."));
        for role in crate::roles::KNOWN_ROLES {
            for id in backend.ladder_for(role) {
                assert!(
                    backend.serves(&id),
                    "role {role} was given `{id}`, which this agent cannot serve"
                );
            }
        }
        assert_ne!(
            backend.ladder_for("explore"),
            backend.ladder_for("implement"),
            "reading and writing must not draw the same tier, or routing saves nothing"
        );
        assert!(
            backend.ladder_for("no-such-role").is_empty(),
            "an unknown role gets the agent's default, never a guess"
        );
    }

    /// A failed turn carries the agent's OWN reason, not a shrug.
    #[test]
    fn a_failure_reports_the_reason_the_agent_gave() {
        let line = r#"{"event":"result","result":{"conversation_id":"c1","status":"ERROR","error":"model sonnet is not recognized"}}"#;
        match parse(line) {
            Wire::Result { result } => {
                let detail = result.error.clone().filter(|e| !e.trim().is_empty());
                assert_eq!(
                    detail.as_deref(),
                    Some("model sonnet is not recognized"),
                    "the reason the agent gave was discarded"
                );
            }
            other => panic!("not a result: {other:?}"),
        }
    }

    /// **The harness's context reaches this agent even with no flag for it.**
    ///
    /// The pinned failure on the other backend was a run where every task was
    /// worked by an agent told none of the guards, findings or roles. This CLI
    /// has no `--append-system-prompt`, so the same loss would happen silently
    /// unless the context is carried in the prompt instead.
    #[test]
    fn the_harness_context_is_carried_in_the_prompt_when_no_flag_exists() {
        let bare = AgyBackend::new(PathBuf::from("."));
        assert_eq!(
            bare.full_prompt("do the work").expect("sent"),
            "do the work"
        );

        let briefed = AgyBackend::new(PathBuf::from("."))
            .with_system_context(vec!["the guards in force: tests/ is protected".to_string()]);
        let sent = briefed.full_prompt("do the work").expect("sent");
        assert!(
            sent.contains("tests/ is protected"),
            "the harness context never reached the agent: {sent}"
        );
        assert!(
            sent.ends_with("do the work"),
            "the task must come last, after its context: {sent}"
        );
    }

    /// An unknown permission mode leaves the agent's default standing.
    #[test]
    fn an_unknown_permission_mode_is_not_passed_through() {
        let mut backend = AgyBackend::new(PathBuf::from("."));
        backend.set_permission_mode("danger-full-access");
        assert!(
            backend.mode.is_none(),
            "passing a word this CLI rejects would fail the turn over a setting"
        );
        backend.set_permission_mode("plan");
        assert_eq!(backend.mode.as_deref(), Some("plan"));
    }
}
