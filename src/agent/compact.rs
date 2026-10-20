//! Keeping an API route's conversation inside its context window.
//!
//! An installed agent CLI compacts its own transcript, inside its own process.
//! The API route has no such process: the conversation IS
//! [`super::api::ApiBackend`]'s `messages`, re-sent whole on every request, so
//! a long session grows until the service refuses it. Nobody else is going to
//! shorten it.
//!
//! # What compaction does
//!
//! The older part of the conversation is handed to the same model with one
//! job — summarise it so the work can continue — and replaced by that summary
//! as a single user message. The most recent turns are kept verbatim, because
//! they are what the model is in the middle of.
//!
//! # Where the conversation may be cut
//!
//! A `tool_use` block and the `tool_result` that answers it travel as a pair:
//! the assistant message asking, then the user message answering. A request
//! holding one without the other is refused by the service. So a cut is made
//! only before an assistant message or before a person's prompt — never before
//! a message of tool results, whose calls would be left behind in the summary.
//! [`split_point`] is the one place that decision is made, and it is tested.

use serde_json::{json, Value};

/// The window assumed for a route whose entry names none. A route that knows
/// better says so in admin.json (`context_window`).
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

/// Compact once the estimate passes this share of the window, in percent —
/// leaving room for the reply and for the turn's next tool results.
pub const THRESHOLD_PERCENT: u64 = 80;

/// How many of the most recent turns are kept verbatim.
pub const KEEP_TURNS: usize = 2;

/// Rough characters per token. An estimate, not a count: it decides WHEN to
/// compact, and erring early costs a summary, never a refused request.
const CHARS_PER_TOKEN: u64 = 4;

/// What one picture is counted as. Its base64 is not what it costs.
const IMAGE_TOKENS: u64 = 1_600;

/// How much of one tool call's input, or one result, the summariser is shown.
const MAX_CALL_CHARS: usize = 600;
const MAX_RESULT_CHARS: usize = 2_000;

/// How the summary opens, so a reader of the conversation — the model, or a
/// person reading the request — knows what it is looking at.
pub const SUMMARY_PREFIX: &str = "[Earlier conversation, compacted by incode. A summary of \
what was said and done follows; the most recent turns come after it verbatim.]";

/// The assistant's side after the summary, keeping the roles alternating when
/// the kept turns open with a person's prompt.
const ACK: &str =
    "Understood. I have the summary of the earlier conversation and will continue from it.";

/// The summariser's standing instructions.
pub const SUMMARIZER_SYSTEM: &str = "You compact a coding agent's conversation so it can \
continue in a smaller context. Write a dense summary that another instance of the agent can \
pick up from without the original. Keep: the user's goals and every explicit instruction or \
constraint; decisions made and why; files read, created or changed (with paths) and what \
changed; commands run and their important results, including errors not yet fixed; open \
questions; and what was about to happen next. Drop: pleasantries, repeated output, and tool \
output that no longer matters. Do not invent anything. Do not address the user. Write the \
summary only.";

/// A rough count of the tokens a request holds.
#[must_use]
pub fn estimate_tokens(system: &str, messages: &[Value], tools: &[Value]) -> u64 {
    let chars = system.len() as u64
        + messages.iter().map(chars_in).sum::<u64>()
        + tools.iter().map(chars_in).sum::<u64>();
    chars.div_ceil(CHARS_PER_TOKEN)
}

fn chars_in(value: &Value) -> u64 {
    match value {
        Value::String(s) => s.len() as u64,
        Value::Array(items) => items.iter().map(chars_in).sum(),
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("image") {
                return IMAGE_TOKENS * CHARS_PER_TOKEN;
            }
            map.iter()
                .map(|(key, v)| key.len() as u64 + chars_in(v))
                .sum()
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => 4,
    }
}

/// Whether `estimate` tokens is past the point to compact in a `window`.
/// A window of zero means the route asked never to compact on its own.
#[must_use]
pub fn over_threshold(estimate: u64, window: u64) -> bool {
    window > 0 && estimate.saturating_mul(100) >= window.saturating_mul(THRESHOLD_PERCENT)
}

fn role(message: &Value) -> &str {
    message.get("role").and_then(Value::as_str).unwrap_or("")
}

/// A user message carrying tool results: the second half of a call's pair.
fn is_tool_results(message: &Value) -> bool {
    role(message) == "user"
        && message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            })
}

/// A person's prompt: the start of a turn.
fn is_prompt(message: &Value) -> bool {
    role(message) == "user" && !is_tool_results(message)
}

/// Whether the conversation may be cut just before `index` without parting a
/// call from its result.
#[must_use]
pub fn is_cut_point(messages: &[Value], index: usize) -> bool {
    messages
        .get(index)
        .is_some_and(|m| role(m) == "assistant" || is_prompt(m))
}

/// Where to cut: everything before the returned index is summarised, the rest
/// is kept. `None` when there is nothing older to summarise.
///
/// The last `keep_turns` turns are kept whole. When those are the whole
/// conversation — one long turn of tool calls — the cut falls inside it, at
/// the first assistant message from the halfway point on, so even a single
/// turn that outgrew the window can be shortened. Zero keeps nothing.
#[must_use]
pub fn split_point(messages: &[Value], keep_turns: usize) -> Option<usize> {
    let prompts: Vec<usize> = (0..messages.len())
        .filter(|&i| is_prompt(&messages[i]))
        .collect();
    let cut = if keep_turns == 0 {
        messages.len()
    } else if prompts.len() > keep_turns {
        prompts[prompts.len() - keep_turns]
    } else {
        0
    };
    if cut > 0 {
        return Some(cut);
    }
    let half = (messages.len() / 2).max(1);
    (half..messages.len()).find(|&i| is_cut_point(messages, i))
}

/// The conversation after compaction: the summary, then what was kept.
#[must_use]
pub fn rebuild(summary: &str, kept: Vec<Value>) -> Vec<Value> {
    let mut out = vec![json!({
        "role": "user",
        "content": format!("{SUMMARY_PREFIX}\n\n{}", summary.trim()),
    })];
    if kept.first().is_none_or(|m| role(m) != "assistant") {
        out.push(json!({"role": "assistant", "content": [{"type": "text", "text": ACK}]}));
    }
    out.extend(kept);
    out
}

/// The older messages as plain text for the summariser.
#[must_use]
pub fn transcript(messages: &[Value]) -> String {
    let mut out = String::new();
    for message in messages {
        let speaker = if role(message) == "assistant" {
            "Assistant"
        } else if is_tool_results(message) {
            "Tool results"
        } else {
            "User"
        };
        out.push_str(&format!("## {speaker}\n"));
        match message.get("content") {
            Some(Value::String(text)) => out.push_str(text),
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    out.push_str(&block_text(block));
                    out.push('\n');
                }
            }
            _ => {}
        }
        out.push_str("\n\n");
    }
    out
}

fn block_text(block: &Value) -> String {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Some("tool_use") => format!(
            "[called {} with {}]",
            block.get("name").and_then(Value::as_str).unwrap_or("?"),
            clip(
                &block.get("input").map(Value::to_string).unwrap_or_default(),
                MAX_CALL_CHARS
            )
        ),
        Some("tool_result") => {
            let failed = block.get("is_error").and_then(Value::as_bool) == Some(true);
            let content = match block.get("content") {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Array(parts)) => {
                    parts.iter().map(block_text).collect::<Vec<_>>().join("\n")
                }
                _ => String::new(),
            };
            format!(
                "[{}: {}]",
                if failed { "failed" } else { "result" },
                clip(&content, MAX_RESULT_CHARS)
            )
        }
        Some("image") => "[image]".to_string(),
        _ => String::new(),
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} more characters)", &text[..end], text.len() - end)
}

/// What the summariser is asked, with the operator's own focus when given
/// (`/compact <instructions>`).
#[must_use]
pub fn summary_request(transcript: &str, instructions: &str) -> String {
    let focus = instructions.trim();
    let focus = if focus.is_empty() {
        String::new()
    } else {
        format!("\n\nThe user asked the summary to focus on: {focus}")
    };
    format!(
        "Summarise this conversation so far, following your instructions.{focus}\n\n\
         <conversation>\n{transcript}</conversation>"
    )
}

/// What one compaction did, for the person who asked or the trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    /// Messages folded into the summary.
    pub summarised: usize,
    /// Messages kept verbatim after it.
    pub kept: usize,
    pub tokens_before: u64,
    pub tokens_after: u64,
}

impl Compacted {
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "Compacted {} earlier message(s) into a summary, keeping the last {} verbatim \
             (~{} → ~{} tokens).",
            self.summarised, self.kept, self.tokens_before, self.tokens_after
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(text: &str) -> Value {
        json!({"role": "user", "content": text})
    }

    fn says(text: &str) -> Value {
        json!({"role": "assistant", "content": [{"type": "text", "text": text}]})
    }

    fn calls(id: &str) -> Value {
        json!({"role": "assistant", "content": [
            {"type": "text", "text": "looking"},
            {"type": "tool_use", "id": id, "name": "Read", "input": {"path": "a.rs"}},
        ]})
    }

    fn answers(id: &str) -> Value {
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": "fn main() {}", "is_error": false},
        ]})
    }

    /// Every `tool_use` in `messages` is answered by the very next message,
    /// and every `tool_result` answers the message before it.
    fn pairs_intact(messages: &[Value]) -> bool {
        let ids = |m: &Value, kind: &str, key: &str| -> Vec<String> {
            m.get("content")
                .and_then(Value::as_array)
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some(kind))
                        .filter_map(|b| b.get(key).and_then(Value::as_str).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        messages.iter().enumerate().all(|(i, m)| {
            let asked = ids(m, "tool_use", "id");
            let answered = ids(m, "tool_result", "tool_use_id");
            let asked_ok = asked.is_empty()
                || messages
                    .get(i + 1)
                    .is_some_and(|next| ids(next, "tool_result", "tool_use_id") == asked);
            let answered_ok = answered.is_empty()
                || i.checked_sub(1)
                    .and_then(|p| messages.get(p))
                    .is_some_and(|prev| ids(prev, "tool_use", "id") == answered);
            asked_ok && answered_ok
        })
    }

    fn roles_alternate(messages: &[Value]) -> bool {
        messages.windows(2).all(|w| role(&w[0]) != role(&w[1]))
    }

    fn conversation() -> Vec<Value> {
        vec![
            prompt("first"),
            calls("t1"),
            answers("t1"),
            says("done one"),
            prompt("second"),
            says("done two"),
            prompt("third"),
            calls("t2"),
            answers("t2"),
            says("done three"),
        ]
    }

    #[test]
    fn the_last_turns_are_kept_whole() {
        let messages = conversation();
        // Two turns kept: the cut is at the second-to-last prompt.
        assert_eq!(split_point(&messages, 2), Some(4));
        assert_eq!(split_point(&messages, 1), Some(6));
    }

    #[test]
    fn a_cut_never_parts_a_call_from_its_result() {
        let messages = conversation();
        for keep in 0..4 {
            if let Some(cut) = split_point(&messages, keep) {
                assert!(cut == messages.len() || is_cut_point(&messages, cut));
                let rebuilt = rebuild("summary", messages[cut..].to_vec());
                assert!(pairs_intact(&rebuilt), "keep {keep}: {rebuilt:?}");
                assert!(roles_alternate(&rebuilt), "keep {keep}: {rebuilt:?}");
            }
        }
    }

    #[test]
    fn a_message_of_results_is_never_a_cut_point() {
        let messages = conversation();
        assert!(!is_cut_point(&messages, 2));
        assert!(!is_cut_point(&messages, 8));
        assert!(is_cut_point(&messages, 1));
        assert!(is_cut_point(&messages, 4));
        assert!(!is_cut_point(&messages, messages.len()));
    }

    #[test]
    fn one_long_turn_is_cut_inside_at_an_assistant_message() {
        let messages = vec![
            prompt("fix it"),
            calls("a"),
            answers("a"),
            calls("b"),
            answers("b"),
            calls("c"),
            answers("c"),
        ];
        let cut = split_point(&messages, KEEP_TURNS).expect("a long turn can still be cut");
        assert_eq!(role(&messages[cut]), "assistant");
        assert!(cut > 0 && cut < messages.len());
        let rebuilt = rebuild("summary", messages[cut..].to_vec());
        assert!(pairs_intact(&rebuilt));
        assert!(roles_alternate(&rebuilt));
        // Mid-turn the conversation still ends on the results the next
        // request answers.
        assert!(is_tool_results(rebuilt.last().expect("kept")));
    }

    #[test]
    fn nothing_to_summarise_is_said_so() {
        assert_eq!(split_point(&[], KEEP_TURNS), None);
        assert_eq!(split_point(&[prompt("hi")], KEEP_TURNS), None);
    }

    #[test]
    fn keeping_nothing_summarises_everything() {
        let messages = conversation();
        assert_eq!(split_point(&messages, 0), Some(messages.len()));
        let rebuilt = rebuild("all of it", Vec::new());
        assert_eq!(rebuilt.len(), 2);
        assert_eq!(role(&rebuilt[1]), "assistant");
    }

    #[test]
    fn the_summary_opens_the_conversation_as_a_prompt() {
        let rebuilt = rebuild("  the gist  ", vec![prompt("next")]);
        let text = rebuilt[0]["content"].as_str().expect("a string");
        assert!(text.starts_with(SUMMARY_PREFIX));
        assert!(text.ends_with("the gist"));
        // The summary counts as a turn of its own when compacted again.
        assert!(is_prompt(&rebuilt[0]));
    }

    #[test]
    fn the_threshold_is_a_share_of_the_window() {
        assert!(!over_threshold(79_999, 100_000));
        assert!(over_threshold(80_000, 100_000));
        assert!(!over_threshold(u64::MAX / 2, 0), "zero never compacts");
    }

    #[test]
    fn a_picture_is_counted_as_a_picture_not_as_its_base64() {
        let big = "x".repeat(400_000);
        let with_image = vec![json!({"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "data": big}},
        ]})];
        assert!(estimate_tokens("", &with_image, &[]) < 2_000);
        let text = vec![prompt(&"y".repeat(4_000))];
        assert!(estimate_tokens("", &text, &[]) >= 1_000);
    }

    #[test]
    fn the_transcript_names_calls_and_results() {
        let text = transcript(&conversation()[..4]);
        assert!(text.contains("## User\nfirst"));
        assert!(text.contains("[called Read with {\"path\":\"a.rs\"}]"));
        assert!(text.contains("## Tool results"));
        assert!(text.contains("[result: fn main() {}]"));
    }

    #[test]
    fn a_long_result_is_clipped_on_a_character_boundary() {
        let clipped = clip(&"é".repeat(2_000), 3);
        assert!(clipped.starts_with('é'));
        assert!(clipped.contains("more characters"));
    }
}
