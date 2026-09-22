//! Agent discovery — find agent CLIs the user already installed and
//! authenticated, and register them as available backends.
//!
//! Deliberately does NOT read any credential store. The harness detects that a
//! binary exists and lets the agent authenticate itself; an auth failure
//! surfaces on first use, from the agent's own process. That keeps the harness
//! clear of vendor credentials entirely.
//!
//! Mirrors the probe/detect shape already used by [`crate::tools::tool_status`].

use std::process::{Command, Stdio};

/// The Antigravity agent's id.
///
/// A CONSTANT because the id (`antigravity`) and the binary (`agy`) differ, and
/// the backend selector originally matched the BINARY name. That compiled, and
/// a unit test that had invented the same wrong id on both sides passed — so
/// the second agent was unreachable in every real run until a live check said
/// "NOT INSTALLED". One name, in one place, cannot drift like that.
pub const ANTIGRAVITY: &str = "antigravity";

/// The OpenAI Codex agent's id.
pub const CODEX: &str = "codex";

/// How to drive an agent non-interactively.
///
/// Only protocols whose wire format has actually been verified are listed. A
/// detected agent whose contract has not been verified is registered as
/// present-but-not-drivable rather than driven on guessed CLI flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// `-p <prompt> --output-format stream-json --verbose`, newline-delimited
    /// JSON tagged on `type`. Verified against Claude Code 2.1.229.
    ClaudeStreamJson,
    /// Installed, but its headless contract has not been verified here.
    Unverified,
    /// `--print <prompt> --output-format stream-json`, newline-delimited JSON
    /// tagged on `event`. Verified against Antigravity 1.1.23 and implemented
    /// by [`crate::agent::agy::AgyBackend`].
    ///
    /// A SEPARATE variant rather than reusing the other stream-json one: the
    /// two formats share a flag name and agree on almost nothing else — the
    /// envelope key, the session field and the usage shape all differ — so one
    /// value covering both would be a claim that reading either is the same
    /// job.
    AgyStreamJson,
    /// `exec --json`, newline-delimited JSON tagged on `type`. Verified against
    /// OpenAI Codex CLI 0.153.4 and implemented by
    /// [`crate::agent::codex::CodexBackend`].
    CodexJson,
    /// A model reached over an HTTP API, driven by this harness directly.
    ///
    /// Not discovered on the machine like the others — it exists because the
    /// operator configured an endpoint. The harness supplies the tools on this
    /// route, so it is drivable wherever it is configured.
    HarnessApi,
}

pub struct AgentProbe {
    pub id: &'static str,
    pub display_name: &'static str,
    /// Candidate binary names, tried in order.
    pub commands: &'static [&'static str],
    pub version_args: &'static [&'static str],
    pub protocol: Protocol,
}

/// Agents the harness knows how to look for. Presence here is not a claim of
/// support — `protocol` decides whether a turn can actually be routed to it.
pub const KNOWN_AGENTS: &[AgentProbe] = &[
    AgentProbe {
        id: "claude-code",
        display_name: "Claude Code",
        commands: &["claude"],
        version_args: &["--version"],
        protocol: Protocol::ClaudeStreamJson,
    },
    AgentProbe {
        id: CODEX,
        display_name: "Codex CLI",
        commands: &["codex"],
        version_args: &["--version"],
        protocol: Protocol::CodexJson,
    },
    AgentProbe {
        id: "gemini",
        display_name: "Gemini CLI",
        commands: &["gemini"],
        version_args: &["--version"],
        protocol: Protocol::Unverified,
    },
    AgentProbe {
        id: "opencode",
        display_name: "OpenCode",
        commands: &["opencode"],
        version_args: &["--version"],
        protocol: Protocol::Unverified,
    },
    AgentProbe {
        id: ANTIGRAVITY,
        display_name: "Antigravity",
        // Confirmed on a real machine: `agy --version` answered `1.1.22`.
        commands: &["agy"],
        version_args: &["--version"],
        // Read, and now driven. The contract was taken from `agy --help` and a
        // real `--output-format stream-json` run of 1.1.23 — the envelope is
        // keyed on `event`, the session is a `conversation_id`, and assistant
        // text arrives on a `step_update`. None of that is guessable from the
        // other agent's format, which is why this line stayed `Unverified`
        // until somebody actually ran it.
        protocol: Protocol::AgyStreamJson,
    },
    AgentProbe {
        id: "cursor",
        display_name: "Cursor",
        // `cursor-agent` is the headless binary and `cursor` the editor
        // launcher, so the headless one is tried first.
        //
        // UNVERIFIED ON ANY MACHINE HERE — neither name was found on the device
        // this was written against, so unlike every other entry the COMMAND
        // NAMES themselves are unconfirmed, not just the protocol. If Cursor is
        // installed and `/doctor` still does not list it, this is the line to
        // correct.
        commands: &["cursor-agent", "cursor"],
        version_args: &["--version"],
        protocol: Protocol::Unverified,
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedAgent {
    pub id: String,
    pub display_name: String,
    pub command: String,
    pub path: Option<String>,
    pub version: Option<String>,
    pub protocol: Protocol,
}

/// The route the work takes: a configured API if there is one, otherwise an
/// installed CLI.
///
/// The API comes first because it is the operator's explicit choice — they set
/// an endpoint, which nothing does by accident — and because it works on a
/// machine with no agent installed at all, where `require_drivable` can only
/// refuse.
pub fn require_route() -> Result<DetectedAgent, String> {
    match crate::agent::api::Settings::from_env() {
        Some(settings) => Ok(DetectedAgent::over_api(&settings.model)),
        None => require_drivable(),
    }
}

/// The agent that will do the work, or why there is none.
///
/// Distinguishes the two cases a single "no agent found" message used to
/// flatten: nothing installed at all, and something installed that this harness
/// cannot yet speak to. Those need different things from the operator, so
/// telling them the same thing sends half of them looking for the wrong fix.
pub fn require_drivable() -> Result<DetectedAgent, String> {
    let found = discover();
    match choose(&found, preferred_agent().as_deref()) {
        Some(agent) => Ok(agent),
        None => Err(no_drivable_reason(&found)),
    }
}

/// The agent the operator asked for, if they asked for one.
///
/// Checks the environment variable first (`INCORD_AGENT`), then the operator's
/// standing choice in `admin.json`.
#[must_use]
pub fn preferred_agent() -> Option<String> {
    if let Ok(v) = std::env::var(crate::admin::AGENT_ENV) {
        let trimmed = v.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    crate::admin::AdminConfig::load_default().agent
}

/// Which discovered agent does the work: the one ASKED FOR when it can be
/// driven, otherwise the first drivable one.
///
/// # The bug this closes
///
/// This used to be `found.iter().find(|a| a.is_drivable())` — the first
/// drivable agent, with the operator's choice never consulted. The admin screen
/// read `INCORD_AGENT` and reported it, so `--admin` said `antigravity` while
/// the run drove Claude Code and printed "via Claude Code" in its own banner.
/// Two resolvers, one of them authoritative and the other on display.
///
/// Split out from [`require_drivable`] because that one probes the real machine
/// and cannot be tested for a state this machine is not in; the CHOICE is the
/// part that has to be right, and it is pure.
/// An ordered chain of drivable agents to try: the preferred agent first (if drivable),
/// followed by all other drivable agents discovered on the system in discovery order.
#[must_use]
pub fn drivable_chain(found: &[DetectedAgent], preferred: Option<&str>) -> Vec<DetectedAgent> {
    let mut chain = Vec::new();
    if let Some(want) = preferred.map(str::trim).filter(|w| !w.is_empty()) {
        if let Some(agent) = found
            .iter()
            .find(|a| a.id.eq_ignore_ascii_case(want) && a.is_drivable())
        {
            chain.push(agent.clone());
        } else {
            crate::trace!(
                "agent",
                "{want} was asked for but is not drivable here; falling back"
            );
        }
    }
    for agent in found {
        if agent.is_drivable() && !chain.iter().any(|a| a.id.eq_ignore_ascii_case(&agent.id)) {
            chain.push(agent.clone());
        }
    }
    chain
}

/// All drivable agents discovered on this machine, ordered with preferred first if set,
/// or an explanation of why nothing can be driven.
pub fn require_drivable_chain() -> Result<Vec<DetectedAgent>, String> {
    let found = discover();
    let chain = drivable_chain(&found, preferred_agent().as_deref());
    if chain.is_empty() {
        Err(no_drivable_reason(&found))
    } else {
        Ok(chain)
    }
}

#[must_use]
pub fn choose(found: &[DetectedAgent], preferred: Option<&str>) -> Option<DetectedAgent> {
    drivable_chain(found, preferred).into_iter().next()
}

/// Why nothing can be driven, given what was found.
///
/// Split from [`require_drivable`] because that one probes the real machine and
/// cannot be tested for a state this machine is not in — and the message is the
/// part that has to be right.
#[must_use]
pub fn no_drivable_reason(found: &[DetectedAgent]) -> String {
    if found.is_empty() {
        return format!(
            "Nothing to send the work to. Either install an agent CLI and log in — incode drives              one on your own subscription — or configure a model endpoint by setting {} and {},              which needs no CLI at all.",
            crate::agent::api::API_URL_ENV,
            crate::agent::api::API_KEY_ENV
        );
    }
    let names = found
        .iter()
        .map(|a| a.display_name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Found {names}, but this harness cannot drive it yet. Only agents whose stream protocol has been verified against a real run are used: guessing at one produces a loop that fails silently, which is worse than refusing. Install a supported agent."
    )
}

impl DetectedAgent {
    /// The configured API, as a route.
    ///
    /// Reported as an agent because that is what it is from here: somewhere a
    /// turn can be sent. Named for what it is rather than for a provider — the
    /// endpoint is the operator's, and this harness does not know or care whose
    /// it is.
    #[must_use]
    pub fn over_api(model: &str) -> Self {
        Self {
            id: "api".to_string(),
            display_name: "a model over the configured API".to_string(),
            // No binary: nothing is spawned on this route.
            command: String::new(),
            path: None,
            version: if model.is_empty() {
                None
            } else {
                Some(model.to_string())
            },
            protocol: Protocol::HarnessApi,
        }
    }

    /// Whether the harness can route a turn to this agent today.
    #[must_use]
    pub fn is_drivable(&self) -> bool {
        self.protocol != Protocol::Unverified
    }

    /// One display line, e.g. `claude-code  2.1.229 (Claude Code)  [drivable]`.
    ///
    /// # Wired to
    ///
    /// `doctor::check_agents`, in the arm that fires when agents are installed
    /// and none can be driven. That is the line someone reads while working
    /// out why nothing runs, and a list of display names does not answer it —
    /// whether the installed one is too old is the first thing to check.
    #[must_use]
    pub fn display_line(&self) -> String {
        format!(
            "{:<14} {:<28} [{}]",
            self.id,
            self.version.as_deref().unwrap_or("unknown version"),
            if self.is_drivable() {
                "drivable"
            } else {
                "detected"
            },
        )
    }
}

/// Resolve a command to an absolute path via `where` / `which`.
fn find_command_path(cmd: &str) -> Option<String> {
    #[cfg(windows)]
    let which_cmd = "where";
    #[cfg(not(windows))]
    let which_cmd = "which";

    let out = Command::new(which_cmd)
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // `where` returns one line per match; take the first.
    let first = text.lines().next()?.trim().to_string();
    if first.is_empty() {
        None
    } else {
        Some(first)
    }
}

/// Run `<cmd> --version` and return the first line, capped at 80 characters.
fn probe_version(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    // Some CLIs print their version to stderr, so accept either stream.
    let raw = if out.stdout.is_empty() {
        String::from_utf8_lossy(&out.stderr).to_string()
    } else {
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let line = raw.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    // Truncate by CHARACTER, not byte — a version banner can carry non-ASCII,
    // and slicing mid-codepoint panics.
    Some(line.chars().take(80).collect())
}

/// Discover every known agent present on this machine.
#[must_use]
pub fn discover() -> Vec<DetectedAgent> {
    let mut found = Vec::new();
    for probe in KNOWN_AGENTS {
        for cmd in probe.commands {
            let Some(path) = find_command_path(cmd) else {
                continue;
            };
            found.push(DetectedAgent {
                id: probe.id.to_string(),
                display_name: probe.display_name.to_string(),
                command: (*cmd).to_string(),
                path: Some(path),
                version: probe_version(cmd, probe.version_args),
                protocol: probe.protocol,
            });
            break; // first matching command wins
        }
    }
    found
}

#[cfg(test)]
mod tests {

    /// **"Nothing installed" and "installed but unsupported" are different
    /// problems.** One message for both sent half the operators looking for the
    /// wrong fix — a user with codex installed was told no agent was found.
    /// **"Nothing installed" and "installed but unsupported" are different
    /// problems.** One message for both sent half the operators looking for the
    /// wrong fix — someone with codex installed was told no agent was found.
    #[test]
    fn an_installed_but_undrivable_agent_is_not_reported_as_missing() {
        let reason = no_drivable_reason(&[sample(Protocol::Unverified)]);
        assert!(
            reason.contains("Codex CLI"),
            "it must name what it found: {reason}"
        );
        assert!(
            !reason.contains("No agent CLI is installed"),
            "an installed agent must not be reported as absent: {reason}"
        );
    }

    /// And an empty machine is told to install one, not that something it has
    /// is unsupported.
    #[test]
    fn an_empty_machine_is_told_both_ways_out() {
        // It used to pin the sentence "No agent CLI is installed", which was the
        // whole story when installing one was the only route. It is not any
        // more: a configured endpoint needs no CLI at all, and naming only the
        // CLI sends half of the people who read this to the wrong fix — the
        // exact failure this function's own doc comment exists to avoid.
        let reason = no_drivable_reason(&[]);
        assert!(reason.contains("install an agent CLI"), "got: {reason}");
        assert!(
            reason.contains(crate::agent::api::API_URL_ENV),
            "and how to configure an endpoint: {reason}"
        );
        assert!(
            reason.contains(crate::agent::api::API_KEY_ENV),
            "naming both variables, or it cannot be acted on: {reason}"
        );
    }

    /// A verified protocol is drivable; an unverified one is not. Guessing at a
    /// stream format produces a loop that fails silently, which is worse than
    /// refusing.
    #[test]
    fn only_a_verified_protocol_is_drivable() {
        assert!(!sample(Protocol::Unverified).is_drivable());
        assert!(sample(Protocol::ClaudeStreamJson).is_drivable());
    }
    use super::*;

    fn sample(protocol: Protocol) -> DetectedAgent {
        DetectedAgent {
            id: "codex".into(),
            display_name: "Codex CLI".into(),
            command: "codex".into(),
            path: None,
            version: None,
            protocol,
        }
    }

    /// The guard that stops the harness inventing CLI flags for an agent whose
    /// headless contract nobody has verified.
    #[test]
    fn unverified_agent_is_not_drivable() {
        assert!(
            !sample(Protocol::Unverified).is_drivable(),
            "an agent with an unverified protocol must never be driven"
        );
        assert!(
            sample(Protocol::ClaudeStreamJson).is_drivable(),
            "an agent with a verified protocol must be drivable"
        );
    }

    /// Version banners are external input; truncating one by byte index panics
    /// on a multi-byte boundary.
    #[test]
    fn version_truncation_does_not_split_a_codepoint() {
        let banner = "é".repeat(200);
        let capped: String = banner.chars().take(80).collect();
        assert_eq!(capped.chars().count(), 80);
    }

    #[test]
    fn display_line_marks_drivability() {
        let mut a = sample(Protocol::ClaudeStreamJson);
        a.version = Some("2.1.229 (Claude Code)".into());
        assert!(a.display_line().contains("drivable"));
        assert!(sample(Protocol::Unverified)
            .display_line()
            .contains("detected"));
    }

    fn agent(id: &str, protocol: Protocol) -> DetectedAgent {
        DetectedAgent {
            id: id.to_string(),
            display_name: id.to_string(),
            command: id.to_string(),
            path: Some(format!("/bin/{id}")),
            version: Some("1.0".to_string()),
            protocol,
        }
    }

    /// **The agent the operator ASKED FOR does the work.**
    ///
    /// The bug: this took the first drivable agent and never read the
    /// preference, while the admin screen read it and displayed it. `--admin`
    /// reported one agent and the run drove another, announcing the wrong one
    /// in its own banner.
    #[test]
    fn the_agent_the_operator_asked_for_is_the_one_chosen() {
        let found = vec![
            agent("claude-code", Protocol::ClaudeStreamJson),
            agent(ANTIGRAVITY, Protocol::AgyStreamJson),
            agent(CODEX, Protocol::CodexJson),
        ];

        // Control: with no preference the first drivable one still wins, so a
        // pass below cannot be the list order doing the work.
        assert_eq!(choose(&found, None).unwrap().id, "claude-code");

        assert_eq!(
            choose(&found, Some(ANTIGRAVITY)).unwrap().id,
            ANTIGRAVITY,
            "the operator's choice was ignored — the run would drive a \
             different agent than the admin screen reports"
        );
        assert_eq!(
            choose(&found, Some(CODEX)).unwrap().id,
            CODEX,
            "the operator's choice of CODEX was ignored"
        );
    }

    /// An agent that is asked for but cannot be driven falls back rather than
    /// failing the run — the work still gets done by something.
    #[test]
    fn asking_for_an_undrivable_agent_falls_back_instead_of_stopping() {
        let found = vec![
            agent("claude-code", Protocol::ClaudeStreamJson),
            agent("cursor", Protocol::Unverified),
        ];
        assert_eq!(choose(&found, Some("cursor")).unwrap().id, "claude-code");
    }

    /// A name nobody installed is not a silent no-op either.
    #[test]
    fn asking_for_an_absent_agent_still_yields_a_drivable_one() {
        let found = vec![agent("claude-code", Protocol::ClaudeStreamJson)];
        assert_eq!(
            choose(&found, Some("nothing-installed")).unwrap().id,
            "claude-code"
        );
        assert!(choose(&[], Some("claude-code")).is_none());
    }

    /// Drivable chain preserves preferred agent at the front and appends all
    /// other drivable agents without duplicates.
    #[test]
    fn drivable_chain_orders_preferred_first_followed_by_remaining() {
        let found = vec![
            agent("claude-code", Protocol::ClaudeStreamJson),
            agent(ANTIGRAVITY, Protocol::AgyStreamJson),
            agent(CODEX, Protocol::CodexJson),
            agent("unverified", Protocol::Unverified),
        ];
        let chain = drivable_chain(&found, Some(ANTIGRAVITY));
        let ids: Vec<_> = chain.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec![ANTIGRAVITY, "claude-code", CODEX]);

        // When preferred is unverified/undrivable, all drivable agents are returned in order
        let fallback_chain = drivable_chain(&found, Some("unverified"));
        let fallback_ids: Vec<_> = fallback_chain.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(fallback_ids, vec!["claude-code", ANTIGRAVITY, CODEX]);

        // When no preference, all drivable agents are returned in order
        let default_chain = drivable_chain(&found, None);
        let default_ids: Vec<_> = default_chain.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(default_ids, vec!["claude-code", ANTIGRAVITY, CODEX]);
    }
}
