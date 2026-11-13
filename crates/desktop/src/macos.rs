//! macOS: the two permissions a person grants, the accessibility tree, and
//! the clipboard.
//!
//! # Why the permissions are checked before every action
//!
//! macOS gates input and the accessibility tree behind the Accessibility
//! grant, and capture behind Screen Recording (`docs/COMPUTER_USE.md` 13a).
//! Without them nothing errors: a click is silently dropped and a screenshot
//! comes back as wallpaper. That looks exactly like a broken implementation,
//! so each action asks first and, when the grant is missing, refuses with the
//! grant's name and where to give it — the failure names its own cause.
//!
//! # No new package
//!
//! Plain C calls into the system frameworks (ApplicationServices,
//! CoreGraphics, CoreFoundation), declared here, and `pbcopy` for the
//! clipboard. Every reference this module owns is released when its
//! [`Owned`] drops.

use std::ffi::{c_char, c_void};

use super::uia::{find, role_of_ax, walk, Done, Element, Tree, Want, WindowInfo};
use super::{DesktopError, Rect};

type CFTypeRef = *const c_void;
type CFIndex = isize;
type CFTypeId = usize;
type AxError = i32;
type Boolean = u8;

const AX_SUCCESS: AxError = 0;
const AX_VALUE_CG_POINT: u32 = 1;
const AX_VALUE_CG_SIZE: u32 = 2;
const UTF8: u32 = 0x0800_0100;

// How long one accessibility request may take is admin.json
// `desktop_limits.accessibility_call_timeout_ms` (default 1 s): an
// application that has stopped answering would otherwise hold the walk — and
// the agent — for the system default of several seconds per element.

#[repr(C)]
#[derive(Default)]
struct CgPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Default)]
struct CgSize {
    width: f64,
    height: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> Boolean;
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        attribute: CFTypeRef,
        value: *mut CFTypeRef,
    ) -> AxError;
    fn AXUIElementSetMessagingTimeout(element: CFTypeRef, seconds: f32) -> AxError;
    fn AXValueGetValue(value: CFTypeRef, kind: u32, out: *mut c_void) -> Boolean;
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementGetPid(element: CFTypeRef, pid: *mut i32) -> AxError;
    fn AXUIElementSetAttributeValue(
        element: CFTypeRef,
        attribute: CFTypeRef,
        value: CFTypeRef,
    ) -> AxError;
    fn AXUIElementPerformAction(element: CFTypeRef, action: CFTypeRef) -> AxError;
    fn AXUIElementCopyActionNames(element: CFTypeRef, names: *mut CFTypeRef) -> AxError;
    // Not in the public headers, but exported by HIServices since 10.x and
    // the one link from an accessibility window to the window server's own
    // number for it — the identity a window keeps while others open, close
    // and reorder around it.
    fn _AXUIElementGetWindow(element: CFTypeRef, window: *mut u32) -> AxError;
}

/// `kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements`.
const CG_ON_SCREEN_WITHOUT_DESKTOP: u32 = 1 | 16;
/// `kCFNumberSInt64Type`.
const CF_NUMBER_SINT64: CFIndex = 4;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFTypeRef;
}

#[link(name = "Security", kind = "framework")]
extern "C" {
    fn SessionGetInfo(session: i32, session_id: *mut u32, attributes: *mut u32) -> i32;
}

/// `callerSecuritySession`: the session this process runs in.
const CALLER_SECURITY_SESSION: i32 = -1;
/// `sessionHasGraphicAccess` in `SessionAttributeBits`.
const SESSION_HAS_GRAPHIC_ACCESS: u32 = 0x0010;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFGetTypeID(cf: CFTypeRef) -> CFTypeId;
    fn CFStringGetTypeID() -> CFTypeId;
    fn CFBooleanGetTypeID() -> CFTypeId;
    fn CFArrayGetTypeID() -> CFTypeId;
    fn CFBooleanGetValue(boolean: CFTypeRef) -> Boolean;
    fn CFArrayGetCount(array: CFTypeRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, index: CFIndex) -> CFTypeRef;
    fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        len: CFIndex,
        encoding: u32,
        external: Boolean,
    ) -> CFTypeRef;
    static kCFBooleanTrue: CFTypeRef;
    fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> Boolean;
    fn CFDictionaryGetValue(dictionary: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFDictionaryGetTypeID() -> CFTypeId;
    fn CFNumberGetTypeID() -> CFTypeId;
    fn CFNumberGetValue(number: CFTypeRef, kind: CFIndex, out: *mut c_void) -> Boolean;
    fn CFStringGetLength(string: CFTypeRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(len: CFIndex, encoding: u32) -> CFIndex;
    fn CFStringGetCString(
        string: CFTypeRef,
        buffer: *mut c_char,
        size: CFIndex,
        encoding: u32,
    ) -> Boolean;
}

/// A CoreFoundation reference this module owns, released on drop.
pub(crate) struct Owned(CFTypeRef);

impl Owned {
    /// Take ownership of a reference returned by a Create or Copy call.
    fn take(raw: CFTypeRef) -> Option<Self> {
        (!raw.is_null()).then_some(Owned(raw))
    }

    /// Own a reference borrowed from a container, by retaining it.
    fn retain(raw: CFTypeRef) -> Option<Self> {
        if raw.is_null() {
            return None;
        }
        // SAFETY: `raw` is a live CF object borrowed from a container this
        // module still owns; retaining it keeps it alive past that container.
        Some(Owned(unsafe { CFRetain(raw) }))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: constructed only from a non-null reference this module owns
        // (a Create/Copy result or a retain), released exactly once here.
        unsafe { CFRelease(self.0) }
    }
}

impl Clone for Owned {
    fn clone(&self) -> Self {
        // SAFETY: `self.0` is a live reference this value owns; the retain
        // gives the clone its own, released by its own drop.
        Owned(unsafe { CFRetain(self.0) })
    }
}

/// A permission macOS grants per program, by a person, in System Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Grant {
    /// Input and the accessibility tree.
    Accessibility,
    /// Capture.
    ScreenRecording,
}

impl Grant {
    fn name(self) -> &'static str {
        match self {
            Grant::Accessibility => "Accessibility",
            Grant::ScreenRecording => "Screen Recording",
        }
    }

    fn granted(self) -> bool {
        // SAFETY: both are argument-free queries of this process's own TCC
        // state; neither prompts nor changes anything.
        unsafe {
            match self {
                Grant::Accessibility => AXIsProcessTrusted() != 0,
                Grant::ScreenRecording => CGPreflightScreenCaptureAccess(),
            }
        }
    }
}

/// The grants this program does not have, by name, for `screen_info`.
pub(crate) fn missing_grants() -> Vec<String> {
    [Grant::Accessibility, Grant::ScreenRecording]
        .into_iter()
        .filter(|g| !g.granted())
        .map(|g| g.name().to_string())
        .collect()
}

/// Whether this process runs in a session that can reach the window server.
///
/// Measured on 26.6.2: a process started over SSH (attributes 0x5020) passes
/// the Accessibility check, yet every event it posts is dropped without an
/// error — `move_mouse` answered "moved" and the cursor stayed put — while
/// the logged-in user's session (0x6030) carries `sessionHasGraphicAccess`.
fn has_graphic_session() -> bool {
    let (mut id, mut attributes) = (0u32, 0u32);
    // SAFETY: two out-pointers to locals; the call only reads this process's
    // own session.
    let status = unsafe { SessionGetInfo(CALLER_SECURITY_SESSION, &mut id, &mut attributes) };
    status == 0 && attributes & SESSION_HAS_GRAPHIC_ACCESS != 0
}

/// Refuse when this process cannot reach the window server, so a dropped
/// action is never reported as done.
pub(crate) fn require_graphic_session() -> Result<(), DesktopError> {
    if has_graphic_session() {
        return Ok(());
    }
    Err(DesktopError::Os(
        "this process is not in a logged-in graphical session (it was started over SSH, or \
         as a background service), so macOS would drop every click and keystroke without an \
         error. Start it inside the user's session: from Terminal on the Mac, or as a \
         LaunchAgent in gui/<uid>."
            .to_string(),
    ))
}

/// Refuse, naming the grant and where to give it, when it is missing.
pub(crate) fn require(grant: Grant) -> Result<(), DesktopError> {
    if grant.granted() {
        return Ok(());
    }
    Err(DesktopError::Os(refusal(grant, &program_path())))
}

fn program_path() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "this program".to_string())
}

/// What a missing grant says. Pure, so its wording is tested anywhere.
pub(crate) fn refusal(grant: Grant, program: &str) -> String {
    let name = grant.name();
    format!(
        "macOS has not granted {name} to this program ({program}), so the action was not \
         attempted — without it macOS drops the action silently. A person grants it once: \
         System Settings > Privacy & Security > {name}, add the program (or, when it was \
         started from a terminal, that terminal app), then restart it. A rebuilt, unsigned \
         binary can lose the grant and need it again."
    )
}

/// A CoreFoundation string for `text`, owned.
fn cf_string(text: &str) -> Option<Owned> {
    // SAFETY: the bytes are valid UTF-8 of the given length, copied by
    // CoreFoundation (external = false); the result is owned by the caller.
    Owned::take(unsafe {
        CFStringCreateWithBytes(
            std::ptr::null(),
            text.as_ptr(),
            text.len() as CFIndex,
            UTF8,
            0,
        )
    })
}

/// The text of a CoreFoundation string, or `None` for anything else.
fn rust_string(value: &Owned) -> Option<String> {
    // SAFETY: type checked before any string call; the buffer is sized by
    // CoreFoundation's own maximum for this length, plus the terminator.
    unsafe {
        if CFGetTypeID(value.0) != CFStringGetTypeID() {
            return None;
        }
        let size = CFStringGetMaximumSizeForEncoding(CFStringGetLength(value.0), UTF8) + 1;
        let mut buffer = vec![0 as c_char; usize::try_from(size).ok()?];
        if CFStringGetCString(value.0, buffer.as_mut_ptr(), size, UTF8) == 0 {
            return None;
        }
        let bytes: Vec<u8> = buffer
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// One attribute of an accessibility element, owned, when it has it.
fn attribute(element: &Owned, name: &str) -> Option<Owned> {
    let key = cf_string(name)?;
    let mut value: CFTypeRef = std::ptr::null();
    // SAFETY: `element` and `key` are live, owned references; on success the
    // copied value is owned by the caller and taken into `Owned`.
    let error = unsafe { AXUIElementCopyAttributeValue(element.0, key.0, &mut value) };
    if error != AX_SUCCESS {
        return None;
    }
    Owned::take(value)
}

fn text_attribute(element: &Owned, name: &str) -> Option<String> {
    attribute(element, name)
        .as_ref()
        .and_then(rust_string)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn bool_attribute(element: &Owned, name: &str) -> Option<bool> {
    let value = attribute(element, name)?;
    // SAFETY: type checked before the value is read.
    unsafe {
        (CFGetTypeID(value.0) == CFBooleanGetTypeID()).then(|| CFBooleanGetValue(value.0) != 0)
    }
}

/// An `AXValue` of a C struct kind (a point or a size).
fn struct_attribute<T: Default>(element: &Owned, name: &str, kind: u32) -> Option<T> {
    let value = attribute(element, name)?;
    let mut out = T::default();
    // SAFETY: `kind` names the struct `T` is declared as (CGPoint / CGSize,
    // both `#[repr(C)]` pairs of f64); AXValueGetValue returns false rather
    // than writing when the value is of another kind.
    let ok = unsafe { AXValueGetValue(value.0, kind, (&mut out as *mut T).cast::<c_void>()) };
    (ok != 0).then_some(out)
}

/// One element, in the shared vocabulary. Its rectangle is in global points,
/// top-left origin — the space mouse events use.
fn describe(element: &Owned, id: u32, depth: u32) -> Element {
    let role = text_attribute(element, "AXRole").unwrap_or_default();
    let subrole = text_attribute(element, "AXSubrole");
    let secret = is_secure(&role, subrole.as_deref());
    // A secure field's AXValue is NEVER read: only its title or description
    // names it. Every other element may be named by its value, which is how
    // static text says what it says.
    let name = text_attribute(element, "AXTitle")
        .or_else(|| text_attribute(element, "AXDescription"))
        .or_else(|| {
            (!secret)
                .then(|| text_attribute(element, "AXValue"))
                .flatten()
        })
        .unwrap_or_default();
    let at: CgPoint =
        struct_attribute(element, "AXPosition", AX_VALUE_CG_POINT).unwrap_or_default();
    let size: CgSize = struct_attribute(element, "AXSize", AX_VALUE_CG_SIZE).unwrap_or_default();
    Element {
        id,
        depth,
        role: role_of_ax(&role, subrole.as_deref()),
        name,
        rect: Rect {
            x: at.x.round() as i32,
            y: at.y.round() as i32,
            width: size.width.round() as i32,
            height: size.height.round() as i32,
        },
        // Most static elements do not carry AXEnabled at all; only an element
        // that says it is disabled is marked so.
        enabled: bool_attribute(element, "AXEnabled").unwrap_or(true),
        secret,
    }
}

/// Whether an element is a password field: NSSecureTextField reports the
/// `AXSecureTextField` subrole under `AXTextField`, and some toolkits use it
/// as the role itself. Pure.
pub(crate) fn is_secure(role: &str, subrole: Option<&str>) -> bool {
    role == "AXSecureTextField" || subrole == Some("AXSecureTextField")
}

/// An element's children, each owned.
fn children(element: &Owned) -> Vec<Owned> {
    let Some(array) = attribute(element, "AXChildren") else {
        return Vec::new();
    };
    // SAFETY: type checked before use; each child is borrowed from `array`
    // and retained into its own `Owned` before `array` is released.
    unsafe {
        if CFGetTypeID(array.0) != CFArrayGetTypeID() {
            return Vec::new();
        }
        (0..CFArrayGetCount(array.0))
            .filter_map(|i| Owned::retain(CFArrayGetValueAtIndex(array.0, i)))
            .collect()
    }
}

fn ax_timeout() -> f32 {
    super::limits::get()
        .accessibility_call_timeout
        .as_secs_f32()
}

/// The system-wide accessibility element, with the configured timeout.
fn system_wide() -> Result<Owned, DesktopError> {
    require(Grant::Accessibility)?;
    // SAFETY: creates the system-wide element, owned by the result. The
    // timeout set on it is the global one for every later request.
    let system = Owned::take(unsafe { AXUIElementCreateSystemWide() })
        .ok_or_else(|| DesktopError::Os("the accessibility service is unavailable".to_string()))?;
    unsafe { AXUIElementSetMessagingTimeout(system.0, ax_timeout()) };
    Ok(system)
}

/// Set one attribute on an element; whether the element took it.
fn set_attribute(element: &Owned, name: &str, value: CFTypeRef) -> bool {
    let Some(key) = cf_string(name) else {
        return false;
    };
    // SAFETY: `element` and `key` are live, owned references; `value` is a
    // live CF object the caller holds for the length of the call.
    unsafe { AXUIElementSetAttributeValue(element.0, key.0, value) == AX_SUCCESS }
}

/// An application's accessibility element.
///
/// Chromium and Electron build their accessibility tree only for a client
/// that asks — `AXManualAccessibility`, the attribute Electron documents for
/// exactly this — so it is set on every application looked at. One that does
/// not know it ignores it.
fn application(pid: i32) -> Option<Owned> {
    // SAFETY: creates an element for a pid; owned by the result.
    let app = Owned::take(unsafe { AXUIElementCreateApplication(pid) })?;
    unsafe { AXUIElementSetMessagingTimeout(app.0, ax_timeout()) };
    // SAFETY: kCFBooleanTrue is a static CoreFoundation constant.
    set_attribute(&app, "AXManualAccessibility", unsafe { kCFBooleanTrue });
    Some(app)
}

fn pid_of(element: &Owned) -> Option<i32> {
    let mut pid = 0i32;
    // SAFETY: `element` is live; `pid` is this function's own.
    (unsafe { AXUIElementGetPid(element.0, &mut pid) } == AX_SUCCESS).then_some(pid)
}

/// The window server's number for an accessibility window.
fn window_number(window: &Owned) -> Option<u32> {
    let mut number = 0u32;
    // SAFETY: `window` is live; `number` is this function's own.
    (unsafe { _AXUIElementGetWindow(window.0, &mut number) } == AX_SUCCESS && number != 0)
        .then_some(number)
}

fn same(a: &Owned, b: &Owned) -> bool {
    // SAFETY: both are live, owned references.
    unsafe { CFEqual(a.0, b.0) != 0 }
}

/// The elements of an array attribute, each owned.
fn array_attribute(element: &Owned, name: &str) -> Vec<Owned> {
    let Some(array) = attribute(element, name) else {
        return Vec::new();
    };
    // SAFETY: type checked before use; each item is retained into its own
    // `Owned` before `array` is released.
    unsafe {
        if CFGetTypeID(array.0) != CFArrayGetTypeID() {
            return Vec::new();
        }
        (0..CFArrayGetCount(array.0))
            .filter_map(|i| Owned::retain(CFArrayGetValueAtIndex(array.0, i)))
            .collect()
    }
}

/// The pids that own a window on screen, with the owner's name, in the
/// window server's front-to-back order. Every layer counts, not only
/// ordinary windows: a password manager's menu-bar popover sits above them,
/// and a check that skipped it would let a click land on it.
fn window_owners() -> Vec<(i32, String)> {
    // SAFETY: a copy of the window list, owned by `list`.
    let Some(list) =
        Owned::take(unsafe { CGWindowListCopyWindowInfo(CG_ON_SCREEN_WITHOUT_DESKTOP, 0) })
    else {
        return Vec::new();
    };
    let number = |dict: CFTypeRef, key: &str| -> Option<i64> {
        let key = cf_string(key)?;
        // SAFETY: `dict` is a live dictionary from `list`; the value is
        // borrowed and type checked before it is read.
        unsafe {
            let value = CFDictionaryGetValue(dict, key.0);
            if value.is_null() || CFGetTypeID(value) != CFNumberGetTypeID() {
                return None;
            }
            let mut out = 0i64;
            (CFNumberGetValue(value, CF_NUMBER_SINT64, (&mut out as *mut i64).cast()) != 0)
                .then_some(out)
        }
    };
    let mut owners: Vec<(i32, String)> = Vec::new();
    // SAFETY: `list` is a live array of dictionaries, read only while it is
    // held.
    unsafe {
        if CFGetTypeID(list.0) != CFArrayGetTypeID() {
            return owners;
        }
        for i in 0..CFArrayGetCount(list.0) {
            let dict = CFArrayGetValueAtIndex(list.0, i);
            if dict.is_null() || CFGetTypeID(dict) != CFDictionaryGetTypeID() {
                continue;
            }
            let Some(pid) = number(dict, "kCGWindowOwnerPID") else {
                continue;
            };
            let pid = pid as i32;
            if owners.iter().any(|(p, _)| *p == pid) {
                continue;
            }
            let name = cf_string("kCGWindowOwnerName")
                .map(|key| CFDictionaryGetValue(dict, key.0))
                .and_then(Owned::retain)
                .as_ref()
                .and_then(rust_string)
                .unwrap_or_default();
            owners.push((pid, name));
        }
    }
    owners
}

fn rect_of(element: &Owned) -> Rect {
    let at: CgPoint =
        struct_attribute(element, "AXPosition", AX_VALUE_CG_POINT).unwrap_or_default();
    let size: CgSize = struct_attribute(element, "AXSize", AX_VALUE_CG_SIZE).unwrap_or_default();
    Rect {
        x: at.x.round() as i32,
        y: at.y.round() as i32,
        width: size.width.round() as i32,
        height: size.height.round() as i32,
    }
}

/// The focused application's pid and window, when there are both.
fn focused() -> Result<Option<(i32, Owned)>, DesktopError> {
    let system = system_wide()?;
    let Some(app) = attribute(&system, "AXFocusedApplication") else {
        return Ok(None);
    };
    let Some(pid) = pid_of(&app) else {
        return Ok(None);
    };
    Ok(attribute(&app, "AXFocusedWindow")
        .or_else(|| attribute(&app, "AXMainWindow"))
        .map(|window| (pid, window)))
}

/// `None` when the window server has no number for the window: such a
/// window cannot be named again, so it is not offered to be.
fn info_of(
    pid: i32,
    window: &Owned,
    app: &str,
    focus: &Option<(i32, Owned)>,
) -> Option<WindowInfo> {
    Some(WindowInfo {
        id: format!("{pid}.{}", window_number(window)?),
        title: text_attribute(window, "AXTitle").unwrap_or_default(),
        app: app.to_string(),
        pid: pid as u32,
        focused: matches!(focus, Some((p, w)) if *p == pid && same(w, window)),
        rect: rect_of(window),
        has_tree: true,
    })
}

/// The name an application goes by: its accessibility title, else the
/// window server's owner name for the pid. Empty only when neither can be
/// read — which the blocklist then treats as blocked.
fn app_name(app: &Owned, pid: i32) -> String {
    text_attribute(app, "AXTitle")
        .filter(|name| !name.trim().is_empty())
        .or_else(|| {
            window_owners()
                .into_iter()
                .find(|(p, _)| *p == pid)
                .map(|(_, owner)| owner)
        })
        .unwrap_or_default()
}

pub(crate) fn list_windows() -> Result<Vec<WindowInfo>, DesktopError> {
    let focus = focused()?;
    let mut out = Vec::new();
    for (pid, owner) in window_owners() {
        let Some(app) = application(pid) else {
            continue;
        };
        let name = text_attribute(&app, "AXTitle")
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(owner);
        for window in array_attribute(&app, "AXWindows") {
            if bool_attribute(&window, "AXMinimized") == Some(true) {
                continue;
            }
            out.extend(info_of(pid, &window, &name, &focus));
        }
    }
    Ok(out)
}

pub(crate) fn foreground() -> Result<Option<WindowInfo>, DesktopError> {
    let focus = focused()?;
    let Some((pid, window)) = &focus else {
        return Ok(None);
    };
    let Some(app) = application(*pid) else {
        return Ok(None);
    };
    let name = app_name(&app, *pid);
    Ok(info_of(*pid, window, &name, &focus))
}

/// `pid.number`, as [`list_windows`] gives it: the owner's pid and the
/// window server's number for the window.
pub(crate) fn parse_window_id(id: &str) -> Option<(i32, u32)> {
    let (pid, number) = id.trim().split_once('.')?;
    Some((
        pid.parse().ok().filter(|p| *p > 0)?,
        number.parse().ok().filter(|n| *n > 0)?,
    ))
}

/// The window `window` names, or the focused one.
fn target(window: Option<&str>) -> Result<(Owned, WindowInfo), DesktopError> {
    let focus = focused()?;
    let (pid, number) = match window {
        Some(id) => parse_window_id(id).ok_or_else(|| {
            DesktopError::Os(format!(
                "\"{id}\" is not a window id; take one from list_windows"
            ))
        })?,
        None => {
            let info = foreground()?.ok_or_else(|| {
                DesktopError::Os("no application has a focused window".to_string())
            })?;
            parse_window_id(&info.id)
                .ok_or_else(|| DesktopError::Os("no focused window".to_string()))?
        }
    };
    let app = application(pid)
        .ok_or_else(|| DesktopError::Os(format!("no application with pid {pid}")))?;
    let window = array_attribute(&app, "AXWindows")
        .into_iter()
        .find(|w| window_number(w) == Some(number))
        .ok_or_else(|| {
            DesktopError::Os(format!(
                "window {pid}.{number} is not open any more; call list_windows again"
            ))
        })?;
    let name = app_name(&app, pid);
    let info = info_of(pid, &window, &name, &focus)
        .ok_or_else(|| DesktopError::Os(format!("window {pid}.{number} has no number")))?;
    Ok((window, info))
}

pub(crate) fn window_info(window: Option<&str>) -> Result<WindowInfo, DesktopError> {
    Ok(target(window)?.1)
}

pub(crate) fn tree(window: Option<&str>, max_depth: u32) -> Result<Tree, DesktopError> {
    let (root, info) = target(window)?;
    let walked = walk(&root, max_depth, &describe, &children);
    Ok(Tree {
        root_name: info.title,
        window: info.id,
        app: info.app,
        no_positions: None,
        elements: walked.elements,
        truncated: walked.truncated,
        offscreen: walked.offscreen,
    })
}

fn control(
    window: Option<&str>,
    want: &Want,
    max_depth: u32,
) -> Result<(Owned, Element), DesktopError> {
    let (root, info) = target(window)?;
    find(&root, max_depth, &describe, &children, want)
        .ok_or_else(|| super::uia::gone(want.id, &info))
}

pub(crate) fn set_value(
    window: Option<&str>,
    want: &Want,
    max_depth: u32,
    value: &str,
) -> Result<Done, DesktopError> {
    let (element, described) = control(window, want, max_depth)?;
    let what = described.label();
    let text =
        cf_string(value).ok_or_else(|| DesktopError::Os("could not hold the value".to_string()))?;
    if !set_attribute(&element, "AXValue", text.0) {
        return Err(DesktopError::Os(format!(
            "{what} has no value to set; click_element it and use type_text instead"
        )));
    }
    let did = described.set_to(value);
    if described.secret {
        return Ok(Done::unverified(did, "a password field is never read back"));
    }
    Ok(match text_attribute(&element, "AXValue") {
        Some(now) if now == value.trim() => Done::verified(did),
        Some(now) => Done::unverified(
            did,
            &format!(
                "it reads back as {} characters, not the {} sent",
                now.chars().count(),
                value.chars().count()
            ),
        ),
        None => Done::unverified(did, "its value could not be read back"),
    })
}

/// The `AX…` action for one of the shared vocabulary's names. Pure.
pub(crate) fn ax_action(wanted: &str) -> Option<&'static str> {
    Some(match wanted.trim().to_ascii_lowercase().as_str() {
        "press" | "invoke" | "click" | "toggle" | "default" | "expand" | "collapse" => "AXPress",
        "show_menu" | "menu" => "AXShowMenu",
        "confirm" => "AXConfirm",
        "increment" => "AXIncrement",
        "decrement" => "AXDecrement",
        "cancel" => "AXCancel",
        "raise" => "AXRaise",
        "select" | "pick" => "AXPick",
        _ => return None,
    })
}

pub(crate) fn act(
    window: Option<&str>,
    want: &Want,
    max_depth: u32,
    action: &str,
) -> Result<Done, DesktopError> {
    let (element, described) = control(window, want, max_depth)?;
    let what = described.label();
    if action.trim().eq_ignore_ascii_case("focus") {
        // SAFETY: kCFBooleanTrue is a static CoreFoundation constant.
        let took = set_attribute(&element, "AXFocused", unsafe { kCFBooleanTrue });
        return Ok(match (took, bool_attribute(&element, "AXFocused")) {
            (true, Some(true)) => Done::verified(format!("focused {what}")),
            _ => Done::unverified(
                format!("asked {what} to take focus"),
                "it does not report focus",
            ),
        });
    }
    let offered: Vec<String> = {
        let mut names: CFTypeRef = std::ptr::null();
        // SAFETY: `element` is live; on success the array is owned by `names`.
        let ok = unsafe { AXUIElementCopyActionNames(element.0, &mut names) } == AX_SUCCESS;
        match (ok, Owned::take(names)) {
            (true, Some(array)) => {
                // SAFETY: type checked before use; items are borrowed while
                // `array` is held.
                unsafe {
                    if CFGetTypeID(array.0) == CFArrayGetTypeID() {
                        (0..CFArrayGetCount(array.0))
                            .filter_map(|i| Owned::retain(CFArrayGetValueAtIndex(array.0, i)))
                            .filter_map(|s| rust_string(&s))
                            .collect()
                    } else {
                        Vec::new()
                    }
                }
            }
            _ => Vec::new(),
        }
    };
    let name = ax_action(action)
        .map(str::to_string)
        .unwrap_or_else(|| action.trim().to_string());
    if !offered.contains(&name) {
        return Err(DesktopError::Os(format!(
            "{what} does not offer \"{action}\" ({name}); it offers: {}",
            if offered.is_empty() {
                "nothing — use click_element or set_value".to_string()
            } else {
                offered.join(", ")
            }
        )));
    }
    let key = cf_string(&name).ok_or_else(|| DesktopError::Os("bad action name".to_string()))?;
    // SAFETY: both references are live and owned.
    let result = unsafe { AXUIElementPerformAction(element.0, key.0) };
    if result != AX_SUCCESS {
        return Err(DesktopError::Os(format!(
            "{what} refused {name} (error {result})"
        )));
    }
    Ok(Done::unverified(
        format!("ran {name} on {what}"),
        "an action has no state to read back; look at the result",
    ))
}

pub(crate) fn focus_window(window: &str) -> Result<Done, DesktopError> {
    let (element, info) = target(Some(window))?;
    if let Some(app) = application(info.pid as i32) {
        // SAFETY: kCFBooleanTrue is a static CoreFoundation constant.
        set_attribute(&app, "AXFrontmost", unsafe { kCFBooleanTrue });
    }
    if let Some(raise) = cf_string("AXRaise") {
        // SAFETY: both references are live and owned.
        unsafe { AXUIElementPerformAction(element.0, raise.0) };
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    let now = foreground()?;
    Ok(match now {
        Some(w) if w.id == info.id => Done::verified(format!("focused window \"{}\"", info.title)),
        _ => Done::unverified(
            format!("asked window \"{}\" to come to the front", info.title),
            "another window still has the focus",
        ),
    })
}

/// The clipboard's text now, through the system's own `pbpaste`: `None`
/// when it holds no text. `pbpaste` prints nothing for a clipboard holding
/// only an image, so empty output is "no text", never text to put back.
pub(crate) fn get_clipboard() -> Result<Option<String>, String> {
    let output = std::process::Command::new("/usr/bin/pbpaste")
        .env("LANG", "en_US.UTF-8")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run pbpaste: {e}"))?;
    if !output.status.success() {
        return Err(format!("pbpaste failed ({})", output.status));
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok((!text.is_empty()).then_some(text))
}

/// Put text on the clipboard, through the system's own `pbcopy`.
pub(crate) fn set_clipboard(text: &str) -> Result<(), DesktopError> {
    use std::io::Write;
    let mut child = std::process::Command::new("/usr/bin/pbcopy")
        // pbcopy reads its input in the locale's encoding; without a UTF-8
        // one, anything outside ASCII arrives as something else.
        .env("LANG", "en_US.UTF-8")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| DesktopError::Os(format!("could not start pbcopy: {e}")))?;
    if let Some(mut input) = child.stdin.take() {
        input
            .write_all(text.as_bytes())
            .map_err(|e| DesktopError::Os(format!("could not hand pbcopy the text: {e}")))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| DesktopError::Os(format!("pbcopy did not finish: {e}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(DesktopError::Os(format!(
            "pbcopy failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A secure text field is recognised by role or subrole, and nothing else
    /// is.
    #[test]
    fn a_password_field_is_recognised_however_it_is_reported() {
        assert!(is_secure("AXTextField", Some("AXSecureTextField")));
        assert!(is_secure("AXSecureTextField", None));
        assert!(!is_secure("AXTextField", None));
        assert!(!is_secure("AXTextField", Some("AXSearchField")));
    }

    /// The shared action names become AX actions; an unknown one does not.
    #[test]
    fn the_shared_actions_name_ax_actions() {
        assert_eq!(ax_action("press"), Some("AXPress"));
        assert_eq!(ax_action(" Show_Menu "), Some("AXShowMenu"));
        assert_eq!(ax_action("increment"), Some("AXIncrement"));
        assert_eq!(ax_action("fly"), None);
    }

    /// A window id is `pid.number` (the window server's number, never 0);
    /// anything else is refused, not guessed.
    #[test]
    fn a_window_id_is_a_pid_and_a_window_number() {
        assert_eq!(parse_window_id("501.4211"), Some((501, 4211)));
        assert_eq!(parse_window_id(" 77.3 "), Some((77, 3)));
        for bad in ["501", "x.1", "0.1", "-4.1", "1.x", "1.0", ""] {
            assert_eq!(parse_window_id(bad), None, "{bad}");
        }
    }

    /// The refusal names the grant, where to give it, and the program —
    /// everything a person needs, since the OS itself says nothing.
    #[test]
    fn a_missing_grant_says_what_and_where() {
        let said = refusal(Grant::ScreenRecording, "/usr/local/bin/incode");
        assert!(said.contains("Screen Recording"), "{said}");
        assert!(said.contains("Privacy & Security"), "{said}");
        assert!(said.contains("/usr/local/bin/incode"), "{said}");
        assert!(refusal(Grant::Accessibility, "x").contains("Accessibility"));
    }

    /// Strings make the round trip through CoreFoundation intact, including
    /// text outside ASCII.
    #[test]
    fn a_string_round_trips_through_core_foundation() {
        for text in ["AXButton", "Ünïcødé ✓", ""] {
            let cf = cf_string(text).expect("created");
            assert_eq!(rust_string(&cf).as_deref(), Some(text));
        }
    }

    /// The grant probe answers without prompting or failing, whatever the
    /// state of this machine.
    #[test]
    fn the_grant_probe_answers() {
        let missing = missing_grants();
        assert!(missing.len() <= 2, "{missing:?}");
    }

    fn pasteboard() -> String {
        let read = std::process::Command::new("/usr/bin/pbpaste")
            .env("LANG", "en_US.UTF-8")
            .output()
            .expect("pbpaste");
        String::from_utf8_lossy(&read.stdout).into_owned()
    }

    /// Live: the clipboard takes the text it is given, read back through
    /// `pbpaste` — and the person's own clipboard is put back afterwards, so
    /// running the tests does not eat what they copied.
    #[test]
    fn the_clipboard_holds_what_was_set() {
        let theirs = pasteboard();
        let text = "incode clipboard check ✓";
        set_clipboard(text).expect("set");
        let read = pasteboard();
        set_clipboard(&theirs).expect("restore");
        assert_eq!(read, text);
    }
}
