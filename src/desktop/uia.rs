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
pub const DEFAULT_DEPTH: u32 = 12;

/// Most elements returned in one walk, whatever the depth allows.
pub const MAX_ELEMENTS: usize = 400;

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
    pub fn is_useful(&self) -> bool {
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
        out.push_str(&format!("Window \"{}\"\n", self.root_name));
        for e in &self.elements {
            out.push_str(&format!(
                "{:indent$}[e{}] {:<10} \"{}\" ({},{},{}x{}){}\n",
                "",
                e.id,
                format!("{:?}", e.role),
                e.name,
                e.rect.x,
                e.rect.y,
                e.rect.width,
                e.rect.height,
                if e.enabled { "" } else { "  disabled" },
                indent = (e.depth as usize).min(12) * 2,
            ));
        }
        if self.truncated {
            out.push_str(&format!(
                "\n… stopped at {MAX_ELEMENTS} elements. Narrow the request.\n"
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

/// Walk the focused window's accessibility tree.
pub fn focused_window_tree(depth: u32) -> Result<Tree, DesktopError> {
    platform::focused_window_tree(depth.min(DEFAULT_DEPTH * 4))
}

/// Where the element with this id is now, re-read from the live tree.
///
/// Deliberately re-walks rather than trusting the earlier rectangle: between
/// the agent reading a tree and acting on it, a list can scroll or a dialog can
/// move, and clicking the remembered position would hit whatever took its
/// place. Returning `None` when it is gone is the honest answer.
pub fn locate(id: u32, depth: u32) -> Result<Option<Element>, DesktopError> {
    Ok(focused_window_tree(depth)?.get(id).cloned())
}

#[cfg(windows)]
mod platform {
    use super::*;
    use uiautomation::controls::ControlType;
    use uiautomation::{UIAutomation, UIElement, UITreeWalker};

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

    fn describe(element: &UIElement, id: u32, depth: u32) -> Element {
        let rect = element
            .get_bounding_rectangle()
            .map(|r| Rect::from_edges(r.get_left(), r.get_top(), r.get_right(), r.get_bottom()))
            .unwrap_or(Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            });

        Element {
            id,
            depth,
            role: element
                .get_control_type()
                .map(role_of)
                .unwrap_or(Role::Other),
            name: element.get_name().unwrap_or_default(),
            rect,
            enabled: element.is_enabled().unwrap_or(false),
        }
    }

    pub(super) fn focused_window_tree(max_depth: u32) -> Result<Tree, DesktopError> {
        // Awareness first, for the same reason capture needs it: the rectangles
        // this returns are only physical pixels if the process is per-monitor
        // aware, and a virtualised tree would place every click by the scaling
        // ratio.
        super::super::dpi::ensure_per_monitor_aware();

        let automation = UIAutomation::new()
            .map_err(|e| DesktopError::Os(format!("UI Automation is unavailable: {e}")))?;

        let focused = automation
            .get_focused_element()
            .map_err(|e| DesktopError::Os(format!("no focused element: {e}")))?;

        let walker = automation
            .create_tree_walker()
            .map_err(|e| DesktopError::Os(format!("could not walk the tree: {e}")))?;

        // Climb to the owning window so the tree describes a whole window
        // rather than whatever happens to hold focus inside it.
        let root = window_of(&walker, &focused);
        let root_name = root.get_name().unwrap_or_default();

        let mut elements = Vec::new();
        let mut next_id = 0u32;
        let mut truncated = false;
        visit(
            &walker,
            &root,
            0,
            max_depth,
            &mut elements,
            &mut next_id,
            &mut truncated,
        );

        Ok(Tree {
            root_name,
            elements,
            truncated,
        })
    }

    fn window_of(walker: &UITreeWalker, from: &UIElement) -> UIElement {
        let mut current = from.clone();
        // Bounded: a malformed tree that reports itself as its own parent would
        // otherwise spin here forever.
        for _ in 0..24 {
            if matches!(current.get_control_type().map(role_of), Ok(Role::Window)) {
                return current;
            }
            match walker.get_parent(&current) {
                Ok(parent) => current = parent,
                Err(_) => return current,
            }
        }
        current
    }

    fn visit(
        walker: &UITreeWalker,
        element: &UIElement,
        depth: u32,
        max_depth: u32,
        out: &mut Vec<Element>,
        next_id: &mut u32,
        truncated: &mut bool,
    ) {
        if out.len() >= MAX_ELEMENTS {
            *truncated = true;
            return;
        }

        let described = describe(element, *next_id, depth);
        // The id advances for every element walked, not only the kept ones, so
        // two walks of an unchanged window agree on ids even though the filter
        // drops different things as the UI enables and disables controls.
        *next_id += 1;

        if described.is_useful() {
            out.push(described);
        }

        if depth >= max_depth {
            return;
        }

        let mut child = match walker.get_first_child(element) {
            Ok(child) => child,
            Err(_) => return,
        };
        loop {
            visit(
                walker,
                &child,
                depth + 1,
                max_depth,
                out,
                next_id,
                truncated,
            );
            if out.len() >= MAX_ELEMENTS {
                *truncated = true;
                return;
            }
            child = match walker.get_next_sibling(&child) {
                Ok(next) => next,
                Err(_) => return,
            };
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub(super) fn focused_window_tree(max_depth: u32) -> Result<Tree, DesktopError> {
        super::super::macos::focused_window_tree(max_depth)
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::*;

    pub(super) fn focused_window_tree(_max_depth: u32) -> Result<Tree, DesktopError> {
        // Linux needs AT-SPI2, which is not written yet. Naming the platform
        // beats a generic failure — see `DesktopError`.
        Err(DesktopError::Unsupported("this platform"))
    }
}

/// The walk every platform's tree shares: depth-first, ids advancing for
/// every element visited (kept or not, so two walks of an unchanged window
/// agree), at most [`MAX_ELEMENTS`] kept, and no deeper than `max_depth`.
///
/// `describe` reads one element; `children` lists one element's children.
/// Generic over the handle, so a platform supplies only those two.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn walk<N>(
    root: &N,
    max_depth: u32,
    describe: &dyn Fn(&N, u32, u32) -> Element,
    children: &dyn Fn(&N) -> Vec<N>,
) -> (Vec<Element>, bool) {
    struct Walk<'a, N> {
        max_depth: u32,
        describe: &'a dyn Fn(&N, u32, u32) -> Element,
        children: &'a dyn Fn(&N) -> Vec<N>,
        out: Vec<Element>,
        next_id: u32,
        truncated: bool,
    }

    impl<N> Walk<'_, N> {
        fn visit(&mut self, node: &N, depth: u32) {
            if self.out.len() >= MAX_ELEMENTS {
                self.truncated = true;
                return;
            }
            let described = (self.describe)(node, self.next_id, depth);
            self.next_id += 1;
            if described.is_useful() {
                self.out.push(described);
            }
            if depth >= self.max_depth {
                return;
            }
            for child in (self.children)(node) {
                self.visit(&child, depth + 1);
                if self.out.len() >= MAX_ELEMENTS {
                    self.truncated = true;
                    return;
                }
            }
        }
    }

    let mut walk = Walk {
        max_depth,
        describe,
        children,
        out: Vec::new(),
        next_id: 0,
        truncated: false,
    };
    walk.visit(root, 0);
    (walk.out, walk.truncated)
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
        }
    }

    /// A small tree for [`walk`]: each node is (role, name, children).
    #[derive(Clone)]
    struct Node(Role, &'static str, Vec<Node>);

    fn walk_of(root: &Node, depth: u32) -> (Vec<Element>, bool) {
        walk(
            root,
            depth,
            &|n: &Node, id, d| Element {
                id,
                depth: d,
                role: n.0,
                name: n.1.to_string(),
                rect: Rect::from_edges(0, 0, 10, 10),
                enabled: true,
            },
            &|n: &Node| n.2.clone(),
        )
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
