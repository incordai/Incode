//! What actually goes over the wire, read by a server on this machine: the
//! path, the one header the key travels in, the version header, and the body
//! in the endpoint's own shape.

use super::*;
use std::io::{Read, Write};

/// Serve ONE request on a fresh local port and answer it with `reply` (a
/// complete body, sent as JSON or as an event stream). The request comes back
/// whole — head and body — from the join handle.
fn serve_once(reply: &'static str, stream: bool) -> (String, std::thread::JoinHandle<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        let head_end = loop {
            if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            let n = conn.read(&mut buf).expect("read");
            if n == 0 {
                break request.len();
            }
            request.extend_from_slice(&buf[..n]);
        };
        let head = String::from_utf8_lossy(&request[..head_end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while request.len() < head_end + length {
            let n = conn.read(&mut buf).expect("read body");
            if n == 0 {
                break;
            }
            request.extend_from_slice(&buf[..n]);
        }
        let kind = if stream {
            "text/event-stream"
        } else {
            "application/json"
        };
        let answer = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
            reply.len()
        );
        conn.write_all(answer.as_bytes()).expect("write");
        String::from_utf8_lossy(&request).to_string()
    });
    (format!("http://{addr}"), server)
}

fn http(endpoint: String, format: WireFormat, auth: KeyHeader) -> Http {
    Http {
        endpoint,
        key: "k-123".to_string(),
        format,
        auth,
        timeout: std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        api_version: DEFAULT_API_VERSION.to_string(),
        retries: DEFAULT_MAX_RETRIES,
        prompt_cache: true,
    }
}

/// Serve one connection per reply, in order: each `(status, extra headers,
/// body)`. Returns the address and a count of the requests that came in.
fn serve_in_turn(
    replies: Vec<(u16, &'static str, &'static str)>,
) -> (
    String,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    std::thread::JoinHandle<()>,
) {
    use std::sync::atomic::Ordering;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = seen.clone();
    let server = std::thread::spawn(move || {
        for (status, headers, body) in replies {
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 8192];
            let mut request = Vec::new();
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match conn.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            counted.fetch_add(1, Ordering::SeqCst);
            let answer = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{headers}\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = conn.write_all(answer.as_bytes());
        }
    });
    (format!("http://{addr}"), seen, server)
}

/// **A BUSY SERVICE IS ASKED AGAIN.** A 429 with `Retry-After: 0`, then a 529,
/// then the answer: the turn gets the answer, after exactly three requests.
#[test]
fn a_busy_answer_is_retried_until_the_service_answers() {
    let (base, seen, server) = serve_in_turn(vec![
        (429, "retry-after: 0\r\n", r#"{"error":"slow down"}"#),
        (529, "retry-after: 0\r\n", r#"{"error":"overloaded"}"#),
        (200, "", MESSAGES_REPLY),
    ]);
    let transport = http(
        format!("{base}/v1/messages"),
        WireFormat::Anthropic,
        KeyHeader::XApiKey,
    );
    let raw = transport.send(&prompt_body()).expect("answered in the end");
    server.join().expect("server");
    assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(parse_reply(&raw).expect("parsed").text, "hello");
}

/// A request that is WRONG is not retried — a 400 would be the same 400 —
/// and when retries run out the error says how many were made.
#[test]
fn a_wrong_request_is_not_retried_and_running_out_says_so() {
    let (base, seen, server) = serve_in_turn(vec![(400, "", r#"{"error":"bad field"}"#)]);
    let transport = http(
        format!("{base}/v1/messages"),
        WireFormat::Anthropic,
        KeyHeader::XApiKey,
    );
    let why = transport.send(&prompt_body()).expect_err("refused");
    server.join().expect("server");
    assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(why.contains("400") && why.contains("bad field"), "{why}");
    assert!(!why.contains("retries"), "{why}");

    let (base, seen, server) = serve_in_turn(vec![
        (503, "retry-after: 0\r\n", "busy"),
        (503, "retry-after: 0\r\n", "still busy"),
    ]);
    let transport = Http {
        retries: 1,
        ..http(
            format!("{base}/v1/messages"),
            WireFormat::Anthropic,
            KeyHeader::XApiKey,
        )
    };
    let why = transport.send(&prompt_body()).expect_err("gave up");
    server.join().expect("server");
    assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(
        why.contains("after 1 retries") && why.contains("still busy"),
        "{why}"
    );
}

/// The wait is the service's `Retry-After` when it gives one, else 1, 2, 4
/// seconds — and never more than the cap, whatever it asks for.
#[test]
fn the_wait_between_retries_is_bounded() {
    use std::time::Duration;
    assert_eq!(retry_wait(0, None), Duration::from_secs(1));
    assert_eq!(retry_wait(2, None), Duration::from_secs(4));
    assert_eq!(retry_wait(0, Some("2.5")), Duration::from_millis(2500));
    assert_eq!(retry_wait(0, Some("86400")), MAX_RETRY_WAIT);
    assert_eq!(retry_wait(9, None), MAX_RETRY_WAIT);
    assert_eq!(retry_wait(1, Some("soon")), Duration::from_secs(2));
    assert!(is_retryable(429) && is_retryable(503) && is_retryable(529));
    assert!(!is_retryable(400) && !is_retryable(401) && !is_retryable(500));
}

fn prompt_body() -> Value {
    request_body(
        "the-model",
        "be brief",
        &[json!({"role": "user", "content": "hi"})],
        &[json!({"name": "Read", "description": "read", "input_schema": {"type": "object"}})],
    )
}

fn head_and_body(request: &str) -> (String, Value) {
    let (head, body) = request.split_once("\r\n\r\n").expect("a head and a body");
    (
        head.to_ascii_lowercase(),
        serde_json::from_str(body).expect("the body is JSON"),
    )
}

const MESSAGES_REPLY: &str =
    r#"{"content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","model":"m"}"#;

/// **The messages format sends the version header it requires, and the key
/// in `x-api-key` ONLY.** A request without the version header is refused by
/// the format's own service; a key in a second header is a key handed where
/// nobody asked for it.
#[test]
fn the_messages_format_sends_its_version_and_only_its_own_key_header() {
    let (base, server) = serve_once(MESSAGES_REPLY, false);
    let transport = http(
        format!("{base}/v1/messages"),
        WireFormat::Anthropic,
        KeyHeader::XApiKey,
    );
    let raw = transport.send(&prompt_body()).expect("sent");
    let (head, body) = head_and_body(&server.join().expect("server"));
    assert!(head.starts_with("post /v1/messages http/1.1"), "{head}");
    assert!(head.contains("anthropic-version: 2023-06-01"), "{head}");
    assert!(head.contains("x-api-key: k-123"), "{head}");
    assert!(
        !head.contains("authorization:"),
        "the key went in a second header: {head}"
    );
    assert_eq!(body["system"][0]["text"], "be brief");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(parse_reply(&raw).expect("parsed").text, "hello");
}

/// **A TOOL LOOP READS ITS PREFIX FROM CACHE.** Every round re-sends the
/// whole conversation, so the messages format marks the tools, the system
/// prompt and the newest message as cache breakpoints — and an entry that
/// turns `prompt_cache` off sends the request exactly as built.
#[test]
fn the_messages_format_marks_its_cache_breakpoints_unless_told_not_to() {
    let (base, server) = serve_once(MESSAGES_REPLY, false);
    let transport = http(
        format!("{base}/v1/messages"),
        WireFormat::Anthropic,
        KeyHeader::XApiKey,
    );
    transport.send(&prompt_body()).expect("sent");
    let (_, body) = head_and_body(&server.join().expect("server"));
    let ephemeral = json!({"type": "ephemeral"});
    assert_eq!(body["tools"][0]["cache_control"], ephemeral, "{body}");
    assert_eq!(body["system"][0]["cache_control"], ephemeral, "{body}");
    assert_eq!(body["messages"][0]["content"][0]["text"], "hi", "{body}");
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"], ephemeral,
        "{body}"
    );

    let (base, server) = serve_once(MESSAGES_REPLY, false);
    let transport = Http {
        prompt_cache: false,
        ..http(
            format!("{base}/v1/messages"),
            WireFormat::Anthropic,
            KeyHeader::XApiKey,
        )
    };
    transport.send(&prompt_body()).expect("sent");
    let (_, body) = head_and_body(&server.join().expect("server"));
    assert_eq!(body, prompt_body(), "sent as built");
}

/// A tool result is the usual newest message: its LAST block takes the mark,
/// earlier blocks and earlier messages are left as they were.
#[test]
fn only_the_newest_block_of_the_conversation_is_marked() {
    let body = request_body(
        "m",
        "",
        &[
            json!({"role": "user", "content": "first"}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "a", "content": "one"},
                {"type": "tool_result", "tool_use_id": "b", "content": "two"},
            ]}),
        ],
        &[],
    );
    let marked = with_cache_marks(&body);
    assert_eq!(marked["messages"][0]["content"], "first");
    let blocks = &marked["messages"][1]["content"];
    assert!(blocks[0].get("cache_control").is_none(), "{marked}");
    assert_eq!(blocks[1]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        marked["system"], body["system"],
        "an empty system stays as it was"
    );
}

/// **The version header is the entry's, when it names one** (`api_version`):
/// a service that moves to a newer version is a change to admin.json, not to
/// this code.
#[test]
fn an_entrys_own_version_is_the_one_sent() {
    let (base, server) = serve_once(MESSAGES_REPLY, false);
    let transport = Http {
        api_version: "2099-01-01".to_string(),
        ..http(
            format!("{base}/v1/messages"),
            WireFormat::Anthropic,
            KeyHeader::XApiKey,
        )
    };
    transport.send(&prompt_body()).expect("sent");
    let (head, _) = head_and_body(&server.join().expect("server"));
    assert!(head.contains("anthropic-version: 2099-01-01"), "{head}");
    assert!(!head.contains(DEFAULT_API_VERSION), "{head}");
}

/// A service speaking the messages format for token-based tools reads the key
/// as a bearer — sent there, and only there.
#[test]
fn the_messages_format_can_carry_the_key_as_a_bearer_instead() {
    let (base, server) = serve_once(MESSAGES_REPLY, false);
    let transport = http(
        format!("{base}/v1/messages"),
        WireFormat::Anthropic,
        KeyHeader::Bearer,
    );
    transport.send(&prompt_body()).expect("sent");
    let (head, _) = head_and_body(&server.join().expect("server"));
    assert!(head.contains("authorization: bearer k-123"), "{head}");
    assert!(!head.contains("x-api-key"), "{head}");
    assert!(head.contains("anthropic-version: 2023-06-01"), "{head}");
}

/// **The chat-completions format goes out in its own shape and comes back in
/// the loop's.** The system prompt is the first message, the tools are
/// functions, the key is a bearer; a tool call in the reply is a call the
/// loop can run.
#[test]
fn the_chat_format_is_translated_both_ways_over_the_wire() {
    let reply = r#"{"model":"served","choices":[{"finish_reason":"tool_calls","message":{"content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"Read","arguments":"{\"path\":\"a.txt\"}"}}]}}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#;
    let (base, server) = serve_once(reply, false);
    let transport = http(
        format!("{base}/v1/chat/completions"),
        WireFormat::Openai,
        KeyHeader::Bearer,
    );
    let raw = transport.send(&prompt_body()).expect("sent");
    let (head, body) = head_and_body(&server.join().expect("server"));
    assert!(
        head.starts_with("post /v1/chat/completions http/1.1"),
        "{head}"
    );
    assert!(head.contains("authorization: bearer k-123"), "{head}");
    assert!(!head.contains("anthropic-version"), "{head}");
    assert!(!head.contains("x-api-key"), "{head}");
    assert!(body.get("system").is_none(), "{body}");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["content"], "hi");
    assert_eq!(body["tools"][0]["function"]["name"], "Read");
    assert_eq!(body["model"], "the-model");
    let parsed = parse_reply(&raw).expect("parsed");
    assert_eq!(parsed.calls[0].name, "Read");
    assert_eq!(parsed.calls[0].input, json!({"path": "a.txt"}));
    assert_eq!(parsed.model, "served");
}

/// A streamed chat reply is shown as it arrives and read as one reply.
#[test]
fn a_streamed_chat_reply_arrives_in_pieces_and_is_read_whole() {
    let events = concat!(
        "data: {\"model\":\"served\",\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (base, server) = serve_once(events, true);
    let transport = http(
        format!("{base}/v1/chat/completions"),
        WireFormat::Openai,
        KeyHeader::Bearer,
    );
    let mut shown = String::new();
    let raw = transport
        .send_streaming(&prompt_body(), &mut |t| shown.push_str(t))
        .expect("sent");
    let (_, body) = head_and_body(&server.join().expect("server"));
    assert_eq!(body["stream"], true);
    assert_eq!(shown, "Hello");
    let parsed = parse_reply(&raw).expect("parsed");
    assert_eq!(parsed.text, "Hello");
    assert_eq!(parsed.stop_reason, "end_turn");
}

/// **No key goes over plain http to another machine**, whichever route built
/// the backend: the turn fails before a request is made, naming the rule.
#[test]
fn a_turn_to_plain_http_on_another_machine_is_refused_before_sending() {
    let dir = tempfile::tempdir().expect("tmp");
    let mut backend = ApiBackend::new(
        Settings {
            endpoint: "http://example.invalid/v1/messages".to_string(),
            key: "k".to_string(),
            route: "router".to_string(),
            ..Default::default()
        },
        dir.path(),
        "",
        true,
    );
    let err = backend.run_turn("hi", &mut |_| {}).expect_err("refused");
    assert!(err.to_string().contains("https"), "{err}");
    assert!(err.to_string().contains("router"), "{err}");
    assert_eq!(
        backend.id(),
        "router",
        "the backend answers to its own route"
    );
}

/// A route with a default model, a sub-agents' model and aliases, as a
/// service speaking the messages format for token-based tools documents it.
fn aliased_route() -> Settings {
    Settings {
        endpoint: "https://service.invalid/v1/messages".to_string(),
        key: "k".to_string(),
        model: "main-model".to_string(),
        sub_model: "fast-model".to_string(),
        aliases: std::collections::BTreeMap::from([
            ("opus".to_string(), "main-model".to_string()),
            ("haiku".to_string(), "fast-model".to_string()),
        ]),
        route: "service".to_string(),
        ..Default::default()
    }
}

fn backend_on(settings: Settings) -> (tempfile::TempDir, ApiBackend) {
    let dir = tempfile::tempdir().expect("tmp");
    let backend = ApiBackend::new(settings, dir.path(), "", true);
    (dir, backend)
}

/// **A ROLE TABLE WRITTEN FOR EVERY AGENT DOES NOT BREAK AN API ROUTE.**
/// The owner's `plan` candidates are CLI names and other vendors' ids; the
/// route takes the first it serves — `opus` through its alias — and a list it
/// serves none of leaves the default in hand instead of sending `opus` to a
/// service that has no such model. Revert `set_models` to "take the first"
/// and this fails.
#[test]
fn a_role_tables_candidates_are_taken_only_as_the_route_serves_them() {
    let plan = [
        "opus".to_string(),
        "gemini-3.1-pro-high".to_string(),
        "gpt-5.6-terra".to_string(),
    ];
    let (_dir, mut backend) = backend_on(aliased_route());
    backend.set_models(&["haiku".to_string(), "sonnet".to_string()]);
    assert_eq!(backend.model(), Some("fast-model"), "haiku's alias");
    backend.set_models(&plan);
    assert_eq!(backend.model(), Some("main-model"), "opus's alias");

    let (_dir, mut bare) = backend_on(Settings {
        aliases: std::collections::BTreeMap::new(),
        ..aliased_route()
    });
    bare.set_models(&plan);
    assert_eq!(
        bare.model(),
        Some("main-model"),
        "nothing served: the default stands"
    );
    bare.set_models(&["sonnet".to_string(), "fast-model".to_string()]);
    assert_eq!(
        bare.model(),
        Some("fast-model"),
        "its own sub-agents' model is served"
    );
}

#[test]
fn a_qualified_candidate_is_taken_only_for_its_own_route() {
    let route = aliased_route();
    assert_eq!(route.serves("service:any-id").as_deref(), Some("any-id"));
    assert_eq!(route.serves("service:haiku").as_deref(), Some("fast-model"));
    assert_eq!(route.serves("claude-code:opus"), None, "another agent's");
    assert_eq!(
        route.serves("opus@lab-a").as_deref(),
        Some("main-model"),
        "a family tag"
    );
    assert_eq!(route.serves(""), None);
}

/// `/model` and `-m` take any id the service offers, through the aliases.
#[test]
fn a_model_named_outright_is_taken_whatever_it_is() {
    let (_dir, mut backend) = backend_on(aliased_route());
    backend.choose_model("some-other-model");
    assert_eq!(backend.model(), Some("some-other-model"));
    backend.choose_model("OPUS");
    assert_eq!(backend.model(), Some("main-model"));
}
