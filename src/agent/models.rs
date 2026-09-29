//! Which API route an id names, and the settings a backend is built from.
//!
//! # Three kinds of API route, one resolution
//!
//! - [`API_ROUTE`] — the one the environment configures
//!   ([`super::api::API_URL_ENV`] and its siblings).
//! - A model in admin.json `api_models`, chosen by its `name`. Anthropic,
//!   Z.ai, OpenRouter, a local server: each is one entry, differing only in
//!   what the operator wrote.
//! - An agent of a provider in admin.json `api_providers`, chosen as
//!   `<provider>/<agent>`. Its turns go to `{url}`[`PROVIDER_TURNS_PATH`] in
//!   the chat-completions format, asking for the agent's id as the model —
//!   the provider contract's second half, beside the listing on
//!   [`super::catalogue`].
//!
//! The catalogue lists a route as usable by exactly these functions, and every
//! place that builds a backend builds it from them, so the two cannot disagree
//! about whether an id can be driven.
//!
//! # The key
//!
//! Read from the environment variable the entry names, when it is needed, and
//! never written anywhere. It is sent only to `https`, or to plain `http` on
//! this machine ([`super::catalogue::key_may_travel_to`]).
//!
//! [`API_ROUTE`]: super::registry::API_ROUTE

use super::api::{Settings, WireFormat};
use super::registry::API_ROUTE;
use crate::admin::{AdminConfig, ApiModel, ApiProvider};

/// Where a provider agent's turns are sent, under the provider's `url`.
pub const PROVIDER_TURNS_PATH: &str = "/v1/chat/completions";

/// The settings for one configured model, or why it cannot be driven now.
///
/// # Errors
///
/// When the entry is incomplete, its url would expose the key, or the
/// variable holding the key is not set — each said in words the operator can
/// act on.
pub fn model_settings(
    model: &ApiModel,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Settings, String> {
    let name = model.name.trim();
    if name.is_empty() {
        return Err("this model has no name, so it could not be chosen".to_string());
    }
    let url = model.url.trim();
    if url.is_empty() {
        return Err("no url is configured".to_string());
    }
    super::catalogue::key_may_travel_to(url)?;
    let key = key_from(&model.key_env, env)?;
    Ok(Settings {
        endpoint: url.to_string(),
        key,
        model: model.model.trim().to_string(),
        format: model.format,
        auth: model.auth.unwrap_or_else(|| model.format.default_auth()),
        sub_model: model.sub_model.trim().to_string(),
        route: name.to_string(),
    })
}

/// The settings for one agent a provider lists, or why it cannot be driven.
///
/// # Errors
///
/// As [`model_settings`], for the provider's url and key.
pub fn provider_settings(
    provider: &ApiProvider,
    agent: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Settings, String> {
    let url = provider.url.trim();
    if url.is_empty() {
        return Err("no url is configured".to_string());
    }
    super::catalogue::key_may_travel_to(url)?;
    let key = key_from(&provider.key_env, env)?;
    Ok(Settings {
        endpoint: format!("{}{PROVIDER_TURNS_PATH}", url.trim_end_matches('/')),
        key,
        model: agent.to_string(),
        format: WireFormat::Openai,
        auth: WireFormat::Openai.default_auth(),
        // A provider agent is one model; its sub-agents are the same agent.
        sub_model: String::new(),
        route: format!("{}/{agent}", provider.name.trim()),
    })
}

/// Where a route's service lists its models: beside the endpoint turns go to
/// — `…/v1/messages` lists at `…/v1/models`, `…/chat/completions` at
/// `…/models`. `None` for an endpoint that ends in neither, which does not say.
#[must_use]
pub fn models_url(settings: &Settings) -> Option<String> {
    let endpoint = settings.endpoint.trim_end_matches('/');
    let base = endpoint
        .strip_suffix("/messages")
        .or_else(|| endpoint.strip_suffix("/chat/completions"))?;
    Some(format!("{base}/models"))
}

/// How many pages of a model list are followed before stopping. A service
/// that pages forever is a bug on its side, not a reason to hang here.
const MAX_MODEL_PAGES: usize = 10;

/// The ids of the models the route's service offers, as its own list says.
///
/// # Errors
///
/// When the endpoint does not say where the list is, the key may not travel
/// there, the service does not answer, or its answer is not a model list.
pub fn list_models(settings: &Settings) -> Result<Vec<String>, String> {
    let url = models_url(settings).ok_or_else(|| {
        format!(
            "{} does not end in /messages or /chat/completions, so where its model list is \
             is not known",
            settings.endpoint
        )
    })?;
    super::catalogue::key_may_travel_to(&url)?;
    let mut ids: Vec<String> = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_MODEL_PAGES {
        let page = fetch_models_page(settings, &url, after.as_deref())?;
        let (page_ids, next) = parse_model_page(&page)?;
        ids.extend(page_ids);
        match next {
            Some(last) => after = Some(last),
            None => return Ok(ids),
        }
    }
    Ok(ids)
}

fn fetch_models_page(
    settings: &Settings,
    url: &str,
    after: Option<&str>,
) -> Result<String, String> {
    let mut query: Vec<(String, String)> = Vec::new();
    // The messages format pages its list; the most it hands over at once is
    // asked for, and the rest followed by `after_id`.
    if settings.format == WireFormat::Anthropic {
        query.push(("limit".to_string(), "1000".to_string()));
        if let Some(after) = after {
            query.push(("after_id".to_string(), after.to_string()));
        }
    }
    let (url, key, auth, format) = (
        url.to_string(),
        settings.key.clone(),
        settings.auth,
        settings.format,
    );
    crate::runtime::http::request_text(
        move |client| {
            let request = client
                .get(&url)
                .query(&query)
                .timeout(std::time::Duration::from_secs(15));
            let request = match auth {
                super::api::KeyHeader::XApiKey => request.header("x-api-key", key),
                super::api::KeyHeader::Bearer => request.bearer_auth(key),
            };
            match format {
                WireFormat::Anthropic => request.header("anthropic-version", "2023-06-01"),
                WireFormat::Openai => request,
            }
        },
        "model service",
    )
}

/// One page of a model list: its ids, and the id to continue after when the
/// service says there is more. Both formats list under `data`, each with an
/// `id`; the messages format adds `has_more` and `last_id`.
fn parse_model_page(body: &str) -> Result<(Vec<String>, Option<String>), String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("the model list did not parse: {e}"))?;
    let data = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or("the answer is not a model list: it has no data")?;
    let ids = data
        .iter()
        .filter_map(|m| m.get("id").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect();
    let more = value.get("has_more").and_then(serde_json::Value::as_bool) == Some(true);
    let next = if more {
        value
            .get("last_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    } else {
        None
    };
    Ok((ids, next))
}

/// The key the variable `key_env` names holds.
fn key_from(key_env: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let key_env = key_env.trim();
    if key_env.is_empty() {
        return Err("no key_env names the variable that holds its key".to_string());
    }
    env(key_env).ok_or_else(|| format!("{key_env} is not set"))
}

/// The settings for the API route `id` names, from the environment and
/// admin.json; `None` when it names none, or one that cannot be driven now.
#[must_use]
pub fn settings_for(id: &str) -> Option<Settings> {
    settings_in(id, &AdminConfig::load_default(), &super::api::non_empty)
}

/// [`settings_for`], from given inputs, so a test sets neither the process
/// environment nor the operator's file.
pub(crate) fn settings_in(
    id: &str,
    admin: &AdminConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<Settings> {
    let id = id.trim();
    if id.eq_ignore_ascii_case(API_ROUTE) {
        return Settings::from_lookup(env)
            .filter(|s| super::catalogue::key_may_travel_to(&s.endpoint).is_ok());
    }
    if let Some(model) = admin
        .api_models
        .iter()
        .find(|m| m.name.trim().eq_ignore_ascii_case(id))
    {
        return model_settings(model, env).ok();
    }
    let (provider, agent) = id.split_once('/')?;
    let provider = admin
        .api_providers
        .iter()
        .find(|p| p.name.trim().eq_ignore_ascii_case(provider))?;
    provider_settings(provider, agent, env).ok()
}

/// Whether `id` names a route over an API rather than an installed CLI.
///
/// Such a route holds its conversation in this process: there is no session
/// on the far side to resume, and nothing to keep warm as a standby.
#[must_use]
pub fn is_api_route(id: &str) -> bool {
    let id = id.trim();
    // A local agent's id never holds a `/`; a provider agent's always does.
    id.eq_ignore_ascii_case(API_ROUTE)
        || id.contains('/')
        || AdminConfig::load_default()
            .api_models
            .iter()
            .any(|m| m.name.trim().eq_ignore_ascii_case(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::api::KeyHeader;

    fn env_with(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    fn model(name: &str, format: WireFormat, url: &str) -> ApiModel {
        ApiModel {
            name: name.to_string(),
            format,
            url: url.to_string(),
            key_env: "MODEL_KEY_VARIABLE".to_string(),
            model: "some-model".to_string(),
            sub_model: String::new(),
            auth: None,
        }
    }

    fn admin() -> AdminConfig {
        AdminConfig {
            api_models: vec![
                model(
                    "messages-service",
                    WireFormat::Anthropic,
                    "https://a.invalid/v1/messages",
                ),
                ApiModel {
                    auth: Some(KeyHeader::Bearer),
                    sub_model: "a-fast-model".to_string(),
                    ..model(
                        "token-service",
                        WireFormat::Anthropic,
                        "https://b.invalid/v1/messages",
                    )
                },
                model(
                    "chat-service",
                    WireFormat::Openai,
                    "https://c.invalid/v1/chat/completions",
                ),
            ],
            api_providers: vec![ApiProvider {
                name: "team".to_string(),
                url: "https://team.invalid/".to_string(),
                key_env: "TEAM_KEY_VARIABLE".to_string(),
            }],
            ..AdminConfig::default()
        }
    }

    const KEYS: &[(&str, &str)] = &[
        ("MODEL_KEY_VARIABLE", "k-model"),
        ("TEAM_KEY_VARIABLE", "k-team"),
    ];

    #[test]
    fn a_configured_model_resolves_with_its_own_format_and_header() {
        let env = env_with(KEYS);
        let messages = settings_in("messages-service", &admin(), &env).expect("resolves");
        assert_eq!(messages.route, "messages-service");
        assert_eq!(messages.format, WireFormat::Anthropic);
        assert_eq!(messages.auth, KeyHeader::XApiKey, "the format's own header");
        assert_eq!(messages.key, "k-model");
        assert_eq!(messages.model, "some-model");

        let token = settings_in("TOKEN-SERVICE", &admin(), &env).expect("case does not matter");
        assert_eq!(token.auth, KeyHeader::Bearer, "the entry's own choice wins");
        assert_eq!(
            token.sub_model, "a-fast-model",
            "its sub-agents' model travels too"
        );
        assert!(messages.sub_model.is_empty(), "none named, none invented");

        let chat = settings_in("chat-service", &admin(), &env).expect("resolves");
        assert_eq!(chat.format, WireFormat::Openai);
        assert_eq!(chat.auth, KeyHeader::Bearer);
    }

    /// **A PROVIDER AGENT IS DRIVEN**: chat completions at the contract's
    /// path, asking for the agent's id as the model.
    #[test]
    fn a_provider_agent_resolves_to_the_contract_endpoint() {
        let settings = settings_in("team/coder", &admin(), &env_with(KEYS)).expect("resolves");
        assert_eq!(
            settings.endpoint,
            "https://team.invalid/v1/chat/completions"
        );
        assert_eq!(settings.model, "coder");
        assert_eq!(settings.format, WireFormat::Openai);
        assert_eq!(settings.key, "k-team");
        assert_eq!(settings.route, "team/coder");
    }

    #[test]
    fn a_missing_key_or_an_unknown_id_resolves_to_nothing() {
        assert!(settings_in("chat-service", &admin(), &env_with(&[])).is_none());
        assert!(settings_in("team/coder", &admin(), &env_with(&[])).is_none());
        assert!(settings_in("claude-code", &admin(), &env_with(KEYS)).is_none());
        assert!(settings_in("nobody/coder", &admin(), &env_with(KEYS)).is_none());
    }

    /// The key never travels in clear to another machine, whichever kind of
    /// route it belongs to.
    #[test]
    fn a_key_is_never_sent_over_plain_http_to_another_machine() {
        let mut admin = admin();
        admin.api_models[0].url = "http://a.invalid/v1/messages".to_string();
        admin.api_providers[0].url = "http://team.invalid".to_string();
        let env = env_with(KEYS);
        let why = model_settings(&admin.api_models[0], &env).expect_err("refused");
        assert!(why.contains("https"), "{why}");
        assert!(settings_in("team/coder", &admin, &env).is_none());
        let exposed = env_with(&[
            ("INCODE_API_URL", "http://a.invalid/v1/messages"),
            ("INCODE_API_KEY", "k"),
        ]);
        assert!(settings_in(API_ROUTE, &admin, &exposed).is_none());
        // The control: this machine is fine, as a local server is.
        admin.api_models[0].url = "http://127.0.0.1:8080/v1/messages".to_string();
        assert!(model_settings(&admin.api_models[0], &env).is_ok());
    }

    /// Serve `replies` in order, one per connection, returning each request's
    /// head (lowercased) from the join handle.
    fn serve(replies: Vec<&'static str>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let mut heads = Vec::new();
            for reply in replies {
                let (mut conn, _) = listener.accept().expect("accept");
                let mut request = Vec::new();
                let mut buf = [0u8; 2048];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = conn.read(&mut buf).expect("read");
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                }
                heads.push(String::from_utf8_lossy(&request).to_ascii_lowercase());
                let answer = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                conn.write_all(answer.as_bytes()).expect("write");
            }
            heads
        });
        (format!("http://{addr}"), server)
    }

    fn route(endpoint: String, format: WireFormat) -> Settings {
        Settings {
            endpoint,
            key: "k-9".to_string(),
            format,
            auth: format.default_auth(),
            route: "service".to_string(),
            ..Settings::default()
        }
    }

    #[test]
    fn the_model_list_sits_beside_the_endpoint() {
        let at = |endpoint: &str| models_url(&route(endpoint.to_string(), WireFormat::Anthropic));
        assert_eq!(
            at("https://api.invalid/v1/messages").as_deref(),
            Some("https://api.invalid/v1/models")
        );
        assert_eq!(
            at("https://router.invalid/api/v1/chat/completions/").as_deref(),
            Some("https://router.invalid/api/v1/models")
        );
        assert_eq!(at("https://odd.invalid/generate"), None);
    }

    /// **EVERY MODEL, NOT THE FIRST PAGE.** The messages format pages its
    /// list; each page is followed by `after_id` until it says there is no
    /// more, with the format's own key header and version on every request.
    #[test]
    fn the_messages_formats_model_list_is_followed_to_its_end() {
        let (base, server) = serve(vec![
            r#"{"data":[{"id":"model-a"},{"id":"model-b"}],"has_more":true,"last_id":"model-b"}"#,
            r#"{"data":[{"id":"model-c"}],"has_more":false,"last_id":"model-c"}"#,
        ]);
        let ids = list_models(&route(format!("{base}/v1/messages"), WireFormat::Anthropic))
            .expect("listed");
        // Checked BEFORE the server is waited on: a client that stopped early
        // leaves the server waiting for a request that never comes, and the
        // test must fail on the ids rather than hang on the join.
        assert_eq!(ids, ["model-a", "model-b", "model-c"]);
        let heads = server.join().expect("server");
        assert!(
            heads[0].starts_with("get /v1/models?limit=1000 "),
            "{}",
            heads[0]
        );
        assert!(heads[1].contains("after_id=model-b"), "{}", heads[1]);
        for head in &heads {
            assert!(head.contains("x-api-key: k-9"), "{head}");
            assert!(head.contains("anthropic-version: 2023-06-01"), "{head}");
            assert!(!head.contains("authorization:"), "{head}");
        }
    }

    /// The chat format's list: one page, the key as a bearer.
    #[test]
    fn the_chat_formats_model_list_is_read_with_a_bearer() {
        let (base, server) = serve(vec![r#"{"data":[{"id":"x/one"},{"id":"y/two"}]}"#]);
        let ids = list_models(&route(
            format!("{base}/api/v1/chat/completions"),
            WireFormat::Openai,
        ))
        .expect("listed");
        assert_eq!(ids, ["x/one", "y/two"]);
        let heads = server.join().expect("server");
        assert!(heads[0].starts_with("get /api/v1/models "), "{}", heads[0]);
        assert!(
            heads[0].contains("authorization: bearer k-9"),
            "{}",
            heads[0]
        );
        assert!(!heads[0].contains("anthropic-version"), "{}", heads[0]);
    }

    #[test]
    fn the_environment_route_reads_its_format_and_header() {
        let env = env_with(&[
            ("INCODE_API_URL", "https://x.invalid/v1/chat/completions"),
            ("INCODE_API_KEY", "k"),
            ("INCODE_API_FORMAT", "openai"),
        ]);
        let settings = settings_in("api", &AdminConfig::default(), &env).expect("resolves");
        assert_eq!(settings.format, WireFormat::Openai);
        assert_eq!(settings.auth, KeyHeader::Bearer);
        assert_eq!(settings.route, API_ROUTE);
        // A word that names no format is not quietly read as the default.
        let typo = env_with(&[
            ("INCODE_API_URL", "https://x.invalid/v1/messages"),
            ("INCODE_API_KEY", "k"),
            ("INCODE_API_FORMAT", "openia"),
        ]);
        assert!(settings_in("api", &AdminConfig::default(), &typo).is_none());
    }
}
