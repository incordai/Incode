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
//! - **Calls** [`crate::thread::InputThread`] for the mouse and the
//!   keyboard, [`crate::uia`] for windows and elements,
//!   [`crate::capture`] for pictures, and [`super::guard`] before
//!   each of them.

use base64::Engine as _;
use serde_json::{json, Value};

use crate::capture::{self, CaptureOptions, ShotFormat};
use crate::coords::PhysicalPoint;
use crate::selector::Selector;
use crate::thread::{InputThread, MouseButton};
use crate::uia::{self, WindowInfo};

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
            if wanted && looks_after(name) {
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
            "wait_for" => {
                // Asked between polls and between sleeps, so the operator's
                // stop ends a wait within a fraction of a second.
                let stopped = || {
                    self.catch_stop_file();
                    self.safety.is_stopped()
                };
                super::wait_tool::wait_for(args, &stopped)
            }
            "watch" => {
                self.journal_note("watch", &Self::describe("watch", args));
                // The same stop, asked as often, for a wait of hours.
                let stopped = || {
                    self.catch_stop_file();
                    self.safety.is_stopped()
                };
                super::wait_tool::watch(args, &stopped)
            }
            "routine_save" | "routine_run" | "routine_forget" | "routine_list"
            | "routine_match" => self.routine_tool(name, args),
            other => Err(format!("no tool named '{other}'")),
        }
    }
}

/// Whether a call to `name` may end with the screenshot `screenshot_after` asks
/// for: an [`ACTING`] tool that does something on the screen. Saving or
/// forgetting a routine changes a file, not the screen, so there is nothing to
/// look at afterwards. Also read by `definitions`, so the schema offers the
/// argument exactly where it is honoured.
pub(super) fn looks_after(name: &str) -> bool {
    ACTING.contains(&name) && !matches!(name, "routine_save" | "routine_forget")
}

/// The text of a tool's answer: its text blocks, joined by newlines. An image
/// block has no text and is left out.
pub(super) fn text_of(content: &[Value]) -> String {
    content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `screen_info`: every display. Called by [`Server::run_tool`].
fn screen_info() -> Result<Vec<Value>, String> {
    let info = crate::screen_info().map_err(|e| e.to_string())?;
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
        .map(|w| match crate::limits::get().blocked(&w.app) {
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
    let target = element_target(args)?;
    let value = args
        .get("value")
        .and_then(Value::as_str)
        .ok_or("value is required")?;
    match target {
        Target::Id(id) => {
            let info = uia::window_info(window.as_deref()).map_err(|e| e.to_string())?;
            refuse_blocked(&info)?;
            let done = uia::set_value(Some(&info.id), id, value).map_err(|e| e.to_string())?;
            Ok(vec![text(done.render())])
        }
        Target::Selector(selector) => {
            let found = resolve_selector(window.as_deref(), &selector)?;
            let done = uia::set_value_on(&found, value).map_err(|e| e.to_string())?;
            Ok(vec![text(by_selector(done.render(), &found))])
        }
    }
}

/// `element_action`: run an element's own action, without the mouse.
/// Called by [`Server::run_tool`].
fn element_action(args: &Value) -> Result<Vec<Value>, String> {
    let window = window_arg(args);
    let target = element_target(args)?;
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .ok_or("action is required, e.g. \"press\"")?;
    match target {
        Target::Id(id) => {
            let info = uia::window_info(window.as_deref()).map_err(|e| e.to_string())?;
            refuse_blocked(&info)?;
            let done = uia::act(Some(&info.id), id, action).map_err(|e| e.to_string())?;
            Ok(vec![text(done.render())])
        }
        Target::Selector(selector) => {
            let found = resolve_selector(window.as_deref(), &selector)?;
            let done = uia::act_on(&found, action).map_err(|e| e.to_string())?;
            Ok(vec![text(by_selector(done.render(), &found))])
        }
    }
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
        .unwrap_or(u64::from(crate::limits::get().tree_depth)) as u32;
    let tree = uia::window_tree(window_arg(args).as_deref(), depth).map_err(|e| e.to_string())?;
    // Walked in this process, and discarded unread when blocked.
    refuse_app(&tree.app)?;
    // With no displays readable the tree is still shown, unmarked:
    // reading it is harmless, and every action re-checks its point.
    let screens = screen_rects().unwrap_or_default();
    // The depth the walk really went to (held to its bound), because the
    // selectors printed are counted in that walk and say so when it is not
    // the default one.
    Ok(vec![text(format!(
        "{}\n{UNTRUSTED}",
        tree.render_at(&screens, uia::bounded(depth))
    ))])
}

/// `click_element`: click an element at its centre as the live tree has it
/// now. Called by [`Server::run_tool`].
fn click_element(input: &InputThread, args: &Value) -> Result<Vec<Value>, String> {
    let window = window_arg(args);
    let target = element_target(args)?;
    let button = mouse_button(args);

    // RE-READ, never trust the id's remembered rectangle. Between
    // the agent reading a tree and acting on it a list can scroll
    // or a dialog can move, and clicking the old position would hit
    // whatever took its place — silently, and on the wrong control.
    //
    // A selector is resolved against the live tree here, the same way: its
    // one match (or an error saying none or several) is the control as it is
    // now.
    let (element, label, cut) = match &target {
        Target::Id(id) => {
            let info = uia::window_info(window.as_deref()).map_err(|e| e.to_string())?;
            refuse_blocked(&info)?;
            let element = live_element(&info, *id)?;
            let label = element.label();
            (element, label, String::new())
        }
        Target::Selector(selector) => {
            let found = resolve_selector(window.as_deref(), selector)?;
            if let Some(why) = &found.no_positions {
                return Err(format!(
                    "{why} so the control cannot be found on screen; use element_action or \
                     set_value on it"
                ));
            }
            let label = match found.element.name_for_messages() {
                Some(name) => format!(
                    "selector `{}` (\"{name}\", {:?})",
                    selector.render_redacted(),
                    found.element.role
                ),
                None => format!(
                    "selector `{}` ({:?})",
                    selector.render_redacted(),
                    found.element.role
                ),
            };
            let cut = with_cut_note(String::new(), &found);
            (found.element, label, cut)
        }
    };
    if !element.enabled {
        return Err(disabled_error(&target, &element));
    }

    let at = element.click_point();
    // The element is in an allowed window, but its centre may be
    // under a blocked one (or scrolled out of its own view).
    refuse_point(at)?;
    input.click(at, button, 1).map_err(|e| e.to_string())?;

    Ok(vec![text(format!(
        "clicked {label} at {},{} (unverified: a click has no state to read back; look \
         at the result){cut}",
        at.x, at.y
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
    let max = i64::from(crate::limits::get().max_scroll_steps);
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

/// How a call names a control: the number of the last `ui_tree`, or a selector
/// (`crate::selector`) resolved against the live tree.
#[derive(Debug)]
pub(super) enum Target {
    Id(u32),
    Selector(Selector),
}

/// The `id` or the `selector` of an element tool, one of them and not both.
pub(super) fn element_target(args: &Value) -> Result<Target, String> {
    match (args.get("id"), args.get("selector")) {
        (Some(_), Some(_)) => Err("give `id` or `selector`, not both".to_string()),
        (None, Some(raw)) => {
            let raw = raw
                .as_str()
                .ok_or("selector must be text like role=button;name=Save")?;
            let selector = Selector::parse(raw).map_err(|e| format!("selector: {e}"))?;
            selector.require_element()?;
            Ok(Target::Selector(selector))
        }
        _ => element_id(args).map(Target::Id).map_err(|_| {
            "id (the eN number from ui_tree) or selector (as ui_tree prints it, \
             e.g. role=button;name=Save) is required"
                .to_string()
        }),
    }
}

/// The one control `selector` names, in the window it names (or `window`, or
/// the focused one), refused before anything is read when that window is a
/// blocked application's.
///
/// The blocked-application refusal is the window guard handed to
/// `uia::window_for`, so it comes BEFORE the selector's window part is
/// compared with a window named by id: a mismatch error describes the window,
/// and for a blocked one that is its title, and a match and a mismatch would
/// be told apart.
fn resolve_selector(window: Option<&str>, selector: &Selector) -> Result<uia::Resolved, String> {
    let info = uia::window_for(window, selector, &refuse_blocked).map_err(|e| e.to_string())?;
    uia::resolve_in(info, selector).map_err(|e| e.to_string())
}

/// Why clicking a disabled control is refused. The id path names the control
/// as it always has, except that an edit or text field is not named
/// ([`uia::Element::name_for_messages`]). A selector is named redacted and the
/// control is not named at all: this error is journalled word for word, and a
/// text field's name is what was typed in it.
pub(super) fn disabled_error(target: &Target, element: &uia::Element) -> String {
    match target {
        Target::Id(id) => match element.name_for_messages() {
            Some(name) => format!("e{id} \"{name}\" is disabled; clicking it would do nothing"),
            None => format!("e{id} is disabled; clicking it would do nothing"),
        },
        Target::Selector(selector) => format!(
            "selector `{}` ({:?}) is disabled; clicking it would do nothing",
            selector.render_redacted(),
            element.role
        ),
    }
}

/// The id of the one window a window-only `selector` names, now: how a replayed
/// routine's keyboard step (`in_window`) becomes the `window` the tool checks
/// the focus against. Refused, as everything else is, when that window is a
/// blocked application's (the guard is handed to `uia::window_for`, as for every
/// other selector).
///
/// Called by `routine_tool`'s replay.
pub(super) fn window_id_for(selector: &Selector) -> Result<String, String> {
    let info = uia::window_for(None, selector, &refuse_blocked).map_err(|e| e.to_string())?;
    Ok(info.id)
}

/// `done`, with what only a selector adds: its `eN` numbers this call's own
/// walk and not the agent's last `ui_tree`, and a cut tree cannot rule out a
/// second match.
fn by_selector(done: String, found: &uia::Resolved) -> String {
    format!(
        "{done} (found by selector; any eN here numbers this call's own walk, not your last \
         ui_tree){}",
        with_cut_note(String::new(), found)
    )
}

/// `base`, and the warning that the tree was cut when it was.
fn with_cut_note(base: String, found: &uia::Resolved) -> String {
    if found.truncated {
        format!(
            "{base} (the walk was cut at its bound of {} nodes visited, so a second control \
             that fits the selector may lie beyond it)",
            crate::limits::get().tree_max_visited
        )
    } else {
        base
    }
}

/// What must hold before the screen, or one window of it, is captured:
/// refused when that window is a blocked application's, when its position is
/// not known, or when a blocked window is in what the picture would show. The
/// window it is, when one was named. Called by [`shoot`] and `wait_for`'s
/// screen condition, so both are held to the same rule.
pub(super) fn guard_capture(window: Option<&str>) -> Result<Option<WindowInfo>, String> {
    let Some(id) = window else {
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
        return Ok(None);
    };
    let info = uia::window_info(Some(id)).map_err(|e| e.to_string())?;
    refuse_blocked(&info)?;
    if let Some(why) = uia::window_position_unknown(&info) {
        return Err(format!(
            "{why} so that window's area cannot be cut out of the screen; take a \
             screenshot without `window`"
        ));
    }
    refuse_region(info.rect)?;
    Ok(Some(info))
}

/// A screenshot of one window, or of the primary display — refused when a
/// blocked application's window is in what would be captured.
fn shoot(window: Option<&str>, options: CaptureOptions) -> Result<Vec<Value>, String> {
    let of = guard_capture(window)?;
    let shot = match &of {
        Some(info) => capture::capture_region(info.rect, options),
        None => capture::capture_primary(options),
    }
    .map_err(|e| e.to_string())?;
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
    std::thread::sleep(crate::limits::get().settle);
    let window = window_arg(args).or_else(|| uia::foreground().ok().flatten().map(|w| w.id));
    match shoot(window.as_deref(), CaptureOptions::default()) {
        Ok(content) => content,
        Err(why) => vec![text(format!("(no screenshot after: {why})"))],
    }
}

/// A text content block.
pub(super) fn text(body: String) -> Value {
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
