//! Who is told what incode's tools and workers are doing, while they do it.
//!
//! A tool call is announced before it runs and its result after, through the
//! turn's own event stream ([`AgentEvent`]). Three things happen in between
//! that a front end drawing the turn wants to see as they happen, and that no
//! event carried until now:
//!
//! - **output** — a command that runs for minutes prints as it goes, and the
//!   person watching should see it growing, not all at once at the end;
//! - **decisions** — what the operator's guards or the memory engine decided
//!   about the call before it ran, and the recorder's answer to a
//!   `watchman_lookup`;
//! - **workers** — the agents incode starts for a turn or a run: the `Agent`
//!   tool's sub-agent and each parallel lane of `/run`. A worker's own
//!   conversation is incode's event stream too; it is handed, event by event,
//!   to the [`WorkerTap`] the watch gave out when the worker started.
//!
//! A [`Watch`] is optional everywhere it is taken: the REPL has none, and a
//! toolbox without one runs exactly as it always did. `incode --acp` is the
//! one front end that installs one (`acp_agent::workers`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::agent::AgentEvent;

/// The most of a running call's output shown while it runs, in bytes: the
/// newest part. The whole output is still the call's result when it ends.
pub const MAX_LIVE_OUTPUT_BYTES: usize = 8 * 1024;

/// How often a running call's output is shown again, at most. A command
/// printing thousands of lines a second is then a few updates a second, not
/// thousands of messages.
pub const LIVE_OUTPUT_EVERY: Duration = Duration::from_millis(500);

/// Who decided about a call before it ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decider {
    /// The operator's protected paths (`guards.rs`).
    Guards,
    /// The memory engine's `pre-tool` hook.
    Engine,
    /// The operator's own pre-tool command (`crate::hooks`), when it refused.
    Hook,
}

impl Decider {
    /// The word a front end shows.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Guards => "guards",
            Self::Engine => "engine",
            Self::Hook => "hook",
        }
    }
}

/// What the gate decided about one call before it ran — only what was
/// actually decided, and only by the guards or the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub by: Decider,
    pub allowed: bool,
    /// The protected pattern that matched, when the guards refused.
    pub rule: Option<String>,
    /// The path the refusal was about, when there was one.
    pub path: Option<String>,
    /// The engine's own reason, when it gave one.
    pub reason: Option<String>,
}

/// One worker, as it is announced.
#[derive(Debug, Clone)]
pub struct WorkerSpec {
    /// What kind of worker: `sub-agent` or `lane`. Part of its id.
    pub kind: &'static str,
    /// What a person reads, e.g. `implement · lane 2`.
    pub name: String,
    /// What it was asked to do.
    pub task: String,
    /// The role its model was routed by, when it was routed by one.
    pub role: Option<String>,
    /// The agent that serves it, by catalogue id.
    pub agent: Option<String>,
    /// The model it asked for, when one was chosen.
    pub model: Option<String>,
    /// The lane it works in, for a lane of `/run`.
    pub lane: Option<String>,
    /// Where it works: its edits are named inside this directory.
    pub workspace: PathBuf,
}

/// How a worker ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerEnd {
    Completed,
    Failed,
    Cancelled,
}

impl WorkerEnd {
    /// The state as the sub-agent extension spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Someone watching a toolbox and the workers it and a run start.
///
/// `Send + Sync`: a lane's worker runs on a thread of its own and is handed
/// the watch there.
pub trait Watch: Send + Sync {
    /// The call `call_id` is still running; `shown` is its output so far, as
    /// [`LiveOutput`] bounds it.
    fn output(&self, call_id: &str, shown: &str);

    /// What the guards or the engine decided about `call_id` before it ran.
    fn decided(&self, call_id: &str, decision: &Decision);

    /// The recorder answered a `watchman_lookup` call: a `hit` and how many
    /// times the output was seen, when it said.
    fn recorded(&self, call_id: &str, hit: bool, observations: Option<u64>);

    /// A worker is starting. Its events go to the tap returned; `None` when
    /// this watch does not follow workers.
    fn worker(&self, spec: &WorkerSpec) -> Option<Box<dyn WorkerTap>>;
}

/// Where one worker's own events go, until it ends.
pub trait WorkerTap: Send {
    /// One event of the worker's own conversation.
    fn event(&mut self, event: &AgentEvent);

    /// The worker is over. Called once; a tap dropped without it was a
    /// worker that died, and says so.
    fn ended(&mut self, end: WorkerEnd);

    /// The watch for the worker's own tools and the workers it starts.
    fn watch(&self) -> Arc<dyn Watch>;
}

/// A running call's output, shown while it runs: the newest
/// [`MAX_LIVE_OUTPUT_BYTES`], at most once per [`LIVE_OUTPUT_EVERY`].
///
/// Bounds the action — what is SENT — and not the command: everything the
/// command prints still reaches its result.
pub struct LiveOutput<'a> {
    watch: &'a dyn Watch,
    call_id: &'a str,
    /// The newest output, never much more than twice the bound.
    held: Vec<u8>,
    /// Bytes printed and no longer held.
    dropped: usize,
    /// When it was last shown.
    shown_at: Option<Instant>,
    /// Output arrived since it was last shown.
    fresh: bool,
}

impl<'a> LiveOutput<'a> {
    #[must_use]
    pub fn new(watch: &'a dyn Watch, call_id: &'a str) -> Self {
        Self {
            watch,
            call_id,
            held: Vec::new(),
            dropped: 0,
            shown_at: None,
            fresh: false,
        }
    }

    /// More output: held, and shown when the last showing is old enough. An
    /// empty piece is time passing: what is held and not yet shown goes out
    /// once its time comes, even when nothing more arrives.
    pub fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            self.tick();
            return;
        }
        self.held.extend_from_slice(bytes);
        if self.held.len() > 2 * MAX_LIVE_OUTPUT_BYTES {
            let cut = self.held.len() - MAX_LIVE_OUTPUT_BYTES;
            self.held.drain(..cut);
            self.dropped += cut;
        }
        self.fresh = true;
        self.tick();
    }

    /// Show what is held if it changed and the last showing is old enough.
    fn tick(&mut self) {
        if !self.fresh {
            return;
        }
        if self
            .shown_at
            .is_some_and(|at| at.elapsed() < LIVE_OUTPUT_EVERY)
        {
            return;
        }
        self.watch.output(self.call_id, &self.shown());
        self.shown_at = Some(Instant::now());
        self.fresh = false;
    }

    /// The text shown: the newest part, with what was left out said.
    fn shown(&self) -> String {
        let start = self.held.len().saturating_sub(MAX_LIVE_OUTPUT_BYTES);
        let tail = String::from_utf8_lossy(&self.held[start..]);
        let earlier = self.dropped + start;
        if earlier == 0 {
            tail.into_owned()
        } else {
            format!("[… {earlier} earlier bytes not shown while it runs]\n{tail}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Seen {
        shown: Mutex<Vec<String>>,
    }

    impl Watch for Seen {
        fn output(&self, _call_id: &str, shown: &str) {
            self.shown
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(shown.to_string());
        }
        fn decided(&self, _call_id: &str, _decision: &Decision) {}
        fn recorded(&self, _call_id: &str, _hit: bool, _observations: Option<u64>) {}
        fn worker(&self, _spec: &WorkerSpec) -> Option<Box<dyn WorkerTap>> {
            None
        }
    }

    /// **LIVE OUTPUT IS THROTTLED AND BOUNDED, AND THE CUT IS SAID.** The
    /// first arrival is shown at once; a flood inside the interval is not
    /// shown again until the interval passes; what is shown is never more
    /// than the bound plus the line saying what was left out.
    #[test]
    fn live_output_is_throttled_bounded_and_says_what_it_left_out() {
        let seen = Seen::default();
        let mut live = LiveOutput::new(&seen, "c1");
        live.push(b"first\n");
        for _ in 0..1000 {
            live.push(&[b'x'; 100]);
        }
        let shown = seen.shown.lock().expect("shown").clone();
        assert_eq!(shown.len(), 1, "a flood inside the interval is one update");
        assert_eq!(shown[0], "first\n");

        std::thread::sleep(LIVE_OUTPUT_EVERY + Duration::from_millis(50));
        live.tick();
        let shown = seen.shown.lock().expect("shown").clone();
        assert_eq!(shown.len(), 2, "held output is shown once its time comes");
        let last = &shown[1];
        assert!(last.starts_with("[… "), "{}", &last[..40]);
        assert!(last.len() < MAX_LIVE_OUTPUT_BYTES + 80, "{}", last.len());
        live.tick();
        assert_eq!(
            seen.shown.lock().expect("shown").len(),
            2,
            "nothing new, nothing sent"
        );
    }
}
