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
//! bounded by [`MAX_STEPS`] — a model that calls a failing tool forever would
//! otherwise spend an account rather than fail.
//!
//! # No vendor in the code
//!
//! Endpoint, key and model all come from the environment. Nothing here names a
//! provider, so pointing this at a different one is configuration rather than a
//! change — including at a local model, which is the case that has no bill at
//! all.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use super::{AgentBackend, AgentError, AgentEvent, TurnOutcome};
use crate::toolbox::{ToolOutput, Toolbox};

/// Where requests go. The full URL of the messages endpoint.
pub const API_URL_ENV: &str = "INCODE_API_URL";
/// The credential, read from the environment and never stored by this program.
pub const API_KEY_ENV: &str = "INCODE_API_KEY";
/// Which model to ask for.
pub const API_MODEL_ENV: &str = "INCODE_API_MODEL";

/// How many times the model may call tools within ONE turn.
///
/// Bounded because the loop is driven by the model: one that keeps calling a
/// tool that keeps failing would otherwise run until the account is empty. Hit
/// the ceiling and the turn ends saying so, rather than quietly looking finished.
const MAX_STEPS: usize = 40;

/// The reply-length ceiling asked for on each request.
const MAX_TOKENS: u64 = 8_192;

/// How long one request may take.
///
/// Generous because a long reply legitimately takes minutes, but present because
/// a hung connection must not hang the session.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Where to send a request, and as whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub endpoint: String,
    pub key: String,
    pub model: String,
}

impl Settings {
    /// Read the settings from the environment.
    ///
    /// `None` when the endpoint or the key is missing, because those cannot be
    /// guessed. A missing MODEL is different: some endpoints serve exactly one,
    /// and demanding a name there would refuse a working setup.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Self::from_lookup(&non_empty)
    }

    /// The same rule as [`Self::from_env`], reading variables through `get`.
    ///
    /// Split out so the agent catalogue lists this route by the one rule that
    /// decides whether it can be driven, and a test can supply the variables
    /// without changing the process environment. `get` answers a variable's
    /// trimmed, non-empty value, as [`non_empty`] does.
    #[must_use]
    pub(crate) fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        let endpoint = get(API_URL_ENV)?;
        let key = get(API_KEY_ENV)?;
        Some(Self {
            endpoint,
            key,
            model: get(API_MODEL_ENV).unwrap_or_default(),
        })
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
        "max_tokens": MAX_TOKENS,
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
        for _ in 0..MAX_STEPS {
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
            let body = request_body(
                &self.settings.model,
                &self.system,
                &self.messages,
                &definitions,
            );
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
                "the model kept asking for tools past {MAX_STEPS} rounds in one turn, so the \
                 turn was stopped. Whatever it was retrying was not working."
            )),
            served_by: self.served_by.clone(),
        })
    }
}

/// The real transport.
struct Http {
    endpoint: String,
    key: String,
}

impl Transport for Http {
    fn send(&self, body: &Value) -> Result<String, String> {
        let endpoint = self.endpoint.clone();
        let key = self.key.clone();
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
                let client = reqwest::Client::builder()
                    .timeout(REQUEST_TIMEOUT)
                    .build()
                    .map_err(|e| format!("client: {e}"))?;
                let response = client
                    .post(&endpoint)
                    .header("content-type", "application/json")
                    // Both spellings, because which one an endpoint wants is a
                    // property of the endpoint and the operator picks that.
                    .header("authorization", format!("Bearer {key}"))
                    .header("x-api-key", &key)
                    .json(&payload)
                    .send()
                    .await
                    .map_err(|e| format!("request failed: {e}"))?;
                let status = response.status();
                let text = response
                    .text()
                    .await
                    .map_err(|e| format!("reading the response failed: {e}"))?;
                if !status.is_success() {
                    // The body is kept: a 400 explains itself there, and the
                    // status alone is the category without the reason.
                    return Err(format!("the model service answered {status}: {text}"));
                }
                Ok(text)
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
        let endpoint = self.endpoint.clone();
        let key = self.key.clone();
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
                let client = reqwest::Client::builder()
                    .timeout(REQUEST_TIMEOUT)
                    .build()
                    .map_err(|e| format!("client: {e}"))?;
                let mut response = client
                    .post(&endpoint)
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {key}"))
                    .header("x-api-key", &key)
                    .json(&payload)
                    .send()
                    .await
                    .map_err(|e| format!("request failed: {e}"))?;
                let status = response.status();
                if !status.is_success() {
                    let text = response.text().await.unwrap_or_default();
                    return Err(format!("the model service answered {status}: {text}"));
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
        crate::agent::registry::API_ROUTE
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
        let transport = Http {
            endpoint: self.settings.endpoint.clone(),
            key: self.settings.key.clone(),
        };
        self.run_turn_with(&transport, prompt, on_event)
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

    fn share_session_tools(&mut self, state: std::sync::Arc<crate::toolbox::SessionTools>) {
        self.tools.share_session(state);
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

    // set_extra_dirs, deny_rules, set_permission_mode and set_session_id keep
    // the defaults, and the defaults are the truth. The toolbox works in one
    // workspace, it checks the guards itself rather than handing rules to
    // anyone, permission tiers belong to a CLI that has them, and a session
    // cannot be resumed by id because the conversation lives in this process
    // and is not written down. Each of those is a real limit of this route; a
    // pretend implementation would hide it.

    fn set_models(&mut self, candidates: &[String]) {
        // No fallback chain: one request goes to one model, and a caller that
        // wants a second tried must ask again. Taking the first is honest about
        // that; pretending to hold a chain would not be.
        if let Some(first) = candidates.first() {
            self.settings.model.clone_from(first);
        }
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

    #[test]
    fn the_tool_loop_is_bounded() {
        // A model that keeps calling a failing tool would otherwise run until
        // the account is empty.
        let dir = tempfile::tempdir().expect("tmp");
        let forever: Vec<String> = (0..MAX_STEPS + 5)
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
            MAX_STEPS,
            "stopped at the cap"
        );
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
