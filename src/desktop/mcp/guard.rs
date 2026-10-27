//! The screen guards a tool consults before it reads or touches anything: a
//! window that belongs to a blocked application, the focused window an input
//! would land in, a pointer point that is on no display or inside a blocked
//! window, and a capture region a blocked window overlaps.
//!
//! # Why these are in a file of their own
//!
//! Root cause: `desktop/mcp.rs` was 2,375 lines, over the code auditor's
//! 1,000-line file threshold, because it mixed the JSON-RPC transport, the
//! MCP protocol, the per-call gates, the tools and these guards in one file.
//! These are the refusals a single TOOL makes about the desktop it is aimed
//! at, which is a different question from whether the CALL may run at all
//! (that is [`super::gate`]). Moved here unchanged: every message, and the
//! order each tool asks them in, is as it was.
//!
//! # Wired to
//!
//! - **Called by** the tool steps in [`super::tools`] (`focus_window`,
//!   `set_value`, `element_action`, `move_mouse`, `click`, `ui_tree`,
//!   `click_element`, `type_text`, `paste_text`, `key`, `scroll`, `drag`,
//!   and the screenshot path `shoot`).
//! - **Calls** [`crate::desktop::limits::get`] for the blocked applications,
//!   [`crate::desktop::uia`] for the focused window and the window list, and
//!   [`crate::desktop::screen_info`] for the displays.

use crate::desktop::coords::PhysicalPoint;
use crate::desktop::uia::{self, WindowInfo};
use crate::desktop::Rect;

/// Refuse a window that belongs to a blocked application (a password
/// manager, or one admin.json names). Checked BEFORE anything is read from
/// it or done to it.
pub(super) fn refuse_blocked(info: &WindowInfo) -> Result<(), String> {
    refuse_app(&info.app)
}

/// Refuse an application by name; [`refuse_blocked`] for a window, and
/// called directly by `ui_tree` for the application a walked tree names.
pub(super) fn refuse_app(app: &str) -> Result<(), String> {
    match crate::desktop::limits::get().blocked(app) {
        Some(entry) if entry == crate::desktop::limits::UNNAMED_APP => Err(
            "refused: the application that owns that window could not be named, so it cannot be \
             told apart from a password manager; incode does not read or act on it."
                .to_string(),
        ),
        Some(entry) => Err(format!(
            "refused: that window belongs to \"{app}\", which is blocked (\"{entry}\" — a \
             password manager, or desktop_limits.blocked_apps). incode never reads or acts on it."
        )),
        None => Ok(()),
    }
}

/// The focused window, checked before an action that goes wherever the focus
/// is: refused when it belongs to a blocked application, and — when the
/// caller named `window` — when that window is not the focused one, so
/// keystrokes can never land in a window nobody meant.
pub(super) fn guard_focus(window: Option<&str>) -> Result<Option<WindowInfo>, String> {
    match (uia::foreground(), window) {
        (Ok(Some(focused)), want) => {
            refuse_blocked(&focused)?;
            if let Some(want) = want {
                if focused.id != want {
                    return Err(format!(
                        "refused: window {want} is not focused — \"{}\" ({}) is, so the input \
                         would go there. Call focus_window (or click in the window) first.",
                        focused.title, focused.app
                    ));
                }
            }
            Ok(Some(focused))
        }
        (Ok(None), Some(want)) => Err(format!(
            "refused: no window is focused, so the input would not reach window {want}. Call \
             focus_window first."
        )),
        (Err(e), Some(want)) => Err(format!(
            "refused: could not tell which window is focused ({e}), so the input cannot be \
             checked to reach window {want}"
        )),
        // Nothing named and nothing knowable: pixels-only desktops still work.
        (_, None) => Ok(None),
    }
}

/// Why a pointer action at `at` is refused, when no display in `screens`
/// holds it. The decision [`refuse_point`] makes, apart from reading the
/// displays.
pub(super) fn off_every_screen(at: PhysicalPoint, screens: &[Rect]) -> Option<String> {
    if screens.iter().any(|screen| screen.contains(at.x, at.y)) {
        return None;
    }
    Some(format!(
        "refused: {},{} is on no display, so the pointer would be pushed to a screen edge and \
         act on whatever is there; an element marked off-screen in ui_tree can be read, not \
         clicked — scroll its window to bring it into view and call ui_tree again",
        at.x, at.y
    ))
}

/// Every display's rectangle, in the physical pixels pointer actions take.
/// Refuses, as [`blocked_windows`] does, when they cannot be read: a point
/// that cannot be checked is not acted on.
///
/// Called by [`refuse_point`] and by `ui_tree` (to mark off-screen elements).
pub(super) fn screen_rects() -> Result<Vec<Rect>, String> {
    let info = crate::desktop::screen_info().map_err(|e| {
        format!(
            "refused: the displays could not be read ({e}), so this cannot be checked to be on \
             one"
        )
    })?;
    Ok(info.monitors.into_iter().map(|m| m.rect).collect())
}

/// The blocked windows on screen. A desktop whose windows cannot be listed
/// is refused rather than taken to have none: the check would otherwise
/// pass exactly when it can see nothing.
pub(super) fn blocked_windows() -> Result<Vec<WindowInfo>, String> {
    let windows = uia::list_windows().map_err(|e| {
        format!(
            "refused: the windows on screen could not be listed ({e}), so this cannot be checked \
             against blocked applications"
        )
    })?;
    Ok(windows
        .into_iter()
        .filter(|w| crate::desktop::limits::get().blocked(&w.app).is_some())
        .collect())
}

/// Refuse a pointer action at a point inside a blocked application's window.
///
/// By rectangle, not by what is drawn on top: a point inside a blocked
/// window is refused even where another window covers it, since stacking can
/// change between this check and the click, and a click that falls through
/// to a password manager is the one that must never happen.
///
/// And a point on no display at all: the OS clamps such a pointer to the
/// nearest screen edge, so the action would land on whatever is there, not
/// on what was aimed at — an element a page keeps 16,000 px off-screen for
/// screen readers (#52), shown by `ui_tree` as `off-screen`, or a coordinate
/// past the edge.
///
/// Called before every pointer action: `click`, `click_element`, `scroll`
/// (over an element or at the cursor) and `drag` (both ends).
pub(super) fn refuse_point(at: PhysicalPoint) -> Result<(), String> {
    if let Some(why) = off_every_screen(at, &screen_rects()?) {
        return Err(why);
    }
    match blocked_windows()?
        .iter()
        .find(|w| w.rect.contains(at.x, at.y))
    {
        Some(w) => Err(format!(
            "refused: {},{} is inside a window of a blocked application ({}); incode never acts \
             on it",
            at.x,
            at.y,
            if w.app.trim().is_empty() {
                "one whose name could not be read"
            } else {
                &w.app
            }
        )),
        None => Ok(()),
    }
}

/// Refuse a capture of `region` when any blocked window shares area with it:
/// a window's picture is a crop of the screen, so whatever overlaps it is in
/// the picture too.
///
/// Called by `shoot` in [`super::tools`], for a one-window screenshot.
pub(super) fn refuse_region(region: Rect) -> Result<(), String> {
    match blocked_windows()?.iter().find(|w| w.rect.overlaps(region)) {
        Some(_) => Err(
            "refused: a blocked application's window overlaps that area, and a capture would \
             show it. Move or minimise it first."
                .to_string(),
        ),
        None => Ok(()),
    }
}
