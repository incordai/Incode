//! The stop control, and the check that nobody is still supervising.
//!
//! # Why this is not a hotkey
//!
//! An earlier draft of `docs/COMPUTER_USE.md` 7b specified a global hotkey and
//! physical-input detection — "if the operator moves the mouse, stop". Decision
//! 7 made the desktop the agent's own workspace, with no human at it, and that
//! turned both into dead code: a hotkey nobody can press, and an
//! input-contention detector with only one input source. Shipping them would
//! have been worse than shipping nothing, because they read as protection.
//!
//! What replaces them has to work with nobody watching:
//!
//! - **[`Safety::stop`]** — an explicit, one-way halt. Reachable from any
//!   thread, so an HTTP handler on the harness's own port can call it while the
//!   input thread is mid-sequence.
//! - **The dead-man's switch** — if the harness stops calling
//!   [`Safety::heartbeat`], the desktop stops acting on its own. An orphaned
//!   input thread with nothing supervising it is the failure this exists for.
//!
//! # Stop and silence are deliberately different
//!
//! [`Safety::stop`] is permanent for the session: a kill switch that can be
//! un-pressed is not one. A lapsed heartbeat is **not** permanent — it means
//! "nobody is supervising *right now*", and if the supervisor comes back the
//! desktop may act again. A harness that crashed and restarted should not
//! require a human to unwedge it; a harness that is hung should leave the
//! desktop frozen the whole time it is hung. Recoverable-on-heartbeat is what
//! gives both.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::DesktopError;

/// How long the desktop keeps acting after the last heartbeat.
///
/// Long enough that a slow turn or a paused debugger does not trip it, short
/// enough that a crashed supervisor cannot leave a clicking thread running for
/// minutes. The harness is expected to beat far more often than this.
/// admin.json `desktop_limits.heartbeat_deadline_secs` may shorten it.
pub const DEFAULT_HEARTBEAT_DEADLINE: Duration =
    Duration::from_secs(super::limits::DEFAULT_HEARTBEAT_DEADLINE_SECS);

/// Why an action was refused.
///
/// Separate variants because the two mean opposite things to whoever reads the
/// message: one is "someone deliberately halted this", the other is "nothing is
/// watching". Collapsing them into one string would send an operator looking
/// for a person who never pressed anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// [`Safety::stop`] was called. Permanent for this session.
    Stopped,
    /// No heartbeat within the deadline. Clears if the supervisor returns.
    Unsupervised { silent_for: Duration },
}

impl Refusal {
    pub fn message(self) -> String {
        match self {
            Refusal::Stopped => {
                "refused: the desktop was stopped. This is permanent for this session — \
                 a new session is required, and retrying will not help."
                    .to_string()
            }
            Refusal::Unsupervised { silent_for } => format!(
                "refused: no heartbeat for {:.1}s, so nothing is supervising this desktop. \
                 Actions resume if the harness starts beating again.",
                silent_for.as_secs_f64()
            ),
        }
    }
}

struct Inner {
    stopped: AtomicBool,
    last_beat: Mutex<Instant>,
    deadline: Duration,
}

/// A stop control shared between the input thread and whatever supervises it.
///
/// Cloning shares the state rather than copying it — that is the point, and it
/// is why the HTTP side and the input thread can hold the same switch.
#[derive(Clone)]
pub struct Safety {
    inner: Arc<Inner>,
}

impl Safety {
    /// A live switch, counted as supervised from this moment.
    ///
    /// Starting at "beating" rather than "silent" is deliberate: the thing that
    /// constructs this is the supervisor, and it has just proved it is running
    /// by constructing it. Starting silent would refuse the first action of
    /// every session.
    pub fn new(deadline: Duration) -> Self {
        Safety {
            inner: Arc::new(Inner {
                stopped: AtomicBool::new(false),
                last_beat: Mutex::new(Instant::now()),
                deadline,
            }),
        }
    }

    /// A switch with the configured deadline: admin.json
    /// `desktop_limits.heartbeat_deadline_secs`, default
    /// [`DEFAULT_HEARTBEAT_DEADLINE`], stricter only.
    pub fn with_default_deadline() -> Self {
        Safety::new(super::limits::get().heartbeat_deadline)
    }

    /// Report that the supervisor is still alive.
    pub fn heartbeat(&self) {
        self.beat_at(Instant::now());
    }

    fn beat_at(&self, now: Instant) {
        if let Ok(mut last) = self.inner.last_beat.lock() {
            // Never move the mark backwards. An out-of-order beat from a slow
            // thread must not shorten the window a fresher one already granted.
            if now > *last {
                *last = now;
            }
        }
    }

    /// Halt the desktop. Permanent for this session, and safe to call twice.
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.inner.stopped.load(Ordering::SeqCst)
    }

    /// Whether an action may proceed.
    ///
    /// Takes `now` so the policy is testable without sleeping — a dead-man's
    /// switch tested with real waits is a test suite that takes a minute and
    /// still only checks one timing.
    pub fn check(&self, now: Instant) -> Result<(), Refusal> {
        if self.is_stopped() {
            return Err(Refusal::Stopped);
        }

        let last = match self.inner.last_beat.lock() {
            Ok(last) => *last,
            // A poisoned lock means a thread panicked while holding it. Refusing
            // is the only safe reading: the state cannot be trusted, and the
            // failure mode of a stuck desktop is far better than the failure
            // mode of an unsupervised one.
            Err(_) => {
                return Err(Refusal::Unsupervised {
                    silent_for: self.inner.deadline,
                })
            }
        };

        let silent_for = now.saturating_duration_since(last);
        if silent_for >= self.inner.deadline {
            return Err(Refusal::Unsupervised { silent_for });
        }

        Ok(())
    }

    /// [`Self::check`] as a `DesktopError`, for call sites that just propagate.
    pub fn authorise(&self, now: Instant) -> Result<(), DesktopError> {
        self.check(now)
            .map_err(|refusal| DesktopError::Os(refusal.message()))
    }
}

/// The file whose EXISTENCE means "stop this desktop".
///
/// # Why a file
///
/// Same reason as [`super::journal`], and it is the other half of the same
/// problem. `incode --desktop-mcp` is a separate process from everything that
/// might want to stop it, and [`Safety::stop`] is only reachable from inside
/// that process. A file is the one channel both ends already share.
///
/// # Why existence, and not contents
///
/// There is nothing to parse, so there is nothing to parse WRONG. A truncated
/// write, a half-flushed buffer, a file written by a shell redirect that has not
/// closed yet — none of them can produce a stop file that means "keep going".
/// The failure mode of an unreadable stop file is a stop, which is the correct
/// direction.
///
/// It sits beside the journal: an operator can only sensibly stop a desktop they
/// can watch, and deriving one path from the other means they cannot drift apart
/// into a desktop that reports to one directory and listens in another.
pub fn stop_file_beside(journal: &std::path::Path) -> std::path::PathBuf {
    journal.with_extension("stop")
}

/// The stop file for a workspace.
#[must_use]
pub fn stop_path_in(workspace: &std::path::Path) -> std::path::PathBuf {
    stop_file_beside(&super::journal::path_in(workspace))
}

/// Ask the desktop at this path to stop.
///
/// # Errors
///
/// Returns the I/O error. A caller must REPORT it rather than swallow it: a
/// stop request that silently failed is the worst outcome this module has, and
/// it is worse than no stop button at all.
pub fn request_stop(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        crate::folders::create_dir_all(parent)?;
    }
    // The timestamp is for a human reading the directory later. Nothing parses
    // it — see the note above.
    std::fs::write(
        path,
        format!(
            "stopped at {}\n",
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        ),
    )
}

/// Whether a stop has been requested at this path.
#[must_use]
pub fn stop_requested(path: &std::path::Path) -> bool {
    path.exists()
}

impl std::fmt::Debug for Safety {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Safety")
            .field("stopped", &self.is_stopped())
            .field("deadline", &self.inner.deadline)
            .finish()
    }
}

/// Beats a [`Safety`] for exactly as long as something external stays alive.
///
/// # Why the liveness test is the caller's, and must be
///
/// The obvious way to "make the heartbeat work" is a thread that beats on a
/// timer. That does not supervise anything — it is the desktop certifying its
/// own health, and it turns the dead-man's switch into a switch that is always
/// on while looking, in review, exactly like a fix.
///
/// So the beat is conditional on a predicate the CALLER supplies, and the
/// predicate must observe something that dies when the supervisor dies. For the
/// stdio MCP server (decision 4) that signal is EOF on stdin: when the agent
/// process goes, the pipe closes, and no timer can fake it. The supervisor
/// passes `|| !stdin_closed`, and this type never has to know what liveness
/// means.
///
/// # Failing safe in both directions
///
/// - Predicate returns false -> [`Safety::stop`], permanently. The supervisor
///   did not merely go quiet; it is gone.
/// - This handle is dropped -> the beating stops, so the switch trips on its
///   own deadline. Nothing extra is needed, and forgetting to shut down
///   cleanly cannot leave the desktop authorised.
pub struct Supervisor {
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Supervisor {
    /// Start beating `safety` every `interval` for as long as `alive` holds.
    ///
    /// `interval` should be comfortably shorter than the safety's deadline —
    /// beating once per deadline leaves no margin for a slow tick.
    pub fn start<F>(safety: Safety, interval: Duration, alive: F) -> Self
    where
        F: Fn() -> bool + Send + 'static,
    {
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);

        let handle = std::thread::Builder::new()
            .name("incode-desktop-supervisor".to_string())
            .spawn(move || {
                while !thread_shutdown.load(Ordering::SeqCst) {
                    if !alive() {
                        // Gone, not quiet. A stop is permanent, which is right:
                        // the thing that was supervising this desktop no longer
                        // exists, and a later beat from something else must not
                        // resurrect it.
                        safety.stop();
                        return;
                    }
                    safety.heartbeat();
                    std::thread::sleep(interval);
                }
            })
            .ok();

        Supervisor { shutdown, handle }
    }

    /// Stop beating and join. The switch is left to trip on its own deadline.
    pub fn shutdown(mut self) {
        self.stop_thread();
    }

    fn stop_thread(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop_thread();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deadline() -> Duration {
        Duration::from_secs(30)
    }

    #[test]
    fn a_fresh_switch_permits_action() {
        // Starting silent would refuse the first action of every session.
        let safety = Safety::new(deadline());
        assert_eq!(safety.check(Instant::now()), Ok(()));
    }

    #[test]
    fn stop_refuses_immediately_and_permanently() {
        let safety = Safety::new(deadline());
        let now = Instant::now();

        safety.stop();
        assert_eq!(safety.check(now), Err(Refusal::Stopped));

        // A heartbeat must NOT revive it. If it could, any still-running
        // supervisor would undo the stop on its next tick.
        safety.heartbeat();
        assert_eq!(safety.check(Instant::now()), Err(Refusal::Stopped));
    }

    #[test]
    fn stop_is_idempotent() {
        let safety = Safety::new(deadline());
        safety.stop();
        safety.stop();
        assert!(safety.is_stopped());
    }

    #[test]
    fn silence_past_the_deadline_refuses() {
        let safety = Safety::new(deadline());
        let start = Instant::now();
        safety.beat_at(start);

        assert_eq!(safety.check(start + Duration::from_secs(29)), Ok(()));

        match safety.check(start + Duration::from_secs(31)) {
            Err(Refusal::Unsupervised { silent_for }) => {
                assert!(silent_for >= Duration::from_secs(31));
            }
            other => panic!("expected an unsupervised refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_returning_supervisor_clears_silence_but_stop_stays() {
        let safety = Safety::new(deadline());
        let start = Instant::now();
        safety.beat_at(start);

        // Gone long enough to be refused...
        assert!(safety.check(start + Duration::from_secs(31)).is_err());

        // ...and back. A harness that crashed and restarted should not need a
        // human to unwedge it.
        safety.beat_at(start + Duration::from_secs(31));
        assert_eq!(safety.check(start + Duration::from_secs(32)), Ok(()));
    }

    #[test]
    fn an_out_of_order_heartbeat_cannot_shorten_the_window() {
        let safety = Safety::new(deadline());
        let start = Instant::now();

        safety.beat_at(start + Duration::from_secs(20));
        // A slow thread reporting an OLDER beat must not move the mark back,
        // which would make the desktop look unsupervised sooner than it is.
        safety.beat_at(start);

        assert_eq!(safety.check(start + Duration::from_secs(45)), Ok(()));
    }

    #[test]
    fn clones_share_one_switch() {
        // The whole point: the HTTP handler and the input thread hold the same
        // state, not two copies that drift.
        let safety = Safety::new(deadline());
        let other = safety.clone();

        other.stop();

        assert!(safety.is_stopped(), "stop did not reach the other handle");
        assert_eq!(safety.check(Instant::now()), Err(Refusal::Stopped));
    }

    #[test]
    fn stop_reaches_the_switch_from_another_thread() {
        // A stop that only works on the calling thread would be useless: the
        // input thread is busy when it matters.
        let safety = Safety::new(deadline());
        let remote = safety.clone();

        std::thread::spawn(move || remote.stop())
            .join()
            .expect("the stopping thread must finish");

        assert!(safety.is_stopped());
    }

    #[test]
    fn the_two_refusals_read_differently() {
        // They mean opposite things — one had a person behind it, the other had
        // nobody — and an operator must not be sent looking for the wrong one.
        let stopped = Refusal::Stopped.message();
        let silent = Refusal::Unsupervised {
            silent_for: Duration::from_secs(42),
        }
        .message();

        assert!(stopped.contains("permanent"), "got: {stopped}");
        assert!(silent.contains("supervising"), "got: {silent}");
        assert!(
            silent.contains("42"),
            "the message must name the gap: {silent}"
        );
        assert_ne!(stopped, silent);
    }

    /// A LIVE supervisor keeps the desktop working past the deadline.
    ///
    /// This is the hole that existed until now: `Safety` was wired into the
    /// input thread, nothing beat it, so a real deployment would have refused
    /// every action once the deadline passed. Short real durations, because the
    /// thing under test is the thread actually beating — a fake clock would
    /// test the arithmetic that `silence_past_the_deadline_refuses` already
    /// covers.
    #[test]
    fn a_live_supervisor_keeps_the_switch_open_past_its_deadline() {
        let safety = Safety::new(Duration::from_millis(150));
        let supervisor = Supervisor::start(safety.clone(), Duration::from_millis(20), || true);

        std::thread::sleep(Duration::from_millis(400));

        assert_eq!(
            safety.check(Instant::now()),
            Ok(()),
            "a beating supervisor must keep the desktop authorised"
        );
        supervisor.shutdown();
    }

    /// A supervisor that reports its subject is GONE stops permanently.
    #[test]
    fn a_dead_supervisor_stops_the_desktop_for_good() {
        let safety = Safety::new(Duration::from_secs(30));
        let alive = Arc::new(AtomicBool::new(true));
        let probe = Arc::clone(&alive);

        let supervisor = Supervisor::start(safety.clone(), Duration::from_millis(20), move || {
            probe.load(Ordering::SeqCst)
        });

        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(safety.check(Instant::now()), Ok(()), "should still be live");

        // The supervised thing dies.
        alive.store(false, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(120));

        assert_eq!(
            safety.check(Instant::now()),
            Err(Refusal::Stopped),
            "a supervisor that lost its subject must STOP, not merely go quiet"
        );
        supervisor.shutdown();
    }

    /// Dropping the supervisor must not leave the desktop authorised.
    ///
    /// Forgetting to shut down cleanly is the likeliest mistake, so the safe
    /// outcome has to be the automatic one: beating stops, the deadline does
    /// the rest.
    #[test]
    fn dropping_the_supervisor_lets_the_switch_trip_on_its_own() {
        let safety = Safety::new(Duration::from_millis(120));

        {
            let _supervisor = Supervisor::start(safety.clone(), Duration::from_millis(20), || true);
            std::thread::sleep(Duration::from_millis(60));
            assert_eq!(safety.check(Instant::now()), Ok(()));
        } // dropped here

        std::thread::sleep(Duration::from_millis(250));

        match safety.check(Instant::now()) {
            Err(Refusal::Unsupervised { .. }) => {}
            other => panic!("an unsupervised desktop must refuse, got {other:?}"),
        }
    }

    #[test]
    fn authorise_carries_the_reason_into_the_error() {
        let safety = Safety::new(deadline());
        safety.stop();

        let error = safety
            .authorise(Instant::now())
            .expect_err("a stopped desktop must refuse");
        assert!(
            error.to_string().contains("permanent"),
            "the refusal reason was lost: {error}"
        );
    }
}
