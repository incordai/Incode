//! Windows: the UI Automation tree of any top-level window, the list of those
//! windows, and acting on a control through its own patterns — invoke,
//! toggle, set a value — rather than through the mouse.
//!
//! A window's id is its handle (HWND) in decimal: stable for the window's
//! life, and what `list_windows` reports.

use std::path::Path;
use std::time::Duration;

use uiautomation::controls::ControlType;
use uiautomation::patterns::{
    UIExpandCollapsePattern, UIInvokePattern, UILegacyIAccessiblePattern, UIScrollItemPattern,
    UISelectionItemPattern, UITogglePattern, UIValuePattern,
};
use uiautomation::types::{ExpandCollapseState, Handle};
use uiautomation::{UIAutomation, UIElement, UITreeWalker};
use windows_sys::Win32::Foundation::{CloseHandle, HWND};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

use super::{find, walk, Done, Element, Role, Tree, Want, WindowInfo};
use crate::desktop::{DesktopError, Rect};

/// The actions [`act`] knows, in the shared vocabulary.
const ACTIONS: &[&str] = &[
    "press",
    "toggle",
    "expand",
    "collapse",
    "select",
    "scroll_into_view",
    "focus",
    "default",
];

fn role_of(control: ControlType) -> Role {
    match control {
        ControlType::Button => Role::Button,
        ControlType::Edit | ControlType::Document => Role::Edit,
        ControlType::Text => Role::Text,
        ControlType::List => Role::List,
        ControlType::ListItem => Role::ListItem,
        ControlType::CheckBox => Role::CheckBox,
        ControlType::RadioButton => Role::RadioButton,
        ControlType::ComboBox => Role::ComboBox,
        ControlType::Menu | ControlType::MenuBar => Role::Menu,
        ControlType::MenuItem => Role::MenuItem,
        ControlType::Tab => Role::Tab,
        ControlType::TabItem => Role::TabItem,
        ControlType::Hyperlink => Role::Link,
        ControlType::Image => Role::Image,
        ControlType::Window => Role::Window,
        ControlType::Pane => Role::Pane,
        ControlType::Tree => Role::Tree,
        ControlType::TreeItem => Role::TreeItem,
        ControlType::Table | ControlType::DataGrid => Role::Table,
        ControlType::Slider => Role::Slider,
        ControlType::Group => Role::Group,
        _ => Role::Other,
    }
}

fn rect_of(element: &UIElement) -> Rect {
    element
        .get_bounding_rectangle()
        .map(|r| Rect::from_edges(r.get_left(), r.get_top(), r.get_right(), r.get_bottom()))
        .unwrap_or(Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        })
}

fn describe(element: &UIElement, id: u32, depth: u32) -> Element {
    Element {
        id,
        depth,
        role: element
            .get_control_type()
            .map(role_of)
            .unwrap_or(Role::Other),
        // The Name of an edit is its label, never its content, so a password
        // field's name is safe to show; the flag says what it is.
        name: element.get_name().unwrap_or_default(),
        rect: rect_of(element),
        enabled: element.is_enabled().unwrap_or(false),
        secret: element.is_password().unwrap_or(false),
    }
}

fn automation() -> Result<UIAutomation, DesktopError> {
    // Awareness first: the rectangles are only physical pixels if the
    // process is per-monitor aware.
    super::super::dpi::ensure_per_monitor_aware();
    UIAutomation::new().map_err(|e| DesktopError::Os(format!("UI Automation is unavailable: {e}")))
}

fn parse_id(window: &str) -> Result<isize, DesktopError> {
    window
        .trim()
        .parse::<isize>()
        .ok()
        .filter(|h| *h != 0)
        .ok_or_else(|| {
            DesktopError::Os(format!(
                "\"{window}\" is not a window id; take one from list_windows"
            ))
        })
}

fn foreground_hwnd() -> isize {
    // SAFETY: an argument-free query of the foreground window.
    unsafe { GetForegroundWindow() as isize }
}

/// The program a process runs, by file stem ("notepad", "1Password").
fn process_name(pid: u32) -> String {
    if pid == 0 {
        return String::new();
    }
    let mut buffer = [0u16; 1024];
    let mut len = buffer.len() as u32;
    // SAFETY: the handle is checked before use and closed exactly once; the
    // buffer and its length are this function's own.
    let ok = unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return String::new();
        }
        let ok =
            QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut len);
        CloseHandle(process);
        ok
    };
    if ok == 0 {
        return String::new();
    }
    let path = String::from_utf16_lossy(&buffer[..len as usize]);
    Path::new(&path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn info_of(element: &UIElement, hwnd: isize) -> WindowInfo {
    let pid = element.get_process_id().unwrap_or(0);
    WindowInfo {
        id: hwnd.to_string(),
        title: element.get_name().unwrap_or_default(),
        app: process_name(pid),
        pid,
        focused: hwnd == foreground_hwnd(),
        rect: rect_of(element),
    }
}

fn window_element(automation: &UIAutomation, hwnd: isize) -> Result<UIElement, DesktopError> {
    automation
        .element_from_handle(Handle::from(hwnd))
        .map_err(|e| {
            DesktopError::Os(format!(
                "window {hwnd} is unavailable to UI Automation ({e}): it has closed, or belongs \
                 to a program running as administrator, which this process cannot read. Take \
                 another from list_windows."
            ))
        })
}

/// The window `window` names, or the foreground one.
fn target(
    automation: &UIAutomation,
    window: Option<&str>,
) -> Result<(UIElement, WindowInfo), DesktopError> {
    let hwnd = match window {
        Some(id) => parse_id(id)?,
        None => match foreground_hwnd() {
            0 => return Err(DesktopError::Os("no window has the focus".to_string())),
            hwnd => hwnd,
        },
    };
    let element = window_element(automation, hwnd)?;
    let info = info_of(&element, hwnd);
    Ok((element, info))
}

fn children_of(walker: &UITreeWalker, element: &UIElement) -> Vec<UIElement> {
    let mut out = Vec::new();
    let Ok(mut child) = walker.get_first_child(element) else {
        return out;
    };
    loop {
        out.push(child.clone());
        match walker.get_next_sibling(&child) {
            Ok(next) => child = next,
            Err(_) => return out,
        }
    }
}

pub(super) fn list_windows() -> Result<Vec<WindowInfo>, DesktopError> {
    let automation = automation()?;
    let root = automation
        .get_root_element()
        .map_err(|e| DesktopError::Os(format!("no desktop element: {e}")))?;
    let walker = automation
        .create_tree_walker()
        .map_err(|e| DesktopError::Os(format!("could not walk the desktop: {e}")))?;
    Ok(children_of(&walker, &root)
        .into_iter()
        .filter_map(|element| {
            let hwnd: isize = element.get_native_window_handle().ok()?.into();
            if hwnd == 0 || element.is_offscreen().unwrap_or(true) {
                return None;
            }
            let info = info_of(&element, hwnd);
            // An untitled window is no target to offer, but a blocked
            // application's still counts: the checks that keep clicks and
            // captures off password managers are made against this list.
            let listed = !info.title.trim().is_empty()
                || super::super::limits::get().blocked(&info.app).is_some();
            (info.rect.width > 0 && info.rect.height > 0 && listed).then_some(info)
        })
        .collect())
}

pub(super) fn foreground() -> Result<Option<WindowInfo>, DesktopError> {
    let hwnd = foreground_hwnd();
    if hwnd == 0 {
        return Ok(None);
    }
    let automation = automation()?;
    let element = window_element(&automation, hwnd)?;
    Ok(Some(info_of(&element, hwnd)))
}

pub(super) fn window_info(window: Option<&str>) -> Result<WindowInfo, DesktopError> {
    let automation = automation()?;
    Ok(target(&automation, window)?.1)
}

pub(super) fn tree(window: Option<&str>, max_depth: u32) -> Result<Tree, DesktopError> {
    let automation = automation()?;
    let (root, info) = target(&automation, window)?;
    let walker = automation
        .create_tree_walker()
        .map_err(|e| DesktopError::Os(format!("could not walk the tree: {e}")))?;
    let (elements, truncated) = walk(&root, max_depth, &describe, &|e: &UIElement| {
        children_of(&walker, e)
    });
    Ok(Tree {
        root_name: info.title,
        window: info.id,
        app: info.app,
        no_positions: None,
        elements,
        truncated,
    })
}

/// The live control with this id, re-found by walking the window again.
fn control(
    window: Option<&str>,
    want: &Want,
    max_depth: u32,
) -> Result<(UIElement, Element, WindowInfo), DesktopError> {
    let automation = automation()?;
    let (root, info) = target(&automation, window)?;
    let walker = automation
        .create_tree_walker()
        .map_err(|e| DesktopError::Os(format!("could not walk the tree: {e}")))?;
    let (element, described) = find(
        &root,
        max_depth,
        &describe,
        &|e: &UIElement| children_of(&walker, e),
        want,
    )
    .ok_or_else(|| super::gone(want.id, &info))?;
    Ok((element, described, info))
}

pub(super) fn set_value(
    window: Option<&str>,
    want: &Want,
    max_depth: u32,
    value: &str,
) -> Result<Done, DesktopError> {
    let (element, described, _) = control(window, want, max_depth)?;
    let what = described.label();
    if let Ok(pattern) = element.get_pattern::<UIValuePattern>() {
        pattern
            .set_value(value)
            .map_err(|e| DesktopError::Os(format!("could not set {what}: {e}")))?;
        if described.secret {
            return Ok(Done::unverified(
                format!("set {what}"),
                "a password field is never read back",
            ));
        }
        return Ok(match pattern.get_value() {
            Ok(now) if now == value => Done::verified(format!("set {what}")),
            Ok(now) => Done::unverified(
                format!("set {what}"),
                &format!(
                    "it reads back as {} characters, not the {} sent",
                    now.chars().count(),
                    value.chars().count()
                ),
            ),
            Err(_) => Done::unverified(format!("set {what}"), "its value could not be read back"),
        });
    }
    if let Ok(pattern) = element.get_pattern::<UILegacyIAccessiblePattern>() {
        if pattern.set_value(value).is_ok() {
            return Ok(Done::unverified(
                format!("set {what}"),
                "set through the legacy accessibility interface, which cannot be read back",
            ));
        }
    }
    Err(DesktopError::Os(format!(
        "{what} has no value to set; click_element it and use type_text instead"
    )))
}

/// The actions this control supports, in the shared vocabulary.
fn supported(element: &UIElement) -> Vec<&'static str> {
    let mut out = Vec::new();
    if element.get_pattern::<UIInvokePattern>().is_ok() {
        out.push("press");
    }
    if element.get_pattern::<UITogglePattern>().is_ok() {
        out.push("toggle");
    }
    if element.get_pattern::<UIExpandCollapsePattern>().is_ok() {
        out.extend(["expand", "collapse"]);
    }
    if element.get_pattern::<UISelectionItemPattern>().is_ok() {
        out.push("select");
    }
    if element.get_pattern::<UIScrollItemPattern>().is_ok() {
        out.push("scroll_into_view");
    }
    if element.is_keyboard_focusable().unwrap_or(false) {
        out.push("focus");
    }
    if element.get_pattern::<UILegacyIAccessiblePattern>().is_ok() {
        out.push("default");
    }
    out
}

pub(super) fn act(
    window: Option<&str>,
    want: &Want,
    max_depth: u32,
    action: &str,
) -> Result<Done, DesktopError> {
    let (element, described, _) = control(window, want, max_depth)?;
    let what = described.label();
    let action = action.trim().to_ascii_lowercase();
    let failed =
        |e: uiautomation::Error| DesktopError::Os(format!("could not {action} {what}: {e}"));
    let unsupported = || {
        DesktopError::Os(format!(
            "{what} does not support \"{action}\"; it supports: {}",
            supported(&element).join(", ")
        ))
    };
    match action.as_str() {
        "press" | "invoke" | "click" => {
            let pattern = element
                .get_pattern::<UIInvokePattern>()
                .map_err(|_| unsupported())?;
            pattern.invoke().map_err(failed)?;
            Ok(Done::unverified(
                format!("pressed {what}"),
                "a press has no state to read back; look at the result",
            ))
        }
        "toggle" => {
            let pattern = element
                .get_pattern::<UITogglePattern>()
                .map_err(|_| unsupported())?;
            let before = pattern.get_toggle_state().ok();
            pattern.toggle().map_err(failed)?;
            let after = pattern.get_toggle_state().ok();
            Ok(match (before, after) {
                (Some(b), Some(a)) if a != b => {
                    Done::verified(format!("toggled {what} from {b:?} to {a:?}"))
                }
                _ => Done::unverified(format!("toggled {what}"), "its state did not change"),
            })
        }
        "expand" | "collapse" => {
            let pattern = element
                .get_pattern::<UIExpandCollapsePattern>()
                .map_err(|_| unsupported())?;
            let want = if action == "expand" {
                pattern.expand().map_err(failed)?;
                ExpandCollapseState::Expanded
            } else {
                pattern.collapse().map_err(failed)?;
                ExpandCollapseState::Collapsed
            };
            Ok(match pattern.get_state() {
                Ok(state) if state == want => Done::verified(format!("{action}ed {what}")),
                Ok(state) => Done::unverified(
                    format!("{action}ed {what}"),
                    &format!("it reports {state:?}"),
                ),
                Err(_) => Done::unverified(format!("{action}ed {what}"), "no state to read back"),
            })
        }
        "select" => {
            let pattern = element
                .get_pattern::<UISelectionItemPattern>()
                .map_err(|_| unsupported())?;
            pattern.select().map_err(failed)?;
            Ok(match pattern.is_selected() {
                Ok(true) => Done::verified(format!("selected {what}")),
                _ => Done::unverified(format!("selected {what}"), "it does not report selected"),
            })
        }
        "scroll_into_view" => {
            let pattern = element
                .get_pattern::<UIScrollItemPattern>()
                .map_err(|_| unsupported())?;
            pattern.scroll_into_view().map_err(failed)?;
            Ok(Done::unverified(
                format!("scrolled {what} into view"),
                "call ui_tree to see where it is now",
            ))
        }
        "focus" => {
            element.set_focus().map_err(failed)?;
            Ok(match element.has_keyboard_focus() {
                Ok(true) => Done::verified(format!("focused {what}")),
                _ => Done::unverified(format!("focused {what}"), "it does not report focus"),
            })
        }
        "default" => {
            let pattern = element
                .get_pattern::<UILegacyIAccessiblePattern>()
                .map_err(|_| unsupported())?;
            pattern.do_default_action().map_err(failed)?;
            Ok(Done::unverified(
                format!("ran {what}'s default action"),
                "a default action has no state to read back",
            ))
        }
        other => Err(DesktopError::Os(format!(
            "unknown action \"{other}\"; one of: {}",
            ACTIONS.join(", ")
        ))),
    }
}

pub(super) fn focus_window(window: &str) -> Result<Done, DesktopError> {
    let hwnd = parse_id(window)?;
    let automation = automation()?;
    let element = window_element(&automation, hwnd)?;
    let title = element.get_name().unwrap_or_default();
    // SAFETY: a window handle the caller named and UI Automation resolved;
    // both calls only change which window is shown in front.
    unsafe {
        if IsIconic(hwnd as HWND) != 0 {
            ShowWindow(hwnd as HWND, SW_RESTORE);
        }
    }
    let _ = element.set_focus();
    if foreground_hwnd() != hwnd {
        // SAFETY: as above.
        unsafe { SetForegroundWindow(hwnd as HWND) };
    }
    std::thread::sleep(Duration::from_millis(150));
    Ok(if foreground_hwnd() == hwnd {
        Done::verified(format!("focused window \"{title}\""))
    } else {
        Done::unverified(
            format!("asked for window \"{title}\" to come to the front"),
            "Windows kept another window in front (its foreground lock); click the window \
             instead",
        )
    })
}
