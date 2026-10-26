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

/// Where a durable lesson is written: the engine's `lesson_write`, over REST.
///
/// It was `/v1/memory/remember` until the engine retired that tool for
/// `lesson_write` — after which every save from here answered 404 (measured
/// 2026-10-26), the model's `remember`, `/remember` and the runner's record of
/// finished work alike, and nothing on this side said why.
pub const REMEMBER_PATH: &str = "/v1/memory/lesson_write";

/// Where the ranked snippet search lives.
pub const SEARCH_PATH: &str = "/v1/memory/search";

/// Where one stored message is corrected or removed.
pub const MESSAGE_PATH: &str = "/v1/memory/messages";

/// Where the engine serves its tools — the code index's among them, which the
/// three code tools below ask for the checkout the session works in.
pub const MCP_PATH: &str = "/v1/memory/mcp";

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

/// The kinds `lesson_write` keeps that one line of text can carry, pinned here
/// so a rejected kind is caught before a round trip rather than after one.
///
/// The engine's own list is issue, finding, decision, convention, fact,
/// preference, todo (its refusal names them). `issue` is left out because it
/// needs fields one line cannot hold; `entity` is gone because the engine keeps
/// entities as graph nodes, not as memories.
pub const KINDS: [&str; 6] = [
    "fact",
    "decision",
    "preference",
    "todo",
    "convention",
    "finding",
];

/// One durable thing to save, and where it belongs.
///
/// Built by [`run_remember`] for the model, by `/remember`
/// (`cli/slash_commands/memory.rs`) and by the runner's journal
/// (`runner/seams.rs` `MemoryJournal`) — the three writers of a lesson.
#[derive(Debug, Clone)]
pub struct Lesson<'a> {
    pub kind: &'a str,
    pub text: &'a str,
    /// The reason. Required for a `decision`, which the engine refuses without.
    pub why: &'a str,
    /// A project, person or file the lesson is about; filed as a tag.
    pub subject: &'a str,
    /// A folder inside the project — how the engine decides which project's
    /// memory this lands in, and which checkout `locations` are checked in.
    pub cwd: &'a std::path::Path,
    /// The session the lesson was learned in, so it can be traced back.
    pub session: &'a str,
    /// Code it is about, as `[{file, symbol}]`. The engine checks each one
    /// against the checkout and links the lesson to it.
    pub locations: Option<&'a serde_json::Value>,
}

impl<'a> Lesson<'a> {
    /// A lesson of `kind` saying `text`, filed in the project `cwd` is in. The
    /// folder is not optional: the engine refuses a lesson that names no project.
    #[must_use]
    pub fn new(kind: &'a str, text: &'a str, cwd: &'a std::path::Path) -> Self {
        Self {
            kind,
            text,
            why: "",
            subject: "",
            cwd,
            session: "",
            locations: None,
        }
    }
}

/// The `lesson_write` body for `lesson`, or why there is none.
///
/// Pure, so the shape the engine reads (`project_stores/tools.rs` `write`,
/// and the engine's own CLI `cli/mem_cmds.rs`, whose mapping this mirrors) is
/// checked without one running.
pub fn lesson_body(lesson: &Lesson<'_>) -> Result<serde_json::Value, String> {
    let text = lesson.text.trim();
    if text.is_empty() {
        return Err("nothing to remember: the text is empty".to_string());
    }
    let why = lesson.why.trim();
    let mut record = match lesson.kind {
        "fact" | "preference" | "todo" => serde_json::json!({ "text": text }),
        "decision" if why.is_empty() => {
            return Err("a decision needs `why`: the reason it was made".to_string())
        }
        "decision" => serde_json::json!({ "decision": text, "why": why }),
        "convention" if why.is_empty() => serde_json::json!({ "convention": text }),
        "convention" => serde_json::json!({ "convention": text, "why": why }),
        "finding" => serde_json::json!({ "issue": text }),
        other => {
            return Err(format!(
                "`{other}` is not a kind this store keeps. Use one of: {}",
                KINDS.join(", ")
            ))
        }
    };
    let subject = lesson.subject.trim();
    if !subject.is_empty() {
        record["tags"] = serde_json::json!([subject]);
    }
    let mut body = serde_json::json!({
        "kind": lesson.kind,
        "record": record,
        "cwd": lesson.cwd.display().to_string(),
        "agent_id": crate::toolbox::ROOT_AGENT,
    });
    if !lesson.session.trim().is_empty() {
        body["session_id"] = serde_json::json!(lesson.session.trim());
    }
    if let Some(locations) = lesson.locations.filter(|l| !l.is_null()) {
        if !locations.is_array() {
            return Err("`locations` must be a list of {file, symbol}".to_string());
        }
        body["locations"] = locations.clone();
    }
    Ok(body)
}

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

/// Save a durable lesson.
///
/// Returns what the store assigned — its id, and whether it was already
/// held — so a caller can say what to correct later.
pub fn remember(lesson: &Lesson<'_>) -> Result<String, String> {
    let body = lesson_body(lesson)?.to_string();
    let url = url(REMEMBER_PATH);
    let answer = crate::runtime::http::request_text_explained(
        move |c| {
            c.post(&url)
                .header("content-type", "application/json")
                .body(body)
                .timeout(TIMEOUT)
        },
        "memory service",
    )?;
    parse_remember(&answer)
}

/// Read what a save did out of `lesson_write`'s answer.
///
/// The REST bridge answers with the tool's OWN JSON (`tools_api.rs`
/// `rest_body`), no longer the `{content:[{text}]}` envelope this once
/// unwrapped — reading the envelope now would turn every successful save into
/// "no usable result". The envelope is still read, so an engine from before
/// that change is not misread either.
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
    let enveloped = value
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|f| f.get("text"))
        .and_then(|t| t.as_str());
    let payload = match enveloped {
        Some(inner) => serde_json::from_str(inner)
            .map_err(|e| format!("the store's answer carried no usable result: {e}"))?,
        None => value,
    };
    if !payload
        .get("stored")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        // "Accepted but not stored" is the one outcome that reads as success and
        // is not: the caller would report a fact saved that nobody can recall.
        return Err(payload.get("error").and_then(|e| e.as_str()).map_or_else(
            || "the store accepted the call but did not store anything".to_string(),
            |e| format!("the store did not save it: {e}"),
        ));
    }
    let id = ["id", "msg_id"]
        .iter()
        .find_map(|k| payload.get(*k).and_then(|v| v.as_str()))
        .unwrap_or("saved");
    let held = payload
        .get("duplicate")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(if held {
        format!("{id} (it was already held)")
    } else {
        id.to_string()
    })
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
            "description": "Save one durable lesson — a fact, decision, preference, \
                todo, convention or finding — to this project's shared memory, which \
                every agent on this machine reads. Use it when something worth \
                recalling in a later session comes up. Point at code with \
                `locations` rather than quoting it: the lesson is then linked to \
                those symbols and surfaces when they are worked on. This is the \
                memory rules' `lesson_write`. Never save secrets, credentials, API \
                keys, internal URLs, or raw source.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "The thing to remember, stated plainly."},
                    "kind": {"type": "string", "enum": KINDS, "description": "What kind of knowledge this is."},
                    "why": {"type": "string", "description": "The reason. Required for a decision; optional for a convention."},
                    "subject": {"type": "string", "description": "Optional: the project, person or file it is about."},
                    "locations": {
                        "type": "array",
                        "description": "Optional: the code it is about.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "file": {"type": "string", "description": "Path relative to the workspace."},
                                "symbol": {"type": "string", "description": "A function, type or constant in it."}
                            },
                            "required": ["file"]
                        }
                    }
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
                line range — from the code index of the checkout this session works in. Use \
                it instead of searching when you know the name: it answers with the \
                definition rather than with every place the word appears. `Type::method` \
                narrows to one type's.",
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
            "description": "Every definition in one file as signatures without bodies, in \
                source order, with line spans (~10x cheaper than reading the file).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "The file, absolute or relative to the workspace."}
                },
                "required": ["path"]
            }
        }),
        serde_json::json!({
            "name": BLAST_RADIUS_TOOL,
            "description": "The blast radius of the uncommitted changes in this checkout: \
                the definitions they touch, how far each reaches through its callers \
                (depth 1-5), and the tests among those callers — the ones to run.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "depth": {"type": "integer", "description": "Caller walk depth, 1-5 (default 2)."},
                    "repo_path": {"type": "string", "description": "Another checkout to measure instead of this session's."}
                }
            }
        }),
    ]
}

/// What these tools need of the session: the folder the engine resolves the
/// project and checkout from, the session a lesson is filed under, and how a
/// path the model named is resolved — the way the file tools resolve it,
/// inside the workspace.
pub struct CodeContext<'a> {
    pub cwd: &'a std::path::Path,
    pub session: &'a str,
    pub resolve: &'a dyn Fn(&str) -> Result<std::path::PathBuf, String>,
}

/// Run one of this module's tools. `None` when the name is not one of them.
pub fn handle_tool(
    name: &str,
    input: &serde_json::Value,
    code: &CodeContext<'_>,
) -> Option<Result<String, String>> {
    match name {
        REMEMBER_TOOL => Some(run_remember(input, code)),
        SEARCH_TOOL => Some(run_search(input)),
        FORGET_TOOL => Some(run_forget(input)),
        SYMBOL_TOOL => Some(run_symbol(input, code)),
        SKELETON_TOOL => Some(run_skeleton(input, code)),
        BLAST_RADIUS_TOOL => Some(run_blast_radius(input, code)),
        _ => None,
    }
}

/// Ask the checkout's code index where a name is defined (`code_find_symbol`).
///
/// # Why this instead of a language server
///
/// Going to a definition is most of what a language server is wanted for here,
/// and the engine already indexes every checkout it knows — so this is one
/// request rather than a server per language, each with its own lifecycle to
/// manage. What it does NOT give is hover types, completions or
/// rename-across-project; if those are ever needed, they need the real thing.
fn run_symbol(input: &serde_json::Value, code: &CodeContext<'_>) -> Result<String, String> {
    let symbol = input
        .get("symbol")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim();
    if symbol.is_empty() {
        return Err("say which name to look up".to_string());
    }
    engine_tool(
        "code_find_symbol",
        serde_json::json!({ "name": symbol, "cwd": code.cwd }),
    )
}

fn run_remember(input: &serde_json::Value, code: &CodeContext<'_>) -> Result<String, String> {
    let text = |key: &str| input.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let kind = input.get("kind").and_then(|k| k.as_str()).unwrap_or("fact");
    remember(&Lesson {
        kind,
        text: text("text"),
        why: text("why"),
        subject: text("subject"),
        cwd: code.cwd,
        session: code.session,
        locations: input.get("locations"),
    })
    .map(|id| format!("Saved as {id}."))
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

/// One file's definitions as signatures (`code_outline`). The engine takes the
/// file relative to the checkout's root, which need not be the workspace — so
/// the path is resolved the way the file tools resolve it and made relative to
/// the root the engine reports for this session's checkout.
fn run_skeleton(input: &serde_json::Value, code: &CodeContext<'_>) -> Result<String, String> {
    let path = input
        .get("path")
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .trim();
    if path.is_empty() {
        return Err("say which file to outline".to_string());
    }
    let file = (code.resolve)(path)?;
    let status = engine_tool("code_index_status", serde_json::json!({ "cwd": code.cwd }))?;
    let root = serde_json::from_str::<serde_json::Value>(&status)
        .ok()
        .and_then(|v| {
            v.get("root")
                .and_then(|r| r.as_str())
                .map(std::path::PathBuf::from)
        })
        .ok_or_else(|| format!("the code index named no checkout root: {status}"))?;
    let rel = relative_to(&file, &root)
        .ok_or_else(|| format!("`{path}` is not inside the checkout at {}", root.display()))?;
    engine_tool(
        "code_outline",
        serde_json::json!({ "file": rel, "cwd": code.cwd }),
    )
}

/// The blast radius of the checkout's uncommitted changes (`git_blast_radius`
/// on the code index).
fn run_blast_radius(input: &serde_json::Value, code: &CodeContext<'_>) -> Result<String, String> {
    let depth = input
        .get("depth")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(2)
        .clamp(1, 5);
    let cwd = match input
        .get("repo_path")
        .and_then(|r| r.as_str())
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        Some(repo) => (code.resolve)(repo)?,
        None => code.cwd.to_path_buf(),
    };
    engine_tool(
        "git_blast_radius",
        serde_json::json!({ "cwd": cwd, "depth": depth }),
    )
}

/// `file` relative to `root`, `/`-separated, both compared where they really
/// are (a file not written yet by its folder); `None` when it is not under it.
fn relative_to(file: &std::path::Path, root: &std::path::Path) -> Option<String> {
    let real = |p: &std::path::Path| {
        std::fs::canonicalize(p).ok().or_else(|| {
            let parent = std::fs::canonicalize(p.parent()?).ok()?;
            Some(parent.join(p.file_name()?))
        })
    };
    let rel = real(file)?
        .strip_prefix(real(root)?)
        .ok()?
        .to_string_lossy()
        .replace('\\', "/");
    (!rel.is_empty()).then_some(rel)
}

/// Call one of the engine's tools and return what it said.
fn engine_tool(name: &str, arguments: serde_json::Value) -> Result<String, String> {
    let url = format!(
        "{}{MCP_PATH}",
        crate::brief::memory_url().trim_end_matches('/')
    );
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    parse_tool_reply(&post(&url, body.to_string())?)
}

/// The text of an engine tool's answer, or its error.
///
/// Split out so the reading is testable without the engine running: a
/// JSON-RPC error and a tool result flagged `isError` are both failures, and
/// only the text of a successful result is the answer.
pub fn parse_tool_reply(body: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("the engine's answer did not parse: {e}"))?;
    if let Some(error) = value.get("error") {
        return Err(error
            .get("message")
            .and_then(|m| m.as_str())
            .map_or_else(|| error.to_string(), str::to_string));
    }
    let result = value
        .get("result")
        .ok_or("the engine's answer carried no result")?;
    let text = result
        .get("content")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    if result.get("isError").and_then(serde_json::Value::as_bool) == Some(true) {
        return Err(text);
    }
    Ok(text)
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
        let here = std::path::Path::new(".");
        assert!(remember(&Lesson::new("fact", "   ", here)).is_err());
        assert!(remember(&Lesson::new("not-a-kind", "something", here)).is_err());
        // The engine keeps entities as graph nodes, not memories.
        assert!(lesson_body(&Lesson::new("entity", "Ravi", here)).is_err());
    }

    /// The body is what the engine's `lesson_write` reads: the kind's own
    /// fields inside `record`, and the folder that names the project. The old
    /// `{text, kind, subject}` went to a route the engine no longer has.
    #[test]
    fn each_kind_is_sent_in_the_fields_lesson_write_reads() {
        let here = std::path::Path::new("D:/work/app");
        let fact = lesson_body(&Lesson {
            subject: "app",
            session: "s-1",
            ..Lesson::new("fact", "  the build needs --locked  ", here)
        })
        .expect("a fact is sendable");
        assert_eq!(fact["kind"], "fact");
        assert_eq!(fact["record"]["text"], "the build needs --locked");
        assert_eq!(fact["record"]["tags"][0], "app");
        assert_eq!(fact["cwd"], "D:/work/app");
        assert_eq!(fact["session_id"], "s-1");
        assert_eq!(fact["agent_id"], crate::toolbox::ROOT_AGENT);

        let decision = lesson_body(&Lesson {
            why: "the engine retired it",
            ..Lesson::new("decision", "save through lesson_write", here)
        })
        .expect("a decision with its reason is sendable");
        assert_eq!(decision["record"]["decision"], "save through lesson_write");
        assert_eq!(decision["record"]["why"], "the engine retired it");

        let finding = lesson_body(&Lesson::new("finding", "x leaks", here)).expect("sendable");
        assert_eq!(finding["record"]["issue"], "x leaks");
        let convention =
            lesson_body(&Lesson::new("convention", "tests beside code", here)).expect("sendable");
        assert_eq!(convention["record"]["convention"], "tests beside code");
    }

    /// A decision without its reason is refused HERE, in words that name the
    /// fix, rather than after a round trip that answers 400.
    #[test]
    fn a_decision_without_a_reason_is_refused_before_it_is_sent() {
        let here = std::path::Path::new(".");
        let why = lesson_body(&Lesson::new("decision", "do X", here)).expect_err("no why");
        assert!(why.contains("why"), "{why}");
    }

    /// Locations go to the engine as given, so it can check and link them;
    /// anything but a list is refused rather than sent to be misread.
    #[test]
    fn code_locations_ride_along_and_must_be_a_list() {
        let here = std::path::Path::new(".");
        let list = serde_json::json!([{ "file": "src/a.rs", "symbol": "run" }]);
        let body = lesson_body(&Lesson {
            locations: Some(&list),
            ..Lesson::new("fact", "run is the entry", here)
        })
        .expect("sendable");
        assert_eq!(body["locations"][0]["symbol"], "run");

        let not_a_list = serde_json::json!({ "file": "src/a.rs" });
        assert!(lesson_body(&Lesson {
            locations: Some(&not_a_list),
            ..Lesson::new("fact", "t", here)
        })
        .is_err());
    }

    /// The body `lesson_write` answers with today: its own JSON, unwrapped by
    /// the REST bridge (`tools_api.rs` `rest_body`). Reading only the old
    /// envelope turned this success into "no usable result".
    #[test]
    fn the_bare_lesson_write_answer_yields_its_id() {
        let saved = r#"{"id":"m-7f3a","kind":"fact","stored":true,"duplicate":false,
            "updated_existing":null,"similar":[],"topic_id":"t-1","topic_ids":["t-1"]}"#;
        assert_eq!(parse_remember(saved).expect("parses"), "m-7f3a");

        let held = r#"{"id":"m-7f3a","stored":true,"duplicate":true}"#;
        assert!(parse_remember(held)
            .expect("parses")
            .contains("already held"));

        let refused = r#"{"error":"lesson_write: code blocks are not stored","stored":false}"#;
        let why = parse_remember(refused).expect_err("not stored");
        assert!(
            why.contains("code blocks"),
            "the engine's reason is kept: {why}"
        );
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
        let saved = remember(&Lesson::new("fact", "a fact", std::path::Path::new(".")));
        let searched = search("anything", 3);
        std::env::remove_var(crate::brief::MEMORY_URL_ENV);
        assert!(
            saved.is_err(),
            "a lost write must never look saved: {saved:?}"
        );
        assert!(searched.is_err(), "{searched:?}");
    }

    /// The store's own answer, taken from a live call rather than written from
    /// the parser — a shape invented here would pass while the real fields drift.
    #[test]
    fn a_tool_reply_is_its_text_and_an_error_is_an_error() {
        // The engine's tool answer, as the MCP endpoint sends it.
        let ok = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"function · run_turn · src/agent.rs:10-40"}],"isError":false}}"#;
        assert_eq!(
            parse_tool_reply(ok).unwrap(),
            "function · run_turn · src/agent.rs:10-40"
        );

        let flagged = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"not_in_project"}],"isError":true}}"#;
        assert_eq!(parse_tool_reply(flagged).unwrap_err(), "not_in_project");

        let rpc = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"unknown tool"}}"#;
        assert_eq!(parse_tool_reply(rpc).unwrap_err(), "unknown tool");

        // An answer that is not JSON is an error, not an empty result.
        assert!(parse_tool_reply("<html>down</html>").is_err());
    }

    #[test]
    fn a_file_is_named_relative_to_the_checkout_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("crates").join("a").join("src")).unwrap();
        std::fs::write(root.join("crates/a/src/lib.rs"), "").unwrap();
        assert_eq!(
            relative_to(
                &root.join("crates").join("a").join("src").join("lib.rs"),
                root
            )
            .as_deref(),
            Some("crates/a/src/lib.rs")
        );
        // Not written yet: placed by its folder.
        assert_eq!(
            relative_to(
                &root.join("crates").join("a").join("src").join("new.rs"),
                root
            )
            .as_deref(),
            Some("crates/a/src/new.rs")
        );
        // Outside the root, or the root itself: nothing to name.
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("x.rs"), "").unwrap();
        assert_eq!(relative_to(&other.path().join("x.rs"), root), None);
        assert_eq!(relative_to(root, root), None);
    }

    #[test]
    fn the_code_tools_are_advertised_and_wired() {
        let names: Vec<String> = tool_definitions()
            .iter()
            .filter_map(|d| d["name"].as_str().map(str::to_string))
            .collect();
        // Every call below is answered here, before any engine is asked: a test
        // must never reach the engine this machine runs.
        let refuse = |_: &str| Err("outside the workspace".to_string());
        let code = CodeContext {
            cwd: std::path::Path::new("."),
            session: "",
            resolve: &refuse,
        };
        for (tool, input, said) in [
            (
                SYMBOL_TOOL,
                serde_json::json!({}),
                "say which name to look up",
            ),
            (
                SKELETON_TOOL,
                serde_json::json!({}),
                "say which file to outline",
            ),
            (
                SKELETON_TOOL,
                serde_json::json!({ "path": "../x.rs" }),
                "outside the workspace",
            ),
            (
                BLAST_RADIUS_TOOL,
                serde_json::json!({ "repo_path": "../other" }),
                "outside the workspace",
            ),
        ] {
            assert!(
                names.iter().any(|n| n == tool),
                "{tool} not advertised: {names:?}"
            );
            let answer = handle_tool(tool, &input, &code).expect("advertised but not wired");
            assert_eq!(answer.unwrap_err(), said, "{tool} {input}");
        }
    }

    #[test]
    fn work_query_subject_includes_agent_fingerprint() {
        let agent = crate::identity::AgentId::new("backend-x", "grunt");
        let attr = crate::identity::Attribution::new(agent.clone());
        let subj = work_query_subject(&attr);
        assert!(subj.ends_with(&format!("#{}", agent.fingerprint())));
    }
}
