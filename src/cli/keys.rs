//! Editing modes and rebindable keys for the input box.
//!
//! # Editing mode
//!
//! `editing_mode = "vi"` in admin.json (or `/vim` for the session) gives the
//! box a vi normal mode: Esc leaves insert mode, and `h l 0 ^ $ w b e x X D C
//! dd cc S s i a I A j k` do what they do in vi. Enter submits from either
//! mode. The non-terminal path (rustyline) gets rustyline's own vi mode.
//!
//! # Key bindings
//!
//! `keybindings.toml` in the data directory (`~/.incode/`) maps keys to the
//! box's editing actions, one per line:
//!
//! ```toml
//! "ctrl-l" = "clear-line"
//! "alt-b"  = "word-left"
//! ```
//!
//! This is the flat `key = "action"` subset of TOML, read by hand — a section
//! header or comment is skipped, anything else malformed is reported and
//! skipped. A binding is consulted before the built-in keys, so it can
//! override them.

use crossterm::event::{KeyCode, KeyModifiers};

/// The bindings file, in the data directory.
pub const KEYBINDINGS_FILE: &str = "keybindings.toml";

/// How the input box edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditingMode {
    #[default]
    Emacs,
    Vi,
}

impl EditingMode {
    /// Read `editing_mode`. Anything but `vi`/`vim` is the default.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(|r| r.trim().to_ascii_lowercase()).as_deref() {
            Some("vi" | "vim") => Self::Vi,
            _ => Self::Emacs,
        }
    }

    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Emacs => Self::Vi,
            Self::Vi => Self::Emacs,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Emacs => "emacs",
            Self::Vi => "vi",
        }
    }
}

/// An editing action a key can be bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ClearLine,
    KillToStart,
    KillToEnd,
    DeleteWord,
    LineStart,
    LineEnd,
    CharLeft,
    CharRight,
    WordLeft,
    WordRight,
    HistoryPrev,
    HistoryNext,
    Newline,
    Submit,
    Complete,
}

impl Action {
    /// Every action, by the name the bindings file uses.
    pub const ALL: &'static [(&'static str, Action)] = &[
        ("clear-line", Action::ClearLine),
        ("kill-to-start", Action::KillToStart),
        ("kill-to-end", Action::KillToEnd),
        ("delete-word", Action::DeleteWord),
        ("line-start", Action::LineStart),
        ("line-end", Action::LineEnd),
        ("char-left", Action::CharLeft),
        ("char-right", Action::CharRight),
        ("word-left", Action::WordLeft),
        ("word-right", Action::WordRight),
        ("history-prev", Action::HistoryPrev),
        ("history-next", Action::HistoryNext),
        ("newline", Action::Newline),
        ("submit", Action::Submit),
        ("complete", Action::Complete),
    ];

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase().replace('_', "-");
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, a)| *a)
    }
}

/// A key and its modifiers, as the bindings file names it: `ctrl-l`,
/// `alt-enter`, `ctrl-alt-x`, `f5`, `tab`.
#[must_use]
pub fn parse_key(spec: &str) -> Option<(KeyCode, KeyModifiers)> {
    let spec = spec.trim().to_ascii_lowercase();
    let mut mods = KeyModifiers::NONE;
    let mut parts: Vec<&str> = spec.split(['-', '+']).collect();
    // `ctrl--` is ctrl and a minus, which split leaves as two empty parts.
    let last = if spec.ends_with("--") || spec.ends_with("+-") || spec == "-" {
        parts.retain(|p| !p.is_empty());
        "-"
    } else {
        parts.pop()?
    };
    for m in parts {
        match m {
            "ctrl" | "control" | "c" => mods |= KeyModifiers::CONTROL,
            "alt" | "meta" | "m" => mods |= KeyModifiers::ALT,
            "shift" | "s" => mods |= KeyModifiers::SHIFT,
            _ => return None,
        }
    }
    let code = match last {
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "esc" | "escape" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "space" => KeyCode::Char(' '),
        f if f.len() > 1 && f.starts_with('f') => KeyCode::F(f[1..].parse().ok()?),
        c if c.chars().count() == 1 => KeyCode::Char(c.chars().next()?),
        _ => return None,
    };
    Some((code, mods))
}

/// The operator's bindings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keymap {
    bindings: Vec<(KeyCode, KeyModifiers, Action)>,
}

impl Keymap {
    /// Parse the file's text. Returns the map and a message per line skipped.
    #[must_use]
    pub fn parse(text: &str) -> (Self, Vec<String>) {
        let mut map = Self::default();
        let mut problems = Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim();
            if line.is_empty() || line.starts_with('[') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                problems.push(format!("line {}: expected `\"key\" = \"action\"`", n + 1));
                continue;
            };
            let key = unquote(key.trim());
            let value = unquote(value.trim());
            match (parse_key(key), Action::parse(value)) {
                (Some((code, mods)), Some(action)) => map.bind(code, mods, action),
                (None, _) => problems.push(format!("line {}: unknown key `{key}`", n + 1)),
                (_, None) => problems.push(format!("line {}: unknown action `{value}`", n + 1)),
            }
        }
        (map, problems)
    }

    /// Read the bindings file in `data_dir`; empty when there is none.
    #[must_use]
    pub fn load(data_dir: &std::path::Path) -> (Self, Vec<String>) {
        match std::fs::read_to_string(data_dir.join(KEYBINDINGS_FILE)) {
            Ok(text) => Self::parse(&text),
            Err(_) => (Self::default(), Vec::new()),
        }
    }

    pub fn bind(&mut self, code: KeyCode, mods: KeyModifiers, action: Action) {
        self.bindings
            .retain(|(c, m, _)| !(*c == code && *m == mods));
        self.bindings.push((code, mods, action));
    }

    /// The action bound to this key, if any. Shift is ignored on a character
    /// key, since terminals disagree about reporting it there.
    #[must_use]
    pub fn lookup(&self, code: KeyCode, mods: KeyModifiers) -> Option<Action> {
        let mods = if matches!(code, KeyCode::Char(_)) {
            mods - KeyModifiers::SHIFT
        } else {
            mods
        };
        let code = match code {
            KeyCode::Char(c) if mods.contains(KeyModifiers::CONTROL) => {
                KeyCode::Char(c.to_ascii_lowercase())
            }
            other => other,
        };
        self.bindings
            .iter()
            .find(|(c, m, _)| *c == code && *m == mods)
            .map(|(_, _, a)| *a)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}

fn strip_comment(line: &str) -> &str {
    // A `#` inside quotes is a key (`"alt-#"`), not a comment.
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' | '\'' => quoted = !quoted,
            '#' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

fn unquote(text: &str) -> &str {
    text.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .or_else(|| text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')))
        .unwrap_or(text)
}

// ---------------------------------------------------------------------------
// Word motions, shared by the bindings and vi mode
// ---------------------------------------------------------------------------

/// Start of the word before `at`.
#[must_use]
pub fn word_left(line: &[char], at: usize) -> usize {
    let mut at = at.min(line.len());
    while at > 0 && line[at - 1].is_whitespace() {
        at -= 1;
    }
    while at > 0 && !line[at - 1].is_whitespace() {
        at -= 1;
    }
    at
}

/// Start of the next word after `at`.
#[must_use]
pub fn word_right(line: &[char], at: usize) -> usize {
    let mut at = at.min(line.len());
    while at < line.len() && !line[at].is_whitespace() {
        at += 1;
    }
    while at < line.len() && line[at].is_whitespace() {
        at += 1;
    }
    at
}

/// The last character of the word at or after `at` (vi `e`).
#[must_use]
pub fn word_end(line: &[char], at: usize) -> usize {
    let mut at = (at + 1).min(line.len());
    while at < line.len() && line[at].is_whitespace() {
        at += 1;
    }
    while at + 1 < line.len() && !line[at + 1].is_whitespace() {
        at += 1;
    }
    at.min(line.len().saturating_sub(1))
}

// ---------------------------------------------------------------------------
// vi normal mode
// ---------------------------------------------------------------------------

/// What a normal-mode key asks of the box beyond editing the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViOutcome {
    /// Stay in normal mode.
    Normal,
    /// Switch to insert mode.
    Insert,
    HistoryPrev,
    HistoryNext,
    /// A key normal mode does not use.
    Ignored,
}

/// One normal-mode key. `pending` holds the first key of `dd` / `cc`.
pub fn vi_normal(
    line: &mut Vec<char>,
    caret: &mut usize,
    pending: &mut Option<char>,
    key: char,
) -> ViOutcome {
    let len = line.len();
    let last = len.saturating_sub(1);
    if let Some(first) = pending.take() {
        if first == key && (key == 'd' || key == 'c') {
            line.clear();
            *caret = 0;
            return if key == 'c' {
                ViOutcome::Insert
            } else {
                ViOutcome::Normal
            };
        }
        // Any other second key cancels the operator, as in vi.
        return ViOutcome::Normal;
    }
    let outcome = match key {
        'h' => {
            *caret = caret.saturating_sub(1);
            ViOutcome::Normal
        }
        'l' => {
            *caret = (*caret + 1).min(last);
            ViOutcome::Normal
        }
        '0' | '^' => {
            *caret = 0;
            ViOutcome::Normal
        }
        '$' => {
            *caret = last;
            ViOutcome::Normal
        }
        'w' => {
            *caret = word_right(line, *caret).min(last);
            ViOutcome::Normal
        }
        'b' => {
            *caret = word_left(line, *caret);
            ViOutcome::Normal
        }
        'e' => {
            *caret = word_end(line, *caret);
            ViOutcome::Normal
        }
        'x' => {
            if *caret < len {
                line.remove(*caret);
            }
            ViOutcome::Normal
        }
        'X' => {
            if *caret > 0 {
                *caret -= 1;
                line.remove(*caret);
            }
            ViOutcome::Normal
        }
        'D' => {
            line.truncate(*caret);
            ViOutcome::Normal
        }
        'C' => {
            line.truncate(*caret);
            ViOutcome::Insert
        }
        's' => {
            if *caret < len {
                line.remove(*caret);
            }
            ViOutcome::Insert
        }
        'S' => {
            line.clear();
            *caret = 0;
            ViOutcome::Insert
        }
        'd' | 'c' => {
            *pending = Some(key);
            ViOutcome::Normal
        }
        'i' => ViOutcome::Insert,
        'a' => {
            *caret = (*caret + 1).min(len);
            ViOutcome::Insert
        }
        'I' => {
            *caret = 0;
            ViOutcome::Insert
        }
        'A' => {
            *caret = len;
            ViOutcome::Insert
        }
        'k' => ViOutcome::HistoryPrev,
        'j' => ViOutcome::HistoryNext,
        _ => ViOutcome::Ignored,
    };
    if outcome != ViOutcome::Insert {
        // Normal mode sits ON a character, never past the end.
        *caret = (*caret).min(line.len().saturating_sub(1));
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, caret: usize, keys: &str) -> (String, usize, ViOutcome) {
        let mut line: Vec<char> = text.chars().collect();
        let mut at = caret;
        let mut pending = None;
        let mut last = ViOutcome::Normal;
        for k in keys.chars() {
            last = vi_normal(&mut line, &mut at, &mut pending, k);
        }
        (line.into_iter().collect(), at, last)
    }

    #[test]
    fn the_editing_mode_is_read_from_admin_json() {
        assert_eq!(EditingMode::parse(None), EditingMode::Emacs);
        assert_eq!(EditingMode::parse(Some("vi")), EditingMode::Vi);
        assert_eq!(EditingMode::parse(Some(" VIM")), EditingMode::Vi);
        assert_eq!(EditingMode::parse(Some("emacs")), EditingMode::Emacs);
        assert_eq!(EditingMode::Vi.toggled(), EditingMode::Emacs);
        let admin: crate::admin::AdminConfig =
            serde_json::from_str(r#"{"editing_mode":"vi","statusline_command":"x"}"#).unwrap();
        assert_eq!(
            EditingMode::parse(admin.editing_mode.as_deref()),
            EditingMode::Vi
        );
    }

    #[test]
    fn vi_motions_move_and_stay_on_a_character() {
        assert_eq!(run("hello world", 11, "").1, 11);
        assert_eq!(run("hello world", 10, "l").1, 10);
        assert_eq!(run("hello world", 0, "w").1, 6);
        assert_eq!(run("hello world", 8, "b").1, 6);
        assert_eq!(run("hello world", 0, "e").1, 4);
        assert_eq!(run("hello world", 4, "$").1, 10);
        assert_eq!(run("hello world", 4, "0").1, 0);
    }

    #[test]
    fn vi_edits_do_what_vi_does() {
        assert_eq!(run("hello", 0, "x").0, "ello");
        assert_eq!(
            run("hello", 2, "D"),
            ("he".to_string(), 1, ViOutcome::Normal)
        );
        assert_eq!(run("hello", 2, "dd").0, "");
        assert_eq!(run("hello", 2, "cc"), (String::new(), 0, ViOutcome::Insert));
        assert_eq!(
            run("hello", 2, "dx").0,
            "hello",
            "an unknown second key cancels"
        );
        assert_eq!(
            run("hello", 1, "A"),
            ("hello".to_string(), 5, ViOutcome::Insert)
        );
        assert_eq!(run("hello", 4, "a").1, 5);
        assert_eq!(run("hello", 3, "I").1, 0);
        assert_eq!(run("hi", 0, "k").2, ViOutcome::HistoryPrev);
        assert_eq!(run("hi", 0, "Q").2, ViOutcome::Ignored);
        assert_eq!(run("", 0, "x"), (String::new(), 0, ViOutcome::Normal));
    }

    #[test]
    fn keys_are_named_the_way_people_write_them() {
        assert_eq!(
            parse_key("ctrl-l"),
            Some((KeyCode::Char('l'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_key("Ctrl+Alt+X"),
            Some((
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ))
        );
        assert_eq!(
            parse_key("alt-enter"),
            Some((KeyCode::Enter, KeyModifiers::ALT))
        );
        assert_eq!(parse_key("f5"), Some((KeyCode::F(5), KeyModifiers::NONE)));
        assert_eq!(
            parse_key("ctrl--"),
            Some((KeyCode::Char('-'), KeyModifiers::CONTROL))
        );
        assert_eq!(parse_key("hyper-x"), None);
        assert_eq!(parse_key("ctrl-nonsense"), None);
    }

    #[test]
    fn the_bindings_file_is_read_and_its_mistakes_are_named() {
        let (map, problems) = Keymap::parse(
            "# my keys\n[bindings]\n\"ctrl-l\" = \"clear-line\"  # wipe\n'alt-b' = 'word_left'\n\"ctrl-q\" = \"explode\"\nnonsense\n",
        );
        assert_eq!(
            map.lookup(KeyCode::Char('l'), KeyModifiers::CONTROL),
            Some(Action::ClearLine)
        );
        assert_eq!(
            map.lookup(
                KeyCode::Char('L'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            Some(Action::ClearLine)
        );
        assert_eq!(
            map.lookup(KeyCode::Char('b'), KeyModifiers::ALT),
            Some(Action::WordLeft)
        );
        assert_eq!(map.lookup(KeyCode::Char('b'), KeyModifiers::NONE), None);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("explode"));
        assert!(problems[1].contains("line 6"));
    }

    #[test]
    fn a_missing_bindings_file_is_no_bindings() {
        let (map, problems) = Keymap::load(std::path::Path::new("/definitely/not/here"));
        assert!(map.is_empty());
        assert!(problems.is_empty());
    }
}
