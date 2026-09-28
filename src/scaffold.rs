//! The first roadmap item of a project with no code yet: create the layout.
//!
//! # Why the layout is an item, not a preamble
//!
//! On a new project the first thing built is the skeleton — the folders, the
//! manifest, the entry point — and it is the thing most often hand-written
//! wrong: a tree that almost matches what the stack's own generator makes, and
//! that every later task then works around. So the architecture names the
//! generator (`## Layout` in `docs/ARCHITECTURE.md`, see [`crate::spec`]), and
//! running it is roadmap item [`ID`], first, like any other goal.
//!
//! Like any other goal, it is finished when a command says so: its check in
//! `docs/VERIFICATION.md` is the stack's build or test command, read by
//! [`crate::goal`]. A layout that "was created" but does not build is not
//! created.
//!
//! # What is deliberately not here
//!
//! Templates. incode carries no skeleton for any stack: the stack's own
//! generator knows its current shape and incode's copy would be out of date
//! by the next release. The owner can pin the generator per stack in
//! `admin.json` (`scaffold`), and that pin reaches the model as a preference.

use std::collections::BTreeMap;
use std::path::Path;

use crate::brief::RoadmapItem;

/// The scaffold's roadmap id. A goal check needs a digit in its id, and this
/// one must not collide with the `d<n>` ids derived from the PRD.
pub const ID: &str = "s1";

/// The scaffold's roadmap title.
pub const TITLE: &str = "Create the layout";

/// Whether this workspace starts with the scaffold: it has no code yet.
#[must_use]
pub fn applies(workspace: &Path) -> bool {
    !crate::spec::has_existing_code(workspace)
}

/// Whether a roadmap item is the scaffold.
#[must_use]
pub fn is_scaffold(item: &RoadmapItem) -> bool {
    item.id.trim().eq_ignore_ascii_case(ID)
}

/// Put the scaffold at the front of `roadmap` unless it is there already.
/// Returns whether it was added.
///
/// An empty roadmap is left empty: a plan with nothing to build has nothing
/// to scaffold for, and a roadmap of one scaffold would satisfy the brief's
/// "roadmap" artifact while planning nothing.
pub fn put_first(roadmap: &mut Vec<RoadmapItem>) -> bool {
    if roadmap.is_empty() {
        return false;
    }
    if let Some(at) = roadmap.iter().position(is_scaffold) {
        if at != 0 {
            let item = roadmap.remove(at);
            roadmap.insert(0, item);
        }
        return false;
    }
    roadmap.insert(
        0,
        RoadmapItem {
            id: ID.to_string(),
            title: TITLE.to_string(),
            status: "todo".to_string(),
        },
    );
    true
}

/// The `docs/VERIFICATION.md` line that proves the scaffold, as shown to
/// whoever writes that document.
#[must_use]
pub fn check_line() -> String {
    format!("- **{ID} {TITLE}** — `<the stack's build or test command, e.g. cargo build>`")
}

/// Whether a verification document carries a runnable check for the scaffold.
#[must_use]
pub fn verified_in(verification: &str) -> bool {
    crate::goal::parse_checks(verification)
        .iter()
        .any(|check| check.roadmap_id.eq_ignore_ascii_case(ID))
}

/// The owner's pinned generators, as a preference for the model, or `None`
/// when there are none.
#[must_use]
pub fn pinned_note(pins: &BTreeMap<String, String>) -> Option<String> {
    let lines: Vec<String> = pins
        .iter()
        .filter(|(stack, command)| !stack.trim().is_empty() && !command.trim().is_empty())
        .map(|(stack, command)| format!("  - {}: `{}`", stack.trim(), command.trim()))
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "The owner has pinned the generator for these stacks (`scaffold` in \
         admin.json). If the stack you choose is one of them, use that command \
         in `## Layout` rather than another; its version is still yours to \
         verify:\n{}",
        lines.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> RoadmapItem {
        RoadmapItem {
            id: id.to_string(),
            title: format!("item {id}"),
            status: "todo".to_string(),
        }
    }

    #[test]
    fn the_scaffold_goes_first_once() {
        let mut roadmap = vec![item("d1"), item("d2")];
        assert!(put_first(&mut roadmap));
        assert_eq!(roadmap[0].id, ID);
        assert_eq!(roadmap[0].title, TITLE);
        assert_eq!(roadmap.len(), 3);

        assert!(!put_first(&mut roadmap), "already there");
        assert_eq!(roadmap.len(), 3);
    }

    /// A scaffold the model put later in its plan is moved to the front, not
    /// duplicated: nothing can be built before the layout exists.
    #[test]
    fn a_scaffold_written_later_is_moved_to_the_front() {
        let mut roadmap = vec![item("d1"), item("S1"), item("d2")];
        assert!(!put_first(&mut roadmap));
        let ids: Vec<&str> = roadmap.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["S1", "d1", "d2"]);
    }

    #[test]
    fn an_empty_roadmap_is_not_given_a_scaffold() {
        let mut roadmap = Vec::new();
        assert!(!put_first(&mut roadmap));
        assert!(roadmap.is_empty());
    }

    /// The line shown to the model is one the goal parser reads as a check —
    /// once the placeholder is replaced by a command.
    #[test]
    fn the_shown_check_line_is_what_the_goal_parser_reads() {
        let written = check_line().replace(
            "<the stack's build or test command, e.g. cargo build>",
            "cargo build",
        );
        assert!(verified_in(&written), "{written}");
        assert!(!verified_in("- **s1 Create the layout** — done by hand"));
        assert!(!verified_in("- **d1 Something else** — `cargo build`"));
    }

    #[test]
    fn pins_become_a_preference_and_blank_ones_are_dropped() {
        assert_eq!(pinned_note(&BTreeMap::new()), None);
        let pins = BTreeMap::from([
            ("rust".to_string(), "cargo new --vcs none app".to_string()),
            ("python".to_string(), "  ".to_string()),
        ]);
        let note = pinned_note(&pins).expect("one real pin");
        assert!(
            note.contains("- rust: `cargo new --vcs none app`"),
            "{note}"
        );
        assert!(!note.contains("python"), "{note}");
    }
}
