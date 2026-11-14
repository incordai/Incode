//! Stable element selectors: naming a control by what it is, not by where the
//! last `ui_tree` happened to number it.
//!
//! # Why this exists next to the `eN` ids
//!
//! An id is a position in ONE walk (`uia.rs`, `Shown`): a row inserted above
//! it, or a walk to another depth, renumbers everything after. That is right
//! for an agent acting a moment after reading the tree, and useless for a
//! flow written down and run later (`docs/COMPUTER_USE.md`, replay). A
//! selector says which window, and which control in it, in terms that survive a
//! restart of the program: the role, the name, the automation id where the
//! platform has one, and failing those a position among look-alikes.
//!
//! It is resolved against the LIVE tree at call time (`uia::resolve`), and the
//! rule is the one the ids already keep: zero matches or several is a clear
//! error that says which, never a guess at the nearest or the first.
//!
//! # The text form
//!
//! `key=value` pairs joined by `;`, no spaces around the `;` (a value keeps its
//! own spaces):
//!
//! | key | meaning |
//! |---|---|
//! | `app` | the owning program, exact, case-insensitive, a trailing `.exe` ignored |
//! | `window` / `window~` | the window title: exact / contains (case-insensitive) |
//! | `role` | `button`, `edit`, `listitem`, ... (the `role` names of `ui_tree`, lower case) |
//! | `name` / `name~` | the control's name: exact / contains (case-insensitive) |
//! | `aid` | the automation id: UIA `AutomationId`, AX `AXIdentifier`, AT-SPI `AccessibleId` |
//! | `path` | child indices from the window down, dot separated (`0.2.1`), among the controls `ui_tree` lists |
//! | `nth` | which of several matches, from 0, in `ui_tree` order |
//! | `depth` | how deep the window is walked (default 12). `ui_tree` writes it into each selector when it was asked for another depth, so `nth` and `path` count the same controls when resolved |
//!
//! A value escapes `\` as `\\`, `;` as `\;` and a line break as `\n` / `\r`.
//! `app` and `window` choose the WINDOW; the rest choose the CONTROL inside it.
//! `ui_tree` prints the control part beside every element, and the window part
//! once at the top.
//!
//! This module is pure, so it is tested on every platform.

use crate::uia::{Element, Role, WindowInfo};

/// Every role, with the lower-case word a selector spells it as.
const ROLES: &[(&str, Role)] = &[
    ("button", Role::Button),
    ("edit", Role::Edit),
    ("text", Role::Text),
    ("list", Role::List),
    ("listitem", Role::ListItem),
    ("checkbox", Role::CheckBox),
    ("radiobutton", Role::RadioButton),
    ("combobox", Role::ComboBox),
    ("menu", Role::Menu),
    ("menuitem", Role::MenuItem),
    ("tab", Role::Tab),
    ("tabitem", Role::TabItem),
    ("link", Role::Link),
    ("image", Role::Image),
    ("window", Role::Window),
    ("pane", Role::Pane),
    ("tree", Role::Tree),
    ("treeitem", Role::TreeItem),
    ("table", Role::Table),
    ("slider", Role::Slider),
    ("group", Role::Group),
    ("other", Role::Other),
];

impl Role {
    /// The word a selector uses for this role.
    #[must_use]
    pub fn word(self) -> &'static str {
        ROLES
            .iter()
            .find(|(_, role)| *role == self)
            .map_or("other", |(word, _)| word)
    }

    /// The role a selector's `role=` word names.
    #[must_use]
    pub fn from_word(word: &str) -> Option<Role> {
        let word = word.trim().to_ascii_lowercase();
        ROLES
            .iter()
            .find(|(w, _)| *w == word)
            .map(|(_, role)| *role)
    }
}

/// How a selector compares a piece of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Text {
    /// The whole text, character for character.
    Exact(String),
    /// A part of it, ignoring case.
    Contains(String),
}

/// `text` with case ignored, one character at a time.
///
/// Not `str::to_lowercase`: that reads the text around a character, and a
/// capital sigma becomes `ς` at the end of a word and `σ` elsewhere. A name
/// cut off at a sigma (a derived selector keeps only the first
/// [`LONGEST_NAME`] characters) would then lowercase differently from the same
/// letters inside the whole name, and the selector would miss its own element.
/// Folding each character on its own gives the same letters wherever the text
/// is cut.
///
/// A final sigma `ς` is folded to `σ` as well. `char::to_lowercase` leaves a
/// typed `ς` alone, so `λόγος` would not match `ΛΌΓΟΣ` (whose capital Σ folds
/// to `σ`), which `str::to_lowercase` did match.
fn fold(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|c| if c == 'ς' { 'σ' } else { c })
        .collect()
}

impl Text {
    fn matches(&self, actual: &str) -> bool {
        match self {
            Text::Exact(want) => actual == want,
            Text::Contains(part) => fold(actual).contains(&fold(part)),
        }
    }
}

/// A window and/or a control, as text. See the module header for the form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selector {
    pub app: Option<String>,
    pub window: Option<Text>,
    pub role: Option<Role>,
    pub name: Option<Text>,
    pub automation_id: Option<String>,
    pub path: Option<Vec<u32>>,
    pub nth: Option<u32>,
    /// How deep the window's tree is walked to find the control. `nth` and
    /// `path` count the controls of THAT walk, so a selector derived from a
    /// walk of one depth must be resolved by a walk of the same depth, or
    /// `nth=1` is a different control. Absent: the default depth
    /// (`desktop_limits.tree_depth`, 12). `ui_tree` writes it into each
    /// selector when it was asked for another depth.
    pub depth: Option<u32>,
}

/// Names longer than this are written into a derived selector as their first
/// this-many characters, matched as a part: a paragraph of static text is not
/// a name anyone should have to copy out whole.
const LONGEST_NAME: usize = 60;

/// Most look-alikes an ambiguity error lists.
const EXAMPLES: usize = 4;

/// Why a selector did not pick exactly one thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    /// Nothing matched.
    Missing,
    /// `nth` asked for a match there are not that many of.
    NthOutOfRange { nth: u32, count: usize },
    /// Several matched and the selector does not say which.
    Ambiguous { count: usize, examples: Vec<String> },
}

impl std::fmt::Display for SelectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelectError::Missing => write!(f, "matches no element"),
            SelectError::NthOutOfRange { nth, count } => {
                write!(f, "has nth={nth} but only {count} element(s) match")
            }
            SelectError::Ambiguous { count, examples } => {
                write!(f, "matches {count} elements, so it does not say which")?;
                if !examples.is_empty() {
                    write!(f, " (for example")?;
                    for e in examples {
                        write!(f, " `{e}`")?;
                    }
                    write!(f, ")")?;
                }
                write!(f, "; add name, aid, path or nth to pick one")
            }
        }
    }
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(raw: &str) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some(';') => out.push(';'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(_) => {
                return Err(
                    "has an unknown escape (only `\\\\`, `\\;`, `\\n` and `\\r` exist)".to_string(),
                )
            }
            None => return Err("ends in a lone `\\`".to_string()),
        }
    }
    Ok(out)
}

/// `raw` cut at each `;` that is not escaped.
fn split_pairs(raw: &str) -> Vec<String> {
    let mut pieces = vec![String::new()];
    let mut escaped = false;
    for c in raw.chars() {
        let piece = pieces.last_mut().expect("one piece always exists");
        if escaped {
            piece.push(c);
            escaped = false;
        } else if c == '\\' {
            piece.push(c);
            escaped = true;
        } else if c == ';' {
            pieces.push(String::new());
        } else {
            piece.push(c);
        }
    }
    pieces
}

/// Store `value` in `slot`, refusing a key that appears twice. `key` is the
/// name reported, so `name` and `name~` count as one key.
fn once<T>(slot: &mut Option<T>, key: &str, value: T) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("`{key}` appears twice"));
    }
    *slot = Some(value);
    Ok(())
}

impl Selector {
    /// Read the text form. Strict: an unknown key, a repeated one, a bad number
    /// or role is an error, never skipped.
    ///
    /// **An error never quotes what it was given.** It names the item by its
    /// position (`item 2`) and says what is wrong with it, because the text of
    /// a selector may be a typed value (`role=edit;hunter2`), and an error is
    /// journalled word for word (`mcp/gate.rs`, `record_call`). Only the
    /// fixed names of this grammar appear in one.
    pub fn parse(raw: &str) -> Result<Selector, String> {
        const KEYS: &str = "app, window, window~, role, name, name~, aid, path, nth, depth";
        let mut s = Selector::default();
        let items = split_pairs(raw)
            .into_iter()
            .filter(|piece| !piece.trim().is_empty());
        for (at, piece) in items.enumerate() {
            let item = at + 1;
            let Some((key, value)) = piece.split_once('=') else {
                return Err(format!("item {item} is not key=value (keys: {KEYS})"));
            };
            let key = key.trim();
            let value = unescape(value).map_err(|e| format!("item {item}: the value {e}"))?;
            match key {
                "app" => once(&mut s.app, key, value)?,
                "window" => once(&mut s.window, key, Text::Exact(value))?,
                "window~" => once(&mut s.window, "window", Text::Contains(value))?,
                "role" => {
                    let role = Role::from_word(&value).ok_or_else(|| {
                        format!(
                            "item {item}: the role is not one of {}",
                            ROLES.iter().map(|(w, _)| *w).collect::<Vec<_>>().join(", ")
                        )
                    })?;
                    once(&mut s.role, key, role)?;
                }
                "name" => once(&mut s.name, key, Text::Exact(value))?,
                "name~" => once(&mut s.name, "name", Text::Contains(value))?,
                "aid" => once(&mut s.automation_id, key, value)?,
                "path" => {
                    let path = value
                        .split('.')
                        .map(|n| n.trim().parse::<u32>())
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|_| {
                            format!("item {item}: path must be child indices like 0.2.1")
                        })?;
                    once(&mut s.path, key, path)?;
                }
                "nth" => {
                    let nth = value
                        .trim()
                        .parse::<u32>()
                        .map_err(|_| format!("item {item}: nth must be a whole number from 0"))?;
                    once(&mut s.nth, key, nth)?;
                }
                "depth" => {
                    let depth = value
                        .trim()
                        .parse::<u32>()
                        .map_err(|_| format!("item {item}: depth must be a whole number"))?;
                    once(&mut s.depth, key, depth)?;
                }
                _ => return Err(format!("item {item} has an unknown key (keys: {KEYS})")),
            }
        }
        if s == Selector::default() {
            return Err("the selector is empty".to_string());
        }
        Ok(s)
    }

    /// The text form, [`Self::parse`]'s inverse.
    #[must_use]
    pub fn render(&self) -> String {
        let mut parts = Vec::new();
        self.push_window_parts(&mut parts);
        self.push_element_parts(&mut parts, true);
        parts.join(";")
    }

    /// Only the window part (`app`, `window`), as the header of a tree shows it.
    #[must_use]
    pub fn render_window_part(&self) -> String {
        let mut parts = Vec::new();
        self.push_window_parts(&mut parts);
        parts.join(";")
    }

    /// Only the control part, as `ui_tree` shows it beside each element.
    #[must_use]
    pub fn render_element_part(&self) -> String {
        let mut parts = Vec::new();
        self.push_element_parts(&mut parts, true);
        parts.join(";")
    }

    fn push_window_parts(&self, parts: &mut Vec<String>) {
        if let Some(app) = &self.app {
            parts.push(format!("app={}", escape(app)));
        }
        match &self.window {
            Some(Text::Exact(t)) => parts.push(format!("window={}", escape(t))),
            Some(Text::Contains(t)) => parts.push(format!("window~={}", escape(t))),
            None => {}
        }
    }

    fn push_element_parts(&self, parts: &mut Vec<String>, with_name: bool) {
        if let Some(role) = self.role {
            parts.push(format!("role={}", role.word()));
        }
        if with_name {
            match &self.name {
                Some(Text::Exact(t)) => parts.push(format!("name={}", escape(t))),
                Some(Text::Contains(t)) => parts.push(format!("name~={}", escape(t))),
                None => {}
            }
        }
        if let Some(aid) = &self.automation_id {
            parts.push(format!("aid={}", escape(aid)));
        }
        if let Some(path) = &self.path {
            let path: Vec<String> = path.iter().map(u32::to_string).collect();
            parts.push(format!("path={}", path.join(".")));
        }
        if let Some(nth) = self.nth {
            parts.push(format!("nth={nth}"));
        }
        if let Some(depth) = self.depth {
            parts.push(format!("depth={depth}"));
        }
    }

    /// The selector as a journal may carry it. A control's name is left out
    /// when it might be what the user typed: an edit field's (on macOS the
    /// name of a text field IS its value), or any control whose role is not
    /// given. Everything else is the same as [`Self::render`].
    #[must_use]
    pub fn render_redacted(&self) -> String {
        let keep_name = self.role.is_some_and(|r| r != Role::Edit);
        let mut parts = Vec::new();
        self.push_window_parts(&mut parts);
        self.push_element_parts(&mut parts, keep_name);
        if !keep_name && self.name.is_some() {
            parts.push("name=<not journalled>".to_string());
        }
        parts.join(";")
    }

    /// Whether it names a window (`app` or `window`).
    #[must_use]
    pub fn has_window_part(&self) -> bool {
        self.app.is_some() || self.window.is_some()
    }

    /// Whether it names a control (`role`, `name`, `aid` or `path`).
    #[must_use]
    pub fn has_element_part(&self) -> bool {
        self.role.is_some()
            || self.name.is_some()
            || self.automation_id.is_some()
            || self.path.is_some()
    }

    /// Fit for acting on a control: it names one. `nth` alone is "the third of
    /// anything", which is a guess.
    ///
    /// These errors, like every one that carries a selector, show it as
    /// [`Self::render_redacted`]: an error is journalled word for word.
    pub fn require_element(&self) -> Result<(), String> {
        if self.has_element_part() {
            Ok(())
        } else {
            Err(format!(
                "selector `{}` names no control: give at least one of role, name, aid, path",
                self.render_redacted()
            ))
        }
    }

    /// Fit for naming a window: only `app`/`window`.
    pub fn require_window_only(&self) -> Result<(), String> {
        if !self.has_window_part() {
            return Err(format!(
                "selector `{}` names no window: give app and/or window (or window~)",
                self.render_redacted()
            ));
        }
        if self.has_element_part() || self.nth.is_some() || self.depth.is_some() {
            return Err(format!(
                "selector `{}` names a control too, but this asks about a window: use only \
                 app and window",
                self.render_redacted()
            ));
        }
        Ok(())
    }

    /// Whether `window` is what the window part names. With no window part,
    /// any window is.
    #[must_use]
    pub fn matches_window(&self, window: &WindowInfo) -> bool {
        let app_ok = self
            .app
            .as_deref()
            .is_none_or(|want| same_app(want, &window.app));
        let title_ok = self
            .window
            .as_ref()
            .is_none_or(|text| text.matches(&window.title));
        app_ok && title_ok
    }

    /// The windows of `windows` this names.
    #[must_use]
    pub fn windows_named<'a>(&self, windows: &'a [WindowInfo]) -> Vec<&'a WindowInfo> {
        windows.iter().filter(|w| self.matches_window(w)).collect()
    }

    /// Whether `element` fits the control part, `nth` aside. `path` is the
    /// element's own path ([`paths`]) when the selector gives one.
    fn matches_element(&self, element: &Element, path: Option<&[u32]>) -> bool {
        self.role.is_none_or(|r| r == element.role)
            && self
                .name
                .as_ref()
                .is_none_or(|text| text.matches(&element.name))
            && self
                .automation_id
                .as_deref()
                .is_none_or(|aid| aid == element.automation_id)
            && match (&self.path, path) {
                (Some(want), Some(have)) => want.as_slice() == have,
                (Some(_), None) => false,
                (None, _) => true,
            }
    }

    /// The places in `elements` that fit the control part, `nth` not yet applied.
    #[must_use]
    pub fn candidates(&self, elements: &[Element]) -> Vec<usize> {
        let own_paths = self.path.as_ref().map(|_| paths(elements));
        elements
            .iter()
            .enumerate()
            .filter(|(i, e)| self.matches_element(e, own_paths.as_ref().map(|p| p[*i].as_slice())))
            .map(|(i, _)| i)
            .collect()
    }

    /// The one element this picks out of `elements` (a fresh walk): the single
    /// match, or the `nth` of several. Zero, or several without `nth`, is an
    /// error and never a choice.
    pub fn resolve_in<'a>(&self, elements: &'a [Element]) -> Result<&'a Element, SelectError> {
        let found = self.candidates(elements);
        let at = match (found.len(), self.nth) {
            (0, _) => return Err(SelectError::Missing),
            (1, None) => found[0],
            (count, None) => {
                // The examples are in an error, which is journalled, so a text
                // field is written by its place and never by its name: on
                // macOS the name IS what was typed there (`names_are_values`,
                // here on every platform because an example costs nothing).
                let examples = found
                    .iter()
                    .take(EXAMPLES)
                    .map(|&i| {
                        // With the depth of THIS selector: an example's `nth`
                        // counts the controls of the walk that found the
                        // matches, and the resolver must walk as deep to count
                        // the same ones (see [`Selector::depth`]).
                        Selector {
                            depth: self.depth,
                            ..selector_for(elements, i, true)
                        }
                        .render_element_part()
                    })
                    .collect();
                return Err(SelectError::Ambiguous { count, examples });
            }
            (count, Some(nth)) => *found
                .get(nth as usize)
                .ok_or(SelectError::NthOutOfRange { nth, count })?,
        };
        Ok(&elements[at])
    }
}

/// Whether two program names are the same one: case-insensitive, and a
/// trailing `.exe` (Windows) is not part of the name.
fn same_app(want: &str, actual: &str) -> bool {
    fn norm(s: &str) -> String {
        let s = s.trim().to_lowercase();
        s.strip_suffix(".exe").map_or(s.clone(), str::to_string)
    }
    norm(want) == norm(actual)
}

/// Every element's path: its index among its siblings, then its parent's, up
/// to the window. Siblings are the elements `ui_tree` LISTS (a wrapper it
/// drops is not a level), so the path reads the same on every platform and
/// does not depend on what the toolkit calls a wrapper.
#[must_use]
pub fn paths(elements: &[Element]) -> Vec<Vec<u32>> {
    // (depth, path, children so far) of the elements still open.
    let mut open: Vec<(u32, Vec<u32>, u32)> = Vec::new();
    let mut roots = 0u32;
    let mut out = Vec::with_capacity(elements.len());
    for e in elements {
        while open.last().is_some_and(|(depth, _, _)| *depth >= e.depth) {
            open.pop();
        }
        let (index, mut path) = match open.last_mut() {
            Some((_, parent, children)) => {
                let index = *children;
                *children += 1;
                (index, parent.clone())
            }
            None => {
                let index = roots;
                roots += 1;
                (index, Vec::new())
            }
        };
        path.push(index);
        open.push((e.depth, path.clone(), 0));
        out.push(path);
    }
    out
}

/// A name as a selector writes it: whole when short, else its beginning as a
/// "contains".
fn name_text(name: &str) -> Text {
    if name.chars().count() > LONGEST_NAME {
        let head: String = name.chars().take(LONGEST_NAME).collect();
        Text::Contains(head)
    } else {
        Text::Exact(name.to_string())
    }
}

/// The shortest control selector that picks `elements[index]` and nothing
/// else, in the order of what changes least: the automation id, then the name,
/// then — when several look alike — their order (`nth`) among those.
///
/// `names_are_values`: the platform names a text field by its content (macOS),
/// which changes as it is typed in, so the field's name is not used.
#[must_use]
pub fn selector_for(elements: &[Element], index: usize, names_are_values: bool) -> Selector {
    let e = &elements[index];
    let name = if (names_are_values && e.role == Role::Edit) || e.name.trim().is_empty() {
        None
    } else {
        Some(name_text(&e.name))
    };
    let aid = Some(e.automation_id.clone()).filter(|a| !a.is_empty());
    let base = Selector {
        role: Some(e.role),
        ..Selector::default()
    };
    let by_aid = Selector {
        automation_id: aid.clone(),
        ..base.clone()
    };
    let by_name = Selector {
        name: name.clone(),
        ..base.clone()
    };
    for attempt in [by_aid, by_name] {
        if attempt.has_more_than_role() && attempt.candidates(elements).len() == 1 {
            return attempt;
        }
    }
    let mut picked = Selector {
        name,
        automation_id: aid,
        ..base
    };
    picked.nth = picked
        .candidates(elements)
        .iter()
        .position(|&i| i == index)
        .map(|n| n as u32);
    picked
}

impl Selector {
    fn has_more_than_role(&self) -> bool {
        self.name.is_some() || self.automation_id.is_some()
    }
}

/// [`selector_for`] every element: what `ui_tree` prints beside each.
///
/// `depth` is how deep the walk that produced `elements` went, when that is
/// not the default one: it is written into every selector, because `nth` and
/// `path` count the controls of that walk and the resolver must walk as deep
/// to count the same ones (see [`Selector::depth`]).
#[must_use]
pub fn selectors_for(
    elements: &[Element],
    names_are_values: bool,
    depth: Option<u32>,
) -> Vec<Selector> {
    (0..elements.len())
        .map(|i| Selector {
            depth,
            ..selector_for(elements, i, names_are_values)
        })
        .collect()
}

/// The selector that records WHERE the element is as well as what it is: the
/// short one plus its `path`, which is a hard condition, so a control that has
/// moved is NOT found. For a flow written down to replay later, which wants to
/// notice that. The path already says which of several look-alikes, so the
/// `nth` of the short form is dropped (it would count the matches before the
/// path narrowed them).
#[must_use]
pub fn selector_with_path(elements: &[Element], index: usize, names_are_values: bool) -> Selector {
    let mut s = selector_for(elements, index, names_are_values);
    s.path = Some(paths(elements)[index].clone());
    s.nth = None;
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rect;

    fn el(id: u32, depth: u32, role: Role, name: &str) -> Element {
        Element {
            id,
            depth,
            role,
            name: name.to_string(),
            rect: Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            enabled: true,
            secret: false,
            automation_id: String::new(),
        }
    }

    fn aid(mut e: Element, id: &str) -> Element {
        e.automation_id = id.to_string();
        e
    }

    /// A small dialog: two Save buttons (one in each pane), an OK button, two
    /// unnamed edits, a long paragraph.
    fn dialog() -> Vec<Element> {
        vec![
            el(1, 1, Role::Pane, "Left"),
            aid(el(2, 2, Role::Button, "Save"), "saveLeft"),
            el(3, 2, Role::Edit, ""),
            el(4, 1, Role::Pane, "Right"),
            el(5, 2, Role::Button, "Save"),
            el(6, 2, Role::Edit, ""),
            el(7, 1, Role::Button, "OK"),
            el(8, 1, Role::Text, &"x".repeat(200)),
        ]
    }

    fn window(app: &str, title: &str) -> WindowInfo {
        WindowInfo {
            id: "1".to_string(),
            title: title.to_string(),
            app: app.to_string(),
            pid: 1,
            focused: false,
            rect: Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            has_tree: true,
        }
    }

    #[test]
    fn a_selector_parses_every_key_and_renders_back_the_same() {
        let text = "app=notepad;window=Untitled - Notepad;role=button;name=Save;aid=btn1;\
                    path=0.2.1;nth=1";
        let s = Selector::parse(text).unwrap();
        assert_eq!(s.app.as_deref(), Some("notepad"));
        assert_eq!(s.window, Some(Text::Exact("Untitled - Notepad".into())));
        assert_eq!(s.role, Some(Role::Button));
        assert_eq!(s.name, Some(Text::Exact("Save".into())));
        assert_eq!(s.automation_id.as_deref(), Some("btn1"));
        assert_eq!(s.path, Some(vec![0, 2, 1]));
        assert_eq!(s.nth, Some(1));
        assert_eq!(s.render(), text);
    }

    #[test]
    fn contains_keys_round_trip_and_keep_their_meaning() {
        let s = Selector::parse("window~=save as;name~=ok").unwrap();
        assert_eq!(s.window, Some(Text::Contains("save as".into())));
        assert_eq!(s.render(), "window~=save as;name~=ok");
    }

    #[test]
    fn values_escape_the_separator_and_line_breaks() {
        let s = Selector {
            name: Some(Text::Exact("a;b\\c\nd=e".into())),
            role: Some(Role::Text),
            ..Selector::default()
        };
        let text = s.render();
        assert_eq!(text, "role=text;name=a\\;b\\\\c\\nd=e");
        assert_eq!(Selector::parse(&text).unwrap(), s);
    }

    #[test]
    fn a_value_may_contain_equals_signs_and_keep_its_spaces() {
        let s = Selector::parse("name= a = b ").unwrap();
        assert_eq!(s.name, Some(Text::Exact(" a = b ".into())));
    }

    #[test]
    fn a_bad_selector_is_refused_by_name_never_skipped() {
        let bad = |t: &str| Selector::parse(t).unwrap_err();
        assert!(bad("").contains("empty"));
        assert!(bad(";;").contains("empty"));
        assert!(bad("colour=red").contains("item 1 has an unknown key"));
        assert!(bad("role=gizmo").contains("item 1: the role is not one of"));
        assert!(bad("role=button;role=edit").contains("twice"));
        assert!(bad("name=a;name~=b").contains("twice"));
        assert!(bad("nth=-1").contains("nth"));
        assert!(bad("path=1..2").contains("path"));
        assert!(bad("depth=deep").contains("depth"));
        assert!(bad("justaword").contains("item 1 is not key=value"));
        assert!(bad("name=a\\qb").contains("unknown escape"));
        assert!(bad("name=a\\").contains("lone"));
    }

    /// **An error never quotes what it was given.** A selector may carry a
    /// typed value, and an error is journalled word for word.
    #[test]
    fn a_parse_error_names_the_position_and_never_quotes_the_text() {
        for raw in [
            "role=edit;hunter2",
            "hunter2=1",
            "role=hunter2",
            "nth=hunter2",
            "path=hunter2",
            "depth=hunter2",
            "name=a\\hunter2",
            "name=hunter2\\",
            "role=edit;name=hunter2;name=hunter2",
        ] {
            let why = Selector::parse(raw).unwrap_err();
            assert!(!why.contains("hunter2"), "`{raw}` was quoted: {why}");
        }
        let why = Selector::parse("role=edit;hunter2").unwrap_err();
        assert!(why.contains("item 2"), "{why}");
    }

    #[test]
    fn the_requirement_errors_show_the_selector_redacted() {
        let s = Selector::parse("role=edit;name=hunter2;nth=0;depth=3").unwrap();
        let none = Selector::parse("app=x;nth=1").unwrap();
        assert!(none.require_element().unwrap_err().contains("app=x"));
        let why = s.require_window_only().unwrap_err();
        assert!(!why.contains("hunter2"), "{why}");
        let typed = Selector::parse("name=hunter2;nth=0").unwrap();
        let why = typed.require_window_only().unwrap_err();
        assert!(!why.contains("hunter2"), "{why}");
    }

    #[test]
    fn an_ambiguity_error_lists_a_text_field_by_its_place_and_never_its_name() {
        let tree = vec![
            el(1, 1, Role::Edit, "hunter2"),
            el(2, 1, Role::Edit, "hunter3"),
            el(3, 1, Role::Button, "OK"),
            el(4, 1, Role::Button, "OK"),
        ];
        let why = Selector::parse("role=edit")
            .unwrap()
            .resolve_in(&tree)
            .unwrap_err()
            .to_string();
        assert!(why.contains("matches 2 elements"), "{why}");
        assert!(!why.contains("hunter"), "{why}");
        assert!(why.contains("role=edit;nth=1"), "{why}");
        // A button's name is not content, and is still shown.
        let why = Selector::parse("role=button")
            .unwrap()
            .resolve_in(&tree)
            .unwrap_err()
            .to_string();
        assert!(why.contains("name=OK"), "{why}");
    }

    #[test]
    fn depth_is_part_of_the_text_form_and_not_a_control_part() {
        let s = Selector::parse("role=button;name=OK;nth=1;depth=20").unwrap();
        assert_eq!(s.depth, Some(20));
        assert_eq!(s.render(), "role=button;name=OK;nth=1;depth=20");
        // It narrows nothing by itself.
        assert!(Selector::parse("depth=20")
            .unwrap()
            .require_element()
            .is_err());
        assert!(Selector::parse("role=button;depth=20")
            .unwrap()
            .require_window_only()
            .is_err());
        let stamped = selectors_for(&dialog(), false, Some(20));
        assert!(stamped.iter().all(|s| s.depth == Some(20)));
        let plain = selectors_for(&dialog(), false, None);
        assert!(plain.iter().all(|s| s.depth.is_none()));
    }

    /// An ambiguity error's examples count `nth` in the walk that found the
    /// matches, so they carry that walk's depth.
    #[test]
    fn an_ambiguity_examples_carry_the_depth_of_the_selector_that_failed() {
        let tree = vec![el(1, 1, Role::Button, "OK"), el(2, 1, Role::Button, "OK")];
        let at_three = Selector::parse("role=button;name=OK;depth=3").unwrap();
        match at_three.resolve_in(&tree) {
            Err(SelectError::Ambiguous { examples, .. }) => {
                assert_eq!(examples.len(), 2);
                for example in &examples {
                    assert!(example.ends_with(";depth=3"), "{example}");
                }
                assert_eq!(examples[1], "role=button;name=OK;nth=1;depth=3");
                // What the error suggests is a selector that resolves, in a
                // walk of that depth, to the control it named.
                let again = Selector::parse(&examples[1]).unwrap();
                assert_eq!(again.resolve_in(&tree).unwrap().id, 2);
            }
            other => panic!("{other:?}"),
        }
        // No depth said, none suggested.
        let plain = Selector::parse("role=button;name=OK").unwrap();
        match plain.resolve_in(&tree) {
            Err(SelectError::Ambiguous { examples, .. }) => {
                assert!(
                    examples.iter().all(|e| !e.contains("depth")),
                    "{examples:?}"
                )
            }
            other => panic!("{other:?}"),
        }
    }

    /// A typed final sigma is the same letter as a capital one in a name:
    /// `str::to_lowercase` made them match, so the per-character fold must.
    #[test]
    fn a_typed_final_sigma_matches_a_capital_sigma_in_the_name() {
        let tree = vec![el(1, 1, Role::Text, "ΛΌΓΟΣ"), el(2, 1, Role::Text, "other")];
        for typed in ["name~=λόγος", "name~=λόγοσ", "name~=ΛΌΓΟΣ", "name~=γος"] {
            let s = Selector::parse(typed).unwrap();
            assert_eq!(s.resolve_in(&tree).unwrap().id, 1, "{typed}");
        }
        // And a lowercase name, with the final form, is found by a capital one.
        let tree = vec![el(1, 1, Role::Text, "λόγος")];
        let s = Selector::parse("name~=ΛΌΓΟΣ").unwrap();
        assert_eq!(s.resolve_in(&tree).unwrap().id, 1);
        // The derived selector for a name ending in ς still finds it.
        let tree = vec![el(1, 1, Role::Text, &format!("{}ς", "a".repeat(80)))];
        assert_eq!(
            selector_for(&tree, 0, false).resolve_in(&tree).unwrap().id,
            1
        );
    }

    /// A name cut off at a capital sigma lowercases to a different letter than
    /// the same sigma inside the whole name: `ς` at the end of a word, `σ`
    /// elsewhere. The derived selector must still find its own element.
    #[test]
    fn a_name_cut_at_a_sigma_still_finds_its_own_element() {
        let name = format!("{}Σ{}", "a".repeat(LONGEST_NAME - 1), "bcdef");
        assert_eq!(name.chars().nth(LONGEST_NAME - 1), Some('Σ'));
        let tree = vec![
            el(1, 1, Role::Text, &name),
            el(2, 1, Role::Text, "something else"),
        ];
        let s = selector_for(&tree, 0, false);
        assert!(matches!(s.name, Some(Text::Contains(_))), "{s:?}");
        assert_eq!(s.resolve_in(&tree).unwrap().id, 1, "{}", s.render());
        // Through the text form an agent would copy.
        let again = Selector::parse(&s.render()).unwrap();
        assert_eq!(again.resolve_in(&tree).unwrap().id, 1);
        // And case is still ignored for the rest.
        let upper = Selector::parse("name~=SOMETHING").unwrap();
        assert_eq!(upper.resolve_in(&tree).unwrap().id, 2);
    }

    #[test]
    fn every_role_has_one_word_that_reads_back() {
        for (word, role) in ROLES {
            assert_eq!(role.word(), *word);
            assert_eq!(Role::from_word(word), Some(*role));
        }
        assert_eq!(Role::from_word(" Button "), Some(Role::Button));
        assert_eq!(ROLES.len(), 22, "a new Role needs a selector word");
    }

    #[test]
    fn nth_alone_or_a_window_alone_does_not_name_a_control() {
        assert!(Selector::parse("nth=2").unwrap().require_element().is_err());
        assert!(Selector::parse("app=x;window=y")
            .unwrap()
            .require_element()
            .is_err());
        assert!(Selector::parse("role=button")
            .unwrap()
            .require_element()
            .is_ok());
    }

    #[test]
    fn a_window_question_takes_only_window_keys() {
        assert!(Selector::parse("app=notepad")
            .unwrap()
            .require_window_only()
            .is_ok());
        assert!(Selector::parse("role=button")
            .unwrap()
            .require_window_only()
            .is_err());
        assert!(Selector::parse("app=a;role=button")
            .unwrap()
            .require_window_only()
            .is_err());
    }

    #[test]
    fn windows_match_by_program_and_title() {
        let w = window("Notepad.exe", "Untitled - Notepad");
        let m = |t: &str| Selector::parse(t).unwrap().matches_window(&w);
        assert!(m("app=notepad"));
        assert!(m("app=NOTEPAD.EXE"));
        assert!(!m("app=note"), "app is exact, not a prefix");
        assert!(m("window=Untitled - Notepad"));
        assert!(!m("window=untitled - notepad"), "exact is case-sensitive");
        assert!(m("window~=UNTITLED"));
        assert!(m("app=notepad;window~=untitled"));
        assert!(!m("app=notepad;window~=report"));
        assert_eq!(
            Selector::parse("app=notepad")
                .unwrap()
                .windows_named(&[w.clone(), window("calc", "Calculator")])
                .len(),
            1
        );
    }

    #[test]
    fn one_match_resolves_and_none_or_many_is_an_error() {
        let tree = dialog();
        let get = |t: &str| Selector::parse(t).unwrap().resolve_in(&tree);
        assert_eq!(get("role=button;name=OK").unwrap().id, 7);
        assert_eq!(get("role=button;name=Nope"), Err(SelectError::Missing));
        match get("role=button;name=Save") {
            Err(SelectError::Ambiguous { count, examples }) => {
                assert_eq!(count, 2);
                assert_eq!(examples.len(), 2);
            }
            other => panic!("two Save buttons must be ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn ambiguity_is_never_settled_by_taking_the_first() {
        let tree = dialog();
        let err = Selector::parse("role=edit")
            .unwrap()
            .resolve_in(&tree)
            .unwrap_err();
        let said = err.to_string();
        assert!(said.contains("matches 2 elements"), "{said}");
        assert!(said.contains("nth"), "{said}");
    }

    #[test]
    fn nth_picks_among_several_and_out_of_range_is_refused() {
        let tree = dialog();
        let nth = |n: u32| {
            Selector::parse(&format!("role=button;name=Save;nth={n}"))
                .unwrap()
                .resolve_in(&tree)
        };
        assert_eq!(nth(0).unwrap().id, 2);
        assert_eq!(nth(1).unwrap().id, 5);
        assert_eq!(nth(2), Err(SelectError::NthOutOfRange { nth: 2, count: 2 }));
    }

    #[test]
    fn the_automation_id_tells_look_alikes_apart_and_must_match_exactly() {
        let tree = dialog();
        let by = |t: &str| Selector::parse(t).unwrap().resolve_in(&tree);
        assert_eq!(by("role=button;aid=saveLeft").unwrap().id, 2);
        assert_eq!(by("aid=saveLeft").unwrap().id, 2);
        assert_eq!(by("aid=saveleft"), Err(SelectError::Missing));
    }

    #[test]
    fn contains_ignores_case_and_exact_does_not() {
        let tree = dialog();
        let by = |t: &str| Selector::parse(t).unwrap().resolve_in(&tree);
        assert_eq!(by("name~=ok").unwrap().id, 7);
        assert_eq!(by("name=ok"), Err(SelectError::Missing));
    }

    #[test]
    fn paths_follow_the_listed_hierarchy_and_pick_one_element() {
        let tree = dialog();
        let p = paths(&tree);
        assert_eq!(p[0], vec![0]); // Left pane
        assert_eq!(p[1], vec![0, 0]); // its Save
        assert_eq!(p[2], vec![0, 1]); // its edit
        assert_eq!(p[3], vec![1]); // Right pane
        assert_eq!(p[4], vec![1, 0]);
        assert_eq!(p[6], vec![2]); // OK
        let by = Selector::parse("role=button;path=1.0").unwrap();
        assert_eq!(by.resolve_in(&tree).unwrap().id, 5);
    }

    #[test]
    fn a_path_skips_wrappers_the_tree_does_not_list() {
        // A depth-3 control under a dropped depth-2 wrapper is the first child
        // of its nearest listed ancestor.
        let tree = vec![
            el(1, 1, Role::Pane, "P"),
            el(2, 3, Role::Button, "A"),
            el(3, 3, Role::Button, "B"),
            el(4, 1, Role::Pane, "Q"),
        ];
        let p = paths(&tree);
        assert_eq!(p[1], vec![0, 0]);
        assert_eq!(p[2], vec![0, 1]);
        assert_eq!(p[3], vec![1]);
    }

    #[test]
    fn a_derived_selector_resolves_back_to_its_own_element() {
        let mut tree = dialog();
        tree.push(el(9, 1, Role::Button, "OK")); // a second OK
        tree.push(aid(el(10, 1, Role::Edit, "Name"), "nameBox"));
        for names_are_values in [false, true] {
            for i in 0..tree.len() {
                let s = selector_for(&tree, i, names_are_values);
                let got = s.resolve_in(&tree).unwrap_or_else(|e| {
                    panic!("`{}` for {:?} did not resolve: {e}", s.render(), tree[i])
                });
                assert_eq!(got.id, tree[i].id, "`{}`", s.render());
                // And through the text form an agent would copy.
                let again = Selector::parse(&s.render()).unwrap();
                assert_eq!(again, s);
                assert_eq!(again.resolve_in(&tree).unwrap().id, tree[i].id);
                let full = selector_with_path(&tree, i, names_are_values);
                assert_eq!(full.resolve_in(&tree).unwrap().id, tree[i].id);
            }
        }
    }

    #[test]
    fn a_derived_selector_is_the_shortest_that_is_unique() {
        let tree = dialog();
        // OK is the only button of that name: role and name are enough.
        assert_eq!(
            selector_for(&tree, 6, false).render_element_part(),
            "role=button;name=OK"
        );
        // The Save with an automation id is told by it, not by its name.
        assert_eq!(
            selector_for(&tree, 1, false).render_element_part(),
            "role=button;aid=saveLeft"
        );
        // The other Save has only a name shared with the first: its place.
        assert_eq!(
            selector_for(&tree, 4, false).render_element_part(),
            "role=button;name=Save;nth=1"
        );
        // Unnamed look-alikes are told by their order.
        assert_eq!(
            selector_for(&tree, 5, false).render_element_part(),
            "role=edit;nth=1"
        );
    }

    #[test]
    fn a_text_field_named_by_its_value_is_not_selected_by_it() {
        let mut tree = dialog();
        tree[2].name = "what the user typed".to_string();
        let on_macos = selector_for(&tree, 2, true).render_element_part();
        assert!(!on_macos.contains("typed"), "{on_macos}");
        let elsewhere = selector_for(&tree, 2, false).render_element_part();
        assert!(elsewhere.contains("what the user typed"), "{elsewhere}");
    }

    #[test]
    fn a_long_name_is_selected_by_its_beginning() {
        let tree = dialog();
        let s = selector_for(&tree, 7, false);
        match &s.name {
            Some(Text::Contains(head)) => assert_eq!(head.chars().count(), LONGEST_NAME),
            other => panic!("expected a prefix match, got {other:?}"),
        }
        assert_eq!(s.resolve_in(&tree).unwrap().id, 8);
    }

    #[test]
    fn the_journal_form_drops_a_text_fields_name_and_keeps_a_buttons() {
        let button = Selector::parse("role=button;name=Save").unwrap();
        assert_eq!(button.render_redacted(), "role=button;name=Save");
        let edit = Selector::parse("role=edit;name=hunter2;nth=0").unwrap();
        let said = edit.render_redacted();
        assert!(!said.contains("hunter2"), "{said}");
        let no_role = Selector::parse("name=hunter2").unwrap();
        assert!(!no_role.render_redacted().contains("hunter2"));
    }

    #[test]
    fn the_parts_render_separately() {
        let s = Selector::parse("app=a;window=W;role=edit;nth=1").unwrap();
        assert_eq!(s.render_window_part(), "app=a;window=W");
        assert_eq!(s.render_element_part(), "role=edit;nth=1");
    }
}
