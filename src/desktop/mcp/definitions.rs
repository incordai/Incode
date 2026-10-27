//! Every desktop tool as MCP describes it to a client: name, description and
//! input schema, with the `window` and `screenshot_after` arguments added to
//! the tools that take them.
//!
//! # Why this is a file of its own
//!
//! Root cause: `desktop/mcp.rs` was 2,375 lines, over the code auditor's
//! 1,000-line file threshold, because it mixed the JSON-RPC transport, these
//! definitions, the per-call gates, the tools and the screen guards. The
//! definitions are what the model reads, nearly 300 lines of descriptions
//! that change for a different reason than any code does. Moved unchanged:
//! every name, description and schema is as it was.
//!
//! # Wired to
//!
//! - **Called by** `tools/list` in [`super::transport`], and by the toolbox
//!   (`toolbox::desktop`) through [`tool_definitions`], which [`super`]
//!   re-exports as `desktop::mcp::tool_definitions`.
//! - **Calls** nothing; reads [`super::ACTING`] and [`super::UNTRUSTED`].

use serde_json::{json, Value};

use super::{ACTING, UNTRUSTED};

/// Every desktop tool, in MCP's shape. The toolbox offers these same
/// definitions to a model (`toolbox::desktop`), so they are written once.
pub(crate) fn tool_definitions() -> Vec<Value> {
    /// The tools that take a `window` from list_windows.
    const WINDOWED: &[&str] = &[
        "screenshot",
        "ui_tree",
        "click_element",
        "set_value",
        "element_action",
        "focus_window",
        "type_text",
        "paste_text",
        "key",
        "scroll",
        "drag",
    ];
    let mut tools = base_definitions();
    for tool in &mut tools {
        let name = tool["name"].as_str().unwrap_or_default().to_string();
        let properties = &mut tool["inputSchema"]["properties"];
        if WINDOWED.contains(&name.as_str()) {
            let what = match name.as_str() {
                "type_text" | "paste_text" | "key" => {
                    "The window this input is meant for, from list_windows. If it is not the \
                     focused window the call is REFUSED rather than typing somewhere else."
                }
                "screenshot" => "Capture only this window, from list_windows.",
                _ => "The window, from list_windows. Default: the focused window.",
            };
            properties["window"] = json!({"type": "string", "description": what});
        }
        if ACTING.contains(&name.as_str()) {
            properties["screenshot_after"] = json!({
                "type": "boolean",
                "description": "Also return a screenshot of the window afterwards, to check \
                                the result. Default false."
            });
        }
    }
    tools
}

/// The definitions before the shared arguments are added. Called by
/// [`tool_definitions`].
fn base_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "list_windows",
            "description": format!(
                "Every top-level window: id, title, app, pid, whether it is focused, its \
                 rectangle, and `tree` — false when the window exposes no accessibility tree, \
                 so ui_tree has nothing to show and screenshot + click are the way in. Pass an \
                 id as `window` to ui_tree, screenshot and the actions to work on that window \
                 rather than whichever has focus. Windows of blocked \
                 applications (password managers) are listed as blocked, without a title. \
                 {UNTRUSTED}"
            ),
            "inputSchema": {"type": "object", "properties": {}},
        }),
        json!({
            "name": "focus_window",
            "description": "Bring a window to the front so keystrokes go to it. Reports \
                            whether it actually came to the front.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": ["window"],
            },
        }),
        json!({
            "name": "set_value",
            "description": "Set a control's value (a text field, a slider) directly through \
                            the accessibility interface, by the id ui_tree gave it — no \
                            typing, no focus needed, no clipboard. PREFER THIS over \
                            type_text for a field. The value is read back where allowed and \
                            the result says whether it matched (a password field is never \
                            read back).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "The eN number from ui_tree."},
                    "value": {"type": "string"}
                },
                "required": ["id", "value"],
            },
        }),
        json!({
            "name": "element_action",
            "description": "Run a control's own action by the id ui_tree gave it, without \
                            the mouse: the cursor does not move and no other window gets \
                            the click. Actions: press, toggle, expand, collapse, select, \
                            scroll_into_view, focus, default (macOS also show_menu, \
                            confirm, increment, decrement, cancel, raise). An action the \
                            control does not have is refused with the ones it does.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "The eN number from ui_tree."},
                    "action": {"type": "string"}
                },
                "required": ["id", "action"],
            },
        }),
        json!({
            "name": "screen_info",
            "description": "Describe every display: id, position, size in physical pixels, \
                            DPI, scale and which is primary. Call this before naming any \
                            coordinate. Also reports whether the process is DPI-aware — if \
                            it is not, the geometry is virtualised and coordinates are wrong. \
                            On Linux, `display` names the X display this server drives: \
                            start an application there (`DISPLAY=<display>`, with \
                            WAYLAND_DISPLAY unset) for it to be seen and driven.",
            "inputSchema": {"type": "object", "properties": {}},
        }),
        json!({
            "name": "screenshot",
            "description": format!(
                "Capture the primary display, or one `window`, downscaled so the longest edge \
                 fits `max_edge`. Returns the image AND the size it was sent at, its origin \
                 and the region it covers: physical = origin + image point * region / size. \
                 A full capture is refused while a blocked application's window is on \
                 screen. {UNTRUSTED}"
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "max_edge": {
                        "type": "integer",
                        "description": "Longest edge of the returned image. Default 1092."
                    },
                    "format": {
                        "type": "string",
                        "enum": ["png", "jpeg"],
                        "description": "png for UI (default), jpeg for photographic content."
                    }
                }
            },
        }),
        json!({
            "name": "ui_tree",
            "description": format!(
                "A window's controls (the focused one, or `window` from list_windows), with \
                 names, roles and exact rectangles; password fields are marked and never \
                 read. PREFER THIS OVER screenshot: clicking by element id hits the true centre \
                 of a named control, where a coordinate read off a downscaled image is a \
                 guess that can miss silently. Falls back to screenshot for canvas, images \
                 and custom-drawn UI, which expose no tree. {UNTRUSTED}"
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "depth": {
                        "type": "integer",
                        "description": "How deep to walk. Default 12. Deeper on a browser \
                                        returns a great deal and may be truncated."
                    }
                }
            },
        }),
        json!({
            "name": "click_element",
            "description": "Click a control by the id ui_tree gave it. The element is \
                            looked up again first, so a control that moved or vanished is \
                            reported instead of clicked at its old position. Deterministic \
                            where click(x,y) is a guess.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The eN number from ui_tree, without the 'e'."
                    },
                    "button": {"type": "string", "enum": ["left", "right", "middle"]}
                },
                "required": ["id"],
            },
        }),
        json!({
            "name": "type_text",
            "description": "Type text as keystrokes into whatever has focus. SLOW, and some \
                            applications drop characters from a fast synthetic stream — use \
                            paste_text for anything long or non-ASCII. Click or click_element \
                            the field first; this types wherever focus already is.",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
            },
        }),
        json!({
            "name": "paste_text",
            "description": "Put text on the clipboard and paste it with Ctrl+V (Cmd+V on \
                            macOS). Much more reliable than type_text for long or non-ASCII \
                            text, and near instant. The clipboard's previous text is put \
                            back afterwards.",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
            },
        }),
        json!({
            "name": "key",
            "description": "Press keys together, released in reverse — [\"control\",\"c\"] is \
                            Ctrl+C. Names: control shift alt meta enter tab escape backspace \
                            delete space up down left right home end pageup pagedown f1-f12, \
                            or any single character. An unknown name is refused before \
                            anything is pressed, so a modifier can never be left stuck.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "keys": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["keys"],
            },
        }),
        json!({
            "name": "scroll",
            "description": "Scroll. Positive dy scrolls down, positive dx right. Over the \
                            element `id` from ui_tree when given; otherwise at the cursor.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dx": {"type": "integer"},
                    "dy": {"type": "integer"},
                    "id": {"type": "integer", "description": "Scroll over this element."}
                },
            },
        }),
        json!({
            "name": "drag",
            "description": "Press at one point, move in steps, release at another. Stepped \
                            rather than teleported, because many drag targets ignore a single \
                            jump. Either from_id and to_id (elements from ui_tree, dragged \
                            centre to centre) or the four coordinates in PHYSICAL pixels.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from_id": {"type": "integer"},
                    "to_id": {"type": "integer"},
                    "from_x": {"type": "integer"},
                    "from_y": {"type": "integer"},
                    "to_x": {"type": "integer"},
                    "to_y": {"type": "integer"},
                    "button": {"type": "string", "enum": ["left", "right", "middle"]}
                },
            },
        }),
        json!({
            "name": "cursor_position",
            "description": "Where the mouse cursor is now, in physical pixels.",
            "inputSchema": {"type": "object", "properties": {}},
        }),
        json!({
            "name": "move_mouse",
            "description": "Move the cursor to a point in PHYSICAL pixels. If you read the \
                            coordinate off a screenshot, convert it first — a screenshot is \
                            usually downscaled.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": {"type": "integer"},
                    "y": {"type": "integer"}
                },
                "required": ["x", "y"],
            },
        }),
        json!({
            "name": "click",
            "description": "Move to a point in PHYSICAL pixels and click there. Moving and \
                            clicking are one action so nothing can interleave between them.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": {"type": "integer"},
                    "y": {"type": "integer"},
                    "button": {"type": "string", "enum": ["left", "right", "middle"]},
                    "count": {
                        "type": "integer",
                        "description": "1 single, 2 double, 3 triple. Default 1."
                    }
                },
                "required": ["x", "y"],
            },
        }),
    ]
}
