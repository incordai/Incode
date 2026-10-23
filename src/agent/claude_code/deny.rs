//! The harness's restrictions, in this agent's own deny syntax.

/// What THIS agent calls its sub-agent tool.
///
/// One backend's name for one capability. [`crate::guards::Restrictions`] says
/// only that sub-agents are refused; the word for it lives here, because the
/// next backend will have a different one — or none.
///
/// Measured: a turn given this on `--disallowedTools` reports the tool ABSENT
/// rather than merely refused, so the child is never offered a capability it
/// cannot use.
pub(super) const SUBAGENT_TOOL: &str = "Task";

/// The harness's restrictions, in this agent's own deny syntax.
///
/// # `Edit`, never `Write` — measured, not assumed
///
/// The tool name is `Edit`. A rule written as `Write(tests/**)` is **silently
/// ineffective**: it parses, it is accepted, and the write goes through. That
/// was measured — a probe denying only `Write(tests/**)` created the file
/// anyway, and this CLI reports that only `Edit(path)` rules are matched by
/// file permission checks, `Edit` covering every file-editing tool.
///
/// A guard that looks right and does nothing is worse than no guard, so the
/// choice is pinned by a test rather than left to whoever edits next.
///
/// Measured to cover shell redirection too: with `Edit(tests/**)` in force a
/// `printf > tests/x` was denied, and the identical command without it wrote
/// the file.
pub(super) fn render_denials(restrictions: &crate::guards::Restrictions) -> Vec<String> {
    let mut rules = Vec::new();
    if restrictions.no_subagents {
        rules.push(SUBAGENT_TOOL.to_string());
    }
    rules.extend(
        restrictions
            .no_write
            .iter()
            .map(|glob| format!("Edit({glob})")),
    );
    rules
}
