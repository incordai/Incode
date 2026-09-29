//! The OpenAI chat-completions wire format, translated at the edge.
//!
//! # Why translate rather than run a second loop
//!
//! The API route keeps its conversation in ONE shape — the messages format of
//! [`super::api`] — and the turn loop, the parser and every test of them are
//! written against it. A service that speaks chat completions (a router, a
//! vendor's pay-as-you-go endpoint, the owner's own API) is reached by turning
//! the request into its shape on the way out and its reply back on the way in.
//! A second loop written against the second format would be free to disagree
//! with the first about what a turn is; a translation cannot.
//!
//! # What does not carry over
//!
//! - A tool result's pictures: a `tool` message holds text only, so they
//!   follow in one `user` message after the results, said to be theirs.
//! - `is_error` on a tool result: there is no field for it, so the text is
//!   marked instead. A model that reads a failure as a result carries on as
//!   though the file was written.

use serde_json::{json, Value};

/// A request in the messages shape, as a chat-completions request.
#[must_use]
pub fn request(body: &Value) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = body.get("system").and_then(Value::as_str) {
        if !system.is_empty() {
            messages.push(json!({"role": "system", "content": system}));
        }
    }
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        messages.extend(message_out(message));
    }
    let mut out = json!({ "messages": messages });
    for key in ["model", "max_tokens", "stream"] {
        if let Some(value) = body.get(key) {
            out[key] = value.clone();
        }
    }
    // Left out when there are none: an empty list is refused by some services
    // as a malformed request rather than read as "no tools".
    let tools: Vec<Value> = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(tool_out)
        .collect();
    if !tools.is_empty() {
        out["tools"] = Value::Array(tools);
    }
    out
}

/// One tool definition as a chat-completions function.
fn tool_out(tool: &Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.get("name").cloned().unwrap_or(Value::Null),
            "description": tool.get("description").cloned().unwrap_or(json!("")),
            "parameters": tool
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"})),
        }
    })
}

/// One message in the messages shape, as the chat-completions messages it
/// becomes: a person's message may carry tool results, which are messages of
/// their own there.
fn message_out(message: &Value) -> Vec<Value> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user");
    let content = message.get("content").unwrap_or(&Value::Null);
    if let Some(text) = content.as_str() {
        return vec![json!({"role": role, "content": text})];
    }
    let blocks = content.as_array().map_or(&[][..], Vec::as_slice);
    if role == "assistant" {
        return vec![assistant_out(blocks)];
    }
    let mut out: Vec<Value> = Vec::new();
    let mut parts: Vec<Value> = Vec::new();
    let mut result_pictures: Vec<Value> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_result") => {
                let (text, pictures) = result_content(block.get("content"));
                let failed = block.get("is_error").and_then(Value::as_bool) == Some(true);
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": block.get("tool_use_id").cloned().unwrap_or(json!("")),
                    "content": if failed { format!("ERROR: {text}") } else { text },
                }));
                result_pictures.extend(pictures);
            }
            Some("text") => parts.push(json!({
                "type": "text",
                "text": block.get("text").cloned().unwrap_or(json!("")),
            })),
            Some("image") => parts.extend(image_out(block)),
            _ => {}
        }
    }
    if !result_pictures.is_empty() {
        parts.insert(
            0,
            json!({"type": "text", "text": "The pictures the tool calls above returned:"}),
        );
        parts.splice(1..1, result_pictures);
    }
    if !parts.is_empty() {
        out.push(json!({"role": "user", "content": parts}));
    }
    out
}

/// The model's own earlier reply: its text, and the calls it made.
fn assistant_out(blocks: &[Value]) -> Value {
    let mut text = String::new();
    let mut calls: Vec<Value> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => calls.push(json!({
                "id": block.get("id").cloned().unwrap_or(json!("")),
                "type": "function",
                "function": {
                    "name": block.get("name").cloned().unwrap_or(json!("")),
                    "arguments": block
                        .get("input")
                        .map_or_else(|| "{}".to_string(), Value::to_string),
                },
            })),
            _ => {}
        }
    }
    let mut out = json!({
        "role": "assistant",
        "content": if text.is_empty() && !calls.is_empty() { Value::Null } else { json!(text) },
    });
    if !calls.is_empty() {
        out["tool_calls"] = Value::Array(calls);
    }
    out
}

/// A tool result's content: its text, and its pictures as image parts.
fn result_content(content: Option<&Value>) -> (String, Vec<Value>) {
    match content {
        Some(Value::String(text)) => (text.clone(), Vec::new()),
        Some(Value::Array(blocks)) => {
            let mut text = String::new();
            let mut pictures = Vec::new();
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        text.push_str(block.get("text").and_then(Value::as_str).unwrap_or(""));
                    }
                    Some("image") => pictures.extend(image_out(block)),
                    _ => {}
                }
            }
            (text, pictures)
        }
        _ => (String::new(), Vec::new()),
    }
}

/// A base64 image block as an `image_url` part carrying a data URL.
fn image_out(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    let mime = source.get("media_type").and_then(Value::as_str)?;
    let data = source.get("data").and_then(Value::as_str)?;
    Some(json!({
        "type": "image_url",
        "image_url": {"url": format!("data:{mime};base64,{data}")},
    }))
}

/// A chat-completions reply body, in the messages shape the parser reads.
///
/// A body carrying `error` is handed back as it is: the parser already reports
/// the service's own message from it.
///
/// # Errors
///
/// When the body is not JSON, or is neither an error nor a completion.
pub fn reply(body: &str) -> Result<String, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| format!("the reply did not parse: {e}"))?;
    if value.get("error").is_some() {
        return Ok(body.to_string());
    }
    Ok(to_messages_shape(&value)?.to_string())
}

/// One completion, in the messages shape.
fn to_messages_shape(completion: &Value) -> Result<Value, String> {
    let choice = completion
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .ok_or("the reply is not a chat completion: it has no choices")?;
    let message = choice.get("message").unwrap_or(&Value::Null);
    let mut blocks: Vec<Value> = Vec::new();
    let text = match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        // Some services send the content as parts even in a reply.
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    };
    if !text.is_empty() {
        blocks.push(json!({"type": "text", "text": text}));
    }
    let calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    for call in calls {
        let function = call.get("function").unwrap_or(&Value::Null);
        let input = match function.get("arguments") {
            // The arguments are JSON written as a string. A string that is not
            // JSON is passed on as it came, so the tool's own schema check
            // tells the model what was wrong rather than a guessed `{}` running.
            Some(Value::String(raw)) if raw.trim().is_empty() => json!({}),
            Some(Value::String(raw)) => {
                serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.clone()))
            }
            Some(other) => other.clone(),
            None => json!({}),
        };
        blocks.push(json!({
            "type": "tool_use",
            "id": call.get("id").cloned().unwrap_or(json!("")),
            "name": function.get("name").cloned().unwrap_or(json!("")),
            "input": input,
        }));
    }
    let finish = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    let stop_reason = if calls.is_empty() {
        match finish {
            "length" => "max_tokens",
            "stop" | "" => "end_turn",
            other => other,
        }
    } else {
        "tool_use"
    };
    let usage = completion.get("usage").unwrap_or(&Value::Null);
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    // A chat completion counts cached prompt tokens INSIDE the prompt count;
    // the messages shape counts them apart, so they are taken out once here.
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Ok(json!({
        "content": blocks,
        "stop_reason": stop_reason,
        "model": completion.get("model").cloned().unwrap_or(json!("")),
        "usage": {
            "input_tokens": count("prompt_tokens").saturating_sub(cached),
            "output_tokens": count("completion_tokens"),
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": cached,
        }
    }))
}

/// Rebuilds one completion from a streamed chat-completions response, then
/// reads it as [`reply`] reads a whole one — one converter for both.
#[derive(Debug, Default)]
pub struct ChatStream {
    /// Whatever arrived after the last complete line.
    partial: String,
    text: String,
    /// Each call by the index the stream numbers it with: id, name, arguments.
    calls: Vec<(String, String, String)>,
    finish: String,
    model: String,
    usage: Option<Value>,
    /// An error the service sent in the middle of the stream.
    error: Option<Value>,
    events: usize,
}

impl ChatStream {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the next piece of the stream, reporting text as it arrives. A
    /// piece may split a line, so what follows the last newline is held.
    pub fn feed(&mut self, chunk: &str, on_text: &mut dyn FnMut(&str)) {
        self.partial.push_str(chunk);
        while let Some(at) = self.partial.find('\n') {
            let line = self.partial[..at].trim_end_matches('\r').to_string();
            self.partial.drain(..=at);
            self.line(&line, on_text);
        }
    }

    fn line(&mut self, line: &str, on_text: &mut dyn FnMut(&str)) {
        let Some(payload) = line.strip_prefix("data:") else {
            return;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            return;
        }
        let Ok(event): Result<Value, _> = serde_json::from_str(payload) else {
            return;
        };
        self.events += 1;
        if let Some(error) = event.get("error") {
            self.error = Some(error.clone());
            return;
        }
        if let Some(model) = event.get("model").and_then(Value::as_str) {
            if !model.is_empty() {
                self.model = model.to_string();
            }
        }
        if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage.clone());
        }
        let Some(choice) = event
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = reason.to_string();
        }
        let Some(delta) = choice.get("delta") else {
            return;
        };
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                on_text(text);
                self.text.push_str(text);
            }
        }
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(self.calls.len().saturating_sub(1));
            while self.calls.len() <= index {
                self.calls
                    .push((String::new(), String::new(), String::new()));
            }
            let held = &mut self.calls[index];
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                held.0 = id.to_string();
            }
            let function = call.get("function");
            if let Some(name) = function.and_then(|f| f.get("name")).and_then(Value::as_str) {
                held.1.push_str(name);
            }
            if let Some(args) = function
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
            {
                held.2.push_str(args);
            }
        }
    }

    /// Whether any event was read — see `api::Reassembler::saw_events`.
    #[must_use]
    pub const fn saw_events(&self) -> bool {
        self.events > 0
    }

    /// The whole reply, in the messages shape.
    ///
    /// # Errors
    ///
    /// Never for a stream that carried events; kept fallible because it is
    /// the same conversion [`reply`] makes.
    pub fn finish(self) -> Result<String, String> {
        if let Some(error) = self.error {
            return Ok(json!({ "error": error }).to_string());
        }
        let calls: Vec<Value> = self
            .calls
            .into_iter()
            .filter(|(_, name, _)| !name.is_empty())
            .map(|(id, name, arguments)| {
                json!({"id": id, "type": "function",
                       "function": {"name": name, "arguments": arguments}})
            })
            .collect();
        let mut message = json!({"role": "assistant", "content": self.text});
        if !calls.is_empty() {
            message["tool_calls"] = Value::Array(calls);
        }
        let completion = json!({
            "model": self.model,
            "choices": [{"message": message, "finish_reason": self.finish}],
            "usage": self.usage.unwrap_or(Value::Null),
        });
        Ok(to_messages_shape(&completion)?.to_string())
    }
}

#[cfg(test)]
#[path = "openai_tests.rs"]
mod tests;
