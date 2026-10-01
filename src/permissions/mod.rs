//! Permission system — controls which tools can execute based on active mode.

use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// PermissionMode
// ---------------------------------------------------------------------------

/// How much a session is allowed to do.
///
/// `ReadOnly`, `WorkspaceWrite` and `DangerFullAccess` are rungs on a capability
/// ladder and can be compared. `Prompt` and `Allow` are **policies**, not rungs:
/// "ask a human" is not more or less capable than "may write files".
///
/// `Ord` is deliberately NOT derived. It previously was, and because the
/// variants were declared in this order it made `Prompt` (3) compare as greater
/// than `DangerFullAccess` (2) — so a `current >= required` check silently
/// granted every tool in Prompt mode and the prompt was never shown. Compare
/// modes only through [`PermissionMode::grants`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
    Prompt,
    Allow,
}

impl PermissionMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
            Self::Prompt => "prompt",
            Self::Allow => "allow",
        }
    }

    /// What this mode means to the agent that does the writing.
    ///
    /// A mode only matters if it REACHES whatever runs the tools. It did not:
    /// `-p read-only` was shown in the banner and never sent, so an operator
    /// who started that way believed nothing could be written while the agent
    /// wrote freely. Where incode runs the tools itself — the claude route,
    /// through [`crate::tool_bridge`] — this spelling is read back into a mode
    /// that incode's toolbox enforces, and nothing is handed to the CLI.
    ///
    /// **`danger-full-access` deliberately does NOT map to the agent's
    /// permission bypass.** That bypass would drop the deny rules with
    /// everything else, and the protected paths are not the operator's
    /// convenience — they are what stops the thing being judged from editing its
    /// own exam. Full access means "do not ask me", never "unprotect the tests".
    #[must_use]
    pub fn agent_mode(self) -> &'static str {
        match self {
            // Propose, do not apply.
            Self::ReadOnly => "plan",
            // Apply, within the deny rules.
            Self::WorkspaceWrite | Self::DangerFullAccess | Self::Allow => "acceptEdits",
            // Ask, which is the agent's own default behaviour.
            Self::Prompt => "default",
        }
    }

    /// The mode an agent-mode word names — the inverse of
    /// [`Self::agent_mode`], for a backend whose tools are incode's own. A
    /// word nobody recognises lands on read-only, as
    /// [`Self::from_str_lossy`] does: an unknown mode never widens.
    #[must_use]
    pub fn from_agent_mode(word: &str) -> Self {
        match word {
            "acceptEdits" => Self::WorkspaceWrite,
            "default" => Self::Prompt,
            _ => Self::ReadOnly,
        }
    }

    /// Rung on the capability ladder, if this mode is one.
    ///
    /// `Prompt` and `Allow` are policies and have no rung — which is exactly
    /// why they must never be compared with `<`/`>`.
    fn capability_rank(self) -> Option<u8> {
        match self {
            Self::ReadOnly => Some(0),
            Self::WorkspaceWrite => Some(1),
            Self::DangerFullAccess => Some(2),
            Self::Prompt | Self::Allow => None,
        }
    }

    /// Whether this mode grants `required` outright, with no human approval.
    ///
    /// The single place modes may be compared. `Prompt` never grants outright —
    /// granting is the whole thing it exists to withhold.
    #[must_use]
    pub fn grants(self, required: Self) -> bool {
        match self {
            Self::Allow => true,
            Self::Prompt => false,
            _ => match (self.capability_rank(), required.capability_rank()) {
                (Some(have), Some(need)) => have >= need,
                // A requirement that is itself a policy cannot be satisfied by
                // a capability rung; deny rather than guess.
                _ => false,
            },
        }
    }

    /// Parse a CLI string, or `None` if it names no known mode.
    ///
    /// Prefer this over [`Self::from_str_lossy`] so a typo in a security flag
    /// can be reported instead of silently resolving to something.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read-only" | "readonly" => Some(Self::ReadOnly),
            "workspace-write" | "write" => Some(Self::WorkspaceWrite),
            "danger-full-access" | "full" | "danger" => Some(Self::DangerFullAccess),
            "prompt" | "ask" => Some(Self::Prompt),
            "allow" => Some(Self::Allow),
            _ => None,
        }
    }

    /// Parse from CLI string, falling back to the most restrictive mode.
    ///
    /// Fails CLOSED. An unrecognised value previously resolved to `Prompt`,
    /// which — combined with the ordering bug above — meant a typo like
    /// `--permission-mode wrokspace-write` silently granted full access.
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        Self::parse(s).unwrap_or(Self::ReadOnly)
    }
}

// ---------------------------------------------------------------------------
// PermissionRequest / Decision / Outcome
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub input: String,
    pub current_mode: PermissionMode,
    pub required_mode: PermissionMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionPromptDecision {
    Allow,
    Deny { reason: String },
}

pub trait PermissionPrompter {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    Allow,
    Deny { reason: String },
}

// ---------------------------------------------------------------------------
// PermissionPolicy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPolicy {
    active_mode: PermissionMode,
    tool_requirements: BTreeMap<String, PermissionMode>,
}

impl PermissionPolicy {
    #[must_use]
    pub fn new(active_mode: PermissionMode) -> Self {
        Self {
            active_mode,
            tool_requirements: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_tool_requirement(
        mut self,
        tool_name: impl Into<String>,
        required_mode: PermissionMode,
    ) -> Self {
        self.tool_requirements
            .insert(tool_name.into(), required_mode);
        self
    }

    /// The requirements for the tools this harness actually dispatches: the
    /// twelve on the desktop MCP server.
    ///
    /// # Why the desktop is where this policy finally applies
    ///
    /// `PermissionPolicy` was written for a tool loop that no longer exists —
    /// the agent runs file and shell tools in its own process now, under its
    /// own rules, and this crate has no seat between the decision and the call.
    /// [`Self::with_defaults`] still names those tools and still has nobody to
    /// ask on their behalf.
    ///
    /// The desktop server is different: it IS the dispatcher.
    /// `desktop::mcp::call_tool` is a choke point in this crate's own process,
    /// and every click, keystroke and screenshot passes through it.
    ///
    /// # The split, and why it is only two rungs
    ///
    /// The four observing tools need `ReadOnly`; the eight that move something
    /// need `WorkspaceWrite`. Not `DangerFullAccess`, even though a click
    /// reaches far outside any workspace — because `workspace-write` is the
    /// default mode, and putting the desktop above it would mean the feature
    /// ships switched off and the flag becomes the thing people turn off.
    ///
    /// What this buys is the distinction that was missing: `-p read-only` now
    /// means the agent may LOOK at the screen and may not touch it. Before
    /// this, read-only sessions could type.
    ///
    /// `Prompt` denies everything here, and that is correct rather than
    /// unfortunate: stdin and stdout are the MCP protocol, so there is no
    /// channel to ask a human on, and [`Self::authorize`] with no prompter
    /// denies.
    #[must_use]
    pub fn for_desktop(active_mode: PermissionMode) -> Self {
        Self::new(active_mode)
            // Observing. A screenshot photographs a machine signed into real
            // accounts, which is why it is gated at all rather than left
            // ungoverned — but it takes nothing and changes nothing.
            .with_tool_requirement("screen_info", PermissionMode::ReadOnly)
            .with_tool_requirement("cursor_position", PermissionMode::ReadOnly)
            .with_tool_requirement("screenshot", PermissionMode::ReadOnly)
            .with_tool_requirement("ui_tree", PermissionMode::ReadOnly)
            // Acting. Each one changes the state of a machine the operator is
            // signed into.
            .with_tool_requirement("move_mouse", PermissionMode::WorkspaceWrite)
            .with_tool_requirement("click", PermissionMode::WorkspaceWrite)
            .with_tool_requirement("click_element", PermissionMode::WorkspaceWrite)
            .with_tool_requirement("type_text", PermissionMode::WorkspaceWrite)
            .with_tool_requirement("paste_text", PermissionMode::WorkspaceWrite)
            .with_tool_requirement("key", PermissionMode::WorkspaceWrite)
            .with_tool_requirement("scroll", PermissionMode::WorkspaceWrite)
            .with_tool_requirement("drag", PermissionMode::WorkspaceWrite)
    }

    /// Default permission requirements for the standard file/bash tools.
    #[must_use]
    pub fn with_defaults(self) -> Self {
        self.with_tool_requirement("read_file", PermissionMode::ReadOnly)
            .with_tool_requirement("glob_search", PermissionMode::ReadOnly)
            .with_tool_requirement("grep_search", PermissionMode::ReadOnly)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite)
            .with_tool_requirement("edit_file", PermissionMode::WorkspaceWrite)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            // Not ReadOnly, despite only reading. The URL is chosen by the
            // model, and a URL is an outbound channel: anything in context can
            // be encoded into the path or query of a host someone else
            // controls. ReadOnly must keep meaning "nothing leaves".
            .with_tool_requirement("web_fetch", PermissionMode::DangerFullAccess)
            // Same reasoning as web_fetch, one step weaker: the endpoint is the
            // operator's, not the model's, but the query still leaves the
            // machine and the model writes it.
            .with_tool_requirement("web_search", PermissionMode::DangerFullAccess)
            // Delegation confers no capability of its own. The subagent runs
            // under a clone of THIS policy, so every tool it reaches for is
            // authorised against the same rules, and it has no prompter — so
            // anything the parent could have approved interactively is simply
            // refused down there. It is strictly weaker than the parent, never
            // stronger. Gating it any higher would stop a read-only session from
            // delegating a read-only search, which is the most useful thing it
            // can do. Stated explicitly rather than left to the unknown-tool
            // default, so the reasoning survives a change to that default.
            .with_tool_requirement("task", PermissionMode::ReadOnly)
            // Writes this project's brief, and only ever the fields named in the
            // call — the patch is proven to leave the owner's rules, the proxy's
            // settings and every other project's brief untouched.
            //
            // WorkspaceWrite rather than the unknown-tool default of
            // DangerFullAccess: completing the brief is the sanctioned first job
            // of a session in the default mode, and a requirement the default
            // mode cannot meet would make the intake unable to finish the very
            // thing it exists to gather. Not ReadOnly, because it does write.
            .with_tool_requirement("save_brief", PermissionMode::WorkspaceWrite)
            // Reads what this machine already remembers, from a service on this
            // machine. ReadOnly rather than the tier the web tools need: the
            // query never leaves the host, so it is not the outbound channel
            // that makes `web_fetch` and `web_search` dangerous.
            // Runs the project's check, which runs code — the same thing the
            // `bash` tier exists to gate. Mechanical proof is worth having, and
            // it is not worth smuggling command execution in under a lower bar.
            .with_tool_requirement("verify", PermissionMode::DangerFullAccess)
            .with_tool_requirement("recall_index", PermissionMode::ReadOnly)
            .with_tool_requirement("recall_get", PermissionMode::ReadOnly)
    }

    #[must_use]
    pub fn active_mode(&self) -> PermissionMode {
        self.active_mode
    }

    #[must_use]
    pub fn required_mode_for(&self, tool_name: &str) -> PermissionMode {
        self.tool_requirements
            .get(tool_name)
            .copied()
            .unwrap_or(PermissionMode::DangerFullAccess)
    }

    #[must_use]
    pub fn authorize(
        &self,
        tool_name: &str,
        input: &str,
        mut prompter: Option<&mut dyn PermissionPrompter>,
    ) -> PermissionOutcome {
        let current = self.active_mode();
        let required = self.required_mode_for(tool_name);

        if current.grants(required) {
            return PermissionOutcome::Allow;
        }

        // WorkspaceWrite can prompt for DangerFullAccess escalation
        if current == PermissionMode::Prompt
            || (current == PermissionMode::WorkspaceWrite
                && required == PermissionMode::DangerFullAccess)
        {
            let request = PermissionRequest {
                tool_name: tool_name.to_string(),
                input: input.to_string(),
                current_mode: current,
                required_mode: required,
            };
            return match prompter.as_mut() {
                Some(prompter) => match prompter.decide(&request) {
                    PermissionPromptDecision::Allow => PermissionOutcome::Allow,
                    PermissionPromptDecision::Deny { reason } => PermissionOutcome::Deny { reason },
                },
                None => PermissionOutcome::Deny {
                    reason: format!(
                        "tool '{}' requires approval (no prompter available)",
                        tool_name
                    ),
                },
            };
        }

        PermissionOutcome::Deny {
            reason: format!(
                "tool '{}' requires {} permission; current mode is {}",
                tool_name,
                required.as_str(),
                current.as_str()
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    /// **A mode that is not sent is a mode that does not exist.** `-p read-only`
    /// was shown in the banner and never reached the agent, so an operator who
    /// started that way believed nothing could be written.
    #[test]
    fn read_only_tells_the_agent_to_propose_rather_than_apply() {
        assert_eq!(PermissionMode::ReadOnly.agent_mode(), "plan");
    }

    /// **Full access must never unprotect the tests.** The agent's own bypass
    /// would drop the deny rules along with everything else; the protected paths
    /// are not a convenience the operator can waive by asking for less prompting.
    #[test]
    fn full_access_does_not_bypass_the_protected_paths() {
        for mode in [
            PermissionMode::DangerFullAccess,
            PermissionMode::Allow,
            PermissionMode::WorkspaceWrite,
        ] {
            assert_ne!(
                mode.agent_mode(),
                "bypassPermissions",
                "{mode:?} must not disable the guards"
            );
        }
    }
    use super::*;

    struct AllowPrompter;
    impl PermissionPrompter for AllowPrompter {
        fn decide(&mut self, _request: &PermissionRequest) -> PermissionPromptDecision {
            PermissionPromptDecision::Allow
        }
    }

    #[test]
    fn allows_when_mode_meets_requirement() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("read_file", PermissionMode::ReadOnly)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);

        assert_eq!(
            policy.authorize("read_file", "{}", None),
            PermissionOutcome::Allow
        );
        assert_eq!(
            policy.authorize("write_file", "{}", None),
            PermissionOutcome::Allow
        );
    }

    #[test]
    fn denies_read_only_escalation() {
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess);

        assert!(matches!(
            policy.authorize("bash", "{}", None),
            PermissionOutcome::Deny { .. }
        ));
    }

    /// Counts whether the human was actually consulted.
    struct CountingPrompter {
        asked: usize,
    }
    impl PermissionPrompter for CountingPrompter {
        fn decide(&mut self, _request: &PermissionRequest) -> PermissionPromptDecision {
            self.asked += 1;
            PermissionPromptDecision::Deny {
                reason: "denied by test".to_string(),
            }
        }
    }

    /// Prompt mode must ASK, for every tool, including ones requiring the
    /// highest capability. It previously auto-allowed everything because the
    /// derived ordering made `Prompt` compare greater than `DangerFullAccess`,
    /// so the prompter was never reached.
    #[test]
    fn prompt_mode_asks_instead_of_granting() {
        let policy = PermissionPolicy::new(PermissionMode::Prompt).with_defaults();

        for tool in ["bash", "write_file", "read_file", "some_unknown_tool"] {
            let mut prompter = CountingPrompter { asked: 0 };
            let outcome = policy.authorize(tool, "{}", Some(&mut prompter));

            assert_eq!(
                prompter.asked, 1,
                "Prompt mode must consult the human for `{tool}`, not decide alone"
            );
            assert!(
                matches!(outcome, PermissionOutcome::Deny { .. }),
                "the prompter denied, so `{tool}` must be denied"
            );
        }
    }

    /// With no prompter available there is nobody to ask, so Prompt mode must
    /// deny rather than fall through to allowing.
    #[test]
    fn prompt_mode_denies_when_no_prompter_exists() {
        let policy = PermissionPolicy::new(PermissionMode::Prompt).with_defaults();
        assert!(
            matches!(
                policy.authorize("bash", "{}", None),
                PermissionOutcome::Deny { .. }
            ),
            "unattended Prompt mode must deny, never auto-allow"
        );
    }

    /// A mistyped security flag must fail CLOSED. It previously resolved to
    /// `Prompt`, which auto-allowed everything.
    #[test]
    fn an_unrecognised_mode_string_fails_closed() {
        assert_eq!(
            PermissionMode::from_str_lossy("wrokspace-write"),
            PermissionMode::ReadOnly,
            "a typo must land on the most restrictive mode"
        );
        assert_eq!(PermissionMode::parse("wrokspace-write"), None);
        assert_eq!(
            PermissionMode::parse("workspace-write"),
            Some(PermissionMode::WorkspaceWrite)
        );
    }

    /// The capability ladder still works, and policies never masquerade as rungs.
    #[test]
    fn grants_follows_the_capability_ladder_only() {
        use PermissionMode::{Allow, DangerFullAccess, Prompt, ReadOnly, WorkspaceWrite};

        assert!(DangerFullAccess.grants(WorkspaceWrite));
        assert!(WorkspaceWrite.grants(ReadOnly));
        assert!(!ReadOnly.grants(WorkspaceWrite));
        assert!(!WorkspaceWrite.grants(DangerFullAccess));

        assert!(Allow.grants(DangerFullAccess), "Allow grants everything");
        assert!(
            !Prompt.grants(ReadOnly),
            "Prompt must never grant outright, not even the lowest rung"
        );
        assert!(!Prompt.grants(DangerFullAccess));
    }

    #[test]
    fn prompts_for_escalation() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess);

        assert_eq!(
            policy.authorize("bash", "echo hi", Some(&mut AllowPrompter)),
            PermissionOutcome::Allow
        );
    }
}
