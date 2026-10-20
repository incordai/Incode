//! The input box, pinned to the bottom of the terminal.
//!
//! # Why this is not a prompt
//!
//! A prompt is printed, so it scrolls away the moment anything else is written,
//! and the next one is printed lower down. Drawing a border around one was the
//! wrong fix, four times over: it still scrolled, so the box the operator was
//! typing into kept sliding up the screen with the conversation.
//!
//! What a chat application does instead is RESERVE the bottom rows. This sets a
//! scroll region — `DECSTBM`, the escape every terminal has had since the VT100
//! — so the conversation scrolls in the rows ABOVE and physically cannot touch
//! the rows below. The box is drawn once into that reserved space and stays
//! exactly where it is, whatever is printed.
//!
//! # Why the line editing is hand-written
//!
//! A line editor draws wherever the cursor happens to be. Inside a reserved
//! region it has to draw at a FIXED row, redraw itself after a resize, and grow
//! the region when the command menu opens. That is not configuration of an
//! existing editor; it is a different thing.

use std::io::{IsTerminal, Write};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};

use super::slash_commands::{command_name, COMMANDS};

/// Start of a synchronized update — the terminal shows nothing until the end.
const SYNC_BEGIN: &str = "\x1b[?2026h";
/// End of a synchronized update; the whole frame appears at once.
const SYNC_END: &str = "\x1b[?2026l";

/// Rows the box itself occupies: top rule, the typing line, bottom rule.
const BOX_ROWS: u16 = 3;
/// The most typing rows the box grows to before it stops.
const MAX_INPUT_ROWS: u16 = 8;
/// Rows the wheel moves per notch.
pub(crate) const SCROLL_STEP: usize = 3;
/// A paste of this many lines or fewer goes straight onto the line.
const PASTE_INLINE_LINES: usize = 10;
/// The most command suggestions shown at once.
const MENU_ROWS: usize = 12;
/// Below this many rows there is no room to reserve any, so the box is skipped.
const MIN_TERMINAL_ROWS: u16 = 8;

/// How many past lines are kept on disk.
///
/// There was no ceiling: [`Footer::save_history`] wrote the whole list back on
/// every submit, so the file grew for the life of the install and the rewrite
/// got longer with it. A bound costs nothing — nobody arrows back past a few
/// hundred lines — and it is what stops one long-lived install carrying every
/// prompt ever typed.
const MAX_HISTORY: usize = 500;

/// Whether a stored line is one of rustyline's own directives rather than
/// something a person typed.
///
/// **Two writers, one file.** [`super::input::LineEditor`] is used where there
/// is no terminal to pin a box to, and it is rustyline — which writes its own
/// format, beginning with a `#V2` version marker. This footer writes plain
/// lines. Whichever ran last decides the format, and this reader took every line
/// verbatim, so after one non-terminal run the literal text `#V2` sat in the
/// history and came back as the first thing Up recalled.
///
/// Dropping the directives makes either format readable as what it is: the lines
/// a person typed.
fn is_history_directive(line: &str) -> bool {
    line.starts_with('#')
}

/// The lines a person typed, newest [`MAX_HISTORY`] of them.
///
/// Split out of `Footer::open` so what it claims can be asserted without a
/// terminal to attach to — the same reason the viewport arithmetic lives apart
/// from the drawing.
fn history_from(text: &str) -> Vec<String> {
    let kept: Vec<String> = text
        .lines()
        .filter(|line| !is_history_directive(line) && !line.trim().is_empty())
        .map(str::to_string)
        .collect();
    // Keep the NEWEST when a file written before the cap existed is read: the
    // end of the list is the recent past, which is what Up is reaching for.
    kept[kept.len().saturating_sub(MAX_HISTORY)..].to_vec()
}

// ---------------------------------------------------------------------------
// Footer
// ---------------------------------------------------------------------------

/// The facts the box shows about this session, along its own rule.
///
/// Only what the harness actually knows. A cost figure and a context-window
/// percentage would both look at home here and neither is measured, so neither
/// appears — a number nobody computed is worse than a gap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    /// The model the next turn will ask for.
    pub model: String,
    /// The workspace's own name, not its whole path.
    pub workspace: String,
    /// Current branch, when this is a git tree.
    pub branch: Option<String>,
    /// Whether that tree has uncommitted changes.
    pub dirty: bool,
}

impl Session {
    /// The segments, joined the way the rule shows them.
    ///
    /// These are the facts that decide whether an instruction is safe to give —
    /// which model, which directory, which branch, and whether that branch has
    /// uncommitted work. They were only in the banner, which scrolls away.
    #[must_use]
    pub fn segments(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.model.is_empty() {
            parts.push(self.model.clone());
        }
        if !self.workspace.is_empty() {
            parts.push(self.workspace.clone());
        }
        if let Some(branch) = &self.branch {
            // The dot marks uncommitted work: the thing worth knowing before
            // telling an agent to change more of it.
            parts.push(if self.dirty {
                format!("\u{2387} {branch}\u{2022}")
            } else {
                format!("\u{2387} {branch}")
            });
        }
        parts.join(" \u{00b7} ")
    }
}

/// A permanently visible input box occupying the bottom rows of the terminal.
pub struct Footer {
    width: u16,
    height: u16,
    /// What is typed, as characters — the cursor is an index into this, and a
    /// byte index would land inside anything non-ASCII.
    line: Vec<char>,
    /// Where the caret sits, as a character index.
    caret: usize,
    /// Previously submitted lines, oldest first.
    history: Vec<String>,
    /// Where the operator is in the history while pressing Up.
    browsing: Option<usize>,
    history_path: Option<String>,
    /// Every command the typed word matches — not only the visible ones.
    menu: Vec<&'static str>,
    /// Which of them is highlighted, as an index into `menu`.
    selected: usize,
    /// The first visible entry, so a list longer than the box can be scrolled.
    menu_top: usize,
    /// Set by Escape, cleared as soon as the line changes.
    menu_hidden: bool,
    /// What this session is, shown on the box's own rule.
    session: Session,
    /// Pasted blocks too long to show, kept whole behind their markers.
    ///
    /// A hundred-line paste in a one-line box hides everything else that was
    /// typed. The box shows `[paste #1 +100 lines]`; the agent gets the paste.
    pastes: Vec<String>,
    /// Emacs-style (the default) or vi-style editing.
    editing: super::keys::EditingMode,
    /// In vi mode, whether keys insert text (otherwise they are commands).
    vi_insert: bool,
    /// The first key of a two-key vi command (`dd`, `cc`).
    vi_pending: Option<char>,
    /// The operator's own key bindings, consulted before the built-in keys.
    keymap: super::keys::Keymap,
    /// The operator's status line, shown on the rule in place of `session`.
    status_line: Option<String>,
}

impl Footer {
    /// Take the bottom of the terminal, or `None` when there is no terminal.
    pub fn open(history_path: Option<String>) -> Option<Self> {
        if !std::io::stdout().is_terminal() {
            return None;
        }
        // Take the alternate screen BEFORE measuring it.
        //
        // The main buffer has scrollback, which is what lets the operator wheel
        // the box out of view — the scroll region holds it against OUTPUT, but
        // not against the viewport moving back over history. The alternate
        // screen has no scrollback, so there is nowhere for the box to go and
        // the terminal stops drawing a scrollbar at all.
        //
        // The order matters and cost two wrong fixes. Measured first, the size
        // is the MAIN screen's; entering then changes it, so `resync` fired on
        // the first read, zeroed `reserved` to force the region to be re-set,
        // and the grow that followed scrolled the already-painted box three
        // rows up where it stayed. That was the second box on screen.
        // THE RESULT IS KEPT. It was discarded, and that is the one failure this
        // whole layout cannot survive quietly: without the alternate screen the
        // box is drawn into the main buffer, which HAS scrollback, so the
        // terminal scrolls the box along with everything else and every symptom
        // reads as a bug in the painting rather than as a screen that was never
        // taken. Reported below rather than assumed.
        let took_screen = execute!(std::io::stdout(), EnterAlternateScreen).is_ok();
        crate::trace!("ui", "alternate screen taken: {took_screen}");
        // Nothing prints itself from here on: the conversation is recorded and
        // this program paints it. Output that wrote straight to the terminal
        // would land on rows the box owns and scroll everything sideways.
        crate::cli::console::set_mode(crate::cli::console::Mode::Buffered);
        let (width, height) = crossterm::terminal::size().ok()?;
        if height < MIN_TERMINAL_ROWS {
            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
            return None;
        }
        let history: Vec<String> = history_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| history_from(&text))
            .unwrap_or_default();

        let footer = Self {
            width,
            height,
            line: Vec::new(),
            caret: 0,
            history,
            browsing: None,
            history_path,
            menu: Vec::new(),
            selected: 0,
            menu_top: 0,
            menu_hidden: false,
            session: Session::default(),
            pastes: Vec::new(),
            editing: super::keys::EditingMode::Emacs,
            vi_insert: true,
            vi_pending: None,
            keymap: super::keys::Keymap::default(),
            status_line: None,
        };
        footer.draw();
        Some(footer)
    }

    /// Read one submitted line. `None` on Ctrl-C, Ctrl-D or a closed terminal.
    pub fn read_line(&mut self) -> Option<String> {
        self.line.clear();
        self.caret = 0;
        self.browsing = None;
        // Every line starts out typing, in vi mode as in any other.
        self.vi_insert = true;
        self.vi_pending = None;
        self.refresh_menu();
        self.draw();

        if enable_raw_mode().is_err() {
            return None;
        }
        let _ = execute!(std::io::stdout(), EnableBracketedPaste);
        // Paint the prompt, typing row, and caret immediately so the box is visible
        // and ready before waiting for keystrokes.
        self.draw();

        // The wheel has to reach this program to be useful.
        //
        // On the alternate screen there is no scrollback for the terminal to
        // move through, so a wheel event is either dropped or handed to the
        // application. Captured here it scrolls the CONVERSATION and leaves the
        // box alone — which is the whole point: the background moves, the thing
        // being typed into does not.
        // Written as escapes rather than through the terminal library.
        //
        // Its Windows path calls the console API instead, which sets flags on a
        // console this program does not own: under a terminal emulator the
        // wheel is never sent, so the handler below simply never fires. Read
        // from crossterm 0.28.1 event.rs:319 — the command has both an ANSI
        // form and a `execute_winapi` one, and Windows takes the latter.
        // Mouse capture (1000 reports wheel and clicks, 1006 SGR encoding).
        // Capturing button presses intercepts mouse gestures, preventing the
        // operator from highlighting and selecting text in their terminal emulator.
        // We leave mouse capture off by default so native terminal click-and-drag text selection
        // works out of the box without requiring Shift.
        // When mouse capture is off, we disable alternate scroll mode (1007l) so the terminal
        // does not translate mouse wheel into Up/Down arrow keys.
        // Operators who explicitly want wheel scrolling in alternate screen can opt in via INCODE_MOUSE=1.
        let mouse_capture =
            mouse_capture_enabled_from(std::env::var("INCODE_MOUSE").as_deref().ok());
        if mouse_capture {
            let _ = execute!(std::io::stdout(), EnableMouseCapture);
        }
        let enable_seq = mouse_enable_sequence(mouse_capture);
        let _ = write!(std::io::stdout(), "{enable_seq}");
        let _ = std::io::stdout().flush();
        let outcome = self.key_loop();
        let disable_seq = mouse_disable_sequence(mouse_capture);
        if !disable_seq.is_empty() {
            let _ = write!(std::io::stdout(), "{disable_seq}");
            let _ = std::io::stdout().flush();
        }
        if mouse_capture {
            let _ = execute!(std::io::stdout(), DisableMouseCapture);
            let _ = std::io::stdout().flush();
        }
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();

        match outcome {
            Some(text) => {
                self.remember(&text);
                self.line.clear();
                self.caret = 0;
                self.menu.clear();
                self.draw();
                // Echoed into the conversation, so the transcript reads as an
                // exchange rather than as replies with no questions.
                //
                // BLANK LINE BEFORE, NEWLINE AFTER. Without the trailing one the
                // agent's first streamed fragment continued this very row, so
                // the question and the beginning of its answer were one line and
                // nothing marked where the operator stopped and the agent
                // started. The blank line before separates one exchange from the
                // last, which is what makes a long transcript skimmable at all.
                self.write_above(&format!("\n> {text}\n"));
                Some(text)
            }
            None => None,
        }
    }

    /// Say what this session is, so the box's rule can show it.
    pub fn set_session(&mut self, session: Session) {
        if self.session != session {
            self.session = session;
            self.draw();
        }
    }

    /// Choose emacs- or vi-style editing.
    pub fn set_editing_mode(&mut self, mode: super::keys::EditingMode) {
        self.editing = mode;
        self.vi_insert = true;
        self.vi_pending = None;
        self.draw();
    }

    /// The operator's key bindings.
    pub fn set_keymap(&mut self, keymap: super::keys::Keymap) {
        self.keymap = keymap;
    }

    /// The operator's status line; `None` shows the built-in facts.
    pub fn set_status_line(&mut self, line: Option<String>) {
        if self.status_line != line {
            self.status_line = line;
            self.draw();
        }
    }

    /// Whether vi normal mode is reading keys as commands.
    fn vi_normal(&self) -> bool {
        self.editing == super::keys::EditingMode::Vi && !self.vi_insert
    }

    /// Run a bound action. `Some(_)` ends the read, as in [`Self::on_key`].
    #[allow(clippy::option_option)]
    fn run_action(&mut self, action: super::keys::Action) -> Option<Option<String>> {
        use super::keys::Action;
        match action {
            Action::ClearLine => {
                self.line.clear();
                self.caret = 0;
            }
            Action::KillToStart => {
                self.line.drain(..self.caret);
                self.caret = 0;
            }
            Action::KillToEnd => self.line.truncate(self.caret),
            Action::DeleteWord => self.delete_word_before(),
            Action::LineStart => self.caret = 0,
            Action::LineEnd => self.caret = self.line.len(),
            Action::CharLeft => self.caret = self.caret.saturating_sub(1),
            Action::CharRight => self.caret = (self.caret + 1).min(self.line.len()),
            Action::WordLeft => self.caret = super::keys::word_left(&self.line, self.caret),
            Action::WordRight => self.caret = super::keys::word_right(&self.line, self.caret),
            Action::HistoryPrev => self.recall_history(true),
            Action::HistoryNext => self.recall_history(false),
            Action::Newline => {
                self.line.insert(self.caret, '\n');
                self.caret += 1;
                self.menu_hidden = true;
            }
            Action::Complete => self.complete(),
            Action::Submit => return self.submit(),
        }
        self.refresh_menu();
        None
    }

    /// A key in vi normal mode. `None` when the key is not a vi command and
    /// should be handled as usual (Enter, arrows, Ctrl keys).
    #[allow(clippy::option_option)]
    fn on_vi_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<Option<Option<String>>> {
        use super::keys::ViOutcome;
        let KeyCode::Char(ch) = code else {
            return None;
        };
        if mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return None;
        }
        match super::keys::vi_normal(&mut self.line, &mut self.caret, &mut self.vi_pending, ch) {
            ViOutcome::Insert => self.vi_insert = true,
            ViOutcome::HistoryPrev => self.recall_history(true),
            ViOutcome::HistoryNext => self.recall_history(false),
            ViOutcome::Normal | ViOutcome::Ignored => {}
        }
        self.refresh_menu();
        Some(None)
    }

    /// Take a pasted block, showing a marker when it is too long to display.
    ///
    /// Returns what belongs on the line. The paste itself is kept whole and put
    /// back by [`Self::expand_pastes`] when the line is submitted, so the box
    /// stays readable without the agent losing a character.
    fn accept_paste(&mut self, text: &str) -> String {
        let lines = text.lines().count();
        if lines <= PASTE_INLINE_LINES {
            return text.to_string();
        }
        self.pastes.push(text.to_string());
        format!("[paste #{} +{lines} lines]", self.pastes.len())
    }

    /// Put every pasted block back where its marker sits.
    fn expand_pastes(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (n, body) in self.pastes.iter().enumerate() {
            // Rebuilt rather than remembered, so a marker the operator edited
            // or deleted simply does not match and its paste is dropped with it.
            let lines = body.lines().count();
            let marker = format!("[paste #{} +{lines} lines]", n + 1);
            out = out.replace(&marker, body);
        }
        out
    }

    /// Print into the scrolling region above the box.
    ///
    /// The cursor is parked on the last scrolling row, so ordinary `println!`
    /// from anywhere else in the program lands here too and scrolls under the
    /// box without the caller knowing the box exists.
    pub fn write_above(&self, text: &str) {
        crate::cli::console::emit_line(text);
        self.draw();
    }

    /// Give the terminal back, and the conversation with it.
    pub fn close(&mut self) {
        self.save_history();
        let mut out = std::io::stdout();
        let _ = execute!(out, DisableMouseCapture);
        // Reset DECSTBM to the whole screen, or every later command scrolls
        // inside the region this left behind.
        let _ = write!(out, "\x1b[r\x1b[?1006l\x1b[?1002l\x1b[?1000l");
        crate::cli::console::set_mode(crate::cli::console::Mode::Direct);
        let _ = execute!(out, LeaveAlternateScreen, Show);
        // The alternate screen is discarded on the way out, so without this the
        // whole session would vanish the moment it ended. Every line went
        // through the console, which is what makes replaying it possible.
        for line in crate::cli::console::lines() {
            let _ = writeln!(out, "{line}");
        }
        let trailing = crate::cli::console::pending();
        if !trailing.is_empty() {
            let _ = writeln!(out, "{trailing}");
        }
        let _ = out.flush();
        // Reset DECSTBM to the whole screen, or every later command scrolls
        // inside the region this left behind.
        let _ = write!(out, "\x1b[r");
        let _ = execute!(out, MoveTo(0, self.height.saturating_sub(1)), Show);
        let _ = writeln!(out);
        let _ = out.flush();
    }

    // -- input ------------------------------------------------------------

    fn key_loop(&mut self) -> Option<String> {
        loop {
            let ev = event::read().ok()?;
            match ev {
                Event::Resize(w, h) => {
                    // Erase the box FIRST, while these fields still say where
                    // it is. Zeroing `reserved` below forces the region to be
                    // re-set, but it makes the fields lie about the box, and
                    // the grow that follows scrolls the painted one up the
                    // screen where it stays.
                    //
                    // This is the sibling of the same code in `resync`, and it
                    // is the one that actually fires: entering the alternate
                    // screen makes the terminal report a resize. Fixing only
                    // the other one changed nothing, twice.
                    self.width = w;
                    self.height = h;
                    self.draw();
                }
                // Arrives whole, rather than as hundreds of keystrokes, because
                // bracketed paste is enabled for the read. Without it a pasted
                // block is indistinguishable from typing and a newline in it
                // submits the line halfway through.
                Event::Paste(text) => {
                    let shown = self.accept_paste(&text);
                    for ch in shown.chars() {
                        self.line.insert(self.caret, ch);
                        self.caret += 1;
                    }
                    self.refresh_menu();
                    self.draw();
                }
                Event::Mouse(MouseEvent { kind, .. }) => match kind {
                    MouseEventKind::ScrollUp => {
                        crate::cli::paint::scroll_up(SCROLL_STEP, self.width, self.height);
                        crate::cli::paint::conversation(self.width, self.height);
                        self.draw();
                    }
                    MouseEventKind::ScrollDown => {
                        crate::cli::paint::scroll_down(SCROLL_STEP);
                        crate::cli::paint::conversation(self.width, self.height);
                        self.draw();
                    }
                    _ => {}
                },
                Event::Key(KeyEvent {
                    code,
                    modifiers,
                    kind: KeyEventKind::Press,
                    ..
                }) => {
                    if let Some(done) = self.on_key(code, modifiers) {
                        return done;
                    }
                    self.draw();
                }
                _ => {}
            }
        }
    }

    /// Handle one keypress. `Some(_)` ends the read; `None` keeps editing.
    #[allow(clippy::option_option)]
    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<Option<String>> {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        if let Some(action) = self.keymap.lookup(code, mods) {
            return self.run_action(action);
        }
        if self.vi_normal() {
            if let Some(done) = self.on_vi_key(code, mods) {
                return done;
            }
        }
        match code {
            KeyCode::Char('c') if ctrl => return Some(None),
            KeyCode::Char('d') if ctrl && self.line.is_empty() => return Some(None),
            // Ctrl-U clears to the START of the line, which is what it does in
            // every readline-shaped editor; it used to clear the whole line.
            KeyCode::Char('u') if ctrl => {
                self.line.drain(..self.caret);
                self.caret = 0;
            }
            KeyCode::Char('k') if ctrl => {
                self.line.truncate(self.caret);
            }
            KeyCode::Char('a') if ctrl => self.caret = 0,
            KeyCode::Char('e') if ctrl => self.caret = self.line.len(),
            KeyCode::Char('w') if ctrl => self.delete_word_before(),
            // Alt-Backspace is the same gesture on a keyboard that sends it.
            KeyCode::Backspace if mods.contains(KeyModifiers::ALT) => self.delete_word_before(),
            KeyCode::Char(ch) => {
                self.line.insert(self.caret, ch);
                self.caret += 1;
                self.menu_hidden = false;
                // Typing means attention is back on the newest output, not on
                // whatever was being read further up.
                crate::cli::paint::scroll_to_bottom();
            }
            KeyCode::Backspace if self.caret > 0 => {
                self.caret -= 1;
                self.line.remove(self.caret);
            }
            KeyCode::Delete if self.caret < self.line.len() => {
                self.line.remove(self.caret);
            }
            KeyCode::Left => self.caret = self.caret.saturating_sub(1),
            KeyCode::Right => self.caret = (self.caret + 1).min(self.line.len()),
            KeyCode::Home => self.caret = 0,
            KeyCode::End => self.caret = self.line.len(),
            // While the menu is up the arrows steer IT. Off the menu they are
            // history, which is what they were before there was a menu.
            KeyCode::Up if self.menu_open() => {
                self.selected = self.selected.saturating_sub(1);
                self.scroll_to_selection();
            }
            KeyCode::Down if self.menu_open() => {
                self.selected = (self.selected + 1).min(self.menu.len().saturating_sub(1));
                self.scroll_to_selection();
            }
            KeyCode::Up if mods.contains(KeyModifiers::SHIFT) => {
                crate::cli::paint::scroll_up(SCROLL_STEP, self.width, self.height);
                crate::cli::paint::conversation(self.width, self.height);
            }
            KeyCode::Down if mods.contains(KeyModifiers::SHIFT) => {
                crate::cli::paint::scroll_down(SCROLL_STEP);
                crate::cli::paint::conversation(self.width, self.height);
            }
            KeyCode::Up => self.recall_history(true),
            KeyCode::Down => self.recall_history(false),
            KeyCode::PageUp => {
                crate::cli::paint::scroll_up(
                    crate::cli::paint::conversation_height(self.height).max(1),
                    self.width,
                    self.height,
                );
            }
            KeyCode::PageDown => {
                crate::cli::paint::scroll_down(
                    crate::cli::paint::conversation_height(self.height).max(1),
                );
            }
            KeyCode::Esc if self.menu_open() => self.menu_hidden = true,
            // Insert mode ends on Esc, and the caret steps back onto the
            // character it was after, as vi's does.
            KeyCode::Esc if self.editing == super::keys::EditingMode::Vi && self.vi_insert => {
                self.vi_insert = false;
                self.vi_pending = None;
                self.caret = self.caret.saturating_sub(1);
            }
            KeyCode::Tab => self.complete(),
            // A newline rather than a submit. Terminals disagree about which of
            // these they send, so all three are accepted; Alt-Enter is the one
            // that works most widely.
            KeyCode::Enter
                if mods.intersects(
                    KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::CONTROL,
                ) =>
            {
                self.line.insert(self.caret, '\n');
                self.caret += 1;
                self.menu_hidden = true;
            }
            KeyCode::Enter => return self.submit(),
            _ => return None,
        }
        self.refresh_menu();
        None
    }

    /// What Enter does: the line (or the chosen command), or `None` to keep
    /// editing an empty line.
    #[allow(clippy::option_option)]
    fn submit(&self) -> Option<Option<String>> {
        // A command is CHOSEN from the menu, not typed out in full. The
        // highlighted entry is what runs; the half-typed prefix on the
        // line is not a command and would only be rejected.
        let text = if self.menu_open() {
            self.menu.get(self.selected).map(|name| (*name).to_string())
        } else {
            None
        }
        .unwrap_or_else(|| self.line.iter().collect::<String>().trim().to_string());
        if text.is_empty() {
            return None;
        }
        // The box showed a marker; the agent gets the whole paste.
        Some(Some(self.expand_pastes(&text)))
    }

    /// What is typed, split into the lines the box draws.
    ///
    /// Always at least one, so an empty box still has a row for the caret.
    fn lines(&self) -> Vec<String> {
        let typed: String = self.line.iter().collect();
        let mut out: Vec<String> = typed.split('\n').map(str::to_string).collect();
        if out.is_empty() {
            out.push(String::new());
        }
        out
    }

    /// Which drawn line the caret is on, and how far along it.
    fn caret_position(&self) -> (usize, usize) {
        let mut line = 0;
        let mut offset = 0;
        for ch in self.line.iter().take(self.caret) {
            if *ch == '\n' {
                line += 1;
                offset = 0;
            } else {
                offset += 1;
            }
        }
        (line, offset)
    }

    /// How many rows the typing area needs.
    fn text_rows(&self) -> u16 {
        // Capped, or one pasted screenful takes the whole terminal and leaves
        // nowhere for the conversation it is being written into.
        u16::try_from(self.lines().len())
            .unwrap_or(1)
            .clamp(1, MAX_INPUT_ROWS)
    }

    /// Delete the word before the caret, and the run of spaces before it.
    ///
    /// Trailing spaces go first so that deleting after `foo bar ` removes
    /// `bar `, not just the gap — which is what makes the key feel right.
    fn delete_word_before(&mut self) {
        let mut at = self.caret;
        while at > 0 && self.line[at - 1].is_whitespace() {
            at -= 1;
        }
        while at > 0 && !self.line[at - 1].is_whitespace() {
            at -= 1;
        }
        self.line.drain(at..self.caret);
        self.caret = at;
    }

    /// Replace the command word with the single command that matches.
    fn complete(&mut self) {
        let typed: String = self.line.iter().collect();
        let Some(head) = command_word(&typed) else {
            return;
        };
        let matches = matching(head);
        // With one match this completes it; with several, the menu is already
        // showing them and the shared prefix is as far as anyone can honestly go.
        let filled = match matches.as_slice() {
            [only] => format!("{only} "),
            [] => return,
            // With the menu open, Tab takes whatever is highlighted — the
            // operator can see which one that is. With it closed, the shared
            // prefix is as far as anyone can honestly go.
            many => match (self.menu_open(), self.menu.get(self.selected)) {
                (true, Some(name)) => format!("{name} "),
                _ => shared_prefix(many),
            },
        };
        if filled.len() <= head.len() {
            return;
        }
        self.line = filled.chars().collect();
        self.caret = self.line.len();
        self.refresh_menu();
    }

    fn recall_history(&mut self, back: bool) {
        if self.history.is_empty() {
            return;
        }
        let at = match (self.browsing, back) {
            (None, true) => self.history.len() - 1,
            (None, false) => return,
            (Some(0), true) => 0,
            (Some(i), true) => i - 1,
            (Some(i), false) if i + 1 < self.history.len() => i + 1,
            (Some(_), false) => {
                self.browsing = None;
                self.line.clear();
                self.caret = 0;
                return;
            }
        };
        self.browsing = Some(at);
        self.line = self.history[at].chars().collect();
        self.caret = self.line.len();
    }

    fn remember(&mut self, text: &str) {
        if self.history.last().map(String::as_str) == Some(text) {
            return;
        }
        self.history.push(text.to_string());
        self.save_history();
    }

    fn save_history(&self) {
        if let Some(ref path) = self.history_path {
            // Bounded at the WRITE as well as the read, or the file keeps every
            // line this process ever held and only the reading is capped.
            let from = self.history.len().saturating_sub(MAX_HISTORY);
            let _ = std::fs::write(path, self.history[from..].join("\n"));
        }
    }

    // -- layout -----------------------------------------------------------

    fn refresh_menu(&mut self) {
        let typed: String = self.line.iter().collect();
        let found = command_word(&typed).map(matching).unwrap_or_default();
        if found != self.menu {
            // A different set of candidates means the old highlight pointed at
            // something that may no longer be offered.
            self.selected = 0;
            self.menu_top = 0;
            self.menu_hidden = false;
        }
        // The FULL list is kept. Truncating it here is what made the menu say
        // there were six commands when there are twenty-nine.
        self.menu = found;
        self.scroll_to_selection();
    }

    /// Keep the highlighted entry inside the visible window.
    fn scroll_to_selection(&mut self) {
        if self.selected < self.menu_top {
            self.menu_top = self.selected;
        } else if self.selected >= self.menu_top + MENU_ROWS {
            self.menu_top = self.selected + 1 - MENU_ROWS;
        }
    }

    /// Whether the menu is on screen and can be steered.
    fn menu_open(&self) -> bool {
        !self.menu.is_empty() && !self.menu_hidden
    }

    /// How many entries are shown at once.
    ///
    /// Bounded by the SCREEN as well as by [`MENU_ROWS`]. `wanted_rows` clamps
    /// the total to what the terminal has, so a cap that ignored the height
    /// would have this return more rows than were reserved — and the loop that
    /// draws them would run off the bottom, scrolling the screen. That is the
    /// one thing a pinned box cannot survive, so the limit is applied where the
    /// count is decided rather than after it.
    fn visible_count(&self) -> usize {
        if !self.menu_open() {
            return 0;
        }
        let extra = usize::from(self.text_rows().saturating_sub(1));
        // The box, the typing rows it grew by, the activity row above it, and a
        // row of conversation so the list never becomes the whole screen.
        let room = usize::from(self.height)
            .saturating_sub(usize::from(BOX_ROWS))
            .saturating_sub(extra)
            .saturating_sub(3);
        self.menu
            .len()
            .saturating_sub(self.menu_top)
            .min(MENU_ROWS)
            .min(room)
    }

    fn wanted_rows(&self) -> u16 {
        let menu = u16::try_from(self.visible_count()).unwrap_or(0);
        // BOX_ROWS counts one typing row; the rest are the extra lines.
        let extra = self.text_rows().saturating_sub(1);
        (BOX_ROWS + menu + extra).min(self.height.saturating_sub(2))
    }

    /// Paint the reserved rows: the menu, then the box.
    fn draw(&self) {
        // Tell the painter how many rows are ours before it paints, or the
        // conversation is drawn into rows the box is about to cover.
        crate::cli::paint::set_box_rows(self.wanted_rows());
        // EVERY NUMBER THE LAYOUT DEPENDS ON, once per frame. Between them they
        // answer the questions that cost this file four wrong fixes: is the
        // terminal the size we think, does the box start where the conversation
        // stops, and is anything left over in between.
        crate::trace!(
            "ui",
            "box: mode={:?} terminal={:?} h={} want={} rows={} first_box={} conv={} status_row={} menu={}",
            crate::cli::console::mode(),
            crossterm::terminal::size(),
            self.height,
            self.wanted_rows(),
            crate::cli::paint::box_rows(),
            crate::cli::paint::first_box_row(self.height),
            crate::cli::paint::conversation_height(self.height),
            crate::cli::paint::status_row(self.height),
            self.visible_count()
        );
        crate::cli::paint::conversation(self.width, self.height);
        // The activity row belongs to a running turn, and no turn is running
        // while the box is reading. Nobody else paints it between turns, so
        // whatever was last there stays — after the command menu shrank, its
        // bottom entry sat on this row looking like output.
        crate::cli::paint::status(self.height, "");

        // Built into a buffer rather than straight onto the terminal, so the
        // finished frame can be handed to `set_box_frame` and replayed by the
        // ticker. One emit also means the box arrives whole: written directly,
        // a frame competes for stdout with whatever the running turn is
        // printing and can be split down the middle.
        let mut out: Vec<u8> = Vec::new();
        let _ = write!(out, "{SYNC_BEGIN}");
        let first = crate::cli::paint::first_box_row(self.height);
        let width = (self.width.max(8) as usize).saturating_sub(1);
        let _ = execute!(out, Hide);

        let mut row = first;
        let inner = input_capacity(width);

        // THE LIST SITS ABOVE THE FRAME, and the frame comes after it.
        //
        // It used to be drawn inside the borders, on the reasoning that commands
        // belong to the thing being typed into. That reasoning was fine and the
        // result was not: inside the frame the list is boxed in beside the
        // typing row, only a few entries fit before it has to be scrolled, and
        // choosing from a list you have to scroll to see is worse than not
        // having one.
        //
        // Above it, the list gets the width and the room of the background
        // without becoming part of it: these rows are RESERVED by
        // `wanted_rows`, so the conversation never paints over them, and they
        // are given straight back the moment the menu closes. Visible while
        // choosing, gone afterwards — never a permanent band across the screen.
        let shown = self.visible_count();
        for offset in 0..shown {
            let at = self.menu_top + offset;
            let Some(name) = self.menu.get(at) else { break };
            let desc = COMMANDS
                .iter()
                .find(|(entry, _)| command_name(entry) == *name)
                .map_or("", |(_, d)| *d);
            let chosen = at == self.selected;
            let marker = if chosen { "\u{203a}" } else { " " };
            // Two columns further in than the frame, so the list reads as
            // hovering over the background rather than as output that happens
            // to start at the margin.
            let text = fit(&format!("{marker} {name:<14} {desc}"), inner);
            let pad = " ".repeat(inner.saturating_sub(text.chars().count()));
            let _ = execute!(out, MoveTo(0, row), Clear(ClearType::CurrentLine));
            let _ = write!(out, "  ");
            if chosen {
                let _ = write!(out, "\x1b[7m{text}{pad}\x1b[0m");
            } else {
                let _ = write!(out, "\x1b[2m{text}{pad}\x1b[0m");
            }
            row = row.saturating_add(1);
        }

        // The operator's own line, when they configured one, in place of the
        // built-in facts.
        let segments = match &self.status_line {
            Some(line) => line.clone(),
            None => self.session.segments(),
        };
        let mut hint = self.more_hint();
        if self.vi_normal() {
            hint.push_str(" NORMAL ");
        }
        let _ = execute!(out, MoveTo(0, row), Clear(ClearType::CurrentLine));
        let _ = write!(
            out,
            "\x1b[2m{}\x1b[0m",
            top_rule_with(width, &segments, &hint)
        );
        row = row.saturating_add(1);

        let (caret_line, caret_offset) = self.caret_position();
        let mut caret_col = 2;
        let mut caret_row = row;
        for (n, logical) in self.lines().iter().enumerate() {
            let text = fit(logical, inner);
            let _ = execute!(out, MoveTo(0, row), Clear(ClearType::CurrentLine));
            let _ = write!(
                out,
                "\x1b[2m│\x1b[0m {text}{}\x1b[2m│\x1b[0m",
                " ".repeat(inner.saturating_sub(text.chars().count()))
            );
            if n == caret_line {
                caret_col = 2 + u16::try_from(caret_offset.min(inner)).unwrap_or(0);
                caret_row = row;
            }
            row = row.saturating_add(1);
        }

        let _ = execute!(out, MoveTo(0, row), Clear(ClearType::CurrentLine));
        let _ = write!(out, "\x1b[2m{}\x1b[0m", bottom_rule(width));

        let _ = execute!(out, MoveTo(caret_col, caret_row), Show);
        let _ = write!(out, "{SYNC_END}");

        let mut screen = std::io::stdout();
        let _ = screen.write_all(&out);
        let _ = screen.flush();
        crate::cli::paint::set_box_frame(out);
    }

    /// What is off-screen above and below the visible slice of the menu.
    ///
    /// Without this the menu simply stopped after six entries and there was no
    /// way to know the other twenty-three existed.
    fn more_hint(&self) -> String {
        if !self.menu_open() {
            return String::new();
        }
        hint_for(self.menu_top, self.visible_count(), self.menu.len())
    }
}

impl Drop for Footer {
    fn drop(&mut self) {
        // A process that exits without resetting DECSTBM leaves the operator's
        // shell scrolling inside a region it never asked for.
        self.close();
        let _ = disable_raw_mode();
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// The command word being typed, or `None` when the line is not one.
#[must_use]
pub fn command_word(line: &str) -> Option<&str> {
    if !line.starts_with('/') || line.contains(char::is_whitespace) {
        return None;
    }
    Some(line)
}

/// Commands whose name starts with `head`.
#[must_use]
pub fn matching(head: &str) -> Vec<&'static str> {
    COMMANDS
        .iter()
        .map(|(entry, _)| command_name(entry))
        .filter(|name| name.starts_with(head))
        .collect()
}

/// The longest prefix every candidate shares.
#[must_use]
pub fn shared_prefix(names: &[&str]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut end = first.len();
    for name in &names[1..] {
        end = end.min(
            first
                .chars()
                .zip(name.chars())
                .take_while(|(a, b)| a == b)
                .count(),
        );
    }
    first.chars().take(end).collect()
}

/// How many characters fit between the two side borders.
///
/// The row is `│` + space + text + `│`, so three columns are spoken for. It was
/// four, which left the caret row one column shorter than the rules above and
/// below it and gave the box a ragged right edge.
#[must_use]
pub fn input_capacity(width: usize) -> usize {
    width.saturating_sub(3)
}

/// The typing row exactly as it is drawn, without the dimming escapes.
///
/// Exists so the three rows of the box can be measured against each other in a
/// test. They disagreed by one column and only the operator's eye caught it.
#[must_use]
pub fn input_row(text: &str, width: usize) -> String {
    let inner = input_capacity(width);
    let shown = fit(text, inner);
    format!(
        "│ {shown}{}│",
        " ".repeat(inner.saturating_sub(shown.chars().count()))
    )
}

/// Cut to `width` characters, never inside one.
#[must_use]
pub fn fit(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

/// What to say about entries scrolled off the menu, from the window alone.
///
/// Separated from [`Footer::more_hint`] so the arithmetic can be tested without
/// a terminal to attach to.
#[must_use]
pub fn hint_for(top: usize, visible: usize, total: usize) -> String {
    let below = total.saturating_sub(top + visible);
    match (top, below) {
        (0, 0) => String::new(),
        (0, b) => format!(" ↓ {b} more "),
        (a, 0) => format!(" ↑ {a} more "),
        (a, b) => format!(" ↑ {a}  ↓ {b} "),
    }
}

/// The line above the typing area, carrying anything the menu needs to say.
///
/// The hint rides the rule rather than taking a row of its own: it sits
/// directly under the menu it describes, and it cannot scroll away.
#[must_use]
pub fn top_rule(width: usize, hint: &str) -> String {
    top_rule_with(width, "", hint)
}

/// The rule with what this session IS written along it.
///
/// The facts that decide whether an instruction is safe to give — which model,
/// which directory, which branch, and whether that branch has uncommitted work
/// — were only in the banner that scrolled away ten minutes ago. On the rule
/// they are in front of the operator at the moment they type.
///
/// The segments are dropped before the hint when the terminal is too narrow to
/// carry both: the hint says the menu continues, and losing it strands the
/// reader, while the segments are merely useful.
#[must_use]
pub fn top_rule_with(width: usize, segments: &str, hint: &str) -> String {
    // With facts on the rule the word "you" is noise: the box's position
    // already says whose it is, and the room is better spent on the facts.
    let label = if segments.is_empty() {
        "\u{2500} you "
    } else {
        "\u{2500} "
    };
    let fixed = 1 + label.chars().count() + hint.chars().count() + 1;
    let room = width.saturating_sub(fixed);
    let shown = if segments.is_empty() || segments.chars().count() + 2 > room {
        String::new()
    } else {
        format!("{segments} ")
    };
    let dashes = room.saturating_sub(shown.chars().count());
    format!(
        "\u{256d}{label}{shown}{}{hint}\u{256e}",
        "\u{2500}".repeat(dashes)
    )
}

/// The line below the typing area.
#[must_use]
pub fn bottom_rule(width: usize) -> String {
    format!("╰{}╯", "─".repeat(width.saturating_sub(2)))
}

/// Determine whether mouse capture should be enabled from an environment value.
/// Defaults to true so that mouse wheel scrolling scrolls the terminal conversation
/// rather than hijacking the input box by sending Up/Down arrow history keys.
pub(crate) fn mouse_capture_enabled_from(env_val: Option<&str>) -> bool {
    match env_val {
        Some(v) => v == "1" || v.eq_ignore_ascii_case("true"),
        None => true,
    }
}

/// Sequence to emit when entering read_line. When enabled, SGR mouse reporting is activated
/// so the mouse wheel scrolls the conversation rather than sending Up/Down arrow keys.
pub(crate) fn mouse_enable_sequence(enabled: bool) -> &'static str {
    if enabled {
        "\x1b[?1000h\x1b[?1002h\x1b[?1006h"
    } else {
        "\x1b[?1007l"
    }
}

/// Sequence to emit when leaving read_line to restore terminal state.
pub(crate) fn mouse_disable_sequence(enabled: bool) -> &'static str {
    if enabled {
        "\x1b[?1006l\x1b[?1002l\x1b[?1000l"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The other writer's format is not something a person typed.**
    ///
    /// One file, two writers: `input::LineEditor` is rustyline and stamps a
    /// `#V2` marker at the top; this footer writes plain lines. After any
    /// non-terminal run the reader took that marker verbatim, so the literal
    /// text `#V2` was the first thing Up recalled.
    #[test]
    fn the_rustyline_version_marker_is_never_recalled_as_a_typed_line() {
        let history = history_from("#V2\n/help\nhi there\n");
        assert_eq!(history, vec!["/help".to_string(), "hi there".to_string()]);
    }

    /// Blank lines are not history either — they come from the trailing newline
    /// the join leaves behind, and recalling one puts an empty prompt up.
    #[test]
    fn blank_lines_are_not_kept() {
        assert_eq!(history_from("one\n\n\ntwo\n"), vec!["one", "two"]);
    }

    /// **The file is bounded.** It had no ceiling and was rewritten whole on
    /// every submit, so it grew for the life of the install.
    #[test]
    fn only_the_most_recent_lines_are_kept() {
        let text: String = (0..MAX_HISTORY + 40)
            .map(|n| format!("line-{n}\n"))
            .collect();
        let history = history_from(&text);
        assert_eq!(history.len(), MAX_HISTORY);
        // The NEWEST end is what survives — Up reaches for the recent past.
        assert_eq!(history[0], format!("line-{}", 40));
        assert_eq!(
            history[MAX_HISTORY - 1],
            format!("line-{}", MAX_HISTORY + 39)
        );
    }

    /// A file that was never written yields nothing rather than one empty entry.
    #[test]
    fn an_empty_history_file_yields_no_entries() {
        assert!(history_from("").is_empty());
        assert!(history_from("\n\n").is_empty());
    }

    #[test]
    fn the_box_is_exactly_as_wide_as_the_terminal_allows() {
        for width in [24usize, 40, 80, 120] {
            assert_eq!(top_rule(width, "").chars().count(), width);
            assert_eq!(bottom_rule(width).chars().count(), width);
        }
    }

    #[test]
    fn the_box_never_touches_the_last_column() {
        for terminal in [40usize, 80, 100, 120] {
            let drawn = terminal.saturating_sub(1);
            assert!(
                top_rule(drawn, "").chars().count() < terminal,
                "a {terminal}-column terminal would wrap"
            );
            assert!(bottom_rule(drawn).chars().count() < terminal);
        }
    }

    #[test]
    fn the_rule_stays_the_same_width_whatever_the_hint_says() {
        // The hint lives ON the rule, so a longer hint must eat dashes rather
        // than push the corner past the edge and wrap the box.
        for hint in ["", " ↓ 23 more ", " ↑ 12  ↓ 4 "] {
            assert_eq!(
                top_rule(80, hint).chars().count(),
                80,
                "hint {hint:?} changed the width"
            );
            assert!(top_rule(80, hint).ends_with('╮'));
        }
    }

    #[test]
    fn the_hint_says_how_many_are_hidden() {
        // Six of twenty-nine were shown with nothing to say the rest existed.
        assert!(hint_for(0, 6, 29).contains("23"));
        assert!(hint_for(0, 6, 29).contains('↓'));
        assert!(hint_for(23, 6, 29).contains('↑'));
        assert_eq!(hint_for(0, 6, 6), "", "nothing hidden, nothing said");
    }

    #[test]
    fn all_three_rows_of_the_box_are_the_same_width() {
        // The right edge was ragged: the typing row came out one column short
        // of the rules, so the box did not close.
        for width in [24usize, 40, 80, 99, 120] {
            let top = top_rule(width, "").chars().count();
            let mid = input_row("", width).chars().count();
            let bottom = bottom_rule(width).chars().count();
            assert_eq!(top, mid, "top {top} vs typing row {mid} at width {width}");
            assert_eq!(mid, bottom, "typing row {mid} vs bottom {bottom}");
        }
    }

    #[test]
    fn a_full_line_does_not_push_the_border_out() {
        let long = "x".repeat(500);
        assert_eq!(input_row(&long, 80).chars().count(), 80);
        assert!(input_row(&long, 80).ends_with('│'));
    }

    /// Drive the editing keys the way a keyboard does, so the assertions are
    /// about what the operator ends up with, not about internal state.
    /// The bug this pins: during a turn the box has NO owner. The thread that
    /// draws it is parked reading, the ticker repaints the conversation at
    /// absolute rows, and anything that scrolls the screen carries the box off
    /// with it — the box was gone until the operator typed, which is after they
    /// had already lost it.
    ///
    /// So `draw` must leave its finished frame somewhere ANOTHER thread can
    /// paint from. Delete the `set_box_frame` call at the end of `draw` and
    /// this fails: the store stays empty and a replay paints nothing.
    ///
    /// The only test that touches the process-global frame store, so it needs
    /// no lock of its own.
    #[test]
    fn drawing_the_box_leaves_a_frame_the_ticker_can_replay() {
        crate::cli::paint::set_box_frame(Vec::new());
        assert_eq!(
            crate::cli::paint::frame_to_replay(),
            None,
            "nothing has drawn yet, so the ticker must have nothing to paint"
        );

        typed("hello").draw();

        let frame = crate::cli::paint::frame_to_replay()
            .expect("drawing the box must leave a frame for the ticker");
        let text = String::from_utf8_lossy(&frame);
        assert!(
            text.contains("hello"),
            "the replayed frame must carry what was typed, got: {text:?}"
        );
        assert!(
            text.contains("\u{1b}["),
            "the frame must position itself absolutely, or a replay lands \
             wherever the cursor happened to be"
        );

        crate::cli::paint::set_box_frame(Vec::new());
    }

    fn typed(text: &str) -> Footer {
        Footer {
            width: 80,
            height: 30,
            line: text.chars().collect(),
            caret: text.chars().count(),
            history: Vec::new(),
            browsing: None,
            history_path: None,
            menu: Vec::new(),
            selected: 0,
            menu_top: 0,
            menu_hidden: false,
            session: Session::default(),
            pastes: Vec::new(),
            editing: crate::cli::keys::EditingMode::Emacs,
            vi_insert: true,
            vi_pending: None,
            keymap: crate::cli::keys::Keymap::default(),
            status_line: None,
        }
    }

    #[test]
    fn in_vi_mode_escape_leaves_insert_and_dd_clears_the_line() {
        let mut f = typed("fix the parser");
        f.editing = crate::cli::keys::EditingMode::Vi;
        assert!(f.on_key(KeyCode::Esc, KeyModifiers::NONE).is_none());
        assert!(f.vi_normal());
        assert_eq!(f.caret, 13, "Esc steps back onto the last character");
        f.on_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert_eq!(f.caret, 8);
        f.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        f.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(line_of(&f), "");
        f.on_key(KeyCode::Char('i'), KeyModifiers::NONE);
        f.on_key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(line_of(&f), "x", "after i, keys type again");
    }

    #[test]
    fn enter_submits_from_vi_normal_mode() {
        let mut f = typed("go");
        f.editing = crate::cli::keys::EditingMode::Vi;
        f.on_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(
            f.on_key(KeyCode::Enter, KeyModifiers::NONE),
            Some(Some("go".to_string()))
        );
    }

    #[test]
    fn a_bound_key_runs_its_action_before_the_built_in_one() {
        let mut f = typed("hello world");
        let (map, _) = crate::cli::keys::Keymap::parse(
            "\"ctrl-a\" = \"word-left\"\n\"ctrl-l\" = \"clear-line\"",
        );
        f.keymap = map;
        f.on_key(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(f.caret, 6, "ctrl-a now moves a word, not to the start");
        f.on_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(line_of(&f), "");
    }

    fn line_of(f: &Footer) -> String {
        f.line.iter().collect()
    }

    /// A long menu on a short screen must not ask for more rows than exist.
    ///
    /// `wanted_rows` clamps the TOTAL to the terminal height, so a menu count
    /// that ignored the height would have the draw loop run past the bottom
    /// row — and writing past the bottom scrolls the screen, carrying the
    /// pinned box with it. The count is where the limit has to be applied.
    #[test]
    fn the_menu_never_asks_for_more_rows_than_the_screen_has() {
        let mut f = typed("/");
        f.menu = vec!["/command"; 40];
        for height in [8u16, 10, 14, 24, 40] {
            f.height = height;
            let shown = u16::try_from(f.visible_count()).expect("fits");
            let extra = f.text_rows().saturating_sub(1);
            assert!(
                shown + BOX_ROWS + extra < height,
                "height {height}: menu {shown} + box {BOX_ROWS} + {extra} does not fit"
            );
            assert_eq!(
                f.wanted_rows(),
                shown + BOX_ROWS + extra,
                "height {height}: the reserved rows must match what is drawn"
            );
        }
    }

    #[test]
    fn a_tall_screen_shows_more_of_the_menu_than_a_short_one() {
        // The control for the bound above: it must limit a short screen without
        // capping a roomy one, or the fix would be "show almost nothing".
        let mut f = typed("/");
        f.menu = vec!["/command"; 40];
        f.height = 10;
        let cramped = f.visible_count();
        f.height = 40;
        let roomy = f.visible_count();
        assert!(roomy > cramped, "cramped {cramped}, roomy {roomy}");
        assert_eq!(
            roomy, MENU_ROWS,
            "a tall screen is capped only by MENU_ROWS"
        );
    }

    #[test]
    fn ctrl_w_deletes_the_word_and_the_gap_before_it() {
        let mut f = typed("cargo test --release ");
        f.delete_word_before();
        assert_eq!(
            line_of(&f),
            "cargo test ",
            "trailing gap goes with the word"
        );
        f.delete_word_before();
        assert_eq!(line_of(&f), "cargo ");
        assert_eq!(f.caret, 6, "the caret follows the text it deleted");
    }

    #[test]
    fn deleting_a_word_from_an_empty_line_is_not_a_panic() {
        let mut f = typed("");
        f.delete_word_before();
        assert_eq!(line_of(&f), "");
        assert_eq!(f.caret, 0);
    }

    #[test]
    fn word_deletion_counts_characters_not_bytes() {
        // An em dash is three bytes. Draining by byte index would panic, and
        // that exact class of bug has already reached the operator once.
        let mut f = typed("fix the — thing");
        f.delete_word_before();
        assert_eq!(line_of(&f), "fix the — ");
    }

    #[test]
    fn ctrl_u_clears_to_the_start_and_ctrl_k_to_the_end() {
        let mut f = typed("keep this");
        f.caret = 5; // after "keep "
        f.line.drain(..f.caret);
        f.caret = 0;
        assert_eq!(line_of(&f), "this");

        let mut g = typed("keep this");
        g.caret = 4;
        g.line.truncate(g.caret);
        assert_eq!(line_of(&g), "keep");
    }

    #[test]
    fn the_synchronized_frame_is_opened_and_closed() {
        // Left unclosed, the terminal buffers forever and the box never appears.
        assert_eq!(SYNC_BEGIN, "[?2026h");
        assert_eq!(SYNC_END, "[?2026l");
        assert_ne!(SYNC_BEGIN, SYNC_END);
    }

    #[test]
    fn the_rule_shows_what_this_session_is() {
        let bar = Session {
            model: "opus".into(),
            workspace: "incord-code".into(),
            branch: Some("main".into()),
            dirty: true,
        };
        let said = bar.segments();
        assert!(said.contains("opus"), "{said}");
        assert!(said.contains("incord-code"), "{said}");
        assert!(said.contains("main"), "{said}");
        assert!(
            said.contains('\u{2022}'),
            "uncommitted work is marked: {said}"
        );
    }

    #[test]
    fn a_clean_tree_is_not_marked_dirty() {
        // The control: an unconditional marker would say every tree had
        // uncommitted work, which is worse than saying nothing at all.
        let bar = Session {
            branch: Some("main".into()),
            dirty: false,
            ..Session::default()
        };
        assert!(!bar.segments().contains('\u{2022}'), "{}", bar.segments());
    }

    #[test]
    fn a_directory_outside_git_shows_no_branch() {
        // This very project is not a git tree. Inventing a branch for it would
        // be a fact the operator could act on and that is not true.
        let bar = Session {
            model: "opus".into(),
            workspace: "incord-code".into(),
            branch: None,
            dirty: false,
        };
        assert_eq!(bar.segments(), "opus \u{00b7} incord-code");
    }

    #[test]
    fn the_rule_keeps_its_width_with_segments_on_it() {
        for segments in [
            "",
            "opus \u{00b7} incord-code",
            "opus \u{00b7} a \u{00b7} \u{2387} main\u{2022}",
        ] {
            assert_eq!(
                top_rule_with(80, segments, " \u{2193} 4 more ")
                    .chars()
                    .count(),
                80,
                "segments {segments:?} changed the width"
            );
        }
    }

    #[test]
    fn segments_are_dropped_before_the_hint_on_a_narrow_terminal() {
        // The hint says the menu continues; losing it strands the reader, while
        // the segments are merely useful.
        let narrow = top_rule_with(
            28,
            "a-very-long-model \u{00b7} a-long-workspace",
            " \u{2193} 9 more ",
        );
        assert_eq!(narrow.chars().count(), 28);
        assert!(narrow.contains("9 more"), "{narrow}");
    }

    #[test]
    fn a_short_paste_stays_inline_and_a_long_one_becomes_a_marker() {
        let mut f = typed("");
        let short = "one\ntwo\nthree";
        assert_eq!(f.accept_paste(short), short, "a short paste is just text");
        assert!(f.pastes.is_empty());

        let long: String = (0..40).map(|n| format!("line {n}\n")).collect();
        let shown = f.accept_paste(&long);
        assert!(shown.starts_with("[paste #1 +40 lines]"), "{shown}");
        assert_eq!(f.pastes.len(), 1);
        // What the agent receives is the whole thing, not the marker.
        assert_eq!(f.expand_pastes(&shown), long);
    }

    #[test]
    fn a_deleted_paste_marker_takes_its_paste_with_it() {
        // The marker is rebuilt to match, so editing it off the line drops the
        // paste rather than silently smuggling forty lines to the agent.
        let mut f = typed("");
        let long: String = (0..40).map(|n| format!("line {n}\n")).collect();
        let _ = f.accept_paste(&long);
        assert_eq!(f.expand_pastes("just this"), "just this");
    }

    #[test]
    fn a_lone_slash_offers_every_command() {
        assert_eq!(matching("/").len(), COMMANDS.len());
    }

    #[test]
    fn a_prefix_narrows_the_menu() {
        assert_eq!(matching("/doc"), vec!["/doctor"]);
    }

    #[test]
    fn an_argument_is_not_a_command_word() {
        assert_eq!(command_word("/add-dir /tmp"), None);
        assert_eq!(command_word("hello"), None);
        assert_eq!(command_word("/doc"), Some("/doc"));
    }

    #[test]
    fn completion_stops_at_what_the_candidates_agree_on() {
        // Guessing past the shared prefix would put a command nobody chose on
        // the line, one keystroke from being run.
        assert_eq!(shared_prefix(&["/save", "/sessions"]), "/s");
        assert_eq!(shared_prefix(&["/doctor"]), "/doctor");
        assert_eq!(shared_prefix(&[]), "");
    }

    #[test]
    fn cutting_a_line_never_splits_a_character() {
        // An em dash is three bytes; slicing on byte counts panics. That exact
        // panic has already reached the operator once, as `Error: t`.
        let text = "a—b—c";
        for width in 0..=text.chars().count() + 2 {
            assert!(fit(text, width).chars().count() <= text.chars().count());
        }
        assert_eq!(fit("a—b", 2), "a—");
    }

    #[test]
    fn mouse_capture_defaults_to_on_and_enables_mouse_reporting() {
        // Default is on: mouse wheel scrolls the terminal conversation,
        // rather than hijacking the input box with Up/Down arrow history.
        assert!(mouse_capture_enabled_from(None));
        assert!(!mouse_capture_enabled_from(Some("0")));
        assert!(!mouse_capture_enabled_from(Some("false")));
        assert!(mouse_capture_enabled_from(Some("1")));
        assert!(mouse_capture_enabled_from(Some("true")));

        // When on, mouse capture and SGR reporting escape sequences are sent.
        assert_eq!(
            mouse_enable_sequence(true),
            "\x1b[?1000h\x1b[?1002h\x1b[?1006h"
        );
        assert_eq!(
            mouse_disable_sequence(true),
            "\x1b[?1006l\x1b[?1002l\x1b[?1000l"
        );

        // When explicitly disabled (INCODE_MOUSE=0), alternate scroll mode (1007l) is sent.
        assert_eq!(mouse_enable_sequence(false), "\x1b[?1007l");
        assert_eq!(mouse_disable_sequence(false), "");
    }

    #[test]
    fn shift_up_and_down_scrolls_conversation_without_recalling_history() {
        let mut footer = Footer {
            width: 80,
            height: 24,
            line: "my typed text".chars().collect(),
            caret: 13,
            history: vec!["/quit".to_string()],
            browsing: None,
            history_path: None,
            menu: Vec::new(),
            selected: 0,
            menu_top: 0,
            menu_hidden: false,
            session: Session::default(),
            pastes: Vec::new(),
            editing: crate::cli::keys::EditingMode::Emacs,
            vi_insert: true,
            vi_pending: None,
            keymap: crate::cli::keys::Keymap::default(),
            status_line: None,
        };

        // Shift+Up must not recall history into footer.line
        let _ = footer.on_key(KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(
            footer.line.iter().collect::<String>(),
            "my typed text",
            "Shift+Up must leave line untouched"
        );
        assert_eq!(footer.browsing, None);

        // Plain Up without Shift recalls history into footer.line
        let _ = footer.on_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(
            footer.line.iter().collect::<String>(),
            "/quit",
            "Plain Up recalls previous command from history"
        );
        assert_eq!(footer.browsing, Some(0));
    }

    #[test]
    fn the_active_model_is_rendered_in_the_top_rule() {
        let session = Session {
            model: "claude-3-7-sonnet".to_string(),
            workspace: "incord-code".to_string(),
            branch: Some("main".to_string()),
            dirty: false,
        };
        let segments = session.segments();
        assert!(segments.contains("claude-3-7-sonnet"));
        assert!(segments.contains("incord-code"));
        assert!(segments.contains("main"));

        let rule = top_rule_with(80, &segments, "");
        assert!(
            rule.contains("claude-3-7-sonnet"),
            "top rule must display the model name"
        );
    }
}
