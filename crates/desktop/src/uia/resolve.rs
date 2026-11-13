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
//! Wired to: `mcp::tools` (`click_element`, `set_value`, `element_action`) and
//! `mcp::wait_tool` (`wait_for`), through [`resolve`], [`window_for`] and
//! [`tree_now`].

use super::{bounded, platform, same_control, Done, Element, Tree, Want, WindowInfo};
use crate::selector::Selector;
use crate::DesktopError;

/// Windows listed in an error about a window that was not found.
const LISTED: usize = 8;

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

/// The window a call acts in: the `window` id when one is given (and the
/// selector's own `app`/`window` must then agree with it), else the one the
/// selector names, else the focused window.
pub fn window_for(window: Option<&str>, selector: &Selector) -> Result<WindowInfo, DesktopError> {
    match window {
        Some(id) => {
            let info = platform::window_info(Some(id))?;
            if selector.has_window_part() && !selector.matches_window(&info) {
                return Err(os(format!(
                    "window {id} is \"{}\" ({}), which the selector's window part `{}` does \
                     not name; drop `window` or fix the selector",
                    info.title,
                    info.app,
                    selector.render_window_part()
                )));
            }
            Ok(info)
        }
        None if selector.has_window_part() => {
            pick_named_window(selector, platform::list_windows()?)
        }
        None => platform::window_info(None),
    }
}

/// A fresh walk of `window` as deep as the walk may go, for a caller that only
/// looks (`wait_for`). Not recorded as shown: an id is not being handed out.
pub fn tree_now(window: &str) -> Result<Tree, DesktopError> {
    platform::tree(Some(window), bounded(u32::MAX))
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
                    " (the tree was cut at {} elements, so it may lie beyond the cut)",
                    super::max_elements()
                )
            } else {
                String::new()
            };
            os(format!(
                "selector `{}` in window \"{}\" {e}{cut}",
                selector.render(),
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

/// Walk the window the selector names and return the one control it picks.
///
/// Ambiguous or missing is an error, never a guess: see [`Selector::resolve_in`].
pub fn resolve(window: Option<&str>, selector: &Selector) -> Result<Resolved, DesktopError> {
    selector.require_element().map_err(os)?;
    resolve_in(window_for(window, selector)?, selector)
}

/// [`resolve`] in a window already chosen, so a caller can refuse the window
/// (a blocked application's) before anything is read from it.
pub fn resolve_in(info: WindowInfo, selector: &Selector) -> Result<Resolved, DesktopError> {
    selector.require_element().map_err(os)?;
    let depth = bounded(u32::MAX);
    let tree = platform::tree(Some(&info.id), depth)?;
    resolved_from(info, tree, selector, depth)
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
    use crate::uia::Role;
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

    #[test]
    fn a_control_selector_needs_a_control_part() {
        let err = resolve(None, &sel("app=notepad;nth=0")).unwrap_err();
        assert!(err.to_string().contains("names no control"), "{err}");
    }
}
