//! What a tool hands back: text, and sometimes a picture.
//!
//! # Why this is a type and not a string
//!
//! A tool result was a `String` from the tool to the model: the toolbox, the
//! bridge's reply, the API route's `tool_result`, the ACP update. A picture
//! cannot travel as text without being base64 pasted into the model's
//! context as characters, which it cannot see and pays for by the token. So a
//! result is [`ToolOutput`]: the text, plus [`Image`]s that each layer sends in
//! its own image shape — an MCP `image` block over the bridge, a base64
//! `image` block inside the API route's `tool_result`.
//!
//! # What never carries the pixels
//!
//! incode's own event stream, its transcript and the memory engine see
//! [`ToolOutput::shown`]: the text with one placeholder per picture,
//! `[image 1280x720 png]`. A transcript full of base64 is megabytes nobody
//! reads, and the engine would file it as prose.
//!
//! # Bounds
//!
//! Every [`Image`] is checked when it is made, from its own header: a PNG or
//! a JPEG, at most [`MAX_IMAGE_EDGE`] pixels on a side, [`MAX_IMAGE_PIXELS`] in
//! all, and [`MAX_IMAGE_BYTES`] encoded. [`Image::fitted`] makes one fit —
//! scaled down, then re-encoded as JPEG — for a picture this process drew
//! itself; [`Image::from_encoded`] refuses one that does not, for a picture
//! that arrived from another process.

use std::io::Cursor;

use base64::Engine as _;

/// The most encoded bytes one image may have. Its base64 is a third larger,
/// 5 000 000 characters, which is where model endpoints commonly stop taking
/// an image at all.
pub const MAX_IMAGE_BYTES: usize = 3_750_000;
/// The longest side, in pixels, an image may have.
pub const MAX_IMAGE_EDGE: u32 = 8_000;
/// The most pixels an image may have. Also bounds what decoding one to fit
/// it costs: 4 bytes a pixel, 64 MiB.
pub const MAX_IMAGE_PIXELS: u64 = 16_777_216;
/// The most images one result may carry.
pub const MAX_IMAGES_PER_RESULT: usize = 4;

/// How an image is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
}

impl ImageFormat {
    /// The media type every image shape on the wire names it by.
    #[must_use]
    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }

    /// The short word the placeholder uses.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
        }
    }

    /// The format the first bytes say, whatever the sender claimed.
    fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else {
            None
        }
    }

    fn codec(self) -> image::ImageFormat {
        match self {
            Self::Png => image::ImageFormat::Png,
            Self::Jpeg => image::ImageFormat::Jpeg,
        }
    }
}

/// One encoded picture, inside the bounds.
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    format: ImageFormat,
    bytes: Vec<u8>,
    width: u32,
    height: u32,
}

impl std::fmt::Debug for Image {
    // The placeholder, never the bytes: a debug print of a result must not
    // become a megabyte of numbers.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.placeholder())
    }
}

impl Image {
    /// An encoded PNG or JPEG, as it is: refused when it is neither, when its
    /// header cannot be read, or when it is past a bound.
    ///
    /// # Errors
    ///
    /// Why the picture was refused, naming the bound it broke.
    pub fn from_encoded(bytes: Vec<u8>) -> Result<Self, String> {
        let format = ImageFormat::sniff(&bytes)
            .ok_or("the picture is neither a PNG nor a JPEG, so it was not passed on")?;
        let (width, height) = image::ImageReader::with_format(Cursor::new(&bytes), format.codec())
            .into_dimensions()
            .map_err(|e| format!("the picture's header could not be read: {e}"))?;
        let made = Self {
            format,
            bytes,
            width,
            height,
        };
        made.within_bounds()?;
        Ok(made)
    }

    /// [`Self::from_encoded`] for base64 text, as the MCP and CDP shapes
    /// carry a picture.
    ///
    /// # Errors
    ///
    /// The text is not base64, or the picture it holds is refused.
    pub fn from_base64(data: &str) -> Result<Self, String> {
        // Checked BEFORE decoding: a reply claiming a picture a thousand times
        // too large is refused without allocating it.
        if data.len() / 4 * 3 > MAX_IMAGE_BYTES + 3 {
            return Err(format!(
                "the picture is larger than {MAX_IMAGE_BYTES} bytes, so it was not passed on"
            ));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .map_err(|e| format!("the picture was not valid base64: {e}"))?;
        Self::from_encoded(bytes)
    }

    /// A picture this process drew, made to fit: scaled down until both edges
    /// and the pixel count are inside the bounds, then, if the PNG is still
    /// too many bytes, re-encoded as JPEG at falling quality. What was done
    /// is returned beside it, for the model to be told.
    ///
    /// # Errors
    ///
    /// Not a PNG or JPEG, not decodable, or larger than the bounds even as
    /// the smallest JPEG tried.
    pub fn fitted(bytes: Vec<u8>) -> Result<(Self, Option<String>), String> {
        let format = ImageFormat::sniff(&bytes).ok_or("the picture is neither a PNG nor a JPEG")?;
        let (width, height) = image::ImageReader::with_format(Cursor::new(&bytes), format.codec())
            .into_dimensions()
            .map_err(|e| format!("the picture's header could not be read: {e}"))?;
        let scale = fit_scale(width, height);
        if scale >= 1.0 && bytes.len() <= MAX_IMAGE_BYTES {
            return Ok((
                Self {
                    format,
                    bytes,
                    width,
                    height,
                },
                None,
            ));
        }
        // Decoding is bounded by the header already read: a picture past
        // twice the pixel bound is refused rather than decoded to be shrunk.
        if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS * 2 {
            return Err(format!(
                "the picture is {width}x{height}, too large to decode and shrink"
            ));
        }
        let decoded = image::load_from_memory_with_format(&bytes, format.codec())
            .map_err(|e| format!("the picture could not be decoded: {e}"))?
            .to_rgba8();
        let shown = if scale < 1.0 {
            let w = ((f64::from(width) * scale).floor() as u32).max(1);
            let h = ((f64::from(height) * scale).floor() as u32).max(1);
            image::imageops::resize(&decoded, w, h, image::imageops::FilterType::Triangle)
        } else {
            decoded
        };
        let png =
            crate::desktop::capture::encode(&shown, crate::desktop::capture::ShotFormat::Png, 0)
                .map_err(|e| e.to_string())?;
        let mut note = (scale < 1.0).then(|| {
            format!(
                "scaled from {width}x{height} to {}x{} to fit the image bounds",
                shown.width(),
                shown.height()
            )
        });
        if png.len() <= MAX_IMAGE_BYTES {
            return Ok((Self::from_encoded(png)?, note));
        }
        for quality in [85u8, 70, 50] {
            let jpeg = crate::desktop::capture::encode(
                &shown,
                crate::desktop::capture::ShotFormat::Jpeg,
                quality,
            )
            .map_err(|e| e.to_string())?;
            if jpeg.len() <= MAX_IMAGE_BYTES {
                let said =
                    format!("sent as JPEG at quality {quality} to fit {MAX_IMAGE_BYTES} bytes");
                note = Some(match note {
                    Some(before) => format!("{before}; {said}"),
                    None => said,
                });
                return Ok((Self::from_encoded(jpeg)?, note));
            }
        }
        Err(format!(
            "the picture is still larger than {MAX_IMAGE_BYTES} bytes as a JPEG, so it was not sent"
        ))
    }

    fn within_bounds(&self) -> Result<(), String> {
        let Self {
            width,
            height,
            bytes,
            ..
        } = self;
        if *width == 0 || *height == 0 {
            return Err("the picture has no pixels".to_string());
        }
        if (*width).max(*height) > MAX_IMAGE_EDGE {
            return Err(format!(
                "the picture is {width}x{height}, past {MAX_IMAGE_EDGE} pixels on a side, so it \
                 was not passed on"
            ));
        }
        if u64::from(*width) * u64::from(*height) > MAX_IMAGE_PIXELS {
            return Err(format!(
                "the picture is {width}x{height}, past {MAX_IMAGE_PIXELS} pixels, so it was not \
                 passed on"
            ));
        }
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(format!(
                "the picture is {} bytes, past {MAX_IMAGE_BYTES}, so it was not passed on",
                bytes.len()
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn format(&self) -> ImageFormat {
        self.format
    }

    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The encoded bytes, for a file an agent reads the picture from.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The encoded bytes as base64, the way every wire shape carries them.
    #[must_use]
    pub fn base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(&self.bytes)
    }

    /// What the transcript, the event stream and the engine record in the
    /// picture's place.
    #[must_use]
    pub fn placeholder(&self) -> String {
        format!(
            "[image {}x{} {}]",
            self.width,
            self.height,
            self.format.word()
        )
    }
}

/// The factor that brings `width` x `height` inside both the edge and the
/// pixel bound; 1.0 or more when it already is.
fn fit_scale(width: u32, height: u32) -> f64 {
    let edge = f64::from(MAX_IMAGE_EDGE) / f64::from(width.max(height).max(1));
    let pixels =
        (MAX_IMAGE_PIXELS as f64 / (f64::from(width.max(1)) * f64::from(height.max(1)))).sqrt();
    edge.min(pixels)
}

/// One tool's answer: its text, and any pictures.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolOutput {
    /// What the tool said. Every text tool's whole answer, unchanged.
    pub text: String,
    /// Pictures, in order, each already inside the bounds.
    pub images: Vec<Image>,
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

impl ToolOutput {
    /// Text and one picture.
    #[must_use]
    pub fn with_image(text: String, image: Image) -> Self {
        Self {
            text,
            images: vec![image],
        }
    }

    /// The answer as incode records it: the text, then one placeholder per
    /// picture. Never the pixels.
    #[must_use]
    pub fn shown(&self) -> String {
        if self.images.is_empty() {
            return self.text.clone();
        }
        let marks: Vec<String> = self.images.iter().map(Image::placeholder).collect();
        if self.text.is_empty() {
            marks.join("\n")
        } else {
            format!("{}\n{}", self.text, marks.join("\n"))
        }
    }

    /// The answer in MCP's content shape: a text block, then an `image`
    /// block per picture.
    #[must_use]
    pub fn mcp_content(&self) -> Vec<serde_json::Value> {
        let mut blocks = vec![serde_json::json!({"type": "text", "text": self.text})];
        blocks.extend(self.images.iter().map(|image| {
            serde_json::json!({
                "type": "image",
                "data": image.base64(),
                "mimeType": image.format().mime(),
            })
        }));
        blocks
    }

    /// Read an answer back from MCP content blocks: every text block joined,
    /// every image block checked against the bounds. A picture that fails is
    /// replaced by a line saying why, so the text still arrives.
    #[must_use]
    pub fn from_mcp_content(blocks: &[serde_json::Value]) -> Self {
        let mut text = Vec::new();
        let mut images = Vec::new();
        for block in blocks {
            match block.get("type").and_then(serde_json::Value::as_str) {
                Some("text") => {
                    if let Some(said) = block.get("text").and_then(serde_json::Value::as_str) {
                        text.push(said.to_string());
                    }
                }
                Some("image") => {
                    let data = block
                        .get("data")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    if images.len() >= MAX_IMAGES_PER_RESULT {
                        text.push(format!(
                            "[a picture past the {MAX_IMAGES_PER_RESULT} one result may carry was \
                             left out]"
                        ));
                        continue;
                    }
                    match Image::from_base64(data) {
                        Ok(image) => images.push(image),
                        Err(why) => text.push(format!("[picture left out: {why}]")),
                    }
                }
                _ => {}
            }
        }
        Self {
            text: text.join("\n"),
            images,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let canvas = image::RgbaImage::from_pixel(width, height, image::Rgba([30, 60, 120, 255]));
        crate::desktop::capture::encode(&canvas, crate::desktop::capture::ShotFormat::Png, 0)
            .expect("encode")
    }

    /// Random pixels: a PNG of them does not compress, so a picture well
    /// inside the pixel bound can still be past the byte bound.
    fn noise(width: u32, height: u32) -> Vec<u8> {
        let mut state = 0x2545_f491_u32;
        let canvas = image::RgbaImage::from_fn(width, height, |_, _| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let [a, b, c, _] = state.to_le_bytes();
            image::Rgba([a, b, c, 255])
        });
        crate::desktop::capture::encode(&canvas, crate::desktop::capture::ShotFormat::Png, 0)
            .expect("encode")
    }

    #[test]
    fn a_picture_is_read_from_its_own_header() {
        let image = Image::from_encoded(png(64, 32)).expect("a small png");
        assert_eq!((image.width(), image.height()), (64, 32));
        assert_eq!(image.format(), ImageFormat::Png);
        assert_eq!(image.placeholder(), "[image 64x32 png]");
    }

    /// **A PICTURE PAST A BOUND IS REFUSED, NOT PASSED ON.**
    #[test]
    fn a_picture_past_the_edge_or_the_bytes_is_refused() {
        let wide = Image::from_encoded(png(MAX_IMAGE_EDGE + 1, 1)).expect_err("too wide");
        assert!(wide.contains("pixels on a side"), "{wide}");
        let heavy = noise(1100, 1100);
        assert!(heavy.len() > MAX_IMAGE_BYTES, "{} bytes", heavy.len());
        let refused = Image::from_encoded(heavy).expect_err("too many bytes");
        assert!(refused.contains("bytes"), "{refused}");
        let not = Image::from_encoded(b"GIF89a....".to_vec()).expect_err("not png");
        assert!(not.contains("neither a PNG nor a JPEG"), "{not}");
    }

    /// **A PICTURE THIS PROCESS DREW IS MADE TO FIT, AND SAYS HOW.**
    #[test]
    fn a_drawn_picture_is_scaled_and_reencoded_to_fit() {
        let (tall, note) = Image::fitted(png(1000, 12_000)).expect("fits once scaled");
        assert!(tall.height() <= MAX_IMAGE_EDGE, "{tall:?}");
        assert!(note.expect("a note").contains("scaled from 1000x12000"));
        let (heavy, note) = Image::fitted(noise(1100, 1100)).expect("fits as jpeg");
        assert_eq!(heavy.format(), ImageFormat::Jpeg);
        assert!(heavy.bytes.len() <= MAX_IMAGE_BYTES);
        assert!(note.expect("a note").contains("JPEG"));
        let (small, note) = Image::fitted(png(10, 10)).expect("already fits");
        assert_eq!(small.format(), ImageFormat::Png);
        assert!(note.is_none());
    }

    #[test]
    fn base64_claiming_too_much_is_refused_before_it_is_decoded() {
        let huge = "A".repeat(MAX_IMAGE_BYTES / 3 * 4 + 400);
        let why = Image::from_base64(&huge).expect_err("too large");
        assert!(why.contains("larger than"), "{why}");
    }

    /// **THE RECORD HOLDS A PLACEHOLDER, NEVER THE PIXELS.**
    #[test]
    fn what_incode_records_is_the_text_and_a_placeholder() {
        let image = Image::from_encoded(png(8, 4)).expect("png");
        let base = image.base64();
        let out = ToolOutput::with_image("a page".to_string(), image);
        let shown = out.shown();
        assert_eq!(shown, "a page\n[image 8x4 png]");
        assert!(!shown.contains(&base));
        assert_eq!(ToolOutput::from("plain".to_string()).shown(), "plain");
    }

    #[test]
    fn mcp_content_round_trips_and_a_bad_picture_becomes_a_line() {
        let out = ToolOutput::with_image(
            "said".to_string(),
            Image::from_encoded(png(3, 2)).expect("png"),
        );
        let blocks = out.mcp_content();
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["mimeType"], "image/png");
        assert_eq!(ToolOutput::from_mcp_content(&blocks), out);
        let bad = [
            serde_json::json!({"type": "text", "text": "t"}),
            serde_json::json!({"type": "image", "data": "!!", "mimeType": "image/png"}),
        ];
        let read = ToolOutput::from_mcp_content(&bad);
        assert!(read.images.is_empty());
        assert!(read.text.contains("picture left out"), "{}", read.text);
    }
}
