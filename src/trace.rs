//! What the harness actually did, written down as it happens.
//!
//! # Why this exists
//!
//! A day was lost to guessing. The input box moved and nothing said why; the
//! status line went blank and nothing said which of three writers had cleared
//! it; a model got the wrong work and nothing said which role had chosen it.
//! Every one of those was diagnosable in a minute FROM A LINE OF OUTPUT, and in
//! each case the line did not exist, so the answer was reached by editing code
//! and looking at the screen — which is the slowest instrument available and the
//! only one that cannot be reviewed afterwards.
//!
//! # One switch, not four
//!
//! There were three already: `INCORD_DEBUG_FOOTER`, `INCORD_DEBUG_TIMING` and
//! `INCORD_DEBUG_STREAM`, each with its own format and its own idea of where to
//! write. Three switches means knowing which one to set before you know what is
//! wrong, which is exactly backwards. This is one: [`TRACE_ENV`], on or off, and
//! everything that matters says what it did.
//!
//! # Cost when it is off
//!
//! One atomic read. The destination is resolved once and the macro checks
//! [`on`] before it formats anything, so a trace call on a hot path costs
//! nothing but a branch when nobody is looking.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Where the trace goes. A path writes there; `1`, `true` or `stderr` writes to
/// standard error, which is the one destination that always exists.
pub const TRACE_ENV: &str = "INCORD_TRACE";

/// Not yet decided.
const UNKNOWN: u8 = 0;
/// Decided, and nobody is listening.
const OFF: u8 = 1;
/// Decided, and someone is.
const ON: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(UNKNOWN);
static TO_STDERR: AtomicBool = AtomicBool::new(false);

/// The file, opened once and kept.
fn sink() -> &'static Mutex<Option<std::fs::File>> {
    static SINK: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();
    SINK.get_or_init(|| Mutex::new(None))
}

/// When the process started, so every line carries a relative time.
///
/// Relative rather than a clock: what a reader needs is the GAP between two
/// lines — the four seconds a lookup waited, the 120ms between repaints — and a
/// wall clock makes that arithmetic the reader's job.
fn started() -> Instant {
    static STARTED: OnceLock<Instant> = OnceLock::new();
    *STARTED.get_or_init(Instant::now)
}

/// Whether anything is listening.
#[must_use]
pub fn on() -> bool {
    match STATE.load(Ordering::Relaxed) {
        ON => true,
        OFF => false,
        _ => decide(),
    }
}

/// Read the environment once and remember the answer.
fn decide() -> bool {
    let _ = started();
    let Ok(value) = std::env::var(TRACE_ENV) else {
        STATE.store(OFF, Ordering::Relaxed);
        return false;
    };
    let value = value.trim();
    if value.is_empty() || value == "0" || value.eq_ignore_ascii_case("false") {
        STATE.store(OFF, Ordering::Relaxed);
        return false;
    }
    if value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("stderr") {
        TO_STDERR.store(true, Ordering::Relaxed);
        STATE.store(ON, Ordering::Relaxed);
        return true;
    }
    // A PATH. Opened here and held, because opening per line turns a trace into
    // the slowest thing in the loop it is measuring.
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(value)
    {
        Ok(file) => {
            if let Ok(mut held) = sink().lock() {
                *held = Some(file);
            }
            STATE.store(ON, Ordering::Relaxed);
            true
        }
        Err(e) => {
            // SAID, not swallowed. A trace that silently goes nowhere is worse
            // than no trace: the reader concludes the code never ran.
            eprintln!("incord: cannot write the trace to {value}: {e} — using stderr instead");
            TO_STDERR.store(true, Ordering::Relaxed);
            STATE.store(ON, Ordering::Relaxed);
            true
        }
    }
}

/// Write one line. Prefer the [`trace!`](crate::trace) macro, which skips the
/// formatting when nothing is listening.
pub fn write(area: &str, message: &str) {
    if !on() {
        return;
    }
    let line = format!(
        "{:>8.3}s {area:<8} {message}\n",
        started().elapsed().as_secs_f64()
    );
    if TO_STDERR.load(Ordering::Relaxed) {
        let _ = std::io::stderr().write_all(line.as_bytes());
        return;
    }
    if let Ok(mut held) = sink().lock() {
        if let Some(file) = held.as_mut() {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
    }
}

/// Record what happened, under an area name.
///
/// `trace!("route", "explore -> {model}")`. The arguments are not formatted
/// unless something is listening, so this is free in a normal run.
#[macro_export]
macro_rules! trace {
    ($area:expr, $($arg:tt)*) => {
        if $crate::trace::on() {
            $crate::trace::write($area, &format!($($arg)*));
        }
    };
}

/// Time one step and record how long it took.
///
/// The gap between two lines answers "what is slow"; this answers it for a step
/// that does not bracket neatly, and returns whatever the step returned.
pub fn timed<T>(area: &str, what: &str, step: impl FnOnce() -> T) -> T {
    if !on() {
        return step();
    }
    let began = Instant::now();
    let out = step();
    write(area, &format!("{what} took {:?}", began.elapsed()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The switch is read once and cached, so tests that change it would fight
    /// each other over a value neither had claimed.
    static ENV: Mutex<()> = Mutex::new(());

    #[test]
    fn nothing_is_written_when_nobody_is_listening() {
        // The whole cost argument: a trace call on a hot path must be a branch,
        // not a format and a write.
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        STATE.store(OFF, Ordering::Relaxed);
        assert!(!on());
        // Formatting would panic here if the macro evaluated its arguments.
        trace!("test", "{}", panic_if_formatted());
    }

    fn panic_if_formatted() -> &'static str {
        // Only reached if the macro formats while the trace is off, which is the
        // thing being asserted.
        panic!("the macro formatted its arguments while the trace was off");
    }

    #[test]
    fn a_line_carries_its_area_and_the_time_since_start() {
        // A reader needs the GAP between lines; that is the whole reason the
        // time is relative rather than a wall clock.
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("trace.log");
        std::env::set_var(TRACE_ENV, &path);
        STATE.store(UNKNOWN, Ordering::Relaxed);
        TO_STDERR.store(false, Ordering::Relaxed);
        if let Ok(mut held) = sink().lock() {
            *held = None;
        }

        assert!(on(), "a path switches it on");
        write("route", "explore -> cheap-model");

        let written = std::fs::read_to_string(&path).unwrap_or_default();
        std::env::remove_var(TRACE_ENV);
        STATE.store(OFF, Ordering::Relaxed);
        if let Ok(mut held) = sink().lock() {
            *held = None;
        }

        assert!(
            written.contains("route"),
            "the area names the subsystem: {written}"
        );
        assert!(
            written.contains("explore -> cheap-model"),
            "and the message survives: {written}"
        );
        assert!(written.contains('s'), "with a time on it: {written}");
    }

    #[test]
    fn an_empty_or_zero_value_is_off() {
        // `INCORD_TRACE=` in a shell profile must not turn it on for everyone.
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        for value in ["", "0", "false", "FALSE"] {
            std::env::set_var(TRACE_ENV, value);
            STATE.store(UNKNOWN, Ordering::Relaxed);
            assert!(!on(), "{value:?} should be off");
        }
        std::env::remove_var(TRACE_ENV);
        STATE.store(OFF, Ordering::Relaxed);
    }
}
