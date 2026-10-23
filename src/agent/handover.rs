//! Whether a failed turn is the agent's own failure — out of room, signed
//! out, gone quiet — rather than the work's, which decides whether the work
//! moves to a standby agent.
//!
//! Wired to: [`super::AgentBackend::agent_failed`] (the default and each
//! backend's), and [`super::TurnOutcome::failure`] for the sign-in remedy.

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
}
