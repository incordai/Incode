//! A small fingerprint of a picture of the screen, for asking "did it change?"
//! without keeping or sending the picture (`wait_for` `screen_changes`).
//!
//! # Why two parts
//!
//! A difference hash (does each cell get brighter or darker than its
//! neighbour) sees edges, which is what a dialog opening or a page loading
//! changes. It is blind to a change of brightness alone: a white screen and a
//! black one have no edges, so they hash the same. So the fingerprint is 128
//! bits: that gradient hash (64) beside the average brightness of a 4x4 grid,
//! four bits a cell (64). The screens are compared by how many bits differ
//! ([`distance`]), so a blinking cursor or a clock tick that moves a bit or two
//! is below a tolerance, and a window opening is far above it.
//!
//! Pure over pixels, so tested on every platform; the capture that feeds it is
//! `mcp::wait_tool`.

use image::imageops::{grayscale, resize, FilterType};
use image::RgbaImage;

/// 128 bits: gradient hash in the high half, brightness grid in the low half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint(pub u128);

/// The fingerprint of `picture`.
#[must_use]
pub fn of(picture: &RgbaImage) -> Fingerprint {
    let grey = grayscale(picture);

    // Gradient: 9x8 cells, one bit for each of the 8x8 left-brighter-than-right.
    let cells = resize(&grey, 9, 8, FilterType::Triangle);
    let mut gradient = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            gradient <<= 1;
            if cells.get_pixel(x, y).0[0] > cells.get_pixel(x + 1, y).0[0] {
                gradient |= 1;
            }
        }
    }

    // Brightness: 4x4 cells, the top four bits of each average.
    let grid = resize(&grey, 4, 4, FilterType::Triangle);
    let mut levels = 0u64;
    for y in 0..4 {
        for x in 0..4 {
            levels = (levels << 4) | u64::from(grid.get_pixel(x, y).0[0] >> 4);
        }
    }

    Fingerprint((u128::from(gradient) << 64) | u128::from(levels))
}

/// How many of the 128 bits differ.
#[must_use]
pub fn distance(a: Fingerprint, b: Fingerprint) -> u32 {
    (a.0 ^ b.0).count_ones()
}

impl Fingerprint {
    /// 32 hex digits, for a reply the next call can pass back as a baseline.
    #[must_use]
    pub fn hex(self) -> String {
        format!("{:032x}", self.0)
    }

    /// [`Self::hex`]'s inverse; `None` for anything that is not 32 hex digits.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Fingerprint> {
        let text = text.trim();
        (text.len() == 32 && text.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| u128::from_str_radix(text, 16).ok().map(Fingerprint))
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn flat(width: u32, height: u32, grey: u8) -> RgbaImage {
        RgbaImage::from_pixel(width, height, Rgba([grey, grey, grey, 255]))
    }

    /// A page: light, with a dark bar across the top and a dark box inside.
    fn page() -> RgbaImage {
        let mut img = flat(320, 200, 235);
        for y in 0..20 {
            for x in 0..320 {
                img.put_pixel(x, y, Rgba([40, 40, 40, 255]));
            }
        }
        for y in 80..140 {
            for x in 100..220 {
                img.put_pixel(x, y, Rgba([90, 120, 200, 255]));
            }
        }
        img
    }

    #[test]
    fn the_same_picture_is_the_same_fingerprint() {
        assert_eq!(distance(of(&page()), of(&page())), 0);
        assert_eq!(of(&page()), of(&page()));
    }

    #[test]
    fn white_and_black_differ_though_neither_has_an_edge() {
        let d = distance(of(&flat(160, 90, 255)), of(&flat(160, 90, 0)));
        assert!(
            d >= 16,
            "a whole-screen brightness change is far apart: {d}"
        );
    }

    #[test]
    fn a_dialog_opening_is_far_above_a_tolerance_of_a_few_bits() {
        let before = page();
        let mut after = page();
        // A dark dialog over the middle third.
        for y in 50..150 {
            for x in 60..260 {
                after.put_pixel(x, y, Rgba([20, 20, 30, 255]));
            }
        }
        let d = distance(of(&before), of(&after));
        assert!(d > 8, "{d}");
    }

    #[test]
    fn a_blinking_cursor_is_below_the_default_tolerance() {
        let before = page();
        let mut after = page();
        for y in 100..112 {
            for x in 150..152 {
                after.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
        let d = distance(of(&before), of(&after));
        assert!(d <= 4, "a 2x12 pixel mark moved {d} bits");
    }

    #[test]
    fn the_size_of_the_picture_does_not_matter() {
        let small = of(&image::imageops::resize(
            &page(),
            160,
            100,
            FilterType::Triangle,
        ));
        let large = of(&page());
        assert!(distance(small, large) <= 4);
    }

    #[test]
    fn hex_round_trips_and_refuses_anything_else() {
        let f = of(&page());
        assert_eq!(f.hex().len(), 32);
        assert_eq!(Fingerprint::from_hex(&f.hex()), Some(f));
        assert_eq!(Fingerprint::from_hex(&format!(" {} ", f.hex())), Some(f));
        assert_eq!(Fingerprint::from_hex("abc"), None);
        assert_eq!(Fingerprint::from_hex(&"z".repeat(32)), None);
        assert_eq!(Fingerprint(0).hex(), "0".repeat(32));
    }
}
