//! What a client is told about `watch` and the five routine tools.
//!
//! Kept apart from [`super::definitions`] for the reason that file is apart
//! from the transport: these are paragraphs a model reads, and they change for a
//! different reason than the code does. [`super::definitions::tool_definitions`]
//! appends [`definitions`] to the base list and adds `window` and
//! `screenshot_after` the same way it does for every other tool.
//!
//! # Wired to
//!
//! - **Called by** [`super::definitions::tool_definitions`].
//! - **Calls** nothing; reads [`super::UNTRUSTED`] and [`crate::wait`]'s limits.

use serde_json::{json, Value};

use super::UNTRUSTED;

/// `watch` and the routine tools, in MCP's shape.
pub(super) fn definitions() -> Vec<Value> {
    vec![
        watch_definition(),
        routine_save_definition(),
        routine_run_definition(),
        routine_forget_definition(),
        routine_list_definition(),
        routine_match_definition(),
    ]
}

/// The `phrase` argument shared by four of the tools.
fn phrase_property(what: &str) -> Value {
    json!({"type": "string", "description": what})
}

fn watch_definition() -> Value {
    json!({
        "name": "watch",
        "description": format!(
            "Watch for something to happen and tell the operator when it does, with no \
             model polling: the same conditions as wait_for (element_appears, \
             element_gone, window_appears, window_gone, screen_changes) looked for every \
             second (every {screen_poll} ms for screen_changes, which captures the whole \
             screen each look) for up to {max_hours} hours, then the operator's post-tool \
             hook for `watch` is run with the reply as its `tool_response`, which is how \
             it notifies (a desktop notification, a message, a follow-up command: \
             whatever the operator wrote). THIS CALL BLOCKS until the condition holds, \
             the time runs out, the operator stops the desktop (the stop file ends it \
             within a second) or the client disconnects, and your turn waits with it. \
             IT ALSO HOLDS THIS SERVER: while it runs no other desktop call is answered, \
             including yours. Use it only when waiting is the task. For a few seconds \
             use wait_for. `timeout_ms` defaults to {default} \
             ms (one hour) and is held to {max} ms; `poll_ms` {min_poll} to {max_poll}. \
             `message` is what the notification should say; it is returned in the reply. \
             The reply is JSON whose `result` is `found` or `timeout`, as wait_for's. \
             {UNTRUSTED}",
            max_hours = crate::wait::MAX_WATCH_MS / 3_600_000,
            screen_poll = crate::wait::DEFAULT_WATCH_SCREEN_POLL_MS,
            default = crate::wait::DEFAULT_WATCH_MS,
            max = crate::wait::MAX_WATCH_MS,
            min_poll = crate::wait::MIN_WATCH_POLL_MS,
            max_poll = crate::wait::MAX_WATCH_POLL_MS,
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "condition": {
                    "type": "string",
                    "enum": [
                        "element_appears", "element_gone", "window_appears", "window_gone",
                        "screen_changes"
                    ],
                },
                "selector": {
                    "type": "string",
                    "description": "key=value pairs joined by ';' — control: role, name \
                                    (name~ for contains), aid, path, nth; window: app, \
                                    window (window~ for contains). E.g. \
                                    role=button;name=Save or app=notepad;window~=Save As. \
                                    For element_* conditions it must name a control; for \
                                    window_* only app and/or window.",
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": format!(
                        "Longest to watch. Default {}, at most {}.",
                        crate::wait::DEFAULT_WATCH_MS,
                        crate::wait::MAX_WATCH_MS
                    ),
                },
                "poll_ms": {
                    "type": "integer",
                    "description": format!(
                        "How often to look. Default {} ({} for screen_changes), from {} to {}.",
                        crate::wait::DEFAULT_WATCH_POLL_MS,
                        crate::wait::DEFAULT_WATCH_SCREEN_POLL_MS,
                        crate::wait::MIN_WATCH_POLL_MS,
                        crate::wait::MAX_WATCH_POLL_MS
                    ),
                },
                "baseline": {
                    "type": "string",
                    "description": "screen_changes: the `hash` of an earlier reply to compare \
                                    with. Default: the screen as the watch begins.",
                },
                "tolerance": {
                    "type": "integer",
                    "description": format!(
                        "screen_changes: how many of the 128 bits may differ. Default {}.",
                        crate::wait::DEFAULT_TOLERANCE
                    ),
                },
                "message": {
                    "type": "string",
                    "description": "What the notification should say, up to 200 characters. \
                                    Returned in the reply, so the hook can show it.",
                },
            },
            "required": ["condition"],
        },
    })
}

fn routine_save_definition() -> Value {
    json!({
        "name": "routine_save",
        "description": format!(
            "Keep what was just done at the desktop as a routine that `routine_run` can \
             do again later with no model. Give the `phrase` that asks for it and EITHER \
             `records` (Watchman record ids of the turns that did the task, in any \
             order; needs the local memory engine) OR `steps` (a list of {{tool, args}} \
             you write). The looking calls (ui_tree, screenshot, list_windows, \
             screen_info, cursor_position) are dropped and wait_for is kept. NOTHING IS \
             KEPT, and the reason is given, when: the phrase says \"this\" or \"that\"; \
             ANY call or span of the records failed (a shell command too, not only a \
             desktop call), or the record itself says its run failed, or a wait_for \
             timed out; or any step uses an eN id, a pixel \
             coordinate (click, move_mouse, drag) or a window handle (`window`, \
             focus_window), because those hold for one screen only — name controls with \
             a `selector` and windows inside it (app=...;window=...). Every acting step \
             must say WHERE: a selector needs an app= or window= part, and a typing or \
             key step needs `in_window` (e.g. app=notepad;window~=Untitled; a recording \
             is given the window of the step before it, and the replay refuses to type \
             unless that window has focus); scroll is never kept. A replay invokes links \
             and buttons without the mouse and stops when a set_value (or an \
             increment/decrement) answers \"unverified\" and the next step depends on \
             it; a button press, which always says \"unverified\", never stops it. \
             Saving under a phrase that means the same as a saved one (same words, any \
             order, filler words ignored; but the words on each side of to, from, into, \
             onto, over, under, before or after stay on that side) REPLACES it: that is \
             how a routine that failed is redone. TYPED TEXT IS STORED AS IT IS in the \
             routines file, and the reply shows every step's full text so you and the \
             person can read it, EXCEPT text typed into a password or other secure \
             field, and text a recording kept only as a placeholder or a digest: those \
             steps are stored as a parameter, `{{{{name}}}}`, listed in the reply, and \
             `routine_run` must be given every one as `params` or it refuses before \
             step 1. You may write `{{{{name}}}}` in a typed text yourself to make \
             one. A routine unused for 30 days is forgotten; only the person can \
             keep one for good (`incu routine keep`), so there is no argument for it. \
             {UNTRUSTED}"
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "phrase": phrase_property(
                    "The words that ask for this routine, e.g. \"open the weekly report\". \
                     Not \"this\" or \"that\"."
                ),
                "records": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Watchman record ids (w-...) of the turns that did the \
                                    task. Their desktop calls are joined in the order they \
                                    happened.",
                },
                "steps": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "tool": {"type": "string"},
                            "args": {"type": "object"}
                        },
                        "required": ["tool"],
                    },
                    "description": "The steps themselves, instead of records.",
                },
            },
            "required": ["phrase"],
        },
    })
}

fn routine_run_definition() -> Value {
    json!({
        "name": "routine_run",
        "description": format!(
            "Do a saved routine again, on the real desktop, now: the phrase is matched by \
             its words (any order, filler ignored; see routine_match). Every step runs \
             through the same checks as a call you make yourself (blocked applications, \
             the operator's hooks, the stop file, the journal), in order, and stops at the \
             first one that fails. There is NO repair: on a failure you are told which \
             step and why, and what was already done stays done. Then do the whole task \
             yourself, from the screen as it is, and routine_save it under the same \
             phrase to replace the routine (or routine_forget it). A picture of the \
             window each step is for (else the screen) is taken before and after the \
             step, kept for the last run only, and the paths of the failed step's two \
             pictures are in the failure. A routine whose list shows `params_needed` \
             needs every one of them in `params` (secrets it does not keep): without \
             them it refuses before step 1. A value is used in memory for that run \
             only. The routine moves the mouse and keyboard of the machine the \
             operator is using. {UNTRUSTED}"
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "phrase": phrase_property("The words that ask for the routine."),
                "params": {
                    "type": "object",
                    "additionalProperties": {"type": "string"},
                    "description": "Values for the routine's parameters, by name, e.g. \
                                    {\"password\": \"...\"}: every name in the routine's \
                                    `params_needed` and no other.",
                },
            },
            "required": ["phrase"],
        },
    })
}

fn routine_forget_definition() -> Value {
    json!({
        "name": "routine_forget",
        "description": "Delete the saved routine a phrase asks for. Use it for a routine \
                        that no longer does the right thing.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "phrase": phrase_property("The words that ask for the routine."),
            },
            "required": ["phrase"],
        },
    })
}

fn routine_list_definition() -> Value {
    json!({
        "name": "routine_list",
        "description": "Every saved routine that has not expired: its phrase, each step \
                        with ALL the text it types and its full selector (read it: it is \
                        what will run), the parameters it needs (`params_needed`), when \
                        it was learned and last used, whether the person kept it (never \
                        expires), and how many days it has left.",
        "inputSchema": {"type": "object", "properties": {}},
    })
}

fn routine_match_definition() -> Value {
    json!({
        "name": "routine_match",
        "description": "Which saved routine a phrase would run, without running it: the \
                        routine and its steps (all the text each types, in full), or that \
                        none matches. A phrase matches a routine when it has the same \
                        words in any order, ignoring filler words (a, the, please, ...); \
                        the words on each side of to, from, into, onto, over, under, \
                        before or after must stay on that side (copy a to b is not copy \
                        b to a); a near miss does not match. Call this before \
                        routine_run to see what it would do.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "phrase": phrase_property("The words to match."),
            },
            "required": ["phrase"],
        },
    })
}
