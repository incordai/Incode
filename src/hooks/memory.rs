//! The memory engine's session lifecycle, fired by incode itself.
//!
//! # Why incode fires these
//!
//! incode is the harness; an agent CLI is only the model. The engine records a
//! conversation, hands a session its context and decides whether a finished
//! turn should carry on through five hooks — `session-start`, `user-prompt`,
//! `code-edit`, `capture` (the stop) and `session-end` — that an agent CLI
//! used to fire from its own settings. The claude route now starts its CLI
//! with no settings, and codex and antigravity are never relied on for them, so
//! nothing fires them unless incode does. Without them the owner's memory stops
//! capturing what incode sessions do.
//!
//! One [`MemoryHooks`] per incode session, keyed by incode's own session id —
//! the id the toolbox also gives the engine's pre-tool hook, so the engine's
//! cards, its capture and the context it hands back all name one session.
//!
//! # The transcript
//!
//! The engine records a conversation by reading a transcript: `hook capture`
//! does nothing without `transcript_path`, and neither does `hook session-end`.
//! It reads the shape an agent CLI writes — one JSON object per line with
//! `type` (`user`/`assistant`), `uuid`, `timestamp` and `message.content`, plus
//! the `cwd` it files the project under. incode writes exactly that, per
//! session, at `.incode/transcripts/<session>.jsonl` in the workspace, beside
//! the rest of the workspace's run state. Only when an engine is present, or
//! when the session is one a client may load again (`incode --acp`'s
//! `session/load` replays it — [`MemoryHooks::keep_transcript`]): with no
//! reader, the file would be a copy nobody asked for.
//!
//! # Context that does not fit
//!
//! The engine's blocks run to tens of kilobytes, and a CLI model is handed its
//! prompt on a command line with a hard limit — an oversized block would push
//! the person's own instruction off the end. A block over its budget is
//! written whole to `.incode/context/` and the model is given its head and the
//! path, the way an agent CLI persists an oversized hook payload. The engine
//! already puts what matters most at the top of the block for that reason.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::engine::{Conversation, Engine};
use crate::runtime::cancel::CancelToken;

/// How many times one turn may be carried on by the engine.
///
/// The engine's own brake (`stop_hook_active`) is the authority; this is a
/// second fuse, so a stop channel that keeps answering "go on" costs a bounded
/// number of model turns rather than an unbounded one.
pub const MAX_CONTINUATIONS: u8 = 3;

/// The most of the session-start block given inline, in bytes. It rides the
/// system context of every turn.
pub const START_INLINE: usize = 12_000;

/// The most of a turn's block given inline, in bytes. It goes in front of the
/// person's instruction, so it must leave that instruction room.
pub const TURN_INLINE: usize = 6_000;

/// Where transcripts are written, under the workspace.
pub(crate) const TRANSCRIPTS: &str = ".incode/transcripts";

/// Where an oversized block is written whole, under the workspace. Also where
/// `crate::agent::budget_for_spawn` writes a task too long for a command
/// line (src/agent/mod.rs `spill`), so `prune` keeps both bounded.
pub(crate) const CONTEXT: &str = ".incode/context";

/// How many files each of those two directories keeps. The newest survive.
const KEEP: usize = 200;

/// Who said a line of the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    /// The person — or incode speaking for them, as with a continuation.
    Person,
    /// The model.
    Agent,
}

/// What the model said in one run, rebuilt from its events.
///
/// Backends do not agree on what a text event is: one sends each streamed
/// fragment as both a delta and a "whole" block, another sends deltas and then
/// the real whole block. So the streamed fragments are the text wherever there
/// are any, the whole blocks only where there are none, and a block ends where
/// the model turned to a tool.
#[derive(Debug, Clone, Default)]
pub struct Said {
    blocks: Vec<String>,
    /// The last block is still being streamed into.
    open: bool,
    streamed: bool,
}

impl Said {
    /// Take one event of the run into account.
    pub fn hear(&mut self, event: &crate::agent::AgentEvent) {
        use crate::agent::AgentEvent;
        match event {
            AgentEvent::TextDelta(fragment) => {
                if !self.streamed {
                    self.streamed = true;
                    self.blocks.clear();
                    self.open = false;
                }
                match self.blocks.last_mut() {
                    Some(block) if self.open => block.push_str(fragment),
                    _ => {
                        self.blocks.push(fragment.clone());
                        self.open = true;
                    }
                }
            }
            AgentEvent::Text(text) if !self.streamed => self.blocks.push(text.clone()),
            AgentEvent::ToolUse { .. } | AgentEvent::ToolResult { .. } => self.open = false,
            _ => {}
        }
    }

    /// The model's last message of the run: what it stopped on.
    #[must_use]
    pub fn last(&self) -> &str {
        self.blocks
            .iter()
            .rev()
            .map(|b| b.trim())
            .find(|b| !b.is_empty())
            .unwrap_or("")
    }

    /// Everything it said in the run.
    #[must_use]
    pub fn whole(&self) -> String {
        self.blocks
            .iter()
            .map(|b| b.trim())
            .filter(|b| !b.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// One incode session's lifecycle, as the engine is told it.
pub struct MemoryHooks {
    /// `None` when no engine is named or installed: then nothing is fired and
    /// no transcript is written.
    engine: Option<Engine>,
    cwd: PathBuf,
    session: String,
    /// This session's transcript, when one is written.
    transcript: Option<PathBuf>,
    /// Set once a continuation this hook ordered has been sent, until the next
    /// person's line: the next stop then says `stop_hook_active`.
    stop_active: bool,
    /// Continuations sent in this turn.
    continued: u8,
    /// incode's background jobs still running. While there are any, a stop is
    /// not a finished turn and the engine is not asked about it.
    background: usize,
    /// The transcript is written whether or not an engine reads it: the
    /// session can be loaded again (`incode --acp`'s `session/load`), and it
    /// is replayed from this.
    keep: bool,
}

impl MemoryHooks {
    /// The lifecycle of session `session` in workspace `cwd`.
    #[must_use]
    pub fn new(engine: Option<Engine>, cwd: &Path, session: &str) -> Self {
        let transcript = engine.as_ref().map(|_| transcript_for(cwd, session));
        Self {
            engine,
            cwd: cwd.to_path_buf(),
            session: session.to_string(),
            transcript,
            stop_active: false,
            continued: 0,
            background: 0,
            keep: false,
        }
    }

    /// Write the transcript with or without an engine to read it — for a
    /// session a client may load again, which is replayed from it. Nothing
    /// for a lifecycle with no session id.
    pub fn keep_transcript(&mut self) {
        if self.session.is_empty() {
            return;
        }
        self.keep = true;
        if self.transcript.is_none() {
            self.transcript = Some(transcript_for(&self.cwd, &self.session));
        }
    }

    /// A lifecycle that fires nothing: no engine, no transcript.
    #[must_use]
    pub fn off(cwd: &Path) -> Self {
        Self::new(None, cwd, "")
    }

    /// incode's id for this session.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// The workspace this session works in.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn at(&self) -> Conversation<'_> {
        Conversation {
            session: &self.session,
            cwd: &self.cwd,
            // Named only once it exists: a path to nothing is a promise the
            // engine would find broken.
            transcript: self.transcript.as_deref().filter(|p| p.is_file()),
        }
    }

    /// The session opened (`startup`, `resume`) or was cleared (`clear`). The
    /// engine's context for it, bounded, or `None`.
    pub fn start(&mut self, source: &str) -> Option<String> {
        let engine = self.engine.as_ref()?;
        start_block(engine, self.at(), source)
    }

    /// [`Self::start`] on a thread of its own: returns at once, and the
    /// block is collected with [`PendingStart::wait`]. `None` when there is
    /// no engine to ask.
    ///
    /// For opening a session without holding its answer: the engine's
    /// session-start was measured at 8.2 s of `session/new`'s 14.
    #[must_use]
    pub fn start_in_background(&self, source: &str) -> Option<PendingStart> {
        let engine = self.engine.clone()?;
        let session = self.session.clone();
        let cwd = self.cwd.clone();
        let transcript = self.transcript.clone();
        let source = source.to_string();
        let (tx, answer) = std::sync::mpsc::channel();
        let asked = source.clone();
        std::thread::spawn(move || {
            let at = Conversation {
                session: &session,
                cwd: &cwd,
                transcript: transcript.as_deref().filter(|p| p.is_file()),
            };
            // Nobody waiting any more (the session ended) is not an error.
            let _ = tx.send(start_block(&engine, at, &source));
        });
        Some(PendingStart {
            answer,
            source: asked,
            began: std::time::Instant::now(),
        })
    }

    /// A person's line, about to be sent to the model: written to the
    /// transcript, and the engine's context for it returned, bounded.
    ///
    /// Also where a turn begins, so the stop channel's per-turn state starts
    /// again here.
    pub fn prompt(&mut self, line: &str) -> Option<String> {
        self.stop_active = false;
        self.continued = 0;
        self.said(Speaker::Person, line);
        let engine = self.engine.as_ref()?;
        let began = std::time::Instant::now();
        let block = engine.user_prompt(self.at(), line);
        crate::trace!(
            "memory",
            "user-prompt for {} in {:?}: {} bytes",
            self.session,
            began.elapsed(),
            block.as_ref().map_or(0, String::len)
        );
        let name = format!("{}-turn", safe_name(&self.session));
        block.map(|b| self.inline(&b, &name, TURN_INLINE))
    }

    /// Write one line of the conversation to the transcript.
    pub fn said(&mut self, speaker: Speaker, text: &str) {
        let Some(path) = self.transcript.as_deref() else {
            return;
        };
        if text.trim().is_empty() {
            return;
        }
        let (kind, content) = match speaker {
            Speaker::Person => ("user", json!(text)),
            // As an array of text blocks: the engine reads an assistant line
            // only in that shape.
            Speaker::Agent => ("assistant", json!([{ "type": "text", "text": text }])),
        };
        let line = json!({
            "type": kind,
            "uuid": uuid::Uuid::new_v4().to_string(),
            "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "sessionId": self.session,
            "cwd": self.cwd.to_string_lossy(),
            "message": { "role": kind, "content": content },
        });
        if let Err(e) = append_line(path, &line.to_string()) {
            crate::trace!("memory", "transcript {} not written: {e}", path.display());
        }
    }

    /// How many of incode's background jobs are still running.
    pub fn set_background(&mut self, running: usize) {
        self.background = running;
    }

    /// The model has stopped. The engine records the turn, and may answer that
    /// it should carry on: the words it is to carry on with come back, already
    /// written to the transcript as the next line. `None` ends the turn.
    ///
    /// Not asked at all when the turn was stopped — a person who pressed stop
    /// is not held for the engine's ten minutes — or while a background job is
    /// still running, which makes this stop a wait rather than an end. At
    /// [`MAX_CONTINUATIONS`] the engine is still asked, so the turn is
    /// recorded, but a further "carry on" is not followed.
    pub fn stop(&mut self, last_message: &str, cancel: Option<&CancelToken>) -> Option<String> {
        let engine = self.engine.as_ref()?;
        if cancel.is_some_and(CancelToken::is_cancelled) {
            crate::trace!("memory", "stop not sent: the turn was stopped");
            return None;
        }
        if self.background > 0 {
            crate::trace!(
                "memory",
                "stop not sent: waiting on {} background job(s)",
                self.background
            );
            return None;
        }
        let began = std::time::Instant::now();
        let answer = engine.stop(self.at(), last_message, self.stop_active, cancel);
        crate::trace!(
            "memory",
            "capture for {} (active={}) in {:?}: {}",
            self.session,
            self.stop_active,
            began.elapsed(),
            if answer.is_some() { "carry on" } else { "end" }
        );
        let reason = answer?;
        if self.continued >= MAX_CONTINUATIONS {
            crate::trace!(
                "memory",
                "carry-on not followed: {MAX_CONTINUATIONS} already sent this turn"
            );
            return None;
        }
        self.continued += 1;
        self.stop_active = true;
        self.said(Speaker::Person, &reason);
        Some(reason)
    }

    /// The session is over.
    pub fn end(&mut self, reason: &str) {
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let began = std::time::Instant::now();
        engine.session_end(self.at(), reason);
        crate::trace!(
            "memory",
            "session-end ({reason}) for {} in {:?}",
            self.session,
            began.elapsed()
        );
    }

    /// End this session and open `next` in its place — `/clear`. The old id
    /// is the one the end names; the new one's context comes back.
    pub fn restart(&mut self, next: &str, source: &str) -> Option<String> {
        self.end(source);
        self.session = next.to_string();
        self.transcript =
            (self.engine.is_some() || self.keep).then(|| transcript_for(&self.cwd, next));
        self.stop_active = false;
        self.continued = 0;
        self.start(source)
    }

    /// `block` as the model is given it: whole when it fits in `limit` bytes,
    /// otherwise its head, with the whole written to a file the model can read.
    fn inline(&self, block: &str, name: &str, limit: usize) -> String {
        inline_in(&self.cwd, block, name, limit)
    }
}

/// A session-start the engine is still answering, on a thread of its own —
/// see [`MemoryHooks::start_in_background`].
pub struct PendingStart {
    answer: std::sync::mpsc::Receiver<Option<String>>,
    source: String,
    began: std::time::Instant,
}

impl PendingStart {
    /// The engine's block, waiting at most `bound` for it: `None` when it had
    /// nothing to say, or did not answer in time (traced either way).
    #[must_use]
    pub fn wait(self, bound: std::time::Duration) -> Option<String> {
        let waited = std::time::Instant::now();
        let block = match self.answer.recv_timeout(bound) {
            Ok(block) => block,
            Err(why) => {
                crate::trace!(
                    "memory",
                    "session-start ({}) not answered within {bound:?} ({why}); the turn goes \
                     without its block",
                    self.source
                );
                None
            }
        };
        crate::trace!(
            "memory",
            "session-start ({}) collected {:?} after it was asked, {:?} of it spent waiting here",
            self.source,
            self.began.elapsed(),
            waited.elapsed()
        );
        block
    }
}

/// The engine's session-start block for `at`, as every model is handed it:
/// headed with the session's id, and inlined within [`START_INLINE`].
fn start_block(engine: &Engine, at: Conversation<'_>, source: &str) -> Option<String> {
    let began = std::time::Instant::now();
    let block = engine.session_start(at, source);
    crate::trace!(
        "memory",
        "session-start ({source}) for {} in {:?}: {} bytes",
        at.session,
        began.elapsed(),
        block.as_ref().map_or(0, String::len)
    );
    let name = format!("{}-start", safe_name(at.session));
    // The engine's block tells the model to file things under its session,
    // and never says which one: the model only knows its own CLI's id.
    // Measured: handed the block alone, a model posted a session entry
    // under an id the engine had never seen.
    block.map(|b| {
        format!(
            "Your session id in Incord Memory is `{}`: use it wherever the memory \
             service asks for a session_id.\n\n{}",
            at.session,
            inline_in(at.cwd, &b, &name, START_INLINE)
        )
    })
}

/// [`MemoryHooks::inline`], for a workspace `cwd`.
fn inline_in(cwd: &Path, block: &str, name: &str, limit: usize) -> String {
    if block.len() <= limit {
        return block.to_string();
    }
    let dir = cwd.join(CONTEXT);
    let path = dir.join(format!("{name}.md"));
    let written = crate::folders::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&path, block))
        .is_ok();
    prune(&dir);
    let head = crate::text::head_bytes(block, limit);
    let rest = block.len() - head.len();
    if written {
        let shown = Path::new(CONTEXT).join(format!("{name}.md"));
        format!(
            "{head}\n\n[… {rest} more bytes of this block are in `{}` — read it when \
             the part above is not enough.]",
            shown.display()
        )
    } else {
        format!("{head}\n\n[… {rest} more bytes of this block were cut.]")
    }
}

/// The transcript file for one session.
pub(crate) fn transcript_for(cwd: &Path, session: &str) -> PathBuf {
    cwd.join(TRANSCRIPTS)
        .join(format!("{}.jsonl", safe_name(session)))
}

/// A session id as a file name that cannot leave its directory.
fn safe_name(id: &str) -> String {
    let mut out: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("unnamed");
    }
    out
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let fresh = !path.exists();
    if let Some(dir) = path.parent() {
        crate::folders::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    if fresh {
        if let Some(dir) = path.parent() {
            prune(dir);
        }
    }
    Ok(())
}

/// Keep the newest [`KEEP`] files in `dir`, so a workspace used daily does
/// not grow a directory nobody prunes.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file()
                .then(|| (meta.modified().unwrap_or(std::time::UNIX_EPOCH), e.path()))
        })
        .collect();
    if files.len() <= KEEP {
        return;
    }
    files.sort_by_key(|file| std::cmp::Reverse(file.0));
    for (_, path) in files.into_iter().skip(KEEP) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::engine::fake::{self, Reply};
    use std::time::Duration;

    const BLOCK: &str = r#"{"decision":"block","reason":"now run the tests"}"#;

    fn hooks(dir: &Path, replies: &[(&str, Reply)]) -> MemoryHooks {
        let bin = fake::write(dir, replies);
        let engine = fake::engine(bin).capped(Duration::from_secs(20));
        MemoryHooks::new(Some(engine), dir, "sess-1")
    }

    fn context(text: &str, event: &str) -> String {
        json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}})
            .to_string()
    }

    /// **EACH EVENT CARRIES WHAT THE ENGINE READS.** `session_id` and `cwd` on
    /// all of them; `source` on the start; `prompt` and the transcript on the
    /// prompt; the last message, the brake and the transcript on the stop;
    /// the reason and the transcript on the end.
    #[test]
    fn each_event_is_sent_with_the_fields_the_engine_reads() {
        let dir = tempfile::tempdir().expect("scratch");
        let mut memory = hooks(
            dir.path(),
            &[
                ("session-start", Reply::exit(0)),
                ("user-prompt", Reply::exit(0)),
                ("capture", Reply::exit(0)),
                ("session-end", Reply::exit(0)),
            ],
        );
        let _ = memory.start("startup");
        let _ = memory.prompt("add a readme");
        memory.said(Speaker::Agent, "I added README.md.");
        let _ = memory.stop("I added README.md.", None);
        memory.end("prompt_input_exit");

        let cwd = dir.path().to_string_lossy().into_owned();
        let transcript = transcript_for(dir.path(), "sess-1")
            .to_string_lossy()
            .into_owned();

        let start = &fake::events(dir.path(), "session-start")[0];
        assert_eq!(start["hook_event_name"], "SessionStart");
        assert_eq!(start["session_id"], "sess-1");
        assert_eq!(start["cwd"], cwd.as_str());
        assert_eq!(start["source"], "startup");

        let prompt = &fake::events(dir.path(), "user-prompt")[0];
        assert_eq!(prompt["hook_event_name"], "UserPromptSubmit");
        assert_eq!(prompt["prompt"], "add a readme");
        assert_eq!(prompt["transcript_path"], transcript.as_str());

        let stop = &fake::events(dir.path(), "capture")[0];
        assert_eq!(stop["hook_event_name"], "Stop");
        assert_eq!(stop["session_id"], "sess-1");
        assert_eq!(stop["last_assistant_message"], "I added README.md.");
        assert_eq!(stop["stop_hook_active"], false);
        assert_eq!(stop["transcript_path"], transcript.as_str());

        let end = &fake::events(dir.path(), "session-end")[0];
        assert_eq!(end["hook_event_name"], "SessionEnd");
        assert_eq!(end["reason"], "prompt_input_exit");
        assert_eq!(end["transcript_path"], transcript.as_str());
    }

    /// **THE TRANSCRIPT IS IN THE SHAPE THE ENGINE CAPTURES FROM.** Its
    /// reader keeps a line only with `type`, a `uuid` and `message.content` —
    /// a string for a person, an array of text blocks for the model — and
    /// files the project under the first `"cwd":"` it finds.
    #[test]
    fn the_transcript_is_written_in_the_shape_the_engine_reads() {
        let dir = tempfile::tempdir().expect("scratch");
        let mut memory = hooks(dir.path(), &[("user-prompt", Reply::exit(0))]);
        let _ = memory.prompt("create a.txt with hi");
        memory.said(Speaker::Agent, "Created a.txt.");

        let text = std::fs::read_to_string(transcript_for(dir.path(), "sess-1")).expect("written");
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).expect("JSON"))
            .collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(lines[0]["type"], "user");
        assert_eq!(lines[0]["message"]["content"], "create a.txt with hi");
        assert_eq!(lines[1]["type"], "assistant");
        assert_eq!(lines[1]["message"]["content"][0]["type"], "text");
        assert_eq!(lines[1]["message"]["content"][0]["text"], "Created a.txt.");
        for line in &lines {
            assert!(line["uuid"].as_str().is_some_and(|u| !u.is_empty()));
            assert!(chrono::DateTime::parse_from_rfc3339(
                line["timestamp"].as_str().expect("timestamp")
            )
            .is_ok());
        }
        let key = "\"cwd\":\"";
        let at = text.find(key).expect("a cwd field") + key.len();
        let rest = &text[at..];
        let cwd = rest[..rest.find('"').expect("closed")].replace("\\\\", "\\");
        assert_eq!(cwd, dir.path().to_string_lossy());
    }

    /// No engine, nothing written: a transcript nobody reads is a copy of the
    /// conversation nobody asked for.
    #[test]
    fn without_an_engine_nothing_is_fired_or_written() {
        let dir = tempfile::tempdir().expect("scratch");
        let mut memory = MemoryHooks::off(dir.path());
        assert_eq!(memory.start("startup"), None);
        assert_eq!(memory.prompt("hello there"), None);
        memory.said(Speaker::Agent, "hi");
        assert_eq!(memory.stop("hi", None), None);
        memory.end("other");
        assert!(!dir.path().join(TRANSCRIPTS).exists());
    }

    #[test]
    fn a_block_decision_is_a_carry_on_and_anything_else_ends_the_turn() {
        let dir = tempfile::tempdir().expect("scratch");
        let mut memory = hooks(dir.path(), &[("capture", Reply::exit(0).stdout(BLOCK))]);
        let _ = memory.prompt("go");
        assert_eq!(
            memory.stop("done", None),
            Some("now run the tests".to_string())
        );
        // The second stop of the same turn carries the brake.
        let _ = memory.stop("ran them", None);
        let stops = fake::events(dir.path(), "capture");
        assert_eq!(stops[0]["stop_hook_active"], false);
        assert_eq!(stops[1]["stop_hook_active"], true);

        let quiet = tempfile::tempdir().expect("scratch");
        let mut memory = hooks(quiet.path(), &[("capture", Reply::exit(0))]);
        assert_eq!(memory.stop("done", None), None);
        let failing = tempfile::tempdir().expect("scratch");
        let mut memory = hooks(failing.path(), &[("capture", Reply::exit(1).stdout(BLOCK))]);
        assert_eq!(
            memory.stop("done", None),
            None,
            "a failed hook never carries on"
        );
    }

    /// **THE CONTINUATION CAP HOLDS.** An engine that always says carry on
    /// gets [`MAX_CONTINUATIONS`] of them, and the stop after that is still
    /// sent — the turn is recorded — but not followed.
    #[test]
    fn the_continuation_cap_holds() {
        let dir = tempfile::tempdir().expect("scratch");
        let mut memory = hooks(dir.path(), &[("capture", Reply::exit(0).stdout(BLOCK))]);
        let _ = memory.prompt("go");
        let mut followed = 0;
        while memory.stop("again", None).is_some() {
            followed += 1;
            assert!(followed <= 10, "the cap did not hold");
        }
        assert_eq!(followed, usize::from(MAX_CONTINUATIONS));
        assert_eq!(
            fake::calls(dir.path(), "capture"),
            usize::from(MAX_CONTINUATIONS) + 1
        );
        // A new line from the person is a new turn, with a fresh allowance.
        let _ = memory.prompt("next");
        assert!(memory.stop("again", None).is_some());
    }

    /// **A STOPPED TURN SPAWNS NOTHING.** Cancel before the stop: the engine
    /// is never started, so nobody waits on it.
    #[test]
    fn a_cancelled_turn_does_not_start_the_stop_hook() {
        let dir = tempfile::tempdir().expect("scratch");
        let mut memory = hooks(dir.path(), &[("capture", Reply::exit(0).stdout(BLOCK))]);
        let cancel = CancelToken::new();
        cancel.cancel();
        assert_eq!(memory.stop("done", Some(&cancel)), None);
        // Counted at the spawn, not by the fake: a process stopped before its
        // first line would never have counted itself.
        assert_eq!(fake::spawned(dir.path(), "capture"), 0);
    }

    /// **BACKGROUND WORK MAKES A STOP A WAIT.** While a job runs the engine is
    /// not asked; once none do, the next stop goes out as usual.
    #[test]
    fn a_running_background_job_holds_the_stop_hook() {
        let dir = tempfile::tempdir().expect("scratch");
        let mut memory = hooks(dir.path(), &[("capture", Reply::exit(0))]);
        memory.set_background(1);
        assert_eq!(memory.stop("started the build", None), None);
        assert_eq!(fake::calls(dir.path(), "capture"), 0);
        memory.set_background(0);
        let _ = memory.stop("the build finished", None);
        assert_eq!(fake::calls(dir.path(), "capture"), 1);
    }

    /// **A CLEAR ENDS THE OLD SESSION BY ITS OWN ID** and opens the new one
    /// as a clear.
    #[test]
    fn a_restart_ends_the_old_id_and_starts_the_new_one() {
        let dir = tempfile::tempdir().expect("scratch");
        let mut memory = hooks(
            dir.path(),
            &[
                ("session-end", Reply::exit(0)),
                (
                    "session-start",
                    Reply::exit(0).stdout(&context("FRESH", "SessionStart")),
                ),
            ],
        );
        let block = memory.restart("sess-2", "clear").expect("the new context");
        assert!(block.ends_with("FRESH"), "{block}");
        assert!(
            block.contains("`sess-2`"),
            "the model is told its new id: {block}"
        );
        let end = &fake::events(dir.path(), "session-end")[0];
        assert_eq!(end["session_id"], "sess-1");
        assert_eq!(end["reason"], "clear");
        let start = &fake::events(dir.path(), "session-start")[0];
        assert_eq!(start["session_id"], "sess-2");
        assert_eq!(start["source"], "clear");
        assert_eq!(memory.session(), "sess-2");
    }

    /// **THE TEXT IS REBUILT THE SAME WHATEVER THE BACKEND SENDS.** One
    /// backend sends every fragment as a delta AND as a "whole" block, another
    /// deltas then the real block, a third whole blocks only; each reads back
    /// as what the model said, split where it turned to a tool.
    #[test]
    fn what_the_model_said_is_rebuilt_from_any_backends_events() {
        use crate::agent::AgentEvent;
        let tool = AgentEvent::ToolUse {
            id: "1".into(),
            name: "Write".into(),
            input: "{}".into(),
        };
        // Each fragment twice over, as a delta and as a block.
        let mut doubled = Said::default();
        for piece in ["I hav", "e read it."] {
            doubled.hear(&AgentEvent::TextDelta(piece.into()));
            doubled.hear(&AgentEvent::Text(piece.into()));
        }
        doubled.hear(&tool);
        for piece in ["Done", "."] {
            doubled.hear(&AgentEvent::TextDelta(piece.into()));
            doubled.hear(&AgentEvent::Text(piece.into()));
        }
        assert_eq!(doubled.whole(), "I have read it.\n\nDone.");
        assert_eq!(doubled.last(), "Done.");

        // Deltas, then the whole block.
        let mut streamed = Said::default();
        streamed.hear(&AgentEvent::TextDelta("Hel".into()));
        streamed.hear(&AgentEvent::TextDelta("lo".into()));
        streamed.hear(&AgentEvent::Text("Hello".into()));
        assert_eq!(streamed.whole(), "Hello");

        // Whole blocks only.
        let mut blocks = Said::default();
        blocks.hear(&AgentEvent::Text("One.".into()));
        blocks.hear(&tool);
        blocks.hear(&AgentEvent::Text("Two.".into()));
        assert_eq!(blocks.whole(), "One.\n\nTwo.");
        assert_eq!(blocks.last(), "Two.");
    }

    /// An oversized block is cut to its budget, and the whole of it is left
    /// where the model can read it.
    #[test]
    fn an_oversized_block_is_cut_and_kept_whole_in_a_file() {
        let dir = tempfile::tempdir().expect("scratch");
        let big = format!("HEAD {}", "x".repeat(TURN_INLINE * 2));
        let mut memory = hooks(
            dir.path(),
            &[(
                "user-prompt",
                Reply::exit(0).stdout(&context(&big, "UserPromptSubmit")),
            )],
        );
        let shown = memory.prompt("go").expect("a block");
        assert!(shown.starts_with("HEAD "));
        assert!(shown.len() < TURN_INLINE + 300, "{} bytes", shown.len());
        let file = dir.path().join(CONTEXT).join("sess-1-turn.md");
        assert_eq!(std::fs::read_to_string(&file).expect("kept"), big);
        assert!(shown.contains("sess-1-turn.md"), "{shown}");
    }
}
