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

use super::uia::{self, Element, Tree};
use super::{DesktopError, Rect};

/// The size of a private display when the setting names none.
pub const DEFAULT_XVFB_SIZE: (u32, u32) = (1920, 1080);

/// How long a private `Xvfb` has to report that it is ready.
const XVFB_START_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the clipboard program has to take the text.
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);

/// Each accessibility call: a hung application must cost a second, not the
/// whole walk.
const ATSPI_CALL_TIMEOUT: Duration = Duration::from_secs(1);

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
    let line = match rx.recv_timeout(XVFB_START_TIMEOUT) {
        Ok(line) => line,
        Err(_) => {
            return Err(fail(
                &mut child,
                format!(
                    "Xvfb did not report a display within {}s",
                    XVFB_START_TIMEOUT.as_secs()
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
        let deadline = Instant::now() + CLIPBOARD_TIMEOUT;
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
                        CLIPBOARD_TIMEOUT.as_secs()
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
                .and_then(|b| b.method_timeout(ATSPI_CALL_TIMEOUT).build())
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
        .and_then(|b| b.method_timeout(ATSPI_CALL_TIMEOUT).build())
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
    let role = bus
        .call_method(
            Some(node.0.as_str()),
            node.1.as_str(),
            Some(ACCESSIBLE),
            "GetRoleName",
            &(),
        )
        .and_then(|reply| reply.body().deserialize::<String>())
        .map(|role| uia::role_of_atspi(&role))
        .unwrap_or(uia::Role::Other);
    // Screen coordinates (`ATSPI_COORD_TYPE_SCREEN` = 0): X11 pixels, the
    // space the mouse uses. An object with no Component has no area and is
    // dropped by the walk's filter, though its children are still walked.
    let rect = bus
        .call_method(
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
        });
    Element {
        id,
        depth,
        role,
        name: name(bus, node),
        rect,
        enabled: is_enabled(&states(bus, node)),
    }
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

/// The display number a process was started for, from its environment.
fn display_of_process(bus: &Connection, bus_name: &str) -> Option<u32> {
    let pid = bus
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetConnectionUnixProcessID",
            &(bus_name,),
        )
        .and_then(|reply| reply.body().deserialize::<u32>())
        .ok()?;
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

/// The active window on this server's display, walked.
///
/// Active is AT-SPI's word for the window that has focus. A bare `Xvfb` has
/// no window manager to make one active, so when none is, the one window
/// showing on the display is taken — and when several are, the walk refuses
/// and names them rather than picking one.
pub(crate) fn focused_window_tree(max_depth: u32) -> Result<Tree, DesktopError> {
    let ours = *ATTACHED.get().ok_or_else(|| {
        DesktopError::Os("no display is attached; see `desktop_display`".to_string())
    })?;
    let bus = a11y_bus()?;
    let root: Node = (
        "org.a11y.atspi.Registry".to_string(),
        OwnedObjectPath::try_from("/org/a11y/atspi/accessible/root")
            .map_err(|e| DesktopError::Os(format!("bad registry path: {e}")))?,
    );

    let mut showing: Vec<(Node, String)> = Vec::new();
    let mut active: Option<(Node, String)> = None;
    for app in children(&bus, &root) {
        if display_of_process(&bus, &app.0) != Some(ours) {
            continue;
        }
        for window in children(&bus, &app) {
            let set = states(&bus, &window);
            let title = name(&bus, &window);
            if has_state(&set, STATE_ACTIVE) {
                active = Some((window.clone(), title.clone()));
            }
            if has_state(&set, STATE_SHOWING) {
                showing.push((window, title));
            }
        }
    }
    let (window, root_name) = match (active, showing.len()) {
        (Some(found), _) => found,
        (None, 1) => showing.remove(0),
        (None, 0) => {
            return Err(DesktopError::Os(format!(
                "no application on display :{ours} exposes an accessibility tree. Start one \
                 there (GTK and Qt applications do); pixels still work: screenshot and click."
            )))
        }
        (None, _) => {
            let names: Vec<String> = showing.iter().map(|(_, t)| format!("\"{t}\"")).collect();
            return Err(DesktopError::Os(format!(
                "no window is active on display :{ours} and {} are showing ({}); click one \
                 to focus it, then ask again",
                names.len(),
                names.join(", ")
            )));
        }
    };

    let (elements, truncated) = uia::walk(
        &window,
        max_depth,
        &|node: &Node, id, depth| describe(&bus, node, id, depth),
        &|node: &Node| children(&bus, node),
    );
    if positions_are_unreal(&elements) {
        // The window's bus name is its application's.
        return Err(DesktopError::Os(format!(
            "\"{root_name}\" ({}) does not report where its controls are on screen — every \
             one claims the same corner — so none can be offered to click_element. Use \
             screenshot and click(x, y); typing and keys work as usual.",
            toolkit(&bus, &window.0)
        )));
    }
    Ok(Tree {
        root_name,
        elements,
        truncated,
    })
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
        }
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
