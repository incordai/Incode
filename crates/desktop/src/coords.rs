//! Mapping between what the model sees and where the mouse actually goes.
//!
//! # The bug this module exists to prevent
//!
//! A vision model downsamples images above roughly 1100-1500px on the long
//! edge. Send it a 3840px-wide screenshot and it reasons about a resized copy,
//! then returns coordinates **in that resized space**. Sending the image
//! full-size does not avoid this — it moves the resize somewhere the server
//! cannot see and therefore cannot invert.
//!
//! So the server resizes deliberately and keeps the factor. Every coordinate
//! arriving from the model is model-space and must be converted before it
//! reaches the OS.
//!
//! That conversion fails **silently**: the click succeeds, it just lands on the
//! wrong control, which is indistinguishable from the model having misread the
//! screen. It gets blamed on the model for a day before anyone checks the
//! arithmetic. Hence a module of its own, pure, with no I/O, tested without a
//! display.
//!
//! # Two different things are called "scale" — do not mix them
//!
//! | | Meaning | Lives on |
//! |---|---|---|
//! | **DPI scale** | physical pixels per *logical* pixel (1.5 at 150% Windows scaling) | [`super::MonitorInfo::scale`] |
//! | **View scale** | physical pixels per *model-image* pixel (2.0 when a 3840px display is sent as 1920px) | [`ViewTransform::scale`] |
//!
//! They are independent: a 100%-scaling display still needs downscaling, and a
//! 150% display might not. Multiplying by the wrong one is off by exactly the
//! amount that looks plausible. [`PhysicalPoint`] and [`ModelPoint`] are
//! separate types specifically so that mixing the two spaces is a compile
//! error rather than an offset.

use serde::{Deserialize, Serialize};

use super::Rect;

/// A point on the real desktop, in physical pixels, top-left origin.
///
/// Can be negative: a display left of the primary one has negative x.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalPoint {
    pub x: i32,
    pub y: i32,
}

/// A point inside the image the model was actually shown.
///
/// Always non-negative and always smaller than the image — it indexes a
/// picture, not a desktop. A distinct type from [`PhysicalPoint`] so the two
/// cannot be swapped by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPoint {
    pub x: u32,
    pub y: u32,
}

/// How an image shown to the model maps back onto the desktop.
///
/// The scale is DERIVED from the two sizes rather than stored beside them.
/// Storing it would let it drift out of step with the dimensions it describes,
/// and a stale scale is exactly the silent failure above.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ViewTransform {
    /// Physical position of the captured region's top-left corner.
    pub origin: PhysicalPoint,

    /// Physical size of the region that was captured.
    pub source_width: u32,
    pub source_height: u32,

    /// Size of the image the model was shown.
    pub model_width: u32,
    pub model_height: u32,
}

impl ViewTransform {
    /// A transform for a region sent at its native size.
    pub fn identity(origin: PhysicalPoint, width: u32, height: u32) -> Self {
        ViewTransform {
            origin,
            source_width: width,
            source_height: height,
            model_width: width,
            model_height: height,
        }
    }

    /// A transform for `region`, downscaled so its longest edge is at most
    /// `max_edge`.
    ///
    /// Never upscales. Enlarging a small display would cost bandwidth, invent
    /// no detail, and make every mapped coordinate land between real pixels.
    ///
    /// Returns `None` for an empty region — a zero dimension would make the
    /// scale a division by zero, and there is no sensible transform for a
    /// display with no pixels.
    pub fn fit(region: Rect, max_edge: u32) -> Option<Self> {
        if region.width <= 0 || region.height <= 0 || max_edge == 0 {
            return None;
        }

        let source_width = region.width as u32;
        let source_height = region.height as u32;
        let longest = source_width.max(source_height);

        let (model_width, model_height) = if longest <= max_edge {
            (source_width, source_height)
        } else {
            // Scale by the LONGEST edge so the whole image fits, and compute
            // the other edge from the same ratio so the aspect ratio is
            // preserved. Scaling each edge independently would stretch the
            // picture, and the model would report coordinates for a shape the
            // desktop does not have.
            let ratio = f64::from(max_edge) / f64::from(longest);
            (
                scaled_edge(source_width, ratio),
                scaled_edge(source_height, ratio),
            )
        };

        Some(ViewTransform {
            origin: PhysicalPoint {
                x: region.x,
                y: region.y,
            },
            source_width,
            source_height,
            model_width,
            model_height,
        })
    }

    /// Physical pixels per model pixel, on the horizontal axis.
    pub fn scale_x(&self) -> f64 {
        f64::from(self.source_width) / f64::from(self.model_width.max(1))
    }

    /// Physical pixels per model pixel, on the vertical axis.
    ///
    /// Separate from [`Self::scale_x`] because integer rounding in [`Self::fit`]
    /// makes the two differ very slightly on most real sizes. Using one for
    /// both axes puts a small, size-dependent error into every y coordinate.
    pub fn scale_y(&self) -> f64 {
        f64::from(self.source_height) / f64::from(self.model_height.max(1))
    }

    /// Where on the desktop a model-space point actually is.
    ///
    /// Maps to the CENTRE of the source pixel block that the model pixel
    /// represents, not its corner. At a scale of 4 the corner would sit on the
    /// block's top-left edge and consistently miss small targets toward the
    /// bottom-right; the centre is the best single guess for "the model meant
    /// here".
    ///
    /// The result is clamped into the captured region, so a model that names a
    /// coordinate one past the edge gets the edge rather than a click on a
    /// neighbouring monitor.
    pub fn to_physical(&self, p: ModelPoint) -> PhysicalPoint {
        let offset_x = (f64::from(p.x) + 0.5) * self.scale_x() - 0.5;
        let offset_y = (f64::from(p.y) + 0.5) * self.scale_y() - 0.5;

        let max_x = self.source_width.saturating_sub(1) as i32;
        let max_y = self.source_height.saturating_sub(1) as i32;

        PhysicalPoint {
            x: self.origin.x + (offset_x.round() as i32).clamp(0, max_x),
            y: self.origin.y + (offset_y.round() as i32).clamp(0, max_y),
        }
    }

    /// Where a desktop point appears in the model's image.
    ///
    /// `None` when the point is outside the captured region — the caller is
    /// asking about something the model was never shown, and answering with a
    /// clamped edge coordinate would be a confident wrong answer.
    pub fn to_model(&self, p: PhysicalPoint) -> Option<ModelPoint> {
        let offset_x = p.x - self.origin.x;
        let offset_y = p.y - self.origin.y;

        if offset_x < 0
            || offset_y < 0
            || offset_x >= self.source_width as i32
            || offset_y >= self.source_height as i32
        {
            return None;
        }

        let mx = ((f64::from(offset_x) + 0.5) / self.scale_x() - 0.5).round();
        let my = ((f64::from(offset_y) + 0.5) / self.scale_y() - 0.5).round();

        Some(ModelPoint {
            x: (mx.max(0.0) as u32).min(self.model_width.saturating_sub(1)),
            y: (my.max(0.0) as u32).min(self.model_height.saturating_sub(1)),
        })
    }

    /// Whether the region and the image are the same size. Not "sent at
    /// native size" on a Retina display, where the region is in points and
    /// the image in pixels — capture compares the image itself instead.
    #[cfg(test)]
    pub fn is_identity(&self) -> bool {
        self.source_width == self.model_width && self.source_height == self.model_height
    }
}

/// One edge of a proportional resize, never rounded down to nothing.
///
/// A 3840x64 banner scaled to fit 1092 gives a height of 18.2; truncating the
/// far edge of an extreme aspect ratio can reach zero, and a zero-width image
/// makes the scale a division by zero rather than a visible error.
fn scaled_edge(edge: u32, ratio: f64) -> u32 {
    ((f64::from(edge) * ratio).round() as u32).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    fn origin(x: i32, y: i32) -> PhysicalPoint {
        PhysicalPoint { x, y }
    }

    fn model(x: u32, y: u32) -> ModelPoint {
        ModelPoint { x, y }
    }

    #[test]
    fn identity_maps_a_point_to_itself_plus_the_origin() {
        let t = ViewTransform::identity(origin(0, 0), 1920, 1080);
        assert!(t.is_identity());
        assert_eq!(t.to_physical(model(100, 200)), origin(100, 200));
        assert_eq!(t.scale_x(), 1.0);
        assert_eq!(t.scale_y(), 1.0);
    }

    #[test]
    fn a_monitor_origin_offsets_every_mapped_point() {
        let t = ViewTransform::identity(origin(1920, 0), 1920, 1080);
        assert_eq!(t.to_physical(model(0, 0)), origin(1920, 0));
        assert_eq!(t.to_physical(model(10, 10)), origin(1930, 10));
    }

    /// The arrangement that breaks naive code: a display to the LEFT of the
    /// primary has a negative origin.
    #[test]
    fn a_negative_origin_maps_into_negative_desktop_space() {
        let t = ViewTransform::identity(origin(-1920, 0), 1920, 1080);
        assert_eq!(t.to_physical(model(0, 0)), origin(-1920, 0));
        assert_eq!(t.to_physical(model(1919, 0)), origin(-1, 0));
        assert_eq!(t.to_model(origin(-1920, 0)), Some(model(0, 0)));
    }

    #[test]
    fn fit_downscales_by_the_longest_edge_and_keeps_the_aspect_ratio() {
        let t = ViewTransform::fit(rect(0, 0, 3840, 2160), 1092).expect("non-empty");
        assert_eq!(t.model_width, 1092);
        // 2160 * (1092/3840) = 614.25 -> 614
        assert_eq!(t.model_height, 614);
        assert!(!t.is_identity());

        let source_ratio = 3840.0 / 2160.0;
        let model_ratio = f64::from(t.model_width) / f64::from(t.model_height);
        assert!(
            (source_ratio - model_ratio).abs() < 0.01,
            "aspect ratio drifted: {source_ratio} vs {model_ratio}"
        );
    }

    #[test]
    fn fit_uses_the_longest_edge_even_when_it_is_the_height() {
        let t = ViewTransform::fit(rect(0, 0, 1080, 1920), 1092).expect("non-empty");
        assert_eq!(t.model_height, 1092);
        assert_eq!(t.model_width, 614);
    }

    #[test]
    fn fit_never_upscales_a_small_display() {
        let t = ViewTransform::fit(rect(0, 0, 800, 600), 1092).expect("non-empty");
        assert!(t.is_identity());
        assert_eq!((t.model_width, t.model_height), (800, 600));
    }

    #[test]
    fn fit_refuses_an_empty_region_rather_than_dividing_by_zero() {
        assert!(ViewTransform::fit(rect(0, 0, 0, 1080), 1092).is_none());
        assert!(ViewTransform::fit(rect(0, 0, 1920, 0), 1092).is_none());
        assert!(ViewTransform::fit(rect(0, 0, -5, 1080), 1092).is_none());
        assert!(ViewTransform::fit(rect(0, 0, 1920, 1080), 0).is_none());
    }

    #[test]
    fn an_extreme_aspect_ratio_never_collapses_an_edge_to_zero() {
        // 64 * (1092/3840) = 18.2, fine. 4 * (1092/3840) = 1.1 -> 1, and
        // anything thinner must still be at least one pixel.
        let t = ViewTransform::fit(rect(0, 0, 3840, 2), 1092).expect("non-empty");
        assert!(t.model_height >= 1, "collapsed to {}", t.model_height);
        assert!(t.scale_y().is_finite());
    }

    #[test]
    fn a_half_size_image_doubles_every_coordinate() {
        let t = ViewTransform::identity(origin(0, 0), 1920, 1080);
        let half = ViewTransform {
            model_width: 960,
            model_height: 540,
            ..t
        };
        assert_eq!(half.scale_x(), 2.0);
        assert_eq!(half.scale_y(), 2.0);

        // At scale 2 each model pixel covers a PAIR of physical pixels, so the
        // block centre falls between them: model m covers 2m..=2m+1, centre
        // 2m+0.5, which rounds to 2m+1. There is no exact centre pixel of an
        // even-sized block, and landing on the second one consistently is what
        // matters — a mapping that sometimes picked the first would drift.
        assert_eq!(half.to_physical(model(0, 0)), origin(1, 1));
        assert_eq!(half.to_physical(model(100, 100)), origin(201, 201));

        // The far edge still lands exactly on the last physical pixel rather
        // than one past it: (959.5 * 2) - 0.5 = 1918.5 -> 1919.
        assert_eq!(half.to_physical(model(959, 539)), origin(1919, 1079));
    }

    #[test]
    fn the_far_edge_of_a_downscaled_image_stays_inside_the_display() {
        let t = ViewTransform::fit(rect(0, 0, 3840, 2160), 1092).expect("non-empty");
        let corner = t.to_physical(model(t.model_width - 1, t.model_height - 1));
        assert!(corner.x < 3840, "x escaped the display: {corner:?}");
        assert!(corner.y < 2160, "y escaped the display: {corner:?}");
    }

    /// A model that names a coordinate one past the edge must get the edge, not
    /// a click on whatever is next to this monitor.
    #[test]
    fn an_out_of_range_model_point_is_clamped_into_the_display() {
        let t = ViewTransform::identity(origin(0, 0), 1920, 1080);
        assert_eq!(t.to_physical(model(5000, 5000)), origin(1919, 1079));
    }

    #[test]
    fn to_model_refuses_a_point_outside_the_captured_region() {
        let t = ViewTransform::identity(origin(0, 0), 1920, 1080);
        assert_eq!(t.to_model(origin(-1, 0)), None);
        assert_eq!(t.to_model(origin(0, -1)), None);
        assert_eq!(t.to_model(origin(1920, 0)), None);
        assert_eq!(t.to_model(origin(0, 1080)), None);
        assert!(t.to_model(origin(1919, 1079)).is_some());
    }

    /// The property that actually matters: a coordinate that survives a round
    /// trip lands within one pixel of where it started.
    #[test]
    fn round_trip_stays_within_one_pixel_at_every_scale() {
        let cases = [
            ViewTransform::fit(rect(0, 0, 3840, 2160), 1092).expect("4k"),
            ViewTransform::fit(rect(-1920, 0, 1920, 1080), 1092).expect("left monitor"),
            ViewTransform::fit(rect(0, 0, 2560, 1440), 800).expect("aggressive"),
            ViewTransform::fit(rect(100, -200, 1280, 1024), 1092).expect("native"),
        ];

        for t in cases {
            for my in (0..t.model_height).step_by(37) {
                for mx in (0..t.model_width).step_by(41) {
                    let start = model(mx, my);
                    let physical = t.to_physical(start);
                    let back = t
                        .to_model(physical)
                        .expect("a point mapped from inside must map back");

                    let dx = back.x.abs_diff(start.x);
                    let dy = back.y.abs_diff(start.y);
                    assert!(
                        dx <= 1 && dy <= 1,
                        "round trip drifted by ({dx},{dy}) at {start:?} with {t:?}"
                    );
                }
            }
        }
    }

    /// Every mapped point must be inside the display, at every scale — a click
    /// that escapes the monitor is the worst outcome available here.
    #[test]
    fn no_model_point_ever_maps_outside_its_display() {
        let region = rect(-1920, -200, 1920, 1080);
        let t = ViewTransform::fit(region, 700).expect("non-empty");

        for my in 0..t.model_height {
            for mx in 0..t.model_width {
                let p = t.to_physical(model(mx, my));
                assert!(
                    p.x >= region.x && p.x < region.x + region.width,
                    "x escaped at ({mx},{my}): {p:?}"
                );
                assert!(
                    p.y >= region.y && p.y < region.y + region.height,
                    "y escaped at ({mx},{my}): {p:?}"
                );
            }
        }
    }

    /// The confusion this module is named after: DPI scale and view scale are
    /// independent, and using one for the other is off by a plausible amount.
    #[test]
    fn view_scale_is_independent_of_dpi_scale() {
        // A 150%-scaling display that needs NO downscale: DPI scale 1.5, view
        // scale 1.0. Multiplying by the DPI scale here would move every click
        // half a screen.
        let small_hidpi = ViewTransform::fit(rect(0, 0, 1024, 768), 1092).expect("non-empty");
        assert_eq!(small_hidpi.scale_x(), 1.0);
        assert_eq!(super::super::scale_from_dpi(144), 1.5);

        // A 100%-scaling display that DOES need downscaling: DPI scale 1.0,
        // view scale 2.0. The mirror image of the same mistake.
        let big_lodpi = ViewTransform::fit(rect(0, 0, 2184, 1000), 1092).expect("non-empty");
        assert_eq!(big_lodpi.scale_x(), 2.0);
        assert_eq!(super::super::scale_from_dpi(96), 1.0);
    }

    #[test]
    fn a_transform_survives_serialisation() {
        // It crosses the MCP boundary as JSON, so a field that does not
        // round-trip would silently lose the mapping.
        let t = ViewTransform::fit(rect(-1920, -200, 3840, 2160), 1092).expect("non-empty");
        let json = serde_json::to_string(&t).expect("serialise");
        let back: ViewTransform = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(t, back);
        assert_eq!(t.to_physical(model(5, 7)), back.to_physical(model(5, 7)));
    }

    /// A hi-DPI 4K display carried through the whole pipeline.
    ///
    /// This machine runs one 1920x1080 panel at 100%, so the hi-DPI path cannot
    /// be exercised against real hardware here. Synthetic is not as good as a
    /// real 150% display — it proves the arithmetic composes, not that Windows
    /// reports what we expect — but it does close the arithmetic half, and it
    /// runs on any machine including CI.
    #[test]
    fn a_4k_150_percent_display_composes_end_to_end() {
        // 3840x2160 physical, reported at 144 DPI. The DPI is recorded for the
        // report; the mapping below must not use it.
        let dpi = 144;
        assert_eq!(super::super::scale_from_dpi(dpi), 1.5);

        let geometry = rect(0, 0, 3840, 2160);
        let t = ViewTransform::fit(geometry, 1092).expect("non-empty");

        // View scale is the SOURCE-to-MODEL ratio, nothing to do with 1.5.
        assert!(
            (t.scale_x() - 3840.0 / f64::from(t.model_width)).abs() < 1e-9,
            "view scale is not the source/model ratio"
        );

        // Every model pixel lands inside the panel, and the extremes land on
        // the extremes.
        assert_eq!(t.to_physical(model(0, 0)), origin(1, 1));
        let corner = t.to_physical(model(t.model_width - 1, t.model_height - 1));
        assert!(
            corner.x < 3840 && corner.y < 2160,
            "corner escaped the panel: {corner:?}"
        );
        assert!(
            corner.x > 3830 && corner.y > 2150,
            "corner should be at the far edge, got {corner:?}"
        );

        // And the round trip survives the larger ratio.
        for (mx, my) in [(0, 0), (500, 300), (t.model_width - 1, t.model_height - 1)] {
            let back = t
                .to_model(t.to_physical(model(mx, my)))
                .expect("a point from inside must map back");
            assert!(
                back.x.abs_diff(mx) <= 1 && back.y.abs_diff(my) <= 1,
                "round trip drifted at ({mx},{my}): got {back:?}"
            );
        }
    }
}
