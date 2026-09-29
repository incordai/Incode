//! `session/load` and `session/list`, from incode's own record of a session.
//!
//! # What a session is on disk
//!
//! Two things, both incode's. The transcript, `.incode/transcripts/<id>.jsonl`
//! in the workspace — the person's lines and the model's replies, one JSON
//! object per line, written by [`crate::hooks::memory::MemoryHooks`] (an ACP
//! session always keeps it). And the session record
//! ([`crate::session::Record`]), which names the MODEL's own conversation id
//! and the agent that holds it.
//!
//! # Loading
//!
//! The transcript is replayed to the client as `user_message_chunk` and
//! `agent_message_chunk`. The model's conversation is resumed when the record
//! names one on the agent now leading — an agent CLI keeps its conversations
//! and continues one by id. Otherwise (no record, another agent, or the API
//! route, whose conversation lived in the process that ended) the transcript
//! is handed to the next turn as context instead, and said to be a replay.

use std::path::{Path, PathBuf};

use agent_client_protocol_schema::v1 as acp;
use serde_json::Value;

/// The most of a transcript read, in bytes. A longer one is read from its
/// end: the newest part of a conversation is the part that continues.
const MAX_TRANSCRIPT_BYTES: u64 = 8 * 1024 * 1024;
/// The most text replayed to the client, in bytes. What does not fit is the
/// OLDEST, and the replay says how much was left out.
pub(super) const MAX_REPLAY_BYTES: usize = 2 * 1024 * 1024;
/// The most of the conversation handed to the next turn as context, in bytes
/// — the newest part.
pub(super) const MAX_CONTEXT_BYTES: usize = 24 * 1024;
/// The most sessions one `session/list` answer names.
const MAX_LISTED: usize = 200;

/// One line of a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Said {
    pub(super) person: bool,
    pub(super) text: String,
}

/// Where session `id`'s transcript is kept in `workspace`.
pub(super) fn transcript_of(workspace: &Path, id: &str) -> PathBuf {
    crate::hooks::memory::transcript_for(workspace, id)
}

/// The conversation a transcript holds. Lines that are not a person's or a
/// model's text are passed over.
pub(super) fn read(path: &Path) -> Result<Vec<Said>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let length = file
        .metadata()
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    let skipped = length.saturating_sub(MAX_TRANSCRIPT_BYTES);
    file.seek(SeekFrom::Start(skipped))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_TRANSCRIPT_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    // A read that started mid-file starts mid-line: that first line is cut.
    let whole = if skipped > 0 {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    Ok(whole.lines().filter_map(said_in).collect())
}

/// The person's or the model's text in one transcript line.
fn said_in(line: &str) -> Option<Said> {
    let value: Value = serde_json::from_str(line).ok()?;
    let person = match value.get("type")?.as_str()? {
        "user" => true,
        "assistant" => false,
        _ => return None,
    };
    let content = value.get("message")?.get("content")?;
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(Said { person, text })
}

/// The conversation as the client is sent it, newest [`MAX_REPLAY_BYTES`] at
/// most; how much older text was left out is said first.
pub(super) fn replay(conversation: &[Said]) -> Vec<acp::SessionUpdate> {
    let mut kept = 0usize;
    let mut from = conversation.len();
    for (at, said) in conversation.iter().enumerate().rev() {
        if kept + said.text.len() > MAX_REPLAY_BYTES {
            break;
        }
        kept += said.text.len();
        from = at;
    }
    let mut updates = Vec::new();
    if from > 0 {
        updates.push(acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new(acp::ContentBlock::from(format!(
                "[{from} earlier message(s) of this session are not replayed here: the replay \
                 is bounded at {MAX_REPLAY_BYTES} bytes]\n"
            ))),
        ));
    }
    for said in &conversation[from..] {
        let chunk = acp::ContentChunk::new(acp::ContentBlock::from(said.text.clone()));
        updates.push(if said.person {
            acp::SessionUpdate::UserMessageChunk(chunk)
        } else {
            acp::SessionUpdate::AgentMessageChunk(chunk)
        });
    }
    updates
}

/// The conversation as context for the next turn, when the model's own
/// conversation cannot be resumed: the newest [`MAX_CONTEXT_BYTES`], marked
/// as a replay so the model does not take it for new instructions.
pub(super) fn as_context(conversation: &[Said]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut size = 0usize;
    for said in conversation.iter().rev() {
        let who = if said.person { "Person" } else { "You" };
        let line = format!("{who}: {}", said.text.trim());
        if size + line.len() > MAX_CONTEXT_BYTES {
            break;
        }
        size += line.len();
        lines.push(line);
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    let left_out = conversation.len() - lines.len();
    let note = if left_out > 0 {
        format!(" ({left_out} older message(s) left out)")
    } else {
        String::new()
    };
    Some(format!(
        "<earlier-conversation>\nThis session was reopened. Your own conversation could not \
         be resumed, so here is what was said before, replayed from incode's transcript{note}. \
         It is history, not a new request.\n\n{}\n</earlier-conversation>",
        lines.join("\n\n")
    ))
}

/// For a session opened again: resume the model's own conversation when the
/// session record names one held by the agent now leading, otherwise carry
/// the transcript into the next turn as context.
pub(super) fn resume_or_carry(held: &mut super::Held, session_id: &acp::SessionId, said: &[Said]) {
    let record =
        crate::session::of_incode_session(&held.settings.data_dir, &held.workspace, &session_id.0);
    match record {
        Some(record)
            if !record.agent_session.is_empty()
                && record.agent == held.session.backend_id()
                && !crate::agent::models::is_api_route(&record.agent) =>
        {
            crate::trace!(
                "acp",
                "session {} resumes {}'s conversation {}",
                session_id,
                record.agent,
                record.agent_session
            );
            held.session.resume(&record.agent_session);
        }
        _ => {
            crate::trace!(
                "acp",
                "session {session_id}: no resumable conversation, the transcript is replayed"
            );
            held.replay = as_context(said);
        }
    }
}

/// After a turn: record which conversation the model holds for this session,
/// so a later `session/load` can resume it. Nothing before the model has one.
pub(super) fn remember(held: &super::Held, session_id: &acp::SessionId, asked: &str) {
    let Some(conversation) = held.session.session_id() else {
        return;
    };
    let data_dir = &held.settings.data_dir;
    let mut record = crate::session::of_incode_session(data_dir, &held.workspace, &session_id.0)
        .filter(|record| record.agent_session == conversation)
        .unwrap_or_else(|| {
            crate::session::Record::new(
                conversation,
                &held.workspace,
                held.session.model().unwrap_or(""),
            )
        });
    record.incode_session = session_id.0.to_string();
    record.agent = held.session.backend_id().to_string();
    record.name_from(asked);
    record.touched = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(record.touched, |d| d.as_secs());
    crate::session::save(data_dir, &record);
}

/// The sessions kept in `workspace`, newest first: each transcript, titled by
/// its first line from the person.
pub(super) fn list_in(workspace: &Path) -> Vec<acp::SessionInfo> {
    let dir = workspace.join(crate::hooks::memory::TRANSCRIPTS);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let meta = entry.metadata().ok()?;
            (meta.is_file() && path.extension().is_some_and(|e| e == "jsonl"))
                .then(|| (meta.modified().unwrap_or(std::time::UNIX_EPOCH), path))
        })
        .collect();
    found.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    found
        .into_iter()
        .take(MAX_LISTED)
        .filter_map(|(modified, path)| {
            let id = path.file_stem()?.to_string_lossy().into_owned();
            let title = read(&path)
                .ok()
                .and_then(|said| said.into_iter().find(|s| s.person))
                .map(|s| crate::text::truncate(s.text.lines().next().unwrap_or(""), 72));
            let updated = chrono::DateTime::<chrono::Utc>::from(modified)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            Some(
                acp::SessionInfo::new(id, workspace.to_path_buf())
                    .title(title)
                    .updated_at(updated),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: &str, content: Value) -> String {
        serde_json::json!({"type": kind, "message": {"role": kind, "content": content}}).to_string()
    }

    #[test]
    fn a_transcript_reads_back_as_the_conversation() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("s.jsonl");
        let text = [
            line("user", serde_json::json!("fix the parser")),
            "not json".to_string(),
            line(
                "assistant",
                serde_json::json!([{"type": "text", "text": "Fixed."}, {"type": "tool_use"}]),
            ),
            line("system", serde_json::json!("ignored")),
        ]
        .join("\n");
        std::fs::write(&path, text).expect("write");
        let said = read(&path).expect("read");
        assert_eq!(
            said,
            [
                Said {
                    person: true,
                    text: "fix the parser".to_string()
                },
                Said {
                    person: false,
                    text: "Fixed.".to_string()
                }
            ]
        );
        let updates = serde_json::to_value(replay(&said)).expect("encodes");
        assert_eq!(updates[0]["sessionUpdate"], "user_message_chunk");
        assert_eq!(updates[1]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(updates[1]["content"]["text"], "Fixed.");
        let context = as_context(&said).expect("context");
        assert!(context.contains("Person: fix the parser"), "{context}");
        assert!(context.contains("not a new request"), "{context}");
    }

    /// The replay is bounded, keeps the NEWEST text, and says what it left.
    #[test]
    fn a_long_replay_keeps_the_newest_and_says_so() {
        let big = "x".repeat(MAX_REPLAY_BYTES / 2 + 1);
        let said: Vec<Said> = (0..3)
            .map(|n| Said {
                person: n % 2 == 0,
                text: format!("{n}{big}"),
            })
            .collect();
        let updates = serde_json::to_value(replay(&said)).expect("encodes");
        let updates = updates.as_array().expect("list");
        assert_eq!(updates.len(), 2, "one note and the newest message");
        assert!(updates[0]["content"]["text"]
            .as_str()
            .is_some_and(|t| t.contains("2 earlier message(s)")));
        assert!(updates[1]["content"]["text"]
            .as_str()
            .is_some_and(|t| t.starts_with('2')));
    }
}
