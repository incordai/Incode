//! Painting the screen, now that the program owns it.
//!
//! # Why the scroll region is gone
//!
//! The box used to be held down by a terminal scroll region: reset the region,
//! print newlines to scroll the conversation, clear some rows, set the region
//! back. Four separate defects came out of that dance — the command menu
//! leaking into the transcript, the box drawn twice, the box vanishing, and the
//! conversation drifting upward. Each fix moved the problem rather than ending
//! it, because the terminal was doing the layout and we were steering it with
//! side effects.
//!
//! On the alternate screen there is nothing to share. So nothing scrolls: every
//! row is painted where it belongs, every time. A box that is never moved
//! cannot be moved wrongly.
//!
//! # Who paints
//!
//! Two callers, one function. The input box paints between keystrokes; the
//! status line's ticker paints while a turn runs, which is when output arrives
//! and the box is not reading. They agree on the layout through [`box_rows`],
//! so neither needs to know anything about the other.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::Mutex;

use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};

use super::viewport::{self, Viewport};

/// Synchronized update sequences to avoid frame tearing and blinking.
const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";

/// How many bottom rows the input box occupies.
///
/// Shared so the status line can paint the conversation without knowing what
/// the box looks like, or whether its command menu happens to be open.
static BOX_ROWS: AtomicU16 = AtomicU16::new(0);

/// How far back through the conversation the operator has scrolled.
static VIEW: Mutex<Viewport> = Mutex::new(Viewport::new());

static LAST_PAINT_VERSION: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_PAINT_WIDTH: AtomicU16 = AtomicU16::new(0);
static LAST_PAINT_HEIGHT: AtomicU16 = AtomicU16::new(0);
static VIEW_DIRTY: AtomicBool = AtomicBool::new(true);

/// Mark that the conversation view has changed and must be repainted.
pub fn mark_dirty() {
    VIEW_DIRTY.store(true, Ordering::Relaxed);
}

/// Rows kept for the activity line, between conversation and box.
pub const STATUS_ROWS: u16 = 1;

/// Say how many rows the box needs.
pub fn set_box_rows(rows: u16) {
    BOX_ROWS.store(rows, Ordering::Relaxed);
}

/// How many rows the box currently occupies.
#[must_use]
pub fn box_rows() -> u16 {
    BOX_ROWS.load(Ordering::Relaxed)
}

/// The exact bytes the input box last painted.
///
/// # Why the box needs an owner that is not the box
///
/// The box is drawn by the thread READING it, and that thread is parked for the
/// whole of a turn. The ticker in [`crate::cli::render`] repaints the
/// conversation every 120ms and paints at ABSOLUTE rows, so anything that
/// scrolls the screen underneath it — a raw write on the unbuffered path —
/// moves the box off its rows and nothing puts it back. The box was gone until
/// the operator typed, which is exactly when they had already lost it.
///
/// A frame is self-contained: synchronised-update markers, absolute `MoveTo`
/// for every row, and no dependence on where the cursor was. So replaying it is
/// the same as drawing it, and the ticker can own the box without owning the
/// [`Footer`](crate::cli::footer::Footer) that built it.
static BOX_FRAME: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// The frame a replay would paint, or `None` when the box has never drawn.
///
/// Separated from [`repaint_box`] so the thing worth asserting — that the box's
/// bytes reach a place another thread can read — can be asserted without a
/// terminal.
#[must_use]
pub fn frame_to_replay() -> Option<Vec<u8>> {
    let frame = BOX_FRAME.lock().ok()?;
    (!frame.is_empty()).then(|| frame.clone())
}

/// Record the frame the box just painted, so the ticker can put it back.
pub fn set_box_frame(frame: Vec<u8>) {
    if let Ok(mut held) = BOX_FRAME.lock() {
        *held = frame;
    }
}

/// Paint the box's last frame again.
///
/// The caret is HIDDEN afterwards even though the frame ends by showing it: a
/// replay happens while a turn is running, and a caret parked in the box during
/// a turn reads as the box being ready for input when it is not. The box shows
/// it again itself the moment it draws for real.
pub fn repaint_box() {
    let Some(frame) = frame_to_replay() else {
        return;
    };
    let mut out = std::io::stdout();
    let _ = out.write_all(&frame);
    let _ = crossterm::execute!(out, crossterm::cursor::Hide);
    let _ = out.flush();
}

/// The first row the box owns; everything above it is ours to paint.
#[must_use]
pub fn first_box_row(height: u16) -> u16 {
    height.saturating_sub(box_rows())
}

/// The row the activity line sits on, directly above the box.
#[must_use]
pub fn status_row(height: u16) -> u16 {
    first_box_row(height).saturating_sub(STATUS_ROWS)
}

/// How many rows the conversation gets.
#[must_use]
pub fn conversation_height(height: u16) -> usize {
    usize::from(status_row(height))
}

// ---------------------------------------------------------------------------
// Scrolling
// ---------------------------------------------------------------------------

/// Move back through the conversation by `rows`.
pub fn scroll_up(rows: usize, width: u16, height: u16) {
    let total = wrapped(width).len();
    if let Ok(mut view) = VIEW.lock() {
        view.up(rows, total, conversation_height(height));
    }
    mark_dirty();
}

/// Move towards the newest output by `rows`.
pub fn scroll_down(rows: usize) {
    if let Ok(mut view) = VIEW.lock() {
        view.down(rows);
    }
    mark_dirty();
}

/// Jump to the newest output.
pub fn scroll_to_bottom() {
    if let Ok(mut view) = VIEW.lock() {
        view.to_bottom();
    }
    mark_dirty();
}

/// Whether the conversation is following new output.
#[must_use]
pub fn at_bottom() -> bool {
    VIEW.lock().map_or(true, |v| v.at_bottom())
}

/// The conversation as display rows for this width.
fn wrapped(width: u16) -> Vec<String> {
    let mut lines = super::console::lines();
    // The line still being written belongs on screen too, or a streaming reply
    // is invisible until it ends.
    let pending = super::console::pending();
    if !pending.is_empty() {
        lines.push(pending);
    }
    // Clamping to width.saturating_sub(1) prevents writing in the terminal's rightmost column,
    // which otherwise triggers terminal auto-wrap and hardware scrolling!
    viewport::wrap(&lines, usize::from(width.saturating_sub(1).max(1)))
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

/// Paint the conversation into the rows above the box.
///
/// Every row is written, including the blank ones. Painting only the rows that
/// have content leaves whatever was there before showing through — which is how
/// a command menu ended up sitting in the middle of the transcript.
pub fn conversation(width: u16, height: u16) {
    let rows = conversation_height(height);
    if rows == 0 {
        return;
    }
    let current_version = super::console::version();
    let is_dirty = VIEW_DIRTY.swap(false, Ordering::Relaxed);
    let prev_version = LAST_PAINT_VERSION.load(Ordering::Relaxed);
    let prev_w = LAST_PAINT_WIDTH.load(Ordering::Relaxed);
    let prev_h = LAST_PAINT_HEIGHT.load(Ordering::Relaxed);

    if !is_dirty && current_version == prev_version && width == prev_w && height == prev_h {
        return;
    }

    LAST_PAINT_VERSION.store(current_version, Ordering::Relaxed);
    LAST_PAINT_WIDTH.store(width, Ordering::Relaxed);
    LAST_PAINT_HEIGHT.store(height, Ordering::Relaxed);

    let all = wrapped(width);
    if let Ok(mut view) = VIEW.lock() {
        view.clamp(all.len(), rows);
    }
    let window: Vec<String> = VIEW
        .lock()
        .map(|v| v.window(&all, rows).to_vec())
        .unwrap_or_default();

    let mut out = std::io::stdout();
    let _ = write!(out, "{SYNC_BEGIN}");
    let max_col = usize::from(width.saturating_sub(1).max(1));
    // Older content sits at the top, so a short conversation starts at the top
    // rather than floating in the middle of an empty screen.
    for row in 0..rows {
        let text = window.get(row).map_or("", String::as_str);
        let clipped = crate::cli::footer::fit(text, max_col);
        let _ = execute!(out, MoveTo(0, u16::try_from(row).unwrap_or(0)));
        let _ = write!(out, "{clipped}");
        let _ = execute!(out, Clear(ClearType::UntilNewLine));
    }
    let _ = write!(out, "{SYNC_END}");
    let _ = out.flush();
}

/// Paint the activity line on its own row above the box.
pub fn status(height: u16, text: &str) {
    let mut out = std::io::stdout();
    let _ = write!(out, "{SYNC_BEGIN}");
    let _ = execute!(out, MoveTo(0, status_row(height)));
    let _ = write!(out, "{text}");
    let _ = execute!(out, Clear(ClearType::UntilNewLine));
    let _ = write!(out, "{SYNC_END}");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every test below writes the same process-global.** [`BOX_ROWS`] is one
    /// `AtomicU16` for the whole program, and cargo runs tests in parallel
    /// THREADS of a single process — so two of these racing meant one stored 3,
    /// another stored 9 in between, and the first asserted on the second's
    /// value. It passed for five consecutive runs and failed on the sixth,
    /// which is the worst way for a test to be wrong: it discredits every green
    /// suite around it without ever pointing at itself.
    ///
    /// Serialising them is the fix rather than making the state per-test: the
    /// global is what production actually uses, and a test that swapped it for
    /// a local would stop testing the thing that ships.
    static SCREEN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take BOTH locks, surviving a panic in an earlier test.
    ///
    /// A poisoned mutex would otherwise turn one genuine failure into four.
    ///
    /// # Why the console lock too
    ///
    /// Every measurement here reads the global console — `conversation_height`
    /// counts `wrapped()` rows, and `wrapped()` reads the buffer. Holding only
    /// the screen lock left `cli::console`'s own tests free to emit a line
    /// mid-measurement, and they did: this test module failed intermittently
    /// seeing three rows where one line had been written. Two locks over one
    /// piece of state is not exclusion.
    ///
    /// SCREEN first, console second, always. The console tests take only the
    /// inner one, so the order cannot cycle.
    fn screen() -> (
        std::sync::MutexGuard<'static, ()>,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let outer = SCREEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inner = crate::cli::console::test_lock();
        (outer, inner)
    }

    #[test]
    fn the_box_and_the_conversation_divide_the_screen_exactly() {
        let _guard = screen();
        // Overlap paints the box over the conversation; a gap leaves a dead
        // row that nothing ever clears.
        set_box_rows(3);
        let height = 30;
        assert_eq!(first_box_row(height), 27);
        assert_eq!(status_row(height), 26);
        assert_eq!(conversation_height(height), 26);
        assert_eq!(
            conversation_height(height) + usize::from(STATUS_ROWS) + usize::from(box_rows()),
            usize::from(height),
            "every row is accounted for exactly once"
        );
    }

    #[test]
    fn a_taller_box_takes_rows_from_the_conversation() {
        let _guard = screen();
        // The command menu grows the box; the conversation must give up rows
        // rather than the two overlapping.
        set_box_rows(9);
        assert_eq!(conversation_height(30), 20);
        set_box_rows(3);
        assert_eq!(conversation_height(30), 26);
    }

    #[test]
    fn no_row_above_the_box_is_left_unowned() {
        let _guard = screen();
        // The gap between the conversation and the box is the activity row.
        // Owned by nobody it keeps whatever was last painted there, which is
        // how a shrinking command menu left an entry stranded on screen.
        for menu in [0u16, 1, 6] {
            set_box_rows(3 + menu);
            let height = 30u16;
            let painted = conversation_height(height);
            let cleared = usize::from(first_box_row(height)) - painted;
            assert_eq!(
                painted + cleared,
                usize::from(first_box_row(height)),
                "rows above the box must all be accounted for (menu {menu})"
            );
            assert_eq!(cleared, usize::from(STATUS_ROWS), "menu {menu}");
        }
        set_box_rows(3);
    }

    #[test]
    fn a_box_taller_than_the_screen_leaves_no_negative_room() {
        let _guard = screen();
        set_box_rows(200);
        assert_eq!(conversation_height(30), 0);
        assert_eq!(first_box_row(30), 0);
        set_box_rows(3);
    }

    #[test]
    fn wrapped_clamps_width_to_minus_one_to_prevent_terminal_autowrap() {
        let _guard = screen();
        crate::cli::console::reset();
        crate::cli::console::emit_line("1234567890");
        // For width 10, usable width is 9. A 10-char line must wrap into 2 rows,
        // so it never touches column 10 (the rightmost edge).
        let rows = wrapped(10);
        assert_eq!(rows.len(), 2, "the console held: {rows:?}");
        assert_eq!(rows[0], "123456789");
        assert_eq!(rows[1], "0");
        crate::cli::console::reset();
    }
}
