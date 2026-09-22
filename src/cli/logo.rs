//! The mark, drawn into the terminal.
//!
//! # Why it is computed rather than drawn
//!
//! The mark is a pinwheel: eight curved blades sweeping out from the centre.
//! Hand-drawn ASCII of that is a guess at one orientation and cannot turn. So
//! the shape is described the way it actually is — a logarithmic spiral with
//! eight-fold symmetry — and sampled into Braille, whose cells carry 2x4
//! sub-pixels each. Eight times the vertical resolution of block characters,
//! from a font every terminal already has.
//!
//! Rotation is then just a phase added to the angle, which is why the splash
//! can spin and the same code can render a still frame.

use std::io::Write;

/// Sub-pixel columns across the mark. Two per Braille cell.
const GRID_W: usize = 34;
/// Sub-pixel rows down the mark. Four per Braille cell.
const GRID_H: usize = 32;
/// Blades. Eight, as the mark has.
const BLADES: f64 = 8.0;
/// How hard the blades curve. Higher sweeps them further round.
const SWEEP: f64 = 2.6;
/// Inside this radius is the open centre.
const INNER: f64 = 0.34;
/// Outside this radius is past the rim.
const OUTER: f64 = 1.0;
/// Base blade width. HIGHER IS FATTER: it lowers the threshold the spiral
/// has to clear, so more of each turn is inked.
const THICKNESS: f64 = 0.30;

/// Frames in one full turn. Eight blades means an eighth-turn repeats.
pub const FRAMES: usize = 12;

/// The mark at one rotation, as lines of Braille.
///
/// `frame` selects the rotation; it wraps, so any number is valid.
#[must_use]
pub fn render(frame: usize) -> Vec<String> {
    // An eighth of a turn returns the same picture, so a whole turn of the
    // ANIMATION only needs to cover that eighth.
    #[allow(clippy::cast_precision_loss)]
    let phase = (frame % FRAMES) as f64 / FRAMES as f64 * std::f64::consts::TAU / BLADES;

    let mut lines = Vec::new();
    for cell_row in 0..GRID_H / 4 {
        let mut line = String::new();
        for cell_col in 0..GRID_W / 2 {
            let mut bits: u32 = 0;
            for (dot, (dx, dy)) in DOTS.iter().enumerate() {
                let x = cell_col * 2 + dx;
                let y = cell_row * 4 + dy;
                if inked(x, y, phase) {
                    bits |= DOT_BITS[dot];
                }
            }
            // 0x2800 is Braille blank; the low eight bits are the dots.
            line.push(char::from_u32(0x2800 + bits).unwrap_or(' '));
        }
        lines.push(line.trim_end().to_string());
    }
    lines
}

/// Sub-pixel offsets within a Braille cell, in the order [`DOT_BITS`] expects.
const DOTS: [(usize, usize); 8] = [
    (0, 0),
    (0, 1),
    (0, 2),
    (1, 0),
    (1, 1),
    (1, 2),
    (0, 3),
    (1, 3),
];

/// The bit each of [`DOTS`] sets. Braille numbers its dots in this order, not
/// in reading order, which is the one thing about it that catches everybody.
const DOT_BITS: [u32; 8] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80];

/// Whether one sub-pixel falls on a blade.
fn inked(x: usize, y: usize, phase: f64) -> bool {
    // To a square space centred on zero, so the mark is round rather than oval
    // in a terminal whose cells are taller than they are wide.
    #[allow(clippy::cast_precision_loss)]
    let nx = (x as f64 / (GRID_W - 1) as f64) * 2.0 - 1.0;
    #[allow(clippy::cast_precision_loss)]
    let ny = (y as f64 / (GRID_H - 1) as f64) * 2.0 - 1.0;

    let r = nx.hypot(ny);
    if !(INNER..=OUTER).contains(&r) {
        return false;
    }
    let theta = ny.atan2(nx);
    // The blade edges follow a logarithmic spiral: adding ln(r) to the angle is
    // what makes them curve instead of radiating straight out like a star.
    let along = BLADES.mul_add(theta, SWEEP * r.ln()) + phase * BLADES;
    // Blades WIDEN outward, as the mark's do: narrow where they leave the
    // centre, broad at the rim. Tapering the other way — which is what this
    // did first — makes a fat hub with wisps coming off it.
    let taper = ((r - INNER) / (OUTER - INNER)).clamp(0.0, 1.0);
    along.sin() > 1.0 - THICKNESS - taper * 0.5
}

/// How wide the mark is, in terminal columns.
#[must_use]
pub fn width() -> usize {
    GRID_W / 2
}

/// Draw the mark once beside the given lines of text.
///
/// The mark is on the left and the facts on the right, so the eye lands on the
/// picture and reads the session details without the two competing for rows.
#[must_use]
pub fn beside(frame: usize, info: &[String]) -> Vec<String> {
    let art = render(frame);
    let rows = art.len().max(info.len());
    let gutter = 3;
    let mark_width = width();
    let mut out = Vec::with_capacity(rows);
    for row in 0..rows {
        let left = art.get(row).map_or("", String::as_str);
        let pad = mark_width + gutter - left.chars().count();
        let right = info.get(row).map_or("", String::as_str);
        out.push(
            format!("{left}{}{right}", " ".repeat(pad))
                .trim_end()
                .to_string(),
        );
    }
    out
}

/// Spin the mark in place for a moment, then leave it still.
///
/// Skipped entirely when output is not a terminal, and by `INCORD_NO_SPLASH`.
/// A pipe has no cursor to move back over, and a transcript full of half-drawn
/// pinwheels is worse than no pinwheel at all.
pub fn play(info: &[String]) {
    let lines = beside(0, info);
    if !std::io::IsTerminal::is_terminal(&std::io::stdout())
        || std::env::var_os("INCORD_NO_SPLASH").is_some()
    {
        for line in &lines {
            println!("{line}");
        }
        return;
    }

    let mut out = std::io::stdout();
    let height = u16::try_from(lines.len()).unwrap_or(0);
    for frame in 0..FRAMES {
        for line in &beside(frame, info) {
            let _ = writeln!(out, "{line}");
        }
        let _ = out.flush();
        // Bounded and short: this is a flourish, not a loading screen. Two
        // thirds of a second in total, and the last frame is what stays.
        std::thread::sleep(std::time::Duration::from_millis(55));
        if frame + 1 < FRAMES {
            let _ = crossterm::execute!(out, crossterm::cursor::MoveUp(height));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_has_ink_in_it() {
        // A threshold that inked nothing would render a silent blank block.
        let art = render(0);
        let inked: usize = art
            .iter()
            .map(|l| l.chars().filter(|c| *c != '\u{2800}' && *c != ' ').count())
            .sum();
        assert!(inked > 40, "only {inked} cells inked:\n{}", art.join("\n"));
    }

    #[test]
    fn the_centre_is_open() {
        // The mark is a pinwheel, not a disc: the middle must be empty or the
        // blades read as one blob.
        let art = render(0);
        let mid_row = &art[art.len() / 2];
        let mid = mid_row.chars().nth(width() / 2).unwrap_or(' ');
        assert!(
            mid == '\u{2800}' || mid == ' ',
            "the centre is filled: {mid:?}"
        );
    }

    #[test]
    fn every_frame_is_the_same_size() {
        // A frame of a different height would make the animation walk down the
        // screen instead of turning in place.
        let rows = render(0).len();
        for frame in 1..FRAMES {
            assert_eq!(render(frame).len(), rows, "frame {frame} changed height");
        }
    }

    #[test]
    fn the_mark_actually_turns() {
        // The control: if the phase did nothing, every frame would be identical
        // and the "animation" would be a still picture played twelve times.
        let first = render(0);
        let moved = (1..FRAMES).any(|f| render(f) != first);
        assert!(moved, "no frame differs from the first");
    }

    #[test]
    fn a_full_turn_comes_back_to_the_start() {
        assert_eq!(render(0), render(FRAMES));
    }

    #[test]
    fn the_info_sits_clear_of_the_mark() {
        let info = vec!["Incord Code".to_string(), "opus".to_string()];
        let out = beside(0, &info);
        assert!(out.iter().any(|l| l.ends_with("Incord Code")), "{out:?}");
        // Never overlapping: the text must start beyond the mark's own columns.
        let line = out.iter().find(|l| l.contains("Incord Code")).expect("row");
        let at = line.find("Incord Code").expect("found");
        assert!(
            at >= width(),
            "text at {at} overlaps a mark {} wide",
            width()
        );
    }

    #[test]
    fn more_text_than_mark_still_all_shows() {
        let info: Vec<String> = (0..20).map(|n| format!("line {n}")).collect();
        let out = beside(0, &info);
        assert_eq!(out.len(), 20, "rows were dropped");
        assert!(out.last().expect("last").contains("line 19"));
    }
}
