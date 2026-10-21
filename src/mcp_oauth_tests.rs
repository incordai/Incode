use super::*;
use crate::toolbox::Connected;
use crate::toolbox::{ServerSpec, Transport};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response};
use std::collections::BTreeSet;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

#[test]
fn a_bearer_challenge_is_parsed_and_another_scheme_is_not() {
    let challenge = parse_challenge(
        r#"Bearer realm="mcp", resource_metadata="https://s.example/.well-known/oauth-protected-resource/mcp", scope="read write""#,
    )
    .expect("bearer");
    assert_eq!(
        challenge.resource_metadata.as_deref(),
        Some("https://s.example/.well-known/oauth-protected-resource/mcp")
    );
    assert_eq!(challenge.scope.as_deref(), Some("read write"));
    let bare = parse_challenge("bearer error=invalid_token,scope=a").expect("bearer");
    assert_eq!(bare.scope.as_deref(), Some("a"));
    assert_eq!(bare.resource_metadata, None);
    let escaped = parse_challenge(r#"Bearer scope="a \"b\"""#).expect("bearer");
    assert_eq!(escaped.scope.as_deref(), Some(r#"a "b""#));
    assert_eq!(parse_challenge("Bearer"), Some(Challenge::default()));
    assert_eq!(parse_challenge(r#"Basic realm="x""#), None);
    assert_eq!(parse_challenge("Bearerish x=y"), None);
}

#[test]
fn well_known_addresses_are_built_with_the_path_inserted() {
    assert_eq!(
        resource_metadata_candidates("https://s.example/api/mcp").expect("ok"),
        [
            "https://s.example/.well-known/oauth-protected-resource/api/mcp",
            "https://s.example/.well-known/oauth-protected-resource",
        ]
    );
    assert_eq!(
        resource_metadata_candidates("https://s.example/").expect("ok"),
        ["https://s.example/.well-known/oauth-protected-resource"]
    );
    assert_eq!(
        as_metadata_candidates("https://auth.example/tenant1").expect("ok"),
        [
            "https://auth.example/.well-known/oauth-authorization-server/tenant1",
            "https://auth.example/.well-known/openid-configuration/tenant1",
            "https://auth.example/tenant1/.well-known/openid-configuration",
        ]
    );
    assert_eq!(
        as_metadata_candidates("https://auth.example").expect("ok"),
        [
            "https://auth.example/.well-known/oauth-authorization-server",
            "https://auth.example/.well-known/openid-configuration",
        ]
    );
    assert!(
        as_metadata_candidates("http://auth.example").is_err(),
        "plain http elsewhere"
    );
    assert!(
        as_metadata_candidates("http://127.0.0.1:9/a").is_ok(),
        "loopback"
    );
}

#[test]
fn metadata_is_parsed_and_checked() {
    let server = "https://s.example/mcp";
    let resource = parse_resource_metadata(
        r#"{"resource": "https://s.example/mcp", "authorization_servers": ["https://auth.example"],
            "scopes_supported": ["read"]}"#,
        server,
    )
    .expect("resource metadata");
    assert_eq!(resource.authorization_servers, ["https://auth.example"]);
    assert_eq!(resource.scopes_supported, ["read"]);
    let elsewhere = parse_resource_metadata(
        r#"{"resource": "https://other.example/mcp", "authorization_servers": ["https://a.example"]}"#,
        server,
    );
    assert!(elsewhere
        .expect_err("other origin")
        .contains("another origin"));
    assert!(parse_resource_metadata(r#"{"authorization_servers": []}"#, server).is_err());

    let good = r#"{"issuer": "https://auth.example/", "authorization_endpoint": "https://auth.example/authorize",
                   "token_endpoint": "https://auth.example/token", "registration_endpoint": "https://auth.example/register",
                   "code_challenge_methods_supported": ["plain", "S256"]}"#;
    let metadata = parse_server_metadata(good, "https://auth.example").expect("metadata");
    assert_eq!(metadata.token_endpoint, "https://auth.example/token");
    assert_eq!(
        metadata.registration_endpoint.as_deref(),
        Some("https://auth.example/register")
    );
    let wrong_issuer = parse_server_metadata(good, "https://evil.example");
    assert!(wrong_issuer.expect_err("issuer").contains("another issuer"));
    let no_s256 = good.replace(r#"["plain", "S256"]"#, r#"["plain"]"#);
    assert!(parse_server_metadata(&no_s256, "https://auth.example")
        .expect_err("pkce")
        .contains("S256"));
    let clear = good.replace("https://auth.example/token", "http://auth.example/token");
    assert!(parse_server_metadata(&clear, "https://auth.example").is_err());
    assert!(parse_server_metadata(r#"{"issuer": "x"}"#, "x").is_err());
}

// ---------------------------------------------------------------------------
// PKCE, the authorization request and the callback
// ---------------------------------------------------------------------------

#[test]
fn the_pkce_challenge_is_s256_of_the_verifier() {
    // RFC 7636, appendix B.
    let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_string());
    assert_eq!(
        pkce.challenge,
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    let fresh = Pkce::new();
    assert_eq!(fresh.verifier.len(), 43);
    assert!(fresh
        .verifier
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    assert_ne!(fresh.verifier, Pkce::new().verifier);
}

#[test]
fn the_authorization_url_carries_every_parameter() {
    let scopes = ["read".to_string(), "write".to_string()];
    let url = authorization_url(&AuthRequest {
        endpoint: "https://auth.example/authorize?tenant=t1",
        client_id: "client 1",
        redirect_uri: "http://127.0.0.1:5555/callback",
        scopes: &scopes,
        state: "st&te",
        challenge: "ch",
        resource: "https://s.example/mcp",
    })
    .expect("url");
    let parsed = Url::parse(&url).expect("parses");
    let params: BTreeMap<String, String> = parsed.query_pairs().into_owned().collect();
    assert_eq!(params["tenant"], "t1", "its own query is kept");
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["client_id"], "client 1");
    assert_eq!(params["redirect_uri"], "http://127.0.0.1:5555/callback");
    assert_eq!(params["state"], "st&te");
    assert_eq!(params["code_challenge"], "ch");
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(params["resource"], "https://s.example/mcp");
    assert_eq!(params["scope"], "read write");
    assert!(url.contains("st%26te"), "{url}");
    let no_scopes = authorization_url(&AuthRequest {
        scopes: &[],
        endpoint: "https://auth.example/authorize",
        client_id: "c",
        redirect_uri: "http://127.0.0.1:1/callback",
        state: "s",
        challenge: "c",
        resource: "r",
    })
    .expect("url");
    assert!(!no_scopes.contains("scope="), "{no_scopes}");
    let clear = AuthRequest {
        endpoint: "http://auth.example/authorize",
        client_id: "c",
        redirect_uri: "http://127.0.0.1:1/callback",
        scopes: &[],
        state: "s",
        challenge: "c",
        resource: "r",
    };
    assert!(authorization_url(&clear).is_err());
}

/// **A SERVER'S ENDPOINT IS NOT A COMMAND LINE.** Its metadata can put `|`,
/// `&` or `%NAME%` in the authorization URL, and they reach the address the
/// browser is sent to unchanged — which is why no shell may read it.
#[test]
fn an_endpoint_can_carry_shell_text_into_the_browser_address() {
    let url = authorization_url(&AuthRequest {
        endpoint: "https://auth.example/a|b?x=%PATH%|calc",
        client_id: "c",
        redirect_uri: "http://127.0.0.1:1/callback",
        scopes: &[],
        state: "s",
        challenge: "c",
        resource: "r",
    })
    .expect("url");
    assert!(url.contains("/a|b?x=%PATH%|calc&"), "{url}");
    assert_eq!(web_address(&url).as_deref(), Some(url.as_str()));
}

/// Only an http(s) address is opened: a path or another scheme would be run,
/// or opened as a document, by the OS's handler for it.
#[test]
fn only_a_web_address_is_handed_to_the_browser() {
    for address in [
        r"C:\Windows\System32\calc.exe",
        "calc.exe",
        "file:///C:/Windows/System32/calc.exe",
        r"\\host\share\run.exe",
        "javascript:alert(1)",
        "ms-settings:privacy",
        "",
    ] {
        assert_eq!(web_address(address), None, "`{address}`");
    }
    assert!(web_address("http://127.0.0.1:5555/callback").is_some());
    assert!(web_address("https://auth.example/authorize?a=1").is_some());
}

/// **THE BROWSER IS OPENED WITHOUT A SHELL.** Under `cmd /c start`, the text
/// after a `|` in the address ran as a command. The address must still reach
/// a browser: it calls back to a listener here. It opens a browser tab, so it
/// is run by hand: `cargo test --lib open_browser_runs_nothing -- --ignored`.
#[cfg(windows)]
#[test]
#[ignore = "opens a browser tab"]
fn open_browser_runs_nothing_from_the_address() {
    let probe =
        std::env::temp_dir().join(format!("incode-open-browser-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir(&probe);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("addr").port();
    let address = format!("http://127.0.0.1:{port}/?q=|mkdir,{}", probe.display());
    open_browser(&address);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut reached = false;
    while Instant::now() < deadline {
        if let Ok((mut stream, _)) = listener.accept() {
            reached = true;
            let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
        }
        if reached {
            // Time for a command started beside the browser to have run.
            std::thread::sleep(Duration::from_secs(2));
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let ran = probe.exists();
    let _ = std::fs::remove_dir(&probe);
    assert!(
        !ran,
        "text from the address ran as a command: it made {}",
        probe.display()
    );
    assert!(reached, "no browser asked for the address within 20 s");
}

#[test]
fn the_callback_is_parsed_and_its_state_checked() {
    assert_eq!(
        parse_callback("GET /callback?code=abc%2F1&state=s1 HTTP/1.1", "s1"),
        Ok(Arrived::Code("abc/1".to_string()))
    );
    let wrong = parse_callback("GET /callback?code=abc&state=other HTTP/1.1", "s1");
    assert!(wrong.expect_err("state").contains("wrong state"));
    let missing = parse_callback("GET /callback?code=abc HTTP/1.1", "s1");
    assert!(missing.expect_err("state").contains("wrong state"));
    let refused = parse_callback(
        "GET /callback?error=access_denied&error_description=no+thanks&state=s1 HTTP/1.1",
        "s1",
    );
    let refused = refused.expect_err("refused");
    assert!(
        refused.contains("access_denied") && refused.contains("no thanks"),
        "{refused}"
    );
    assert!(parse_callback("GET /callback?state=s1 HTTP/1.1", "s1").is_err());
    assert_eq!(
        parse_callback("GET /favicon.ico HTTP/1.1", "s1"),
        Ok(Arrived::Elsewhere)
    );
    assert_eq!(
        parse_callback("POST /callback?code=a&state=s1 HTTP/1.1", "s1"),
        Ok(Arrived::Elsewhere)
    );
    assert_eq!(parse_callback("", "s1"), Ok(Arrived::Elsewhere));
}

/// A GET to `url` on this machine, by hand: the status line and the body.
fn get_loopback(url: &str) -> String {
    let url = Url::parse(url).expect("url");
    let address = format!(
        "{}:{}",
        url.host_str().expect("host"),
        url.port().expect("port")
    );
    let mut stream = TcpStream::connect(address).expect("connect");
    let target = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    write!(stream, "GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").expect("write");
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    answer
}

/// **THE LISTENER TAKES ONE CODE, AND REFUSES A WRONG STATE.** A request
/// elsewhere is answered 404 and waited past.
#[test]
fn the_loopback_listener_takes_the_code_and_refuses_a_wrong_state() {
    let callback = Callback::bind(None).expect("bind");
    assert!(callback.redirect_uri.starts_with("http://127.0.0.1:"));
    let redirect = callback.redirect_uri.clone();
    let browser = std::thread::spawn(move || {
        let favicon = get_loopback(&redirect.replace("/callback", "/favicon.ico"));
        let answer = get_loopback(&format!("{redirect}?code=c0de&state=good"));
        (favicon, answer)
    });
    let code = callback
        .wait("good", Duration::from_secs(20))
        .expect("code");
    assert_eq!(code, "c0de");
    let (favicon, answer) = browser.join().expect("browser");
    assert!(favicon.starts_with("HTTP/1.1 404"), "{favicon}");
    assert!(
        answer.starts_with("HTTP/1.1 200") && answer.contains("Signed in"),
        "{answer}"
    );

    let callback = Callback::bind(None).expect("bind");
    let redirect = callback.redirect_uri.clone();
    let browser =
        std::thread::spawn(move || get_loopback(&format!("{redirect}?code=c0de&state=forged")));
    let refused = callback
        .wait("good", Duration::from_secs(20))
        .expect_err("forged state");
    assert!(refused.contains("wrong state"), "{refused}");
    assert!(browser.join().expect("browser").starts_with("HTTP/1.1 400"));

    let quiet = Callback::bind(None).expect("bind");
    let late = quiet.wait("s", Duration::from_millis(200));
    assert!(late.expect_err("timeout").contains("did not come back"));
}

// ---------------------------------------------------------------------------
// The token store
// ---------------------------------------------------------------------------

fn stored(access: &str, refresh: Option<&str>, expires_at: Option<u64>) -> Stored {
    Stored {
        access_token: access.to_string(),
        refresh_token: refresh.map(str::to_string),
        expires_at,
        token_endpoint: "https://auth.example/token".to_string(),
        client_id: "client-1".to_string(),
        client_secret: None,
        resource: Some("https://s.example/mcp".to_string()),
        scope: None,
    }
}

/// **TOKENS ARE KEPT BY NAME AND ADDRESS**, owner-readable only, and
/// forgotten by name.
#[test]
fn the_token_store_round_trips_by_name_and_address() {
    let dir = tempfile::tempdir().expect("dir");
    let store = dir.path().join("nested").join(TOKENS_FILE);
    assert_eq!(load(&store, "docs", "https://s.example/mcp"), None);
    let tokens = stored("s3cret-access", Some("s3cret-refresh"), Some(100));
    save(&store, "docs", "https://s.example/mcp", &tokens).expect("save");
    save(
        &store,
        "other",
        "https://o.example/mcp",
        &stored("o", None, None),
    )
    .expect("save");
    assert_eq!(load(&store, "docs", "https://s.example/mcp"), Some(tokens));
    assert_eq!(
        load(&store, "docs", "https://evil.example/mcp"),
        None,
        "a token goes only to the address it was issued for"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&store)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let shown = format!("{:?}", load(&store, "docs", "https://s.example/mcp"));
    assert!(!shown.contains("s3cret"), "{shown}");
    save(
        &store,
        "docs x",
        "https://x.example/mcp",
        &stored("x", None, None),
    )
    .expect("save");
    assert!(forget(&store, "docs").expect("forget"));
    assert!(!forget(&store, "docs").expect("forget"));
    assert_eq!(load(&store, "docs", "https://s.example/mcp"), None);
    assert!(load(&store, "other", "https://o.example/mcp").is_some());
    assert!(
        load(&store, "docs x", "https://x.example/mcp").is_some(),
        "a name that starts with another is its own"
    );
}

#[test]
fn a_token_is_refreshed_only_near_its_expiry() {
    let now = 1_000_000;
    assert!(
        !needs_refresh(&stored("a", Some("r"), None), now),
        "no lifetime"
    );
    assert!(!needs_refresh(
        &stored("a", Some("r"), Some(now + 3600)),
        now
    ));
    assert!(
        needs_refresh(&stored("a", Some("r"), Some(now + 30)), now),
        "within the margin"
    );
    assert!(
        needs_refresh(&stored("a", Some("r"), Some(now - 1)), now),
        "expired"
    );
}

#[test]
fn a_token_answer_keeps_what_it_does_not_replace() {
    let base = stored("old", Some("keep-me"), Some(5));
    let fresh = parse_token_answer(
        r#"{"access_token": "new", "token_type": "bearer", "expires_in": 60}"#,
        1000,
        &base,
    )
    .expect("parsed");
    assert_eq!(fresh.access_token, "new");
    assert_eq!(fresh.refresh_token.as_deref(), Some("keep-me"));
    assert_eq!(fresh.expires_at, Some(1060));
    assert_eq!(fresh.token_endpoint, base.token_endpoint);
    let rotated = parse_token_answer(r#"{"access_token": "n", "refresh_token": "r2"}"#, 0, &base)
        .expect("parsed");
    assert_eq!(rotated.refresh_token.as_deref(), Some("r2"));
    assert_eq!(rotated.expires_at, None);
    assert!(parse_token_answer(r#"{"token_type": "Bearer"}"#, 0, &base).is_err());
    assert!(parse_token_answer(r#"{"access_token": "a", "token_type": "mac"}"#, 0, &base).is_err());
}

// ---------------------------------------------------------------------------
// End to end: a fake authorization server and a fake MCP server that needs it
// ---------------------------------------------------------------------------

type Body = http_body_util::Full<Bytes>;

#[derive(Default)]
struct AuthState {
    base: String,
    /// Codes handed out: code -> (challenge, client_id, redirect_uri, resource).
    codes: BTreeMap<String, (String, String, String, String)>,
    access: BTreeSet<String>,
    refresh: BTreeSet<String>,
    issued: usize,
    refreshes: usize,
    unauthorized: usize,
    registered: usize,
    /// The bearer tokens the MCP endpoint was sent, in order.
    bearers: Vec<Option<String>>,
}

/// One loopback server playing both parts: an MCP endpoint at `/mcp` that
/// wants a bearer token, and the authorization server at `/auth` it names.
struct FakeAuth {
    base: String,
    state: Arc<Mutex<AuthState>>,
}

impl FakeAuth {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let state = Arc::new(Mutex::new(AuthState {
            base: base.clone(),
            ..AuthState::default()
        }));
        let shared = Arc::clone(&state);
        crate::runtime::http::SHARED_RUNTIME.spawn(async move {
            let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
                return;
            };
            while let Ok((socket, _)) = listener.accept().await {
                let shared = Arc::clone(&shared);
                tokio::spawn(async move {
                    let service =
                        hyper::service::service_fn(move |req| answer(req, Arc::clone(&shared)));
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        Self { base, state }
    }

    fn url(&self) -> String {
        format!("{}/mcp", self.base)
    }

    fn with<T>(&self, f: impl FnOnce(&mut AuthState) -> T) -> T {
        f(&mut self.state.lock().expect("state"))
    }
}

fn reply(status: u16, body: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("response")
}

fn sha256_b64(text: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(text.as_bytes()))
}

fn issue(state: &mut AuthState) -> String {
    state.issued += 1;
    let (access, refresh) = (
        format!("acc-{}", state.issued),
        format!("ref-{}", state.issued),
    );
    state.access.insert(access.clone());
    state.refresh.insert(refresh.clone());
    json!({"access_token": access, "token_type": "Bearer", "expires_in": 3600,
           "refresh_token": refresh})
    .to_string()
}

async fn answer(
    req: Request<Incoming>,
    shared: Arc<Mutex<AuthState>>,
) -> Result<Response<Body>, Infallible> {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let bearer = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let bytes = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => Bytes::new(),
    };
    let form: BTreeMap<String, String> =
        Url::parse(&format!("http://x/?{}", String::from_utf8_lossy(&bytes)))
            .map(|u| u.query_pairs().into_owned().collect())
            .unwrap_or_default();
    let params: BTreeMap<String, String> = Url::parse(&format!("http://x/?{query}"))
        .map(|u| u.query_pairs().into_owned().collect())
        .unwrap_or_default();
    let mut state = shared.lock().expect("state");
    let base = state.base.clone();
    let response = match path.as_str() {
        "/mcp" => {
            state.bearers.push(bearer.clone());
            if !bearer.is_some_and(|b| state.access.contains(&b)) {
                state.unauthorized += 1;
                let challenge = format!(
                    r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/mcp", scope="mcp:read""#
                );
                return Ok(Response::builder()
                    .status(401)
                    .header("www-authenticate", challenge)
                    .body(Full::new(Bytes::new()))
                    .expect("response"));
            }
            let message: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let id = message["id"].clone();
            let result = match message["method"].as_str().unwrap_or("") {
                "initialize" => {
                    json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                                       "serverInfo": {"name": "fake-oauth", "version": "0"}})
                }
                "tools/list" => {
                    json!({"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]})
                }
                "tools/call" => json!({"content": [{"type": "text",
                    "text": format!("echo: {}", message["params"]["arguments"]["text"].as_str().unwrap_or(""))}]}),
                _ => {
                    return Ok(Response::builder()
                        .status(202)
                        .body(Full::new(Bytes::new()))
                        .expect("202"))
                }
            };
            reply(
                200,
                json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
            )
        }
        "/.well-known/oauth-protected-resource/mcp" => reply(
            200,
            json!({"resource": format!("{base}/mcp"),
                   "authorization_servers": [format!("{base}/auth")],
                   "scopes_supported": ["mcp:read", "mcp:write"]})
            .to_string(),
        ),
        "/.well-known/oauth-authorization-server/auth" => reply(
            200,
            json!({"issuer": format!("{base}/auth"),
                   "authorization_endpoint": format!("{base}/auth/authorize"),
                   "token_endpoint": format!("{base}/auth/token"),
                   "registration_endpoint": format!("{base}/auth/register"),
                   "code_challenge_methods_supported": ["S256"]})
            .to_string(),
        ),
        "/auth/register" => {
            state.registered += 1;
            let asked: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            assert_eq!(asked["token_endpoint_auth_method"], "none");
            reply(201, json!({"client_id": "client-1"}).to_string())
        }
        "/auth/authorize" => {
            let code = format!("code-{}", state.codes.len() + 1);
            state.codes.insert(
                code.clone(),
                (
                    params["code_challenge"].clone(),
                    params["client_id"].clone(),
                    params["redirect_uri"].clone(),
                    params["resource"].clone(),
                ),
            );
            assert_eq!(params["code_challenge_method"], "S256");
            assert_eq!(params["scope"], "mcp:read", "the challenge's scope");
            let mut to = Url::parse(&params["redirect_uri"]).expect("redirect");
            to.query_pairs_mut()
                .append_pair("code", &code)
                .append_pair("state", &params["state"]);
            Response::builder()
                .status(302)
                .header("location", to.as_str())
                .body(Full::new(Bytes::new()))
                .expect("302")
        }
        "/auth/token" => match form.get("grant_type").map(String::as_str) {
            Some("authorization_code") => {
                let known = state.codes.remove(&form["code"]);
                let good = known.is_some_and(|(challenge, client, redirect, resource)| {
                    challenge == sha256_b64(&form["code_verifier"])
                        && client == form["client_id"]
                        && redirect == form["redirect_uri"]
                        && resource == form["resource"]
                });
                if good {
                    reply(200, issue(&mut state))
                } else {
                    reply(400, json!({"error": "invalid_grant"}).to_string())
                }
            }
            Some("refresh_token") => {
                if state.refresh.remove(&form["refresh_token"]) {
                    state.refreshes += 1;
                    reply(200, issue(&mut state))
                } else {
                    reply(400, json!({"error": "invalid_grant"}).to_string())
                }
            }
            _ => reply(400, json!({"error": "unsupported_grant_type"}).to_string()),
        },
        _ => reply(404, String::new()),
    };
    Ok(response)
}

/// The browser: follow the authorization URL to its redirect, and the
/// redirect to incode's listener — on a thread, as a browser would.
fn browse(address: &str) {
    let address = address.to_string();
    std::thread::spawn(move || {
        let location = crate::runtime::http::run_async(async move {
            let client = crate::runtime::http::no_redirect_client()?;
            let answer = client
                .get(&address)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            answer
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .ok_or_else(|| "no redirect".to_string())
        })
        .expect("authorize");
        get_loopback(&location)
    });
}

fn spec(fake: &FakeAuth, store: &Path) -> ServerSpec {
    ServerSpec {
        name: "web".to_string(),
        transport: Transport::Http {
            url: fake.url(),
            headers: Vec::new(),
            oauth: Some(Binding {
                name: "web".to_string(),
                store: store.to_path_buf(),
            }),
        },
    }
}

fn connect(fake: &FakeAuth, store: &Path) -> (Connected, Vec<String>) {
    let dir = tempfile::tempdir().expect("dir");
    Connected::now(vec![spec(fake, store)], dir.path())
}

/// **401, LOGIN, TOKEN, CALL, REFRESH.** With no token the server is dropped
/// with the command that logs in; `login` discovers, registers, runs the
/// browser flow with PKCE and keeps the tokens; a session then calls with the
/// bearer, refreshes a token the server stopped taking, and refreshes an
/// expired one before sending it.
#[test]
fn a_server_needing_oauth_is_logged_in_to_called_and_refreshed() {
    let fake = FakeAuth::start();
    let dir = tempfile::tempdir().expect("dir");
    let store = dir.path().join(TOKENS_FILE);

    let (servers, said) = connect(&fake, &store);
    assert!(servers.definitions().is_empty());
    assert!(
        said.iter()
            .any(|s| s.contains("`web` was not started") && s.contains("incode mcp login web")),
        "{said:?}"
    );

    let printed = Mutex::new(Vec::new());
    let done = login(
        "web",
        &fake.url(),
        &OAuthConfig::default(),
        &store,
        &|line| printed.lock().expect("printed").push(line.to_string()),
        &browse,
    )
    .expect("logged in");
    assert!(done.contains("logged in to `web`"), "{done}");
    let printed = printed.lock().expect("printed").join("\n");
    assert!(
        printed.contains("/auth/authorize?response_type=code"),
        "{printed}"
    );
    assert_eq!(fake.with(|s| (s.registered, s.issued)), (1, 1));
    let tokens = load(&store, "web", &fake.url()).expect("stored");
    assert_eq!(tokens.access_token, "acc-1");
    assert_eq!(tokens.client_id, "client-1");
    assert_eq!(tokens.resource.as_deref(), Some(fake.url().as_str()));

    let (servers, said) = connect(&fake, &store);
    assert!(servers.has("mcp__web__echo"), "{said:?}");
    let out = servers
        .call("mcp__web__echo", &json!({"text": "hi"}), &|| false)
        .expect("called");
    assert_eq!(out.text, "echo: hi");
    assert_eq!(
        fake.with(|s| s.bearers.last().cloned()),
        Some(Some("acc-1".to_string()))
    );

    // The server stops taking the token: a 401, a refresh, the call again.
    fake.with(|s| s.access.clear());
    let out = servers
        .call("mcp__web__echo", &json!({"text": "again"}), &|| false)
        .expect("called after a refresh");
    assert_eq!(out.text, "echo: again");
    assert_eq!(fake.with(|s| s.refreshes), 1);
    assert_eq!(
        load(&store, "web", &fake.url()).expect("kept").access_token,
        "acc-2",
        "the refreshed token is kept"
    );
    drop(servers);

    // An expired token is refreshed before it is sent: no 401 for it.
    let mut expired = load(&store, "web", &fake.url()).expect("stored");
    expired.expires_at = Some(1);
    save(&store, "web", &fake.url(), &expired).expect("save");
    let refused_before = fake.with(|s| s.unauthorized);
    let (servers, said) = connect(&fake, &store);
    assert!(servers.has("mcp__web__echo"), "{said:?}");
    assert_eq!(fake.with(|s| s.refreshes), 2);
    assert_eq!(fake.with(|s| s.unauthorized), refused_before);
    assert!(fake.with(|s| s.bearers.last().cloned()) == Some(Some("acc-3".to_string())));
    drop(servers);

    // Refused and not refreshable: dropped, with the command that logs in.
    fake.with(|s| {
        s.access.clear();
        s.refresh.clear();
    });
    let (servers, said) = connect(&fake, &store);
    assert!(servers.definitions().is_empty());
    assert!(
        said.iter()
            .any(|s| s.contains("could not be refreshed") && s.contains("incode mcp login web")),
        "{said:?}"
    );
}

/// **A CONFIGURED OAUTH SERVER WITH NO TOKEN IS NOT STARTED**, `incode mcp
/// list` says so, and after a token is kept it is started bound to it.
#[test]
fn the_configuration_skips_a_server_not_logged_in_and_lists_its_status() {
    let ws = tempfile::tempdir().expect("ws");
    let data = tempfile::tempdir().expect("data");
    std::fs::write(
        data.path().join(crate::mcp_config::USER_FILE),
        r#"{"mcpServers": {
             "docs": {"url": "https://s.example/mcp", "oauth": {"scopes": ["read"]}},
             "plain": {"url": "https://p.example/mcp"},
             "keyed": {"url": "https://k.example/mcp", "headers": {"Authorization": "Bearer x"}},
             "blank": {"url": "https://b.example/mcp", "headers": {"Authorization": "Bearer "}}}}"#,
    )
    .expect("write");
    let (specs, said) = crate::mcp_config::session_servers(ws.path(), data.path(), None);
    let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["blank", "keyed", "plain"]);
    assert!(
        said.iter()
            .any(|s| s.contains("`docs` needs OAuth") && s.contains("incode mcp login docs")),
        "{said:?}"
    );
    let bound = |name: &str, specs: &[ServerSpec]| {
        specs
            .iter()
            .find(|s| s.name == name)
            .is_some_and(|s| matches!(&s.transport, Transport::Http { oauth: Some(_), .. }))
    };
    assert!(
        bound("plain", &specs),
        "an http server may still be asked to log in"
    );
    assert!(!bound("keyed", &specs), "its own Authorization header wins");
    assert!(bound("blank", &specs), "a blank one does not");
    let blank = specs.iter().find(|s| s.name == "blank").expect("blank");
    assert!(
        matches!(&blank.transport, Transport::Http { headers, .. } if headers.is_empty()),
        "and is not sent"
    );

    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        crate::mcp_config::command(&args, ws.path(), data.path())
    };
    let listed = run(&["list"]).expect("list");
    assert!(
        listed.contains("OAuth: not logged in — incode mcp login docs"),
        "{listed}"
    );

    let store = data.path().join(TOKENS_FILE);
    save(
        &store,
        "docs",
        "https://s.example/mcp",
        &stored("a", None, None),
    )
    .expect("save");
    let (specs, _) = crate::mcp_config::session_servers(ws.path(), data.path(), None);
    assert!(bound("docs", &specs));
    let listed = run(&["list"]).expect("list");
    assert!(
        listed.contains("docs [user]") && listed.contains("OAuth: logged in"),
        "{listed}"
    );

    assert!(run(&["logout", "docs"])
        .expect("logout")
        .contains("logged out"));
    assert!(run(&["logout", "docs"])
        .expect("logout")
        .contains("was not logged in"));
    assert!(run(&["login"]).is_err());
    assert!(run(&["login", "nope"]).is_err());
}
