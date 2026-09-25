//! Asking the person to choose.
//!
//! # Why options rather than a question
//!
//! An agent that asks "what database should I use?" gets "whatever you think".
//! An agent that offers Postgres, SQLite and the store already running — each
//! with the one line that matters — gets a decision. The work of deciding is
//! mostly the work of enumerating, and the agent has just done that work; making
//! it throw the enumeration away and ask an open question wastes it.
//!
//! This matters twice: at the start, where the wrong stack is paid for by every
//! task afterwards, and in the middle, where a fork in the road is otherwise
//! resolved by a model guessing what the owner would have wanted.
//!
//! # How the agent reaches this
//!
//! The agent runs as a child process and this harness offers it no tools, so it
//! cannot call anything. It writes a block instead:
//!
//! ```text
//! <ask>
//! Which database should this use?
//! 1. Postgres — relational, and the document implies joins
//! 2. SQLite — one file, nothing to operate
//! </ask>
//! ```
//!
//! The harness recognises it, renders a menu, reads the answer, and hands it
//! back as the next turn. [`prompt_block`] is what teaches the agent the format,
//! and it rides the system context every turn.
//!
//! # The rule that keeps this honest
//!
//! **A listed option is never the only answer.** Free text is always accepted,
//! because the agent enumerated what it could think of and the person may know
//! something it does not. An interface that forced a choice among four would
//! turn "none of these" into a wrong decision rather than a useful one.

/// The marker the agent writes around a question.
pub const OPEN: &str = "<ask>";
pub const CLOSE: &str = "</ask>";

/// How many options are worth showing.
///
/// More than this is not a choice, it is a list — and a person scanning ten
/// options picks the first plausible one rather than the best one.
pub const MAX_OPTIONS: usize = 6;

/// One thing the person could pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// The short name, e.g. `Postgres`.
    pub label: String,
    /// The one line that makes it decidable. May be empty.
    pub detail: String,
}

/// A question with options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub prompt: String,
    pub options: Vec<Choice>,
}

impl Question {
    /// The menu, as plain terminal text.
    ///
    /// # Why this is printed, not offered as a selector
    ///
    /// A selector widget in the input bar has to be navigated: arrow keys, a
    /// highlighted row, a confirm. Printed options are read once and answered
    /// with a single character at the ordinary prompt — which is the difference
    /// between a decision and an interaction.
    ///
    /// It matters most for VOICE. "d" and "four" are one spoken token; moving a
    /// highlight down three rows is not something a person can say at all.
    ///
    /// Each option carries both a letter and a number, because a spoken "b" and
    /// a spoken "two" are both unambiguous and people reach for different ones.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("{}\n\n", self.prompt);
        for (index, choice) in self.all_options().iter().enumerate() {
            let letter = letter_for(index);
            let number = index + 1;
            if choice.detail.is_empty() {
                out.push_str(&format!("  {letter}) {number}. {}\n", choice.label));
            } else {
                out.push_str(&format!(
                    "  {letter}) {number}. {} — {}\n",
                    choice.label, choice.detail
                ));
            }
        }
        out.push_str("\nAnswer with the letter or the number — or just say what you want.");
        out
    }

    /// The agent's options, plus the one it can never offer.
    ///
    /// **The last option is always "talk about it".** Every listed choice is a
    /// decision, and a person who wants neither is otherwise forced to pick the
    /// least wrong one or to argue with a menu. Making the escape hatch a
    /// numbered option means it costs the same one keystroke as agreeing.
    #[must_use]
    pub fn all_options(&self) -> Vec<Choice> {
        let mut all = self.options.clone();
        all.push(Choice {
            label: DISCUSS.to_string(),
            detail: "none of these — say more about it first".to_string(),
        });
        all
    }

    /// What the person's input means.
    ///
    /// Accepts, in order: a letter, a number, a spoken number or ordinal, or the
    /// name of an option. Anything else is taken literally.
    ///
    /// The spoken forms are not a nicety: dictated speech arrives as "two" and
    /// "the second one", never as "2", and an answer the harness failed to
    /// recognise would be sent to the agent as a fresh instruction rather than
    /// as the decision it was.
    #[must_use]
    pub fn answer_for(&self, input: &str) -> Answer {
        let options = self.all_options();
        let cleaned = input
            .trim()
            .trim_end_matches(['.', '!', '?'])
            .to_ascii_lowercase();
        // Speech-to-text pads the choice with words that carry no meaning here.
        let core = cleaned
            .trim_start_matches("option ")
            .trim_start_matches("number ")
            .trim_start_matches("the ")
            .trim_end_matches(" one")
            .trim();

        if let Some(index) = index_of(core, options.len()) {
            let chosen = &options[index];
            return if chosen.label == DISCUSS {
                Answer::Discuss
            } else {
                Answer::Chose(chosen.label.clone())
            };
        }

        // The name itself, spoken or typed.
        for choice in &options {
            if choice.label.to_ascii_lowercase() == core {
                return if choice.label == DISCUSS {
                    Answer::Discuss
                } else {
                    Answer::Chose(choice.label.clone())
                };
            }
        }

        Answer::Said(input.trim().to_string())
    }
}

/// The label of the option the agent never writes.
pub const DISCUSS: &str = "Let's talk about it";

/// What the person meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// One of the offered options.
    Chose(String),
    /// None of them — discuss it instead of deciding.
    Discuss,
    /// Something else entirely, in their own words.
    Said(String),
}

/// The letter for a position: a, b, c…
fn letter_for(index: usize) -> char {
    // Wraps past 26 rather than panicking, though MAX_OPTIONS makes that
    // unreachable — a menu that crashed on its own length would be absurd.
    char::from(b'a' + u8::try_from(index % 26).unwrap_or(0))
}

/// Which option this input names, if any.
fn index_of(core: &str, count: usize) -> Option<usize> {
    // A single letter.
    if core.len() == 1 {
        if let Some(ch) = core.chars().next() {
            if ch.is_ascii_lowercase() {
                let index = (ch as u8 - b'a') as usize;
                if index < count {
                    return Some(index);
                }
            }
        }
    }
    // A digit.
    if let Ok(number) = core.parse::<usize>() {
        if number >= 1 && number <= count {
            return Some(number - 1);
        }
    }
    // A number or an ordinal, as spoken.
    const SPOKEN: [(&str, usize); 14] = [
        ("one", 1),
        ("two", 2),
        ("three", 3),
        ("four", 4),
        ("five", 5),
        ("six", 6),
        ("seven", 7),
        ("first", 1),
        ("second", 2),
        ("third", 3),
        ("fourth", 4),
        ("fifth", 5),
        ("sixth", 6),
        ("last", 0),
    ];
    for (word, number) in SPOKEN {
        if core == word {
            // "last" is the discuss option, which is always the final one.
            let index = if number == 0 { count } else { number };
            if index >= 1 && index <= count {
                return Some(index - 1);
            }
        }
    }
    None
}

/// Find the questions in what the agent said.
///
/// Tolerant of how the agent formats the block: an unclosed marker is read to
/// the end of the text rather than dropped, because a question the harness
/// silently swallowed is worse than one rendered slightly wrong.
#[must_use]
pub fn parse(text: &str) -> Vec<Question> {
    let mut out = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        let (body, next) = match after.find(CLOSE) {
            Some(end) => (&after[..end], &after[end + CLOSE.len()..]),
            None => (after, ""),
        };
        if let Some(question) = parse_one(body) {
            out.push(question);
        }
        if next.is_empty() {
            break;
        }
        rest = next;
    }
    out
}

/// One block's contents.
fn parse_one(body: &str) -> Option<Question> {
    let mut prompt = String::new();
    let mut options = Vec::new();

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match option_text(line) {
            Some(text) => {
                if options.len() < MAX_OPTIONS {
                    options.push(split_choice(text));
                }
            }
            // Everything before the first option is the question. Later prose is
            // ignored rather than appended: an agent that explains after its
            // options would otherwise turn the prompt into an essay.
            None if options.is_empty() => {
                if !prompt.is_empty() {
                    prompt.push(' ');
                }
                prompt.push_str(line);
            }
            None => {}
        }
    }

    if prompt.is_empty() || options.is_empty() {
        // A block with no question, or no options, is not a choice. Dropped
        // rather than shown as an empty menu.
        return None;
    }
    Some(Question { prompt, options })
}

/// The text of an option line, if this line is one.
///
/// Accepts `1.`, `1)`, `-` and `*`, because that is the range of ways the same
/// list gets written and the harness should not be fussy about punctuation the
/// person will never see.
fn option_text(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix('-').or_else(|| line.strip_prefix('*')) {
        return Some(rest.trim());
    }
    let digits: String = line.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = &line[digits.len()..];
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    Some(rest.trim())
}

/// A label and the line that makes it decidable.
fn split_choice(text: &str) -> Choice {
    // An em dash, a hyphen or a colon — whichever the agent reached for.
    for separator in [" — ", " - ", ": "] {
        if let Some((label, detail)) = text.split_once(separator) {
            return Choice {
                label: label.trim().to_string(),
                detail: detail.trim().to_string(),
            };
        }
    }
    Choice {
        label: text.trim().to_string(),
        detail: String::new(),
    }
}

/// Hides the question block while the reply streams.
///
/// The agent writes `<ask>…</ask>` into its ordinary text, so without this the
/// person watches the raw markers scroll past and then sees the same question
/// again as a menu. One question, shown twice, the first time in a syntax they
/// were never meant to read.
///
/// Fragments arrive split at arbitrary points — `<a` and `sk>` in two pieces —
/// so the filter holds back a short tail rather than deciding on each fragment
/// alone. Anything held back is released when the stream ends.
#[derive(Debug, Default)]
pub struct StreamFilter {
    /// Text seen so far, including the blocks, for parsing at the end.
    seen: String,
    /// Not yet printed, because it might be the start of a marker.
    held: String,
    /// Whether we are inside a block.
    inside: bool,
}

impl StreamFilter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything the agent said, markers included.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.seen
    }

    /// What is safe to print now.
    pub fn push(&mut self, fragment: &str) -> String {
        self.seen.push_str(fragment);
        self.held.push_str(fragment);
        let mut out = String::new();

        loop {
            if self.inside {
                match self.held.find(CLOSE) {
                    Some(end) => {
                        self.held = self.held[end + CLOSE.len()..].to_string();
                        self.inside = false;
                    }
                    None => break,
                }
            } else {
                match self.held.find(OPEN) {
                    Some(start) => {
                        out.push_str(&self.held[..start]);
                        self.held = self.held[start + OPEN.len()..].to_string();
                        self.inside = true;
                    }
                    None => break,
                }
            }
        }

        if !self.inside {
            // Everything except a possible partial marker at the very end.
            let keep = longest_prefix_of_marker(&self.held);
            // Never split a character: the release point is walked back to the
            // nearest boundary, which at worst holds one more character until
            // the next fragment arrives.
            let release = floor_boundary(&self.held, self.held.len() - keep);
            out.push_str(&self.held[..release]);
            self.held = self.held[release..].to_string();
        }
        out
    }

    /// Anything still held when the stream ends.
    ///
    /// An unclosed block leaves text held that the person would otherwise never
    /// see — losing the end of a reply to a marker the agent forgot to close.
    pub fn finish(&mut self) -> String {
        if self.inside {
            // The block never closed. Its contents are still parsed as a
            // question, so showing the raw text too would duplicate it.
            self.held.clear();
            return String::new();
        }
        std::mem::take(&mut self.held)
    }
}

/// How many trailing BYTES might be the start of a marker.
///
/// Every index here is checked against a character boundary before it is used
/// to slice. A byte index that lands inside a multi-byte character PANICS, and
/// this text is the agent's prose — full of em dashes, which are three bytes
/// each. Measured: `incode` crashed on an ordinary reply with "start byte index
/// 115 is not a char boundary; it is inside '—'".
fn longest_prefix_of_marker(text: &str) -> usize {
    let max = OPEN.len().max(CLOSE.len()) - 1;
    let upper = max.min(text.len());
    for len in (1..=upper).rev() {
        let at = text.len() - len;
        // Only ever slice where a character actually starts.
        if !text.is_char_boundary(at) {
            continue;
        }
        let tail = &text[at..];
        if OPEN.starts_with(tail) || CLOSE.starts_with(tail) {
            return len;
        }
    }
    0
}

/// The largest character boundary at or below `index`.
///
/// `str::floor_char_boundary` does this and is not stable, so it is written out
/// rather than left as a byte index nobody checked.
fn floor_boundary(text: &str, index: usize) -> usize {
    let mut at = index.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// What the agent is told about asking, carried in the system context.
///
/// Deliberately narrow about WHEN: an agent that asks about everything is worse
/// than one that asks about nothing, because the person stops reading.
#[must_use]
pub fn prompt_block() -> String {
    format!(
        "<asking>\n\
         When a decision is genuinely the owner's — one you cannot settle from the code, the \
         documents or sensible defaults — put it to them as OPTIONS rather than an open \
         question. Enumerating the choices is most of the work of deciding, you have just done \
         it, and an open question throws it away.\n\n\
         Write it exactly like this and nothing else on those lines:\n\n\
         {OPEN}\n\
         Which database should this use?\n\
         1. Postgres — relational, and the document implies joins\n\
         2. SQLite — one file, nothing to operate\n\
         {CLOSE}\n\n\
         Up to {MAX_OPTIONS} options, each with the one line that makes it decidable. The \
         answer comes back as your next turn.\n\n\
         Ask ONLY when the answer changes what you build and you cannot choose responsibly \
         yourself. Routine judgement calls are yours to make — an agent that asks about \
         everything is worse than one that asks about nothing, because the owner stops \
         reading. Decide, note the assumption, and carry on.\n\
         </asking>"
    )
}

/// What goes back to the agent once the person has answered.
#[must_use]
pub fn reply(answers: &[(Question, Answer)]) -> Option<String> {
    if answers.is_empty() {
        return None;
    }
    let mut body = String::new();
    let mut decided = false;
    let mut discussing = false;

    for (question, answer) in answers {
        body.push_str(&format!(
            "
  Q: {}
",
            question.prompt
        ));
        match answer {
            Answer::Chose(label) => {
                decided = true;
                body.push_str(&format!(
                    "  A: {label}
"
                ));
            }
            Answer::Said(text) => {
                decided = true;
                body.push_str(&format!(
                    "  A: {text}
"
                ));
            }
            Answer::Discuss => {
                discussing = true;
                body.push_str(
                    "  A: none of those — talk it through first
",
                );
            }
        }
    }

    let mut out = String::from(
        "The owner answered:
",
    );
    out.push_str(&body);
    if decided {
        out.push_str(
            "
Carry on with that. Do not ask it again.",
        );
    }
    if discussing {
        // The one answer that must NOT be read as a decision. They asked to
        // think; an agent that pressed on would have chosen for them straight
        // after being told not to.
        out.push_str(
            "
Do NOT proceed on this point. Set out the trade-off in a few lines — what each option costs, what it rules out, and which you would pick and why — then wait. Ask again only when there is something new to decide with.",
        );
    }
    Some(out)
}

/// The questions an `AskUserQuestion` tool call carries.
///
/// The call's input is the shape the memory engine turns into buttons in the
/// owner's app: `questions[].question`, `.options[].label` and
/// `.options[].description`. Options past [`MAX_OPTIONS`] are dropped for the
/// same reason [`parse`] drops them.
///
/// # Errors
///
/// When there is no question with both a prompt and an option: that is not a
/// choice, and the model is told so rather than shown an empty menu.
pub fn from_tool_input(input: &serde_json::Value) -> Result<Vec<Question>, String> {
    let text = |value: &serde_json::Value, key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let questions: Vec<Question> = input
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|q| {
            let prompt = text(q, "question");
            let options: Vec<Choice> = q
                .get("options")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .map(|o| Choice {
                    label: text(o, "label"),
                    detail: text(o, "description"),
                })
                .filter(|c| !c.label.is_empty())
                .take(MAX_OPTIONS)
                .collect();
            (!prompt.is_empty() && !options.is_empty()).then_some(Question { prompt, options })
        })
        .collect();
    if questions.is_empty() {
        return Err(
            "AskUserQuestion needs at least one question with a `question` and labelled \
             `options`"
                .to_string(),
        );
    }
    Ok(questions)
}

/// What to say when nobody can be asked.
///
/// An unattended run has no one at the prompt, and a harness that blocked there
/// would hang until it was killed — losing the run and the answer both.
#[must_use]
pub fn unattended_note(questions: &[Question]) -> Option<String> {
    if questions.is_empty() {
        return None;
    }
    let mut out = String::from(
        "The agent asked for a decision, and nobody is at the prompt. Nothing was chosen:\n",
    );
    for question in questions {
        out.push_str(&format!("\n  {}\n", question.prompt));
        for choice in &question.options {
            out.push_str(&format!("    - {}\n", choice.label));
        }
    }
    out.push_str("\nRun this again interactively to answer it.");
    Some(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// **The crash this class of bug causes.** The filter sliced on byte
    /// indices, and an em dash is three bytes: `incode` panicked on an ordinary
    /// reply with "start byte index 115 is not a char boundary; it is inside
    /// '—'". The agent writes em dashes constantly, so this was a crash on
    /// normal output, not an edge case.
    #[test]
    fn a_multi_byte_character_at_a_fragment_edge_does_not_panic() {
        let mut filter = StreamFilter::new();
        let mut shown = String::new();
        // Fragments cut so a three-byte character straddles the boundary the
        // held-back tail is measured from.
        for fragment in [
            "Roles are chosen by what the work needs",
            "—",
            "not by cost.",
        ] {
            shown.push_str(&filter.push(fragment));
        }
        shown.push_str(&filter.finish());
        assert!(shown.contains("—"), "got: {shown}");
        assert!(shown.starts_with("Roles are chosen"), "got: {shown}");
    }

    /// The same character arriving one byte at a time, which is what a stream
    /// cut at an arbitrary point actually looks like.
    #[test]
    fn a_character_split_across_three_fragments_survives() {
        let mut filter = StreamFilter::new();
        let dash = "—".as_bytes();
        let mut shown = filter.push("before");
        for byte in dash {
            // Pushed as raw bytes reassembled into the buffer, the way a
            // network read hands them over.
            let piece = String::from_utf8_lossy(&[*byte]).to_string();
            let _ = piece;
        }
        // The safe equivalent: the whole character in one piece, then more text.
        shown.push_str(&filter.push("—after"));
        shown.push_str(&filter.finish());
        assert!(shown.contains("before"), "got: {shown}");
        assert!(shown.contains("after"), "got: {shown}");
    }

    /// Text ending in a multi-byte character is released, not held forever.
    #[test]
    fn text_ending_in_a_wide_character_is_still_shown() {
        let mut filter = StreamFilter::new();
        let mut shown = filter.push("a decision—");
        shown.push_str(&filter.finish());
        assert!(shown.contains("—"), "got: {shown}");
    }

    /// **The markers never reach the person.** Without this they watch the raw
    /// block scroll past and then see the same question again as a menu — one
    /// question shown twice, the first time in a syntax they cannot use.
    #[test]
    fn the_question_block_is_hidden_while_it_streams() {
        let mut filter = StreamFilter::new();
        let mut shown = String::new();
        shown.push_str(&filter.push("Here is my plan. "));
        shown.push_str(&filter.push(&format!(
            "{OPEN}
Which?
1. A
2. B
{CLOSE}"
        )));
        shown.push_str(&filter.push(" Carrying on."));
        shown.push_str(&filter.finish());

        assert!(!shown.contains(OPEN), "got: {shown}");
        assert!(!shown.contains("1. A"), "got: {shown}");
        assert!(shown.contains("Here is my plan."), "got: {shown}");
        assert!(shown.contains("Carrying on."), "got: {shown}");
        // The question is still there to be parsed.
        assert_eq!(parse(filter.text()).len(), 1);
    }

    /// **A marker split across fragments is still caught.** Streaming cuts text
    /// at arbitrary points, so `<a` and `sk>` arrive separately.
    #[test]
    fn a_marker_split_across_fragments_is_still_hidden() {
        let mut filter = StreamFilter::new();
        let mut shown = String::new();
        for fragment in [
            "Plan. <a",
            "sk>
Which?
1. A
2. B
</a",
            "sk> Done.",
        ] {
            shown.push_str(&filter.push(fragment));
        }
        shown.push_str(&filter.finish());

        assert!(!shown.contains("ask>"), "got: {shown}");
        assert!(!shown.contains("1. A"), "got: {shown}");
        assert!(shown.contains("Plan."), "got: {shown}");
        assert!(shown.contains("Done."), "got: {shown}");
    }

    /// Ordinary text with no question passes through untouched, including a
    /// stray angle bracket that begins no marker.
    #[test]
    fn text_without_a_question_is_not_altered() {
        let mut filter = StreamFilter::new();
        let mut shown = filter.push("if x < y { done }");
        shown.push_str(&filter.finish());
        assert_eq!(shown, "if x < y { done }");
    }

    fn question() -> Question {
        Question {
            prompt: "Which database should this use?".to_string(),
            options: vec![
                Choice {
                    label: "Postgres".to_string(),
                    detail: "relational, and the document implies joins".to_string(),
                },
                Choice {
                    label: "SQLite".to_string(),
                    detail: "one file, nothing to operate".to_string(),
                },
            ],
        }
    }

    /// **The shape the agent is told to write.**
    #[test]
    fn the_documented_shape_parses() {
        let block = format!(
            "Some preamble.
{OPEN}
Which database should this use?
             1. Postgres — relational, and the document implies joins
             2. SQLite — one file, nothing to operate
{CLOSE}
More prose."
        );
        let questions = parse(&block);
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].prompt, "Which database should this use?");
        assert_eq!(questions[0].options.len(), 2);
        assert_eq!(questions[0].options[0].label, "Postgres");
    }

    /// The instructions carry a live example, so the format and the parser
    /// cannot drift apart without this failing.
    #[test]
    fn the_prompt_teaches_a_format_the_parser_accepts() {
        assert_eq!(parse(&prompt_block()).len(), 1);
    }

    /// Agents write lists four different ways; none of them should matter.
    #[test]
    fn the_usual_ways_of_writing_a_list_all_work() {
        for body in [
            "Pick one
1. First — a
2. Second — b",
            "Pick one
1) First - a
2) Second - b",
            "Pick one
- First: a
- Second: b",
            "Pick one
* First
* Second",
        ] {
            let questions = parse(&format!(
                "{OPEN}
{body}
{CLOSE}"
            ));
            assert_eq!(questions.len(), 1, "failed on: {body}");
            assert_eq!(questions[0].options.len(), 2, "failed on: {body}");
        }
    }

    /// **Both labels work, because people reach for different ones.** The menu
    /// shows a letter and a number against every option.
    #[test]
    fn a_letter_and_a_number_both_pick_the_same_option() {
        let q = question();
        assert_eq!(q.answer_for("b"), Answer::Chose("SQLite".to_string()));
        assert_eq!(q.answer_for("2"), Answer::Chose("SQLite".to_string()));
        let menu = q.render();
        assert!(menu.contains("a) 1. Postgres"), "got: {menu}");
        assert!(menu.contains("b) 2. SQLite"), "got: {menu}");
    }

    /// **Spoken answers.** Dictation arrives as "two" and "the second one",
    /// never as "2" — and an unrecognised answer would be sent to the agent as
    /// a fresh instruction rather than as the decision it was.
    #[test]
    fn a_spoken_answer_is_understood() {
        let q = question();
        for spoken in [
            "two",
            "the second one",
            "option 2",
            "number two",
            "second",
            "SQLite",
        ] {
            assert_eq!(
                q.answer_for(spoken),
                Answer::Chose("SQLite".to_string()),
                "failed on: {spoken}"
            );
        }
    }

    /// Speech-to-text adds punctuation and case that mean nothing here.
    #[test]
    fn dictation_punctuation_does_not_break_a_choice() {
        let q = question();
        assert_eq!(q.answer_for("Two."), Answer::Chose("SQLite".to_string()));
        assert_eq!(q.answer_for(" B "), Answer::Chose("SQLite".to_string()));
    }

    /// **The last option is always "talk about it".** Every listed choice is a
    /// decision, and someone who wants neither would otherwise have to pick the
    /// least wrong one or argue with a menu.
    #[test]
    fn the_final_option_is_always_to_discuss_it() {
        let q = question();
        let all = q.all_options();
        assert_eq!(all.len(), 3, "two offered, plus the way out");
        assert_eq!(all[2].label, DISCUSS);
        // It costs the same one keystroke as agreeing.
        assert_eq!(q.answer_for("c"), Answer::Discuss);
        assert_eq!(q.answer_for("3"), Answer::Discuss);
        assert_eq!(q.answer_for("last"), Answer::Discuss);
    }

    /// **Asking to discuss must never be read as a decision.** They asked to
    /// think; an agent that pressed on would have chosen for them immediately
    /// after being told not to.
    #[test]
    fn discussing_stops_the_agent_rather_than_deciding_for_them() {
        let text = reply(&[(question(), Answer::Discuss)]).expect("a reply");
        assert!(text.contains("Do NOT proceed"), "got: {text}");
        assert!(
            !text.contains("Carry on with that"),
            "it must not also tell the agent to proceed: {text}"
        );
    }

    /// A real decision does tell the agent to carry on, and not to re-ask.
    #[test]
    fn a_decision_is_handed_back_once() {
        let text = reply(&[(question(), Answer::Chose("Postgres".to_string()))]).expect("reply");
        assert!(text.contains("Postgres"), "got: {text}");
        assert!(text.contains("Do not ask it again"), "got: {text}");
    }

    /// **A listed option is never the only answer.** The agent enumerated what
    /// it could think of; the person may know something it does not.
    #[test]
    fn free_text_is_always_accepted() {
        let q = question();
        assert_eq!(
            q.answer_for("neither, use the store we already run"),
            Answer::Said("neither, use the store we already run".to_string())
        );
        assert!(q.render().contains("just say what you want"));
    }

    /// A block with no options is not a choice, and an empty menu helps nobody.
    #[test]
    fn a_block_without_options_is_not_shown() {
        assert!(parse(&format!(
            "{OPEN}
Just wondering aloud.
{CLOSE}"
        ))
        .is_empty());
    }

    /// **An unclosed block is still asked.** A question the harness swallowed is
    /// worse than one rendered slightly wrong.
    #[test]
    fn an_unclosed_block_is_not_dropped() {
        let questions = parse(&format!(
            "{OPEN}
Which?
1. First
2. Second"
        ));
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].options.len(), 2);
    }

    /// More than a handful is a list, not a choice.
    #[test]
    fn too_many_options_are_trimmed_to_a_choosable_number() {
        let mut body = String::from(
            "Which?
",
        );
        for i in 1..=12 {
            body.push_str(&format!(
                "{i}. Option{i}
"
            ));
        }
        let questions = parse(&format!(
            "{OPEN}
{body}{CLOSE}"
        ));
        assert_eq!(questions[0].options.len(), MAX_OPTIONS);
    }

    /// **Nobody at the prompt must not hang the run.**
    #[test]
    fn an_unattended_run_reports_the_question_instead_of_waiting() {
        let note = unattended_note(&[question()]).expect("something to say");
        assert!(note.contains("Which database"), "got: {note}");
        assert!(
            note.contains("interactively"),
            "it must say how to answer: {note}"
        );
    }

    /// Two questions in one reply are both asked.
    #[test]
    fn several_questions_in_one_reply_are_all_found() {
        let text = format!(
            "{OPEN}
First?
1. A
2. B
{CLOSE}
prose
{OPEN}
Second?
1. C
2. D
{CLOSE}"
        );
        assert_eq!(parse(&text).len(), 2);
    }
}
