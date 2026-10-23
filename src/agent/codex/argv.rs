//! The command each turn starts codex with, on either route: the flags, the
//! `-c` overrides that make codex a model and nothing else on the bridged
//! route, and the registration of incode's tool server.
//!
//! Wired to: [`super::CodexBackend`]'s `run_turn` (both routes) and its
//! `mcp_config`, which reports [`bridge_registration`].

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::CodexBackend;

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
///   runs (see the notes on [`crate::agent::codex`]) — and nothing waits on
///   a person.
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
///
/// Called by [`CodexBackend::bridged_command`] and by the backend's
/// `mcp_config`.
pub(super) fn bridge_registration(exe: &Path) -> Vec<String> {
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

impl CodexBackend {
    /// The binary codex is told to start as the tool server.
    ///
    /// Called by [`Self::command`] and by the backend's `mcp_config`.
    pub(super) fn server_exe(&self) -> PathBuf {
        self.tool_server
            .clone()
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| PathBuf::from("incode"))
    }

    /// The command for one turn, on whichever route this backend is on.
    ///
    /// Called by the backend's `run_turn` on codex's own tools.
    pub(super) fn command(&self, prompt: &str) -> Command {
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
    ///
    /// Called by [`Self::command`] and, on the bridged route, by the
    /// backend's `run_turn`, which names the binary the turn's channel opened.
    pub(super) fn bridged_command(&self, prompt: &str, exe: &Path) -> Command {
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
