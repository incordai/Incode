//! The entries `admin.json` lists: services that serve API agents, agent
//! CLIs, models reached with a key, the search service, which PDF printer,
//! and which optional plugins are on. Each is one field's type in
//! [`super::AdminConfig`]; what they resolve to is decided there.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One service that serves API agents and lists them on request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiProvider {
    /// Unique among providers; prefixes each of its agents' catalogue ids as
    /// `<name>/<agent id>`, so they cannot collide with a local agent's.
    pub name: String,
    /// The service's base URL. The agent list is read from
    /// `{url}{agents_path}`, and turns are sent to `{url}{turns_path}`.
    pub url: String,
    /// The NAME of the environment variable holding the key — never the key.
    pub key_env: String,
    /// Where its agent list is read, under `url`. Unset is
    /// [`crate::agent::catalogue::DEFAULT_AGENTS_PATH`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_path: Option<String>,
    /// Where its agents' turns are sent, under `url`. Unset is
    /// [`crate::agent::models::DEFAULT_TURNS_PATH`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns_path: Option<String>,
    /// How long it has to answer with its agent list, in seconds. Unset is
    /// [`crate::agent::catalogue::DEFAULT_PROVIDER_TIMEOUT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_timeout_secs: Option<u64>,
}

/// One agent CLI, as admin.json `cli_agents` states it: an entry whose `id`
/// is a built-in agent's changes only the fields it sets; any other `id`
/// adds an agent — see [`crate::agent::registry::agent_table`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliAgent {
    /// What the operator types to choose it.
    pub id: String,
    /// What it is called, for a person. Unset: the built-in's, or the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The program names to look for on `PATH`, tried in order. Empty keeps
    /// the built-in's; a new agent without any is not looked for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
    /// The arguments that make it print its version. Unset: the built-in's,
    /// or `--version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_args: Option<Vec<String>>,
    /// Which headless protocol it speaks — one this harness has verified:
    /// `claude-stream-json`, `codex-json`, `agy-stream-json`, or `unverified`
    /// (found and listed, never driven). Unset: the built-in's, or
    /// `unverified`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<crate::agent::registry::Protocol>,
    /// The agent that takes the work it hands out, unless `sub_agent` names
    /// another. Unset: the built-in's, or itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_sub_agent: Option<String>,
}

/// One model reached over an API with a key.
///
/// Nothing about a vendor is built in: Anthropic, Z.ai, OpenRouter or a local
/// server are each one of these, differing only in what is written here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiModel {
    /// The catalogue id it is chosen by (`/agent <name>`). Unique, and not a
    /// local agent's id.
    pub name: String,
    /// The request shape the endpoint speaks. Unset is `anthropic`.
    #[serde(default)]
    pub format: crate::agent::api::WireFormat,
    /// The FULL endpoint: `.../v1/messages` or `.../chat/completions`.
    pub url: String,
    /// The NAME of the environment variable holding the key — never the key.
    pub key_env: String,
    /// The model asked for. Blank asks for none, for an endpoint serving one.
    #[serde(default)]
    pub model: String,
    /// The model this entry's sub-agents run on — the same service's
    /// cheaper model for delegated work. Blank, they inherit `model`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sub_model: String,
    /// The role table's names (`opus`, `sonnet`, `haiku`, …) mapped to this
    /// service's own model ids, so one role table serves every entry. A name
    /// this service is not given is not sent to it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, String>,
    /// The reply-length ceiling asked for. Unset is
    /// [`crate::agent::api::DEFAULT_MAX_TOKENS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// How many tool rounds one turn may take. Unset is
    /// [`crate::agent::api::DEFAULT_MAX_STEPS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<usize>,
    /// How long one request may take, in seconds. Unset is
    /// [`crate::agent::api::DEFAULT_TIMEOUT_SECS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// How many times a busy answer (429, 503, 529) is retried, with a
    /// bounded wait between. Unset is
    /// [`crate::agent::api::DEFAULT_MAX_RETRIES`]; 0 never retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    /// Whether the `anthropic` format marks prompt-cache breakpoints (the
    /// tools, the system prompt, the conversation so far). Unset is on; set
    /// false for a service that refuses `cache_control`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<bool>,
    /// Which header carries the key: `x-api-key` or `bearer`. Unset is the
    /// format's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<crate::agent::api::KeyHeader>,
    /// The `anthropic-version` header the messages format sends. Unset is
    /// [`crate::agent::api::DEFAULT_API_VERSION`]; the chat format sends none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    /// The model's context window in tokens. Past 80% of it the older turns
    /// are summarised into one message. Unset is
    /// [`crate::agent::compact::DEFAULT_CONTEXT_WINDOW`]; 0 never compacts
    /// on its own (`/compact` still does).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

/// Which program `RenderPdf` prints with — see `toolbox::pdf`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PdfRenderer {
    /// The headless browser (the browser tool) when its PDFs carry text
    /// (probed once per session), else the system headless browser, else the
    /// headless browser as it is.
    #[default]
    Auto,
    /// Only the system's headless Chromium-family browser (Edge, Chrome).
    System,
    /// Only the headless browser: the configured browser tool.
    Browser,
}

/// The web search service `WebSearch` asks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchProvider {
    /// The search endpoint. The query is added as `q`.
    pub url: String,
    /// The NAME of the environment variable holding its key, when it takes
    /// one — never the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
}

/// Which optional parts of the body are in use.
///
/// # Why these are opt-out rather than opt-in
///
/// The body is normally present, and a harness that ignored it by default would
/// re-ask what it has already been told — the thing this product exists to stop.
/// So each defaults to on and each can be switched off, and switching one off is
/// a decision the operator takes deliberately and can see in `/admin`.
///
/// # Why the owner channel is different from the rest
///
/// The others enrich a turn: without them the work is less informed and no less
/// correct. The owner channel decides questions a machine must not settle, and
/// turning it off does not move those decisions elsewhere — it means the runner
/// STOPS at them. That is the honest behaviour and it is not a downgrade: the
/// alternative, letting another model answer, is precisely how both earlier
/// autonomous loops failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plugins {
    /// Ask the owner (through the proxy) when the runner reaches something a
    /// machine must not settle. Off means the runner stops there instead.
    #[serde(default = "enabled")]
    pub owner: bool,
    /// Show what the code auditor has already found in this project.
    #[serde(default = "enabled")]
    pub auditor: bool,
    /// Show what this project has already been observed to solve.
    #[serde(default = "enabled")]
    pub recorder: bool,
    /// Show the owner's standing guard categories before the agent trips one.
    #[serde(default = "enabled")]
    pub guards: bool,
    /// Load the bundled expert skills.
    #[serde(default = "enabled")]
    pub skills: bool,
    /// Connect the configured MCP servers.
    #[serde(default = "enabled")]
    pub mcp: bool,
    /// Individual skills to leave out, by name.
    ///
    /// A removal list rather than a replacement list: an operator who wrote out
    /// the ones they wanted would silently lose every skill added afterwards,
    /// and would not find out until they needed one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_skills: Vec<String>,
    /// Individual MCP servers to leave out, by name. Same reasoning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_mcp: Vec<String>,
}

const fn enabled() -> bool {
    true
}

impl Default for Plugins {
    fn default() -> Self {
        Self {
            owner: true,
            auditor: true,
            recorder: true,
            guards: true,
            skills: true,
            mcp: true,
            disabled_skills: Vec::new(),
            disabled_mcp: Vec::new(),
        }
    }
}

impl Plugins {
    /// The ones switched off, for the report.
    #[must_use]
    pub fn disabled(&self) -> Vec<&'static str> {
        let mut off = Vec::new();
        if !self.owner {
            off.push("owner");
        }
        if !self.auditor {
            off.push("auditor");
        }
        if !self.recorder {
            off.push("recorder");
        }
        if !self.guards {
            off.push("guards");
        }
        if !self.skills {
            off.push("skills");
        }
        if !self.mcp {
            off.push("mcp");
        }
        off
    }

    /// Whether one named skill is loaded.
    #[must_use]
    pub fn skill_enabled(&self, name: &str) -> bool {
        self.skills
            && !self
                .disabled_skills
                .iter()
                .any(|d| d.eq_ignore_ascii_case(name))
    }

    /// Whether one named MCP server is connected.
    #[must_use]
    pub fn mcp_enabled(&self, name: &str) -> bool {
        self.mcp
            && !self
                .disabled_mcp
                .iter()
                .any(|d| d.eq_ignore_ascii_case(name))
    }
}
