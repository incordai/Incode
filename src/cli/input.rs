//! Line input — the box the operator types in, and the menu of what they can type.
//!
//! Used only where there is no terminal to pin a box to — a pipe, a CI run.
//! On a real terminal the input lives in [`super::footer::Footer`], which
//! reserves the bottom rows so it cannot scroll away.
//!
//! # Why `/` opens a menu
//!
//! A slash command that cannot be discovered is a slash command that does not
//! exist. Typing `/` alone lists them; Tab completes them; and an unambiguous
//! prefix hints its description inline while it is being typed.

use std::borrow::Cow;

use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::{ColorMode, CompletionType, Config, Context, Editor};

use super::slash_commands::command_name;

/// Editor with the command menu attached.
type Prompted = Editor<Menu, rustyline::history::FileHistory>;

// ---------------------------------------------------------------------------
// Menu
// ---------------------------------------------------------------------------

/// Completion and hints over the slash commands.
#[derive(rustyline::Helper, rustyline::Validator)]
pub struct Menu;

impl Menu {
    /// The commands whose name starts with `head`, as (name, description).
    fn matching(head: &str) -> impl Iterator<Item = (&'static str, &'static str)> + '_ {
        crate::custom_commands::all_commands()
            .map(|(entry, desc)| (command_name(entry), desc))
            .filter(move |(name, _)| name.starts_with(head))
    }

    /// The part of the line that is being completed, or `None` if none is.
    ///
    /// Only the command word completes. Once there is a space the rest is an
    /// argument — a path, a query — and the menu has nothing to say about it.
    fn word(line: &str, pos: usize) -> Option<&str> {
        let mut at = pos.min(line.len());
        while at > 0 && !line.is_char_boundary(at) {
            at -= 1;
        }
        let head = &line[..at];
        if !head.starts_with('/') || head.contains(char::is_whitespace) {
            return None;
        }
        Some(head)
    }
}

impl Completer for Menu {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let Some(head) = Self::word(line, pos) else {
            return Ok((0, Vec::new()));
        };
        let found = Self::matching(head)
            .map(|(name, desc)| Pair {
                display: format!("{name:<18} {desc}"),
                replacement: format!("{name} "),
            })
            .collect();
        Ok((0, found))
    }
}

impl Hinter for Menu {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        if pos != line.len() {
            return None;
        }
        let head = Self::word(line, pos)?;
        let mut found = Self::matching(head);
        let (name, desc) = found.next()?;
        // Several still match: completing to any one of them would be a guess.
        if found.next().is_some() {
            return None;
        }
        Some(format!("{}  {desc}", name.get(head.len()..)?))
    }
}

impl Highlighter for Menu {
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        // Dim, so the hint reads as a suggestion rather than as typed text.
        Cow::Owned(format!("\x1b[2m{hint}\x1b[0m"))
    }
}

// ---------------------------------------------------------------------------
// LineEditor
// ---------------------------------------------------------------------------

/// Wraps the editor with history, a command menu, and the input frame.
pub struct LineEditor {
    editor: Prompted,
    history_path: Option<String>,
}

impl LineEditor {
    /// Create a new editor. Loads history from the given path if it exists.
    pub fn new(history_path: Option<String>) -> Self {
        let config = Config::builder()
            // List the candidates rather than cycling through them blindly:
            // the point is to SHOW what is available.
            .completion_type(CompletionType::List)
            .auto_add_history(false)
            .color_mode(ColorMode::Enabled)
            .build();
        let mut editor = Editor::with_config(config).expect("rustyline init failed");
        editor.set_helper(Some(Menu));

        if let Some(ref path) = history_path {
            let _ = editor.load_history(path);
        }

        Self {
            editor,
            history_path,
        }
    }

    /// Read one line from the user. Returns `None` on Ctrl-C / Ctrl-D / EOF.
    pub fn readline(&mut self, prompt: &str) -> Option<String> {
        match self.editor.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim().to_string();
                if !trimmed.is_empty() {
                    let _ = self.editor.add_history_entry(&line);
                }
                Some(trimmed)
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => None,
            Err(_) => None,
        }
    }

    /// Save history to disk.
    pub fn save_history(&mut self) {
        if let Some(ref path) = self.history_path {
            let _ = self.editor.save_history(path);
        }
    }
}

impl Drop for LineEditor {
    fn drop(&mut self) {
        self.save_history();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::slash_commands::COMMANDS;

    #[test]
    fn a_lone_slash_completes_to_every_command() {
        let found: Vec<_> = Menu::matching("/").collect();
        assert_eq!(
            found.len(),
            COMMANDS.len(),
            "typing `/` must offer all of them"
        );
    }

    #[test]
    fn a_prefix_narrows_to_the_commands_that_start_with_it() {
        let found: Vec<_> = Menu::matching("/do").map(|(n, _)| n).collect();
        assert_eq!(found, vec!["/doctor"]);
    }

    #[test]
    fn an_argument_is_not_a_command_to_complete() {
        assert_eq!(Menu::word("/add-dir ", 9), None);
        assert_eq!(Menu::word("/add-dir /tmp", 13), None);
        assert_eq!(Menu::word("hello", 5), None);
        assert_eq!(Menu::word("/doc", 4), Some("/doc"));
    }

    #[test]
    fn splitting_the_line_never_lands_inside_a_character() {
        // The `—` in a pasted line is three bytes; slicing blind panics. That
        // exact panic reached the operator once already, as `Error: t`.
        let line = "/x—y";
        for pos in 0..=line.len() {
            let _ = Menu::word(line, pos);
        }
    }

    #[test]
    fn an_unambiguous_prefix_hints_what_it_will_become() {
        let history = rustyline::history::MemHistory::new();
        let ctx = Context::new(&history);
        let hint = Menu.hint("/doc", 4, &ctx).expect("`/doc` is unambiguous");
        assert!(hint.starts_with("tor"), "completes the word: {hint}");
        assert!(hint.contains("Check"), "and says what it does: {hint}");
    }

    #[test]
    fn an_ambiguous_prefix_hints_nothing() {
        let history = rustyline::history::MemHistory::new();
        let ctx = Context::new(&history);
        // `/s` is at least /status, /skills, /sessions and /save.
        assert!(Menu.hint("/s", 2, &ctx).is_none());
    }

    #[test]
    fn every_listed_command_actually_parses() {
        for (entry, _) in COMMANDS {
            let name = command_name(entry);
            assert!(
                super::super::slash_commands::SlashCommand::parse(name).is_some(),
                "`{name}` is offered by the menu but nothing handles it"
            );
        }
    }
}
