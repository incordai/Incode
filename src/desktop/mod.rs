//! Computer use — what the agent can see of a screen, and later what it can do
//! to one.
//!
//! The design and the decisions behind it are in `docs/COMPUTER_USE.md`. Three
//! of those decisions shape this module and are repeated here because a reader
//! who redraws them by accident will produce something that compiles and is
//! wrong:
//!
//! - **This is offered to a DELEGATED agent over MCP.** It is not dispatched by
//!   a loop in this crate, because there is no such loop — see
//!   [`crate::tools`], which records that it was deliberately removed. Nothing
//!   here may depend on [`crate::agent`].
//!
//! - **One coordinate space leaves this module: physical pixels, top-left
//!   origin, in virtual-desktop coordinates.** Platform quirks — Windows DPI
//!   virtualisation, macOS points-versus-pixels and its bottom-left origin —
//!   are converted at the boundary and never travel further. A second
//!   convention escaping into the tool surface is the defect this rule exists
//!   to prevent.
//!
//! - **Three platforms, not two.** The rest of the crate splits `cfg(windows)`
//!   against `cfg(not(windows))`. That is fine where the alternative is "some
//!   POSIX shell", but it is wrong here: macOS and Linux need different
//!   accessibility and capture backends. Nothing in this module may treat
//!   `not(windows)` as Linux.
//!
//! Windows came first (decision 8), then macOS (`macos.rs`) and Linux on X11
//! (`linux.rs`). Any other platform returns a [`DesktopError::Unsupported`]
//! that names itself, rather than silently reporting an empty or invented
//! display.

use std::fmt;

use serde::{Deserialize, Serialize};

pub mod capture;
pub mod coords;
pub mod dpi;
pub mod journal;
pub mod limits;
pub mod mcp;
pub mod safety;
pub mod thread;
pub mod uia;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(windows)]
mod win32;
// Pure, so its tests run on every platform; only Linux uses it.
#[cfg(any(test, target_os = "linux"))]
pub(crate) mod x11;

/// Why a desktop query could not be answered.
///
/// Deliberately not `anyhow::Error`: the MCP surface has to turn these into
/// distinct, actionable messages, and "this platform has no implementation" is
/// a different conversation from "the OS call failed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopError {
    /// No implementation for this target yet. Carries the platform so the
    /// message can say which one.
    Unsupported(&'static str),

    /// An OS call failed. Carries what was being attempted, because the caller
    /// cannot see which of several calls went wrong.
    Os(String),
}

impl fmt::Display for DesktopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DesktopError::Unsupported(platform) => write!(
                f,
                "computer use is not implemented on {platform} yet (see docs/COMPUTER_USE.md)"
            ),
            DesktopError::Os(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for DesktopError {}

/// A rectangle in physical pixels, top-left origin.
///
/// Width and height rather than right/bottom, because every consumer wants a
/// size and the conversion is a subtraction that was already wrong once in
/// every codebase that kept the Win32 shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    /// From a Win32-style edge rectangle.
    pub fn from_edges(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Rect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        }
    }

    /// The point at the centre — where a click on this rectangle should land.
    ///
    /// Rounds toward the top-left on an odd dimension. Which way it rounds does
    /// not matter; that it is deterministic does, because a click target that
    /// moves by a pixel between calls is a test that fails once a week.
    pub fn center(self) -> (i32, i32) {
        (self.x + self.width / 2, self.y + self.height / 2)
    }

    /// The smallest rectangle containing both.
    pub fn union(self, other: Rect) -> Rect {
        let left = self.x.min(other.x);
        let top = self.y.min(other.y);
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);
        Rect::from_edges(left, top, right, bottom)
    }

    /// Whether the point is inside: the left and top edges are, the right
    /// and bottom are not. In i64, so no rectangle or point can overflow it.
    pub fn contains(self, x: i32, y: i32) -> bool {
        let (x, y) = (i64::from(x), i64::from(y));
        let (left, top) = (i64::from(self.x), i64::from(self.y));
        x >= left
            && y >= top
            && x < left + i64::from(self.width)
            && y < top + i64::from(self.height)
    }

    /// Whether the two share any area.
    pub fn overlaps(self, other: Rect) -> bool {
        let edges = |r: Rect| {
            let (x, y) = (i64::from(r.x), i64::from(r.y));
            (x, y, x + i64::from(r.width), y + i64::from(r.height))
        };
        let (l1, t1, r1, b1) = edges(self);
        let (l2, t2, r2, b2) = edges(other);
        l1 < r2 && l2 < r1 && t1 < b2 && t2 < b1
    }
}

/// One display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// Index within a single [`screen_info`] result. NOT stable across calls —
    /// monitors can be added, removed or reordered — so it is never persisted
    /// or used as a handle.
    pub id: u32,

    /// The OS device name, e.g. `\\.\DISPLAY1`. Useful in a report; not a key.
    pub name: String,

    /// Position and size in physical pixels, in virtual-desktop coordinates.
    ///
    /// `x` and `y` CAN BE NEGATIVE: a display placed to the left of the primary
    /// one starts at a negative x. Code that assumes a zero origin works on one
    /// monitor and fails on the arrangement half of developers actually use.
    pub rect: Rect,

    /// The same area minus taskbars and other reserved space.
    pub work_area: Rect,

    /// Effective DPI. 96 is 100% scaling.
    pub dpi: u32,

    /// `physical_pixels = logical_pixels * scale`. 1.5 at 150%.
    pub scale: f64,

    pub primary: bool,
}

/// Everything known about the displays, plus whether that knowledge is sound.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenInfo {
    /// Carried alongside the geometry ON PURPOSE. Without it a consumer cannot
    /// tell a correct 2560x1440 reading from a virtualised one taken on a
    /// 3840x2160 display, and those look identical in every other field.
    pub dpi_awareness_summary: String,

    /// False when the platform is virtualising coordinates, which makes every
    /// rectangle below wrong by the scaling ratio.
    pub coordinates_are_physical: bool,

    pub monitors: Vec<MonitorInfo>,

    /// Permissions the OS has not granted this program, by name (macOS:
    /// "Accessibility", "Screen Recording"). Without them the OS drops input
    /// and blanks captures silently, so they are reported rather than left
    /// to look like a broken tool. Empty everywhere else.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_grants: Vec<String>,

    /// Linux: the X display this server drives (`":99"`). An application
    /// appears here only when it is started on it — `DISPLAY=:99`, with
    /// `WAYLAND_DISPLAY` unset so the toolkit does not pick Wayland instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

impl ScreenInfo {
    /// The primary display, if the OS reported one.
    pub fn primary(&self) -> Option<&MonitorInfo> {
        self.monitors.iter().find(|m| m.primary)
    }

    /// The bounding box of every display — the whole virtual desktop.
    ///
    /// `None` when there are no monitors, which is a real state on a headless
    /// host and must not be confused with a zero-sized desktop.
    pub fn virtual_bounds(&self) -> Option<Rect> {
        self.monitors
            .iter()
            .map(|m| m.rect)
            .reduce(|acc, r| acc.union(r))
    }

    /// True when the displays do not all share one scale factor.
    ///
    /// Worth reporting: a single global scale constant is correct on a uniform
    /// desktop and silently wrong on a mixed one, so a consumer that is about
    /// to take that shortcut can be told not to.
    pub fn has_mixed_scaling(&self) -> bool {
        let mut seen = self.monitors.iter().map(|m| m.dpi);
        match seen.next() {
            Some(first) => seen.any(|dpi| dpi != first),
            None => false,
        }
    }
}

/// Describe the displays, after making sure the process is allowed to see them
/// truthfully.
///
/// Calls [`dpi::ensure_per_monitor_aware`] FIRST. Enumerating before that would
/// read virtualised geometry and cache a wrong answer, which is the exact
/// failure this module was built to remove.
pub fn screen_info() -> Result<ScreenInfo, DesktopError> {
    let awareness = dpi::ensure_per_monitor_aware();
    let monitors = platform_monitors()?;

    #[cfg(target_os = "macos")]
    let missing_grants = macos::missing_grants();
    #[cfg(not(target_os = "macos"))]
    let missing_grants = Vec::new();

    #[cfg(target_os = "linux")]
    let display = linux::attached_display();
    #[cfg(not(target_os = "linux"))]
    let display = None;

    Ok(ScreenInfo {
        dpi_awareness_summary: awareness.summary(),
        coordinates_are_physical: awareness.coordinates_are_physical(),
        monitors,
        missing_grants,
        display,
    })
}

#[cfg(windows)]
fn platform_monitors() -> Result<Vec<MonitorInfo>, DesktopError> {
    win32::monitors()
}

/// macOS and Linux: the displays as the capture library sees them, which is
/// in the same units input uses — points on macOS, pixels on X11.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn platform_monitors() -> Result<Vec<MonitorInfo>, DesktopError> {
    capture::monitor_infos()
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn platform_monitors() -> Result<Vec<MonitorInfo>, DesktopError> {
    Err(DesktopError::Unsupported("this platform"))
}

/// Scale factor from effective DPI. 96 DPI is 100%.
///
/// A free function rather than a method so it can be tested on every platform,
/// including the ones with no monitor implementation yet.
#[cfg_attr(not(windows), allow(dead_code))] // the monitor list's on Windows
pub(crate) fn scale_from_dpi(dpi: u32) -> f64 {
    if dpi == 0 {
        // Never observed, but a zero here would divide the whole coordinate
        // system by nothing. 96 (100%) is the one fallback that cannot make
        // geometry worse than leaving it unscaled.
        return 1.0;
    }
    f64::from(dpi) / 96.0
}

/// Decode a fixed-size, NUL-padded UTF-16 OS string.
///
/// Win32 fills the tail of `szDevice` with zeroes; decoding the whole buffer
/// yields a name with a run of NULs on the end that looks fine in a debugger
/// and breaks any comparison.
#[cfg_attr(not(windows), allow(dead_code))] // Win32 strings only
pub(crate) fn decode_utf16_nul_padded(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: u32, rect: Rect, dpi: u32, primary: bool) -> MonitorInfo {
        MonitorInfo {
            id,
            name: format!("\\\\.\\DISPLAY{}", id + 1),
            rect,
            work_area: rect,
            dpi,
            scale: scale_from_dpi(dpi),
            primary,
        }
    }

    fn info(monitors: Vec<MonitorInfo>) -> ScreenInfo {
        ScreenInfo {
            dpi_awareness_summary: "test".to_string(),
            coordinates_are_physical: true,
            monitors,
            missing_grants: Vec::new(),
            display: None,
        }
    }

    #[test]
    fn edges_convert_to_position_and_size() {
        let r = Rect::from_edges(100, 200, 900, 800);
        assert_eq!(r.x, 100);
        assert_eq!(r.y, 200);
        assert_eq!(r.width, 800);
        assert_eq!(r.height, 600);
    }

    #[test]
    fn edges_convert_with_a_negative_origin() {
        // A display to the LEFT of primary. The arrangement that breaks naive
        // code, so it is pinned rather than assumed.
        let r = Rect::from_edges(-1920, 0, 0, 1080);
        assert_eq!(r.x, -1920);
        assert_eq!(r.width, 1920);
        assert_eq!(r.height, 1080);
    }

    #[test]
    fn center_is_inside_the_rect_and_deterministic() {
        let r = Rect::from_edges(10, 20, 31, 41); // odd 21x21
        let (cx, cy) = r.center();
        assert_eq!((cx, cy), (20, 30));
        assert_eq!(r.center(), r.center());
        assert!(cx >= r.x && cx < r.x + r.width);
        assert!(cy >= r.y && cy < r.y + r.height);
    }

    #[test]
    fn center_of_a_negative_origin_rect_stays_negative() {
        let r = Rect::from_edges(-1920, 0, 0, 1080);
        assert_eq!(r.center(), (-960, 540));
    }

    #[test]
    fn union_spans_both_rects() {
        let a = Rect::from_edges(0, 0, 1920, 1080);
        let b = Rect::from_edges(1920, 0, 3840, 1080);
        assert_eq!(a.union(b), Rect::from_edges(0, 0, 3840, 1080));
    }

    #[test]
    fn union_is_order_independent() {
        let a = Rect::from_edges(-1920, -200, 0, 880);
        let b = Rect::from_edges(0, 0, 2560, 1440);
        assert_eq!(a.union(b), b.union(a));
    }

    /// A point on the right or bottom edge is outside, as it is for the next
    /// rectangle along; nothing near `i32::MAX` overflows.
    #[test]
    fn contains_and_overlaps_are_half_open() {
        let r = Rect::from_edges(100, 100, 200, 150);
        assert!(r.contains(100, 100));
        assert!(r.contains(199, 149));
        assert!(!r.contains(200, 120));
        assert!(!r.contains(150, 150));
        assert!(!r.contains(i32::MIN, i32::MAX));
        assert!(r.overlaps(Rect::from_edges(199, 149, 300, 300)));
        assert!(
            !r.overlaps(Rect::from_edges(200, 100, 300, 150)),
            "touching"
        );
        let huge = Rect {
            x: i32::MAX - 1,
            y: 0,
            width: i32::MAX,
            height: 10,
        };
        assert!(huge.contains(i32::MAX, 5));
        assert!(!r.overlaps(huge));
    }

    #[test]
    fn virtual_bounds_covers_a_left_hand_second_monitor() {
        let screen = info(vec![
            monitor(0, Rect::from_edges(0, 0, 2560, 1440), 144, true),
            monitor(1, Rect::from_edges(-1920, 0, 0, 1080), 96, false),
        ]);

        assert_eq!(
            screen.virtual_bounds(),
            Some(Rect::from_edges(-1920, 0, 2560, 1440))
        );
    }

    #[test]
    fn virtual_bounds_of_no_monitors_is_none_not_zero() {
        // A headless host really has no desktop. Reporting a 0x0 rectangle
        // would let a caller compute coordinates inside a display that is not
        // there.
        assert_eq!(info(vec![]).virtual_bounds(), None);
    }

    #[test]
    fn primary_is_found_regardless_of_order() {
        let screen = info(vec![
            monitor(0, Rect::from_edges(0, 0, 1920, 1080), 96, false),
            monitor(1, Rect::from_edges(1920, 0, 3840, 1080), 96, true),
        ]);
        assert_eq!(screen.primary().map(|m| m.id), Some(1));
    }

    #[test]
    fn no_primary_reported_is_none_not_a_guess() {
        let screen = info(vec![monitor(
            0,
            Rect::from_edges(0, 0, 1920, 1080),
            96,
            false,
        )]);
        assert!(screen.primary().is_none());
    }

    #[test]
    fn mixed_scaling_is_detected() {
        let mixed = info(vec![
            monitor(0, Rect::from_edges(0, 0, 2560, 1440), 144, true),
            monitor(1, Rect::from_edges(2560, 0, 4480, 1080), 96, false),
        ]);
        assert!(mixed.has_mixed_scaling());

        let uniform = info(vec![
            monitor(0, Rect::from_edges(0, 0, 1920, 1080), 96, true),
            monitor(1, Rect::from_edges(1920, 0, 3840, 1080), 96, false),
        ]);
        assert!(!uniform.has_mixed_scaling());

        assert!(!info(vec![]).has_mixed_scaling());
    }

    #[test]
    fn scale_matches_the_windows_scaling_presets() {
        assert_eq!(scale_from_dpi(96), 1.0);
        assert_eq!(scale_from_dpi(120), 1.25);
        assert_eq!(scale_from_dpi(144), 1.5);
        assert_eq!(scale_from_dpi(192), 2.0);
    }

    #[test]
    fn zero_dpi_falls_back_to_unscaled_rather_than_dividing_by_nothing() {
        assert_eq!(scale_from_dpi(0), 1.0);
    }

    #[test]
    fn device_name_stops_at_the_first_nul() {
        let mut buf = [0u16; 32];
        for (slot, ch) in buf.iter_mut().zip("\\\\.\\DISPLAY1".encode_utf16()) {
            *slot = ch;
        }
        assert_eq!(decode_utf16_nul_padded(&buf), "\\\\.\\DISPLAY1");
    }

    #[test]
    fn device_name_handles_a_full_buffer_with_no_nul() {
        let buf = [b'A' as u16; 4];
        assert_eq!(decode_utf16_nul_padded(&buf), "AAAA");
    }

    #[test]
    fn unsupported_platform_names_itself() {
        let msg = DesktopError::Unsupported("macOS").to_string();
        assert!(msg.contains("macOS"), "got: {msg}");
        assert!(msg.contains("COMPUTER_USE.md"), "got: {msg}");
    }

    /// The whole point of carrying awareness beside the geometry: a consumer
    /// must be able to distinguish a sound reading from a virtualised one.
    #[test]
    fn screen_info_carries_whether_its_own_geometry_is_trustworthy() {
        let sound = info(vec![monitor(
            0,
            Rect::from_edges(0, 0, 3840, 2160),
            144,
            true,
        )]);
        assert!(sound.coordinates_are_physical);

        let suspect = ScreenInfo {
            dpi_awareness_summary: dpi::Awareness::Virtualised(0).summary(),
            coordinates_are_physical: dpi::Awareness::Virtualised(0).coordinates_are_physical(),
            monitors: sound.monitors.clone(),
            missing_grants: Vec::new(),
            display: None,
        };
        assert!(!suspect.coordinates_are_physical);
        assert!(suspect.dpi_awareness_summary.contains("UNAWARE"));
    }
}
