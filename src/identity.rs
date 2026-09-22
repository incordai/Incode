//! Who did the work.
//!
//! # Why the agent is the identity, and the model is not
//!
//! **The agent chooses the model; the model can change underneath it.** That is
//! not a theoretical worry — it was measured: a turn asking for one model was
//! quietly served by its fallback, and the session reported the name that had
//! been *requested*. An identity built on the model would have credited that
//! work to something that never saw it, and would have split one worker's
//! history in two the first time a fallback fired.
//!
//! So the identity is the agent and the role it was acting in. Both are chosen
//! by the harness, both are stable across a run, and neither moves when a
//! provider is busy.
//!
//! # The model is still recorded — as an attribute
//!
//! What actually ran matters for cost, and for noticing that a fallback fired at
//! all. It is written beside the work, not into the name of the worker.
//!
//! # Why a fingerprint as well as a label
//!
//! The label reads well in a ledger. The fingerprint is short, fixed-width and
//! stable forever, which is what makes "everything this agent did" a query
//! rather than a string match against prose.

/// One worker, as the harness understands it.
///
/// Two fields, because those are the two things the harness actually chooses. A
/// third — the model — belongs to the turn, not to the worker.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AgentId {
    /// Which backend ran it, e.g. the id from agent discovery. The harness's own
    /// in-process loop uses [`IN_PROCESS`].
    pub backend: String,
    /// The role it was acting in. Empty when the work was not routed.
    pub role: String,
}

/// The backend name for work the harness ran itself, rather than delegating.
///
/// Named rather than left blank so a ledger entry never has to be read as
/// "nobody did this".
pub const IN_PROCESS: &str = "harness";

/// How many hex characters a fingerprint carries.
///
/// Eight is enough to separate the handful of workers one project will ever
/// have, and short enough to sit in a ledger line without pushing the reason for
/// the entry off the end.
const FINGERPRINT_LEN: usize = 8;

impl AgentId {
    #[must_use]
    pub fn new(backend: impl Into<String>, role: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            role: role.into(),
        }
    }

    /// The harness itself, in a named role.
    #[must_use]
    pub fn in_process(role: impl Into<String>) -> Self {
        Self::new(IN_PROCESS, role)
    }

    /// How it reads in a ledger or a report.
    ///
    /// `backend:role`, or just the backend when the work was not routed — an
    /// empty role rendered as a dangling colon looks like a bug.
    #[must_use]
    pub fn label(&self) -> String {
        if self.role.trim().is_empty() {
            self.backend.clone()
        } else {
            format!("{}:{}", self.backend, self.role)
        }
    }

    /// A short, stable identifier for this worker.
    ///
    /// Deterministic across runs, machines and releases: the same agent in the
    /// same role always fingerprints the same, which is the whole point — it is
    /// what turns "what has this worker done?" into a lookup.
    ///
    /// Hand-rolled FNV-1a rather than the standard library's hasher, which
    /// explicitly does not promise a stable value between versions. An id that
    /// changed on a toolchain upgrade would silently orphan every record made
    /// before it.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;

        let mut hash = OFFSET;
        // The label, so the fingerprint is a function of exactly what the label
        // shows — two ids that read the same must never fingerprint apart.
        for byte in self.label().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        format!("{hash:016x}")[..FINGERPRINT_LEN].to_string()
    }

    /// Both, for a record a person and a machine will both read.
    #[must_use]
    pub fn stamp(&self) -> String {
        format!("{} ({})", self.label(), self.fingerprint())
    }
}

/// What one worker did on one attempt.
///
/// The model is here rather than in [`AgentId`] because it is a property of the
/// attempt: the same agent may be served by a different model next time without
/// becoming a different worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    pub agent: AgentId,
    /// The model that actually BILLED for the work, when it could be
    /// determined. Empty when the backend does not report it — and empty is
    /// honest, where guessing the requested model would not be.
    pub served_by: Vec<String>,
    /// WHICH agent this was, as opposed to which KIND of agent.
    ///
    /// # Why a second identifier
    ///
    /// [`AgentId::fingerprint`] is deliberately stable — the same backend in the
    /// same role always fingerprints the same, which is what makes "what has
    /// this worker done?" a lookup. The cost is that it cannot tell two agents
    /// apart: every `claude:implement` that ever ran carries one fingerprint, so
    /// a record saying that worker did something names a class, not a doer.
    ///
    /// This is the other half. Unique per spawned agent, so a memory entry says
    /// which agent wrote it and a transcript can be tied to the record it
    /// produced. Empty for work that was never delegated — the harness's own
    /// in-process loop is one actor, and inventing an id for it would suggest
    /// otherwise.
    pub instance: String,
}

impl Attribution {
    #[must_use]
    pub fn new(agent: AgentId) -> Self {
        Self {
            agent,
            served_by: Vec::new(),
            instance: String::new(),
        }
    }

    /// Tag this attribution with the agent that actually did the work.
    #[must_use]
    pub fn by_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = instance.into();
        self
    }

    /// A fresh, unique agent identifier.
    ///
    /// One per spawned agent, not one per task: an agent that survives several
    /// tasks is still the same doer, and a new id per task would make one actor
    /// look like many in the record.
    #[must_use]
    pub fn new_instance_id() -> String {
        format!("agent-{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
    }

    #[must_use]
    pub fn served_by(mut self, models: Vec<String>) -> Self {
        self.served_by = models;
        self
    }

    /// One line for the ledger: who, and what they actually ran on.
    ///
    /// When the served model differs from nothing recorded, the line says so
    /// plainly — a fallback that fired and went unmentioned is exactly the
    /// silence this record exists to break.
    #[must_use]
    pub fn line(&self) -> String {
        let who = if self.instance.is_empty() {
            self.agent.stamp()
        } else {
            // The instance sits beside the stable stamp rather than replacing
            // it: a reader still needs to group by worker, and a line carrying
            // only a unique id would make every entry look unrelated.
            format!("{} [{}]", self.agent.stamp(), self.instance)
        };
        if self.served_by.is_empty() {
            format!("{who} — model not reported")
        } else {
            format!("{who} on {}", self.served_by.join(", "))
        }
    }

    /// Extract the instance id (`agent-...`) from an attribution line, if one was recorded.
    #[must_use]
    pub fn extract_instance(line: &str) -> Option<&str> {
        let start = line.find('[')? + 1;
        let end = line[start..].find(']')? + start;
        let candidate = line[start..end].trim();
        if candidate.is_empty() {
            None
        } else {
            Some(candidate)
        }
    }

    /// Extract the 8-hex character worker fingerprint from an attribution line.
    #[must_use]
    pub fn extract_fingerprint(line: &str) -> Option<&str> {
        let start = line.find('(')? + 1;
        let end = line[start..].find(')')? + start;
        let candidate = line[start..end].trim();
        if candidate.len() == FINGERPRINT_LEN && candidate.chars().all(|c| c.is_ascii_hexdigit()) {
            Some(candidate)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// **The identity survives a model change.** This is the whole design: a
    /// fallback fired mid-run must not turn one worker into two in the record.
    #[test]
    fn the_same_agent_keeps_one_identity_across_models() {
        let agent = AgentId::new("some-backend", "grunt");
        let cheap = Attribution::new(agent.clone()).served_by(vec!["model-a".to_string()]);
        let after_fallback = Attribution::new(agent.clone()).served_by(vec!["model-b".to_string()]);

        assert_eq!(
            cheap.agent.fingerprint(),
            after_fallback.agent.fingerprint(),
            "a fallback must not split one worker's history in two"
        );
        // And the difference is still visible where it belongs.
        assert!(cheap.line().contains("model-a"));
        assert!(after_fallback.line().contains("model-b"));
    }

    /// Different workers are different identities — the same agent in two roles
    /// is two workers, because that is the unit the operator routes.
    #[test]
    fn agent_and_role_together_make_the_worker() {
        let grunt = AgentId::new("some-backend", "grunt");
        let reviewer = AgentId::new("some-backend", "review");
        let other = AgentId::new("other-backend", "grunt");

        assert_ne!(grunt.fingerprint(), reviewer.fingerprint());
        assert_ne!(grunt.fingerprint(), other.fingerprint());
    }

    /// **Stable forever, by construction.** A fingerprint that changed on a
    /// toolchain upgrade would orphan every record written before it, so the
    /// value is pinned here rather than merely asserted to be consistent within
    /// one run.
    #[test]
    fn a_fingerprint_is_the_same_value_on_every_machine_and_release() {
        // Computed independently with a reference FNV-1a, not copied from this
        // implementation's own output — a value read back from the code under
        // test proves only that it is self-consistent.
        assert_eq!(AgentId::new("harness", "grunt").fingerprint(), "c913daa3");
        assert_eq!(AgentId::new("harness", "").fingerprint(), "211fcbba");
    }

    /// Read the same, fingerprint the same: the hash is a function of the label,
    /// so two ids a person cannot tell apart are not secretly different.
    #[test]
    fn ids_that_read_alike_fingerprint_alike() {
        let a = AgentId::new("backend", "role");
        let b = AgentId::new("backend", "role");
        assert_eq!(a.label(), b.label());
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    /// Unrouted work still has an author. A dangling colon reads as a bug, and
    /// "nobody did this" is never the truth.
    #[test]
    fn unrouted_work_still_names_its_author() {
        let plain = AgentId::in_process("");
        assert_eq!(plain.label(), "harness");
        assert!(!plain.label().ends_with(':'));
        assert!(plain.stamp().contains("harness"));
    }

    /// **An unknown model is recorded as unknown.** Filling it in from the
    /// REQUESTED model would be the lie this module exists to prevent.
    #[test]
    fn a_backend_that_cannot_report_its_model_says_so() {
        let line = Attribution::new(AgentId::new("some-backend", "plan")).line();
        assert!(line.contains("model not reported"), "got: {line}");
        assert!(
            !line.contains(" on "),
            "it must not imply a model it does not know: {line}"
        );
    }

    /// The stamp carries both halves: the label a person reads and the
    /// fingerprint a query matches on.
    #[test]
    fn the_stamp_is_readable_and_matchable() {
        let id = AgentId::new("some-backend", "review");
        let stamp = id.stamp();
        assert!(stamp.contains("some-backend:review"));
        assert!(stamp.contains(&id.fingerprint()));
    }

    #[test]
    fn attribution_line_extracts_fingerprint_and_instance() {
        let agent = AgentId::new("some-backend", "grunt");
        let attr = Attribution::new(agent.clone())
            .by_instance("agent-999abc123456")
            .served_by(vec!["cheap-fast-model".to_string()]);
        let line = attr.line();
        assert_eq!(
            Attribution::extract_instance(&line),
            Some("agent-999abc123456")
        );
        assert_eq!(
            Attribution::extract_fingerprint(&line),
            Some(agent.fingerprint().as_str())
        );

        let uninstanced = Attribution::new(agent.clone()).line();
        assert_eq!(Attribution::extract_instance(&uninstanced), None);
        assert_eq!(
            Attribution::extract_fingerprint(&uninstanced),
            Some(agent.fingerprint().as_str())
        );

        assert_eq!(Attribution::extract_instance("not an attribution"), None);
        assert_eq!(Attribution::extract_fingerprint("not an attribution"), None);
    }
}
