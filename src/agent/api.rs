//! Driving a model over an HTTP API, where the harness supplies everything.
//!
//! # Why this is the general case
//!
//! An installed agent CLI arrives with its own tools, its own permission layer
//! and its own session store, so driving one is mostly a matter of staying out
//! of its way. A model reached over an API arrives with none of that. It cannot
//! read a file or run a command; it can only ASK, and something has to carry the
//! request out and hand back what happened. That something is this module plus
//! [`crate::toolbox`].
//!
//! Supporting a subscription CLI is the convenience. This is the floor.
//!
//! # The loop
//!
//! One turn is not one request. The model answers with text, or with calls it
//! wants made; when it calls, the tools are run, the results are appended, and
//! it is asked again. A turn ends when the model stops asking. That loop is
//! bounded by the route's `max_steps` ([`DEFAULT_MAX_STEPS`] unless its entry
//! says otherwise) — a model that calls a failing tool forever would
//! otherwise spend an account rather than fail.
//!
//! # No vendor in the code
//!
//! Endpoint, model and every limit come from the route's entry in admin.json
//! ([`super::models`]); the key from the variable that entry names. Nothing
//! here names a provider, so pointing this at a different one is configuration
//! rather than a change — including at a local model, which is the case that
//! has no bill at all.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use super::{AgentBackend, AgentError, AgentEvent, TurnOutcome};
use crate::toolbox::{ToolOutput, Toolbox};

/// The version header the messages format sends on every request, unless the
/// route's entry names another (`api_version`). The value that format's own
/// documentation gives.
pub const DEFAULT_API_VERSION: &str = "2023-06-01";

/// Which request and reply shape an endpoint speaks.
///
/// The turn loop keeps its conversation in the messages shape either way; a
/// chat-completions endpoint is reached through [`super::openai`], which
/// translates at the edge. Named for the two shapes, not for the companies
/// that published them: many services speak each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireFormat {
    /// `POST .../v1/messages`: `tool_use` blocks, `x-api-key`, and the
    /// `anthropic-version` header.
    #[default]
    Anthropic,
    /// `POST .../chat/completions`: `tool_calls`, `Authorization: Bearer`.
    Openai,
}

impl WireFormat {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
        }
    }

    /// The header this format's own documentation puts the key in.
    #[must_use]
    pub const fn default_auth(self) -> KeyHeader {
        match self {
            Self::Anthropic => KeyHeader::XApiKey,
            Self::Openai => KeyHeader::Bearer,
        }
    }
}

/// Which header carries the key.
///
/// A setting rather than a property of the format: a service that speaks the
/// messages format for tools built around a token reads `Authorization:
/// Bearer` instead of `x-api-key`; which one is the entry's `auth`.
/// Only one is sent, so a service is never handed its key in a header it did
/// not ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyHeader {
    /// `x-api-key: <key>`.
    XApiKey,
    /// `Authorization: Bearer <key>`.
    Bearer,
}

/// How many times the model may call tools within ONE turn, unless the
/// route's entry says otherwise (`max_steps`).
///
/// Bounded because the loop is driven by the model: one that keeps calling a
/// tool that keeps failing would otherwise run until the account is empty. Hit
/// the ceiling and the turn ends saying so, rather than quietly looking finished.
pub const DEFAULT_MAX_STEPS: usize = 40;

/// The reply-length ceiling asked for on each request, unless the route's
/// entry says otherwise (`max_tokens`).
pub const DEFAULT_MAX_TOKENS: u64 = 8_192;

/// How long one request may take, in seconds, unless the route's entry says
/// otherwise (`timeout_secs`).
///
/// Generous because a long reply legitimately takes minutes, but present because
/// a hung connection must not hang the session.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// How many times a request the service turned away as busy (429, 503, 529)
/// is sent again, unless the route's entry says otherwise (`max_retries`).
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// The longest one wait before a retry may be, whatever the service's
/// `Retry-After` asks for — a wait is bounded, not taken on trust.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

/// The request with the messages format's cache breakpoints marked: the last
/// tool, the system prompt, and the last message's last block — at most the
/// four the format allows. A tool loop re-sends the whole conversation every
/// round, so caching it up to the newest message is what makes the next round
/// read the prefix from cache instead of paying for it again.
///
/// Pure, so the shape is tested without a network. A string system prompt or
/// message is turned into one text block, which is the same request.
fn with_cache_marks(body: &Value) -> Value {
    let mark = || json!({"type": "ephemeral"});
    let mut out = body.clone();
    if let Some(last) = out
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .and_then(|tools| tools.last_mut())
    {
        last["cache_control"] = mark();
    }
    if let Some(system) = out.get("system").and_then(Value::as_str) {
        if !system.is_empty() {
            out["system"] = json!([{"type": "text", "text": system, "cache_control": mark()}]);
        }
    }
    if let Some(last) = out
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .and_then(|messages| messages.last_mut())
    {
        match last.get("content").cloned() {
            Some(Value::String(text)) if !text.is_empty() => {
                last["content"] = json!([{"type": "text", "text": text, "cache_control": mark()}]);
            }
            Some(Value::Array(_)) => {
                if let Some(block) = last["content"]
                    .as_array_mut()
                    .and_then(|blocks| blocks.last_mut())
                {
                    block["cache_control"] = mark();
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether a status means "busy, try again" rather than "this request is
/// wrong": rate-limited, unavailable, or overloaded.
fn is_retryable(status: u16) -> bool {
    matches!(status, 429 | 503 | 529)
}

/// How long to wait before retry number `attempt` (0-based): the service's
/// `Retry-After` in seconds when it gave one, else 1, 2, 4… seconds — capped
/// at [`MAX_RETRY_WAIT`] either way.
fn retry_wait(attempt: u32, retry_after: Option<&str>) -> Duration {
    let asked = retry_after
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(Duration::from_secs_f64);
    asked
        .unwrap_or_else(|| Duration::from_secs(1u64 << attempt.min(5)))
        .min(MAX_RETRY_WAIT)
}

/// Where to send a request, and as whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub endpoint: String,
    pub key: String,
    pub model: String,
    pub format: WireFormat,
    pub auth: KeyHeader,
    /// The model this route's sub-agents run on, when it names one: the
    /// same service's cheaper model for delegated work. Blank, a sub-agent
    /// inherits `model`.
    pub sub_model: String,
    /// The names the operator's role table uses (`opus`, `haiku`, …), each
    /// mapped to the model id THIS service knows it by. A service speaking
    /// the messages format for token-based tools documents exactly this
    /// mapping; without it, a role table written once for every agent would
    /// send a name the service does not have.
    pub aliases: std::collections::BTreeMap<String, String>,
    /// The reply-length ceiling asked for on each request.
    pub max_tokens: u64,
    /// How many tool rounds one turn may take.
    pub max_steps: usize,
    /// How long one request may take, in seconds.
    pub timeout_secs: u64,
    /// How many times a busy answer (429, 503, 529) is retried.
    pub max_retries: u32,
    /// Whether the messages format marks its cache breakpoints
    /// ([`with_cache_marks`]); unused by the chat format.
    pub prompt_cache: bool,
    /// The version header the messages format sends; unused by the other.
    pub api_version: String,
    /// The catalogue id this route is chosen by: a configured model's name,
    /// or a provider agent's `<provider>/<agent>` — see [`super::models`].
    pub route: String,
    /// The model's context window in tokens. Past
    /// [`super::compact::THRESHOLD_PERCENT`] of it the older turns are
    /// summarised; zero never compacts on its own.
    pub context_window: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            key: String::new(),
            model: String::new(),
            format: WireFormat::default(),
            auth: WireFormat::default().default_auth(),
            sub_model: String::new(),
            aliases: std::collections::BTreeMap::new(),
            max_tokens: DEFAULT_MAX_TOKENS,
            max_steps: DEFAULT_MAX_STEPS,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            max_retries: DEFAULT_MAX_RETRIES,
            prompt_cache: true,
            api_version: DEFAULT_API_VERSION.to_string(),
            route: String::new(),
            context_window: super::compact::DEFAULT_CONTEXT_WINDOW,
        }
    }
}

impl Settings {
    /// The model id a role-table candidate names on this route, when this
    /// route serves it; `None` when it does not.
    ///
    /// Served: a candidate qualified with this route (`<route>:<id>`), an
    /// alias, or one of the route's own models (`model`, `sub_model`, an
    /// alias's target). A candidate qualified with another agent, or a bare
    /// name this service has not been told of, is not — sending it would
    /// fail the request, which is worse than keeping the model in hand.
    #[must_use]
    pub fn serves(&self, candidate: &str) -> Option<String> {
        let candidate = crate::roles::model_name(candidate).trim();
        if candidate.is_empty() {
            return None;
        }
        // A qualifier is taken only when it names THIS route; a model id
        // may itself hold a `:` (a variant tag), so an unknown prefix is
        // read as part of the id, not as another agent.
        if let Some((agent, model)) = candidate.split_once(':') {
            if agent.trim().eq_ignore_ascii_case(&self.route) {
                let model = model.trim();
                return (!model.is_empty()).then(|| self.alias(model));
            }
            if crate::agent::registry::protocol_of(
                &crate::agent::registry::configured_table(),
                agent,
            )
            .is_some()
                || crate::agent::models::is_api_route(agent.trim())
            {
                return None;
            }
        }
        if let Some(mapped) = self
            .aliases
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(candidate))
            .map(|(_, id)| id.clone())
        {
            return Some(mapped);
        }
        let own = [self.model.as_str(), self.sub_model.as_str()];
        (own.contains(&candidate) || self.aliases.values().any(|id| id == candidate))
            .then(|| candidate.to_string())
    }

    /// A model the operator named outright, through this route's aliases:
    /// `opus` becomes this service's id for it, anything else stands.
    #[must_use]
    pub fn alias(&self, name: &str) -> String {
        let name = name.trim();
        self.aliases
            .iter()
            .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
            .map_or_else(|| name.to_string(), |(_, id)| id.clone())
    }
}

/// A variable's value, trimmed, or `None` when it is unset or blank.
pub(crate) fn non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// One request to the model, and the reply.
///
/// A seam rather than a direct call, so the loop below can be tested against
/// canned replies. The loop is the part with the decisions in it; testing only
/// the parser would leave every one of them unexercised.
pub trait Transport {
    /// Send a request body and return the raw response.
    fn send(&self, body: &Value) -> Result<String, String>;

    /// The same request, reporting reply text as it is generated.
    ///
    /// Returns a reply in the SAME shape as [`Self::send`], so there is one
    /// parser either way — the streaming path rebuilds a whole reply from its
    /// events rather than introducing a second thing to read.
    ///
    /// Defaults to a whole-reply send that reports nothing, which is honest for
    /// a transport that cannot stream: the reply still arrives, it just arrives
    /// all at once.
    fn send_streaming(
        &self,
        body: &Value,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<String, String> {
        let _ = on_text;
        self.send(body)
    }
}

/// Rebuilds one whole reply from the events a streamed response arrives in.
///
/// # Why rebuild instead of consuming the events directly
///
/// The turn loop needs a finished reply: the text, the calls, the stop reason.
/// Streaming delivers those in pieces, and a loop written against the pieces
/// would be a second implementation of the one in [`parse_reply`] — free to
/// disagree with it about what a reply contained. So the pieces are reassembled
/// into the shape the non-streaming endpoint returns, and one parser reads both.
#[derive(Debug, Default)]
pub struct Reassembler {
    /// Whatever arrived after the last complete line.
    partial: String,
    /// Content blocks, by the index the events number them with.
    blocks: Vec<Value>,
    /// A tool call's arguments arrive as JSON in fragments, per block.
    arguments: Vec<String>,
    stop_reason: String,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    /// How many events were understood — see [`Reassembler::saw_events`].
    events: usize,
}

impl Reassembler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the next piece of the stream.
    ///
    /// A chunk is whatever the network handed over and may split a line in half,
    /// so anything after the last newline is held back rather than parsed as a
    /// truncated event.
    pub fn feed(&mut self, chunk: &str, on_text: &mut dyn FnMut(&str)) {
        self.partial.push_str(chunk);
        while let Some(at) = self.partial.find('\n') {
            let line = self.partial[..at].trim_end_matches('\r').to_string();
            self.partial.drain(..=at);
            self.line(&line, on_text);
        }
    }

    fn line(&mut self, line: &str, on_text: &mut dyn FnMut(&str)) {
        // Only the data lines carry the event; `event:` names it again and the
        // blank lines are separators.
        let Some(payload) = line.strip_prefix("data:") else {
            return;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            return;
        }
        let Ok(event): Result<Value, _> = serde_json::from_str(payload) else {
            // A line that does not parse is skipped rather than failing the
            // turn: the rest of the reply is still worth having.
            return;
        };
        self.events += 1;
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let message = event.get("message");
                self.model = message
                    .and_then(|m| m.get("model"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if let Some(usage) = message.and_then(|m| m.get("usage")) {
                    self.take_usage(usage);
                }
            }
            Some("content_block_start") => {
                let index = index_of(&event);
                let block = event.get("content_block").cloned().unwrap_or(json!({}));
                self.put(index, block);
            }
            Some("content_block_delta") => {
                let index = index_of(&event);
                let delta = event.get("delta");
                if let Some(text) = delta.and_then(|d| d.get("text")).and_then(Value::as_str) {
                    on_text(text);
                    if let Some(block) = self.blocks.get_mut(index) {
                        let joined = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                            + text;
                        block["text"] = json!(joined);
                    }
                }
                // A call's arguments arrive as JSON text in fragments; they are
                // only valid once the block ends, so they are held as a string
                // until then.
                if let Some(fragment) = delta
                    .and_then(|d| d.get("partial_json"))
                    .and_then(Value::as_str)
                {
                    if let Some(held) = self.arguments.get_mut(index) {
                        held.push_str(fragment);
                    }
                }
            }
            Some("content_block_stop") => {
                let index = index_of(&event);
                self.close(index);
            }
            Some("message_delta") => {
                if let Some(reason) = event
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = reason.to_string();
                }
                if let Some(usage) = event.get("usage") {
                    self.take_usage(usage);
                }
            }
            _ => {}
        }
    }

    fn put(&mut self, index: usize, block: Value) {
        while self.blocks.len() <= index {
            self.blocks.push(json!({}));
            self.arguments.push(String::new());
        }
        self.blocks[index] = block;
    }

    /// Finish a block: a call's arguments become the JSON they spell out.
    fn close(&mut self, index: usize) {
        let Some(held) = self.arguments.get(index) else {
            return;
        };
        if held.trim().is_empty() {
            return;
        }
        let parsed = serde_json::from_str::<Value>(held).unwrap_or_else(|_| json!({}));
        if let Some(block) = self.blocks.get_mut(index) {
            block["input"] = parsed;
        }
    }

    /// Usage arrives twice — the input side at the start, the output side at the
    /// end — so each field is taken only when the event actually carries it.
    fn take_usage(&mut self, usage: &Value) {
        let count = |key: &str| usage.get(key).and_then(Value::as_u64);
        if let Some(n) = count("input_tokens") {
            self.input_tokens = n;
        }
        if let Some(n) = count("output_tokens") {
            self.output_tokens = n;
        }
        if let Some(n) = count("cache_creation_input_tokens") {
            self.cache_creation_tokens = n;
        }
        if let Some(n) = count("cache_read_input_tokens") {
            self.cache_read_tokens = n;
        }
    }

    /// Whether any event was actually read.
    ///
    /// An endpoint is free to ignore `stream` and answer with an ordinary body.
    /// Without this the reassembly of a non-stream produces an EMPTY reply, and
    /// the turn reports success having said nothing — which is what a model with
    /// nothing to say looks like, so the failure is invisible.
    #[must_use]
    pub const fn saw_events(&self) -> bool {
        self.events > 0
    }

    /// The whole reply, in the shape the non-streaming endpoint returns.
    #[must_use]
    pub fn finish(self) -> Value {
        json!({
            "content": self.blocks,
            "stop_reason": self.stop_reason,
            "model": self.model,
            "usage": {
                "input_tokens": self.input_tokens,
                "output_tokens": self.output_tokens,
                "cache_creation_input_tokens": self.cache_creation_tokens,
                "cache_read_input_tokens": self.cache_read_tokens,
            }
        })
    }
}

/// Which content block an event is about. Absent means the first.
fn index_of(event: &Value) -> usize {
    event
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0)
}

/// A tool the model asked to have run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// What one reply contained.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reply {
    /// Everything the model said, joined.
    pub text: String,
    /// What it wants run before it can continue.
    pub calls: Vec<Call>,
    /// Why it stopped.
    pub stop_reason: String,
    /// The model that actually answered, when the reply names one.
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_tokens: u32,
    pub cache_read_tokens: u32,
}

impl Reply {
    /// Whether the turn continues.
    #[must_use]
    pub fn wants_tools(&self) -> bool {
        !self.calls.is_empty()
    }
}

/// The request to send.
#[must_use]
pub fn request_body(model: &str, system: &str, messages: &[Value], tools: &[Value]) -> Value {
    let mut body = json!({
        "max_tokens": DEFAULT_MAX_TOKENS,
        "messages": messages,
        "tools": tools,
    });
    // Omitted rather than sent empty: an endpoint serving one model rejects a
    // blank name, and a blank system block is noise in every request.
    if !model.is_empty() {
        body["model"] = json!(model);
    }
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    body
}

/// Read one reply.
///
/// An error the SERVICE reported comes back as an error here rather than an
/// empty reply, because a turn that silently answers nothing looks to the
/// operator exactly like a model with nothing to say.
pub fn parse_reply(body: &str) -> Result<Reply, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| format!("the reply did not parse: {e}"))?;

    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the service reported an error and did not say what");
        return Err(message.to_string());
    }

    let mut reply = Reply {
        stop_reason: value
            .get("stop_reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        ..Reply::default()
    };

    if let Some(blocks) = value.get("content").and_then(Value::as_array) {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(said) = block.get("text").and_then(Value::as_str) {
                        reply.text.push_str(said);
                    }
                }
                Some("tool_use") => reply.calls.push(Call {
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input: block.get("input").cloned().unwrap_or_else(|| json!({})),
                }),
                // A block type this build does not know is skipped rather than
                // refused: the wire format gains block types, and one arriving
                // must not end a turn that is otherwise fine.
                _ => {}
            }
        }
    }

    if let Some(usage) = value.get("usage") {
        let count = |key: &str| -> u32 {
            usage
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0)
        };
        reply.input_tokens = count("input_tokens");
        reply.output_tokens = count("output_tokens");
        reply.cache_creation_tokens = count("cache_creation_input_tokens");
        reply.cache_read_tokens = count("cache_read_input_tokens");
    }
    Ok(reply)
}

/// What the model said, as a message to send back with the next request.
#[must_use]
pub fn assistant_message(reply: &Reply) -> Value {
    let mut blocks: Vec<Value> = Vec::new();
    if !reply.text.is_empty() {
        blocks.push(json!({"type": "text", "text": reply.text}));
    }
    for call in &reply.calls {
        blocks.push(json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": call.input,
        }));
    }
    json!({"role": "assistant", "content": blocks})
}

/// What the tools returned, as the message that answers them.
///
/// A failure is marked with `is_error` rather than sent as ordinary text: a
/// model that reads a failure as a result carries on as though the file was
/// written.
///
/// A text answer is sent as the string it always was. An answer with
/// pictures is sent as blocks: its text, then one base64 `image` block per
/// picture, each already inside the toolbox's image bounds.
#[must_use]
pub fn tool_result_message(results: &[(String, ToolOutput, bool)]) -> Value {
    let blocks: Vec<Value> = results
        .iter()
        .map(|(id, output, failed)| {
            let content = if output.images.is_empty() {
                json!(output.text)
            } else {
                let mut parts = vec![json!({"type": "text", "text": output.text})];
                parts.extend(output.images.iter().map(image_block));
                Value::Array(parts)
            };
            json!({
                "type": "tool_result",
                "tool_use_id": id,
                "content": content,
                "is_error": failed,
            })
        })
        .collect();
    json!({"role": "user", "content": blocks})
}

/// One picture as a base64 `image` content block — the shape a tool result
/// carries it in here, and a person's prompt carries it in on this route and
/// on the claude CLI's stream-json input.
#[must_use]
pub fn image_block(image: &crate::toolbox::Image) -> Value {
    json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": image.format().mime(),
            "data": image.base64(),
        }
    })
}

/// A person's prompt as a user message: the plain string it always was, or
/// — with pictures — its text, then one image block per picture.
fn user_prompt(prompt: &str, images: &[crate::toolbox::Image]) -> Value {
    if images.is_empty() {
        return json!({"role": "user", "content": prompt});
    }
    let mut content = vec![json!({"type": "text", "text": prompt})];
    content.extend(images.iter().map(image_block));
    json!({"role": "user", "content": content})
}

/// A model driven over an API, with this harness supplying its tools.
pub struct ApiBackend {
    settings: Settings,
    /// The standing instructions, sent with every request.
    system: String,
    /// The same context, in the shape [`AgentBackend::system_context`] reports.
    ///
    /// Held beside `system` because the trait hands back a slice and a slice
    /// cannot be made from a `String` field. Empty when the session had nothing
    /// to say, matching the other backends: a blank block reads to a model as
    /// an instruction with nothing in it.
    context: Vec<String>,
    /// The conversation so far. Carried across turns so a session is a session.
    messages: Vec<Value>,
    session: String,
    tools: Toolbox,
    /// The models that actually answered, newest last.
    served_by: Vec<String>,
    /// Tripped when the operator asks to stop.
    cancel: Option<crate::runtime::cancel::CancelToken>,
    /// Pictures for the next turn's prompt only.
    images: Vec<crate::toolbox::Image>,
}

impl ApiBackend {
    /// A backend over one workspace.
    #[must_use]
    pub fn new(settings: Settings, workspace: &Path, system: &str, filter_env: bool) -> Self {
        let session = new_session_id();
        let mut tools = Toolbox::new(workspace, filter_env);
        tools.set_session(&session);
        tools.set_api_route(settings.clone());
        Self {
            settings,
            system: system.to_string(),
            context: if system.trim().is_empty() {
                Vec::new()
            } else {
                vec![system.to_string()]
            },
            messages: Vec::new(),
            session,
            tools,
            served_by: Vec::new(),
            cancel: None,
            images: Vec::new(),
        }
    }

    /// Ask this memory engine before each tool runs — see
    /// [`crate::hooks::engine`]. Given by the code that builds a backend for
    /// real work; a backend built without it runs its tools behind the
    /// operator's guards alone.
    #[must_use]
    pub fn with_engine(mut self, engine: Option<crate::hooks::engine::Engine>) -> Self {
        self.tools = self.tools.with_engine(engine);
        self
    }

    /// Announce each file write to the engine's edit check, and record each
    /// read — see `toolbox::code_edits`. Given where the engine is.
    #[must_use]
    pub fn with_code_edits(mut self, edits: Option<crate::toolbox::CodeEdits>) -> Self {
        self.tools = self.tools.with_code_edits(edits);
        self
    }

    /// Offer the model the desktop tools — see `toolbox::desktop`.
    #[must_use]
    pub fn with_desktop(mut self, setup: Option<crate::toolbox::DesktopSetup>) -> Self {
        if let Some(setup) = setup {
            self.tools.enable_desktop(setup);
        }
        self
    }

    /// Run the operator's own pre- and post-tool commands around every call
    /// this model makes — see [`crate::hooks`].
    #[must_use]
    pub fn with_operator_hooks(mut self, hooks: Option<crate::hooks::HookRunner>) -> Self {
        self.tools = self.tools.with_operator_hooks(hooks);
        self
    }

    /// Confine this model's shell commands to `mode` — see [`crate::sandbox`].
    #[must_use]
    pub fn with_sandbox(mut self, policy: crate::sandbox::SandboxPolicy) -> Self {
        self.tools = self.tools.with_sandbox(policy);
        self
    }

    /// Offer the model only `tools`, when given — a user-defined sub-agent's
    /// allow-list. A call to any other tool is refused by the toolbox.
    #[must_use]
    pub fn with_tool_allowlist(mut self, tools: Option<Vec<String>>) -> Self {
        if let Some(tools) = tools {
            self.tools.restrict_tools(tools);
        }
        self
    }

    /// The same backend, one level further down the tree of agents.
    ///
    /// Carried into the toolbox rather than held here: it is the TOOL that has
    /// to refuse a delegation past the limit, because that is where a new agent
    /// would be made.
    #[must_use]
    pub fn at_depth(mut self, depth: u8) -> Self {
        self.tools = self.tools.at_depth(depth);
        self
    }

    /// Attach a checkpoint store so that writes and edits snapshot files before modifying them.
    #[must_use]
    pub fn with_checkpoints(
        mut self,
        checkpoints: std::sync::Arc<std::sync::Mutex<crate::tools::checkpoint::CheckpointStore>>,
    ) -> Self {
        self.tools = self.tools.with_checkpoints(checkpoints);
        self
    }

    /// The real transport for this route, once the key may travel to it.
    ///
    /// Checked on the way out, so no request from this backend — a turn or a
    /// compaction — skips it.
    fn http(&self) -> Result<Http, AgentError> {
        // The key goes only where it cannot be read on the way: https, or
        // plain http to this machine — the rule every other key here is held
        // to.
        super::catalogue::key_may_travel_to(&self.settings.endpoint).map_err(|why| {
            AgentError::new(format!(
                "the request to {} was not sent: {why}",
                self.settings.route
            ))
        })?;
        Ok(Http {
            endpoint: self.settings.endpoint.clone(),
            key: self.settings.key.clone(),
            format: self.settings.format,
            auth: self.settings.auth,
            timeout: Duration::from_secs(self.settings.timeout_secs),
            api_version: self.settings.api_version.clone(),
            retries: self.settings.max_retries,
            prompt_cache: self.settings.prompt_cache,
        })
    }

    /// Summarise all but the last `keep_turns` turns into one message — see
    /// [`super::compact`]. `Ok(None)` when there was nothing older to fold.
    ///
    /// # Errors
    ///
    /// When the summary request fails or comes back empty. The conversation
    /// is left exactly as it was: a failed compaction loses nothing.
    pub fn compact_with(
        &mut self,
        transport: &dyn Transport,
        instructions: &str,
        keep_turns: usize,
    ) -> Result<Option<super::compact::Compacted>, String> {
        use super::compact;
        let Some(cut) = compact::split_point(&self.messages, keep_turns) else {
            return Ok(None);
        };
        let definitions = self.tools.definitions();
        let before = compact::estimate_tokens(&self.system, &self.messages, &definitions);
        let ask =
            compact::summary_request(&compact::transcript(&self.messages[..cut]), instructions);
        let mut body = request_body(
            &self.settings.model,
            compact::SUMMARIZER_SYSTEM,
            &[json!({"role": "user", "content": ask})],
            &[],
        );
        // No tools: the summariser writes, it does not act.
        if let Some(fields) = body.as_object_mut() {
            fields.remove("tools");
        }
        body["max_tokens"] = json!(self.settings.max_tokens);
        let reply = parse_reply(&transport.send(&body)?)?;
        if reply.text.trim().is_empty() {
            return Err("the model returned an empty summary, so nothing was compacted".into());
        }
        if !reply.model.is_empty() && !self.served_by.contains(&reply.model) {
            self.served_by.push(reply.model.clone());
        }
        let kept = self.messages.split_off(cut);
        let kept_len = kept.len();
        let summarised = self.messages.len();
        self.messages = compact::rebuild(&reply.text, kept);
        let after = compact::estimate_tokens(&self.system, &self.messages, &definitions);
        Ok(Some(compact::Compacted {
            summarised,
            kept: kept_len,
            tokens_before: before,
            tokens_after: after,
        }))
    }

    /// Compact before a request that would otherwise near the window.
    ///
    /// A failure is reported and the request goes ahead as it would have:
    /// the service may still take it, and if not it says so itself.
    fn compact_if_near_window(
        &mut self,
        transport: &dyn Transport,
        definitions: &[Value],
        on_event: &mut dyn FnMut(&AgentEvent),
    ) {
        let estimate = super::compact::estimate_tokens(&self.system, &self.messages, definitions);
        if !super::compact::over_threshold(estimate, self.settings.context_window) {
            return;
        }
        // The operator's `pre_compact` commands first, as before `/compact`:
        // told, never asked. What they print is incode's to show.
        for said in self.tools.operator_pre_compact("auto") {
            on_event(&AgentEvent::Notice(format!("[pre_compact hook] {said}")));
        }
        match self.compact_with(transport, "", super::compact::KEEP_TURNS) {
            Ok(Some(done)) => {
                crate::trace!("compact", "{}", done.describe());
                crate::telemetry::compaction("auto", done.tokens_before, done.tokens_after);
                // Shown apart from the reply, and never heard by the
                // transcript or the stop hook: it is the harness speaking.
                on_event(&AgentEvent::Thinking(format!(
                    "[context near the {}-token window] {}",
                    self.settings.context_window,
                    done.describe()
                )));
            }
            Ok(None) => {}
            Err(why) => crate::trace!("compact", "automatic compaction failed: {why}"),
        }
    }

    /// Run a turn against a given transport.
    ///
    /// Separate from [`AgentBackend::run_turn`] so the loop — which holds every
    /// decision worth testing — can be run against canned replies.
    pub fn run_turn_with(
        &mut self,
        transport: &dyn Transport,
        prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError> {
        if self.messages.is_empty() {
            on_event(&AgentEvent::SessionStarted {
                session_id: self.session.clone(),
                model: self.settings.model.clone(),
            });
        }
        let images = std::mem::take(&mut self.images);
        self.messages.push(user_prompt(prompt, &images));

        let definitions = self.tools.definitions();
        let max_steps = self.settings.max_steps;
        for _ in 0..max_steps {
            // BETWEEN steps, which is the only place a turn can be stopped
            // without abandoning a request already in flight or a tool already
            // running. A turn inside a long tool call stops when that call
            // returns — stated rather than implied.
            if self.cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                return Ok(TurnOutcome {
                    session_id: self.session.clone(),
                    stop_reason: "cancelled".to_string(),
                    is_error: false,
                    detail: None,
                    served_by: self.served_by.clone(),
                });
            }
            self.compact_if_near_window(transport, &definitions, on_event);
            let mut body = request_body(
                &self.settings.model,
                &self.system,
                &self.messages,
                &definitions,
            );
            body["max_tokens"] = json!(self.settings.max_tokens);
            // Streamed, so a long reply arrives rather than appearing. The
            // fragments go out as they come; the WHOLE reply is still parsed
            // below by the same parser the non-streaming path uses, so nothing
            // downstream has to know which way it arrived.
            let raw = transport
                .send_streaming(&body, &mut |fragment| {
                    on_event(&AgentEvent::TextDelta(fragment.to_string()));
                })
                .map_err(AgentError::new)?;
            let reply = parse_reply(&raw).map_err(AgentError::new)?;

            if !reply.model.is_empty() && !self.served_by.contains(&reply.model) {
                self.served_by.push(reply.model.clone());
            }
            if !reply.text.is_empty() {
                on_event(&AgentEvent::Text(reply.text.clone()));
            }
            on_event(&AgentEvent::Usage {
                input_tokens: reply.input_tokens,
                output_tokens: reply.output_tokens,
                cache_creation_tokens: reply.cache_creation_tokens,
                cache_read_tokens: reply.cache_read_tokens,
            });

            self.messages.push(assistant_message(&reply));

            if !reply.wants_tools() {
                return Ok(TurnOutcome {
                    session_id: self.session.clone(),
                    stop_reason: reply.stop_reason,
                    is_error: false,
                    detail: None,
                    served_by: self.served_by.clone(),
                });
            }

            let mut results = Vec::with_capacity(reply.calls.len());
            for call in &reply.calls {
                on_event(&AgentEvent::ToolUse {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: call.input.to_string(),
                });
                let (output, failed) = match self.tools.run_call(&call.id, &call.name, &call.input)
                {
                    Ok(out) => (out, false),
                    // Handed BACK to the model rather than ending the turn: a
                    // failed tool is something to recover from, and the model
                    // cannot recover from what it is not told.
                    Err(why) => (ToolOutput::from(why), true),
                };
                // Shown and recorded with a placeholder per picture; the
                // pictures themselves go only to the model.
                on_event(&AgentEvent::ToolResult {
                    id: call.id.clone(),
                    output: output.shown(),
                    is_error: failed,
                });
                results.push((call.id.clone(), output, failed));
            }
            self.messages.push(tool_result_message(&results));
        }

        Ok(TurnOutcome {
            session_id: self.session.clone(),
            stop_reason: "step_limit".to_string(),
            is_error: true,
            detail: Some(format!(
                "the model kept asking for tools past {max_steps} rounds in one turn, so the \
                 turn was stopped. Whatever it was retrying was not working."
            )),
            served_by: self.served_by.clone(),
        })
    }
}

/// The real transport.
#[derive(Clone)]
struct Http {
    endpoint: String,
    key: String,
    format: WireFormat,
    auth: KeyHeader,
    timeout: Duration,
    api_version: String,
    retries: u32,
    prompt_cache: bool,
}

impl Http {
    /// Send the request, sending it again — after a bounded wait — while the
    /// service answers that it is busy and retries remain. Done before any of
    /// the reply is read, so a retried streamed turn cannot repeat text.
    ///
    /// Returns the last response, and how many retries it took.
    async fn post(
        &self,
        client: &reqwest::Client,
        payload: &Value,
    ) -> Result<(reqwest::Response, u32), String> {
        let mut attempt = 0;
        loop {
            let response = self
                .authorised(client, payload)
                .send()
                .await
                .map_err(|e| format!("request failed: {e}"))?;
            if !is_retryable(response.status().as_u16()) || attempt >= self.retries {
                return Ok((response, attempt));
            }
            let wait = retry_wait(
                attempt,
                response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()),
            );
            crate::trace!(
                "api",
                "the model service answered {}; retrying in {:.1}s ({} of {})",
                response.status(),
                wait.as_secs_f64(),
                attempt + 1,
                self.retries
            );
            attempt += 1;
            tokio::time::sleep(wait).await;
        }
    }

    /// A request as the route sends it: the body in its format's shape, and
    /// the key in the ONE header the route names.
    fn authorised(&self, client: &reqwest::Client, payload: &Value) -> reqwest::RequestBuilder {
        let request = client
            .post(&self.endpoint)
            .header("content-type", "application/json");
        let request = match self.auth {
            KeyHeader::XApiKey => request.header("x-api-key", &self.key),
            KeyHeader::Bearer => request.bearer_auth(&self.key),
        };
        match self.format {
            WireFormat::Anthropic if self.prompt_cache => request
                .header("anthropic-version", &self.api_version)
                .json(&with_cache_marks(payload)),
            WireFormat::Anthropic => request
                .header("anthropic-version", &self.api_version)
                .json(payload),
            WireFormat::Openai => request.json(&super::openai::request(payload)),
        }
    }
}

/// What a refused request says: the status, the service's own words, and —
/// when it was busy each time — how often it was tried, so "rate limited
/// once" is not mistaken for "rate limited after four tries".
fn refused(status: reqwest::StatusCode, retried: u32, body: &str) -> String {
    let tries = if retried > 0 {
        format!(" (after {retried} retries)")
    } else {
        String::new()
    };
    format!("the model service answered {status}{tries}: {body}")
}

impl Transport for Http {
    fn send(&self, body: &Value) -> Result<String, String> {
        let http = self.clone();
        let format = self.format;
        let payload = body.clone();
        // On its own thread with its own runtime, matching every other outbound
        // call in this crate: the harness is not async and starting a runtime
        // inside one that is already running panics.
        let handle = std::thread::spawn(move || -> Result<String, String> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("no runtime for the request: {e}"))?;
            runtime.block_on(async move {
                // A model server on this machine is reached directly, never
                // through a proxy (`runtime::http`).
                let client = crate::runtime::http::builder(crate::runtime::http::names_loopback(
                    &http.endpoint,
                ))
                .timeout(http.timeout)
                .build()
                .map_err(|e| format!("client: {e}"))?;
                let (response, retried) = http.post(&client, &payload).await?;
                let status = response.status();
                let text = response
                    .text()
                    .await
                    .map_err(|e| format!("reading the response failed: {e}"))?;
                if !status.is_success() {
                    // The body is kept: a 400 explains itself there, and the
                    // status alone is the category without the reason.
                    return Err(refused(status, retried, &text));
                }
                match format {
                    WireFormat::Anthropic => Ok(text),
                    WireFormat::Openai => super::openai::reply(&text),
                }
            })
        });
        handle
            .join()
            .map_err(|_| "the request thread panicked".to_string())?
    }

    fn send_streaming(
        &self,
        body: &Value,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<String, String> {
        let http = self.clone();
        let format = self.format;
        let mut payload = body.clone();
        payload["stream"] = json!(true);

        // A CHANNEL, because the request runs on its own thread and the callback
        // belongs to this one. The thread sends each fragment as it arrives and
        // this side hands it on; when the thread finishes, the sender drops, the
        // loop below ends, and only then is the result collected.
        let (fragments, arriving) = std::sync::mpsc::channel::<String>();
        let handle = std::thread::spawn(move || -> Result<String, String> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("no runtime for the request: {e}"))?;
            runtime.block_on(async move {
                let client = crate::runtime::http::builder(crate::runtime::http::names_loopback(
                    &http.endpoint,
                ))
                .timeout(http.timeout)
                .build()
                .map_err(|e| format!("client: {e}"))?;
                let (mut response, retried) = http.post(&client, &payload).await?;
                let status = response.status();
                if !status.is_success() {
                    let text = response.text().await.unwrap_or_default();
                    return Err(refused(status, retried, &text));
                }
                if format == WireFormat::Openai {
                    let mut whole = super::openai::ChatStream::new();
                    let mut raw = String::new();
                    while let Some(bytes) = response
                        .chunk()
                        .await
                        .map_err(|e| format!("the reply stopped early: {e}"))?
                    {
                        let text = String::from_utf8_lossy(&bytes);
                        raw.push_str(&text);
                        whole.feed(&text, &mut |fragment| {
                            let _ = fragments.send(fragment.to_string());
                        });
                    }
                    // Not a stream after all: read it as a whole reply.
                    return if whole.saw_events() {
                        whole.finish()
                    } else {
                        super::openai::reply(&raw)
                    };
                }
                let mut whole = Reassembler::new();
                // KEPT AS WELL AS REASSEMBLED. An endpoint may ignore `stream`
                // and answer with an ordinary body; reassembling that produces
                // an empty reply, and an empty reply is indistinguishable from a
                // model with nothing to say. Holding the raw text costs one copy
                // of a reply and is what makes the fallback below possible.
                let mut raw = String::new();
                // `chunk` rather than a stream adaptor: it is on the response
                // itself, so this needs no extra crate to read a body as it
                // arrives.
                while let Some(bytes) = response
                    .chunk()
                    .await
                    .map_err(|e| format!("the reply stopped early: {e}"))?
                {
                    let text = String::from_utf8_lossy(&bytes);
                    raw.push_str(&text);
                    whole.feed(&text, &mut |fragment| {
                        // A send that fails means the reader has gone; the
                        // reassembly still completes, so the turn survives
                        // losing the live display.
                        let _ = fragments.send(fragment.to_string());
                    });
                }
                if whole.saw_events() {
                    Ok(whole.finish().to_string())
                } else {
                    // Not a stream. Hand back what actually arrived, which the
                    // ordinary parser already knows how to read.
                    Ok(raw)
                }
            })
        });

        while let Ok(fragment) = arriving.recv() {
            on_text(&fragment);
        }
        handle
            .join()
            .map_err(|_| "the request thread panicked".to_string())?
    }
}

impl AgentBackend for ApiBackend {
    fn id(&self) -> &str {
        &self.settings.route
    }

    /// Both halves, and enforced more strongly here than on any CLI.
    ///
    /// This route does not hand rules to somebody else and hope: the model's
    /// tools ARE [`crate::toolbox::Toolbox`], and every call passes through
    /// `toolbox.rs:327`, which asks [`crate::guards::Guards::refuse_write`]
    /// before a byte is written. That is PREVENTION at the tool, where a CLI
    /// gets a deny list it applies for itself.
    ///
    /// Sub-agents are refused by construction rather than by a flag — the tool
    /// list is the harness's own and contains nothing that spawns an agent.
    ///
    /// Stated explicitly because the trait default is
    /// [`crate::guards::Enforcement::NOTHING`], and inheriting it here would
    /// have every API-backed task report two guards as unenforced while they
    /// were in fact the most strongly enforced of the three routes.
    fn enforces(&self) -> crate::guards::Enforcement {
        crate::guards::Enforcement::EVERYTHING
    }

    fn run_turn(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError> {
        let transport = self.http()?;
        self.run_turn_with(&transport, prompt, on_event)
    }

    fn compact(&mut self, instructions: &str) -> Option<Result<String, String>> {
        let ran = self
            .http()
            .map_err(|e| e.to_string())
            .and_then(|transport| self.compact_with(&transport, instructions, 0));
        Some(ran.map(|done| match done {
            Some(done) => {
                crate::telemetry::compaction("manual", done.tokens_before, done.tokens_after);
                done.describe()
            }
            None => "Nothing to compact yet: the conversation is empty.".to_string(),
        }))
    }

    fn session_id(&self) -> Option<&str> {
        Some(&self.session)
    }

    fn model(&self) -> Option<&str> {
        // `None` for a blank one, matching the request: an endpoint serving a
        // single model is asked for none, so there is no name to report.
        Some(self.settings.model.as_str()).filter(|m| !m.is_empty())
    }

    fn children(&self) -> Vec<(String, String)> {
        self.tools
            .children()
            .into_iter()
            .map(|c| (c.session, c.title))
            .collect()
    }

    fn offered_tools(&self) -> Vec<String> {
        self.tools.names()
    }

    fn set_watch(&mut self, watch: std::sync::Arc<dyn crate::watch::Watch>) {
        self.tools.set_watch(watch);
    }

    /// Sent as image blocks in the next turn's user message, as a tool
    /// result's pictures already are.
    fn attach_images(&mut self, images: Vec<crate::toolbox::Image>) -> Result<(), String> {
        self.images = images;
        Ok(())
    }

    fn clear_session(&mut self) {
        // The conversation IS the session here, so forgetting it is the whole
        // operation — there is nothing on the far side to tell.
        self.messages.clear();
        self.served_by.clear();
        self.session = new_session_id();
        // The engine is told the NEW conversation from here on, or its cards
        // and backups would be filed under the one the operator just ended.
        self.tools.set_session(&self.session);
    }

    fn set_cancel(&mut self, token: crate::runtime::cancel::CancelToken) {
        self.tools.set_cancel(token.clone());
        self.cancel = Some(token);
    }

    fn set_prompter(&mut self, prompter: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {
        self.tools.set_prompter(prompter);
    }

    fn set_owner(&mut self, owner: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {
        self.tools.set_owner(owner);
    }

    fn share_allowed_writes(&mut self, record: std::sync::Arc<crate::toolbox::AllowedWrites>) {
        self.tools.share_allowed_writes(record);
    }

    fn api_route(&self) -> Option<&Settings> {
        Some(&self.settings)
    }

    fn share_session_tools(&mut self, state: std::sync::Arc<crate::toolbox::SessionTools>) {
        self.tools.share_session(state);
    }

    fn runs_tools_here(&self) -> bool {
        true
    }

    /// The team channel's tools are the toolbox's, so the toolbox is who
    /// speaks.
    fn set_team_identity(&mut self, role: &str) {
        self.tools.set_team_identity(role);
    }

    /// The harness context this route carries, as the one block it was given.
    ///
    /// Reported rather than left to the empty default: this backend HOLDS the
    /// context in `system` and sends it on every request, so answering "none"
    /// would tell a reader the API route drops what the session assembled. The
    /// caller joins the blocks before handing them over, so there is one block
    /// here where a CLI backend keeps several — the count differs, the fact
    /// that the context arrived does not.
    fn system_context(&self) -> &[String] {
        &self.context
    }

    /// The engine hears incode's session id with every call from here on.
    fn set_memory_session(&mut self, session: &str) {
        self.tools.pin_session(session);
    }

    fn end_code_edits(&mut self) {
        self.tools.end_code_edits();
    }

    /// The standing instructions for the following requests, as the one block
    /// the constructor makes of them.
    fn set_system_context(&mut self, blocks: Vec<String>) {
        let joined = blocks
            .iter()
            .map(|b| b.trim())
            .filter(|b| !b.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.context = if joined.is_empty() {
            Vec::new()
        } else {
            vec![joined.clone()]
        };
        self.system = joined;
    }

    /// The session's mode, decided by this route's toolbox for every call —
    /// the same toolbox, and the same decision, as on the CLI route. Before
    /// this, a `plan` session on an API model could still write.
    fn set_permission_mode(&mut self, mode: &str) {
        self.tools
            .set_permission_mode(crate::permissions::PermissionMode::from_agent_mode(mode));
    }

    /// Roots added with `--add-dir`, reachable by this route's toolbox under
    /// the same guards as the workspace.
    fn set_extra_dirs(&mut self, dirs: Vec<std::path::PathBuf>) {
        self.tools.set_extra_dirs(dirs);
    }

    // deny_rules and set_session_id keep the defaults, and the defaults are
    // the truth. The toolbox checks the guards itself rather than handing
    // rules to anyone, and a session cannot be resumed by id because the
    // conversation lives in this process and is not written down. Each is a
    // real limit of this route; a pretend implementation would hide it.

    fn set_models(&mut self, candidates: &[String]) {
        // No fallback chain: one request goes to one model. The FIRST
        // candidate this route serves is taken ([`Settings::serves`]); a role
        // table written for every agent names models this service may not
        // have, and one that serves none leaves the model in hand rather than
        // sending a name the request would fail on.
        if let Some(model) = candidates.iter().find_map(|c| self.settings.serves(c)) {
            self.settings.model = model;
        }
    }

    /// A model the operator named outright (`/model`, `-m`): any id the
    /// service offers, through the route's aliases.
    fn choose_model(&mut self, model: &str) {
        self.settings.model = self.settings.alias(model);
    }
}

/// An id for a conversation this harness owns.
///
/// Ours to invent because, unlike a CLI, the API has no session of its own —
/// the conversation lives in `messages` here.
fn new_session_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("api-{now:x}")
}

#[cfg(test)]
#[path = "api_wire_tests.rs"]
mod wire_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// A transport that answers with whatever the test lined up.
    struct Canned {
        replies: std::cell::RefCell<Vec<String>>,
        sent: std::cell::RefCell<Vec<Value>>,
    }

    impl Canned {
        fn new(replies: &[&str]) -> Self {
            Self {
                replies: std::cell::RefCell::new(
                    replies.iter().rev().map(|s| (*s).to_string()).collect(),
                ),
                sent: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for Canned {
        fn send(&self, body: &Value) -> Result<String, String> {
            self.sent.borrow_mut().push(body.clone());
            self.replies
                .borrow_mut()
                .pop()
                .ok_or_else(|| "the test ran out of replies".to_string())
        }
    }

    fn backend(root: &Path) -> ApiBackend {
        ApiBackend::new(
            Settings {
                endpoint: "http://example.invalid/messages".to_string(),
                key: "<TEST-KEY>".to_string(),
                model: "test-model".to_string(),
                ..Default::default()
            },
            root,
            "be brief",
            true,
        )
    }

    fn said(text: &str) -> String {
        json!({
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 7}
        })
        .to_string()
    }

    fn asked_for(id: &str, name: &str, input: Value) -> String {
        json!({
            "content": [{"type": "tool_use", "id": id, "name": name, "input": input}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 5, "output_tokens": 7}
        })
        .to_string()
    }

    /// **A PROMPT'S PICTURES GO AS IMAGE BLOCKS, ON THAT TURN ONLY**: the
    /// user message holds the text then the picture, in the shape a tool
    /// result's pictures already take; the next turn is plain text again.
    #[test]
    fn a_prompts_pictures_go_as_image_blocks_on_that_turn_only() {
        let dir = tempfile::tempdir().expect("tmp");
        let picture = crate::toolbox::Image::from_base64(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC",
        )
        .expect("a 1x1 png");
        let transport = Canned::new(&[&said("a dot"), &said("still here")]);
        let mut api = backend(dir.path());
        api.attach_images(vec![picture.clone()]).expect("taken");
        api.run_turn_with(&transport, "what is this", &mut |_| {})
            .expect("a turn");
        api.run_turn_with(&transport, "and now", &mut |_| {})
            .expect("a turn");
        let sent = transport.sent.borrow();
        let first = &sent[0]["messages"][0]["content"];
        assert_eq!(first[0], json!({"type": "text", "text": "what is this"}));
        assert_eq!(first[1], image_block(&picture));
        assert_eq!(first[1]["source"]["media_type"], "image/png");
        let later = sent[1]["messages"]
            .as_array()
            .and_then(|all| all.last())
            .expect("the second prompt");
        assert_eq!(later["content"], "and now");
    }

    #[test]
    fn a_plain_answer_ends_the_turn_in_one_request() {
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[&said("here you go")]);
        let mut seen = Vec::new();
        let outcome = backend(dir.path())
            .run_turn_with(&transport, "hello", &mut |e| seen.push(format!("{e:?}")))
            .expect("a turn");
        assert_eq!(outcome.stop_reason, "end_turn");
        assert!(!outcome.is_error);
        assert_eq!(transport.sent.borrow().len(), 1, "one request");
        assert!(
            seen.iter().any(|e| e.contains("here you go")),
            "the text reached the caller: {seen:?}"
        );
    }

    #[test]
    fn a_tool_the_model_asks_for_is_actually_run() {
        // THE POINT OF THE WHOLE MODULE. The model asks; the file must change
        // on disk. Asserting the loop "returned Ok" would pass with no tool run
        // at all.
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[
            &asked_for(
                "call-1",
                crate::toolbox::WRITE,
                json!({"path": "made.txt", "content": "by the model"}),
            ),
            &said("done"),
        ]);
        let outcome = backend(dir.path())
            .run_turn_with(&transport, "make a file", &mut |_| {})
            .expect("a turn");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("made.txt")).expect("the file exists"),
            "by the model",
            "the tool ran for real"
        );
        assert_eq!(outcome.stop_reason, "end_turn");
        assert_eq!(
            transport.sent.borrow().len(),
            2,
            "asked again after the tool"
        );
    }

    #[test]
    fn the_result_is_sent_back_tied_to_the_call_that_asked() {
        // An untied result is one the model cannot match to its own request.
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("r.txt"), "the contents").expect("write");
        let transport = Canned::new(&[
            &asked_for("call-7", crate::toolbox::READ, json!({"path": "r.txt"})),
            &said("read it"),
        ]);
        backend(dir.path())
            .run_turn_with(&transport, "read it", &mut |_| {})
            .expect("a turn");
        let second = &transport.sent.borrow()[1];
        let text = second.to_string();
        assert!(text.contains("tool_result"), "{text}");
        assert!(text.contains("call-7"), "tied to the call: {text}");
        assert!(
            text.contains("the contents"),
            "and carries the output: {text}"
        );
    }

    #[test]
    fn a_failing_tool_is_reported_to_the_model_as_a_failure() {
        // Sent as ordinary content, a failure reads as a result, and the model
        // carries on as though the file had been read.
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[
            &asked_for(
                "call-9",
                crate::toolbox::READ,
                json!({"path": "absent.txt"}),
            ),
            &said("ah"),
        ]);
        let mut errors = 0;
        backend(dir.path())
            .run_turn_with(&transport, "read it", &mut |e| {
                if matches!(e, AgentEvent::ToolResult { is_error: true, .. }) {
                    errors += 1;
                }
            })
            .expect("the turn survives a failed tool");
        assert_eq!(errors, 1, "the caller was told");
        let second = transport.sent.borrow()[1].to_string();
        assert!(second.contains(r#""is_error":true"#), "{second}");
    }

    #[test]
    fn an_unknown_tool_does_not_end_the_turn() {
        // The model can misremember a name. That is a correctable mistake, not
        // a reason to abandon the work.
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[
            &asked_for("call-2", "NoSuchTool", json!({})),
            &said("sorry, retrying"),
        ]);
        let outcome = backend(dir.path())
            .run_turn_with(&transport, "go", &mut |_| {})
            .expect("a turn");
        assert!(!outcome.is_error, "{outcome:?}");
        let second = transport.sent.borrow()[1].to_string();
        assert!(second.contains("no tool called"), "{second}");
    }

    /// **THE SESSION'S MODE GOVERNS AN API MODEL'S WRITES.** The same scripted
    /// model asks to write a file twice: under `plan` the toolbox refuses and
    /// nothing lands; under `acceptEdits` it is written. Before this, the
    /// route kept no mode and wrote under `plan` too.
    #[test]
    fn the_session_mode_decides_whether_an_api_model_may_write() {
        let dir = tempfile::tempdir().expect("tmp");
        let write = asked_for(
            "w1",
            crate::toolbox::WRITE,
            json!({"path": "made.txt", "content": "hello"}),
        );
        let replies = [write.as_str(), &said("done")];

        let mut planning = backend(dir.path());
        AgentBackend::set_permission_mode(&mut planning, "plan");
        planning
            .run_turn_with(&Canned::new(&replies), "write it", &mut |_| {})
            .expect("a turn");
        assert!(
            !dir.path().join("made.txt").exists(),
            "a plan session wrote a file"
        );

        let mut editing = backend(dir.path());
        AgentBackend::set_permission_mode(&mut editing, "acceptEdits");
        editing
            .run_turn_with(&Canned::new(&replies), "write it", &mut |_| {})
            .expect("a turn");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("made.txt")).ok(),
            Some("hello".to_string())
        );
    }

    /// A root added with `--add-dir` is reachable; without it, the same
    /// write outside the workspace is refused.
    #[test]
    fn an_added_root_is_reachable_by_an_api_model() {
        let workspace = tempfile::tempdir().expect("ws");
        let other = tempfile::tempdir().expect("other");
        let target = other.path().join("out.txt");
        let write = asked_for(
            "w1",
            crate::toolbox::WRITE,
            json!({"path": target.to_string_lossy(), "content": "there"}),
        );
        let replies = [write.as_str(), &said("done")];

        let mut confined = backend(workspace.path());
        AgentBackend::set_permission_mode(&mut confined, "acceptEdits");
        confined
            .run_turn_with(&Canned::new(&replies), "write there", &mut |_| {})
            .expect("a turn");
        assert!(!target.exists(), "wrote outside the workspace unasked");

        let mut widened = backend(workspace.path());
        AgentBackend::set_permission_mode(&mut widened, "acceptEdits");
        AgentBackend::set_extra_dirs(&mut widened, vec![other.path().to_path_buf()]);
        widened
            .run_turn_with(&Canned::new(&replies), "write there", &mut |_| {})
            .expect("a turn");
        assert_eq!(
            std::fs::read_to_string(&target).ok(),
            Some("there".to_string())
        );
    }

    #[test]
    fn the_tool_loop_is_bounded() {
        // A model that keeps calling a failing tool would otherwise run until
        // the account is empty.
        let dir = tempfile::tempdir().expect("tmp");
        let forever: Vec<String> = (0..DEFAULT_MAX_STEPS + 5)
            .map(|n| {
                asked_for(
                    &format!("c{n}"),
                    crate::toolbox::READ,
                    json!({"path": "no.txt"}),
                )
            })
            .collect();
        let refs: Vec<&str> = forever.iter().map(String::as_str).collect();
        let transport = Canned::new(&refs);
        let outcome = backend(dir.path())
            .run_turn_with(&transport, "loop", &mut |_| {})
            .expect("a turn");
        assert!(outcome.is_error, "{outcome:?}");
        assert_eq!(outcome.stop_reason, "step_limit");
        assert_eq!(
            transport.sent.borrow().len(),
            DEFAULT_MAX_STEPS,
            "stopped at the cap"
        );
        // The entry's own ceiling is the one kept, and its reply ceiling is
        // the one asked for.
        let transport = Canned::new(&refs);
        let mut tight = backend(dir.path());
        tight.settings.max_steps = 3;
        tight.settings.max_tokens = 1234;
        tight
            .run_turn_with(&transport, "loop", &mut |_| {})
            .expect("a turn");
        assert_eq!(transport.sent.borrow().len(), 3, "the entry's cap");
        assert_eq!(transport.sent.borrow()[0]["max_tokens"], 1234);
        assert!(
            outcome.detail.is_some_and(|d| d.contains("not working")),
            "and says why"
        );
    }

    #[test]
    fn the_tools_are_offered_on_every_request() {
        // A request without them leaves the model unable to do anything at all,
        // which is the failure this whole module exists to fix.
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[&said("hi")]);
        backend(dir.path())
            .run_turn_with(&transport, "hello", &mut |_| {})
            .expect("a turn");
        let sent = &transport.sent.borrow()[0];
        let tools = sent["tools"].as_array().expect("tools were sent");
        assert!(tools.len() >= 12, "only {} tools offered", tools.len());
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&crate::toolbox::READ), "{names:?}");
        assert!(names.contains(&crate::toolbox::BASH), "{names:?}");
    }

    #[test]
    fn the_conversation_is_kept_between_turns() {
        // Otherwise every turn re-sends nothing and the model has no memory of
        // the one before it.
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[&said("one"), &said("two")]);
        let mut agent = backend(dir.path());
        agent
            .run_turn_with(&transport, "first", &mut |_| {})
            .expect("turn one");
        agent
            .run_turn_with(&transport, "second", &mut |_| {})
            .expect("turn two");
        // Against the MESSAGES, not the serialised request. Grepping the whole
        // body passed with the history cleared, because the word it looked for
        // also appears in a tool description — a test that could not fail, and
        // the control run is the only reason that was noticed.
        let sent = transport.sent.borrow();
        let messages = sent[1]["messages"].as_array().expect("messages were sent");
        let said: Vec<&str> = messages
            .iter()
            .filter(|m| m["role"] == "user")
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert!(
            said.contains(&"first"),
            "the first turn must still be in the conversation: {said:?}"
        );
        assert!(said.contains(&"second"), "{said:?}");
    }

    /// **NEAR THE WINDOW, THE OLDER TURNS BECOME A SUMMARY**: the model is
    /// asked for one, with no tools, and the next request carries it in place
    /// of what it summarised.
    #[test]
    fn a_conversation_near_the_window_is_compacted_before_the_request() {
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[&said("one"), &said("THE SUMMARY"), &said("two")]);
        let mut agent = backend(dir.path());
        // Every request is "near" a one-token window.
        agent.settings.context_window = 1;
        agent
            .run_turn_with(&transport, "first", &mut |_| {})
            .expect("turn one");
        let mut notes = Vec::new();
        agent
            .run_turn_with(&transport, "second", &mut |e| {
                if let AgentEvent::Thinking(note) = e {
                    notes.push(note.clone());
                }
            })
            .expect("turn two");

        let sent = transport.sent.borrow();
        assert_eq!(sent.len(), 3, "turn, summary, turn");
        let summary_request = &sent[1];
        assert!(summary_request.get("tools").is_none());
        assert_eq!(
            summary_request["system"],
            json!(super::super::compact::SUMMARIZER_SYSTEM)
        );
        assert!(summary_request["messages"][0]["content"]
            .as_str()
            .expect("text")
            .contains("first"));

        let messages = sent[2]["messages"].as_array().expect("messages");
        let opening = messages[0]["content"].as_str().expect("the summary");
        assert!(opening.contains("THE SUMMARY"), "{opening}");
        let prompts: Vec<&str> = messages
            .iter()
            .filter(|m| m["role"] == "user")
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert!(!prompts.contains(&"first"), "summarised away: {prompts:?}");
        assert_eq!(prompts.last(), Some(&"second"));
        assert_eq!(notes.len(), 1, "the compaction is said once: {notes:?}");
    }

    /// **THE OPERATOR'S `pre_compact` RUNS BEFORE THE ROUTE COMPACTS ON ITS
    /// OWN**, told `trigger: auto`, and what it prints is incode's notice —
    /// before the compaction's own note, so before the summary was asked for.
    #[test]
    fn the_operators_pre_compact_hook_runs_before_an_automatic_compaction() {
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[&said("one"), &said("THE SUMMARY"), &said("two")]);
        // Prints the event it was given on stdin.
        let echo_stdin = if cfg!(windows) { "more" } else { "cat" };
        let mut agent = backend(dir.path()).with_operator_hooks(Some(
            crate::hooks::HookRunner::new(crate::hooks::HookConfig {
                pre_compact: vec![echo_stdin.to_string()],
                ..crate::hooks::HookConfig::default()
            }),
        ));
        agent.settings.context_window = 1;
        agent
            .run_turn_with(&transport, "first", &mut |_| {})
            .expect("turn one");
        let mut shown = Vec::new();
        agent
            .run_turn_with(&transport, "second", &mut |e| match e {
                AgentEvent::Notice(note) => shown.push(format!("notice: {note}")),
                AgentEvent::Thinking(note) => shown.push(format!("thinking: {note}")),
                _ => {}
            })
            .expect("turn two");

        assert_eq!(shown.len(), 2, "{shown:?}");
        assert!(
            shown[0].starts_with("notice: [pre_compact hook]")
                && shown[0].contains("\"trigger\":\"auto\"")
                && shown[0].contains("\"hook_event_name\":\"PreCompact\""),
            "the hook did not run first, or was not told why: {shown:?}"
        );
        assert!(shown[1].starts_with("thinking: [context near"), "{shown:?}");
    }

    #[test]
    fn a_window_of_zero_never_compacts_on_its_own() {
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[&said("one"), &said("two")]);
        let mut agent = backend(dir.path());
        agent.settings.context_window = 0;
        for prompt in ["first", "second"] {
            agent
                .run_turn_with(&transport, prompt, &mut |_| {})
                .expect("turn");
        }
        assert_eq!(transport.sent.borrow().len(), 2);
    }

    /// `/compact <instructions>`: everything is folded, steered by what the
    /// operator asked, and the conversation carries on from the summary.
    #[test]
    fn compacting_by_hand_folds_everything_and_carries_the_instructions() {
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[
            &asked_for("t1", "Read", json!({"path": "missing.rs"})),
            &said("read it"),
            &said("THE SUMMARY"),
            &said("carrying on"),
        ]);
        let mut agent = backend(dir.path());
        agent
            .run_turn_with(&transport, "first", &mut |_| {})
            .expect("turn one");
        let done = agent
            .compact_with(&transport, "keep the file names", 0)
            .expect("compacts")
            .expect("there was something to compact");
        assert_eq!(done.summarised, 4);
        assert_eq!(done.kept, 0);
        assert_eq!(
            agent.messages.len(),
            2,
            "the summary and its acknowledgement"
        );
        agent
            .run_turn_with(&transport, "second", &mut |_| {})
            .expect("turn two");

        let sent = transport.sent.borrow();
        let asked = sent[2]["messages"][0]["content"].as_str().expect("text");
        assert!(asked.contains("keep the file names"));
        assert!(asked.contains("[called Read"));
        let roles: Vec<&str> = sent[3]["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter_map(|m| m["role"].as_str())
            .collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
    }

    #[test]
    fn a_failed_compaction_loses_nothing() {
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[&said("one")]);
        let mut agent = backend(dir.path());
        agent
            .run_turn_with(&transport, "first", &mut |_| {})
            .expect("turn one");
        let before = agent.messages.clone();
        // The canned transport has run out of replies: the request fails.
        assert!(agent.compact_with(&transport, "", 0).is_err());
        assert_eq!(agent.messages, before);
    }

    #[test]
    fn a_service_error_is_reported_rather_than_read_as_an_empty_answer() {
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[
            &json!({"error": {"type": "invalid_request", "message": "model not found"}})
                .to_string(),
        ]);
        let failed = backend(dir.path())
            .run_turn_with(&transport, "hello", &mut |_| {})
            .expect_err("must not look like an empty reply");
        assert!(failed.to_string().contains("model not found"), "{failed}");
    }

    #[test]
    fn an_unknown_block_type_does_not_break_the_reply() {
        // The wire format gains block types; one arriving must not end a turn
        // that is otherwise fine.
        let reply = parse_reply(
            &json!({
                "content": [
                    {"type": "something_new", "data": 1},
                    {"type": "text", "text": "still here"}
                ],
                "stop_reason": "end_turn"
            })
            .to_string(),
        )
        .expect("parses");
        assert_eq!(reply.text, "still here");
    }

    #[test]
    fn usage_is_read_from_the_reply() {
        let reply = parse_reply(
            &json!({
                "content": [],
                "usage": {"input_tokens": 11, "output_tokens": 22,
                          "cache_creation_input_tokens": 33, "cache_read_input_tokens": 44}
            })
            .to_string(),
        )
        .expect("parses");
        assert_eq!(reply.input_tokens, 11);
        assert_eq!(reply.output_tokens, 22);
        assert_eq!(reply.cache_creation_tokens, 33);
        assert_eq!(reply.cache_read_tokens, 44);
    }

    #[test]
    fn the_model_that_answered_is_recorded_not_the_one_asked_for() {
        // A fallback that quietly serves a different model is exactly what the
        // session-started event cannot be trusted for.
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[&json!({
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "model": "the-one-that-actually-ran"
        })
        .to_string()]);
        let outcome = backend(dir.path())
            .run_turn_with(&transport, "hello", &mut |_| {})
            .expect("a turn");
        assert_eq!(outcome.served_by, vec!["the-one-that-actually-ran"]);
    }

    #[test]
    fn asking_to_stop_ends_the_turn_between_steps() {
        // A tool loop that ignores the stop is one the operator cannot get out
        // of short of killing the process.
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[
            &asked_for("c1", crate::toolbox::READ, json!({"path": "no.txt"})),
            &said("carrying on"),
        ]);
        let mut agent = backend(dir.path());
        let token = crate::runtime::cancel::CancelToken::new();
        token.cancel();
        agent.set_cancel(token);
        let outcome = agent
            .run_turn_with(&transport, "go", &mut |_| {})
            .expect("a turn");
        assert_eq!(outcome.stop_reason, "cancelled");
        assert_eq!(
            transport.sent.borrow().len(),
            0,
            "nothing was asked after the stop"
        );
    }

    #[test]
    fn an_uncancelled_turn_still_runs() {
        // The control: the check above must not stop a turn nobody stopped.
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[&said("fine")]);
        let mut agent = backend(dir.path());
        agent.set_cancel(crate::runtime::cancel::CancelToken::new());
        let outcome = agent
            .run_turn_with(&transport, "go", &mut |_| {})
            .expect("a turn");
        assert_eq!(outcome.stop_reason, "end_turn");
        assert_eq!(transport.sent.borrow().len(), 1);
    }

    #[test]
    fn clearing_the_session_forgets_the_conversation() {
        // The conversation IS the session on this route, so a clear that left
        // the messages would keep answering from a conversation the operator
        // ended.
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[&said("one"), &said("two")]);
        let mut agent = backend(dir.path());
        agent
            .run_turn_with(&transport, "remember this", &mut |_| {})
            .expect("turn");
        let before = agent.session_id().map(str::to_string);
        agent.clear_session();
        agent
            .run_turn_with(&transport, "fresh", &mut |_| {})
            .expect("turn");
        let sent = transport.sent.borrow();
        let said_now: Vec<&str> = sent[1]["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert!(
            !said_now.contains(&"remember this"),
            "the old conversation survived: {said_now:?}"
        );
        assert_ne!(
            agent.session_id().map(str::to_string),
            before,
            "and it is a new session"
        );
    }

    /// One streamed reply: some text, then a call whose arguments arrive in
    /// fragments. Written as the wire sends it, so the reassembler is tested
    /// against the shape it will actually meet.
    const STREAM: &str = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"model":"m-1","usage":{"input_tokens":10,"cache_read_input_tokens":4}}}"#,
        "\n\n",
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Loo"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"king"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"c-1","name":"Read","input":{}}}"#,
        "\n\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_stop","index":1}"#,
        "\n\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#,
        "\n\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );

    fn reassemble(stream: &str, chunk_size: usize) -> (Reply, String) {
        let mut whole = Reassembler::new();
        let mut seen = String::new();
        let bytes: Vec<char> = stream.chars().collect();
        for piece in bytes.chunks(chunk_size) {
            let text: String = piece.iter().collect();
            whole.feed(&text, &mut |fragment| seen.push_str(fragment));
        }
        let rebuilt = whole.finish().to_string();
        (
            parse_reply(&rebuilt).expect("the rebuilt reply parses"),
            seen,
        )
    }

    #[test]
    fn a_streamed_reply_rebuilds_what_a_whole_one_would_have_said() {
        // THE INVARIANT. A streamed turn and a whole one must produce the same
        // Reply, or the loop behaves differently depending on how the bytes
        // happened to arrive.
        let (reply, _) = reassemble(STREAM, STREAM.len());
        assert_eq!(reply.text, "Looking");
        assert_eq!(reply.stop_reason, "tool_use");
        assert_eq!(reply.model, "m-1");
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.calls);
        assert_eq!(reply.calls[0].id, "c-1");
        assert_eq!(reply.calls[0].name, "Read");
        assert_eq!(
            reply.calls[0].input["path"], "a.rs",
            "the arguments were rebuilt"
        );
    }

    #[test]
    fn a_chunk_that_splits_a_line_in_half_loses_nothing() {
        // The network decides where a chunk ends, not the sender. Parsing what
        // arrives as though it were whole lines is THE streaming bug, and it
        // shows up as an occasional dropped event rather than a failure.
        let whole = reassemble(STREAM, STREAM.len()).0;
        for size in [1usize, 3, 7, 64, 512] {
            let (reply, _) = reassemble(STREAM, size);
            assert_eq!(reply, whole, "rebuilt differently at chunk size {size}");
        }
    }

    #[test]
    fn text_is_reported_as_it_arrives_not_only_at_the_end() {
        // The whole point of streaming: a reply that arrives rather than
        // appearing. Asserted on the fragments, since the finished text is the
        // same either way.
        let mut whole = Reassembler::new();
        let mut fragments: Vec<String> = Vec::new();
        whole.feed(STREAM, &mut |f| fragments.push(f.to_string()));
        assert_eq!(fragments, vec!["Loo", "king"], "in order, as sent");
    }

    #[test]
    fn a_calls_arguments_are_not_read_until_they_are_complete() {
        // Half of a JSON object is not a smaller JSON object. Parsing early
        // gives a call with arguments that were never sent.
        let mut whole = Reassembler::new();
        let half = STREAM
            .find(r#"\"a.rs\"}"#)
            .expect("the sample splits the arguments");
        whole.feed(&STREAM[..half], &mut |_| {});
        let reply = parse_reply(&whole.finish().to_string()).expect("parses");
        assert_eq!(
            reply.calls.len(),
            1,
            "the call is known even though its arguments are not"
        );
        assert_eq!(
            reply.calls[0].input,
            json!({}),
            "and its arguments stay empty rather than half-read"
        );
    }

    #[test]
    fn usage_is_taken_from_both_ends_of_the_stream() {
        // Input tokens arrive with the first event and output tokens with the
        // last; taking either alone reports half the turn.
        let (reply, _) = reassemble(STREAM, 16);
        assert_eq!(reply.input_tokens, 10);
        assert_eq!(reply.output_tokens, 7);
        assert_eq!(reply.cache_read_tokens, 4);
    }

    #[test]
    fn a_line_that_does_not_parse_does_not_lose_the_rest() {
        // A stream gains event types, and one this build cannot read must not
        // cost the reply it was sent beside.
        let mut whole = Reassembler::new();
        whole.feed(
            "data: {not json at all}\n\ndata: {\"type\":\"unknown_event\"}\n\n",
            &mut |_| {},
        );
        whole.feed(STREAM, &mut |_| {});
        let reply = parse_reply(&whole.finish().to_string()).expect("parses");
        assert_eq!(reply.text, "Looking");
    }

    #[test]
    fn an_endpoint_that_ignores_the_stream_flag_is_noticed() {
        // FOUND LIVE, not by these tests. The request asks for a stream; an
        // endpoint free to ignore it answers with an ordinary body, and feeding
        // that to the reassembler yields NO events and an empty reply — which
        // reads exactly like a model with nothing to say. The turn then reports
        // success having done nothing.
        //
        // The unit tests missed it because the canned transport uses the DEFAULT
        // `send_streaming`, which falls back to `send` — so the streaming
        // transport was never given a non-stream to read.
        let mut whole = Reassembler::new();
        whole.feed(&said("an ordinary reply"), &mut |_| {});
        assert!(
            !whole.saw_events(),
            "an ordinary JSON body carries no events"
        );
        let rebuilt = whole.finish();
        assert!(
            rebuilt["content"].as_array().is_some_and(Vec::is_empty),
            "and reassembling it would lose the reply entirely: {rebuilt}"
        );
    }

    #[test]
    fn a_real_stream_is_still_recognised_as_one() {
        // The control for the check above: it must not report "not a stream"
        // for the streams it exists to read, or every streamed reply would take
        // the fallback path.
        let mut whole = Reassembler::new();
        whole.feed(STREAM, &mut |_| {});
        assert!(whole.saw_events());
    }

    #[test]
    fn a_transport_that_cannot_stream_still_answers() {
        // The default reports nothing and returns the whole reply, which is the
        // truth for an endpoint that does not stream.
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[&said("all at once")]);
        let mut deltas = 0;
        backend(dir.path())
            .run_turn_with(&transport, "hello", &mut |e| {
                if matches!(e, AgentEvent::TextDelta(_)) {
                    deltas += 1;
                }
            })
            .expect("a turn");
        assert_eq!(deltas, 0, "nothing was claimed to have streamed");
    }

    #[test]
    fn no_endpoint_or_key_means_no_backend() {
        // Guessing either is worse than saying it is not configured.
        assert!(non_empty("INCODE_A_VARIABLE_THAT_IS_NOT_SET").is_none());
    }

    #[test]
    fn a_blank_model_is_left_out_of_the_request() {
        // An endpoint that serves exactly one model rejects a blank name.
        let body = request_body("", "sys", &[], &[]);
        assert!(body.get("model").is_none(), "{body}");
        let named = request_body("m", "sys", &[], &[]);
        assert_eq!(named["model"], "m");
    }

    /// §3 test 5. **THE ENGINE IS TOLD WHICH CONVERSATION, WHERE, AND WHAT.**
    /// The event on its stdin carries the tool, its command, the workspace as
    /// an absolute path, and THIS backend's session id — and after a clear,
    /// the new one.
    #[test]
    fn the_engine_is_told_the_call_the_workspace_and_this_session() {
        use crate::hooks::engine::fake::{self, Reply};
        let (dir, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        let bin = fake::write(engine.path(), &[("pre-tool", Reply::exit(0))]);
        let mut agent = backend(dir.path())
            .with_engine(Some(fake::engine(bin).capped(Duration::from_secs(30))));
        let transport = Canned::new(&[
            &asked_for(
                "c1",
                crate::toolbox::BASH,
                json!({"command": "echo guarded"}),
            ),
            &said("done"),
            &asked_for("c2", crate::toolbox::BASH, json!({"command": "echo again"})),
            &said("done"),
        ]);
        agent
            .run_turn_with(&transport, "run it", &mut |_| {})
            .expect("a turn");
        let first = agent.session_id().map(str::to_string).expect("a session");
        agent.clear_session();
        agent
            .run_turn_with(&transport, "run it again", &mut |_| {})
            .expect("a turn");
        let second = agent.session_id().map(str::to_string).expect("a session");

        let events = fake::events(engine.path(), "pre-tool");
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0]["tool_name"], "Bash");
        assert_eq!(events[0]["tool_input"]["command"], "echo guarded");
        let cwd = events[0]["cwd"].as_str().expect("cwd");
        assert!(Path::new(cwd).is_absolute(), "{cwd}");
        assert_eq!(events[0]["session_id"], json!(first));
        assert_eq!(
            events[1]["session_id"],
            json!(second),
            "the cleared session's id"
        );
        assert_ne!(first, second);
    }

    #[test]
    fn the_session_id_is_stable_across_turns() {
        // It is what a session record points at; a new one per turn would make
        // every turn look like a new conversation.
        let dir = tempfile::tempdir().expect("tmp");
        let transport = Canned::new(&[&said("one"), &said("two")]);
        let mut agent = backend(dir.path());
        let first = agent
            .run_turn_with(&transport, "a", &mut |_| {})
            .expect("turn")
            .session_id;
        let second = agent
            .run_turn_with(&transport, "b", &mut |_| {})
            .expect("turn")
            .session_id;
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }
}
