//! OAuth 2.1 for remote (http) MCP servers, as the MCP authorization spec
//! lays it out: `incode mcp login <name>` once, a bearer token on every
//! request after.
//!
//! # The flow
//!
//! 1. **Discovery.** The server is sent an `initialize` with no token. A `401`
//!    whose `WWW-Authenticate` names `resource_metadata` points at its
//!    Protected Resource Metadata (RFC 9728); without one, the well-known
//!    addresses beside the server are tried. That names the authorization
//!    server, whose own metadata is read from RFC 8414's
//!    `/.well-known/oauth-authorization-server`, falling back to OpenID's
//!    `/.well-known/openid-configuration` ([`as_metadata_candidates`]). A
//!    server with no resource metadata at all is taken to be its own
//!    authorization server, as the spec's first revision had it.
//! 2. **A client.** The `client_id` (and `client_secret`) in the server's
//!    `"oauth"` entry when it has one; otherwise Dynamic Client Registration
//!    (RFC 7591) as a public client, when the authorization server offers a
//!    `registration_endpoint`. Neither is an error that says which to add.
//! 3. **Authorization code with PKCE** (S256, [`Pkce`]) and the `resource`
//!    parameter (RFC 8707), redirected to a one-request listener on
//!    `127.0.0.1` at an ephemeral port ([`Callback`]). The browser is opened
//!    and the address printed as well, for a machine with no browser.
//! 4. **Tokens** are exchanged and kept in `~/.incode/mcp-tokens.json`
//!    ([`TOKENS_FILE`], `0600` on unix), keyed by the server's name AND its
//!    address ([`key`]): a project that reuses a name for another address gets
//!    no token. Each entry carries what refreshing it needs (the token
//!    endpoint and the client), so a session never repeats discovery.
//!
//! # In a session
//!
//! [`TokenSource`] sends `Authorization: Bearer` on every request to the
//! server, refreshes a token that has expired (or is about to) before it is
//! sent, and once more on a `401`. A server with no token, or whose token
//! cannot be refreshed, is dropped with the command that fixes it: a session
//! never waits on a browser.
//!
//! # What it refuses
//!
//! Every endpoint — metadata, authorization, token, registration — must be
//! https, or plain http to this machine only
//! ([`crate::agent::catalogue::key_may_travel_to`]); no redirect is followed
//! ([`crate::runtime::http::no_redirect_client`]); a metadata document past
//! [`MAX_BODY_BYTES`] is refused; an authorization server whose metadata names
//! another issuer, or whose PKCE methods leave out S256, is refused; resource
//! metadata naming a resource on another origin is refused. Tokens are written
//! nowhere but the token file: not to the trace, not into a message.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use reqwest::header::{HeaderValue, ACCEPT, CONTENT_TYPE, WWW_AUTHENTICATE};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

/// Where tokens are kept, in incode's data directory.
pub const TOKENS_FILE: &str = "mcp-tokens.json";
/// How long one request to a metadata, registration or token endpoint may take.
const HTTP_BOUND: Duration = Duration::from_secs(30);
/// How long `incode mcp login` waits for the browser to come back.
const CALLBACK_BOUND: Duration = Duration::from_secs(300);
/// The most of one metadata, registration or token answer read.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// A token this close to its expiry is refreshed before it is sent.
const REFRESH_MARGIN_SECS: u64 = 60;
/// The most of the browser's request to the callback read.
const MAX_CALLBACK_BYTES: usize = 16 * 1024;
/// The path the authorization server redirects back to.
const CALLBACK_PATH: &str = "/callback";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// A server's `"oauth"` entry in `.mcp.json` / `~/.incode/mcp.json`. Every
/// field is optional: an empty entry only says the server needs a login.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthConfig {
    /// A client registered by hand, for an authorization server that offers
    /// no dynamic registration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Its secret, if it is a confidential client; sent in the token request's
    /// body (`client_secret_post`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// The scopes to ask for; otherwise the ones the server's challenge or
    /// resource metadata names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// A fixed port for the loopback redirect, for a client registered with
    /// one; otherwise an ephemeral port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
}

/// Which stored token an HTTP server uses: its configured name (the key, with
/// its address) and the token file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Binding {
    pub(crate) name: String,
    pub(crate) store: PathBuf,
}

// ---------------------------------------------------------------------------
// The token store
// ---------------------------------------------------------------------------

/// One server's tokens, and what refreshing them needs.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stored {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Seconds since the epoch; `None` when the server named no lifetime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    pub token_endpoint: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// The RFC 8707 resource the token was issued for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl std::fmt::Debug for Stored {
    /// No token or secret, only what is safe to show.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stored")
            .field("refreshable", &self.refresh_token.is_some())
            .field("expires_at", &self.expires_at)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TokenFile {
    #[serde(default)]
    tokens: BTreeMap<String, Stored>,
}

/// The store's key for a server: its name and its address, so a token goes
/// only to the address it was issued for.
#[must_use]
pub fn key(name: &str, url: &str) -> String {
    format!("{name} {url}")
}

/// Seconds since the epoch.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn read_store(store: &Path) -> TokenFile {
    std::fs::read_to_string(store)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Write `text` to `path` readable by its owner only: a new file beside it,
/// created `0600` on unix, then renamed over it.
fn write_private(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let partial = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let written = options
        .open(&partial)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())
                .and_then(|()| file.sync_all())
        })
        .and_then(|()| std::fs::rename(&partial, path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&partial);
        return Err(format!("could not write {}: {e}", path.display()));
    }
    Ok(())
}

fn write_store(store: &Path, file: &TokenFile) -> Result<(), String> {
    let text = serde_json::to_string_pretty(file).map_err(|e| e.to_string())?;
    write_private(store, &(text + "\n"))
}

/// The tokens stored for `name` at `url`.
#[must_use]
pub fn load(store: &Path, name: &str, url: &str) -> Option<Stored> {
    read_store(store).tokens.remove(&key(name, url))
}

/// Keep `tokens` for `name` at `url`.
///
/// # Errors
///
/// The token file could not be written.
pub fn save(store: &Path, name: &str, url: &str, tokens: &Stored) -> Result<(), String> {
    let mut file = read_store(store);
    file.tokens.insert(key(name, url), tokens.clone());
    write_store(store, &file)
}

/// Forget every token stored for `name`, at any address. Whether there was one.
///
/// # Errors
///
/// The token file could not be written.
pub fn forget(store: &Path, name: &str) -> Result<bool, String> {
    let mut file = read_store(store);
    let before = file.tokens.len();
    // The address has no space in it, so the name is all before the last.
    file.tokens
        .retain(|k, _| k.rsplit_once(' ').map(|(named, _)| named) != Some(name));
    if file.tokens.len() == before {
        return Ok(false);
    }
    write_store(store, &file)?;
    Ok(true)
}

/// Whether `tokens` should be refreshed before being sent at `now`: it has a
/// lifetime, and it ends within [`REFRESH_MARGIN_SECS`].
#[must_use]
pub fn needs_refresh(tokens: &Stored, now: u64) -> bool {
    tokens
        .expires_at
        .is_some_and(|at| now.saturating_add(REFRESH_MARGIN_SECS) >= at)
}

/// For `incode mcp list`: whether `name` at `url` is logged in. `None` when
/// there is nothing to say (no token, and no `"oauth"` entry).
#[must_use]
pub fn status(store: &Path, name: &str, url: &str, configured: bool) -> Option<String> {
    let Some(tokens) = load(store, name, url) else {
        return configured.then(|| format!("OAuth: not logged in — incode mcp login {name}"));
    };
    Some(match tokens.expires_at {
        Some(_) if !needs_refresh(&tokens, now_secs()) => "OAuth: logged in".to_string(),
        Some(_) if tokens.refresh_token.is_some() => {
            "OAuth: logged in (token expired; refreshed on next use)".to_string()
        }
        Some(_) => format!("OAuth: token expired — incode mcp login {name}"),
        None => "OAuth: logged in".to_string(),
    })
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// What a `WWW-Authenticate: Bearer …` challenge says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Challenge {
    pub(crate) resource_metadata: Option<String>,
    pub(crate) scope: Option<String>,
}

/// A `Bearer` challenge's parameters; `None` for another scheme.
pub(crate) fn parse_challenge(header: &str) -> Option<Challenge> {
    let header = header.trim();
    let scheme_end = header.find(char::is_whitespace).unwrap_or(header.len());
    if !header[..scheme_end].eq_ignore_ascii_case("bearer") {
        return None;
    }
    let mut challenge = Challenge::default();
    let mut rest = header[scheme_end..].chars().peekable();
    loop {
        while rest.next_if(|c| c.is_whitespace() || *c == ',').is_some() {}
        let name: String = std::iter::from_fn(|| rest.next_if(|c| *c != '=' && *c != ','))
            .collect::<String>()
            .trim()
            .to_ascii_lowercase();
        if name.is_empty() || rest.next_if_eq(&'=').is_none() {
            break;
        }
        while rest.next_if(|c| c.is_whitespace()).is_some() {}
        let mut value = String::new();
        if rest.next_if_eq(&'"').is_some() {
            while let Some(c) = rest.next() {
                match c {
                    '\\' => value.extend(rest.next()),
                    '"' => break,
                    c => value.push(c),
                }
            }
        } else {
            value = std::iter::from_fn(|| rest.next_if(|c| *c != ','))
                .collect::<String>()
                .trim()
                .to_string();
        }
        match name.as_str() {
            "resource_metadata" => challenge.resource_metadata = Some(value),
            "scope" => challenge.scope = Some(value),
            _ => {}
        }
    }
    Some(challenge)
}

/// `url` parsed, and an address a token may travel to.
fn checked(url: &str, what: &str) -> Result<Url, String> {
    crate::agent::catalogue::key_may_travel_to(url).map_err(|why| format!("{what}: {why}"))?;
    Url::parse(url).map_err(|e| format!("{what}: {e}"))
}

/// `well_known` inserted between `url`'s host and its path (RFC 8414 §3,
/// RFC 9728 §3.1), and — when it has a path — at the root as well.
fn well_known(url: &Url, well_known: &str) -> Vec<String> {
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    let mut candidates = Vec::new();
    if !path.is_empty() {
        candidates.push(format!("{origin}/.well-known/{well_known}{path}"));
    }
    candidates.push(format!("{origin}/.well-known/{well_known}"));
    candidates
}

/// Where a server's Protected Resource Metadata may be, absent a challenge
/// that names it.
pub(crate) fn resource_metadata_candidates(server: &str) -> Result<Vec<String>, String> {
    let url = checked(server, "the server's address")?;
    Ok(well_known(&url, "oauth-protected-resource"))
}

/// Where an authorization server's metadata may be, in the order the MCP spec
/// gives: RFC 8414 with the path inserted, OpenID with the path inserted,
/// OpenID with the path appended — or, for an issuer with no path, RFC 8414
/// then OpenID at the root.
pub(crate) fn as_metadata_candidates(issuer: &str) -> Result<Vec<String>, String> {
    let url = checked(issuer, "the authorization server")?;
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    Ok(if path.is_empty() {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server"),
            format!("{origin}/.well-known/openid-configuration"),
        ]
    } else {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server{path}"),
            format!("{origin}/.well-known/openid-configuration{path}"),
            format!("{origin}{path}/.well-known/openid-configuration"),
        ]
    })
}

/// A server's Protected Resource Metadata (RFC 9728).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct ResourceMetadata {
    #[serde(default)]
    pub(crate) resource: Option<String>,
    #[serde(default)]
    pub(crate) authorization_servers: Vec<String>,
    #[serde(default)]
    pub(crate) scopes_supported: Vec<String>,
}

/// An authorization server's metadata (RFC 8414, or OpenID's).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct ServerMetadata {
    #[serde(default)]
    pub(crate) issuer: Option<String>,
    pub(crate) authorization_endpoint: String,
    pub(crate) token_endpoint: String,
    #[serde(default)]
    pub(crate) registration_endpoint: Option<String>,
    #[serde(default)]
    pub(crate) code_challenge_methods_supported: Option<Vec<String>>,
}

/// Resource metadata read from `text`, for the server at `server`: it must
/// name an authorization server, and a resource on the server's own origin.
pub(crate) fn parse_resource_metadata(
    text: &str,
    server: &str,
) -> Result<ResourceMetadata, String> {
    let metadata: ResourceMetadata = serde_json::from_str(text)
        .map_err(|e| format!("its resource metadata is not valid: {e}"))?;
    if metadata.authorization_servers.is_empty() {
        return Err("its resource metadata names no authorization server".to_string());
    }
    if let Some(resource) = &metadata.resource {
        let named = Url::parse(resource).map_err(|e| format!("its resource is not a url: {e}"))?;
        let own = Url::parse(server).map_err(|e| e.to_string())?;
        if named.origin() != own.origin() {
            return Err(format!(
                "its resource metadata names a resource on another origin ({})",
                named.origin().ascii_serialization()
            ));
        }
    }
    Ok(metadata)
}

/// Same issuer, ignoring one trailing slash.
fn same_issuer(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// Authorization server metadata read from `text`, for `issuer`: the issuer
/// it names must be that one, every endpoint must be one a token may travel
/// to, and its PKCE methods, when listed, must include S256.
pub(crate) fn parse_server_metadata(text: &str, issuer: &str) -> Result<ServerMetadata, String> {
    let metadata: ServerMetadata = serde_json::from_str(text)
        .map_err(|e| format!("the authorization server's metadata is not valid: {e}"))?;
    if let Some(named) = &metadata.issuer {
        if !same_issuer(named, issuer) {
            return Err(format!(
                "the authorization server's metadata names another issuer (`{named}`, not \
                 `{issuer}`)"
            ));
        }
    }
    checked(
        &metadata.authorization_endpoint,
        "its authorization endpoint",
    )?;
    checked(&metadata.token_endpoint, "its token endpoint")?;
    if let Some(endpoint) = &metadata.registration_endpoint {
        checked(endpoint, "its registration endpoint")?;
    }
    if let Some(methods) = &metadata.code_challenge_methods_supported {
        if !methods.iter().any(|m| m == "S256") {
            return Err("the authorization server does not support PKCE with S256".to_string());
        }
    }
    Ok(metadata)
}

/// What discovery found for one server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Discovered {
    pub(crate) server: ServerMetadata,
    /// The `resource` parameter: the resource metadata's, else the server's
    /// own address.
    pub(crate) resource: String,
    /// The scopes to ask for when the configuration names none.
    pub(crate) scopes: Vec<String>,
}

/// A JSON document at `url`: `None` for an answer that is not a success.
async fn fetch_json(client: &reqwest::Client, url: &str) -> Result<Option<String>, String> {
    let response = client
        .get(url)
        .header(ACCEPT, "application/json")
        .timeout(HTTP_BOUND)
        .send()
        .await
        .map_err(|e| format!("{url} could not be reached: {}", e.without_url()))?;
    if !response.status().is_success() {
        return Ok(None);
    }
    read_bounded(response).await.map(Some)
}

/// A body as text, refused past [`MAX_BODY_BYTES`].
async fn read_bounded(mut response: reqwest::Response) -> Result<String, String> {
    let mut body = Vec::new();
    while let Some(piece) = response
        .chunk()
        .await
        .map_err(|e| format!("an answer stopped early: {}", e.without_url()))?
    {
        if body.len() + piece.len() > MAX_BODY_BYTES {
            return Err(format!("an answer was larger than {MAX_BODY_BYTES} bytes"));
        }
        body.extend_from_slice(&piece);
    }
    String::from_utf8(body).map_err(|_| "an answer was not UTF-8".to_string())
}

/// Find the authorization server for the MCP server at `server`.
async fn discover(server: String) -> Result<Discovered, String> {
    checked(&server, "the server's address")?;
    let client = crate::runtime::http::no_redirect_client()?;
    let hello = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                       "params": {"protocolVersion": crate::mcp_stdio::PROTOCOL_VERSION,
                                  "capabilities": {},
                                  "clientInfo": {"name": "incode",
                                                 "version": env!("CARGO_PKG_VERSION")}}});
    let probe = client
        .post(&server)
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .timeout(HTTP_BOUND)
        .body(hello.to_string())
        .send()
        .await
        .map_err(|e| format!("the server could not be reached: {}", e.without_url()))?;
    let challenge = if probe.status() == StatusCode::UNAUTHORIZED {
        probe
            .headers()
            .get_all(WWW_AUTHENTICATE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(parse_challenge)
            .unwrap_or_default()
    } else {
        Challenge::default()
    };
    drop(probe);
    let mut candidates = Vec::new();
    if let Some(named) = &challenge.resource_metadata {
        checked(named, "the resource metadata the server named")?;
        candidates.push(named.clone());
    }
    candidates.extend(resource_metadata_candidates(&server)?);
    let mut resource_metadata = None;
    for candidate in &candidates {
        if let Some(text) = fetch_json(client, candidate).await? {
            resource_metadata = Some(parse_resource_metadata(&text, &server)?);
            break;
        }
    }
    let challenged: Vec<String> = challenge
        .scope
        .iter()
        .flat_map(|s| s.split_whitespace().map(str::to_string))
        .collect();
    let (issuer, resource, scopes) = match resource_metadata {
        Some(metadata) => (
            metadata.authorization_servers[0].clone(),
            metadata.resource.unwrap_or_else(|| server.clone()),
            if challenged.is_empty() {
                metadata.scopes_supported
            } else {
                challenged
            },
        ),
        // No resource metadata: the server is its own authorization server.
        None => {
            let url = Url::parse(&server).map_err(|e| e.to_string())?;
            (
                url.origin().ascii_serialization(),
                server.clone(),
                challenged,
            )
        }
    };
    for candidate in as_metadata_candidates(&issuer)? {
        if let Some(text) = fetch_json(client, &candidate).await? {
            return Ok(Discovered {
                server: parse_server_metadata(&text, &issuer)?,
                resource,
                scopes,
            });
        }
    }
    Err(format!(
        "no authorization server metadata was found for {issuer}"
    ))
}

// ---------------------------------------------------------------------------
// PKCE and the authorization request
// ---------------------------------------------------------------------------

/// 32 random bytes, base64url without padding (43 characters). From two v4
/// UUIDs — 244 random bits — because `uuid`, already a dependency, is this
/// crate's source of OS randomness; no `rand` is declared.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A PKCE verifier and its S256 challenge (RFC 7636).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Pkce {
    pub(crate) verifier: String,
    pub(crate) challenge: String,
}

impl Pkce {
    pub(crate) fn new() -> Self {
        Self::from_verifier(random_token())
    }

    pub(crate) fn from_verifier(verifier: String) -> Self {
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier,
            challenge,
        }
    }
}

/// What an authorization request carries.
pub(crate) struct AuthRequest<'a> {
    pub(crate) endpoint: &'a str,
    pub(crate) client_id: &'a str,
    pub(crate) redirect_uri: &'a str,
    pub(crate) scopes: &'a [String],
    pub(crate) state: &'a str,
    pub(crate) challenge: &'a str,
    pub(crate) resource: &'a str,
}

/// The address the browser is sent to: the endpoint with the request's
/// parameters added to whatever query it already has.
pub(crate) fn authorization_url(request: &AuthRequest<'_>) -> Result<String, String> {
    let mut url = checked(request.endpoint, "its authorization endpoint")?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", request.client_id)
            .append_pair("redirect_uri", request.redirect_uri)
            .append_pair("state", request.state)
            .append_pair("code_challenge", request.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", request.resource);
        if !request.scopes.is_empty() {
            query.append_pair("scope", &request.scopes.join(" "));
        }
    }
    Ok(url.into())
}

// ---------------------------------------------------------------------------
// The loopback redirect
// ---------------------------------------------------------------------------

/// What one request to the callback listener was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Arrived {
    /// The authorization code, its state verified.
    Code(String),
    /// Not the callback (a `favicon.ico`, say): answered 404, and waited on.
    Elsewhere,
}

/// The browser's request line (`GET /callback?code=…&state=… HTTP/1.1`),
/// checked against the `state` this login sent.
///
/// # Errors
///
/// The state does not match, or the authorization server sent an error, or
/// the callback carries no code. Each ends the login.
pub(crate) fn parse_callback(request_line: &str, state: &str) -> Result<Arrived, String> {
    let mut words = request_line.split_whitespace();
    let (Some(method), Some(target)) = (words.next(), words.next()) else {
        return Ok(Arrived::Elsewhere);
    };
    let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
        return Ok(Arrived::Elsewhere);
    };
    if method != "GET" || url.path() != CALLBACK_PATH {
        return Ok(Arrived::Elsewhere);
    }
    let params: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
    if params.get("state").map(String::as_str) != Some(state) {
        return Err(
            "the authorization server's answer carried the wrong state; the login \
                    was abandoned"
                .to_string(),
        );
    }
    if let Some(error) = params.get("error") {
        let said = params
            .get("error_description")
            .map(|d| format!(": {}", d.chars().take(200).collect::<String>()))
            .unwrap_or_default();
        let error: String = error.chars().take(60).collect();
        return Err(format!("the authorization server refused: {error}{said}"));
    }
    match params.get("code").filter(|c| !c.is_empty()) {
        Some(code) => Ok(Arrived::Code(code.clone())),
        None => Err("the authorization server's answer carried no code".to_string()),
    }
}

/// A one-login listener on `127.0.0.1`.
pub(crate) struct Callback {
    listener: TcpListener,
    pub(crate) redirect_uri: String,
}

impl Callback {
    /// Listen on `port`, or an ephemeral one.
    pub(crate) fn bind(port: Option<u16>) -> Result<Self, String> {
        let listener = TcpListener::bind(("127.0.0.1", port.unwrap_or(0)))
            .map_err(|e| format!("could not listen for the browser on 127.0.0.1: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        Ok(Self {
            listener,
            redirect_uri: format!("http://127.0.0.1:{port}{CALLBACK_PATH}"),
        })
    }

    /// The authorization code, once the browser brings it back with `state`,
    /// within `bound`. Every request is answered with a small page.
    pub(crate) fn wait(&self, state: &str, bound: Duration) -> Result<String, String> {
        self.listener
            .set_nonblocking(true)
            .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + bound;
        loop {
            if Instant::now() >= deadline {
                return Err(format!(
                    "the browser did not come back within {}s",
                    bound.as_secs()
                ));
            }
            let mut stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Err(e) => return Err(format!("the callback listener failed: {e}")),
            };
            let line = read_request_line(&mut stream);
            match parse_callback(&line, state) {
                Ok(Arrived::Code(code)) => {
                    reply(
                        &mut stream,
                        200,
                        "Signed in. You can close this tab and go back to incode.",
                    );
                    return Ok(code);
                }
                Ok(Arrived::Elsewhere) => reply(&mut stream, 404, "Not found."),
                Err(why) => {
                    reply(&mut stream, 400, "The sign-in failed. See incode for why.");
                    return Err(why);
                }
            }
        }
    }
}

/// The first line of a request, bounded in size and time.
fn read_request_line(stream: &mut TcpStream) -> String {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut read = Vec::new();
    let mut buf = [0u8; 1024];
    while read.len() < MAX_CALLBACK_BYTES && !read.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => read.extend_from_slice(&buf[..n]),
        }
    }
    let text = String::from_utf8_lossy(&read);
    text.lines().next().unwrap_or("").to_string()
}

/// A small HTML page; `said` is fixed text, never what the request carried.
fn reply(stream: &mut TcpStream, status: u16, said: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>incode</title>\
         <body style=\"font-family:sans-serif;margin:3em\"><p>{said}</p></body>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

/// Open `url` in the user's browser; whether a browser was started is not
/// known, so the address is always printed as well.
pub fn open_browser(url: &str) {
    let mut command = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else if cfg!(windows) {
        // `start` is a cmd built-in; `&` would end the command there, so it
        // is escaped. The empty argument, which reaches cmd as `""`, is the window
        // title `start` takes first.
        let mut c = std::process::Command::new("cmd");
        c.args(["/c", "start", "", &url.replace('&', "^&")]);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Ok(mut child) = command.spawn() {
        std::thread::spawn(move || child.wait());
    }
}

// ---------------------------------------------------------------------------
// Registration and tokens
// ---------------------------------------------------------------------------

/// A client: its id, and its secret if it has one.
#[derive(Clone, PartialEq, Eq)]
struct Client {
    id: String,
    secret: Option<String>,
}

/// Register incode as a public client (RFC 7591) for `redirect_uri`.
async fn register(endpoint: String, redirect_uri: String) -> Result<Client, String> {
    checked(&endpoint, "its registration endpoint")?;
    let client = crate::runtime::http::no_redirect_client()?;
    let request = json!({
        "client_name": "incode",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let response = client
        .post(&endpoint)
        .header(ACCEPT, "application/json")
        .json(&request)
        .timeout(HTTP_BOUND)
        .send()
        .await
        .map_err(|e| format!("registration failed: {}", e.without_url()))?;
    let status = response.status();
    let text = read_bounded(response).await?;
    if !status.is_success() {
        return Err(format!(
            "registration was refused ({status}){}",
            oauth_error(&text)
        ));
    }
    let answer: Value =
        serde_json::from_str(&text).map_err(|e| format!("registration's answer: {e}"))?;
    let id = answer["client_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("registration's answer carried no client_id")?;
    Ok(Client {
        id: id.to_string(),
        secret: answer["client_secret"].as_str().map(str::to_string),
    })
}

/// An OAuth error answer's `error` (and description), for a message; never
/// the answer itself, which could carry anything.
fn oauth_error(text: &str) -> String {
    let Ok(answer) = serde_json::from_str::<Value>(text) else {
        return String::new();
    };
    let error: String = answer["error"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(60)
        .collect();
    if error.is_empty() {
        return String::new();
    }
    match answer["error_description"].as_str() {
        Some(d) => format!(": {error}: {}", d.chars().take(200).collect::<String>()),
        None => format!(": {error}"),
    }
}

/// The tokens a token endpoint answered with, as stored. `base` supplies
/// what the answer does not carry: the endpoint and the client, and the
/// refresh token when a refresh answer carries no new one.
pub(crate) fn parse_token_answer(text: &str, now: u64, base: &Stored) -> Result<Stored, String> {
    let answer: Value =
        serde_json::from_str(text).map_err(|e| format!("the token answer is not JSON: {e}"))?;
    let access = answer["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or("the token answer carried no access_token")?;
    if let Some(kind) = answer["token_type"].as_str() {
        if !kind.eq_ignore_ascii_case("bearer") {
            let kind: String = kind.chars().take(30).collect();
            return Err(format!("the token is of type `{kind}`, not Bearer"));
        }
    }
    let lifetime = answer["expires_in"]
        .as_u64()
        .or_else(|| answer["expires_in"].as_str().and_then(|s| s.parse().ok()));
    Ok(Stored {
        access_token: access.to_string(),
        refresh_token: answer["refresh_token"]
            .as_str()
            .map(str::to_string)
            .or_else(|| base.refresh_token.clone()),
        expires_at: lifetime.map(|secs| now.saturating_add(secs)),
        scope: answer["scope"]
            .as_str()
            .map(str::to_string)
            .or_else(|| base.scope.clone()),
        ..base.clone()
    })
}

/// POST `form` to `base`'s token endpoint, and the tokens it answers with.
async fn token_request(
    base: Stored,
    mut form: Vec<(&'static str, String)>,
) -> Result<Stored, String> {
    checked(&base.token_endpoint, "its token endpoint")?;
    form.push(("client_id", base.client_id.clone()));
    if let Some(secret) = &base.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    if let Some(resource) = &base.resource {
        form.push(("resource", resource.clone()));
    }
    let client = crate::runtime::http::no_redirect_client()?;
    let response = client
        .post(&base.token_endpoint)
        .header(ACCEPT, "application/json")
        .form(&form)
        .timeout(HTTP_BOUND)
        .send()
        .await
        .map_err(|e| {
            format!(
                "the token endpoint could not be reached: {}",
                e.without_url()
            )
        })?;
    let status = response.status();
    let text = read_bounded(response).await?;
    if !status.is_success() {
        return Err(format!(
            "the token endpoint refused ({status}){}",
            oauth_error(&text)
        ));
    }
    parse_token_answer(&text, now_secs(), &base)
}

/// Trade `stored`'s refresh token for new tokens.
async fn refresh(stored: Stored) -> Result<Stored, String> {
    let token = stored
        .refresh_token
        .clone()
        .ok_or("it has no refresh token")?;
    token_request(
        stored,
        vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", token),
        ],
    )
    .await
}

// ---------------------------------------------------------------------------
// `incode mcp login`
// ---------------------------------------------------------------------------

/// Log in to the server `name` at `url`, keeping its tokens in `store`.
/// `say` is told what the person must see — the address to visit — and
/// `open` is handed that address to open in a browser.
///
/// # Errors
///
/// Discovery, registration, the browser's answer or the token exchange
/// failed, or the token file could not be written.
pub fn login(
    name: &str,
    url: &str,
    config: &OAuthConfig,
    store: &Path,
    say: &dyn Fn(&str),
    open: &dyn Fn(&str),
) -> Result<String, String> {
    let discovered = crate::runtime::http::run_async(discover(url.to_string()))?;
    let callback = Callback::bind(config.callback_port)?;
    let client = match (&config.client_id, &discovered.server.registration_endpoint) {
        (Some(id), _) => Client {
            id: id.clone(),
            secret: config.client_secret.clone(),
        },
        (None, Some(endpoint)) => crate::runtime::http::run_async(register(
            endpoint.clone(),
            callback.redirect_uri.clone(),
        ))?,
        (None, None) => {
            return Err(format!(
                "the authorization server offers no dynamic client registration; register a \
                 client with it and add `\"oauth\": {{\"client_id\": \"…\"}}` to `{name}`'s entry"
            ))
        }
    };
    let pkce = Pkce::new();
    let state = random_token();
    let scopes = if config.scopes.is_empty() {
        discovered.scopes.clone()
    } else {
        config.scopes.clone()
    };
    let address = authorization_url(&AuthRequest {
        endpoint: &discovered.server.authorization_endpoint,
        client_id: &client.id,
        redirect_uri: &callback.redirect_uri,
        scopes: &scopes,
        state: &state,
        challenge: &pkce.challenge,
        resource: &discovered.resource,
    })?;
    say(&format!(
        "Opening your browser to authorize `{name}`. If it does not open, visit:\n\n  {address}\n"
    ));
    open(&address);
    let code = callback.wait(&state, CALLBACK_BOUND)?;
    let base = Stored {
        access_token: String::new(),
        refresh_token: None,
        expires_at: None,
        token_endpoint: discovered.server.token_endpoint.clone(),
        client_id: client.id,
        client_secret: client.secret,
        resource: Some(discovered.resource.clone()),
        scope: (!scopes.is_empty()).then(|| scopes.join(" ")),
    };
    let form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("redirect_uri", callback.redirect_uri.clone()),
        ("code_verifier", pkce.verifier),
    ];
    let tokens = crate::runtime::http::run_async(token_request(base, form))?;
    save(store, name, url, &tokens)?;
    Ok(format!(
        "logged in to `{name}`; its token is kept in {}",
        store.display()
    ))
}

// ---------------------------------------------------------------------------
// In a session
// ---------------------------------------------------------------------------

/// The bearer token for one HTTP server in a session, refreshed as needed.
pub(crate) struct TokenSource {
    name: String,
    url: String,
    store: PathBuf,
    held: tokio::sync::Mutex<Option<Stored>>,
}

impl TokenSource {
    pub(crate) fn new(binding: &Binding, url: &str) -> Self {
        Self {
            held: tokio::sync::Mutex::new(load(&binding.store, &binding.name, url)),
            name: binding.name.clone(),
            url: url.to_string(),
            store: binding.store.clone(),
        }
    }

    /// What to tell the person when the server will not take this session's
    /// requests.
    fn login_hint(&self, why: &str) -> String {
        format!("{why}: run `incode mcp login {}`", self.name)
    }

    /// Refresh `held`, keep the result, and return the new access token.
    async fn refresh_held(&self, held: &mut Option<Stored>) -> Result<String, String> {
        let Some(stored) = held.clone() else {
            return Err(self.login_hint("it needs OAuth authorization"));
        };
        let fresh = refresh(stored).await.map_err(|why| {
            self.login_hint(&format!("its OAuth token could not be refreshed ({why})"))
        })?;
        if let Err(why) = save(&self.store, &self.name, &self.url, &fresh) {
            crate::trace!(
                "mcp",
                "`{}`: a refreshed token was not kept: {why}",
                self.name
            );
        }
        let access = fresh.access_token.clone();
        *held = Some(fresh);
        crate::trace!("mcp", "`{}`: OAuth token refreshed", self.name);
        Ok(access)
    }

    /// The token to send now, refreshed first when it has expired; `None`
    /// when there is none. A refresh that fails sends the old token, and the
    /// server's `401` says the rest.
    pub(crate) async fn current(&self) -> Option<String> {
        let mut held = self.held.lock().await;
        if held.is_none() {
            // A login since the session started.
            *held = load(&self.store, &self.name, &self.url);
        }
        let stored = held.as_ref()?;
        if needs_refresh(stored, now_secs()) && stored.refresh_token.is_some() {
            if let Ok(access) = self.refresh_held(&mut held).await {
                return Some(access);
            }
        }
        held.as_ref().map(|s| s.access_token.clone())
    }

    /// A token to try once more after the server answered `401` to
    /// `rejected`: one a login stored meanwhile, else a refreshed one.
    pub(crate) async fn renew(&self, rejected: Option<&str>) -> Result<String, String> {
        let mut held = self.held.lock().await;
        if let Some(on_disk) = load(&self.store, &self.name, &self.url) {
            if Some(on_disk.access_token.as_str()) != rejected {
                let access = on_disk.access_token.clone();
                *held = Some(on_disk);
                return Ok(access);
            }
            *held = Some(on_disk);
        }
        match held.as_ref() {
            Some(stored) if stored.refresh_token.is_some() => self.refresh_held(&mut held).await,
            Some(_) => Err(self.login_hint("its OAuth token was refused and cannot be refreshed")),
            None => Err(self.login_hint("it needs OAuth authorization")),
        }
    }

    /// The message for a token refused again right after renewing it.
    pub(crate) fn refused(&self) -> String {
        self.login_hint("it refused its OAuth token (401)")
    }

    /// The token held now, without waiting, for a request sent on the way
    /// out.
    pub(crate) fn held_now(&self) -> Option<String> {
        self.held
            .try_lock()
            .ok()
            .and_then(|held| held.as_ref().map(|s| s.access_token.clone()))
    }
}

/// `Authorization: Bearer <token>`, marked sensitive.
pub(crate) fn bearer_header(token: &str) -> Option<HeaderValue> {
    let mut value = HeaderValue::from_str(&format!("Bearer {token}")).ok()?;
    value.set_sensitive(true);
    Some(value)
}

#[cfg(test)]
#[path = "mcp_oauth_tests.rs"]
mod tests;
