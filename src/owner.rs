//! Asking the owner, and waiting for the answer.
//!
//! The channel already exists in the body: a card is queued, the proxy reviews it
//! as the owner, and either answers it or escalates to the person. There is no
//! approval mechanism to build here and no second queue to keep — only a card to
//! raise and a verdict to read.
//!
//! # Why the card is raised as a guard
//!
//! The proxy refuses outright to auto-decide a card whose source is `guard`
//! (`proxy::gate` in the memory service). That is exactly what the runner wants:
//! it escalates only when it has reached something a machine must not settle, and
//! a proxy quietly approving those is the failure mode both of the earlier
//! autonomous loops died of — a question raised, auto-answered, and the agent told
//! the owner had approved.
//!
//! # What a verdict may and may not do
//!
//! It may decide whether to keep trying. It may not decide that work is finished.
//! Nothing here can produce a [`crate::verify::Verified`], so no answer from this
//! channel — the owner's included — can move a task to `Done`. That edge stays
//! gated on a command that exited zero, which is the whole point of it.

use std::time::Duration;

/// Where a card is raised.
pub const ENQUEUE_PATH: &str = "/v1/owner/enqueue";

/// Where the answer is waited for.
pub const WAIT_PATH: &str = "/v1/owner/wait";

/// Where decisions already made are read back.
pub const HISTORY_PATH: &str = "/v1/owner/history";

/// How far back to look for an answer already given.
///
/// Bounded because this runs before every escalation. Recent decisions are the
/// ones worth reusing; an approval from six months and a rewrite ago is not a
/// standing answer, it is an artefact.
pub const HISTORY_LIMIT: u32 = 100;

/// The source that keeps the vote with the person.
///
/// Not `"forward"`, `"continuation"` or `"handoff"`, all of which the proxy is
/// entitled to answer silently. A runner that escalated on one of those would be
/// told "approved" by a program and report it as the owner's decision.
pub const GUARD_SOURCE: &str = "guard";

/// Long enough for a person to reach a phone, short enough that a wedged runner
/// is noticed. The service caps its own wait at 300s regardless.
pub const WAIT_SECONDS: u64 = 120;

/// Bounded, because raising a card must not hang the runner.
const TIMEOUT: Duration = Duration::from_secs(10);

/// A card raised and the token that will carry its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub token: String,
    pub item_id: String,
}

/// What came back.
///
/// Five verbs, because the service defines five (`chat_bridge::OwnerVerb`):
/// `approve`, `dismiss`, `hold`, `info` and `change:<note>`. Reading only three
/// of them and erroring on the rest would stop a run on a perfectly ordinary
/// answer, and would silently discard the one verb that exists to give the agent
/// its next instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nobody has answered inside the wait. The card stays live.
    Pending,
    /// Go ahead. Never a statement that the work is correct — only that the
    /// question is settled.
    Approved { by: String, reason: String },
    /// **Do it differently, and here is how.** The service's own words: the note
    /// "carries a free-text instruction the agent's next pass reads as a round".
    /// It is the one verb that hands work back to the coding agent, so the note
    /// has to reach the agent and not merely the log.
    Change {
        note: String,
        by: String,
        reason: String,
    },
    /// Drop it.
    Dismissed { by: String, reason: String },
    /// Parked. Answered, the token is spent, and the answer is "not now" — so
    /// there is nothing to retry and nothing to drop.
    Held { by: String, reason: String },
    /// "Show me more." Read-only, and deliberately does NOT consume the token in
    /// the service, so the card stays live and the question is still open.
    Info { by: String, reason: String },
}

impl Verdict {
    /// One line for the ledger's note, so the record says who decided and why.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Pending => "no answer yet".to_string(),
            Self::Approved { by, reason } => join("approved", by, reason),
            Self::Change { note, by, reason } => {
                let head = join("change", by, reason);
                format!("{head}: {note}")
            }
            Self::Dismissed { by, reason } => join("dismissed", by, reason),
            Self::Held { by, reason } => join("held", by, reason),
            Self::Info { by, reason } => join("more detail asked for", by, reason),
        }
    }

    /// Who applied it. Empty while nothing has been decided.
    ///
    /// Worth carrying separately from the verb: the proxy deciding alone reads
    /// very differently from the owner deciding, and a report that says only
    /// "approved" hides which of the two happened.
    #[must_use]
    pub fn decided_by(&self) -> &str {
        match self {
            Self::Pending => "",
            Self::Approved { by, .. }
            | Self::Change { by, .. }
            | Self::Dismissed { by, .. }
            | Self::Held { by, .. }
            | Self::Info { by, .. } => by,
        }
    }
}

fn join(verb: &str, by: &str, reason: &str) -> String {
    let who = if by.is_empty() { "unknown" } else { by };
    if reason.trim().is_empty() {
        format!("{verb} by {who}")
    } else {
        format!("{verb} by {who} — {}", reason.trim())
    }
}

/// Raise a card for a decision only a person should make.
///
/// `severity` is clamped by the service to 1..=5 and decides whether a closed
/// phone is buzzed, so it is passed through rather than fixed here.
pub fn ask(title: &str, detail: &str, project: &str, severity: u8) -> Result<Ticket, String> {
    if title.trim().is_empty() {
        return Err("a card with no title is rejected by the service".to_string());
    }
    let body = serde_json::json!({
        "title": title,
        "detail": detail,
        "severity": severity,
        "source": GUARD_SOURCE,
        "project": project,
    })
    .to_string();

    let answer = post(&url(ENQUEUE_PATH), body)?;
    let value: serde_json::Value = serde_json::from_str(&answer)
        .map_err(|e| format!("the card's receipt did not parse: {e}"))?;
    let token = value
        .get("token")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    if token.is_empty() {
        // Without a token there is nothing to wait on, so treating this as
        // success would leave the runner waiting on an answer that can never be
        // delivered to it.
        return Err("the card was accepted without a token, so no answer can be read".to_string());
    }
    Ok(Ticket {
        token,
        item_id: value
            .get("item_id")
            .and_then(|i| i.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

/// Wait for the answer to a card.
pub fn wait(ticket: &Ticket, seconds: u64) -> Result<Verdict, String> {
    parse_verdict(&get(&wait_url(ticket, seconds))?)
}

/// An answer already given to this exact question, if there is one.
///
/// **Only `approve` and `dismiss` are durable.** Those settle whether the work
/// should happen, and asking twice wastes the owner's attention on a decision
/// they already made. `change` is an instruction for one attempt, `hold` means
/// "not now" and says nothing about later, and `info` is not an answer at all —
/// reusing any of the three would apply an old reply to a new situation.
///
/// The match is exact on title and project. A question phrased differently is a
/// different question, and loosening this is how a stale approval starts
/// covering work nobody approved.
pub fn past_verdict(title: &str, project: &str) -> Result<Option<Verdict>, String> {
    let url = format!("{}{HISTORY_PATH}?limit={HISTORY_LIMIT}", base());
    find_past_verdict(&get(&url)?, title, project)
}

/// Pick a reusable answer out of the decision history.
pub fn find_past_verdict(
    body: &str,
    title: &str,
    project: &str,
) -> Result<Option<Verdict>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("the history did not parse: {e}"))?;
    let Some(rows) = value.get("decided").and_then(|d| d.as_array()) else {
        return Ok(None);
    };
    for row in rows {
        let field = |key: &str| row.get(key).and_then(|v| v.as_str()).unwrap_or_default();
        if field("title") != title {
            continue;
        }
        // The service files a card under a project NAME while the runner asks
        // with a workspace PATH, so the two are compared by suffix rather than
        // by equality — and never by "contains", which would let one project's
        // approval answer for another whose name it happens to contain.
        if !same_project(field("project"), project) {
            continue;
        }
        if !row
            .get("consumed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let by = field("decided_by").to_string();
        let reason = field("proxy_note").to_string();
        match field("verb") {
            "approve" => return Ok(Some(Verdict::Approved { by, reason })),
            "dismiss" => return Ok(Some(Verdict::Dismissed { by, reason })),
            // Deliberately not reused — see the doc comment.
            _ => continue,
        }
    }
    Ok(None)
}

/// Whether a card's project and a workspace path name the same project.
fn same_project(card: &str, asked: &str) -> bool {
    if card.is_empty() || asked.is_empty() {
        return false;
    }
    let normal = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_lowercase();
    let (card, asked) = (normal(card), normal(asked));
    card == asked
        || asked.rsplit('/').next().is_some_and(|leaf| leaf == card)
        || card.rsplit('/').next().is_some_and(|leaf| leaf == asked)
}

/// Raise the card and wait for it in one step.
pub fn ask_and_wait(
    title: &str,
    detail: &str,
    project: &str,
    severity: u8,
    seconds: u64,
) -> Result<Verdict, String> {
    wait(&ask(title, detail, project, severity)?, seconds)
}

/// The URL a verdict is read from.
///
/// Pinned by a test: the contract came from reading the service's own handler,
/// and a parser tested alone would pass while the request went nowhere.
#[must_use]
pub fn wait_url(ticket: &Ticket, seconds: u64) -> String {
    format!(
        "{}{WAIT_PATH}?token={}&timeout_secs={seconds}",
        crate::brief::memory_url().trim_end_matches('/'),
        ticket.token.trim()
    )
}

fn url(path: &str) -> String {
    format!("{}{path}", base())
}

fn base() -> String {
    crate::brief::memory_url().trim_end_matches('/').to_string()
}

/// Read what the owner or the proxy said.
///
/// `decided: false` is a real answer meaning "not yet", and is deliberately not an
/// error: the runner may wait again, and treating a quiet minute as a failure
/// would abandon work nobody had refused.
pub fn parse_verdict(body: &str) -> Result<Verdict, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("the verdict did not parse: {e}"))?;
    if !value
        .get("decided")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(Verdict::Pending);
    }
    let field = |key: &str| -> String {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let by = field("by");
    let reason = field("reason");
    match field("verb").as_str() {
        "approve" => Ok(Verdict::Approved { by, reason }),
        "change" => Ok(Verdict::Change {
            note: field("note"),
            by,
            reason,
        }),
        "dismiss" => Ok(Verdict::Dismissed { by, reason }),
        "hold" => Ok(Verdict::Held { by, reason }),
        "info" => Ok(Verdict::Info { by, reason }),
        // An unrecognised verb is NOT read as approval. A verb this crate does
        // not know is a decision it cannot act on, and guessing which way it
        // pointed is how an agent ends up reporting a refusal as a go-ahead.
        other => Err(format!(
            "the verdict `{other}` is not one this harness knows how to act on"
        )),
    }
}

fn post(url: &str, body: String) -> Result<String, String> {
    let url = url.to_string();
    request(move |client| {
        client
            .post(&url)
            .header("content-type", "application/json")
            .body(body)
    })
}

fn get(url: &str) -> Result<String, String> {
    let url = url.to_string();
    request(move |client| client.get(&url))
}

/// One place that owns the runtime, the timeout and the status check.
fn request<F>(build: F) -> Result<String, String>
where
    F: FnOnce(&reqwest::Client) -> reqwest::RequestBuilder + Send + 'static,
{
    crate::runtime::http::request_text(
        move |client| build(client).timeout(TIMEOUT + Duration::from_secs(WAIT_SECONDS)),
        "owner channel",
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> Ticket {
        Ticket {
            token: "ab12cd".to_string(),
            item_id: "item-1".to_string(),
        }
    }

    /// The body the running service actually returned for a real card raised by
    /// this harness and answered by a person — not a shape written from the
    /// handler and hoped to match it. Field order as received.
    const REAL_VERDICT: &str = r#"{"by":"owner","decided":true,"note":"",
        "reason":"recommends dismiss - self-test of the escalation channel",
        "verb":"approve"}"#;

    /// A round trip that really happened: card raised, proxy reviewed, person
    /// answered, verdict read back.
    #[test]
    fn a_real_answered_card_parses_the_way_the_runner_will_read_it() {
        let verdict = parse_verdict(REAL_VERDICT).expect("the observed body parses");
        assert_eq!(
            verdict,
            Verdict::Approved {
                by: "owner".to_string(),
                reason: "recommends dismiss - self-test of the escalation channel".to_string(),
            }
        );
        assert_eq!(
            verdict.decided_by(),
            "owner",
            "a person answering and the proxy answering must not read alike"
        );
    }

    /// The shapes the service's own handlers emit, read from its source rather
    /// than imagined: `{ok, token, item_id}` from enqueue, and a verdict carrying
    /// `decided`, `verb`, `note`, `reason` and `by`.
    #[test]
    fn the_services_own_verdict_shapes_parse() {
        assert_eq!(
            parse_verdict(r#"{"decided":false}"#).expect("parses"),
            Verdict::Pending
        );
        assert_eq!(
            parse_verdict(
                r#"{"decided":true,"verb":"approve","note":"","reason":"routine","by":"proxy"}"#
            )
            .expect("parses"),
            Verdict::Approved {
                by: "proxy".to_string(),
                reason: "routine".to_string()
            }
        );
        assert_eq!(
            parse_verdict(
                r#"{"decided":true,"verb":"change","note":"use the npm check","reason":"","by":"owner"}"#
            )
            .expect("parses"),
            Verdict::Change {
                note: "use the npm check".to_string(),
                by: "owner".to_string(),
                reason: String::new()
            }
        );
    }

    /// **All five verbs the service defines, read off the wire.** `hold` and
    /// `info` were missing at first and fell through to "unknown verdict", which
    /// stopped a run and blamed the service for words it defines itself.
    #[test]
    fn every_verb_the_service_can_send_is_understood() {
        let wire = |verb: &str| {
            format!(r#"{{"decided":true,"verb":"{verb}","note":"n","reason":"r","by":"owner"}}"#)
        };
        for verb in ["approve", "dismiss", "hold", "info", "change"] {
            let parsed = parse_verdict(&wire(verb));
            assert!(parsed.is_ok(), "`{verb}` must be understood: {parsed:?}");
        }
        assert!(matches!(
            parse_verdict(&wire("hold")).expect("parses"),
            Verdict::Held { .. }
        ));
        assert!(matches!(
            parse_verdict(&wire("info")).expect("parses"),
            Verdict::Info { .. }
        ));
    }

    /// The refusal has to reach the runner as a refusal. A verb this crate does
    /// not recognise must never fall through to "carry on" — that is how an agent
    /// reports a rejection as a go-ahead.
    #[test]
    fn an_unknown_verb_is_an_error_and_never_an_approval() {
        let parsed = parse_verdict(r#"{"decided":true,"verb":"escalate-to-legal","by":"owner"}"#);
        assert!(parsed.is_err(), "got: {parsed:?}");

        // And the control: the same body with a known verb does resolve, so the
        // error above is about the verb and not about the body.
        assert!(parse_verdict(r#"{"decided":true,"verb":"dismiss","by":"owner"}"#).is_ok());
    }

    /// Who decided is part of the answer. A proxy approving alone and the owner
    /// approving are different events, and a note saying only "approved" hides
    /// which one happened — the exact confusion behind both earlier loop failures.
    #[test]
    fn the_record_says_who_decided_not_only_what() {
        let by_proxy = parse_verdict(
            r#"{"decided":true,"verb":"approve","reason":"inside the brief","by":"proxy"}"#,
        )
        .expect("parses");
        assert_eq!(by_proxy.decided_by(), "proxy");
        assert!(
            by_proxy.summary().contains("proxy"),
            "{}",
            by_proxy.summary()
        );
        assert!(
            by_proxy.summary().contains("inside the brief"),
            "the reason travels with the verdict: {}",
            by_proxy.summary()
        );
    }

    /// A card with no title is rejected by the service; catching it here saves a
    /// round trip and gives the caller the real reason.
    #[test]
    fn a_card_needs_a_title() {
        assert!(ask("   ", "detail", "proj", 3).is_err());
    }

    #[test]
    fn a_malformed_verdict_is_reported_rather_than_panicking() {
        assert!(parse_verdict("not json").is_err());
    }

    /// The request must go where the service answers.
    #[test]
    fn the_wait_goes_to_the_endpoint_that_answers() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::brief::MEMORY_URL_ENV, "http://127.0.0.1:9180/");
        let waiting = wait_url(&ticket(), 30);
        let raising = url(ENQUEUE_PATH);
        std::env::remove_var(crate::brief::MEMORY_URL_ENV);
        assert_eq!(
            waiting,
            "http://127.0.0.1:9180/v1/owner/wait?token=ab12cd&timeout_secs=30"
        );
        assert_eq!(raising, "http://127.0.0.1:9180/v1/owner/enqueue");
    }

    /// An unreachable channel is an error, not a silent approval. Every other
    /// memory call in this crate fails open; this one must not.
    #[test]
    fn an_unreachable_owner_channel_never_reads_as_approval() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::brief::MEMORY_URL_ENV, "http://127.0.0.1:1");
        let raised = ask("a question", "detail", "proj", 3);
        let waited = wait(&ticket(), 1);
        std::env::remove_var(crate::brief::MEMORY_URL_ENV);
        assert!(raised.is_err());
        assert!(waited.is_err());
    }

    /// The exact history shape the running service returns, trimmed to the
    /// fields this reads.
    const REAL_HISTORY: &str = r#"{"decided":[
        {"title":"Runner blocked on `t-1`","project":"incord-code","verb":"approve",
         "decided_by":"owner","proxy_note":"docs only","consumed":true},
        {"title":"Runner blocked on `t-2`","project":"incord-code","verb":"change",
         "decided_by":"owner","proxy_note":"","consumed":true},
        {"title":"Runner blocked on `t-3`","project":"incord-code","verb":"dismiss",
         "decided_by":"proxy","proxy_note":"outside the brief","consumed":true},
        {"title":"Runner blocked on `t-4`","project":"incord-code","verb":"hold",
         "decided_by":"owner","proxy_note":"","consumed":true},
        {"title":"Runner blocked on `t-5`","project":"incord-code","verb":"approve",
         "decided_by":"owner","proxy_note":"","consumed":false}
    ]}"#;

    /// **R4: a settled decision is not asked twice.** An approval already given
    /// for this exact question comes straight back.
    #[test]
    fn a_settled_answer_is_reused_instead_of_re_asked() {
        let found = find_past_verdict(REAL_HISTORY, "Runner blocked on `t-1`", "incord-code")
            .expect("parses");
        assert_eq!(
            found,
            Some(Verdict::Approved {
                by: "owner".to_string(),
                reason: "docs only".to_string()
            })
        );

        let dismissed = find_past_verdict(REAL_HISTORY, "Runner blocked on `t-3`", "incord-code")
            .expect("parses");
        assert!(matches!(dismissed, Some(Verdict::Dismissed { .. })));
    }

    /// **Only settled answers are durable.** `change` was an instruction for one
    /// attempt, `hold` said "not now" and says nothing about now, and an
    /// unconsumed row was never answered at all. Reusing any of them would apply
    /// an old reply to a new situation.
    #[test]
    fn an_answer_about_one_attempt_is_never_reused_as_policy() {
        for (title, why) in [
            ("Runner blocked on `t-2`", "a change is for one attempt"),
            ("Runner blocked on `t-4`", "a hold is not a standing answer"),
            (
                "Runner blocked on `t-5`",
                "an unconsumed card was never decided",
            ),
        ] {
            assert_eq!(
                find_past_verdict(REAL_HISTORY, title, "incord-code").expect("parses"),
                None,
                "{why}"
            );
        }
    }

    /// A different question is a different question. Loosening this is how a
    /// stale approval starts covering work nobody approved.
    #[test]
    fn a_different_question_does_not_inherit_an_old_answer() {
        assert_eq!(
            find_past_verdict(REAL_HISTORY, "Runner blocked on `t-99`", "incord-code")
                .expect("parses"),
            None
        );
        assert_eq!(
            find_past_verdict(
                REAL_HISTORY,
                "Runner blocked on `t-1`",
                "some-other-project"
            )
            .expect("parses"),
            None,
            "another project must not inherit this one's approvals"
        );
    }

    /// The service files a card under a project NAME while the runner asks with
    /// a workspace PATH; the two must still match, and a project whose name is
    /// merely contained in another must not.
    #[test]
    fn a_workspace_path_matches_the_card_it_was_filed_under() {
        assert!(same_project("incord-code", "C:/Projects/incord-code"));
        assert!(same_project("incord-code", r"C:\Projects\incord-code"));
        assert!(same_project("C:/Projects/incord-code", "incord-code"));
        assert!(!same_project("code", "C:/Projects/incord-code"));
        assert!(!same_project(
            "incord-code",
            "C:/Projects/incord-code-backup"
        ));
        assert!(!same_project("", "incord-code"));
    }

    /// An empty or unreadable history answers nothing, and must not be mistaken
    /// for a decision.
    #[test]
    fn an_unreadable_history_is_not_an_answer() {
        assert_eq!(find_past_verdict(r#"{}"#, "t", "p").expect("parses"), None);
        assert!(find_past_verdict("not json", "t", "p").is_err());
    }
}
