//! What the agent did to the desktop, written where someone can watch it.
//!
//! # Why a file and not a channel
//!
//! `incode --desktop-mcp` is a **separate process** from `incode serve` — that
//! is decision 4, and it is what keeps a crashing desktop driver from taking a
//! live harness session with it. Two processes cannot share a
//! `tokio::sync::broadcast`, so the semantic viewer channel (`docs/COMPUTER_USE.md`
//! section 14c) needs a medium both can reach.
//!
//! The medium already exists and is already the one this harness uses. The SSE
//! route does not read memory: `serve::snapshot_at` (`src/serve/mod.rs:27`)
//! reads the ledger and the transcripts off disk every poll, and the stream
//! diffs what it finds. A journal file is the same shape of thing, written by
//! the process that knows and read by the process that serves. No IPC, no lock,
//! no new dependency.
//!
//! # Single writer, many readers
//!
//! Exactly one process appends: the desktop MCP server, which is
//! single-threaded over a stdio pipe. Readers only read. That is why there is
//! no lock file here — not because locking was forgotten, but because the one
//! case a lock would protect against cannot arise.
//!
//! # The sequence number is the cursor, not the byte offset
//!
//! A journal that grows without bound is a disk leak, so this one is trimmed
//! (see [`MAX_BYTES`]). Trimming moves every later record to a new offset,
//! which silently corrupts any reader holding a byte position — it resumes
//! mid-line, or re-emits records it already showed. So each record carries a
//! monotonic [`Entry::seq`] and a reader tracks the highest one it has emitted.
//! Trimming can then drop old records without a reader ever seeing a duplicate
//! or a torn line.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How much journal to keep before the oldest records are dropped.
///
/// The input thread caps the agent at 120 actions per minute by default
/// ([`crate::desktop::limits`]), so an agent driving flat out writes on the
/// order of 30 KB a minute. A quarter of a megabyte is therefore several
/// minutes of the most recent activity, which is what an operator asking
/// "what is it doing right now" actually wants, and it bounds the cost of the
/// tail read the viewer performs on every poll. admin.json
/// `desktop_limits.journal_max_bytes` changes it.
pub const MAX_BYTES: u64 = super::limits::DEFAULT_JOURNAL_MAX_BYTES;

/// Where the journal lives inside a workspace.
///
/// In `.incode/`, with the transcripts (`crate::transcript::DIR`), rather than
/// in a new top-level directory: it is the same kind of artefact — a record of
/// what an agent did — and `.incode/` is already the harness's own space.
pub const FILE: &str = ".incode/desktop.jsonl";

/// The journal path for a workspace.
#[must_use]
pub fn path_in(workspace: &Path) -> PathBuf {
    workspace.join(FILE)
}

/// One thing that happened on the desktop.
///
/// Flat rather than nested, because the consumer is a browser rendering a line
/// of text per entry, and every level of nesting is a branch in that renderer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Monotonic within a journal file. See the module note: this is the
    /// reader's cursor, and it is why trimming is safe.
    pub seq: u64,

    /// RFC 3339, UTC, millisecond precision — the same spelling
    /// `serve::snapshot` uses, so an interface can sort the two together.
    pub at: String,

    /// What kind of record this is: the tool that ran, or `refused`.
    pub t: String,

    /// Human-readable detail. What was clicked, what was typed into (never
    /// WHAT was typed — see [`Journal::record`]), or why a refusal happened.
    pub detail: String,

    /// Whether the action was carried out. A refusal is still an event: an
    /// operator needs to see the agent being stopped, not just the agent
    /// succeeding.
    pub ok: bool,
}

/// An append-only record of desktop activity.
///
/// Holds a path rather than an open handle. Opening per write costs a syscall
/// that is irrelevant beside the screenshot and UI-tree work this sits next to,
/// and it buys something worth more: an operator can delete or move the file
/// mid-run and the next write recreates it instead of writing into a deleted
/// inode nobody can read.
#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
    next_seq: u64,
    /// admin.json `desktop_limits.journal_max_bytes`, default [`MAX_BYTES`].
    max_bytes: u64,
}

impl Journal {
    /// Open (or create) the journal at `path`.
    ///
    /// Resumes numbering after whatever is already there, so a restarted
    /// desktop process does not hand a watching reader a sequence that goes
    /// backwards — which would make the reader treat new records as already
    /// seen and show nothing at all.
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        let next_seq = last_seq(&path).map_or(0, |s| s + 1);
        Self {
            path,
            next_seq,
            max_bytes: super::limits::get().journal_max_bytes,
        }
    }

    /// The file being written.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record.
    ///
    /// # What must never be written here
    ///
    /// **`detail` must not carry typed text, clipboard contents, or anything
    /// read off the screen.** The journal exists so an operator can watch an
    /// agent working in real accounts; a password typed into a login form would
    /// otherwise be written to disk in plain text and pushed to every browser
    /// holding the stream open. Callers pass the NAME of the target, never the
    /// value that went into it.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error. Callers treat this as non-fatal: a
    /// journal that cannot be written is a lost view of the work, not a reason
    /// to refuse the work.
    pub fn record(&mut self, t: &str, detail: &str, ok: bool) -> std::io::Result<Entry> {
        let entry = Entry {
            seq: self.next_seq,
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            t: t.to_string(),
            detail: detail.to_string(),
            ok,
        };

        // Trim BEFORE appending, not after: trimming after means the file is
        // over the cap for the whole window between the two operations, and
        // the last write before a crash is the one that leaves it there.
        self.trim()?;

        if let Some(parent) = self.path.parent() {
            crate::folders::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let mut line = serde_json::to_string(&entry).unwrap_or_default();
        line.push('\n');
        // One write_all of one whole line, so a reader polling mid-append sees
        // either the line or nothing rather than half of it.
        file.write_all(line.as_bytes())?;

        self.next_seq += 1;
        Ok(entry)
    }

    /// Drop the oldest records if the file has grown past [`MAX_BYTES`].
    ///
    /// Keeps the newest half rather than emptying the file: an operator who
    /// looks a moment after a trim should still see recent context, and
    /// halving means the rewrite happens once per half-cap of writing rather
    /// than on every write once the cap is reached.
    fn trim(&self) -> std::io::Result<()> {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return Ok(()); // nothing written yet
        };
        if meta.len() <= self.max_bytes {
            return Ok(());
        }

        let text = std::fs::read_to_string(&self.path)?;
        let lines: Vec<&str> = text.lines().collect();
        let keep = lines.split_at(lines.len() / 2).1;
        let mut kept = keep.join("\n");
        if !kept.is_empty() {
            kept.push('\n');
        }
        std::fs::write(&self.path, kept)
    }
}

/// The highest sequence number in a journal, or `None` if there is none.
fn last_seq(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Entry>(line).ok())
        .map(|entry| entry.seq)
}

/// Records newer than `after`, oldest first.
///
/// `after` is the highest sequence a reader has already shown; pass `None` on
/// the first read to get everything still in the file. Unparseable lines are
/// skipped rather than failing the read — a half-written last line during an
/// append must not blank the viewer.
#[must_use]
pub fn since(path: &Path, after: Option<u64>) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
        .filter(|entry| after.is_none_or(|last| entry.seq > last))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_journal(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "incode-journal-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join(FILE)
    }

    #[test]
    fn a_record_round_trips_through_the_file() {
        let path = temp_journal("roundtrip");
        let mut journal = Journal::at(path.clone());
        journal
            .record("click_element", "listitem \"Bluetooth\"", true)
            .expect("write");

        let read = since(&path, None);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].t, "click_element");
        assert_eq!(read[0].detail, "listitem \"Bluetooth\"");
        assert!(read[0].ok);
    }

    /// A refusal is an event, not a silence.
    #[test]
    fn a_refusal_is_recorded_as_loudly_as_a_success() {
        let path = temp_journal("refusal");
        let mut journal = Journal::at(path.clone());
        journal
            .record(
                "refused",
                "type_text: the operator stopped the desktop",
                false,
            )
            .expect("write");

        let read = since(&path, None);
        assert_eq!(read.len(), 1);
        assert!(!read[0].ok, "a refusal must not be recorded as a success");
    }

    /// **THE CURSOR MUST NOT REPLAY.**
    ///
    /// A viewer polls; without this the operator sees every past action again
    /// on every tick, which is indistinguishable from the agent doing them
    /// again.
    #[test]
    fn a_reader_sees_each_record_exactly_once() {
        let path = temp_journal("cursor");
        let mut journal = Journal::at(path.clone());
        journal.record("click", "1", true).expect("write");
        journal.record("click", "2", true).expect("write");

        let first = since(&path, None);
        assert_eq!(first.len(), 2);
        let cursor = first.last().expect("two records").seq;

        // Nothing new: a poll with nothing to show must show nothing.
        assert!(since(&path, Some(cursor)).is_empty());

        journal.record("click", "3", true).expect("write");
        let next = since(&path, Some(cursor));
        assert_eq!(next.len(), 1, "only the new record");
        assert_eq!(next[0].detail, "3");
    }

    /// **A RESTARTED WRITER MUST NOT REUSE SEQUENCES.**
    ///
    /// If it restarted at 0, a reader holding cursor 5 would discard the next
    /// five records as already seen — the viewer would go quiet exactly when
    /// the desktop came back.
    #[test]
    fn a_restarted_journal_continues_the_sequence() {
        let path = temp_journal("restart");
        let mut first = Journal::at(path.clone());
        first.record("click", "a", true).expect("write");
        let last = first.record("click", "b", true).expect("write").seq;
        drop(first);

        let mut reopened = Journal::at(path.clone());
        let after = reopened.record("click", "c", true).expect("write");
        assert!(
            after.seq > last,
            "restarted at {} which is not after {last}",
            after.seq
        );

        let unseen = since(&path, Some(last));
        assert_eq!(unseen.len(), 1);
        assert_eq!(unseen[0].detail, "c");
    }

    /// **TRIMMING MUST NOT BREAK A READER.**
    ///
    /// This is the reason the cursor is a sequence number and not a byte
    /// offset. The test writes past the cap, which moves every surviving
    /// record to a new position in the file, and then reads with a cursor
    /// taken from BEFORE the trim.
    #[test]
    fn trimming_bounds_the_file_without_replaying_or_skipping() {
        let path = temp_journal("trim");
        let mut journal = Journal::at(path.clone());

        // A detail long enough that a few hundred records pass the cap.
        let bulk = "x".repeat(512);
        journal.record("click", &bulk, true).expect("write");
        let cursor = since(&path, None).last().expect("one").seq;

        for _ in 0..800 {
            journal.record("click", &bulk, true).expect("write");
        }

        let size = std::fs::metadata(&path).expect("exists").len();
        assert!(
            size <= MAX_BYTES,
            "journal grew to {size} bytes, past the {MAX_BYTES} cap"
        );

        // The cursor is older than the trim. It must still be honoured, and
        // the records it returns must be strictly increasing.
        let after = since(&path, Some(cursor));
        assert!(!after.is_empty(), "the trim took everything");
        let mut previous = cursor;
        for entry in &after {
            assert!(
                entry.seq > previous,
                "sequence went backwards: {} after {previous}",
                entry.seq
            );
            previous = entry.seq;
        }

        // And the cursor must bite on records that SURVIVED the trim, not only
        // on ones the trim happened to remove. Taking it from the middle of
        // the surviving range is what makes this an assertion about the cursor
        // rather than about the trim: with `after` ignored, everything below
        // `midpoint` comes back and this fails.
        let midpoint = after[after.len() / 2].seq;
        let newer = since(&path, Some(midpoint));
        assert!(
            newer.iter().all(|entry| entry.seq > midpoint),
            "a record at or before the cursor was replayed"
        );
        assert!(
            newer.len() < after.len(),
            "advancing the cursor returned just as much: {} of {}",
            newer.len(),
            after.len()
        );
    }

    /// A torn or foreign line must not blank the viewer.
    #[test]
    fn a_broken_line_is_skipped_not_fatal() {
        let path = temp_journal("torn");
        let mut journal = Journal::at(path.clone());
        journal.record("click", "good", true).expect("write");

        // Exactly what a reader polling mid-append would see.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        file.write_all(b"{\"seq\":99,\"at\":\"2026").expect("write");
        drop(file);

        let read = since(&path, None);
        assert_eq!(read.len(), 1, "the good record must survive the torn one");
        assert_eq!(read[0].detail, "good");
    }

    /// Reading a journal that was never written is empty, not an error.
    #[test]
    fn an_absent_journal_reads_as_nothing_happened() {
        let path = temp_journal("absent");
        assert!(since(&path, None).is_empty());
        assert!(since(&path, Some(7)).is_empty());
    }

    #[test]
    fn the_journal_sits_under_the_harness_directory() {
        let path = path_in(Path::new("C:/work/project"));
        assert!(path.ends_with("desktop.jsonl"), "got {}", path.display());
        assert!(path.to_string_lossy().contains(".incode"));
    }
}
