//! More than one agent, and which one gets the work.
//!
//! # The idea
//!
//! An operator may have more than one agent CLI installed and logged in, each on
//! its own subscription. They are not interchangeable and they do not cost the
//! same, so the choice is worth making per ROLE rather than once per session:
//! reading and renaming can go to whichever is cheapest, while a design decision
//! goes to whichever is best at judgement.
//!
//! # What the operator writes
//!
//! A role's candidates may name the agent that should serve them:
//!
//! ```text
//! INCODE_ROLE_GRUNT=other-agent:fast,some-agent:small
//! INCODE_ROLE_PLAN=some-agent:large
//! INCODE_ROLE_REVIEW=large              # no agent named: whichever is running
//! ```
//!
//! # The rule that makes it safe to configure a fleet you do not have
//!
//! **A candidate naming an agent that is not installed is SKIPPED, not an
//! error.** So one table works on every machine: with both agents installed the
//! work is split between them; with one installed every candidate naming the
//! other falls through and the work runs on what is there; with none, the
//! session refuses to start at all — which happens before this code is reached.
//!
//! That is the whole reason routing is expressed as an ordered list. It is not a
//! preference, it is a fallback chain, and an absent agent is one more reason a
//! link in it does not apply.

use crate::agent::registry::DetectedAgent;

/// What separates an agent from a model in a candidate.
///
/// A colon, because `agent:model` is how everything else on a command line
/// writes a qualified name. A candidate with no colon names only a model.
const QUALIFIER: char = ':';

/// One routing candidate: which agent, and what it should run on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The agent id this must run on, or `None` for "whichever is running".
    pub agent: Option<String>,
    /// The model to ask that agent for. Empty means the agent's own default.
    pub model: String,
}

impl Candidate {
    /// Read one candidate as the operator wrote it.
    ///
    /// `agent:model` binds it to an agent; a bare `model` leaves it free. An
    /// empty agent or model around the colon is treated as absent rather than
    /// as an empty name, so `:model` and `agent:` both still mean something
    /// usable instead of failing on a typo.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim();
        match raw.split_once(QUALIFIER) {
            Some((agent, model)) => {
                let agent = agent.trim();
                Self {
                    agent: (!agent.is_empty()).then(|| agent.to_string()),
                    model: model.trim().to_string(),
                }
            }
            None => Self {
                agent: None,
                model: raw.to_string(),
            },
        }
    }

    /// Whether this candidate can run, given what is installed.
    ///
    /// An unqualified candidate can always run: it names no agent, so whichever
    /// agent is in hand serves it.
    #[must_use]
    pub fn is_available(&self, installed: &[DetectedAgent]) -> bool {
        match &self.agent {
            None => true,
            Some(id) => installed
                .iter()
                .any(|a| a.is_drivable() && a.id.eq_ignore_ascii_case(id)),
        }
    }

    /// How it reads back to a person.
    #[must_use]
    pub fn label(&self) -> String {
        match (&self.agent, self.model.is_empty()) {
            (Some(agent), false) => format!("{agent}:{}", self.model),
            (Some(agent), true) => agent.clone(),
            (None, false) => self.model.clone(),
            (None, true) => "default".to_string(),
        }
    }
}

/// The agents available to this session, and the routing decision.
#[derive(Debug, Clone)]
pub struct Fleet {
    installed: Vec<DetectedAgent>,
}

impl Fleet {
    /// The drivable agents, in discovery order.
    #[must_use]
    pub fn new(installed: Vec<DetectedAgent>) -> Self {
        Self {
            installed: installed
                .into_iter()
                .filter(DetectedAgent::is_drivable)
                .collect(),
        }
    }

    /// Every agent that can be driven.
    #[must_use]
    pub fn agents(&self) -> &[DetectedAgent] {
        &self.installed
    }

    /// Whether more than one agent is available to split work between.
    #[must_use]
    pub fn is_mixed(&self) -> bool {
        self.installed.len() > 1
    }

    /// Who should serve this role, and on what.
    ///
    /// Walks the operator's ordered candidates and returns the first one whose
    /// agent is actually installed. `None` means nothing in the list applies —
    /// the caller then runs the work on the session's own agent and default
    /// model, which is the behaviour before a fleet was ever configured.
    #[must_use]
    pub fn route<'a>(&'a self, candidates: &[String]) -> Option<(&'a DetectedAgent, Candidate)> {
        for raw in candidates {
            let candidate = Candidate::parse(raw);
            let Some(agent) = self.agent_for(&candidate) else {
                // Named an agent that is not here. Not an error: the next
                // candidate is exactly what an ordered list is for.
                continue;
            };
            return Some((agent, candidate));
        }
        None
    }

    /// The models a given agent should be asked for, in order, for one role.
    ///
    /// Once an agent is chosen, the remaining candidates bound to that SAME
    /// agent are its fallback chain — a busy model falls through to the next on
    /// the same subscription rather than jumping to another agent mid-task,
    /// which would abandon the session and re-pay the prompt.
    #[must_use]
    pub fn models_for(&self, agent: &DetectedAgent, candidates: &[String]) -> Vec<String> {
        candidates
            .iter()
            .map(|raw| Candidate::parse(raw))
            .filter(|c| match &c.agent {
                Some(id) => id.eq_ignore_ascii_case(&agent.id),
                None => true,
            })
            .filter(|c| !c.model.is_empty())
            .map(|c| c.model)
            .collect()
    }

    /// The installed agent a candidate binds to.
    fn agent_for(&self, candidate: &Candidate) -> Option<&DetectedAgent> {
        match &candidate.agent {
            // Unqualified: whichever agent leads the fleet.
            None => self.installed.first(),
            Some(id) => self
                .installed
                .iter()
                .find(|a| a.id.eq_ignore_ascii_case(id)),
        }
    }

    /// One line naming what is available, for the operator to see at startup.
    ///
    /// Said out loud for the same reason the ceiling and the role table are: a
    /// fleet the operator cannot see is a fleet they cannot correct.
    #[must_use]
    pub fn summary_line(&self) -> String {
        match self.installed.len() {
            0 => "No agent available.".to_string(),
            1 => format!("Agent: {}.", self.installed[0].display_name),
            _ => format!(
                "Agents: {} — roles route between them.",
                self.installed
                    .iter()
                    .map(|a| a.display_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::registry::Protocol;

    fn agent(id: &str, protocol: Protocol) -> DetectedAgent {
        DetectedAgent {
            id: id.to_string(),
            display_name: format!("{id} CLI"),
            command: id.to_string(),
            path: None,
            version: None,
            protocol,
        }
    }

    fn both() -> Fleet {
        Fleet::new(vec![
            agent("first-agent", Protocol::ClaudeStreamJson),
            agent("second-agent", Protocol::ClaudeStreamJson),
        ])
    }

    fn only_first() -> Fleet {
        Fleet::new(vec![agent("first-agent", Protocol::ClaudeStreamJson)])
    }

    /// **With both installed, the work goes where the operator sent it.**
    #[test]
    fn a_role_routes_to_the_agent_it_names() {
        let fleet = both();
        let (agent, candidate) = fleet
            .route(&["second-agent:fast".to_string()])
            .expect("routed");
        assert_eq!(agent.id, "second-agent");
        assert_eq!(candidate.model, "fast");
    }

    /// **With only one installed, a candidate naming the other is SKIPPED and
    /// the next one runs.** This is what lets one table work on every machine:
    /// the operator does not maintain a per-machine config.
    #[test]
    fn a_candidate_naming_an_absent_agent_falls_through_to_the_next() {
        let fleet = only_first();
        let (agent, candidate) = fleet
            .route(&[
                "second-agent:fast".to_string(),
                "first-agent:small".to_string(),
            ])
            .expect("the second candidate applies");
        assert_eq!(agent.id, "first-agent");
        assert_eq!(candidate.model, "small");
    }

    /// **A list naming only absent agents routes nowhere**, rather than
    /// silently running on something the operator did not choose. The caller
    /// then uses the session default, which it can say out loud.
    #[test]
    fn a_list_of_absent_agents_routes_nowhere() {
        let fleet = only_first();
        assert!(fleet.route(&["second-agent:fast".to_string()]).is_none());
    }

    /// An unqualified candidate names no agent, so it runs on whichever is
    /// leading — the behaviour every existing role table already has.
    #[test]
    fn an_unqualified_candidate_runs_on_whatever_is_there() {
        let fleet = only_first();
        let (agent, candidate) = fleet.route(&["small".to_string()]).expect("routed");
        assert_eq!(agent.id, "first-agent");
        assert_eq!(candidate.model, "small");
        assert_eq!(candidate.agent, None);
    }

    /// **Fallbacks stay on the chosen agent.** Falling through to a different
    /// agent mid-task would abandon its session and re-pay the whole prompt, so
    /// the chain handed to a backend is only its own models.
    #[test]
    fn the_fallback_chain_does_not_jump_between_agents() {
        let fleet = both();
        let candidates = vec![
            "first-agent:large".to_string(),
            "second-agent:fast".to_string(),
            "first-agent:small".to_string(),
        ];
        let (agent, _) = fleet.route(&candidates).expect("routed");
        assert_eq!(agent.id, "first-agent");
        assert_eq!(
            fleet.models_for(agent, &candidates),
            vec!["large".to_string(), "small".to_string()],
            "the other agent's model must not be in this agent's fallback chain"
        );
    }

    /// An unverified agent is not part of the fleet: it was detected, and it
    /// still cannot be driven.
    #[test]
    fn an_undrivable_agent_is_not_in_the_fleet() {
        let fleet = Fleet::new(vec![
            agent("first-agent", Protocol::ClaudeStreamJson),
            agent("unsupported", Protocol::Unverified),
        ]);
        assert_eq!(fleet.agents().len(), 1);
        assert!(!fleet.is_mixed());
        assert!(fleet.route(&["unsupported:any".to_string()]).is_none());
    }

    /// Two agents is a mix; one is not. The operator is told which.
    #[test]
    fn the_summary_says_whether_work_is_being_split() {
        assert!(both().summary_line().contains("route between them"));
        assert!(!only_first().summary_line().contains("route between them"));
    }

    /// A typo around the colon still yields something usable rather than an
    /// empty agent name that matches nothing.
    #[test]
    fn a_malformed_candidate_is_read_as_charitably_as_it_can_be() {
        assert_eq!(Candidate::parse(":model").agent, None);
        assert_eq!(Candidate::parse(":model").model, "model");
        assert_eq!(
            Candidate::parse("some-agent:").agent.as_deref(),
            Some("some-agent")
        );
        assert!(Candidate::parse("some-agent:").model.is_empty());
    }

    /// Agent ids are matched without case, because an operator typing a config
    /// file is not checking capitalisation.
    #[test]
    fn agent_ids_match_regardless_of_case() {
        let fleet = only_first();
        assert!(fleet.route(&["FIRST-AGENT:small".to_string()]).is_some());
    }
}
