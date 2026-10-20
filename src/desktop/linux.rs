//! Linux: the X display computer use drives, its clipboard, and its
//! accessibility tree.
//!
//! # One display, named by the operator
//!
//! `docs/COMPUTER_USE.md` 7e: the server is TOLD which display to use and
//! refuses when it is not — it never falls back to whatever `$DISPLAY` the
//! process inherited, because that is the operator's own desktop. The setting
//! is admin.json `desktop_display`:
//!
//! - `":N"` attaches to an X server that is already running;
//! - `"xvfb"` / `"xvfb:WIDTHxHEIGHT"` starts a private `Xvfb` for this
//!   server's lifetime. It is stopped when the server ends, and the kernel
//!   stops it if the server is killed (`PR_SET_PDEATHSIG`), so an agent's
//!   desktop is never left running behind it.
//!
//! The chosen display is put in this process's `DISPLAY` before the input
//! thread opens its connection, and the Wayland variables are cleared, so
//! capture (xcap) and input (enigo) both take the X11 path to that display.
//!
//! # The accessibility tree
//!
//! AT-SPI2 over D-Bus. Every application on the session's accessibility bus is
//! listed there, whichever display it draws on, so the tree keeps only the
//! applications whose own `DISPLAY` is this server's — read from the process
//! that owns the application's bus name. A window on the operator's desktop
//! is never offered as something to click on the agent's.
//!
//! # The windows
//!
//! The X server's own list is the whole truth of what is on the display;
//! AT-SPI knows only the programs with an accessibility bridge. Both are read
//! and merged (`x11.rs`), so a window with no tree — xterm — is still listed,
//! focused, pictured and kept out of a click when its owner is blocked.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use zbus::blocking::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::uia::{self, Done, Element, Tree, WindowInfo};
use super::x11::{self, Listed, X11Window};
use super::{DesktopError, Rect};

/// The size of a private display when the setting names none.
pub const DEFAULT_XVFB_SIZE: (u32, u32) = (1920, 1080);

// How long a private Xvfb has to report it is ready, the clipboard program
// has to take the text, and each accessibility call may take (a hung
// application costs that, not the whole walk): admin.json `desktop_limits`
// (`xvfb_start_timeout_secs`, `clipboard_timeout_secs`,
// `accessibility_call_timeout_ms`), read through `limits::get()`.

/// The display this process attached to, by number.
static ATTACHED: OnceLock<u32> = OnceLock::new();

/// What `desktop_display` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplaySetting {
    /// An X server already running, by display number.
    Existing(u32),
    /// A private `Xvfb` of this size.
    Private { width: u32, height: u32 },
}

/// Read the `desktop_display` setting. Pure, so every form is tested.
pub fn parse_setting(raw: &str) -> Result<DisplaySetting, String> {
    let raw = raw.trim();
    let accepted = "use \":N\" for a running X display, or \"xvfb\" / \"xvfb:WIDTHxHEIGHT\" \
                    for a private one";
    if let Some(rest) = raw.strip_prefix("xvfb") {
        let (width, height) = match rest.strip_prefix(':') {
            None if rest.is_empty() => DEFAULT_XVFB_SIZE,
            Some(size) => {
                let parsed = size
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)));
                match parsed {
                    Some((w, h)) if (64..=8192).contains(&w) && (64..=8192).contains(&h) => (w, h),
                    _ => {
                        return Err(format!(
                            "desktop_display \"{raw}\": the size must be WIDTHxHEIGHT, each \
                             64 to 8192"
                        ))
                    }
                }
            }
            None => {
                return Err(format!(
                    "desktop_display \"{raw}\" is not understood: {accepted}"
                ))
            }
        };
        return Ok(DisplaySetting::Private { width, height });
    }
    display_number(raw)
        .map(DisplaySetting::Existing)
        .ok_or_else(|| format!("desktop_display \"{raw}\" is not understood: {accepted}"))
}

/// The number of a LOCAL display name: `":99"`, `":99.0"`, `"unix:99"`.
/// `None` for a remote one (`"host:0"`) or anything else, which this server
/// does not drive.
pub fn display_number(name: &str) -> Option<u32> {
    let (host, rest) = name.trim().rsplit_once(':')?;
    if !(host.is_empty() || host == "unix") {
        return None;
    }
    let number = rest.split('.').next()?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

/// The display this server drives, as `":N"`, once attached.
pub fn attached_display() -> Option<String> {
    ATTACHED.get().map(|n| format!(":{n}"))
}

/// The display this server is attached to. Holding it keeps a private `Xvfb`
/// alive; dropping it stops that server.
pub struct Display {
    number: u32,
    server: Option<Child>,
}

impl Display {
    /// `":N"`.
    pub fn name(&self) -> String {
        format!(":{}", self.number)
    }

    /// Whether this server started the display (and will stop it).
    pub fn is_private(&self) -> bool {
        self.server.is_some()
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        if let Some(mut server) = self.server.take() {
            let _ = server.kill();
            let _ = server.wait();
        }
    }
}

/// Attach this process to the display `desktop_display` names.
///
/// MUST run before any other thread starts: it sets `DISPLAY` and clears the
/// Wayland variables in this process's environment, which the capture and
/// input libraries read when they connect. `incode --desktop-mcp` calls it
/// first, while the process is still single-threaded.
pub fn attach(setting: Option<&str>) -> Result<Display, DesktopError> {
    let Some(raw) = setting.filter(|s| !s.trim().is_empty()) else {
        return Err(DesktopError::Os(
            "computer use on Linux needs `desktop_display` in ~/.incode/admin.json: \":N\" for \
             a running X display, or \"xvfb\" for a private one this server starts and stops. \
             It does not fall back to $DISPLAY, which is the operator's own desktop \
             (docs/COMPUTER_USE.md 7e)."
                .to_string(),
        ));
    };
    let display = match parse_setting(raw).map_err(DesktopError::Os)? {
        DisplaySetting::Existing(number) => {
            let socket = format!("/tmp/.X11-unix/X{number}");
            if !std::path::Path::new(&socket).exists() {
                return Err(DesktopError::Os(format!(
                    "desktop_display \":{number}\" has no X server: {socket} does not exist. \
                     Start one there, or set \"xvfb\" to have this server start its own."
                )));
            }
            Display {
                number,
                server: None,
            }
        }
        DisplaySetting::Private { width, height } => start_xvfb(width, height)?,
    };
    std::env::set_var("DISPLAY", display.name());
    std::env::remove_var("WAYLAND_DISPLAY");
    std::env::set_var("XDG_SESSION_TYPE", "x11");
    let _ = ATTACHED.set(display.number);
    Ok(display)
}

/// Start a private `Xvfb` on a free display number and wait until it says it
/// is ready.
///
/// `-displayfd 1` makes Xvfb choose the number itself and write it to its
/// stdout once it accepts connections, which answers "which display" and "is
/// it up" in one line, with no polling and no race for a number.
fn start_xvfb(width: u32, height: u32) -> Result<Display, DesktopError> {
    let mut command = Command::new("Xvfb");
    command
        .args(["-displayfd", "1", "-screen", "0"])
        .arg(format!("{width}x{height}x24"))
        .args(["-nolisten", "tcp"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: prctl is async-signal-safe and touches only the child. It asks
    // the kernel to stop Xvfb when this process dies, however it dies.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|e| {
        DesktopError::Os(if e.kind() == std::io::ErrorKind::NotFound {
            "desktop_display \"xvfb\" needs Xvfb on PATH (Debian/Ubuntu: apt install xvfb)"
                .to_string()
        } else {
            format!("could not start Xvfb: {e}")
        })
    })?;

    // Both pipes are drained for Xvfb's whole life, so a chatty server can
    // never fill one and stall; stderr's last lines are kept for the error.
    let said = Arc::new(Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let said = said.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Ok(mut said) = said.lock() {
                    said.push_str(&line);
                    said.push('\n');
                    let mut cut = said.len().saturating_sub(2048);
                    while !said.is_char_boundary(cut) {
                        cut += 1;
                    }
                    said.drain(..cut);
                }
            }
        });
    }
    let (tx, rx) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
            let _ = tx.send(first);
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        });
    }

    let fail = |child: &mut Child, why: String| {
        let _ = child.kill();
        let _ = child.wait();
        let said = said
            .lock()
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        DesktopError::Os(if said.is_empty() {
            why
        } else {
            format!("{why}. Xvfb said:\n{said}")
        })
    };
    let start_timeout = super::limits::get().xvfb_start_timeout;
    let line = match rx.recv_timeout(start_timeout) {
        Ok(line) => line,
        Err(_) => {
            return Err(fail(
                &mut child,
                format!(
                    "Xvfb did not report a display within {}s",
                    start_timeout.as_secs()
                ),
            ))
        }
    };
    match line.trim().parse::<u32>() {
        Ok(number) => Ok(Display {
            number,
            server: Some(child),
        }),
        Err(_) => Err(fail(
            &mut child,
            "Xvfb exited before reporting a display".to_string(),
        )),
    }
}

/// Put text on the display's clipboard, through `xclip` or else `xsel`.
///
/// Both keep serving the selection from a background copy of themselves after
/// the command returns, which X requires: the clipboard is held by a client,
/// not stored by the server. Their output is discarded rather than piped — a
/// pipe the background copy inherits would never close.
pub(crate) fn set_clipboard(text: &str) -> Result<(), DesktopError> {
    let programs: [(&str, &[&str]); 2] = [
        ("xclip", &["-selection", "clipboard", "-in"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (program, args) in programs {
        let mut child = match Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(DesktopError::Os(format!("could not run {program}: {e}"))),
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).map_err(|e| {
                DesktopError::Os(format!("could not hand the text to {program}: {e}"))
            })?;
        }
        let timeout = super::limits::get().clipboard_timeout;
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => {
                    return Err(DesktopError::Os(format!(
                        "{program} could not set the clipboard ({status})"
                    )))
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(DesktopError::Os(format!(
                        "{program} did not take the text within {}s",
                        timeout.as_secs()
                    )));
                }
                Err(e) => return Err(DesktopError::Os(format!("{program}: {e}"))),
            }
        }
    }
    Err(DesktopError::Os(
        "paste_text on Linux needs xclip or xsel on PATH (Debian/Ubuntu: apt install xclip); \
         type_text works without either"
            .to_string(),
    ))
}

/// AT-SPI2 state bits, as numbered in `atspi-constants.h` (`AtspiStateType`).
const STATE_ACTIVE: u32 = 1;
const STATE_ENABLED: u32 = 8;
const STATE_FOCUSED: u32 = 12;
const STATE_SENSITIVE: u32 = 24;
const STATE_SHOWING: u32 = 25;

/// Whether a control takes input. GTK4 reports SENSITIVE and never ENABLED
/// (measured: a live zenity dialog's every node had bits 24, 25 and 30 and
/// not 8); GTK3 reports both. Either one means enabled.
pub(crate) fn is_enabled(states: &[u32]) -> bool {
    has_state(states, STATE_ENABLED) || has_state(states, STATE_SENSITIVE)
}

/// Whether a state set (`au`: bit N of the set is word N/32, bit N%32) holds
/// this state. Pure.
pub(crate) fn has_state(states: &[u32], state: u32) -> bool {
    states
        .get((state / 32) as usize)
        .is_some_and(|word| word & (1 << (state % 32)) != 0)
}

/// An accessible object: the bus name that serves it and its path.
type Node = (String, OwnedObjectPath);

const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";

/// The accessibility bus: `AT_SPI_BUS_ADDRESS` if set, else the address the
/// session bus's `org.a11y.Bus` gives — the order AT-SPI's own clients use.
fn a11y_bus() -> Result<Connection, DesktopError> {
    let address = match std::env::var("AT_SPI_BUS_ADDRESS") {
        Ok(address) if !address.is_empty() => address,
        _ => {
            let session = zbus::blocking::connection::Builder::session()
                .and_then(|b| {
                    b.method_timeout(super::limits::get().accessibility_call_timeout)
                        .build()
                })
                .map_err(|e| {
                    DesktopError::Os(format!(
                        "the accessibility tree needs a D-Bus session bus, and none answered \
                         ({e}). Pixels still work: screenshot and click."
                    ))
                })?;
            session
                .call_method(
                    Some("org.a11y.Bus"),
                    "/org/a11y/bus",
                    Some("org.a11y.Bus"),
                    "GetAddress",
                    &(),
                )
                .and_then(|reply| reply.body().deserialize::<String>())
                .map_err(|e| {
                    DesktopError::Os(format!(
                        "the session bus has no accessibility bus ({e}); is at-spi2-core \
                         installed?"
                    ))
                })?
        }
    };
    zbus::blocking::connection::Builder::address(address.as_str())
        .and_then(|b| {
            b.method_timeout(super::limits::get().accessibility_call_timeout)
                .build()
        })
        .map_err(|e| DesktopError::Os(format!("could not reach the accessibility bus: {e}")))
}

fn children(bus: &Connection, node: &Node) -> Vec<Node> {
    bus.call_method(
        Some(node.0.as_str()),
        node.1.as_str(),
        Some(ACCESSIBLE),
        "GetChildren",
        &(),
    )
    .and_then(|reply| reply.body().deserialize::<Vec<Node>>())
    .unwrap_or_default()
}

fn states(bus: &Connection, node: &Node) -> Vec<u32> {
    bus.call_method(
        Some(node.0.as_str()),
        node.1.as_str(),
        Some(ACCESSIBLE),
        "GetState",
        &(),
    )
    .and_then(|reply| reply.body().deserialize::<Vec<u32>>())
    .unwrap_or_default()
}

fn name(bus: &Connection, node: &Node) -> String {
    bus.call_method(
        Some(node.0.as_str()),
        node.1.as_str(),
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &(ACCESSIBLE, "Name"),
    )
    .and_then(|reply| reply.body().deserialize::<OwnedValue>())
    .ok()
    .and_then(|value| String::try_from(value).ok())
    .unwrap_or_default()
}

fn describe(bus: &Connection, node: &Node, id: u32, depth: u32) -> Element {
    let role_name = bus
        .call_method(
            Some(node.0.as_str()),
            node.1.as_str(),
            Some(ACCESSIBLE),
            "GetRoleName",
            &(),
        )
        .and_then(|reply| reply.body().deserialize::<String>())
        .unwrap_or_default();
    let role = uia::role_of_atspi(&role_name);
    // Screen coordinates (`ATSPI_COORD_TYPE_SCREEN` = 0): X11 pixels, the
    // space the mouse uses. An object with no Component has no area and is
    // dropped by the walk's filter, though its children are still walked.
    let rect = extents(bus, node);
    Element {
        id,
        depth,
        role,
        // The Name is a label, never the field's text; a password field's
        // content is only ever reached through the Text interface, which
        // nothing here reads for one.
        name: name(bus, node),
        rect,
        enabled: is_enabled(&states(bus, node)),
        secret: role_name == "password text",
    }
}

fn extents(bus: &Connection, node: &Node) -> Rect {
    bus.call_method(
        Some(node.0.as_str()),
        node.1.as_str(),
        Some("org.a11y.atspi.Component"),
        "GetExtents",
        &(0u32,),
    )
    .and_then(|reply| reply.body().deserialize::<(i32, i32, i32, i32)>())
    .map(|(x, y, width, height)| Rect {
        x,
        y,
        width,
        height,
    })
    .unwrap_or(Rect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    })
}

/// Whether a walk's rectangles are not positions at all: every control with
/// an area claims the same top-left corner. No real layout does that — a
/// window's buttons are never at its label's origin — and GTK4 does it to
/// every node (measured: GTK 4.18.6 reported (0,0) for a dialog and all its
/// descendants in both screen and parent coordinates, where GTK 3.24.49
/// reported the true screen positions). Clicking such a tree would land every
/// click on one corner, so it is refused instead. Pure.
pub(crate) fn positions_are_unreal(elements: &[Element]) -> bool {
    let sized: Vec<&Element> = elements
        .iter()
        .filter(|e| e.rect.width > 0 && e.rect.height > 0)
        .collect();
    let Some(first) = sized.first() else {
        return false;
    };
    sized.len() >= 3
        && sized.iter().any(|e| e.role.is_interactive())
        && sized
            .iter()
            .all(|e| (e.rect.x, e.rect.y) == (first.rect.x, first.rect.y))
}

/// "GTK 4.18.6", from the application's own report, for a message.
fn toolkit(bus: &Connection, app: &str) -> String {
    let read = |property: &str| {
        bus.call_method(
            Some(app),
            "/org/a11y/atspi/accessible/root",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.a11y.atspi.Application", property),
        )
        .and_then(|reply| reply.body().deserialize::<OwnedValue>())
        .ok()
        .and_then(|value| String::try_from(value).ok())
        .unwrap_or_default()
    };
    format!("{} {}", read("ToolkitName"), read("Version"))
        .trim()
        .to_string()
}

/// The process that owns a bus name.
fn pid_of(bus: &Connection, bus_name: &str) -> Option<u32> {
    bus.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "GetConnectionUnixProcessID",
        &(bus_name,),
    )
    .and_then(|reply| reply.body().deserialize::<u32>())
    .ok()
}

/// A process's program name, as the kernel keeps it.
fn program_of(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The display number a process was started for, from its environment.
fn display_of_pid(pid: u32) -> Option<u32> {
    let mut environ = Vec::new();
    std::fs::File::open(format!("/proc/{pid}/environ"))
        .ok()?
        .read_to_end(&mut environ)
        .ok()?;
    environ
        .split(|b| *b == 0)
        .find_map(|entry| entry.strip_prefix(b"DISPLAY="))
        .and_then(|value| display_number(&String::from_utf8_lossy(value)))
}

/// The display this server is attached to.
fn ours() -> Result<u32, DesktopError> {
    ATTACHED.get().copied().ok_or_else(|| {
        DesktopError::Os("no display is attached; see `desktop_display`".to_string())
    })
}

/// A window's id: its application's bus name and its object path. Opaque to
/// the agent, and exact — two windows of one application differ by path.
fn window_id(node: &Node) -> String {
    format!("{}|{}", node.0, node.1.as_str())
}

/// Every window SHOWING on this server's display that has an accessibility
/// tree. Only applications whose own `DISPLAY` is this server's are looked
/// at: the accessibility bus lists every application of the session, the
/// operator's own desktop included.
fn windows_on(bus: &Connection, display: u32) -> Result<Vec<(Node, WindowInfo)>, DesktopError> {
    let root: Node = (
        "org.a11y.atspi.Registry".to_string(),
        OwnedObjectPath::try_from("/org/a11y/atspi/accessible/root")
            .map_err(|e| DesktopError::Os(format!("bad registry path: {e}")))?,
    );
    let mut out = Vec::new();
    for app in children(bus, &root) {
        let Some(pid) = pid_of(bus, &app.0) else {
            continue;
        };
        if display_of_pid(pid) != Some(display) {
            continue;
        }
        let program = program_of(pid);
        for window in children(bus, &app) {
            let set = states(bus, &window);
            if !has_state(&set, STATE_SHOWING) {
                continue;
            }
            let info = WindowInfo {
                id: window_id(&window),
                title: name(bus, &window),
                app: program.clone(),
                pid,
                focused: has_state(&set, STATE_ACTIVE),
                rect: extents(bus, &window),
                has_tree: true,
            };
            out.push((window, info));
        }
    }
    Ok(out)
}

/// What is on this server's display: every window, merged from the X
/// server's list and the accessibility tree's ([`x11::merge`]), and the
/// accessibility bus — or why it did not answer.
struct Desktop {
    display: u32,
    bus: Result<Connection, DesktopError>,
    windows: Vec<Listed<Node>>,
    window_manager: bool,
}

/// Read the display. The X server's answer is required: it is the one list
/// that holds every window, so without it no click or capture can be checked
/// against a blocked application. The accessibility bus is not — without it
/// every window is still listed, with no tree.
fn desktop() -> Result<Desktop, DesktopError> {
    let display = ours()?;
    let shown = x_windows(display).map_err(|e| {
        DesktopError::Os(format!(
            "the X server on :{display} could not be asked which windows it shows ({e})"
        ))
    })?;
    let bus = a11y_bus();
    let accessible = match &bus {
        Ok(bus) => windows_on(bus, display)?,
        Err(_) => Vec::new(),
    };
    let mut windows = x11::merge(accessible, &shown.windows);
    mark_focus(&mut windows, &shown.focus_chain, display);
    Ok(Desktop {
        display,
        bus,
        windows,
        window_manager: shown.window_manager,
    })
}

/// Mark the window keystrokes reach. The X server's input focus decides
/// when it lands in a listed window — it is where keys go, whatever a toolkit
/// last reported — and then only that window is focused. Otherwise an
/// accessible window that says it is ACTIVE is (a window manager set it);
/// otherwise the one showing accessible window of the process keys reach —
/// and if it has several, or the owner cannot be named, none is, rather than
/// a guess.
fn mark_focus(windows: &mut [Listed<Node>], focus_chain: &[u32], display: u32) {
    if let Some(at) = x11::focused_index(windows, focus_chain) {
        for (i, window) in windows.iter_mut().enumerate() {
            window.info.focused = i == at;
        }
        return;
    }
    if windows.iter().any(|w| w.info.focused) {
        return;
    }
    if let KeyTarget::Process(pid) = keyboard_target(display) {
        let theirs: Vec<usize> = (0..windows.len())
            .filter(|&i| windows[i].node.is_some() && windows[i].info.pid == pid)
            .collect();
        if let [only] = theirs[..] {
            windows[only].info.focused = true;
        }
    }
}

/// What the X server says of its display.
struct XWindows {
    windows: Vec<X11Window>,
    /// From the window keys reach (the focus, or under `PointerRoot` the
    /// window under the pointer) up toward the root, the root left out.
    /// Empty when keys reach no window.
    focus_chain: Vec<u32>,
    /// A window manager answers `_NET_SUPPORTING_WM_CHECK` on the root.
    window_manager: bool,
}

/// How many parents a window is climbed through toward the root. Real
/// stacks are two or three deep (client, frame, root); the bound keeps a
/// broken tree from looping.
const MAX_CLIMB: usize = 32;

/// The top-level windows on the display: the window manager's client list
/// (`_NET_CLIENT_LIST`) when one runs, else the root's children — each kept
/// when it is mapped, can be drawn in (InputOutput) and says who it is (a
/// title, a WM_CLASS or a pid). Rectangles are on the root, in pixels.
fn x_windows(display: u32) -> Result<XWindows, String> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{
        AtomEnum, ConnectionExt as _, GetPropertyReply, MapState, WindowClass,
    };

    let (conn, screen) = x11rb::connect(Some(&format!(":{display}"))).map_err(|e| e.to_string())?;
    let root = conn
        .setup()
        .roots
        .get(screen)
        .ok_or("the display has no screen")?
        .root;
    let e = |e: &dyn std::fmt::Display| e.to_string();
    let atom = |name: &[u8]| -> Result<u32, String> {
        Ok(conn
            .intern_atom(true, name)
            .map_err(|x| e(&x))?
            .reply()
            .map_err(|x| e(&x))?
            .atom)
    };
    let net_client_list = atom(b"_NET_CLIENT_LIST")?;
    let net_wm_name = atom(b"_NET_WM_NAME")?;
    let net_wm_pid = atom(b"_NET_WM_PID")?;
    let net_wm_check = atom(b"_NET_SUPPORTING_WM_CHECK")?;
    // `None` for a property this server has never heard of (its atom is 0).
    let property = |window: u32, name: u32| -> Result<Option<GetPropertyReply>, String> {
        if name == 0 {
            return Ok(None);
        }
        conn.get_property(false, window, name, AtomEnum::ANY, 0, 1024)
            .map_err(|x| e(&x))?
            .reply()
            .map(Some)
            .map_err(|x| e(&x))
    };
    let bytes = |reply: Option<GetPropertyReply>| reply.map(|r| r.value).unwrap_or_default();
    let words = |reply: Option<GetPropertyReply>| -> Vec<u32> {
        reply
            .as_ref()
            .and_then(GetPropertyReply::value32)
            .map(Iterator::collect)
            .unwrap_or_default()
    };

    let window_manager = words(property(root, net_wm_check)?)
        .first()
        .is_some_and(|&w| w != 0);
    let listed = words(property(root, net_client_list)?);
    let candidates = if listed.is_empty() {
        conn.query_tree(root)
            .map_err(|x| e(&x))?
            .reply()
            .map_err(|x| e(&x))?
            .children
    } else {
        listed
    };

    let mut windows = Vec::new();
    for xid in candidates {
        // A window can close between the list and these questions; it is
        // then simply not on screen.
        let Ok(Ok(attributes)) = conn.get_window_attributes(xid).map(|c| c.reply()) else {
            continue;
        };
        if attributes.map_state != MapState::VIEWABLE
            || attributes.class != WindowClass::INPUT_OUTPUT
        {
            continue;
        }
        let utf8 = bytes(property(xid, net_wm_name)?);
        let title = if utf8.is_empty() {
            String::from_utf8_lossy(&bytes(property(xid, AtomEnum::WM_NAME.into())?)).into_owned()
        } else {
            String::from_utf8_lossy(&utf8).into_owned()
        };
        let class: Vec<String> = bytes(property(xid, AtomEnum::WM_CLASS.into())?)
            .split(|b| *b == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect();
        let pid = words(property(xid, net_wm_pid)?)
            .first()
            .copied()
            .filter(|&p| p != 0);
        if title.trim().is_empty() && class.is_empty() && pid.is_none() {
            continue;
        }
        let (Ok(Ok(geometry)), Ok(Ok(at))) = (
            conn.get_geometry(xid).map(|c| c.reply()),
            conn.translate_coordinates(xid, root, 0, 0)
                .map(|c| c.reply()),
        ) else {
            continue;
        };
        windows.push(X11Window {
            xid,
            title,
            class,
            pid,
            program: pid.map(program_of).unwrap_or_default(),
            rect: Rect {
                x: i32::from(at.dst_x),
                y: i32::from(at.dst_y),
                width: i32::from(geometry.width),
                height: i32::from(geometry.height),
            },
        });
    }

    let focus = conn
        .get_input_focus()
        .map_err(|x| e(&x))?
        .reply()
        .map_err(|x| e(&x))?
        .focus;
    // 0 is None and 1 is PointerRoot: keys follow the pointer.
    let mut window = if focus <= 1 || focus == root {
        conn.query_pointer(root)
            .map_err(|x| e(&x))?
            .reply()
            .map_err(|x| e(&x))?
            .child
    } else {
        focus
    };
    let mut focus_chain = Vec::new();
    for _ in 0..MAX_CLIMB {
        if window == 0 || window == root {
            break;
        }
        focus_chain.push(window);
        window = match conn.query_tree(window).map(|c| c.reply()) {
            Ok(Ok(tree)) => tree.parent,
            _ => break,
        };
    }
    Ok(XWindows {
        windows,
        focus_chain,
        window_manager,
    })
}

/// Give an X window the keyboard: through the window manager when one runs
/// (`_NET_ACTIVE_WINDOW`, which it may honour or refuse), else directly —
/// raised to the top and given the input focus, since with no manager
/// nothing else would. Each request is checked, so a window that closed in
/// between is an error rather than a silent no-op.
fn give_focus(display: u32, xid: u32, window_manager: bool) -> Result<(), DesktopError> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{
        ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask, InputFocus,
        StackMode,
    };

    let fail = |e: &dyn std::fmt::Display| {
        DesktopError::Os(format!(
            "could not give window {} the keyboard: {e}",
            x11::id_of(xid)
        ))
    };
    let (conn, screen) = x11rb::connect(Some(&format!(":{display}"))).map_err(|e| fail(&e))?;
    let root = conn
        .setup()
        .roots
        .get(screen)
        .ok_or_else(|| fail(&"the display has no screen"))?
        .root;
    if window_manager {
        let active = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")
            .map_err(|e| fail(&e))?
            .reply()
            .map_err(|e| fail(&e))?
            .atom;
        // Source 2 is a pager: a request from outside the application, which
        // a manager's focus-stealing prevention lets through.
        let event = ClientMessageEvent::new(32, xid, active, [2u32, 0, 0, 0, 0]);
        conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )
        .map_err(|e| fail(&e))?
        .check()
        .map_err(|e| fail(&e))?;
    } else {
        conn.configure_window(xid, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))
            .map_err(|e| fail(&e))?
            .check()
            .map_err(|e| fail(&e))?;
        conn.set_input_focus(InputFocus::PARENT, xid, x11rb::CURRENT_TIME)
            .map_err(|e| fail(&e))?
            .check()
            .map_err(|e| fail(&e))?;
    }
    conn.flush().map_err(|e| fail(&e))?;
    Ok(())
}

/// Where keystrokes go now, as the X server says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyTarget {
    /// The root window, or no window: keys reach no application.
    Nobody,
    /// A window of this process.
    Process(u32),
    /// A window whose owner cannot be read — or the server could not be
    /// asked. Never taken to mean "nobody".
    Unnamed,
}

/// Without a window manager the input focus stays `PointerRoot`, and keys go
/// to the top-level window under the pointer; with one, the focus is a
/// window. The owner is read from `_NET_WM_PID` (GTK and Qt set it),
/// climbing from an inner window to the top-level one that carries it.
fn keyboard_target(display: u32) -> KeyTarget {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};

    let ask = || -> Option<KeyTarget> {
        let (conn, screen) = x11rb::connect(Some(&format!(":{display}"))).ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        let focus = conn.get_input_focus().ok()?.reply().ok()?.focus;
        // 0 is None and 1 is PointerRoot: keys follow the pointer.
        let mut window = if focus <= 1 || focus == root {
            conn.query_pointer(root).ok()?.reply().ok()?.child
        } else {
            focus
        };
        if window == 0 || window == root {
            return Some(KeyTarget::Nobody);
        }
        let pid_atom = conn
            .intern_atom(true, b"_NET_WM_PID")
            .ok()?
            .reply()
            .ok()?
            .atom;
        if pid_atom == 0 {
            return None;
        }
        for _ in 0..32 {
            let property = conn
                .get_property(false, window, pid_atom, AtomEnum::CARDINAL, 0, 1)
                .ok()?
                .reply()
                .ok()?;
            if let Some(pid) = property.value32().and_then(|mut values| values.next()) {
                return Some(KeyTarget::Process(pid));
            }
            window = conn.query_tree(window).ok()?.reply().ok()?.parent;
            if window == 0 || window == root {
                return None;
            }
        }
        None
    };
    ask().unwrap_or(KeyTarget::Unnamed)
}

/// Why a window's rectangle is not where it is on screen, when it is not:
/// GTK 4 reports no screen positions at all, for its windows as for their
/// controls. Named, so a capture of "that window" is refused rather than
/// cut from wherever the rectangle happens to say.
pub(crate) fn window_position_unknown(window: &WindowInfo) -> Option<String> {
    // A window with no tree is placed by the X server itself.
    if !window.has_tree {
        return None;
    }
    let bus_name = window.id.split('|').next()?;
    let bus = a11y_bus().ok()?;
    let toolkit = toolkit(&bus, bus_name);
    toolkit
        .to_ascii_lowercase()
        .starts_with("gtk 4")
        .then(|| format!("{toolkit} does not report where its windows are on screen,"))
}

/// Every window on the display — the accessibility tree's and the X
/// server's — so the checks that keep clicks and captures off a blocked
/// application see a window whether or not it has a tree.
pub(crate) fn list_windows() -> Result<Vec<WindowInfo>, DesktopError> {
    Ok(desktop()?.windows.into_iter().map(|w| w.info).collect())
}

/// The window keystrokes reach. When that is not one listed window — a
/// window that says nothing about itself, or a program with several accessible
/// windows and no X window paired — it is still reported, by its owner, so
/// the checks on what may be typed into still apply: an unnamed owner is
/// treated as blocked. `None` only when keys reach no window at all.
pub(crate) fn foreground() -> Result<Option<WindowInfo>, DesktopError> {
    let desktop = desktop()?;
    if let Some(focused) = desktop.windows.iter().find(|w| w.info.focused) {
        return Ok(Some(focused.info.clone()));
    }
    let (pid, app) = match keyboard_target(desktop.display) {
        KeyTarget::Nobody => return Ok(None),
        KeyTarget::Process(pid) => (pid, program_of(pid)),
        KeyTarget::Unnamed => (0, String::new()),
    };
    Ok(Some(WindowInfo {
        id: format!("x11-keyboard:{pid}"),
        title: String::new(),
        app,
        pid,
        focused: true,
        rect: Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        has_tree: false,
    }))
}

/// The window `window` names on this display, or the focused one — refusing
/// and naming the candidates rather than guessing between several.
fn target<'d>(
    desktop: &'d Desktop,
    window: Option<&str>,
) -> Result<&'d Listed<Node>, DesktopError> {
    let display = desktop.display;
    let windows = &desktop.windows;
    if let Some(id) = window {
        if id.trim().starts_with("x11:") && x11::parse_id(id).is_none() {
            return Err(DesktopError::Os(format!(
                "\"{id}\" is not an X window id (x11:0x…); take one from list_windows"
            )));
        }
        return windows.iter().find(|w| w.info.id == id).ok_or_else(|| {
            DesktopError::Os(format!(
                "no window {id} is showing on display :{display}; take one from list_windows"
            ))
        });
    }
    if let Some(focused) = windows.iter().find(|w| w.info.focused) {
        return Ok(focused);
    }
    Err(DesktopError::Os(if windows.is_empty() {
        format!(
            "no window is showing on display :{display}. Start a program there (DISPLAY=:{display})."
        )
    } else {
        // A blocked window is named by its id alone: its title is exactly
        // what is never read out of it.
        let names: Vec<String> = windows
            .iter()
            .map(|w| match super::limits::get().blocked(&w.info.app) {
                Some(_) => format!("a blocked application's window ({})", w.info.id),
                None => format!("\"{}\" ({})", w.info.title, w.info.id),
            })
            .collect();
        format!(
            "no window is focused on display :{display} and {} are showing ({}); pass one \
             as `window`",
            names.len(),
            names.join(", ")
        )
    }))
}

/// The accessible window behind a listed one and the bus it is reached on —
/// or, for a window only the X server knows, the refusal that points at the
/// pixel tools. Named by id: a blocked window's title is never read out.
fn accessible<'d>(
    desktop: &'d Desktop,
    listed: &'d Listed<Node>,
) -> Result<(&'d Connection, &'d Node), DesktopError> {
    // Pointing a blocked window at the pixel tools would point at a refusal.
    if listed.node.is_none() && super::limits::get().blocked(&listed.info.app).is_some() {
        return Err(DesktopError::Os(format!(
            "refused: window {} belongs to a blocked application, and incode does not read or \
             act on it",
            listed.info.id
        )));
    }
    match (&listed.node, &desktop.bus) {
        (Some(node), Ok(bus)) => Ok((bus, node)),
        (_, Err(why)) => Err(DesktopError::Os(format!(
            "window {} has no accessibility tree to read: {why}. Use screenshot (with this \
             window) and click instead.",
            listed.info.id
        ))),
        (None, Ok(_)) => Err(DesktopError::Os(format!(
            "window {} exposes no accessibility tree (its program has no accessibility \
             bridge); use screenshot (with this window) and click instead.",
            listed.info.id
        ))),
    }
}

pub(crate) fn window_info(window: Option<&str>) -> Result<WindowInfo, DesktopError> {
    let desktop = desktop()?;
    Ok(target(&desktop, window)?.info.clone())
}

pub(crate) fn tree(window: Option<&str>, max_depth: u32) -> Result<Tree, DesktopError> {
    let desktop = desktop()?;
    let listed = target(&desktop, window)?;
    let (bus, window) = accessible(&desktop, listed)?;
    let walked = uia::walk(
        window,
        max_depth,
        &|node: &Node, id, depth| describe(bus, node, id, depth),
        &|node: &Node| children(bus, node),
    );
    let no_positions = positions_are_unreal(&walked.elements).then(|| {
        // The window's bus name is its application's.
        format!(
            "{} does not report where its controls are on screen (every one claims the same \
             corner).",
            toolkit(bus, &window.0)
        )
    });
    let info = listed.info.clone();
    Ok(Tree {
        root_name: info.title,
        window: info.id,
        app: info.app,
        no_positions,
        elements: walked.elements,
        truncated: walked.truncated,
        offscreen: walked.offscreen,
    })
}

/// The live element with this id in the window, re-found by walking again,
/// with the bus it is reached on.
fn control<'d>(
    desktop: &'d Desktop,
    window: Option<&str>,
    want: &uia::Want,
    max_depth: u32,
) -> Result<(&'d Connection, Node, Element), DesktopError> {
    let listed = target(desktop, window)?;
    let (bus, root) = accessible(desktop, listed)?;
    let (node, element) = uia::find(
        root,
        max_depth,
        &|node: &Node, id, depth| describe(bus, node, id, depth),
        &|node: &Node| children(bus, node),
        want,
    )
    .ok_or_else(|| uia::gone(want.id, &listed.info))?;
    Ok((bus, node, element))
}

/// Call a method on one element and read its one return value.
fn ask<R, B>(
    bus: &Connection,
    node: &Node,
    interface: &str,
    method: &str,
    body: &B,
) -> Result<R, String>
where
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    bus.call_method(
        Some(node.0.as_str()),
        node.1.as_str(),
        Some(interface),
        method,
        body,
    )
    .and_then(|reply| reply.body().deserialize::<R>())
    .map_err(|e| e.to_string())
}

pub(crate) fn set_value(
    window: Option<&str>,
    want: &uia::Want,
    max_depth: u32,
    value: &str,
) -> Result<Done, DesktopError> {
    let desktop = desktop()?;
    let (bus, node, described) = control(&desktop, window, want, max_depth)?;
    let what = described.label();
    let set: bool = ask(
        bus,
        &node,
        "org.a11y.atspi.EditableText",
        "SetTextContents",
        &(value,),
    )
    .map_err(|e| {
        DesktopError::Os(format!(
            "{what} has no editable text ({e}); click_element it and use type_text instead"
        ))
    })?;
    if !set {
        return Err(DesktopError::Os(format!("{what} refused the new text")));
    }
    let did = described.set_to(value);
    if described.secret {
        return Ok(Done::unverified(did, "a password field is never read back"));
    }
    Ok(
        match ask::<String, _>(bus, &node, "org.a11y.atspi.Text", "GetText", &(0i32, -1i32)) {
            Ok(now) if now == value => Done::verified(did),
            Ok(now) => Done::unverified(
                did,
                &format!(
                    "it reads back as {} characters, not the {} sent",
                    now.chars().count(),
                    value.chars().count()
                ),
            ),
            Err(_) => Done::unverified(did, "its text could not be read back"),
        },
    )
}

/// An AT-SPI action name for one of the shared vocabulary's, among those the
/// element offers. Pure.
pub(crate) fn pick_action(wanted: &str, offered: &[String]) -> Option<usize> {
    let wanted = wanted.trim().to_ascii_lowercase();
    let synonyms: &[&str] = match wanted.as_str() {
        "press" | "invoke" | "click" => &["click", "press", "activate", "jump"],
        "toggle" => &["toggle", "click", "press"],
        "expand" | "collapse" => &["expand or contract", "expand", "collapse"],
        "select" => &["select", "click"],
        "default" => &["click", "press", "activate"],
        _ => &[],
    };
    let lower: Vec<String> = offered.iter().map(|o| o.to_ascii_lowercase()).collect();
    synonyms
        .iter()
        .chain(std::iter::once(&wanted.as_str()))
        .find_map(|name| lower.iter().position(|o| o == name))
}

pub(crate) fn act(
    window: Option<&str>,
    want: &uia::Want,
    max_depth: u32,
    action: &str,
) -> Result<Done, DesktopError> {
    let desktop = desktop()?;
    let (bus, node, described) = control(&desktop, window, want, max_depth)?;
    let what = described.label();
    if action.trim().eq_ignore_ascii_case("focus") {
        let ok: bool = ask(bus, &node, "org.a11y.atspi.Component", "GrabFocus", &())
            .map_err(|e| DesktopError::Os(format!("{what} cannot take focus: {e}")))?;
        let focused = has_state(&states(bus, &node), STATE_FOCUSED);
        return Ok(if ok && focused {
            Done::verified(format!("focused {what}"))
        } else {
            Done::unverified(
                format!("asked {what} to take focus"),
                "it does not report focus",
            )
        });
    }
    let offered: Vec<(String, String, String)> =
        ask(bus, &node, "org.a11y.atspi.Action", "GetActions", &()).unwrap_or_default();
    let names: Vec<String> = offered.into_iter().map(|(name, _, _)| name).collect();
    let Some(index) = pick_action(action, &names) else {
        return Err(DesktopError::Os(if names.is_empty() {
            format!("{what} offers no actions; use click_element or set_value")
        } else {
            format!(
                "{what} does not offer \"{action}\"; it offers: {}, focus",
                names.join(", ")
            )
        }));
    };
    let ok: bool = ask(
        bus,
        &node,
        "org.a11y.atspi.Action",
        "DoAction",
        &(index as i32),
    )
    .map_err(|e| DesktopError::Os(format!("could not {action} {what}: {e}")))?;
    if !ok {
        return Err(DesktopError::Os(format!(
            "{what} refused \"{}\"",
            names[index]
        )));
    }
    Ok(Done::unverified(
        format!("ran \"{}\" on {what}", names[index]),
        "an action has no state to read back; look at the result",
    ))
}

/// Bring a window forward and give it the keyboard. Its X window, when it
/// has one, is focused through the X server (see [`give_focus`]) — the one
/// way to reach a window with no tree, and the one way keys move on a
/// display with no window manager, where the input focus otherwise stays
/// wherever it was last put. An accessible window is asked as well.
///
/// Verified by asking again where keys go: the X input focus inside the
/// window, or the window reporting itself active.
pub(crate) fn focus_window(window: &str) -> Result<Done, DesktopError> {
    let desktop = desktop()?;
    let listed = target(&desktop, Some(window))?;
    let title = &listed.info.title;
    if let Some(xid) = listed.xid {
        give_focus(desktop.display, xid, desktop.window_manager)?;
    }
    if let (Some(node), Ok(bus)) = (&listed.node, &desktop.bus) {
        let _ = ask::<bool, _>(bus, node, "org.a11y.atspi.Component", "GrabFocus", &());
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
    let has_keys = match listed.xid {
        Some(xid) => x_windows(desktop.display).is_ok_and(|now| now.focus_chain.contains(&xid)),
        None => false,
    };
    let active = match (&listed.node, &desktop.bus) {
        (Some(node), Ok(bus)) => has_state(&states(bus, node), STATE_ACTIVE),
        _ => false,
    };
    Ok(if has_keys || active {
        Done::verified(format!("focused window \"{title}\""))
    } else {
        Done::unverified(
            format!("asked window \"{title}\" to take the focus"),
            if desktop.window_manager {
                "the window manager has not given it the keyboard (yet); click in it, or call \
                 focus_window again"
            } else {
                "it does not report itself active and no X window could be matched to it; click \
                 in it instead"
            },
        )
    })
}

/// The clipboard's text now, through `xclip` or else `xsel`: `None` when it
/// holds no text (both fail when nothing offers text), an error when it
/// could not be read in time.
///
/// The output is drained on its own thread WHILE the program runs: a
/// clipboard larger than the pipe's buffer (64 KiB) would otherwise leave
/// the program blocked writing and the wait for its exit would time out.
pub(crate) fn get_clipboard() -> Result<Option<String>, String> {
    let programs: [(&str, &[&str]); 2] = [
        ("xclip", &["-selection", "clipboard", "-out"]),
        ("xsel", &["--clipboard", "--output"]),
    ];
    for (program, args) in programs {
        let mut child = match Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("could not run {program}: {e}")),
        };
        let reader = child.stdout.take().map(|mut stdout| {
            std::thread::spawn(move || {
                let mut out = Vec::new();
                stdout.read_to_end(&mut out).map(|_| out)
            })
        });
        let timeout = super::limits::get().clipboard_timeout;
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{program} did not hand over the clipboard within {}s",
                        timeout.as_secs()
                    ));
                }
                Err(e) => return Err(format!("{program}: {e}")),
            }
        };
        if !status.success() {
            return Ok(None);
        }
        let bytes = match reader.map(std::thread::JoinHandle::join) {
            Some(Ok(Ok(bytes))) => bytes,
            _ => return Err(format!("could not read {program}'s output")),
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        return Ok((!text.is_empty()).then_some(text));
    }
    Err("neither xclip nor xsel is installed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **THE DISPLAY IS NAMED, OR NOTHING STARTS.** Every accepted form reads
    /// as what it says; a size out of range or a word nobody uses is refused
    /// with the forms that work.
    #[test]
    fn the_display_setting_reads_every_form_and_refuses_the_rest() {
        assert_eq!(parse_setting(":99"), Ok(DisplaySetting::Existing(99)));
        assert_eq!(parse_setting(" :0.0 "), Ok(DisplaySetting::Existing(0)));
        assert_eq!(
            parse_setting("xvfb"),
            Ok(DisplaySetting::Private {
                width: DEFAULT_XVFB_SIZE.0,
                height: DEFAULT_XVFB_SIZE.1
            })
        );
        assert_eq!(
            parse_setting("xvfb:1280x800"),
            Ok(DisplaySetting::Private {
                width: 1280,
                height: 800
            })
        );
        for bad in [
            "xvfb:12x800",
            "xvfb:1280",
            "xvfbx",
            "host:0",
            "wayland-0",
            "99",
        ] {
            let why = parse_setting(bad).expect_err(bad);
            assert!(why.contains(bad), "{why}");
        }
    }

    /// Only a LOCAL display is one this server drives.
    #[test]
    fn only_a_local_display_has_a_number() {
        assert_eq!(display_number(":99"), Some(99));
        assert_eq!(display_number(":99.1"), Some(99));
        assert_eq!(display_number("unix:7"), Some(7));
        assert_eq!(display_number("remote:0"), None);
        assert_eq!(display_number(":"), None);
        assert_eq!(display_number(":x"), None);
    }

    /// An unset setting is a refusal that says what to set.
    #[test]
    fn no_setting_is_a_refusal_not_the_operators_desktop() {
        for unset in [None, Some(""), Some("  ")] {
            let why = attach(unset).err().expect("refused").to_string();
            assert!(why.contains("desktop_display"), "{why}");
            assert!(why.contains("$DISPLAY"), "{why}");
        }
    }

    /// State bits are counted across the words of the set.
    #[test]
    fn a_state_is_read_from_its_own_word_and_bit() {
        let set = [
            1 << STATE_ACTIVE | 1 << STATE_ENABLED,
            1 << (STATE_SHOWING % 32),
        ];
        assert!(has_state(&set, STATE_ACTIVE));
        assert!(has_state(&set, STATE_ENABLED));
        assert!(
            !has_state(&set, STATE_SHOWING),
            "bit 25 is in the FIRST word"
        );
        assert!(has_state(&[0, 1 << 1], 33));
        assert!(!has_state(&[], STATE_ACTIVE));
    }

    fn at(id: u32, role: uia::Role, x: i32, y: i32, width: i32, height: i32) -> Element {
        Element {
            id,
            depth: 1,
            role,
            name: format!("e{id}"),
            rect: Rect {
                x,
                y,
                width,
                height,
            },
            enabled: true,
            secret: false,
        }
    }

    /// The shared action names find the toolkit's own: GTK's buttons say
    /// "click", a check box "toggle" or "click", an expander "expand or
    /// contract"; a name the element does not offer is not invented.
    #[test]
    fn an_action_is_found_by_the_name_the_toolkit_uses() {
        let offered = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(pick_action("press", &offered(&["click"])), Some(0));
        assert_eq!(pick_action("Press", &offered(&["activate"])), Some(0));
        assert_eq!(pick_action("toggle", &offered(&["click"])), Some(0));
        assert_eq!(
            pick_action("expand", &offered(&["activate", "expand or contract"])),
            Some(1)
        );
        assert_eq!(pick_action("press", &offered(&["menu"])), None);
        assert_eq!(
            pick_action("menu", &offered(&["click", "menu"])),
            Some(1),
            "an exact name"
        );
        assert_eq!(pick_action("press", &[]), None);
    }

    /// **A TREE WHOSE CONTROLS ALL CLAIM ONE CORNER IS NOT A MAP.** The GTK4
    /// shape (measured: everything at (0,0)) is refused; the GTK3 shape
    /// (measured: frame at (200,150), its contents inside at (224,174)) is
    /// kept, as is a tree too small to judge.
    #[test]
    fn a_tree_with_every_control_on_one_corner_is_refused() {
        use uia::Role::*;
        let gtk4 = [
            at(0, Window, 0, 0, 310, 257),
            at(1, Text, 0, 0, 252, 22),
            at(2, Button, 0, 0, 120, 44),
            at(3, Button, 0, 0, 120, 44),
        ];
        assert!(positions_are_unreal(&gtk4));

        let gtk3 = [
            at(0, Window, 200, 150, 216, 157),
            at(1, Pane, 224, 174, 168, 109),
            at(2, Text, 224, 174, 168, 17),
            at(3, Edit, 224, 203, 168, 34),
            at(4, Button, 224, 249, 168, 34),
        ];
        assert!(!positions_are_unreal(&gtk3));

        // Two elements, or nothing to click: too little to call it unreal.
        assert!(!positions_are_unreal(&gtk4[..2]));
        assert!(!positions_are_unreal(&[
            at(0, Window, 0, 0, 10, 10),
            at(1, Text, 0, 0, 5, 5),
            at(2, Text, 0, 0, 4, 4),
        ]));
    }

    /// A GTK4 control is enabled though it never says ENABLED: the state
    /// word below is one a live GTK4 dialog reported for a working button.
    #[test]
    fn sensitive_alone_is_enabled_as_gtk4_reports_it() {
        let gtk4_button = [0x4300_0000, 0];
        assert!(!has_state(&gtk4_button, STATE_ENABLED));
        assert!(is_enabled(&gtk4_button));
        assert!(is_enabled(&[1 << STATE_ENABLED, 0]), "GTK3's ENABLED");
        assert!(
            !is_enabled(&[1 << STATE_SHOWING, 0]),
            "showing is not enabled"
        );
    }
}
