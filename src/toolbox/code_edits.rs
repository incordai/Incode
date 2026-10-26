//! The memory engine's edit check, around every file this toolbox writes and
//! every file it reads.
//!
//! incode is the harness: the engine learns who changed which definition, and
//! refuses a write over a change the agent never saw, from THIS executor — not
//! from any agent CLI's hooks or edit events (the engine's design,
//! `per-project-code-index.md` §4.9, decision D9).
//!
//! - **Before a write**, per file: `POST /v1/memory/code/edits/mark` with
//!   `phase: pending`, naming the byte ranges of the file as it stands that the
//!   write replaces. The engine checks every definition those ranges touch
//!   against what this session last read of it. A refusal is the call's result
//!   and nothing is written.
//! - **After it**: `phase: confirmed` when the write landed — the engine
//!   reindexes the file at once and credits what changed to this agent — or
//!   `phase: cancelled` when it did not.
//! - **After a read**: `POST /v1/memory/code/edits/read`, for the lines shown.
//!
//! The check is the engine's, and only where it has one: a folder that is not
//! one of its projects, a file its code index does not cover, or an engine that
//! does not answer leaves the call to run unchecked, as it did before. That is
//! not a gap in the guard — the pre-tool gate has already refused every change
//! when the engine cannot decide — it is the edit check having nothing to say.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use similar::{DiffTag, TextDiff};

use super::{ToolOutput, Toolbox, APPLY_PATCH, EDIT, MULTI_EDIT, NOTEBOOK_EDIT, WRITE};

/// The route a write is announced, confirmed and cancelled on.
pub const MARK_PATH: &str = "/v1/memory/code/edits/mark";
/// The route a read is recorded on.
pub const READ_PATH: &str = "/v1/memory/code/edits/read";
/// The route a session's end is told on: its claims released, its reads and
/// marks dropped.
pub const END_PATH: &str = "/v1/memory/code/edits/end";
/// The route a shell command's window is opened and closed on: a file the
/// command changes inside it is credited to the agent that ran it
/// (`code_edits_api.rs` `shell` in the engine).
pub const SHELL_PATH: &str = "/v1/memory/code/edits/shell";

/// The agent the operator started, as the engine names it; a sub-agent is
/// `incode/sub-1`, its own sub-agent `incode/sub-1/sub-1`.
pub const ROOT_AGENT: &str = "incode";

/// Most byte ranges one announcement names; the engine takes no more. A write
/// that changes more places than this is announced as the whole file.
pub const MAX_RANGES: usize = 1_000;

/// How long one call to the engine may take. A confirm reindexes the file, so
/// this is not a ping's bound.
const CALL_BOUND: Duration = Duration::from_secs(15);

/// What `/code/edits/shell` is sent for one command's window: the checkout,
/// the phase, who ran it, and — opening it — the command, cut to the 1,000
/// characters the engine keeps of it.
#[must_use]
pub fn shell_body(
    workspace: &std::path::Path,
    phase: &str,
    agent_id: &str,
    session_id: &str,
    command: &str,
    call_id: &str,
) -> Value {
    let mut body = json!({
        "cwd": workspace.to_string_lossy(),
        "phase": phase,
        "agent_id": agent_id,
        "session_id": session_id,
    });
    if phase == "begin" {
        body["command"] = json!(command.chars().take(1_000).collect::<String>());
    }
    if !call_id.is_empty() {
        body["tool_use_id"] = json!(call_id);
    }
    body
}

/// The routes, so a test's ledger can tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Mark,
    Read,
    End,
    Shell,
}

/// Why a call to the engine brought no answer to act on. Either way it means
/// "no check here", never "refused".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerError {
    /// Nobody answered: no engine, a refused connection, a timeout. The
    /// rest of the call's files are not asked either — each would wait for
    /// the same silence.
    NoAnswer(String),
    /// The engine answered and declined this one: a folder that is not one
    /// of its projects, a path outside the checkout.
    Declined(String),
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAnswer(why) => write!(f, "no answer: {why}"),
            Self::Declined(why) => write!(f, "declined: {why}"),
        }
    }
}

/// Where the announcements go: the engine over HTTP, or a test's recorder.
pub trait EditLedger: Send + Sync {
    fn post(&self, route: Route, body: &Value) -> Result<Value, LedgerError>;
}

/// The engine at `base` (`http://127.0.0.1:9180`).
pub struct EngineLedger {
    base: String,
}

impl EngineLedger {
    /// The engine at [`crate::brief::memory_url`], when that is this machine.
    ///
    /// Only this machine's: the engine resolves the paths it is given on its
    /// own disk, so an engine anywhere else could check nothing, and every
    /// write would pay two round trips to be told so.
    #[must_use]
    pub fn locate() -> Option<Self> {
        let base = crate::brief::memory_url();
        is_loopback(&base).then(|| Self::at(&base))
    }

    /// The engine at `base`, for a caller that has already chosen it.
    #[must_use]
    pub fn at(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
        }
    }
}

impl EditLedger for EngineLedger {
    fn post(&self, route: Route, body: &Value) -> Result<Value, LedgerError> {
        let url = format!(
            "{}{}",
            self.base,
            match route {
                Route::Mark => MARK_PATH,
                Route::Read => READ_PATH,
                Route::End => END_PATH,
                Route::Shell => SHELL_PATH,
            }
        );
        let body = body.clone();
        let answered = crate::runtime::http::run_async(async move {
            let response = match crate::runtime::http::client_for(&url)
                .post(&url)
                .timeout(CALL_BOUND)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(e) => {
                    return Ok(Err(LedgerError::NoAnswer(
                        crate::runtime::http::error_chain(&e),
                    )))
                }
            };
            let status = response.status();
            let text = match response.text().await {
                Ok(text) => text,
                Err(e) => {
                    return Ok(Err(LedgerError::NoAnswer(format!(
                        "reading the answer failed: {e}"
                    ))))
                }
            };
            if !status.is_success() {
                // The engine's own words: "not in a project" reads very
                // differently from a missing route in the trace.
                return Ok(Err(LedgerError::Declined(format!(
                    "the engine answered {status}: {}",
                    crate::text::truncate(&text, 300)
                ))));
            }
            Ok(serde_json::from_str(&text).map_err(|e| {
                LedgerError::Declined(format!("the engine's answer is not JSON: {e}"))
            }))
        });
        answered.map_err(LedgerError::NoAnswer)?
    }
}

/// Whether `url` names this machine.
fn is_loopback(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Sub-agents started in this process so far: each one's number, so no two
/// are ever one writer — not two started by different backends of one
/// session, which would each count from one.
static SUB_AGENTS: AtomicU32 = AtomicU32::new(0);

/// One agent's side of the edit check: where it is announced, and as whom.
#[derive(Clone)]
pub struct CodeEdits {
    ledger: Arc<dyn EditLedger>,
    /// Empty for the operator's agent; `/sub-3` for a sub-agent, and
    /// `/sub-3/sub-7` for one of its own.
    scope: String,
}

impl CodeEdits {
    /// The operator's agent, announced to `ledger`.
    #[must_use]
    pub fn new(ledger: Arc<dyn EditLedger>) -> Self {
        Self {
            ledger,
            scope: String::new(),
        }
    }

    /// The operator's agent, announced to this machine's engine — `None`
    /// when the engine is configured elsewhere ([`EngineLedger::locate`]).
    #[must_use]
    pub fn locate() -> Option<Self> {
        EngineLedger::locate().map(|ledger| Self::new(Arc::new(ledger)))
    }

    /// A sub-agent's: the same engine, a name and a conversation of its own.
    ///
    /// Its own session as the engine sees it, too, not only its own name. A
    /// sub-agent is pinned to its parent's session, but it is another
    /// conversation: what it read is not what its parent read. Under one
    /// session id the engine would take the sub-agent's write as its parent's
    /// read, and pass the parent's next write over it unseen.
    #[must_use]
    pub fn child(&self) -> Self {
        let n = SUB_AGENTS.fetch_add(1, Ordering::Relaxed) + 1;
        Self {
            ledger: Arc::clone(&self.ledger),
            scope: format!("{}/sub-{n}", self.scope),
        }
    }

    /// Who the engine is told wrote: `incode`, `incode/sub-3`, …
    #[must_use]
    pub fn agent_id(&self) -> String {
        format!("{ROOT_AGENT}{}", self.scope)
    }

    /// The conversation the engine files reads and marks under: `session`
    /// itself for the operator's agent, `session/sub-3` for a sub-agent.
    #[must_use]
    pub fn session_of(&self, session: &str) -> String {
        format!("{session}{}", self.scope)
    }
}

/// One file a write was announced for.
#[derive(Debug, Clone)]
struct Target {
    /// The folder the engine resolves the project from.
    cwd: String,
    /// The file, absolute.
    path: String,
    /// What it held when announced ([`fingerprint`]), so a call that failed
    /// after changing it is confirmed, not cancelled.
    before: Option<u64>,
}

/// A fingerprint of the file at `path` as it is now; `None` when it cannot be
/// read (it does not exist).
fn fingerprint(path: &str) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(path).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    Some(h.finish())
}

/// What was announced before a call ran: the files to confirm or cancel once
/// it has, and what the engine said to add to its result.
#[derive(Debug, Default)]
pub(super) struct Announced {
    targets: Vec<Target>,
    warnings: Vec<String>,
}

/// What one write will change in one file: the byte ranges of the file as it
/// stands, or `None` for all of it (a new, deleted or unreadable file, or a
/// write whose effect could not be worked out beforehand).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Planned {
    pub(super) named: String,
    pub(super) ranges: Option<Vec<[u64; 2]>>,
}

impl Toolbox {
    /// Announce the write `name` is about to make, file by file, and stop it
    /// when the engine refuses any file — withdrawing what was already
    /// announced for the others, since none of them will be written either.
    pub(super) fn announce_writes(
        &self,
        call_id: &str,
        name: &str,
        input: &Value,
    ) -> Result<Announced, String> {
        let mut announced = Announced::default();
        let Some(edits) = &self.code_edits else {
            return Ok(announced);
        };
        if self.session.is_empty() {
            crate::trace!(
                "edits",
                "{name} unchecked: no session id to announce it under"
            );
            return Ok(announced);
        }
        for planned in self.plan_writes(name, input) {
            if self.stopped() {
                break;
            }
            let Ok(path) = self.resolve(&planned.named) else {
                // Refused as an escape by the tool itself, which writes nothing.
                continue;
            };
            let path = path.to_string_lossy().into_owned();
            let target = Target {
                cwd: self
                    .root_for_path(&planned.named)
                    .to_string_lossy()
                    .into_owned(),
                before: fingerprint(&path),
                path,
            };
            let mut body = self.mark_body(edits, call_id, &target, "pending");
            if let Some(ranges) = &planned.ranges {
                body["ranges"] = json!(ranges);
            }
            match edits.ledger.post(Route::Mark, &body) {
                Ok(said) if said.get("allowed").and_then(Value::as_bool) == Some(false) => {
                    self.settle_targets(edits, call_id, &announced.targets, "cancelled");
                    let refusal = said
                        .get("refusal")
                        .and_then(Value::as_str)
                        .unwrap_or("the engine refused this write");
                    let file = said
                        .get("file")
                        .and_then(Value::as_str)
                        .unwrap_or(&planned.named);
                    crate::trace!("edits", "{name} REFUSED for {file}: {refusal}");
                    return Err(format!(
                        "NOT WRITTEN — the edit check refused `{file}`; no file was changed.\n{refusal}"
                    ));
                }
                Ok(said) => {
                    announced.warnings.extend(strings(&said, "warnings"));
                    announced.targets.push(target);
                }
                Err(LedgerError::Declined(why)) => {
                    crate::trace!("edits", "{name} unchecked for {}: {why}", target.path);
                }
                Err(LedgerError::NoAnswer(why)) => {
                    crate::trace!(
                        "edits",
                        "{name} unchecked from {}: no answer: {why}",
                        target.path
                    );
                    break;
                }
            }
        }
        Ok(announced)
    }

    /// Once the call ran: confirm every announced file when it succeeded, so
    /// the engine credits the change to this agent at once, or cancel them
    /// when it failed. A confirmed write's warnings, and any update of
    /// someone else's the engine thinks it overwrote, travel with the result.
    pub(super) fn settle_writes(
        &self,
        call_id: &str,
        announced: Announced,
        outcome: Result<ToolOutput, String>,
    ) -> Result<ToolOutput, String> {
        let Some(edits) = &self.code_edits else {
            return outcome;
        };
        if announced.targets.is_empty() {
            return outcome;
        }
        let Ok(mut out) = outcome else {
            // Failed is not always untouched: a patch whose put-back failed,
            // a write cut short. What changed is confirmed as this agent's, so
            // the watcher does not credit it to a human; the rest is withdrawn.
            let (changed, untouched): (Vec<Target>, Vec<Target>) = announced
                .targets
                .into_iter()
                .partition(|t| fingerprint(&t.path) != t.before);
            self.settle_targets(edits, call_id, &changed, "confirmed");
            self.settle_targets(edits, call_id, &untouched, "cancelled");
            return outcome;
        };
        let mut notes = announced.warnings;
        for said in self.settle_targets(edits, call_id, &announced.targets, "confirmed") {
            if let Some(lost) = said.get("lost_update_suspected").and_then(Value::as_array) {
                notes.extend(
                    lost.iter()
                        .filter_map(|l| l.get("message").and_then(Value::as_str))
                        .map(|m| format!("LOST UPDATE SUSPECTED: {m}")),
                );
            }
        }
        if !notes.is_empty() {
            let lines: Vec<String> = notes.iter().map(|n| format!("[edit check] {n}")).collect();
            out.text = format!("{}\n\n{}", out.text, lines.join("\n"));
        }
        Ok(out)
    }

    /// Tell the engine `phase` for every target; the answers that came back.
    fn settle_targets(
        &self,
        edits: &CodeEdits,
        call_id: &str,
        targets: &[Target],
        phase: &str,
    ) -> Vec<Value> {
        let mut answers = Vec::new();
        for target in targets {
            match edits
                .ledger
                .post(Route::Mark, &self.mark_body(edits, call_id, target, phase))
            {
                Ok(said) => answers.push(said),
                // Unconfirmed, the change is still found by the engine's
                // watcher — as nobody's; uncancelled, the mark lapses.
                Err(why @ LedgerError::Declined(_)) => {
                    crate::trace!("edits", "{phase} not recorded for {}: {why}", target.path);
                }
                Err(why @ LedgerError::NoAnswer(_)) => {
                    crate::trace!(
                        "edits",
                        "{phase} not recorded from {} on: {why}",
                        target.path
                    );
                    break;
                }
            }
        }
        answers
    }

    fn mark_body(&self, edits: &CodeEdits, call_id: &str, target: &Target, phase: &str) -> Value {
        let mut body = json!({
            "cwd": target.cwd,
            "path": target.path,
            "phase": phase,
            "agent_id": edits.agent_id(),
            "session_id": edits.session_of(&self.session),
        });
        if !call_id.is_empty() {
            body["tool_use_id"] = json!(call_id);
        }
        body
    }

    /// Tell the engine that `edits`' conversation in this session is over —
    /// a sub-agent that finished, or this agent's session ending: its claims
    /// are released and its reads and marks dropped, rather than holding the
    /// next agent off until their TTL runs out. Nothing to say without a
    /// session; an engine that does not answer leaves the TTL to do it.
    pub(super) fn end_code_session(&self, edits: &CodeEdits) {
        if self.session.is_empty() {
            return;
        }
        let body = json!({
            "cwd": self.workspace.to_string_lossy(),
            "session_id": edits.session_of(&self.session),
        });
        match edits.ledger.post(Route::End, &body) {
            Ok(said) => crate::trace!("edits", "session {} ended: {said}", body["session_id"]),
            Err(why) => crate::trace!("edits", "session {} not ended: {why}", body["session_id"]),
        }
    }

    /// Open (`begin`) or close (`end`) the engine's window around one shell
    /// command, so a file it changes is credited to this agent rather than to
    /// nobody: a write through the shell is not a write tool, and has no mark
    /// of its own (`announce_writes` plans only the write tools).
    ///
    /// Foreground commands only, called around `run_shell` in
    /// `Toolbox::run_tool`. A background job is given none: the engine
    /// credits nobody while two windows overlap, so one held open for as long
    /// as a job runs would hide every foreground command's changes meanwhile.
    /// Bounded, like every call here; no answer is traced, never a refusal.
    pub(super) fn shell_window(&self, phase: &str, command: &str, call_id: &str) {
        let Some(edits) = &self.code_edits else {
            return;
        };
        if self.session.is_empty() || command.trim().is_empty() {
            return;
        }
        let body = shell_body(
            &self.workspace,
            phase,
            &edits.agent_id(),
            &edits.session_of(&self.session),
            command,
            call_id,
        );
        if let Err(why) = edits.ledger.post(Route::Shell, &body) {
            crate::trace!("edits", "shell window {phase} not told: {why}");
        }
    }

    /// This agent's session is over (the REPL or `--acp` session closing or
    /// cleared, a `--task` run or a `/run` worker ending): let go of what it
    /// claimed, read and announced. Nothing without the edit check.
    pub fn end_code_edits(&self) {
        if let Some(edits) = &self.code_edits {
            self.end_code_session(edits);
        }
    }

    /// Record, after a Read that succeeded, what it showed. Waited on, and
    /// bounded: the next write's check compares against this, and a model can
    /// ask for that write the moment the read comes back.
    pub(super) fn record_read(&self, input: &Value) {
        let Some(edits) = &self.code_edits else {
            return;
        };
        if self.session.is_empty() {
            return;
        }
        let Some(named) = input.get("path").and_then(Value::as_str) else {
            return;
        };
        let Ok(path) = self.resolve(named) else {
            return;
        };
        // A document read as markdown shows no source; nothing to record.
        if crate::tools::documents::Kind::of(&path).is_some() {
            return;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let limit = input.get("limit").and_then(Value::as_u64);
        // Known limit: the engine records the definitions as they are when this
        // arrives, a round trip after the tool read them. A write landing in
        // between is taken as seen; the route takes no hash of what was shown.
        let mut body = json!({
            "cwd": self.root_for_path(named).to_string_lossy(),
            "path": path.to_string_lossy(),
            "session_id": edits.session_of(&self.session),
            "via": "file_read",
        });
        if let Some(range) = shown_range(&text, offset, limit) {
            body["ranges"] = json!([range]);
        }
        if let Err(why) = edits.ledger.post(Route::Read, &body) {
            crate::trace!("edits", "read not recorded for {}: {why}", path.display());
        }
    }

    /// Every file `name` will write and what it will change in each, worked
    /// out from the files as they stand — the same arithmetic the tool does,
    /// without the write.
    fn plan_writes(&self, name: &str, input: &Value) -> Vec<Planned> {
        let path_of = || {
            input
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let current = |named: &str| {
            self.resolve(named)
                .ok()
                .and_then(|p| std::fs::read_to_string(p).ok())
        };
        match name {
            WRITE => {
                let Some(named) = path_of() else {
                    return Vec::new();
                };
                let content = input.get("content").and_then(Value::as_str).unwrap_or("");
                planned(named.clone(), current(&named).as_deref(), |_| {
                    Some(content.to_string())
                })
            }
            EDIT => {
                let Some(named) = path_of() else {
                    return Vec::new();
                };
                // As the tool does it: the one place `old_string` matches.
                let edit = [json!({
                    "old_string": input.get("old_string").and_then(Value::as_str).unwrap_or(""),
                    "new_string": input.get("new_string").and_then(Value::as_str).unwrap_or(""),
                })];
                planned(named.clone(), current(&named).as_deref(), |text| {
                    multi_edited(text, &edit)
                })
            }
            MULTI_EDIT => {
                let Some(named) = path_of() else {
                    return Vec::new();
                };
                let edits = input
                    .get("edits")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                planned(named.clone(), current(&named).as_deref(), |text| {
                    multi_edited(text, &edits)
                })
            }
            APPLY_PATCH => {
                let Some(patch) = input.get("patch").and_then(Value::as_str) else {
                    return Vec::new();
                };
                let Ok(patches) = crate::tools::diff::parse_unified(patch) else {
                    return Vec::new();
                };
                patches
                    .iter()
                    .map(|p| {
                        let named = p.target().to_string();
                        let before = current(&named);
                        // A created or deleted file is all of it.
                        let after = match (&p.old_path, &p.new_path) {
                            (Some(_), Some(_)) => before
                                .as_deref()
                                .and_then(|t| crate::tools::diff::apply_hunks(t, &p.hunks).ok()),
                            _ => None,
                        };
                        let ranges = before
                            .as_deref()
                            .zip(after.as_deref())
                            .map(|(b, a)| changed_ranges(b, a));
                        Planned { named, ranges }
                    })
                    .filter(|p| p.ranges.as_ref().is_none_or(|r| !r.is_empty()))
                    .map(|p| Planned {
                        ranges: p.ranges.filter(|r| r.len() <= MAX_RANGES),
                        ..p
                    })
                    .collect()
            }
            NOTEBOOK_EDIT => path_of()
                .map(|named| Planned {
                    named,
                    ranges: None,
                })
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// The plan for one file whose new text `after` works out from its current
/// text: nothing when the text would not change, all of it when the file is
/// new or the result could not be worked out.
fn planned(
    named: String,
    before: Option<&str>,
    after: impl FnOnce(&str) -> Option<String>,
) -> Vec<Planned> {
    let Some(before) = before else {
        return vec![Planned {
            named,
            ranges: None,
        }];
    };
    let ranges = match after(before) {
        Some(after) if after == before => return Vec::new(),
        Some(after) => Some(changed_ranges(before, &after)).filter(|r| r.len() <= MAX_RANGES),
        None => None,
    };
    vec![Planned { named, ranges }]
}

/// `text` after MultiEdit's `edits`, applied in order exactly as the tool
/// applies them; `None` when one would not apply (the tool then fails, and
/// the whole file was announced).
fn multi_edited(text: &str, edits: &[Value]) -> Option<String> {
    let mut out = text.to_string();
    for edit in edits {
        let old = edit.get("old_string").and_then(Value::as_str)?;
        let new = edit.get("new_string").and_then(Value::as_str)?;
        let all = edit
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let count = if old.is_empty() {
            0
        } else {
            out.matches(old).count()
        };
        if count == 0 || (count > 1 && !all) {
            return None;
        }
        out = if all {
            out.replace(old, new)
        } else {
            out.replacen(old, new, 1)
        };
    }
    Some(out)
}

/// The byte ranges of `before` that differ in `after`, by whole lines: each
/// changed or removed run of lines, and for lines only inserted, the line
/// before and the line after the insertion point.
///
/// Both neighbours because an insertion changes the definition it lands in
/// even at its edge — a doc comment added right above a function is that
/// function's first bytes, a line appended to a Python body is past its last
/// — and the engine checks only what a range overlaps.
///
/// Lines are the diff's own, so an offset is where the diff's line starts:
/// it also ends a line at a lone `\r`.
pub(super) fn changed_ranges(before: &str, after: &str) -> Vec<[u64; 2]> {
    let diff = TextDiff::from_lines(before, after);
    let mut starts = vec![0u64];
    let mut at = 0u64;
    for line in diff.old_slices() {
        at += line.len() as u64;
        starts.push(at);
    }
    let lines = starts.len() - 1;
    let byte = |line: usize| starts[line.min(lines)];
    diff.ops()
        .iter()
        .map(similar::DiffOp::as_tag_tuple)
        .filter(|(tag, _, _)| *tag != DiffTag::Equal)
        .map(|(_, old, _)| match old.is_empty() {
            true => [byte(old.start.saturating_sub(1)), byte(old.start + 1)],
            false => [byte(old.start), byte(old.end)],
        })
        .collect()
}

/// The bytes a Read of `offset` and `limit` lines showed, or `None` for the
/// whole file. Counted the way the tool counts: `offset` lines skipped.
pub(super) fn shown_range(text: &str, offset: u64, limit: Option<u64>) -> Option<[u64; 2]> {
    let lines = text.lines().count() as u64;
    let end = limit.map_or(lines, |l| offset.saturating_add(l).min(lines));
    if offset == 0 && end >= lines {
        return None;
    }
    let mut starts = vec![0u64];
    let mut at = 0u64;
    for line in text.split_inclusive('\n') {
        at += line.len() as u64;
        starts.push(at);
    }
    let byte = |line: u64| {
        usize::try_from(line)
            .ok()
            .and_then(|l| starts.get(l).copied())
            .unwrap_or(at)
    };
    Some([byte(offset), byte(end)])
}

/// The strings of the array `key` in `said`.
fn strings(said: &Value, key: &str) -> Vec<String> {
    said.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "code_edits_tests.rs"]
mod tests;
