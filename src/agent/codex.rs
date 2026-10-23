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

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use super::{AgentBackend, AgentError, AgentEvent, TurnOutcome};

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

/// Codex's own hooks file off, on both spawn paths: incode fires the memory
/// engine's lifecycle itself, and codex firing it too would record each
/// session twice. Proved on 0.145.0 and 0.153.4, `exec` and `exec resume`:
/// with a probe hooks file (trust bypassed), hooks on recorded `session-start`
/// and `user-prompt`; `--disable hooks` recorded nothing, and codex no longer
/// read `~/.codex/hooks.json`.
const HOOKS_OFF: [&str; 2] = ["--disable", "hooks"];

/// The `-c` overrides that make codex a model and nothing else, on the
/// bridged route. Each key is from codex's own config types (`ConfigToml`,
/// `ToolsToml`, `features/src/lib.rs`), present in 0.145.0, 0.153.4 and
/// 0.163.0 unless said; a key a version does not know is ignored by it, not
/// refused (measured on 0.163.0 with an invented feature name), so a typo
/// here would do nothing — which is why each was watched leave the tool list.
///
/// - `features.shell_tool=false`: no `exec_command` / `write_stdin` / `shell`.
/// - `features.view_image=false`: no `view_image` (the key is 0.153+; 0.145
///   has no switch).
/// - `web_search="disabled"`: no hosted `web_search`.
/// - `features.goals=false`: no `get_goal` / `create_goal` / `update_goal`.
/// - `tools.experimental_request_user_input.enabled=false`: no
///   `request_user_input` — incode asks the person itself.
/// - `agents.enabled=false`: no sub-agents of codex's own. Needed beside
///   `features.multi_agent=false`, which only covers models the catalogue
///   leaves to the features: measured on 0.163.0, `gpt-5.6-terra` (whose
///   catalogue entry names multi-agent v2) was still offered
///   `collaboration.spawn_agent` & co. with the feature off. `agents.enabled`
///   wins over the catalogue (`Config::multi_agent_version_override`).
/// - `features.multi_agent=false`, `features.apps=false`,
///   `features.plugins=false`, `features.browser_use=false`,
///   `features.computer_use=false`, `features.image_generation=false`: no
///   sub-agents, connectors, plugin tools, browser, desktop or image tools of
///   codex's own; incode's toolbox has its own sub-agent, browser and desktop.
/// - `sandbox_mode="read-only"`, `approval_policy="never"`: what is left of
///   codex's own reach writes nothing — `apply_patch` is rejected before it
///   runs (see the module notes) — and nothing waits on a person.
pub const MODEL_ONLY_OVERRIDES: [&str; 14] = [
    "features.shell_tool=false",
    "features.view_image=false",
    "web_search=\"disabled\"",
    "features.goals=false",
    "tools.experimental_request_user_input.enabled=false",
    "agents.enabled=false",
    "features.multi_agent=false",
    "features.apps=false",
    "features.plugins=false",
    "features.browser_use=false",
    "features.computer_use=false",
    "features.image_generation=false",
    "sandbox_mode=\"read-only\"",
    "approval_policy=\"never\"",
];

/// Codex's user config (`$CODEX_HOME/config.toml`) is not read on the
/// bridged route — the counterpart of the Claude route's
/// `--setting-sources ""`: an MCP server the operator registered there would
/// be a tool incode cannot refuse. Auth still comes from `CODEX_HOME`
/// (`codex exec --help`). A global flag, taken by `exec resume` too.
const IGNORE_USER_CONFIG: &str = "--ignore-user-config";

/// The bridge's registration, as `-c` overrides: incode's tool server and
/// nothing else.
///
/// - `env_vars`: codex starts a stdio MCP server with ONLY a short list of
///   variables (`HOME`, `PATH`, …; `rmcp-client/src/utils.rs`
///   `create_env_for_mcp_server`) plus the ones named here. Measured on
///   0.163.0: a stand-in server received the two named and NOT a third set
///   beside them. The values never reach the argv.
/// - `default_tools_approval_mode="approve"`: the one mode in which codex never
///   asks before an MCP call (`requires_mcp_tool_approval_for_mode`); `auto`
///   asks for any tool not marked read-only, and with `approval_policy="never"`
///   that ask is a refusal. Whether a call may run is incode's decision.
/// - `required=true`: a server that fails to start fails the turn, rather than
///   the model running on with no tools.
/// - `tool_timeout_sec`: codex gives up on an MCP call after 300 s by default;
///   incode may still be asking the person, or running a long command.
/// - `omit_tools_from=["deferred"]`: the tools are listed to the model
///   directly. Without it codex hid them behind its `tool_search` (measured);
///   0.145.0 has no such key, and its model finds them through `tool_search`.
fn bridge_registration(exe: &Path) -> Vec<String> {
    let key = crate::tool_bridge::CLIENT_KEY;
    // TOML basic strings take backslash escapes: a Windows path must have its
    // separators doubled — see `desktop::mcp::codex_config_overrides`.
    let exe = exe.to_string_lossy().replace('\\', "\\\\");
    vec![
        format!("mcp_servers.{key}.command=\"{exe}\""),
        format!(
            "mcp_servers.{key}.args=[\"{}\"]",
            crate::tool_bridge::SERVER_FLAG
        ),
        format!(
            "mcp_servers.{key}.env_vars=[\"{}\",\"{}\"]",
            crate::tool_bridge::ADDR_ENV,
            crate::tool_bridge::TOKEN_ENV
        ),
        format!("mcp_servers.{key}.default_tools_approval_mode=\"approve\""),
        format!("mcp_servers.{key}.required=true"),
        format!(
            "mcp_servers.{key}.tool_timeout_sec={}",
            crate::tool_bridge::REPLY_BOUND_SECS
        ),
        format!("mcp_servers.{key}.omit_tools_from=[\"deferred\"]"),
    ]
}

// ---------------------------------------------------------------------------
// Wire types — measured from real Codex CLI runs
// ---------------------------------------------------------------------------

/// One line of `codex exec --json`.
///
/// Tagged on `type`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Wire {
    #[serde(rename = "thread.started")]
    ThreadStarted { thread_id: String },
    #[serde(rename = "turn.started")]
    TurnStarted,
    #[serde(rename = "item.started")]
    ItemStarted { item: WireItem },
    #[serde(rename = "item.completed")]
    ItemCompleted { item: WireItem },
    #[serde(rename = "turn.completed")]
    TurnCompleted {
        #[serde(default)]
        usage: Option<WireUsage>,
    },
    #[serde(rename = "turn.failed")]
    TurnFailed {
        #[serde(default)]
        error: Option<WireError>,
    },
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        message: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, Deserialize)]
struct WireItem {
    #[serde(default)]
    id: String,
    #[serde(rename = "type", default)]
    item_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    aggregated_output: Option<String>,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    arguments: Option<serde_json::Value>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<serde_json::Value>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    cached_input_tokens: u32,
    #[serde(default)]
    cache_write_input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[allow(dead_code)]
    #[serde(default)]
    reasoning_output_tokens: u32,
}

#[derive(Debug, Default, Deserialize)]
struct WireError {
    #[serde(default)]
    message: String,
}

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
    /// Unix only, as its one caller is (the bridged-turn test's fakes are
    /// shell scripts).
    #[cfg(all(test, unix))]
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

    /// The binary codex is told to start as the tool server.
    fn server_exe(&self) -> PathBuf {
        self.tool_server
            .clone()
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| PathBuf::from("incode"))
    }

    /// The command for one turn, on whichever route this backend is on.
    fn command(&self, prompt: &str) -> Command {
        if self.tools.is_some() {
            return self.bridged_command(prompt, &self.server_exe());
        }
        let mut cmd = Command::new(&self.binary);
        crate::agent::parent_session::detach_from_parent_session(&mut cmd);
        cmd.current_dir(&self.workspace);

        // WHO THIS AGENT IS ON THE TEAM CHANNEL, for the task about to run.
        // The team MCP server is a child of this process and reads it from
        // here — the registration cannot carry it, because MCP servers are
        // configured once and the role changes every task.
        if let Some(role) = &self.team_identity {
            cmd.env(crate::team_mcp::IDENTITY_ENV, role);
        }

        // The pictures for this turn, as files — `codex exec -i, --image
        // <FILE>...` (and the same on `exec resume`). Each goes FIRST, before
        // any other flag, so the next flag ends the list: `--image` takes
        // several values, and one placed before the prompt would take the
        // prompt for a file name.
        let images = |cmd: &mut Command| {
            for file in &self.image_files {
                cmd.arg("--image").arg(file);
            }
        };
        if let Some(tid) = &self.thread_id {
            cmd.arg("exec");
            cmd.arg("resume");
            images(&mut cmd);
            cmd.arg("--skip-git-repo-check");
            cmd.arg("--json");
            cmd.args(HOOKS_OFF);
            cmd.arg("--dangerously-bypass-approvals-and-sandbox");
            cmd.arg("-c")
                .arg("mcp_servers.memory.default_tools_approval_mode=\"auto\"");
            for override_ in &self.mcp_overrides {
                cmd.arg("-c").arg(override_);
            }
            if let Some(model) = &self.model {
                cmd.arg("-m").arg(model);
            }
            cmd.arg(tid);
            cmd.arg(prompt);
        } else {
            cmd.arg("exec");
            images(&mut cmd);
            cmd.arg("--skip-git-repo-check");
            cmd.arg("--json");
            cmd.args(HOOKS_OFF);
            cmd.arg("-c")
                .arg("mcp_servers.memory.default_tools_approval_mode=\"auto\"");
            for override_ in &self.mcp_overrides {
                cmd.arg("-c").arg(override_);
            }
            if let Some(sandbox) = &self.sandbox {
                cmd.arg("-s").arg(sandbox);
            } else {
                cmd.arg("--dangerously-bypass-approvals-and-sandbox");
            }
            if let Some(model) = &self.model {
                cmd.arg("-m").arg(model);
            }
            for dir in &self.extra_dirs {
                cmd.arg("--add-dir").arg(dir);
            }
            cmd.arg("-C").arg(&self.workspace);
            cmd.arg(prompt);
        }

        // Close stdin so Codex does not wait on interactive input.
        cmd.stdin(Stdio::null());
        cmd
    }

    /// The command for one bridged turn: codex as a model only, with `exe`
    /// started as its one MCP server. The bridge's address and token are
    /// added by the caller, which holds the bridge.
    ///
    /// Not handed over, because incode decides them in its toolbox: the
    /// permission mode (`-s`), the added roots (`--add-dir`), the bypass
    /// flag, the team and desktop servers ([`Self::with_mcp_overrides`] — both
    /// are toolbox tools here) and the memory server's approval override (the
    /// server lives in the user config this route does not read).
    fn bridged_command(&self, prompt: &str, exe: &Path) -> Command {
        let mut cmd = Command::new(&self.binary);
        crate::agent::parent_session::detach_from_parent_session(&mut cmd);
        cmd.current_dir(&self.workspace);
        cmd.arg("exec");
        if self.thread_id.is_some() {
            cmd.arg("resume");
        }
        // The pictures FIRST, as on the other route: `--image` takes several
        // values, and the next flag is what ends the list.
        for file in &self.image_files {
            cmd.arg("--image").arg(file);
        }
        cmd.arg("--skip-git-repo-check");
        cmd.arg("--json");
        cmd.args(HOOKS_OFF);
        cmd.arg(IGNORE_USER_CONFIG);
        for override_ in MODEL_ONLY_OVERRIDES
            .iter()
            .map(|o| (*o).to_string())
            .chain(bridge_registration(exe))
        {
            cmd.arg("-c").arg(override_);
        }
        if let Some(model) = &self.model {
            cmd.arg("-m").arg(model);
        }
        match &self.thread_id {
            // `exec resume` takes no `-C`: the thread carries its directory.
            Some(tid) => {
                cmd.arg(tid);
            }
            None => {
                cmd.arg("-C").arg(&self.workspace);
            }
        }
        cmd.arg(prompt);
        // No team identity in the environment: the team channel's tools are
        // the toolbox's on this route, and it is told who speaks.
        cmd.stdin(Stdio::null());
        cmd
    }
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
            return Some(bridge_registration(&self.server_exe()).join("\n"));
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
        let sent = self.full_prompt(prompt)?;
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
            let command = self.bridged_command(&sent, &exe);
            self.image_files.clear();
            return self.run_bridged(command, turn, on_event);
        }
        let mut command = self.command(&sent);
        self.image_files.clear();
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

impl CodexBackend {
    /// One bridged turn: `command` spawned with the bridge's address and token
    /// in its environment, its tool calls answered by the toolbox, its stream
    /// read to the end — the Claude route's loop ([`crate::tool_bridge::drive`])
    /// over codex's lines.
    fn run_bridged(
        &mut self,
        mut command: Command,
        turn: crate::tool_bridge::Turn,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError> {
        let crate::tool_bridge::Turn {
            bridge, to, from, ..
        } = turn;
        // How the tool server finds this process: codex's environment, which
        // it passes on to the server by the names `env_vars` lists — never
        // its argv.
        for (key, value) in bridge.env() {
            command.env(key, value);
        }
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
        crate::tool_bridge::pump(BufReader::new(stdout), to);

        let Self {
            tools,
            calls,
            thread_id,
            model,
            cancel,
            ..
        } = self;
        let tools = tools
            .as_mut()
            .ok_or_else(|| AgentError::new("a bridged turn needs incode's toolbox"))?;
        let mut reading = Reading::new(thread_id, model.clone(), true);
        let driven = crate::tool_bridge::drive(
            &from,
            cancel.as_ref(),
            tools,
            calls,
            on_event,
            &mut |line, tools| {
                let events = reading.line(&line);
                for event in &events {
                    if let AgentEvent::SessionStarted { session_id, .. } = event {
                        // The engine files its cards under the conversation.
                        tools.set_session(session_id);
                    }
                }
                events
            },
        );
        let (outcome, usage) = reading.finish();
        // Closed as soon as the loop is done: a call that arrives from here on
        // is refused rather than left waiting on a loop that is gone.
        drop(bridge);
        if let Err(e) = driven {
            // Whatever happened, codex must not outlive the turn — nor
            // whatever it started.
            tree.kill(&mut child);
            return Err(e);
        }

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

/// What one turn of `codex exec --json` has said so far, and what each line
/// means — the same reading on both routes.
struct Reading<'a> {
    /// The backend's own: a thread that started is resumable even if the
    /// turn is then stopped.
    thread_id: &'a mut Option<String>,
    model: Option<String>,
    outcome: Option<TurnOutcome>,
    usage: crate::runtime::usage::TokenUsage,
    /// Bridged: a call to incode's own server is shown by the bridge, as
    /// incode ran it, under incode's id; codex's record of the same call is
    /// not shown a second time.
    bridged: bool,
}

impl<'a> Reading<'a> {
    fn new(thread_id: &'a mut Option<String>, model: Option<String>, bridged: bool) -> Self {
        Self {
            thread_id,
            model,
            outcome: None,
            usage: crate::runtime::usage::TokenUsage::default(),
            bridged,
        }
    }

    /// The turn's result, if it reported one, and what it cost.
    fn finish(self) -> (Option<TurnOutcome>, crate::runtime::usage::TokenUsage) {
        (self.outcome, self.usage)
    }

    /// Whether an item is codex's own echo of a call the bridge already showed.
    fn shown_by_the_bridge(&self, item: &WireItem) -> bool {
        self.bridged && item.server.as_deref() == Some(crate::tool_bridge::CLIENT_KEY)
    }

    /// The events one line stands for. A line that is not one of codex's is
    /// nothing.
    fn line(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(wire) = serde_json::from_str::<Wire>(line) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        match wire {
            Wire::ThreadStarted { thread_id } => {
                *self.thread_id = Some(thread_id.clone());
                events.push(AgentEvent::SessionStarted {
                    session_id: thread_id,
                    model: self.model.clone().unwrap_or_default(),
                });
            }
            Wire::TurnStarted => {}
            Wire::ItemStarted { item } if self.shown_by_the_bridge(&item) => {}
            Wire::ItemStarted { item } => match item.item_type.as_str() {
                "command_execution" => {
                    if let Some(cmd) = item.command {
                        events.push(AgentEvent::ToolUse {
                            id: item.id,
                            name: "Bash".to_string(),
                            input: cmd,
                        });
                    }
                }
                "mcp_tool_call" => {
                    if let Some(tool) = item.tool {
                        let input = item
                            .arguments
                            .as_ref()
                            .map(|a| a.to_string())
                            .unwrap_or_default();
                        events.push(AgentEvent::ToolUse {
                            id: item.id,
                            name: tool,
                            input,
                        });
                    }
                }
                _ => {}
            },
            Wire::ItemCompleted { item } if self.shown_by_the_bridge(&item) => {}
            Wire::ItemCompleted { item } => match item.item_type.as_str() {
                "agent_message" => {
                    if let Some(text) = item.text {
                        if !text.trim().is_empty() {
                            events.push(AgentEvent::TextDelta(text.clone()));
                            events.push(AgentEvent::Text(text));
                        }
                    }
                }
                "command_execution" => {
                    if let Some(cmd) = item.command {
                        events.push(AgentEvent::ToolUse {
                            id: item.id.clone(),
                            name: "Bash".to_string(),
                            input: cmd,
                        });
                        events.push(AgentEvent::ToolResult {
                            id: item.id,
                            output: item.aggregated_output.unwrap_or_default(),
                            is_error: item.exit_code.unwrap_or(0) != 0,
                        });
                    }
                }
                "mcp_tool_call" => {
                    if let Some(tool) = item.tool {
                        let input = item
                            .arguments
                            .as_ref()
                            .map(|a| a.to_string())
                            .unwrap_or_default();
                        events.push(AgentEvent::ToolUse {
                            id: item.id.clone(),
                            name: tool,
                            input,
                        });
                        let mut output = extract_mcp_output(&item.result);
                        if output.is_empty() {
                            if let Some(err) = &item.error {
                                output = err
                                    .as_str()
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| parse_error_detail(&err.to_string()));
                            }
                        }
                        let is_error =
                            item.error.is_some() || item.status.as_deref() == Some("failed");
                        events.push(AgentEvent::ToolResult {
                            id: item.id,
                            output,
                            is_error,
                        });
                    }
                }
                "error" => {
                    if let Some(msg) = item.text {
                        crate::trace!("agent", "codex item error: {msg}");
                    }
                }
                _ => {}
            },
            Wire::TurnCompleted { usage: u } => {
                if let Some(u) = &u {
                    self.usage = to_usage(u);
                }
                let sid = self.thread_id.clone().unwrap_or_default();
                self.outcome = Some(TurnOutcome {
                    session_id: sid,
                    stop_reason: "SUCCESS".to_string(),
                    is_error: false,
                    detail: None,
                    served_by: self.model.clone().into_iter().collect(),
                });
            }
            Wire::TurnFailed { error } => {
                let err_msg = error
                    .map(|e| parse_error_detail(&e.message))
                    .unwrap_or_else(|| "turn failed".to_string());
                let sid = self.thread_id.clone().unwrap_or_default();
                self.outcome = Some(TurnOutcome {
                    session_id: sid,
                    stop_reason: "ERROR".to_string(),
                    is_error: true,
                    detail: Some(err_msg),
                    served_by: self.model.clone().into_iter().collect(),
                });
            }
            Wire::Error { message } => {
                let err_msg = message
                    .map(|m| parse_error_detail(&m))
                    .unwrap_or_else(|| "codex error".to_string());
                let sid = self.thread_id.clone().unwrap_or_default();
                self.outcome = Some(TurnOutcome {
                    session_id: sid,
                    stop_reason: "ERROR".to_string(),
                    is_error: true,
                    detail: Some(err_msg),
                    served_by: self.model.clone().into_iter().collect(),
                });
            }
            Wire::Other => {}
        }
        events
    }
}

/// Extract readable text from MCP tool result JSON.
fn extract_mcp_output(res: &Option<serde_json::Value>) -> String {
    let Some(val) = res else {
        return String::new();
    };
    if let Some(content) = val.get("content").and_then(|c| c.as_array()) {
        let texts: Vec<&str> = content
            .iter()
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .collect();
        if !texts.is_empty() {
            return texts.join("\n");
        }
    }
    val.to_string()
}

/// Parse error message, unpacking nested JSON error objects if present.
fn parse_error_detail(raw: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(inner) = v.pointer("/error/message").and_then(|m| m.as_str()) {
            return inner.to_string();
        }
        if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
            return msg.to_string();
        }
    }
    raw.to_string()
}

fn to_usage(wire: &WireUsage) -> crate::runtime::usage::TokenUsage {
    crate::runtime::usage::TokenUsage {
        input_tokens: wire.input_tokens,
        output_tokens: wire.output_tokens,
        cache_creation_input_tokens: wire.cache_write_input_tokens,
        cache_read_input_tokens: wire.cached_input_tokens,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod pinned_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #3: launched from inside another agent session, a child that
    /// inherits its variables reports that session as its own — on both the
    /// fresh and the resumed spawn.
    #[test]
    fn the_cli_never_joins_the_session_that_launched_incode() {
        let mut backend = CodexBackend::new(PathBuf::from("."));
        for resumed in [false, true] {
            if resumed {
                backend.thread_id = Some("t-1".to_string());
            }
            let cmd = backend.command("p");
            assert!(
                cmd.get_envs()
                    .any(|(k, v)| k == "CLAUDECODE" && v.is_none()),
                "CLAUDECODE is inherited (resumed={resumed}), so the child joins the parent's session"
            );
        }
    }

    fn parse(line: &str) -> Wire {
        serde_json::from_str(line).expect("a wire line")
    }

    #[test]
    fn the_thread_started_line_sets_session_id() {
        let wire = parse(
            r#"{"type":"thread.started","thread_id":"01a081c2-bdb1-7c71-a922-9a10bbb2ccae"}"#,
        );
        match wire {
            Wire::ThreadStarted { thread_id } => {
                assert_eq!(thread_id, "01a081c2-bdb1-7c71-a922-9a10bbb2ccae");
            }
            other => panic!("expected ThreadStarted: {other:?}"),
        }
    }

    #[test]
    fn agent_message_item_is_parsed() {
        let wire = parse(
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"The package name is incord-code."}}"#,
        );
        match wire {
            Wire::ItemCompleted { item } => {
                assert_eq!(item.item_type, "agent_message");
                assert_eq!(
                    item.text.as_deref(),
                    Some("The package name is incord-code.")
                );
            }
            other => panic!("expected ItemCompleted: {other:?}"),
        }
    }

    #[test]
    fn command_execution_item_is_parsed() {
        let wire = parse(
            r#"{"type":"item.completed","item":{"id":"item_5","type":"command_execution","command":"powershell.exe -Command Get-Location","aggregated_output":"C:\\Projects\\incord-code\n","exit_code":0,"status":"completed"}}"#,
        );
        match wire {
            Wire::ItemCompleted { item } => {
                assert_eq!(item.item_type, "command_execution");
                assert_eq!(
                    item.command.as_deref(),
                    Some("powershell.exe -Command Get-Location")
                );
                assert_eq!(item.exit_code, Some(0));
            }
            other => panic!("expected ItemCompleted: {other:?}"),
        }
    }

    #[test]
    fn mcp_tool_call_item_is_parsed() {
        let wire = parse(
            r#"{"type":"item.completed","item":{"id":"item_1","type":"mcp_tool_call","server":"memory","tool":"watchman_lookup","arguments":{"task":"C:\\Projects\\incord-code"},"result":{"content":[{"type":"text","text":"{\"hit\":false}"}]},"error":null,"status":"completed"}}"#,
        );
        match wire {
            Wire::ItemCompleted { item } => {
                assert_eq!(item.item_type, "mcp_tool_call");
                assert_eq!(item.tool.as_deref(), Some("watchman_lookup"));
                assert_eq!(extract_mcp_output(&item.result), "{\"hit\":false}");
            }
            other => panic!("expected ItemCompleted: {other:?}"),
        }
    }

    #[test]
    fn turn_completed_usage_is_parsed() {
        let wire = parse(
            r#"{"type":"turn.completed","usage":{"input_tokens":122070,"cached_input_tokens":112128,"cache_write_input_tokens":100,"output_tokens":438,"reasoning_output_tokens":83}}"#,
        );
        match wire {
            Wire::TurnCompleted { usage } => {
                let u = to_usage(&usage.expect("usage"));
                assert_eq!(u.input_tokens, 122070);
                assert_eq!(u.cache_read_input_tokens, 112128);
                assert_eq!(u.cache_creation_input_tokens, 100);
                assert_eq!(u.output_tokens, 438);
            }
            other => panic!("expected TurnCompleted: {other:?}"),
        }
    }

    #[test]
    fn turn_failed_unpacks_json_error() {
        let wire = parse(
            r#"{"type":"turn.failed","error":{"message":"{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'bad-model' model is not supported.\"}}"}}"#,
        );
        match wire {
            Wire::TurnFailed { error } => {
                let detail = parse_error_detail(&error.expect("error").message);
                assert_eq!(detail, "The 'bad-model' model is not supported.");
            }
            other => panic!("expected TurnFailed: {other:?}"),
        }
    }

    #[test]
    fn foreign_model_id_is_dropped() {
        let mut backend = CodexBackend::new(PathBuf::from("."));
        backend.set_models(&[
            "claude-sonnet-4-6".to_string(),
            "gemini-3.8-flash-high".to_string(),
        ]);
        assert_eq!(backend.model(), None);

        backend.set_models(&["claude-sonnet-4-6".to_string(), "gpt-5.6-terra".to_string()]);
        assert_eq!(backend.model(), Some("gpt-5.6-terra"));
    }

    #[test]
    fn role_ladder_contains_supported_models() {
        let backend = CodexBackend::new(PathBuf::from("."));
        for role in crate::roles::KNOWN_ROLES {
            for id in backend.ladder_for(role) {
                assert!(
                    backend.serves(&id),
                    "role {role} drew model {id} which codex cannot serve"
                );
            }
        }
        assert_ne!(
            backend.ladder_for("explore"),
            backend.ladder_for("implement")
        );
    }

    #[test]
    fn harness_context_is_carried_in_prompt() {
        let bare = CodexBackend::new(PathBuf::from("."));
        assert_eq!(
            bare.full_prompt("fix the bug").expect("sent"),
            "fix the bug"
        );

        let briefed = CodexBackend::new(PathBuf::from("."))
            .with_system_context(vec!["Guards: no writes outside src/".to_string()]);
        let sent = briefed.full_prompt("fix the bug").expect("sent");
        assert!(sent.contains("Guards: no writes outside src/"));
        assert!(sent.ends_with("fix the bug"));
    }

    #[test]
    fn enforces_claims_nothing() {
        let backend = CodexBackend::new(PathBuf::from("."));
        assert_eq!(backend.enforces(), crate::guards::Enforcement::NOTHING);
    }

    /// **A TURN'S PICTURES GO AS `--image` FILES, BEFORE ANY OTHER FLAG**, on
    /// a new thread and on a resumed one, so the prompt is never read as one
    /// more file; the files are removed when the turn's hold on them ends.
    #[test]
    fn pictures_go_first_as_image_files_and_are_removed_after() {
        let picture = crate::toolbox::Image::from_base64(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC",
        )
        .expect("a 1x1 png");
        let files = ImageFiles::write(std::slice::from_ref(&picture)).expect("written");
        let path = files.paths[0].clone();
        assert_eq!(std::fs::read(&path).expect("on disk"), picture.bytes());
        let mut backend = CodexBackend::new(PathBuf::from("."));
        backend.image_files.clone_from(&files.paths);
        for resumed in [false, true] {
            if resumed {
                backend.thread_id = Some("t-1".to_string());
            }
            let args: Vec<String> = backend
                .command("the prompt")
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let at = if resumed { 2 } else { 1 };
            assert_eq!(args[at], "--image", "{args:?}");
            assert_eq!(args[at + 1], path.to_string_lossy(), "{args:?}");
            assert!(
                args[at + 2].starts_with("--"),
                "the list is ended: {args:?}"
            );
            assert_eq!(args.last().map(String::as_str), Some("the prompt"));
        }
        drop(files);
        assert!(!path.exists(), "the picture outlived its turn");
    }

    #[test]
    fn command_includes_mcp_auto_approval_config() {
        let backend = CodexBackend::new(PathBuf::from("."));
        let cmd = backend.command("test prompt");
        let args: Vec<std::ffi::OsString> = cmd.get_args().map(|a| a.to_os_string()).collect();
        let args_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            args_str
                .windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("default_tools_approval_mode=\"auto\"")),
            "codex command must include auto approval for memory MCP tools so calls are not cancelled"
        );
    }

    /// **THE TEAM IDENTITY MUST REACH THE SPAWNED PROCESS'S ENVIRONMENT.**
    ///
    /// The team MCP server is a child of the agent and reads who it speaks
    /// for from this variable. It cannot come from the MCP registration —
    /// that is written once and the role changes every task — so if it does
    /// not land on the spawn, every mid-turn message is filed as `unrouted`
    /// and the channel stops being addressable.
    #[test]
    fn the_team_identity_is_set_on_the_spawn() {
        let mut backend = CodexBackend::new(PathBuf::from("."));
        backend.set_team_identity("implement");

        let cmd = backend.command("test prompt");
        let found = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(crate::team_mcp::IDENTITY_ENV));

        let (_, value) = found.expect("the identity never reached the environment");
        assert_eq!(
            value.expect("a value").to_string_lossy(),
            "implement",
            "the agent would speak under the wrong name"
        );

        // And a backend nobody told stays silent rather than inventing one.
        let plain = CodexBackend::new(PathBuf::from("."));
        assert!(
            !plain
                .command("p")
                .get_envs()
                .any(|(k, _)| k == std::ffi::OsStr::new(crate::team_mcp::IDENTITY_ENV)),
            "an identity was invented for a task that had no role"
        );
    }

    /// **THE DESKTOP MUST REACH ARGV, ON BOTH SPAWN PATHS.**
    ///
    /// `mcp_config()` is an accessor — it reads the field back and would keep
    /// answering after the `-c` loops were deleted. What the spawned process
    /// actually receives is argv, so that is what this asserts. Both paths,
    /// because `command` builds a *different* argument list when resuming a
    /// thread (line 218) than when starting fresh (line 234): wiring one and
    /// forgetting the other is a desktop that works until the first resume.
    #[test]
    fn the_desktop_overrides_reach_argv_on_both_spawn_paths() {
        let overrides = crate::desktop::mcp::codex_config_overrides(
            std::path::Path::new(r"C:\Users\someone\incode.exe"),
            Some(std::path::Path::new(r"C:\work\.incode\desktop.jsonl")),
            crate::permissions::PermissionMode::WorkspaceWrite,
        );

        let argv = |backend: &CodexBackend| -> Vec<String> {
            backend
                .command("test prompt")
                .get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect()
        };
        let carries = |args: &[String]| {
            args.windows(2)
                .filter(|w| w[0] == "-c" && w[1].starts_with("mcp_servers.desktop."))
                .count()
        };

        let fresh = CodexBackend::new(PathBuf::from(".")).with_mcp_overrides(overrides.clone());
        assert_eq!(
            carries(&argv(&fresh)),
            overrides.len(),
            "fresh spawn dropped the desktop: {:?}",
            argv(&fresh)
        );

        let mut resumed =
            CodexBackend::new(PathBuf::from(".")).with_mcp_overrides(overrides.clone());
        resumed.thread_id = Some("thread-abc".to_string());
        assert_eq!(
            carries(&argv(&resumed)),
            overrides.len(),
            "resume spawn dropped the desktop: {:?}",
            argv(&resumed)
        );

        // And without it, nothing: the flag is not unconditional.
        let plain = CodexBackend::new(PathBuf::from("."));
        assert_eq!(carries(&argv(&plain)), 0);
    }

    /// **CODEX'S OWN HOOKS ARE OFF ON BOTH SPAWN PATHS.** incode fires the
    /// memory engine's lifecycle itself; codex loading its hooks file too
    /// would record every session twice, under two ids.
    #[test]
    fn codex_is_started_with_its_own_hooks_off_on_both_paths() {
        let off = |backend: &CodexBackend| {
            let args: Vec<String> = backend
                .command("hi")
                .get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect();
            args.windows(2)
                .any(|w| w[0] == "--disable" && w[1] == "hooks")
        };
        let fresh = CodexBackend::new(PathBuf::from("."));
        assert!(off(&fresh));
        let mut resumed = CodexBackend::new(PathBuf::from("."));
        resumed.thread_id = Some("thread-abc".to_string());
        assert!(off(&resumed));
        for mut backend in [fresh, resumed] {
            backend = backend.with_tools(CodexTools::Incode);
            assert!(off(&backend), "bridged too");
        }
    }

    // -----------------------------------------------------------------------
    // Bridged: codex as a model, incode's toolbox as its tools.
    // -----------------------------------------------------------------------

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// Every `-c` value on an argv.
    fn overrides_in(args: &[String]) -> Vec<String> {
        args.windows(2)
            .filter(|w| w[0] == "-c")
            .map(|w| w[1].clone())
            .collect()
    }

    /// **CODEX ON ITS OWN TOOLS IS STARTED EXACTLY AS BEFORE.** The whole argv,
    /// both spawn paths, pinned: the bridged route must not leak a flag onto
    /// the route that has run live.
    #[test]
    fn the_own_tools_argv_is_unchanged() {
        let mut backend = CodexBackend::new(PathBuf::from("."))
            .with_mcp_overrides(vec!["mcp_servers.team.command=\"incode\"".to_string()]);
        backend.set_models(&["gpt-5.5".to_string()]);
        backend.set_permission_mode("plan");
        backend.set_extra_dirs(vec![PathBuf::from("/more")]);
        let auto = "mcp_servers.memory.default_tools_approval_mode=\"auto\"";
        let team = "mcp_servers.team.command=\"incode\"";
        assert_eq!(
            args_of(&backend.command("the task")),
            [
                "exec",
                "--skip-git-repo-check",
                "--json",
                "--disable",
                "hooks",
                "-c",
                auto,
                "-c",
                team,
                "-s",
                "read-only",
                "-m",
                "gpt-5.5",
                "--add-dir",
                "/more",
                "-C",
                ".",
                "the task",
            ]
        );
        backend.thread_id = Some("t-1".to_string());
        assert_eq!(
            args_of(&backend.command("again")),
            [
                "exec",
                "resume",
                "--skip-git-repo-check",
                "--json",
                "--disable",
                "hooks",
                "--dangerously-bypass-approvals-and-sandbox",
                "-c",
                auto,
                "-c",
                team,
                "-m",
                "gpt-5.5",
                "t-1",
                "again",
            ]
        );
        assert!(!backend.runs_tools_here());
        assert!(backend.offered_tools().is_empty());
    }

    /// **BRIDGED, CODEX IS STARTED AS A MODEL AND NOTHING ELSE**, on both
    /// spawn paths: its user config unread, every tool switch off, read-only
    /// and never asking, and incode's tool server its one MCP server — and
    /// none of the flags the other route hands it, because incode's toolbox
    /// decides what they decided.
    #[test]
    fn bridged_codex_is_started_as_a_model_with_incodes_server_only() {
        let exe = Path::new(r"C:\Program Files\incode\incode.exe");
        let mut backend = CodexBackend::new(PathBuf::from("."))
            .with_mcp_overrides(vec!["mcp_servers.team.command=\"incode\"".to_string()])
            .with_tools(CodexTools::Incode);
        backend.set_models(&["gpt-5.5".to_string()]);
        backend.set_permission_mode("acceptEdits");
        backend.set_extra_dirs(vec![PathBuf::from("/more")]);
        backend.set_team_identity("implement");
        for resumed in [false, true] {
            if resumed {
                backend.thread_id = Some("t-1".to_string());
            }
            let cmd = backend.bridged_command("the task", exe);
            let args = args_of(&cmd);
            let lead: &[&str] = if resumed {
                &["exec", "resume"]
            } else {
                &["exec"]
            };
            assert_eq!(&args[..lead.len()], lead, "{args:?}");
            for flag in ["--skip-git-repo-check", "--json", "--ignore-user-config"] {
                assert!(args.iter().any(|a| a == flag), "{flag}: {args:?}");
            }
            assert!(args.windows(2).any(|w| w == ["--disable", "hooks"]));
            let overrides = overrides_in(&args);
            let mut expected: Vec<String> = MODEL_ONLY_OVERRIDES
                .iter()
                .map(|o| (*o).to_string())
                .collect();
            expected.extend([
                r#"mcp_servers.incode.command="C:\\Program Files\\incode\\incode.exe""#.to_string(),
                r#"mcp_servers.incode.args=["--tool-server"]"#.to_string(),
                r#"mcp_servers.incode.env_vars=["INCODE_TOOL_BRIDGE","INCODE_TOOL_TOKEN"]"#
                    .to_string(),
                r#"mcp_servers.incode.default_tools_approval_mode="approve""#.to_string(),
                "mcp_servers.incode.required=true".to_string(),
                "mcp_servers.incode.tool_timeout_sec=3600".to_string(),
                r#"mcp_servers.incode.omit_tools_from=["deferred"]"#.to_string(),
            ]);
            assert_eq!(overrides, expected, "resumed={resumed}");
            for gone in [
                "--dangerously-bypass-approvals-and-sandbox",
                "-s",
                "--add-dir",
            ] {
                assert!(!args.iter().any(|a| a == gone), "{gone}: {args:?}");
            }
            assert!(
                !overrides
                    .iter()
                    .any(|o| o.starts_with("mcp_servers.team.")
                        || o.starts_with("mcp_servers.memory.")),
                "a server beside the bridge: {overrides:?}"
            );
            assert_eq!(
                args.iter().any(|a| a == "-C"),
                !resumed,
                "`exec resume` takes no -C: {args:?}"
            );
            assert_eq!(args.last().map(String::as_str), Some("the task"));
            if resumed {
                assert_eq!(args[args.len() - 2], "t-1", "{args:?}");
            }
            assert!(
                !cmd.get_envs()
                    .any(|(k, _)| k == std::ffi::OsStr::new(crate::team_mcp::IDENTITY_ENV)),
                "the team identity is the toolbox's on this route"
            );
            assert!(
                cmd.get_envs()
                    .any(|(k, v)| k == "CLAUDECODE" && v.is_none()),
                "issue #3 on this route too"
            );
        }
        // The mode and the roots went to the toolbox, not to codex.
        assert_eq!(backend.sandbox, None);
    }

    /// Bridged, the backend says so to every caller that asks: its calls run
    /// here, it enforces both halves, and it offers the toolbox's tools.
    #[test]
    fn bridged_codex_runs_its_tools_here() {
        let backend = CodexBackend::new(PathBuf::from(".")).with_tools(CodexTools::Incode);
        assert!(backend.runs_tools_here());
        assert_eq!(backend.enforces(), crate::guards::Enforcement::EVERYTHING);
        let tools = backend.offered_tools();
        for tool in ["Read", "Write", "Edit", "Bash", "team_ask"] {
            assert!(tools.iter().any(|t| t == tool), "{tool}: {tools:?}");
        }
        assert!(
            backend
                .mcp_config()
                .is_some_and(|c| c.contains("--tool-server")),
            "{:?}",
            backend.mcp_config()
        );
        // And setting it back to codex's own is the route as it was.
        let own = CodexBackend::new(PathBuf::from("."))
            .with_tools(CodexTools::Incode)
            .with_tools(CodexTools::Own);
        assert!(!own.runs_tools_here());
        assert_eq!(own.mcp_config(), None);
    }

    #[test]
    fn the_setting_spells_own_and_incode() {
        assert_eq!(CodexTools::parse("own"), Some(CodexTools::Own));
        assert_eq!(CodexTools::parse(" INCODE "), Some(CodexTools::Incode));
        assert_eq!(CodexTools::parse("bridge"), None);
        assert_eq!(CodexTools::parse(""), None);
        assert_eq!(CodexTools::default(), CodexTools::Own);
        for tools in [CodexTools::Own, CodexTools::Incode] {
            assert_eq!(CodexTools::parse(tools.as_str()), Some(tools));
        }
    }

    /// The events a sequence of `codex exec --json` lines stands for.
    fn read(lines: &[&str], bridged: bool) -> (Vec<AgentEvent>, Option<String>) {
        let mut thread = None;
        let mut reading = Reading::new(&mut thread, Some("gpt-5.5".to_string()), bridged);
        let events = lines.iter().flat_map(|l| reading.line(l)).collect();
        drop(reading);
        (events, thread)
    }

    /// An MCP call to incode's own server, in the shape `codex exec --json`
    /// prints it (`exec/src/exec_events.rs` `McpToolCallItem`, 0.163.0): a
    /// started item, then a completed one with the result's content blocks.
    const INCODE_CALL: [&str; 2] = [
        r#"{"type":"item.started","item":{"id":"item_1","type":"mcp_tool_call","server":"incode","tool":"Write","arguments":{"path":"a.txt","content":"x"},"result":null,"error":null,"status":"in_progress"}}"#,
        r#"{"type":"item.completed","item":{"id":"item_1","type":"mcp_tool_call","server":"incode","tool":"Write","arguments":{"path":"a.txt","content":"x"},"result":{"content":[{"type":"text","text":"wrote a.txt"}],"structured_content":null},"error":null,"status":"completed"}}"#,
    ];

    /// **BRIDGED, A CALL TO incode's SERVER IS SHOWN ONCE — AS INCODE RAN
    /// IT.** The bridge shows the call under incode's id when it runs it;
    /// codex's own record of the same call is not shown again. A call to any
    /// other server is still shown from codex's record.
    #[test]
    fn bridged_codex_does_not_show_incodes_calls_twice() {
        let (events, _) = read(&INCODE_CALL, true);
        assert!(events.is_empty(), "{events:?}");

        let other = r#"{"type":"item.completed","item":{"id":"item_2","type":"mcp_tool_call","server":"elsewhere","tool":"lookup","arguments":{},"result":{"content":[{"type":"text","text":"found"}]},"error":null,"status":"completed"}}"#;
        let (events, _) = read(&[other], true);
        assert!(
            matches!(&events[..], [AgentEvent::ToolUse { name, .. }, AgentEvent::ToolResult { output, is_error: false, .. }] if name == "lookup" && output == "found"),
            "{events:?}"
        );
    }

    /// On codex's own tools the same lines are its tool calls, mapped as
    /// they always were: a use when the call starts, and the use and its
    /// result when it completes.
    #[test]
    fn own_tools_codex_shows_an_mcp_call_from_its_record() {
        let (events, _) = read(&INCODE_CALL, false);
        assert!(
            matches!(
                &events[..],
                [
                    AgentEvent::ToolUse { id: a, name: n1, .. },
                    AgentEvent::ToolUse { id: b, name: n2, input },
                    AgentEvent::ToolResult { output, is_error: false, .. },
                ] if a == "item_1" && b == "item_1" && n1 == "Write" && n2 == "Write"
                    && input.contains("a.txt") && output == "wrote a.txt"
            ),
            "{events:?}"
        );
    }

    /// A failed MCP call carries its error as an object — `{"message": …}`,
    /// not a string — and is shown as a failed result with that message.
    #[test]
    fn a_failed_mcp_call_shows_its_message() {
        let failed = r#"{"type":"item.completed","item":{"id":"item_3","type":"mcp_tool_call","server":"elsewhere","tool":"Write","arguments":{},"result":null,"error":{"message":"tool call timed out"},"status":"failed"}}"#;
        let (events, _) = read(&[failed], false);
        assert!(
            matches!(&events[..], [_, AgentEvent::ToolResult { output, is_error: true, .. }] if output == "tool call timed out"),
            "{events:?}"
        );
    }

    /// The rest of a turn reads the same on both routes: the thread, the
    /// reply, and the result.
    #[test]
    fn a_turn_reads_the_same_on_both_routes() {
        let lines = [
            r#"{"type":"thread.started","thread_id":"th-1"}"#,
            r#"{"type":"turn.started"}"#,
            r#"Reading additional input from stdin..."#,
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"done"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":3,"cached_input_tokens":1,"output_tokens":2}}"#,
        ];
        let (own, own_thread) = read(&lines, false);
        let (bridged, bridged_thread) = read(&lines, true);
        assert_eq!(own, bridged);
        assert_eq!(own_thread.as_deref(), Some("th-1"));
        assert_eq!(bridged_thread.as_deref(), Some("th-1"));
        assert!(
            matches!(&own[0], AgentEvent::SessionStarted { session_id, .. } if session_id == "th-1")
        );
        assert!(own
            .iter()
            .any(|e| matches!(e, AgentEvent::Text(t) if t == "done")));
    }

    // -----------------------------------------------------------------------
    // The flow, end to end, with a stand-in codex.
    // -----------------------------------------------------------------------

    /// Set only in the stand-in codex the flow test starts: the directory it
    /// reads its argv from.
    const FAKE_CODEX: &str = "INCODE_FAKE_CODEX";
    /// Set only in the stand-in tool server the stand-in codex starts.
    const FAKE_TOOL_SERVER: &str = "INCODE_FAKE_TOOL_SERVER";

    /// The value of the `-c` override for `key`, as codex's TOML reads it.
    fn override_value<'a>(overrides: &'a [String], key: &str) -> &'a str {
        overrides
            .iter()
            .find_map(|o| o.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("no {key} among {overrides:?}"))
    }

    /// The tool server half, as `incode --tool-server` runs it.
    #[test]
    fn tool_server_child() {
        if std::env::var_os(FAKE_TOOL_SERVER).is_none() {
            return;
        }
        let _ = crate::tool_bridge::run_stdio();
        std::process::exit(0);
    }

    /// A stand-in codex: reads the argv it was started with, starts the MCP
    /// server it was told to — with ONLY the variables `env_vars` names, as
    /// codex does — and, over MCP stdio, lists the tools and calls `Write`
    /// twice. It prints what it did as `codex exec --json` lines.
    #[test]
    fn fake_codex_child() {
        use std::io::Write as _;
        let Some(dir) = std::env::var_os(FAKE_CODEX) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let argv = std::fs::read(dir.join("argv")).expect("argv");
        let args: Vec<String> = argv
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        let overrides = overrides_in(&args);
        let command = override_value(&overrides, "mcp_servers.incode.command")
            .trim_matches('"')
            .replace("\\\\", "\\");
        let server_args: Vec<String> =
            serde_json::from_str(override_value(&overrides, "mcp_servers.incode.args"))
                .expect("args");
        let passed: Vec<String> =
            serde_json::from_str(override_value(&overrides, "mcp_servers.incode.env_vars"))
                .expect("env_vars");
        let mut server = std::process::Command::new(&command)
            .args(&server_args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .envs(
                passed
                    .iter()
                    .filter_map(|name| Some((name, std::env::var_os(name)?))),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start the tool server");
        let mut to = server.stdin.take().expect("stdin");
        let mut from = BufReader::new(server.stdout.take().expect("stdout"));
        let mut ask = |id: u64, method: &str, params: serde_json::Value| -> serde_json::Value {
            let request =
                serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
            writeln!(to, "{request}").expect("send");
            to.flush().expect("flush");
            loop {
                let mut line = String::new();
                if from.read_line(&mut line).expect("read") == 0 {
                    panic!("the tool server went away");
                }
                // The test harness's own banner is not a reply.
                if let Ok(reply) = serde_json::from_str::<serde_json::Value>(&line) {
                    if reply["id"] == id {
                        return reply;
                    }
                }
            }
        };
        let say = |line: serde_json::Value| println!("{line}");
        say(serde_json::json!({"type":"thread.started","thread_id":"fake-thread"}));
        say(serde_json::json!({"type":"turn.started"}));
        ask(
            0,
            "initialize",
            serde_json::json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fake-codex","version":"0"}}),
        );
        let listed = ask(1, "tools/list", serde_json::json!({}));
        let names: Vec<&str> = listed["result"]["tools"]
            .as_array()
            .map(|t| t.iter().filter_map(|t| t["name"].as_str()).collect())
            .unwrap_or_default();
        say(
            serde_json::json!({"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":format!("tools: {}", names.join(","))}}),
        );
        for (n, path) in [(2, "notes.txt"), (3, "secret.txt")] {
            let arguments = serde_json::json!({"path": path, "content": "written"});
            let item = format!("item_{n}");
            say(
                serde_json::json!({"type":"item.started","item":{"id":item,"type":"mcp_tool_call","server":"incode","tool":"Write","arguments":arguments,"result":null,"error":null,"status":"in_progress"}}),
            );
            let reply = ask(
                n,
                "tools/call",
                serde_json::json!({"name":"Write","arguments":arguments}),
            );
            let failed = reply["result"]["isError"] == true;
            say(
                serde_json::json!({"type":"item.completed","item":{"id":item,"type":"mcp_tool_call","server":"incode","tool":"Write","arguments":arguments,
                "result": if failed { serde_json::Value::Null } else { serde_json::json!({"content": reply["result"]["content"]}) },
                "error": if failed { serde_json::json!({"message": reply["result"]["content"][0]["text"]}) } else { serde_json::Value::Null },
                "status": if failed { "failed" } else { "completed" }}}),
            );
        }
        drop(to);
        let _ = server.wait();
        say(
            serde_json::json!({"type":"item.completed","item":{"id":"item_9","type":"agent_message","text":"done"}}),
        );
        say(
            serde_json::json!({"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}),
        );
        std::process::exit(0);
    }

    /// A shell script at `path` that runs this test binary's `test` with
    /// `marker` set to `value`, after `before`.
    #[cfg(unix)]
    fn child_script(path: &Path, before: &str, marker: &str, value: &Path, test: &str) {
        use std::os::unix::fs::PermissionsExt;
        let exe = std::env::current_exe().expect("test binary");
        let body = format!(
            "#!/bin/sh\n{before}\n{marker}='{}' exec '{}' --exact '{test}' --nocapture -q --test-threads=1\n",
            value.display(),
            exe.display()
        );
        std::fs::write(path, body).expect("write the script");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("runnable");
    }

    /// **A BRIDGED CODEX TURN RUNS ITS TOOL CALLS IN incode's TOOLBOX**, end
    /// to end: the backend opens the bridge and starts "codex" with the
    /// bridged argv; the stand-in reads the registration off that argv,
    /// starts the tool server with only the variables it names, and calls
    /// `Write` twice over MCP. The first lands — written by incode. The
    /// second is refused by a deny rule in incode's toolbox and nothing is
    /// written. Each call is shown once, under incode's id; codex's record of
    /// it is not shown again.
    #[cfg(unix)]
    #[test]
    fn a_bridged_turn_runs_its_calls_in_incodes_toolbox_and_a_deny_rule_refuses_one() {
        let ws = tempfile::tempdir().expect("ws");
        let bin = tempfile::tempdir().expect("bin");
        let codex = bin.path().join("codex.sh");
        let server = bin.path().join("server.sh");
        child_script(
            &codex,
            &format!(
                "printf '%s\\0' \"$@\" > '{}'",
                bin.path().join("argv").display()
            ),
            FAKE_CODEX,
            bin.path(),
            "agent::codex::tests::fake_codex_child",
        );
        child_script(
            &server,
            "",
            FAKE_TOOL_SERVER,
            Path::new("1"),
            "agent::codex::tests::tool_server_child",
        );

        let mut backend = CodexBackend::new(ws.path().to_path_buf())
            .with_binary(codex.to_string_lossy())
            .with_tools(CodexTools::Incode)
            .with_tool_server(server.clone());
        let mut rules = crate::permissions::rules::PermissionRules::none();
        rules.add(
            &crate::permissions::rules::RuleLists {
                deny: vec!["Write(secret.txt)".to_string()],
                ..Default::default()
            },
            crate::permissions::rules::Layer::User,
            None,
        );
        backend
            .tools
            .as_mut()
            .expect("bridged")
            .set_permission_rules(rules);

        let mut seen = Vec::new();
        let outcome = backend
            .run_turn("write two files", &mut |e| seen.push(e.clone()))
            .expect("the turn completes");
        assert!(!outcome.is_error, "{outcome:?}");
        assert_eq!(outcome.session_id, "fake-thread");
        assert_eq!(backend.session_id(), Some("fake-thread"));

        // The argv codex was really started with: the bridged one.
        let argv = String::from_utf8(std::fs::read(bin.path().join("argv")).expect("argv"))
            .expect("utf-8");
        assert!(argv.contains("--ignore-user-config"), "{argv:?}");
        assert!(argv.contains("features.shell_tool=false"), "{argv:?}");
        assert!(
            argv.contains(&format!(
                "mcp_servers.incode.command=\"{}\"",
                server.display()
            )),
            "{argv:?}"
        );

        // The model saw incode's tools.
        assert!(
            seen.iter().any(|e| matches!(e, AgentEvent::Text(t) if t.starts_with("tools: ") && t.contains("Write") && t.contains("Read"))),
            "{seen:?}"
        );
        // The first call ran in incode's toolbox; the second was refused there.
        assert_eq!(
            std::fs::read_to_string(ws.path().join("notes.txt")).expect("written by incode"),
            "written"
        );
        assert!(
            !ws.path().join("secret.txt").exists(),
            "the deny rule did not hold"
        );
        let uses: Vec<(&str, &str)> = seen
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolUse { id, name, .. } => Some((id.as_str(), name.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            uses,
            [("incode-1", "Write"), ("incode-2", "Write")],
            "each call once, under incode's id: {seen:?}"
        );
        let results: Vec<(&str, bool, &str)> = seen
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult {
                    id,
                    is_error,
                    output,
                } => Some((id.as_str(), *is_error, output.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2, "{seen:?}");
        assert_eq!((results[0].0, results[0].1), ("incode-1", false));
        assert_eq!((results[1].0, results[1].1), ("incode-2", true));
        assert!(
            results[1].2.contains("Write(secret.txt)"),
            "refused by the rule, by name: {}",
            results[1].2
        );
        assert!(
            seen.iter()
                .any(|e| matches!(e, AgentEvent::Text(t) if t == "done")),
            "{seen:?}"
        );
    }
}
