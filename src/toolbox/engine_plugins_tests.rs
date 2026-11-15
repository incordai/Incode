//! The memory engine's plugin tool servers, through the toolbox, against a
//! stand-in engine that answers the way the real one was measured to
//! (2026-10-30): the index from `incord_describe {}`, a server's tools from
//! `incord_describe {"name": "plugin:<server>"}`, a call's MCP result from
//! `incord_call`, and a refusal as a 400 with `{"error": …}`.

use super::*;
use crate::toolbox::{SessionTools, Toolbox};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::sync::{Arc, Mutex};

const INDEX: &str = r#"{"tools": [], "plugin_tools": [
    {"name": "plugin:greptile", "category": "coding", "description": "Code review"},
    {"name": "plugin:github", "category": "coding", "description": "Repositories"},
    {"name": "plugin:web", "category": "testing", "description": "Kept by a local plugin"},
    {"name": "plugin:bad name", "category": "x", "description": "Not a plain word"},
    {"name": "plugin:greptile", "category": "coding", "description": "Listed twice"},
    {"name": "memory_search", "description": "A memory tool, not a server"}]}"#;

const GREPTILE_TOOLS: &str = r#"{"tools": [{"name": "greptile.get_me",
    "description": "Who is calling", "inputSchema": {"type": "object", "properties": {}}}]}"#;

const UNAUTHORIZED: &str = r#"{"error": "https://api.github.example/mcp answered 401 Unauthorized: missing required Authorization header"}"#;

/// Every request the stand-in engine was sent: its path and its body.
type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// A stand-in engine on a free port: every request's path and body are kept,
/// and each is answered by `answer`. It serves until the test ends.
fn engine(answer: fn(&str, &Value) -> (u16, String)) -> (String, Seen) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&seen);
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut socket) = incoming else { break };
            let mut reader = BufReader::new(socket.try_clone().expect("clone"));
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
            let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                }
            }
            let mut body = vec![0u8; length];
            let _ = reader.read_exact(&mut body);
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let (status, reply) = answer(&path, &body);
            kept.lock().expect("seen").push((path, body));
            let _ = write!(
                socket,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{reply}",
                reply.len()
            );
            let _ = socket.flush();
        }
    });
    (format!("http://{addr}"), seen)
}

/// The stand-in's answers: greptile describes and runs, github is not signed in.
fn measured(path: &str, body: &Value) -> (u16, String) {
    let name = body.get("name").and_then(Value::as_str);
    match (path, name) {
        ("/v1/memory/tools/incord_describe", None) => (200, INDEX.to_string()),
        ("/v1/memory/tools/incord_describe", Some("plugin:greptile")) => {
            (200, GREPTILE_TOOLS.to_string())
        }
        ("/v1/memory/tools/incord_call", Some("greptile.get_me")) => (
            200,
            r#"{"content": [{"type": "text", "text": "you are the owner"}], "isError": false}"#
                .to_string(),
        ),
        ("/v1/memory/tools/incord_call", Some("greptile.refuses")) => (
            200,
            r#"{"content": [{"type": "text", "text": "no such repository"}], "isError": true}"#
                .to_string(),
        ),
        _ => (400, UNAUTHORIZED.to_string()),
    }
}

/// A toolbox whose session was offered `index`.
fn toolbox(ws: &std::path::Path, index: Option<Index>) -> Toolbox {
    let state = SessionTools::new(true);
    if let Some(index) = index {
        state.set_engine_plugins(index);
    }
    let mut tools = Toolbox::new(ws, true);
    tools.share_session(state);
    tools
}

/// **THE ENGINE'S PLUGIN SERVERS REACH THE MODEL AS TWO TOOLS, THROUGH THE
/// GATE**: the index names each server once — not one a local plugin runs,
/// not a name that is not a plain word — `PluginTools` lists a server's
/// tools, `PluginCall` runs one with its arguments and hands back its
/// answer, a tool's own refusal is an error with its words, and a server the
/// engine cannot sign in to is said to be BLOCKED ON SIGN-IN.
#[test]
fn the_engines_plugin_servers_are_offered_and_run_through_the_toolbox() {
    let (url, seen) = engine(measured);
    let ws = tempfile::tempdir().expect("ws");
    let _guard = crate::brief::ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::set_var(crate::brief::MEMORY_URL_ENV, &url);

    let index = fetch_index(&["web".to_string()]).expect("an index");
    let names: Vec<&str> = index.servers.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["greptile", "github"]);

    let tools = toolbox(ws.path(), Some(index));
    let offered = tools.names();
    assert!(offered.iter().any(|n| n == PLUGIN_TOOLS), "{offered:?}");
    assert!(offered.iter().any(|n| n == PLUGIN_CALL), "{offered:?}");
    let described = tools
        .definitions()
        .into_iter()
        .find(|d| d["name"] == PLUGIN_TOOLS)
        .expect("PluginTools");
    let text = described["description"].as_str().unwrap_or_default();
    assert!(text.contains("- greptile [coding]: Code review"), "{text}");
    assert!(!text.contains("- web"), "{text}");

    let listed = tools
        .run(PLUGIN_TOOLS, &json!({"server": "greptile"}))
        .expect("listed");
    assert!(listed.contains("## greptile.get_me"), "{listed}");
    assert!(listed.contains("Who is calling"), "{listed}");

    let ran = tools
        .run(
            PLUGIN_CALL,
            &json!({"tool": "greptile.get_me", "arguments": {"org": "x"}}),
        )
        .expect("ran");
    assert_eq!(ran, "you are the owner");

    let refused = tools
        .run(PLUGIN_CALL, &json!({"tool": "greptile.refuses"}))
        .expect_err("the tool refused");
    assert!(refused.contains("no such repository"), "{refused}");

    let blocked = tools
        .run(PLUGIN_CALL, &json!({"tool": "github.get_me"}))
        .expect_err("not signed in");
    assert!(blocked.starts_with("BLOCKED ON SIGN-IN"), "{blocked}");
    assert!(blocked.contains("`github`"), "{blocked}");

    let unknown = tools
        .run(PLUGIN_CALL, &json!({"tool": "web.anything"}))
        .expect_err("not in the index");
    assert!(unknown.contains("no plugin tool server `web`"), "{unknown}");

    std::env::remove_var(crate::brief::MEMORY_URL_ENV);

    let seen = seen.lock().expect("seen");
    let call = seen
        .iter()
        .find(|(path, body)| path.ends_with("incord_call") && body["name"] == "greptile.get_me")
        .expect("the call reached the engine");
    assert_eq!(call.1["arguments"], json!({"org": "x"}));
    assert!(
        !seen.iter().any(|(_, body)| body["name"] == "web.anything"),
        "a server outside the index was asked for"
    );
}

/// **WITHOUT THE ENGINE'S INDEX THE TWO TOOLS ARE NOT OFFERED**, and a model
/// that calls one anyway is told why.
#[test]
fn without_an_index_the_plugin_tools_are_not_offered() {
    let ws = tempfile::tempdir().expect("ws");
    let tools = toolbox(ws.path(), None);
    let offered = tools.names();
    assert!(
        !offered
            .iter()
            .any(|n| n == PLUGIN_TOOLS || n == PLUGIN_CALL),
        "{offered:?}"
    );
    let refused = tools
        .run(PLUGIN_TOOLS, &json!({"server": "greptile"}))
        .expect_err("not offered");
    assert!(refused.contains("not available"), "{refused}");
}

/// **AN ENGINE WITH NO PLUGIN SERVERS, OR NONE AT ALL, OFFERS NOTHING** —
/// and an answer without the index's shape is an error, not an empty index.
#[test]
fn no_servers_or_no_engine_means_no_index() {
    let _guard = crate::brief::ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::set_var(crate::brief::MEMORY_URL_ENV, "http://127.0.0.1:1");
    assert_eq!(fetch_index(&[]), None);
    std::env::remove_var(crate::brief::MEMORY_URL_ENV);

    assert_eq!(
        parse_index(r#"{"tools": [], "plugin_tools": []}"#, &[]).expect("parses"),
        Index::default()
    );
    assert!(parse_index(r#"{"tools": []}"#, &[]).is_err());
}

/// The index in the description is bounded, and what it leaves out is
/// counted.
#[test]
fn the_index_is_bounded_and_counts_the_rest() {
    let index = Index {
        servers: (0..MAX_INDEXED + 3)
            .map(|i| Server {
                name: format!("s{i}"),
                category: "c".to_string(),
                description: "d".repeat(400),
            })
            .collect(),
    };
    let text = index.description();
    assert_eq!(text.matches("\n- s").count(), MAX_INDEXED);
    assert!(text.ends_with("- and 3 more, not named here"), "{text}");
    assert!(
        !text.contains(&"d".repeat(INDEX_DESCRIPTION + 1)),
        "a description ran long"
    );
}

/// The real engine on this machine (`INCORD_MEMORY_URL`, else the default
/// address): its index parses, a server it lists is described or said to be
/// blocked, and a call is answered or said to be blocked — never a bare
/// status. Run by hand with `--ignored`; it asks the engine to reach the
/// plugin's real server, and changes nothing there.
#[test]
#[ignore = "talks to the real memory engine and the plugin servers it reaches"]
fn the_real_engines_plugin_servers_answer_or_say_why_not() {
    let index = fetch_index(&[]).expect("the engine named plugin servers");
    eprintln!("index: {:?}", index.servers.iter().map(|s| &s.name).collect::<Vec<_>>());
    let server = index.servers[0].name.clone();
    match describe(&index, &json!({"server": server})) {
        Ok(listed) => eprintln!("described `{server}`: {}", listed.lines().next().unwrap_or("")),
        Err(why) => {
            eprintln!("describe `{server}`: {why}");
            assert!(why.starts_with("BLOCKED ON SIGN-IN") || why.contains("could not run"));
        }
    }
    let ran = call(&index, &json!({"tool": format!("{server}.get_me")}));
    match ran {
        Ok(out) => eprintln!("call answered: {}", crate::text::truncate(&out.text, 200)),
        Err(why) => {
            eprintln!("call: {}", crate::text::truncate(&why, 300));
            assert!(why.starts_with("BLOCKED ON SIGN-IN") || why.contains("could not run"));
        }
    }
}
