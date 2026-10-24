//! The CLI's `--output-format stream-json` lines, and the harness events each
//! one becomes.
//!
//! The wire shapes below were taken from real runs of CLI 2.1.229, not from
//! documentation.

use serde::Deserialize;

use crate::agent::{AgentEvent, TurnOutcome};

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
    // No `user` variant: those lines carry the results of tool calls, which
    // are incode's own and were shown by incode when it ran them.
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
    /// A `thinking_delta`'s text. Measured on 2.1.283: EMPTY unless the run
    /// asked for `--thinking-display summarized` (the default display,
    /// `updates`, sends only `estimated_tokens`).
    #[serde(default)]
    thinking: String,
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

/// One block of a message on the wire.
///
/// Only text is read. The CLI also reports the tool calls it forwarded and
/// their results, but every tool it has is incode's own, run by incode — which
/// shows each call and its result itself, from its own toolbox (see
/// [`crate::tool_bridge`]). Reading the CLI's copy as well would show every call
/// twice, and would take the CLI's word for what incode did.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum WireContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
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
    /// Per request. Measured on a two-request turn: this held ONE entry, the
    /// last request's, while the fields above were the sum of both.
    #[serde(default)]
    iterations: Vec<WireIteration>,
}

/// One request's tokens, as `usage.iterations` reports it.
#[derive(Debug, Default, Deserialize)]
struct WireIteration {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

/// How full the context window was when the turn ended, when the result
/// reports both halves.
///
/// `used` is the LAST request's tokens — input, both cache kinds and its
/// output, which is what the next request starts from. The turn's top-level
/// usage cannot stand in for it: it sums every request, counting the same
/// cached prefix once per request.
///
/// `size` is the `contextWindow` the CLI put on the result. With more than one
/// model listed, it is the window of the model given the most input — the one
/// that held the conversation; a side model sees a fragment of it.
fn context_of(
    usage: &WireUsage,
    model_usage: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Option<AgentEvent> {
    let last = usage.iterations.last()?;
    let input_of = |entry: &serde_json::Value| -> u64 {
        [
            "inputTokens",
            "cacheReadInputTokens",
            "cacheCreationInputTokens",
        ]
        .iter()
        .filter_map(|key| entry.get(*key).and_then(serde_json::Value::as_u64))
        .sum()
    };
    let size = model_usage
        .values()
        .filter_map(|entry| {
            let window = entry.get("contextWindow")?.as_u64()?;
            (window > 0).then_some((input_of(entry), window))
        })
        .max_by_key(|(input, _)| *input)
        .map(|(_, window)| window)?;
    Some(AgentEvent::Context {
        used: last.input_tokens
            + last.cache_read_input_tokens
            + last.cache_creation_input_tokens
            + last.output_tokens,
        size,
    })
}

// ---------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------

/// Map one wire line to zero or more harness events.
///
/// Split out from the process plumbing so it can be tested against captured
/// output without spawning anything.
///
/// Called by `consume` (`turn.rs`) for every line of a turn that is not blank.
pub(super) fn map_line(line: &str, outcome: &mut Option<TurnOutcome>) -> Vec<AgentEvent> {
    // A line we cannot model is data we don't understand yet, never a reason to
    // abort the turn — the CLI adds event types over time.
    let Ok(evt) = serde_json::from_str::<CliEvent>(line) else {
        return Vec::new();
    };

    match evt {
        CliEvent::System(s) => session_started(s).into_iter().collect(),
        CliEvent::StreamEvent(e) => stream_delta(e).into_iter().collect(),
        CliEvent::Assistant(a) => assistant_text(a),
        CliEvent::RateLimit(r) => vec![quota(r.rate_limit_info)],
        CliEvent::Result(r) => result_events(*r, outcome),
        CliEvent::Other => Vec::new(),
    }
}

/// Whether this is the `init` line and it says incode's tool server failed
/// to start.
///
/// The CLI connects its MCP servers before it starts the session, and the
/// `init` line lists each with a status. Measured (Claude Code 2.1, Windows):
/// `{"name":"incode","status":"connected"}` normally;
/// `{"name":"incode","status":"failed"}` under a write-restricted token that
/// stops the CLI starting any child with piped stdio (#68), after which the
/// model runs with no tools until it gives up. Only `failed` counts: any
/// other status, or no list at all, is left to the end-of-turn check
/// (`tool_bridge::require_tools`).
///
/// Called by `consume` (`turn.rs`) for every line, before [`map_line`].
pub(super) fn incode_server_failed(line: &str) -> bool {
    if !line.contains("\"init\"") {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    if value.get("type").and_then(serde_json::Value::as_str) != Some("system")
        || value.get("subtype").and_then(serde_json::Value::as_str) != Some("init")
    {
        return false;
    }
    value
        .get("mcp_servers")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .any(|server| {
            server.get("name").and_then(serde_json::Value::as_str)
                == Some(crate::tool_bridge::CLIENT_KEY)
                && server.get("status").and_then(serde_json::Value::as_str) == Some("failed")
        })
}

/// The session's start, from the `init` line. Every other `system` subtype —
/// the hook events that precede `init`, among others — says nothing.
///
/// Called by [`map_line`].
fn session_started(s: SystemEvent) -> Option<AgentEvent> {
    if s.subtype != "init" {
        return None;
    }
    Some(AgentEvent::SessionStarted {
        session_id: s.session_id.unwrap_or_default(),
        model: s.model.unwrap_or_else(|| "unknown".to_string()),
    })
}

/// Text or thinking as it is generated, from a partial-message frame: only a
/// non-empty `text_delta` or `thinking_delta` of a `content_block_delta`.
///
/// Called by [`map_line`].
fn stream_delta(e: StreamEvent) -> Option<AgentEvent> {
    // Subagent output is not the main conversation's text — the same rule
    // `assistant_text` applies to whole blocks.
    if e.parent_tool_use_id.is_some() || e.event.event_type != "content_block_delta" {
        return None;
    }
    let delta = e.event.delta?;
    if delta.delta_type == "text_delta" && !delta.text.is_empty() {
        Some(AgentEvent::TextDelta(delta.text))
    } else if delta.delta_type == "thinking_delta" && !delta.thinking.is_empty() {
        Some(AgentEvent::Thinking(delta.thinking))
    } else {
        None
    }
}

/// Each text block of a whole assistant message, in order.
///
/// Called by [`map_line`].
fn assistant_text(a: AssistantMsg) -> Vec<AgentEvent> {
    // Subagent output is not the main conversation's text.
    if a.parent_tool_use_id.is_some() {
        return Vec::new();
    }
    a.message
        .content
        .into_iter()
        .filter_map(|block| match block {
            WireContentBlock::Text { text } => Some(AgentEvent::Text(text)),
            WireContentBlock::Other => None,
        })
        .collect()
}

/// The subscription quota a `rate_limit_event` reports.
///
/// Called by [`map_line`].
fn quota(info: RateLimitInfo) -> AgentEvent {
    AgentEvent::Quota {
        status: info.status,
        kind: info.rate_limit_type,
        resets_at: info.resets_at,
        using_overage: info.is_using_overage,
    }
}

/// The token counts and the context of a `result` line, with the turn's
/// outcome written to `outcome`.
///
/// Called by [`map_line`].
fn result_events(r: ResultEvent, outcome: &mut Option<TurnOutcome>) -> Vec<AgentEvent> {
    let mut events = vec![AgentEvent::Usage {
        input_tokens: r.usage.input_tokens,
        output_tokens: r.usage.output_tokens,
        cache_creation_tokens: r.usage.cache_creation_input_tokens,
        cache_read_tokens: r.usage.cache_read_input_tokens,
    }];
    if let Some(context) = context_of(&r.usage, &r.model_usage) {
        events.push(context);
    }
    // NOTE: the result line also carries a `result` field holding the
    // final assembled text. It repeats what the assistant content
    // blocks already delivered, so emitting both duplicates every
    // reply. The incremental stream is the single source of truth.
    let detail = failure_detail(r.is_error, r.result, r.subtype);
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
    events
}

/// What the agent said about a failed turn: its category and its sentence,
/// whichever it gave, trimmed.
///
/// Only when the turn FAILED. On success `result` is a copy of the reply
/// that already streamed, and carrying it would duplicate every answer.
///
/// Called by [`result_events`].
fn failure_detail(
    is_error: bool,
    result: Option<String>,
    subtype: Option<String>,
) -> Option<String> {
    if !is_error {
        return None;
    }
    let said = result.unwrap_or_default();
    let kind = subtype.unwrap_or_default();
    let joined = match (kind.trim(), said.trim()) {
        ("", "") => String::new(),
        (k, "") => k.to_string(),
        ("", t) => t.to_string(),
        (k, t) => format!("{k}: {t}"),
    };
    (!joined.is_empty()).then_some(joined)
}
