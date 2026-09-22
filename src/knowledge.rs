//! Saving to the body's memory, and searching it.
//!
//! The read path is [`crate::recall`]; this is the other half — write, search,
//! and correct. Both talk to the same store every agent on this machine shares.
//!
//! # Why this replaced a local store
//!
//! The harness used to keep its own memory: a JSON file, a keyword search, and
//! five tools that wrote to it. It was strictly worse than the thing it shadowed
//! — no embeddings, no graph, no timeline, invisible to every other agent — and
//! it became actively harmful once the body's own recall was wired in beside it,
//! because then the model had two memories in one tool list and no way to tell
//! which was real. A note saved to the wrong one is a note nobody reads again.
//!
//! # Correcting a memory is two steps, and that is the service's rule
//!
//! A delete without `confirmed` does not delete: it returns the current value so
//! a person can be shown what is about to go. Only a second call with
//! `confirmed: true` applies it. That handshake belongs to the service and is
//! reproduced faithfully here rather than collapsed into one call for
//! convenience — the convenience would be a memory deleted on a model's say-so.

use std::time::Duration;

/// Where a durable fact is written.
pub const REMEMBER_PATH: &str = "/v1/memory/remember";

/// Where the ranked snippet search lives.
pub const SEARCH_PATH: &str = "/v1/memory/search";

/// Where one stored message is corrected or removed.
pub const MESSAGE_PATH: &str = "/v1/memory/messages";

/// Where a name is looked up in the store's index of this machine's code.
pub const SYMBOL_PATH: &str = "/v1/memory/graph/find-symbol";

/// Where a signatures-only AST view of a file is fetched.
pub const SKELETON_PATH: &str = "/v1/memory/graph/code-skeleton";

/// Where the transitive blast radius of git changes is calculated.
pub const BLAST_RADIUS_PATH: &str = "/v1/memory/graph/git-blast-radius";

/// The tools this module advertises.
pub const REMEMBER_TOOL: &str = "remember";
pub const SEARCH_TOOL: &str = "memory_search";
pub const FORGET_TOOL: &str = "forget_memory";
pub const SYMBOL_TOOL: &str = "find_symbol";
pub const SKELETON_TOOL: &str = "code_skeleton";
pub const BLAST_RADIUS_TOOL: &str = "git_blast_radius";

/// Bounded: these run inside a turn.
const TIMEOUT: Duration = Duration::from_secs(8);

/// Default number of snippets, small because each one is paid for in context.
const DEFAULT_TOP_K: u64 = 5;

/// The kinds the store recognises, pinned here so a rejected kind is caught
/// before a round trip rather than after one.
pub const KINDS: [&str; 5] = ["fact", "decision", "preference", "todo", "entity"];

/// One thing the store found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub conv_id: String,
    pub idx: u64,
    pub role: String,
    pub content: String,
}

/// What a finished task is written down as, and under whose name.
///
/// Pure, so the wording and the attribution can be checked without a store
/// behind them. Returns `(text, subject)` for [`remember`].
///
/// # Why the subject carries the fingerprint
///
/// The subject is what the store scopes a memory by, so putting the agent's
/// fingerprint in it turns "everything this worker did" into a lookup instead of
/// a string match against prose. The project comes first because that is the
/// coarser filter and the one a person types.
#[must_use]
pub fn work_note(
    attribution: &crate::identity::Attribution,
    task_id: &str,
    title: &str,
    evidence: &str,
) -> (String, String) {
    let text = format!(
        "{task_id} ({title}) was finished by {}. Proved by: {}",
        attribution.line(),
        first_line(evidence)
    );
    let subject = format!("{}#{}", project_subject(), attribution.agent.fingerprint());
    (text, subject)
}

/// The first line of a command's output, bounded.
///
/// Evidence can be an entire test run; a memory that carries one is a memory
/// nobody reads and context everybody pays for.
fn first_line(evidence: &str) -> String {
    const MAX: usize = 200;
    let line = evidence
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let line = line.trim();
    if line.is_empty() {
        return "(the check passed without output)".to_string();
    }
    // Bounded on a CHARACTER boundary: slicing bytes here panics the moment a
    // check prints anything non-ASCII, which is a crash in the recording path
    // of successful work.
    match line.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

/// Which project a memory belongs to.
fn project_subject() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "incord-code".to_string())
}

/// Save a durable fact.
///
/// Returns the identifier the store assigned, so a caller can say what to
/// correct later.
pub fn remember(text: &str, kind: &str, subject: &str) -> Result<String, String> {
    if text.trim().is_empty() {
        return Err("nothing to remember: the text is empty".to_string());
    }
    if !KINDS.contains(&kind) {
        return Err(format!(
            "`{kind}` is not a kind this store keeps. Use one of: {}",
            KINDS.join(", ")
        ));
    }
    let body = serde_json::json!({ "text": text, "kind": kind, "subject": subject }).to_string();
    parse_remember(&post(&url(REMEMBER_PATH), body)?)
}

/// Read the identifier out of a save.
///
/// The endpoint answers in the tool-call envelope — `{content:[{text}], isError}`
/// — where the payload is JSON *as a string* inside `content[0].text`. Measured,
/// not assumed: the search endpoint beside it answers with a plain body, so one
/// parser written for both would have quietly found nothing.
pub fn parse_remember(body: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("the store's answer did not parse: {e}"))?;
    if value
        .get("isError")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Err("the store refused to save it".to_string());
    }
    let inner = value
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|f| f.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    let payload: serde_json::Value = serde_json::from_str(inner)
        .map_err(|e| format!("the store's answer carried no usable result: {e}"))?;
    if !payload
        .get("stored")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        // "Accepted but not stored" is the one outcome that reads as success and
        // is not: the caller would report a fact saved that nobody can recall.
        return Err("the store accepted the call but did not store anything".to_string());
    }
    Ok(payload
        .get("msg_id")
        .and_then(|m| m.as_str())
        .unwrap_or("saved")
        .to_string())
}

/// Search the store for snippets.
pub fn search(query: &str, top_k: u64) -> Result<Vec<Match>, String> {
    search_within(query, top_k, TIMEOUT)
}

/// The same search, with the caller's own deadline.
///
/// Startup and any other path where a person is watching an empty screen passes
/// [`crate::recall::WAITING_TIMEOUT`]. The eight-second default is for a tool
/// call the model asked for and is willing to wait on; spending it in front of
/// a prompt that has not appeared yet is how startup came to take 17 seconds.
pub fn search_within(
    query: &str,
    top_k: u64,
    timeout: std::time::Duration,
) -> Result<Vec<Match>, String> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let body = serde_json::json!({ "query": query, "top_k": top_k }).to_string();
    parse_matches(&post_within(&url(SEARCH_PATH), body, timeout)?)
}

/// Read a search result. Unknown fields are ignored: the store ships separately.
pub fn parse_matches(body: &str) -> Result<Vec<Match>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("the search answer did not parse: {e}"))?;
    let Some(items) = value.get("matches").and_then(|m| m.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(items
        .iter()
        .map(|m| Match {
            conv_id: field(m, "conv_id"),
            idx: m
                .get("idx")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            role: field(m, "role"),
            content: field(m, "content"),
        })
        .collect())
}

/// What a delete did, or what it is waiting for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Forget {
    /// Nothing was deleted. This is what the store says first, every time: here
    /// is the value, show it to a person, call again to mean it.
    NeedsConfirmation { current: String },
    /// Gone.
    Deleted { conv_id: String, idx: u64 },
}

/// Remove a stored message. Two steps, always.
pub fn forget(conv_id: &str, idx: u64, confirmed: bool) -> Result<Forget, String> {
    let body = serde_json::json!({ "confirmed": confirmed }).to_string();
    let target = format!("{}/{conv_id}/{idx}", url(MESSAGE_PATH));
    parse_forget(&delete(&target, body)?)
}

/// Read a delete answer.
pub fn parse_forget(body: &str) -> Result<Forget, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("the delete answer did not parse: {e}"))?;
    if value
        .get("confirmation_required")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(Forget::NeedsConfirmation {
            current: field(&value, "current"),
        });
    }
    if value
        .get("deleted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(Forget::Deleted {
            conv_id: field(&value, "conv_id"),
            idx: value
                .get("idx")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        });
    }
    // Neither confirmed-pending nor deleted. Saying so beats reporting a
    // deletion that did not happen.
    Err("the store neither asked for confirmation nor deleted anything".to_string())
}

fn field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn url(path: &str) -> String {
    format!("{}{path}", crate::brief::memory_url().trim_end_matches('/'))
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// What the model may do to the shared memory.
///
/// Three tools, not five. The store's own recall (`recall_index` / `recall_get`)
/// is the read path and is registered separately; a fourth way to read would put
/// the model back where this module exists to stop it being — several memories
/// in one list, with no way to tell which one matters.
#[must_use]
pub fn tool_definitions() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": REMEMBER_TOOL,
            "description": "Save one durable fact, decision, preference, todo or \
                entity to the shared memory every agent on this machine reads. \
                Use it when something worth recalling later comes up. Never save \
                secrets, credentials, API keys, internal URLs, or raw source.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "The thing to remember, stated plainly."},
                    "kind": {"type": "string", "enum": KINDS, "description": "What kind of knowledge this is."},
                    "subject": {"type": "string", "description": "Optional: the project, person or file it is about."}
                },
                "required": ["text", "kind"]
            }
        }),
        serde_json::json!({
            "name": SEARCH_TOOL,
            "description": "Search the shared memory for source snippets, matched \
                by meaning rather than by shared words. Use it when you only need \
                the passages; use recall_index when you need to choose what to read.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "A natural-language question."},
                    "top_k": {"type": "integer", "description": "How many snippets. Small is better."}
                },
                "required": ["query"]
            }
        }),
        serde_json::json!({
            "name": FORGET_TOOL,
            "description": "Delete one stored message that is wrong or stale. TWO \
                STEPS: call it first without `confirmed`, show the user the value \
                it returns, and call again with confirmed=true only once they have \
                agreed. Never confirm on your own judgement.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "conv_id": {"type": "string"},
                    "idx": {"type": "integer"},
                    "confirmed": {"type": "boolean", "description": "Only ever true after the user has seen the value and agreed."}
                },
                "required": ["conv_id", "idx"]
            }
        }),
        serde_json::json!({
            "name": SYMBOL_TOOL,
            "description": "Find where a type, function or constant is DEFINED — its file and \
                line range — from the index the store keeps of this machine's code. Use it \
                instead of searching when you know the name: it answers with the definition \
                rather than with every place the word appears.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "symbol": {"type": "string", "description": "The exact name, e.g. `Toolbox` or `run_turn`."}
                },
                "required": ["symbol"]
            }
        }),
        serde_json::json!({
            "name": SKELETON_TOOL,
            "description": "Signatures-only view of a file with implementation bodies stripped (~10x cheaper than reading the file). Returns function, method, struct, enum, and class definitions with line spans, signatures, and doc comments.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Absolute or repo-relative path of the code file to skeletonize."}
                },
                "required": ["path"]
            }
        }),
        serde_json::json!({
            "name": BLAST_RADIUS_TOOL,
            "description": "Calculate blast radius of uncommitted edits or a git diff. Automatically maps changed lines to enclosing code definitions, traces transitive callers (depth 1-5), and identifies impacted test suites to run.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "depth": {"type": "integer", "description": "Transitive caller walk depth, 1-5 (default 2)."},
                    "limit": {"type": "integer", "description": "Max callers per name per level (default 25, max 200)."},
                    "repo_path": {"type": "string", "description": "Path to the repository/project root."}
                }
            }
        }),
    ]
}

/// Run one of this module's tools. `None` when the name is not one of them.
pub fn handle_tool(name: &str, input: &serde_json::Value) -> Option<Result<String, String>> {
    match name {
        REMEMBER_TOOL => Some(run_remember(input)),
        SEARCH_TOOL => Some(run_search(input)),
        FORGET_TOOL => Some(run_forget(input)),
        SYMBOL_TOOL => Some(run_symbol(input)),
        SKELETON_TOOL => Some(run_skeleton(input)),
        BLAST_RADIUS_TOOL => Some(run_blast_radius(input)),
        _ => None,
    }
}

/// Ask the store where a name is defined.
///
/// # Why this instead of a language server
///
/// Going to a definition is most of what a language server is wanted for here,
/// and the store already indexes it across every project on this machine — so
/// this is one request rather than a server per language, each with its own
/// lifecycle to manage. What it does NOT give is hover types, completions or
/// rename-across-project; if those are ever needed, they need the real thing.
fn run_symbol(input: &serde_json::Value) -> Result<String, String> {
    let symbol = input
        .get("symbol")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim();
    if symbol.is_empty() {
        return Err("say which name to look up".to_string());
    }
    let url = format!(
        "{}{SYMBOL_PATH}",
        crate::brief::memory_url().trim_end_matches('/')
    );
    let body = serde_json::json!({ "symbol": symbol }).to_string();
    parse_symbol(&post(&url, body)?, symbol)
}

/// Read the store's answer into something a model can act on.
///
/// Split out so the rendering is testable without the store running — the shape
/// was taken from a live call, and a parser tested only against itself passes
/// while the field names drift.
pub fn parse_symbol(body: &str, symbol: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("the symbol index's answer did not parse: {e}"))?;
    let found = value
        .get("definitions")
        .and_then(|d| d.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    if found.is_empty() {
        // Said plainly rather than as an empty list: "not indexed" and "does not
        // exist" are different, and a model told nothing will assume the second.
        return Ok(format!(
            "`{symbol}` is not in the symbol index. It may still exist — search for it."
        ));
    }
    let mut out = format!("`{symbol}` is defined in:\n");
    for one in found {
        let field = |key: &str| {
            one.get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
        };
        let line = |key: &str| {
            one.get(key)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };
        out.push_str(&format!(
            "  {}:{}-{}  {} {}\n",
            field("path"),
            line("start_line"),
            line("end_line"),
            field("kind"),
            field("symbol"),
        ));
    }
    Ok(out)
}

fn run_remember(input: &serde_json::Value) -> Result<String, String> {
    let text = input.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let kind = input.get("kind").and_then(|k| k.as_str()).unwrap_or("fact");
    let subject = input.get("subject").and_then(|s| s.as_str()).unwrap_or("");
    remember(text, kind, subject).map(|id| format!("Saved as {id}."))
}

fn run_search(input: &serde_json::Value) -> Result<String, String> {
    let query = input.get("query").and_then(|q| q.as_str()).unwrap_or("");
    let top_k = input
        .get("top_k")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(DEFAULT_TOP_K)
        .clamp(1, 20);
    let matches = search(query, top_k)?;
    if matches.is_empty() {
        return Ok("Nothing in memory matches that.".to_string());
    }
    Ok(render_matches(&matches))
}

/// One block per match, each naming where it came from so the caller can fetch
/// the whole thing with `recall_get` instead of acting on a fragment.
#[must_use]
pub fn render_matches(matches: &[Match]) -> String {
    let mut out = String::new();
    for m in matches {
        out.push_str(&format!(
            "\n[{}#{} {}] {}\n",
            m.conv_id,
            m.idx,
            m.role,
            crate::text::head_bytes(&m.content, 600)
        ));
    }
    out.trim_start().to_string()
}

fn run_forget(input: &serde_json::Value) -> Result<String, String> {
    let conv_id = input.get("conv_id").and_then(|c| c.as_str()).unwrap_or("");
    let idx = input
        .get("idx")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let confirmed = input
        .get("confirmed")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    match forget(conv_id, idx, confirmed)? {
        Forget::NeedsConfirmation { current } => Ok(format!(
            "NOT deleted yet. This is what would go:\n\n{current}\n\nShow the user \
             this exact text and call again with confirmed=true only if they agree."
        )),
        Forget::Deleted { conv_id, idx } => Ok(format!("Deleted {conv_id}#{idx}.")),
    }
}

fn run_skeleton(input: &serde_json::Value) -> Result<String, String> {
    let path = input
        .get("path")
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .trim();
    if path.is_empty() {
        return Err("say which file path to skeletonize".to_string());
    }
    let url = format!(
        "{}{SKELETON_PATH}",
        crate::brief::memory_url().trim_end_matches('/')
    );
    let body = serde_json::json!({ "path": path }).to_string();
    parse_code_skeleton(&post(&url, body)?)
}

fn run_blast_radius(input: &serde_json::Value) -> Result<String, String> {
    let depth = input.get("depth").and_then(|d| d.as_u64()).unwrap_or(2);
    let limit = input.get("limit").and_then(|l| l.as_u64()).unwrap_or(25);
    let repo_path = input
        .get("repo_path")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .trim();
    let url = format!(
        "{}{BLAST_RADIUS_PATH}",
        crate::brief::memory_url().trim_end_matches('/')
    );
    let mut payload = serde_json::json!({
        "depth": depth,
        "limit": limit,
    });
    if !repo_path.is_empty() {
        payload["repo_path"] = serde_json::json!(repo_path);
    }
    parse_blast_radius(&post(&url, payload.to_string())?)
}

/// Read the code skeleton store answer.
pub fn parse_code_skeleton(body: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("the code skeleton answer did not parse: {e}"))?;
    if let Some(formatted) = value.get("formatted").and_then(|f| f.as_str()) {
        if !formatted.trim().is_empty() {
            return Ok(formatted.to_string());
        }
    }
    let path = value.get("path").and_then(|p| p.as_str()).unwrap_or("");
    let items = value
        .get("items")
        .and_then(|i| i.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    if items.is_empty() {
        return Ok(format!("no code skeleton definitions found for `{path}`"));
    }
    let mut out = format!("# Skeleton — {path} ({} definitions)\n\n", items.len());
    for item in items {
        let symbol = item.get("symbol").and_then(|s| s.as_str()).unwrap_or("");
        let kind = item.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        let start = item.get("start_line").and_then(|l| l.as_u64()).unwrap_or(0);
        let end = item.get("end_line").and_then(|l| l.as_u64()).unwrap_or(0);
        let sig = item.get("signature").and_then(|s| s.as_str()).unwrap_or("");
        let doc = item.get("doc").and_then(|d| d.as_str()).unwrap_or("");
        out.push_str(&format!("- L{start}-L{end} [{kind}] {symbol}: `{sig}`\n"));
        if !doc.trim().is_empty() {
            out.push_str(&format!("  /// {}\n", doc.trim()));
        }
    }
    Ok(out)
}

/// Read the git blast radius answer into a concise summary.
pub fn parse_blast_radius(body: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("the blast radius answer did not parse: {e}"))?;
    let status = value.get("status").and_then(|s| s.as_str()).unwrap_or("");
    let message = value.get("message").and_then(|m| m.as_str()).unwrap_or("");
    let affected_symbols = value
        .get("affected_symbols")
        .and_then(|a| a.as_u64())
        .unwrap_or(0);
    let changed_files = value
        .get("changed_files")
        .and_then(|c| c.as_u64())
        .unwrap_or(0);
    let impacted_tests = value
        .get("impacted_tests")
        .and_then(|t| t.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();

    if status == "clean" || (changed_files == 0 && affected_symbols == 0) {
        if !message.is_empty() {
            return Ok(format!("Blast radius clean: {message}"));
        }
        return Ok("Blast radius clean: no uncommitted changes or affected symbols.".to_string());
    }

    let mut out = format!(
        "Blast radius: {changed_files} file(s) changed, {affected_symbols} symbol(s) affected.\n"
    );
    if !impacted_tests.is_empty() {
        out.push_str("Impacted tests to verify:\n");
        for t in impacted_tests {
            if let Some(name) = t.as_str() {
                out.push_str(&format!("  - {name}\n"));
            }
        }
    }
    Ok(out)
}

/// The subject filter for searching finished work by a specific worker in Incord Memory.
#[must_use]
pub fn work_query_subject(attribution: &crate::identity::Attribution) -> String {
    format!("{}#{}", project_subject(), attribution.agent.fingerprint())
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

fn post(url: &str, body: String) -> Result<String, String> {
    post_within(url, body, TIMEOUT)
}

/// The same POST, bounded by the caller's deadline rather than the default.
fn post_within(url: &str, body: String, timeout: std::time::Duration) -> Result<String, String> {
    let url = url.to_string();
    request_within(
        move |c| {
            c.post(&url)
                .header("content-type", "application/json")
                .body(body)
        },
        timeout,
    )
}

fn delete(url: &str, body: String) -> Result<String, String> {
    let url = url.to_string();
    request(move |c| {
        c.delete(&url)
            .header("content-type", "application/json")
            .body(body)
    })
}

fn request<F>(build: F) -> Result<String, String>
where
    F: FnOnce(&reqwest::Client) -> reqwest::RequestBuilder + Send + 'static,
{
    request_within(build, TIMEOUT)
}

fn request_within<F>(build: F, timeout: std::time::Duration) -> Result<String, String>
where
    F: FnOnce(&reqwest::Client) -> reqwest::RequestBuilder + Send + 'static,
{
    crate::runtime::http::request_text(
        move |client| build(client).timeout(timeout),
        "memory service",
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    /// **The note names the agent, not the model.** A model in the name would
    /// split one worker's history the first time a fallback fired.
    #[test]
    fn a_work_note_is_written_under_the_agent() {
        let attribution = crate::identity::Attribution::new(crate::identity::AgentId::new(
            "some-backend",
            "grunt",
        ))
        .served_by(vec!["some-model".to_string()]);

        let (text, subject) = work_note(&attribution, "t-1", "add the thing", "`check` passed");

        assert!(text.contains("some-backend:grunt"), "got: {text}");
        assert!(
            text.contains("t-1") && text.contains("add the thing"),
            "got: {text}"
        );
        assert!(
            subject.ends_with(&format!("#{}", attribution.agent.fingerprint())),
            "the subject must scope by agent so the history is a lookup: {subject}"
        );
    }

    /// **The same agent writes to the same subject on a different model.** This
    /// is what makes "everything this worker did" one query rather than several.
    #[test]
    fn a_fallback_does_not_move_the_note_to_a_different_subject() {
        let agent = crate::identity::AgentId::new("some-backend", "grunt");
        let before =
            crate::identity::Attribution::new(agent.clone()).served_by(vec!["model-a".to_string()]);
        let after = crate::identity::Attribution::new(agent).served_by(vec!["model-b".to_string()]);

        let (text_a, subject_a) = work_note(&before, "t-1", "work", "ok");
        let (text_b, subject_b) = work_note(&after, "t-2", "work", "ok");

        assert_eq!(subject_a, subject_b, "one worker, one subject");
        assert!(text_a.contains("model-a") && text_b.contains("model-b"));
    }

    /// **A whole test run does not go into a memory.** Evidence can be
    /// thousands of lines; a note that carries one is context everybody pays for
    /// and nobody reads.
    #[test]
    fn evidence_is_bounded_to_one_line() {
        let attribution =
            crate::identity::Attribution::new(crate::identity::AgentId::new("b", "grunt"));
        let huge = format!("{}\nand a second line\n", "x".repeat(5_000));

        let (text, _) = work_note(&attribution, "t-1", "work", &huge);

        assert!(
            text.len() < 400,
            "the note must stay small, got {} chars",
            text.len()
        );
        assert!(!text.contains("and a second line"), "got: {text}");
        assert!(text.contains('…'), "and it must say it was cut: {text}");
    }

    /// **Non-ASCII output does not panic the recording path.** Slicing a bounded
    /// string on a byte index crashes the moment a check prints a character that
    /// is not one byte wide — and it would crash it AFTER the work succeeded.
    #[test]
    fn output_that_is_not_ascii_does_not_crash_the_note() {
        let attribution =
            crate::identity::Attribution::new(crate::identity::AgentId::new("b", "grunt"));
        // THREE bytes wide, chosen so the byte at the cut falls INSIDE a
        // character: 200 is not divisible by 3. A two-byte character lands
        // exactly on the boundary and the test passes with the bug present —
        // measured, after the first version of this test did precisely that.
        let noisy = "\u{20ac}".repeat(5_000);

        let (text, _) = work_note(&attribution, "t-1", "work", &noisy);
        assert!(text.contains('…'), "got: {text}");
    }

    /// A check that passed silently still gets a readable note.
    #[test]
    fn a_silent_check_is_recorded_as_silent() {
        let attribution =
            crate::identity::Attribution::new(crate::identity::AgentId::new("b", "grunt"));
        let (text, _) = work_note(&attribution, "t-1", "work", "   \n\n  ");
        assert!(text.contains("without output"), "got: {text}");
    }
    use super::*;

    /// The exact envelope the running service returned for a real save. The
    /// payload is a JSON *string* nested inside the tool-call wrapper.
    const REAL_SAVE: &str = r#"{"content":[{"text":"{\"msg_id\":\"memory#823\",\"conv_id\":\"memory\",\"idx\":823,\"stored\":true,\"embedded\":true}","type":"text"}],"isError":false}"#;

    /// The exact body the running service returned for a real search.
    const REAL_SEARCH: &str = r#"{"matches":[{"msg_id":"abc#389","conv_id":"abc","idx":389,
        "role":"assistant","content":"the admin layer precedence ladder"}]}"#;

    #[test]
    fn a_real_save_yields_the_id_the_store_assigned() {
        assert_eq!(
            parse_remember(REAL_SAVE).expect("the observed envelope parses"),
            "memory#823"
        );
    }

    /// The two endpoints answer in DIFFERENT shapes — one wrapped, one plain.
    /// Assuming they matched would have produced a parser that always found
    /// nothing and never failed.
    #[test]
    fn the_two_endpoints_are_parsed_by_their_own_shapes() {
        let matches = parse_matches(REAL_SEARCH).expect("the observed body parses");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].conv_id, "abc");
        assert_eq!(matches[0].idx, 389);
        // The save envelope is not a search body, and must not silently read as
        // an empty result set.
        assert!(parse_matches(REAL_SAVE).expect("parses").is_empty());
    }

    /// **Accepted is not stored.** The one answer that reads as success and is
    /// not: a fact reported saved that nobody can ever recall.
    #[test]
    fn an_accepted_but_unstored_save_is_an_error_not_a_success() {
        let body =
            r#"{"content":[{"text":"{\"msg_id\":\"x\",\"stored\":false}"}],"isError":false}"#;
        assert!(parse_remember(body).is_err());
        let refused = r#"{"content":[{"text":"{}"}],"isError":true}"#;
        assert!(parse_remember(refused).is_err());
    }

    /// **A delete is two steps.** The first answer is the value, not a deletion,
    /// and reading it as one would destroy a memory on a model's say-so.
    #[test]
    fn the_first_delete_answer_is_a_value_to_show_not_a_deletion() {
        let first = r#"{"confirmation_required":true,"current":"the fact itself",
            "note":"re-call with confirmed=true"}"#;
        match parse_forget(first).expect("parses") {
            Forget::NeedsConfirmation { current } => assert_eq!(current, "the fact itself"),
            other => panic!("the first answer must not read as deleted: {other:?}"),
        }

        let second = r#"{"conv_id":"memory","deleted":true,"idx":823}"#;
        assert_eq!(
            parse_forget(second).expect("parses"),
            Forget::Deleted {
                conv_id: "memory".to_string(),
                idx: 823
            }
        );
    }

    /// An answer that is neither is not a deletion either.
    #[test]
    fn an_ambiguous_delete_answer_is_never_reported_as_deleted() {
        assert!(parse_forget(r#"{"ok":true}"#).is_err());
        assert!(parse_forget(r#"{"deleted":false}"#).is_err());
        assert!(parse_forget("not json").is_err());
    }

    /// The tool must not let a caller invent a kind the store will reject after
    /// a round trip, and must not save nothing at all.
    #[test]
    fn a_save_is_checked_before_it_is_sent() {
        assert!(remember("   ", "fact", "").is_err());
        assert!(remember("something", "not-a-kind", "").is_err());
    }

    /// Every tool is offered, and the two dangerous ones say so in words the
    /// model reads.
    ///
    /// This asserted `defs.len() == 3` before there was a fourth tool. A count
    /// protects nothing — it cannot tell which three — and it breaks on an
    /// addition that was not a mistake. Naming them catches the thing a count
    /// was standing in for: one going missing.
    #[test]
    fn the_tools_state_the_rules_that_matter() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs.iter().filter_map(|d| d["name"].as_str()).collect();
        for expected in [REMEMBER_TOOL, SEARCH_TOOL, FORGET_TOOL, SYMBOL_TOOL] {
            assert!(
                names.contains(&expected),
                "{expected} is missing: {names:?}"
            );
        }
        let rendered = serde_json::to_string(&defs).expect("serialises");
        assert!(
            rendered.contains("Never save secrets"),
            "the write tool must carry the secrets rule"
        );
        assert!(
            rendered.contains("TWO STEPS"),
            "the delete tool must carry the confirmation rule: {rendered}"
        );
    }

    /// A snippet names where it came from, so the caller can fetch the whole
    /// message instead of acting on a fragment.
    #[test]
    fn a_snippet_says_where_to_read_the_rest() {
        let rendered = render_matches(&[Match {
            conv_id: "abc".to_string(),
            idx: 7,
            role: "user".to_string(),
            content: "something said earlier".to_string(),
        }]);
        assert!(rendered.contains("abc#7"), "got: {rendered}");
    }

    /// An unreachable store is an error the caller sees, never a silent success.
    #[test]
    fn an_unreachable_store_fails_loudly_on_the_write_path() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::brief::MEMORY_URL_ENV, "http://127.0.0.1:1");
        let saved = remember("a fact", "fact", "");
        let searched = search("anything", 3);
        std::env::remove_var(crate::brief::MEMORY_URL_ENV);
        assert!(saved.is_err(), "a lost write must never look saved");
        assert!(searched.is_err());
    }

    /// The store's own answer, taken from a live call rather than written from
    /// the parser — a shape invented here would pass while the real fields drift.
    const LIVE_SYMBOL: &str = r#"{
      "definitions": [
        {"end_line": 119, "kind": "struct_item", "node_id": "file-x",
         "path": "src/toolbox.rs", "start_line": 85, "symbol": "Toolbox"}
      ],
      "symbol": "Toolbox", "total": 1
    }"#;

    #[test]
    fn a_definition_says_the_file_and_the_lines() {
        // The whole value over searching: a place to go, not a list of mentions.
        let out = parse_symbol(LIVE_SYMBOL, "Toolbox").expect("the live shape parses");
        assert!(out.contains("src/toolbox.rs"), "{out}");
        assert!(out.contains("85"), "the line it starts on: {out}");
        assert!(out.contains("119"), "and where it ends: {out}");
        assert!(
            out.contains("struct_item"),
            "and what kind of thing it is: {out}"
        );
    }

    #[test]
    fn a_name_that_is_not_indexed_is_not_reported_as_missing() {
        // "not indexed" and "does not exist" are different claims, and a model
        // told the first will act on the second unless the difference is said.
        let out = parse_symbol(r#"{"definitions": [], "total": 0}"#, "Ghost").expect("parses");
        assert!(out.contains("not in the symbol index"), "{out}");
        assert!(out.contains("It may still exist"), "{out}");
    }

    #[test]
    fn an_answer_that_is_not_json_is_an_error_not_an_empty_result() {
        assert!(parse_symbol("<html>down</html>", "Toolbox").is_err());
    }

    #[test]
    fn the_symbol_tool_is_advertised_with_the_rest() {
        let names: Vec<String> = tool_definitions()
            .iter()
            .filter_map(|d| d["name"].as_str().map(str::to_string))
            .collect();
        assert!(names.iter().any(|n| n == SYMBOL_TOOL), "{names:?}");
        assert!(
            handle_tool(SYMBOL_TOOL, &serde_json::json!({})).is_some(),
            "advertised but not wired"
        );
    }

    #[test]
    fn code_skeleton_and_blast_radius_tools_are_advertised_and_wired() {
        let names: Vec<String> = tool_definitions()
            .iter()
            .filter_map(|d| d["name"].as_str().map(str::to_string))
            .collect();
        assert!(names.iter().any(|n| n == SKELETON_TOOL), "{names:?}");
        assert!(names.iter().any(|n| n == BLAST_RADIUS_TOOL), "{names:?}");
        assert!(
            handle_tool(SKELETON_TOOL, &serde_json::json!({})).is_some(),
            "skeleton advertised but not wired"
        );
        assert!(
            handle_tool(BLAST_RADIUS_TOOL, &serde_json::json!({})).is_some(),
            "blast radius advertised but not wired"
        );
    }

    #[test]
    fn parse_code_skeleton_formats_definitions_and_preserves_doc() {
        let json = r#"{
            "path": "src/identity.rs",
            "items": [
                {
                    "symbol": "AgentId",
                    "kind": "struct_item",
                    "start_line": 31,
                    "end_line": 38,
                    "signature": "pub struct AgentId",
                    "doc": "/// One worker, as the harness understands it."
                }
            ]
        }"#;
        let out = parse_code_skeleton(json).expect("parses skeleton");
        assert!(out.contains("Skeleton — src/identity.rs"));
        assert!(out.contains("L31-L38 [struct_item] AgentId: `pub struct AgentId`"));
        assert!(out.contains("/// One worker, as the harness understands it."));

        let formatted_json = r##"{"formatted": "# Pre-rendered markdown skeleton\n"}"##;
        let out_formatted = parse_code_skeleton(formatted_json).expect("parses formatted");
        assert_eq!(out_formatted, "# Pre-rendered markdown skeleton\n");
    }

    #[test]
    fn parse_blast_radius_formats_changed_files_and_impacted_tests() {
        let clean_json = r#"{"status":"clean","affected_symbols":0,"changed_files":0,"impacted_tests":[],"message":"No changes."}"#;
        let clean_out = parse_blast_radius(clean_json).expect("parses clean blast radius");
        assert!(clean_out.contains("Blast radius clean: No changes."));

        let dirty_json = r#"{
            "status": "dirty",
            "affected_symbols": 3,
            "changed_files": 1,
            "impacted_tests": ["tests::test_agent_isolation", "tests::test_ledger_verify"]
        }"#;
        let dirty_out = parse_blast_radius(dirty_json).expect("parses dirty blast radius");
        assert!(dirty_out.contains("1 file(s) changed, 3 symbol(s) affected"));
        assert!(dirty_out.contains("tests::test_agent_isolation"));
        assert!(dirty_out.contains("tests::test_ledger_verify"));
    }

    #[test]
    fn work_query_subject_includes_agent_fingerprint() {
        let agent = crate::identity::AgentId::new("backend-x", "grunt");
        let attr = crate::identity::Attribution::new(agent.clone());
        let subj = work_query_subject(&attr);
        assert!(subj.ends_with(&format!("#{}", agent.fingerprint())));
    }
}
