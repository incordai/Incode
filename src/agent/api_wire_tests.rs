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
    }
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
    assert_eq!(body["system"], "be brief");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(parse_reply(&raw).expect("parsed").text, "hello");
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
