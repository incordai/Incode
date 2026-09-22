//! What the agent is doing right now.
//!
//! # The problem
//!
//! A delegated turn can run for minutes. During it the agent reads twenty
//! files, greps the tree, runs the test suite and edits three modules — and the
//! person watching sees one word: *Thinking*. There is no way to tell work from
//! a hang, and no way to tell reading from deploying.
//!
//! The events were always there. [`crate::agent::AgentEvent::ToolUse`] has been
//! parsed since the first turn this harness ever drove, and the interactive path
//! threw it away.
//!
//! # Why a verb and not the tool name
//!
//! `Bash`, `Glob`, `Grep`, `Read` are the agent's vocabulary, not the person's.
//! "searching" and "running the tests" say what is happening; `Grep` and `Bash`
//! ask the reader to translate.
//!
//! **`Bash` is the interesting one.** The tool name says nothing — the COMMAND
//! says everything. `cargo test` is testing, `git push` is pushing, `docker
//! compose up` is deploying. Reporting all three as "running a command" throws
//! away the only part that mattered.

use std::borrow::Cow;

/// What the agent is doing, in words a person reads.
///
/// # Why this owns its text
///
/// It held a `&'static str`, so the only things it could say were the ones
/// written into this file. Every other tool — every `mcp__*` call, every tool
/// added by a plugin — fell through to "Thinking", which is how a turn making
/// thirteen tool calls sat on `Thinking` for four and a half minutes while the
/// comment below the fallback claimed unknown tools were "named as themselves".
/// They could not be. Now they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity(Cow<'static, str>);

impl Activity {
    /// The word shown while nothing more specific is known.
    pub const THINKING: Self = Self(Cow::Borrowed("Thinking"));

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// What a tool call means, in a word.
    ///
    /// `input` matters only for the shell: everything else is decided by the
    /// tool's name alone.
    #[must_use]
    pub fn from_tool(name: &str, input: &str) -> Self {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "bash" | "shell" | "run" => Self::from_command(input),
            "read" | "notebookread" => Self(Cow::Borrowed("Reading")),
            "glob" | "grep" | "search" | "ls" => Self(Cow::Borrowed("Searching")),
            "edit" | "write" | "notebookedit" | "multiedit" | "str_replace" => {
                Self(Cow::Borrowed("Editing"))
            }
            "webfetch" | "websearch" | "fetch" => Self(Cow::Borrowed("Researching")),
            "task" | "agent" | "dispatch" => Self(Cow::Borrowed("Delegating to another agent")),
            "todowrite" | "todoread" => Self(Cow::Borrowed("Planning")),
            // An unknown tool is named as itself rather than guessed at. A
            // wrong verb is worse than an unfamiliar noun: the reader trusts it.
            _ => Self(Cow::Owned(readable_tool_name(name))),
        }
    }

    /// What a shell command is for.
    ///
    /// Ordered so the most specific wins: `cargo test` is testing before it is
    /// building, and `git push` is pushing before it is git.
    #[must_use]
    pub fn from_command(command: &str) -> Self {
        let c = command.to_ascii_lowercase();

        // Deploying is checked first because it is the one nobody wants to
        // discover after the fact.
        if contains_any(
            &c,
            &[
                "deploy",
                "kubectl",
                "helm",
                "terraform",
                "docker compose up",
            ],
        ) {
            return Self(Cow::Borrowed("Deploying"));
        }
        if contains_any(
            &c,
            &["git push", "npm publish", "cargo publish", "rsync", "scp "],
        ) {
            return Self(Cow::Borrowed("Uploading"));
        }
        if contains_any(
            &c,
            &["test", "pytest", "jest", "vitest", "phpunit", "go test"],
        ) {
            return Self(Cow::Borrowed("Running tests"));
        }
        if contains_any(&c, &["build", "compile", "webpack", "tsc ", "make "]) {
            return Self(Cow::Borrowed("Building"));
        }
        if contains_any(
            &c,
            &["install", "npm i ", "pip install", "cargo add", "apt "],
        ) {
            return Self(Cow::Borrowed("Installing"));
        }
        if contains_any(&c, &["lint", "clippy", "eslint", "fmt", "prettier"]) {
            return Self(Cow::Borrowed("Checking style"));
        }
        if contains_any(&c, &["migrate", "migration"]) {
            return Self(Cow::Borrowed("Migrating"));
        }
        if contains_any(&c, &["git commit", "git add", "git rebase", "git merge"]) {
            return Self(Cow::Borrowed("Committing"));
        }
        if contains_any(&c, &["curl", "wget", "http"]) {
            return Self(Cow::Borrowed("Fetching"));
        }
        if contains_any(&c, &["grep", "find ", "rg ", "ls "]) {
            return Self(Cow::Borrowed("Searching"));
        }
        if contains_any(&c, &["cat ", "head ", "tail ", "less "]) {
            return Self(Cow::Borrowed("Reading"));
        }
        Self(Cow::Borrowed("Running a command"))
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// A one-line status: what is happening, for how long, and what it has cost.
///
/// # Why one line and not a panel
///
/// The reply is streaming into the same terminal. A status that scrolled with
/// it would bury the answer under its own history; a status pinned in a corner
/// would need to own the screen. One line, rewritten in place, sits under the
/// A tool's own name, made readable.
///
/// Keeps the promise the fallback comment makes: an unfamiliar tool is NAMED,
/// never dressed up in a guessed verb. `mcp__memory__recall_index` becomes
/// "Recall index", `ToolSearch` becomes "Tool search". The reader learns what
/// ran; they are not told the agent was thinking when it was not.
#[must_use]
pub fn readable_tool_name(name: &str) -> String {
    // MCP tools arrive namespaced — `mcp__<server>__<tool>`. The server is
    // plumbing; the tool is the part that says what happened.
    let bare = name.rsplit("__").next().unwrap_or(name);
    let mut words = String::new();
    let mut prev_lower = false;
    for ch in bare.chars() {
        if ch == '_' || ch == '-' {
            if !words.ends_with(' ') && !words.is_empty() {
                words.push(' ');
            }
            prev_lower = false;
            continue;
        }
        // A capital after a lower-case letter starts a new word, so `ToolSearch`
        // reads as two words rather than one shout.
        if ch.is_uppercase() && prev_lower {
            words.push(' ');
        }
        prev_lower = ch.is_lowercase() || ch.is_numeric();
        words.push(ch);
    }
    let trimmed = words.trim();
    if trimmed.is_empty() {
        return Activity::THINKING.as_str().to_string();
    }
    // Sentence case: the first letter up, the rest as written, so an acronym
    // inside the name is not flattened.
    let mut chars = trimmed.chars();
    let first = chars
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    let rest: String = chars.collect();
    format!("{first}{}", rest.to_lowercase())
}

/// output and disappears when the turn ends.
#[derive(Debug, Clone)]
pub struct Status {
    pub activity: Activity,
    /// Seconds since the turn began.
    pub elapsed: u64,
    /// Tokens the agent has reported for this turn.
    pub tokens: u64,
    /// Tasks finished and planned, when there is a plan to measure against.
    ///
    /// `None` for a conversation: an interactive turn has no denominator, and a
    /// bar that invented one would be a number the reader could not use. A
    /// progress bar is only honest where something is actually being counted.
    pub progress: Option<(usize, usize)>,
}

impl Status {
    /// The status line, fitted to a terminal of this width.
    #[must_use]
    pub fn render(&self, frame: char, width: usize) -> String {
        let mut left = format!("{frame} {}", self.activity.as_str());
        if self.elapsed > 0 {
            left.push_str(&format!(" · {}", human_duration(self.elapsed)));
        }
        if let Some((done, total)) = self.progress {
            if total > 0 {
                left.push_str(&format!(" · {}", bar(done, total)));
            }
        }

        let right = if self.tokens > 0 {
            format!("{} tokens", human_count(self.tokens))
        } else {
            String::new()
        };

        // Right-aligned to the terminal edge, which is where a number that is
        // not being read belongs. Too narrow to fit both, the activity wins:
        // what is happening matters more than what it cost.
        let gap = width.saturating_sub(left.chars().count() + right.chars().count());
        if right.is_empty() || gap < 2 {
            return left;
        }
        format!("{left}{}{right}", " ".repeat(gap))
    }
}

/// A small bar, in characters that exist in every terminal font.
#[must_use]
pub fn bar(done: usize, total: usize) -> String {
    const WIDTH: usize = 10;
    if total == 0 {
        return String::new();
    }
    let filled = (done * WIDTH) / total.max(1);
    let filled = filled.min(WIDTH);
    format!(
        "[{}{}] {done}/{total}",
        "=".repeat(filled),
        " ".repeat(WIDTH - filled)
    )
}

/// `1m 20s`, not `80`.
#[must_use]
pub fn human_duration(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    format!("{}m {}s", seconds / 60, seconds % 60)
}

/// `12.4k`, not `12400` — the digits past the first three are noise here.
#[must_use]
pub fn human_count(count: u64) -> String {
    if count < 1_000 {
        return count.to_string();
    }
    if count < 1_000_000 {
        return format!("{:.1}k", count as f64 / 1_000.0);
    }
    format!("{:.1}M", count as f64 / 1_000_000.0)
}

/// How wide the terminal is, or a sensible default.
#[must_use]
pub fn terminal_width() -> usize {
    crossterm::terminal::size().map_or(80, |(cols, _)| cols as usize)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// **The shell's tool name says nothing; the command says everything.**
    /// Reporting `cargo test`, `git push` and `docker compose up` all as
    /// "running a command" throws away the only part that mattered.
    #[test]
    fn a_shell_command_is_read_for_what_it_actually_does() {
        for (command, expected) in [
            ("cargo test --all", "Running tests"),
            ("npm run build", "Building"),
            ("git push origin main", "Uploading"),
            ("kubectl apply -f k8s/", "Deploying"),
            ("pip install requests", "Installing"),
            ("cargo clippy -- -D warnings", "Checking style"),
            ("git commit -m 'x'", "Committing"),
            ("grep -rn thing src/", "Searching"),
        ] {
            assert_eq!(
                Activity::from_tool("Bash", command).as_str(),
                expected,
                "failed on: {command}"
            );
        }
    }

    /// **Deploying is checked before everything else.** It is the one nobody
    /// wants to discover after the fact, so a command that both builds and
    /// deploys is reported as deploying.
    #[test]
    fn deploying_wins_over_the_build_that_is_part_of_it() {
        assert_eq!(
            Activity::from_tool("Bash", "npm run build && npm run deploy").as_str(),
            "Deploying"
        );
    }

    /// Tool names become verbs a person reads without translating.
    #[test]
    fn tool_names_become_words_a_person_reads() {
        for (tool, expected) in [
            ("Read", "Reading"),
            ("Grep", "Searching"),
            ("Edit", "Editing"),
            ("WebSearch", "Researching"),
            ("Task", "Delegating to another agent"),
        ] {
            assert_eq!(Activity::from_tool(tool, "").as_str(), expected, "{tool}");
        }
    }

    /// **An unknown tool falls back rather than guessing.** A wrong verb is
    /// worse than a vague one, because the reader believes it.
    #[test]
    fn an_unfamiliar_tool_does_not_invent_a_verb() {
        // The requirement this test's NAME states — no invented verb — is met
        // by naming the tool. Its old assertion pinned `Thinking`, which is the
        // opposite of what the fallback's own comment promises and is how a
        // turn making thirteen tool calls read as four minutes of thought.
        let said = Activity::from_tool("SomeFutureTool", "");
        assert_ne!(said, Activity::THINKING, "a tool ran; thought did not");
        assert_eq!(said.as_str(), "Some future tool");
        for invented in ["Reading", "Editing", "Searching", "Running a command"] {
            assert_ne!(
                said.as_str(),
                invented,
                "guessing `{invented}` for an unknown tool would be a lie"
            );
        }
    }

    /// **The tools that actually ran in a real turn.** Every one of these
    /// rendered as "Thinking" before, which is most of a working turn.
    #[test]
    fn the_tools_a_real_turn_used_are_named_not_hidden() {
        let seen = [
            ("mcp__memory__recall_index", "Recall index"),
            ("mcp__memory__watchman_handoff", "Watchman handoff"),
            ("ToolSearch", "Tool search"),
        ];
        for (tool, expected) in seen {
            let said = Activity::from_tool(tool, "");
            assert_eq!(said.as_str(), expected, "for {tool}");
            assert_ne!(said, Activity::THINKING, "{tool} is not thinking");
        }
    }

    #[test]
    fn a_nameless_tool_falls_back_rather_than_showing_nothing() {
        // An empty status line says less than an honest "Thinking".
        assert_eq!(Activity::from_tool("__", ""), Activity::THINKING);
        assert_eq!(Activity::from_tool("", ""), Activity::THINKING);
    }

    #[test]
    fn the_known_verbs_are_untouched() {
        // The control: if the new branch swallowed everything, the named tools
        // would start reporting their own names instead of what they mean.
        assert_eq!(Activity::from_tool("Read", "").as_str(), "Reading");
        assert_eq!(Activity::from_tool("Edit", "").as_str(), "Editing");
        assert_eq!(
            Activity::from_tool("Bash", "cargo test").as_str(),
            "Running tests"
        );
    }

    /// **A progress bar is only honest where something is counted.** An
    /// interactive turn has no denominator, and an invented percentage is a
    /// number the reader cannot use.
    #[test]
    fn a_conversation_shows_no_progress_bar() {
        let status = Status {
            activity: Activity::THINKING,
            elapsed: 5,
            tokens: 1_200,
            progress: None,
        };
        let line = status.render('|', 80);
        assert!(!line.contains('['), "no bar without a denominator: {line}");
        assert!(line.contains("1.2k tokens"), "got: {line}");
    }

    /// Where there IS a plan, the bar counts real tasks.
    #[test]
    fn a_run_shows_how_much_of_the_plan_is_done() {
        let status = Status {
            activity: Activity::from_tool("Bash", "cargo test"),
            elapsed: 65,
            tokens: 0,
            progress: Some((3, 10)),
        };
        let line = status.render('|', 80);
        assert!(line.contains("Running tests"), "got: {line}");
        assert!(line.contains("1m 5s"), "got: {line}");
        assert!(line.contains("3/10"), "got: {line}");
    }

    /// **Too narrow to fit both, the activity wins.** What is happening matters
    /// more than what it cost, and a truncated line that lost the verb would be
    /// worse than one with no number.
    #[test]
    fn a_narrow_terminal_keeps_the_activity_and_drops_the_count() {
        let status = Status {
            activity: Activity(Cow::Borrowed("Delegating to another agent")),
            elapsed: 0,
            tokens: 50_000,
            progress: None,
        };
        let line = status.render('|', 30);
        assert!(line.contains("Delegating"), "got: {line}");
        assert!(!line.contains("tokens"), "the count must give way: {line}");
    }

    /// The count is readable at a glance rather than exact.
    #[test]
    fn token_counts_are_rounded_to_something_readable() {
        assert_eq!(human_count(999), "999");
        assert_eq!(human_count(12_400), "12.4k");
        assert_eq!(human_count(2_500_000), "2.5M");
    }

    /// A full bar never overflows its own width.
    #[test]
    fn the_bar_never_exceeds_its_width() {
        assert!(bar(10, 10).contains("[=========="));
        // More done than planned is possible if a plan shrinks; it must not panic
        // or draw past the end.
        assert!(bar(20, 10).contains("[=========="));
    }
}
