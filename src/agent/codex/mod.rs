//! OpenAI Codex (`codex`) agent backend, driven as a subprocess.
//!
//! # Where this came from
//!
//! [`crate::agent::registry`] listed `codex` as `Protocol::Unverified`.
//! This module implements that driver: every CLI flag and wire event below was
//! measured directly from real `codex exec --json` runs on Codex CLI (v0.145.0 /
//! v0.153.4).
//!
//! # How it is driven
//!
//! - Non-interactive turns are started with `codex exec [OPTIONS] [PROMPT]`.
//! - Resumed turns are continued with `codex exec resume [OPTIONS] <THREAD_ID> [PROMPT]`.
//! - Output is read from stdout as newline-delimited JSON (`--json`).
//! - Standard input is closed (`Stdio::null()`) so Codex does not wait on interactive input.
//! - By default, headless turns bypass confirmation prompts with
//!   `--dangerously-bypass-approvals-and-sandbox` so unattended runs do not hang waiting
//!   for approvals on a terminal that has nobody typing into it.
//!
//! # Enforcement
//!
//! Like Antigravity, Codex CLI does not provide external flags to disallow specific tools
//! or protected file paths. Protection is derived from [`crate::guards::Sentinel`] which
//! detects changes after the turn. See [`CodexBackend::enforces`].
//!
//! # Bridged: incode's tools instead of codex's ([`CodexTools::Incode`])
//!
//! Opt-in (`codex_tools: "incode"` in admin.json, or `INCODE_CODEX_TOOLS=incode`),
//! because no turn with a REAL model has been run this way yet. When on, codex is
//! driven the way the Claude route is ([`crate::tool_bridge`]): its own tools are
//! switched off by config, it is handed ONE MCP server — `incode --tool-server` —
//! and every call the model makes comes back to this process and runs through the
//! toolbox. What each override does was read from codex's source at
//! `rust-v0.163.0`, checked against `rust-v0.153.4` and `rust-v0.145.0`, and
//! measured on the 0.163.0 binary against a stand-in Responses endpoint
//! (`model_providers.<name>.base_url`) that recorded what codex sent:
//!
//! - `gpt-5.5`, own tools (control): `exec_command, write_stdin,
//!   request_user_input, apply_patch, view_image, get_goal, create_goal,
//!   update_goal, tool_search, web_search`;
//! - `gpt-5.5`, with [`MODEL_ONLY_OVERRIDES`] and the bridge's registration:
//!   `list_mcp_resources, list_mcp_resource_templates, read_mcp_resource,
//!   apply_patch, mcp__incode{…}`;
//! - `gpt-5.6-terra`, a code-mode model: one `exec` tool (JavaScript in an
//!   isolate with no file system or network, calling nested tools) whose nested
//!   tools were `apply_patch`, the three resource readers and every
//!   `mcp__incode__*`, plus `wait` — and, before `agents.enabled=false`, a
//!   `collaboration` namespace (`spawn_agent`, …).
//!
//! Driven end to end through `incode -t` with that endpoint scripted to call
//! `mcp__incode` `Write`: codex started `incode --tool-server`, the call ran in
//! incode's toolbox (trace `tool Write (change)`), the file was written, the event
//! was incode's (`incode-1`), and the model was handed the toolbox's answer.
//!
//! `apply_patch` cannot be switched off: it is offered when the MODEL CATALOGUE
//! says so (`model_info.apply_patch_tool_type`, `core/src/tools/spec_plan.rs`), not
//! by any config key or feature. It is neutralised instead: with
//! `sandbox_mode="read-only"` and `approval_policy="never"`, codex's
//! `assess_patch_safety` (`core/src/safety.rs`) finds no writable root and REJECTS
//! the patch before it runs, on every platform, sandbox or not. The three
//! `*_mcp_resource*` tools only read resources the MCP servers offer, and the only
//! server is incode's.
//!
//! # Layout
//!
//! This file holds the backend and its settings. The command each turn starts
//! is built in `argv.rs`, a bridged turn is run in `bridged.rs`, and what codex
//! prints is read in `wire.rs`.

mod argv;
mod bridged;
mod wire;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Stdio;

use super::{AgentBackend, AgentError, AgentEvent, TurnOutcome};
pub use argv::MODEL_ONLY_OVERRIDES;
use wire::Reading;

/// Whose tools a codex model runs, from admin.json's `codex_tools` or
/// [`CODEX_TOOLS_ENV`] — see [`crate::admin::AdminConfig::resolve_codex_tools`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodexTools {
    /// Codex's own (`exec_command`, `apply_patch`, …), watched afterwards by
    /// the Sentinel. The default until a live turn has proved the other.
    #[default]
    Own,
    /// incode's, through the tool bridge: codex is a model and nothing else.
    Incode,
}

impl CodexTools {
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

/// The variable that wins over admin.json's `codex_tools`.
pub const CODEX_TOOLS_ENV: &str = "INCODE_CODEX_TOOLS";

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Drives OpenAI Codex CLI over its non-interactive interface.
pub struct CodexBackend {
    binary: String,
    workspace: PathBuf,
    /// Thread ID for this conversation. Resumed with `codex exec resume <thread_id>`.
    thread_id: Option<String>,
    model: Option<String>,
    extra_dirs: Vec<PathBuf>,
    /// Sandbox mode, e.g. "read-only" or "workspace-write".
    sandbox: Option<String>,
    /// `-c key=value` overrides registering an MCP server, if the caller wants
    /// this agent to have one.
    ///
    /// Built by [`crate::desktop::mcp::codex_config_overrides`]. Off unless
    /// asked — handing every run a desktop it did not request would widen an
    /// agent's reach as a side effect of a constructor.
    mcp_overrides: Vec<String>,
    /// Who this agent speaks as on the team channel, for the task in hand.
    team_identity: Option<String>,
    /// System context blocks prepended to the prompt.
    system_context: Vec<String>,
    cancel: Option<crate::runtime::cancel::CancelToken>,
    /// Pictures for the next turn's prompt only.
    images: Vec<crate::toolbox::Image>,
    /// Where this turn's pictures were written, while its command is built.
    image_files: Vec<PathBuf>,
    /// Every tool the model has, run HERE — on the bridged route only
    /// ([`CodexTools::Incode`]). `None` is codex running its own tools, and
    /// then nothing below this line is used.
    tools: Option<crate::toolbox::Toolbox>,
    /// How many calls the toolbox has run for this backend, so each gets an
    /// id no earlier turn used.
    calls: u64,
    /// The binary codex is told to start as the tool server, when not this
    /// one — a test's stand-in. `None` is this process's own binary.
    tool_server: Option<PathBuf>,
}

/// A turn's pictures as files for `--image`, removed when the turn is over.
struct ImageFiles {
    paths: Vec<PathBuf>,
}

impl ImageFiles {
    /// Write each picture to a file of its own in the system's temporary
    /// directory, named so no two turns share one.
    fn write(images: &[crate::toolbox::Image]) -> Result<Self, String> {
        let mut written = Self { paths: Vec::new() };
        for image in images {
            let path = std::env::temp_dir().join(format!(
                "incode-prompt-{}.{}",
                uuid::Uuid::new_v4(),
                image.format().word()
            ));
            std::fs::write(&path, image.bytes())
                .map_err(|e| format!("a picture could not be written for the agent: {e}"))?;
            written.paths.push(path);
        }
        Ok(written)
    }
}

impl Drop for ImageFiles {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl CodexBackend {
    #[must_use]
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            binary: "codex".to_string(),
            workspace,
            thread_id: None,
            model: None,
            extra_dirs: Vec::new(),
            sandbox: None,
            mcp_overrides: Vec::new(),
            team_identity: None,
            system_context: Vec::new(),
            cancel: None,
            images: Vec::new(),
            image_files: Vec::new(),
            tools: None,
            calls: 0,
            tool_server: None,
        }
    }

    /// Run this model's tool calls in incode's toolbox, through the bridge,
    /// with codex's own tools switched off — or not, for [`CodexTools::Own`].
    ///
    /// Set before the toolbox builders below, which do nothing without it:
    /// codex's own tools are not incode's to configure.
    #[must_use]
    pub fn with_tools(mut self, tools: CodexTools) -> Self {
        self.tools = match tools {
            // The filtered environment for the shell tool, as on the Claude
            // route: a command the model chose must not inherit this
            // process's credentials. No sub-agent CLI is named: the toolbox
            // drives one as the Claude CLI, which codex is not.
            CodexTools::Incode => Some(crate::toolbox::Toolbox::new(&self.workspace, true)),
            CodexTools::Own => None,
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

    /// Confine this model's shell commands to `policy` — bridged only: codex's
    /// own shell is off there, and incode's runs under incode's sandbox.
    #[must_use]
    pub fn with_sandbox(mut self, policy: crate::sandbox::SandboxPolicy) -> Self {
        self.tools = self.tools.map(|t| t.with_sandbox(policy));
        self
    }

    /// Offer the model the desktop tools, run through incode's toolbox —
    /// bridged only; codex's own route registers the desktop server instead
    /// ([`Self::with_mcp_overrides`]).
    #[must_use]
    pub fn with_desktop(mut self, setup: crate::toolbox::DesktopSetup) -> Self {
        if let Some(tools) = self.toolbox() {
            tools.enable_desktop(setup);
        }
        self
    }

    /// Offer the model only `tools`, when given — a user-defined sub-agent's
    /// allow-list, refused by the toolbox. Bridged only.
    #[must_use]
    pub fn with_tool_allowlist(mut self, allowed: Option<Vec<String>>) -> Self {
        if let (Some(tools), Some(allowed)) = (self.toolbox(), allowed) {
            tools.restrict_tools(allowed);
        }
        self
    }

    /// Have codex start `exe` as the tool server instead of this binary.
    /// Called by the bridged-turn test's stand-ins (`tests.rs`
    /// `stand_in_bridged_backend`).
    #[cfg(test)]
    fn with_tool_server(mut self, exe: PathBuf) -> Self {
        self.tool_server = Some(exe);
        self
    }

    /// Hand this agent an MCP server, as `-c key=value` overrides.
    ///
    /// Build them with [`crate::desktop::mcp::codex_config_overrides`] so the
    /// TOML shape stays in one place.
    #[must_use]
    pub fn with_mcp_overrides(mut self, overrides: Vec<String>) -> Self {
        self.mcp_overrides = overrides;
        self
    }

    /// Carry the harness's context on every turn.
    ///
    /// Blank blocks are dropped so a project with no brief or findings sends
    /// nothing rather than blank text.
    #[must_use]
    pub fn with_system_context(mut self, context: Vec<String>) -> Self {
        self.system_context = context
            .into_iter()
            .filter(|block| !block.trim().is_empty())
            .collect();
        self
    }

    /// Override the binary command or path.
    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self
    }

    /// Whether this agent recognises a model id as one of its own.
    fn serves(&self, model: &str) -> bool {
        const KNOWN_FAMILIES: [&str; 6] = ["gpt-", "o1", "o3", "o4", "chatgpt-", "codex-"];
        let id = model.trim().to_ascii_lowercase();
        KNOWN_FAMILIES.iter().any(|f| id.starts_with(f))
    }

    /// The prompt actually sent — the harness's context, then the task — and
    /// whether it goes on stdin.
    ///
    /// Within one command line it is an argument, as before. Too long for one,
    /// it goes WHOLE on stdin (`codex exec -`: "If not provided as an argument
    /// (or if `-` is used), instructions are read from stdin", 0.163.0) rather
    /// than into a file the model is told to read first — measured: a two-line
    /// request arrived as a 16 KB file with the person's words last.
    fn full_prompt(&self, prompt: &str) -> Result<(String, bool), AgentError> {
        if !crate::agent::fits_command_line(prompt, &self.system_context) {
            let context = self.system_context.join("\n\n");
            return Ok((
                if context.trim().is_empty() {
                    prompt.to_string()
                } else {
                    format!("{context}\n\n{prompt}")
                },
                true,
            ));
        }
        let (budgeted_prompt, budgeted_context) =
            crate::agent::budget_for_spawn(prompt, &self.system_context, &self.workspace)?;
        Ok((
            if budgeted_context.is_empty() {
                budgeted_prompt
            } else {
                format!("{budgeted_context}\n\n{budgeted_prompt}")
            },
            false,
        ))
    }
}

/// Write `text` to `child`'s stdin and close it, on a thread of its own: the
/// prompt can be large, and a pipe codex is not reading yet must not stall the
/// turn. Nothing to do without `text`. Called by both routes' spawns.
pub(super) fn feed_stdin(child: &mut std::process::Child, text: Option<String>) {
    let (Some(text), Some(mut stdin)) = (text, child.stdin.take()) else {
        return;
    };
    std::thread::spawn(move || {
        use std::io::Write as _;
        let _ = stdin.write_all(text.as_bytes());
        // Dropped here: codex reads to EOF.
    });
}

impl AgentBackend for CodexBackend {
    fn id(&self) -> &str {
        "codex"
    }

    /// On its own tools, neither half. Measured from the CLI surface: no
    /// per-tool deny flag exists, so Sentinel detection covers file writes
    /// after the fact.
    ///
    /// Bridged, both — by incode's toolbox, as on the Claude route: every
    /// write the model can make is a toolbox call refused before it runs, and
    /// its only sub-agent tool is incode's, which stops at its depth limit.
    /// What codex keeps of its own cannot do either: `apply_patch` is rejected
    /// unrun under read-only + never, and its sub-agent tools are off
    /// (`features.multi_agent=false`) — both measured; see the module notes.
    fn enforces(&self) -> crate::guards::Enforcement {
        if self.tools.is_some() {
            return crate::guards::Enforcement::EVERYTHING;
        }
        crate::guards::Enforcement::NOTHING
    }

    fn agent_failed(&self, outcome: &TurnOutcome) -> bool {
        outcome.is_error
            || outcome.stop_reason.eq_ignore_ascii_case("ERROR")
            || crate::agent::moves_the_work(&outcome.failure())
    }

    /// Bridged, the team channel's tools are the toolbox's, so the toolbox is
    /// who speaks; otherwise the team server reads it from the spawn.
    fn set_team_identity(&mut self, role: &str) {
        match self.toolbox() {
            Some(tools) => tools.set_team_identity(role),
            None => self.team_identity = Some(role.to_string()),
        }
    }

    fn mcp_config(&self) -> Option<String> {
        // Bridged, the one server codex is handed is the bridge — the binary
        // is the one each turn names, so this is what the next turn sends.
        if self.tools.is_some() {
            return Some(argv::bridge_registration(&self.server_exe()).join("\n"));
        }
        // EVERY override, not the first. Returning one of them read as the
        // whole registration, which was true while there was a single server
        // and became a lie the moment there were two — the second was simply
        // invisible to anything asking what this agent had been handed.
        if self.mcp_overrides.is_empty() {
            return None;
        }
        Some(self.mcp_overrides.join("\n"))
    }

    // What follows reaches the toolbox, and so means something only on the
    // bridged route. On codex's own tools each is the trait's default: there
    // is no call of incode's to ask about, record or watch.

    /// Bridged: codex's own tools are switched off and every call comes back
    /// through the bridge to this backend's toolbox.
    fn runs_tools_here(&self) -> bool {
        self.tools.is_some()
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

    /// The engine hears incode's session id with every call, not codex's
    /// thread id: incode's is the one its session events carry too.
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

    fn system_context(&self) -> &[String] {
        &self.system_context
    }

    fn set_system_context(&mut self, blocks: Vec<String>) {
        self.system_context = blocks
            .into_iter()
            .filter(|b| !b.trim().is_empty())
            .collect();
    }

    /// Model ladder in Codex / OpenAI namespace.
    fn ladder_for(&self, role: &str) -> Vec<String> {
        let ids: &[&str] = match role {
            "explore" | "grunt" => &["gpt-5.6-luna", "gpt-5.4-mini", "o4-mini", "o3-mini"],
            "diagnose" | "review" | "plan" => &["gpt-5.6-terra", "gpt-5.5", "o3"],
            "implement" | "frontend" => &["gpt-5.6-terra", "gpt-5.5", "o3"],
            _ => &[],
        };
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    fn session_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    fn set_session_id(&mut self, session_id: &str) {
        self.thread_id = Some(session_id.to_string());
    }

    fn clear_session(&mut self) {
        self.thread_id = None;
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn set_models(&mut self, candidates: &[String]) {
        self.model = candidates.iter().find(|c| self.serves(c)).cloned();
    }

    /// Bridged, incode's toolbox does the writing, so it is the toolbox that
    /// must reach the added roots — under the same guards. Codex is not told:
    /// its own sandbox is read-only there.
    fn set_extra_dirs(&mut self, dirs: Vec<PathBuf>) {
        if let Some(tools) = self.toolbox() {
            tools.set_extra_dirs(dirs.clone());
        }
        self.extra_dirs = dirs;
    }

    /// Bridged, decided by incode's toolbox and never handed to codex: its
    /// own sandbox stays read-only whatever the mode, because it runs nothing
    /// that writes. A spelling nobody recognises lands on read-only.
    fn set_permission_mode(&mut self, mode: &str) {
        if let Some(tools) = self.toolbox() {
            tools.set_permission_mode(crate::permissions::PermissionMode::from_agent_mode(mode));
            return;
        }
        self.sandbox = match mode {
            "plan" | "read-only" => Some("read-only".to_string()),
            "acceptEdits" | "accept-edits" | "workspace-write" => {
                Some("workspace-write".to_string())
            }
            _ => None,
        };
    }

    fn set_cancel(&mut self, token: crate::runtime::cancel::CancelToken) {
        if let Some(tools) = self.toolbox() {
            tools.set_cancel(token.clone());
        }
        self.cancel = Some(token);
    }

    /// As files, with `--image` — in `codex exec --help` (0.153.4) for both
    /// `exec` and `exec resume`. Not run live here: every live turn on this
    /// machine answers with the account's usage limit.
    fn attach_images(&mut self, images: Vec<crate::toolbox::Image>) -> Result<(), String> {
        self.images = images;
        Ok(())
    }

    fn run_turn(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError> {
        let (sent, on_stdin) = self.full_prompt(prompt)?;
        // `-` makes codex read the instructions from stdin.
        let arg = if on_stdin { "-" } else { sent.as_str() };
        let stdin_text = on_stdin.then(|| sent.clone());
        // Held to the end of the turn, then removed.
        let pictures =
            ImageFiles::write(&std::mem::take(&mut self.images)).map_err(AgentError::new)?;
        self.image_files.clone_from(&pictures.paths);
        if self.tools.is_some() {
            // The turn's channel first: the command names the binary it was
            // opened for, and a channel that cannot open starts nothing.
            let turn = match crate::tool_bridge::Turn::open(self.cancel.clone()) {
                Ok(turn) => turn,
                Err(e) => {
                    self.image_files.clear();
                    return Err(e);
                }
            };
            let exe = self.tool_server.clone().unwrap_or_else(|| turn.exe.clone());
            let command = self.bridged_command(arg, &exe);
            self.image_files.clear();
            return self.run_bridged(command, stdin_text, turn, on_event);
        }
        let mut command = self.command(arg);
        self.image_files.clear();
        crate::agent::process_tree::prepare(&mut command);
        if stdin_text.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| AgentError::new(format!("could not start `{}`: {e}", self.binary)))?;
        feed_stdin(&mut child, stdin_text);
        let tree = crate::agent::process_tree::Tree::adopt(&child);

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::new("the agent produced no output stream"))?;

        let stderr = crate::agent::stderr_tail::StderrTail::capture(child.stderr.take());

        let mut reading = Reading::new(&mut self.thread_id, self.model.clone(), false);

        for line in BufReader::new(stdout).lines() {
            if let Some(token) = &self.cancel {
                if token.is_cancelled() {
                    tree.kill(&mut child);
                    return Err(AgentError::new("the turn was cancelled"));
                }
            }
            let Ok(line) = line else { break };
            for event in reading.line(&line) {
                on_event(&event);
            }
        }
        let (outcome, usage) = reading.finish();

        on_event(&AgentEvent::Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_tokens: usage.cache_creation_input_tokens,
            cache_read_tokens: usage.cache_read_input_tokens,
        });

        let status = child
            .wait()
            .map_err(|e| AgentError::new(format!("the agent did not exit cleanly: {e}")))?;

        if let Some(outcome) = outcome {
            Ok(outcome)
        } else {
            let stderr_output = stderr.finish();
            let detail = if stderr_output.is_empty() {
                String::new()
            } else {
                format!(": {stderr_output}")
            };
            Err(AgentError::new(format!(
                "the agent produced no result line (exit {}){detail}",
                status.code().unwrap_or(-1)
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod pinned_tests;

#[cfg(test)]
mod tests;
