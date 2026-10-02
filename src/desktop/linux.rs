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

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use zbus::blocking::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::uia::{self, Done, Element, Tree, WindowInfo};
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

/// Every window SHOWING on this server's display, with what is known of it.
/// Only applications whose own `DISPLAY` is this server's are looked at: the
/// accessibility bus lists every application of the session, the
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
            };
            out.push((window, info));
        }
    }
    // A bare Xvfb has no window manager to mark a window active. The X
    // server itself says whose window keystrokes reach; the one showing
    // window of that process is the focused one — and if it has several, or
    // the owner cannot be named, none is, rather than a guess.
    if !out.iter().any(|(_, w)| w.focused) {
        if let KeyTarget::Process(pid) = keyboard_target(display) {
            let theirs: Vec<usize> = (0..out.len()).filter(|&i| out[i].1.pid == pid).collect();
            if let [only] = theirs[..] {
                out[only].1.focused = true;
            }
        }
    }
    Ok(out)
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
    let bus_name = window.id.split('|').next()?;
    let bus = a11y_bus().ok()?;
    let toolkit = toolkit(&bus, bus_name);
    toolkit
        .to_ascii_lowercase()
        .starts_with("gtk 4")
        .then(|| format!("{toolkit} does not report where its windows are on screen,"))
}

pub(crate) fn list_windows() -> Result<Vec<WindowInfo>, DesktopError> {
    let bus = a11y_bus()?;
    Ok(windows_on(&bus, ours()?)?
        .into_iter()
        .map(|(_, info)| info)
        .collect())
}

/// The window keystrokes reach. When that is not one accessible window —
/// a program with no tree, one with several windows, or a window nobody can
/// name — it is still reported, by its owner, so the checks on what may be
/// typed into still apply: an unnamed owner is treated as blocked. `None`
/// only when keys reach no window at all.
pub(crate) fn foreground() -> Result<Option<WindowInfo>, DesktopError> {
    let bus = a11y_bus()?;
    let display = ours()?;
    let windows = windows_on(&bus, display)?;
    if let Some((_, focused)) = windows.iter().find(|(_, w)| w.focused) {
        return Ok(Some(focused.clone()));
    }
    let (pid, app) = match keyboard_target(display) {
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
    }))
}

/// The window `window` names on this display, or the focused one — refusing
/// and naming the candidates rather than guessing between several.
fn target(bus: &Connection, window: Option<&str>) -> Result<(Node, WindowInfo), DesktopError> {
    let display = ours()?;
    let mut windows = windows_on(bus, display)?;
    if let Some(id) = window {
        return windows
            .into_iter()
            .find(|(_, info)| info.id == id)
            .ok_or_else(|| {
                DesktopError::Os(format!(
                    "no window {id} is showing on display :{display}; take one from \
                     list_windows"
                ))
            });
    }
    if let Some(at) = windows.iter().position(|(_, info)| info.focused) {
        return Ok(windows.swap_remove(at));
    }
    Err(DesktopError::Os(if windows.is_empty() {
        format!(
            "no application on display :{display} exposes an accessibility tree. Start one \
             there (GTK and Qt applications do); pixels still work: screenshot and click."
        )
    } else {
        // A blocked window is named by its id alone: its title is exactly
        // what is never read out of it.
        let names: Vec<String> = windows
            .iter()
            .map(|(_, w)| match super::limits::get().blocked(&w.app) {
                Some(_) => format!("a blocked application's window ({})", w.id),
                None => format!("\"{}\" ({})", w.title, w.id),
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

pub(crate) fn window_info(window: Option<&str>) -> Result<WindowInfo, DesktopError> {
    let bus = a11y_bus()?;
    Ok(target(&bus, window)?.1)
}

pub(crate) fn tree(window: Option<&str>, max_depth: u32) -> Result<Tree, DesktopError> {
    let bus = a11y_bus()?;
    let (window, info) = target(&bus, window)?;
    let (elements, truncated) = uia::walk(
        &window,
        max_depth,
        &|node: &Node, id, depth| describe(&bus, node, id, depth),
        &|node: &Node| children(&bus, node),
    );
    let no_positions = positions_are_unreal(&elements).then(|| {
        // The window's bus name is its application's.
        format!(
            "{} does not report where its controls are on screen (every one claims the same \
             corner).",
            toolkit(&bus, &window.0)
        )
    });
    Ok(Tree {
        root_name: info.title,
        window: info.id,
        app: info.app,
        no_positions,
        elements,
        truncated,
    })
}

/// The live element with this id in the window, re-found by walking again.
fn control(
    bus: &Connection,
    window: Option<&str>,
    want: &uia::Want,
    max_depth: u32,
) -> Result<(Node, Element), DesktopError> {
    let (root, info) = target(bus, window)?;
    uia::find(
        &root,
        max_depth,
        &|node: &Node, id, depth| describe(bus, node, id, depth),
        &|node: &Node| children(bus, node),
        want,
    )
    .ok_or_else(|| uia::gone(want.id, &info))
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
    let bus = a11y_bus()?;
    let (node, described) = control(&bus, window, want, max_depth)?;
    let what = described.label();
    let set: bool = ask(
        &bus,
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
    if described.secret {
        return Ok(Done::unverified(
            format!("set {what}"),
            "a password field is never read back",
        ));
    }
    Ok(
        match ask::<String, _>(
            &bus,
            &node,
            "org.a11y.atspi.Text",
            "GetText",
            &(0i32, -1i32),
        ) {
            Ok(now) if now == value => Done::verified(format!("set {what}")),
            Ok(now) => Done::unverified(
                format!("set {what}"),
                &format!(
                    "it reads back as {} characters, not the {} sent",
                    now.chars().count(),
                    value.chars().count()
                ),
            ),
            Err(_) => Done::unverified(format!("set {what}"), "its text could not be read back"),
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
    let bus = a11y_bus()?;
    let (node, described) = control(&bus, window, want, max_depth)?;
    let what = described.label();
    if action.trim().eq_ignore_ascii_case("focus") {
        let ok: bool = ask(&bus, &node, "org.a11y.atspi.Component", "GrabFocus", &())
            .map_err(|e| DesktopError::Os(format!("{what} cannot take focus: {e}")))?;
        let focused = has_state(&states(&bus, &node), STATE_FOCUSED);
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
        ask(&bus, &node, "org.a11y.atspi.Action", "GetActions", &()).unwrap_or_default();
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
        &bus,
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

pub(crate) fn focus_window(window: &str) -> Result<Done, DesktopError> {
    let bus = a11y_bus()?;
    let (node, info) = target(&bus, Some(window))?;
    let _ = ask::<bool, _>(&bus, &node, "org.a11y.atspi.Component", "GrabFocus", &());
    std::thread::sleep(std::time::Duration::from_millis(150));
    let now = has_state(&states(&bus, &node), STATE_ACTIVE);
    Ok(if now {
        Done::verified(format!("focused window \"{}\"", info.title))
    } else {
        Done::unverified(
            format!("asked window \"{}\" to take the focus", info.title),
            "it does not report itself active (a display with no window manager has no \
             active window); click in it instead",
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
