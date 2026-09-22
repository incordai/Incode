//! Which lines of the conversation are on screen.
//!
//! # Why this is separate from the drawing
//!
//! Once the program owns the screen it must decide, every repaint, exactly
//! which rows to paint. That decision is arithmetic — how many rows a line
//! takes once it wraps, how far back the operator has scrolled, what happens
//! when the terminal is resized under them — and arithmetic is where the
//! off-by-ones live.
//!
//! Kept here it can be tested against a list of strings, with no terminal to
//! attach to and no agent to run. The drawing code then has nothing left to get
//! wrong except putting the rows on the screen.
//!
//! # Wrapped lines count as the rows they occupy
//!
//! A reply three times the width of the terminal takes three rows. Scrolling
//! that counted it as one would jump three rows for one press and skip content
//! the operator was reading.

/// How the conversation is wrapped and windowed for a given screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewport {
    /// Rows from the BOTTOM. Zero means pinned to the newest output.
    ///
    /// Counted from the bottom rather than the top so that new output does not
    /// move what the operator is looking at: a conversation that grows while
    /// they read would otherwise slide under them.
    scroll: usize,
}

impl Default for Viewport {
    fn default() -> Self {
        Self::new()
    }
}

impl Viewport {
    /// A viewport pinned to the newest output.
    #[must_use]
    pub const fn new() -> Self {
        Self { scroll: 0 }
    }

    /// Whether it is following new output rather than held back in history.
    #[must_use]
    pub const fn at_bottom(&self) -> bool {
        self.scroll == 0
    }

    /// How far back it is held.
    #[must_use]
    pub const fn scroll(&self) -> usize {
        self.scroll
    }

    /// Jump back to the newest output.
    pub const fn to_bottom(&mut self) {
        self.scroll = 0;
    }

    /// Move `rows` back into history, stopping at the oldest line held.
    pub fn up(&mut self, rows: usize, total_rows: usize, height: usize) {
        self.scroll = (self.scroll + rows).min(max_scroll(total_rows, height));
    }

    /// Move `rows` towards the newest output, stopping at the bottom.
    pub const fn down(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_sub(rows);
    }

    /// Keep the position valid after the content or the screen changed.
    ///
    /// A terminal made taller shows more at once, so a scroll that was legal a
    /// moment ago can now point past the top. Left unclamped the viewport shows
    /// a band of blank rows above the conversation.
    pub fn clamp(&mut self, total_rows: usize, height: usize) {
        self.scroll = self.scroll.min(max_scroll(total_rows, height));
    }

    /// The rows to paint, oldest first.
    #[must_use]
    pub fn window<'a>(&self, rows: &'a [String], height: usize) -> &'a [String] {
        if height == 0 || rows.is_empty() {
            return &[];
        }
        let end = rows.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(height);
        &rows[start..end]
    }
}

/// The furthest back a viewport may be scrolled.
///
/// Zero when everything fits: there is no history to move into, and allowing a
/// scroll would push the whole conversation off the top for no reason.
#[must_use]
pub fn max_scroll(total_rows: usize, height: usize) -> usize {
    total_rows.saturating_sub(height)
}

/// Break logical lines into the display rows they actually occupy.
///
/// A line exactly `width` wide takes one row, not two — the wrap happens when
/// it EXCEEDS the width. Getting that boundary wrong inserts a blank row after
/// every full-width line, which on a wall of output is most of them.
#[must_use]
pub fn wrap(lines: &[String], width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows = Vec::with_capacity(lines.len());
    for line in lines {
        // Counted in characters, never bytes: a reply full of em dashes would
        // otherwise wrap in the middle of one and print a broken glyph.
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            rows.push(String::new());
            continue;
        }
        for chunk in chars.chunks(width) {
            rows.push(chunk.iter().collect());
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|t| (*t).to_string()).collect()
    }

    #[test]
    fn a_short_conversation_shows_all_of_itself() {
        let rows = lines(&["one", "two", "three"]);
        let view = Viewport::new();
        assert_eq!(view.window(&rows, 10), rows.as_slice());
    }

    #[test]
    fn a_full_screen_shows_the_newest_lines() {
        // Pinned to the bottom means the LAST rows, which is where new output
        // arrives. Showing the first would leave the operator reading history
        // while the agent talked off-screen.
        let rows = lines(&["1", "2", "3", "4", "5"]);
        let view = Viewport::new();
        assert_eq!(view.window(&rows, 3), &rows[2..5]);
    }

    #[test]
    fn scrolling_up_moves_back_through_history() {
        let rows = lines(&["1", "2", "3", "4", "5"]);
        let mut view = Viewport::new();
        view.up(2, rows.len(), 3);
        assert_eq!(view.window(&rows, 3), &rows[0..3]);
        assert!(!view.at_bottom());
    }

    #[test]
    fn scrolling_stops_at_the_oldest_line_held() {
        // Past the top there is nothing to show, and a viewport that kept going
        // would paint a growing band of blank rows above the conversation.
        let rows = lines(&["1", "2", "3", "4", "5"]);
        let mut view = Viewport::new();
        view.up(999, rows.len(), 3);
        assert_eq!(view.scroll(), 2, "5 rows in a 3-row screen hides 2");
        assert_eq!(view.window(&rows, 3), &rows[0..3]);
    }

    #[test]
    fn scrolling_down_returns_to_the_newest_output() {
        let rows = lines(&["1", "2", "3", "4", "5"]);
        let mut view = Viewport::new();
        view.up(2, rows.len(), 3);
        view.down(999);
        assert!(view.at_bottom());
        assert_eq!(view.window(&rows, 3), &rows[2..5]);
    }

    #[test]
    fn nothing_can_be_scrolled_when_everything_fits() {
        assert_eq!(max_scroll(3, 10), 0);
        let rows = lines(&["1", "2", "3"]);
        let mut view = Viewport::new();
        view.up(5, rows.len(), 10);
        assert!(view.at_bottom(), "there was no history to move into");
    }

    #[test]
    fn a_taller_terminal_pulls_the_view_back_to_what_exists() {
        // Resizing bigger shows more at once, so a scroll that was legal
        // becomes one that points past the top.
        let mut view = Viewport::new();
        view.up(8, 20, 10);
        assert_eq!(view.scroll(), 8);
        view.clamp(20, 18);
        assert_eq!(view.scroll(), 2, "only 2 rows are still off-screen");
    }

    #[test]
    fn new_output_does_not_move_what_is_being_read() {
        // Scroll counts from the BOTTOM, so lines arriving below the window
        // leave the visible band alone. Counted from the top, the conversation
        // would slide under the reader as the agent talked.
        let mut rows = lines(&["1", "2", "3", "4", "5"]);
        let mut view = Viewport::new();
        view.up(2, rows.len(), 3);
        let before = view.window(&rows, 3).to_vec();
        rows.push("6".to_string());
        view.up(1, rows.len(), 3); // one more line means one more row of history
        assert_eq!(view.window(&rows, 3), before.as_slice());
    }

    #[test]
    fn a_line_exactly_the_width_takes_one_row() {
        // Wrapping at `>= width` instead of `> width` puts a blank row after
        // every full-width line, which is most of them on a wall of output.
        let wrapped = wrap(&lines(&["12345"]), 5);
        assert_eq!(wrapped, vec!["12345"]);
    }

    #[test]
    fn a_long_line_takes_the_rows_it_occupies() {
        let wrapped = wrap(&lines(&["1234567890123"]), 5);
        assert_eq!(wrapped, vec!["12345", "67890", "123"]);
    }

    #[test]
    fn wrapping_never_splits_a_character() {
        // Em dashes are three bytes. Chunking bytes would cut one in half and
        // print a broken glyph — the same class of bug that has already reached
        // the operator once in this codebase.
        let wrapped = wrap(&lines(&["a—b—c—d"]), 3);
        assert_eq!(wrapped, vec!["a—b", "—c—", "d"]);
        assert!(wrapped.iter().all(|r| r.chars().count() <= 3));
    }

    #[test]
    fn a_blank_line_survives_wrapping() {
        // Spacing is content: dropping empty lines runs the conversation
        // together into one block.
        let wrapped = wrap(&lines(&["a", "", "b"]), 10);
        assert_eq!(wrapped, vec!["a", "", "b"]);
    }

    #[test]
    fn scrolling_counts_wrapped_rows_not_logical_lines() {
        // One reply three times the width is three rows. Counting it as one
        // would jump three rows per press and skip what was being read.
        let long = lines(&["aaaaaaaaa"]); // 9 chars
        let rows = wrap(&long, 3);
        assert_eq!(rows.len(), 3);
        let mut view = Viewport::new();
        view.up(1, rows.len(), 2);
        assert_eq!(view.window(&rows, 2), &rows[0..2]);
    }

    #[test]
    fn a_zero_height_screen_asks_for_nothing() {
        let rows = lines(&["1", "2"]);
        assert!(Viewport::new().window(&rows, 0).is_empty());
        assert!(wrap(&rows, 0).is_empty());
    }
}
