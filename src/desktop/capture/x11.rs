//! Linux: the screen read straight from the X server, with x11rb.
//!
//! # Why not the capture library
//!
//! xcap, which captures on Windows and macOS, cannot be built on Linux without
//! its Wayland and PipeWire backend, and that backend did not compile on the
//! Ubuntu 22.04 the release is built on (libspa 0.10 against older PipeWire
//! headers). This crate drives X11 only (`docs/COMPUTER_USE.md` decision 5),
//! so the part of xcap it used here is two X requests, made here instead.
//!
//! # What it reports, and where it differs from xcap
//!
//! The same as xcap's X11 path, read from the same places:
//!
//! - **The displays** are RandR 1.5 `GetMonitors` (active ones), in the order
//!   the server gives, each named by its name atom (`HDMI-1`, or the name an
//!   `xrandr --setmonitor` gave it) and flagged primary as the server says.
//!   One entry per monitor: xcap listed a monitor once per OUTPUT it spans,
//!   so a monitor made of two outputs was two identical displays, and one
//!   made of none was missing.
//! - **When RandR reports none** — the extension is absent or older than 1.5,
//!   or a bare server lists nothing — the root window is the one display,
//!   named `screen`. xcap reported no display at all there.
//! - **The scale** is `Xft.dpi` in the root's `RESOURCE_MANAGER` over 96, and
//!   1 without it, as xcap reads it. Unlike xcap, the geometry is NOT divided
//!   by it: X11 input and the accessibility tree are in pixels, so the
//!   displays are too (see `super::desktop_view`), whatever the toolkits'
//!   font scale says.
//! - **The pixels** are `GetImage` (ZPixmap, every plane) of the display's
//!   rectangle of the root window, converted honouring the server's image
//!   byte order, its bits per pixel and scanline pad for the image's depth,
//!   and the root visual's channel masks. xcap assumed BGR whatever the masks
//!   said and read the BITMAP bit order as the byte order; a layout this
//!   cannot convert truthfully (a colour-map visual, an unknown pixel size,
//!   masks that overlap) is refused by name instead of drawn in wrong colours.
//!
//! The connection is the X server `$DISPLAY` names, as xcap's was: on a
//! desktop server that is the display `linux::attach` chose and put there.
//!
//! The parts that decide anything are pure and tested on every platform; the
//! requests themselves are Linux only.

use image::RgbaImage;

use super::super::Rect;

/// One display, as the X server describes it. Read whole at enumeration,
/// so nothing about it is asked again before the capture.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Monitor {
    name: String,
    /// On the root window, in pixels — the space X11 input uses.
    rect: Rect,
    /// What RandR says. A lone display is primary anyway; that rule is
    /// `super::is_primary`, applied where every platform's list is built.
    primary: bool,
    /// `Xft.dpi` over 96: what the toolkits scale by, not the geometry.
    scale: f32,
}

/// The displays to report: what RandR listed, or — when it listed nothing —
/// the root window as one display. Never empty, because an X screen always
/// has a root, so "no displays" is never answered for a server that is
/// there. Pure.
fn displays(reported: Vec<Monitor>, root: Rect, scale: f32) -> Vec<Monitor> {
    if !reported.is_empty() {
        return reported;
    }
    vec![Monitor {
        name: "screen".to_string(),
        rect: root,
        primary: false,
        scale,
    }]
}

/// `Xft.dpi` over 96, from the text of the root's `RESOURCE_MANAGER`
/// property; 1 when it is not there or is not a usable number. Pure.
fn scale_from_resources(resources: &[u8]) -> f32 {
    String::from_utf8_lossy(resources)
        .lines()
        .find_map(|line| line.strip_prefix("Xft.dpi:"))
        .and_then(|value| value.trim().parse::<f32>().ok())
        .filter(|dpi| dpi.is_finite() && *dpi > 0.0)
        .map_or(1.0, |dpi| dpi / 96.0)
}

/// The visual class X11 calls TrueColor: each pixel holds its colour in
/// fixed bit fields. Every other class holds a colour-map index.
const TRUE_COLOR: u8 = 4;

/// How the server lays out the pixels of an image it returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelLayout {
    /// From the server's pixmap format for the image's depth.
    bits_per_pixel: u8,
    /// Each row is padded to a multiple of this many bits.
    scanline_pad: u8,
    /// The server's IMAGE byte order: a pixel's most significant byte
    /// first. (The bitmap bit order is about 1-bit images, not this.)
    msb_first: bool,
    /// The root visual's class, and its channel masks.
    class: u8,
    red: u32,
    green: u32,
    blue: u32,
}

/// One colour channel's place in a pixel.
#[derive(Debug, Clone, Copy)]
struct Channel {
    mask: u32,
    shift: u32,
    /// The largest value the channel holds: `mask >> shift`.
    max: u32,
}

impl Channel {
    /// A channel from its mask, when the mask is one run of set bits — the
    /// only shape a TrueColor visual describes.
    fn of(mask: u32) -> Option<Channel> {
        if mask == 0 {
            return None;
        }
        let shift = mask.trailing_zeros();
        let max = mask >> shift;
        // One run of ones is one less than a power of two.
        if max & max.wrapping_add(1) != 0 {
            return None;
        }
        Some(Channel { mask, shift, max })
    }

    /// The channel of this pixel, in 0..=255.
    fn read(self, pixel: u32) -> u8 {
        let value = u64::from((pixel & self.mask) >> self.shift);
        let max = u64::from(self.max);
        ((value * 255 + max / 2) / max) as u8
    }
}

fn class_name(class: u8) -> &'static str {
    match class {
        0 => "StaticGray",
        1 => "GrayScale",
        2 => "StaticColor",
        3 => "PseudoColor",
        4 => "TrueColor",
        5 => "DirectColor",
        _ => "unknown",
    }
}

/// A ZPixmap image's bytes as RGBA, fully opaque.
///
/// Refuses, saying why, every layout it cannot convert truthfully — so a
/// screen that would come out in the wrong colours is an error, not a
/// picture the model believes. Pure.
fn to_rgba(data: &[u8], width: u32, height: u32, layout: PixelLayout) -> Result<RgbaImage, String> {
    if layout.class != TRUE_COLOR {
        return Err(format!(
            "its pixels are colour-map indices (a {} visual), which this server does not \
             convert; it needs a TrueColor X screen, such as Xvfb's default",
            class_name(layout.class)
        ));
    }
    let bytes_per_pixel = match layout.bits_per_pixel {
        8 | 16 | 24 | 32 => usize::from(layout.bits_per_pixel / 8),
        other => {
            return Err(format!(
                "its pixels are {other} bits each, which this server does not convert \
                 (8, 16, 24 and 32 are)"
            ))
        }
    };
    let pad = usize::from(layout.scanline_pad);
    if pad == 0 || pad % 8 != 0 {
        return Err(format!(
            "its rows are padded to {pad} bits, which is not a whole number of bytes"
        ));
    }
    let masks = [layout.red, layout.green, layout.blue];
    let room = if layout.bits_per_pixel == 32 {
        u32::MAX
    } else {
        (1u32 << layout.bits_per_pixel) - 1
    };
    let channels = masks.map(Channel::of);
    let [Some(red), Some(green), Some(blue)] = channels else {
        return Err(format!(
            "its colour masks ({:#x}, {:#x}, {:#x}) are not one run of bits each",
            layout.red, layout.green, layout.blue
        ));
    };
    if layout.red & layout.green != 0
        || layout.red & layout.blue != 0
        || layout.green & layout.blue != 0
        || (layout.red | layout.green | layout.blue) & !room != 0
    {
        return Err(format!(
            "its colour masks ({:#x}, {:#x}, {:#x}) overlap or do not fit in a {}-bit pixel",
            layout.red, layout.green, layout.blue, layout.bits_per_pixel
        ));
    }

    let (w, h) = (width as usize, height as usize);
    let row_bits = w * usize::from(layout.bits_per_pixel);
    let stride = row_bits.div_ceil(pad) * pad / 8;
    let needed = stride * h;
    if data.len() < needed {
        return Err(format!(
            "the X server sent {} bytes for a {width}x{height} image that needs {needed}",
            data.len()
        ));
    }

    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in data[..needed].chunks_exact(stride) {
        for bytes in row[..w * bytes_per_pixel].chunks_exact(bytes_per_pixel) {
            let pixel = if layout.msb_first {
                bytes.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b))
            } else {
                bytes
                    .iter()
                    .rev()
                    .fold(0u32, |acc, b| (acc << 8) | u32::from(*b))
            };
            rgba.extend_from_slice(&[red.read(pixel), green.read(pixel), blue.read(pixel), 255]);
        }
    }
    RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| format!("a {width}x{height} image could not be assembled"))
}

#[cfg(all(target_os = "linux", feature = "desktop-capture"))]
mod live {
    use std::convert::Infallible;

    use image::RgbaImage;
    use x11rb::connection::{Connection as _, RequestConnection as _};
    use x11rb::protocol::randr::{self, ConnectionExt as _};
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, ImageFormat, ImageOrder};
    use x11rb::rust_connection::RustConnection;

    use super::{displays, scale_from_resources, to_rgba, Monitor, PixelLayout};
    use crate::desktop::Rect;

    /// The X server `$DISPLAY` names, and which of its screens that is.
    fn connect() -> Result<(RustConnection, usize), String> {
        x11rb::connect(None).map_err(|e| {
            let named = std::env::var("DISPLAY").unwrap_or_default();
            format!("the X server DISPLAY={named:?} names could not be reached: {e}")
        })
    }

    fn failed(what: &str) -> impl Fn(&dyn std::fmt::Display) -> String + '_ {
        move |e| format!("{what} failed: {e}")
    }

    /// What RandR 1.5 lists, names resolved. Empty when the server has no
    /// RandR, or one older than 1.5 (`GetMonitors` is 1.5).
    fn randr_monitors(
        conn: &RustConnection,
        root: u32,
        scale: f32,
    ) -> Result<Vec<Monitor>, String> {
        let ask = failed("asking the X server for its monitors (RandR)");
        if conn
            .extension_information(randr::X11_EXTENSION_NAME)
            .map_err(|e| ask(&e))?
            .is_none()
        {
            return Ok(Vec::new());
        }
        let version = conn
            .randr_query_version(1, 5)
            .map_err(|e| ask(&e))?
            .reply()
            .map_err(|e| ask(&e))?;
        if (version.major_version, version.minor_version) < (1, 5) {
            return Ok(Vec::new());
        }
        let reply = conn
            .randr_get_monitors(root, true)
            .map_err(|e| ask(&e))?
            .reply()
            .map_err(|e| ask(&e))?;
        Ok(reply
            .monitors
            .into_iter()
            .map(|m| Monitor {
                // A name is for a report, not a key: one that cannot be read
                // is left empty, as the other platforms' are.
                name: atom_name(conn, m.name).unwrap_or_default(),
                rect: Rect {
                    x: i32::from(m.x),
                    y: i32::from(m.y),
                    width: i32::from(m.width),
                    height: i32::from(m.height),
                },
                primary: m.primary,
                scale,
            })
            .collect())
    }

    fn atom_name(conn: &RustConnection, atom: u32) -> Option<String> {
        if atom == 0 {
            return None;
        }
        let reply = conn.get_atom_name(atom).ok()?.reply().ok()?;
        Some(String::from_utf8_lossy(&reply.name).into_owned())
    }

    /// `Xft.dpi` from the root's resources; 1 when they cannot be read,
    /// which is what a server with no resources set means too.
    fn resources_scale(conn: &RustConnection, root: u32) -> f32 {
        // 64 KiB of resource text, in the 4-byte units the request counts.
        const LONGS: u32 = 16 * 1024;
        let text = conn
            .get_property(
                false,
                root,
                AtomEnum::RESOURCE_MANAGER,
                AtomEnum::STRING,
                0,
                LONGS,
            )
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.value)
            .unwrap_or_default();
        scale_from_resources(&text)
    }

    // The accessors mirror xcap's `Monitor`, so `capture.rs` reads a display
    // the same way on every platform. Everything was read at enumeration, so
    // they cannot fail — hence `Infallible`.
    impl Monitor {
        /// Called by `capture::all_monitors`, the one place every capture and
        /// the display list start from.
        pub(in crate::desktop::capture) fn all() -> Result<Vec<Monitor>, String> {
            let (conn, screen) = connect()?;
            let setup = conn.setup();
            let screen = setup
                .roots
                .get(screen)
                .ok_or_else(|| format!("the X server has no screen {screen}"))?;
            let scale = resources_scale(&conn, screen.root);
            let reported = randr_monitors(&conn, screen.root, scale)?;
            let root = Rect {
                x: 0,
                y: 0,
                width: i32::from(screen.width_in_pixels),
                height: i32::from(screen.height_in_pixels),
            };
            Ok(displays(reported, root, scale))
        }

        /// Called by `capture::capture` and `capture::capture_region`.
        pub(in crate::desktop::capture) fn name(&self) -> Result<String, Infallible> {
            Ok(self.name.clone())
        }

        /// The same as [`Monitor::name`]: X11 has no other. Called by
        /// `capture::monitor_infos`.
        pub(in crate::desktop::capture) fn friendly_name(&self) -> Result<String, Infallible> {
            Ok(self.name.clone())
        }

        /// Called by `capture::capture`, `capture_region` and `monitor_infos`.
        pub(in crate::desktop::capture) fn x(&self) -> Result<i32, Infallible> {
            Ok(self.rect.x)
        }

        /// Called by `capture::capture`, `capture_region` and `monitor_infos`.
        pub(in crate::desktop::capture) fn y(&self) -> Result<i32, Infallible> {
            Ok(self.rect.y)
        }

        /// Called by `capture::capture`, `capture_region` and `monitor_infos`.
        pub(in crate::desktop::capture) fn width(&self) -> Result<u32, Infallible> {
            Ok(self.rect.width.max(0) as u32)
        }

        /// Called by `capture::capture`, `capture_region` and `monitor_infos`.
        pub(in crate::desktop::capture) fn height(&self) -> Result<u32, Infallible> {
            Ok(self.rect.height.max(0) as u32)
        }

        /// Called by `capture::monitor_infos`.
        pub(in crate::desktop::capture) fn scale_factor(&self) -> Result<f32, Infallible> {
            Ok(self.scale)
        }

        /// Called by `capture::capture_primary` and `capture::monitor_infos`.
        pub(in crate::desktop::capture) fn is_primary(&self) -> Result<bool, Infallible> {
            Ok(self.primary)
        }

        /// The display's pixels, from the root window. Called by
        /// `capture::capture` and `capture::capture_region`.
        pub(in crate::desktop::capture) fn capture_image(&self) -> Result<RgbaImage, String> {
            let (conn, screen) = connect()?;
            let setup = conn.setup();
            let screen = setup
                .roots
                .get(screen)
                .ok_or_else(|| format!("the X server has no screen {screen}"))?;
            let outside = || {
                format!(
                    "its rectangle ({},{}) {}x{} is not one X can address",
                    self.rect.x, self.rect.y, self.rect.width, self.rect.height
                )
            };
            let x = i16::try_from(self.rect.x).map_err(|_| outside())?;
            let y = i16::try_from(self.rect.y).map_err(|_| outside())?;
            let width = u16::try_from(self.rect.width).map_err(|_| outside())?;
            let height = u16::try_from(self.rect.height).map_err(|_| outside())?;

            let read = failed("reading the screen from the X server (GetImage)");
            let image = conn
                .get_image(
                    ImageFormat::Z_PIXMAP,
                    screen.root,
                    x,
                    y,
                    width,
                    height,
                    u32::MAX,
                )
                .map_err(|e| read(&e))?
                .reply()
                .map_err(|e| read(&e))?;

            let format = setup
                .pixmap_formats
                .iter()
                .find(|f| f.depth == image.depth)
                .ok_or_else(|| {
                    format!(
                        "the X server sent a depth-{} image and lists no pixel format for it",
                        image.depth
                    )
                })?;
            let visual = screen
                .allowed_depths
                .iter()
                .flat_map(|d| &d.visuals)
                .find(|v| v.visual_id == screen.root_visual)
                .ok_or("the X server does not describe its own root visual")?;
            let layout = PixelLayout {
                bits_per_pixel: format.bits_per_pixel,
                scanline_pad: format.scanline_pad,
                msb_first: setup.image_byte_order == ImageOrder::MSB_FIRST,
                class: u8::from(visual.class),
                red: visual.red_mask,
                green: visual.green_mask,
                blue: visual.blue_mask,
            };
            to_rgba(&image.data, u32::from(width), u32::from(height), layout)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XRGB: PixelLayout = PixelLayout {
        bits_per_pixel: 32,
        scanline_pad: 32,
        msb_first: false,
        class: TRUE_COLOR,
        red: 0x00ff_0000,
        green: 0x0000_ff00,
        blue: 0x0000_00ff,
    };

    fn rgba(image: &RgbaImage) -> Vec<[u8; 4]> {
        image.pixels().map(|p| p.0).collect()
    }

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn monitor(name: &str, at: Rect, primary: bool) -> Monitor {
        Monitor {
            name: name.to_string(),
            rect: at,
            primary,
            scale: 1.0,
        }
    }

    /// **DEPTH 24 IN 32 BITS, LEAST SIGNIFICANT BYTE FIRST** — Xvfb and every
    /// x86 X server. Each pixel's bytes are blue, green, red, pad; the result
    /// is the colour the masks name, opaque.
    #[test]
    fn an_lsb_first_32_bit_screen_reads_as_its_colours() {
        let data = [
            0x00, 0x00, 0xff, 0x00, // red
            0x00, 0xff, 0x00, 0x00, // green
            0xff, 0x00, 0x00, 0x00, // blue
            0x30, 0x20, 0x10, 0xee, // (0x10, 0x20, 0x30); the pad byte is not alpha
        ];
        let image = to_rgba(&data, 2, 2, XRGB).expect("converted");
        assert_eq!((image.width(), image.height()), (2, 2));
        assert_eq!(
            rgba(&image),
            vec![
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [0x10, 0x20, 0x30, 255]
            ]
        );
    }

    /// **THE SAME SCREEN FROM A BIG-ENDIAN SERVER.** The byte order is the
    /// server's image byte order: pad, red, green, blue. Read as if it were
    /// LSB-first, red would come out blue.
    #[test]
    fn an_msb_first_32_bit_screen_reads_as_the_same_colours() {
        let data = [
            0x00, 0xff, 0x00, 0x00, // red
            0x00, 0x00, 0xff, 0x00, // green
            0x00, 0x00, 0x00, 0xff, // blue
            0xee, 0x10, 0x20, 0x30,
        ];
        let layout = PixelLayout {
            msb_first: true,
            ..XRGB
        };
        let image = to_rgba(&data, 2, 2, layout).expect("converted");
        assert_eq!(
            rgba(&image),
            vec![
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [0x10, 0x20, 0x30, 255]
            ]
        );
    }

    /// The masks decide which byte is which colour, not an assumed order: a
    /// visual whose red is the low byte reads that byte as red.
    #[test]
    fn the_visuals_masks_name_the_channels() {
        let layout = PixelLayout {
            red: 0x0000_00ff,
            blue: 0x00ff_0000,
            ..XRGB
        };
        let image = to_rgba(&[0x11, 0x22, 0x33, 0x00], 1, 1, layout).expect("converted");
        assert_eq!(rgba(&image), vec![[0x11, 0x22, 0x33, 255]]);
    }

    /// 16-bit 5-6-5, and a channel narrower than 8 bits scaled to the full
    /// range: the top value is 255, not 248.
    #[test]
    fn a_16_bit_screen_scales_each_channel_to_the_full_range() {
        let layout = PixelLayout {
            bits_per_pixel: 16,
            red: 0xf800,
            green: 0x07e0,
            blue: 0x001f,
            ..XRGB
        };
        // 0xf800 red, 0x07e0 green, 0x001f blue, 0x8410 half grey; LSB first.
        let data = [0x00, 0xf8, 0xe0, 0x07, 0x1f, 0x00, 0x10, 0x84];
        let image = to_rgba(&data, 4, 1, layout).expect("converted");
        assert_eq!(
            rgba(&image),
            vec![
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [132, 130, 132, 255]
            ]
        );
    }

    /// Rows are padded to the scanline pad: two 3-byte pixels take 6 bytes
    /// and the row 8, so the second row starts at byte 8, not 6.
    #[test]
    fn rows_are_read_at_the_padded_stride() {
        let layout = PixelLayout {
            bits_per_pixel: 24,
            ..XRGB
        };
        let data = [
            0x03, 0x02, 0x01, 0x06, 0x05, 0x04, 0xaa, 0xaa, // row 0 + pad
            0x09, 0x08, 0x07, 0x0c, 0x0b, 0x0a, 0xaa, 0xaa, // row 1 + pad
        ];
        let image = to_rgba(&data, 2, 2, layout).expect("converted");
        assert_eq!(
            rgba(&image),
            vec![
                [1, 2, 3, 255],
                [4, 5, 6, 255],
                [7, 8, 9, 255],
                [10, 11, 12, 255]
            ]
        );
    }

    /// **A LAYOUT THAT CANNOT BE CONVERTED TRUTHFULLY IS REFUSED BY NAME** —
    /// a colour-map visual, a pixel size with no conversion, masks that
    /// overlap or are not one run, an image shorter than its size — rather
    /// than drawn in colours the screen does not have.
    #[test]
    fn a_layout_it_cannot_convert_is_refused_by_name() {
        let pseudo = PixelLayout {
            bits_per_pixel: 8,
            scanline_pad: 32,
            class: 3,
            red: 0,
            green: 0,
            blue: 0,
            ..XRGB
        };
        let why = to_rgba(&[0; 4], 1, 1, pseudo).expect_err("a colour map is not colours");
        assert!(why.contains("PseudoColor"), "{why}");

        let four_bits = PixelLayout {
            bits_per_pixel: 4,
            red: 0x4,
            green: 0x2,
            blue: 0x1,
            ..XRGB
        };
        let why = to_rgba(&[0; 4], 1, 1, four_bits).expect_err("no 4-bit conversion");
        assert!(why.contains("4 bits"), "{why}");

        let overlapping = PixelLayout {
            green: 0x00ff_ff00,
            ..XRGB
        };
        let why = to_rgba(&[0; 4], 1, 1, overlapping).expect_err("overlapping masks");
        assert!(why.contains("overlap"), "{why}");

        let split = PixelLayout {
            red: 0x00f0_0f00,
            green: 0x0000_00f0,
            ..XRGB
        };
        let why = to_rgba(&[0; 4], 1, 1, split).expect_err("a mask in two pieces");
        assert!(why.contains("one run"), "{why}");

        let why = to_rgba(&[0; 7], 2, 1, XRGB).expect_err("short data");
        assert!(why.contains("7 bytes") && why.contains("needs 8"), "{why}");
    }

    /// **RANDR'S MONITORS ARE THE DISPLAYS**, in its order, with its names,
    /// places and primary flag; numbered and described the way every
    /// platform's list is. The second of two is the primary one, and only it.
    #[test]
    fn randr_monitors_become_the_display_list() {
        let reported = vec![
            monitor("HDMI-1", rect(-1920, 0, 1920, 1080), false),
            monitor("eDP-1", rect(0, 0, 2560, 1600), true),
        ];
        let list = displays(reported, rect(-1920, 0, 4480, 1600), 1.0);
        let infos: Vec<_> = list
            .iter()
            .enumerate()
            .map(|(i, m)| {
                crate::desktop::capture::describe(
                    i,
                    list.len(),
                    m.name.clone(),
                    m.rect,
                    f64::from(m.scale),
                    m.primary,
                )
            })
            .collect();
        assert_eq!(infos.len(), 2);
        assert_eq!(
            (infos[0].id, infos[0].name.as_str(), infos[0].rect),
            (0, "HDMI-1", rect(-1920, 0, 1920, 1080))
        );
        assert_eq!(infos[0].work_area, infos[0].rect);
        assert!(!infos[0].primary, "not the primary one");
        assert_eq!(
            (infos[1].id, infos[1].name.as_str(), infos[1].rect),
            (1, "eDP-1", rect(0, 0, 2560, 1600))
        );
        assert!(infos[1].primary, "RandR's primary");
        assert_eq!((infos[1].dpi, infos[1].scale), (96, 1.0));
    }

    /// **A SERVER WHOSE RANDR LISTS NOTHING STILL HAS ITS SCREEN.** The root
    /// window is the one display — at the origin, the root's size, named
    /// `screen` — and, being the only one, the primary.
    #[test]
    fn no_randr_monitors_is_the_root_window_as_the_primary_display() {
        let list = displays(Vec::new(), rect(0, 0, 1280, 800), 2.0);
        assert_eq!(list.len(), 1, "{list:?}");
        let only = &list[0];
        assert_eq!(
            (only.name.as_str(), only.rect),
            ("screen", rect(0, 0, 1280, 800))
        );
        assert_eq!(only.scale, 2.0);
        let info = crate::desktop::capture::describe(
            0,
            list.len(),
            only.name.clone(),
            only.rect,
            f64::from(only.scale),
            only.primary,
        );
        assert!(info.primary, "the only display there is");
        assert_eq!((info.dpi, info.scale), (192, 2.0));
    }

    /// `Xft.dpi` over 96, the way a desktop's resources say it; 1 when it is
    /// absent, garbage, zero or negative.
    #[test]
    fn the_scale_is_xft_dpi_over_96() {
        let resources = b"Xcursor.size:\t24\nXft.dpi:\t144\nXft.antialias:\t1\n";
        assert_eq!(scale_from_resources(resources), 1.5);
        assert_eq!(scale_from_resources(b"Xft.dpi: 192"), 2.0);
        for unusable in [
            &b""[..],
            b"Xft.antialias:\t1\n",
            b"Xft.dpi:\tlots",
            b"Xft.dpi:\t0",
            b"Xft.dpi:\t-96",
        ] {
            assert_eq!(
                scale_from_resources(unusable),
                1.0,
                "{:?}",
                String::from_utf8_lossy(unusable)
            );
        }
    }

    /// The live check, against a real X server: what is captured is the size
    /// of the display it was reported as, and in the colours the server drew.
    ///
    /// `DISPLAY=:99 cargo test --lib desktop::capture::x11 -- --ignored`
    /// with an Xvfb (or any X server) on `:99`. It maps a small window of a
    /// known colour, captures, and reads that colour back.
    #[cfg(all(target_os = "linux", feature = "desktop-capture"))]
    #[test]
    #[ignore = "needs an X server on $DISPLAY"]
    fn a_real_x_screen_is_captured_at_its_size_and_in_its_colours() {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};
        use x11rb::wrapper::ConnectionExt as _;

        use crate::desktop::capture::{capture_primary, CaptureOptions};
        use crate::desktop::screen_info;

        let (conn, screen) = x11rb::connect(None).expect("an X server on $DISPLAY");
        let screen = conn.setup().roots[screen].clone();
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == screen.root_visual)
            .expect("the root visual")
            .clone();
        // A window filled with the pixel value the visual calls pure red,
        // on top (override-redirect: no window manager places it).
        let window = conn.generate_id().expect("id");
        conn.create_window(
            screen.root_depth,
            window,
            screen.root,
            40,
            30,
            64,
            48,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .background_pixel(visual.red_mask)
                .override_redirect(1),
        )
        .expect("create");
        conn.map_window(window).expect("map");
        conn.sync().expect("the window is drawn");

        let screen_info = screen_info().expect("the display list");
        let primary = screen_info.primary().expect("a primary display").clone();
        let native = CaptureOptions {
            max_edge: u32::MAX,
            ..CaptureOptions::default()
        };
        let shot = capture_primary(native).expect("capture the X screen");
        let decoded = image::load_from_memory_with_format(&shot.bytes, image::ImageFormat::Png)
            .expect("the bytes decode")
            .to_rgba8();
        conn.destroy_window(window).expect("destroy");
        conn.sync().expect("gone");

        assert_eq!(
            (decoded.width(), decoded.height()),
            (primary.rect.width as u32, primary.rect.height as u32),
            "the image is not the size of the display it came from"
        );
        assert_eq!(
            (shot.transform.source_width, shot.transform.source_height),
            (primary.rect.width as u32, primary.rect.height as u32)
        );
        let (px, py) = (
            (40 + 32 - primary.rect.x) as u32,
            (30 + 24 - primary.rect.y) as u32,
        );
        assert_eq!(
            decoded.get_pixel(px, py).0,
            [255, 0, 0, 255],
            "the red window was captured in another colour"
        );
    }
}
