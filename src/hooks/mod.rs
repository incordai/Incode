//! Run an operator's own shell commands around a tool call.
//!
//! # Where these actually run
//!
//! On the DESKTOP MCP server, which is the only tool dispatcher this crate
//! still owns (`desktop::mcp::call_tool`). The agent runs file and shell tools
//! in its own process, so there is no before-and-after for this crate to sit
//! in there — which is why this module went unwired long enough to become one
//! of the six dead capabilities in [`crate::wiring`].
//!
//! That leaves the capability where it is worth the most. A desktop hook sees
//! every click and keystroke before it reaches a machine the operator is
//! signed into, and can stop it.
//!
//! # The contract with the operator's command
//!
//! - `HOOK_TOOL_NAME` and `HOOK_TOOL_INPUT` are set in its environment.
//! - **Exit code 2 denies the call.** Any other code allows it.
//! - Anything on stdout is passed back to the agent as a message.
//!
//! # `HOOK_TOOL_INPUT` is the REAL input, not the redacted description
//!
//! `desktop::mcp::Server::describe` exists so a typed password becomes
//! "8 characters" in the journal and in the browser stream. A hook is
//! deliberately outside that boundary: it is a command the operator wrote,
//! running on the operator's machine, and inspecting what is about to be typed
//! is the entire reason a pre-tool hook exists. A hook that cannot see the
//! input cannot refuse on it.
//!
//! The redaction still applies to everything else. What a hook does with what
//! it is shown is the operator's decision, made when they wrote the command.

use std::process::Command;
use std::time::Duration;

/// How long a hook may take before it is killed and treated as allowing.
///
/// A hook runs between the agent asking and the desktop moving, so the whole
/// session waits on it. Without a bound, one command that waits on input
/// hangs the desktop with no way to tell from the outside whether the agent
/// or the hook is stuck.
///
/// Ten seconds is long for a guard command and short enough that a hung one is
/// noticed rather than lived with.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// HookConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct HookConfig {
    /// Commands run BEFORE the tool. Exit code 2 refuses the call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_tool_use: Vec<String>,
    /// Commands run AFTER the tool, whatever it returned.
    ///
    /// These cannot refuse anything — the action has already happened, and a
    /// deny here would report a refusal for something that was done.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_tool_use: Vec<String>,
}

impl HookConfig {
    /// Whether there is anything to run at all.
    ///
    /// Checked before a call so the ordinary case — no hooks configured —
    /// costs nothing beyond this.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty() && self.post_tool_use.is_empty()
    }
}

// ---------------------------------------------------------------------------
// HookRunResult
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRunResult {
    denied: bool,
    messages: Vec<String>,
}

impl HookRunResult {
    #[must_use]
    pub fn allow(messages: Vec<String>) -> Self {
        Self {
            denied: false,
            messages,
        }
    }

    #[must_use]
    pub fn is_denied(&self) -> bool {
        self.denied
    }

    #[must_use]
    pub fn messages(&self) -> &[String] {
        &self.messages
    }
}

// ---------------------------------------------------------------------------
// HookRunner
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HookRunner {
    config: HookConfig,
}

impl HookRunner {
    #[must_use]
    pub fn new(config: HookConfig) -> Self {
        Self { config }
    }

    #[must_use]
    pub fn run_pre_tool_use(&self, tool_name: &str, tool_input: &str) -> HookRunResult {
        self.run_commands(&self.config.pre_tool_use, tool_name, tool_input)
    }

    #[must_use]
    pub fn run_post_tool_use(
        &self,
        tool_name: &str,
        tool_input: &str,
        _tool_output: &str,
        _is_error: bool,
    ) -> HookRunResult {
        self.run_commands(&self.config.post_tool_use, tool_name, tool_input)
    }

    fn run_commands(
        &self,
        commands: &[String],
        tool_name: &str,
        tool_input: &str,
    ) -> HookRunResult {
        if commands.is_empty() {
            return HookRunResult::allow(Vec::new());
        }

        let mut messages = Vec::new();

        for command in commands {
            match run_shell_command(command, tool_name, tool_input) {
                Ok((code, stdout)) => {
                    if !stdout.is_empty() {
                        messages.push(stdout);
                    }
                    if code == 2 {
                        // Exit code 2 = deny
                        return HookRunResult {
                            denied: true,
                            messages,
                        };
                    }
                }
                Err(e) => {
                    messages.push(format!("Hook `{command}` failed: {e}"));
                }
            }
        }

        HookRunResult::allow(messages)
    }
}

fn run_shell_command(
    command: &str,
    tool_name: &str,
    tool_input: &str,
) -> std::io::Result<(i32, String)> {
    #[cfg(windows)]
    let mut child = {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    };

    #[cfg(not(windows))]
    let mut child = {
        let mut c = Command::new("sh");
        c.arg("-lc").arg(command);
        c
    };

    child
        .env("HOOK_TOOL_NAME", tool_name)
        .env("HOOK_TOOL_INPUT", tool_input)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // BOUNDED, using the same waiter `verify` uses for a project's check — one
    // timeout implementation in the crate. This used to be `child.output()`,
    // which waits forever: a hook that blocks on input would have hung the
    // desktop between the agent asking and anything happening, with nothing
    // on either side able to say which of them was stuck.
    crate::agent::process_tree::prepare(&mut child);
    let spawned = child.spawn()?;
    let output = crate::tools::bash::wait_with_timeout(spawned, HOOK_TIMEOUT)
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let code = output.status.code().unwrap_or(-1);
    Ok((code, stdout))
}
