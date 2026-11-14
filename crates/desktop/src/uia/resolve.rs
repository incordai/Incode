//! Finding a control by selector, in the live tree, at the moment of the call.
//!
//! The ids `ui_tree` prints are positions in one walk. A selector
//! (`crate::selector`) is resolved the other way round: walk the window now,
//! and keep what it says. Exactly one control must fit; none, or several, is an
//! error that names which. What is returned carries the same `Want` the id path
//! builds, so the platform layers act on it through the code they already have
//! (`set_value`, `act`) and keep the check that the control in that place is
//! still the one that was resolved.
//!
//! # One walk for printing and for resolving
//!
//! `nth` and `path` count the controls of a walk, so the walk that prints a
//! selector (`ui_tree`, to the depth it was asked for) and the walk that
//! resolves it must be the same. The resolver walks to [`resolve_depth`]: the
//! selector's own `depth` when it has one (`ui_tree` writes it when it was
//! asked for another depth than the default), else the default depth, which is
//! the depth `ui_tree` walks without being asked. Any other pairing would let
//! `nth=1` mean one control to the tree and another to the resolver.
//!
//! # Errors are journalled
//!
//! Every error here goes to the operator's journal word for word, so a selector
//! in one is shown redacted ([`Selector::render_redacted`]): a text field's
//! name is what was typed in it. A blocked application's window is refused
//! before its title can appear in any message.
//!
//! Wired to: `mcp::tools` (`click_element`, `set_value`, `element_action`) and
//! `mcp::wait_tool` (`wait_for`), through [`window_for`], [`resolve_in`] and
//! [`tree_now`].

use super::{bounded, platform, same_control, Done, Element, Tree, Want, WindowInfo};
use crate::selector::{SelectError, Selector};
use crate::DesktopError;

/// Windows listed in an error about a window that was not found.
const LISTED: usize = 8;

/// What refuses a window before anything is said about it: the blocked
/// application check, in the words the id path gives (`mcp::guard`).
pub type WindowGuard<'a> = &'a dyn Fn(&WindowInfo) -> Result<(), String>;

/// A control found by selector, and what acting on it takes.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The window it is in.
    pub window: WindowInfo,
    /// The control as the live tree has it now (its `id` is this walk's own).
    pub element: Element,
    /// The walk stopped at a bound, so a second match beyond it cannot be
    /// ruled out. Reported with the result, since an error cannot be.
    pub truncated: bool,
    /// Why the rectangle is not a position, when it is not (GTK 4).
    pub no_positions: Option<String>,
    depth: u32,
    want: Want,
}

fn os(message: String) -> DesktopError {
    DesktopError::Os(message)
}

/// How deep a selector's window is walked: its `depth`, else the default,
/// held to the bound every walk is.
#[must_use]
pub(crate) fn resolve_depth(depth: Option<u32>) -> u32 {
    bounded(depth.unwrap_or(crate::limits::get().tree_depth))
}

/// `id "title" (app)` for a list in a message.
fn describe(w: &WindowInfo) -> String {
    format!("{} \"{}\" ({})", w.id, w.title, w.app)
}

/// The one window a selector's `app`/`window` part names among `windows`.
/// Windows of blocked applications are never candidates: their titles are not
/// shown, and a title must not be learned by matching against it.
pub(crate) fn pick_named_window(
    selector: &Selector,
    windows: Vec<WindowInfo>,
) -> Result<WindowInfo, DesktopError> {
    if let Some(app) = &selector.app {
        if crate::limits::get().blocked(app).is_some() {
            return Err(os(format!(
                "refused: \"{app}\" is a blocked application (a password manager, or \
                 desktop_limits.blocked_apps); incord never reads or acts on it"
            )));
        }
    }
    let open: Vec<WindowInfo> = windows
        .into_iter()
        .filter(|w| crate::limits::get().blocked(&w.app).is_none())
        .collect();
    let mut named: Vec<WindowInfo> = selector.windows_named(&open).into_iter().cloned().collect();
    match named.len() {
        1 => Ok(named.remove(0)),
        0 => Err(os(format!(
            "no window matches `{}`; open: {}",
            selector.render_window_part(),
            open.iter()
                .take(LISTED)
                .map(describe)
                .collect::<Vec<_>>()
                .join(", ")
        ))),
        n => Err(os(format!(
            "`{}` matches {n} windows, so it does not say which: {}. Pass `window` (an id \
             from list_windows) or make the app/window more specific",
            selector.render_window_part(),
            named
                .iter()
                .take(LISTED)
                .map(describe)
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// `info`, once `guard` has let it through and it is what the selector's
/// `app`/`window` part names.
///
/// **The guard comes first.** A window that was named by id and does not match
/// is described in the error (`window 7 is "<title>" (<app>) ... does not
/// name`), which for a blocked application's window would put its title in the
/// message, and make "does it match `window~=a`?" an oracle on that title: a
/// different refusal for a match and for a mismatch. So a blocked window gets
/// the blocked refusal, the same in both cases, before the selector is looked
/// at.
pub(crate) fn confirm_window(
    info: WindowInfo,
    selector: &Selector,
    guard: WindowGuard<'_>,
) -> Result<WindowInfo, DesktopError> {
    guard(&info).map_err(os)?;
    if selector.has_window_part() && !selector.matches_window(&info) {
        return Err(os(format!(
            "window {} is \"{}\" ({}), which the selector's window part `{}` does not name; \
             drop `window` or fix the selector",
            info.id,
            info.title,
            info.app,
            selector.render_window_part()
        )));
    }
    Ok(info)
}

/// The window a call acts in: the `window` id when one is given (and the
/// selector's own `app`/`window` must then agree with it), else the one the
/// selector names, else the focused window. Every one of them passes `guard`
/// first.
pub fn window_for(
    window: Option<&str>,
    selector: &Selector,
    guard: WindowGuard<'_>,
) -> Result<WindowInfo, DesktopError> {
    match window {
        Some(id) => confirm_window(platform::window_info(Some(id))?, selector, guard),
        None if selector.has_window_part() => {
            let info = pick_named_window(selector, platform::list_windows()?)?;
            confirm_window(info, selector, guard)
        }
        None => {
            let info = platform::window_info(None)?;
            guard(&info).map_err(os)?;
            Ok(info)
        }
    }
}

/// A fresh walk of `window` to the depth a selector resolves at (`depth`, its
/// own, or the default), for a caller that only looks (`wait_for`). Not
/// recorded as shown: an id is not being handed out.
pub fn tree_now(window: &str, depth: Option<u32>) -> Result<Tree, DesktopError> {
    platform::tree(
        Some(window),
        resolve_depth(depth),
        super::resolve_max_elements(),
    )
}

/// How long a browser is given, once, to build a page's accessibility tree
/// after a selector found nothing in it.
///
/// A browser does not expose a page until an accessibility client asks (Chrome
/// measured: the first walk of a fresh page held only the browser's own
/// controls, and about 3 s later the page was there). The walk is the ask; this
/// is the pause before the second walk.
const BROWSER_WARMUP: std::time::Duration = std::time::Duration::from_millis(1500);

/// Programs whose windows hold a web page, by the start of their name
/// (lower-cased): Windows and Linux process names, macOS application names.
const BROWSERS: &[&str] = &[
    "chrome",
    "google chrome",
    "chromium",
    "msedge",
    "microsoft edge",
    "brave",
    "vivaldi",
    "opera",
    "firefox",
];

/// Whether `app` is a web browser, whose page content may not be in its first
/// walk.
fn is_browser(app: &str) -> bool {
    let app = app.trim().to_lowercase();
    BROWSERS.iter().any(|b| app.starts_with(b))
}

/// The control `selector` names in `tree`, which was walked at `depth`.
/// Separate from the walk so it is tested without a desktop.
pub(crate) fn resolved_from(
    window: WindowInfo,
    tree: Tree,
    selector: &Selector,
    depth: u32,
) -> Result<Resolved, DesktopError> {
    let element = selector
        .resolve_in(&tree.elements)
        .map_err(|e| {
            let cut = if tree.truncated {
                format!(
                    " (the walk was cut at its bound, {} elements kept or {} nodes visited, so \
                     it may lie beyond)",
                    tree.elements.len(),
                    crate::limits::get().tree_max_visited
                )
            } else {
                String::new()
            };
            os(format!(
                "selector `{}` in window \"{}\" {e}{cut}",
                selector.render_redacted(),
                window.title
            ))
        })?
        .clone();
    Ok(Resolved {
        want: Want {
            id: element.id,
            expect: element.clone(),
        },
        window,
        element,
        truncated: tree.truncated,
        no_positions: tree.no_positions,
        depth,
    })
}

/// Walk `info`'s window to the selector's depth and return the one control it
/// picks. The window is already chosen ([`window_for`]), so a caller has
/// refused it, if it is a blocked application's, before anything is read.
///
/// Ambiguous or missing is an error, never a guess: see [`Selector::resolve_in`].
pub fn resolve_in(info: WindowInfo, selector: &Selector) -> Result<Resolved, DesktopError> {
    selector.require_element().map_err(os)?;
    let depth = resolve_depth(selector.depth);
    let window = info.id.clone();
    resolve_with(
        info,
        selector,
        depth,
        &mut || platform::tree(Some(&window), depth, super::resolve_max_elements()),
        &mut || std::thread::sleep(BROWSER_WARMUP),
    )
}

/// [`resolve_in`] with its two outside effects handed in: `look` walks the
/// window, `pause` waits [`BROWSER_WARMUP`]. Separate so a test needs neither a
/// desktop nor a clock.
///
/// A browser window in which nothing matched is walked a second time after the
/// pause, once: the first walk is what makes the browser build the page's
/// accessibility tree. Only a miss is retried (an ambiguity, a bad `nth`, or a
/// match is the answer already), so a hit costs nothing.
pub(crate) fn resolve_with(
    info: WindowInfo,
    selector: &Selector,
    depth: u32,
    look: &mut dyn FnMut() -> Result<Tree, DesktopError>,
    pause: &mut dyn FnMut(),
) -> Result<Resolved, DesktopError> {
    let mut tree = look()?;
    let warm = is_browser(&info.app)
        && matches!(
            selector.resolve_in(&tree.elements),
            Err(SelectError::Missing)
        );
    if warm {
        pause();
        tree = look()?;
    }
    resolved_from(info, tree, selector, depth).map_err(|e| match (warm, e) {
        (true, DesktopError::Os(message)) => os(format!(
            "{message} (a browser builds a page's accessibility tree when first asked, and \
             this waited {} ms for it once; if the control is on the page, ask again)",
            BROWSER_WARMUP.as_millis()
        )),
        (_, e) => e,
    })
}

/// [`super::set_value`] on a control found by selector.
pub fn set_value_on(found: &Resolved, value: &str) -> Result<Done, DesktopError> {
    platform::set_value(Some(&found.window.id), &found.want, found.depth, value)
}

/// [`super::act`] on a control found by selector.
pub fn act_on(found: &Resolved, action: &str) -> Result<Done, DesktopError> {
    platform::act(Some(&found.window.id), &found.want, found.depth, action)
}

impl Resolved {
    /// Whether the live control is still the one that was resolved: the same
    /// check the id path makes before it acts.
    #[must_use]
    pub fn is_same_as(&self, now: &Element) -> bool {
        same_control(now, &self.want.expect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selector::selectors_for;
    use crate::uia::{walk, Role};
    use crate::Rect;

    fn rect() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        }
    }

    fn window(id: &str, app: &str, title: &str) -> WindowInfo {
        WindowInfo {
            id: id.to_string(),
            title: title.to_string(),
            app: app.to_string(),
            pid: 1,
            focused: false,
            rect: rect(),
            has_tree: true,
        }
    }

    fn button(id: u32, name: &str) -> Element {
        Element {
            id,
            depth: 1,
            role: Role::Button,
            name: name.to_string(),
            rect: rect(),
            enabled: true,
            secret: false,
            automation_id: String::new(),
        }
    }

    fn tree_of(elements: Vec<Element>, truncated: bool) -> Tree {
        Tree {
            root_name: "Doc".to_string(),
            window: "7".to_string(),
            app: "app".to_string(),
            no_positions: None,
            elements,
            truncated,
            offscreen: 0,
        }
    }

    fn sel(text: &str) -> Selector {
        Selector::parse(text).unwrap()
    }

    #[test]
    fn a_unique_match_resolves_with_its_own_id_and_the_walks_depth() {
        let tree = tree_of(vec![button(4, "Save"), button(9, "Open")], false);
        let found = resolved_from(
            window("7", "app", "Doc"),
            tree,
            &sel("role=button;name=Open"),
            5,
        )
        .unwrap();
        assert_eq!(found.element.id, 9);
        assert_eq!(found.want.id, 9);
        assert_eq!(found.depth, 5);
        assert!(!found.truncated);
    }

    #[test]
    fn a_missing_control_is_an_error_naming_the_selector_and_the_window() {
        let tree = tree_of(vec![button(4, "Save")], false);
        let err = resolved_from(
            window("7", "app", "Doc"),
            tree,
            &sel("role=button;name=Nope"),
            5,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("role=button;name=Nope"), "{err}");
        assert!(err.contains("\"Doc\""), "{err}");
        assert!(err.contains("matches no element"), "{err}");
    }

    #[test]
    fn two_matches_are_an_error_and_never_the_first() {
        let tree = tree_of(vec![button(4, "Save"), button(9, "Save")], false);
        let err = resolved_from(window("7", "app", "Doc"), tree, &sel("name=Save"), 5)
            .unwrap_err()
            .to_string();
        assert!(err.contains("matches 2 elements"), "{err}");
        assert!(err.contains("nth"), "{err}");
    }

    /// An error is journalled word for word, and the selector in it may carry
    /// what was typed into a field.
    #[test]
    fn an_error_shows_a_text_fields_name_redacted_whatever_the_selector_says() {
        let edit = |id: u32, name: &str| Element {
            role: Role::Edit,
            ..button(id, name)
        };
        let tree = tree_of(vec![edit(1, "x")], false);
        // A miss.
        let err = resolved_from(
            window("7", "app", "Doc"),
            tree.clone(),
            &sel("role=edit;name=hunter2"),
            5,
        )
        .unwrap_err()
        .to_string();
        assert!(!err.contains("hunter2"), "{err}");
        assert!(err.contains("matches no element"), "{err}");
        // A miss with no role given, so the name might be anything.
        let err = resolved_from(window("7", "app", "Doc"), tree, &sel("name~=hunter2"), 5)
            .unwrap_err()
            .to_string();
        assert!(!err.contains("hunter2"), "{err}");
        // An ambiguity that lists look-alikes: by their place, not their name.
        let tree = tree_of(vec![edit(1, "hunter2"), edit(2, "hunter3")], false);
        let err = resolved_from(window("7", "app", "Doc"), tree, &sel("role=edit"), 5)
            .unwrap_err()
            .to_string();
        assert!(!err.contains("hunter"), "{err}");
        // A button's name is not content, and the selector still says it.
        let tree = tree_of(vec![button(1, "Save")], false);
        let err = resolved_from(
            window("7", "app", "Doc"),
            tree,
            &sel("role=button;name=Nope"),
            5,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("name=Nope"), "{err}");
    }

    #[test]
    fn a_cut_tree_says_a_miss_may_lie_beyond_the_cut() {
        let tree = tree_of(vec![button(4, "Save")], true);
        let err = resolved_from(
            window("7", "app", "Doc"),
            tree.clone(),
            &sel("name=Nope"),
            5,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("cut at"), "{err}");
        // A unique match in a cut tree is still returned, and flagged.
        let found = resolved_from(window("7", "app", "Doc"), tree, &sel("name=Save"), 5).unwrap();
        assert!(found.truncated);
    }

    #[test]
    fn the_window_part_picks_one_window_or_says_why_not() {
        let open = || {
            vec![
                window("1", "notepad", "a.txt - Notepad"),
                window("2", "notepad", "b.txt - Notepad"),
                window("3", "calc", "Calculator"),
            ]
        };
        let picked = pick_named_window(&sel("app=calc"), open()).unwrap();
        assert_eq!(picked.id, "3");
        let picked = pick_named_window(&sel("app=notepad;window~=b.txt"), open()).unwrap();
        assert_eq!(picked.id, "2");

        let both = pick_named_window(&sel("app=notepad"), open())
            .unwrap_err()
            .to_string();
        assert!(both.contains("matches 2 windows"), "{both}");
        assert!(both.contains("a.txt") && both.contains("b.txt"), "{both}");

        let none = pick_named_window(&sel("window=Nope"), open())
            .unwrap_err()
            .to_string();
        assert!(none.contains("no window matches"), "{none}");
        assert!(
            none.contains("Calculator"),
            "the open windows are listed: {none}"
        );
    }

    #[test]
    fn a_blocked_application_is_neither_named_nor_matched_by_its_title() {
        // `keepass` is in the built-in blocklist (limits.rs).
        let open = vec![
            window("1", "keepass", "Passwords - Vault"),
            window("2", "notepad", "notes"),
        ];
        let err = pick_named_window(&sel("app=keepass"), open.clone())
            .unwrap_err()
            .to_string();
        assert!(err.contains("blocked"), "{err}");
        // Probing for its title by a title-only selector finds nothing.
        let probe = pick_named_window(&sel("window~=Vault"), open)
            .unwrap_err()
            .to_string();
        assert!(probe.contains("no window matches"), "{probe}");
        assert!(!probe.contains("Passwords"), "{probe}");
    }

    /// The guard of the id path, as `mcp::tools` hands it: refuses a blocked
    /// application's window, in one fixed message.
    fn blocked_guard(w: &WindowInfo) -> Result<(), String> {
        if crate::limits::get().blocked(&w.app).is_some() {
            Err("refused: that window belongs to a blocked application".to_string())
        } else {
            Ok(())
        }
    }

    /// **A blocked window named by id is refused before the selector is read.**
    /// Otherwise the mismatch error says its title, and a match and a mismatch
    /// are told apart: an oracle on the title of a password manager's entry.
    #[test]
    fn a_blocked_window_is_refused_before_its_title_can_be_told_or_probed() {
        let keepass = || window("9", "KeePassXC", "Bank login - entry");
        let mut said = Vec::new();
        for selector in [
            "window=x;role=button",     // a mismatch
            "window~=bank;role=button", // a match
            "window~=zzz;role=button",  // another mismatch
            "app=notepad;role=button",  // the wrong program
            "role=button",              // no window part at all
        ] {
            let err = confirm_window(keepass(), &sel(selector), &blocked_guard)
                .unwrap_err()
                .to_string();
            assert!(
                !err.contains("Bank login"),
                "`{selector}` told the title: {err}"
            );
            assert!(!err.contains("entry"), "{err}");
            said.push(err);
        }
        assert!(
            said.windows(2).all(|pair| pair[0] == pair[1]),
            "the refusal differs with the selector, which is an oracle: {said:?}"
        );
    }

    #[test]
    fn an_allowed_window_that_does_not_match_is_still_told_apart_from_one_that_does() {
        let notes = || window("2", "notepad", "notes");
        assert!(confirm_window(notes(), &sel("window=notes;role=button"), &blocked_guard).is_ok());
        let err = confirm_window(notes(), &sel("window=other;role=button"), &blocked_guard)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not name"), "{err}");
    }

    #[test]
    fn a_control_selector_needs_a_control_part() {
        let err = resolve_in(window("1", "notepad", "n"), &sel("app=notepad;nth=0")).unwrap_err();
        assert!(err.to_string().contains("names no control"), "{err}");
    }

    // ---- one walk for printing and resolving ----

    /// A window whose OK button sits below the default depth, with two more
    /// OK buttons near the top that come after it in the tree's order.
    #[derive(Clone)]
    struct Node {
        role: Role,
        name: String,
        kids: Vec<Node>,
    }

    fn node(role: Role, name: &str, kids: Vec<Node>) -> Node {
        Node {
            role,
            name: name.to_string(),
            kids,
        }
    }

    fn deep_dialog(wrappers: usize) -> Node {
        let mut chain = node(Role::Button, "OK", vec![]);
        for i in (0..wrappers).rev() {
            chain = node(Role::Group, &format!("wrap{i}"), vec![chain]);
        }
        node(
            Role::Window,
            "Dialog",
            vec![
                chain,
                node(Role::Button, "OK", vec![]),
                node(Role::Button, "OK", vec![]),
            ],
        )
    }

    fn walked(root: &Node, depth: u32) -> Vec<Element> {
        walk(
            root,
            depth,
            crate::uia::resolve_max_elements(),
            &|n: &Node, id, d| Element {
                id,
                depth: d,
                role: n.role,
                name: n.name.clone(),
                rect: rect(),
                enabled: true,
                secret: false,
                automation_id: String::new(),
            },
            &|n: &Node| n.kids.clone(),
        )
        .elements
    }

    /// **`nth` counts the walk that printed it.** `ui_tree` prints the
    /// selector from the walk it was asked for; the resolver walks again. With
    /// the OK button nested below the default depth, a walk to the default sees
    /// two OKs (`nth=0` and `nth=1`) and a deeper one three, so a selector
    /// printed by one and resolved by the other clicks the wrong button.
    #[test]
    fn a_printed_selector_resolves_to_the_control_it_was_printed_for_at_any_depth() {
        let default = crate::limits::get().tree_depth;
        let root = deep_dialog(default as usize + 2);
        for asked in [1, 3, default, default + 4, default + 20] {
            let shown = walked(&root, bounded(asked));
            let stamp = (bounded(asked) != default).then_some(bounded(asked));
            let selectors = selectors_for(&shown, false, stamp);
            for (e, s) in shown.iter().zip(&selectors) {
                // The selector as an agent copies it, from text.
                let s = Selector::parse(&s.render()).unwrap();
                let again = walked(&root, resolve_depth(s.depth));
                let got = s.resolve_in(&again).unwrap_or_else(|why| {
                    panic!("depth {asked}: `{}` for {e:?}: {why}", s.render())
                });
                assert_eq!(
                    (got.id, got.depth, got.name.as_str()),
                    (e.id, e.depth, e.name.as_str()),
                    "depth {asked}: `{}` resolved to another control",
                    s.render()
                );
            }
        }
    }

    #[test]
    fn a_selector_printed_at_another_depth_says_so_and_the_default_does_not() {
        let default = crate::limits::get().tree_depth;
        let root = deep_dialog(default as usize + 2);
        let shown = walked(&root, default);
        let plain = selectors_for(&shown, false, None);
        assert!(plain.iter().all(|s| !s.render().contains("depth=")));
        // The two OKs in sight are told apart by nth, from the walk they are in.
        let oks: Vec<String> = shown
            .iter()
            .zip(&plain)
            .filter(|(e, _)| e.name == "OK")
            .map(|(_, s)| s.render())
            .collect();
        assert_eq!(
            oks,
            ["role=button;name=OK;nth=0", "role=button;name=OK;nth=1"]
        );

        let deeper = walked(&root, default + 4);
        let stamped = selectors_for(&deeper, false, Some(default + 4));
        assert!(stamped
            .iter()
            .all(|s| s.render().ends_with(&format!("depth={}", default + 4))));
        // The deeper walk has three OKs, so a selector from it says nth up to 2.
        assert!(stamped.iter().any(|s| s.render().contains("nth=2")));
    }

    #[test]
    fn the_resolver_walks_as_deep_as_the_selector_says_else_the_default() {
        let default = crate::limits::get().tree_depth;
        assert_eq!(resolve_depth(None), default);
        assert_eq!(resolve_depth(Some(3)), 3);
        assert_eq!(resolve_depth(Some(default + 4)), default + 4);
        assert_eq!(
            resolve_depth(Some(u32::MAX)),
            default * 4,
            "held to the bound every walk is"
        );
    }

    // ---- finding is not bounded by what printing is ----

    /// A long page: `links` links under one window, named `link0`, `link1`, ...
    fn long_page(links: usize) -> Node {
        node(
            Role::Window,
            "Article",
            (0..links)
                .map(|i| node(Role::Link, &format!("link{i}"), vec![]))
                .collect(),
        )
    }

    /// **A link low on a long page is found by selector.** Resolution shared
    /// `ui_tree`'s 400-element cap, so everything past the 400th element of an
    /// article could not be selected (measured on a Wikipedia article).
    #[test]
    fn a_control_past_the_print_cap_is_found_by_selector() {
        let print_cap = crate::limits::get().tree_max_elements;
        let low = print_cap + 600;
        let page = long_page(low + 50);
        let found = walked(&page, 4);
        assert!(found.len() > low, "the resolving walk kept {}", found.len());
        let s = sel(&format!("role=link;name=link{low}"));
        let hit = s.resolve_in(&found).expect("found past the print cap");
        assert_eq!(hit.name, format!("link{low}"));
        // The same walk, cut at what is printed, cannot see it.
        let printed = walk(
            &page,
            4,
            print_cap,
            &|n: &Node, id, d| Element {
                id,
                depth: d,
                role: n.role,
                name: n.name.clone(),
                rect: rect(),
                enabled: true,
                secret: false,
                automation_id: String::new(),
            },
            &|n: &Node| n.kids.clone(),
        );
        assert!(printed.truncated);
        assert_eq!(printed.elements.len(), print_cap);
        assert!(s.resolve_in(&printed.elements).is_err());
        // And the resolving walk is bounded: by visits, not unbounded.
        let huge = walked(
            &long_page(crate::limits::get().tree_max_visited as usize + 500),
            4,
        );
        assert!(huge.len() <= crate::limits::get().tree_max_visited as usize);
    }

    /// What `ui_tree` printed still means the same control to the resolver:
    /// the walk order is the same, the larger cap only keeps more of the tail.
    #[test]
    fn a_selector_printed_within_the_cap_still_means_the_same_control() {
        let print_cap = crate::limits::get().tree_max_elements;
        // Names repeat, so the selectors printed for them carry an nth.
        let twins = node(
            Role::Window,
            "Article",
            (0..print_cap + 300)
                .map(|i| node(Role::Link, &format!("same{}", i % 3), vec![]))
                .collect(),
        );
        let all_twins = walked(&twins, 4);
        let printed_twins = &all_twins[..print_cap];
        let selectors = selectors_for(printed_twins, false, None);
        for (e, s) in printed_twins.iter().zip(&selectors) {
            let s = Selector::parse(&s.render()).unwrap();
            let got = s.resolve_in(&all_twins).unwrap_or_else(|why| {
                panic!("`{}` for {e:?}: {why}", s.render());
            });
            assert_eq!((got.id, got.name.as_str()), (e.id, e.name.as_str()));
        }
        assert!(selectors.iter().any(|s| s.render().contains("nth=")));
    }

    // ---- a browser's first walk ----

    fn page_tree(with_target: bool) -> Tree {
        let mut elements = vec![button(1, "Back")];
        if with_target {
            elements.push(Element {
                role: Role::Link,
                ..button(2, "Honshu")
            });
        }
        tree_of(elements, false)
    }

    /// Run `resolve_with` over `trees` (one per look, the last repeating) and
    /// say how many looks and pauses it took.
    fn resolve_over(
        app: &str,
        selector: &str,
        trees: Vec<Tree>,
    ) -> (Result<Resolved, DesktopError>, usize, usize) {
        let looks = std::cell::Cell::new(0usize);
        let pauses = std::cell::Cell::new(0usize);
        let result = resolve_with(
            window("7", app, "Page"),
            &sel(selector),
            5,
            &mut || {
                let at = looks.get().min(trees.len() - 1);
                looks.set(looks.get() + 1);
                Ok(trees[at].clone())
            },
            &mut || pauses.set(pauses.get() + 1),
        );
        (result, looks.get(), pauses.get())
    }

    /// **A browser's first walk of a page may hold only the browser.** Chrome
    /// builds the page's accessibility tree once asked, so a selector that finds
    /// nothing in a browser is tried once more after a pause.
    #[test]
    fn a_miss_in_a_browser_waits_once_and_looks_again() {
        let (got, looks, pauses) = resolve_over(
            "chrome",
            "role=link;name=Honshu",
            vec![page_tree(false), page_tree(true)],
        );
        assert_eq!(got.unwrap().element.name, "Honshu");
        assert_eq!((looks, pauses), (2, 1));
    }

    #[test]
    fn a_hit_in_a_browser_costs_no_pause_and_a_second_look() {
        let (got, looks, pauses) = resolve_over(
            "Google Chrome",
            "role=link;name=Honshu",
            vec![page_tree(true)],
        );
        assert!(got.is_ok());
        assert_eq!((looks, pauses), (1, 0));
    }

    #[test]
    fn a_browser_that_still_has_no_match_is_asked_once_not_forever_and_says_so() {
        let (got, looks, pauses) =
            resolve_over("msedge", "role=link;name=Honshu", vec![page_tree(false)]);
        let err = got.unwrap_err().to_string();
        assert_eq!(
            (looks, pauses),
            (2, 1),
            "one retry, however often it misses"
        );
        assert!(err.contains("matches no element"), "{err}");
        assert!(err.contains("accessibility tree when first asked"), "{err}");
    }

    #[test]
    fn a_miss_in_another_program_is_not_retried() {
        let (got, looks, pauses) =
            resolve_over("notepad", "role=link;name=Honshu", vec![page_tree(false)]);
        let err = got.unwrap_err().to_string();
        assert_eq!((looks, pauses), (1, 0));
        assert!(!err.contains("when first asked"), "{err}");
    }

    #[test]
    fn an_ambiguity_in_a_browser_is_an_answer_and_is_not_retried() {
        let mut two = page_tree(true);
        two.elements.push(Element {
            role: Role::Link,
            ..button(3, "Honshu")
        });
        let (got, looks, pauses) = resolve_over("firefox", "role=link;name=Honshu", vec![two]);
        assert!(got.unwrap_err().to_string().contains("matches 2 elements"));
        assert_eq!((looks, pauses), (1, 0));
    }

    #[test]
    fn browsers_are_told_from_other_programs_by_name() {
        for app in [
            "chrome",
            "Google Chrome",
            "msedge",
            "Microsoft Edge",
            "firefox",
            "brave",
        ] {
            assert!(is_browser(app), "{app}");
        }
        for app in ["notepad", "explorer", "archive", "search", ""] {
            assert!(!is_browser(app), "{app}");
        }
    }
}
