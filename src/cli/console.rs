//! The one place output goes.
//!
//! # Why everything funnels through here
//!
//! The input box is pinned to the bottom rows by a scroll region, which keeps
//! it still while output arrives — but it lives in the terminal's MAIN buffer,
//! so scrolling the wheel moves the viewport back over history and the box
//! goes with it. To hold the box still through scrolling, the program has to
//! own the screen and draw the conversation itself.
//!
//! It can only draw what it has. A hundred and five `println!` calls scattered
//! across the codebase wrote straight past any buffer we might keep, so the
//! first move is to give them one door to go through. That is this module.
//!
//! # Two modes, one door
//!
//! [`Mode::Direct`] prints as before AND records — behaviour is unchanged, and
//! the record is what the next step needs. [`Mode::Buffered`] records only, and
//! whoever owns the screen decides what to paint. Nothing else in the codebase
//! has to know which is in force.

use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// How many finished lines are kept.
///
/// Bounded on purpose: a long session prints tens of thousands of lines, and an
/// unbounded transcript in memory is a leak with a slow fuse. Old lines fall off
/// the top, which is what a terminal's own scrollback does.
const MAX_LINES: usize = 8_000;

/// Where output actually goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Straight to the terminal, and recorded. The behaviour that shipped.
    Direct,
    /// Recorded only; the screen's owner paints from the record.
    Buffered,
}

/// The conversation so far, and where it is being written.
struct Console {
    /// Finished lines, oldest first.
    lines: Vec<String>,
    /// A line still being built, because `print!` was used without a newline.
    ///
    /// Streaming replies arrive a fragment at a time, so most of a reply lives
    /// here before it ever becomes a line.
    pending: String,
    mode: Mode,
    /// Lines dropped off the top, so a viewer can tell it is not at the start.
    dropped: usize,
    /// Bumped on every write, so a painter can tell whether anything changed.
    ///
    /// A counter rather than a callback: calling out while this lock is held
    /// would deadlock the moment the painter read the lines back.
    version: u64,
}

impl Console {
    const fn new() -> Self {
        Self {
            lines: Vec::new(),
            pending: String::new(),
            mode: Mode::Direct,
            dropped: 0,
            version: 0,
        }
    }

    /// Record `text`, splitting it into lines on the way.
    fn record(&mut self, text: &str) {
        self.version = self.version.wrapping_add(1);
        for ch in text.chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut self.pending);
                self.lines.push(line);
            } else if ch != '\r' {
                // A carriage return is a redraw instruction, not content; the
                // spinner emits one per frame and they are not conversation.
                self.pending.push(ch);
            }
        }
        if self.lines.len() > MAX_LINES {
            let excess = self.lines.len() - MAX_LINES;
            self.lines.drain(..excess);
            self.dropped += excess;
        }
    }
}

/// The single console, made on first use.
fn console() -> &'static Mutex<Console> {
    static CONSOLE: OnceLock<Mutex<Console>> = OnceLock::new();
    CONSOLE.get_or_init(|| Mutex::new(Console::new()))
}

thread_local! {
    /// What this thread has written since [`capture`] began, when one is
    /// running here. `None` when output goes to the console as usual.
    static CAPTURED: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// What receives this thread's writes as they are made, while [`stream`] runs.
type Sink = Box<dyn FnMut(&str)>;

thread_local! {
    /// The receiver [`stream`] installed on this thread, if one is running.
    static STREAMED: std::cell::RefCell<Option<Sink>> = const { std::cell::RefCell::new(None) };
}

/// Hand `text` to the capture running on this thread, if there is one.
/// `true` when it was taken.
fn captured(text: &str) -> bool {
    let streamed = STREAMED
        .try_with(|slot| match slot.try_borrow_mut() {
            Ok(mut held) => match held.as_mut() {
                Some(sink) => {
                    sink(text);
                    true
                }
                None => false,
            },
            // The sink itself wrote through this module: its words are not
            // handed back to it, and not shown either.
            Err(_) => true,
        })
        .unwrap_or(false);
    if streamed {
        return true;
    }
    CAPTURED
        .try_with(|slot| match slot.borrow_mut().as_mut() {
            Some(held) => {
                held.push_str(text);
                true
            }
            None => false,
        })
        .unwrap_or(false)
}

/// Run `f`, and return what it wrote through this module on THIS thread
/// instead of showing it.
///
/// # Why
///
/// The slash-command handlers write for a terminal, through this module.
/// `incode --acp` answers the same commands as message text, so it runs the
/// SAME handler inside a capture rather than keeping a second copy of each
/// one that returns a string.
///
/// Per thread, because each ACP session runs on a thread of its own and
/// another session may be writing at the same moment. Styling is dropped:
/// colour is a terminal instruction, not part of what was said.
pub fn capture<T>(f: impl FnOnce() -> T) -> (T, String) {
    /// Puts back whatever was capturing before, even if `f` panics.
    struct Restore(Option<Option<String>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(previous) = self.0.take() {
                let _ = CAPTURED.try_with(|slot| *slot.borrow_mut() = previous);
            }
        }
    }

    let previous = CAPTURED
        .try_with(|slot| slot.borrow_mut().replace(String::new()))
        .unwrap_or(None);
    let restore = Restore(Some(previous));
    let value = f();
    let text = CAPTURED
        .try_with(|slot| slot.borrow_mut().take())
        .ok()
        .flatten()
        .unwrap_or_default();
    drop(restore);
    (value, text)
}

/// Run `f`, handing everything it writes through this module on THIS thread
/// to `sink` as it is written, instead of showing it.
///
/// [`capture`] for work that takes minutes: `incode --acp` runs the ledger
/// (`/run`) inside this, so what the run prints reaches the client while the
/// run goes rather than all at once when it ends. Per thread and without
/// styling, for the reasons [`capture`] gives. A write made while `sink` is
/// running is dropped rather than handed back to it.
pub fn stream<T>(sink: Sink, f: impl FnOnce() -> T) -> T {
    /// Puts back whatever was streaming before, even if `f` panics.
    struct Restore(Option<Option<Sink>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(previous) = self.0.take() {
                let _ = STREAMED.try_with(|slot| {
                    if let Ok(mut held) = slot.try_borrow_mut() {
                        *held = previous;
                    }
                });
            }
        }
    }

    let previous = STREAMED
        .try_with(|slot| {
            slot.try_borrow_mut()
                .ok()
                .and_then(|mut held| held.replace(sink))
        })
        .unwrap_or(None);
    let _restore = Restore(Some(previous));
    f()
}

/// Write text exactly as given, with no newline added.
///
/// Every `print!` in the codebase becomes this, so streaming replies are
/// recorded as they arrive rather than only once they end.
pub fn emit(text: &str) {
    if captured(text) {
        return;
    }
    let Ok(mut console) = console().lock() else {
        // A poisoned lock must not silence the program. Printing beats losing
        // the output, and the record is only an optimisation for the screen.
        print!("{text}");
        let _ = std::io::stdout().flush();
        return;
    };
    console.record(text);
    if console.mode == Mode::Direct {
        print!("{text}");
        let _ = std::io::stdout().flush();
    }
}

/// Record a styling command as part of the text.
///
/// # Why styling goes through here too
///
/// A colour written straight to the terminal while its text is buffered
/// arrives without the text, painting whatever happens to be on screen. The
/// escape has to travel WITH the words.
///
/// The command formats itself, so this emits exactly the bytes the terminal
/// library would have — no hand-written escape codes to drift out of step with
/// what the rest of the program draws.
pub fn style<T: std::fmt::Display>(command: T) {
    // Inside a capture the text is what is wanted; an escape code in it would
    // reach a client that is not a terminal as noise.
    let held = CAPTURED
        .try_with(|slot| slot.borrow().is_some())
        .unwrap_or(false)
        || STREAMED
            .try_with(|slot| slot.try_borrow().map(|held| held.is_some()).unwrap_or(true))
            .unwrap_or(false);
    if held {
        return;
    }
    emit(&command.to_string());
}

/// Write text followed by a newline.
pub fn emit_line(text: &str) {
    emit(text);
    emit("\n");
}

/// Choose where output goes from now on.
pub fn set_mode(mode: Mode) {
    if let Ok(mut console) = console().lock() {
        console.mode = mode;
    }
}

/// What is being written to right now.
#[must_use]
pub fn mode() -> Mode {
    console().lock().map_or(Mode::Direct, |c| c.mode)
}

/// Every finished line, oldest first.
#[must_use]
pub fn lines() -> Vec<String> {
    console()
        .lock()
        .map(|c| c.lines.clone())
        .unwrap_or_default()
}

/// How many finished lines are held.
#[must_use]
pub fn line_count() -> usize {
    console().lock().map_or(0, |c| c.lines.len())
}

/// How many writes have happened, for a painter deciding whether to repaint.
#[must_use]
pub fn version() -> u64 {
    console().lock().map_or(0, |c| c.version)
}

/// How many lines have fallen off the top.
#[must_use]
pub fn dropped() -> usize {
    console().lock().map_or(0, |c| c.dropped)
}

/// The line still being built, if any.
#[must_use]
pub fn pending() -> String {
    console()
        .lock()
        .map(|c| c.pending.clone())
        .unwrap_or_default()
}

/// Forget everything recorded. For tests, and for `/clear`.
pub fn reset() {
    if let Ok(mut console) = console().lock() {
        console.lines.clear();
        console.pending.clear();
        console.dropped = 0;
        console.version = console.version.wrapping_add(1);
    }
}

/// Print through the console instead of straight at the terminal.
///
/// A drop-in for `println!`, so routing a call site is a one-word change and
/// the code still reads the way it did.
#[macro_export]
macro_rules! outln {
    () => { $crate::cli::console::emit_line("") };
    ($($arg:tt)*) => { $crate::cli::console::emit_line(&format!($($arg)*)) };
}

/// A drop-in for `print!`, for output that must not end the line yet.
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => { $crate::cli::console::emit(&format!($($arg)*)) };
}

/// The one lock every test that touches the global console must hold.
///
/// # Two locks are not a lock
///
/// This used to be private to this module's tests, and `cli::paint`'s tests
/// had a second mutex of their own. Both guard the SAME global console, and
/// neither excluded the other — so a console test emitting a line could
/// interleave with a paint test reading the buffer, and
/// `wrapped_clamps_width_to_minus_one_to_prevent_terminal_autowrap`
/// intermittently saw three rows after writing one line.
///
/// It lives here, beside the state it protects, so a third module that
/// reaches for the console finds the lock rather than inventing another.
///
/// Callers that also hold `paint`'s screen lock must take THAT one first;
/// this is always the inner lock, which is what keeps the order acyclic.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: Mutex<()> = Mutex::new(());
    GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> std::sync::MutexGuard<'static, ()> {
        let guard = test_lock();
        reset();
        set_mode(Mode::Buffered);
        guard
    }

    /// **CAPTURED TEXT IS RETURNED, NOT SHOWN, AND ONLY ON ITS OWN THREAD.**
    ///
    /// What a slash command says over `incode --acp` is this: the command's
    /// own handler, run inside [`capture`]. It must come back whole, without
    /// its colours, and without reaching the console another thread is
    /// writing to at the same moment.
    #[test]
    fn capture_returns_what_this_thread_wrote_and_nothing_else() {
        let _g = fresh();
        emit("before\n");
        let (value, text) = capture(|| {
            style(crossterm::style::SetForegroundColor(
                crossterm::style::Color::Red,
            ));
            emit("Error: ");
            emit_line("it failed");
            // Another thread writing during the capture goes to the console.
            std::thread::spawn(|| emit_line("elsewhere"))
                .join()
                .expect("thread");
            7
        });
        assert_eq!(value, 7);
        assert_eq!(text, "Error: it failed\n", "text only, styling dropped");
        assert_eq!(lines(), vec!["before", "elsewhere"]);

        // Once it returns, writing reaches the console again.
        emit_line("after");
        assert_eq!(lines(), vec!["before", "elsewhere", "after"]);
    }

    /// A capture inside a capture keeps each one's text to itself, and a
    /// panic inside one does not leave the thread capturing for good.
    #[test]
    fn nested_and_panicking_captures_restore_the_outer_state() {
        let _g = fresh();
        let ((), outer) = capture(|| {
            emit("outer ");
            let ((), inner) = capture(|| emit("inner"));
            assert_eq!(inner, "inner");
            emit("again");
        });
        assert_eq!(outer, "outer again");

        let panicked = std::panic::catch_unwind(|| capture(|| panic!("boom")));
        assert!(panicked.is_err());
        emit_line("visible");
        assert_eq!(lines(), vec!["visible"], "the panic left capture on");
    }

    /// **A STREAM HANDS EACH WRITE OVER AS IT IS MADE**, on its own thread
    /// only, without styling, and the thread writes normally again after.
    #[test]
    fn stream_hands_over_each_write_as_it_is_made() {
        let _g = fresh();
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let into = std::rc::Rc::clone(&seen);
        let value = stream(
            Box::new(move |text: &str| into.borrow_mut().push(text.to_string())),
            || {
                style(crossterm::style::SetForegroundColor(
                    crossterm::style::Color::Red,
                ));
                emit("first ");
                emit_line("line");
                std::thread::spawn(|| emit_line("elsewhere"))
                    .join()
                    .expect("thread");
                3
            },
        );
        assert_eq!(value, 3);
        assert_eq!(*seen.borrow(), vec!["first ", "line", "\n"]);
        emit_line("after");
        assert_eq!(lines(), vec!["elsewhere", "after"]);
    }

    #[test]
    fn text_becomes_lines_only_at_a_newline() {
        let _g = fresh();
        emit("half");
        assert_eq!(line_count(), 0, "an unfinished line is not a line yet");
        assert_eq!(pending(), "half");
        emit(" a line\n");
        assert_eq!(lines(), vec!["half a line"]);
        assert_eq!(pending(), "", "the pending text was consumed");
    }

    #[test]
    fn a_streamed_reply_is_recorded_fragment_by_fragment() {
        // Replies arrive a few characters at a time. Recording only whole
        // writes would lose every reply that had not ended yet.
        let _g = fresh();
        for fragment in ["Hel", "lo t", "here\n"] {
            emit(fragment);
        }
        assert_eq!(lines(), vec!["Hello there"]);
    }

    #[test]
    fn several_newlines_in_one_write_become_several_lines() {
        let _g = fresh();
        emit("one\ntwo\nthree\n");
        assert_eq!(lines(), vec!["one", "two", "three"]);
    }

    #[test]
    fn a_blank_line_is_kept() {
        // Spacing is content: dropping empty lines would run the conversation
        // together into one block.
        let _g = fresh();
        emit_line("");
        emit_line("after");
        assert_eq!(lines(), vec!["", "after"]);
    }

    #[test]
    fn carriage_returns_are_not_content() {
        // The spinner emits one per frame to redraw in place. Recorded, they
        // would put a stray control character in the middle of the transcript.
        let _g = fresh();
        emit("\rThinking\r");
        emit_line("");
        assert_eq!(lines(), vec!["Thinking"]);
    }

    #[test]
    fn the_record_is_bounded_and_says_what_it_dropped() {
        // Against a Console of its own, not the global one. The rest of the
        // suite now writes through the console too, so counting lines on the
        // shared instance passed alone and failed in the full run — a flaky
        // test, which is worse than no test.
        let mut console = Console::new();
        for n in 0..(MAX_LINES + 250) {
            console.record(&format!(
                "line {n}
"
            ));
        }
        assert_eq!(
            console.lines.len(),
            MAX_LINES,
            "the record grew past its cap"
        );
        assert_eq!(console.dropped, 250, "it must say how much it lost");
        assert_eq!(
            console.lines.first().map(String::as_str),
            Some("line 250"),
            "the OLDEST lines are the ones dropped"
        );
    }

    #[test]
    fn every_write_bumps_the_version() {
        // The painter repaints when this changes. A version that stood still
        // would leave the screen frozen while the agent talked.
        let _g = fresh();
        let before = version();
        emit("a");
        let after_partial = version();
        assert_ne!(
            after_partial, before,
            "an unfinished line is still a change"
        );
        emit_line("b");
        assert_ne!(version(), after_partial);
    }

    #[test]
    fn buffered_mode_records_the_same_as_direct() {
        // The control for step one: switching the mode must not change what is
        // recorded, only where it is also written.
        let _g = fresh();
        set_mode(Mode::Buffered);
        emit_line("same");
        let buffered = lines();
        reset();
        set_mode(Mode::Direct);
        emit_line("same");
        assert_eq!(lines(), buffered);
        set_mode(Mode::Buffered);
    }

    #[test]
    fn the_macros_write_through_the_console() {
        let _g = fresh();
        outln!("value {}", 41 + 1);
        out!("no newline");
        assert_eq!(lines(), vec!["value 42"]);
        assert_eq!(pending(), "no newline");
    }
}
