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

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Deserialize;

use super::{AgentBackend, AgentError, AgentEvent, TurnOutcome};

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
    ItemStarted {
        #[allow(dead_code)]
        item: WireItem,
    },
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
    #[allow(dead_code)]
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
        }
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
    fn full_prompt(&self, prompt: &str) -> String {
        let (budgeted_prompt, budgeted_context) =
            crate::agent::budget_for_spawn(prompt, &self.system_context);
        if budgeted_context.is_empty() {
            budgeted_prompt
        } else {
            format!("{budgeted_context}\n\n{budgeted_prompt}")
        }
    }

    fn command(&self, prompt: &str) -> Command {
        /// Codex's own hooks file off, on both spawn paths: incode fires the
        /// memory engine's lifecycle itself, and codex firing it too would
        /// record each session twice. Proved on 0.145.0 and 0.153.4, `exec`
        /// and `exec resume`: with a probe hooks file (trust bypassed), hooks
        /// on recorded `session-start` and `user-prompt`; `--disable hooks`
        /// recorded nothing, and codex no longer read `~/.codex/hooks.json`.
        const HOOKS_OFF: [&str; 2] = ["--disable", "hooks"];
        let mut cmd = Command::new(&self.binary);
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
}

impl AgentBackend for CodexBackend {
    fn id(&self) -> &str {
        "codex"
    }

    /// Neither half. Measured from the CLI surface: no per-tool deny flag exists,
    /// so Sentinel detection covers file writes after the fact.
    fn enforces(&self) -> crate::guards::Enforcement {
        crate::guards::Enforcement::NOTHING
    }

    fn agent_failed(&self, outcome: &TurnOutcome) -> bool {
        outcome.is_error
            || outcome.stop_reason.eq_ignore_ascii_case("ERROR")
            || crate::agent::moves_the_work(&outcome.failure())
    }

    fn set_team_identity(&mut self, role: &str) {
        self.team_identity = Some(role.to_string());
    }

    fn mcp_config(&self) -> Option<String> {
        // EVERY override, not the first. Returning one of them read as the
        // whole registration, which was true while there was a single server
        // and became a lie the moment there were two — the second was simply
        // invisible to anything asking what this agent had been handed.
        if self.mcp_overrides.is_empty() {
            return None;
        }
        Some(self.mcp_overrides.join("\n"))
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

    fn set_extra_dirs(&mut self, dirs: Vec<PathBuf>) {
        self.extra_dirs = dirs;
    }

    fn set_permission_mode(&mut self, mode: &str) {
        self.sandbox = match mode {
            "plan" | "read-only" => Some("read-only".to_string()),
            "acceptEdits" | "accept-edits" | "workspace-write" => {
                Some("workspace-write".to_string())
            }
            _ => None,
        };
    }

    fn set_cancel(&mut self, token: crate::runtime::cancel::CancelToken) {
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
        let sent = self.full_prompt(prompt);
        // Held to the end of the turn, then removed.
        let pictures =
            ImageFiles::write(&std::mem::take(&mut self.images)).map_err(AgentError::new)?;
        self.image_files.clone_from(&pictures.paths);
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

        let mut outcome: Option<TurnOutcome> = None;
        let mut usage = crate::runtime::usage::TokenUsage::default();

        for line in BufReader::new(stdout).lines() {
            if let Some(token) = &self.cancel {
                if token.is_cancelled() {
                    tree.kill(&mut child);
                    return Err(AgentError::new("the turn was cancelled"));
                }
            }
            let Ok(line) = line else { break };
            let Ok(wire) = serde_json::from_str::<Wire>(&line) else {
                continue;
            };
            match wire {
                Wire::ThreadStarted { thread_id } => {
                    self.thread_id = Some(thread_id.clone());
                    on_event(&AgentEvent::SessionStarted {
                        session_id: thread_id,
                        model: self.model.clone().unwrap_or_default(),
                    });
                }
                Wire::TurnStarted => {}
                Wire::ItemStarted { item } => match item.item_type.as_str() {
                    "command_execution" => {
                        if let Some(cmd) = item.command {
                            on_event(&AgentEvent::ToolUse {
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
                            on_event(&AgentEvent::ToolUse {
                                id: item.id,
                                name: tool,
                                input,
                            });
                        }
                    }
                    _ => {}
                },
                Wire::ItemCompleted { item } => match item.item_type.as_str() {
                    "agent_message" => {
                        if let Some(text) = item.text {
                            if !text.trim().is_empty() {
                                on_event(&AgentEvent::TextDelta(text.clone()));
                                on_event(&AgentEvent::Text(text));
                            }
                        }
                    }
                    "command_execution" => {
                        if let Some(cmd) = item.command {
                            on_event(&AgentEvent::ToolUse {
                                id: item.id.clone(),
                                name: "Bash".to_string(),
                                input: cmd,
                            });
                            on_event(&AgentEvent::ToolResult {
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
                            on_event(&AgentEvent::ToolUse {
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
                            on_event(&AgentEvent::ToolResult {
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
                        usage = to_usage(u);
                    }
                    let sid = self.thread_id.clone().unwrap_or_default();
                    outcome = Some(TurnOutcome {
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
                    outcome = Some(TurnOutcome {
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
                    outcome = Some(TurnOutcome {
                        session_id: sid,
                        stop_reason: "ERROR".to_string(),
                        is_error: true,
                        detail: Some(err_msg),
                        served_by: self.model.clone().into_iter().collect(),
                    });
                }
                Wire::Other => {}
            }
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
mod tests {
    use super::*;

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
        assert_eq!(bare.full_prompt("fix the bug"), "fix the bug");

        let briefed = CodexBackend::new(PathBuf::from("."))
            .with_system_context(vec!["Guards: no writes outside src/".to_string()]);
        let sent = briefed.full_prompt("fix the bug");
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
    }
}
