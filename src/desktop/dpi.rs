//! Process DPI awareness — set once, before anything reads the screen.
//!
//! # Why this runs before everything else
//!
//! A Windows process that has not declared per-monitor DPI awareness does not
//! see the display it is actually attached to. The OS *virtualises* the
//! coordinate space: a 3840x2160 panel at 150% scaling reports itself as
//! 2560x1440, captures come back at one size while clicks are interpreted in
//! another, and every coordinate is off by the scaling ratio.
//!
//! That failure is silent. Nothing errors; the click simply lands on the wrong
//! control, which is indistinguishable from the model having misread the
//! screen. So this is the first thing built and the first thing called.
//!
//! # Why the return value of the set call is not trusted
//!
//! `SetProcessDpiAwarenessContext` returns FALSE both when the context is
//! invalid and when awareness was *already* set — by an application manifest,
//! or by a library that touched the desktop first. Those mean opposite things,
//! and only one of them is a problem. Rather than guess from a boolean, this
//! module sets the value and then **reads back what the process actually has**,
//! so the caller is told the observed state rather than the attempted one.

use std::sync::OnceLock;

/// What the process's DPI awareness actually is, as measured after the attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Awareness {
    /// Per-monitor aware. Screen coordinates are true physical pixels, and
    /// per-monitor scale factors are meaningful.
    PerMonitor,

    /// The OS is virtualising coordinates. Carries the raw `DPI_AWARENESS`
    /// value so a report can say *which* wrong state it is (0 = unaware,
    /// 1 = system-aware) instead of just "not what we wanted".
    ///
    /// Any geometry read in this state is suspect, and callers should say so
    /// rather than quietly scaling it.
    Virtualised(i32),

    /// Not a Windows target. Other platforms express scaling per-monitor
    /// instead of per-process, so there is nothing to set here.
    NotApplicable,

    /// macOS: the desktop is measured in POINTS, top-left origin at the main
    /// display — the space mouse events and the accessibility tree both use.
    /// A screenshot is in pixels; each display's `scale` is its pixels per
    /// point, and every image carries its own transform back to points.
    Points,
}

impl Awareness {
    /// Whether coordinates read in this state can be trusted as physical pixels.
    ///
    /// `NotApplicable` is trustworthy: on macOS and Linux the scale factor
    /// travels with each monitor rather than being a process-wide mode, so
    /// there is no virtualisation to defeat.
    pub fn coordinates_are_physical(self) -> bool {
        matches!(self, Awareness::PerMonitor | Awareness::NotApplicable)
    }

    /// A one-line description for an operator-facing report.
    pub fn summary(self) -> String {
        match self {
            Awareness::PerMonitor => "per-monitor aware (coordinates are physical)".to_string(),
            Awareness::Virtualised(0) => {
                "DPI-UNAWARE — the OS is virtualising coordinates, geometry is wrong".to_string()
            }
            Awareness::Virtualised(1) => {
                "system-aware only — geometry is wrong on any non-primary scaling".to_string()
            }
            Awareness::Virtualised(other) => {
                format!("not per-monitor aware (DPI_AWARENESS = {other}) — geometry is suspect")
            }
            Awareness::NotApplicable => "not applicable on this platform".to_string(),
            Awareness::Points => "macOS: coordinates are points (top-left, main display); \
                                  each display's scale is its pixels per point"
                .to_string(),
        }
    }
}

/// Cached because the OS only honours the first attempt: whoever sets process
/// DPI awareness first wins, and later calls are refused. Running this twice
/// would produce a second, misleading "refused" signal for the same state.
static AWARENESS: OnceLock<Awareness> = OnceLock::new();

/// Declare per-monitor-v2 DPI awareness, then report what the process actually
/// has.
///
/// Safe to call repeatedly; only the first call touches the OS. Must be called
/// before any capture or coordinate read — see the module docs for what happens
/// when it is not.
pub fn ensure_per_monitor_aware() -> Awareness {
    *AWARENESS.get_or_init(set_and_measure)
}

#[cfg(windows)]
fn set_and_measure() -> Awareness {
    use windows_sys::Win32::UI::HiDpi::{
        GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        DPI_AWARENESS_PER_MONITOR_AWARE,
    };

    // SAFETY: all three are plain Win32 calls with no pointer arguments we own.
    // `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` is a sentinel value, not a
    // real pointer, so nothing is dereferenced. The result of the set is
    // deliberately discarded — see the module docs — and the awareness is read
    // back instead.
    let raw = unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // Thread-scoped by name, but it reports the process default for any
        // thread that has not overridden it, and nothing in this crate does.
        GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext())
    };

    if raw == DPI_AWARENESS_PER_MONITOR_AWARE {
        Awareness::PerMonitor
    } else {
        Awareness::Virtualised(raw)
    }
}

#[cfg(target_os = "macos")]
fn set_and_measure() -> Awareness {
    Awareness::Points
}

#[cfg(not(any(windows, target_os = "macos")))]
fn set_and_measure() -> Awareness {
    Awareness::NotApplicable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_monitor_and_not_applicable_are_both_trustworthy() {
        assert!(Awareness::PerMonitor.coordinates_are_physical());
        assert!(Awareness::NotApplicable.coordinates_are_physical());
    }

    #[test]
    fn virtualised_is_never_trustworthy() {
        for raw in [-1, 0, 1, 3] {
            assert!(
                !Awareness::Virtualised(raw).coordinates_are_physical(),
                "DPI_AWARENESS {raw} must not be reported as physical"
            );
        }
    }

    /// The summary exists so a failure NAMES itself rather than surfacing as a
    /// mysterious offset later, so each wrong state must read differently.
    #[test]
    fn each_virtualised_state_describes_itself_distinctly() {
        let unaware = Awareness::Virtualised(0).summary();
        let system = Awareness::Virtualised(1).summary();
        let other = Awareness::Virtualised(7).summary();

        assert!(unaware.contains("UNAWARE"), "got: {unaware}");
        assert!(system.contains("system-aware"), "got: {system}");
        assert!(other.contains('7'), "got: {other}");
        assert_ne!(unaware, system);
        assert_ne!(system, other);
    }

    #[test]
    fn repeated_calls_agree() {
        // The OS honours only the first attempt, so a second call reporting
        // something different would mean the cache was not doing its job.
        assert_eq!(ensure_per_monitor_aware(), ensure_per_monitor_aware());
    }

    #[cfg(windows)]
    #[test]
    fn windows_process_reaches_per_monitor_awareness() {
        // Nothing in this crate sets awareness before the test harness runs, so
        // this call should be the one that wins. If it does not, every
        // coordinate this module reports is virtualised and the whole design
        // rests on a false reading — worth failing loudly for.
        let awareness = ensure_per_monitor_aware();
        assert_eq!(
            awareness,
            Awareness::PerMonitor,
            "process is {} — see src/desktop/dpi.rs module docs",
            awareness.summary()
        );
    }

    /// macOS speaks points, and says so: not "physical", and a summary that
    /// names the unit, so a reader is not left to guess why rectangles are
    /// half the size of a Retina screenshot.
    #[test]
    fn macos_points_are_reported_as_points() {
        assert!(!Awareness::Points.coordinates_are_physical());
        assert!(Awareness::Points.summary().contains("points"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reports_points() {
        assert_eq!(ensure_per_monitor_aware(), Awareness::Points);
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn non_windows_reports_not_applicable() {
        assert_eq!(ensure_per_monitor_aware(), Awareness::NotApplicable);
    }
}
