//! The interface every agent route implements, and its forwarding through a
//! box.
//!
//! Wired to: every backend in [`crate::agent`] (`api`, `claude_code`, `agy`,
//! `codex`) and every caller that drives one — `agent::worker`, the CLI, the
//! run ledger and the ACP agent.

use super::{moves_the_work, AgentError, AgentEvent, TurnOutcome};

/// An external agent this harness can route a turn to.
pub trait AgentBackend {
    /// Stable identifier, e.g. `"claude-code"`.
    fn id(&self) -> &str;

    /// Run one turn, calling `on_event` as events arrive.
    ///
    /// Implementations must continue an existing session where the underlying
    /// agent supports it: re-establishing a session per turn re-pays the whole
    /// prompt prefix. Measured against Claude Code 2.1.229, a fresh session
    /// costs ~36k cache-creation tokens versus ~1.8k when resuming.
    fn run_turn(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError>;

    /// The agent's current session id, once one exists.
    fn session_id(&self) -> Option<&str>;

    /// Ask for a particular model on the next turn, with ordered fallbacks.
    ///
    /// Defaulted to doing nothing, because not every agent can be told which
    /// model to use and one that cannot should not have to say so. An agent
    /// that CAN is where routing pays: cheap work on a cheap model, without an
    /// API credential anywhere.
    ///
    /// Called with the candidates in preference order. An empty slice means the
    /// caller has no opinion and the agent's own default should stand.
    fn set_models(&mut self, _candidates: &[String]) {}

    /// A model the operator named outright (`/model`, `-m`), rather than a
    /// role table's candidates. Defaulted to the same rule as
    /// [`Self::set_models`]; an API route takes any id its service offers.
    fn choose_model(&mut self, model: &str) {
        self.set_models(&[model.to_string()]);
    }

    // The methods below exist because an interactive session needs them and not
    // every backend can honour them. Each defaults to doing nothing rather than
    // being required, so a backend implements only what it actually supports —
    // and a caller reading the default gets "this route cannot do that", which
    // is the truth, instead of a panic or a pretence.

    /// Also work in these directories, beyond the workspace root.
    fn set_extra_dirs(&mut self, _dirs: Vec<std::path::PathBuf>) {}

    /// The deny rules in force, for showing the operator.
    ///
    /// Empty when the backend protects paths some other way — the toolbox
    /// checks the guards itself rather than handing rules to anyone.
    fn deny_rules(&self) -> &[String] {
        &[]
    }

    /// The harness context this backend will carry into its next turn.
    ///
    /// On the trait so the invariant can be checked for ANY agent, not just the
    /// one that happens to expose it. The context going missing is a measured
    /// regression — twice — and it is invisible from the outside, so the test
    /// that pins it must not be limited to a single backend.
    fn system_context(&self) -> &[String] {
        &[]
    }

    /// The MCP server configuration this agent will be handed, if any.
    ///
    /// Exists for the same reason [`Self::system_context`] does: the flag is
    /// assembled inside a spawn and is invisible from outside, so a test that
    /// checks "did the harness actually hand this over" needs a way to look.
    /// Defaults to `None`, which is the truth for every backend that has no
    /// registration path yet — see `docs/COMPUTER_USE.md` section 2.
    ///
    /// Returns an owned string because a backend may register SEVERAL servers
    /// and hold them as separate pieces. Borrowing forced codex to answer with
    /// the first of its overrides, which read as the whole registration and
    /// hid the second server completely.
    fn mcp_config(&self) -> Option<String> {
        None
    }

    /// Who this agent is on the team channel for the task about to run.
    ///
    /// # Why the environment and not the registration
    ///
    /// An agent CLI's MCP servers are configured ONCE, when the backend is
    /// built, and the role changes every task. So the team server cannot be
    /// told who it serves at registration time — it reads
    /// [`crate::team_mcp::IDENTITY_ENV`] instead, and the server is a child of
    /// the agent process, so setting the variable on the spawn is what reaches
    /// it.
    ///
    /// Identity has to come from here rather than from the model, because the
    /// harness RECORDS what is said on the channel: a name the agent could
    /// choose would be a claim written down as a fact.
    ///
    /// Defaulted to nothing, for a backend that spawns no process.
    fn set_team_identity(&mut self, _role: &str) {}

    /// This agent's OWN model ladder for a role, in preference order.
    ///
    /// # Why a backend supplies one at all
    ///
    /// The operator's ladder in `admin.json` holds concrete model ids, and ids
    /// do not cross vendors: measured, forwarding one agent's `sonnet` to
    /// another returned `model sonnet is not recognized` and failed every task
    /// on arrival. Dropping the foreign name keeps the run alive but loses
    /// routing entirely — every role then runs on the agent's default, which is
    /// the expensive one.
    ///
    /// So a backend may answer for itself, in its own namespace. The operator's
    /// ladder still wins wherever it names something this agent can serve; this
    /// is the floor, not an override.
    ///
    /// Empty by default: an agent whose model names have not been measured must
    /// not have a ladder invented for it.
    fn ladder_for(&self, _role: &str) -> Vec<String> {
        Vec::new()
    }

    /// What this backend can PREVENT for itself.
    ///
    /// Defaulted to [`crate::guards::Enforcement::NOTHING`] deliberately: a
    /// backend whose enforcement has not been MEASURED must not be credited
    /// with any, because the cost of guessing high is a guard the operator
    /// believes in and the agent never applies.
    fn enforces(&self) -> crate::guards::Enforcement {
        crate::guards::Enforcement::NOTHING
    }

    /// Whether a FAILED turn was the agent's own failure rather than the work's.
    ///
    /// # Why this is the backend's judgement and not a phrase list
    ///
    /// [`moves_the_work`] reads the failure MESSAGE, which is all a failure
    /// that never produced a turn gives us. That list can only ever hold the
    /// wordings someone has already seen, and a provider invents new ones:
    /// MEASURED live, a busy model answered `Our servers are experiencing high
    /// traffic right now, please try again in a minute.` — an unmistakable
    /// agent-side failure that matched no needle, so both attempts ran on the
    /// agent that had just said it could not serve them and the task was
    /// abandoned with a standby idle.
    ///
    /// A backend whose protocol has a SEPARATE channel for its own machinery
    /// knows the answer structurally and should say so, rather than waiting for
    /// the message to be recognised. One that has no such channel keeps the
    /// default and loses nothing.
    fn agent_failed(&self, outcome: &TurnOutcome) -> bool {
        moves_the_work(&outcome.failure())
    }

    /// Stop the turn when this token is tripped.
    fn set_cancel(&mut self, _token: crate::runtime::cancel::CancelToken) {}

    /// Who to ask when a tool call needs a person's confirmation, or a
    /// question needs the owner's answer.
    ///
    /// Defaulted to nothing: only a backend whose tools this harness runs
    /// itself ([`crate::toolbox::Toolbox`]) has a moment at which to ask.
    fn set_prompter(&mut self, _prompter: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {}

    /// Who to ask about a protected write when nobody is at the session — a
    /// run's owner channel. Defaulted to nothing, as [`Self::set_prompter`] is.
    fn set_owner(&mut self, _owner: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {}

    /// Record the protected writes a person allows into `record` — a
    /// sub-agent's, into its parent's. Defaulted to nothing, as the rest are.
    fn share_allowed_writes(&mut self, _record: std::sync::Arc<crate::toolbox::AllowedWrites>) {}

    /// The API route this backend drives, when it drives one — what
    /// `/models` asks for its service's model list. `None` for a CLI.
    fn api_route(&self) -> Option<&crate::agent::api::Settings> {
        None
    }

    /// The session's own tool state — its background jobs and task list —
    /// for a backend whose tools this harness runs, so every backend of one
    /// session shares them.
    ///
    /// Defaulted to nothing: a backend whose tools are not incode's has no
    /// jobs or list of incode's to share.
    fn share_session_tools(&mut self, _state: std::sync::Arc<crate::toolbox::SessionTools>) {}

    /// Whether every tool call this backend's model makes runs through
    /// incode's [`crate::toolbox::Toolbox`] — so a restriction set on the
    /// shared [`crate::toolbox::SessionTools`] binds it.
    ///
    /// Defaulted to no: an agent CLI that brings its own tools runs them
    /// where incode cannot refuse one.
    fn runs_tools_here(&self) -> bool {
        false
    }

    /// The model this backend was asked for, when it was asked for one.
    fn model(&self) -> Option<&str> {
        None
    }

    /// Begin a new conversation, forgetting the current one.
    fn clear_session(&mut self) {}

    /// Summarise the older part of the conversation into one message, now,
    /// with the operator's `instructions` steering the summary (`/compact`).
    ///
    /// `None` when this backend does not hold its conversation — an agent CLI
    /// compacts its own, inside its own process. `Some(Ok(_))` says what was
    /// done; `Some(Err(_))` why it could not be.
    fn compact(&mut self, _instructions: &str) -> Option<Result<String, String>> {
        None
    }

    /// How much the backend may do without asking.
    fn set_permission_mode(&mut self, _mode: &str) {}

    /// Continue the conversation with this id.
    fn set_session_id(&mut self, _session_id: &str) {}

    /// incode's id for the session, which the memory engine files everything
    /// under — its pre-tool cards as well as the session's capture.
    ///
    /// Defaulted to nothing: only a backend whose tool calls this harness runs
    /// itself sends the engine anything per call.
    fn set_memory_session(&mut self, _session: &str) {}

    /// The session is over: tell the engine's edit check, so what this agent
    /// claimed, read and announced is let go now rather than at its TTL.
    ///
    /// Defaulted to nothing: only a backend whose file tools this harness
    /// runs itself announced anything.
    fn end_code_edits(&mut self) {}

    /// Replace the harness context this backend carries into its turns.
    ///
    /// Defaulted to nothing for a backend that carries none.
    fn set_system_context(&mut self, _blocks: Vec<String>) {}

    /// The conversations subagents opened, as `(session id, what it was asked)`.
    ///
    /// Empty for a backend that does not delegate, and for one whose subagents
    /// are its own business — an agent CLI spawns them inside a process this
    /// harness cannot see into, so it has nothing to report and says so rather
    /// than guessing.
    fn children(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// The tools incode itself runs for this agent's model, by name: the
    /// toolbox's, for a backend whose model has no tools of its own. Empty
    /// for an agent whose CLI still brings its own. A session traces them
    /// when it starts, so which tools a model was handed is on the record —
    /// including whether the desktop was among them.
    fn offered_tools(&self) -> Vec<String> {
        Vec::new()
    }

    /// Who is told what this backend's tools and workers do while they run
    /// — see [`crate::watch`].
    ///
    /// Defaulted to nothing: only a backend whose tools this harness runs
    /// itself has calls, output and workers of incode's to report.
    fn set_watch(&mut self, _watch: std::sync::Arc<dyn crate::watch::Watch>) {}

    /// Carry these pictures with the next turn's prompt, and only that turn.
    ///
    /// # Errors
    ///
    /// The agent cannot take a picture headless — the default, because no
    /// route may be credited with a capability that was not measured on it.
    /// The error names the agent, for the person who attached them.
    fn attach_images(&mut self, images: Vec<crate::toolbox::Image>) -> Result<(), String> {
        Err(format!(
            "{} cannot take pictures headless, so {} picture(s) could not be sent; \
             describe them in words, or pick an agent that takes them",
            self.id(),
            images.len()
        ))
    }
}

/// A boxed backend is a backend.
///
/// The choice between routes is made at runtime — an installed CLI when there
/// is one, a model over an API when the operator configured one — so the call
/// sites hold one value either way. Without this they would each need to know
/// both types and branch around them.
impl AgentBackend for Box<dyn AgentBackend> {
    fn id(&self) -> &str {
        (**self).id()
    }

    /// Forwarded, so a boxed standby reports what the backend INSIDE it can
    /// enforce. Falling through to the trait default here would credit every
    /// boxed agent with enforcing nothing, and the gap report would be right by
    /// accident rather than by measurement.
    fn enforces(&self) -> crate::guards::Enforcement {
        (**self).enforces()
    }

    /// Forwarded for the same reason as [`Self::enforces`]: the run holds its
    /// agents BOXED, so a default here would mean no backend's structural
    /// judgement ever reached the failover loop — the one place it is read.
    fn agent_failed(&self, outcome: &TurnOutcome) -> bool {
        (**self).agent_failed(outcome)
    }

    fn system_context(&self) -> &[String] {
        (**self).system_context()
    }

    fn ladder_for(&self, role: &str) -> Vec<String> {
        (**self).ladder_for(role)
    }

    fn run_turn(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError> {
        (**self).run_turn(prompt, on_event)
    }

    fn session_id(&self) -> Option<&str> {
        (**self).session_id()
    }

    fn choose_model(&mut self, model: &str) {
        (**self).choose_model(model);
    }

    fn set_models(&mut self, candidates: &[String]) {
        (**self).set_models(candidates);
    }

    fn set_extra_dirs(&mut self, dirs: Vec<std::path::PathBuf>) {
        (**self).set_extra_dirs(dirs);
    }

    fn set_team_identity(&mut self, role: &str) {
        (**self).set_team_identity(role);
    }

    fn deny_rules(&self) -> &[String] {
        (**self).deny_rules()
    }

    fn set_cancel(&mut self, token: crate::runtime::cancel::CancelToken) {
        (**self).set_cancel(token);
    }

    fn set_prompter(&mut self, prompter: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {
        (**self).set_prompter(prompter);
    }

    fn set_owner(&mut self, owner: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {
        (**self).set_owner(owner);
    }

    fn share_allowed_writes(&mut self, record: std::sync::Arc<crate::toolbox::AllowedWrites>) {
        (**self).share_allowed_writes(record);
    }

    fn api_route(&self) -> Option<&crate::agent::api::Settings> {
        (**self).api_route()
    }

    fn share_session_tools(&mut self, state: std::sync::Arc<crate::toolbox::SessionTools>) {
        (**self).share_session_tools(state);
    }

    fn runs_tools_here(&self) -> bool {
        (**self).runs_tools_here()
    }

    fn model(&self) -> Option<&str> {
        (**self).model()
    }

    fn clear_session(&mut self) {
        (**self).clear_session();
    }

    fn compact(&mut self, instructions: &str) -> Option<Result<String, String>> {
        (**self).compact(instructions)
    }

    fn set_permission_mode(&mut self, mode: &str) {
        (**self).set_permission_mode(mode);
    }

    fn set_session_id(&mut self, session_id: &str) {
        (**self).set_session_id(session_id);
    }

    fn set_memory_session(&mut self, session: &str) {
        (**self).set_memory_session(session);
    }

    fn end_code_edits(&mut self) {
        (**self).end_code_edits();
    }

    fn set_system_context(&mut self, blocks: Vec<String>) {
        (**self).set_system_context(blocks);
    }

    fn children(&self) -> Vec<(String, String)> {
        (**self).children()
    }

    fn offered_tools(&self) -> Vec<String> {
        (**self).offered_tools()
    }

    fn set_watch(&mut self, watch: std::sync::Arc<dyn crate::watch::Watch>) {
        (**self).set_watch(watch);
    }

    fn attach_images(&mut self, images: Vec<crate::toolbox::Image>) -> Result<(), String> {
        (**self).attach_images(images)
    }
}

#[cfg(test)]
mod route_parity_tests {
    use super::*;
    use crate::guards::Enforcement;

    /// **Every route declares what it really enforces — none inherits by
    /// accident.**
    ///
    /// The trait default is `NOTHING`, which is right for an unmeasured
    /// backend and WRONG for two of the three that exist. The API route was
    /// left on the default when the capability layer was added, so every task
    /// on it would have reported two guards unenforced while enforcing both by
    /// prevention. A default that is safe in general is not safe for a route
    /// nobody re-checked, which is the whole point of pinning all three here.
    #[test]
    fn each_route_states_its_own_enforcement() {
        let api = crate::agent::api::ApiBackend::new(
            crate::agent::api::Settings {
                endpoint: "http://localhost/x".to_string(),
                key: String::new(),
                model: "m".to_string(),
                ..Default::default()
            },
            std::path::Path::new("."),
            "",
            true,
        );
        assert_eq!(
            api.enforces(),
            Enforcement::EVERYTHING,
            "the API route enforces guards at the toolbox, before any write"
        );

        let cli = crate::agent::claude_code::ClaudeCodeBackend::new(std::path::PathBuf::new());
        assert_eq!(
            cli.enforces(),
            Enforcement::EVERYTHING,
            "this CLI accepts a deny list and was measured honouring it"
        );

        let agy = crate::agent::agy::AgyBackend::new(std::path::PathBuf::new());
        assert_eq!(
            agy.enforces(),
            Enforcement::NOTHING,
            "this CLI has no deny surface and ships sub-agent tools"
        );

        let codex = crate::agent::codex::CodexBackend::new(std::path::PathBuf::new());
        assert_eq!(
            codex.enforces(),
            Enforcement::NOTHING,
            "this CLI has no deny surface; Sentinel detects writes"
        );
    }

    /// **Every route reports the harness context it carries.**
    ///
    /// The API route held the context in a field and answered the empty
    /// default, so anything checking "did the session's context reach the
    /// agent?" would have concluded the API route drops it. The block COUNT
    /// differs by route — this one is handed a joined string — but "it
    /// arrived" must be answerable everywhere.
    #[test]
    fn each_route_reports_the_context_it_was_given() {
        let block = "<guards>the paths the agent may not write</guards>";

        let api = crate::agent::api::ApiBackend::new(
            crate::agent::api::Settings {
                endpoint: "http://localhost/x".to_string(),
                key: String::new(),
                model: "m".to_string(),
                ..Default::default()
            },
            std::path::Path::new("."),
            block,
            true,
        );
        assert!(
            api.system_context()
                .iter()
                .any(|b| b.contains("may not write")),
            "the API route carries the context but reported none: {:?}",
            api.system_context()
        );

        let cli = crate::agent::claude_code::ClaudeCodeBackend::new(std::path::PathBuf::new())
            .with_system_context(vec![block.to_string()]);
        assert!(cli
            .system_context()
            .iter()
            .any(|b| b.contains("may not write")));

        let agy = crate::agent::agy::AgyBackend::new(std::path::PathBuf::new())
            .with_system_context(vec![block.to_string()]);
        assert!(agy
            .system_context()
            .iter()
            .any(|b| b.contains("may not write")));

        let codex = crate::agent::codex::CodexBackend::new(std::path::PathBuf::new())
            .with_system_context(vec![block.to_string()]);
        assert!(codex
            .system_context()
            .iter()
            .any(|b| b.contains("may not write")));

        // And a session with nothing to say sends nothing, on every route.
        let quiet = crate::agent::api::ApiBackend::new(
            crate::agent::api::Settings {
                endpoint: "http://localhost/x".to_string(),
                key: String::new(),
                model: "m".to_string(),
                ..Default::default()
            },
            std::path::Path::new("."),
            "   ",
            true,
        );
        assert!(quiet.system_context().is_empty());
    }
}
