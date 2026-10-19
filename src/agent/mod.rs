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

pub mod agy;
pub mod api;
pub mod catalogue;
pub mod claude_code;
pub mod codex;
pub mod models;
pub mod openai;
pub mod parent_session;
pub mod process_tree;
pub mod registry;
pub mod stderr_tail;
pub mod worker;

use std::fmt::{Display, Formatter};

pub use registry::{discover, DetectedAgent, Protocol};

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

/// Whether a failure is the agent being unable to authenticate.
///
/// Matched on the agent's own words rather than a status code, because the stop
/// reason does not distinguish it: the failure that started this arrived as
/// `stop_sequence` with the subtype `success`, and only the message said what
/// had actually happened.
#[must_use]
pub fn looks_like_auth_failure(detail: &str) -> bool {
    let said = detail.to_ascii_lowercase();
    said.contains("authenticate")
        || said.contains("unauthorized")
        || said.contains("oauth")
        || (said.contains("session") && said.contains("expired"))
        || said.contains("log in")
        || said.contains("login")
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

/// Whether a failed turn failed because the agent ran out of room, rather than
/// because the work was wrong.
///
/// The difference decides whether trying a DIFFERENT agent is sensible: a
/// subscription that is out of room will serve the same task fine tomorrow, or
/// on another subscription now. A task that is genuinely broken will fail
/// everywhere, and retrying it elsewhere just spends someone else's quota.
#[must_use]
pub fn is_limit_failure(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    let needles = [
        "rate limit",
        "rate_limit",
        "quota",
        "usage limit",
        "too many requests",
        "429",
        "overloaded",
        "capacity",
        "weekly limit",
        "monthly limit",
        "daily limit",
        "hourly limit",
        "limit reached",
        "limit exceeded",
        "reached your limit",
        "hit your limit",
        "exceeded your limit",
        "out of credits",
        "insufficient credits",
        "credit balance",
        "billing limit",
        "plan limit",
        "free tier limit",
        "spending limit",
        "token limit",
        "request limit",
    ];
    if needles.iter().any(|needle| reason.contains(needle)) {
        return true;
    }
    if reason.contains("limit")
        && (reason.contains("week")
            || reason.contains("month")
            || reason.contains("day")
            || reason.contains("hour")
            || reason.contains("credit")
            || reason.contains("reset")
            || reason.contains("exhaust")
            || reason.contains("reach")
            || reason.contains("exceed"))
    {
        return true;
    }
    false
}

/// Whether this failure is the AGENT's rather than the WORK's.
///
/// # Why a limit is not the only reason to move
///
/// This used to be [`is_limit_failure`] alone, on the reasoning that a task
/// which failed because it is wrong will fail the same way everywhere — true,
/// and still the reason the list below is narrow rather than "anything".
///
/// But it lumped a whole class in with the task's own faults. Measured: one
/// agent abandoned a task with `timeout waiting for response` at its own
/// deadline, having done real work; another rejected the turn outright with
/// `model sonnet is not recognized`. Neither says anything about the task, and
/// both would have succeeded on a different agent. The work stopped anyway,
/// with a standby sitting idle.
///
/// So: a failure that names the AGENT's own state moves the work. A failure
/// that names the WORK — a check that did not pass, a guard that refused, a
/// contract that was broken — does not, because carrying it to a second
/// subscription only spends that one too.
#[must_use]
pub fn moves_the_work(reason: &str) -> bool {
    if is_limit_failure(reason) {
        return true;
    }
    // An account that cannot sign in cannot do ANY work, and a second agent
    // authenticates separately — so this is the clearest case of all for
    // handing over. Reusing [`looks_like_auth_failure`] rather than copying its
    // needles: two lists of the same thing drift, and the one that drifts is
    // never the one being read.
    if looks_like_auth_failure(reason) {
        return true;
    }
    let reason = reason.to_ascii_lowercase();
    [
        // The agent ran out of time on its own clock, not the harness's.
        "timeout",
        "timed out",
        // The agent could not be started, spawned, or executed by the OS
        // (missing binary, permission denied, Windows command line length limit os error 206, etc.).
        "could not start",
        "not installed",
        "no output stream",
        "did not exit cleanly",
        "produced no result",
        "failed to spawn",
        "cannot spawn",
        "unable to spawn",
        "failed to execute",
        "spawn",
        "os error 206",
        "os error",
        "too long",
        "filename or extension",
        "command line too long",
        "createprocess",
        "executable file not found",
        // The agent refused the request shape itself.
        "not recognized",
        "invalid model",
        "unknown model",
        // The agent's own transport gave out.
        "connection",
        "broken pipe",
        // MEASURED, not guessed: a busy upstream model answered
        // `status: ERROR, error: The stream was interrupted. Please continue
        // the task you were working on.` — the agent's link to its provider
        // broke mid-turn. It says nothing about the task, and it is the most
        // common way a popular model fails, so a list without it leaves the
        // standby idle exactly when it is most needed.
        "stream was interrupted",
        "stream interrupted",
    ]
    .iter()
    .any(|needle| reason.contains(needle))
}

/// Maximum length of payload (prompt + system context) passed to external CLI
/// processes via command-line flags.
///
/// On Windows, Win32 `CreateProcessW` imposes a hard 32,767 character limit on
/// the entire command line (`lpCommandLine`). Exceeding this triggers OS error 206
/// (`ERROR_FILENAME_EXCED_RANGE: The filename or extension is too long`).
/// Keeping the combined payload at or below 28,000 characters leaves ample room
/// (~4,700 chars) for binary paths, flags, arguments, quotes, and whitespace.
#[cfg(windows)]
pub const MAX_CMDLINE_PAYLOAD_LEN: usize = 28_000;
#[cfg(not(windows))]
pub const MAX_CMDLINE_PAYLOAD_LEN: usize = 120_000;

/// Prompt budget ceiling when total payload exceeds [`MAX_CMDLINE_PAYLOAD_LEN`].
pub const MAX_PROMPT_BUDGET: usize = 12_000;

/// Minimum context budget preserved when budgeting payload.
pub const MIN_CONTEXT_BUDGET: usize = 4_000;

/// Compacts / budgets prompt and system context so external agent CLI processes
/// spawned via `Command::new` stay safely within OS command-line limits.
///
/// When the total payload exceeds [`MAX_CMDLINE_PAYLOAD_LEN`], a prompt over
/// [`MAX_PROMPT_BUDGET`] is written whole to a file in `workspace` and the
/// agent is told to read it ([`spill`]); nothing of it is dropped. Then
/// high-priority context blocks (guards, standing rules, intake, asking,
/// roles) are preserved, while large informational blocks (discovered skills
/// catalog, auditor findings, recorder handoff) are truncated to fit the
/// remaining budget. An error only when the spill file cannot be written:
/// the turn is not started on a prompt the agent would never see whole.
///
/// Called by the CLI backends that pass the prompt on a command line:
/// `agy.rs` and `codex.rs` (`full_prompt`) and `claude_code.rs` (`run_once`).
pub fn budget_for_spawn(
    prompt: &str,
    system_context: &[String],
    workspace: &std::path::Path,
) -> Result<(String, String), AgentError> {
    let unbudgeted_context = system_context.join("\n\n");
    let total_len = prompt.len() + unbudgeted_context.len();
    if total_len <= MAX_CMDLINE_PAYLOAD_LEN {
        return Ok((prompt.to_string(), unbudgeted_context));
    }

    // NO PART OF THE PROMPT IS DROPPED. It used to keep the first 2,000 and
    // the last ~9,900 bytes and cut the middle, on the theory that the
    // instruction is always short and last. Measured on Insi's lesson writer:
    // its instruction comes FIRST and its prompt runs to 20 KB, so the model
    // got the preamble's head and the tail of a diff, and answered by asking
    // what it was meant to do — four lessons of fifteen lost.
    let budgeted_prompt = if prompt.len() > MAX_PROMPT_BUDGET {
        spill(prompt, workspace)?
    } else {
        prompt.to_string()
    };

    let context_budget = MAX_CMDLINE_PAYLOAD_LEN
        .saturating_sub(budgeted_prompt.len())
        .max(MIN_CONTEXT_BUDGET);

    if unbudgeted_context.len() <= context_budget {
        return Ok((budgeted_prompt, unbudgeted_context));
    }

    // Partition blocks by priority:
    // High-priority: guards, standing rules, intake task, question prompt, roles
    // Low-priority: discovered skills index, auditor findings, recorder handoff
    let mut high_priority = Vec::new();
    let mut low_priority = Vec::new();

    for block in system_context {
        let trimmed = block.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.contains("<guards>")
            || lower.contains("standing rules")
            || lower.contains("<intake>")
            || lower.contains("<asking>")
            || lower.contains("<roles>")
            || lower.contains("<project-instructions")
            || lower.contains("disallowedtools")
        {
            high_priority.push(trimmed);
        } else {
            low_priority.push(trimmed);
        }
    }

    let mut result_blocks = Vec::new();
    let mut current_len = 0;

    // First pass: add high-priority blocks
    for block in high_priority {
        let block_len = block.len() + 2;
        if current_len + block_len <= context_budget {
            result_blocks.push(block.to_string());
            current_len += block_len;
        } else {
            // Truncate this high-priority block to fit remaining budget
            let remaining = context_budget.saturating_sub(current_len);
            if remaining > 100 {
                let cutoff = crate::text::head_bytes(block, remaining.saturating_sub(80));
                let truncated =
                    format!("{}\n\n[...truncated for OS command line limit...]", cutoff);
                current_len += truncated.len() + 2;
                result_blocks.push(truncated);
            }
            break;
        }
    }

    // Second pass: add low-priority blocks if budget remains
    for block in low_priority {
        let block_len = block.len() + 2;
        if current_len + block_len <= context_budget {
            result_blocks.push(block.to_string());
            current_len += block_len;
        } else {
            let remaining = context_budget.saturating_sub(current_len);
            if remaining > 120 {
                let cutoff = crate::text::head_bytes(block, remaining.saturating_sub(80));
                let truncated =
                    format!("{}\n\n[...truncated for OS command line limit...]", cutoff);
                result_blocks.push(truncated);
            }
            break;
        }
    }

    Ok((budgeted_prompt, result_blocks.join("\n\n")))
}

/// Write the whole of a prompt too long for a command line to the context
/// folder in `workspace` — the folder incode's memory hooks spill their
/// blocks to and prune (`crate::hooks::memory::CONTEXT`) — and return what the
/// agent is told instead: where the file is, and to read all of it first.
/// The name is the prompt's hash, so the same prompt is one file.
fn spill(prompt: &str, workspace: &std::path::Path) -> Result<String, AgentError> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    prompt.hash(&mut hasher);
    let name = format!("task-{:016x}.md", hasher.finish());
    let dir = workspace.join(crate::hooks::memory::CONTEXT);
    let path = dir.join(&name);
    std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&path, prompt))
        .map_err(|e| {
            AgentError::new(format!(
                "the task is {} bytes, more than a command line can carry, and could not \
                 be written to {} for the agent to read: {e}; the turn was not started",
                prompt.len(),
                path.display()
            ))
        })?;
    Ok(format!(
        "This task is {} bytes, more than a command line can carry, so the whole of it \
         is in the file `{}/{name}` in your working folder ({}). Read that file in \
         full, from its first line to its last, before doing anything else: it is the \
         task, and what it asks is what you do.",
        prompt.len(),
        crate::hooks::memory::CONTEXT,
        path.display()
    ))
}

#[cfg(test)]
mod route_parity_tests {
    use super::*;
    use crate::guards::Enforcement;

    /// **Every route declares what it really enforces — none inherits by
    /// accident.**
    ///
    /// The trait default is `NOTHING`, which is right for an unmeasured
    /// backend and WRONG for two of the three that exist. The API route was
    /// left on the default when the capability layer was added, so every task
    /// on it would have reported two guards unenforced while enforcing both by
    /// prevention. A default that is safe in general is not safe for a route
    /// nobody re-checked, which is the whole point of pinning all three here.
    #[test]
    fn each_route_states_its_own_enforcement() {
        let api = crate::agent::api::ApiBackend::new(
            crate::agent::api::Settings {
                endpoint: "http://localhost/x".to_string(),
                key: String::new(),
                model: "m".to_string(),
                ..Default::default()
            },
            std::path::Path::new("."),
            "",
            true,
        );
        assert_eq!(
            api.enforces(),
            Enforcement::EVERYTHING,
            "the API route enforces guards at the toolbox, before any write"
        );

        let cli = crate::agent::claude_code::ClaudeCodeBackend::new(std::path::PathBuf::new());
        assert_eq!(
            cli.enforces(),
            Enforcement::EVERYTHING,
            "this CLI accepts a deny list and was measured honouring it"
        );

        let agy = crate::agent::agy::AgyBackend::new(std::path::PathBuf::new());
        assert_eq!(
            agy.enforces(),
            Enforcement::NOTHING,
            "this CLI has no deny surface and ships sub-agent tools"
        );

        let codex = crate::agent::codex::CodexBackend::new(std::path::PathBuf::new());
        assert_eq!(
            codex.enforces(),
            Enforcement::NOTHING,
            "this CLI has no deny surface; Sentinel detects writes"
        );
    }

    /// **Every route reports the harness context it carries.**
    ///
    /// The API route held the context in a field and answered the empty
    /// default, so anything checking "did the session's context reach the
    /// agent?" would have concluded the API route drops it. The block COUNT
    /// differs by route — this one is handed a joined string — but "it
    /// arrived" must be answerable everywhere.
    #[test]
    fn each_route_reports_the_context_it_was_given() {
        let block = "<guards>the paths the agent may not write</guards>";

        let api = crate::agent::api::ApiBackend::new(
            crate::agent::api::Settings {
                endpoint: "http://localhost/x".to_string(),
                key: String::new(),
                model: "m".to_string(),
                ..Default::default()
            },
            std::path::Path::new("."),
            block,
            true,
        );
        assert!(
            api.system_context()
                .iter()
                .any(|b| b.contains("may not write")),
            "the API route carries the context but reported none: {:?}",
            api.system_context()
        );

        let cli = crate::agent::claude_code::ClaudeCodeBackend::new(std::path::PathBuf::new())
            .with_system_context(vec![block.to_string()]);
        assert!(cli
            .system_context()
            .iter()
            .any(|b| b.contains("may not write")));

        let agy = crate::agent::agy::AgyBackend::new(std::path::PathBuf::new())
            .with_system_context(vec![block.to_string()]);
        assert!(agy
            .system_context()
            .iter()
            .any(|b| b.contains("may not write")));

        let codex = crate::agent::codex::CodexBackend::new(std::path::PathBuf::new())
            .with_system_context(vec![block.to_string()]);
        assert!(codex
            .system_context()
            .iter()
            .any(|b| b.contains("may not write")));

        // And a session with nothing to say sends nothing, on every route.
        let quiet = crate::agent::api::ApiBackend::new(
            crate::agent::api::Settings {
                endpoint: "http://localhost/x".to_string(),
                key: String::new(),
                model: "m".to_string(),
                ..Default::default()
            },
            std::path::Path::new("."),
            "   ",
            true,
        );
        assert!(quiet.system_context().is_empty());
    }
}

#[cfg(test)]
mod handover_tests {
    use super::*;

    /// **An agent's own failure moves the work; the task's own does not.**
    ///
    /// Both halves matter. Measured failures that SHOULD move: a CLI that
    /// abandoned a task at its own deadline, and one that rejected the turn
    /// with `model sonnet is not recognized` — neither says anything about the
    /// task. Failures that must NOT move: a check that did not pass or a guard
    /// that refused will do the same on every agent, and carrying them spends a
    /// second subscription to learn one thing.
    #[test]
    fn only_the_agents_own_failures_move_the_work() {
        for agent_side in [
            "rate limit reached",
            "the agent stopped without finishing (ERROR): timeout waiting for response",
            "could not start `agy`: program not found",
            "invalid model selection: model sonnet is not recognized",
            "the agent produced no result line (exit 1)",
            // Copied from a real busy-model reply, not composed here.
            "The stream was interrupted. Please continue the task you were working on.",
        ] {
            assert!(
                moves_the_work(agent_side),
                "an agent-side failure must reach the standby: {agent_side}"
            );
        }

        for task_side in [
            "`node --check game.js` exited 1",
            "a protected file changed and the owner did not approve it",
            "the contract for this task was broken",
            "the agent finished but the check did not pass",
        ] {
            assert!(
                !moves_the_work(task_side),
                "a task-side failure must NOT walk through every subscription: {task_side}"
            );
        }
    }

    #[test]
    fn weekly_and_credit_limits_move_the_work() {
        assert!(moves_the_work(
            "You've reached your weekly limit for Claude Code"
        ));
        assert!(moves_the_work("weekly limit reached"));
        assert!(moves_the_work("exceeded your limit for the current period"));
        assert!(moves_the_work("out of credits on this account"));
        assert!(moves_the_work(
            "monthly limit exceeded: try again next billing cycle"
        ));
        assert!(moves_the_work("daily request limit reached"));
    }

    #[test]
    fn spawn_and_os_errors_move_the_work() {
        assert!(moves_the_work(
            "failed to spawn `claude`: The filename or extension is too long. (os error 206)"
        ));
        assert!(moves_the_work(
            "failed to spawn `claude`: No such file or directory (os error 2)"
        ));
        assert!(moves_the_work(
            "failed to spawn `agy`: Access is denied. (os error 5)"
        ));
        assert!(moves_the_work("cannot spawn child process: os error 206"));
        assert!(moves_the_work("command line too long"));
        assert!(moves_the_work("failed to execute process"));
        assert!(moves_the_work("CreateProcess failed"));
    }

    #[test]
    fn budget_for_spawn_preserves_under_limit() {
        let prompt = "Implement feature X";
        let context = vec![
            "<guards>no-write tests</guards>".to_string(),
            "standing rules: follow style guide".to_string(),
        ];
        let dir = tempfile::tempdir().expect("dir");
        let (p, c) = budget_for_spawn(prompt, &context, dir.path()).expect("budgeted");
        assert_eq!(p, prompt);
        assert_eq!(c, context.join("\n\n"));
        assert!(
            !dir.path().join(crate::hooks::memory::CONTEXT).exists(),
            "a prompt that fits writes no file"
        );
    }

    #[test]
    fn budget_for_spawn_bounds_large_context_preserving_guards() {
        let prompt = "Fix bug";
        let large_skills = "Skills on this machine:\n".to_string() + &"x".repeat(35_000);
        let context = vec![
            "<guards>protected</guards>".to_string(),
            "The owner's standing rules:\n1. do not delete".to_string(),
            large_skills,
        ];
        let dir = tempfile::tempdir().expect("dir");
        let (p, c) = budget_for_spawn(prompt, &context, dir.path()).expect("budgeted");
        assert_eq!(p, prompt);
        assert!(p.len() + c.len() <= MAX_CMDLINE_PAYLOAD_LEN);
        assert!(c.contains("<guards>protected</guards>"));
        assert!(c.contains("standing rules"));
    }

    /// The file an over-budget prompt was written to, read from what the
    /// agent was told.
    fn spilled(told: &str, workspace: &std::path::Path) -> String {
        let at = told.find("task-").expect("names a task file");
        let name = &told[at..at + "task-".len() + 16 + ".md".len()];
        std::fs::read_to_string(workspace.join(crate::hooks::memory::CONTEXT).join(name))
            .expect("the task file exists")
    }

    /// **AN OVER-BUDGET PROMPT REACHES THE AGENT WHOLE.** Both shapes a
    /// harness sends: the instruction LAST (a task after its context) and
    /// the instruction FIRST, then a long body (Insi's lesson writer, whose
    /// instruction the old middle cut dropped). Each is written to a file in
    /// full, the agent is told where it is and to read all of it, and the
    /// command line stays within the OS limit.
    #[test]
    fn budget_for_spawn_spills_an_oversized_prompt_whole() {
        let body = "context é ".repeat(MAX_CMDLINE_PAYLOAD_LEN / 10);
        let last = format!("<guards>g</guards>\n\n{body}\n\nNow do this:\n\ncreate a.txt with hi");
        let first =
            format!("Write one lesson as JSON with an issue field.\n\n{body}\nVERIFIED_FIX>>>");
        let context = vec!["<guards>safe</guards>".to_string(), "x".repeat(20_000)];
        for prompt in [last, first] {
            let dir = tempfile::tempdir().expect("dir");
            let (p, c) = budget_for_spawn(&prompt, &context, dir.path()).expect("budgeted");
            assert!(
                p.len() + c.len() <= MAX_CMDLINE_PAYLOAD_LEN,
                "{}",
                p.len() + c.len()
            );
            assert!(
                p.len() < 1_000,
                "the agent is told where, not sent a cut: {p}"
            );
            assert!(p.contains("Read that file in full"), "{p}");
            assert_eq!(
                spilled(&p, dir.path()),
                prompt,
                "nothing of the prompt is dropped"
            );
            assert!(c.contains("<guards>safe</guards>"));
        }
    }

    /// **A PROMPT THAT CANNOT BE WRITTEN DOWN IS NOT SENT AT ALL.** The
    /// workspace is a file, so the context folder cannot be made: the turn
    /// stops with the reason instead of starting on a prompt the agent would
    /// never see whole.
    #[test]
    fn budget_for_spawn_refuses_when_the_task_file_cannot_be_written() {
        let dir = tempfile::tempdir().expect("dir");
        let not_a_folder = dir.path().join("plain-file");
        std::fs::write(&not_a_folder, "x").expect("file");
        let prompt = "y".repeat(MAX_CMDLINE_PAYLOAD_LEN + 1);
        let why = budget_for_spawn(&prompt, &[], &not_a_folder).expect_err("refused");
        let why = why.to_string();
        assert!(
            why.contains("could not be written") && why.contains("not started"),
            "{why}"
        );
    }
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

/// An external agent this harness can route a turn to.
pub trait AgentBackend {
    /// Stable identifier, e.g. `"claude-code"`.
    fn id(&self) -> &str;

    /// Run one turn, calling `on_event` as events arrive.
    ///
    /// Implementations must continue an existing session where the underlying
    /// agent supports it: re-establishing a session per turn re-pays the whole
    /// prompt prefix. Measured against Claude Code 2.1.229, a fresh session
    /// costs ~36k cache-creation tokens versus ~1.8k when resuming.
    fn run_turn(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError>;

    /// The agent's current session id, once one exists.
    fn session_id(&self) -> Option<&str>;

    /// Ask for a particular model on the next turn, with ordered fallbacks.
    ///
    /// Defaulted to doing nothing, because not every agent can be told which
    /// model to use and one that cannot should not have to say so. An agent
    /// that CAN is where routing pays: cheap work on a cheap model, without an
    /// API credential anywhere.
    ///
    /// Called with the candidates in preference order. An empty slice means the
    /// caller has no opinion and the agent's own default should stand.
    fn set_models(&mut self, _candidates: &[String]) {}

    /// A model the operator named outright (`/model`, `-m`), rather than a
    /// role table's candidates. Defaulted to the same rule as
    /// [`Self::set_models`]; an API route takes any id its service offers.
    fn choose_model(&mut self, model: &str) {
        self.set_models(&[model.to_string()]);
    }

    // The methods below exist because an interactive session needs them and not
    // every backend can honour them. Each defaults to doing nothing rather than
    // being required, so a backend implements only what it actually supports —
    // and a caller reading the default gets "this route cannot do that", which
    // is the truth, instead of a panic or a pretence.

    /// Also work in these directories, beyond the workspace root.
    fn set_extra_dirs(&mut self, _dirs: Vec<std::path::PathBuf>) {}

    /// The deny rules in force, for showing the operator.
    ///
    /// Empty when the backend protects paths some other way — the toolbox
    /// checks the guards itself rather than handing rules to anyone.
    fn deny_rules(&self) -> &[String] {
        &[]
    }

    /// The harness context this backend will carry into its next turn.
    ///
    /// On the trait so the invariant can be checked for ANY agent, not just the
    /// one that happens to expose it. The context going missing is a measured
    /// regression — twice — and it is invisible from the outside, so the test
    /// that pins it must not be limited to a single backend.
    fn system_context(&self) -> &[String] {
        &[]
    }

    /// The MCP server configuration this agent will be handed, if any.
    ///
    /// Exists for the same reason [`Self::system_context`] does: the flag is
    /// assembled inside a spawn and is invisible from outside, so a test that
    /// checks "did the harness actually hand this over" needs a way to look.
    /// Defaults to `None`, which is the truth for every backend that has no
    /// registration path yet — see `docs/COMPUTER_USE.md` section 2.
    ///
    /// Returns an owned string because a backend may register SEVERAL servers
    /// and hold them as separate pieces. Borrowing forced codex to answer with
    /// the first of its overrides, which read as the whole registration and
    /// hid the second server completely.
    fn mcp_config(&self) -> Option<String> {
        None
    }

    /// Who this agent is on the team channel for the task about to run.
    ///
    /// # Why the environment and not the registration
    ///
    /// An agent CLI's MCP servers are configured ONCE, when the backend is
    /// built, and the role changes every task. So the team server cannot be
    /// told who it serves at registration time — it reads
    /// [`crate::team_mcp::IDENTITY_ENV`] instead, and the server is a child of
    /// the agent process, so setting the variable on the spawn is what reaches
    /// it.
    ///
    /// Identity has to come from here rather than from the model, because the
    /// harness RECORDS what is said on the channel: a name the agent could
    /// choose would be a claim written down as a fact.
    ///
    /// Defaulted to nothing, for a backend that spawns no process.
    fn set_team_identity(&mut self, _role: &str) {}

    /// This agent's OWN model ladder for a role, in preference order.
    ///
    /// # Why a backend supplies one at all
    ///
    /// The operator's ladder in `admin.json` holds concrete model ids, and ids
    /// do not cross vendors: measured, forwarding one agent's `sonnet` to
    /// another returned `model sonnet is not recognized` and failed every task
    /// on arrival. Dropping the foreign name keeps the run alive but loses
    /// routing entirely — every role then runs on the agent's default, which is
    /// the expensive one.
    ///
    /// So a backend may answer for itself, in its own namespace. The operator's
    /// ladder still wins wherever it names something this agent can serve; this
    /// is the floor, not an override.
    ///
    /// Empty by default: an agent whose model names have not been measured must
    /// not have a ladder invented for it.
    fn ladder_for(&self, _role: &str) -> Vec<String> {
        Vec::new()
    }

    /// What this backend can PREVENT for itself.
    ///
    /// Defaulted to [`crate::guards::Enforcement::NOTHING`] deliberately: a
    /// backend whose enforcement has not been MEASURED must not be credited
    /// with any, because the cost of guessing high is a guard the operator
    /// believes in and the agent never applies.
    fn enforces(&self) -> crate::guards::Enforcement {
        crate::guards::Enforcement::NOTHING
    }

    /// Whether a FAILED turn was the agent's own failure rather than the work's.
    ///
    /// # Why this is the backend's judgement and not a phrase list
    ///
    /// [`moves_the_work`] reads the failure MESSAGE, which is all a failure
    /// that never produced a turn gives us. That list can only ever hold the
    /// wordings someone has already seen, and a provider invents new ones:
    /// MEASURED live, a busy model answered `Our servers are experiencing high
    /// traffic right now, please try again in a minute.` — an unmistakable
    /// agent-side failure that matched no needle, so both attempts ran on the
    /// agent that had just said it could not serve them and the task was
    /// abandoned with a standby idle.
    ///
    /// A backend whose protocol has a SEPARATE channel for its own machinery
    /// knows the answer structurally and should say so, rather than waiting for
    /// the message to be recognised. One that has no such channel keeps the
    /// default and loses nothing.
    fn agent_failed(&self, outcome: &TurnOutcome) -> bool {
        moves_the_work(&outcome.failure())
    }

    /// Stop the turn when this token is tripped.
    fn set_cancel(&mut self, _token: crate::runtime::cancel::CancelToken) {}

    /// Who to ask when a tool call needs a person's confirmation, or a
    /// question needs the owner's answer.
    ///
    /// Defaulted to nothing: only a backend whose tools this harness runs
    /// itself ([`crate::toolbox::Toolbox`]) has a moment at which to ask.
    fn set_prompter(&mut self, _prompter: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {}

    /// Who to ask about a protected write when nobody is at the session — a
    /// run's owner channel. Defaulted to nothing, as [`Self::set_prompter`] is.
    fn set_owner(&mut self, _owner: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {}

    /// Record the protected writes a person allows into `record` — a
    /// sub-agent's, into its parent's. Defaulted to nothing, as the rest are.
    fn share_allowed_writes(&mut self, _record: std::sync::Arc<crate::toolbox::AllowedWrites>) {}

    /// The API route this backend drives, when it drives one — what
    /// `/models` asks for its service's model list. `None` for a CLI.
    fn api_route(&self) -> Option<&crate::agent::api::Settings> {
        None
    }

    /// The session's own tool state — its background jobs and task list —
    /// for a backend whose tools this harness runs, so every backend of one
    /// session shares them.
    ///
    /// Defaulted to nothing: a backend whose tools are not incode's has no
    /// jobs or list of incode's to share.
    fn share_session_tools(&mut self, _state: std::sync::Arc<crate::toolbox::SessionTools>) {}

    /// The model this backend was asked for, when it was asked for one.
    fn model(&self) -> Option<&str> {
        None
    }

    /// Begin a new conversation, forgetting the current one.
    fn clear_session(&mut self) {}

    /// How much the backend may do without asking.
    fn set_permission_mode(&mut self, _mode: &str) {}

    /// Continue the conversation with this id.
    fn set_session_id(&mut self, _session_id: &str) {}

    /// incode's id for the session, which the memory engine files everything
    /// under — its pre-tool cards as well as the session's capture.
    ///
    /// Defaulted to nothing: only a backend whose tool calls this harness runs
    /// itself sends the engine anything per call.
    fn set_memory_session(&mut self, _session: &str) {}

    /// The session is over: tell the engine's edit check, so what this agent
    /// claimed, read and announced is let go now rather than at its TTL.
    ///
    /// Defaulted to nothing: only a backend whose file tools this harness
    /// runs itself announced anything.
    fn end_code_edits(&mut self) {}

    /// Replace the harness context this backend carries into its turns.
    ///
    /// Defaulted to nothing for a backend that carries none.
    fn set_system_context(&mut self, _blocks: Vec<String>) {}

    /// The conversations subagents opened, as `(session id, what it was asked)`.
    ///
    /// Empty for a backend that does not delegate, and for one whose subagents
    /// are its own business — an agent CLI spawns them inside a process this
    /// harness cannot see into, so it has nothing to report and says so rather
    /// than guessing.
    fn children(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// The tools incode itself runs for this agent's model, by name: the
    /// toolbox's, for a backend whose model has no tools of its own. Empty
    /// for an agent whose CLI still brings its own. A session traces them
    /// when it starts, so which tools a model was handed is on the record —
    /// including whether the desktop was among them.
    fn offered_tools(&self) -> Vec<String> {
        Vec::new()
    }

    /// Who is told what this backend's tools and workers do while they run
    /// — see [`crate::watch`].
    ///
    /// Defaulted to nothing: only a backend whose tools this harness runs
    /// itself has calls, output and workers of incode's to report.
    fn set_watch(&mut self, _watch: std::sync::Arc<dyn crate::watch::Watch>) {}

    /// Carry these pictures with the next turn's prompt, and only that turn.
    ///
    /// # Errors
    ///
    /// The agent cannot take a picture headless — the default, because no
    /// route may be credited with a capability that was not measured on it.
    /// The error names the agent, for the person who attached them.
    fn attach_images(&mut self, images: Vec<crate::toolbox::Image>) -> Result<(), String> {
        Err(format!(
            "{} cannot take pictures headless, so {} picture(s) could not be sent; \
             describe them in words, or pick an agent that takes them",
            self.id(),
            images.len()
        ))
    }
}

/// A boxed backend is a backend.
///
/// The choice between routes is made at runtime — an installed CLI when there
/// is one, a model over an API when the operator configured one — so the call
/// sites hold one value either way. Without this they would each need to know
/// both types and branch around them.
impl AgentBackend for Box<dyn AgentBackend> {
    fn id(&self) -> &str {
        (**self).id()
    }

    /// Forwarded, so a boxed standby reports what the backend INSIDE it can
    /// enforce. Falling through to the trait default here would credit every
    /// boxed agent with enforcing nothing, and the gap report would be right by
    /// accident rather than by measurement.
    fn enforces(&self) -> crate::guards::Enforcement {
        (**self).enforces()
    }

    /// Forwarded for the same reason as [`Self::enforces`]: the run holds its
    /// agents BOXED, so a default here would mean no backend's structural
    /// judgement ever reached the failover loop — the one place it is read.
    fn agent_failed(&self, outcome: &TurnOutcome) -> bool {
        (**self).agent_failed(outcome)
    }

    fn system_context(&self) -> &[String] {
        (**self).system_context()
    }

    fn ladder_for(&self, role: &str) -> Vec<String> {
        (**self).ladder_for(role)
    }

    fn run_turn(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError> {
        (**self).run_turn(prompt, on_event)
    }

    fn session_id(&self) -> Option<&str> {
        (**self).session_id()
    }

    fn choose_model(&mut self, model: &str) {
        (**self).choose_model(model);
    }

    fn set_models(&mut self, candidates: &[String]) {
        (**self).set_models(candidates);
    }

    fn set_extra_dirs(&mut self, dirs: Vec<std::path::PathBuf>) {
        (**self).set_extra_dirs(dirs);
    }

    fn set_team_identity(&mut self, role: &str) {
        (**self).set_team_identity(role);
    }

    fn deny_rules(&self) -> &[String] {
        (**self).deny_rules()
    }

    fn set_cancel(&mut self, token: crate::runtime::cancel::CancelToken) {
        (**self).set_cancel(token);
    }

    fn set_prompter(&mut self, prompter: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {
        (**self).set_prompter(prompter);
    }

    fn set_owner(&mut self, owner: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {
        (**self).set_owner(owner);
    }

    fn share_allowed_writes(&mut self, record: std::sync::Arc<crate::toolbox::AllowedWrites>) {
        (**self).share_allowed_writes(record);
    }

    fn api_route(&self) -> Option<&crate::agent::api::Settings> {
        (**self).api_route()
    }

    fn share_session_tools(&mut self, state: std::sync::Arc<crate::toolbox::SessionTools>) {
        (**self).share_session_tools(state);
    }

    fn model(&self) -> Option<&str> {
        (**self).model()
    }

    fn clear_session(&mut self) {
        (**self).clear_session();
    }

    fn set_permission_mode(&mut self, mode: &str) {
        (**self).set_permission_mode(mode);
    }

    fn set_session_id(&mut self, session_id: &str) {
        (**self).set_session_id(session_id);
    }

    fn set_memory_session(&mut self, session: &str) {
        (**self).set_memory_session(session);
    }

    fn end_code_edits(&mut self) {
        (**self).end_code_edits();
    }

    fn set_system_context(&mut self, blocks: Vec<String>) {
        (**self).set_system_context(blocks);
    }

    fn children(&self) -> Vec<(String, String)> {
        (**self).children()
    }

    fn offered_tools(&self) -> Vec<String> {
        (**self).offered_tools()
    }

    fn set_watch(&mut self, watch: std::sync::Arc<dyn crate::watch::Watch>) {
        (**self).set_watch(watch);
    }

    fn attach_images(&mut self, images: Vec<crate::toolbox::Image>) -> Result<(), String> {
        (**self).attach_images(images)
    }
}

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
