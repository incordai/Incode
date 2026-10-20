//! Linux: the X server's own top-level windows, beside the accessibility
//! tree's.
//!
//! # Why both lists
//!
//! AT-SPI lists only programs that have an accessibility bridge. xterm, and
//! any program built without one, is on screen and absent from it — and a
//! window that is not listed has no id, so nothing could focus it, picture it
//! or keep a click out of it. The X server knows every window on its display;
//! AT-SPI knows the controls inside some of them. A window both know is listed
//! once, with its tree; a window only the X server knows is listed with
//! `has_tree: false` and an id of its own (`x11:0x40000c`), and the pixel tools
//! are its way in.
//!
//! # What makes two entries the same window
//!
//! The same process (`_NET_WM_PID`) and rectangles that agree. Never the title:
//! two windows of one program often share one, and a title is what the program
//! chooses to say. An X window whose owner cannot be read is never paired, and
//! its name is empty — so it is blocked as `limits::UNNAMED_APP`, exactly as an
//! accessible window with an unreadable owner is.
//!
//! Pure, so it is tested on every platform; `linux.rs` asks the X server and
//! hands the answers here.

use super::uia::WindowInfo;
use super::Rect;

/// What an id of a window only the X server knows starts with. An AT-SPI id
/// starts with a bus name (`:1.42|…`), so the two never collide.
const PREFIX: &str = "x11:";

/// One top-level window, as the X server describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct X11Window {
    pub xid: u32,
    /// `_NET_WM_NAME`, else `WM_NAME`.
    pub title: String,
    /// `WM_CLASS`: the instance and class names the program gave the window.
    pub class: Vec<String>,
    /// `_NET_WM_PID`, when the window carries it.
    pub pid: Option<u32>,
    /// The owner's program name as the kernel keeps it; empty when there is
    /// no pid or it could not be read.
    pub program: String,
    /// On screen, in the pixels input uses.
    pub rect: Rect,
}

/// One window of the merged list: what `list_windows` reports, and the
/// handles behind it.
#[derive(Debug, Clone)]
pub(crate) struct Listed<N> {
    pub info: WindowInfo,
    /// The accessible window, when AT-SPI has one: what `ui_tree` walks.
    pub node: Option<N>,
    /// The X window, when the X server has one paired with it: what
    /// `focus_window` raises and gives the keyboard to.
    pub xid: Option<u32>,
}

/// `x11:0x40000c` — the X server's own number for the window, in the hex
/// `xwininfo` and `xprop` print, so an operator can match it by eye.
pub(crate) fn id_of(xid: u32) -> String {
    format!("{PREFIX}{xid:#x}")
}

/// The X window an `x11:0x…` id names; `None` for any other id, a malformed
/// one, or window 0, which is no window.
pub(crate) fn parse_id(id: &str) -> Option<u32> {
    let hex = id.trim().strip_prefix(PREFIX)?;
    let digits = hex.strip_prefix("0x").or_else(|| hex.strip_prefix("0X"))?;
    u32::from_str_radix(digits, 16).ok().filter(|&xid| xid != 0)
}

/// The name the blocked-application checks read for a window: the owning
/// program, with each `WM_CLASS` name beside it that says something else — so
/// a password manager running under a generic interpreter (`java`, `python3`)
/// is still caught by the name it gives its window. Empty when the owner
/// cannot be read, which blocks it: `WM_CLASS` alone is what a window says
/// about itself, not which process owns it.
pub(crate) fn app_of(window: &X11Window) -> String {
    let program = window.program.trim();
    if program.is_empty() {
        return String::new();
    }
    let lower = program.to_lowercase();
    let mut extra: Vec<&str> = Vec::new();
    for name in &window.class {
        let name = name.trim();
        let low = name.to_lowercase();
        let said = name.is_empty()
            || low.contains(&lower)
            || lower.contains(&low)
            || extra.iter().any(|e| e.eq_ignore_ascii_case(name));
        if !said {
            extra.push(name);
        }
    }
    if extra.is_empty() {
        program.to_string()
    } else {
        format!("{program} ({})", extra.join(", "))
    }
}

/// How far two rectangles are the same one: the shared area over the area
/// either covers, from 0 (apart) to 1 (identical). In i64, so no rectangle
/// overflows it.
fn agreement(a: Rect, b: Rect) -> f64 {
    let edges = |r: Rect| {
        let (x, y) = (i64::from(r.x), i64::from(r.y));
        (
            x,
            y,
            x + i64::from(r.width.max(0)),
            y + i64::from(r.height.max(0)),
        )
    };
    let (l1, t1, r1, b1) = edges(a);
    let (l2, t2, r2, b2) = edges(b);
    let shared = (r1.min(r2) - l1.max(l2)).max(0) * (b1.min(b2) - t1.max(t2)).max(0);
    let area = |l: i64, t: i64, r: i64, b: i64| (r - l) * (b - t);
    let union = area(l1, t1, r1, b1) + area(l2, t2, r2, b2) - shared;
    if union <= 0 {
        return 0.0;
    }
    shared as f64 / union as f64
}

/// Rectangles that share at least this much of their combined area are one
/// window. Half: a window manager's frame or a toolkit's border moves an edge
/// by a few pixels, while a dialog over its own main window shares far less.
const SAME_WINDOW: f64 = 0.5;

/// The accessible windows, each paired with the X window of the same process
/// whose rectangle agrees with it, followed by every X window nothing paired
/// — those listed as windows of their own, with no tree.
///
/// Pairs are taken best first, so a dialog over its own main window pairs with
/// its own X window and not the larger one under it. A toolkit that reports
/// no screen positions (GTK 4, every rectangle at the origin) still pairs by
/// size, when exactly one window of that process has it.
///
/// A paired window keeps AT-SPI's description, except that its app is also
/// read the X way ([`app_of`]), which only ever adds a name the blocked list
/// can match.
pub(crate) fn merge<N>(accessible: Vec<(N, WindowInfo)>, x11: &[X11Window]) -> Vec<Listed<N>> {
    let mut candidates: Vec<(f64, usize, usize)> = Vec::new();
    for (a, (_, info)) in accessible.iter().enumerate() {
        for (x, window) in x11.iter().enumerate() {
            if window.pid != Some(info.pid) || info.pid == 0 {
                continue;
            }
            let score = agreement(info.rect, window.rect);
            if score >= SAME_WINDOW {
                candidates.push((score, a, x));
            }
        }
    }
    candidates.sort_by(|p, q| q.0.total_cmp(&p.0));

    let mut pair_of_accessible: Vec<Option<usize>> = vec![None; accessible.len()];
    let mut paired_x11 = vec![false; x11.len()];
    for (_, a, x) in candidates {
        if pair_of_accessible[a].is_none() && !paired_x11[x] {
            pair_of_accessible[a] = Some(x);
            paired_x11[x] = true;
        }
    }
    for (a, (_, info)) in accessible.iter().enumerate() {
        if pair_of_accessible[a].is_some() {
            continue;
        }
        let same_size: Vec<usize> = (0..x11.len())
            .filter(|&x| {
                !paired_x11[x]
                    && info.pid != 0
                    && x11[x].pid == Some(info.pid)
                    && (x11[x].rect.width, x11[x].rect.height)
                        == (info.rect.width, info.rect.height)
            })
            .collect();
        if let [only] = same_size[..] {
            pair_of_accessible[a] = Some(only);
            paired_x11[only] = true;
        }
    }

    let mut out: Vec<Listed<N>> = accessible
        .into_iter()
        .zip(pair_of_accessible)
        .map(|((node, mut info), pair)| {
            let xid = pair.map(|x| {
                let named = app_of(&x11[x]);
                if !named.is_empty() {
                    info.app = named;
                }
                x11[x].xid
            });
            Listed {
                info,
                node: Some(node),
                xid,
            }
        })
        .collect();
    out.extend(
        x11.iter()
            .zip(paired_x11)
            .filter(|(_, paired)| !paired)
            .map(|(window, _)| Listed {
                info: WindowInfo {
                    id: id_of(window.xid),
                    title: window.title.clone(),
                    app: app_of(window),
                    pid: window.pid.unwrap_or(0),
                    focused: false,
                    rect: window.rect,
                    has_tree: false,
                },
                node: None,
                xid: Some(window.xid),
            }),
    );
    out
}

/// Which listed window keystrokes reach, from the windows between the X
/// input focus and the root — `chain` starts at the window that has the focus
/// (or is under the pointer) and climbs. The first listed X window on it is
/// the one: with a window manager the focus is inside a frame the manager
/// added, and the client window under it is the one listed.
pub(crate) fn focused_index<N>(listed: &[Listed<N>], chain: &[u32]) -> Option<usize> {
    chain
        .iter()
        .find_map(|xid| listed.iter().position(|l| l.xid == Some(*xid)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::limits::{self, DesktopLimits};

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn accessible(id: &str, app: &str, pid: u32, at: Rect) -> (String, WindowInfo) {
        (
            id.to_string(),
            WindowInfo {
                id: id.to_string(),
                title: format!("{id} title"),
                app: app.to_string(),
                pid,
                focused: false,
                rect: at,
                has_tree: true,
            },
        )
    }

    fn x(xid: u32, program: &str, class: &[&str], pid: Option<u32>, at: Rect) -> X11Window {
        X11Window {
            xid,
            title: format!("window {xid:#x}"),
            class: class.iter().map(|c| c.to_string()).collect(),
            pid,
            program: program.to_string(),
            rect: at,
        }
    }

    /// The id round-trips in the hex xwininfo prints; anything else is not an
    /// X window id.
    #[test]
    fn an_x11_id_reads_back_as_the_window_it_names() {
        assert_eq!(id_of(0x40000c), "x11:0x40000c");
        assert_eq!(parse_id("x11:0x40000c"), Some(0x40000c));
        assert_eq!(parse_id(" x11:0X40000C "), Some(0x40000c));
        assert_eq!(parse_id(&id_of(u32::MAX)), Some(u32::MAX));
        for not in [
            "x11:40000c",
            "x11:0x",
            "x11:0x0",
            "x11:0xzz",
            "x11:0x1ffffffff",
            ":1.42|/org/a11y/atspi/accessible/1",
            "0x40000c",
            "",
        ] {
            assert_eq!(parse_id(not), None, "{not}");
        }
    }

    /// **A WINDOW WITH NO TREE IS LISTED, AND A WINDOW WITH ONE IS LISTED
    /// ONCE.** gedit's X window pairs with its accessible window (same pid,
    /// same place) and is not repeated; xterm, which AT-SPI does not know,
    /// is listed by its X id with no tree.
    #[test]
    fn x_windows_merge_into_the_accessible_list_by_pid_and_rectangle() {
        let gedit = accessible(":1.5|/w/1", "gedit", 100, rect(0, 0, 800, 600));
        let windows = [
            x(
                0x200003,
                "gedit",
                &["gedit", "Gedit"],
                Some(100),
                rect(0, 0, 800, 600),
            ),
            x(
                0x40000c,
                "xterm",
                &["xterm", "XTerm"],
                Some(200),
                rect(10, 10, 484, 316),
            ),
        ];
        let listed = merge(vec![gedit], &windows);
        assert_eq!(listed.len(), 2, "{listed:?}");

        assert_eq!(listed[0].info.id, ":1.5|/w/1");
        assert!(listed[0].info.has_tree);
        assert_eq!(listed[0].xid, Some(0x200003));
        assert_eq!(listed[0].node.as_deref(), Some(":1.5|/w/1"));

        let xterm = &listed[1].info;
        assert_eq!(xterm.id, "x11:0x40000c");
        assert!(!xterm.has_tree, "xterm has no accessibility tree");
        assert_eq!(xterm.app, "xterm");
        assert_eq!(xterm.pid, 200);
        assert_eq!(xterm.rect, rect(10, 10, 484, 316));
        assert!(listed[1].node.is_none());
    }

    /// Pairing is by process AND place: another program's window at the same
    /// rectangle is not the accessible one, and a dialog over its own main
    /// window pairs with its own X window, not the larger one beneath.
    #[test]
    fn a_window_pairs_only_with_its_own_process_at_its_own_place() {
        let main = accessible(":1.5|/w/1", "gedit", 100, rect(0, 0, 800, 600));
        let dialog = accessible(":1.5|/w/2", "gedit", 100, rect(250, 200, 300, 200));
        let windows = [
            x(0xa1, "gedit", &[], Some(100), rect(0, 0, 800, 600)),
            x(0xa2, "gedit", &[], Some(100), rect(248, 198, 304, 204)),
            x(0xb1, "other", &[], Some(300), rect(250, 200, 300, 200)),
        ];
        let listed = merge(vec![main, dialog], &windows);
        let pairs: Vec<(&str, Option<u32>)> =
            listed.iter().map(|l| (l.info.id.as_str(), l.xid)).collect();
        assert_eq!(
            pairs,
            vec![
                (":1.5|/w/1", Some(0xa1)),
                (":1.5|/w/2", Some(0xa2)),
                ("x11:0xb1", Some(0xb1)),
            ]
        );
    }

    /// GTK 4 reports every window at the origin; its X window is where it
    /// really is. One window of that process with that size is still the same
    /// window — two of them is a guess, and is not made.
    #[test]
    fn a_window_with_no_real_position_pairs_by_size_only_when_unambiguous() {
        let gtk4 = accessible(":1.9|/w/1", "zenity", 400, rect(0, 0, 310, 257));
        let one = [x(0xc1, "zenity", &[], Some(400), rect(600, 300, 310, 257))];
        let listed = merge(vec![gtk4.clone()], &one);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].xid, Some(0xc1));

        let two = [
            x(0xc1, "zenity", &[], Some(400), rect(600, 300, 310, 257)),
            x(0xc2, "zenity", &[], Some(400), rect(900, 300, 310, 257)),
        ];
        let listed = merge(vec![gtk4], &two);
        assert_eq!(
            listed.len(),
            3,
            "nothing paired, so both X windows stay listed"
        );
        assert_eq!(listed[0].xid, None);
    }

    /// **EVERY BLOCKED-APP CHECK APPLIES TO A WINDOW WITH NO TREE.** A password
    /// manager is caught by its program name or by the WM_CLASS it gives its
    /// window (under an interpreter); an X window whose owner cannot be read
    /// is blocked as unnamed, whatever WM_CLASS it claims.
    #[test]
    fn an_x_window_is_blocked_by_program_or_wm_class_and_unnamed_when_ownerless() {
        let limits = DesktopLimits::default().resolve().expect("defaults");
        let named = |program: &str, class: &[&str], pid: Option<u32>| {
            let listed = merge::<String>(vec![], &[x(0x1, program, class, pid, rect(0, 0, 9, 9))]);
            listed[0].info.app.clone()
        };

        let keepassxc = named("keepassxc", &["keepassxc", "KeePassXC"], Some(7));
        assert_eq!(keepassxc, "keepassxc");
        assert!(limits.blocked(&keepassxc).is_some());

        let under_java = named(
            "java",
            &["sun-awt-X11-XFramePeer", "org-keepass-Main"],
            Some(8),
        );
        assert_eq!(
            under_java,
            "java (sun-awt-X11-XFramePeer, org-keepass-Main)"
        );
        assert!(limits.blocked(&under_java).is_some(), "by WM_CLASS");

        let ownerless = named("", &["KeePassXC"], None);
        assert_eq!(limits.blocked(&ownerless), Some(limits::UNNAMED_APP));
        let unreadable = named("", &["xterm"], Some(9));
        assert_eq!(
            limits.blocked(&unreadable),
            Some(limits::UNNAMED_APP),
            "a pid whose program cannot be read names nobody"
        );

        let xterm = named("xterm", &["xterm", "XTerm"], Some(10));
        assert_eq!(limits.blocked(&xterm), None);
    }

    /// A paired window's app gains the X window's WM_CLASS, so a password
    /// manager that AT-SPI names by its interpreter is still blocked.
    #[test]
    fn pairing_adds_the_wm_class_to_an_accessible_windows_app() {
        let limits = DesktopLimits::default().resolve().expect("defaults");
        let java = accessible(":1.7|/w/1", "java", 500, rect(0, 0, 640, 480));
        let windows = [x(
            0xd1,
            "java",
            &["org-keepass-Main"],
            Some(500),
            rect(0, 0, 640, 480),
        )];
        let listed = merge(vec![java], &windows);
        assert_eq!(listed.len(), 1);
        assert!(
            limits.blocked(&listed[0].info.app).is_some(),
            "{:?}",
            listed[0].info
        );
    }

    /// An X window with no pid is never paired, even at an accessible
    /// window's exact place: who owns it is unknown, so it stays listed — and
    /// blocked — on its own.
    #[test]
    fn an_x_window_with_no_owner_is_never_paired() {
        let gedit = accessible(":1.5|/w/1", "gedit", 100, rect(0, 0, 800, 600));
        let windows = [x(0xe1, "", &["gedit"], None, rect(0, 0, 800, 600))];
        let listed = merge(vec![gedit], &windows);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].xid, None);
        assert_eq!(listed[1].info.app, "");
    }

    /// The focus climbs to the first listed window: with a window manager
    /// the focused window is inside the client, under the manager's frame.
    #[test]
    fn the_focus_is_the_first_listed_window_on_its_way_to_the_root() {
        let windows = [
            x(0xa1, "gedit", &[], Some(100), rect(0, 0, 800, 600)),
            x(0xb1, "xterm", &[], Some(200), rect(0, 0, 400, 300)),
        ];
        let listed = merge::<String>(vec![], &windows);
        assert_eq!(focused_index(&listed, &[0xb17, 0xb1, 0xf00]), Some(1));
        assert_eq!(focused_index(&listed, &[0xa1]), Some(0));
        assert_eq!(focused_index(&listed, &[0x99, 0x98]), None);
        assert_eq!(focused_index(&listed, &[]), None);
    }
}
