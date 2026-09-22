//! What the run looks like right now, assembled from what the harness already
//! wrote down.
//!
//! # No new source of truth
//!
//! Every field below is READ from something the run already produces: the
//! ledger for what the work is and whether it is proved, and the per-task
//! transcripts in [`crate::transcript::DIR`] for who is doing it and what they
//! last did. Nothing here is stored, and nothing here is asked of an agent.
//!
//! That is the whole design. A second store — a status the runner pushes to,
//! kept beside the ledger — would drift from the ledger the first time a run
//! was killed mid-task, and the copy being read would be the stale one. This
//! cannot drift, because there is only one copy.
//!
//! # Derived, never guessed
//!
//! A worker's state comes from the LAST line it wrote and how long ago it wrote
//! it. `writing` is decided by [`crate::cli::edits::change_from`] — the same
//! shape-based parser the edit view uses — so an agent whose editing tool has a
//! name nobody has seen still reads as writing. A second rule here that decided
//! the same thing differently is exactly the drift this harness keeps finding.

use serde::{Deserialize, Serialize};

/// How long after its last line a worker is called idle rather than thinking.
///
/// A turn can be genuinely quiet for a while — a large read, a slow model — so
/// this is generous. It is the difference between "still going" and "nothing is
/// happening here", which is the question the left column answers.
pub const STALE_AFTER_SECS: i64 = 90;

/// What a worker is doing, in the words the interface already uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkState {
    /// Nothing recent. Not an error — a finished or waiting agent looks like
    /// this, and so does one whose run was stopped.
    Idle,
    Thinking,
    Writing,
    /// Running a tool that is not a write — a search, a check, a command.
    Running,
    /// Its last act failed, or the ledger is waiting on a person.
    Blocked,
    Done,
}

/// One agent, as the interface needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worker {
    /// The instance id minted for this agent — `agent-9ac96a08d0f8`.
    ///
    /// The identity, not the model: a fallback can change the model underneath
    /// one agent mid-task, and keying on the model would split one worker's
    /// history in two exactly when a handover happened.
    pub id: String,
    /// `claude-code:implement`, from the ledger's attribution.
    pub name: String,
    /// What it was actually billed for, or empty when nothing said.
    pub model: String,
    pub task: String,
    pub title: String,
    pub state: WorkState,
    /// The line under the blob — `writing · game.js`.
    pub status: String,
    /// When it last did anything, as it was written down.
    pub at: String,
}

/// One roadmap item and how much of it is proved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub title: String,
    /// Tasks PROVED finished — never tasks an agent said were finished.
    pub done: usize,
    pub total: usize,
    /// Something here needs a person: a task with no check, or one set aside.
    pub blocked: bool,
}

/// The whole picture, in one reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    pub workspace: String,
    pub workers: Vec<Worker>,
    pub roadmap: Vec<Item>,
    /// When this was assembled, so a client can tell a stale reply from a quiet
    /// run.
    pub at: String,
}

/// The fields of a transcript line this cares about.
///
/// Deliberately partial: the transcript carries more, and a reader that
/// insisted on every field would break the first time a new event kind was
/// recorded. Unknown kinds fall through to [`WorkState::Thinking`], which is
/// the honest answer for "it did something we do not recognise".
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Line {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub at: String,
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub input: String,
    #[serde(default)]
    pub is_error: bool,
}

/// The state and status one line implies, before the ledger has its say.
///
/// `seconds_ago` is how stale the line is. Passed in rather than measured here
/// so the rule can be tested without waiting.
#[must_use]
pub fn from_line(line: &Line, seconds_ago: i64) -> (WorkState, String) {
    if seconds_ago > STALE_AFTER_SECS {
        return (WorkState::Idle, "waiting".to_string());
    }
    match line.kind.as_str() {
        // A failed tool is the clearest signal there is that an agent is not
        // getting on with it.
        "tool_result" if line.is_error => (WorkState::Blocked, "last step failed".to_string()),
        "tool_result" => (WorkState::Thinking, "reading the result".to_string()),
        "tool_use" => {
            // THE SAME PARSER THE EDIT VIEW USES. A second rule for "is this a
            // write" would be the one that goes stale.
            if let Some(change) = crate::cli::edits::change_from(&line.input) {
                return (WorkState::Writing, format!("writing · {}", change.path));
            }
            match crate::cli::panel::target(&line.input) {
                Some(on) => (WorkState::Running, format!("{} · {on}", line.tool)),
                None => (WorkState::Running, line.tool.clone()),
            }
        }
        "briefed" => (WorkState::Thinking, "getting oriented".to_string()),
        _ => (WorkState::Thinking, "thinking".to_string()),
    }
}

/// The last line of a transcript, or `None` when there is nothing readable.
///
/// Reads the whole file rather than seeking: a transcript is one task's
/// activity and the largest measured is well under a megabyte. Seeking to the
/// tail would be faster and would have to handle a partial line at the end —
/// this is called on a file another process is APPENDING to, so the last line
/// can be half-written, and a half-written line must be skipped rather than
/// parsed into a wrong answer.
#[must_use]
pub fn last_line(text: &str) -> Option<Line> {
    text.lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Line>(line).ok())
}

/// Seconds between two RFC3339 stamps, or 0 if either cannot be read.
///
/// Zero rather than "very old" on an unreadable stamp: an unparseable time is a
/// bug in the writer, and reporting every worker idle because of it would hide
/// a running agent. Erring towards "recent" keeps them visible.
#[must_use]
pub fn seconds_between(then: &str, now: &str) -> i64 {
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    match (parse(then), parse(now)) {
        (Some(a), Some(b)) => (b - a).num_seconds().max(0),
        _ => 0,
    }
}

/// The name and model an attribution line carries.
///
/// `claude-code:implement (1a534c20) [agent-9ac96a08d0f8] on claude-sonnet-5`
/// splits into the doer and what served it. Parsed leniently: an attribution
/// this cannot read yields the whole string as the name, which is still more
/// use to a reader than an empty cell.
#[must_use]
pub fn attributed(worked_by: &str) -> (String, String) {
    let name = worked_by
        .split_whitespace()
        .next()
        .unwrap_or(worked_by)
        .to_string();
    let model = worked_by
        .rsplit_once(" on ")
        .map(|(_, m)| m.trim().to_string())
        .unwrap_or_default();
    (name, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: &str) -> Line {
        Line {
            kind: kind.to_string(),
            ..Line::default()
        }
    }

    /// **An edit reads as `writing`, and names the file.**
    ///
    /// Decided by the edit view's own parser, so this holds for any agent whose
    /// editing tool carries a path and a splice — including ones nobody has
    /// written yet. A tool-name match would answer this for one vendor.
    #[test]
    fn an_agent_making_a_change_is_writing_and_says_what() {
        let mut l = line("tool_use");
        l.tool = "SomeVendorEdit".to_string();
        l.input = r#"{"file_path":"game.js","old_string":"a","new_string":"b"}"#.to_string();

        let (state, status) = from_line(&l, 1);
        assert_eq!(state, WorkState::Writing);
        assert_eq!(status, "writing · game.js");
    }

    /// A tool that is not a write is `running`, and still names its subject.
    #[test]
    fn a_tool_that_changes_nothing_is_running_not_writing() {
        let mut l = line("tool_use");
        l.tool = "Bash".to_string();
        l.input = r#"{"command":"node --check game.js"}"#.to_string();

        let (state, status) = from_line(&l, 1);
        assert_eq!(state, WorkState::Running);
        assert!(status.starts_with("Bash · "), "{status}");
    }

    /// **A failed step is blocked, not thinking.**
    ///
    /// The distinction the left column exists for: an agent that is quietly
    /// working and one that just failed look identical in a spinner.
    #[test]
    fn a_failed_step_reads_as_blocked() {
        let mut l = line("tool_result");
        l.is_error = true;
        assert_eq!(from_line(&l, 1).0, WorkState::Blocked);

        // Control: the same event that SUCCEEDED is not blocked.
        l.is_error = false;
        assert_eq!(from_line(&l, 1).0, WorkState::Thinking);
    }

    /// **Silence becomes idle, whatever it was doing.**
    ///
    /// A run that was killed leaves its last line saying `tool_use`, and
    /// without this the interface shows an agent writing a file forever.
    #[test]
    fn an_agent_that_has_gone_quiet_stops_reading_as_busy() {
        let mut l = line("tool_use");
        l.input = r#"{"file_path":"game.js","old_string":"a","new_string":"b"}"#.to_string();

        assert_eq!(from_line(&l, 1).0, WorkState::Writing, "recent: busy");
        assert_eq!(
            from_line(&l, STALE_AFTER_SECS + 1).0,
            WorkState::Idle,
            "the same line, long enough ago, is not someone at work"
        );
    }

    /// An unknown event kind is thinking, not an error.
    #[test]
    fn an_unrecognised_event_does_not_break_the_view() {
        assert_eq!(from_line(&line("something_new"), 1).0, WorkState::Thinking);
        assert_eq!(from_line(&line(""), 1).0, WorkState::Thinking);
    }

    /// **A half-written last line is skipped, not parsed.**
    ///
    /// This reads a file another process is appending to, so the tail can be a
    /// fragment. Taking it would report the previous state as unknown at
    /// exactly the moment the agent is most active.
    #[test]
    fn a_torn_final_line_falls_back_to_the_last_whole_one() {
        let text = concat!(
            r#"{"kind":"said","agent":"agent-1","at":"2026-09-05T10:00:00.000Z"}"#,
            "\n",
            r#"{"kind":"tool_use","agent":"agent-1","at":"2026-09-05T10:00:01.000Z"}"#,
            "\n",
            r#"{"kind":"tool_re"#,
        );

        let last = last_line(text).expect("the last WHOLE line is readable");
        assert_eq!(last.kind, "tool_use", "the fragment must not win");
    }

    /// Nothing readable at all is `None`, not a default that looks like work.
    #[test]
    fn a_transcript_with_no_readable_line_yields_nothing() {
        assert!(last_line("").is_none());
        assert!(last_line("not json\nalso not json").is_none());
    }

    /// The attribution line splits into who did it and what served it.
    #[test]
    fn an_attribution_gives_the_doer_and_the_model() {
        let (name, model) =
            attributed("claude-code:implement (1a534c20) [agent-9ac96a08d0f8] on claude-sonnet-5");
        assert_eq!(name, "claude-code:implement");
        assert_eq!(model, "claude-sonnet-5");

        // An attribution with no model still names the doer.
        let (name, model) = attributed("agy:implement (2595b0d6)");
        assert_eq!(name, "agy:implement");
        assert_eq!(model, "");

        // And an empty one says nothing rather than inventing.
        assert_eq!(attributed(""), (String::new(), String::new()));
    }

    /// Ages are measured, and an unreadable stamp keeps a worker VISIBLE.
    #[test]
    fn an_unreadable_timestamp_does_not_hide_a_working_agent() {
        assert_eq!(
            seconds_between("2026-09-05T10:00:00.000Z", "2026-09-05T10:00:30.000Z"),
            30
        );
        // Clock skew must not produce a negative age.
        assert_eq!(
            seconds_between("2026-09-05T10:00:30.000Z", "2026-09-05T10:00:00.000Z"),
            0
        );
        // Unreadable: 0, which reads as "just now" — see the doc note.
        assert_eq!(seconds_between("not a time", "2026-09-05T10:00:00.000Z"), 0);
    }
}
