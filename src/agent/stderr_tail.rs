//! What an agent printed to stderr: drained as it arrives, kept bounded, and
//! cleaned of anything shaped like a credential before it goes anywhere.
//!
//! # Why this is one module and not three threads
//!
//! Each adapter pipes its agent's stderr and reports it when the agent fails.
//! Two of them drained it on a thread into an unbounded `String`; the third
//! read it only after `wait()`, which is a hang: an agent that writes more
//! than the pipe holds blocks on the write, never exits, and `wait()` never
//! returns. And all three pasted the raw text into an error that is recorded
//! in the ledger and shown to the operator — including whatever a CLI prints
//! about its own credentials when it fails to authenticate.
//!
//! So: read on a thread from the moment the agent starts, keep only the last
//! [`MAX_LINES`] / [`MAX_BYTES`], strip terminal colour codes, and redact before
//! storing, so an over-long line cannot push a secret past the cut.
//!
//! The ring-buffer-plus-redaction shape follows codeg's `stderr_tail.rs`
//! (Apache-2.0); this is a separate implementation, not a copy.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use regex::Regex;

/// Lines kept, newest last.
pub const MAX_LINES: usize = 200;
/// Bytes kept across all lines.
pub const MAX_BYTES: usize = 32 * 1024;
/// Bytes kept of any one line.
pub const MAX_LINE_BYTES: usize = 512;

/// How long [`StderrTail::finish`] waits for the stream to close.
///
/// The agent has exited by the time anyone asks, so its end of the pipe is
/// closed — unless something it started still holds it. Waiting forever for
/// a grandchild would turn a failed turn into a hung one, which is the bug
/// this module exists to remove; what was read by then is reported.
pub const SETTLE: Duration = Duration::from_secs(2);

const REDACTED: &str = "[redacted]";

#[derive(Default)]
struct Ring {
    lines: VecDeque<String>,
    bytes: usize,
    dropped: usize,
    closed: bool,
}

impl Ring {
    fn push(&mut self, raw: &str) {
        let line = clean(raw);
        if line.trim().is_empty() {
            return;
        }
        self.bytes += line.len();
        self.lines.push_back(line);
        while self.lines.len() > MAX_LINES || self.bytes > MAX_BYTES {
            let Some(old) = self.lines.pop_front() else {
                break;
            };
            self.bytes -= old.len();
            self.dropped += 1;
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        if self.dropped > 0 {
            out.push_str(&format!("({} earlier line(s) dropped)\n", self.dropped));
        }
        for line in &self.lines {
            out.push_str(line);
            out.push('\n');
        }
        out.trim_end().to_string()
    }
}

/// A stderr stream being drained in the background.
pub struct StderrTail {
    shared: Arc<(Mutex<Ring>, Condvar)>,
}

impl StderrTail {
    /// Start draining `stream` now. `None` (nothing was piped) yields an empty
    /// tail rather than an error: there is simply nothing to report.
    #[must_use]
    pub fn capture<R: Read + Send + 'static>(stream: Option<R>) -> Self {
        let shared = Arc::new((Mutex::new(Ring::default()), Condvar::new()));
        let writer = Arc::clone(&shared);
        std::thread::spawn(move || {
            if let Some(stream) = stream {
                let mut reader = BufReader::new(stream);
                let mut buf = Vec::new();
                // Read to the end whatever the ring keeps: the point is that
                // the agent's writes never block.
                while matches!(reader.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
                    let text = String::from_utf8_lossy(&buf);
                    if let Ok(mut ring) = writer.0.lock() {
                        ring.push(text.trim_end_matches(['\r', '\n']));
                    }
                    buf.clear();
                }
            }
            if let Ok(mut ring) = writer.0.lock() {
                ring.closed = true;
            }
            writer.1.notify_all();
        });
        StderrTail { shared }
    }

    /// Everything kept, once the stream closes or [`SETTLE`] passes.
    #[must_use]
    pub fn finish(self) -> String {
        let (lock, closed) = &*self.shared;
        let Ok(ring) = lock.lock() else {
            return String::new();
        };
        match closed.wait_timeout_while(ring, SETTLE, |r| !r.closed) {
            Ok((ring, _)) => ring.render(),
            Err(_) => String::new(),
        }
    }
}

/// One line as it may be stored: colour codes gone, credentials redacted,
/// then cut to [`MAX_LINE_BYTES`] — in that order, so a cut can never leave
/// half a secret behind.
#[must_use]
pub fn clean(line: &str) -> String {
    let mut out = match ansi() {
        Some(ansi) => ansi.replace_all(line, "").into_owned(),
        None => line.to_string(),
    };
    for (pattern, replacement) in secrets() {
        out = pattern.replace_all(&out, *replacement).into_owned();
    }
    crate::text::head_bytes(&out, MAX_LINE_BYTES).to_string()
}

const ANSI_PATTERN: &str = r"\x1b\[[0-9;?]*[ -/]*[@-~]";

fn ansi() -> Option<&'static Regex> {
    static ANSI: OnceLock<Option<Regex>> = OnceLock::new();
    ANSI.get_or_init(|| Regex::new(ANSI_PATTERN).ok()).as_ref()
}

/// Shapes of credentials a CLI prints when it fails — each with what it
/// becomes. Shapes, not issuers' names.
const SECRET_PATTERNS: &[(&str, &str)] = &[
    // `Authorization: Bearer <token>`, `bearer <token>`.
    (r"(?i)\b(bearer)\s+[A-Za-z0-9._~+/=-]{8,}", "$1 [redacted]"),
    // `api_key=…`, `token: …`, `password=…` — the value, not the name.
    (
        r"(?i)\b(api[_-]?key|access[_-]?token|token|secret|password|passwd)(\s*[=:]\s*)\S+",
        "$1$2[redacted]",
    ),
    // Prefixed keys: `sk-…`, `gh?_…`, `xox?-…`.
    (r"\bsk-[A-Za-z0-9_-]{16,}", REDACTED),
    (r"\bgh[pousr]_[A-Za-z0-9]{20,}", REDACTED),
    (r"\bxox[abprs]-[A-Za-z0-9-]{10,}", REDACTED),
    // Cloud access key ids.
    (r"\bAKIA[0-9A-Z]{16}\b", REDACTED),
    // JSON web tokens: three base64url segments, the first a JSON header.
    (
        r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
        REDACTED,
    ),
];

/// [`SECRET_PATTERNS`], compiled. A pattern that failed to compile would be
/// a hole in the redaction, so `every_pattern_compiles` pins that none is
/// dropped here.
fn secrets() -> &'static [(Regex, &'static str)] {
    static SECRETS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    SECRETS.get_or_init(|| {
        SECRET_PATTERNS
            .iter()
            .filter_map(|(p, r)| Regex::new(p).ok().map(|re| (re, *r)))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each fixture is SHAPED like a credential and is not one — a string
    /// of the right prefix and length, so the pattern is tested and nothing
    /// real is written down.
    #[test]
    fn credentials_are_redacted_and_their_names_kept() {
        let fake = "Z".repeat(24);
        for line in [
            format!("Authorization: Bearer {fake}"),
            format!("error: token={fake}"),
            format!("api_key: {fake}"),
            format!("using sk-{fake}"),
            format!("gho_{fake}"),
            format!("jwt eyJ{fake}.{fake}.{fake}"),
        ] {
            let out = clean(&line);
            assert!(!out.contains(&fake), "survived: {out}");
            assert!(out.contains(REDACTED), "nothing was marked: {out}");
        }
        // What the value belonged to is still readable.
        assert!(clean(&format!("token={fake}")).starts_with("token="));
    }

    /// Ordinary diagnostics are not mangled — a redactor that eats the
    /// message is as useless as none.
    #[test]
    fn ordinary_lines_pass_through() {
        for line in [
            "error: could not connect to the service (timeout after 30s)",
            "max_tokens: 4096 exceeded",
            "tokens: 1200 in, 300 out",
        ] {
            assert_eq!(clean(line), line);
        }
    }

    /// Compiled without `expect`, so a broken pattern would be skipped
    /// silently — this is what makes that impossible to ship.
    #[test]
    fn every_pattern_compiles() {
        assert_eq!(secrets().len(), SECRET_PATTERNS.len());
        assert!(ansi().is_some());
    }

    #[test]
    fn colour_codes_are_stripped() {
        assert_eq!(clean("\x1b[31merror\x1b[0m: bad"), "error: bad");
    }

    /// **BOUNDED, AND THE NEWEST LINES ARE THE ONES KEPT** — a CLI's last
    /// words are its reason for exiting.
    #[test]
    fn a_flood_is_bounded_to_its_newest_lines_and_says_so() {
        let flood: String = (0..10_000).map(|i| format!("line {i}\n")).collect();
        let out = StderrTail::capture(Some(std::io::Cursor::new(flood))).finish();
        assert!(out.len() <= MAX_BYTES + 64, "{} bytes kept", out.len());
        assert!(out.contains("line 9999"), "the last line was not kept");
        assert!(!out.contains("line 0\n"), "the oldest line was kept");
        assert!(
            out.starts_with('(') && out.contains("dropped"),
            "{}",
            &out[..80]
        );
    }

    #[test]
    fn one_enormous_line_is_cut_to_the_line_limit() {
        let long = "x".repeat(MAX_LINE_BYTES * 10);
        let out = StderrTail::capture(Some(std::io::Cursor::new(long))).finish();
        assert_eq!(out.len(), MAX_LINE_BYTES);
    }

    #[test]
    fn nothing_piped_is_nothing_to_report() {
        assert_eq!(StderrTail::capture(None::<std::io::Empty>).finish(), "");
    }
}
