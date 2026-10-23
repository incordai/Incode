//! Claude Code agent backend, driven as a subprocess.
//!
//! There is no Rust binding for the Claude Agent SDK (Python and TypeScript
//! only); the documented path for any other language is to run the CLI with
//! `-p` and a structured `--output-format`. The wire shapes were taken from
//! real runs of CLI 2.1.229, not from documentation.
//!
//! # Layout
//!
//! - this file: the backend, its builders and its [`AgentBackend`] face;
//! - `wire`: the stream-json lines and the events each becomes (`map_line`);
//! - `command`: the argv and environment a turn is started with;
//! - `turn`: one run of the CLI, the tool bridge and the stream loop;
//! - `profiles`: which agent profile a turn is pointed at;
//! - `deny`: the harness's restrictions in this agent's deny syntax.

use std::path::PathBuf;

use super::{AgentBackend, AgentError, AgentEvent, TurnOutcome};

mod command;
mod deny;
mod profiles;
mod turn;
mod wire;

use command::{rejects_thinking_display, THINKING_DISPLAY_FLAG};
pub use profiles::{
    keychain_holds_login, login_outside_profiles, pick_usable, process_env, profiles, profiles_in,
    usable_profile, OutsideLogin, Profile,
};
pub use turn::debug_stream_path;

/// What a cancelled turn reports, so the caller can tell it apart from a crash.
/// One wording for every bridged CLI — see [`crate::tool_bridge::CANCELLED`].
pub use crate::tool_bridge::CANCELLED;

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Drives Claude Code over its non-interactive interface.
pub struct ClaudeCodeBackend {
    binary: String,
    workspace: PathBuf,
    /// Set after the first turn; every later turn resumes it.
    session_id: Option<String>,
    /// Which model serves the next turn, if the caller has an opinion.
    ///
    /// **This is what makes role routing work on a subscription.** Verified
    /// against the installed CLI: `--model haiku` really is served by the haiku
    /// model, confirmed by both the session's own `init.model` and the usage
    /// recorded on the result. So cheap work can run cheap without an API key.
    model: Option<String>,
    /// Models to try when the first is overloaded, in order.
    ///
    /// The CLI takes these as one comma-separated list, which is exactly the
    /// ordered candidate chain the role table already produces.
    fallbacks: Vec<String>,
    /// What the harness knows and the agent does not, carried on every turn.
    ///
    /// APPENDED to the agent's own system prompt rather than replacing it: the
    /// agent's instructions are what make it good at the work, and this is the
    /// project's context riding alongside them — the guard rules in force, what
    /// the auditor already found, what the recorder already solved, which roles
    /// exist. Replacing would trade a working coder for a briefed one.
    system_context: Vec<String>,
    /// Directories beyond the workspace this agent may reach.
    extra_dirs: Vec<std::path::PathBuf>,
    /// The policy the deny rules were built from.
    ///
    /// Held so that CHANGING THE ROOTS REBUILDS THE RULES automatically. A
    /// separate "now re-derive them" call is a call someone forgets — and this
    /// one was: reach was widened to a second root while the rules stayed
    /// anchored to the first, and the agent wrote into a protected directory
    /// there unstopped.
    guards: crate::guards::Guards,
    /// Every tool the model has, run HERE.
    ///
    /// The CLI is started with its own tools switched off and handed only
    /// these, through [`crate::tool_bridge`]: each call the model makes comes
    /// back to this process and runs through the toolbox's guards, the memory
    /// engine's hook, the permission mode and the person to ask — the same
    /// path a model over an API takes.
    tools: crate::toolbox::Toolbox,
    /// How many calls this backend has run, so each gets an id no earlier
    /// turn used.
    calls: u64,
    /// Set when the user asks to stop.
    ///
    /// The agent runs as a CHILD PROCESS, so stopping it means killing that
    /// process — there is no cooperative checkpoint to fall back on. Without
    /// this, Ctrl-C during a turn would leave the agent running, still writing
    /// files, with nothing reading its output.
    cancel: Option<crate::runtime::cancel::CancelToken>,
    /// Whether to ask for text as it is generated, not only in whole blocks.
    ///
    /// Off by default: a caller collecting a REPORT wants whole blocks, and
    /// turning this on would make it choose between two copies of the same
    /// text. An interactive caller turns it on and reads the deltas.
    partial_messages: bool,
    /// The protected paths, in the CLI's own deny syntax — REPORTED, no longer
    /// handed to the CLI.
    ///
    /// The CLI has no file tools of its own any more, so a rule in its syntax
    /// would bind nothing. The paths are refused by the toolbox instead, before
    /// the write (see [`crate::toolbox::Toolbox`]); these stay as the readable
    /// statement of what the session protects.
    deny: Vec<String>,
    /// Pictures for the next turn's prompt only, already inside the image
    /// bounds. A turn that carries any is given its prompt on stdin, as a
    /// stream-json user message (see [`command::user_message`]).
    images: Vec<crate::toolbox::Image>,
    /// Whether a turn with partial messages still asks for
    /// `--thinking-display summarized`. The flag is not in the CLI's
    /// `--help`, so a release may drop it; the first turn that fails with
    /// "unknown option" naming it turns this off for the rest of the
    /// session, and that turn is run again without it.
    thinking_display: bool,
}

impl ClaudeCodeBackend {
    #[must_use]
    pub fn new(workspace: PathBuf) -> Self {
        // The filtered environment for the shell tool: a command the model
        // chose must not inherit this process's credentials.
        let mut tools = crate::toolbox::Toolbox::new(&workspace, true);
        // The `Agent` tool's sub-agent runs on this same CLI when no model
        // endpoint is configured.
        tools.run_subagents_on("claude");
        Self {
            binary: "claude".to_string(),
            tools,
            calls: 0,
            workspace,
            session_id: None,
            model: None,
            fallbacks: Vec::new(),
            partial_messages: false,
            system_context: Vec::new(),
            extra_dirs: Vec::new(),
            guards: crate::guards::Guards::default(),
            cancel: None,
            deny: Vec::new(),
            images: Vec::new(),
            thinking_display: true,
        }
    }

    /// The same backend, one level further down the tree of agents: its
    /// toolbox refuses a delegation past the depth limit.
    #[must_use]
    pub fn at_depth(mut self, depth: u8) -> Self {
        self.tools = self.tools.at_depth(depth);
        self
    }

    /// Let the agent work in directories beyond the workspace.
    ///
    /// The caller must extend the SENTINEL to the same roots. This method only
    /// widens what the agent can touch; widening reach without widening the
    /// check is how a protected file stops being protected.
    #[must_use]
    pub fn with_extra_dirs(mut self, dirs: Vec<std::path::PathBuf>) -> Self {
        self.set_extra_dirs(dirs);
        self
    }

    /// The same, for a caller that already owns the backend in place.
    pub fn set_extra_dirs(&mut self, dirs: Vec<std::path::PathBuf>) {
        // incode's toolbox does the writing on this route, so it is the toolbox
        // that must be able to reach the added roots — under the same guards.
        self.tools.set_extra_dirs(dirs.clone());
        self.extra_dirs = dirs;
        // Reach and guard move together, by construction rather than by anyone
        // remembering to call something afterwards.
        self.rebuild_deny();
    }

    /// Give the backend the flag that says the user asked to stop.
    ///
    /// Checked between output lines, so it lands while the agent is talking.
    /// A turn that has gone quiet — a long tool call with no output — is not
    /// interrupted until it next says something; that limit is real and stated
    /// rather than implied.
    #[must_use]
    pub fn with_cancel(mut self, token: crate::runtime::cancel::CancelToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Give the agent the project context this harness assembled.
    ///
    /// Rides EVERY turn, not just the first: the agent may compact its own
    /// conversation, and context that only arrived once would quietly stop
    /// applying part-way through a session.
    #[must_use]
    pub fn with_system_context(mut self, blocks: Vec<String>) -> Self {
        self.system_context = blocks
            .into_iter()
            .filter(|b| !b.trim().is_empty())
            .collect();
        self
    }

    /// What the agent is told about this project, beyond the instruction.
    #[must_use]
    pub fn system_context(&self) -> &[String] {
        &self.system_context
    }

    /// Ask for text as it is generated, for an interactive caller.
    ///
    /// The caller then reads [`AgentEvent::TextDelta`] and must IGNORE
    /// [`AgentEvent::Text`], which still arrives with the same words in it.
    #[must_use]
    pub fn streaming(mut self, on: bool) -> Self {
        self.partial_messages = on;
        self
    }

    /// Refuse the agent write access to the operator's protected paths.
    ///
    /// **Prevention, not detection.** The agent enforces these itself, so the
    /// write never happens — where [`crate::guards::Sentinel`] can only notice
    /// afterwards. Both are used: they cover different routes and neither is
    /// asked to be complete on its own.
    #[must_use]
    pub fn with_guards(mut self, guards: &crate::guards::Guards) -> Self {
        self.guards = guards.clone();
        self.rebuild_deny();
        self
    }

    /// Re-derive the deny rules from the policy and the current roots.
    ///
    /// Called by everything that changes either. A bare glob is matched
    /// relative to the working directory, so a root added later needs its own
    /// absolute rules.
    fn rebuild_deny(&mut self) {
        let restrictions = crate::guards::restrictions_for(&self.guards, &self.extra_dirs);
        self.deny = deny::render_denials(&restrictions);
    }

    /// The deny rules this backend will pass to the agent.
    #[must_use]
    pub fn deny_rules(&self) -> &[String] {
        &self.deny
    }

    /// Offer the model the desktop tools, run through incode's toolbox and
    /// executed by the desktop child it starts — see `toolbox::desktop`.
    /// Off unless a caller asks: handing every run a desktop it did not
    /// request would widen what an agent can reach as a side effect of a
    /// constructor.
    #[must_use]
    pub fn with_desktop(mut self, setup: crate::toolbox::DesktopSetup) -> Self {
        self.tools.enable_desktop(setup);
        self
    }

    /// Run the operator's own pre- and post-tool commands around every call
    /// this model makes — see [`crate::hooks`].
    #[must_use]
    pub fn with_operator_hooks(mut self, hooks: Option<crate::hooks::HookRunner>) -> Self {
        self.tools = self.tools.with_operator_hooks(hooks);
        self
    }

    /// Confine this model's shell commands to `mode` — see [`crate::sandbox`].
    #[must_use]
    pub fn with_sandbox(mut self, policy: crate::sandbox::SandboxPolicy) -> Self {
        self.tools = self.tools.with_sandbox(policy);
        self
    }

    /// Offer the model only `tools`, when given — a user-defined sub-agent's
    /// allow-list. A call to any other tool is refused by the toolbox.
    #[must_use]
    pub fn with_tool_allowlist(mut self, tools: Option<Vec<String>>) -> Self {
        if let Some(tools) = tools {
            self.tools.restrict_tools(tools);
        }
        self
    }

    /// Override the binary (e.g. an absolute path from agent discovery).
    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self.tools.run_subagents_on(&self.binary);
        self
    }

    /// Continue a specific pre-existing session.
    #[must_use]
    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Serve the next turn with a named model, falling back through the rest.
    ///
    /// **Changing the model abandons the session.** A resumed session carries
    /// the earlier turns, and asking a different model to continue them is a
    /// request the harness cannot reason about — it would be paying to resume a
    /// conversation the new model did not have. Starting fresh costs the prompt
    /// prefix once and is the only behaviour that means what it says.
    #[must_use]
    pub fn with_models(mut self, candidates: &[String]) -> Self {
        // One implementation of the rule, not two. A second copy of "changing
        // model abandons the session" is a copy that can drift, and a drifted
        // safety rule is worse than an absent one.
        AgentBackend::set_models(&mut self, candidates);
        self
    }

    /// Forget the session, so the next turn starts a new one.
    pub fn clear_session(&mut self) {
        self.session_id = None;
    }

    /// How much the model's tool calls may do without asking, as the session
    /// spells it (`plan`, `default`, `acceptEdits`).
    ///
    /// Decided by incode's toolbox, never handed to the CLI: the CLI runs no
    /// tools, so a mode given to it would govern nothing. A spelling nobody
    /// recognises lands on read-only, like
    /// [`crate::permissions::PermissionMode::from_str_lossy`].
    pub fn set_permission_mode(&mut self, mode: &str) {
        self.tools
            .set_permission_mode(crate::permissions::PermissionMode::from_agent_mode(mode));
    }

    /// Ask the memory engine before every tool call this model makes.
    #[must_use]
    pub fn with_engine(mut self, engine: Option<crate::hooks::engine::Engine>) -> Self {
        self.tools = self.tools.with_engine(engine);
        self
    }

    /// Announce each file write this model makes to the engine's edit check,
    /// and record each read — see `toolbox::code_edits`.
    #[must_use]
    pub fn with_code_edits(mut self, edits: Option<crate::toolbox::CodeEdits>) -> Self {
        self.tools = self.tools.with_code_edits(edits);
        self
    }

    /// Continue an existing agent session on the next turn.
    pub fn set_session_id(&mut self, session_id: &str) {
        self.session_id = Some(session_id.to_string());
    }

    /// The model the next turn will ask for, if one was chosen.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Whether this agent recognises a model id as one it can serve.
    ///
    /// Recognises Claude families and generic names, and rejects known foreign
    /// vendor namespaces (`gemini-`, `gpt-`, `o1/3/4`, `codex-`) so a cross-vendor
    /// candidate chain from a shared tier never sends a foreign model to the CLI.
    pub fn serves(&self, model: &str) -> bool {
        const FOREIGN_PREFIXES: [&str; 7] =
            ["gemini-", "gpt-", "o1", "o3", "o4", "chatgpt-", "codex-"];
        let id = model.trim().to_ascii_lowercase();
        if id.is_empty() {
            return false;
        }
        !FOREIGN_PREFIXES.iter().any(|f| id.starts_with(f))
    }
}

impl AgentBackend for ClaudeCodeBackend {
    /// Both halves, by incode's own toolbox: a protected path is refused
    /// before the write, and the model has no sub-agent tool but incode's,
    /// which stops at its depth limit.
    fn enforces(&self) -> crate::guards::Enforcement {
        crate::guards::Enforcement::EVERYTHING
    }

    fn system_context(&self) -> &[String] {
        &self.system_context
    }

    fn id(&self) -> &str {
        "claude-code"
    }

    // The session-shaped methods, forwarded to this backend's own. Fully
    // qualified rather than `self.x()`, which would resolve to the inherent
    // method and read as though the trait were calling itself.

    fn set_extra_dirs(&mut self, dirs: Vec<std::path::PathBuf>) {
        ClaudeCodeBackend::set_extra_dirs(self, dirs);
    }

    /// The team channel's tools are incode's toolbox's on this route, so the
    /// toolbox is who speaks.
    fn set_team_identity(&mut self, role: &str) {
        self.tools.set_team_identity(role);
    }

    fn share_session_tools(&mut self, state: std::sync::Arc<crate::toolbox::SessionTools>) {
        self.tools.share_session(state);
    }

    /// The CLI's own tools are switched off; every call comes back through
    /// the bridge to this backend's toolbox.
    fn runs_tools_here(&self) -> bool {
        true
    }

    fn deny_rules(&self) -> &[String] {
        ClaudeCodeBackend::deny_rules(self)
    }

    fn set_cancel(&mut self, token: crate::runtime::cancel::CancelToken) {
        self.tools.set_cancel(token.clone());
        self.cancel = Some(token);
    }

    fn set_prompter(&mut self, prompter: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {
        self.tools.set_prompter(prompter);
    }

    fn set_owner(&mut self, owner: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {
        self.tools.set_owner(owner);
    }

    fn share_allowed_writes(&mut self, record: std::sync::Arc<crate::toolbox::AllowedWrites>) {
        self.tools.share_allowed_writes(record);
    }

    fn children(&self) -> Vec<(String, String)> {
        self.tools
            .children()
            .into_iter()
            .map(|c| (c.session, c.title))
            .collect()
    }

    fn offered_tools(&self) -> Vec<String> {
        self.tools.names()
    }

    /// The tools are incode's toolbox's on this route, so the toolbox is
    /// what is watched.
    fn set_watch(&mut self, watch: std::sync::Arc<dyn crate::watch::Watch>) {
        self.tools.set_watch(watch);
    }

    /// Taken headless: the next turn's prompt goes on stdin as a stream-json
    /// message carrying them — see [`ClaudeCodeBackend::command`].
    fn attach_images(&mut self, images: Vec<crate::toolbox::Image>) -> Result<(), String> {
        self.images = images;
        Ok(())
    }

    fn model(&self) -> Option<&str> {
        ClaudeCodeBackend::model(self)
    }

    fn clear_session(&mut self) {
        ClaudeCodeBackend::clear_session(self);
    }

    fn set_permission_mode(&mut self, mode: &str) {
        ClaudeCodeBackend::set_permission_mode(self, mode);
    }

    fn set_session_id(&mut self, session_id: &str) {
        ClaudeCodeBackend::set_session_id(self, session_id);
    }

    /// The engine hears incode's session id with every call, not the CLI's
    /// conversation id: incode's is the one its session events carry too.
    fn set_memory_session(&mut self, session: &str) {
        self.tools.pin_session(session);
    }

    fn end_code_edits(&mut self) {
        self.tools.end_code_edits();
    }

    fn set_system_context(&mut self, blocks: Vec<String>) {
        self.system_context = blocks
            .into_iter()
            .filter(|b| !b.trim().is_empty())
            .collect();
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    fn set_models(&mut self, candidates: &[String]) {
        let applicable: Vec<String> = candidates
            .iter()
            .filter(|c| self.serves(c))
            .cloned()
            .collect();
        let Some((first, rest)) = applicable.split_first() else {
            return;
        };
        // Changing model abandons the session — see `with_models` for why a
        // resumed conversation cannot be handed to a different model.
        if self.model.as_deref() != Some(first.as_str()) {
            self.session_id = None;
        }
        self.model = Some(first.clone());
        self.fallbacks = rest.to_vec();
    }

    fn ladder_for(&self, role: &str) -> Vec<String> {
        let ids: &[&str] = match role {
            "explore" | "grunt" => &["haiku"],
            "diagnose" | "review" | "plan" => &["opus", "sonnet"],
            "implement" | "frontend" => &["sonnet", "opus"],
            _ => &[],
        };
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    fn run_turn(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError> {
        // This turn's pictures, and only this turn's: taken now, so a turn
        // that fails to start does not carry them into the next one.
        let images = std::mem::take(&mut self.images);
        let asked_for_thinking = self.partial_messages && self.thinking_display;
        match self.run_once(prompt, images.clone(), on_event) {
            // The CLI refused the undocumented thinking flag before it did
            // anything: remembered for the session, and the turn run again,
            // once, without it.
            Err(e) if asked_for_thinking && rejects_thinking_display(&e.to_string()) => {
                crate::trace!(
                    "agent",
                    "{} refused {THINKING_DISPLAY_FLAG}; running without it from now on",
                    self.binary
                );
                self.thinking_display = false;
                self.run_once(prompt, images, on_event)
            }
            other => other,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — fixtures are real lines captured from Claude Code 2.1.229
// ---------------------------------------------------------------------------

#[cfg(test)]
mod command_tests;
#[cfg(test)]
mod profile_tests;
#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
mod wire_tests;
