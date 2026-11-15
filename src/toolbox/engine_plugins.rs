//! The plugin tool servers the memory engine runs for the owner, offered to
//! every agent incode drives as two tools: [`PLUGIN_TOOLS`] (what one server
//! offers) and [`PLUGIN_CALL`] (run one of its tools).
//!
//! # Why an index and not every tool
//!
//! The owner's decision for the engine (its `plugin_tools` module, 2026-10-29):
//! "do not send all tools, send an index of everything with categories; if
//! the AI needs one it checks the index and calls it" — measured there, every
//! plugin's tools in the context cost 8,029 tokens more. So the model is told
//! one line per server, in [`PLUGIN_TOOLS`]'s description, and asks for a
//! server's tools only when it needs one.
//!
//! # The engine's calls
//!
//! - `POST /v1/memory/tools/incord_describe {}` — the index, once per session
//!   ([`fetch_index`], bounded at [`INDEX_TIMEOUT`]): `plugin_tools`, each
//!   `{"name": "plugin:<server>", "category", "description"}`.
//! - `incord_describe {"name": "plugin:<server>"}` — that server's tools,
//!   named `<server>.<tool>` ([`describe`]).
//! - `POST /v1/memory/tools/incord_call {"name": "<server>.<tool>",
//!   "arguments"}` — the tool's own MCP result ([`call`]).
//!
//! A refusal is a 400 whose body is `{"error": "…"}`. When the server the
//! engine reached answered 401, the call is BLOCKED ON SIGN-IN, and said so
//! ([`explain`]): the engine reads a plugin's keys only from its own
//! environment and does no OAuth login, so retrying cannot help.
//!
//! # Through incode's gate
//!
//! Both are [`super::Reach::Run`], like every MCP tool: refused in a read-only
//! session, asked about in a prompting one, refused when the memory engine
//! cannot decide. What a server's tool touches cannot be known from here.
//!
//! # Wired to
//!
//! - The index is fetched where a session's servers are gathered —
//!   `cli::connect_ext_tools` (REPL) and `task_run::connect_ext_tools` (`-t`)
//!   — and kept in `SessionTools`, so every toolbox of the session, a
//!   sub-agent's included, offers the same two tools.
//! - A server a local plugin already runs (`plugins::local_server_keys`,
//!   started by `plugins::session_servers`) is left out of the index, so no
//!   server is started twice.
//! - Sibling: [`super::client_servers`] runs MCP servers incode starts
//!   itself, each tool offered by name.

use std::time::Duration;

use serde_json::{json, Value};

use super::image::ToolOutput;

/// What one server offers.
pub const PLUGIN_TOOLS: &str = "PluginTools";

/// Run one of a server's tools.
pub const PLUGIN_CALL: &str = "PluginCall";

const DESCRIBE_PATH: &str = "/v1/memory/tools/incord_describe";
const CALL_PATH: &str = "/v1/memory/tools/incord_call";

/// What names a server in the engine's index: `plugin:github`.
const DESCRIBE_PREFIX: &str = "plugin:";

/// Between a server and its tool: `github.create_issue`.
const SEPARATOR: char = '.';

/// How long a session's start waits for the index: measured at 0.85 s.
pub(crate) const INDEX_TIMEOUT: Duration = Duration::from_secs(4);

/// How long describing a server may take: the engine may start its program.
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long one call may take, as for an MCP server incode runs itself.
const CALL_TIMEOUT: Duration = super::client_servers::CALL_BOUND;

/// The most servers named in the index; the rest are counted.
const MAX_INDEXED: usize = 32;

/// The longest a server's description runs in the index.
const INDEX_DESCRIPTION: usize = 160;

/// One server in the engine's index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    pub category: String,
    pub description: String,
}

/// The servers the engine runs for this session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Index {
    pub servers: Vec<Server>,
}

impl Index {
    /// Whether `server` is one of these.
    #[must_use]
    pub fn has(&self, server: &str) -> bool {
        self.servers.iter().any(|s| s.name == server)
    }

    /// [`PLUGIN_TOOLS`]'s description: what it does, then a line per server.
    #[must_use]
    pub fn description(&self) -> String {
        let mut text = format!(
            "{} The plugin tool servers the memory engine runs:",
            describe_definition_text()
        );
        for server in self.servers.iter().take(MAX_INDEXED) {
            text.push_str(&format!(
                "\n- {} [{}]: {}",
                server.name,
                server.category,
                crate::text::truncate(&server.description, INDEX_DESCRIPTION)
            ));
        }
        if self.servers.len() > MAX_INDEXED {
            text.push_str(&format!(
                "\n- and {} more, not named here",
                self.servers.len() - MAX_INDEXED
            ));
        }
        text
    }
}

fn describe_definition_text() -> &'static str {
    "List the tools of one plugin tool server the memory engine runs: each \
     tool's name (`<server>.<tool>`), what it does and its input schema. Call \
     this before PluginCall."
}

/// The two tools as the toolbox's catalogue holds them; [`PLUGIN_TOOLS`]'s
/// description is replaced by [`Index::description`] where it is offered.
#[must_use]
pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": PLUGIN_TOOLS,
            "description": describe_definition_text(),
            "input_schema": {
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "A server named in this tool's description."}
                },
                "required": ["server"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": PLUGIN_CALL,
            "description": "Run one tool of a plugin tool server the memory engine runs, by the \
                            name PluginTools gave it (`<server>.<tool>`), with the arguments its \
                            input schema asks for. Returns the tool's own answer.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "tool": {"type": "string", "description": "`<server>.<tool>`, as PluginTools lists it."},
                    "arguments": {"type": "object", "description": "The tool's arguments."}
                },
                "required": ["tool"],
                "additionalProperties": false
            }
        }),
    ]
}

/// The engine's index from its answer, leaving out each server in `local`
/// (already run by a local plugin) and any name that is not a plain word.
///
/// # Errors
///
/// The answer is not the index's shape.
pub fn parse_index(answer: &str, local: &[String]) -> Result<Index, String> {
    let value: Value =
        serde_json::from_str(answer).map_err(|e| format!("the index did not parse: {e}"))?;
    let listed = value
        .get("plugin_tools")
        .and_then(Value::as_array)
        .ok_or("the engine's answer has no `plugin_tools`")?;
    let text = |item: &Value, key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let mut index = Index::default();
    for item in listed {
        let Some(name) = text(item, "name")
            .strip_prefix(DESCRIBE_PREFIX)
            .map(str::to_string)
        else {
            continue;
        };
        let plain = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !plain || local.contains(&name) || index.has(&name) {
            continue;
        }
        index.servers.push(Server {
            name,
            category: text(item, "category"),
            description: text(item, "description"),
        });
    }
    Ok(index)
}

/// The engine's index for this session, without the servers in `local`.
/// `None` when the engine has no plugin servers to offer or did not answer
/// within [`INDEX_TIMEOUT`] — traced, never fatal.
#[must_use]
pub fn fetch_index(local: &[String]) -> Option<Index> {
    let answered = post(DESCRIBE_PATH, &json!({}), INDEX_TIMEOUT)
        .and_then(|answer| parse_index(&answer, local));
    match answered {
        Ok(index) if index.servers.is_empty() => None,
        Ok(index) => {
            crate::trace!(
                "tool",
                "plugin tools: {} server(s) from the memory engine",
                index.servers.len()
            );
            Some(index)
        }
        Err(why) => {
            crate::trace!("tool", "plugin tools: none from the memory engine: {why}");
            None
        }
    }
}

/// [`PLUGIN_TOOLS`]: the tools of the server `input` names.
///
/// # Errors
///
/// The server is not in the index, or the engine could not reach it.
pub fn describe(index: &Index, input: &Value) -> Result<String, String> {
    let server = input
        .get("server")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(|s| s.strip_prefix(DESCRIBE_PREFIX).unwrap_or(s))
        .unwrap_or_default();
    known(index, server)?;
    let answer = post(
        DESCRIBE_PATH,
        &json!({"name": format!("{DESCRIBE_PREFIX}{server}")}),
        DESCRIBE_TIMEOUT,
    )
    .map_err(|why| explain(server, &why))?;
    let value: Value = serde_json::from_str(&answer)
        .map_err(|e| format!("the engine's answer did not parse: {e}"))?;
    let tools = value
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| explain(server, &answer))?;
    let mut lines = vec![format!(
        "{} tool(s) of `{server}` — run one with PluginCall {{\"tool\": \"<name>\", \
         \"arguments\": {{…}}}}:",
        tools.len()
    )];
    for tool in tools {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("?");
        let said = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let schema = tool.get("inputSchema").cloned().unwrap_or(json!({}));
        lines.push(format!("\n## {name}\n{said}\ninput schema: {schema}"));
    }
    Ok(lines.concat())
}

/// [`PLUGIN_CALL`]: run the tool `input` names, with its arguments. A tool's
/// own refusal (`isError`) is an `Err` with its words.
///
/// # Errors
///
/// The tool is not of a server in the index, the engine could not reach the
/// server, or the tool refused.
pub fn call(index: &Index, input: &Value) -> Result<ToolOutput, String> {
    let tool = input
        .get("tool")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let Some((server, name)) = tool.split_once(SEPARATOR) else {
        return Err(format!(
            "`{tool}` is not `<server>.<tool>`: use the name PluginTools gave it"
        ));
    };
    known(index, server)?;
    if name.is_empty() {
        return Err(format!("`{tool}` names no tool of `{server}`"));
    }
    let arguments = input.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let answer = post(
        CALL_PATH,
        &json!({"name": tool, "arguments": arguments}),
        CALL_TIMEOUT,
    )
    .map_err(|why| explain(server, &why))?;
    output_of(server, &answer)
}

/// A call's answer as the model is given it: the MCP result's content, or
/// the text itself when it is not one.
fn output_of(server: &str, answer: &str) -> Result<ToolOutput, String> {
    let Ok(value) = serde_json::from_str::<Value>(answer) else {
        return Ok(ToolOutput::from(answer.to_string()));
    };
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(explain(server, error));
    }
    let Some(blocks) = value.get("content").and_then(Value::as_array) else {
        return Ok(ToolOutput::from(answer.to_string()));
    };
    let output = ToolOutput::from_mcp_content(blocks);
    if value.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(output.text);
    }
    Ok(output)
}

/// `server` is one the index names, or why not.
fn known(index: &Index, server: &str) -> Result<(), String> {
    if index.has(server) {
        return Ok(());
    }
    let names: Vec<&str> = index.servers.iter().map(|s| s.name.as_str()).collect();
    Err(format!(
        "there is no plugin tool server `{server}`; the memory engine runs: {}",
        names.join(", ")
    ))
}

/// What a refusal from the engine about `server` means. A 401 from the
/// server it reached is not the call's fault and cannot be retried away.
#[must_use]
pub fn explain(server: &str, why: &str) -> String {
    if why.contains("answered 401") || why.contains("Unauthorized") {
        return format!(
            "BLOCKED ON SIGN-IN: the memory engine could not sign in to the plugin tool server \
             `{server}`, so none of its tools can run through it — retrying will not help. The \
             engine reads a plugin's keys only from its own environment and does no OAuth login \
             yet; until that changes there, tell the person `{server}` needs signing in. (What \
             the engine said: {why})"
        );
    }
    format!("the memory engine could not run `{server}`: {why}")
}

/// One POST to the engine, a refusal carrying what it said.
fn post(path: &str, body: &Value, timeout: Duration) -> Result<String, String> {
    let url = format!("{}{path}", crate::brief::memory_url().trim_end_matches('/'));
    let body = body.to_string();
    crate::runtime::http::request_text_explained(
        move |client| {
            client
                .post(&url)
                .header("content-type", "application/json")
                .body(body)
                .timeout(timeout)
        },
        "memory engine",
    )
}

#[cfg(test)]
#[path = "engine_plugins_tests.rs"]
mod tests;
