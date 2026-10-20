//! Pictures through the toolbox: the image result on every route, the
//! headless browser's tools, `RenderPdf`, and the desktop behind the gate.
//!
//! The browser tests run the real headless browser when this machine's own
//! detection finds one (`INCODE_BROWSER`, `admin.json`), and say
//! SKIPPED on stderr when it does not. The PDF test runs the real system
//! browser when one is installed.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn png(width: u32, height: u32) -> Vec<u8> {
    let canvas = ::image::RgbaImage::from_pixel(width, height, ::image::Rgba([40, 80, 160, 255]));
    crate::desktop::capture::encode(&canvas, crate::desktop::capture::ShotFormat::Png, 0)
        .expect("png")
}

/// A PNG of random pixels, too many bytes to pass the image bound.
fn heavy_png() -> Vec<u8> {
    let mut state = 0x9e37_79b9_u32;
    let canvas = ::image::RgbaImage::from_fn(1100, 1100, |_, _| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let [a, b, c, _] = state.to_le_bytes();
        ::image::Rgba([a, b, c, 255])
    });
    crate::desktop::capture::encode(&canvas, crate::desktop::capture::ShotFormat::Png, 0)
        .expect("png")
}

/// The width and height a PNG's IHDR says.
fn png_size(bytes: &[u8]) -> (u32, u32) {
    assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "not a PNG");
    let word =
        |at: usize| u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    (word(16), word(20))
}

fn decoded(out: &ToolOutput) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(out.images[0].base64())
        .expect("base64")
}

/// A desktop that answers `screenshot` with `picture` and everything else
/// with a line, counting the calls that reach it.
pub(super) fn fake_desktop(picture: Vec<u8>, calls: Arc<AtomicUsize>) -> desktop::DesktopClient {
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode(picture);
    let link = desktop::tests::InProcess::new(
        move |line| {
            let request: Value = serde_json::from_str(line).ok()?;
            let id = request.get("id")?.clone();
            let result = match request["params"]["name"].as_str() {
                Some("screenshot") => json!({"content": [
                    {"type": "text", "text": "{\"width\":64,\"height\":48}"},
                    {"type": "image", "data": data, "mimeType": "image/png"}
                ], "isError": false}),
                Some(name) => {
                    json!({"content": [{"type": "text", "text": format!("did {name}")}], "isError": false})
                }
                None => json!({}),
            };
            Some(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
        },
        calls,
    );
    desktop::DesktopClient::over(Box::new(link), false).expect("hello")
}

/// **A PICTURE REACHES THE MODEL ON EVERY ROUTE, AND ONLY THE MODEL.** One
/// screenshot through the toolbox: the bridge hands the CLI an MCP image
/// block; the API route's `tool_result` carries a base64 image block; what
/// incode shows, records and tells the engine is the text and a placeholder.
#[test]
fn a_picture_travels_to_the_model_on_every_route_and_is_recorded_as_a_placeholder() {
    let ws = tempfile::tempdir().expect("ws");
    let calls = Arc::new(AtomicUsize::new(0));
    let tools = Toolbox::new(ws.path(), true)
        .with_desktop_client(fake_desktop(png(64, 48), Arc::clone(&calls)));

    // The toolbox's own answer.
    let out = tools
        .run_call("d1", "screenshot", &json!({}))
        .expect("a picture");
    assert_eq!(out.images.len(), 1);
    assert_eq!(png_size(&decoded(&out)), (64, 48));
    let shown = out.shown();
    assert!(shown.ends_with("[image 64x48 png]"), "{shown}");
    assert!(
        !shown.contains(&out.images[0].base64()),
        "pixels in the record"
    );

    // The API route.
    let message = crate::agent::api::tool_result_message(&[("d1".to_string(), out.clone(), false)]);
    let content = &message["content"][0]["content"];
    assert_eq!(content[0]["type"], "text", "{content}");
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["type"], "base64");
    assert_eq!(content[1]["source"]["media_type"], "image/png");
    assert_eq!(content[1]["source"]["data"], out.images[0].base64());
    // A text answer is still the plain string it always was.
    let plain = crate::agent::api::tool_result_message(&[(
        "t1".to_string(),
        ToolOutput::from("words".to_string()),
        false,
    )]);
    assert_eq!(plain["content"][0]["content"], "words");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // The bridge's half is `tool_bridge`'s
    // `the_bridge_hands_the_model_a_picture_and_incode_a_placeholder`.
}

/// **A PICTURE PAST THE BOUNDS IS NOT PASSED ON**, on the way in from the
/// desktop: the text arrives, with a line saying why the picture did not.
#[test]
fn a_picture_past_the_bounds_is_left_out_and_said_so() {
    let ws = tempfile::tempdir().expect("ws");
    let calls = Arc::new(AtomicUsize::new(0));
    let tools = Toolbox::new(ws.path(), true).with_desktop_client(fake_desktop(heavy_png(), calls));
    let out = tools
        .run_call("d1", "screenshot", &json!({}))
        .expect("the text still arrives");
    assert!(out.images.is_empty(), "an oversized picture was passed on");
    assert!(out.text.contains("picture left out"), "{}", out.text);
    assert!(out.text.contains("bytes"), "{}", out.text);
}

/// **THE DESKTOP TOOLS ARE CLASSIFIED: LOOKING OBSERVES, ACTING RUNS.**
#[test]
fn the_desktop_tools_have_their_reach() {
    for name in ["screen_info", "screenshot", "ui_tree", "cursor_position"] {
        assert_eq!(reach_of(name), Some(Reach::Observe), "{name}");
    }
    for name in [
        "click_element",
        "type_text",
        "paste_text",
        "key",
        "scroll",
        "drag",
        "move_mouse",
        "click",
    ] {
        assert_eq!(reach_of(name), Some(Reach::Run), "{name}");
        assert!(Reach::Run.fails_closed(), "{name}");
    }
    assert_eq!(reach_of(SCREENSHOT), Some(Reach::Observe));
    assert_eq!(reach_of(RENDER_PDF), Some(Reach::Change));
}

/// **A READ-ONLY SESSION CANNOT TYPE**, and the refusal is the toolbox's:
/// the call never reaches the desktop. A screenshot still does.
#[test]
fn a_read_only_session_refuses_every_input_action_before_the_desktop() {
    let ws = tempfile::tempdir().expect("ws");
    let calls = Arc::new(AtomicUsize::new(0));
    let client = {
        use base64::Engine as _;
        let data = base64::engine::general_purpose::STANDARD.encode(png(8, 8));
        let link = desktop::tests::InProcess::new(
            move |line| {
                let request: Value = serde_json::from_str(line).ok()?;
                let id = request.get("id")?.clone();
                let result = json!({"content": [
                    {"type": "text", "text": "ok"},
                    {"type": "image", "data": data, "mimeType": "image/png"}
                ], "isError": false});
                Some(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
            },
            Arc::clone(&calls),
        );
        // Started for a read-only session, as the toolbox would start it.
        desktop::DesktopClient::over(Box::new(link), true).expect("hello")
    };
    let mut tools = Toolbox::new(ws.path(), true).with_desktop_client(client);
    tools.set_permission_mode(crate::permissions::PermissionMode::ReadOnly);
    for (name, input) in [
        ("click", json!({"x": 10, "y": 10})),
        ("type_text", json!({"text": "secret"})),
        ("key", json!({"keys": ["control", "v"]})),
        (
            "drag",
            json!({"from_x": 1, "from_y": 1, "to_x": 2, "to_y": 2}),
        ),
    ] {
        let refused = tools.run_call("r", name, &input).expect_err(name);
        assert!(refused.contains("read-only"), "{name}: {refused}");
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a refused action reached the desktop"
    );
    let shot = tools
        .run_call("r", "screenshot", &json!({}))
        .expect("looking is allowed");
    assert_eq!(shot.images.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// **THE REAL DESKTOP'S SCREENSHOT COMES BACK AS A PICTURE** through the
/// toolbox: the desktop server's own code, in this process behind the
/// child's protocol.
#[cfg(windows)]
#[test]
fn the_real_desktop_screenshot_is_an_image_block() {
    let ws = tempfile::tempdir().expect("ws");
    let input = crate::desktop::thread::InputThread::start().expect("input thread");
    let mut server = crate::desktop::mcp::Server::new(input);
    let link = desktop::tests::InProcess::new(
        move |line| server.handle_line(line),
        Arc::new(AtomicUsize::new(0)),
    );
    let client = desktop::DesktopClient::over(Box::new(link), false).expect("hello");
    let tools = Toolbox::new(ws.path(), true).with_desktop_client(client);
    let out = tools
        .run_call("s", "screenshot", &json!({"max_edge": 640}))
        .expect("a screenshot");
    assert_eq!(out.images.len(), 1, "{}", out.text);
    let (w, h) = png_size(&decoded(&out));
    assert!(w.max(h) <= 640 && w > 0 && h > 0, "{w}x{h}");
    assert!(out.text.contains("UNTRUSTED"), "{}", out.text);
    assert!(out.shown().contains(&format!("[image {w}x{h} png]")));
}

/// **NO BROWSER, NO BROWSER TOOLS** — and WebFetch reads exactly as before.
#[test]
fn without_a_browser_the_browser_tools_are_not_offered() {
    let ws = tempfile::tempdir().expect("ws");
    let tools = Toolbox::new(ws.path(), true).with_printer(None, None);
    let names = tools.names();
    assert!(!names.iter().any(|n| n == SCREENSHOT), "{names:?}");
    assert!(!names.iter().any(|n| n == RENDER_PDF), "{names:?}");
    let fetch = tools
        .definitions()
        .into_iter()
        .find(|d| d["name"] == crate::web::FETCH_TOOL)
        .expect("WebFetch");
    assert!(
        fetch["input_schema"]["properties"]
            .get("render_js")
            .is_none(),
        "{fetch}"
    );
    let refused = tools
        .run_call("x", SCREENSHOT, &json!({"path": "a.html"}))
        .expect_err("not offered");
    assert!(refused.contains("not available"), "{refused}");
    assert!(refused.contains(browser::BROWSER_ENV), "{refused}");

    // The control: with a browser named, both are offered and WebFetch
    // gains `render_js`.
    let with = Toolbox::new(ws.path(), true).with_browser(Some(ws.path().join("obscura")));
    let names = with.names();
    assert!(names.iter().any(|n| n == SCREENSHOT), "{names:?}");
    assert!(names.iter().any(|n| n == RENDER_PDF), "{names:?}");
    let fetch = with
        .definitions()
        .into_iter()
        .find(|d| d["name"] == crate::web::FETCH_TOOL)
        .expect("WebFetch");
    assert!(fetch["input_schema"]["properties"]["render_js"].is_object());
}

/// A slide, drawn at 1280x720 by CSS.
const SLIDE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><style>\
    html,body{margin:0}.s{width:1280px;height:720px;background:#1e3c72;color:#fff;\
    font-family:Arial,sans-serif;padding:60px;box-sizing:border-box}</style></head>\
    <body><div class=\"s\"><h1>Quarterly review</h1><p>Rendering check</p></div></body></html>";

/// **A SCREENSHOT OF A WORKSPACE PAGE IS A PNG OF THE VIEWPORT ASKED FOR**,
/// at scale 1 and 2.
#[test]
fn a_screenshot_of_a_local_page_is_a_png_of_the_viewport() {
    let Some(bin) = browser::tests::test_browser() else {
        return;
    };
    let ws = tempfile::tempdir().expect("ws");
    std::fs::write(ws.path().join("slide.html"), SLIDE).expect("seed");
    let tools = Toolbox::new(ws.path(), true).with_browser(Some(bin));
    let out = tools
        .run_call(
            "s1",
            SCREENSHOT,
            &json!({"path": "slide.html", "width": 1280, "height": 720}),
        )
        .expect("a screenshot");
    assert_eq!(png_size(&decoded(&out)), (1280, 720), "{}", out.text);
    assert!(out.text.contains("viewport 1280x720"), "{}", out.text);
    let twice = tools
        .run_call(
            "s2",
            SCREENSHOT,
            &json!({"path": "slide.html", "width": 640, "height": 360, "scale": 2}),
        )
        .expect("a screenshot at scale 2");
    assert_eq!(png_size(&decoded(&twice)), (1280, 720), "{}", twice.text);
}

/// **FULL PAGE CAPTURES BEYOND THE VIEWPORT.**
#[test]
fn a_full_page_screenshot_is_as_tall_as_the_page() {
    let Some(bin) = browser::tests::test_browser() else {
        return;
    };
    let ws = tempfile::tempdir().expect("ws");
    std::fs::write(
        ws.path().join("long.html"),
        "<!doctype html><html><body style=\"margin:0\"><div style=\"height:2400px;\
         background:linear-gradient(#fff,#000)\">top</div></body></html>",
    )
    .expect("seed");
    let tools = Toolbox::new(ws.path(), true).with_browser(Some(bin));
    let viewport = tools
        .run_call(
            "v",
            SCREENSHOT,
            &json!({"path": "long.html", "width": 800, "height": 600}),
        )
        .expect("viewport");
    assert_eq!(png_size(&decoded(&viewport)), (800, 600));
    let full = tools
        .run_call(
            "f",
            SCREENSHOT,
            &json!({"path": "long.html", "width": 800, "height": 600, "full_page": true}),
        )
        .expect("full page");
    let (w, h) = png_size(&decoded(&full));
    assert_eq!(w, 800, "{}", full.text);
    assert!(h >= 2400, "the full page is {w}x{h}: {}", full.text);
    assert!(full.text.contains("full page"), "{}", full.text);
}

/// **THE SERVE PROCESS DIES WITH THE SESSION.** Its port answers while the
/// session's tools live, and stops answering when they are dropped.
#[test]
fn the_browser_serve_process_is_killed_at_session_end() {
    let Some(bin) = browser::tests::test_browser() else {
        return;
    };
    let ws = tempfile::tempdir().expect("ws");
    std::fs::write(ws.path().join("slide.html"), SLIDE).expect("seed");
    let state = SessionTools::new(true);
    let mut tools = Toolbox::new(ws.path(), true).with_browser(Some(bin));
    tools.share_session(Arc::clone(&state));
    tools
        .run_call("s", SCREENSHOT, &json!({"path": "slide.html"}))
        .expect("a screenshot");
    let ports = state
        .browsers
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .ports();
    assert_eq!(ports.len(), 1, "one serve process, for files");
    let port = ports[0];
    assert!(
        cdp::version(port, std::time::Duration::from_secs(2)).is_ok(),
        "control: the serve process answers while the session lives"
    );
    drop(tools);
    drop(state);
    assert!(
        cdp::version(port, std::time::Duration::from_secs(2)).is_err(),
        "the serve process outlived the session"
    );
}

/// An article with a sentence to find in the PDF.
const ARTICLE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>A</title></head>\
    <body><h1>Printing check</h1><p>The printed text must be SELECTABLEWORDS in the file.</p>\
    </body></html>";

/// **THE SYSTEM BROWSER PRINTS A PDF WITH TEXT IN IT**, into the workspace.
#[test]
fn render_pdf_with_the_system_browser_has_extractable_text() {
    let Some(edge) = pdf::find_chromium(&|name| std::env::var_os(name)) else {
        eprintln!("SKIPPED: no system browser to print with — this test did not run");
        return;
    };
    let ws = tempfile::tempdir().expect("ws");
    std::fs::write(ws.path().join("article.html"), ARTICLE).expect("seed");
    let tools =
        Toolbox::new(ws.path(), true).with_printer(None, Some(pdf::Printer::Chromium(edge)));
    let said = match tools.run(
        RENDER_PDF,
        &json!({"path": "article.html", "output": "out/article.pdf"}),
    ) {
        Ok(said) => said,
        // The machine's browser, not incode: a CI runner's Edge whose
        // setuid sandbox helper is not installed as root.
        Err(why) if why.contains(pdf::NO_BROWSER_SANDBOX) => {
            eprintln!("SKIPPED: the system browser cannot sandbox itself here: {why}");
            return;
        }
        Err(why) => panic!("printed: {why}"),
    };
    assert!(!said.contains("IMAGE-ONLY"), "{said}");
    let bytes = std::fs::read(ws.path().join("out/article.pdf")).expect("written");
    assert!(bytes.starts_with(b"%PDF"));
    let read = tools
        .run(READ, &json!({"path": "out/article.pdf"}))
        .expect("read back");
    assert!(
        read.contains("SELECTABLEWORDS"),
        "no text in the PDF: {read}"
    );
}

/// **THE HEADLESS BROWSER'S PDF IS WHAT ITS PROBE SAYS IT IS.** A build that prints
/// text: no image-only note, and Read returns the page's words. A build that
/// prints pictures: the note, and no words. Either way the probe and Read
/// agree — the probe is what decides the note and the `auto` order.
#[test]
fn render_pdf_with_the_headless_browser_matches_its_probe() {
    let Some(bin) = browser::tests::test_browser() else {
        return;
    };
    let ws = tempfile::tempdir().expect("ws");
    std::fs::write(ws.path().join("article.html"), ARTICLE).expect("seed");
    let tools = Toolbox::new(ws.path(), true).with_browser(Some(bin.clone()));
    let said = tools
        .run(
            RENDER_PDF,
            &json!({"path": "article.html", "output": "article.pdf"}),
        )
        .expect("printed");
    let ability = browser::print_ability(&tools.state.browsers, &bin, true).expect("probed");
    eprintln!("probe of {}: {ability:?}", bin.display());
    let name = browser::Found::by_stem(bin.clone()).name;
    assert!(said.contains(&format!("with {name}")), "{said}");
    let bytes = std::fs::read(ws.path().join("article.pdf")).expect("written");
    assert!(bytes.starts_with(b"%PDF"));
    let read = tools
        .run(READ, &json!({"path": "article.pdf"}))
        .expect("read back");
    if ability.text {
        assert!(!said.contains("IMAGE-ONLY"), "{said}");
        assert!(
            read.contains("SELECTABLEWORDS"),
            "no text in the PDF: {read}"
        );
    } else {
        assert!(said.contains("IMAGE-ONLY"), "{said}");
        assert!(!read.contains("SELECTABLEWORDS"), "{read}");
    }
}

/// **`AUTO` WITH BOTH PRINTERS INSTALLED ALWAYS GIVES A PDF WITH TEXT**: by
/// the headless browser when its probe found text, else by the system browser.
#[test]
fn render_pdf_auto_prints_text_with_the_right_printer() {
    let Some(bin) = browser::tests::test_browser() else {
        return;
    };
    let Some(edge) = pdf::find_chromium(&|name| std::env::var_os(name)) else {
        eprintln!("SKIPPED: no system browser to print with — this test did not run");
        return;
    };
    let ws = tempfile::tempdir().expect("ws");
    std::fs::write(ws.path().join("article.html"), ARTICLE).expect("seed");
    let owner = browser::Found::by_stem(bin.clone());
    let printer = pdf::choose(
        crate::admin::PdfRenderer::Auto,
        Some(edge.clone()),
        Some(owner.clone()),
    );
    let tools = Toolbox::new(ws.path(), true).with_printer(Some(bin.clone()), printer);
    let said = tools
        .run(
            RENDER_PDF,
            &json!({"path": "article.html", "output": "article.pdf"}),
        )
        .expect("printed");
    let ability = browser::print_ability(&tools.state.browsers, &bin, true).expect("probed");
    let system = edge
        .file_stem()
        .expect("a program name")
        .to_string_lossy()
        .into_owned();
    if ability.text {
        assert!(said.contains(&format!("with {}", owner.name)), "{said}");
    } else {
        assert!(said.contains(&format!("with {system}")), "{said}");
    }
    assert!(!said.contains("IMAGE-ONLY"), "{said}");
    let read = tools
        .run(READ, &json!({"path": "article.pdf"}))
        .expect("read back");
    assert!(
        read.contains("SELECTABLEWORDS"),
        "no text in the PDF: {read}"
    );
}

/// The first page's `/MediaBox` width and height in points, rounded.
fn media_box(pdf: &[u8]) -> Option<(i64, i64)> {
    let text = String::from_utf8_lossy(pdf);
    let at = text.find("/MediaBox")?;
    let inside = text[at..].split_once('[')?.1.split_once(']')?.0;
    let numbers: Vec<f64> = inside
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    match numbers.as_slice() {
        [x0, y0, x1, y1] => Some(((x1 - x0).round() as i64, (y1 - y0).round() as i64)),
        _ => None,
    }
}

/// **A PAGE'S OWN `@page` SIZE IS HONOURED** by a headless browser whose probe
/// found `preferCSSPageSize` supported — as the system browser's print does —
/// and Letter otherwise.
#[test]
fn render_pdf_with_the_headless_browser_honours_the_page_size_it_can() {
    let Some(bin) = browser::tests::test_browser() else {
        return;
    };
    let ws = tempfile::tempdir().expect("ws");
    std::fs::write(
        ws.path().join("slide.html"),
        "<!doctype html><html><head><style>@page{size:13.333in 7.5in;margin:0}</style>\
         </head><body><h1>Wide slide</h1></body></html>",
    )
    .expect("seed");
    let tools = Toolbox::new(ws.path(), true).with_browser(Some(bin.clone()));
    tools
        .run(
            RENDER_PDF,
            &json!({"path": "slide.html", "output": "slide.pdf"}),
        )
        .expect("printed");
    let ability = browser::print_ability(&tools.state.browsers, &bin, true).expect("probed");
    let bytes = std::fs::read(ws.path().join("slide.pdf")).expect("written");
    let size = media_box(&bytes).expect("a media box");
    let want = if ability.css_page_size {
        (960, 540)
    } else {
        (612, 792)
    };
    assert_eq!(size, want, "{ability:?}");
}

/// **THE GUARDS SEE THE PDF'S PATH**, and a protected one is refused before
/// any printer is started — the printer here cannot start at all, so a
/// print that got past the guard would fail differently.
#[test]
fn render_pdf_into_a_protected_path_is_refused_by_the_guards() {
    let ws = tempfile::tempdir().expect("ws");
    std::fs::create_dir_all(ws.path().join("tests")).expect("dir");
    std::fs::write(ws.path().join("a.html"), ARTICLE).expect("seed");
    let tools = Toolbox::new(ws.path(), true).with_printer(
        None,
        Some(pdf::Printer::Chromium(PathBuf::from(
            "definitely-not-a-program",
        ))),
    );
    let refused = tools
        .run(
            RENDER_PDF,
            &json!({"path": "a.html", "output": "tests/a.pdf"}),
        )
        .expect_err("protected");
    assert!(refused.contains("protected"), "{refused}");
    assert!(!ws.path().join("tests/a.pdf").exists());
    // The control: the same call to an unprotected path gets past the guard
    // and fails at the printer.
    let unguarded = tools
        .run(RENDER_PDF, &json!({"path": "a.html", "output": "a.pdf"}))
        .expect_err("no such printer");
    assert!(unguarded.contains("could not start"), "{unguarded}");
}

/// **A SCREENSHOT IS HELD TO WEBFETCH'S ADDRESS RULES.**
#[test]
fn a_screenshot_of_a_private_address_is_refused() {
    let ws = tempfile::tempdir().expect("ws");
    let tools = Toolbox::new(ws.path(), true).with_browser(Some(ws.path().join("obscura")));
    for url in [
        "http://127.0.0.1:9222/json",
        "http://10.0.0.1/",
        "file:///C:/Windows/win.ini",
    ] {
        let refused = tools
            .run_call("s", SCREENSHOT, &json!({"url": url}))
            .expect_err(url);
        assert!(
            refused.contains("refused"),
            "{url} was not refused: {refused}"
        );
    }
    let outside = tools
        .run_call("s", SCREENSHOT, &json!({"path": "../outside.html"}))
        .expect_err("outside the workspace");
    assert!(!outside.is_empty());
}
