//! Terminal rendering — output formatting + spinner for tool execution.

use crate::{out, outln};
use std::io::{self, IsTerminal, Write};

use crossterm::cursor::{Hide, MoveToColumn, Show};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType};

// ---------------------------------------------------------------------------
// Spinner
// ---------------------------------------------------------------------------

/// A status line that rewrites itself while a turn runs.
///
/// # Why there is no background thread
///
/// There was one, and every terminal bug in this file came from it: two writers
/// on one line. The reply came out as `Thinking · 2m 36st by getting oriented`,
/// an error as `Error: t`, the input line collected stray spaces so what was
/// typed and what was shown disagreed, and finally a reply was erased outright
/// by a wipe that ran after the text had been written.
///
/// Each was fixed on its own and the next one appeared, because the design was
/// wrong rather than the details. A spinner that animates on its own schedule
/// cannot share a line with output it does not know about.
///
/// So this draws only when the CALLER asks, on the caller's thread, between
/// its own writes. The frame advances per update rather than per tick: the
/// spinner moves when something happens, which is the honest thing for it to
/// show anyway. No lock, no thread, and no window in which two writers meet.
pub struct LiveStatus {
    inner: std::sync::Arc<std::sync::Mutex<Screen>>,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// The terminal, and everything anyone is allowed to do to it.
///
/// # One writer
///
/// Every write goes through this, under one lock: the status line AND the
/// reply. That is the whole design. Two writers on one line produced, in turn,
/// a reply printed over the status, an error truncated to `Error: t`, stray
/// spaces on the input line, and finally a reply erased outright.
///
/// Drawing only from the caller's thread fixed all of that and broke something
/// else: with no ticker the icon froze during the long silences, which is
/// exactly when a person needs to see the session is alive. So the ticker is
/// back — and it takes this same lock, so it can never interleave with a reply.
#[derive(Default)]
pub struct Screen {
    /// Whether this terminal gets a status line at all.
    drawing: bool,
    /// What the status says. Empty means nothing to draw.
    text: String,
    /// Which spinner frame comes next.
    frame: usize,
    /// How wide the drawn status is; zero when none is drawn.
    width: usize,
    /// Whether the last thing written was reply text, so the status knows to
    /// start on a fresh line rather than on the end of a sentence.
    after_text: bool,
}

impl Screen {
    /// Erase the status line, if one is drawn.
    ///
    /// Row-aware for the same reason [`Screen::draw`] is: with the input box on
    /// screen the status has a row of its own, and clearing at the CURSOR wipes
    /// whichever row the cursor was left on instead — a line of the
    /// conversation, or the box. This runs once per streamed fragment, so
    /// getting it wrong scribbles over the screen for the length of a reply.
    fn erase(&mut self) {
        if self.width == 0 {
            return;
        }
        if crate::cli::console::mode() == crate::cli::console::Mode::Buffered {
            if let Ok((_, height)) = crossterm::terminal::size() {
                // Painting the row empty IS clearing it, and it is the one call
                // that knows where the row is.
                crate::cli::paint::status(height, "");
                self.width = 0;
                return;
            }
        }
        let _ = execute!(
            io::stdout(),
            MoveToColumn(0),
            Clear(ClearType::UntilNewLine)
        );
        let _ = io::stdout().flush();
        self.width = 0;
    }

    /// Draw the status.
    ///
    /// # Two screens, and only one of them has a cursor to draw at
    ///
    /// Without the input box this writes where the cursor is, which is right:
    /// output scrolls, the status follows it, and the terminal keeps the
    /// history.
    ///
    /// With the box, the screen is PAINTED — every row belongs to someone and
    /// nothing scrolls. Writing at the cursor there put the status on whatever
    /// row the cursor had been left on, and the `println!` below pushed the
    /// whole alternate screen up by a line, carrying the pinned box with it. The
    /// box was not moving because the box was wrong; it was moving because this
    /// function scrolled the screen out from under it, several times a second
    /// for as long as a turn ran.
    ///
    /// `paint::status` owns a row for exactly this, and had no caller.
    fn draw(&mut self) {
        if !self.drawing || self.text.is_empty() {
            return;
        }
        let frames = spinner_frames();
        let frame = frames[self.frame % frames.len()];
        self.frame = self.frame.wrapping_add(1);

        let text = self.text.clone();
        let rendered = text.replace(FRAME_SLOT, &frame.to_string());
        let colour = colour_for(&text);

        if crate::cli::console::mode() == crate::cli::console::Mode::Buffered {
            if let Ok((_, height)) = crossterm::terminal::size() {
                // The colour rides IN the string: the row is written in one go,
                // so an escape sent separately would land on whatever the cursor
                // was pointing at instead.
                let painted = match text.split_once(FRAME_SLOT) {
                    Some((before, after)) => {
                        let (verb, tail) = split_verb(after);
                        format!(
                            "{before}{}{frame}{verb}{}{tail}",
                            SetForegroundColor(colour),
                            ResetColor
                        )
                    }
                    None => rendered.clone(),
                };
                // WHICH BRANCH, and where. Three writers ended up on this one row
                // and the fastest won; nothing said so, and it read as the status
                // simply not working.
                crate::trace!(
                    "ui",
                    "status -> row {} of {height}: {:?}",
                    crate::cli::paint::status_row(height),
                    crate::text::truncate(&rendered, 40)
                );
                crate::cli::paint::status(height, &painted);
                // Nothing was written where a reply ends, so there is no
                // half-finished line for the next draw to step around.
                self.after_text = false;
                self.width = rendered.chars().count();
                return;
            }
        }

        if self.after_text {
            // The reply ended mid-line; the status gets its own.
            println!();
            self.after_text = false;
        }
        let _ = execute!(io::stdout(), Hide);
        match text.split_once(FRAME_SLOT) {
            Some((before, after)) => {
                let (verb, tail) = split_verb(after);
                let _ = execute!(io::stdout(), MoveToColumn(0));
                print!("{before}");
                let _ = execute!(io::stdout(), SetForegroundColor(colour));
                print!("{frame}{verb}");
                let _ = execute!(io::stdout(), ResetColor);
                print!("{tail}");
            }
            None => {
                let _ = execute!(io::stdout(), MoveToColumn(0));
                print!("{rendered}");
            }
        }
        let _ = execute!(io::stdout(), Clear(ClearType::UntilNewLine), Hide);
        let _ = io::stdout().flush();
        self.width = rendered.chars().count();
    }
}

/// Where the spinner frame is substituted into a status line.
pub const FRAME_SLOT: char = '\u{1}';

/// Rotating dots, which is what the icon this imitates actually does.
///
/// Braille cells are the only glyphs in a terminal that genuinely ROTATE: each
/// frame lights a different pair of dots around the cell, so the motion reads as
/// a turning ring rather than a cycling symbol. Windows Terminal, the VS Code
/// terminal and mintty all render them.
const FRAMES_ROTATING: [char; 10] = [
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];

/// For a terminal that cannot show them.
///
/// A legacy console draws an unsupported glyph as a box, and a status line whose
/// first character is visibly broken reads as a bug in the tool. Setting
/// `INCORD_ASCII=1` switches to characters that have worked everywhere for
/// decades.
const FRAMES_PLAIN: [char; 4] = ['|', '/', '-', '\\'];

/// The variable that forces the plain frames.
pub const ASCII_ENV: &str = "INCORD_ASCII";

/// Which frames this terminal should use.
#[must_use]
pub fn spinner_frames() -> &'static [char] {
    // Read per call rather than cached: a session is long, and an operator who
    // sets this after seeing boxes should not have to restart to be believed.
    if let Ok(value) = std::env::var(ASCII_ENV) {
        if value != "0" && !value.is_empty() {
            return &FRAMES_PLAIN;
        }
        if value == "0" {
            return &FRAMES_ROTATING;
        }
    }
    // On Windows, legacy console (cmd.exe / powershell.exe without Windows Terminal or VS Code)
    // lacks font support for Braille unicode characters, causing them to render as missing glyph boxes/blobs.
    #[cfg(windows)]
    {
        let is_modern = std::env::var("WT_SESSION").is_ok()
            || std::env::var("TERM_PROGRAM").is_ok()
            || std::env::var("ConEmuPID").is_ok();
        if !is_modern {
            return &FRAMES_PLAIN;
        }
    }
    &FRAMES_ROTATING
}

/// The colour for what the agent is doing.
///
/// # Why colour carries meaning here
///
/// A status line that is always one colour is decoration. One that changes tells
/// you without reading a word whether the agent is looking at your code or
/// changing it — and whether it is about to touch something outside this
/// machine.
///
/// **Red is reserved for what leaves this machine or cannot be undone**:
/// deploying, uploading, migrating. Red already means "error" everywhere else in
/// this CLI, so spending it on an ordinary state would make every normal turn
/// look like a failure — which is why thinking is white and not red.
#[must_use]
pub fn colour_for(line: &str) -> Color {
    // Matched on the verb the line already carries, so the colours and the words
    // cannot drift into two lists that disagree.
    if contains_word(line, &["Deploying", "Uploading", "Migrating"]) {
        return Color::Red;
    }
    if contains_word(line, &["Editing", "Committing", "Installing"]) {
        return Color::Yellow;
    }
    if contains_word(line, &["Running tests", "Checking style", "Building"]) {
        return Color::Green;
    }
    if contains_word(line, &["Reading", "Searching", "Researching", "Fetching"]) {
        return Color::Cyan;
    }
    if contains_word(line, &["Delegating"]) {
        return Color::Magenta;
    }
    // Thinking, and anything unrecognised: the agent is talking, not touching.
    Color::White
}

fn contains_word(line: &str, words: &[&str]) -> bool {
    words.iter().any(|w| line.contains(w))
}

/// The verb after the icon, and everything after it.
///
/// Split so the icon and its verb take the colour together while the timing and
/// the token count stay plain — a whole coloured line would compete with the
/// reply for attention.
#[must_use]
pub fn split_verb(after: &str) -> (&str, &str) {
    const SEPARATOR: &str = " \u{b7} ";
    match after.find(SEPARATOR) {
        Some(at) => after.split_at(at),
        None => (after, ""),
    }
}

impl LiveStatus {
    /// Begin, showing nothing until the first update.
    #[must_use]
    pub fn start(animate: bool) -> Self {
        Self::start_with_cancel(animate, None)
    }

    /// Begin, listening for Esc/Ctrl-C to cancel if cancel_token is provided.
    #[must_use]
    pub fn start_with_cancel(
        animate: bool,
        cancel_token: Option<crate::runtime::cancel::CancelToken>,
    ) -> Self {
        let inner = std::sync::Arc::new(std::sync::Mutex::new(Screen {
            drawing: animate,
            ..Screen::default()
        }));
        let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(animate));
        if !animate {
            return Self {
                inner,
                running,
                handle: None,
            };
        }

        // The caret belongs to the input box and nowhere else. Left visible, it
        // parks itself at the end of whatever the status line last wrote —
        // "⠿ Thinking▌" — which reads as the agent having a cursor of its own.
        // It is shown again by `stop`, and by the box each time it draws.
        let _ = execute!(io::stdout(), Hide);
        let _ = crossterm::terminal::enable_raw_mode();
        let mouse_capture = crate::cli::footer::mouse_capture_enabled_from(
            std::env::var("INCORD_MOUSE").as_deref().ok(),
        );
        if std::io::stdout().is_terminal() && mouse_capture {
            let _ = execute!(io::stdout(), EnableMouseCapture);
            let _ = write!(io::stdout(), "\x1b[?1000h\x1b[?1002h\x1b[?1006h");
            let _ = io::stdout().flush();
        }

        let shared = std::sync::Arc::clone(&inner);
        let alive = std::sync::Arc::clone(&running);
        let handle = std::thread::spawn(move || {
            while alive.load(std::sync::atomic::Ordering::Relaxed) {
                if crossterm::event::poll(std::time::Duration::from_millis(0)).unwrap_or(false) {
                    match crossterm::event::read() {
                        Ok(crossterm::event::Event::Key(key)) => {
                            if key.code == crossterm::event::KeyCode::Esc
                                || (key
                                    .modifiers
                                    .contains(crossterm::event::KeyModifiers::CONTROL)
                                    && key.code == crossterm::event::KeyCode::Char('c'))
                            {
                                if let Some(ref token) = cancel_token {
                                    token.cancel();
                                }
                            }
                        }
                        Ok(crossterm::event::Event::Mouse(mouse)) => {
                            if let Ok((w, h)) = crossterm::terminal::size() {
                                match mouse.kind {
                                    crossterm::event::MouseEventKind::ScrollUp => {
                                        crate::cli::paint::scroll_up(
                                            crate::cli::footer::SCROLL_STEP,
                                            w,
                                            h,
                                        );
                                        crate::cli::paint::conversation(w, h);
                                        crate::cli::paint::repaint_box();
                                    }
                                    crossterm::event::MouseEventKind::ScrollDown => {
                                        crate::cli::paint::scroll_down(
                                            crate::cli::footer::SCROLL_STEP,
                                        );
                                        crate::cli::paint::conversation(w, h);
                                        crate::cli::paint::repaint_box();
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                {
                    // The SAME lock every other writer takes, so a redraw can
                    // never land in the middle of a reply.
                    if let Ok(mut screen) = shared.lock() {
                        if !screen.text.is_empty() {
                            screen.draw();
                        }
                    }
                    // While a turn runs the input box is not reading, so this
                    // ticker is the only thing left that can put newly
                    // recorded output on screen.
                    if crate::cli::console::mode() == crate::cli::console::Mode::Buffered {
                        if let Ok((w, h)) = crossterm::terminal::size() {
                            crate::cli::paint::conversation(w, h);
                            crate::cli::paint::repaint_box();
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
        });

        Self {
            inner,
            running,
            handle: Some(handle),
        }
    }

    /// Say what the status line should show from now on.
    pub fn set(&self, text: &str) {
        if let Ok(mut screen) = self.inner.lock() {
            screen.text = text.to_string();
            screen.draw();
        }
    }

    /// A writer for code that must print WHILE this is drawing.
    ///
    /// Handed out rather than lending `&self`, because the thing that needs it
    /// — the ledger run's worker, showing file changes as they land — is
    /// constructed before the status line and outlives any borrow of it. The
    /// handle carries the SAME lock, so the one-writer rule this type is built
    /// on still holds: see the note on [`Screen`].
    #[must_use]
    pub fn writer(&self) -> Writer {
        Writer(std::sync::Arc::clone(&self.inner))
    }

    /// Write reply text, taking the terminal from the status line first.
    ///
    /// This is why the ticker is safe: the reply goes through the same lock, so
    /// the two can never be writing at once.
    pub fn write_text(&self, text: &str) {
        self.writer().write_text(text);
    }
}

/// Prints through the status line's own lock, from anywhere.
///
/// Cloneable and `'static`: it holds the screen, not the [`LiveStatus`], so
/// stopping the status line does not invalidate it and a writer can be given to
/// a component built long before the bar it will print above.
#[derive(Clone)]
pub struct Writer(std::sync::Arc<std::sync::Mutex<Screen>>);

impl Writer {
    /// Write text, taking the terminal from the status line first.
    pub fn write_text(&self, text: &str) {
        if let Ok(mut screen) = self.0.lock() {
            // ONLY WHEN THEY SHARE A LINE.
            //
            // Erasing here exists because, without the box, the status sits on
            // the cursor line and the reply is about to be written onto it — so
            // the status has to get out of the way first.
            //
            // With the box the status has a ROW OF ITS OWN and the reply goes
            // into the conversation, so they share nothing. Erasing anyway
            // clears that row once per streamed FRAGMENT while the ticker
            // repaints it every 120ms — fragments win, and the status reads as
            // permanently blank with an occasional flash. Which is exactly what
            // it did.
            if crate::cli::console::mode() != crate::cli::console::Mode::Buffered {
                screen.erase();
            }
            // Recorded, not printed straight out: this is the reply, and the
            // screen's owner needs it in the transcript like everything else.
            crate::cli::console::emit(text);
            let _ = io::stdout().flush();
            screen.after_text = true;
        }
    }
}

impl LiveStatus {
    /// Erase the status line and leave it off until the next update.
    pub fn suspend(&self) {
        if let Ok(mut screen) = self.inner.lock() {
            screen.text.clear();
            screen.erase();
        }
    }

    /// Whether this will draw at all.
    #[must_use]
    pub fn is_drawing(&self) -> bool {
        self.inner.lock().is_ok_and(|s| s.drawing)
    }

    /// How wide the drawn status is, or zero when none is drawn.
    #[must_use]
    pub fn width(&self) -> usize {
        self.inner.lock().map_or(0, |s| s.width)
    }

    /// Stop, leaving the terminal clean.
    pub fn stop(mut self) {
        self.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.suspend();
        let mouse_capture = crate::cli::footer::mouse_capture_enabled_from(
            std::env::var("INCORD_MOUSE").as_deref().ok(),
        );
        if std::io::stdout().is_terminal() && mouse_capture {
            let _ = execute!(io::stdout(), DisableMouseCapture);
            let _ = write!(io::stdout(), "\x1b[?1006l\x1b[?1002l\x1b[?1000l");
            let _ = io::stdout().flush();
        }
        let _ = crossterm::terminal::disable_raw_mode();
        // Handed back, so a terminal that never gets an input box drawn over it
        // is not left without a caret.
        let _ = execute!(io::stdout(), Show);
    }
}

impl Drop for LiveStatus {
    fn drop(&mut self) {
        self.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let mouse_capture = crate::cli::footer::mouse_capture_enabled_from(
            std::env::var("INCORD_MOUSE").as_deref().ok(),
        );
        if std::io::stdout().is_terminal() && mouse_capture {
            let _ = execute!(io::stdout(), DisableMouseCapture);
            let _ = write!(io::stdout(), "\x1b[?1006l\x1b[?1002l\x1b[?1000l");
            let _ = io::stdout().flush();
        }
        let _ = crossterm::terminal::disable_raw_mode();
        // Including when the turn ends by unwinding rather than by `stop`.
        let _ = execute!(io::stdout(), Show);
    }
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

/// Print a banner at REPL startup.
pub fn print_banner(
    model: &str,
    workspace: &str,
    permission: &str,
    git: Option<&crate::tools::git_context::GitContext>,
) {
    // The facts sit beside the mark rather than under a rule of equals signs.
    // Same information, one screenful less of it.
    let mut info = vec![
        String::new(),
        "Incord Code".to_string(),
        "  an agent harness".to_string(),
        String::new(),
        format!("  model       {model}"),
        format!("  workspace   {workspace}"),
        format!("  permission  {permission}"),
    ];
    if let Some(g) = git {
        info.push(format!("  git         {}", g.summary()));
    }
    info.push(String::new());
    info.push("  / for commands".to_string());

    outln!();
    crate::cli::logo::play(&info);
    outln!();
}

/// The widest a result panel is drawn, so it stays readable when maximised.
const PANEL_WIDTH: usize = 100;

/// Print an assistant text response (with optional markdown rendering).
pub fn print_assistant(text: &str) {
    outln!();
    print_colored("Incord: ", Color::DarkCyan);
    if crate::cli::markdown::looks_like_markdown(text) {
        outln!();
        crate::cli::markdown::render_markdown(text);
    } else {
        outln!("{text}");
    }
    outln!();
}

/// Print a tool execution summary.
pub fn print_tool_call(tool_name: &str, _input_preview: &str) {
    print_colored(&format!("  [{tool_name}] "), Color::DarkYellow);
    outln!();
}

/// Print a tool result (truncated if long).
pub fn print_tool_result(tool_name: &str, output: &str, is_error: bool) {
    // A rule carrying the tool's name and its outcome, then the body indented
    // beneath it. The old form buried both in the middle of a truncated line.
    let width = crate::cli::activity::terminal_width().min(PANEL_WIDTH);
    let colour = if is_error {
        Color::Red
    } else {
        Color::DarkGrey
    };
    for (n, line) in crate::cli::panel::result(tool_name, None, output, is_error, width)
        .iter()
        .enumerate()
    {
        if n == 0 {
            print_colored(line, colour);
            outln!();
        } else {
            print_dim(line);
            outln!();
        }
    }
}

/// Print an error message.
pub fn print_error(msg: &str) {
    print_colored("Error: ", Color::Red);
    outln!("{msg}");
}

/// Print an info message.
pub fn print_info(msg: &str) {
    print_colored("  ", Color::DarkCyan);
    outln!("{msg}");
}

/// Print a section header.
pub fn print_header(title: &str) {
    outln!();
    print_colored(&format!("--- {title} ---"), Color::DarkCyan);
    outln!();
}

/// Print a unified diff with color (red for deletions, green for additions).
pub fn print_diff(diff_text: &str) {
    for line in diff_text.lines() {
        if line.starts_with('+') {
            print_colored(line, Color::Green);
            outln!();
        } else if line.starts_with('-') {
            print_colored(line, Color::Red);
            outln!();
        } else {
            outln!("{line}");
        }
    }
}

/// Dim text helper.
pub fn print_dim(text: &str) {
    crate::cli::console::style(SetAttribute(Attribute::Dim));
    out!("{text}");
    crate::cli::console::style(SetAttribute(Attribute::Reset));
}

/// Print a streaming text delta (no newline, flushes immediately).
pub fn print_streaming_delta(text: &str) {
    out!("{text}");
    let _ = io::stdout().flush();
}

/// Finish a streaming response (newline + spacing).
pub fn finish_streaming() {
    outln!();
    outln!();
}

fn print_colored(text: &str, color: Color) {
    crate::cli::console::style(SetForegroundColor(color));
    out!("{text}");
    crate::cli::console::style(ResetColor);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// **Colour carries meaning, so the dangerous states are unmistakable.**
    /// Red is spent only on what leaves this machine or cannot be undone.
    #[test]
    fn what_leaves_this_machine_is_red() {
        for verb in ["Deploying", "Uploading", "Migrating"] {
            assert_eq!(colour_for(&format!("x {verb}")), Color::Red, "{verb}");
        }
    }

    /// **Thinking is NOT red**, because red already means "error" in this CLI
    /// and every ordinary turn would look like a failure.
    #[test]
    fn an_ordinary_turn_does_not_look_like_a_failure() {
        assert_ne!(colour_for("x Thinking"), Color::Red);
        assert_eq!(colour_for("x Thinking"), Color::White);
    }

    /// Reading and changing are told apart at a glance, without reading a word.
    #[test]
    fn looking_and_touching_are_different_colours() {
        assert_eq!(colour_for("x Reading"), Color::Cyan);
        assert_eq!(colour_for("x Editing"), Color::Yellow);
        assert_eq!(colour_for("x Running tests"), Color::Green);
        assert_eq!(colour_for("x Delegating to another agent"), Color::Magenta);
    }

    /// **The frames rotate by default and fall back on request.** A console
    /// that cannot draw braille shows a box, and a status line whose first
    /// character is broken reads as a bug in the tool.
    #[test]
    fn the_rotating_frames_have_a_plain_fallback() {
        assert_eq!(FRAMES_ROTATING.len(), 10);
        assert_eq!(FRAMES_PLAIN.len(), 4);
        assert!(FRAMES_PLAIN.iter().all(char::is_ascii));
        assert!(FRAMES_ROTATING.iter().all(|c| !c.is_ascii()));
    }

    /// Only the icon and its verb are coloured; the timing and the count stay
    /// plain so the line does not compete with the reply.
    #[test]
    fn only_the_verb_takes_the_colour() {
        let line = "Reading · 4s · 1.2k tokens";
        let (verb, tail) = split_verb(line);
        assert_eq!(verb, "Reading");
        assert!(tail.contains("4s"), "got: {tail}");
        assert!(tail.contains("tokens"), "got: {tail}");
    }

    /// A line with nothing after the verb still splits cleanly.
    #[test]
    fn a_verb_with_no_tail_is_handled() {
        let (verb, tail) = split_verb("Thinking");
        assert_eq!(verb, "Thinking");
        assert!(tail.is_empty());
    }

    /// **A status line that is not on a terminal draws nothing.** Piped, the
    /// frames are not redrawn in place — they pile up and bury the reply.
    #[test]
    fn a_status_line_off_a_terminal_draws_nothing() {
        let status = LiveStatus::start(false);
        status.set("anything");
        assert_eq!(status.width(), 0, "nothing may be drawn off a terminal");
        assert!(!status.is_drawing());
    }

    /// **suspend() leaves nothing drawn, so the next writer owns the line.**
    /// This is the invariant every terminal bug in this file violated: a reply
    /// printed while a status line was still on screen, or a wipe that ran
    /// after the reply had been written and erased it.
    #[test]
    fn suspending_leaves_no_drawn_line_behind() {
        let status = LiveStatus::start(true);
        status.set("\u{1} Thinking");
        assert!(status.width() > 0, "it drew something");
        status.suspend();
        assert_eq!(status.width(), 0, "and now it has not");
    }

    /// **Suspending when nothing is drawn erases nothing.** The reply-erasing
    /// bug was exactly a wipe that ran when nothing was on the line: width is
    /// the guard, and it must be honest.
    #[test]
    fn suspending_when_nothing_is_drawn_erases_nothing() {
        let status = LiveStatus::start(true);
        status.suspend();
        assert_eq!(status.width(), 0);
        status.suspend();
        assert_eq!(status.width(), 0, "still nothing, and no line was touched");
    }

    /// An empty update draws nothing rather than an empty line.
    #[test]
    fn an_empty_status_draws_nothing() {
        let status = LiveStatus::start(true);
        status.set("");
        assert_eq!(status.width(), 0);
    }

    /// **Reply text takes the terminal from the status line.** This is the
    /// invariant every terminal bug in this file violated: the two must never
    /// be writing at once, and after text there is no status left drawn.
    /// The console's mode is global, so the two tests that depend on it must not
    /// interleave. Without this they pass alone and fail in the suite, which is
    /// worse than either failing honestly.
    ///
    /// # It is the console's lock, not a third one
    ///
    /// This was its own mutex, and `cli::paint`'s tests had another, and
    /// `cli::console`'s had a third — all guarding the same global console.
    /// Three locks over one piece of state exclude nobody: these tests write
    /// ANSI resets through the console, `paint`'s tests read the buffer, and
    /// `wrapped_clamps_width_to_minus_one_to_prevent_terminal_autowrap`
    /// intermittently found `["123456789", "0", "\u{1b}[0m\u{1b}[0m"]` — this
    /// module's escape codes, landing mid-measurement.
    fn console_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::cli::console::test_lock()
    }

    /// Stop the ticker without disturbing what it drew, so an assertion measures
    /// the last deliberate write rather than whichever thread ran last.
    fn silence(status: &mut LiveStatus) {
        status
            .running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(ticker) = status.handle.take() {
            ticker.join().expect("the ticker stops");
        }
    }

    #[test]
    fn a_reply_does_not_wipe_the_status_when_the_box_owns_the_screen() {
        // The status had a row of its own and the reply went to the conversation,
        // so they shared nothing — yet every streamed FRAGMENT cleared that row
        // while the ticker repainted it only every 120ms. Fragments won, and the
        // status read as permanently blank with an occasional flash.
        let _guard = console_lock();
        crate::cli::console::set_mode(crate::cli::console::Mode::Buffered);

        let mut status = LiveStatus::start(true);
        status.set("\u{1} Thinking");
        silence(&mut status);
        assert!(status.width() > 0, "the status drew");

        status.write_text("a fragment of a reply");
        let kept = status.width();

        crate::cli::console::set_mode(crate::cli::console::Mode::Direct);
        status.stop();
        assert!(kept > 0, "the reply wiped a row it does not share");
    }

    #[test]
    fn writing_text_takes_the_line_from_the_status() {
        // THE TICKER IS STOPPED FIRST, and that is the whole point of the extra
        // four lines. This test used to assert straight after `write_text`, and
        // it failed intermittently — not because the code was wrong but because
        // the ticker thread is entitled to redraw: `draw` only returns early on
        // empty text, and the text is still set. So `width == 0` was true only
        // BETWEEN ticks, and the test was racing a thread rather than checking
        // an invariant.
        //
        // Joining the ticker leaves the screen exactly as it was drawn, with
        // nothing left to repaint behind the assertion, so what is measured is
        // what `write_text` did.
        // The mode is taken and SET, not assumed. This test is about the path
        // with no box, and its sibling sets the global mode to the other one —
        // it failed here first, reading 10 where it wanted 0, because the two
        // were racing over a value neither had claimed.
        let _guard = console_lock();
        crate::cli::console::set_mode(crate::cli::console::Mode::Direct);

        let mut status = LiveStatus::start(true);
        status.set("\u{1} Thinking");
        assert!(status.width() > 0, "the status drew");

        silence(&mut status);
        assert!(status.width() > 0, "control: stopping it changed nothing");

        status.write_text("part of a reply");
        assert_eq!(status.width(), 0, "the status gave up the line");
        status.stop();
    }
}
