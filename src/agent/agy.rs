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
//! It showed up as something worse, and more useful: on its own tools **this
//! agent cannot express the policy at all.** See [`AgyBackend::enforces`].
//!
//! # Bridged: incode's tools instead of agy's ([`AgyTools::Incode`])
//!
//! The default, as on the Claude route and codex (`agy_tools: "own"` in
//! admin.json, or `INCODE_AGY_TOOLS=own`, goes back). On this route agy is a
//! model and nothing else: every tool call runs in incode's toolbox
//! ([`crate::tool_bridge`]), under the guards, the memory engine's pre-tool
//! check, the code-edit marks, the sandbox and the operator's hooks.
//!
//! No FLAG switches agy's tools off or hands it an MCP server, but a custom
//! AGENT does both. Measured on 1.4.7 (2026-10-26/27, real headless turns):
//! an agent file with `tools: [finish]` and one `mcpServers` entry
//! (`agy/agent_file.rs`), run as `agy --print … --agent incode
//! --dangerously-skip-permissions`, gives the model only `finish`,
//! `call_mcp_tool`, `list_resources`, `read_resource` and `manage_task`; the
//! shell never ran, and the server inherited agy's environment, which is how
//! the bridge's address and token reach it. The `init` event lists the whole
//! registry either way, so it is not evidence of the restriction.
//!
//! agy's own memory hooks (`~/.gemini/*/hooks.json`) would fire on such a run
//! beside the ones the toolbox fires, so the spawn carries
//! `INCORD_HOOKS_BY=incode` ([`HOOKS_BY_ENV`]), on which the engine's `--agy`
//! hooks stand down. An engine without that switch only records the run twice.
//!
//! Run live with a real model (1.4.7, `gemini-3.8-flash-high`, `incode -t`,
//! 2026-10-27): agy started `incode --tool-server`, every call ran in the
//! toolbox (trace `tool Write (change)`), the file was written and read back,
//! and asked to reach past incode the model found no tool of agy's that writes
//! or runs — agy refused its calls to other servers. agy gives the model no
//! MCP schema it can open on this route, so the prompt names each tool's
//! inputs (`agy/bridged.rs` `tool_inputs`).
//!
//! # Layout
//!
//! This file holds the backend, its settings and the reading of agy's stream.
//! The agent file is written in `agy/agent_file.rs`, and a bridged turn is
//! run in `agy/bridged.rs`.

mod agent_file;
mod bridged;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Deserialize;

use super::{AgentBackend, AgentError, AgentEvent, TurnOutcome};

/// Whose tools an agy model runs, from admin.json's `agy_tools` or
/// [`AGY_TOOLS_ENV`] — see [`crate::admin::AdminConfig::resolve_agy_tools`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgyTools {
    /// agy's own (`run_command`, the write tools, its sub-agents), watched
    /// afterwards by the Sentinel: outside incode's guard, memory hooks and
    /// sandbox.
    Own,
    /// incode's, through the tool bridge: agy is a model and nothing else.
    /// The default, as for codex — the operator's built-in layer
    /// ([`crate::admin::AdminConfig::resolve_agy_tools`]) reads it from here.
    #[default]
    Incode,
}

impl AgyTools {
    /// `own` or `incode`, ignoring case; anything else is no answer.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "own" => Some(Self::Own),
            "incode" => Some(Self::Incode),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Own => "own",
            Self::Incode => "incode",
        }
    }
}

/// The variable that wins over admin.json's `agy_tools`.
pub const AGY_TOOLS_ENV: &str = "INCODE_AGY_TOOLS";

/// Set to `incode` on a bridged agy: the memory engine's own `--agy` hooks
/// read it and stand down, because incode's toolbox reports every call.
pub const HOOKS_BY_ENV: &str = "INCORD_HOOKS_BY";

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
    /// The tool a `tool` step ran: `call_mcp_tool` for every call that goes
    /// to an MCP server, with the server in `tool_info.parameters.ServerName`.
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    tool_info: Option<serde_json::Value>,
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
    /// Every tool the model has, run HERE — on the bridged route only
    /// ([`AgyTools::Incode`]). `None` is agy running its own tools, and then
    /// nothing below this line is used.
    tools: Option<crate::toolbox::Toolbox>,
    /// How many calls the toolbox has run for this backend, so each gets an
    /// id no earlier turn used.
    calls: u64,
    /// The binary agy is told to start as the tool server, when not this
    /// one — a test's stand-in. `None` is this process's own binary.
    tool_server: Option<PathBuf>,
    /// Where the agent file is written, when not agy's own agents folder —
    /// a test's. `None` is `~/.gemini/config/agents`.
    agents_dir: Option<PathBuf>,
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
            tools: None,
            calls: 0,
            tool_server: None,
            agents_dir: None,
        }
    }

    /// Run this model's tool calls in incode's toolbox, through the bridge,
    /// with agy's own tools switched off by the `incode` agent — or not, for
    /// [`AgyTools::Own`].
    ///
    /// Set before the toolbox builders below, which do nothing without it:
    /// agy's own tools are not incode's to configure.
    #[must_use]
    pub fn with_tools(mut self, tools: AgyTools) -> Self {
        self.tools = match tools {
            // The filtered environment for the shell tool, as on the other
            // bridged routes: a command the model chose must not inherit this
            // process's credentials.
            AgyTools::Incode => Some(crate::toolbox::Toolbox::new(&self.workspace, true)),
            AgyTools::Own => None,
        };
        self
    }

    /// The toolbox, when this model's tools are incode's.
    fn toolbox(&mut self) -> Option<&mut crate::toolbox::Toolbox> {
        self.tools.as_mut()
    }

    /// Ask the memory engine before every tool call this model makes — on the
    /// bridged route, where incode runs them.
    #[must_use]
    pub fn with_engine(mut self, engine: Option<crate::hooks::engine::Engine>) -> Self {
        self.tools = self.tools.map(|t| t.with_engine(engine));
        self
    }

    /// Announce each file write to the engine's edit check — bridged only.
    #[must_use]
    pub fn with_code_edits(mut self, edits: Option<crate::toolbox::CodeEdits>) -> Self {
        self.tools = self.tools.map(|t| t.with_code_edits(edits));
        self
    }

    /// Run the operator's own pre- and post-tool commands around every call —
    /// bridged only. See [`crate::hooks`].
    #[must_use]
    pub fn with_operator_hooks(mut self, hooks: Option<crate::hooks::HookRunner>) -> Self {
        self.tools = self.tools.map(|t| t.with_operator_hooks(hooks));
        self
    }

    /// Confine this model's shell commands to `policy` — bridged only: agy
    /// has no shell of its own there, and incode's runs under incode's
    /// sandbox.
    #[must_use]
    pub fn with_sandbox(mut self, policy: crate::sandbox::SandboxPolicy) -> Self {
        self.tools = self.tools.map(|t| t.with_sandbox(policy));
        self
    }

    /// Offer the model the desktop tools, run through incode's toolbox —
    /// bridged only. agy's own route has no way to be handed a desktop for
    /// one run (`cli/tests.rs` `agy_is_handed_no_server_but_the_bridge`).
    #[must_use]
    pub fn with_desktop(mut self, setup: crate::toolbox::DesktopSetup) -> Self {
        if let Some(tools) = self.toolbox() {
            tools.enable_desktop(setup);
        }
        self
    }

    /// Have agy start `exe` as the tool server instead of this binary, and
    /// write the agent file into `agents` instead of agy's own folder.
    /// Called by the bridged-turn tests' stand-ins (`agy/bridged_tests.rs`).
    #[cfg(test)]
    fn with_stand_ins(mut self, exe: PathBuf, agents: PathBuf) -> Self {
        self.tool_server = Some(exe);
        self.agents_dir = Some(agents);
        self
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
    ///
    /// Bridged, the context opens with the inputs of incode's tools
    /// (`agy/bridged.rs` `tool_inputs`): agy gives the model no schema it can
    /// open on that route.
    fn full_prompt(&self, prompt: &str) -> Result<String, AgentError> {
        let context: Vec<String> = match &self.tools {
            Some(tools) => std::iter::once(bridged::tool_inputs(tools))
                .chain(self.system_context.iter().cloned())
                .collect(),
            None => self.system_context.clone(),
        };
        let (budgeted_prompt, budgeted_context) =
            crate::agent::budget_for_spawn(prompt, &context, &self.workspace)?;
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

    /// **On its own tools, neither half.** Measured, and this is the finding
    /// that made [`crate::guards::Enforcement`] necessary.
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
    ///
    /// **Bridged, both** — by incode's toolbox, as on the Claude route: the
    /// `incode` agent leaves agy no tool that writes, runs or delegates
    /// (`finish`, the MCP caller and readers, its task list — measured), so
    /// every write is a toolbox call refused before it runs, and the only
    /// sub-agent tool is incode's, which stops at its depth limit.
    fn enforces(&self) -> crate::guards::Enforcement {
        if self.tools.is_some() {
            return crate::guards::Enforcement::EVERYTHING;
        }
        crate::guards::Enforcement::NOTHING
    }

    /// Bridged, the one server agy is handed: the agent file the next turn
    /// writes, which names incode's tool server and nothing else. On agy's
    /// own tools, nothing — it has no per-run registration.
    fn mcp_config(&self) -> Option<String> {
        self.tools
            .as_ref()
            .map(|_| agent_file::content(&self.server_exe()))
    }

    // What follows reaches the toolbox, and so means something only on the
    // bridged route. On agy's own tools each is the trait's default: there is
    // no call of incode's to ask about, record or watch.

    /// Bridged: agy's own tools are switched off and every call comes back
    /// through the bridge to this backend's toolbox.
    fn runs_tools_here(&self) -> bool {
        self.tools.is_some()
    }

    fn set_team_identity(&mut self, role: &str) {
        if let Some(tools) = self.toolbox() {
            tools.set_team_identity(role);
        }
    }

    fn set_prompter(&mut self, prompter: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {
        if let Some(tools) = self.toolbox() {
            tools.set_prompter(prompter);
        }
    }

    fn set_owner(&mut self, owner: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {
        if let Some(tools) = self.toolbox() {
            tools.set_owner(owner);
        }
    }

    fn share_allowed_writes(&mut self, record: std::sync::Arc<crate::toolbox::AllowedWrites>) {
        if let Some(tools) = self.toolbox() {
            tools.share_allowed_writes(record);
        }
    }

    fn share_session_tools(&mut self, state: std::sync::Arc<crate::toolbox::SessionTools>) {
        if let Some(tools) = self.toolbox() {
            tools.share_session(state);
        }
    }

    fn set_watch(&mut self, watch: std::sync::Arc<dyn crate::watch::Watch>) {
        if let Some(tools) = self.toolbox() {
            tools.set_watch(watch);
        }
    }

    /// The engine hears incode's session id with every call, not agy's
    /// conversation id: incode's is the one its session events carry too.
    fn set_memory_session(&mut self, session: &str) {
        if let Some(tools) = self.toolbox() {
            tools.pin_session(session);
        }
    }

    fn end_code_edits(&mut self) {
        if let Some(tools) = self.toolbox() {
            tools.end_code_edits();
        }
    }

    fn children(&self) -> Vec<(String, String)> {
        self.tools.as_ref().map_or_else(Vec::new, |tools| {
            tools
                .children()
                .into_iter()
                .map(|c| (c.session, c.title))
                .collect()
        })
    }

    fn offered_tools(&self) -> Vec<String> {
        self.tools
            .as_ref()
            .map_or_else(Vec::new, crate::toolbox::Toolbox::names)
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
    /// The ids are taken from `agy models` on 1.4.7 (2026-10-27), not guessed:
    /// `gemini-3.8-flash-{high,medium,low}`, `claude-sonnet-4-6`,
    /// `claude-opus-4-6-thinking`, `gpt-oss-120b-medium`. The ladder named
    /// `gemini-3.1-pro-high` before, which agy no longer serves — every
    /// role-routed turn then failed "model … is not recognized" unless `-m`
    /// named one it does. The shape mirrors the harness's own tiering — cheap
    /// models for reading, strong ones for deciding and writing — so a role
    /// means the same thing here as it does on any other agent even though
    /// the names share nothing.
    ///
    /// Two candidates per role wherever there are two: this CLI has no fallback
    /// flag, so only the first is ever sent, but the list documents the
    /// intended order and costs nothing if that changes.
    fn ladder_for(&self, role: &str) -> Vec<String> {
        let ids: &[&str] = match role {
            // Reading and locating: the cheapest model that can follow a file.
            "explore" | "grunt" => &["gemini-3.8-flash-low", "gemini-3.8-flash-medium"],
            // Judgement about code that already exists: the strongest it has.
            "diagnose" | "review" | "plan" => &["claude-opus-4-6-thinking", "claude-sonnet-4-6"],
            // Writing code, where being wrong is most expensive.
            "implement" | "frontend" => &["claude-sonnet-4-6", "gemini-3.8-flash-high"],
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

    /// Bridged, incode's toolbox does the writing, so it is the toolbox that
    /// must reach the added roots — under the same guards. agy is told too,
    /// as on its own route: with no tools of its own the roots only widen
    /// what it may name.
    fn set_extra_dirs(&mut self, dirs: Vec<PathBuf>) {
        if let Some(tools) = self.toolbox() {
            tools.set_extra_dirs(dirs.clone());
        }
        self.extra_dirs = dirs;
    }

    fn set_permission_mode(&mut self, mode: &str) {
        // Bridged, the toolbox decides what a call may do in this mode.
        if let Some(tools) = self.toolbox() {
            tools.set_permission_mode(crate::permissions::PermissionMode::from_agent_mode(mode));
        }
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
        if let Some(tools) = self.toolbox() {
            tools.set_cancel(token.clone());
        }
        self.cancel = Some(token);
    }

    fn run_turn(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError> {
        let sent = self.full_prompt(prompt)?;
        if self.tools.is_some() {
            return self.run_bridged(&sent, on_event);
        }
        let Running {
            mut child,
            tree,
            stdout,
            stderr,
        } = self.spawn(self.command(&sent))?;
        let mut reading = Reading::new(&mut self.conversation_id, self.model.clone(), false);

        for line in BufReader::new(stdout).lines() {
            if self.cancel.as_ref().is_some_and(|t| t.is_cancelled()) {
                tree.kill(&mut child);
                return Err(AgentError::new("the turn was cancelled"));
            }
            let Ok(line) = line else { break };
            reading.line(&line, on_event);
        }
        let outcome = reading.finish(on_event);

        let status = child
            .wait()
            .map_err(|e| AgentError::new(format!("the agent did not exit cleanly: {e}")))?;

        outcome.ok_or_else(|| no_result_line(status, stderr))
    }
}

/// A started agy, with what `run_turn` reads from it.
struct Running {
    child: std::process::Child,
    tree: crate::agent::process_tree::Tree,
    stdout: std::process::ChildStdout,
    stderr: crate::agent::stderr_tail::StderrTail,
}

impl AgyBackend {
    /// `command` started with its output piped, everything it starts taken
    /// in, and its stderr drained from the start.
    ///
    /// Called once per turn, by `run_turn` on agy's own tools and by
    /// `bridged.rs` `run_bridged` on incode's.
    fn spawn(&self, mut command: Command) -> Result<Running, AgentError> {
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
        Ok(Running {
            child,
            tree,
            stdout,
            stderr,
        })
    }
}

/// One turn's reading of agy's stream, on either route: the conversation it
/// runs in (kept on the backend as soon as it is named, so a turn that fails
/// partway can still be resumed), the reply, the usage and the outcome.
struct Reading<'a> {
    conversation_id: &'a mut Option<String>,
    /// The model that was asked for — the only answer this CLI gives.
    model: Option<String>,
    /// Whether the tools are incode's: then agy's `init` list is not what the
    /// model has, and a tool step of agy's own is worth a trace line.
    bridged: bool,
    outcome: Option<TurnOutcome>,
    usage: crate::runtime::usage::TokenUsage,
    reply: StepText,
    /// The last tool step traced, so a step's several updates trace once.
    traced_step: Option<u64>,
}

impl<'a> Reading<'a> {
    fn new(conversation_id: &'a mut Option<String>, model: Option<String>, bridged: bool) -> Self {
        Self {
            conversation_id,
            model,
            bridged,
            outcome: None,
            usage: crate::runtime::usage::TokenUsage::default(),
            reply: StepText::default(),
            traced_step: None,
        }
    }

    /// One line of the stream, its events handed to `on_event`.
    ///
    /// A line this module does not understand is SKIPPED, never fatal: the
    /// CLI is free to add events, and a turn that died because of an
    /// unrecognised one would be a self-inflicted outage on upgrade.
    fn line(&mut self, line: &str, on_event: &mut dyn FnMut(&AgentEvent)) {
        if let Ok(wire) = serde_json::from_str::<Wire>(line) {
            self.read(wire, on_event);
        }
    }

    /// The stream is over: the last step's text said, the usage reported,
    /// and the outcome, when a result line gave one.
    fn finish(mut self, on_event: &mut dyn FnMut(&AgentEvent)) -> Option<TurnOutcome> {
        self.reply.finish(on_event);
        on_event(&AgentEvent::Usage {
            input_tokens: self.usage.input_tokens,
            output_tokens: self.usage.output_tokens,
            cache_creation_tokens: self.usage.cache_creation_input_tokens,
            cache_read_tokens: self.usage.cache_read_input_tokens,
        });
        self.outcome
    }

    /// One understood line of the stream, folded in.
    ///
    /// Called by [`Self::line`] for every line that parses.
    fn read(&mut self, wire: Wire, on_event: &mut dyn FnMut(&AgentEvent)) {
        match wire {
            Wire::Init(init) => self.started(init, on_event),
            Wire::StepUpdate { step_update } => {
                self.reply.feed(&step_update, on_event);
                self.trace_tool_step(&step_update);
                if let Some(u) = step_update.usage {
                    self.usage = to_usage(&u);
                }
            }
            Wire::Result { result } => {
                if let Some(u) = &result.usage {
                    self.usage = to_usage(u);
                }
                self.outcome = Some(self.finished(&result));
            }
            Wire::Other => {}
        }
    }

    /// On the bridged route, which tool a `tool` step ran, on the trace: a
    /// call to incode's server is what the route expects, and anything else —
    /// a tool of agy's own, or another MCP server — is named, so a run shows
    /// whether agy reached past incode's toolbox.
    ///
    /// Called by [`Self::read`] on every step update.
    fn trace_tool_step(&mut self, step: &StepUpdate) {
        if !self.bridged || step.step_type != "tool" || step.tool_name.is_empty() {
            return;
        }
        if self.traced_step == Some(step.step_index) {
            return;
        }
        self.traced_step = Some(step.step_index);
        let server = step
            .tool_info
            .as_ref()
            .and_then(|info| info.get("parameters"))
            .and_then(|p| p.get("ServerName"))
            .and_then(serde_json::Value::as_str);
        match (step.tool_name.as_str(), server) {
            ("call_mcp_tool", Some(crate::tool_bridge::CLIENT_KEY)) => {
                crate::trace!("agent", "agy forwarded a call to incode's tool server");
            }
            ("call_mcp_tool", server) => crate::trace!(
                "guards",
                "agy called MCP server {} — not incode's tool server",
                server.unwrap_or("(unnamed)")
            ),
            // Ends the turn; nothing to report.
            ("finish", _) => {}
            (name, _) => crate::trace!("agent", "agy ran its own tool {name}"),
        }
    }

    /// The conversation this turn runs in, kept for the next one and
    /// announced.
    ///
    /// Called by [`Self::read`] on the `init` line.
    fn started(&mut self, init: InitEvent, on_event: &mut dyn FnMut(&AgentEvent)) {
        *self.conversation_id = Some(init.conversation_id.clone());
        on_event(&AgentEvent::SessionStarted {
            session_id: init.conversation_id,
            model: self.model.clone().unwrap_or_default(),
        });
        // Reported rather than acted on: on its own tools the harness cannot
        // remove a tool from this agent, so the honest thing is to record
        // that the sub-agent tools were on offer. Bridged, the list is agy's
        // whole registry whatever the agent allows (measured), so it says
        // nothing about what the model has.
        if self.bridged {
            return;
        }
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

    /// The turn's outcome, from the `result` line; the conversation it names
    /// is kept for the next turn.
    ///
    /// Called by [`Self::read`] on the `result` line.
    fn finished(&mut self, result: &ResultEvent) -> TurnOutcome {
        let is_error = !result.status.eq_ignore_ascii_case("SUCCESS");
        *self.conversation_id = Some(result.conversation_id.clone());
        TurnOutcome {
            session_id: result.conversation_id.clone(),
            stop_reason: result.status.clone(),
            is_error,
            // This CLI reports no per-model billing record, so the
            // model that was ASKED for is the only answer available
            // — and it is labelled as such by being the requested
            // one, never presented as a billing fact.
            detail: result.error.clone().filter(|e| !e.trim().is_empty()),
            served_by: self.model.clone().into_iter().collect(),
        }
    }
}

/// The error for a turn that ended without a result line: its exit code,
/// and what it wrote on stderr when it wrote anything.
///
/// Called by `run_turn` once the agent has exited.
fn no_result_line(
    status: std::process::ExitStatus,
    stderr: crate::agent::stderr_tail::StderrTail,
) -> AgentError {
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
#[path = "agy_tests.rs"]
mod tests;
