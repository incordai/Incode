//! The accessibility tree — clicking a named control instead of a guessed pixel.
//!
//! # Why this exists when `click(x, y)` already works
//!
//! `docs/COMPUTER_USE.md` section 5. A pixel click is a guess derived from a
//! downscaled image: the model reads a button off a 1092-wide picture, the
//! coordinate is mapped back, and the click lands *near* the button. Usually
//! that is enough. When it is not, the failure is silent — something else got
//! clicked — and it looks like the model misread the screen.
//!
//! Windows already knows where every control is. UI Automation exposes a tree
//! of them with names, roles and exact rectangles, so `click_element` can click
//! the true centre of a named button and, before it does, CHECK THE CONTROL IS
//! STILL THERE. That is a different class of operation from a pixel guess: it
//! can be wrong, but it cannot be silently wrong.
//!
//! # What this deliberately does not do
//!
//! It does not replace the screenshot. Canvas content, images, games and
//! custom-drawn UI expose nothing useful here, and Electron and Java apps
//! expose poor trees. The tree is the PRIMARY path and pixels are the fallback;
//! `screen_info` reports which is available so an agent can tell "this window
//! has no tree" from "this machine has no trees".

use serde::{Deserialize, Serialize};

use super::coords::PhysicalPoint;
use super::{DesktopError, Rect};

/// How deep to walk by default.
///
/// Deep enough for real dialogs and settings panes, shallow enough that a
/// browser — whose DOM arrives here as tens of thousands of elements — cannot
/// turn one `ui_tree` call into a context-exhausting reply.
/// admin.json `desktop_limits.tree_depth` changes it.
pub const DEFAULT_DEPTH: u32 = super::limits::DEFAULT_TREE_DEPTH;

/// Most elements returned in one walk, whatever the depth allows, by default;
/// admin.json `desktop_limits.tree_max_elements` changes it.
pub const MAX_ELEMENTS: usize = super::limits::DEFAULT_TREE_MAX_ELEMENTS as usize;

/// The element cap in force.
fn max_elements() -> usize {
    super::limits::get().tree_max_elements
}

/// A control's kind, in a small closed vocabulary.
///
/// NOT the platform's own name. Windows says "Button", macOS says
/// "AXButton", AT-SPI says "push button"; an agent that learned one would
/// misread the others. Mapping at the boundary keeps the tool surface the same
/// on every platform — the same reasoning that keeps `ShotFormat` out of
/// `image::ImageFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Button,
    Edit,
    Text,
    List,
    ListItem,
    CheckBox,
    RadioButton,
    ComboBox,
    Menu,
    MenuItem,
    Tab,
    TabItem,
    Link,
    Image,
    Window,
    Pane,
    Tree,
    TreeItem,
    Table,
    Slider,
    Group,
    Other,
}

impl Role {
    /// Whether clicking this is a normal thing for an agent to want.
    ///
    /// Used to filter the noise out of a tree: a window full of anonymous
    /// `Pane` and `Group` wrappers is structure, not something to act on.
    pub fn is_interactive(self) -> bool {
        matches!(
            self,
            Role::Button
                | Role::Edit
                | Role::ListItem
                | Role::CheckBox
                | Role::RadioButton
                | Role::ComboBox
                | Role::MenuItem
                | Role::TabItem
                | Role::Link
                | Role::TreeItem
                | Role::Slider
        )
    }
}

/// One control.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Element {
    /// Position in this walk's depth-first order. Valid only for the result it
    /// came from — the tree changes as the UI does, so it is never persisted.
    pub id: u32,
    /// Depth below the root, for rendering the tree as text.
    pub depth: u32,
    pub role: Role,
    pub name: String,
    /// Physical pixels, the same space `move_mouse` and `click` take.
    pub rect: Rect,
    pub enabled: bool,
    /// A password or other secret field. Its CONTENT is never read — only
    /// that it is one — so neither a tree nor a journal can carry a password,
    /// and the agent knows what it would be typing into.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub secret: bool,
}

impl Element {
    /// Where a click on this control should land.
    pub fn click_point(&self) -> PhysicalPoint {
        let (x, y) = self.rect.center();
        PhysicalPoint { x, y }
    }

    /// Whether this is worth showing to an agent.
    ///
    /// An unnamed structural wrapper tells it nothing and costs tokens; an
    /// off-screen or zero-sized control cannot be clicked.
    fn is_useful(&self) -> bool {
        if self.rect.width <= 0 || self.rect.height <= 0 {
            return false;
        }
        !self.name.trim().is_empty() || self.role.is_interactive()
    }
}

/// The result of one walk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tree {
    /// The window this describes.
    pub root_name: String,
    /// The window's id, as [`list_windows`] gives it — what `window` takes.
    #[serde(default)]
    pub window: String,
    /// The application that owns the window.
    #[serde(default)]
    pub app: String,
    /// Why the rectangles cannot be clicked, when they cannot — a toolkit
    /// that reports every control at one corner (GTK4). The tree is still
    /// good for `set_value` and `element_action`, which need no position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_positions: Option<String>,
    pub elements: Vec<Element>,
    /// True when the walk stopped at [`MAX_ELEMENTS`] rather than running out
    /// of tree. Reported so an agent can narrow its request instead of assuming
    /// it has seen everything.
    pub truncated: bool,
}

impl Tree {
    /// Render as the indented text an agent reads.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("Window \"{}\"", self.root_name));
        if !self.window.is_empty() {
            out.push_str(&format!(" [window {}, app {}]", self.window, self.app));
        }
        out.push('\n');
        if let Some(why) = &self.no_positions {
            out.push_str(&format!(
                "NOTE: {why} Positions below are not real: use set_value and element_action \
                 by id; click_element, scroll and drag by id are refused.\n"
            ));
        }
        for e in &self.elements {
            out.push_str(&format!(
                "{:indent$}[e{}] {:<10} \"{}\" ({},{},{}x{}){}{}\n",
                "",
                e.id,
                format!("{:?}", e.role),
                e.name,
                e.rect.x,
                e.rect.y,
                e.rect.width,
                e.rect.height,
                if e.enabled { "" } else { "  disabled" },
                if e.secret { "  password" } else { "" },
                indent = (e.depth as usize).min(12) * 2,
            ));
        }
        if self.truncated {
            out.push_str(&format!(
                "\n… stopped at {} elements. Narrow the request.\n",
                max_elements()
            ));
        }
        out
    }

    /// The element with this id, if the walk produced one.
    pub fn get(&self, id: u32) -> Option<&Element> {
        self.elements.iter().find(|e| e.id == id)
    }
}

/// A macOS accessibility role (`AXButton`, …) and subrole, in this module's
/// vocabulary.
///
/// Pure and on every platform, so the mapping is tested where it is written.
/// A tab is a radio button with the `AXTabButton` subrole on macOS, which is
/// why the subrole is consulted at all.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn role_of_ax(role: &str, subrole: Option<&str>) -> Role {
    if subrole == Some("AXTabButton") {
        return Role::TabItem;
    }
    match role {
        "AXButton" | "AXMenuButton" | "AXDisclosureTriangle" => Role::Button,
        "AXTextField" | "AXTextArea" | "AXSearchField" | "AXSecureTextField" => Role::Edit,
        "AXStaticText" | "AXHeading" => Role::Text,
        "AXList" | "AXCollection" => Role::List,
        "AXCell" | "AXRow" => Role::ListItem,
        "AXCheckBox" | "AXSwitch" => Role::CheckBox,
        "AXRadioButton" => Role::RadioButton,
        "AXComboBox" | "AXPopUpButton" => Role::ComboBox,
        "AXMenu" | "AXMenuBar" => Role::Menu,
        "AXMenuItem" | "AXMenuBarItem" => Role::MenuItem,
        "AXTabGroup" => Role::Tab,
        "AXLink" => Role::Link,
        "AXImage" => Role::Image,
        "AXWindow" | "AXSheet" | "AXDrawer" => Role::Window,
        "AXScrollArea" | "AXSplitGroup" | "AXLayoutArea" | "AXToolbar" => Role::Pane,
        "AXOutline" | "AXBrowser" => Role::Tree,
        "AXTable" | "AXGrid" => Role::Table,
        "AXSlider" | "AXIncrementor" => Role::Slider,
        "AXGroup" | "AXRadioGroup" => Role::Group,
        _ => Role::Other,
    }
}

/// An AT-SPI2 role name (`GetRoleName`: "push button", "entry", …) in this
/// module's vocabulary.
///
/// Pure and on every platform, like [`role_of_ax`]. The names are AT-SPI's
/// own (`atspi_role_get_name`), the same whatever toolkit drew the control.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn role_of_atspi(role: &str) -> Role {
    match role {
        "push button" | "toggle button" | "button" | "push button menu" => Role::Button,
        "entry" | "text" | "password text" | "editbar" | "spin button" | "terminal" => Role::Edit,
        "label" | "static" | "heading" | "paragraph" | "caption" => Role::Text,
        "list" | "list box" => Role::List,
        "list item" | "table cell" => Role::ListItem,
        "check box" | "switch" => Role::CheckBox,
        "radio button" => Role::RadioButton,
        "combo box" => Role::ComboBox,
        "menu" | "menu bar" | "popup menu" => Role::Menu,
        "menu item" | "check menu item" | "radio menu item" | "tearoff menu item" => Role::MenuItem,
        "page tab list" => Role::Tab,
        "page tab" => Role::TabItem,
        "link" => Role::Link,
        "image" | "icon" => Role::Image,
        "frame" | "dialog" | "window" | "alert" | "file chooser" => Role::Window,
        "panel" | "scroll pane" | "split pane" | "viewport" | "tool bar" | "filler"
        | "internal frame" | "layered pane" | "root pane" | "status bar" => Role::Pane,
        "tree" | "tree table" => Role::Tree,
        "tree item" => Role::TreeItem,
        "table" => Role::Table,
        "slider" | "scroll bar" => Role::Slider,
        // "generic" is GTK4's name for a layout box.
        "grouping" | "section" | "form" | "generic" => Role::Group,
        _ => Role::Other,
    }
}

/// One top-level window, as [`list_windows`] reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    /// What every `window` argument takes. Opaque: a handle on Windows,
    /// `pid.index` on macOS, a bus name and object path on Linux.
    pub id: String,
    pub title: String,
    /// The program that owns it ("notepad", "TextEdit", "gedit").
    pub app: String,
    pub pid: u32,
    /// Whether it is the window keystrokes go to.
    pub focused: bool,
    pub rect: Rect,
}

/// What an action did, and whether that was checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub detail: String,
    /// `None` when the result was read back and matched; otherwise why it
    /// could not be — an action is never reported as more certain than it is.
    pub unverified: Option<String>,
}

impl Done {
    pub fn verified(detail: String) -> Self {
        Done {
            detail,
            unverified: None,
        }
    }

    pub fn unverified(detail: String, why: &str) -> Self {
        Done {
            detail,
            unverified: Some(why.to_string()),
        }
    }

    /// One line for the agent.
    pub fn render(&self) -> String {
        match &self.unverified {
            None => format!("{} (verified)", self.detail),
            Some(why) => format!("{} (unverified: {why})", self.detail),
        }
    }
}

impl Element {
    /// `e7 "Save" (Button)`, for a message.
    pub fn label(&self) -> String {
        format!("e{} \"{}\" ({:?})", self.id, self.name, self.role)
    }
}

/// The refusal for an id the window no longer has.
#[cfg_attr(
    not(any(windows, target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
pub(crate) fn gone(id: u32, window: &WindowInfo) -> DesktopError {
    DesktopError::Os(format!(
        "element e{id} is no longer in window \"{}\". The UI changed — call ui_tree again \
         rather than acting where it used to be.",
        window.title
    ))
}

/// The depth a caller asked for, held to four times the configured default.
fn bounded(depth: u32) -> u32 {
    depth.min(super::limits::get().tree_depth.saturating_mul(4))
}

/// Every top-level window there is to act on.
pub fn list_windows() -> Result<Vec<WindowInfo>, DesktopError> {
    platform::list_windows()
}

/// The window keystrokes go to now, if one can be named.
pub fn foreground() -> Result<Option<WindowInfo>, DesktopError> {
    platform::foreground()
}

/// The window `window` names, or the focused one.
pub fn window_info(window: Option<&str>) -> Result<WindowInfo, DesktopError> {
    platform::window_info(window)
}

/// Why `window`'s rectangle is not its place on screen, when the toolkit
/// does not report one (GTK 4 on Linux). Elsewhere a window's rectangle is
/// the window server's own.
pub fn window_position_unknown(window: &WindowInfo) -> Option<String> {
    #[cfg(target_os = "linux")]
    return super::linux::window_position_unknown(window);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = window;
        None
    }
}

/// Walk a window's accessibility tree: `window` from [`list_windows`], or
/// the focused window.
pub fn window_tree(window: Option<&str>, depth: u32) -> Result<Tree, DesktopError> {
    let depth = bounded(depth);
    let tree = platform::tree(window, depth)?;
    remember_shown(&tree, depth);
    Ok(tree)
}

/// Walk the focused window's accessibility tree. The tools go through
/// [`window_tree`]; this is the live tests' shorthand.
#[cfg(test)]
pub fn focused_window_tree(depth: u32) -> Result<Tree, DesktopError> {
    window_tree(None, depth)
}

/// What [`window_tree`] last showed of one window: the depth it walked to and
/// the elements it returned. An id means something only against this — it is
/// a position in THAT walk — so acting by id is checked against it.
struct Shown {
    window: String,
    depth: u32,
    elements: Vec<Element>,
}

/// The last tree shown of each window, newest last. Bounded: an agent works
/// in a handful of windows, and an id from a tree this old is stale anyway.
static SHOWN: std::sync::Mutex<Vec<Shown>> = std::sync::Mutex::new(Vec::new());
const SHOWN_WINDOWS: usize = 16;

fn remember_shown(tree: &Tree, depth: u32) {
    let mut shown = SHOWN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    shown.retain(|s| s.window != tree.window);
    if shown.len() >= SHOWN_WINDOWS {
        shown.remove(0);
    }
    shown.push(Shown {
        window: tree.window.clone(),
        depth,
        elements: tree.elements.clone(),
    });
}

/// An element to act on: its id, and what `ui_tree` showed there. The live
/// walk resolves the id only when the control in that place is still that
/// one, so a row inserted above it, or a walk to another depth, cannot turn
/// the id into a different control.
#[derive(Debug, Clone)]
pub(crate) struct Want {
    pub id: u32,
    expect: Element,
}

/// Whether `now` is the control `shown` was: the same role, depth and secret
/// flag, and the same name — except an editable field's, which on macOS IS
/// its value and changes as the agent types.
fn same_control(now: &Element, shown: &Element) -> bool {
    now.role == shown.role
        && now.depth == shown.depth
        && now.secret == shown.secret
        && (now.role == Role::Edit || now.name == shown.name)
}

/// The window `window` names (or the focused one), the depth its last tree
/// was walked to, and the element `id` was in that tree.
fn wanted(window: Option<&str>, id: u32) -> Result<(WindowInfo, u32, Want), DesktopError> {
    let info = platform::window_info(window)?;
    let shown = SHOWN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(tree) = shown.iter().find(|s| s.window == info.id) else {
        return Err(DesktopError::Os(format!(
            "no ui_tree has been read of window \"{}\" yet: an element id is a position in \
             one tree, so call ui_tree first and act on an id it shows",
            info.title
        )));
    };
    let Some(expect) = tree.elements.iter().find(|e| e.id == id).cloned() else {
        return Err(DesktopError::Os(format!(
            "e{id} was not in the last ui_tree of window \"{}\"; act only on an id it shows",
            info.title
        )));
    };
    let depth = tree.depth;
    drop(shown);
    Ok((info, depth, Want { id, expect }))
}

/// Where the element with this id is now, re-read from the live tree.
///
/// Deliberately re-walks rather than trusting the earlier rectangle: between
/// the agent reading a tree and acting on it, a list can scroll or a dialog can
/// move, and clicking the remembered position would hit whatever took its
/// place. Returning `None` when it is gone — or when something else now has
/// its id — is the honest answer.
pub fn locate(window: Option<&str>, id: u32) -> Result<Option<Element>, DesktopError> {
    let (info, depth, want) = wanted(window, id)?;
    // Not `window_tree`: this walk is a check, and recording it as shown
    // would let a retry of the same stale id pass against it unseen.
    let tree = platform::tree(Some(&info.id), depth)?;
    // A position is what the caller wants from this; one that is not real
    // would put the click in the window's corner.
    if let Some(why) = tree.no_positions {
        return Err(DesktopError::Os(format!(
            "{why} so e{id} cannot be found on screen; use element_action or set_value on it"
        )));
    }
    Ok(tree
        .get(id)
        .filter(|now| now.is_useful() && same_control(now, &want.expect))
        .cloned())
}

/// Set a control's value through the accessibility interface — no typing,
/// no focus, no clipboard — and read it back where that is allowed.
pub fn set_value(window: Option<&str>, id: u32, value: &str) -> Result<Done, DesktopError> {
    let (info, depth, want) = wanted(window, id)?;
    platform::set_value(Some(&info.id), &want, depth, value)
}

/// Run one of a control's own actions (press, toggle, expand, …) without the
/// mouse, so the cursor stays where it is and nothing else gets the click.
pub fn act(window: Option<&str>, id: u32, action: &str) -> Result<Done, DesktopError> {
    let (info, depth, want) = wanted(window, id)?;
    platform::act(Some(&info.id), &want, depth, action)
}

/// Bring a window to the front, so keystrokes go to it.
pub fn focus_window(window: &str) -> Result<Done, DesktopError> {
    platform::focus_window(window)
}

#[cfg(windows)]
#[path = "uia_windows.rs"]
mod platform;

#[cfg(target_os = "macos")]
use super::macos as platform;

#[cfg(target_os = "linux")]
use super::linux as platform;

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod platform {
    // Naming the platform beats a generic failure — see `DesktopError`.
    use super::*;
    const NO: DesktopError = DesktopError::Unsupported("this platform");
    pub(super) fn list_windows() -> Result<Vec<WindowInfo>, DesktopError> {
        Err(NO)
    }
    pub(super) fn foreground() -> Result<Option<WindowInfo>, DesktopError> {
        Err(NO)
    }
    pub(super) fn window_info(_: Option<&str>) -> Result<WindowInfo, DesktopError> {
        Err(NO)
    }
    pub(super) fn tree(_: Option<&str>, _: u32) -> Result<Tree, DesktopError> {
        Err(NO)
    }
    pub(super) fn set_value(
        _: Option<&str>,
        _: &Want,
        _: u32,
        _: &str,
    ) -> Result<Done, DesktopError> {
        Err(NO)
    }
    pub(super) fn act(_: Option<&str>, _: &Want, _: u32, _: &str) -> Result<Done, DesktopError> {
        Err(NO)
    }
    pub(super) fn focus_window(_: &str) -> Result<Done, DesktopError> {
        Err(NO)
    }
}

/// The walk every platform's tree shares: depth-first, ids advancing for
/// every element visited (kept or not, so two walks of an unchanged window
/// agree), at most the configured number kept, and no deeper than
/// `max_depth`. With `want`, it stops at the element with that id and keeps
/// its handle — the same numbering, so the id `ui_tree` showed is the
/// control acted on, and only while it is still the control shown there.
///
/// The visit count is bounded as well as the kept count: a tree of thousands
/// of unnamed wrappers keeps nothing and would otherwise be walked in full.
struct Walk<'a, N> {
    max_depth: u32,
    describe: &'a dyn Fn(&N, u32, u32) -> Element,
    children: &'a dyn Fn(&N) -> Vec<N>,
    out: Vec<Element>,
    next_id: u32,
    max_visited: u32,
    truncated: bool,
    want: Option<&'a Want>,
    found: Option<(N, Element)>,
    done: bool,
}

impl<N: Clone> Walk<'_, N> {
    fn visit(&mut self, node: &N, depth: u32) {
        if self.done {
            return;
        }
        if self.out.len() >= max_elements() || self.next_id >= self.max_visited {
            self.truncated = true;
            self.done = true;
            return;
        }
        let id = self.next_id;
        let described = (self.describe)(node, id, depth);
        self.next_id += 1;
        let useful = described.is_useful();
        if let Some(want) = self.want.filter(|w| w.id == id) {
            // Only the element `ui_tree` showed at this id resolves.
            if useful && same_control(&described, &want.expect) {
                self.found = Some((node.clone(), described));
            }
            self.done = true;
            return;
        }
        if useful {
            self.out.push(described);
        }
        if depth >= self.max_depth {
            return;
        }
        for child in (self.children)(node) {
            self.visit(&child, depth + 1);
            if self.done {
                return;
            }
        }
    }
}

/// `describe` reads one element; `children` lists one element's children.
/// Generic over the handle, so a platform supplies only those two.
#[cfg_attr(
    not(any(windows, target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
pub(crate) fn walk<N: Clone>(
    root: &N,
    max_depth: u32,
    describe: &dyn Fn(&N, u32, u32) -> Element,
    children: &dyn Fn(&N) -> Vec<N>,
) -> (Vec<Element>, bool) {
    let mut walk = Walk {
        max_depth,
        describe,
        children,
        out: Vec::new(),
        next_id: 0,
        max_visited: super::limits::get().tree_max_visited,
        truncated: false,
        want: None,
        found: None,
        done: false,
    };
    walk.visit(root, 0);
    (walk.out, walk.truncated)
}

/// The handle and description of the element numbered `want.id` by [`walk`],
/// when it is still the control `ui_tree` showed there.
#[cfg_attr(
    not(any(windows, target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
pub(crate) fn find<N: Clone>(
    root: &N,
    max_depth: u32,
    describe: &dyn Fn(&N, u32, u32) -> Element,
    children: &dyn Fn(&N) -> Vec<N>,
    want: &Want,
) -> Option<(N, Element)> {
    let mut walk = Walk {
        max_depth,
        describe,
        children,
        out: Vec::new(),
        next_id: 0,
        max_visited: super::limits::get().tree_max_visited,
        truncated: false,
        want: Some(want),
        found: None,
        done: false,
    };
    walk.visit(root, 0);
    walk.found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(id: u32, role: Role, name: &str, rect: Rect) -> Element {
        Element {
            id,
            depth: 1,
            role,
            name: name.to_string(),
            rect,
            enabled: true,
            secret: false,
        }
    }

    /// A small tree for [`walk`]: each node is (role, name, children).
    #[derive(Clone)]
    struct Node(Role, &'static str, Vec<Node>);

    fn node(n: &Node, id: u32, d: u32) -> Element {
        Element {
            id,
            depth: d,
            role: n.0,
            name: n.1.to_string(),
            rect: Rect::from_edges(0, 0, 10, 10),
            enabled: true,
            secret: false,
        }
    }

    fn walk_of(root: &Node, depth: u32) -> (Vec<Element>, bool) {
        walk(root, depth, &node, &|n: &Node| n.2.clone())
    }

    fn want(shown: &Element) -> Want {
        Want {
            id: shown.id,
            expect: shown.clone(),
        }
    }

    fn settings(first: Vec<Node>) -> Node {
        let mut rows = first;
        rows.push(Node(
            Role::Group,
            "",
            vec![Node(Role::Button, "OK", vec![])],
        ));
        rows.push(Node(Role::Edit, "Name", vec![]));
        Node(Role::Window, "Settings", rows)
    }

    /// **THE ID ui_tree SHOWED IS THE CONTROL ACTED ON.** `find` numbers the
    /// tree exactly as `walk` does, so a structural wrapper the tree hid
    /// still takes its number, and every element a walk shows resolves to
    /// itself at that walk's depth.
    #[test]
    fn find_resolves_the_same_ids_the_walk_shows() {
        let tree = settings(vec![]);
        let children = |n: &Node| n.2.clone();
        for depth in 0..4 {
            let (shown, _) = walk_of(&tree, depth);
            for e in &shown {
                let (found, described) =
                    find(&tree, depth, &node, &children, &want(e)).expect("shown");
                assert_eq!(found.1, e.name, "depth {depth}, e{}", e.id);
                assert_eq!(described.id, e.id);
            }
        }
        let (full, _) = walk_of(&tree, 8);
        assert_eq!(
            full.iter()
                .map(|e| (e.id, e.name.as_str()))
                .collect::<Vec<_>>(),
            [(0, "Settings"), (2, "OK"), (3, "Name")]
        );
    }

    /// **An id never becomes another control.** The two ways it could: the
    /// same tree walked to another depth (at depth 1 "OK" is not visited, so
    /// e2 is "Name"), and a row inserted above (every later id shifts by
    /// one). Both are refused rather than acted on.
    #[test]
    fn a_shifted_id_is_refused_not_acted_on() {
        let tree = settings(vec![]);
        let children = |n: &Node| n.2.clone();
        let (shallow, _) = walk_of(&tree, 1);
        let name_at_depth_1 = shallow.iter().find(|e| e.id == 2).expect("e2");
        assert_eq!(name_at_depth_1.name, "Name");
        assert!(find(&tree, 8, &node, &children, &want(name_at_depth_1)).is_none());

        let (before, _) = walk_of(&tree, 8);
        let ok = before.iter().find(|e| e.name == "OK").expect("OK");
        let grown = settings(vec![Node(Role::Button, "Delete", vec![])]);
        assert!(
            find(&grown, 8, &node, &children, &want(ok)).is_none(),
            "e{} is now another control",
            ok.id
        );
    }

    /// An edit field is the same field after the agent types into it, though
    /// on macOS its name is its value — but never a field of another kind,
    /// and never a password field in place of a plain one.
    #[test]
    fn an_edit_field_survives_its_own_value_changing() {
        let rect = Rect::from_edges(0, 0, 90, 20);
        let shown = element(3, Role::Edit, "before", rect);
        assert!(same_control(&element(3, Role::Edit, "after", rect), &shown));
        let mut secret = element(3, Role::Edit, "before", rect);
        secret.secret = true;
        assert!(!same_control(&secret, &shown));
        assert!(!same_control(
            &element(3, Role::Button, "before", rect),
            &shown
        ));
        let button = element(3, Role::Button, "OK", rect);
        assert!(!same_control(
            &element(3, Role::Button, "Delete", rect),
            &button
        ));
    }

    /// A password field says it is one, and the line never carries more than
    /// its label.
    #[test]
    fn a_secret_field_is_marked_in_the_tree() {
        let mut field = element(4, Role::Edit, "Password", Rect::from_edges(0, 0, 90, 20));
        field.secret = true;
        let text = Tree {
            root_name: "Sign in".to_string(),
            window: "42".to_string(),
            app: "app".to_string(),
            no_positions: None,
            elements: vec![field],
            truncated: false,
        }
        .render();
        assert!(text.contains("[e4] Edit"), "{text}");
        assert!(text.contains("password"), "{text}");
        assert!(text.contains("[window 42, app app]"), "{text}");
    }

    /// **The shared walk numbers every element it visits**, kept or not, so a
    /// structural wrapper the filter drops still takes its id — the property
    /// that makes two walks of one window agree.
    #[test]
    fn the_walk_numbers_what_it_visits_and_keeps_what_is_useful() {
        let tree = Node(
            Role::Window,
            "Settings",
            vec![
                Node(Role::Group, "", vec![Node(Role::Button, "OK", vec![])]),
                Node(Role::Edit, "Name", vec![]),
            ],
        );
        let (kept, truncated) = walk_of(&tree, 8);
        let seen: Vec<(u32, &str)> = kept.iter().map(|e| (e.id, e.name.as_str())).collect();
        assert_eq!(seen, vec![(0, "Settings"), (2, "OK"), (3, "Name")]);
        assert!(!truncated);

        // The depth limit stops the walk below it.
        let (shallow, _) = walk_of(&tree, 1);
        assert!(shallow.iter().all(|e| e.depth <= 1), "{shallow:?}");
        assert!(!shallow.iter().any(|e| e.name == "OK"));
    }

    /// The walk stops at [`MAX_ELEMENTS`] and says so.
    #[test]
    fn a_huge_tree_is_cut_and_reported_as_cut() {
        let leaves = vec![Node(Role::Button, "b", vec![]); MAX_ELEMENTS + 50];
        let (kept, truncated) = walk_of(&Node(Role::Window, "w", leaves), 4);
        assert_eq!(kept.len(), MAX_ELEMENTS);
        assert!(truncated);
    }

    /// macOS roles map onto the same vocabulary as Windows; a tab is a radio
    /// button with a subrole there, and an unknown role is `Other`.
    #[test]
    fn macos_roles_map_onto_the_shared_vocabulary() {
        assert_eq!(role_of_ax("AXButton", None), Role::Button);
        assert_eq!(role_of_ax("AXTextField", None), Role::Edit);
        assert_eq!(
            role_of_ax("AXRadioButton", Some("AXTabButton")),
            Role::TabItem
        );
        assert_eq!(role_of_ax("AXRadioButton", None), Role::RadioButton);
        assert_eq!(role_of_ax("AXPopUpButton", None), Role::ComboBox);
        assert_eq!(role_of_ax("AXSomethingNew", None), Role::Other);
    }

    /// AT-SPI's role names map onto the same vocabulary: a GTK entry is an
    /// edit, a notebook page is a tab item, a frame is a window.
    #[test]
    fn atspi_roles_map_onto_the_shared_vocabulary() {
        assert_eq!(role_of_atspi("push button"), Role::Button);
        assert_eq!(role_of_atspi("entry"), Role::Edit);
        assert_eq!(role_of_atspi("label"), Role::Text);
        assert_eq!(role_of_atspi("page tab"), Role::TabItem);
        assert_eq!(role_of_atspi("check menu item"), Role::MenuItem);
        assert_eq!(role_of_atspi("frame"), Role::Window);
        assert_eq!(role_of_atspi("filler"), Role::Pane);
        assert_eq!(role_of_atspi("something new"), Role::Other);
    }

    #[test]
    fn a_click_lands_on_the_centre_of_the_control() {
        let e = element(1, Role::Button, "OK", Rect::from_edges(100, 200, 200, 240));
        assert_eq!(e.click_point(), PhysicalPoint { x: 150, y: 220 });
    }

    #[test]
    fn a_control_on_a_left_hand_monitor_clicks_in_negative_space() {
        let e = element(
            1,
            Role::Button,
            "Back",
            Rect::from_edges(-1900, 10, -1800, 50),
        );
        assert_eq!(e.click_point(), PhysicalPoint { x: -1850, y: 30 });
    }

    /// A zero-sized control cannot be clicked, whatever it is called.
    #[test]
    fn elements_with_no_area_are_dropped() {
        let collapsed = element(1, Role::Button, "Hidden", Rect::from_edges(5, 5, 5, 5));
        assert!(!collapsed.is_useful());
    }

    /// Anonymous structure is noise; anonymous CONTROLS are not.
    #[test]
    fn unnamed_wrappers_are_dropped_but_unnamed_controls_are_kept() {
        let rect = Rect::from_edges(0, 0, 100, 20);
        assert!(!element(1, Role::Pane, "", rect).is_useful());
        assert!(!element(2, Role::Group, "   ", rect).is_useful());

        // An unnamed icon button is still a button, and still clickable.
        assert!(element(3, Role::Button, "", rect).is_useful());
        assert!(element(4, Role::Text, "Some label", rect).is_useful());
    }

    #[test]
    fn structural_roles_are_not_offered_as_things_to_click() {
        for role in [
            Role::Pane,
            Role::Group,
            Role::Window,
            Role::Text,
            Role::Image,
        ] {
            assert!(!role.is_interactive(), "{role:?} should not be interactive");
        }
        for role in [Role::Button, Role::Edit, Role::MenuItem, Role::Link] {
            assert!(role.is_interactive(), "{role:?} should be interactive");
        }
    }

    #[test]
    fn roles_cross_the_wire_in_this_crates_own_vocabulary() {
        // Not "Button" or "AXButton" — the tool surface must read the same on
        // every platform.
        assert_eq!(
            serde_json::to_string(&Role::ListItem).expect("serialise"),
            "\"listitem\""
        );
        assert_eq!(
            serde_json::to_string(&Role::CheckBox).expect("serialise"),
            "\"checkbox\""
        );
    }

    #[test]
    fn a_truncated_tree_says_so() {
        let tree = Tree {
            root_name: "Settings".to_string(),
            window: String::new(),
            app: String::new(),
            no_positions: None,
            elements: vec![element(
                0,
                Role::Button,
                "OK",
                Rect::from_edges(0, 0, 10, 10),
            )],
            truncated: true,
        };
        let text = tree.render();
        assert!(text.contains("Settings"), "{text}");
        assert!(text.contains("[e0]"), "{text}");
        assert!(
            text.contains("Narrow the request"),
            "a truncated tree must say so: {text}"
        );
    }

    #[test]
    fn lookup_by_id_finds_only_what_the_walk_returned() {
        let tree = Tree {
            root_name: "W".to_string(),
            window: String::new(),
            app: String::new(),
            no_positions: None,
            elements: vec![
                element(3, Role::Button, "Yes", Rect::from_edges(0, 0, 10, 10)),
                element(7, Role::Button, "No", Rect::from_edges(20, 0, 30, 10)),
            ],
            truncated: false,
        };
        assert_eq!(tree.get(7).map(|e| e.name.as_str()), Some("No"));
        assert!(
            tree.get(4).is_none(),
            "ids that were filtered out must not resolve"
        );
    }

    #[cfg(windows)]
    mod live {
        use super::*;

        /// The real tree of whatever window has focus.
        ///
        /// Assertions are deliberately weak: this runs against whatever is on
        /// screen, which in a test run is a terminal. What must hold is that
        /// the walk completes, stays bounded, and reports rectangles in the
        /// same coordinate space the mouse uses.
        #[test]
        fn the_focused_window_can_be_walked() {
            let tree = match focused_window_tree(DEFAULT_DEPTH) {
                Ok(tree) => tree,
                // A headless or service session has no focused element. That is
                // a real state, not a failure of this code.
                Err(e) => {
                    eprintln!("no focused window in this session: {e}");
                    return;
                }
            };

            assert!(
                tree.elements.len() <= MAX_ELEMENTS,
                "the walk ran past its own bound: {}",
                tree.elements.len()
            );

            let screen = crate::desktop::screen_info().expect("screen");
            let bounds = screen.virtual_bounds().expect("a desktop");

            for e in &tree.elements {
                assert!(
                    e.rect.width > 0 && e.rect.height > 0,
                    "kept an empty: {e:?}"
                );
                let p = e.click_point();
                // Inside the virtual desktop, which is what proves these are
                // physical pixels rather than some window-local space.
                assert!(
                    p.x >= bounds.x - 1 && p.x <= bounds.x + bounds.width + 1,
                    "element {e:?} clicks outside the desktop at {p:?}"
                );
            }
        }

        /// Not an assertion — the readout.
        ///
        /// `cargo test --lib desktop::uia::tests::live::report_focused_tree -- --ignored --nocapture`
        #[test]
        #[ignore = "prints the focused window's accessibility tree"]
        fn report_focused_tree() {
            match focused_window_tree(DEFAULT_DEPTH) {
                Ok(tree) => println!("\n{}", tree.render()),
                Err(e) => println!("\nno tree: {e}"),
            }
        }
    }
}
