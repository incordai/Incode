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

use super::uia::{role_of_ax, walk, Element, Tree};
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

/// How long one accessibility request may take. An application that has
/// stopped answering would otherwise hold the walk — and the agent — for the
/// system default of several seconds per element.
const AX_TIMEOUT_SECS: f32 = 1.0;

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
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
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
    let name = text_attribute(element, "AXTitle")
        .or_else(|| text_attribute(element, "AXDescription"))
        .or_else(|| text_attribute(element, "AXValue"))
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
    }
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

/// The focused window's accessibility tree.
pub(crate) fn focused_window_tree(max_depth: u32) -> Result<Tree, DesktopError> {
    require(Grant::Accessibility)?;
    // SAFETY: creates the system-wide element, owned by `system`. The timeout
    // set on it is the global one for every later request in this process.
    let system = Owned::take(unsafe { AXUIElementCreateSystemWide() })
        .ok_or_else(|| DesktopError::Os("the accessibility service is unavailable".to_string()))?;
    unsafe { AXUIElementSetMessagingTimeout(system.0, AX_TIMEOUT_SECS) };

    let app = attribute(&system, "AXFocusedApplication")
        .ok_or_else(|| DesktopError::Os("no application has the focus".to_string()))?;
    let window = attribute(&app, "AXFocusedWindow")
        .or_else(|| attribute(&app, "AXMainWindow"))
        .ok_or_else(|| DesktopError::Os("the focused application has no window".to_string()))?;
    let root_name = text_attribute(&window, "AXTitle")
        .or_else(|| text_attribute(&app, "AXTitle"))
        .unwrap_or_default();

    let (elements, truncated) = walk(&window, max_depth, &describe, &children);
    Ok(Tree {
        root_name,
        elements,
        truncated,
    })
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
