//! The desktop tools themselves: [`Server::run_tool`] names the tool and one
//! step function per tool reads its arguments, asks the guards in
//! [`super::guard`], acts through the input thread or the accessibility
//! layer, and renders what happened; plus the argument readers, the
//! screenshot path and the content blocks every result is made of.
//!
//! # Why these are steps, in a file of their own
//!
//! Root cause: `run_tool` in `desktop/mcp.rs` was 315 lines, over the code
//! auditor's 100-line function threshold, because all sixteen tools were
//! arms of one `match`; and the file was 2,375 lines, over its 1,000-line
//! threshold, because it also held the transport, the definitions, the
//! per-call gates and the guards. `run_tool` is now the `match` alone, and
//! each arm is a named step here with its body unchanged: every argument is
//! read, every guard asked and every refusal returned in the order the arm
//! had them.
//!
//! # Wired to
//!
//! - **Called by** `Server::call_tool` in [`super`], through
//!   [`Server::run_and_look`], once the gates in [`super::gate`] have let
//!   the call through; [`window_arg`] also by `Server::describe`.
//! - **Calls** [`crate::desktop::thread::InputThread`] for the mouse and the
//!   keyboard, [`crate::desktop::uia`] for windows and elements,
//!   [`crate::desktop::capture`] for pictures, and [`super::guard`] before
//!   each of them.

use base64::Engine as _;
use serde_json::{json, Value};

use crate::desktop::capture::{self, CaptureOptions, ShotFormat};
use crate::desktop::coords::PhysicalPoint;
use crate::desktop::thread::{InputThread, MouseButton};
use crate::desktop::uia::{self, WindowInfo};

use super::guard::{
    blocked_windows, guard_focus, refuse_app, refuse_blocked, refuse_point, refuse_region,
    screen_rects,
};
use super::{Server, ACTING, UNTRUSTED};

impl Server {
    /// The tool, and the screenshot it asked for with `screenshot_after`
    /// when it is one of the [`ACTING`] tools.
    ///
    /// Called by `Server::call_tool` only for a call every gate allowed.
    pub(super) fn run_and_look(&mut self, name: &str, args: &Value) -> Result<Vec<Value>, String> {
        self.run_tool(name, args).map(|mut content| {
            let wanted = args.get("screenshot_after").and_then(Value::as_bool) == Some(true);
            if wanted && ACTING.contains(&name) {
                content.extend(after_shot(args));
            }
            content
        })
    }

    /// Run the tool `name`: each arm is the tool's step below.
    ///
    /// Called by [`Server::run_and_look`].
    fn run_tool(&mut self, name: &str, args: &Value) -> Result<Vec<Value>, String> {
        let input = &self.input;
        match name {
            "screen_info" => screen_info(),
            "cursor_position" => cursor_position(input),
            "screenshot" => screenshot(args),
            "list_windows" => list_windows(),
            "focus_window" => focus_window(args),
            "set_value" => set_value(args),
            "element_action" => element_action(args),
            "move_mouse" => move_mouse(input, args),
            "click" => click(input, args),
            "ui_tree" => ui_tree(args),
            "click_element" => click_element(input, args),
            "type_text" => type_text(input, args),
            "paste_text" => paste_text(input, args),
            "key" => key(input, args),
            "scroll" => scroll(input, args),
            "drag" => drag(input, args),
            other => Err(format!("no tool named '{other}'")),
        }
    }
}

/// `screen_info`: every display. Called by [`Server::run_tool`].
fn screen_info() -> Result<Vec<Value>, String> {
    let info = crate::desktop::screen_info().map_err(|e| e.to_string())?;
    Ok(vec![text(
        serde_json::to_string_pretty(&info).unwrap_or_default(),
    )])
}

/// `cursor_position`: where the pointer is. Called by [`Server::run_tool`].
fn cursor_position(input: &InputThread) -> Result<Vec<Value>, String> {
    let at = input.cursor_position().map_err(|e| e.to_string())?;
    Ok(vec![text(json!({"x": at.x, "y": at.y}).to_string())])
}

/// `screenshot`: the options read off the arguments, then [`shoot`].
/// Called by [`Server::run_tool`].
fn screenshot(args: &Value) -> Result<Vec<Value>, String> {
    let options = CaptureOptions {
        max_edge: args
            .get("max_edge")
            .and_then(Value::as_u64)
            .unwrap_or(u64::from(CaptureOptions::default().max_edge)) as u32,
        format: match args.get("format").and_then(Value::as_str) {
            Some("jpeg") => ShotFormat::Jpeg,
            _ => ShotFormat::Png,
        },
        ..CaptureOptions::default()
    };

    shoot(window_arg(args).as_deref(), options)
}

/// `list_windows`: every top-level window, a blocked one without its title.
/// Called by [`Server::run_tool`].
fn list_windows() -> Result<Vec<Value>, String> {
    let windows = uia::list_windows().map_err(|e| e.to_string())?;
    let listed: Vec<Value> = windows
        .iter()
        .map(|w| match crate::desktop::limits::get().blocked(&w.app) {
            // Listed so the agent knows it is there and not to
            // try, with the area every click and capture keeps
            // off; its title (often an entry's name) is not.
            Some(_) => {
                json!({"id": w.id, "app": w.app, "blocked": true, "rect": w.rect})
            }
            None => json!({
                "id": w.id,
                "title": w.title,
                "app": w.app,
                "pid": w.pid,
                "focused": w.focused,
                "rect": w.rect,
                "tree": w.has_tree,
            }),
        })
        .collect();
    Ok(vec![text(format!(
        "{}\n{UNTRUSTED}",
        serde_json::to_string_pretty(&listed).unwrap_or_default()
    ))])
}

/// `focus_window`: bring a window that is not blocked to the front.
/// Called by [`Server::run_tool`].
fn focus_window(args: &Value) -> Result<Vec<Value>, String> {
    let window = window_arg(args).ok_or("window is required — take it from list_windows")?;
    let info = uia::window_info(Some(&window)).map_err(|e| e.to_string())?;
    refuse_blocked(&info)?;
    let done = uia::focus_window(&window).map_err(|e| e.to_string())?;
    Ok(vec![text(done.render())])
}

/// `set_value`: set an element's value through the accessibility layer.
/// Called by [`Server::run_tool`].
fn set_value(args: &Value) -> Result<Vec<Value>, String> {
    let window = window_arg(args);
    let id = element_id(args)?;
    let value = args
        .get("value")
        .and_then(Value::as_str)
        .ok_or("value is required")?;
    let info = uia::window_info(window.as_deref()).map_err(|e| e.to_string())?;
    refuse_blocked(&info)?;
    let done = uia::set_value(Some(&info.id), id, value).map_err(|e| e.to_string())?;
    Ok(vec![text(done.render())])
}

/// `element_action`: run an element's own action, without the mouse.
/// Called by [`Server::run_tool`].
fn element_action(args: &Value) -> Result<Vec<Value>, String> {
    let window = window_arg(args);
    let id = element_id(args)?;
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .ok_or("action is required, e.g. \"press\"")?;
    let info = uia::window_info(window.as_deref()).map_err(|e| e.to_string())?;
    refuse_blocked(&info)?;
    let done = uia::act(Some(&info.id), id, action).map_err(|e| e.to_string())?;
    Ok(vec![text(done.render())])
}

/// `move_mouse`: move the pointer, the focused window checked first.
/// Called by [`Server::run_tool`].
fn move_mouse(input: &InputThread, args: &Value) -> Result<Vec<Value>, String> {
    let at = point(args)?;
    guard_focus(None)?;
    input.move_mouse(at).map_err(|e| e.to_string())?;
    Ok(vec![text(format!("moved to {},{}", at.x, at.y))])
}

/// `click`: click at a point that is on a display and in no blocked window.
/// Called by [`Server::run_tool`].
fn click(input: &InputThread, args: &Value) -> Result<Vec<Value>, String> {
    let at = point(args)?;
    guard_focus(None)?;
    refuse_point(at)?;
    let button = mouse_button(args);
    let count = args.get("count").and_then(Value::as_u64).unwrap_or(1) as u8;

    input.click(at, button, count).map_err(|e| e.to_string())?;
    Ok(vec![text(format!(
        "clicked {button:?} x{count} at {},{}",
        at.x, at.y
    ))])
}

/// `ui_tree`: a window's controls, walked and then refused when its
/// application is blocked. Called by [`Server::run_tool`].
fn ui_tree(args: &Value) -> Result<Vec<Value>, String> {
    let depth = args
        .get("depth")
        .and_then(Value::as_u64)
        .unwrap_or(u64::from(crate::desktop::limits::get().tree_depth)) as u32;
    let tree = uia::window_tree(window_arg(args).as_deref(), depth).map_err(|e| e.to_string())?;
    // Walked in this process, and discarded unread when blocked.
    refuse_app(&tree.app)?;
    // With no displays readable the tree is still shown, unmarked:
    // reading it is harmless, and every action re-checks its point.
    let screens = screen_rects().unwrap_or_default();
    Ok(vec![text(format!(
        "{}\n{UNTRUSTED}",
        tree.render_on(&screens)
    ))])
}

/// `click_element`: click an element at its centre as the live tree has it
/// now. Called by [`Server::run_tool`].
fn click_element(input: &InputThread, args: &Value) -> Result<Vec<Value>, String> {
    let window = window_arg(args);
    let id = element_id(args)?;
    let button = mouse_button(args);
    let info = uia::window_info(window.as_deref()).map_err(|e| e.to_string())?;
    refuse_blocked(&info)?;

    // RE-READ, never trust the id's remembered rectangle. Between
    // the agent reading a tree and acting on it a list can scroll
    // or a dialog can move, and clicking the old position would hit
    // whatever took its place — silently, and on the wrong control.
    let element = live_element(&info, id)?;
    if !element.enabled {
        return Err(format!(
            "e{id} \"{}\" is disabled; clicking it would do nothing",
            element.name
        ));
    }

    let at = element.click_point();
    // The element is in an allowed window, but its centre may be
    // under a blocked one (or scrolled out of its own view).
    refuse_point(at)?;
    input.click(at, button, 1).map_err(|e| e.to_string())?;

    Ok(vec![text(format!(
        "clicked {} at {},{} (unverified: a click has no state to read back; look \
         at the result)",
        element.label(),
        at.x,
        at.y
    ))])
}

/// `type_text`: keystrokes into the focused window, which must be the one
/// named. Called by [`Server::run_tool`].
fn type_text(input: &InputThread, args: &Value) -> Result<Vec<Value>, String> {
    let body = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or("text is required")?;
    let focused = guard_focus(window_arg(args).as_deref())?;
    input.type_text(body).map_err(|e| e.to_string())?;
    Ok(vec![text(format!(
        "typed {} characters{} (unverified: keystrokes are not read back)",
        body.chars().count(),
        into(&focused)
    ))])
}

/// `paste_text`: the text pasted through the clipboard, which is put back.
/// Called by [`Server::run_tool`].
fn paste_text(input: &InputThread, args: &Value) -> Result<Vec<Value>, String> {
    let body = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or("text is required")?;
    let focused = guard_focus(window_arg(args).as_deref())?;
    let restored = input.paste_text(body).map_err(|e| e.to_string())?;
    Ok(vec![text(format!(
        "pasted {} characters{} ({})",
        body.chars().count(),
        into(&focused),
        if restored {
            "the clipboard was put back as it was"
        } else {
            "the clipboard held no text before, so it now holds this; an image or \
             anything else that was on it is gone"
        }
    ))])
}

/// `key`: a chord pressed into the focused window. Called by
/// [`Server::run_tool`].
fn key(input: &InputThread, args: &Value) -> Result<Vec<Value>, String> {
    let keys: Vec<String> = args
        .get("keys")
        .and_then(Value::as_array)
        .ok_or("keys is required and must be an array, e.g. [\"control\",\"c\"]")?
        .iter()
        .filter_map(|k| k.as_str().map(str::to_string))
        .collect();
    let focused = guard_focus(window_arg(args).as_deref())?;
    input.chord(&keys).map_err(|e| e.to_string())?;
    Ok(vec![text(format!(
        "pressed {}{}",
        keys.join("+"),
        into(&focused)
    ))])
}

/// `scroll`: wheel steps over a named element, else at the cursor, the point
/// checked either way. Called by [`Server::run_tool`].
fn scroll(input: &InputThread, args: &Value) -> Result<Vec<Value>, String> {
    let dx = scroll_steps(args, "dx")?;
    let dy = scroll_steps(args, "dy")?;
    if dx == 0 && dy == 0 {
        return Err("scroll needs a non-zero dx or dy".to_string());
    }
    // Over an element when one is named: the cursor goes to its
    // centre first, since a wheel scrolls whatever is under it.
    let over = match args.get("id") {
        Some(_) => {
            let info = uia::window_info(window_arg(args).as_deref()).map_err(|e| e.to_string())?;
            refuse_blocked(&info)?;
            let element = live_element(&info, element_id(args)?)?;
            refuse_point(element.click_point())?;
            input
                .move_mouse(element.click_point())
                .map_err(|e| e.to_string())?;
            format!(" over {}", element.label())
        }
        None => {
            guard_focus(None)?;
            refuse_point(input.cursor_position().map_err(|e| e.to_string())?)?;
            String::new()
        }
    };
    input.scroll(dx, dy).map_err(|e| e.to_string())?;
    Ok(vec![text(format!("scrolled dx={dx} dy={dy}{over}"))])
}

/// `drag`: element to element, or point to point, with both ends checked.
/// Called by [`Server::run_tool`].
fn drag(input: &InputThread, args: &Value) -> Result<Vec<Value>, String> {
    let (from, to) = match (args.get("from_id"), args.get("to_id")) {
        (Some(from), Some(to)) => {
            let info = uia::window_info(window_arg(args).as_deref()).map_err(|e| e.to_string())?;
            refuse_blocked(&info)?;
            let id = |v: &Value, what: &str| {
                v.as_u64()
                    .map(|n| n as u32)
                    .ok_or(format!("{what} must be an element id from ui_tree"))
            };
            (
                live_element(&info, id(from, "from_id")?)?.click_point(),
                live_element(&info, id(to, "to_id")?)?.click_point(),
            )
        }
        _ => {
            guard_focus(None)?;
            let n = |key: &str| {
                coordinate(args, key)?.ok_or(format!(
                    "{key} is required (or from_id and to_id from ui_tree)"
                ))
            };
            (
                PhysicalPoint {
                    x: n("from_x")?,
                    y: n("from_y")?,
                },
                PhysicalPoint {
                    x: n("to_x")?,
                    y: n("to_y")?,
                },
            )
        }
    };
    // Both ends: the press picks up what is under `from`, and the
    // release drops onto whatever is under `to`.
    refuse_point(from)?;
    refuse_point(to)?;
    let button = mouse_button(args);
    input.drag(from, to, button).map_err(|e| e.to_string())?;
    Ok(vec![text(format!(
        "dragged from {},{} to {},{}",
        from.x, from.y, to.x, to.y
    ))])
}

/// The `button` argument: right, middle, or left for anything else.
/// Called by [`click`], [`click_element`] and [`drag`].
fn mouse_button(args: &Value) -> MouseButton {
    match args.get("button").and_then(Value::as_str) {
        Some("right") => MouseButton::Right,
        Some("middle") => MouseButton::Middle,
        _ => MouseButton::Left,
    }
}

/// The `window` argument, when one was given.
///
/// Called by the tool steps here and by `Server::describe` in
/// [`super::gate`].
pub(super) fn window_arg(args: &Value) -> Option<String> {
    args.get("window")
        .and_then(|w| match w {
            Value::String(s) => Some(s.trim().to_string()),
            // A handle is a number on Windows; take it either way.
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .filter(|s| !s.is_empty())
}

/// Where a result says the input went.
fn into(focused: &Option<WindowInfo>) -> String {
    match focused {
        Some(w) => format!(" into \"{}\" ({})", w.title, w.app),
        None => String::new(),
    }
}

/// A wheel step count, held to `desktop_limits.max_scroll_steps`.
fn scroll_steps(args: &Value, key: &str) -> Result<i32, String> {
    let Some(raw) = args.get(key) else {
        return Ok(0);
    };
    let max = i64::from(crate::desktop::limits::get().max_scroll_steps);
    match raw.as_i64() {
        Some(n) if n.abs() <= max => Ok(n as i32),
        Some(n) => Err(format!(
            "{key}={n} is more than {max} wheel steps (desktop_limits.max_scroll_steps); scroll \
             in smaller steps and look between them"
        )),
        None => Err(format!("{key} must be a whole number of wheel steps")),
    }
}

/// A coordinate argument, refused rather than wrapped when it does not fit.
fn coordinate(args: &Value, key: &str) -> Result<Option<i32>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| format!("{key} must be a whole number of pixels")),
    }
}

/// The `x` and `y` of a pointer action, both required.
pub(super) fn point(args: &Value) -> Result<PhysicalPoint, String> {
    Ok(PhysicalPoint {
        x: coordinate(args, "x")?.ok_or("x is required and must be a number")?,
        y: coordinate(args, "y")?.ok_or("y is required and must be a number")?,
    })
}

/// The `id` of an element, as `ui_tree` numbered it.
fn element_id(args: &Value) -> Result<u32, String> {
    args.get("id")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| "id is required — take it from ui_tree".to_string())
}

/// The element `id` in `window` as it is NOW, re-read from the live tree.
fn live_element(window: &WindowInfo, id: u32) -> Result<uia::Element, String> {
    uia::locate(Some(&window.id), id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "element e{id} is no longer in window \"{}\". The UI changed — call ui_tree \
                 again rather than acting where it used to be.",
                window.title
            )
        })
}

/// A screenshot of one window, or of the primary display — refused when a
/// blocked application's window is in what would be captured.
fn shoot(window: Option<&str>, options: CaptureOptions) -> Result<Vec<Value>, String> {
    let (shot, of) = match window {
        Some(id) => {
            let info = uia::window_info(Some(id)).map_err(|e| e.to_string())?;
            refuse_blocked(&info)?;
            if let Some(why) = uia::window_position_unknown(&info) {
                return Err(format!(
                    "{why} so that window's area cannot be cut out of the screen; take a \
                     screenshot without `window`"
                ));
            }
            refuse_region(info.rect)?;
            let shot = capture::capture_region(info.rect, options).map_err(|e| e.to_string())?;
            (shot, Some(info))
        }
        None => {
            // Every window on screen is in a full capture, so every one is
            // checked — and a desktop that cannot list them is refused.
            if let Some(blocked) = blocked_windows()?.first() {
                return Err(format!(
                    "refused: a window of \"{}\" is on screen, and it is blocked; a full \
                     screenshot would show it. Pass `window` to capture one window, or close \
                     or minimise it.",
                    blocked.app
                ));
            }
            (
                capture::capture_primary(options).map_err(|e| e.to_string())?,
                None,
            )
        }
    };
    let (width, height) = shot.model_size();
    let t = shot.transform;
    Ok(vec![
        // The transform travels BESIDE the image, because a coordinate the
        // model names is meaningless without it: physical = origin +
        // image point * (region / image size).
        text(
            json!({
                "width": width,
                "height": height,
                "origin": {"x": t.origin.x, "y": t.origin.y},
                "region": {"width": t.source_width, "height": t.source_height},
                "monitor_id": shot.monitor_id,
                "monitor": shot.monitor_name,
                "window": of.as_ref().map(|w| &w.id),
                "note": UNTRUSTED,
            })
            .to_string(),
        ),
        image(&shot.bytes, shot.format),
    ])
}

/// The screenshot an action asked for with `screenshot_after`: of the window
/// it named, else of the focused window, after the screen has had a moment
/// to change. A failure is reported beside the action's result rather than
/// replacing it — the action itself already happened.
fn after_shot(args: &Value) -> Vec<Value> {
    std::thread::sleep(crate::desktop::limits::get().settle);
    let window = window_arg(args).or_else(|| uia::foreground().ok().flatten().map(|w| w.id));
    match shoot(window.as_deref(), CaptureOptions::default()) {
        Ok(content) => content,
        Err(why) => vec![text(format!("(no screenshot after: {why})"))],
    }
}

/// A text content block.
fn text(body: String) -> Value {
    json!({"type": "text", "text": body})
}

/// An image content block.
///
/// UNVERIFIED against a live server: the memory service exposes no tool that
/// returns an image, so unlike every other shape in this file this one comes
/// from the specification rather than from a measurement. Confirm it against a
/// real client before trusting it.
pub(super) fn image(bytes: &[u8], format: ShotFormat) -> Value {
    json!({
        "type": "image",
        "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        "mimeType": match format {
            ShotFormat::Png => "image/png",
            ShotFormat::Jpeg => "image/jpeg",
        },
    })
}
