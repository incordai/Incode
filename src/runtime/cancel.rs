//! Cancellation — letting a turn already under way be stopped.
//!
//! Ctrl-C used to reach the harness in exactly one place: the readline prompt
//! (`cli/input.rs`), where it ends the session. Once a turn was under way there
//! was no path to stop it, so a model looping over tool calls ran to
//! `max_iterations` and the only way out was to kill the process — which loses
//! the session with it.
//!
//! The token is a flag rather than a channel because of where it has to be
//! read: the agentic loop is synchronous, and the points worth checking are
//! ordinary statements in it, not `await`s.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A shared "stop what you are doing" flag.
///
/// Cloning shares the same flag, so the handler that sets it and the loop that
/// reads it hold two handles to one piece of state.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask whatever is running to stop at its next checkpoint.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Clear the flag so the next turn starts clean.
    ///
    /// Needed because the prompt and the turn share one signal: rustyline
    /// handles Ctrl-C itself while it is reading a line, and a handler watching
    /// the same signal will have set this flag too. Without a reset at the top
    /// of each turn, that stale press would cancel the *next* turn instead —
    /// the user would type a message and watch it stop for no visible reason.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    /// Cancel this token when a stop request appears at `path`, for as long as
    /// the returned [`StopWatch`] lives.
    ///
    /// The other way to press Ctrl-C: `--serve` runs in a different process,
    /// so its stop button writes a file ([`stop_path_in`]) and this notices it.
    /// A request is CONSUMED when obeyed, so it stops one run and not the next.
    /// One already there when watching starts was made while nothing was
    /// running; obeying it would stop a run nobody asked to stop, so it is
    /// cleared instead, and [`StopWatch::cleared_stale`] says so.
    #[must_use]
    pub fn cancel_on_request(&self, path: std::path::PathBuf) -> StopWatch {
        let cleared_stale = path.exists() && std::fs::remove_file(&path).is_ok();
        let done = Arc::new(AtomicBool::new(false));
        let (token, watching) = (self.clone(), Arc::clone(&done));
        let handle = std::thread::spawn(move || {
            while !watching.load(Ordering::SeqCst) {
                if path.exists() {
                    let _ = std::fs::remove_file(&path);
                    eprintln!("stop requested from the interface");
                    token.cancel();
                    return;
                }
                std::thread::sleep(STOP_POLL);
            }
        });
        StopWatch {
            done,
            handle: Some(handle),
            cleared_stale,
        }
    }
}

/// Where a stop request for a workspace's run is written, inside `.incode/`.
pub const STOP_FILE: &str = "run.stop";

/// How often a [`StopWatch`] looks for a request. A person pressing stop
/// waits at most this long for it to be noticed.
const STOP_POLL: std::time::Duration = std::time::Duration::from_millis(300);

/// The stop request file for `workspace`.
#[must_use]
pub fn stop_path_in(workspace: &std::path::Path) -> std::path::PathBuf {
    workspace.join(crate::ledger::LEDGER_DIR).join(STOP_FILE)
}

/// Watching for a stop request; stops watching when dropped.
pub struct StopWatch {
    done: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    cleared_stale: bool,
}

impl StopWatch {
    /// Whether a request left over from before this run was cleared unobeyed.
    #[must_use]
    pub fn cleared_stale(&self) -> bool {
        self.cleared_stale
    }
}

impl Drop for StopWatch {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eventually(what: impl Fn() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if what() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        what()
    }

    /// **A STOP PRESSED IN THE INTERFACE STOPS THE RUN** — and is consumed, so
    /// it stops this run and not the next.
    #[test]
    fn a_stop_request_cancels_the_token_and_is_consumed() {
        let dir = tempfile::tempdir().expect("a workspace");
        let path = stop_path_in(dir.path());
        let token = CancelToken::new();
        let watch = token.cancel_on_request(path.clone());

        crate::desktop::safety::request_stop(&path).expect("request");
        assert!(eventually(|| token.is_cancelled()), "the stop was not seen");
        assert!(
            eventually(|| !path.exists()),
            "the request was not consumed"
        );
        assert!(!watch.cleared_stale());
    }

    /// A request left from before the run started is not obeyed.
    #[test]
    fn a_request_older_than_the_run_is_cleared_not_obeyed() {
        let dir = tempfile::tempdir().expect("a workspace");
        let path = stop_path_in(dir.path());
        crate::desktop::safety::request_stop(&path).expect("request");

        let token = CancelToken::new();
        let watch = token.cancel_on_request(path.clone());
        assert!(watch.cleared_stale(), "the old request was not reported");
        std::thread::sleep(STOP_POLL * 3);
        assert!(!token.is_cancelled(), "a stale request stopped a new run");
    }

    /// Once the run is over, a request is not acted on by a watcher nobody
    /// holds any more.
    #[test]
    fn a_dropped_watch_no_longer_listens() {
        let dir = tempfile::tempdir().expect("a workspace");
        let path = stop_path_in(dir.path());
        let token = CancelToken::new();
        drop(token.cancel_on_request(path.clone()));

        crate::desktop::safety::request_stop(&path).expect("request");
        std::thread::sleep(STOP_POLL * 3);
        assert!(!token.is_cancelled(), "a finished run's watcher acted");
    }

    /// The whole point: the handle that cancels and the handle that checks are
    /// different objects. If cloning copied the flag instead of sharing it,
    /// every cancel would be silently lost.
    #[test]
    fn a_clone_shares_the_flag_rather_than_copying_it() {
        let held_by_the_loop = CancelToken::new();
        let held_by_the_handler = held_by_the_loop.clone();

        assert!(!held_by_the_loop.is_cancelled(), "must start uncancelled");
        held_by_the_handler.cancel();

        assert!(
            held_by_the_loop.is_cancelled(),
            "cancelling one handle must be visible through the other"
        );
    }

    /// Cancelling from another thread is the only way this is ever used — the
    /// signal handler never runs on the thread driving the turn.
    #[test]
    fn a_cancel_from_another_thread_is_seen_by_the_loop() {
        let token = CancelToken::new();
        let from_handler = token.clone();

        std::thread::spawn(move || from_handler.cancel())
            .join()
            .expect("the handler thread must finish");

        assert!(
            token.is_cancelled(),
            "a cross-thread cancel must be visible"
        );
    }

    #[test]
    fn reset_clears_a_previous_cancel() {
        let token = CancelToken::new();
        token.cancel();
        token.reset();
        assert!(
            !token.is_cancelled(),
            "reset must clear the flag, or a stale Ctrl-C at the prompt would \
             cancel the next turn"
        );
    }
}
