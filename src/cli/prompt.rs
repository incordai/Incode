//! Asking the person at the terminal, in the middle of a turn.
//!
//! The memory engine can want a person to confirm a tool call, and a model can
//! put a question to the owner. In the REPL that person is at the keyboard, so
//! the question is asked there — while the turn waits — and answered with ONE
//! key. The owner's app may answer the same thing first (see
//! [`crate::hooks::engine::Engine::settle`]); the prompt then stands down.
//!
//! One key rather than a typed line: the input box is not reading during a
//! turn, and a half-typed line in a terminal that is also streaming the turn
//! is a line nobody can see. Free words go through the question's last option,
//! "let's talk about it", which ends the turn so they can be typed at the
//! prompt.

use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::ask::{Answer, Question};
use crate::cli::render::PromptLease;
use crate::hooks::engine::{Ask, Prompter, Verdict};

/// How often the prompt looks at whether the answer came from elsewhere.
const LOOK_EVERY: Duration = Duration::from_millis(100);

/// The person at the REPL's terminal.
pub struct Terminal;

/// Raw mode for as long as this lives, restored to what it was.
struct Raw {
    was_raw: bool,
}

impl Raw {
    fn on() -> Self {
        let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
        if !was_raw {
            let _ = crossterm::terminal::enable_raw_mode();
        }
        Self { was_raw }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        if !self.was_raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

/// One key press, or `None` when the wait should end.
enum Key {
    Char(char),
    /// Enter: the recommended option, when the question has one.
    Enter,
    Escape,
    Interrupt,
}

/// The next key, looking at `stop` between polls. `None` when `stop` turned
/// true or the terminal cannot be read.
fn next_key(stop: &dyn Fn() -> bool) -> Option<Key> {
    loop {
        if stop() {
            return None;
        }
        match crossterm::event::poll(LOOK_EVERY) {
            Ok(false) => {}
            Ok(true) => match crossterm::event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        return Some(Key::Interrupt);
                    }
                    match key.code {
                        KeyCode::Esc => return Some(Key::Escape),
                        KeyCode::Enter => return Some(Key::Enter),
                        KeyCode::Char(c) => return Some(Key::Char(c)),
                        _ => {}
                    }
                }
                Ok(_) => {}
                Err(_) => return None,
            },
            Err(_) => return None,
        }
    }
}

/// What the call touches, in a few words: its command or its file.
fn what(ask: &Ask<'_>) -> String {
    ["command", "path", "file_path"]
        .iter()
        .find_map(|key| ask.tool_input.get(*key).and_then(serde_json::Value::as_str))
        .map(|target| format!("{} {}", ask.tool_name, crate::text::truncate(target, 120)))
        .unwrap_or_else(|| ask.tool_name.to_string())
}

impl Prompter for Terminal {
    fn confirm(&self, ask: &Ask<'_>, settled: &dyn Fn() -> bool) -> Option<Verdict> {
        let lease = PromptLease::begin();
        lease.say(&format!(
            "\n{} needs your confirmation.\n  {}\nAllow it? y = yes, n = no. The owner's app can answer it too.\n",
            what(ask),
            ask.reason
        ));
        let _raw = Raw::on();
        loop {
            match next_key(settled) {
                None => {
                    lease.say("Answered elsewhere.\n");
                    return None;
                }
                Some(Key::Char('y' | 'Y')) => {
                    lease.say("Allowed.\n");
                    return Some(Verdict::Allow);
                }
                Some(Key::Char('n' | 'N') | Key::Escape | Key::Interrupt) => {
                    lease.say("Refused.\n");
                    return Some(Verdict::Deny(
                        "NOT RUN — the person at the terminal refused this call.".to_string(),
                    ));
                }
                // Enter is not an answer here: allowing a tool call takes an
                // explicit `y`.
                Some(Key::Char(_) | Key::Enter) => {}
            }
        }
    }

    fn choose(&self, questions: &[Question]) -> Option<Vec<Answer>> {
        let lease = PromptLease::begin();
        let _raw = Raw::on();
        let mut answers = Vec::with_capacity(questions.len());
        for question in questions {
            // The menu without its closing line, which invites typed words
            // that a one-key prompt cannot take.
            let menu = question.render();
            let options: Vec<&str> = menu.lines().collect();
            let shown = options[..options.len().saturating_sub(1)].join("\n");
            let default = question
                .recommended()
                .map(|c| format!(" Enter takes the recommended one: {}.", c.label))
                .unwrap_or_default();
            lease.say(&format!(
                "\n{shown}\nPress a letter or a number.{default} To say something else, pick the \
                 last one. Esc leaves it unanswered.\n"
            ));
            loop {
                // Enter is the empty answer, which is the recommendation —
                // and, with none, nothing, so the prompt keeps waiting.
                let typed = match next_key(&|| false) {
                    None | Some(Key::Escape | Key::Interrupt) => {
                        lease.say("Left unanswered.\n");
                        return None;
                    }
                    Some(Key::Enter) => String::new(),
                    Some(Key::Char(c)) => c.to_string(),
                };
                match question.answer_for(&typed) {
                    Answer::Said(_) => {}
                    picked => {
                        if let Answer::Chose(label) = &picked {
                            lease.say(&format!("{label}\n"));
                        }
                        answers.push(picked);
                        break;
                    }
                }
            }
        }
        Some(answers)
    }
}
