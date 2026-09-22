//! Claude Code agent backend, driven as a subprocess.
//!
//! There is no Rust binding for the Claude Agent SDK (Python and TypeScript
//! only); the documented path for any other language is to run the CLI with
//! `-p` and a structured `--output-format`. The wire shapes below were taken
//! from real runs of CLI 2.1.229, not from documentation.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Deserialize;

use super::{AgentBackend, AgentError, AgentEvent, TurnOutcome};

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// One line of `--output-format stream-json`.
///
/// Tagged on `type`, which every line carries — but field ORDER varies (the
/// final `result` line puts `type` near the end), so this must be a tagged
/// enum rather than anything positional.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CliEvent {
    #[serde(rename = "system")]
    System(SystemEvent),
    #[serde(rename = "assistant")]
    Assistant(AssistantMsg),
    /// Tool results come back addressed to the model, so the transport labels
    /// them "user". They are the tools talking, not the operator.
    #[serde(rename = "user")]
    User(AssistantMsg),
    #[serde(rename = "stream_event")]
    StreamEvent(StreamEvent),
    #[serde(rename = "rate_limit_event")]
    RateLimit(RateLimitEvent),
    #[serde(rename = "result")]
    Result(Box<ResultEvent>),
    #[serde(other)]
    Other,
}

/// A partial-message frame, present only with `--include-partial-messages`.
///
/// Shape captured from a real run rather than assumed:
/// `{"type":"stream_event","event":{"type":"content_block_delta","index":0,
///   "delta":{"type":"text_delta","text":"h"}},"parent_tool_use_id":null}`
#[derive(Debug, Deserialize)]
struct StreamEvent {
    event: StreamInner,
    #[serde(default)]
    parent_tool_use_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamInner {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    delta: Option<StreamDelta>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    #[serde(rename = "type", default)]
    delta_type: String,
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize)]
struct SystemEvent {
    subtype: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AssistantMsg {
    message: InnerMessage,
    /// `None` for the main conversation; `Some(id)` for a subagent's output.
    #[serde(default)]
    parent_tool_use_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InnerMessage {
    #[serde(default)]
    content: Vec<WireContentBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum WireContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        /// A string on some results, a list of blocks on others.
        #[serde(default)]
        content: serde_json::Value,
        #[serde(default)]
        is_error: bool,
    },
    #[serde(other)]
    Other,
}

/// Subscription quota signal. Not on the published headless docs page, but
/// emitted on every run — this is the figure a subscription-backed host should
/// surface, since no per-token money is being spent.
#[derive(Debug, Deserialize)]
struct RateLimitEvent {
    rate_limit_info: RateLimitInfo,
}

#[derive(Debug, Deserialize)]
struct RateLimitInfo {
    status: String,
    #[serde(rename = "resetsAt", default)]
    resets_at: i64,
    #[serde(rename = "rateLimitType", default)]
    rate_limit_type: String,
    #[serde(rename = "isUsingOverage", default)]
    is_using_overage: bool,
}

#[derive(Debug, Deserialize)]
struct ResultEvent {
    is_error: bool,
    session_id: String,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: WireUsage,
    /// Which models were actually BILLED for this turn, keyed by model id.
    ///
    /// The only trustworthy answer to "what ran?". The session's start event
    /// reports the model that was *requested* — measured: asking for a model
    /// that does not exist echoes that name back while the fallback quietly
    /// answers. Attribution built on the request would credit work to a model
    /// that never saw it.
    #[serde(default, rename = "modelUsage")]
    model_usage: std::collections::BTreeMap<String, serde_json::Value>,
    /// What the agent says about how the turn ended.
    ///
    /// On success this repeats the streamed reply and is dropped. On FAILURE it
    /// is the only place the reason is written down — and it was not read at
    /// all, so every failed turn reported the bare word `stop_sequence` and
    /// nothing else. A whole day went into guessing a value the program was
    /// receiving and discarding.
    #[serde(default)]
    result: Option<String>,
    /// The agent's own category for the ending, e.g. `success`, `error_*`.
    #[serde(default)]
    subtype: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Drives Claude Code over its non-interactive interface.
pub struct ClaudeCodeBackend {
    binary: String,
    workspace: PathBuf,
    /// Set after the first turn; every later turn resumes it.
    session_id: Option<String>,
    /// Which model serves the next turn, if the caller has an opinion.
    ///
    /// **This is what makes role routing work on a subscription.** Verified
    /// against the installed CLI: `--model haiku` really is served by the haiku
    /// model, confirmed by both the session's own `init.model` and the usage
    /// recorded on the result. So cheap work can run cheap without an API key.
    model: Option<String>,
    /// Models to try when the first is overloaded, in order.
    ///
    /// The CLI takes these as one comma-separated list, which is exactly the
    /// ordered candidate chain the role table already produces.
    fallbacks: Vec<String>,
    /// What the harness knows and the agent does not, carried on every turn.
    ///
    /// APPENDED to the agent's own system prompt rather than replacing it: the
    /// agent's instructions are what make it good at the work, and this is the
    /// project's context riding alongside them — the guard rules in force, what
    /// the auditor already found, what the recorder already solved, which roles
    /// exist. Replacing would trade a working coder for a briefed one.
    system_context: Vec<String>,
    /// Directories beyond the workspace this agent may reach.
    extra_dirs: Vec<std::path::PathBuf>,
    /// The policy the deny rules were built from.
    ///
    /// Held so that CHANGING THE ROOTS REBUILDS THE RULES automatically. A
    /// separate "now re-derive them" call is a call someone forgets — and this
    /// one was: reach was widened to a second root while the rules stayed
    /// anchored to the first, and the agent wrote into a protected directory
    /// there unstopped.
    guards: crate::guards::Guards,
    /// How much the agent may do on its own, if the caller has an opinion.
    permission_mode: Option<String>,
    /// Set when the user asks to stop.
    ///
    /// The agent runs as a CHILD PROCESS, so stopping it means killing that
    /// process — there is no cooperative checkpoint to fall back on. Without
    /// this, Ctrl-C during a turn would leave the agent running, still writing
    /// files, with nothing reading its output.
    cancel: Option<crate::runtime::cancel::CancelToken>,
    /// Whether to ask for text as it is generated, not only in whole blocks.
    ///
    /// Off by default: a caller collecting a REPORT wants whole blocks, and
    /// turning this on would make it choose between two copies of the same
    /// text. An interactive caller turns it on and reads the deltas.
    partial_messages: bool,
    /// Paths this agent must not write, in the CLI's own deny syntax.
    ///
    /// **This is prevention, where the sentinel is only detection.** The agent
    /// enforces these itself, so the write never happens — see
    /// [`crate::guards::deny_rules`] for why the rules say `Edit` and not
    /// `Write`, and for what was measured.
    deny: Vec<String>,
    /// An MCP server this agent should be handed, as `--mcp-config` JSON.
    ///
    /// Measured 2026-09-16: this backend passed NO MCP flag at all, so the
    /// memory server reached Claude Code only through the operator's own global
    /// configuration — the harness's reach depended on machine state it does
    /// not control and cannot verify. See `docs/COMPUTER_USE.md` section 2.
    ///
    /// **`--strict-mcp-config` is deliberately NOT passed alongside it.** The
    /// installed CLI 2.1.273 documents that flag as "only use MCP servers from
    /// --mcp-config, ignoring all other MCP configurations" — which would
    /// silently cut the agent off from the memory server it is expected to use.
    /// Adding a capability must not remove one.
    desktop_mcp: Option<String>,
    /// Who this agent speaks as on the team channel, for the task in hand.
    team_identity: Option<String>,
}

/// What THIS agent calls its sub-agent tool.
///
/// One backend's name for one capability. [`crate::guards::Restrictions`] says
/// only that sub-agents are refused; the word for it lives here, because the
/// next backend will have a different one — or none.
///
/// Measured: a turn given this on `--disallowedTools` reports the tool ABSENT
/// rather than merely refused, so the child is never offered a capability it
/// cannot use.
const SUBAGENT_TOOL: &str = "Task";

/// The harness's restrictions, in this agent's own deny syntax.
///
/// # `Edit`, never `Write` — measured, not assumed
///
/// The tool name is `Edit`. A rule written as `Write(tests/**)` is **silently
/// ineffective**: it parses, it is accepted, and the write goes through. That
/// was measured — a probe denying only `Write(tests/**)` created the file
/// anyway, and this CLI reports that only `Edit(path)` rules are matched by
/// file permission checks, `Edit` covering every file-editing tool.
///
/// A guard that looks right and does nothing is worse than no guard, so the
/// choice is pinned by a test rather than left to whoever edits next.
///
/// Measured to cover shell redirection too: with `Edit(tests/**)` in force a
/// `printf > tests/x` was denied, and the identical command without it wrote
/// the file.
fn render_denials(restrictions: &crate::guards::Restrictions) -> Vec<String> {
    let mut rules = Vec::new();
    if restrictions.no_subagents {
        rules.push(SUBAGENT_TOOL.to_string());
    }
    rules.extend(
        restrictions
            .no_write
            .iter()
            .map(|glob| format!("Edit({glob})")),
    );
    rules
}

impl ClaudeCodeBackend {
    #[must_use]
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            binary: "claude".to_string(),
            workspace,
            session_id: None,
            // Off unless a caller asks. Handing every run a desktop it did not
            // request would widen what an agent can reach as a side effect of
            // a constructor.
            desktop_mcp: None,
            team_identity: None,
            model: None,
            fallbacks: Vec::new(),
            partial_messages: false,
            system_context: Vec::new(),
            extra_dirs: Vec::new(),
            guards: crate::guards::Guards::default(),
            permission_mode: None,
            cancel: None,
            deny: Vec::new(),
        }
    }

    /// Let the agent work in directories beyond the workspace.
    ///
    /// The caller must extend the SENTINEL to the same roots. This method only
    /// widens what the agent can touch; widening reach without widening the
    /// check is how a protected file stops being protected.
    #[must_use]
    pub fn with_extra_dirs(mut self, dirs: Vec<std::path::PathBuf>) -> Self {
        self.set_extra_dirs(dirs);
        self
    }

    /// The same, for a caller that already owns the backend in place.
    pub fn set_extra_dirs(&mut self, dirs: Vec<std::path::PathBuf>) {
        self.extra_dirs = dirs;
        // Reach and guard move together, by construction rather than by anyone
        // remembering to call something afterwards.
        self.rebuild_deny();
    }

    /// Give the backend the flag that says the user asked to stop.
    ///
    /// Checked between output lines, so it lands while the agent is talking.
    /// A turn that has gone quiet — a long tool call with no output — is not
    /// interrupted until it next says something; that limit is real and stated
    /// rather than implied.
    #[must_use]
    pub fn with_cancel(mut self, token: crate::runtime::cancel::CancelToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Give the agent the project context this harness assembled.
    ///
    /// Rides EVERY turn, not just the first: the agent may compact its own
    /// conversation, and context that only arrived once would quietly stop
    /// applying part-way through a session.
    #[must_use]
    pub fn with_system_context(mut self, blocks: Vec<String>) -> Self {
        self.system_context = blocks
            .into_iter()
            .filter(|b| !b.trim().is_empty())
            .collect();
        self
    }

    /// What the agent is told about this project, beyond the instruction.
    #[must_use]
    pub fn system_context(&self) -> &[String] {
        &self.system_context
    }

    /// Ask for text as it is generated, for an interactive caller.
    ///
    /// The caller then reads [`AgentEvent::TextDelta`] and must IGNORE
    /// [`AgentEvent::Text`], which still arrives with the same words in it.
    #[must_use]
    pub fn streaming(mut self, on: bool) -> Self {
        self.partial_messages = on;
        self
    }

    /// Refuse the agent write access to the operator's protected paths.
    ///
    /// **Prevention, not detection.** The agent enforces these itself, so the
    /// write never happens — where [`crate::guards::Sentinel`] can only notice
    /// afterwards. Both are used: they cover different routes and neither is
    /// asked to be complete on its own.
    #[must_use]
    pub fn with_guards(mut self, guards: &crate::guards::Guards) -> Self {
        self.guards = guards.clone();
        self.rebuild_deny();
        self
    }

    /// Re-derive the deny rules from the policy and the current roots.
    ///
    /// Called by everything that changes either. A bare glob is matched
    /// relative to the working directory, so a root added later needs its own
    /// absolute rules.
    fn rebuild_deny(&mut self) {
        let restrictions = crate::guards::restrictions_for(&self.guards, &self.extra_dirs);
        self.deny = render_denials(&restrictions);
    }

    /// The deny rules this backend will pass to the agent.
    #[must_use]
    pub fn deny_rules(&self) -> &[String] {
        &self.deny
    }

    /// Hand this agent an MCP server, as `--mcp-config` JSON.
    ///
    /// Build the JSON with [`crate::desktop::mcp::client_config_json`] so the
    /// flag that selects the server mode cannot drift from the one `main.rs`
    /// parses.
    #[must_use]
    pub fn with_mcp_config(mut self, config: impl Into<String>) -> Self {
        self.desktop_mcp = Some(config.into());
        self
    }

    /// Override the binary (e.g. an absolute path from agent discovery).
    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self
    }

    /// Continue a specific pre-existing session.
    #[must_use]
    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Serve the next turn with a named model, falling back through the rest.
    ///
    /// **Changing the model abandons the session.** A resumed session carries
    /// the earlier turns, and asking a different model to continue them is a
    /// request the harness cannot reason about — it would be paying to resume a
    /// conversation the new model did not have. Starting fresh costs the prompt
    /// prefix once and is the only behaviour that means what it says.
    #[must_use]
    pub fn with_models(mut self, candidates: &[String]) -> Self {
        // One implementation of the rule, not two. A second copy of "changing
        // model abandons the session" is a copy that can drift, and a drifted
        // safety rule is worse than an absent one.
        AgentBackend::set_models(&mut self, candidates);
        self
    }

    /// Forget the session, so the next turn starts a new one.
    pub fn clear_session(&mut self) {
        self.session_id = None;
    }

    /// How much the agent may do without asking, e.g. `plan` or `acceptEdits`.
    pub fn set_permission_mode(&mut self, mode: &str) {
        self.permission_mode = Some(mode.to_string());
    }

    /// Continue an existing agent session on the next turn.
    pub fn set_session_id(&mut self, session_id: &str) {
        self.session_id = Some(session_id.to_string());
    }

    /// The model the next turn will ask for, if one was chosen.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Whether this agent recognises a model id as one it can serve.
    ///
    /// Recognises Claude families and generic names, and rejects known foreign
    /// vendor namespaces (`gemini-`, `gpt-`, `o1/3/4`, `codex-`) so a cross-vendor
    /// candidate chain from a shared tier never sends a foreign model to the CLI.
    pub fn serves(&self, model: &str) -> bool {
        const FOREIGN_PREFIXES: [&str; 7] =
            ["gemini-", "gpt-", "o1", "o3", "o4", "chatgpt-", "codex-"];
        let id = model.trim().to_ascii_lowercase();
        if id.is_empty() {
            return false;
        }
        !FOREIGN_PREFIXES.iter().any(|f| id.starts_with(f))
    }
}

/// Map one wire line to zero or more harness events.
///
/// Split out from the process plumbing so it can be tested against captured
/// output without spawning anything.
fn map_line(line: &str, outcome: &mut Option<TurnOutcome>) -> Vec<AgentEvent> {
    let mut events = Vec::new();

    // A line we cannot model is data we don't understand yet, never a reason to
    // abort the turn — the CLI adds event types over time.
    let Ok(evt) = serde_json::from_str::<CliEvent>(line) else {
        return events;
    };

    match evt {
        CliEvent::System(s) if s.subtype == "init" => {
            events.push(AgentEvent::SessionStarted {
                session_id: s.session_id.unwrap_or_default(),
                model: s.model.unwrap_or_else(|| "unknown".to_string()),
            });
        }
        CliEvent::System(_) => {}
        CliEvent::StreamEvent(e) => {
            // Subagent output is not the main conversation's text — the same
            // rule the whole-block branch below applies.
            if e.parent_tool_use_id.is_some() {
                return events;
            }
            if e.event.event_type == "content_block_delta" {
                if let Some(delta) = e.event.delta {
                    if delta.delta_type == "text_delta" && !delta.text.is_empty() {
                        events.push(AgentEvent::TextDelta(delta.text));
                    }
                }
            }
        }
        CliEvent::Assistant(a) => {
            // Subagent output is not the main conversation's text.
            if a.parent_tool_use_id.is_some() {
                return events;
            }
            for block in a.message.content {
                match block {
                    WireContentBlock::Text { text } => events.push(AgentEvent::Text(text)),
                    WireContentBlock::ToolUse { id, name, input } => {
                        events.push(AgentEvent::ToolUse {
                            id,
                            name,
                            input: input.to_string(),
                        });
                    }
                    // A result never arrives on an assistant message; it is
                    // handled where it does, on the `user` one.
                    WireContentBlock::ToolResult { .. } | WireContentBlock::Other => {}
                }
            }
        }
        CliEvent::RateLimit(r) => events.push(AgentEvent::Quota {
            status: r.rate_limit_info.status,
            kind: r.rate_limit_info.rate_limit_type,
            resets_at: r.rate_limit_info.resets_at,
            using_overage: r.rate_limit_info.is_using_overage,
        }),
        CliEvent::Result(r) => {
            events.push(AgentEvent::Usage {
                input_tokens: r.usage.input_tokens,
                output_tokens: r.usage.output_tokens,
                cache_creation_tokens: r.usage.cache_creation_input_tokens,
                cache_read_tokens: r.usage.cache_read_input_tokens,
            });
            // NOTE: the result line also carries a `result` field holding the
            // final assembled text. It repeats what the assistant content
            // blocks already delivered, so emitting both duplicates every
            // reply. The incremental stream is the single source of truth.
            // Only when the turn FAILED. On success `result` is a copy of the
            // reply that already streamed, and carrying it would duplicate
            // every answer.
            let detail = if r.is_error {
                let said = r.result.unwrap_or_default();
                let kind = r.subtype.unwrap_or_default();
                let joined = match (kind.trim(), said.trim()) {
                    ("", "") => String::new(),
                    (k, "") => k.to_string(),
                    ("", t) => t.to_string(),
                    (k, t) => format!("{k}: {t}"),
                };
                (!joined.is_empty()).then_some(joined)
            } else {
                None
            };
            *outcome = Some(TurnOutcome {
                session_id: r.session_id,
                stop_reason: r.stop_reason.unwrap_or_else(|| "unknown".to_string()),
                is_error: r.is_error,
                detail,
                // The keys are model ids; the values are per-model token counts
                // this harness has no use for. Sorted by the map, so the record
                // is the same on every run for the same turn.
                served_by: r.model_usage.into_keys().collect(),
            });
        }
        CliEvent::User(u) => {
            for block in u.message.content {
                if let WireContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } = block
                {
                    events.push(AgentEvent::ToolResult {
                        id: tool_use_id,
                        output: flatten(&content),
                        is_error,
                    });
                }
            }
        }
        CliEvent::Other => {}
    }

    events
}

/// Consume an agent's event stream to completion.
///
/// Split from the spawn plumbing so the loop that actually matters — event
/// mapping, session capture for `--resume`, and outcome extraction — is
/// testable over canned bytes without launching a process.
///
/// `session` is updated in place as soon as the session id appears, so a turn
/// that fails partway still leaves the backend able to resume.
/// The environment variable naming which agent profile to use.
const PROFILE_ENV: &str = "CLAUDE_CONFIG_DIR";
/// Profile directories are the home entries beginning with this.
const PROFILE_PREFIX: &str = ".claude";
/// Where a profile records its session.
const CREDENTIALS_FILE: &str = ".credentials.json";
/// A session with less than this left is treated as already gone, so a turn is
/// not started on a login that will expire while it runs.
const SESSION_MARGIN_SECS: i64 = 120;

/// One agent profile and how much longer its session is good for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// The directory to point the agent at.
    pub dir: std::path::PathBuf,
    /// Seconds of session left; negative means it expired that long ago.
    pub seconds_left: i64,
}

/// Every agent profile in the home directory, whether or not its session is live.
///
/// # Why this exists
///
/// The agent falls back to its default profile when nothing says otherwise, and
/// that profile is not necessarily the one the operator logged in with. On this
/// machine the default held a session that had expired while a second profile
/// was live, so every turn failed with an authentication error that named
/// neither profile. Choosing the profile that can actually work removes a whole
/// class of failure that looks exactly like a broken harness.
///
/// # Credentials are never read
///
/// Only the expiry timestamp is taken from the file. The tokens beside it are
/// not read, not copied and not logged — the harness has no use for them and
/// every reason not to touch them.
#[must_use]
pub fn profiles() -> Vec<Profile> {
    home_dir()
        .map(|home| profiles_in(&home))
        .unwrap_or_default()
}

/// The profiles under a given home directory.
///
/// Split from [`profiles`] so the selection can be tested against a directory
/// built for the purpose, rather than against whatever the machine happens to
/// hold.
#[must_use]
pub fn profiles_in(home: &std::path::Path) -> Vec<Profile> {
    let Ok(entries) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let mut found: Vec<Profile> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        // A directory only: the home also holds `.claude.json` and a long tail
        // of its backups, none of which are profiles.
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(PROFILE_PREFIX))
        })
        .filter_map(|dir| {
            session_seconds_left(&dir).map(|seconds_left| Profile { dir, seconds_left })
        })
        .collect();
    // Longest-lived first, then by path so the order is the same on every run.
    found.sort_by(|a, b| {
        b.seconds_left
            .cmp(&a.seconds_left)
            .then_with(|| a.dir.cmp(&b.dir))
    });
    found
}

/// The profile whose session is live and lasts longest, if any is.
#[must_use]
pub fn usable_profile() -> Option<Profile> {
    pick_usable(profiles())
}

/// The first profile with enough session left to be worth starting a turn on.
#[must_use]
pub fn pick_usable(found: Vec<Profile>) -> Option<Profile> {
    found
        .into_iter()
        .find(|p| p.seconds_left > SESSION_MARGIN_SECS)
}

/// How long this profile's session has left, or `None` if it records none.
fn session_seconds_left(dir: &std::path::Path) -> Option<i64> {
    let text = std::fs::read_to_string(dir.join(CREDENTIALS_FILE)).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;
    let expires_at_ms = find_expiry(&parsed)?;
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()?;
    Some((expires_at_ms - now_ms) / 1_000)
}

/// The session expiry anywhere in the document, in milliseconds.
///
/// Searched rather than addressed by path: the key sits one level down today,
/// and a layout change should cost the selection, never break the harness.
fn find_expiry(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(found) = map.get("expiresAt").and_then(serde_json::Value::as_i64) {
                return Some(found);
            }
            map.values().find_map(find_expiry)
        }
        _ => None,
    }
}

/// The operator's home directory.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

/// A tool result's content, whatever shape it arrived in.
///
/// Some tools return a plain string, others a list of blocks. Assuming either
/// one loses the output of every tool that uses the other.
fn flatten(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join(
                "
",
            ),
        other => other.to_string(),
    }
}

/// Where to write the agent's raw output, if the operator asked for it.
///
/// `INCORD_DEBUG_STREAM=<path>` appends every line the agent sends. It is the
/// only way to answer "why did this turn fail" without guessing.
#[must_use]
pub fn debug_stream_path() -> Option<String> {
    std::env::var("INCORD_DEBUG_STREAM")
        .ok()
        .filter(|p| !p.is_empty())
}

/// What a cancelled turn reports, so the caller can tell it apart from a crash.
pub const CANCELLED: &str = "the turn was cancelled";

fn consume_stream<R: BufRead>(
    reader: R,
    session: &mut Option<String>,
    on_event: &mut dyn FnMut(&AgentEvent),
    cancel: Option<&crate::runtime::cancel::CancelToken>,
) -> Result<Option<TurnOutcome>, AgentError> {
    let mut outcome = None;

    for line in reader.lines() {
        // Checked per line, which is where the flag can actually land: the read
        // below blocks until the agent says something.
        if cancel.is_some_and(crate::runtime::cancel::CancelToken::is_cancelled) {
            return Err(AgentError::new(CANCELLED));
        }
        let line = line.map_err(|e| AgentError::new(format!("reading agent output: {e}")))?;
        // Every raw line, when asked for. A turn that fails for a reason the
        // harness cannot explain — "stopped without finishing: stop_sequence" —
        // is unanswerable without the bytes the agent actually sent, and
        // guessing at them from a screenshot is how an afternoon disappears.
        if let Some(path) = debug_stream_path() {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                use std::io::Write as _;
                let _ = writeln!(file, "{line}");
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        for event in map_line(&line, &mut outcome) {
            if let AgentEvent::SessionStarted { session_id, .. } = &event {
                if !session_id.is_empty() {
                    *session = Some(session_id.clone());
                }
            }
            on_event(&event);
        }
    }

    Ok(outcome)
}

impl AgentBackend for ClaudeCodeBackend {
    /// Both halves, and both MEASURED rather than assumed.
    ///
    /// Writes: a probe with `Edit(protected.txt)` in force left the file
    /// untouched, including when a sub-agent was explicitly told to write it.
    /// Sub-agents: a turn given the sub-agent tool on `--disallowedTools`
    /// reported it ABSENT, not merely refused.
    fn enforces(&self) -> crate::guards::Enforcement {
        crate::guards::Enforcement::EVERYTHING
    }

    fn system_context(&self) -> &[String] {
        &self.system_context
    }

    fn id(&self) -> &str {
        "claude-code"
    }

    // The session-shaped methods, forwarded to this backend's own. Fully
    // qualified rather than `self.x()`, which would resolve to the inherent
    // method and read as though the trait were calling itself.

    fn set_extra_dirs(&mut self, dirs: Vec<std::path::PathBuf>) {
        ClaudeCodeBackend::set_extra_dirs(self, dirs);
    }

    fn mcp_config(&self) -> Option<String> {
        self.desktop_mcp.clone()
    }

    fn set_team_identity(&mut self, role: &str) {
        self.team_identity = Some(role.to_string());
    }

    fn deny_rules(&self) -> &[String] {
        ClaudeCodeBackend::deny_rules(self)
    }

    fn set_cancel(&mut self, token: crate::runtime::cancel::CancelToken) {
        self.cancel = Some(token);
    }

    fn model(&self) -> Option<&str> {
        ClaudeCodeBackend::model(self)
    }

    fn clear_session(&mut self) {
        ClaudeCodeBackend::clear_session(self);
    }

    fn set_permission_mode(&mut self, mode: &str) {
        ClaudeCodeBackend::set_permission_mode(self, mode);
    }

    fn set_session_id(&mut self, session_id: &str) {
        ClaudeCodeBackend::set_session_id(self, session_id);
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    fn set_models(&mut self, candidates: &[String]) {
        let applicable: Vec<String> = candidates
            .iter()
            .filter(|c| self.serves(c))
            .cloned()
            .collect();
        let Some((first, rest)) = applicable.split_first() else {
            return;
        };
        // Changing model abandons the session — see `with_models` for why a
        // resumed conversation cannot be handed to a different model.
        if self.model.as_deref() != Some(first.as_str()) {
            self.session_id = None;
        }
        self.model = Some(first.clone());
        self.fallbacks = rest.to_vec();
    }

    fn ladder_for(&self, role: &str) -> Vec<String> {
        let ids: &[&str] = match role {
            "explore" | "grunt" => &["haiku"],
            "diagnose" | "review" | "plan" => &["opus", "sonnet"],
            "implement" | "frontend" => &["sonnet", "opus"],
            _ => &[],
        };
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    fn run_turn(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError> {
        let (budgeted_prompt, budgeted_context) =
            crate::agent::budget_for_spawn(prompt, &self.system_context);

        let mut cmd = Command::new(&self.binary);
        cmd.arg("-p")
            .arg(&budgeted_prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose");

        // An MCP server the harness hands over, rather than one the operator
        // happened to configure. NOT accompanied by --strict-mcp-config: see
        // the field's documentation for why that would cut off memory.
        if let Some(config) = &self.desktop_mcp {
            cmd.arg("--mcp-config").arg(config);
        }

        // Which model serves this turn. Absent, the CLI uses its own default,
        // which is the right behaviour for a caller with no opinion.
        if let Some(model) = &self.model {
            cmd.arg("--model").arg(model);
        }
        if self.partial_messages {
            cmd.arg("--include-partial-messages");
        }
        if !self.extra_dirs.is_empty() {
            cmd.arg("--add-dir");
            for dir in &self.extra_dirs {
                cmd.arg(dir);
            }
        }
        if let Some(mode) = &self.permission_mode {
            cmd.arg("--permission-mode").arg(mode);
        }
        if !budgeted_context.is_empty() {
            cmd.arg("--append-system-prompt").arg(&budgeted_context);
        }
        // The protected paths, enforced by the agent's own permission layer
        // before a write happens rather than caught after one.
        if !self.deny.is_empty() {
            cmd.arg("--disallowedTools");
            for rule in &self.deny {
                cmd.arg(rule);
            }
        }
        if !self.fallbacks.is_empty() {
            // One comma-separated list, tried in order — the CLI's own shape for
            // the ordered chain the role table produces.
            cmd.arg("--fallback-model").arg(self.fallbacks.join(","));
        }

        // Continue the existing session rather than starting a new one. A fresh
        // session re-pays the entire prompt prefix; resuming does not.
        if let Some(sid) = &self.session_id {
            cmd.arg("--resume").arg(sid);
        }

        // Deliberately NOT --bare: bare mode never reads OAuth credentials or
        // the system keychain, which drops the run off the user's subscription
        // and onto an API key.
        //
        // Point the agent at a profile whose session is actually live.
        //
        // Skipped entirely when the operator has already said which profile to
        // use: an explicit choice is a decision, and silently overriding it
        // would be worse than the fallback this replaces.
        if std::env::var_os(PROFILE_ENV).is_none() {
            if let Some(profile) = usable_profile() {
                cmd.env(PROFILE_ENV, &profile.dir);
            }
        }

        // WHO THIS AGENT IS ON THE TEAM CHANNEL, for the task about to run.
        // The team MCP server is a child of this process and reads it from
        // here — see [`crate::agent::AgentBackend::set_team_identity`] for why
        // it cannot come from the registration or from the model.
        if let Some(role) = &self.team_identity {
            cmd.env(crate::team_mcp::IDENTITY_ENV, role);
        }

        // WHAT WAS ACTUALLY SPAWNED, on the ordinary trace. The flags decide the
        // whole turn — which model, which profile, which deny rules — and a
        // failure is unreadable without them. The full argv, including the
        // appended system prompt, still goes to the stream dump below; this is
        // the one-line version that belongs beside everything else that happened.
        if crate::trace::on() {
            let flags: Vec<String> = cmd
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .filter(|a| a.starts_with("--"))
                .collect();
            crate::trace!(
                "agent",
                "spawning {} model={:?} resume={:?} flags={}",
                self.binary,
                self.model,
                self.session_id,
                flags.join(" ")
            );
        }

        // The exact command, when the operator asks for it.
        //
        // A turn that fails cannot be investigated from outside the harness
        // without the argv that produced it: the flags, the deny rules and the
        // whole appended system prompt are assembled here and were, until now,
        // invisible. Written alongside the raw stream so a failure can be
        // replayed by hand and the flags bisected.
        if let Some(path) = debug_stream_path() {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                use std::io::Write as _;
                let argv: Vec<String> = std::iter::once(self.binary.clone())
                    .chain(cmd.get_args().map(|a| a.to_string_lossy().into_owned()))
                    .collect();
                // As JSON, so an argument containing spaces or newlines — the
                // system prompt does — survives being read back.
                let _ = writeln!(
                    file,
                    "{}",
                    serde_json::json!({ "type": "incord_argv", "argv": argv })
                );
            }
        }

        // stdin must be closed explicitly, or the CLI waits ~3s for piped input
        // on every invocation.
        let mut child = cmd
            .current_dir(&self.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| AgentError::new(format!("failed to spawn `{}`: {e}", self.binary)))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::new("child stdout was not piped"))?;

        let outcome = consume_stream(
            BufReader::new(stdout),
            &mut self.session_id,
            on_event,
            self.cancel.as_ref(),
        );

        // Whatever happened, the child must not outlive this call. A cancelled
        // turn that left the agent running would keep writing files with
        // nobody reading its output.
        let cancelled = matches!(outcome, Err(ref e) if e.to_string() == CANCELLED);
        if cancelled {
            let _ = child.kill();
        }
        let outcome = outcome?;

        let status = child
            .wait()
            .map_err(|e| AgentError::new(format!("waiting for agent: {e}")))?;

        match outcome {
            Some(o) => {
                if !o.session_id.is_empty() {
                    self.session_id = Some(o.session_id.clone());
                }
                Ok(o)
            }
            None => {
                let mut stderr = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.read_to_string(&mut stderr);
                }
                Err(AgentError::new(format!(
                    "agent exited with {} before producing a result: {}",
                    status.code().unwrap_or(-1),
                    stderr.trim()
                )))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — fixtures are real lines captured from Claude Code 2.1.229
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    /// Build a home directory holding the named profiles.
    ///
    /// `hours` is how long each session has left; negative means it expired.
    fn home_with(profiles: &[(&str, Option<f64>)]) -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("tmp");
        let now_ms = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_millis(),
        )
        .expect("fits");
        for (name, hours) in profiles {
            let dir = home.path().join(name);
            std::fs::create_dir_all(&dir).expect("dir");
            if let Some(h) = hours {
                #[allow(clippy::cast_possible_truncation)]
                let expires = now_ms + (h * 3_600_000.0) as i64;
                // Same nesting the real file uses, and the same field names.
                // The token fields are deliberately absent: the code must never
                // need them, and a test that supplied them would hide it if it did.
                std::fs::write(
                    dir.join(".credentials.json"),
                    format!(r#"{{"claudeAiOauth":{{"expiresAt":{expires},"scopes":[]}}}}"#),
                )
                .expect("write");
            }
        }
        home
    }

    #[test]
    fn the_profile_with_a_live_session_is_chosen_over_the_default() {
        // The exact situation that broke every turn for a day: the default
        // profile's session had expired while another profile was still good,
        // and the agent fell back to the default because nothing said otherwise.
        let home = home_with(&[(".claude", Some(-4.0)), (".claude-a", Some(2.0))]);
        let chosen = pick_usable(profiles_in(home.path())).expect("one is live");
        assert!(
            chosen.dir.ends_with(".claude-a"),
            "chose {:?}, but only .claude-a can work",
            chosen.dir
        );
    }

    #[test]
    fn the_longest_lived_session_wins() {
        let home = home_with(&[
            (".claude", Some(1.0)),
            (".claude-a", Some(9.0)),
            (".claude-b", Some(4.0)),
        ]);
        let chosen = pick_usable(profiles_in(home.path())).expect("live");
        assert!(chosen.dir.ends_with(".claude-a"), "{:?}", chosen.dir);
    }

    #[test]
    fn every_session_expired_means_no_choice_rather_than_a_bad_one() {
        // Picking an expired profile would produce the same unexplained failure
        // this replaces. Choosing nothing lets the turn fail with the message
        // that actually names the problem.
        let home = home_with(&[(".claude", Some(-1.0)), (".claude-a", Some(-9.0))]);
        assert_eq!(pick_usable(profiles_in(home.path())), None);
    }

    #[test]
    fn a_session_about_to_expire_is_not_treated_as_live() {
        // Starting a turn on a session with seconds left fails halfway through,
        // which is worse than not starting it.
        let home = home_with(&[(".claude", Some(0.01))]); // 36 seconds
        assert_eq!(pick_usable(profiles_in(home.path())), None);
    }

    #[test]
    fn only_directories_count_as_profiles() {
        // The home also holds `.claude.json` and a long tail of its backups.
        let home = home_with(&[(".claude-a", Some(3.0))]);
        std::fs::write(home.path().join(".claude.json"), "{}").expect("write");
        std::fs::write(home.path().join(".claude.json.backup"), "{}").expect("write");
        let found = profiles_in(home.path());
        assert_eq!(found.len(), 1, "found {found:?}");
        assert!(found[0].dir.ends_with(".claude-a"));
    }

    #[test]
    fn a_directory_with_no_session_recorded_is_skipped() {
        let home = home_with(&[(".claude", None), (".claude-a", Some(3.0))]);
        let found = profiles_in(home.path());
        assert_eq!(found.len(), 1, "found {found:?}");
    }

    #[test]
    fn the_expiry_is_found_wherever_it_is_nested() {
        let doc: serde_json::Value =
            serde_json::from_str(r#"{"a":{"b":{"expiresAt":1234567890123}}}"#).expect("parse");
        assert_eq!(find_expiry(&doc), Some(1_234_567_890_123));
        let none: serde_json::Value = serde_json::from_str(r#"{"a":{"b":1}}"#).expect("parse");
        assert_eq!(find_expiry(&none), None);
    }

    /// **The harness's project context must actually REACH the agent.** It was
    /// assembled and then dropped when the loop was removed: the build stayed
    /// green, the tests passed and the live runs worked, while the guard rules,
    /// the auditor's findings and the recorder's handoff silently stopped being
    /// sent. Nothing catches that except asserting the argument is built.
    #[test]
    fn the_session_context_is_carried_to_the_agent() {
        let backend = ClaudeCodeBackend::new(std::path::PathBuf::from("."))
            .with_system_context(vec!["<guards>do not edit tests</guards>".to_string()]);
        assert_eq!(
            backend.system_context(),
            &["<guards>do not edit tests</guards>".to_string()]
        );
    }

    /// Empty blocks are dropped rather than sent as blank lines — a context
    /// section with nothing in it is noise the agent pays for.
    #[test]
    fn empty_context_blocks_are_not_sent() {
        let backend = ClaudeCodeBackend::new(std::path::PathBuf::from("."))
            .with_system_context(vec![String::new(), "   ".to_string(), "real".to_string()]);
        assert_eq!(backend.system_context(), &["real".to_string()]);
    }

    /// A session with no context sends no flag at all.
    #[test]
    fn no_context_means_no_flag() {
        let backend = ClaudeCodeBackend::new(std::path::PathBuf::from("."));
        assert!(backend.system_context().is_empty());
    }

    /// **A cancelled turn stops reading and says so.** Without a distinct
    /// error the caller cannot tell "the user pressed Ctrl-C" from "the agent
    /// crashed", and would report a crash.
    #[test]
    fn a_cancelled_turn_stops_and_names_itself() {
        let token = crate::runtime::cancel::CancelToken::new();
        token.cancel();
        let lines = "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\"}\n";
        let mut session = None;
        let mut seen = Vec::new();
        let err = consume_stream(
            std::io::BufReader::new(lines.as_bytes()),
            &mut session,
            &mut |e: &AgentEvent| seen.push(e.clone()),
            Some(&token),
        )
        .expect_err("a cancelled turn must not report success");
        assert_eq!(err.to_string(), CANCELLED);
        assert!(seen.is_empty(), "it must stop BEFORE handling more output");
    }

    /// Without cancellation the same stream is read normally — the control
    /// that proves the test above is measuring the flag and not the fixture.
    #[test]
    fn the_same_stream_reads_normally_when_not_cancelled() {
        let lines = "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\"}\n";
        let mut session = None;
        let mut seen = Vec::new();
        consume_stream(
            std::io::BufReader::new(lines.as_bytes()),
            &mut session,
            &mut |e: &AgentEvent| seen.push(e.clone()),
            None,
        )
        .expect("it reads");
        assert_eq!(session.as_deref(), Some("s"));
        assert_eq!(seen.len(), 1);
    }

    /// **Captured from a real run, not composed here.** The delta shape is the
    /// CLI's, and a parser written against an imagined one would pass its own
    /// test and fail the first real line.
    #[test]
    fn a_text_delta_from_the_real_wire_becomes_a_delta_event() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"h"}},"session_id":"a77936b0","parent_tool_use_id":null,"uuid":"bfc09fc8"}"#;
        let mut outcome = None;
        let events = map_line(line, &mut outcome);
        assert_eq!(events, vec![AgentEvent::TextDelta("h".to_string())]);
    }

    /// A subagent's deltas are not the main conversation's text — the same rule
    /// the whole-block branch already applies.
    #[test]
    fn a_subagents_delta_is_not_the_conversation() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"h"}},"parent_tool_use_id":"toolu_1"}"#;
        let mut outcome = None;
        assert!(map_line(line, &mut outcome).is_empty());
    }

    /// The other stream frames carry no user-facing text and must stay silent —
    /// message_start and content_block_stop arrive for every single turn.
    #[test]
    fn stream_frames_that_are_not_text_produce_nothing() {
        let mut outcome = None;
        for line in [
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"m"}},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null}"#,
        ] {
            assert!(
                map_line(line, &mut outcome).is_empty(),
                "should be silent: {line}"
            );
        }
    }

    /// **Deltas are opt-in.** A caller collecting a report must not be handed
    /// two copies of the same words, so the flag is off unless asked for.
    #[test]
    fn partial_messages_are_off_unless_asked_for() {
        let plain = ClaudeCodeBackend::new(std::path::PathBuf::from("."));
        assert!(!plain.partial_messages);
        let streaming = ClaudeCodeBackend::new(std::path::PathBuf::from(".")).streaming(true);
        assert!(streaming.partial_messages);
    }
    use super::*;

    const REAL_ASSISTANT: &str = r#"{"type":"assistant","message":{"model":"claude-opus-5","id":"msg_011CdyZz5W5Z65fqwbCLUrvy","type":"message","role":"assistant","content":[{"type":"text","text":"OK"}],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":1}},"parent_tool_use_id":null,"session_id":"e2996c17-f358-46fa-89b9-e757a4742abe"}"#;

    const REAL_RATE_LIMIT: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1786575600,"rateLimitType":"five_hour","overageStatus":"rejected","isUsingOverage":false},"session_id":"e2996c17-f358-46fa-89b9-e757a4742abe"}"#;

    /// The real result line puts `type` near the END of the object — a
    /// deserializer that assumes `type` comes first will not parse it.
    /// A FAILED result, in the wire shape the agent actually emits.
    ///
    /// The `result` and `subtype` fields here were not deserialized at all, so
    /// a turn that ended this way was reported to the operator as the single
    /// word `stop_sequence` — for an entire day, while the sentence explaining
    /// it sat unread in the same line.
    const FAILED_RESULT: &str = r#"{"is_error":true,"duration_api_ms":900,"num_turns":3,"stop_reason":"stop_sequence","session_id":"e2996c17-f358-46fa-89b9-e757a4742abe","usage":{"input_tokens":1,"output_tokens":2},"subtype":"error_during_execution","result":"tool use was interrupted before it completed","type":"result","duration_ms":1200}"#;

    const REAL_RESULT: &str = r#"{"is_error":false,"duration_api_ms":1846,"num_turns":1,"stop_reason":"end_turn","session_id":"393279e4-f6e9-4d64-bd17-57926443ef94","total_cost_usd":0.399,"usage":{"input_tokens":1,"cache_creation_input_tokens":36371,"cache_read_input_tokens":23432,"output_tokens":4},"subtype":"success","result":"OK","type":"result","duration_ms":31639}"#;

    fn map(line: &str) -> (Vec<AgentEvent>, Option<TurnOutcome>) {
        let mut outcome = None;
        let events = map_line(line, &mut outcome);
        (events, outcome)
    }

    #[test]
    fn parses_assistant_text_from_real_output() {
        let (events, _) = map(REAL_ASSISTANT);
        assert_eq!(events, vec![AgentEvent::Text("OK".to_string())]);
    }

    /// A subagent's messages must not be mistaken for the main conversation.
    #[test]
    fn subagent_output_is_not_emitted_as_main_text() {
        let sub = REAL_ASSISTANT.replace(
            r#""parent_tool_use_id":null"#,
            r#""parent_tool_use_id":"toolu_1""#,
        );
        assert_ne!(sub, REAL_ASSISTANT, "fixture must actually differ");
        let (events, _) = map(&sub);
        assert!(
            events.is_empty(),
            "subagent text must not surface as main-conversation output; got {events:?}"
        );
    }

    #[test]
    fn parses_subscription_quota_signal() {
        let (events, _) = map(REAL_RATE_LIMIT);
        assert_eq!(
            events,
            vec![AgentEvent::Quota {
                status: "allowed".to_string(),
                kind: "five_hour".to_string(),
                resets_at: 1_786_575_600,
                using_overage: false,
            }]
        );
    }

    /// Guards the field-order hazard: `type` is not the first key on this line.
    #[test]
    fn a_failed_turn_reports_what_the_agent_said_about_it() {
        let (_, outcome) = map(FAILED_RESULT);
        let outcome = outcome.expect("the result line produces an outcome");
        assert!(outcome.is_error);
        let said = outcome.failure();
        assert!(
            said.contains("tool use was interrupted"),
            "the agent's own explanation must reach the operator: {said}"
        );
        assert!(
            said.contains("error_during_execution"),
            "and so must the category: {said}"
        );
        assert!(
            !said.contains("gave no reason"),
            "it DID give a reason: {said}"
        );
    }

    #[test]
    fn a_successful_turn_carries_no_detail() {
        // On success `result` repeats the streamed reply; carrying it would
        // print every answer twice.
        let (_, outcome) = map(REAL_RESULT);
        assert_eq!(outcome.expect("outcome").detail, None);
    }

    #[test]
    fn parses_result_line_despite_type_being_last() {
        let (events, outcome) = map(REAL_RESULT);
        let outcome = outcome.expect("result line must yield a turn outcome");
        assert_eq!(outcome.stop_reason, "end_turn");
        assert!(!outcome.is_error);
        assert_eq!(outcome.session_id, "393279e4-f6e9-4d64-bd17-57926443ef94");
        assert_eq!(
            events,
            vec![AgentEvent::Usage {
                input_tokens: 1,
                output_tokens: 4,
                cache_creation_tokens: 36371,
                cache_read_tokens: 23432,
            }]
        );
    }

    /// The final text must be emitted exactly once. The result line repeats it,
    /// so counting both duplicates every reply the user sees.
    #[test]
    fn final_text_is_not_emitted_twice() {
        let mut outcome = None;
        let mut texts = Vec::new();
        for line in [REAL_ASSISTANT, REAL_RESULT] {
            for e in map_line(line, &mut outcome) {
                if let AgentEvent::Text(t) = e {
                    texts.push(t);
                }
            }
        }
        assert_eq!(
            texts,
            vec!["OK".to_string()],
            "assistant blocks and the result line both carry the reply; only one may be emitted"
        );
    }

    /// An unrecognised line must be skipped, not abort the turn.
    #[test]
    fn unknown_event_type_is_skipped() {
        let (events, outcome) = map(r#"{"type":"some_future_event","payload":{}}"#);
        assert!(events.is_empty());
        assert!(outcome.is_none());
    }

    #[test]
    fn malformed_line_does_not_panic() {
        let (events, outcome) = map("not json at all");
        assert!(events.is_empty());
        assert!(outcome.is_none());
    }

    /// Drive the stream loop the way a real turn does, over canned bytes.
    fn drive(stream: &str) -> (Vec<AgentEvent>, Option<String>, Option<TurnOutcome>) {
        let mut session = None;
        let mut seen = Vec::new();
        let outcome = {
            let mut sink = |e: &AgentEvent| seen.push(e.clone());
            consume_stream(
                std::io::BufReader::new(std::io::Cursor::new(stream.as_bytes())),
                &mut session,
                &mut sink,
                None,
            )
            .expect("a well-formed stream must not error")
        };
        (seen, session, outcome)
    }

    /// A whole turn, in the order and shape the real CLI emits it — including
    /// the hook events that precede `init` when the agent loads user config.
    const REAL_SESSION: &str = concat!(
        r#"{"type":"system","subtype":"hook_started","hook_id":"b5d0"}"#,
        "\n",
        r#"{"type":"system","subtype":"hook_response","hook_id":"b5d0"}"#,
        "\n",
        r#"{"type":"system","subtype":"init","session_id":"sess-real","model":"claude-opus-5[1m]"}"#,
        "\n",
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"OK"}]},"parent_tool_use_id":null}"#,
        "\n",
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1786575600,"rateLimitType":"five_hour","isUsingOverage":false}}"#,
        "\n",
        r#"{"is_error":false,"stop_reason":"end_turn","session_id":"sess-real","usage":{"input_tokens":1,"output_tokens":4,"cache_creation_input_tokens":36371,"cache_read_input_tokens":23432},"type":"result"}"#,
        "\n",
    );

    /// The stream loop must turn a real turn into the right events, in order,
    /// and skip the blank line without treating it as data.
    #[test]
    fn consumes_a_real_turn_into_ordered_events() {
        let (events, session, outcome) = drive(REAL_SESSION);

        assert_eq!(
            events,
            vec![
                AgentEvent::SessionStarted {
                    session_id: "sess-real".to_string(),
                    model: "claude-opus-5[1m]".to_string(),
                },
                AgentEvent::Text("OK".to_string()),
                AgentEvent::Quota {
                    status: "allowed".to_string(),
                    kind: "five_hour".to_string(),
                    resets_at: 1_786_575_600,
                    using_overage: false,
                },
                AgentEvent::Usage {
                    input_tokens: 1,
                    output_tokens: 4,
                    cache_creation_tokens: 36371,
                    cache_read_tokens: 23432,
                },
            ],
            "hook events must be dropped and the rest emitted in stream order"
        );

        assert_eq!(session.as_deref(), Some("sess-real"));
        let outcome = outcome.expect("a result line must produce an outcome");
        assert_eq!(outcome.stop_reason, "end_turn");
        assert!(!outcome.is_error);
    }

    /// The session id must be captured even if the turn dies before the result
    /// line — otherwise a crashed turn forces the next one to start fresh and
    /// re-pay the whole prompt prefix.
    #[test]
    fn session_is_captured_even_when_the_turn_never_completes() {
        let truncated = REAL_SESSION
            .lines()
            .take(3) // hook, hook, init — then the agent dies
            .collect::<Vec<_>>()
            .join("\n");

        let (_, session, outcome) = drive(&truncated);

        assert_eq!(
            session.as_deref(),
            Some("sess-real"),
            "a turn that dies after init must still leave a resumable session"
        );
        assert!(
            outcome.is_none(),
            "no result line means no outcome — the caller must see the failure"
        );
    }

    /// Garbage in the middle of a stream must not abort the turn; the CLI adds
    /// event types over time and an unknown line is not a failure.
    #[test]
    fn a_corrupt_line_does_not_abort_the_turn() {
        let mut polluted = String::from(REAL_SESSION);
        polluted = polluted.replace(
            r#"{"type":"rate_limit_event""#,
            r#"{{{NOT JSON}}}
{"type":"rate_limit_event""#,
        );

        let (events, _, outcome) = drive(&polluted);
        assert!(
            outcome.is_some(),
            "the turn must still complete despite an unparseable line"
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Text(t) if t == "OK")));
    }

    /// Session reuse is the single largest cost lever in this backend: measured
    /// against CLI 2.1.229, a fresh session costs ~36k cache-creation tokens per
    /// turn versus ~1.8k when resuming — roughly 5.8x. That only holds if the
    /// backend actually remembers the session and asks to continue it, so this
    /// asserts the state transition that makes `--resume` possible.
    #[test]
    fn backend_remembers_the_session_so_later_turns_can_resume() {
        let mut backend = ClaudeCodeBackend::new(PathBuf::from("."));
        assert!(
            backend.session_id().is_none(),
            "control: a fresh backend has no session to resume"
        );

        // Drive the state transition the way a real turn does: the init line
        // carries the session id.
        let mut outcome = None;
        for event in map_line(
            r#"{"type":"system","subtype":"init","session_id":"sess-xyz","model":"m"}"#,
            &mut outcome,
        ) {
            if let AgentEvent::SessionStarted { session_id, .. } = event {
                backend.session_id = Some(session_id);
            }
        }

        assert_eq!(
            backend.session_id(),
            Some("sess-xyz"),
            "the backend must retain the session id, or every turn starts fresh \
             and re-pays the whole prompt prefix"
        );
    }

    /// An explicitly supplied session must be carried too, so a caller can
    /// continue a task across process restarts.
    #[test]
    fn explicit_session_is_carried() {
        let backend = ClaudeCodeBackend::new(PathBuf::from(".")).with_session("prior-session");
        assert_eq!(backend.session_id(), Some("prior-session"));
    }

    #[test]
    fn session_id_is_captured_for_resume() {
        let (events, _) = map(
            r#"{"type":"system","subtype":"init","session_id":"abc-123","model":"claude-opus-5[1m]"}"#,
        );
        assert_eq!(
            events,
            vec![AgentEvent::SessionStarted {
                session_id: "abc-123".to_string(),
                model: "claude-opus-5[1m]".to_string(),
            }]
        );
    }

    /// **Changing the model abandons the session, and that is deliberate.** A
    /// resumed session carries earlier turns; asking a different model to
    /// continue them means paying to resume a conversation it never had.
    #[test]
    fn switching_model_starts_a_fresh_session() {
        let mut backend = ClaudeCodeBackend::new(PathBuf::from("."))
            .with_session("s-existing")
            .with_models(&["cheap".to_string()]);
        assert_eq!(backend.model(), Some("cheap"));
        assert_eq!(
            backend.session_id(),
            None,
            "a session belonging to another model must not be resumed"
        );

        // Re-asking for the SAME model must not throw away a session — that
        // would re-pay the whole prompt prefix on every turn.
        backend.session_id = Some("s-cheap".to_string());
        backend.set_models(&["cheap".to_string()]);
        assert_eq!(backend.session_id(), Some("s-cheap"));

        // And switching again drops it.
        backend.set_models(&["expensive".to_string()]);
        assert_eq!(backend.session_id(), None);
    }

    /// An empty candidate list is "no opinion", not "clear the model".
    #[test]
    fn no_candidates_leaves_the_choice_untouched() {
        let mut backend =
            ClaudeCodeBackend::new(PathBuf::from(".")).with_models(&["cheap".to_string()]);
        backend.set_models(&[]);
        assert_eq!(backend.model(), Some("cheap"));
    }

    /// The rest of the chain becomes the CLI's own fallback list, which is the
    /// shape it takes: one comma-separated argument, tried in order.
    #[test]
    fn the_candidate_chain_becomes_the_fallback_list() {
        let backend = ClaudeCodeBackend::new(PathBuf::from(".")).with_models(&[
            "first".to_string(),
            "second".to_string(),
            "third".to_string(),
        ]);
        assert_eq!(backend.model(), Some("first"));
        assert_eq!(
            backend.fallbacks,
            vec!["second".to_string(), "third".to_string()]
        );
    }

    /// **What actually ran, from the billing record.** Captured from a real
    /// turn: the request named a model that does not exist, the CLI warned, and
    /// the FALLBACK answered — so `modelUsage` and the requested name disagree,
    /// which is exactly the case attribution has to get right.
    #[test]
    fn the_served_model_comes_from_the_billing_record_not_the_request() {
        let real = r#"{"type":"result","is_error":false,"session_id":"s-1",
            "stop_reason":"end_turn","result":"OK",
            "usage":{"input_tokens":1,"output_tokens":4},
            "modelUsage":{"claude-haiku-4-5-20251001":{"inputTokens":1,"outputTokens":4}}}"#;
        let (_, outcome) = map(real);
        let outcome = outcome.expect("a result line produces an outcome");
        assert_eq!(
            outcome.served_by,
            vec!["claude-haiku-4-5-20251001".to_string()]
        );
    }

    /// **Not reported is empty, never guessed.** A backend that says nothing
    /// about the model must not have the requested one filled in for it.
    #[test]
    fn a_result_without_a_billing_record_reports_no_model() {
        let (_, outcome) = map(REAL_RESULT);
        assert!(
            outcome.expect("an outcome").served_by.is_empty(),
            "an absent billing record must stay absent"
        );
    }

    /// **`Edit`, never `Write`.** Measured: a rule denying only `Write(tests/**)`
    /// let the file through, and this CLI reports that only `Edit(path)` rules
    /// are matched by file permission checks. A guard that parses and does
    /// nothing is worse than no guard, so the tool name is pinned.
    ///
    /// This test lives HERE, beside the renderer, because it is a fact about one
    /// agent's permission layer rather than about the harness's policy.
    #[test]
    fn the_rendered_rules_use_the_tool_name_that_actually_binds() {
        let rules = render_denials(&crate::guards::restrictions(
            &crate::guards::Guards::default(),
        ));
        assert!(!rules.is_empty(), "the defaults must produce rules");
        for rule in rules.iter().filter(|r| *r != SUBAGENT_TOOL) {
            assert!(
                rule.starts_with("Edit("),
                "`Write(...)` is silently ignored by the permission layer: {rule}"
            );
        }
    }

    /// The policy's "no sub-agents" becomes THIS agent's word for it.
    #[test]
    fn refusing_sub_agents_renders_as_this_agents_tool_name() {
        let with = crate::guards::Restrictions {
            no_write: Vec::new(),
            no_subagents: true,
        };
        let without = crate::guards::Restrictions {
            no_write: Vec::new(),
            no_subagents: false,
        };
        assert_eq!(render_denials(&with), vec![SUBAGENT_TOOL.to_string()]);
        assert!(
            render_denials(&without).is_empty(),
            "a policy that permits delegation must not deny it anyway"
        );
    }

    /// A bare glob from the policy is wrapped, not passed through raw.
    ///
    /// The policy deliberately carries `**/tests/**` with no syntax around it;
    /// handing that to the CLI unwrapped would be accepted and enforce nothing.
    #[test]
    fn a_bare_policy_glob_is_wrapped_in_this_agents_syntax() {
        let rules = render_denials(&crate::guards::Restrictions {
            no_write: vec!["**/tests/**".to_string()],
            no_subagents: false,
        });
        assert_eq!(rules, vec!["Edit(**/tests/**)".to_string()]);
    }

    /// Foreign vendor model IDs are dropped so mixed-fleet candidate lists from
    /// shared tiers do not cause Claude Code CLI to error on unsupported flags.
    #[test]
    fn foreign_model_id_is_dropped_from_claude_candidates() {
        let mut backend = ClaudeCodeBackend::new(PathBuf::from("."));
        backend.set_models(&[
            "gemini-3.8-flash-high".to_string(),
            "gpt-5.6-terra".to_string(),
        ]);
        assert_eq!(backend.model(), None);

        backend.set_models(&[
            "gemini-3.8-flash-high".to_string(),
            "sonnet".to_string(),
            "gpt-5.6-terra".to_string(),
        ]);
        assert_eq!(backend.model(), Some("sonnet"));
        assert_eq!(backend.fallbacks, Vec::<String>::new());
    }

    /// Every role in the native Claude Code ladder can be served by Claude Code.
    #[test]
    fn claude_code_ladder_for_known_roles_serves_every_model() {
        let backend = ClaudeCodeBackend::new(PathBuf::from("."));
        for role in crate::roles::KNOWN_ROLES {
            for id in backend.ladder_for(role) {
                assert!(
                    backend.serves(&id),
                    "role {role} drew model {id} which claude-code cannot serve"
                );
            }
        }
        assert_ne!(
            backend.ladder_for("explore"),
            backend.ladder_for("diagnose"),
            "reading and reasoning must not draw the same tier"
        );
    }
}
