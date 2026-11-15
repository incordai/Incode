//! What a session may do to the machine: the crate's own permission tier.
//!
//! # Where this sits
//!
//! The server asks [`Tier::authorise`] first of its three per-call gates
//! (`mcp/gate.rs`, `Server::authorise`), before the stop is consulted about
//! anything and before a single pixel is read. The tier arrives on the command
//! line ([`crate::cli`]: `--tier`, or `-p <mode>` as an alias) or from
//! [`crate::mcp::Server::with_tier`].
//!
//! # Why the crate owns this, and how it matches incode's
//!
//! Until the desktop became a crate of its own this was incode's
//! `PermissionPolicy::for_desktop(PermissionMode)`. The crate cannot depend on
//! incode, so the tier is restated here as the two rungs the 23 tools actually
//! use, and [`Tier::from_mode_word`] reads incode's own mode words so the argv
//! incode has always passed (`-p read-only`) keeps meaning the same thing:
//!
//! | mode word | tier | reaches |
//! |---|---|---|
//! | `read-only` | [`Tier::Observe`] | the 9 observing tools |
//! | `workspace-write` (the default), `danger-full-access`, `allow` | [`Tier::Act`] | all 23 |
//! | `prompt` | [`Tier::Deny`] | nothing |
//!
//! `prompt` denies everything, and that is correct rather than unfortunate:
//! stdin and stdout are the MCP protocol, so there is no channel to ask a human
//! on. A word nobody recognises is not guessed at; the caller refuses to start.
//!
//! # Only two rungs, on purpose
//!
//! The observing tools need `read-only`; the acting ones `workspace-write`. Not
//! `danger-full-access`, even though a click reaches far outside any workspace:
//! `workspace-write` is the default mode, and putting the desktop above it would
//! mean the feature ships switched off and the flag becomes the thing people
//! turn off. What the split buys is that `read-only` means the agent may LOOK at
//! the screen and may not touch it. Before it, read-only sessions could type.

/// The tools that only look. A screenshot photographs a machine signed into real
/// accounts, which is why it is gated at all rather than left ungoverned, but it
/// takes nothing and changes nothing.
pub const OBSERVING: &[&str] = &[
    "screen_info",
    "cursor_position",
    "screenshot",
    "ui_tree",
    "list_windows",
    // Waits and looks: it polls the tree, the window list and the screen and
    // changes nothing, so it is held to the same tier as the tools it is made of.
    "wait_for",
    // The same look, held for hours, then the operator's hook is told: it
    // looks and changes nothing, so it is held to the tier of what it is made of.
    "watch",
    // They read the routines file and touch neither the screen nor the file.
    "routine_list",
    "routine_match",
];

/// The tools that change the state of a machine the operator is signed into, and
/// so may end with a screenshot (`screenshot_after`, `mcp.rs`).
pub const ACTING: &[&str] = &[
    "click_element",
    "set_value",
    "element_action",
    "focus_window",
    "type_text",
    "paste_text",
    "key",
    "scroll",
    "drag",
    "move_mouse",
    "click",
    // `routine_run` replays steps that move the mouse and the keyboard, each
    // through the same gates (and so the same tier) as a live call.
    // `routine_save` and `routine_forget` write the routines file, and that file
    // is what a later `routine_run` does to the machine, so changing it is
    // acting, not looking.
    "routine_save",
    "routine_run",
    "routine_forget",
];

/// How much of the desktop a server will hand over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Nothing at all: there is nobody to ask for approval.
    Deny,
    /// Look, never touch: [`OBSERVING`].
    Observe,
    /// Look and touch: [`OBSERVING`] and [`ACTING`]. The default.
    Act,
}

impl Tier {
    /// The tier a `--tier` value names: `observe` or `act`.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "observe" => Some(Self::Observe),
            "act" => Some(Self::Act),
            _ => None,
        }
    }

    /// The tier an incode permission-mode word names, or `None` if it names no
    /// mode. The spellings are the ones incode's `PermissionMode::parse` takes.
    ///
    /// Called by [`crate::cli`] for `-p`, and by incode's `--desktop-mcp`
    /// wrapper with `PermissionMode::as_str()`.
    #[must_use]
    pub fn from_mode_word(word: &str) -> Option<Self> {
        match word {
            "read-only" | "readonly" => Some(Self::Observe),
            "workspace-write" | "write" | "danger-full-access" | "full" | "danger" | "allow" => {
                Some(Self::Act)
            }
            "prompt" | "ask" => Some(Self::Deny),
            _ => None,
        }
    }

    /// The name this tier is shown under: the mode word a refusal quotes and the
    /// startup line prints, so an operator who asked for `read-only` sees that it
    /// arrived.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Deny => "prompt",
            Self::Observe => "read-only",
            Self::Act => "workspace-write",
        }
    }

    /// Whether this tier lets `tool` run, with the reason when it does not.
    ///
    /// The words of a refusal are the ones the agent has always been given
    /// ("requires workspace-write permission; current mode is read-only"), so a
    /// client that matches on them keeps working.
    ///
    /// A tool the tables do not name is refused at every tier: the tables are the
    /// only way in. (Under incode's `danger-full-access` and `allow` such a name
    /// used to reach the dispatcher and be refused there as unknown; the
    /// refusal now comes from here, with the same effect and different words.)
    ///
    /// # Errors
    ///
    /// The tier does not reach `tool`.
    pub fn authorise(self, tool: &str) -> Result<(), String> {
        let acting = ACTING.contains(&tool);
        let observing = OBSERVING.contains(&tool);
        match self {
            Self::Act if acting || observing => Ok(()),
            Self::Observe if observing => Ok(()),
            Self::Observe => Err(format!(
                "tool '{tool}' requires {} permission; current mode is {}",
                if acting {
                    "workspace-write"
                } else {
                    "danger-full-access"
                },
                self.label()
            )),
            // `Deny`, and an unknown tool under `Act`: someone would have to
            // approve it, and nobody can be asked.
            _ => Err(format!(
                "tool '{tool}' requires approval (no prompter available)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tier table must be the server's tool list, or a tool is either
    /// ungoverned or unreachable. Checked against what `tools/list` offers, so a
    /// twenty-fourth tool cannot be added without deciding its tier.
    #[test]
    fn every_listed_tool_has_a_tier_and_there_are_twenty_three() {
        let listed: Vec<String> = crate::mcp::tool_definitions()
            .iter()
            .map(|t| t["name"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(listed.len(), 23, "{listed:?}");
        for name in &listed {
            assert!(
                OBSERVING.contains(&name.as_str()) ^ ACTING.contains(&name.as_str()),
                "`{name}` must be in exactly one of OBSERVING and ACTING"
            );
        }
        assert_eq!(OBSERVING.len() + ACTING.len(), listed.len());
    }

    #[test]
    fn read_only_looks_and_never_touches() {
        for tool in OBSERVING {
            assert!(Tier::Observe.authorise(tool).is_ok(), "{tool}");
        }
        for tool in ACTING {
            let why = Tier::Observe.authorise(tool).expect_err(tool);
            assert!(why.contains("workspace-write"), "{why}");
            assert!(
                why.contains("read-only"),
                "the refusal names the mode: {why}"
            );
        }
    }

    #[test]
    fn the_default_tier_reaches_every_tool() {
        for tool in OBSERVING.iter().chain(ACTING) {
            assert!(Tier::Act.authorise(tool).is_ok(), "{tool}");
        }
    }

    /// `prompt` denies everything: stdin and stdout are the protocol, so there is
    /// nobody to ask.
    #[test]
    fn prompt_reaches_nothing() {
        for tool in OBSERVING.iter().chain(ACTING) {
            let why = Tier::Deny.authorise(tool).expect_err(tool);
            assert!(why.contains("requires approval"), "{why}");
        }
    }

    #[test]
    fn a_tool_nobody_serves_is_refused_at_every_tier() {
        for tier in [Tier::Deny, Tier::Observe, Tier::Act] {
            assert!(tier.authorise("format_disk").is_err(), "{tier:?}");
        }
    }

    /// The words incode has always passed on `-p` keep their meaning, and a typo
    /// in a security flag is reported rather than resolved to something.
    #[test]
    fn incodes_mode_words_map_to_the_same_rungs() {
        assert_eq!(Tier::from_mode_word("read-only"), Some(Tier::Observe));
        assert_eq!(Tier::from_mode_word("workspace-write"), Some(Tier::Act));
        assert_eq!(Tier::from_mode_word("danger-full-access"), Some(Tier::Act));
        assert_eq!(Tier::from_mode_word("allow"), Some(Tier::Act));
        assert_eq!(Tier::from_mode_word("prompt"), Some(Tier::Deny));
        assert_eq!(Tier::from_mode_word("wrokspace-write"), None);
    }

    #[test]
    fn the_tier_flag_takes_observe_and_act_only() {
        assert_eq!(Tier::parse("observe"), Some(Tier::Observe));
        assert_eq!(Tier::parse("act"), Some(Tier::Act));
        assert_eq!(Tier::parse("read-only"), None);
        assert_eq!(Tier::parse("deny"), None);
    }
}
