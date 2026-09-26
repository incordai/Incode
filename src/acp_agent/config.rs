//! The session's pickers: which agent serves the turns, and which role's
//! model the next turn asks for — `configOptions` in `session/new` and
//! `session/load`, changed with `session/set_config_option`, and re-sent as
//! `config_option_update` whenever they change for any other reason (a
//! standby that took over, a model the agent settled on).
//!
//! # Where the truth is
//!
//! The client's choice is kept here, on the reader, so `set_config_option`
//! is answered at once — like `set_mode`, it applies from the next turn. The
//! session's thread applies it before that turn (or at once when it is
//! idle) and reports the model the turn will actually ask for.

use agent_client_protocol_schema::v1 as acp;

use super::transport::Outbox;
use super::{Held, Shared};

/// The agent select's id.
pub(super) const AGENT: &str = "agent";
/// The role select's id.
pub(super) const ROLE: &str = "role";

/// The role a session opens on: the one `prepare_session` routes the
/// session's model by.
pub(super) const OPENING_ROLE: &str = "plan";

/// One agent a client may pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Pick {
    pub(super) id: String,
    pub(super) name: String,
    /// What the agent's backend calls itself may be its command (`agy`)
    /// rather than its catalogue id.
    pub(super) command: String,
}

/// What the pickers offer, and what is chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Choices {
    /// The usable agents, in catalogue order.
    pub(super) agents: Vec<Pick>,
    /// Each known role, with the operator's models for it.
    pub(super) roles: Vec<(String, Vec<String>)>,
    /// The agent chosen — for the next turn, until it is applied.
    pub(super) agent: String,
    /// The role chosen.
    pub(super) role: String,
    /// The model the next turn asks for, as the session last reported it.
    pub(super) model: Option<String>,
}

impl Choices {
    /// The agent whose backend calls itself `backend_id`.
    pub(super) fn agent_for_backend(&self, backend_id: &str) -> Option<&Pick> {
        self.agents
            .iter()
            .find(|pick| pick.id == backend_id || pick.command == backend_id)
    }

    /// Record a client's choice. `Err` names what is offered.
    pub(super) fn choose(&mut self, config_id: &str, value: &str) -> Result<(), String> {
        match config_id {
            AGENT => {
                if !self.agents.iter().any(|pick| pick.id == value) {
                    return Err(format!(
                        "`{value}` is not an agent this session can use: {}",
                        self.agents
                            .iter()
                            .map(|pick| pick.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                self.agent = value.to_string();
                Ok(())
            }
            ROLE => {
                if !self.roles.iter().any(|(role, _)| role == value) {
                    return Err(format!(
                        "`{value}` is not a role: {}",
                        crate::roles::KNOWN_ROLES.join(", ")
                    ));
                }
                self.role = value.to_string();
                Ok(())
            }
            other => Err(format!(
                "`{other}` is not an option this agent offers: {AGENT}, {ROLE}"
            )),
        }
    }

    /// The options, in the protocol's shape.
    pub(super) fn options(&self) -> Vec<acp::SessionConfigOption> {
        let agents: Vec<acp::SessionConfigSelectOption> = self
            .agents
            .iter()
            .map(|pick| {
                acp::SessionConfigSelectOption::new(pick.id.clone(), pick.name.clone())
                    .description(format!("`{}` in incode's agent catalogue", pick.id))
            })
            .collect();
        let roles: Vec<acp::SessionConfigSelectOption> = self
            .roles
            .iter()
            .map(|(role, models)| {
                let said = if models.is_empty() {
                    "No model assigned: the agent's own model for this kind of work".to_string()
                } else {
                    format!("Assigned: {}", models.join(" > "))
                };
                acp::SessionConfigSelectOption::new(role.clone(), capitalised(role))
                    .description(said)
            })
            .collect();
        let model = self.model.as_deref().map_or_else(
            || "the agent's own default".to_string(),
            |m| format!("`{m}`"),
        );
        vec![
            acp::SessionConfigOption::select(AGENT, "Agent", self.agent.clone(), agents)
                .description(
                    "The agent that serves this session's turns, from the next turn. Another \
                     agent starts a fresh conversation there; incode's tools, guards and memory \
                     stay the same.",
                ),
            acp::SessionConfigOption::select(ROLE, "Role", self.role.clone(), roles)
                .description(format!(
                    "Which kind of work the next turn is, which decides its model. The next \
                     turn asks for {model}."
                ))
                .category(acp::SessionConfigOptionCategory::Model),
        ]
    }
}

/// The pickers a session opens with: the catalogue's usable agents, the lead
/// chosen, and the role its model was routed by.
pub(super) fn opening(
    settings: &super::commands::Settings,
    roles: &crate::roles::RoleTable,
    model: Option<&str>,
) -> Choices {
    let agents = settings
        .catalogue
        .usable_chain(None)
        .unwrap_or_default()
        .into_iter()
        .map(|agent| Pick {
            id: agent.id,
            name: agent.display_name,
            command: agent.command,
        })
        .collect();
    Choices {
        agents,
        roles: crate::roles::KNOWN_ROLES
            .iter()
            .map(|role| ((*role).to_string(), roles.candidates_for(role).to_vec()))
            .collect(),
        agent: settings.lead.id.clone(),
        role: OPENING_ROLE.to_string(),
        model: model.map(str::to_string),
    }
}

/// Apply what the client picked, on the session's thread, before a turn — or
/// at once when it is idle. A new agent is built the way the session's own
/// was and switched in; the role then routes its model.
pub(super) fn apply(
    held: &mut Held,
    shared: &Shared,
    outbox: &Outbox,
    session_id: &acp::SessionId,
) {
    let Some(wanted) = shared.choices().clone() else {
        return;
    };
    let mut switched = false;
    if wanted.agent != held.lead {
        let agent = held
            .settings
            .catalogue
            .usable_chain(None)
            .unwrap_or_default()
            .into_iter()
            .find(|agent| agent.id == wanted.agent);
        match agent {
            Some(agent) => {
                let backend = (held.build)(&agent);
                held.session.switch_backend(backend);
                crate::trace!("acp", "agent switched to {} for the next turn", agent.id);
                held.lead = agent.id.clone();
                held.settings.lead = agent;
                switched = true;
            }
            // No longer usable since it was offered: the picker goes back to
            // the agent that is really serving.
            None => {
                if let Some(choices) = shared.choices().as_mut() {
                    choices.agent.clone_from(&held.lead);
                }
            }
        }
    }
    if switched || wanted.role != held.role {
        let model = held.session.use_role(&held.roles, &wanted.role);
        crate::trace!("acp", "role {} -> {model:?}", wanted.role);
        held.role = wanted.role;
    }
    report(held, shared, outbox, session_id);
}

/// Tell the client the pickers again when they changed for a reason other
/// than its own request: the agent that serves the session is now another
/// (a standby took over), or the model the next turn asks for moved.
pub(super) fn report(
    held: &mut Held,
    shared: &Shared,
    outbox: &Outbox,
    session_id: &acp::SessionId,
) {
    let options = {
        let mut guard = shared.choices();
        let Some(choices) = guard.as_mut() else {
            return;
        };
        let serving = choices
            .agent_for_backend(held.session.backend_id())
            .map(|pick| pick.id.clone());
        if let Some(serving) = serving {
            if serving != held.lead {
                held.lead.clone_from(&serving);
                choices.agent = serving;
            }
        }
        choices.model = held.session.model().map(str::to_string);
        choices.options()
    };
    if options != held.shown {
        outbox.notify(
            acp::CLIENT_METHOD_NAMES.session_update,
            acp::SessionNotification::new(
                session_id.clone(),
                acp::SessionUpdate::ConfigOptionUpdate(acp::ConfigOptionUpdate::new(
                    options.clone(),
                )),
            ),
        );
        held.shown = options;
    }
}

fn capitalised(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choices() -> Choices {
        Choices {
            agents: vec![
                Pick {
                    id: "claude-code".to_string(),
                    name: "Claude Code".to_string(),
                    command: "claude".to_string(),
                },
                Pick {
                    id: "antigravity".to_string(),
                    name: "Antigravity".to_string(),
                    command: "agy".to_string(),
                },
            ],
            roles: crate::roles::KNOWN_ROLES
                .iter()
                .map(|r| ((*r).to_string(), Vec::new()))
                .collect(),
            agent: "claude-code".to_string(),
            role: OPENING_ROLE.to_string(),
            model: None,
        }
    }

    /// The shape the owner's app reads: a flat option list per select,
    /// `currentValue` a string.
    #[test]
    fn the_options_are_flat_selects_the_app_reads() {
        let encoded = serde_json::to_value(choices().options()).expect("encodes");
        assert_eq!(encoded[0]["id"], "agent", "{encoded}");
        assert_eq!(encoded[0]["type"], "select", "{encoded}");
        assert_eq!(encoded[0]["currentValue"], "claude-code", "{encoded}");
        assert_eq!(
            encoded[0]["options"][1]["value"], "antigravity",
            "{encoded}"
        );
        assert_eq!(encoded[1]["id"], "role", "{encoded}");
        assert_eq!(encoded[1]["currentValue"], "plan", "{encoded}");
        assert_eq!(
            encoded[1]["options"].as_array().map(Vec::len),
            Some(crate::roles::KNOWN_ROLES.len()),
            "{encoded}"
        );
    }

    #[test]
    fn only_what_is_offered_can_be_chosen() {
        let mut picks = choices();
        assert!(picks.choose(AGENT, "antigravity").is_ok());
        assert!(picks.choose(AGENT, "not-installed").is_err());
        assert!(picks.choose(ROLE, "explore").is_ok());
        assert!(picks.choose(ROLE, "wizard").is_err());
        assert!(picks.choose("model", "x").is_err());
        assert_eq!(picks.agent, "antigravity");
        assert_eq!(picks.role, "explore");
        assert_eq!(
            picks.agent_for_backend("agy").map(|p| p.id.as_str()),
            Some("antigravity")
        );
    }
}
