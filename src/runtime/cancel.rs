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
}

#[cfg(test)]
mod tests {
    use super::*;

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
