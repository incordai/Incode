//! What every agent did, written down as it happens.
//!
//! # Why this exists
//!
//! The backend has parsed [`AgentEvent::ToolUse`] and [`AgentEvent::ToolResult`]
//! for a long time, and the worker threw both away on a `_ => {}` arm. A run
//! therefore recorded whether each task PASSED and nothing whatever about what
//! the agent did to get there — which tools it reached for, what they returned,
//! what it was told when one refused. The only durable record was
//! [`crate::runner::WorkJournal`], and that writes one note per FINISHED task,
//! after the fact.
//!
//! So an agent could take twenty actions inside a task and leave no trace of
//! nineteen of them. That is the "doing things blindly" problem, and nothing
//! about it needed new plumbing: the events were already arriving at a callback
//! that dropped them.
//!
//! # Shape: one JSONL file per task
//!
//! Append-only, one JSON object per line, at `.incode/agents/<task-id>.jsonl`.
//! Line-per-event rather than one document because a run that is killed
//! mid-task must still leave a readable record — a partially-written JSON array
//! parses as nothing, while a partially-written JSONL file parses as every line
//! that finished.
//!
//! # What is NOT promised
//!
//! **These files are not redacted.** A tool result can contain whatever the tool
//! read, and this module makes no attempt to detect secrets in it — content
//! sniffing for credentials is false-negative-prone, and a filter that catches
//! most secrets invites treating the output as safe to move somewhere it is not.
//! The transcript is therefore treated exactly like the rest of `.incode/`:
//! local workspace state, never uploaded anywhere by this module. Anything that
//! later ships a transcript off the machine owns that decision and the redaction
//! it requires.

use crate::agent::AgentEvent;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where a run's per-agent transcripts live, relative to the workspace.
pub const DIR: &str = ".incode/agents";

/// The most of any single recorded field that reaches the file.
///
/// A tool result can be a whole file. Recording it whole turns a long run into
/// a transcript larger than the repository it describes, so each field is
/// clipped and the clip is marked rather than silently applied — a reader who
/// cannot tell truncation from completion cannot trust either.
const MAX_FIELD_CHARS: usize = 2_000;

/// Clip to [`MAX_FIELD_CHARS`], marking the cut.
///
/// Counts CHARACTERS, not bytes, and this is correctness rather than taste:
/// slicing a `&str` at a non-boundary panics, so a byte-wise clip would take
/// down the run on the first multi-byte character that happened to straddle the
/// limit.
fn clip(text: &str) -> String {
    let mut out: String = text.chars().take(MAX_FIELD_CHARS).collect();
    if out.chars().count() < text.chars().count() {
        out.push_str(" …[clipped]");
    }
    out
}

/// A task id as a filename that cannot escape [`DIR`].
///
/// Task ids are planner-derived (`d1.1`), so they are usually already safe. They
/// are not GUARANTEED safe, and a `..` or a separator arriving in one would
/// otherwise write outside the directory. Anything that is not alphanumeric,
/// `-`, `.` or `_` becomes `_`, and a leading dot is escaped so `..` cannot
/// survive.
fn safe_id(id: &str) -> String {
    let mut out: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.starts_with('.') {
        out.insert(0, '_');
    }
    if out.is_empty() {
        out.push_str("unnamed");
    }
    out
}

/// The JSON line for one event, or `None` for events not worth a line.
///
/// [`AgentEvent::TextDelta`] is deliberately dropped: the enum's own
/// documentation records that it is emitted BESIDE [`AgentEvent::Text`] when
/// partial messages are on, so recording both writes every reply into the file
/// twice.
#[must_use]
pub fn line_for(event: &AgentEvent) -> Option<serde_json::Value> {
    let value = match event {
        AgentEvent::SessionStarted { session_id, model } => serde_json::json!({
            "kind": "session",
            "session_id": session_id,
            "model_asked": model,
        }),
        AgentEvent::Text(text) => serde_json::json!({
            "kind": "said",
            "text": clip(text),
        }),
        AgentEvent::ToolUse { id, name, input } => serde_json::json!({
            "kind": "tool_use",
            "id": id,
            "tool": name,
            "input": clip(input),
        }),
        AgentEvent::ToolResult {
            id,
            output,
            is_error,
        } => serde_json::json!({
            "kind": "tool_result",
            "id": id,
            "is_error": is_error,
            "output": clip(output),
        }),
        // Reasoning is not the work's record: the reply and the calls are.
        AgentEvent::TextDelta(_)
        | AgentEvent::Thinking(_)
        | AgentEvent::Quota { .. }
        | AgentEvent::Usage { .. }
        | AgentEvent::Context { .. } => return None,
    };
    Some(value)
}

/// An open, append-only record of one task's agent activity.
pub struct Transcript {
    path: PathBuf,
    task_id: String,
    role: String,
    /// Which agent produced these lines.
    ///
    /// Task and role say what the work WAS; they do not say who did it. Two
    /// agents can hold the same role in one run, and a retry after a failover
    /// is a different agent doing the same task — so without this, a record
    /// cannot answer "who did this" for exactly the cases where it matters.
    instance: String,
}

impl Transcript {
    /// Open (creating if needed) the transcript for one task.
    ///
    /// Does no IO itself — the directory and file are created on the first
    /// [`Self::record`], so a task whose agent emits nothing leaves no empty
    /// file behind.
    #[must_use]
    pub fn for_task(workspace: &Path, task_id: &str, role: &str) -> Self {
        Self {
            path: workspace
                .join(DIR)
                .join(format!("{}.jsonl", safe_id(task_id))),
            task_id: task_id.to_string(),
            role: role.to_string(),
            instance: String::new(),
        }
    }

    /// Name the agent whose actions these are.
    #[must_use]
    pub fn by(mut self, instance: &str) -> Self {
        self.instance = instance.to_string();
        self
    }

    /// Where this transcript is written.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record what the agent was TOLD, before it does anything.
    ///
    /// The event stream carries only what the agent did and said, so a record
    /// built from it alone answers "what happened" and never "what was it
    /// asked". Those come apart exactly where it matters: a briefing carrying
    /// another task's findings (see [`crate::runner::handoff_block`]) is
    /// invisible in the events, so whether one agent's finding actually reached
    /// the next could not be established after the fact from the record.
    ///
    /// Written as its own `briefed` line rather than folded into the first
    /// `said` line, so a reader can tell the harness's words from the agent's.
    pub fn opened(&mut self, instruction: &str) -> bool {
        self.write(serde_json::json!({
            "kind": "briefed",
            "instruction": clip(instruction),
        }))
    }

    /// Append one event.
    ///
    /// FAIL-SOFT, and deliberately so: this mirrors the reasoning already
    /// written on [`crate::runner::WorkJournal::record`] — losing the note is
    /// bad, losing the finished work because the note could not be written
    /// would be worse. The boolean says whether the line landed, for a caller
    /// that wants to report it; ignoring it is a valid choice.
    pub fn record(&mut self, event: &AgentEvent) -> bool {
        let Some(value) = line_for(event) else {
            return false;
        };
        self.write(value)
    }

    /// Stamp a line with who and when, then append it.
    fn write(&self, mut value: serde_json::Value) -> bool {
        if let Some(map) = value.as_object_mut() {
            map.insert("task".into(), self.task_id.clone().into());
            map.insert("role".into(), self.role.clone().into());
            map.insert("agent".into(), self.instance.clone().into());
            map.insert(
                "at".into(),
                chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                    .into(),
            );
        }
        self.append(&value)
    }

    fn append(&self, value: &serde_json::Value) -> bool {
        if let Some(parent) = self.path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return false;
            }
        }
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            return false;
        };
        // `to_string` never emits a raw newline — every one inside a field is
        // escaped — which is what keeps one event on exactly one line and the
        // file parseable as JSONL.
        writeln!(file, "{value}").is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_use(name: &str, input: &str) -> AgentEvent {
        AgentEvent::ToolUse {
            id: "t1".into(),
            name: name.into(),
            input: input.into(),
        }
    }

    #[test]
    fn a_tool_call_and_its_result_both_reach_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = Transcript::for_task(dir.path(), "d1.1", "implement");
        assert!(t.record(&tool_use("Edit", "{\"path\":\"a.rs\"}")));
        assert!(t.record(&AgentEvent::ToolResult {
            id: "t1".into(),
            output: "ok".into(),
            is_error: false,
        }));

        let body = std::fs::read_to_string(t.path()).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one line per event: {body}");
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["kind"], "tool_use");
        assert_eq!(first["tool"], "Edit");
        assert_eq!(first["task"], "d1.1");
        assert_eq!(first["role"], "implement");
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["kind"], "tool_result");
        assert_eq!(second["id"], "t1", "the result ties back to the call");
    }

    #[test]
    fn a_multi_line_tool_input_stays_on_one_line() {
        // The whole format depends on this: an unescaped newline inside a field
        // would split one event across two lines and make the file unparseable.
        let dir = tempfile::tempdir().unwrap();
        let mut t = Transcript::for_task(dir.path(), "d1", "explore");
        t.record(&tool_use("Bash", "line one\nline two\nline three"));

        let body = std::fs::read_to_string(t.path()).unwrap();
        assert_eq!(body.lines().count(), 1, "newlines were not escaped: {body}");
        let parsed: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(parsed["input"], "line one\nline two\nline three");
    }

    #[test]
    fn a_huge_tool_result_is_clipped_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = Transcript::for_task(dir.path(), "d1", "review");
        t.record(&AgentEvent::ToolResult {
            id: "t9".into(),
            output: "x".repeat(MAX_FIELD_CHARS * 3),
            is_error: false,
        });

        let body = std::fs::read_to_string(t.path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        let out = parsed["output"].as_str().unwrap();
        assert!(
            out.chars().count() < MAX_FIELD_CHARS * 2,
            "an unbounded field would let one tool result outweigh the run"
        );
        assert!(out.ends_with("…[clipped]"), "truncation must be visible");
    }

    #[test]
    fn clipping_a_multi_byte_field_does_not_panic() {
        // A byte-wise clip panics on a codepoint boundary. This is the case that
        // would have taken the run down rather than merely truncating badly.
        let text = "é".repeat(MAX_FIELD_CHARS + 50);
        let clipped = clip(&text);
        assert!(clipped.starts_with('é'));
        assert!(clipped.ends_with("…[clipped]"));
    }

    #[test]
    fn text_deltas_are_not_recorded_beside_the_text_they_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = Transcript::for_task(dir.path(), "d1", "explore");
        assert!(!t.record(&AgentEvent::TextDelta("par".into())));
        assert!(t.record(&AgentEvent::Text("partial".into())));

        let body = std::fs::read_to_string(t.path()).unwrap();
        assert_eq!(
            body.lines().count(),
            1,
            "recording both spellings writes every reply twice: {body}"
        );
    }

    #[test]
    fn a_task_id_cannot_write_outside_the_transcript_directory() {
        let dir = tempfile::tempdir().unwrap();
        let t = Transcript::for_task(dir.path(), "../../escaped", "implement");
        let parent = dir.path().join(DIR);
        assert!(
            t.path().starts_with(&parent),
            "a traversing id escaped the directory: {}",
            t.path().display()
        );
    }

    #[test]
    fn nothing_is_written_for_a_task_whose_agent_says_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = Transcript::for_task(dir.path(), "d2", "grunt");
        t.record(&AgentEvent::TextDelta("ignored".into()));
        assert!(
            !t.path().exists(),
            "an empty file per silent task is litter, not a record"
        );
    }
}
