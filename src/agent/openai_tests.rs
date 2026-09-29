use super::*;

/// A conversation in the messages shape with every kind of block the loop
/// writes: a prompt with a picture, a reply that calls a tool, and the result
/// — failed, with a picture.
fn conversation() -> Value {
    json!({
        "model": "some-model",
        "max_tokens": 100,
        "system": "be brief",
        "stream": true,
        "tools": [{"name": "Read", "description": "read a file",
                   "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}}],
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAA"}}
            ]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "reading"},
                {"type": "tool_use", "id": "c1", "name": "Read", "input": {"path": "a.txt"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "c1", "is_error": true, "content": [
                    {"type": "text", "text": "no such file"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBB"}}
                ]}
            ]},
            {"role": "user", "content": "plain"}
        ]
    })
}

#[test]
fn a_request_carries_system_tools_and_every_message_across() {
    let out = request(&conversation());
    assert_eq!(out["model"], "some-model");
    assert_eq!(out["max_tokens"], 100);
    assert_eq!(out["stream"], true);
    assert_eq!(out["tools"][0]["type"], "function");
    assert_eq!(out["tools"][0]["function"]["name"], "Read");
    assert_eq!(out["tools"][0]["function"]["parameters"]["type"], "object");

    let messages = out["messages"].as_array().expect("messages");
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        roles,
        ["system", "user", "assistant", "tool", "user", "user"],
        "{out:#}"
    );
    assert_eq!(messages[0]["content"], "be brief");
    assert_eq!(
        messages[1]["content"][1]["image_url"]["url"],
        "data:image/png;base64,AAA"
    );
    // The call, with its arguments as the JSON string the format wants.
    let call = &messages[2]["tool_calls"][0];
    assert_eq!(call["id"], "c1");
    assert_eq!(call["function"]["name"], "Read");
    let arguments: Value =
        serde_json::from_str(call["function"]["arguments"].as_str().expect("string"))
            .expect("json");
    assert_eq!(arguments, json!({"path": "a.txt"}));
    assert_eq!(messages[2]["content"], "reading");
    // A failed result says so, and its picture follows as the user's.
    assert_eq!(messages[3]["tool_call_id"], "c1");
    assert_eq!(messages[3]["content"], "ERROR: no such file");
    assert_eq!(
        messages[4]["content"][1]["image_url"]["url"],
        "data:image/jpeg;base64,BBB"
    );
    assert_eq!(messages[5]["content"], "plain");
}

/// No tools is no `tools` key: an empty list is refused by some services.
#[test]
fn a_request_with_no_tools_sends_no_tools_key() {
    let out = request(&json!({"messages": [{"role": "user", "content": "hi"}], "tools": []}));
    assert!(out.get("tools").is_none(), "{out}");
    assert!(out.get("model").is_none(), "a blank model is not invented");
}

/// A reply that only calls has `content: null`, as the format expects.
#[test]
fn a_reply_that_only_calls_sends_null_content() {
    let out = request(&json!({"messages": [{"role": "assistant", "content": [
        {"type": "tool_use", "id": "c", "name": "LS", "input": {}}
    ]}]}));
    assert!(out["messages"][0]["content"].is_null(), "{out}");
}

#[test]
fn a_completion_reads_as_text_calls_stop_and_usage() {
    let body = json!({
        "model": "served-model",
        "choices": [{"finish_reason": "tool_calls", "message": {
            "content": "on it",
            "tool_calls": [{"id": "t1", "type": "function",
                            "function": {"name": "Read", "arguments": "{\"path\":\"b.txt\"}"}}]
        }}],
        "usage": {"prompt_tokens": 50, "completion_tokens": 7,
                  "prompt_tokens_details": {"cached_tokens": 20}}
    })
    .to_string();
    let parsed = crate::agent::api::parse_reply(&reply(&body).expect("converted")).expect("parsed");
    assert_eq!(parsed.text, "on it");
    assert_eq!(parsed.calls.len(), 1);
    assert_eq!(parsed.calls[0].id, "t1");
    assert_eq!(parsed.calls[0].input, json!({"path": "b.txt"}));
    assert_eq!(parsed.stop_reason, "tool_use");
    assert_eq!(parsed.model, "served-model");
    assert_eq!(
        parsed.input_tokens, 30,
        "cached tokens are counted once, apart"
    );
    assert_eq!(parsed.cache_read_tokens, 20);
    assert_eq!(parsed.output_tokens, 7);
}

#[test]
fn finish_reasons_map_to_the_messages_shape() {
    for (finish, expected) in [
        ("stop", "end_turn"),
        ("length", "max_tokens"),
        ("", "end_turn"),
    ] {
        let body = json!({"choices": [{"finish_reason": finish, "message": {"content": "x"}}]});
        let parsed =
            crate::agent::api::parse_reply(&reply(&body.to_string()).expect("ok")).expect("parsed");
        assert_eq!(parsed.stop_reason, expected, "{finish}");
    }
}

/// Arguments that are not JSON reach the tool as they came, so its schema
/// check says what was wrong rather than an empty call running.
#[test]
fn arguments_that_are_not_json_are_passed_on_not_emptied() {
    let body = json!({"choices": [{"finish_reason": "tool_calls", "message": {"tool_calls": [
        {"id": "t", "function": {"name": "Read", "arguments": "{broken"}}
    ]}}]});
    let parsed =
        crate::agent::api::parse_reply(&reply(&body.to_string()).expect("ok")).expect("parsed");
    assert_eq!(parsed.calls[0].input, json!("{broken"));
}

/// The service's own error message reaches the operator.
#[test]
fn an_error_body_is_reported_with_its_message() {
    let body = r#"{"error":{"message":"No credits left","code":402}}"#;
    let err = crate::agent::api::parse_reply(&reply(body).expect("passed through"))
        .expect_err("an error");
    assert_eq!(err, "No credits left");
}

#[test]
fn a_body_that_is_not_a_completion_is_an_error() {
    assert!(reply(r#"{"id":"x"}"#).is_err());
    assert!(reply("not json").is_err());
}

/// **A STREAM REBUILDS THE SAME REPLY.** Text arrives in pieces and is shown
/// as it comes; a call's arguments arrive split across events and across
/// network chunks, and only the whole of them is parsed.
#[test]
fn a_stream_is_reassembled_into_one_reply() {
    let events = [
        r#"data: {"model":"m-1","choices":[{"delta":{"role":"assistant","content":"Hel"}}]}"#,
        r#"data: {"choices":[{"delta":{"content":"lo"}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c9","function":{"name":"Read","arguments":"{\"pa"}}]}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"x\"}"}}]}}]}"#,
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":9,"completion_tokens":4}}"#,
        "data: [DONE]",
    ];
    let whole = events.join("\n\n") + "\n";
    let mut stream = ChatStream::new();
    let mut shown = String::new();
    // Fed in awkward pieces, as a network hands them over.
    for piece in whole.as_bytes().chunks(17) {
        stream.feed(&String::from_utf8_lossy(piece), &mut |t| shown.push_str(t));
    }
    assert!(stream.saw_events());
    assert_eq!(shown, "Hello");
    let parsed =
        crate::agent::api::parse_reply(&stream.finish().expect("finished")).expect("parsed");
    assert_eq!(parsed.text, "Hello");
    assert_eq!(parsed.calls[0].id, "c9");
    assert_eq!(parsed.calls[0].input, json!({"path": "x"}));
    assert_eq!(parsed.stop_reason, "tool_use");
    assert_eq!(parsed.model, "m-1");
    assert_eq!((parsed.input_tokens, parsed.output_tokens), (9, 4));
}

/// An error sent in the middle of a stream is the turn's error, not an empty
/// reply that reads like a model with nothing to say.
#[test]
fn an_error_in_a_stream_is_reported() {
    let mut stream = ChatStream::new();
    stream.feed(
        "data: {\"error\":{\"message\":\"rate limited\"}}\n",
        &mut |_| {},
    );
    let err =
        crate::agent::api::parse_reply(&stream.finish().expect("finished")).expect_err("an error");
    assert_eq!(err, "rate limited");
}
