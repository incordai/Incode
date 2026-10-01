//! The one thread allowed to touch the mouse and keyboard.
//!
//! # Why a thread and not a lock
//!
//! Three facts collide:
//!
//! 1. **`Enigo` is not `Send` on Windows.** Measured in enigo 0.6.1: there is an
//!    `unsafe impl Send for Enigo` in `src/macos/macos_impl.rs:100` and no
//!    equivalent under `src/win/`. So the handle cannot cross a thread boundary
//!    here — and because that differs by platform, code that happens to compile
//!    on a Mac would stop compiling on Windows. Owning it on exactly one thread
//!    removes the question everywhere.
//! 2. **Input synthesis blocks.** It is a synchronous OS call.
//! 3. **This crate is `tokio` throughout.** Blocking on a runtime worker stalls
//!    every other task in the process.
//!
//! One dedicated OS thread, reached by channel, answers all three at once. It
//! also gives Windows a stable place to pump messages if a later backend needs
//! one, and it gives the rate cap a single choke point that cannot be bypassed
//! by calling a different function.
//!
//! # The rate cap is here deliberately, ahead of its step
//!
//! The build order puts the kill switch at step 5 and this at step 4. The cap
//! is pulled forward because it belongs structurally to the thread that does
//! the acting: a runaway loop clicking fifty times a second does real damage
//! long before anyone reads a log, and on an unattended desktop (decision 7)
//! there is nobody to notice. A capability that can click should not exist for
//! even a day without a bound on how fast.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};

use super::coords::PhysicalPoint;
use super::safety::Safety;
use super::DesktopError;

/// Most actions allowed in any rolling minute.
///
/// Sized for an agent driving a UI, which acts in ones and twos with thinking
/// in between — not for a loop. A human clicking briskly does not approach it;
/// a runaway hits it within seconds.
pub const MAX_ACTIONS_PER_MINUTE: usize = 120;

const WINDOW: Duration = Duration::from_secs(60);

/// Which mouse button, in this crate's own vocabulary.
///
/// Not `enigo::Button`, so the dependency does not leak into the MCP schema
/// that agents see — the same reasoning as `ShotFormat` in
/// [`super::capture`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    fn to_enigo(self) -> Button {
        match self {
            MouseButton::Left => Button::Left,
            MouseButton::Right => Button::Right,
            MouseButton::Middle => Button::Middle,
        }
    }
}

/// What the worker thread can be asked to do.
///
/// Move and click only, which is all step 4 of the build order covers. Keyboard
/// and clipboard arrive at step 8 as further variants; the thread, the channel
/// and the cap do not change when they do.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    MoveMouse(PhysicalPoint),
    Click {
        at: PhysicalPoint,
        button: MouseButton,
        /// 1 = single, 2 = double, 3 = triple.
        count: u8,
    },
    CursorPosition,
    /// Not an action; used to prove the thread is alive without moving anything.
    Ping,

    /// Synthesise keystrokes for this text.
    ///
    /// Slow, and some applications drop characters from a fast synthetic
    /// stream. `paste_text` exists for anything long — see [`InputThread::paste_text`].
    Type(String),

    /// Press these keys together, releasing in reverse order.
    ///
    /// The whole reason a chord is ONE command: pressing Ctrl and then sending
    /// C as a separate request leaves Ctrl held if anything fails between them,
    /// and every later keystroke arrives modified.
    Chord(Vec<String>),

    /// Scroll at the current cursor position. Positive `dy` scrolls down.
    Scroll {
        dx: i32,
        dy: i32,
    },

    /// Press at one point, move, release at another.
    Drag {
        from: PhysicalPoint,
        to: PhysicalPoint,
        button: MouseButton,
    },
}

impl Command {
    /// Whether this consumes rate-cap budget.
    ///
    /// Reads do not: refusing to tell an agent where the cursor is would not
    /// make a runaway safer, it would only blind the thing trying to recover.
    fn is_action(&self) -> bool {
        !matches!(self, Command::CursorPosition | Command::Ping)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reply {
    Done,
    Position(PhysicalPoint),
}

struct Job {
    command: Command,
    reply: SyncSender<Result<Reply, DesktopError>>,
}

/// A handle to the input thread.
///
/// Dropping it stops the thread and joins it, so a session cannot leave an
/// input-capable thread running behind it — see the dead-man's-switch reasoning
/// in `docs/COMPUTER_USE.md` 7b.
pub struct InputThread {
    tx: Option<Sender<Job>>,
    handle: Option<JoinHandle<()>>,
    safety: Safety,
}

impl InputThread {
    /// Start the thread with its own stop control.
    pub fn start() -> Result<Self, DesktopError> {
        InputThread::start_with(Safety::with_default_deadline())
    }

    /// Start the thread, sharing a stop control the caller already holds.
    ///
    /// The caller keeps its clone, which is how an HTTP handler on the
    /// harness's port can halt a sequence that is already running — see
    /// [`super::safety`].
    ///
    /// Blocks until the worker has actually constructed its `Enigo`: a
    /// constructor that failed would otherwise surface much later, on the first
    /// click, as a channel error with nothing to say about the cause.
    pub fn start_with(safety: Safety) -> Result<Self, DesktopError> {
        // Awareness first: coordinates handed to this thread are physical, and
        // they are only physical if the process is per-monitor aware.
        super::dpi::ensure_per_monitor_aware();

        let (tx, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);

        let worker_safety = safety.clone();
        let handle = std::thread::Builder::new()
            .name("incode-desktop-input".to_string())
            .spawn(move || worker(rx, ready_tx, worker_safety))
            .map_err(|e| DesktopError::Os(format!("could not start the input thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(InputThread {
                tx: Some(tx),
                handle: Some(handle),
                safety,
            }),
            Ok(Err(why)) => Err(DesktopError::Os(why)),
            Err(_) => Err(DesktopError::Os(
                "the input thread stopped before it reported readiness".to_string(),
            )),
        }
    }

    /// The stop control this thread obeys.
    ///
    /// Handed out rather than hidden: the switch is only useful to something
    /// that is NOT this thread.
    pub fn safety(&self) -> Safety {
        self.safety.clone()
    }

    fn send(&self, command: Command) -> Result<Reply, DesktopError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| DesktopError::Os("the input thread is shutting down".to_string()))?;

        tx.send(Job {
            command,
            reply: reply_tx,
        })
        .map_err(|_| DesktopError::Os("the input thread is gone".to_string()))?;

        reply_rx
            .recv()
            .map_err(|_| DesktopError::Os("the input thread died mid-action".to_string()))?
    }

    /// Move the cursor. Coordinates are physical pixels.
    pub fn move_mouse(&self, to: PhysicalPoint) -> Result<(), DesktopError> {
        self.send(Command::MoveMouse(to)).map(|_| ())
    }

    /// Move to a point and click there.
    ///
    /// Move and click are one command on purpose: as two, another action could
    /// be interleaved between them and the click would land somewhere the
    /// caller never named.
    pub fn click(
        &self,
        at: PhysicalPoint,
        button: MouseButton,
        count: u8,
    ) -> Result<(), DesktopError> {
        if count == 0 || count > 3 {
            return Err(DesktopError::Os(format!(
                "click count must be 1, 2 or 3; got {count}"
            )));
        }
        self.send(Command::Click { at, button, count }).map(|_| ())
    }

    /// Type text as keystrokes.
    ///
    /// For anything long or non-ASCII prefer [`Self::paste_text`]: synthesised
    /// keystrokes are slow and some applications drop characters from a fast
    /// stream.
    pub fn type_text(&self, text: &str) -> Result<(), DesktopError> {
        if text.is_empty() {
            return Ok(());
        }
        self.send(Command::Type(text.to_string())).map(|_| ())
    }

    /// Put text on the clipboard and paste it with Ctrl+V.
    ///
    /// Far more reliable than typing for long or non-ASCII text, and near
    /// instant where typing is linear in length.
    ///
    /// **It costs the clipboard.** Whatever the operator had copied is gone,
    /// and this does not restore it — restoring would need to read the previous
    /// contents, which on an agent's desktop may be something it should not
    /// see. Losing a clipboard is the smaller harm, and it is stated rather
    /// than hidden.
    pub fn paste_text(&self, text: &str) -> Result<(), DesktopError> {
        if text.is_empty() {
            return Ok(());
        }
        set_clipboard(text)?;
        self.chord(&["control".to_string(), "v".to_string()])
    }

    /// Press keys together, releasing in reverse order.
    ///
    /// `["control", "c"]` is Ctrl+C. Names are matched case-insensitively; see
    /// [`key_from_name`] for the vocabulary.
    pub fn chord(&self, keys: &[String]) -> Result<(), DesktopError> {
        if keys.is_empty() {
            return Err(DesktopError::Os(
                "a chord needs at least one key".to_string(),
            ));
        }
        // Rejected HERE, before the thread holds anything down: an unknown name
        // discovered halfway through a press sequence would leave the earlier
        // keys stuck.
        for name in keys {
            key_from_name(name)?;
        }
        self.send(Command::Chord(keys.to_vec())).map(|_| ())
    }

    /// Scroll at the cursor. Positive `dy` scrolls down.
    pub fn scroll(&self, dx: i32, dy: i32) -> Result<(), DesktopError> {
        self.send(Command::Scroll { dx, dy }).map(|_| ())
    }

    /// Press, move, release — one command, so nothing interleaves mid-drag.
    pub fn drag(
        &self,
        from: PhysicalPoint,
        to: PhysicalPoint,
        button: MouseButton,
    ) -> Result<(), DesktopError> {
        self.send(Command::Drag { from, to, button }).map(|_| ())
    }

    /// Where the cursor is now.
    pub fn cursor_position(&self) -> Result<PhysicalPoint, DesktopError> {
        match self.send(Command::CursorPosition)? {
            Reply::Position(p) => Ok(p),
            Reply::Done => Err(DesktopError::Os(
                "the input thread answered a position query with nothing".to_string(),
            )),
        }
    }

    /// Prove the thread is alive without touching the desktop.
    pub fn ping(&self) -> Result<(), DesktopError> {
        self.send(Command::Ping).map(|_| ())
    }
}

impl Drop for InputThread {
    fn drop(&mut self) {
        // Dropping the sender is what ends the worker's loop; joining then
        // guarantees no input can be synthesised after this returns.
        self.tx = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn worker(rx: Receiver<Job>, ready: SyncSender<Result<(), String>>, safety: Safety) {
    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(enigo) => {
            let _ = ready.send(Ok(()));
            enigo
        }
        Err(e) => {
            let _ = ready.send(Err(format!("could not open an input connection: {e}")));
            return;
        }
    };

    let mut recent: VecDeque<Instant> = VecDeque::new();

    while let Ok(job) = rx.recv() {
        let now = Instant::now();

        // Order matters. The stop switch is checked FIRST and the rate cap
        // second, so a halted desktop says "stopped" rather than "too fast" —
        // and so a stopped action never spends rate budget it will not use.
        let result = if !job.command.is_action() {
            run(&mut enigo, job.command)
        } else if let Err(refused) = safety.authorise(now) {
            Err(refused)
        } else if !allow(&mut recent, now) {
            Err(DesktopError::Os(format!(
                "refused: more than {MAX_ACTIONS_PER_MINUTE} actions in the last minute. \
                 This is the runaway guard, not a transient error — an agent hitting it \
                 is looping, and waiting will not help."
            )))
        } else {
            run(&mut enigo, job.command)
        };

        // A receiver that has gone away means the caller stopped waiting. The
        // action already happened, so there is nothing to undo; drop the answer
        // and carry on rather than killing the thread.
        let _ = job.reply.send(result);
    }
}

/// Rate-cap decision over a rolling window. Pure, so the policy is testable
/// without a thread or a desktop.
fn allow(recent: &mut VecDeque<Instant>, now: Instant) -> bool {
    while let Some(front) = recent.front() {
        if now.duration_since(*front) >= WINDOW {
            recent.pop_front();
        } else {
            break;
        }
    }

    if recent.len() >= MAX_ACTIONS_PER_MINUTE {
        return false;
    }

    recent.push_back(now);
    true
}

/// A key name in this crate's vocabulary, mapped to enigo's.
///
/// A small closed set rather than a pass-through, for the same reason `Role`
/// and `ShotFormat` are: the tool surface must read the same everywhere, and an
/// agent should get a refusal naming the problem rather than a silent no-op for
/// a key that does not exist.
pub fn key_from_name(name: &str) -> Result<Key, DesktopError> {
    let lower = name.trim().to_ascii_lowercase();
    Ok(match lower.as_str() {
        "control" | "ctrl" => Key::Control,
        "shift" => Key::Shift,
        "alt" => Key::Alt,
        "meta" | "win" | "super" | "command" | "cmd" => Key::Meta,
        "enter" | "return" => Key::Return,
        "tab" => Key::Tab,
        "escape" | "esc" => Key::Escape,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "space" => Key::Space,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        // A single character is itself. Counted in CHARS, not bytes: "é" is two
        // bytes and one key.
        other => {
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Key::Unicode(c),
                _ => {
                    return Err(DesktopError::Os(format!(
                        "unknown key '{name}'. Use a single character, or one of: \
                         control shift alt meta enter tab escape backspace delete \
                         space up down left right home end pageup pagedown f1-f12"
                    )))
                }
            }
        }
    })
}

/// Put text on the Windows clipboard.
#[cfg(windows)]
fn set_clipboard(text: &str) -> Result<(), DesktopError> {
    // enigo pulls clipboard-win already (it is in the lock as a dependency of
    // enigo's Windows backend), so this adds no package.
    clipboard_win::set_clipboard(clipboard_win::formats::Unicode, text)
        .map_err(|e| DesktopError::Os(format!("could not set the clipboard: {e}")))
}

/// Put text on the macOS clipboard.
#[cfg(target_os = "macos")]
fn set_clipboard(text: &str) -> Result<(), DesktopError> {
    super::macos::set_clipboard(text)
}

/// Put text on the X display's clipboard.
#[cfg(target_os = "linux")]
fn set_clipboard(text: &str) -> Result<(), DesktopError> {
    super::linux::set_clipboard(text)
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn set_clipboard(_text: &str) -> Result<(), DesktopError> {
    Err(DesktopError::Unsupported("this platform"))
}

fn run(enigo: &mut Enigo, command: Command) -> Result<Reply, DesktopError> {
    // Without the Accessibility grant macOS drops synthetic input without a
    // word, which reads as a click that missed. Refuse by name instead.
    #[cfg(target_os = "macos")]
    if !matches!(command, Command::Ping) {
        super::macos::require(super::macos::Grant::Accessibility)?;
    }
    match command {
        Command::Ping => Ok(Reply::Done),

        Command::Type(text) => {
            enigo
                .text(&text)
                .map_err(|e| DesktopError::Os(format!("could not type: {e}")))?;
            Ok(Reply::Done)
        }

        Command::Chord(names) => {
            let keys: Vec<Key> = names
                .iter()
                .map(|n| key_from_name(n))
                .collect::<Result<_, _>>()?;

            // Press in order, release in REVERSE. Releasing in the same order
            // would lift Ctrl before C on a Ctrl+C, which some applications see
            // as a bare C keypress.
            let mut pressed = Vec::new();
            let mut failure = None;
            for key in &keys {
                if let Err(e) = enigo.key(*key, Direction::Press) {
                    failure = Some(format!("could not press a key: {e}"));
                    break;
                }
                pressed.push(*key);
            }
            // Release everything actually pressed even on failure — a stuck
            // modifier would modify every later keystroke in the session.
            for key in pressed.iter().rev() {
                let _ = enigo.key(*key, Direction::Release);
            }

            match failure {
                Some(why) => Err(DesktopError::Os(why)),
                None => Ok(Reply::Done),
            }
        }

        Command::Scroll { dx, dy } => {
            if dy != 0 {
                enigo
                    .scroll(dy, Axis::Vertical)
                    .map_err(|e| DesktopError::Os(format!("could not scroll: {e}")))?;
            }
            if dx != 0 {
                enigo
                    .scroll(dx, Axis::Horizontal)
                    .map_err(|e| DesktopError::Os(format!("could not scroll: {e}")))?;
            }
            Ok(Reply::Done)
        }

        Command::Drag { from, to, button } => {
            enigo
                .move_mouse(from.x, from.y, Coordinate::Abs)
                .map_err(|e| DesktopError::Os(format!("could not move to the drag start: {e}")))?;
            enigo
                .button(button.to_enigo(), Direction::Press)
                .map_err(|e| DesktopError::Os(format!("could not press to drag: {e}")))?;

            // Stepped, not teleported. A single jump between press and release
            // is ignored by many drag targets, which need intermediate motion
            // to recognise a drag at all.
            const STEPS: i32 = 12;
            for step in 1..=STEPS {
                let x = from.x + (to.x - from.x) * step / STEPS;
                let y = from.y + (to.y - from.y) * step / STEPS;
                let _ = enigo.move_mouse(x, y, Coordinate::Abs);
            }

            let released = enigo.button(button.to_enigo(), Direction::Release);
            released.map_err(|e| DesktopError::Os(format!("could not release the drag: {e}")))?;
            Ok(Reply::Done)
        }

        Command::CursorPosition => {
            let (x, y) = enigo
                .location()
                .map_err(|e| DesktopError::Os(format!("could not read the cursor: {e}")))?;
            Ok(Reply::Position(PhysicalPoint { x, y }))
        }

        Command::MoveMouse(to) => {
            enigo
                .move_mouse(to.x, to.y, Coordinate::Abs)
                .map_err(|e| DesktopError::Os(format!("could not move the cursor: {e}")))?;
            Ok(Reply::Done)
        }

        Command::Click { at, button, count } => {
            enigo
                .move_mouse(at.x, at.y, Coordinate::Abs)
                .map_err(|e| DesktopError::Os(format!("could not move before clicking: {e}")))?;

            for _ in 0..count {
                enigo
                    .button(button.to_enigo(), Direction::Click)
                    .map_err(|e| DesktopError::Os(format!("could not click: {e}")))?;
            }
            Ok(Reply::Done)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_do_not_spend_the_rate_budget() {
        // Blinding an operator who is trying to work out what a runaway is
        // doing would make recovery harder, not safer.
        assert!(!Command::CursorPosition.is_action());
        assert!(!Command::Ping.is_action());
        assert!(Command::MoveMouse(PhysicalPoint { x: 0, y: 0 }).is_action());
        assert!(Command::Click {
            at: PhysicalPoint { x: 0, y: 0 },
            button: MouseButton::Left,
            count: 1,
        }
        .is_action());
    }

    #[test]
    fn the_cap_allows_a_normal_burst_and_refuses_a_runaway() {
        let mut recent = VecDeque::new();
        let start = Instant::now();

        for i in 0..MAX_ACTIONS_PER_MINUTE {
            assert!(
                allow(&mut recent, start),
                "action {i} refused while still inside the cap"
            );
        }
        assert!(
            !allow(&mut recent, start),
            "action {MAX_ACTIONS_PER_MINUTE} must be refused"
        );
    }

    #[test]
    fn the_window_rolls_rather_than_resetting() {
        let mut recent = VecDeque::new();
        let start = Instant::now();

        for _ in 0..MAX_ACTIONS_PER_MINUTE {
            assert!(allow(&mut recent, start));
        }
        assert!(!allow(&mut recent, start));

        // Just inside the window: still refused. A fixed bucket that reset on
        // the minute would allow a double-rate burst across the boundary.
        assert!(!allow(&mut recent, start + Duration::from_secs(59)));

        // Past it: the old entries age out and budget returns.
        assert!(allow(&mut recent, start + Duration::from_secs(61)));
    }

    #[test]
    fn the_key_vocabulary_covers_what_an_agent_actually_presses() {
        for name in [
            "control",
            "ctrl",
            "shift",
            "alt",
            "meta",
            "enter",
            "return",
            "tab",
            "escape",
            "esc",
            "backspace",
            "delete",
            "space",
            "up",
            "down",
            "left",
            "right",
            "home",
            "end",
            "pageup",
            "pagedown",
            "f1",
            "f12",
        ] {
            assert!(key_from_name(name).is_ok(), "'{name}' should be known");
        }
        // Case does not matter — an agent writing "Control" must not be refused.
        assert!(key_from_name("CONTROL").is_ok());
        assert!(key_from_name("  Enter  ").is_ok());
    }

    #[test]
    fn a_single_character_is_a_key_but_a_word_is_not() {
        assert!(key_from_name("c").is_ok());
        assert!(key_from_name("7").is_ok());
        // Multi-byte, single CHARACTER: counting bytes would refuse this.
        assert!(key_from_name("é").is_ok(), "one char, two bytes");

        let refused = key_from_name("contrl").expect_err("a typo must be refused");
        assert!(
            refused.to_string().contains("unknown key"),
            "the refusal must name the problem: {refused}"
        );
        // And list the alternatives, so the agent can correct itself.
        assert!(refused.to_string().contains("pagedown"), "{refused}");
    }

    /// An unknown key is rejected BEFORE anything is held down.
    ///
    /// Validating mid-sequence would leave the earlier keys pressed, and a
    /// stuck Ctrl modifies every later keystroke in the session.
    #[cfg(windows)]
    #[test]
    fn a_chord_with_a_bad_key_is_refused_before_anything_is_pressed() {
        let input = InputThread::start().expect("start");

        let refused = input
            .chord(&["control".to_string(), "nonsense".to_string()])
            .expect_err("an unknown key must be refused");
        assert!(refused.to_string().contains("unknown key"), "{refused}");

        // The thread is still healthy, which it would not be if a press had
        // been left dangling.
        input.ping().expect("the thread must still answer");
    }

    #[cfg(windows)]
    #[test]
    fn an_empty_chord_is_refused_and_empty_text_is_a_no_op() {
        let input = InputThread::start().expect("start");
        assert!(input.chord(&[]).is_err(), "an empty chord means nothing");
        // Empty text is not an error — it is simply nothing to type, and
        // failing would make a caller special-case every optional field.
        assert!(input.type_text("").is_ok());
    }

    /// Typing is an action and spends rate budget; reading does not.
    #[test]
    fn every_new_input_command_counts_against_the_runaway_cap() {
        assert!(Command::Type("x".to_string()).is_action());
        assert!(Command::Chord(vec!["control".to_string()]).is_action());
        assert!(Command::Scroll { dx: 0, dy: 3 }.is_action());
        assert!(Command::Drag {
            from: PhysicalPoint { x: 0, y: 0 },
            to: PhysicalPoint { x: 1, y: 1 },
            button: MouseButton::Left,
        }
        .is_action());

        assert!(!Command::CursorPosition.is_action());
        assert!(!Command::Ping.is_action());
    }

    #[test]
    fn button_names_round_trip_through_json() {
        // They cross the MCP boundary, so a rename changes the wire format.
        assert_eq!(
            serde_json::to_string(&MouseButton::Left).expect("serialise"),
            "\"left\""
        );
        assert_eq!(
            serde_json::to_string(&MouseButton::Middle).expect("serialise"),
            "\"middle\""
        );
    }

    /// These START THE THREAD but move nothing.
    #[cfg(windows)]
    #[test]
    fn the_thread_starts_reports_ready_and_stops_cleanly() {
        let input = InputThread::start().expect("the input thread must start");
        input.ping().expect("a live thread must answer a ping");

        let at = input
            .cursor_position()
            .expect("a live thread must report the cursor");
        // Sanity only: the cursor is somewhere on a virtual desktop that may
        // have negative coordinates, so the assertion is deliberately weak.
        assert!(
            at.x > -100_000 && at.x < 100_000,
            "implausible cursor: {at:?}"
        );

        drop(input);
        // Dropping joined the thread; nothing can synthesise input now.
    }

    #[cfg(windows)]
    #[test]
    fn a_click_count_outside_one_to_three_is_refused_before_it_moves_anything() {
        let input = InputThread::start().expect("start");
        let at = input.cursor_position().expect("cursor");

        for bad in [0u8, 4, 255] {
            let refused = input.click(at, MouseButton::Left, bad);
            assert!(refused.is_err(), "click count {bad} should be refused");
        }
    }

    /// THE KILL SWITCH, PROVEN AGAINST THE REAL THREAD.
    ///
    /// Not a test of `Safety` — `safety.rs` covers the policy. This is the
    /// wiring: that the switch is actually consulted by the thread that moves
    /// the mouse, from a handle held by somebody else. A stop control that is
    /// correct but unwired is the failure mode 7b was rewritten to avoid.
    ///
    /// It is safe to run in the normal suite BECAUSE the stop takes effect: if
    /// the wiring were missing the cursor would move, which is exactly what the
    /// assertion catches.
    #[cfg(windows)]
    #[test]
    fn stopping_the_switch_refuses_a_real_action_and_the_cursor_does_not_move() {
        let input = InputThread::start().expect("start");
        let before = input.cursor_position().expect("cursor");

        // Somebody else's handle — the shape an HTTP stop handler would have.
        let remote = input.safety();
        remote.stop();

        let target = PhysicalPoint {
            x: before.x + 137,
            y: before.y + 89,
        };
        let refused = input
            .move_mouse(target)
            .expect_err("a stopped desktop must refuse to move");
        assert!(
            refused.to_string().contains("permanent"),
            "the refusal did not come from the stop switch: {refused}"
        );

        let after = input.cursor_position().expect("cursor");
        assert_eq!(
            before, after,
            "THE CURSOR MOVED AFTER A STOP — the switch is not wired to the thread"
        );
    }

    /// Reads must keep working after a stop.
    ///
    /// An operator diagnosing why the desktop halted still needs to see where
    /// things are; a stop that also blinds them makes recovery harder.
    #[cfg(windows)]
    #[test]
    fn a_stopped_desktop_can_still_be_asked_where_the_cursor_is() {
        let input = InputThread::start().expect("start");
        input.safety().stop();

        input
            .cursor_position()
            .expect("a stopped desktop must still answer reads");
        input.ping().expect("ping must still work after a stop");
    }

    /// THE ONLY TEST THAT MOVES THE REAL CURSOR.
    ///
    /// `#[ignore]` on purpose: the normal suite must never grab the mouse out
    /// from under whoever is using this machine. It restores the original
    /// position afterwards, but a cursor that jumps mid-sentence is disruptive
    /// even when it jumps back.
    ///
    /// `cargo test --lib desktop::thread::tests::moving_the_real_cursor -- --ignored --nocapture`
    #[cfg(windows)]
    #[test]
    #[ignore = "moves the real mouse cursor"]
    fn moving_the_real_cursor_lands_where_it_was_told() {
        let input = InputThread::start().expect("start");
        let original = input.cursor_position().expect("cursor");
        println!("\ncursor was at {original:?}");

        let screen = super::super::screen_info().expect("screen");
        let primary = screen.primary().expect("primary");
        let (cx, cy) = primary.rect.center();
        let target = PhysicalPoint { x: cx, y: cy };

        input.move_mouse(target).expect("move");
        let landed = input.cursor_position().expect("cursor");
        println!("asked for   {target:?}\nlanded at   {landed:?}");

        // Put it back before asserting, so a failure does not also leave the
        // operator's cursor parked in the middle of the screen.
        input.move_mouse(original).expect("restore");

        assert!(
            (landed.x - target.x).abs() <= 1 && (landed.y - target.y).abs() <= 1,
            "cursor landed at {landed:?} when told {target:?}"
        );
        println!(
            "restored to {:?}\n",
            input.cursor_position().expect("cursor")
        );
    }
}
