//! The session record and the project's facts, written by the model through
//! incode rather than by `curl`.
//!
//! # Why this exists
//!
//! The engine's rules tell every agent to keep a session record — `state`,
//! `pending` and `decision` lines, retired when they stop being true — and to
//! write down a project fact the moment one is learned the hard way. For a
//! Claude Code session that is a `curl` to the local engine. A model running
//! inside incode had no way to do either: no tool wrote them, and a shell
//! `curl` is the very thing incode's sandbox and guards exist to question. So
//! the rules asked for something the model could not do, and what it learned
//! died with the session (reported by a model inside incode, 2026-10-26).
//!
//! # Wired to
//!
//! - **Called by** `crate::toolbox` — `reach_of` (all three record, like
//!   `remember`), `catalogue`, and the dispatch in `Toolbox::run_tool`.
//! - **Calls** the engine: `POST /v1/memory/session/entry`,
//!   `GET /v1/memory/session` (to read back the id the entry was given),
//!   `POST /v1/memory/session/entry/resolve`, `POST /v1/memory/project/facts`.
//! - **Siblings.** [`crate::lessons`] writes the runner's own facts to the same
//!   facts store, marked as learned by a run; [`crate::knowledge::remember`]
//!   writes lessons. The session the entries are filed under is the one the
//!   engine's session-start, prompt and stop events carry
//!   (`Toolbox::pin_session`), so the record and the captured turns are one
//!   session in the engine.

use std::path::Path;
use std::time::Duration;

/// Adds a line to this session's record.
pub const NOTE_TOOL: &str = "session_note";
/// Retires a line of the record.
pub const RESOLVE_TOOL: &str = "resolve_note";
/// Records a fact about how this project is built, run, tested or shipped.
pub const FACT_TOOL: &str = "project_fact";

const ENTRY_PATH: &str = "/v1/memory/session/entry";
const RESOLVE_PATH: &str = "/v1/memory/session/entry/resolve";
const SESSION_PATH: &str = "/v1/memory/session";

/// The categories a model writes. The engine also keeps `finding`, `done` and
/// `touched`, which its own hooks write; a model saying "done" resolves the
/// entry instead, so the record does not keep both.
pub const NOTE_CATEGORIES: [&str; 3] = ["state", "pending", "decision"];

/// The engine's fact kinds (`facts_api.rs` refuses any other).
pub const FACT_KINDS: [&str; 7] = [
    "deploy",
    "build",
    "run",
    "test",
    "install",
    "access",
    "invariant",
];

/// A write: not on a path a person is waiting on, and a lost note is worse
/// than a slow one. The same bound as [`crate::lessons`]' writes.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Whose record a call writes to.
pub struct Record<'a> {
    /// The session's own id (`Toolbox::pin_session`).
    pub session: &'a str,
    /// The folder the engine resolves the project from.
    pub workspace: &'a Path,
}

/// The three tools, as the model is told about them.
#[must_use]
pub fn tool_definitions() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": NOTE_TOOL,
            "description": "Add one line to this session's record — what the next \
                session starts from. `state`: something now TRUE about the world. \
                `pending`: open work, naming the NEXT concrete step. `decision`: \
                settled, WITH its reason. Write each the moment it becomes true, \
                not at the end. Returns the entry's id; retire the entry with \
                resolve_note when it stops being true. This IS the record the \
                memory rules tell you to `POST /v1/memory/session/entry` — use this \
                tool for it, not curl.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "category": {"type": "string", "enum": NOTE_CATEGORIES},
                    "line": {"type": "string", "description": "One line, stated plainly. Cite code as file:line."}
                },
                "required": ["category", "line"]
            }
        }),
        serde_json::json!({
            "name": RESOLVE_TOOL,
            "description": "Retire a line of this session's record: done=true when it \
                happened, done=false when it should never have been recorded. A stale \
                `pending` is worse than a missing one — the next session works the \
                step again. The id is the one session_note returned, or the one the \
                record shown at the start of the session prints beside the line. \
                This is `POST /v1/memory/session/entry/resolve` in the memory rules.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "The entry's id, in hex."},
                    "done": {"type": "boolean", "description": "True (the default) when it happened."}
                },
                "required": ["id"]
            }
        }),
        serde_json::json!({
            "name": FACT_TOOL,
            "description": "Record one fact about how THIS project is built, run, \
                tested, installed, deployed or reached — the flag a build needed, the \
                command that actually runs the tests, a trap that cost time — so no \
                later session pays to find it again. Give `cites` (file:line where it \
                is true) and `verify` (a command that checks it); a fact without a \
                citation is kept but marked unverified. Never record secrets. This \
                is `POST /v1/memory/project/facts` in the memory rules.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": FACT_KINDS},
                    "line": {"type": "string", "description": "The fact, on one line."},
                    "cites": {"type": "string", "description": "Where it is true, as file:line."},
                    "verify": {"type": "string", "description": "A command whose output confirms it."}
                },
                "required": ["kind", "line"]
            }
        }),
    ]
}

/// Run one of this module's tools. `None` when the name is not one of them.
pub fn handle_tool(
    name: &str,
    input: &serde_json::Value,
    record: &Record<'_>,
) -> Option<Result<String, String>> {
    let text = |key: &str| input.get(key).and_then(|v| v.as_str()).unwrap_or("");
    Some(match name {
        NOTE_TOOL => note(record, text("category"), text("line")),
        RESOLVE_TOOL => resolve(
            record,
            text("id"),
            input
                .get("done")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
        ),
        FACT_TOOL => fact(
            record,
            text("kind"),
            text("line"),
            text("cites"),
            text("verify"),
        ),
        _ => return None,
    })
}

/// A record entry the engine will take, or why it would not.
pub fn note_body(
    record: &Record<'_>,
    category: &str,
    line: &str,
) -> Result<serde_json::Value, String> {
    let session = session_of(record)?;
    let category = category.trim().to_ascii_lowercase();
    if !NOTE_CATEGORIES.contains(&category.as_str()) {
        return Err(format!(
            "`{category}` is not a category this record keeps. Use one of: {}",
            NOTE_CATEGORIES.join(", ")
        ));
    }
    let line = one_line(line, "the line")?;
    Ok(serde_json::json!({
        "session_id": session,
        "workspace": record.workspace.display().to_string(),
        "category": category,
        "line": line,
    }))
}

fn note(record: &Record<'_>, category: &str, line: &str) -> Result<String, String> {
    let body = note_body(record, category, line)?;
    post(ENTRY_PATH, &body)?;
    // The write answers `{ok, project}` without the entry's id, and resolving
    // needs it — so it is read back from the record it was written to. A
    // failure here does not undo the write; it is said, not hidden.
    let category = body["category"].as_str().unwrap_or_default();
    let line = body["line"].as_str().unwrap_or_default();
    let read = get(
        SESSION_PATH,
        "session_id",
        body["session_id"].as_str().unwrap_or_default(),
    );
    Ok(match read.map(|answer| entry_id(&answer, category, line)) {
        Ok(Some(id)) => format!(
            "Recorded as `{id}` ({category}). Retire it with {RESOLVE_TOOL} when it \
             stops being true."
        ),
        Ok(None) => format!(
            "Recorded ({category}), but its id was not in the record read back, so it \
             can only be resolved from the record shown at the next session start."
        ),
        Err(e) => format!(
            "Recorded ({category}), but its id could not be read back ({e}), so it can \
             only be resolved from the record shown at the next session start."
        ),
    })
}

/// The id of the newest entry in a `GET /v1/memory/session` answer whose
/// category and line are `category` and `line`, in the hex form resolving
/// takes.
///
/// The engine stores the id as a u64 and prints it `{:x}` in the block it
/// injects; `resolve` accepts that form only (`session_api.rs` `resolve_entry`
/// refuses decimal, because every decimal digit is also a hex digit).
#[must_use]
pub fn entry_id(answer: &str, category: &str, line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(answer).ok()?;
    value
        .get("session")?
        .get("entries")?
        .as_array()?
        .iter()
        .rev()
        .find(|entry| {
            entry.get("line").and_then(|l| l.as_str()) == Some(line)
                && entry
                    .get("category")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.eq_ignore_ascii_case(category))
        })
        .and_then(|entry| entry.get("id")?.as_u64())
        .map(|id| format!("{id:x}"))
}

fn resolve(record: &Record<'_>, id: &str, done: bool) -> Result<String, String> {
    let session = session_of(record)?;
    let id = id.trim();
    let raw = id.strip_prefix("0x").unwrap_or(id);
    if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "`{id}` is not an entry id: ids are hex, as session_note returned them"
        ));
    }
    post(
        RESOLVE_PATH,
        &serde_json::json!({ "session_id": session, "id": raw, "done": done }),
    )?;
    Ok(if done {
        format!("Entry `{raw}` resolved as done.")
    } else {
        format!("Entry `{raw}` withdrawn: it should never have been recorded.")
    })
}

/// A project fact the engine will take, or why it would not.
pub fn fact_body(
    record: &Record<'_>,
    kind: &str,
    line: &str,
    cites: &str,
    verify: &str,
) -> Result<serde_json::Value, String> {
    let kind = kind.trim().to_ascii_lowercase();
    if !FACT_KINDS.contains(&kind.as_str()) {
        return Err(format!(
            "`{kind}` is not a fact kind. Use one of: {}",
            FACT_KINDS.join(", ")
        ));
    }
    Ok(serde_json::json!({
        "cwd": record.workspace.display().to_string(),
        "kind": kind,
        // The engine refuses a fact with a line break in it: one fact, one line.
        "line": one_line(line, "the fact")?,
        "cites": cites.trim(),
        "verify": verify.trim(),
    }))
}

fn fact(
    record: &Record<'_>,
    kind: &str,
    line: &str,
    cites: &str,
    verify: &str,
) -> Result<String, String> {
    let body = fact_body(record, kind, line, cites, verify)?;
    let cited = !body["cites"].as_str().unwrap_or_default().is_empty();
    parse_fact(&post(crate::lessons::FACTS_PATH, &body)?, cited)
}

/// What a fact write did, from `POST /v1/memory/project/facts`'s answer.
pub fn parse_fact(answer: &str, cited: bool) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(answer)
        .map_err(|e| format!("the facts store's answer did not parse: {e}"))?;
    if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(format!("the facts store did not take it: {answer}"));
    }
    let id = value.get("id").and_then(|i| i.as_str()).unwrap_or("?");
    let mut said = if value.get("duplicate").and_then(serde_json::Value::as_bool) == Some(true) {
        format!("Already known as fact `{id}`; nothing new was stored.")
    } else if let Some(existing) = value.get("updated_existing").and_then(|u| u.as_str()) {
        format!("Added to the fact `{existing}` that already held most of it.")
    } else {
        format!("Recorded as fact `{id}`.")
    };
    if !cited {
        said.push_str(" It has no citation, so it is kept as unverified.");
    }
    Ok(said)
}

fn session_of<'a>(record: &Record<'a>) -> Result<&'a str, String> {
    let session = record.session.trim();
    if session.is_empty() {
        return Err(
            "this session has no id in the shared memory, so it has no record to write to"
                .to_string(),
        );
    }
    Ok(session)
}

/// `text`, trimmed, or why it cannot be one line of a record.
fn one_line<'a>(text: &'a str, what: &str) -> Result<&'a str, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(format!("{what} is empty"));
    }
    if text.contains('\n') {
        return Err(format!(
            "{what} must be one line — split it into separate entries"
        ));
    }
    Ok(text)
}

fn url(path: &str) -> String {
    format!("{}{path}", crate::brief::memory_url().trim_end_matches('/'))
}

fn post(path: &str, body: &serde_json::Value) -> Result<String, String> {
    let (url, body) = (url(path), body.to_string());
    crate::runtime::http::request_text_explained(
        move |c| {
            c.post(&url)
                .header("content-type", "application/json")
                .body(body)
                .timeout(TIMEOUT)
        },
        "memory service",
    )
}

/// `GET path?key=value`, the value encoded by the client.
fn get(path: &str, key: &'static str, value: &str) -> Result<String, String> {
    let (url, value) = (url(path), value.to_string());
    crate::runtime::http::request_text_explained(
        move |c| c.get(&url).query(&[(key, value)]).timeout(TIMEOUT),
        "memory service",
    )
}

#[cfg(test)]
mod tests;
