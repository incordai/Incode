//! Hook system — run shell commands before/after tool execution.

use std::process::Command;

// ---------------------------------------------------------------------------
// HookConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HookConfig {
    pub pre_tool_use: Vec<String>,
    pub post_tool_use: Vec<String>,
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

    let output = child.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let code = output.status.code().unwrap_or(-1);
    Ok((code, stdout))
}
