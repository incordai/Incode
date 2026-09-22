//! The owner's standing rules, read from the body rather than restated here.
//!
//! The rules are already written, already embedded and already ranked — in the
//! memory service, where every agent on this machine reads the same set. This
//! module is the *loader*, and deliberately nothing more: no rule engine, no
//! policy language, no second copy that drifts from the original.
//!
//! # Why they ride the turn instead of the system prompt
//!
//! The service returns the always-on rules *unioned with* the ones that match what
//! is being asked. Which rules matter therefore depends on the question, and a set
//! pinned once at session start would be the wrong set by the second turn — while
//! pinning all of them would spend the context budget on rules about deployment
//! during a conversation about parsing.
//!
//! The always-on rules come back in **every** response, so nothing that must always
//! apply can fall out by being unlucky in the ranking.
//!
//! # Rules are instructions; everything else retrieved is not
//!
//! This is the one block pulled from memory that the model is *meant* to obey. It
//! is the owner's own text, edited by the owner, in the owner's own store — unlike
//! recalled conversations or recorded tool outputs, which are data about the past.
//! The block says which it is, because a model that treats all injected text alike
//! either obeys the wrong thing or obeys nothing.

use std::time::Duration;

/// Where the ranked rules come from, verified against the running service.
pub const RULES_PATH: &str = "/v1/memory/rules/for";

/// Bounded: this runs on the turn path and a slow store must not stall a prompt.
const TIMEOUT: Duration = Duration::from_secs(4);

/// How long a turn will wait for the rules before going without them.
///
/// The shared deadline, not a copy of it — see [`crate::recall::WAITING_TIMEOUT`]
/// for why there is only one.
pub const TURN_TIMEOUT: Duration = crate::recall::WAITING_TIMEOUT;

/// How long STARTUP will wait for the always-rules.
///
/// Longer than a turn's budget because it is paid once and overlaps the rest of
/// the opening work — measured at 464ms against the live service, so this is
/// headroom rather than an expected wait.
const SESSION_TIMEOUT: Duration = Duration::from_secs(4);

/// How many ranked matches to ask for, on top of the always-on set.
///
/// Small on purpose. The always-on rules are returned regardless, so this only
/// decides how much *situational* guidance rides along, and every extra rule is
/// paid for on every turn.
const TOP_K: usize = 4;

/// What the service said applies here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ruleset {
    /// Always-on plus matched, in the store's stable order.
    pub rules: Vec<String>,
    /// The subset that applies to everything, whatever is being asked.
    pub always: Vec<String>,
    /// True when the whole set came back rather than a ranked selection.
    pub complete: bool,
}

impl Ruleset {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// Ask which rules apply to what is being done.
pub fn rules_for(query: &str) -> Result<Ruleset, String> {
    parse(&get(&rules_url(query))?)
}

/// The exact URL the rules are read from.
///
/// Pinned by a test: the contract was established by calling the running service,
/// and a parser tested alone passes happily while the request goes nowhere.
#[must_use]
pub fn rules_url(query: &str) -> String {
    format!(
        "{}{RULES_PATH}?top_k={TOP_K}&query={}",
        crate::brief::memory_url().trim_end_matches('/'),
        query_escape(query)
    )
}

/// Read the service's answer.
///
/// Unknown fields are ignored: the store is a separate program on its own release
/// schedule, and a field added there must not stop a turn here.
pub fn parse(body: &str) -> Result<Ruleset, String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("the rule store's answer did not parse: {e}"))?;
    let strings = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|r| r.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(Ruleset {
        rules: strings("rules"),
        always: strings("always"),
        complete: value
            .get("complete")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

/// The block to put in front of the model for this turn, if there is one.
///
/// Silent on failure. A turn without the rules block is worse than one with it,
/// and both are better than a session that will not start because the store is
/// down — the rules are also enforced mechanically by hooks and permission tiers,
/// which do not depend on this call.
#[must_use]
pub fn context_for_turn(line: &str) -> Option<String> {
    // Bounded by TURN_TIMEOUT, not by the tool timeout: this runs in front of
    // every keystroke. Measured at 4.0s per message before this existed.
    let began = std::time::Instant::now();
    let answer = match get_within(&rules_url(line), TURN_TIMEOUT) {
        Ok(answer) => answer,
        Err(why) => {
            // A TIMEOUT IS NOT AN EMPTY RULESET. Both leave the prompt without a
            // block, and the turn proceeds either way — so without this the
            // difference between "nothing matched" and "the store never
            // answered in time" is invisible for as long as it keeps happening.
            crate::trace!(
                "memory",
                "rules(turn): no answer in {:?}: {why}",
                began.elapsed()
            );
            return None;
        }
    };
    let ruleset = parse(&answer).ok()?;
    crate::trace!(
        "memory",
        "rules(turn): {} matched in {:?}",
        ruleset.rules.len(),
        began.elapsed()
    );
    as_context(&ruleset)
}

/// The rules that apply to EVERY turn, for the session's cached prefix.
///
/// # Why these are separate from the per-turn block
///
/// The service returns two sets: rules matched to the message, and rules marked
/// `always`. Measured against four different queries, the matched set differed
/// every time — so it has to travel with the message. The `always` set was
/// identical in all four, so sending it per turn would buy nothing and be paid
/// for every time.
///
/// It was being parsed and then dropped: `Ruleset::always` had no reader
/// outside a test, so these rules never reached the agent at all. In the cached
/// prefix they arrive once and cost almost nothing thereafter.
#[must_use]
pub fn always_context(ruleset: &Ruleset) -> Option<String> {
    if ruleset.always.is_empty() {
        return None;
    }
    let mut out = String::from(
        "The owner's standing rules that apply to EVERY action, not just this one. \
         They are instructions, not context, and are not optional:\n",
    );
    for rule in &ruleset.always {
        out.push_str("\n- ");
        out.push_str(rule.trim());
        out.push('\n');
    }
    Some(out)
}

/// Fetch the always-rules once, for the session.
///
/// Given a longer deadline than a turn gets: this runs at startup, alongside
/// the other opening work, and is paid once rather than on every message.
#[must_use]
pub fn session_context() -> Option<String> {
    let began = std::time::Instant::now();
    // FAILURE AND EMPTINESS SAY DIFFERENT THINGS. Both end as no block in the
    // prompt, and for a long time the only visible difference was that the
    // agent behaved oddly — the always-rules were parsed and dropped for weeks
    // without a word. Each step reports for itself.
    let answer = match get_within(&rules_url("session start"), SESSION_TIMEOUT) {
        Ok(answer) => answer,
        Err(why) => {
            crate::trace!(
                "memory",
                "rules: store did not answer in {:?}: {why}",
                began.elapsed()
            );
            return None;
        }
    };
    let ruleset = match parse(&answer) {
        Ok(ruleset) => ruleset,
        Err(why) => {
            crate::trace!("memory", "rules: answer did not parse: {why}");
            return None;
        }
    };
    crate::trace!(
        "memory",
        "rules: {} always-on of {} in {:?}",
        ruleset.always.len(),
        ruleset.rules.len(),
        began.elapsed()
    );
    always_context(&ruleset)
}

/// Render the rules MATCHED to this message, as the block the model reads.
///
/// Stays per-turn because it genuinely varies: measured against four different
/// queries, the matched set differed every time. Only [`always_context`] is
/// constant enough to live in the cached prefix.
#[must_use]
pub fn as_context(ruleset: &Ruleset) -> Option<String> {
    if ruleset.is_empty() {
        return None;
    }
    let mut out = String::from(
        "The owner's standing rules for this work. Unlike recalled context, these \
         ARE instructions and are not optional — follow every one. They come from \
         the owner's own store and are shared by every agent on this machine:\n",
    );
    for rule in &ruleset.rules {
        out.push_str("\n- ");
        out.push_str(rule.trim());
        out.push('\n');
    }
    Some(out)
}

/// Percent-escape a query value.
///
/// Hand-rolled rather than another dependency: the set is small and fixed, and the
/// only thing that goes through it is one line of the user's own text.
fn query_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn get(url: &str) -> Result<String, String> {
    get_within(url, TIMEOUT)
}

/// The same request, with the caller's own deadline.
pub(crate) fn get_within(url: &str, timeout: Duration) -> Result<String, String> {
    let url = url.to_string();
    crate::runtime::http::request_text(
        move |client| client.get(&url).timeout(timeout),
        "rule store",
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape the running service actually returned, with the rule text cut
    /// short. Written against the real wire format rather than an imagined one.
    const REAL_BODY: &str = r#"{"always":["GLOBAL RULE 19 - Diagnosis discipline"],
        "complete":false,"matched":1,
        "rules":["GLOBAL RULE 15 - Test in the project's own stack",
                 "GLOBAL RULE 19 - Diagnosis discipline"]}"#;

    #[test]
    fn the_services_own_rule_shape_parses() {
        let set = parse(REAL_BODY).expect("the recorded body parses");
        assert_eq!(set.rules.len(), 2);
        assert_eq!(set.always.len(), 1);
        assert!(!set.complete);
    }

    /// `rules` is the union the service already computed. Rendering `always`
    /// separately would print the always-on rules twice.
    #[test]
    fn the_block_carries_the_union_the_service_computed() {
        let set = parse(REAL_BODY).expect("parses");
        let block = as_context(&set).expect("a block");
        assert_eq!(
            block.matches("GLOBAL RULE 19").count(),
            1,
            "an always-on rule must appear once, not once per list it is in"
        );
        assert!(block.contains("GLOBAL RULE 15"));
    }

    /// The one retrieved block the model is meant to obey has to say so. Recalled
    /// conversations and recorded outputs are data; these are not.
    #[test]
    fn the_block_says_these_are_instructions_unlike_everything_else_retrieved() {
        let block = as_context(&Ruleset {
            rules: vec!["a rule".to_string()],
            always: Vec::new(),
            complete: true,
        })
        .expect("a block");
        assert!(block.contains("ARE instructions"), "got: {block}");
        assert!(block.contains("not optional"), "got: {block}");
    }

    /// A store with no rules yet is a normal state, not an error.
    #[test]
    fn no_rules_produces_no_block() {
        let set = parse(r#"{"rules":[],"always":[],"complete":true}"#).expect("parses");
        assert!(set.is_empty());
        assert!(as_context(&set).is_none());
    }

    #[test]
    fn unknown_fields_do_not_break_the_loader() {
        let set = parse(r#"{"rules":["r"],"always":["r"],"complete":true,"future":{"a":1}}"#)
            .expect("parses");
        assert_eq!(set.rules, vec!["r".to_string()]);
    }

    #[test]
    fn a_malformed_answer_is_reported_rather_than_panicking() {
        assert!(parse("not json").is_err());
    }

    /// The request must go where the service answers, and the user's own words go
    /// into a query string — a line with `&` in it would otherwise truncate the
    /// query and silently ask about something else.
    #[test]
    fn the_request_goes_to_the_endpoint_that_answered() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::brief::MEMORY_URL_ENV, "http://127.0.0.1:9180/");
        let url = rules_url("fix auth & ship");
        std::env::remove_var(crate::brief::MEMORY_URL_ENV);
        assert_eq!(
            url,
            "http://127.0.0.1:9180/v1/memory/rules/for?top_k=4&query=fix%20auth%20%26%20ship"
        );
    }

    /// An unreachable store costs the block, never the turn: the rules are also
    /// enforced by hooks and permission tiers, which do not call this.
    #[test]
    fn an_unreachable_store_costs_the_block_not_the_turn() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::brief::MEMORY_URL_ENV, "http://127.0.0.1:1");
        let block = context_for_turn("anything");
        std::env::remove_var(crate::brief::MEMORY_URL_ENV);
        assert!(block.is_none());
    }
}
