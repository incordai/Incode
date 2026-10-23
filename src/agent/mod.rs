//! Agent backends — drive an external coding agent that owns its own loop.
//!
//! This sits at a different altitude from [`api`]. A model reached over an HTTP
//! API is just that — a *model*: it returns tokens and this harness owns the agentic
//! loop, the tools, permissions and sessions. An [`AgentBackend`] is a *complete
//! agent*: it owns the loop and brings its own tools, and the harness feeds it
//! turns and consumes its event stream.
//!
//! Two harnesses cannot both own the loop, which is why an agent cannot be
//! plugged into the `ApiClient` slot. Keeping them as separate traits is what
//! lets one binary talk to a model over an API key and to an already-installed,
//! already-authenticated agent CLI over its own subscription.
//!
//! The harness never reads, copies or transmits an agent's credentials. It
//! detects that a binary exists and lets the agent authenticate itself.
//!
//! # Layout
//!
//! This file holds what a turn produces ([`AgentEvent`], [`TurnOutcome`],
//! [`AgentError`]). The trait every route implements is in `backend.rs`,
//! whether a failure moves the work to another agent in `handover.rs`, and
//! the command-line budget for a prompt in `spawn_budget.rs` — each
//! re-exported here, so callers name them as `crate::agent::…`.

pub mod agy;
pub mod api;
mod backend;
pub mod catalogue;
pub mod claude_code;
pub mod codex;
pub mod compact;
mod handover;
pub mod models;
pub mod openai;
pub mod parent_session;
pub mod process_tree;
pub mod registry;
mod spawn_budget;
pub mod stderr_tail;
pub mod worker;

use std::fmt::{Display, Formatter};

pub use backend::AgentBackend;
pub use handover::{is_limit_failure, looks_like_auth_failure, moves_the_work};
pub use registry::{discover, DetectedAgent, Protocol};
pub use spawn_budget::{
    budget_for_spawn, MAX_CMDLINE_PAYLOAD_LEN, MAX_PROMPT_BUDGET, MIN_CONTEXT_BUDGET,
};

/// An event produced by an agent backend while running one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// The agent opened (or resumed) a session.
    ///
    /// `model` is the model the session ASKED FOR, not the one that served it.
    /// Measured: a turn requesting a nonexistent model reported that name back
    /// verbatim here, while the fallback quietly answered it. Anything that
    /// needs to know what actually ran must read the usage record instead —
    /// treating this as fact is how a fallback goes unnoticed.
    SessionStarted { session_id: String, model: String },
    /// User-facing assistant text, as one whole block.
    ///
    /// Always emitted. [`Self::TextDelta`] is emitted BESIDE it when partial
    /// messages are switched on, so a consumer must choose one and ignore the
    /// other — using both prints every reply twice.
    Text(String),
    /// A fragment of assistant text, as it is generated.
    ///
    /// Only emitted when the backend was asked for partial messages. This is
    /// what lets an interactive session show a reply arriving rather than
    /// appearing all at once, and it costs nothing when nobody asks for it.
    TextDelta(String),
    /// A fragment of the model's reasoning, as it is generated.
    ///
    /// Only from a backend that streams it — the claude CLI with partial
    /// messages on, which then sends a summary of the model's thinking
    /// (`--thinking-display summarized`). Never the reply: a front end shows
    /// it apart, and the transcript and the stop hook do not hear it.
    Thinking(String),
    /// The agent invoked one of its own tools.
    /// What a tool actually returned.
    ///
    /// The agent has always sent these — as `tool_result` blocks inside its
    /// `user` messages — and this harness never parsed them, so a turn showed
    /// which tools RAN and never what any of them said. Eight of these went
    /// unread in a single measured turn.
    ToolResult {
        /// Ties the result back to the [`Self::ToolUse`] that asked for it.
        id: String,
        output: String,
        is_error: bool,
    },
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    /// Subscription quota status. Reported instead of a dollar figure, because
    /// on a subscription no per-token money is being spent.
    Quota {
        status: String,
        kind: String,
        resets_at: i64,
        using_overage: bool,
    },
    /// Cumulative token usage for the turn.
    Usage {
        input_tokens: u32,
        output_tokens: u32,
        cache_creation_tokens: u32,
        cache_read_tokens: u32,
    },
    /// How full the agent's context window is, when the agent reports BOTH
    /// numbers: the tokens its latest request held (`used`) and the window
    /// that request ran in (`size`).
    ///
    /// Emitted only when both were reported. A backend that reports token
    /// counts and no window sends [`Self::Usage`] alone; nothing here fills
    /// in a window from a table, because a guessed window is a wrong
    /// percentage on the owner's screen.
    Context { used: u64, size: u64 },
    /// incode itself speaking during a turn, not the model — what the
    /// operator's `pre_compact` commands printed before the API route
    /// compacted on its own. Every front end shows it apart from the reply;
    /// the transcript and the stop hook do not hear it.
    Notice(String),
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    pub session_id: String,
    pub stop_reason: String,
    pub is_error: bool,
    /// What the agent said about a FAILED turn, when it said anything.
    ///
    /// `None` on success, and on a failure the agent described only by its
    /// stop reason. An error message that names the stop reason and nothing
    /// else tells the operator that something went wrong and refuses to say
    /// what — which is a bug in the message, not a limitation of the agent.
    pub detail: Option<String>,
    /// The models actually BILLED for this turn, when the backend reports them.
    ///
    /// Empty means "not reported", never "none" — a backend that cannot say
    /// which model ran leaves this empty and the record says so, rather than
    /// filling in the model that was asked for. See [`AgentEvent::SessionStarted`].
    #[allow(clippy::struct_field_names)]
    pub served_by: Vec<String>,
}

impl TurnOutcome {
    /// Why the turn failed, in as much detail as the agent gave.
    ///
    /// The stop reason alone is a category, not an explanation: `stop_sequence`
    /// says the turn ended and refuses to say why, which sent a whole day into
    /// guessing. Whatever the agent actually wrote goes first, because that is
    /// the part a person can act on.
    #[must_use]
    pub fn failure(&self) -> String {
        match self
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            // An expired login is not a turn that went wrong; it is a session
            // that was never able to start, and it has a known remedy. Reported
            // as an ordinary stop reason it cost a full day of hunting for a
            // bug in the harness that was never there.
            Some(detail) if looks_like_auth_failure(detail) => format!(
                "the agent could not sign in: {detail}
  Log in again with the                  agent's own CLI, then retry. If you keep more than one agent                  profile, check that this shell points at the one you logged in                  with — a shell without that setting falls back to the default                  profile, which may hold an expired session."
            ),
            Some(detail) => {
                format!(
                    "the agent stopped without finishing ({}): {detail}",
                    self.stop_reason
                )
            }
            None => format!(
                "the agent stopped without finishing: {} (the agent gave no reason)",
                self.stop_reason
            ),
        }
    }
}

/// The one quota status that means work can proceed.
///
/// Captured from a real turn. The spelling of an EXHAUSTED status has not been
/// observed, so [`is_limit_status`] tests for "not this" rather than matching a
/// guessed value — a guard that only fires on a string nobody has seen is a
/// guard that never fires.
pub const QUOTA_ALLOWED: &str = "allowed";

/// Whether a quota status means the subscription is out of room.
#[must_use]
pub fn is_limit_status(status: &str) -> bool {
    !status.trim().is_empty() && !status.eq_ignore_ascii_case(QUOTA_ALLOWED)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentError {
    message: String,
}

impl AgentError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for AgentError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AgentError {}

#[cfg(test)]
mod budget_pinned_tests;

#[cfg(test)]
mod outcome_tests {
    use super::TurnOutcome;

    fn failed_with(detail: Option<&str>) -> TurnOutcome {
        TurnOutcome {
            session_id: "s".into(),
            stop_reason: "stop_sequence".into(),
            is_error: true,
            detail: detail.map(str::to_string),
            served_by: Vec::new(),
        }
    }

    #[test]
    fn a_failure_message_says_what_the_agent_said() {
        // The whole point: `stop_sequence` alone is a category, not a reason.
        let said = failed_with(Some("error_during_execution: tool use was interrupted")).failure();
        assert!(said.contains("stop_sequence"), "keeps the category: {said}");
        assert!(
            said.contains("tool use was interrupted"),
            "and carries what the agent actually said: {said}"
        );
    }

    #[test]
    fn an_expired_login_is_reported_as_an_expired_login() {
        // The exact string the agent sent, which the harness spent a day
        // reporting as `stop_sequence` and nothing else.
        let said = failed_with(Some(
            "success: Failed to authenticate: OAuth session expired and could not be refreshed",
        ))
        .failure();
        assert!(
            said.contains("could not sign in"),
            "names the real problem: {said}"
        );
        assert!(said.contains("Log in again"), "and says what to do: {said}");
        assert!(
            !said.contains("stopped without finishing"),
            "an expired login is not a turn that went wrong: {said}"
        );
    }

    #[test]
    fn an_ordinary_failure_is_not_mistaken_for_a_login_problem() {
        // The control: if everything matched, the remedy would be printed for
        // failures it cannot possibly fix.
        let said = failed_with(Some("error_during_execution: the tool timed out")).failure();
        assert!(said.contains("stopped without finishing"), "{said}");
        assert!(!said.contains("Log in again"), "{said}");
    }

    #[test]
    fn a_failure_with_nothing_said_admits_it() {
        // Printing the bare reason is what made this undiagnosable for a day.
        // The message must say the agent gave nothing, not imply it explained.
        assert!(failed_with(None).failure().contains("gave no reason"));
    }

    #[test]
    fn whitespace_is_not_a_reason() {
        assert!(failed_with(Some("   "))
            .failure()
            .contains("gave no reason"));
    }
}
