//! Every agent this session could use, in one list.
//!
//! # Three sources, and none of them is a list in this code
//!
//! - **Local agents** — CLIs found on this machine by
//!   [`super::registry::discover`]. The operator never lists them; they are
//!   found, and the operator only picks.
//! - **The API route** — the one API configured by [`API_URL_ENV`],
//!   [`API_KEY_ENV`] and [`super::api::API_MODEL_ENV`]. It used to REPLACE
//!   every local agent the moment it was set, so an operator who exported two
//!   variables for one experiment silently lost the agents they had installed.
//!   It is now one more entry, and leads only when chosen or when nothing local
//!   can.
//! - **Provider agents** — each service in admin.json `api_providers` is asked
//!   which agents it serves. The request and the reply are specified once, on
//!   [`parse_agents`].
//!
//! # Usable, or why not
//!
//! Every entry says whether a turn can be sent to it now, and when it cannot,
//! why. A provider that cannot be reached, has no key, or answers with
//! something unreadable is ONE line saying so — never a silent absence, because
//! an agent that vanished from the list reads exactly like one that was never
//! configured, and the operator goes looking in the wrong place.
//!
//! # The lead and the sub-agents
//!
//! The lead is the agent the session talks to. The sub-agent is the agent that
//! takes the work the lead hands out. By default it is the lead's entry in the
//! built-in table ([`super::registry::AgentProbe::default_sub_agent`]); the
//! operator may name another. This picks an AGENT. Which MODEL a kind of work
//! runs on is the role table's job ([`crate::roles`]), and the two are kept
//! separate.

use std::time::Duration;

use serde::Deserialize;

use super::api::{Settings, API_AUTH_ENV, API_FORMAT_ENV, API_KEY_ENV, API_MODEL_ENV, API_URL_ENV};
use super::registry::{no_drivable_reason, AgentProbe, DetectedAgent, API_ROUTE, KNOWN_AGENTS};
use crate::admin::{ApiModel, ApiProvider};

/// How long one provider has to answer with its agent list.
///
/// The list is read while the session starts, before the first prompt, so a
/// provider that does not answer is a person waiting to type. Three seconds is
/// far above a healthy service's reply to one small GET, and short enough that
/// a dead one costs a pause rather than a terminal that looks hung. Providers
/// are asked one after another, so the worst case is this times the number
/// configured.
pub const PROVIDER_TIMEOUT: Duration = Duration::from_secs(3);

/// The longest name or model shown for a provider-listed agent.
///
/// These strings come from another service and are printed on the operator's
/// terminal, so they are cut to a line's worth as well as cleaned.
const MAX_LISTED_TEXT: usize = 80;

/// Where an agent runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A CLI installed on this machine.
    Local,
    /// A model reached over an HTTP API.
    Api,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Api => "api",
        }
    }
}

/// Whether a turn can be sent to an entry now.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    /// It can, and this is the route a backend is built from.
    Usable(DetectedAgent),
    /// It cannot, and this is why.
    Unusable(String),
}

/// One agent in the catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    /// What the operator types to choose it.
    id: String,
    /// What it is called, for a person.
    name: String,
    kind: Kind,
    /// A local agent's version, or an API agent's model.
    detail: Option<String>,
    status: Status,
}

impl Entry {
    fn route(&self) -> Option<&DetectedAgent> {
        match &self.status {
            Status::Usable(agent) => Some(agent),
            Status::Unusable(_) => None,
        }
    }

    /// One line of `/agent`: id, kind, name, version or model, and usable or
    /// why not.
    fn line(&self, lead: &str) -> String {
        let marker = if self.id.eq_ignore_ascii_case(lead) {
            '*'
        } else {
            ' '
        };
        let state = match &self.status {
            Status::Usable(_) => "usable".to_string(),
            Status::Unusable(why) => format!("not usable: {why}"),
        };
        format!(
            "{marker} {:<20} {:<5}  {:<18} {:<24} {state}",
            self.id,
            self.kind.as_str(),
            self.name,
            self.detail.as_deref().unwrap_or("-"),
        )
    }
}

/// The agent that takes the work the lead hands out, and how it was decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubAgent {
    pub agent: DetectedAgent,
    /// `your choice`, or `default for <lead>`.
    pub origin: String,
    /// What was passed over on the way, when something was.
    pub note: Option<String>,
}

/// Every agent this session could use.
#[derive(Debug, Clone, Default)]
pub struct Catalogue {
    entries: Vec<Entry>,
    /// Every local agent found, drivable or not — what an empty choice is
    /// explained from.
    found: Vec<DetectedAgent>,
}

impl Catalogue {
    /// Probe this machine, read the API variables and the configured models,
    /// and ask each provider.
    #[must_use]
    pub fn gather(admin: &crate::admin::AdminConfig) -> Self {
        Self::assemble_with(
            super::registry::discover(),
            &admin.api_providers,
            &admin.api_models,
            &super::api::non_empty,
            &fetch_agents,
        )
    }

    /// Every local agent the machine was probed for, drivable or not: what
    /// `discover` found when the catalogue was gathered.
    #[must_use]
    pub(crate) fn found(&self) -> &[DetectedAgent] {
        &self.found
    }

    /// The catalogue from its inputs, with nothing read from the machine.
    ///
    /// `env` answers a variable's trimmed, non-empty value; `fetch` answers a
    /// provider's agent list for `(url, key)`. Both are seams so every outcome
    /// — each failure included — can be tested without a network.
    #[cfg(test)]
    fn assemble(
        found: Vec<DetectedAgent>,
        providers: &[ApiProvider],
        env: &dyn Fn(&str) -> Option<String>,
        fetch: &dyn Fn(&str, &str) -> Result<String, String>,
    ) -> Self {
        Self::assemble_with(found, providers, &[], env, fetch)
    }

    /// [`Self::assemble`], with the models configured in admin.json.
    fn assemble_with(
        found: Vec<DetectedAgent>,
        providers: &[ApiProvider],
        models: &[ApiModel],
        env: &dyn Fn(&str) -> Option<String>,
        fetch: &dyn Fn(&str, &str) -> Result<String, String>,
    ) -> Self {
        let mut entries: Vec<Entry> = found.iter().map(local_entry).collect();
        entries.extend(api_route_entry(env));
        for model in models {
            let entry = model_entry(model, &entries, providers, env);
            entries.push(entry);
        }
        let mut seen: Vec<&str> = Vec::new();
        for provider in providers {
            entries.extend(provider_entries(provider, &seen, env, fetch));
            seen.push(provider.name.trim());
        }
        Self { entries, found }
    }

    /// A catalogue of these local agents alone, for another module's tests.
    #[cfg(test)]
    pub(crate) fn of_local(found: Vec<DetectedAgent>) -> Self {
        Self::assemble(found, &[], &|_| None, &|_, _| {
            Err("no provider in this catalogue".to_string())
        })
    }

    fn find(&self, id: &str) -> Option<&Entry> {
        let id = id.trim();
        self.entries.iter().find(|e| e.id.eq_ignore_ascii_case(id))
    }

    /// The route for one id, or why it cannot take work.
    ///
    /// # Errors
    ///
    /// When the id is not in the catalogue, naming what is, or when it is but
    /// cannot be used now, with the reason.
    pub fn route_for(&self, id: &str) -> Result<DetectedAgent, String> {
        match self.find(id) {
            Some(entry) => match &entry.status {
                Status::Usable(agent) => Ok(agent.clone()),
                Status::Unusable(why) => Err(format!("`{}` cannot be used: {why}", entry.id)),
            },
            None => {
                let known: Vec<&str> = self.entries.iter().map(|e| e.id.as_str()).collect();
                Err(format!(
                    "`{}` is not in the catalogue. Known: {}",
                    id.trim(),
                    if known.is_empty() {
                        "none".to_string()
                    } else {
                        known.join(", ")
                    }
                ))
            }
        }
    }

    /// Every id, split into those that can take work now and those that
    /// cannot, each in catalogue order.
    #[must_use]
    pub fn ids_by_usability(&self) -> (Vec<String>, Vec<String>) {
        let (usable, unusable): (Vec<&Entry>, Vec<&Entry>) =
            self.entries.iter().partition(|e| e.route().is_some());
        let ids = |entries: Vec<&Entry>| entries.into_iter().map(|e| e.id.clone()).collect();
        (ids(usable), ids(unusable))
    }

    /// Every usable agent, in the order to try them.
    ///
    /// The one asked for first when it is usable; then every usable local
    /// agent in discovery order; then every usable API agent. A local agent
    /// comes before an API one because that is what a session did before the
    /// catalogue existed for anyone who configured nothing, and a machine that
    /// has both should not start billing an API because a variable is set.
    ///
    /// # Errors
    ///
    /// When nothing is usable, with the reason that tells the operator which
    /// fix they need.
    pub fn usable_chain(&self, preferred: Option<&str>) -> Result<Vec<DetectedAgent>, String> {
        let mut chain: Vec<DetectedAgent> = Vec::new();
        if let Some(want) = preferred.map(str::trim).filter(|w| !w.is_empty()) {
            match self.route_for(want) {
                Ok(agent) => chain.push(agent),
                Err(why) => crate::trace!("agent", "{why}; falling back"),
            }
        }
        for kind in [Kind::Local, Kind::Api] {
            for agent in self
                .entries
                .iter()
                .filter(|e| e.kind == kind)
                .filter_map(Entry::route)
            {
                if !chain.iter().any(|c| c.id.eq_ignore_ascii_case(&agent.id)) {
                    chain.push(agent.clone());
                }
            }
        }
        if chain.is_empty() {
            Err(no_drivable_reason(&self.found))
        } else {
            Ok(chain)
        }
    }

    /// The agent that leads: the first of [`Self::usable_chain`].
    ///
    /// # Errors
    ///
    /// When nothing is usable.
    pub fn lead(&self, preferred: Option<&str>) -> Result<DetectedAgent, String> {
        self.usable_chain(preferred)?
            .into_iter()
            .next()
            .ok_or_else(|| no_drivable_reason(&self.found))
    }

    /// The agent that takes the work `lead` hands out.
    ///
    /// 1. The operator's choice (`chosen`, admin.json `sub_agent`) when it is
    ///    usable.
    /// 2. Otherwise the lead's entry in the built-in table beside
    ///    [`KNOWN_AGENTS`].
    /// 3. When that is not usable — or the lead has no entry, as an API lead
    ///    does not — the lead itself.
    ///
    /// Whatever was passed over on the way is carried in `note`, so `/agent`
    /// can say it rather than leave a choice that quietly did not apply.
    #[must_use]
    pub fn sub_agent(&self, lead: &DetectedAgent, chosen: Option<&str>) -> SubAgent {
        self.sub_agent_by(lead, chosen, KNOWN_AGENTS)
    }

    fn sub_agent_by(
        &self,
        lead: &DetectedAgent,
        chosen: Option<&str>,
        table: &[AgentProbe],
    ) -> SubAgent {
        let mut passed_over: Vec<String> = Vec::new();
        if let Some(want) = chosen.map(str::trim).filter(|w| !w.is_empty()) {
            match self.route_for(want) {
                Ok(agent) => {
                    return SubAgent {
                        agent,
                        origin: "your choice".to_string(),
                        note: None,
                    }
                }
                Err(why) => passed_over.push(format!("your choice is not used: {why}")),
            }
        }
        let origin = format!("default for {}", lead.id);
        let default = table
            .iter()
            .find(|probe| probe.id.eq_ignore_ascii_case(&lead.id))
            .map(|probe| probe.default_sub_agent);
        if let Some(default) = default {
            match self.route_for(default) {
                Ok(agent) => {
                    return SubAgent {
                        agent,
                        origin,
                        note: joined(passed_over),
                    }
                }
                Err(why) => passed_over.push(format!(
                    "the default for {}, {default}, is not usable ({why}), so the lead itself",
                    lead.id
                )),
            }
        }
        SubAgent {
            agent: lead.clone(),
            origin,
            note: joined(passed_over),
        }
    }

    /// What `/agent` prints: every entry, the lead, the sub-agent and where it
    /// came from, and the lead the saved choice will give the next session.
    #[must_use]
    pub fn report(&self, lead: &DetectedAgent, sub: &SubAgent, preferred: Option<&str>) -> String {
        let mut out = String::from("Agents (* = this session's lead)\n");
        for entry in &self.entries {
            out.push_str(&entry.line(&lead.id));
            out.push('\n');
        }
        out.push_str(&format!("  lead         {}\n", lead.id));
        out.push_str(&format!(
            "  sub-agents   {} ({})\n",
            sub.agent.id, sub.origin
        ));
        if let Some(note) = &sub.note {
            out.push_str(&format!("               {note}\n"));
        }
        if let Ok(next) = self.lead(preferred) {
            if !next.id.eq_ignore_ascii_case(&lead.id) {
                out.push_str(&format!(
                    "  next lead    {} (saved; this session stays on {})\n",
                    next.id, lead.id
                ));
            }
        }
        out
    }
}

fn joined(notes: Vec<String>) -> Option<String> {
    (!notes.is_empty()).then(|| notes.join("; "))
}

fn local_entry(agent: &DetectedAgent) -> Entry {
    Entry {
        id: agent.id.clone(),
        name: agent.display_name.clone(),
        kind: Kind::Local,
        detail: agent.version.clone(),
        status: if agent.is_drivable() {
            Status::Usable(agent.clone())
        } else {
            Status::Unusable(
                "its headless protocol has not been verified here, so it is not driven".to_string(),
            )
        },
    }
}

/// The API route configured by environment variables, when any of them is set.
///
/// Usable by exactly the rule that builds its backend
/// ([`Settings::from_lookup`]); half-configured, it is listed with the
/// variable that is missing rather than left out.
fn api_route_entry(env: &dyn Fn(&str) -> Option<String>) -> Option<Entry> {
    let url = env(API_URL_ENV);
    let key = env(API_KEY_ENV);
    if url.is_none() && key.is_none() {
        return None;
    }
    let status = match Settings::from_lookup(env) {
        Some(settings) => match key_may_travel_to(&settings.endpoint) {
            Ok(()) => Status::Usable(DetectedAgent::over_api(&settings.model)),
            Err(why) => Status::Unusable(format!("{API_URL_ENV}: {why}")),
        },
        None if url.is_none() => Status::Unusable(format!("{API_URL_ENV} is not set")),
        None if key.is_none() => Status::Unusable(format!("{API_KEY_ENV} is not set")),
        // Both are set, so what failed is a word naming a format or a header.
        None => Status::Unusable(format!(
            "{API_FORMAT_ENV} must be `anthropic` or `openai`, and {API_AUTH_ENV} \
             `x-api-key` or `bearer`"
        )),
    };
    Some(Entry {
        id: API_ROUTE.to_string(),
        name: "configured API".to_string(),
        kind: Kind::Api,
        detail: env(API_MODEL_ENV),
        status,
    })
}

/// One model configured in admin.json `api_models`: usable by exactly the rule
/// its backend is built by ([`super::models::model_settings`]), or listed with
/// why not.
fn model_entry(
    model: &ApiModel,
    earlier: &[Entry],
    providers: &[ApiProvider],
    env: &dyn Fn(&str) -> Option<String>,
) -> Entry {
    let name = model.name.trim();
    let id = if name.is_empty() {
        "(unnamed model)".to_string()
    } else {
        name.to_string()
    };
    // An id that is already taken would make `/agent <id>` choose whichever
    // was listed first, so the later one is refused by name instead.
    let clash = earlier.iter().any(|e| e.id.eq_ignore_ascii_case(name))
        || providers
            .iter()
            .any(|p| p.name.trim().eq_ignore_ascii_case(name))
        || name.contains('/');
    let status = if clash {
        Status::Unusable(format!(
            "`{name}` is already an agent's or a provider's id, or holds a `/`; give it another name"
        ))
    } else {
        match super::models::model_settings(model, env) {
            Ok(settings) => Status::Usable(DetectedAgent::api_route(&settings)),
            Err(why) => Status::Unusable(why),
        }
    };
    Entry {
        id,
        name: format!("api model ({})", model.format.as_str()),
        kind: Kind::Api,
        detail: Some(model.model.trim().to_string()).filter(|m| !m.is_empty()),
        status,
    }
}

/// One provider's agents, or one line saying why there are none.
fn provider_entries(
    provider: &ApiProvider,
    seen: &[&str],
    env: &dyn Fn(&str) -> Option<String>,
    fetch: &dyn Fn(&str, &str) -> Result<String, String>,
) -> Vec<Entry> {
    let name = provider.name.trim();
    let broken = |why: String| {
        vec![Entry {
            id: if name.is_empty() {
                "(unnamed provider)".to_string()
            } else {
                name.to_string()
            },
            name: "provider".to_string(),
            kind: Kind::Api,
            detail: None,
            status: Status::Unusable(why),
        }]
    };
    if name.is_empty() {
        return broken("this provider has no name, so its agents would have no id".to_string());
    }
    if seen.iter().any(|s| s.eq_ignore_ascii_case(name)) {
        return broken(format!("another provider is already named `{name}`"));
    }
    let url = provider.url.trim();
    if url.is_empty() {
        return broken("no url is configured".to_string());
    }
    if let Err(why) = key_may_travel_to(url) {
        return broken(why);
    }
    let key_env = provider.key_env.trim();
    if key_env.is_empty() {
        return broken("no key_env names the variable that holds its key".to_string());
    }
    let Some(key) = env(key_env) else {
        return broken(format!("{key_env} is not set"));
    };
    let listed = match fetch(url, &key).and_then(|body| parse_agents(&body)) {
        Ok(listed) => listed,
        Err(why) => return broken(format!("could not list its agents: {why}")),
    };
    if listed.is_empty() {
        return broken("it lists no agents".to_string());
    }
    listed
        .into_iter()
        .map(|agent| {
            // Driven by the rule its backend is built by, so listing it as
            // usable and building it cannot disagree.
            let status = match super::models::provider_settings(provider, &agent.id, env) {
                Ok(settings) => Status::Usable(DetectedAgent::api_route(&settings)),
                Err(why) => Status::Unusable(why),
            };
            Entry {
                id: format!("{name}/{}", agent.id),
                name: agent.name.unwrap_or_else(|| agent.id.clone()),
                kind: Kind::Api,
                detail: agent.model,
                status,
            }
        })
        .collect()
}

/// One agent as a provider lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ListedAgent {
    id: String,
    name: Option<String>,
    model: Option<String>,
}

#[derive(Deserialize)]
struct WireList {
    agents: Vec<WireAgent>,
}

#[derive(Deserialize)]
struct WireAgent {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// Read a provider's agent list. THE PROVIDER CONTRACT is stated here, once.
///
/// # The request
///
/// `GET {url}/v1/agents`, where `url` is the provider's `url` from admin.json
/// with any trailing `/` removed, carrying `Authorization: Bearer <key>`, where
/// the key is the value of the environment variable the provider's `key_env`
/// names. The whole request must finish within [`PROVIDER_TIMEOUT`]. The url
/// must be `https`, or plain `http` to this machine only
/// ([`key_may_travel_to`]); anything else is refused before the key is sent.
///
/// # The reply
///
/// A 2xx status with a JSON body:
///
/// ```json
/// {"agents": [{"id": "coder", "name": "Coder", "model": "model-1"}]}
/// ```
///
/// - `agents` is required and may be empty.
/// - `id` is required: non-empty, with no whitespace or control characters,
///   and unique within the reply. It becomes the catalogue id
///   `<provider name>/<id>`.
/// - `name` and `model` are optional and shown beside the agent; a missing
///   `name` shows the `id`. Control characters are removed from both and they
///   are cut to [`MAX_LISTED_TEXT`] characters.
/// - Any other field, at either level, is ignored.
///
/// Any other status, a body that is not this shape, or a broken rule above
/// lists the provider as one entry that is not usable, with the reason.
fn parse_agents(body: &str) -> Result<Vec<ListedAgent>, String> {
    let list: WireList =
        serde_json::from_str(body).map_err(|e| format!("the reply is not an agent list: {e}"))?;
    let mut agents: Vec<ListedAgent> = Vec::new();
    for (index, wire) in list.agents.into_iter().enumerate() {
        let id = wire.id.trim();
        if id.is_empty() || id.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!(
                "agent #{} has an id that is empty or holds spaces or control characters",
                index + 1
            ));
        }
        if agents.iter().any(|a| a.id == id) {
            return Err(format!("`{id}` is listed twice"));
        }
        agents.push(ListedAgent {
            id: id.to_string(),
            name: wire.name.as_deref().and_then(clean),
            model: wire.model.as_deref().and_then(clean),
        });
    }
    Ok(agents)
}

/// Text from another service, fit for the operator's terminal.
fn clean(raw: &str) -> Option<String> {
    let text: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_LISTED_TEXT)
        .collect();
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Whether a provider's key may be sent to `url`.
///
/// The key travels in a header, so over plain `http` anything on the path
/// between here and the provider can read it. `https` is accepted anywhere;
/// plain `http` only to this machine, where a provider is run while it is
/// being built and the traffic never leaves the host.
///
/// The one statement of the rule: the web search provider's key
/// ([`crate::web`]) is held to it too.
pub(crate) fn key_may_travel_to(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("its url is not valid: {e}"))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if parsed.host_str().is_some_and(is_this_machine) => Ok(()),
        "http" => Err(
            "its url is plain http to another machine, which would send the key \
             unencrypted; use https"
                .to_string(),
        ),
        other => Err(format!(
            "its url uses `{other}`; only https, or http to this machine, is accepted"
        )),
    }
}

/// Whether a URL's host names this machine: `localhost` or a loopback address.
fn is_this_machine(host: &str) -> bool {
    // An IPv6 host keeps its brackets in a URL.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Ask one provider for its agent list, under [`PROVIDER_TIMEOUT`].
fn fetch_agents(url: &str, key: &str) -> Result<String, String> {
    let endpoint = format!("{}/v1/agents", url.trim_end_matches('/'));
    let key = key.to_string();
    crate::runtime::http::request_text(
        move |client| {
            client
                .get(&endpoint)
                .bearer_auth(key)
                .timeout(PROVIDER_TIMEOUT)
        },
        "agent provider",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::registry::{Protocol, ANTIGRAVITY, CODEX};
    use std::cell::Cell;

    fn local(id: &str, protocol: Protocol) -> DetectedAgent {
        DetectedAgent {
            id: id.to_string(),
            display_name: id.to_string(),
            command: id.to_string(),
            path: Some(format!("/bin/{id}")),
            version: Some("1.0".to_string()),
            protocol,
        }
    }

    fn provider(name: &str, key_env: &str) -> ApiProvider {
        ApiProvider {
            name: name.to_string(),
            url: "https://provider.invalid/".to_string(),
            key_env: key_env.to_string(),
        }
    }

    /// An environment holding exactly these variables.
    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    fn no_network(_: &str, _: &str) -> Result<String, String> {
        panic!("nothing in this test may reach a provider")
    }

    fn ids(catalogue: &Catalogue) -> Vec<&str> {
        catalogue.entries.iter().map(|e| e.id.as_str()).collect()
    }

    fn entry<'a>(catalogue: &'a Catalogue, id: &str) -> &'a Entry {
        catalogue
            .find(id)
            .unwrap_or_else(|| panic!("`{id}` is missing from {:?}", ids(catalogue)))
    }

    /// **A provider's agents appear beside the local ones, under the provider's
    /// name.** The provider is asked with the key its variable holds, and each
    /// agent's id is prefixed so it cannot collide with a local agent's.
    #[test]
    fn a_providers_agents_appear_beside_the_local_agents() {
        let asked = Cell::new(0);
        let fetch = |url: &str, key: &str| {
            asked.set(asked.get() + 1);
            assert_eq!(url, "https://provider.invalid/");
            assert_eq!(key, "the-key-value");
            Ok(r#"{"agents":[{"id":"coder","name":"Coder","model":"model-1"},{"id":"claude-code"}]}"#
                .to_string())
        };
        let catalogue = Catalogue::assemble(
            vec![local("claude-code", Protocol::ClaudeStreamJson)],
            &[provider("team", "TEAM_KEY")],
            &env_of(&[("TEAM_KEY", "the-key-value")]),
            &fetch,
        );
        assert_eq!(asked.get(), 1, "the provider is asked exactly once");
        assert_eq!(
            ids(&catalogue),
            vec!["claude-code", "team/coder", "team/claude-code"],
            "a provider agent named like a local one must not replace it"
        );
        let coder = entry(&catalogue, "team/coder");
        assert_eq!(coder.kind, Kind::Api);
        assert_eq!(coder.name, "Coder");
        assert_eq!(coder.detail.as_deref(), Some("model-1"));
        assert_eq!(entry(&catalogue, "team/claude-code").name, "claude-code");
    }

    /// **A provider whose key variable is unset is listed, NOT usable, with
    /// that reason** — and it is not asked at all, since a request without the
    /// key could only be refused.
    #[test]
    fn a_provider_whose_key_variable_is_unset_is_listed_as_not_usable_with_that_reason() {
        let catalogue = Catalogue::assemble(
            Vec::new(),
            &[provider("team", "TEAM_KEY")],
            &env_of(&[]),
            &no_network,
        );
        assert_eq!(ids(&catalogue), vec!["team"]);
        assert_eq!(
            entry(&catalogue, "team").status,
            Status::Unusable("TEAM_KEY is not set".to_string())
        );
    }

    /// **Setting the API variables no longer hides the local agents.** They
    /// used to replace every installed CLI; the configured API is now one more
    /// entry, and with no choice made a local agent still leads.
    #[test]
    fn setting_the_api_variables_no_longer_hides_the_local_agents() {
        let catalogue = Catalogue::assemble(
            vec![
                local("claude-code", Protocol::ClaudeStreamJson),
                local(CODEX, Protocol::CodexJson),
            ],
            &[],
            &env_of(&[
                (API_URL_ENV, "https://endpoint.invalid/v1/messages"),
                (API_KEY_ENV, "k"),
                (API_MODEL_ENV, "some-model"),
            ]),
            &no_network,
        );
        assert_eq!(ids(&catalogue), vec!["claude-code", CODEX, API_ROUTE]);
        assert_eq!(
            catalogue.lead(None).map(|a| a.id),
            Ok("claude-code".to_string()),
            "with nothing chosen, the first usable LOCAL agent leads"
        );
        let chain: Vec<String> = catalogue
            .usable_chain(None)
            .unwrap_or_default()
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(chain, vec!["claude-code", CODEX, API_ROUTE]);
        assert_eq!(
            entry(&catalogue, API_ROUTE).detail.as_deref(),
            Some("some-model")
        );
    }

    /// The configured API leads when it is ASKED for, and when nothing local
    /// can — the two cases the old "the API replaces everything" rule served.
    #[test]
    fn the_configured_api_leads_when_chosen_or_when_nothing_local_can() {
        let env = env_of(&[
            (API_URL_ENV, "https://endpoint.invalid"),
            (API_KEY_ENV, "k"),
        ]);
        let with_local = Catalogue::assemble(
            vec![local("claude-code", Protocol::ClaudeStreamJson)],
            &[],
            &env,
            &no_network,
        );
        let chosen = with_local.lead(Some(API_ROUTE));
        assert_eq!(chosen.as_ref().map(|a| a.id.as_str()), Ok(API_ROUTE));
        assert_eq!(
            chosen.map(|a| a.protocol),
            Ok(Protocol::HarnessApi),
            "the API route is built as the API backend"
        );

        let only_unverified = Catalogue::assemble(
            vec![local("cursor", Protocol::Unverified)],
            &[],
            &env,
            &no_network,
        );
        assert_eq!(
            only_unverified.lead(None).map(|a| a.id),
            Ok(API_ROUTE.to_string())
        );
    }

    /// Half-configured, the API route is listed with the variable that is
    /// missing — not dropped, and not usable.
    #[test]
    fn a_half_configured_api_route_names_the_missing_variable() {
        let catalogue = Catalogue::assemble(
            Vec::new(),
            &[],
            &env_of(&[(API_URL_ENV, "https://endpoint.invalid")]),
            &no_network,
        );
        assert_eq!(
            entry(&catalogue, API_ROUTE).status,
            Status::Unusable(format!("{API_KEY_ENV} is not set"))
        );
        assert!(catalogue.lead(None).is_err(), "nothing usable, so no lead");
    }

    /// **Every way a provider can fail is ONE line with the reason** — never a
    /// panic, never a silent drop.
    #[test]
    fn every_provider_failure_is_one_unusable_line_with_its_reason() {
        let cases: [(&str, Result<String, String>, &str); 5] = [
            (
                "unreachable",
                Err("request failed: connection refused".to_string()),
                "connection refused",
            ),
            (
                "refused",
                Err("the agent provider answered 401 Unauthorized".to_string()),
                "401",
            ),
            ("garbled", Ok("<html>".to_string()), "not an agent list"),
            (
                "empty",
                Ok(r#"{"agents":[]}"#.to_string()),
                "lists no agents",
            ),
            (
                "twice",
                Ok(r#"{"agents":[{"id":"a"},{"id":"a"}]}"#.to_string()),
                "listed twice",
            ),
        ];
        for (name, reply, expected) in cases {
            let fetch = |_: &str, _: &str| reply.clone();
            let catalogue = Catalogue::assemble(
                Vec::new(),
                &[provider(name, "KEY")],
                &env_of(&[("KEY", "k")]),
                &fetch,
            );
            assert_eq!(ids(&catalogue), vec![name], "one line for `{name}`");
            match &entry(&catalogue, name).status {
                Status::Unusable(why) => assert!(
                    why.contains(expected),
                    "`{name}` must say `{expected}`, got: {why}"
                ),
                Status::Usable(_) => panic!("`{name}` is listed as usable"),
            }
        }
    }

    /// **The key is never sent unencrypted off this machine.** A provider
    /// reached over plain http to another host is refused before it is asked,
    /// so the key never leaves; https anywhere and http to loopback are asked.
    #[test]
    fn a_key_is_never_sent_over_plain_http_to_another_machine() {
        let asked: Cell<usize> = Cell::new(0);
        let fetch = |_: &str, _: &str| {
            asked.set(asked.get() + 1);
            Ok(r#"{"agents":[{"id":"coder"}]}"#.to_string())
        };
        let with_url = |url: &str| {
            let mut p = provider("team", "KEY");
            p.url = url.to_string();
            Catalogue::assemble(Vec::new(), &[p], &env_of(&[("KEY", "k")]), &fetch)
        };

        for url in [
            "http://agents.example.com",
            "http://192.168.1.20:8080",
            "ftp://agents.example.com",
            "not a url",
        ] {
            let catalogue = with_url(url);
            match &entry(&catalogue, "team").status {
                Status::Unusable(why) => assert!(
                    why.contains("http") || why.contains("not valid"),
                    "`{url}` refused without saying why: {why}"
                ),
                Status::Usable(_) => panic!("`{url}` is listed as usable"),
            }
        }
        assert_eq!(asked.get(), 0, "a refused url was still sent the key");

        for url in [
            "https://agents.example.com",
            "http://127.0.0.1:8080",
            "http://localhost:8080/",
            "http://[::1]:8080",
        ] {
            let catalogue = with_url(url);
            assert_eq!(ids(&catalogue), vec!["team/coder"], "`{url}` was not asked");
        }
        assert_eq!(asked.get(), 4);
    }

    /// **A PROVIDER'S AGENT IS DRIVEN.** Chosen, it leads, over its provider's
    /// chat-completions endpoint, asking for its own id as the model.
    #[test]
    fn a_provider_agent_is_usable_and_leads_when_chosen() {
        let fetch = |_: &str, _: &str| Ok(r#"{"agents":[{"id":"coder"}]}"#.to_string());
        let catalogue = Catalogue::assemble(
            vec![local("claude-code", Protocol::ClaudeStreamJson)],
            &[provider("team", "KEY")],
            &env_of(&[("KEY", "k")]),
            &fetch,
        );
        let lead = catalogue.lead(Some("team/coder")).expect("usable");
        assert_eq!(lead.id, "team/coder");
        assert_eq!(lead.protocol, Protocol::HarnessApi);
        assert_eq!(lead.version.as_deref(), Some("coder"));
    }

    /// A configured model is listed, usable when its key is set, and refused
    /// by name when it would take another entry's id.
    #[test]
    fn a_configured_model_is_listed_and_a_clashing_name_is_refused() {
        let model = |name: &str| ApiModel {
            name: name.to_string(),
            format: crate::agent::api::WireFormat::Openai,
            url: "https://models.invalid/v1/chat/completions".to_string(),
            key_env: "MODEL_KEY".to_string(),
            model: "some-model".to_string(),
            sub_model: String::new(),
            auth: None,
        };
        let catalogue = Catalogue::assemble_with(
            vec![local("claude-code", Protocol::ClaudeStreamJson)],
            &[],
            &[model("router"), model("claude-code"), model("unset")],
            &|name| (name == "MODEL_KEY").then(|| "k".to_string()),
            &|_, _| Err("no provider".to_string()),
        );
        assert_eq!(catalogue.lead(Some("router")).expect("usable").id, "router");
        assert_eq!(
            entry(&catalogue, "router").detail.as_deref(),
            Some("some-model")
        );
        // The second `claude-code` is refused; the local one still leads by it.
        assert_eq!(
            catalogue.lead(Some("claude-code")).expect("local").protocol,
            Protocol::ClaudeStreamJson
        );
        let clash = catalogue
            .entries
            .iter()
            .filter(|e| e.id == "claude-code")
            .nth(1)
            .expect("listed");
        assert!(matches!(&clash.status, Status::Unusable(why) if why.contains("another name")));
        let without_key =
            Catalogue::assemble_with(Vec::new(), &[], &[model("router")], &|_| None, &|_, _| {
                Err("no provider".to_string())
            });
        assert!(without_key
            .route_for("router")
            .is_err_and(|why| why.contains("MODEL_KEY is not set")));
    }

    /// Two providers with one name would give their agents the same ids.
    #[test]
    fn a_second_provider_with_the_same_name_is_refused() {
        let fetch = |_: &str, _: &str| Ok(r#"{"agents":[{"id":"coder"}]}"#.to_string());
        let catalogue = Catalogue::assemble(
            Vec::new(),
            &[provider("team", "KEY"), provider("Team", "KEY")],
            &env_of(&[("KEY", "k")]),
            &fetch,
        );
        assert_eq!(ids(&catalogue), vec!["team/coder", "Team"]);
        assert!(matches!(
            &entry(&catalogue, "Team").status,
            Status::Unusable(why) if why.contains("already named")
        ));
    }

    /// The contract as written on [`parse_agents`]: optional fields, ignored
    /// extras, and text from another service made safe for a terminal.
    #[test]
    fn the_agent_list_is_read_exactly_as_the_contract_states() {
        let listed = parse_agents(
            r#"{"agents":[{"id":" coder ","model":"m\u001b[31m-1","extra":true},{"id":"b","name":""}],"next":null}"#,
        );
        assert_eq!(
            listed,
            Ok(vec![
                ListedAgent {
                    id: "coder".to_string(),
                    name: None,
                    model: Some("m[31m-1".to_string()),
                },
                ListedAgent {
                    id: "b".to_string(),
                    name: None,
                    model: None,
                },
            ])
        );
        assert!(parse_agents(r#"{"agents":[{"name":"no id"}]}"#).is_err());
        assert!(parse_agents(r#"{"agents":[{"id":"has space"}]}"#).is_err());
        assert!(parse_agents(r#"{"other":[]}"#).is_err());
    }

    /// **Step 1: the operator's choice of sub-agent is used when usable.**
    #[test]
    fn the_operators_sub_agent_choice_is_used_when_it_is_usable() {
        let catalogue = Catalogue::assemble(
            vec![
                local("claude-code", Protocol::ClaudeStreamJson),
                local(CODEX, Protocol::CodexJson),
            ],
            &[],
            &env_of(&[]),
            &no_network,
        );
        let lead = local("claude-code", Protocol::ClaudeStreamJson);
        let sub = catalogue.sub_agent(&lead, Some(CODEX));
        assert_eq!(sub.agent.id, CODEX);
        assert_eq!(sub.origin, "your choice");
        assert_eq!(sub.note, None);
    }

    /// **Step 2: with no usable choice, the lead's entry in the built-in table
    /// decides.** A table that pairs two agents proves the lookup is what
    /// chose — the shipped table maps each agent to itself, which would pass
    /// with the lookup deleted.
    #[test]
    fn without_a_choice_the_built_in_table_names_the_sub_agent() {
        let catalogue = Catalogue::assemble(
            vec![
                local("claude-code", Protocol::ClaudeStreamJson),
                local(CODEX, Protocol::CodexJson),
            ],
            &[],
            &env_of(&[]),
            &no_network,
        );
        let table = [AgentProbe {
            id: "claude-code",
            display_name: "a local agent",
            commands: &["claude"],
            version_args: &[],
            protocol: Protocol::ClaudeStreamJson,
            default_sub_agent: CODEX,
        }];
        let lead = local("claude-code", Protocol::ClaudeStreamJson);
        let sub = catalogue.sub_agent_by(&lead, Some("gemini"), &table);
        assert_eq!(sub.agent.id, CODEX, "the table's pairing was not used");
        assert_eq!(sub.origin, "default for claude-code");
        assert!(
            sub.note
                .as_deref()
                .is_some_and(|n| n.contains("your choice is not used")),
            "a choice that did not apply must be said: {:?}",
            sub.note
        );
    }

    /// **Step 3: when the table's entry cannot be used, the lead itself — and
    /// `/agent` says so.** An API lead has no entry and gets itself too.
    #[test]
    fn when_the_default_is_not_usable_the_lead_itself_takes_the_work() {
        let catalogue = Catalogue::assemble(
            vec![local("claude-code", Protocol::ClaudeStreamJson)],
            &[],
            &env_of(&[
                (API_URL_ENV, "https://endpoint.invalid"),
                (API_KEY_ENV, "k"),
            ]),
            &no_network,
        );
        let table = [AgentProbe {
            id: "claude-code",
            display_name: "a local agent",
            commands: &["claude"],
            version_args: &[],
            protocol: Protocol::ClaudeStreamJson,
            default_sub_agent: ANTIGRAVITY,
        }];
        let lead = local("claude-code", Protocol::ClaudeStreamJson);
        let sub = catalogue.sub_agent_by(&lead, None, &table);
        assert_eq!(sub.agent.id, "claude-code");
        assert!(
            sub.note
                .as_deref()
                .is_some_and(|n| n.contains(ANTIGRAVITY) && n.contains("the lead itself")),
            "{:?}",
            sub.note
        );
        let report = catalogue.report(&lead, &sub, None);
        assert!(report.contains("the lead itself"), "{report}");

        let api_lead = DetectedAgent::over_api("");
        let api_sub = catalogue.sub_agent(&api_lead, None);
        assert_eq!(api_sub.agent.id, API_ROUTE);
        assert_eq!(api_sub.origin, format!("default for {API_ROUTE}"));
    }

    /// `/agent`'s listing: every entry with its state, the lead marked, and
    /// where the sub-agent came from.
    #[test]
    fn the_report_marks_the_lead_and_says_where_the_sub_agent_came_from() {
        let catalogue = Catalogue::assemble(
            vec![
                local("claude-code", Protocol::ClaudeStreamJson),
                local("cursor", Protocol::Unverified),
            ],
            &[],
            &env_of(&[]),
            &no_network,
        );
        let lead = local("claude-code", Protocol::ClaudeStreamJson);
        let sub = catalogue.sub_agent(&lead, None);
        let report = catalogue.report(&lead, &sub, None);
        assert!(report.contains("* claude-code"), "{report}");
        assert!(
            report.contains("not usable: its headless protocol"),
            "{report}"
        );
        assert!(
            report.contains("sub-agents   claude-code (default for claude-code)"),
            "{report}"
        );
        assert!(!report.contains("next lead"), "{report}");
    }

    /// **The real request, against a real socket**: the path, the bearer
    /// header, and the body handed back as it arrived.
    #[test]
    fn the_request_goes_to_the_agents_path_with_the_key_as_a_bearer() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).expect("read");
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            let body = r#"{"agents":[{"id":"coder"}]}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).expect("write");
            String::from_utf8_lossy(&request).to_string()
        });
        let body = fetch_agents(&format!("http://{addr}/"), "k-123");
        let request = server.join().expect("server");
        assert_eq!(body, Ok(r#"{"agents":[{"id":"coder"}]}"#.to_string()));
        assert!(request.starts_with("GET /v1/agents HTTP/1.1"), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer k-123"),
            "{request}"
        );
    }

    /// **A provider that never answers costs the timeout, not the session.**
    #[test]
    fn a_provider_that_never_answers_is_abandoned_within_the_timeout() {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Hold the connection open and say nothing until the client
                // gives up and closes it.
                let mut buf = [0u8; 1024];
                while matches!(stream.read(&mut buf), Ok(n) if n > 0) {}
            }
        });
        let started = std::time::Instant::now();
        let outcome = fetch_agents(&format!("http://{addr}"), "k");
        let took = started.elapsed();
        assert!(outcome.is_err(), "{outcome:?}");
        assert!(
            took < PROVIDER_TIMEOUT + Duration::from_secs(5),
            "took {took:?}, the bound is {PROVIDER_TIMEOUT:?}"
        );
    }
}
