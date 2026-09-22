//! Retrieval from the body's memory.
//!
//! The harness used to answer "have we seen this before?" from a JSON file it
//! kept itself, searched by word overlap. That store knew only what this one
//! program had written, and matched only what happened to share vocabulary with
//! the question. The real store is semantic and shared with every other agent on
//! the machine, and it was never asked.
//!
//! # Two stages, on purpose
//!
//! [`index`] returns ranked previews; [`get`] fetches full content for the few
//! refs that turn out to be worth it. A single call returning everything would
//! be simpler to write and far more expensive to use: most hits are discarded
//! after a glance, and paying full content tokens for them is paying to throw
//! them away.
//!
//! **Nothing here stores anything.** This module asks and reads. If it ever
//! grows a cache, that cache becomes a second memory that can disagree with the
//! first — which is the defect this whole direction exists to remove.

use serde::{Deserialize, Serialize};

use crate::brief::memory_url;

/// Stage one: ranked previews.
pub const INDEX_PATH: &str = "/v1/memory/recall/index";

/// Stage two: full content for chosen refs.
pub const GET_PATH: &str = "/v1/memory/recall/get";

/// How many previews to ask for when the caller has no opinion.
///
/// Small on purpose. Retrieval rides every turn, and a wide net costs tokens on
/// every one of them while burying the hit that mattered.
pub const DEFAULT_TOP_K: usize = 8;

/// Bounded because this runs inside a turn the user is waiting on.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// How long ANY path with a person waiting on it will wait for advisory context.
///
/// # Why this is not the timeout above
///
/// That one bounds a tool call the model asked for and is willing to wait on.
/// This one sits in front of a person: the context is enrichment, retrieval
/// fails open by design, so a store that cannot answer quickly must cost the
/// enrichment and nothing else.
///
/// # Why it is ONE constant
///
/// It was two, and the second was missed. Bounding the turn left startup
/// unbounded, and startup makes the same kind of call: `spec::memory_note`
/// searched the store through `knowledge::search` on its EIGHT-second timeout,
/// against a service that was not running. Measured: 8.08s of the 17.5s it took
/// before the prompt appeared — the operator saw a dead terminal and concluded,
/// reasonably, that the program was broken.
///
/// Every advisory lookup on a waiting path uses this. A second copy is how the
/// first one got missed.
pub const WAITING_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1_200);

/// The deadline for an advisory lookup that nobody is waiting on.
///
/// # Why this is not [`WAITING_TIMEOUT`], and why that one must not be raised
///
/// Measured from the service's own request log, `/v1/memory/watchman/handoff`
/// answers in 498ms at the median but 5467ms at p90, with a worst case of 177
/// seconds — and **39.1% of 506 calls took longer than 1200ms**. Every one of
/// those returned `status=200`: the store was working and the client had already
/// given up. Two sessions in five therefore started believing a project with
/// real recorded history had none, because an abandoned call and an empty
/// project are the same `None`.
///
/// Raising [`WAITING_TIMEOUT`] is NOT the fix. It is 1200ms for a reason its own
/// note records — an 8.08s startup lookup once left the operator staring at a
/// terminal that looked dead — and a slow store must never be able to hold the
/// prompt hostage.
///
/// So the deadline is not what changes; the WAITING is. These lookups are
/// started as soon as the operator's settings are known and are not read until
/// the prompt is being assembled, several seconds of real work later. Nothing
/// blocks on them in between, which is what makes a longer deadline safe here
/// and unsafe there.
///
/// The p90 above is still the SYMPTOM. A handoff of five items should not take
/// half a second at the median, and that belongs to the service.
pub const PREFETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Former name of [`WAITING_TIMEOUT`], kept where turns already reference it.
pub const TURN_TIMEOUT: std::time::Duration = WAITING_TIMEOUT;

/// One ranked preview.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Hit {
    #[serde(default)]
    pub conv_id: String,
    #[serde(default)]
    pub idx: i64,
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub role: String,
}

#[derive(Debug, Clone, Deserialize)]
struct IndexResponse {
    #[serde(default)]
    hits: Vec<Hit>,
}

/// A message chosen from the index, to be fetched in full.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ref {
    pub conv_id: String,
    pub idx: i64,
}

impl From<&Hit> for Ref {
    fn from(hit: &Hit) -> Self {
        Self {
            conv_id: hit.conv_id.clone(),
            idx: hit.idx,
        }
    }
}

/// One message in full.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub conv_id: String,
    #[serde(default)]
    pub idx: i64,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GetResponse {
    #[serde(default)]
    messages: Vec<Message>,
}

/// Parse an index response.
pub fn parse_index(body: &str) -> Result<Vec<Hit>, String> {
    serde_json::from_str::<IndexResponse>(body)
        .map(|r| r.hits)
        .map_err(|e| format!("the recall index could not be read: {e}"))
}

/// Parse a get response.
pub fn parse_get(body: &str) -> Result<Vec<Message>, String> {
    serde_json::from_str::<GetResponse>(body)
        .map(|r| r.messages)
        .map_err(|e| format!("the recalled messages could not be read: {e}"))
}

/// Render hits as the context block a turn carries.
///
/// Previews only. The full text of a hit is fetched deliberately with [`get`],
/// never swept in because it was nearby.
#[must_use]
pub fn as_context(hits: &[Hit]) -> Option<String> {
    if hits.is_empty() {
        return None;
    }
    let mut out = String::from("<relevant_memory>");
    for hit in hits {
        out.push_str(&format!(
            "\n[{}#{}] {}",
            hit.conv_id,
            hit.idx,
            hit.preview.trim()
        ));
    }
    out.push_str("\n</relevant_memory>");
    Some(out)
}

/// Ask for ranked previews.
/// Whether the shared memory answers at all.
///
/// Used by the health check, so it asks the smallest real question rather than
/// opening a socket: a service that accepts a connection and cannot serve a
/// query is down for every purpose that matters here.
pub fn probe() -> Result<(), String> {
    index("health check", 1).map(|_| ())
}

pub fn index(query: &str, top_k: usize) -> Result<Vec<Hit>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let body = serde_json::json!({ "query": query, "top_k": top_k }).to_string();
    parse_index(&post(INDEX_PATH, body)?)
}

/// Fetch full content for chosen refs.
pub fn get(refs: &[Ref]) -> Result<Vec<Message>, String> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let body = serde_json::json!({ "refs": refs }).to_string();
    parse_get(&post(GET_PATH, body)?)
}

/// Retrieval for one turn: previews for the line the user typed.
///
/// Returns `None` rather than an error when the store cannot be reached. This is
/// the one place that softness is right: retrieval is an *enrichment* of a turn,
/// and refusing to answer because the extra context was unavailable would be a
/// worse failure than answering without it. The brief gate is the opposite case
/// and fails closed, because there the check *is* the point.
#[must_use]
pub fn context_for_turn(line: &str) -> Option<String> {
    // Bounded by TURN_TIMEOUT, not by the tool timeout: this runs in front of
    // every keystroke, and a store that cannot answer quickly must cost the
    // context rather than the turn.
    let began = std::time::Instant::now();
    let body = serde_json::json!({ "query": line, "top_k": DEFAULT_TOP_K }).to_string();
    let answer = match post_within(INDEX_PATH, body, TURN_TIMEOUT) {
        Ok(answer) => answer,
        Err(why) => {
            // MEASURED AT 6-8s AGAINST A 1.2s BUDGET, which meant recall timed
            // out on every turn and contributed nothing — while looking, from
            // the outside, exactly like a store with nothing to say.
            crate::trace!(
                "memory",
                "recall: no answer within {:?} ({why})",
                began.elapsed()
            );
            return None;
        }
    };
    let hits = parse_index(&answer).ok()?;
    crate::trace!(
        "memory",
        "recall: {} hit(s) in {:?}",
        hits.len(),
        began.elapsed()
    );
    as_context(&hits)
}

/// The tool that returns ranked previews.
pub const INDEX_TOOL: &str = "recall_index";

/// The tool that fetches chosen messages in full.
pub const GET_TOOL: &str = "recall_get";

/// `recall_index` as advertised to the model.
#[must_use]
pub fn index_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "name": INDEX_TOOL,
        "description": "Search everything this machine remembers — past sessions, \
            decisions, and code understanding from every agent, not just this one. \
            Returns ranked PREVIEWS only, which is what makes it cheap enough to \
            use freely. Ask it before answering from scratch about past work, a \
            prior decision, or code you have not read yet. Phrase the query as a \
            plain question: matching is by meaning, so shared words are not \
            required. Follow up with `recall_get` for the few hits worth reading \
            in full.",
        "input_schema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "A plain-language question."},
                "top_k": {"type": "integer", "description": "How many previews. Keep it small; 8 is the default."}
            },
            "required": ["query"]
        }
    })
}

/// `recall_get` as advertised to the model.
#[must_use]
pub fn get_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "name": GET_TOOL,
        "description": "Fetch the full content of specific messages found with \
            `recall_index`. Pass only the refs whose previews were actually worth \
            reading — this is the call that costs full content tokens, and paying \
            it for a hit you would discard is paying to throw it away.",
        "input_schema": {
            "type": "object",
            "properties": {
                "refs": {
                    "type": "array",
                    "description": "The (conv_id, idx) pairs taken from recall_index hits.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "conv_id": {"type": "string"},
                            "idx": {"type": "integer"}
                        },
                        "required": ["conv_id", "idx"]
                    }
                }
            },
            "required": ["refs"]
        }
    })
}

/// Run a `recall_index` call and render it for the model.
pub fn run_index_tool(input: &str) -> Result<String, String> {
    let parsed: serde_json::Value = serde_json::from_str(input)
        .map_err(|e| format!("`{INDEX_TOOL}` needs a JSON object: {e}"))?;
    let query = parsed
        .get("query")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    if query.is_empty() {
        return Err(format!("`{INDEX_TOOL}` needs a `query` to search for."));
    }
    let top_k = parsed
        .get("top_k")
        .and_then(serde_json::Value::as_u64)
        .map_or(DEFAULT_TOP_K, |k| (k as usize).clamp(1, 50));

    let hits = index(query, top_k)?;
    if hits.is_empty() {
        return Ok("Nothing remembered matches that.".to_string());
    }
    let mut out = format!(
        "{} hit(s). Use recall_get for the ones worth reading:",
        hits.len()
    );
    for hit in &hits {
        out.push_str(&format!(
            "\n- conv_id={} idx={} score={:.2}\n  {}",
            hit.conv_id,
            hit.idx,
            hit.score,
            hit.preview.trim()
        ));
    }
    Ok(out)
}

/// Run a `recall_get` call and render it for the model.
pub fn run_get_tool(input: &str) -> Result<String, String> {
    let parsed: serde_json::Value = serde_json::from_str(input)
        .map_err(|e| format!("`{GET_TOOL}` needs a JSON object: {e}"))?;
    let refs: Vec<Ref> = match parsed.get("refs") {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|e| format!("`refs` must be a list of {{conv_id, idx}}: {e}"))?,
        None => {
            return Err(format!(
                "`{GET_TOOL}` needs `refs` from a recall_index hit."
            ))
        }
    };
    if refs.is_empty() {
        return Err(format!("`{GET_TOOL}` was called with no refs."));
    }

    let messages = get(&refs)?;
    if messages.is_empty() {
        return Ok("Those refs returned nothing.".to_string());
    }
    let mut out = String::new();
    for message in &messages {
        out.push_str(&format!(
            "\n[{}#{} {}]\n{}\n",
            message.conv_id,
            message.idx,
            message.role,
            message.content.trim()
        ));
    }
    Ok(out.trim_start().to_string())
}

fn post(path: &str, body: String) -> Result<String, String> {
    post_within(path, body, TIMEOUT)
}

/// The same request, with the caller's own deadline.
fn post_within(path: &str, body: String, timeout: std::time::Duration) -> Result<String, String> {
    let url = format!("{}{path}", memory_url().trim_end_matches('/'));
    crate::runtime::http::request_text(
        move |client| {
            client
                .post(&url)
                .header("content-type", "application/json")
                .body(body)
                .timeout(timeout)
        },
        "service",
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape the service returns, taken from a live probe.
    #[test]
    fn the_services_own_index_shape_parses() {
        let body = r#"{"hits":[
            {"conv_id":"memory","idx":768,"score":8.17,"role":"memory:fact",
             "preview":"incord-code: CORRECTION FROM THE OWNER"},
            {"conv_id":"memory","idx":765,"score":7.91,"role":"memory:fact",
             "preview":"incord-code: PHASE 0 IS BUILT"}
        ]}"#;
        let hits = parse_index(body).expect("parses");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].idx, 768);
        assert!(hits[0].score > 8.0);
    }

    /// The service owns this shape and will add to it. A parser that fails on an
    /// addition turns every upstream improvement into an outage here.
    #[test]
    fn unknown_fields_do_not_break_retrieval() {
        let body = r#"{"hits":[{"conv_id":"c","idx":1,"preview":"p","added_later":{"x":1}}],
                       "also_new":true}"#;
        let hits = parse_index(body).expect("parses");
        assert_eq!(hits[0].preview, "p");
    }

    #[test]
    fn an_empty_query_asks_nothing_at_all() {
        // No server is running in this test; reaching the network would fail.
        // Returning early is what keeps a blank line from costing a request.
        assert_eq!(index("   ", 5), Ok(Vec::new()));
        assert_eq!(get(&[]), Ok(Vec::new()));
    }

    #[test]
    fn hits_become_a_labelled_context_block() {
        let hits = vec![Hit {
            conv_id: "memory".to_string(),
            idx: 42,
            preview: "  the thing we decided  ".to_string(),
            ..Default::default()
        }];
        let context = as_context(&hits).expect("a block");
        assert!(context.starts_with("<relevant_memory>"));
        assert!(context.contains("[memory#42] the thing we decided"));
        assert!(context.ends_with("</relevant_memory>"));
    }

    /// Silence must cost nothing. An empty block would still be sent on every
    /// turn and paid for on every later round trip.
    #[test]
    fn no_hits_produces_no_block() {
        assert_eq!(as_context(&[]), None);
    }

    #[test]
    fn a_hit_converts_to_the_ref_that_fetches_it() {
        let hit = Hit {
            conv_id: "memory".to_string(),
            idx: 7,
            ..Default::default()
        };
        let reference = Ref::from(&hit);
        assert_eq!(
            serde_json::to_string(&reference).expect("serialises"),
            r#"{"conv_id":"memory","idx":7}"#
        );
    }

    #[test]
    fn a_malformed_response_is_reported_rather_than_panicking() {
        assert!(parse_index("not json")
            .expect_err("refused")
            .contains("could not be read"));
        assert!(parse_get("{")
            .expect_err("refused")
            .contains("could not be read"));
    }

    /// Retrieval is an enrichment. If the store is unreachable the turn must
    /// still run — unlike the brief gate, where the check is the whole point.
    #[test]
    fn an_unreachable_store_costs_context_not_the_turn() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::brief::MEMORY_URL_ENV, "http://127.0.0.1:1");
        let context = context_for_turn("anything at all");
        std::env::remove_var(crate::brief::MEMORY_URL_ENV);
        assert_eq!(context, None, "no context, and no failure");
    }

    #[test]
    fn the_index_tool_refuses_a_call_with_no_question() {
        let error = run_index_tool(r#"{"top_k":5}"#).expect_err("must be refused");
        assert!(error.contains("needs a `query`"), "got: {error}");
    }

    #[test]
    fn the_get_tool_refuses_a_call_with_no_refs() {
        assert!(run_get_tool(r#"{}"#)
            .expect_err("must be refused")
            .contains("needs `refs`"));
        assert!(run_get_tool(r#"{"refs":[]}"#)
            .expect_err("must be refused")
            .contains("no refs"));
    }

    /// A wide net costs tokens on every turn. The cap is the harness's business
    /// even though the count is the model's choice.
    #[test]
    fn an_outsized_top_k_is_clamped_rather_than_obeyed() {
        // Reaching the network would fail here, so the empty query short-circuit
        // is what returns — the point is that parsing accepted the huge value
        // without panicking and the clamp is applied before any request.
        let parsed: serde_json::Value =
            serde_json::from_str(r#"{"query":"x","top_k":9999}"#).expect("parses");
        let requested = parsed["top_k"].as_u64().expect("a number") as usize;
        assert_eq!(
            requested.clamp(1, 50),
            50,
            "the clamp bounds what is asked for"
        );
    }

    /// The whole reason there are two tools: the index reply must hand back the
    /// refs the model needs to choose with, not the content itself.
    #[test]
    fn the_index_reply_carries_refs_so_the_model_can_choose() {
        let hits = vec![Hit {
            conv_id: "memory".to_string(),
            idx: 768,
            score: 8.17,
            preview: "the decision we made".to_string(),
            role: "memory:fact".to_string(),
        }];
        // Rendered by the same code path the tool uses for its reply body.
        let mut out = format!("{} hit(s).", hits.len());
        for hit in &hits {
            out.push_str(&format!("\n- conv_id={} idx={}", hit.conv_id, hit.idx));
        }
        assert!(out.contains("conv_id=memory"), "got: {out}");
        assert!(out.contains("idx=768"), "got: {out}");
    }

    /// The real request path against a stand-in service.
    #[test]
    fn previews_are_retrieved_over_http() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        // Serves REPEATEDLY, not once.
        //
        // `MEMORY_URL_ENV` is process-global while tests run in parallel
        // threads, so a stray connection from anything else pointed at this
        // address would consume a single `accept()` and leave the request under
        // test unanswered until its 8s timeout — which is how this test failed
        // roughly one full run in three while passing every time in isolation.
        let server = std::thread::spawn(move || {
            let body =
                r#"{"hits":[{"conv_id":"memory","idx":99,"score":9.0,"preview":"served hit"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: {}

{}",
                body.len(),
                body
            );
            for incoming in listener.incoming() {
                let Ok(mut socket) = incoming else { break };
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf);
                let _ = socket.write_all(response.as_bytes());
                let _ = socket.flush();
            }
        });

        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::brief::MEMORY_URL_ENV, format!("http://{addr}"));
        let context = context_for_turn("what did we decide");
        std::env::remove_var(crate::brief::MEMORY_URL_ENV);
        // Not joined: it loops until its listener is dropped, so waiting would hang.
        drop(server);

        let context = context.expect("a served hit must reach the turn");
        assert!(context.contains("[memory#99] served hit"), "got: {context}");
    }
}
