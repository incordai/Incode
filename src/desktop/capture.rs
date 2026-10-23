//! Turning a screen into something a model can reason about.
//!
//! # The two rules this module exists to enforce
//!
//! **1. DPI awareness is established before anything is captured.** Not as
//! tidiness — xcap's own Windows path reads the calling process's awareness and
//! branches on it (`xcap-0.9.8/src/windows/capture.rs:93-97`), so the pixels it
//! returns are a FUNCTION of whether [`super::dpi::ensure_per_monitor_aware`]
//! has run. Capturing first and setting awareness later produces a correct
//! looking image of the wrong size.
//!
//! **2. The [`ViewTransform`] is built from the image that was actually
//! captured**, never from a second query about the display. If the capture
//! comes back 1920 wide and a fresh `monitor.width()` says 2560, the image is
//! the truth — it is what the model will see and name coordinates in. Deriving
//! the transform from anything else is how a click lands on the wrong monitor.
//!
//! Everything here is downstream of [`super::coords`], which owns the actual
//! mapping arithmetic. This module decides *what* to capture and how to encode
//! it; it does not do geometry of its own.
//!
//! # Where the pixels come from
//!
//! On Windows and macOS, the xcap library. On Linux, the X server itself
//! ([`x11`], over the x11rb connection input already uses): xcap cannot be
//! built there without a Wayland and PipeWire backend this crate never uses,
//! and that backend broke the release build on Ubuntu 22.04. Both answer the
//! same accessors (`x`, `width`, `is_primary`, `capture_image`, ...), so
//! everything below reads a display one way on every platform.
//!
//! # Built without capture
//!
//! Screen capture is the `desktop-capture` cargo feature, on by default, on
//! every platform. A build without it answers every capture with a refusal
//! that names the feature, rather than an empty or invented display.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, ImageEncoder, ImageFormat, RgbaImage};
use serde::{Deserialize, Serialize};
#[cfg(all(feature = "desktop-capture", target_os = "linux"))]
use x11::Monitor;
#[cfg(all(feature = "desktop-capture", not(target_os = "linux")))]
use xcap::Monitor;

use super::coords::{PhysicalPoint, ViewTransform};
#[cfg(feature = "desktop-capture")]
use super::dpi;
use super::{DesktopError, Rect};

// Linux's capture backend. Its pure parts are tested on every platform.
#[cfg(any(test, all(target_os = "linux", feature = "desktop-capture")))]
mod x11;

/// How a screenshot is encoded on the way to the model.
///
/// This crate's own enum rather than `image::ImageFormat`, so the dependency
/// does not leak into the MCP schema that agents see. Two variants because two
/// are the whole requirement — see the `image` entry in Cargo.toml.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShotFormat {
    /// Lossless. Correct for UI, where text and edges matter.
    Png,
    /// Lossy, much smaller. For photographic content, where PNG wastes tokens
    /// on noise the model does not need.
    Jpeg,
}

/// What to capture and how.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureOptions {
    /// Longest edge of the image the model is shown.
    pub max_edge: u32,
    pub format: ShotFormat,
    /// 1..=100. Ignored for PNG.
    pub jpeg_quality: u8,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        CaptureOptions {
            // Vision models downsample above roughly this size anyway. Sending
            // more moves the resize somewhere this process cannot see, and
            // therefore cannot invert — which is the whole failure `coords`
            // exists to prevent.
            max_edge: 1092,
            format: ShotFormat::Png,
            jpeg_quality: 80,
        }
    }
}

/// A captured screen, and everything needed to map a coordinate back onto it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Screenshot {
    /// The encoded image. NOT base64 — that happens at the MCP boundary, so
    /// nothing here holds two copies of a multi-megabyte buffer.
    pub bytes: Vec<u8>,
    pub format: ShotFormat,

    /// Describes THIS image. Carrying it alongside the pixels is the point:
    /// without it a consumer cannot turn a model's coordinate back into a
    /// place on the desktop.
    pub transform: ViewTransform,

    pub monitor_id: u32,
    pub monitor_name: String,
}

impl Screenshot {
    /// Size of the image the model is shown.
    pub fn model_size(&self) -> (u32, u32) {
        (self.transform.model_width, self.transform.model_height)
    }

    /// Where a model-space coordinate actually is on the desktop.
    ///
    /// The whole reason the transform travels with the image.
    pub fn to_physical(&self, x: u32, y: u32) -> PhysicalPoint {
        self.transform
            .to_physical(super::coords::ModelPoint { x, y })
    }
}

/// Capture the primary display — or the first, when none says it is primary
/// (a lone Xvfb screen does not).
#[cfg(feature = "desktop-capture")]
pub fn capture_primary(options: CaptureOptions) -> Result<Screenshot, DesktopError> {
    let monitors = all_monitors()?;
    let index = monitors
        .iter()
        .position(|m| m.is_primary().unwrap_or(false))
        .unwrap_or(0);
    let monitor = monitors
        .get(index)
        .ok_or_else(|| DesktopError::Os("no displays are attached to this session".to_string()))?;

    capture(monitor, index, options)
}

/// Capture one display by the id [`super::screen_info`] reported.
#[cfg(feature = "desktop-capture")]
pub fn capture_monitor(
    monitor_id: u32,
    options: CaptureOptions,
) -> Result<Screenshot, DesktopError> {
    let monitors = all_monitors()?;
    let monitor = monitors
        .get(monitor_id as usize)
        .ok_or_else(|| DesktopError::Os(format!("no display with id {monitor_id}")))?;

    capture(monitor, monitor_id as usize, options)
}

/// Capture one region of the desktop — a window's rectangle — from the
/// display its centre is on.
///
/// The region is in desktop units (the space input and the accessibility
/// tree use), clipped to that display. What is IN FRONT of the window is
/// captured with it: this is a crop of the screen, not the window's own
/// buffer, so the caller brings the window forward first when it matters.
#[cfg(feature = "desktop-capture")]
pub fn capture_region(region: Rect, options: CaptureOptions) -> Result<Screenshot, DesktopError> {
    let monitors = all_monitors()?;
    let (cx, cy) = region.center();
    let read = |what: &str, e: &dyn std::fmt::Display| {
        DesktopError::Os(format!("could not read a display's {what}: {e}"))
    };
    let mut chosen = None;
    for (index, monitor) in monitors.iter().enumerate() {
        let (x, y) = (
            monitor.x().map_err(|e| read("x", &e))?,
            monitor.y().map_err(|e| read("y", &e))?,
        );
        let (w, h) = (
            monitor.width().map_err(|e| read("width", &e))? as i32,
            monitor.height().map_err(|e| read("height", &e))? as i32,
        );
        if cx >= x && cx < x + w && cy >= y && cy < y + h {
            chosen = Some((
                index,
                monitor,
                Rect {
                    x,
                    y,
                    width: w,
                    height: h,
                },
            ));
            break;
        }
    }
    let (index, monitor, screen) = chosen.ok_or_else(|| {
        DesktopError::Os(format!(
            "the window at ({},{}) {}x{} is not on any display",
            region.x, region.y, region.width, region.height
        ))
    })?;
    // The id screen_info gives this display: its place in the list. The
    // library's own id (an X11 RandR output, a Windows handle) is a number
    // nothing else in the tool surface uses.
    let id = index as u32;

    // The part of the region that is on this display, in desktop units.
    let left = region.x.max(screen.x);
    let top = region.y.max(screen.y);
    let right = (region.x + region.width).min(screen.x + screen.width);
    let bottom = (region.y + region.height).min(screen.y + screen.height);
    if right <= left || bottom <= top {
        return Err(DesktopError::Os(
            "the window has no visible area".to_string(),
        ));
    }

    #[cfg(target_os = "macos")]
    super::macos::require(super::macos::Grant::ScreenRecording)?;
    let full = monitor
        .capture_image()
        .map_err(|e| DesktopError::Os(format!("could not capture display {id}: {e}")))?;

    // Desktop units to image pixels: 1 on Windows and X11, the Retina factor
    // on macOS.
    let scale = f64::from(full.width()) / f64::from(screen.width.max(1));
    let px = |v: i32| (f64::from(v) * scale).round() as u32;
    let crop_x = px(left - screen.x).min(full.width().saturating_sub(1));
    let crop_y = px(top - screen.y).min(full.height().saturating_sub(1));
    let crop_w = px(right - left).clamp(1, full.width() - crop_x);
    let crop_h = px(bottom - top).clamp(1, full.height() - crop_y);
    let cropped = image::imageops::crop_imm(&full, crop_x, crop_y, crop_w, crop_h).to_image();

    let transform = desktop_view(
        PhysicalPoint { x: left, y: top },
        ((right - left) as u32, (bottom - top) as u32),
        (cropped.width(), cropped.height()),
        options.max_edge,
    )
    .map_err(|why| DesktopError::Os(format!("the window region: {why}")))?;
    let shown =
        if (cropped.width(), cropped.height()) == (transform.model_width, transform.model_height) {
            cropped
        } else {
            image::imageops::resize(
                &cropped,
                transform.model_width,
                transform.model_height,
                image::imageops::FilterType::Triangle,
            )
        };
    Ok(Screenshot {
        bytes: encode(&shown, options.format, options.jpeg_quality)?,
        format: options.format,
        transform,
        monitor_id: id,
        monitor_name: monitor.name().unwrap_or_default(),
    })
}

/// Every display as [`super::MonitorInfo`], from the capture backend — the
/// monitor list on macOS and Linux, where its units are the ones input uses
/// (points on macOS, pixels on X11). `id` is the position in this list, the
/// same index [`capture_monitor`] takes.
///
/// The backend reports no reserved areas (menu bar, dock, panels), so the
/// work area is the whole display there.
#[cfg(feature = "desktop-capture")]
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn monitor_infos() -> Result<Vec<super::MonitorInfo>, DesktopError> {
    let read = |what: &str, i: usize, e: &dyn std::fmt::Display| {
        DesktopError::Os(format!("could not read display {i}'s {what}: {e}"))
    };
    let monitors = all_monitors()?;
    let count = monitors.len();
    monitors
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let rect = Rect {
                x: m.x().map_err(|e| read("x", i, &e))?,
                y: m.y().map_err(|e| read("y", i, &e))?,
                width: m.width().map_err(|e| read("width", i, &e))? as i32,
                height: m.height().map_err(|e| read("height", i, &e))? as i32,
            };
            let scale = f64::from(m.scale_factor().map_err(|e| read("scale", i, &e))?);
            Ok(describe(
                i,
                count,
                m.friendly_name().or_else(|_| m.name()).unwrap_or_default(),
                rect,
                scale,
                m.is_primary().unwrap_or(false),
            ))
        })
        .collect()
}

/// Display `index` of `count`, as [`super::MonitorInfo`]: numbered by its
/// place in the list, its whole rectangle as the work area, and primary by
/// [`is_primary`]. Called by [`monitor_infos`]; pure, so Linux's RandR
/// reading is tested through it on every platform.
#[cfg(any(test, feature = "desktop-capture"))]
#[cfg_attr(all(windows, not(test)), allow(dead_code))]
fn describe(
    index: usize,
    count: usize,
    name: String,
    rect: Rect,
    scale: f64,
    reported_primary: bool,
) -> super::MonitorInfo {
    super::MonitorInfo {
        id: index as u32,
        name,
        rect,
        work_area: rect,
        dpi: (96.0 * scale).round() as u32,
        scale,
        primary: is_primary(reported_primary, count),
    }
}

/// Whether a display is the primary one: what the OS says, or the only
/// display there is. A lone Xvfb screen reports no primary (measured:
/// `"primary": false`), yet it is the one every unqualified screenshot
/// captures. Among several displays none is guessed at. Pure.
#[cfg(any(test, feature = "desktop-capture"))]
fn is_primary(reported: bool, displays: usize) -> bool {
    reported || displays == 1
}

#[cfg(feature = "desktop-capture")]
fn all_monitors() -> Result<Vec<Monitor>, DesktopError> {
    // BEFORE the enumeration, not merely before the capture: xcap reads process
    // awareness while it works, so a late call would leave part of this answer
    // computed under the old mode. (On Linux there is no awareness to set;
    // the call is kept so every platform starts the same way.)
    dpi::ensure_per_monitor_aware();

    Monitor::all().map_err(|e| DesktopError::Os(format!("could not enumerate displays: {e}")))
}

/// `index` is the display's place in the list, which is the id
/// `screen_info` reports for it; the library's own id is not shown.
#[cfg(feature = "desktop-capture")]
fn capture(
    monitor: &Monitor,
    index: usize,
    options: CaptureOptions,
) -> Result<Screenshot, DesktopError> {
    let id = index as u32;
    let name = monitor.name().unwrap_or_else(|_| String::new());
    let origin_x = monitor
        .x()
        .map_err(|e| DesktopError::Os(format!("could not read display {id}'s x: {e}")))?;
    let origin_y = monitor
        .y()
        .map_err(|e| DesktopError::Os(format!("could not read display {id}'s y: {e}")))?;

    let desktop_width = monitor
        .width()
        .map_err(|e| DesktopError::Os(format!("could not read display {id}'s width: {e}")))?;
    let desktop_height = monitor
        .height()
        .map_err(|e| DesktopError::Os(format!("could not read display {id}'s height: {e}")))?;

    #[cfg(target_os = "macos")]
    super::macos::require(super::macos::Grant::ScreenRecording)?;
    let full = monitor
        .capture_image()
        .map_err(|e| DesktopError::Os(format!("could not capture display {id}: {e}")))?;

    let transform = desktop_view(
        PhysicalPoint {
            x: origin_x,
            y: origin_y,
        },
        (desktop_width, desktop_height),
        (full.width(), full.height()),
        options.max_edge,
    )
    .map_err(|why| DesktopError::Os(format!("display {id}: {why}")))?;

    let shown = if (full.width(), full.height()) == (transform.model_width, transform.model_height)
    {
        full
    } else {
        image::imageops::resize(
            &full,
            transform.model_width,
            transform.model_height,
            // Triangle, not Nearest: nearest-neighbour drops thin strokes
            // entirely at these ratios, and the model reads text. Not Lanczos
            // either — it is slower and its ringing adds edges that are not on
            // the screen.
            image::imageops::FilterType::Triangle,
        )
    };

    let bytes = encode(&shown, options.format, options.jpeg_quality)?;

    Ok(Screenshot {
        bytes,
        format: options.format,
        transform,
        monitor_id: id,
        monitor_name: name,
    })
}

/// What every capture answers in a build without the capture library.
#[cfg(not(feature = "desktop-capture"))]
fn not_built() -> DesktopError {
    DesktopError::Os(
        "this incode was built without screen capture (the `desktop-capture` cargo feature), so \
         it has no screenshots and no display list. Rebuild with default features to have them; \
         the accessibility tree and input do not need it."
            .to_string(),
    )
}

#[cfg(not(feature = "desktop-capture"))]
pub fn capture_primary(_options: CaptureOptions) -> Result<Screenshot, DesktopError> {
    Err(not_built())
}

#[cfg(not(feature = "desktop-capture"))]
pub fn capture_region(_region: Rect, _options: CaptureOptions) -> Result<Screenshot, DesktopError> {
    Err(not_built())
}

#[cfg(all(
    not(feature = "desktop-capture"),
    any(target_os = "macos", target_os = "linux")
))]
pub(crate) fn monitor_infos() -> Result<Vec<super::MonitorInfo>, DesktopError> {
    Err(not_built())
}

/// How an image of a display maps back onto the desktop.
///
/// The REGION is the display in desktop units — what the OS's monitor API
/// reports, and the space input and the accessibility tree use. The MODEL
/// size comes from the image actually captured. On Windows and X11 the two
/// are the same numbers; on a macOS Retina display the desktop is in points
/// and the image in pixels at twice that. Taking the region's size from the
/// image, as this once did, put a click at twice its distance from the
/// display's corner.
///
/// Pure, so the arithmetic is tested without a screen.
#[cfg(any(test, feature = "desktop-capture"))]
pub(crate) fn desktop_view(
    origin: PhysicalPoint,
    (desktop_width, desktop_height): (u32, u32),
    (image_width, image_height): (u32, u32),
    max_edge: u32,
) -> Result<ViewTransform, String> {
    if desktop_width == 0 || desktop_height == 0 || image_width == 0 || image_height == 0 {
        return Err(format!(
            "it measured {desktop_width}x{desktop_height} and was captured as \
             {image_width}x{image_height}, which has no usable geometry"
        ));
    }
    // One scale for both axes, or the capture is not of this display: a
    // mismatch here would put every click somewhere the model did not mean.
    let across = f64::from(image_width) / f64::from(desktop_width);
    let down = f64::from(image_height) / f64::from(desktop_height);
    if (across - down).abs() > 0.02 * across.max(down) {
        return Err(format!(
            "it measures {desktop_width}x{desktop_height} but was captured as \
             {image_width}x{image_height}, a different shape, so no coordinate in the \
             image could be mapped back"
        ));
    }
    let image = Rect {
        x: 0,
        y: 0,
        width: image_width as i32,
        height: image_height as i32,
    };
    let fitted = ViewTransform::fit(image, max_edge)
        .ok_or_else(|| "it has no usable geometry".to_string())?;
    Ok(ViewTransform {
        origin,
        source_width: desktop_width,
        source_height: desktop_height,
        model_width: fitted.model_width,
        model_height: fitted.model_height,
    })
}

/// Encode an image. Pure, so it is testable with no screen attached.
pub(crate) fn encode(
    image: &RgbaImage,
    format: ShotFormat,
    jpeg_quality: u8,
) -> Result<Vec<u8>, DesktopError> {
    let mut out = Cursor::new(Vec::new());

    match format {
        ShotFormat::Png => image
            .write_to(&mut out, ImageFormat::Png)
            .map_err(|e| DesktopError::Os(format!("PNG encoding failed: {e}")))?,

        ShotFormat::Jpeg => {
            // JPEG has no alpha channel. Handing it RGBA does not fail loudly
            // on every path, so the conversion is explicit rather than left to
            // whatever the encoder decides to do with a channel it cannot
            // represent.
            let rgb = image::DynamicImage::ImageRgba8(image.clone()).to_rgb8();
            let quality = jpeg_quality.clamp(1, 100);
            JpegEncoder::new_with_quality(&mut out, quality)
                .write_image(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    ExtendedColorType::Rgb8,
                )
                .map_err(|e| DesktopError::Os(format!("JPEG encoding failed: {e}")))?;
        }
    }

    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn swatch(width: u32, height: u32) -> RgbaImage {
        // A gradient rather than a flat fill: a flat image compresses to almost
        // nothing and would hide a resize that silently produced 1x1.
        RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        })
    }

    /// **A RETINA CLICK LANDS WHERE THE MODEL POINTED.** A 1512x982-point
    /// display captured at 3024x1964 pixels: the model's right edge is the
    /// display's right edge in POINTS — not twice as far, where the old
    /// pixel-sized region put it.
    #[test]
    fn a_retina_capture_maps_back_to_points() {
        let origin = PhysicalPoint { x: 0, y: 0 };
        let view = desktop_view(origin, (1512, 982), (3024, 1964), 1092).expect("view");
        assert_eq!((view.model_width, view.model_height), (1092, 709));
        let right = view.to_physical(super::super::coords::ModelPoint { x: 1091, y: 354 });
        assert!(
            (1505..=1511).contains(&right.x),
            "the right edge mapped to x={} on a 1512-point display",
            right.x
        );
        assert!((485..=495).contains(&right.y), "y={}", right.y);
    }

    /// Windows and X11, where desktop units ARE the image's pixels: the result
    /// is exactly what fitting the image alone gave before.
    #[test]
    fn a_pixel_desktop_maps_as_before() {
        let origin = PhysicalPoint { x: -1920, y: 0 };
        let view = desktop_view(origin, (1920, 1080), (1920, 1080), 1092).expect("view");
        let before = ViewTransform::fit(
            Rect {
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
            1092,
        )
        .expect("fit");
        assert_eq!(view, before);
    }

    /// A capture whose shape is not the display's is refused with both sizes,
    /// rather than mapped — every click from it would land somewhere else.
    #[test]
    fn a_capture_of_another_shape_is_refused_by_name() {
        let origin = PhysicalPoint { x: 0, y: 0 };
        let why = desktop_view(origin, (1920, 1080), (1920, 1200), 1092).expect_err("refused");
        assert!(
            why.contains("1920x1080") && why.contains("1920x1200"),
            "{why}"
        );
        assert!(desktop_view(origin, (0, 1080), (1920, 1080), 1092).is_err());
    }

    /// The round trip holds within one desktop unit on a Retina display:
    /// a point read off the image goes back to where it was.
    #[test]
    fn a_retina_point_round_trips() {
        let origin = PhysicalPoint { x: 100, y: 50 };
        let view = desktop_view(origin, (1512, 982), (3024, 1964), 1092).expect("view");
        for (x, y) in [(100, 50), (856, 541), (1611, 1031)] {
            let p = PhysicalPoint { x, y };
            let m = view.to_model(p).expect("on the display");
            let back = view.to_physical(m);
            assert!(
                (back.x - x).abs() <= 1 && (back.y - y).abs() <= 1,
                "{p:?} -> {m:?} -> {back:?}"
            );
        }
    }

    #[test]
    fn png_output_carries_the_png_signature() {
        let bytes = encode(&swatch(16, 8), ShotFormat::Png, 80).expect("encode");
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "not a PNG stream"
        );
    }

    #[test]
    fn jpeg_output_carries_the_jpeg_signature() {
        let bytes = encode(&swatch(16, 8), ShotFormat::Jpeg, 80).expect("encode");
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "not a JPEG stream");
    }

    /// The reason the RGBA -> RGB conversion is explicit: an alpha channel must
    /// not make JPEG encoding fail or emit a four-channel file.
    #[test]
    fn jpeg_encodes_an_image_that_has_an_alpha_channel() {
        let mut img = swatch(8, 8);
        for pixel in img.pixels_mut() {
            pixel.0[3] = 0;
        }
        let bytes = encode(&img, ShotFormat::Jpeg, 80).expect("alpha must not break JPEG");
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn jpeg_quality_is_clamped_rather_than_rejected() {
        for quality in [0, 1, 100, 255] {
            let bytes = encode(&swatch(8, 8), ShotFormat::Jpeg, quality)
                .unwrap_or_else(|e| panic!("quality {quality} failed: {e}"));
            assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
        }
    }

    #[test]
    fn a_lower_quality_jpeg_is_smaller() {
        let big = encode(&swatch(64, 64), ShotFormat::Jpeg, 95).expect("encode");
        let small = encode(&swatch(64, 64), ShotFormat::Jpeg, 20).expect("encode");
        assert!(
            small.len() < big.len(),
            "quality had no effect: {} vs {}",
            small.len(),
            big.len()
        );
    }

    #[test]
    fn default_options_do_not_send_a_native_4k_frame() {
        let options = CaptureOptions::default();
        let transform = ViewTransform::fit(
            Rect {
                x: 0,
                y: 0,
                width: 3840,
                height: 2160,
            },
            options.max_edge,
        )
        .expect("non-empty");
        assert!(!transform.is_identity());
        assert_eq!(transform.model_width, options.max_edge);
    }

    #[test]
    fn format_round_trips_through_json() {
        // It crosses the MCP boundary, so a rename would silently change the
        // wire format agents see.
        assert_eq!(
            serde_json::to_string(&ShotFormat::Png).expect("serialise"),
            "\"png\""
        );
        assert_eq!(
            serde_json::to_string(&ShotFormat::Jpeg).expect("serialise"),
            "\"jpeg\""
        );
    }

    /// **A LONE DISPLAY IS THE PRIMARY ONE.** Xvfb reports its only screen as
    /// not primary; screen_info must still name the display an unqualified
    /// screenshot captures. Among several, an unreported primary stays
    /// unreported rather than guessed.
    #[test]
    fn a_lone_display_is_primary_and_several_are_not_guessed() {
        assert!(is_primary(false, 1), "the only display there is");
        assert!(is_primary(true, 1));
        assert!(is_primary(true, 3));
        assert!(!is_primary(false, 2), "one of two is not guessed at");
        assert!(!is_primary(false, 0));
    }

    #[cfg(all(windows, feature = "desktop-capture"))]
    mod live {
        use super::*;
        use crate::desktop::screen_info;

        #[test]
        fn a_real_capture_matches_the_display_it_came_from() {
            let shot = capture_primary(CaptureOptions::default()).expect("capture the real screen");
            let screen = screen_info().expect("describe the real screen");
            let primary = screen.primary().expect("a primary display");

            assert!(!shot.bytes.is_empty(), "captured zero bytes");
            assert_eq!(
                &shot.bytes[..8],
                &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
            );

            // The transform must describe the display it was taken from.
            assert_eq!(
                shot.transform.origin,
                PhysicalPoint {
                    x: primary.rect.x,
                    y: primary.rect.y
                },
                "transform origin does not match the primary display"
            );
            assert_eq!(shot.transform.source_width, primary.rect.width as u32);
            assert_eq!(shot.transform.source_height, primary.rect.height as u32);

            let (mw, mh) = shot.model_size();
            assert!(mw > 0 && mh > 0);
            assert!(
                mw <= CaptureOptions::default().max_edge
                    && mh <= CaptureOptions::default().max_edge,
                "transform exceeds the requested max edge: {mw}x{mh}"
            );

            // THE ASSERTION THAT MATTERS, and the one this test was missing.
            //
            // Everything above interrogates the TRANSFORM. The transform can be
            // perfectly computed while the encoded image is a completely
            // different size — and then the model reasons about a picture whose
            // coordinates mean something else entirely. Nothing in the struct
            // reveals that; only decoding the bytes does.
            //
            // Found by a control run: removing the resize left all of the
            // earlier assertions green, because the transform was still right.
            let decoded = image::load_from_memory_with_format(&shot.bytes, ImageFormat::Png)
                .expect("the bytes we emitted must decode");
            assert_eq!(
                (decoded.width(), decoded.height()),
                (mw, mh),
                "THE IMAGE AND ITS TRANSFORM DISAGREE: the model is shown {}x{} \
                 while coordinates are mapped as if it were {mw}x{mh}",
                decoded.width(),
                decoded.height()
            );
        }

        /// THE DRIFT GUARD.
        ///
        /// Two independent things now read this display: `win32.rs`, which calls
        /// GetDpiForMonitor directly, and xcap, which loads the same function by
        /// name (`xcap-0.9.8/src/windows/impl_monitor.rs:102`). They agree today.
        /// If they ever stop agreeing, every captured coordinate is computed
        /// against one set of numbers and clicked against the other — which is
        /// silent, and lands on the wrong control.
        #[test]
        fn xcap_and_win32_report_the_same_geometry() {
            dpi::ensure_per_monitor_aware();
            let screen = screen_info().expect("win32 enumeration");
            let monitors = Monitor::all().expect("xcap enumeration");

            assert_eq!(
                monitors.len(),
                screen.monitors.len(),
                "the two enumerations disagree on how many displays exist"
            );

            let primary = screen.primary().expect("a primary display");
            let xcap_primary = monitors
                .iter()
                .find(|m| m.is_primary().unwrap_or(false))
                .expect("xcap must also report a primary");

            assert_eq!(xcap_primary.x().expect("x"), primary.rect.x);
            assert_eq!(xcap_primary.y().expect("y"), primary.rect.y);
            assert_eq!(xcap_primary.width().expect("w"), primary.rect.width as u32);
            assert_eq!(
                xcap_primary.height().expect("h"),
                primary.rect.height as u32
            );

            let xcap_scale = f64::from(xcap_primary.scale_factor().expect("scale"));
            assert!(
                (xcap_scale - primary.scale).abs() < 0.01,
                "scale drift: xcap says {xcap_scale}, win32 says {}",
                primary.scale
            );
        }

        /// Not an assertion — the readout that proves the coordinate chain end
        /// to end against real pixels.
        ///
        /// `cargo test --lib desktop::capture::tests::live::report_real_capture -- --ignored --nocapture`
        #[test]
        #[ignore = "captures the real screen and prints the mapping"]
        fn report_real_capture() {
            let shot = capture_primary(CaptureOptions::default()).expect("capture");
            let (mw, mh) = shot.model_size();

            println!(
                "\nmonitor      : {} (id {})",
                shot.monitor_name, shot.monitor_id
            );
            println!(
                "source       : {}x{}",
                shot.transform.source_width, shot.transform.source_height
            );
            println!("model sees   : {mw}x{mh}");
            println!(
                "view scale   : {:.4} x, {:.4} y",
                shot.transform.scale_x(),
                shot.transform.scale_y()
            );
            println!(
                "encoded      : {} bytes as {:?}",
                shot.bytes.len(),
                shot.format
            );
            println!(
                "centre maps  : model({},{}) -> {:?}",
                mw / 2,
                mh / 2,
                shot.to_physical(mw / 2, mh / 2)
            );
            println!(
                "far corner   : model({},{}) -> {:?}\n",
                mw - 1,
                mh - 1,
                shot.to_physical(mw - 1, mh - 1)
            );
        }
    }
}
