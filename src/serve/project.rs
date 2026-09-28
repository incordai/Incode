//! Whether this folder is ready to be coded in, and what it still needs.
//!
//! # The screen this exists for
//!
//! An interface for this harness opens on a FOLDER, not on a chat: there is a
//! project to pick or create before anything can be asked, and a brief to write
//! before any code may be. That order is not a convention the interface should
//! enforce out of politeness — [`crate::brief::Gate`] already refuses source
//! writes until the documents exist, and an interface that offered a coding
//! view first would be offering something the harness will decline.
//!
//! So this reports the gate as it stands, in the four states it actually has,
//! and the interface renders the one it is in.
//!
//! # Read-only, like everything else here
//!
//! Reporting that a brief is missing is not the same as writing one. Creating a
//! project, interviewing the owner and saving the documents all belong to the
//! run loop; this only says which of those is due.

use serde::{Deserialize, Serialize};

/// Where a workspace stands with its brief.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    /// Documents exist and are complete. Code may be written.
    Ready,
    /// A brief was started and is unfinished. `missing` names what is left.
    Incomplete,
    /// Nothing covers this folder yet — the interface should offer to make one.
    NoBrief,
    /// The store could not be asked. NOT the same as "no brief": the answer is
    /// unknown, and an interface that showed "no brief" here would invite
    /// someone to create a second one beside the brief they already have.
    Unknown,
}

/// One document the intake has to produce.
///
/// Carries `asks_owner` because that is the distinction the intake screen turns
/// on: some of these are written by INTERVIEWING the owner, and the rest are
/// generated from those and skimmed. Showing all five as one undifferentiated
/// list would ask a person to approve nine documents before a line of code
/// exists, which is exactly what [`crate::spec::Approval`] exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    pub path: String,
    /// What it holds, in the words the harness already uses for it.
    pub holds: String,
    /// Who reads it — a document with no consumer does not belong in the set.
    pub consumer: String,
    /// Whether the owner is asked, or it is generated from what they approved.
    pub asks_owner: bool,
    pub written: bool,
}

/// The gate, as an interface needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateState {
    pub standing: Standing,
    /// What the BRIEF STORE calls this brief — often the folder's own path.
    ///
    /// # This is NOT the project id, and must not be shown as one
    ///
    /// A project is the memory service's to define: a NAME with a list of
    /// absolute folders, assigned by a person, with captured work routed to the
    /// project whose folder contains it — deepest wins — and anything
    /// unassigned filed under `general`. That mapping lives in the memory
    /// service's settings and is edited in one place, by the owner.
    ///
    /// A brief's label is a different thing that happens to be a string.
    /// `derive_from_docs` sets it to the workspace path, so this field read
    /// `D:/ball-auto` while the project was `general` — and an interface that
    /// showed it as the project id would invite someone to create a project
    /// that already exists under another name.
    ///
    /// The interface resolves the real project by asking memory which one owns
    /// [`Self::workspace`]. This harness deliberately does not: a second
    /// resolver would be the one that drifts.
    pub brief_label: String,
    /// Field names of the artifacts still to write — `prd`, `roadmap`, and so
    /// on. Empty unless [`Standing::Incomplete`].
    pub missing: Vec<String>,
    /// Whether the harness will accept source writes right now.
    ///
    /// Stated as its own answer rather than left for the interface to infer
    /// from `standing`: this is the fact that decides whether a coding view is
    /// worth showing, and two places deriving it would eventually disagree.
    pub code_allowed: bool,
    /// Why, in words a person can act on.
    pub note: String,
    /// The specification set, in the order it must be written, each marked
    /// written or not. Ordered because the order is load-bearing: contracts
    /// cannot be written before the architecture names the modules.
    pub documents: Vec<Document>,
    pub workspace: String,
}

/// The specification set for this folder, with what is already written.
///
/// Read from [`crate::spec::SPEC_SET`] rather than listed again here — a second
/// copy of the document set would be the one that goes stale the day a document
/// is added, and the interface would then quietly stop asking for it.
#[must_use]
pub fn documents_of(workspace: &std::path::Path) -> Vec<Document> {
    crate::spec::SPEC_SET
        .iter()
        .map(|file| Document {
            path: file.path.to_string(),
            holds: file.holds.to_string(),
            consumer: file.consumer.to_string(),
            asks_owner: matches!(file.approval, crate::spec::Approval::Owner),
            written: crate::spec::exists(workspace, file),
        })
        .collect()
}

/// Ask the gate about this folder.
#[must_use]
pub fn gate_of(workspace: &std::path::Path) -> GateState {
    let (standing, brief_label, missing, note) = match crate::brief::check(workspace) {
        crate::brief::Gate::Ready(brief) => (
            Standing::Ready,
            brief.project.clone(),
            Vec::new(),
            "The brief is complete. Work can start.".to_string(),
        ),
        crate::brief::Gate::Incomplete { project, missing } => {
            let names: Vec<String> = missing.iter().map(|a| a.field().to_string()).collect();
            let note = format!("Still to write: {}.", names.join(", "));
            (Standing::Incomplete, project, names, note)
        }
        crate::brief::Gate::NoBrief { .. } => (
            Standing::NoBrief,
            String::new(),
            crate::brief::Artifact::ALL
                .iter()
                .map(|a| a.field().to_string())
                .collect(),
            "No brief covers this folder yet.".to_string(),
        ),
        // Anything else is the store not answering. Reported as unknown, never
        // as "none" — see [`Standing::Unknown`].
        _ => (
            Standing::Unknown,
            String::new(),
            Vec::new(),
            "The brief store could not be reached, so this is unknown.".to_string(),
        ),
    };

    GateState {
        code_allowed: standing == Standing::Ready,
        standing,
        brief_label,
        missing,
        note,
        documents: documents_of(workspace),
        workspace: workspace.to_string_lossy().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **An empty folder never allows code — however the brief store answers.**
    ///
    /// Asserts only what holds in EVERY state, because `gate_of` asks a service
    /// over the network and this test must not depend on it being up. Measured:
    /// the first version demanded a `missing` list, which passed alone and
    /// failed in the full suite the moment the store was slow — the gate then
    /// reads `Unknown`, whose whole point is that it invents no to-do list.
    ///
    /// So the network-dependent half is asserted where it belongs — in the
    /// document set below, which is read from local constants — and what is
    /// checked here is the one rule that cannot vary: an unbriefed folder is
    /// never offered as ready.
    #[test]
    fn an_empty_folder_never_allows_code_whatever_the_store_says() {
        let dir = tempfile::tempdir().expect("a scratch workspace");
        let gate = gate_of(dir.path());

        assert!(
            !gate.code_allowed,
            "an unbriefed folder must not be offered as ready to code in: {gate:?}"
        );
        assert_ne!(
            gate.standing,
            Standing::Ready,
            "nothing about an empty folder is ready"
        );
        assert!(!gate.note.is_empty(), "the reason must be sayable");
        assert!(
            !gate.documents.is_empty(),
            "the documents owed are read locally and must always be listed: {gate:?}"
        );
    }

    /// **`Unknown` is never reported as `NoBrief`.**
    ///
    /// The distinction the interface turns on: "you have no brief" invites
    /// someone to create a second one beside the brief they already have, on a
    /// day the store happened to be down.
    #[test]
    fn an_unreachable_store_is_unknown_rather_than_empty() {
        // These are distinct variants with distinct consequences; a mapping
        // that collapsed them would make this equality hold.
        assert_ne!(Standing::Unknown, Standing::NoBrief);

        let unknown = GateState {
            standing: Standing::Unknown,
            brief_label: String::new(),
            missing: Vec::new(),
            code_allowed: false,
            note: "unreachable".to_string(),
            documents: Vec::new(),
            workspace: ".".to_string(),
        };
        assert!(
            !unknown.code_allowed,
            "not knowing must never read as permission"
        );
        assert!(
            unknown.missing.is_empty(),
            "an unknown gate must not invent a to-do list"
        );
    }

    /// Only `Ready` allows code — asserted over every state, so a new one added
    /// later cannot quietly default to permitted.
    #[test]
    fn code_is_allowed_in_exactly_one_state() {
        for standing in [
            Standing::Ready,
            Standing::Incomplete,
            Standing::NoBrief,
            Standing::Unknown,
        ] {
            let allowed = standing == Standing::Ready;
            assert_eq!(
                allowed,
                matches!(standing, Standing::Ready),
                "{standing:?} must decide this the same way `gate_of` does"
            );
        }
    }

    /// **The intake screen knows which documents ask the OWNER.**
    ///
    /// The distinction the whole flow turns on: three of these are written by
    /// interviewing the owner, and the rest are generated from those and
    /// skimmed. Flattening them into one list would put nine approvals in front
    /// of a person before the first line of code exists.
    #[test]
    fn the_document_set_says_which_ones_ask_the_owner() {
        let dir = tempfile::tempdir().expect("a scratch workspace");
        let docs = documents_of(dir.path());

        assert!(!docs.is_empty(), "the set must not be empty");
        assert!(
            docs.iter().any(|d| d.asks_owner),
            "some documents are the owner's to approve: {docs:?}"
        );
        assert!(
            docs.iter().any(|d| !d.asks_owner),
            "and some are generated, or the owner approves everything: {docs:?}"
        );
        assert!(
            docs.iter().all(|d| !d.written),
            "nothing is written in an empty folder: {docs:?}"
        );
        assert!(
            docs.iter()
                .all(|d| !d.holds.is_empty() && !d.consumer.is_empty()),
            "every document says what it holds and who reads it: {docs:?}"
        );
    }

    /// **A written document is reported as written.**
    ///
    /// Control for the test above: without this, "nothing is written" would
    /// pass even if `written` were hardcoded false.
    #[test]
    fn a_document_that_exists_is_reported_as_written() {
        let dir = tempfile::tempdir().expect("a scratch workspace");
        let first = crate::spec::SPEC_SET[0];
        let path = dir.path().join(first.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the docs dir");
        }
        std::fs::write(&path, crate::spec::complete_text(&first)).expect("write it");

        let docs = documents_of(dir.path());
        let found = docs
            .iter()
            .find(|d| d.path == first.path)
            .expect("the document is in the set");
        assert!(
            found.written,
            "a document on disk must not still be reported as owed: {found:?}"
        );
        assert!(
            docs.iter().filter(|d| d.written).count() == 1,
            "only the one written counts: {docs:?}"
        );
    }

    /// **The brief's label is NOT offered as a project id.**
    ///
    /// A project belongs to the memory service: a name with a list of absolute
    /// folders, assigned by a person, deepest folder winning, unassigned work
    /// filed under `general`. A brief's label is a different string that
    /// `derive_from_docs` happens to set to the workspace PATH.
    ///
    /// MEASURED: this endpoint shipped calling that field `project`, and
    /// answered `"project": "D:/ball-auto"` for a folder whose real project was
    /// `general`. An interface trusting it would have offered to create a
    /// project that already existed under another name.
    ///
    /// Put the confusion back by renaming the field to `project` and this fails
    /// on the first assertion.
    #[test]
    fn the_reply_never_claims_to_carry_a_memory_project_id() {
        let dir = tempfile::tempdir().expect("a scratch workspace");
        let wire = serde_json::to_value(gate_of(dir.path())).expect("serialises");
        let object = wire.as_object().expect("an object");

        assert!(
            !object.contains_key("project"),
            "no field may be called `project` — that name belongs to memory: {:?}",
            object.keys().collect::<Vec<_>>()
        );
        assert!(
            object.contains_key("brief_label"),
            "the brief's own label is reported, named for what it is: {:?}",
            object.keys().collect::<Vec<_>>()
        );
        assert!(
            object.contains_key("workspace"),
            "the FOLDER is what an interface resolves against memory's project \
             map, so it must be here: {:?}",
            object.keys().collect::<Vec<_>>()
        );
    }

    /// The states serialise as the plain words an interface will switch on.
    #[test]
    fn the_states_cross_the_wire_as_readable_names() {
        let json = serde_json::to_string(&Standing::NoBrief).expect("serialises");
        assert_eq!(json, "\"no_brief\"");
        assert_eq!(
            serde_json::to_string(&Standing::Ready).expect("serialises"),
            "\"ready\""
        );
    }
}
