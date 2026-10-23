//! What `codex exec --json` prints, and what each line of it means — the same
//! reading on both routes.
//!
//! Wired to: [`super::CodexBackend`]'s `run_turn` on codex's own tools, and
//! its bridged turn (`bridged.rs`), each reading the child's stdout a line
//! at a time through [`Reading::line`].

use serde::Deserialize;

use crate::agent::{AgentEvent, TurnOutcome};

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

impl WireItem {
    /// The call's input as the model wrote it: its arguments as JSON, or
    /// nothing.
    ///
    /// Called by [`Reading::item_started`] and [`Reading::mcp_call_completed`].
    fn input(&self) -> String {
        self.arguments
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_default()
    }

    /// What a finished MCP call answered: the text of its result, or — when
    /// that is empty — its error, unpacked.
    ///
    /// Called by [`Reading::mcp_call_completed`].
    fn mcp_output(&self) -> String {
        let mut output = extract_mcp_output(&self.result);
        if output.is_empty() {
            if let Some(err) = &self.error {
                output = err
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| parse_error_detail(&err.to_string()));
            }
        }
        output
    }

    /// Whether a finished MCP call failed: it carries an error, or says so.
    ///
    /// Called by [`Reading::mcp_call_completed`].
    fn mcp_failed(&self) -> bool {
        self.error.is_some() || self.status.as_deref() == Some("failed")
    }
}

/// A shell command codex ran, as the use it stands for.
///
/// Called by [`Reading::item_started`] and [`Reading::item_completed`].
fn bash_use(id: String, command: String) -> AgentEvent {
    AgentEvent::ToolUse {
        id,
        name: "Bash".to_string(),
        input: command,
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// What one turn of `codex exec --json` has said so far, and what each line
/// means — the same reading on both routes.
pub(super) struct Reading<'a> {
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
    /// Called by the backend's `run_turn` and its bridged turn, once per turn.
    pub(super) fn new(
        thread_id: &'a mut Option<String>,
        model: Option<String>,
        bridged: bool,
    ) -> Self {
        Self {
            thread_id,
            model,
            outcome: None,
            usage: crate::runtime::usage::TokenUsage::default(),
            bridged,
        }
    }

    /// The turn's result, if it reported one, and what it cost.
    pub(super) fn finish(self) -> (Option<TurnOutcome>, crate::runtime::usage::TokenUsage) {
        (self.outcome, self.usage)
    }

    /// Whether an item is codex's own echo of a call the bridge already showed.
    fn shown_by_the_bridge(&self, item: &WireItem) -> bool {
        self.bridged && item.server.as_deref() == Some(crate::tool_bridge::CLIENT_KEY)
    }

    /// The events one line stands for. A line that is not one of codex's is
    /// nothing.
    pub(super) fn line(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(wire) = serde_json::from_str::<Wire>(line) else {
            return Vec::new();
        };
        match wire {
            Wire::ThreadStarted { thread_id } => vec![self.thread_started(thread_id)],
            Wire::ItemStarted { item } => self.item_started(item),
            Wire::ItemCompleted { item } => self.item_completed(item),
            Wire::TurnCompleted { usage } => {
                self.turn_completed(usage.as_ref());
                Vec::new()
            }
            Wire::TurnFailed { error } => {
                let detail = error
                    .map(|e| parse_error_detail(&e.message))
                    .unwrap_or_else(|| "turn failed".to_string());
                self.failed(detail);
                Vec::new()
            }
            Wire::Error { message } => {
                let detail = message
                    .map(|m| parse_error_detail(&m))
                    .unwrap_or_else(|| "codex error".to_string());
                self.failed(detail);
                Vec::new()
            }
            Wire::TurnStarted | Wire::Other => Vec::new(),
        }
    }

    /// The thread is codex's session: held as the backend's, and announced.
    ///
    /// Called by [`Self::line`].
    fn thread_started(&mut self, thread_id: String) -> AgentEvent {
        *self.thread_id = Some(thread_id.clone());
        AgentEvent::SessionStarted {
            session_id: thread_id,
            model: self.model.clone().unwrap_or_default(),
        }
    }

    /// A call starting: its use, for a shell command or an MCP call that
    /// names its tool.
    ///
    /// Called by [`Self::line`].
    fn item_started(&self, item: WireItem) -> Vec<AgentEvent> {
        if self.shown_by_the_bridge(&item) {
            return Vec::new();
        }
        match item.item_type.as_str() {
            "command_execution" => item
                .command
                .map(|cmd| bash_use(item.id, cmd))
                .into_iter()
                .collect(),
            "mcp_tool_call" => {
                let input = item.input();
                item.tool
                    .map(|tool| AgentEvent::ToolUse {
                        id: item.id,
                        name: tool,
                        input,
                    })
                    .into_iter()
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// An item finished: the reply's text, or a call's use and its result.
    ///
    /// Called by [`Self::line`].
    fn item_completed(&self, item: WireItem) -> Vec<AgentEvent> {
        if self.shown_by_the_bridge(&item) {
            return Vec::new();
        }
        match item.item_type.as_str() {
            "agent_message" => match item.text {
                Some(text) if !text.trim().is_empty() => {
                    vec![AgentEvent::TextDelta(text.clone()), AgentEvent::Text(text)]
                }
                _ => Vec::new(),
            },
            "command_execution" => match item.command {
                Some(cmd) => vec![
                    bash_use(item.id.clone(), cmd),
                    AgentEvent::ToolResult {
                        id: item.id,
                        output: item.aggregated_output.unwrap_or_default(),
                        is_error: item.exit_code.unwrap_or(0) != 0,
                    },
                ],
                None => Vec::new(),
            },
            "mcp_tool_call" => Self::mcp_call_completed(item),
            "error" => {
                if let Some(msg) = item.text {
                    crate::trace!("agent", "codex item error: {msg}");
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// A finished MCP call that names its tool: its use, then its result.
    ///
    /// Called by [`Self::item_completed`].
    fn mcp_call_completed(item: WireItem) -> Vec<AgentEvent> {
        let input = item.input();
        let output = item.mcp_output();
        let is_error = item.mcp_failed();
        let Some(tool) = item.tool else {
            return Vec::new();
        };
        vec![
            AgentEvent::ToolUse {
                id: item.id.clone(),
                name: tool,
                input,
            },
            AgentEvent::ToolResult {
                id: item.id,
                output,
                is_error,
            },
        ]
    }

    /// The turn succeeded, at the cost it reported (if it reported one).
    ///
    /// Called by [`Self::line`].
    fn turn_completed(&mut self, usage: Option<&WireUsage>) {
        if let Some(u) = usage {
            self.usage = to_usage(u);
        }
        self.outcome = Some(self.outcome_of("SUCCESS", false, None));
    }

    /// The turn failed, for the reason given.
    ///
    /// Called by [`Self::line`], for `turn.failed` and `error`.
    fn failed(&mut self, detail: String) {
        self.outcome = Some(self.outcome_of("ERROR", true, Some(detail)));
    }

    /// The turn's result on the thread it ran in, served by the model asked
    /// for.
    ///
    /// Called by [`Self::turn_completed`] and [`Self::failed`].
    fn outcome_of(&self, stop_reason: &str, is_error: bool, detail: Option<String>) -> TurnOutcome {
        TurnOutcome {
            session_id: self.thread_id.clone().unwrap_or_default(),
            stop_reason: stop_reason.to_string(),
            is_error,
            detail,
            served_by: self.model.clone().into_iter().collect(),
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

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
