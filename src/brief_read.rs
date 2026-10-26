//! `read_brief`: the project's brief as the model can read it back.
//!
//! The intake told a model to complete the brief and gave it `save_brief` to
//! do so, but nothing to read what was already there — so a model in a
//! session whose brief existed could save to it and never see it (reported by
//! a model inside incode, 2026-10-26).
//!
//! # Wired to
//!
//! - **Called by** `crate::toolbox` (`reach_of`: observe; `catalogue`; the
//!   dispatch in `Toolbox::run_tool`).
//! - **Calls** [`crate::brief::check`] — the SAME read the intake gate makes,
//!   worktree resolution and the outage cache included, so what the model is
//!   shown is what the gate decided on.
//! - **Sibling** to `save_brief` (`crate::brief::SAVE_TOOL`), its write half.

use std::path::Path;

use crate::brief::{Gate, ProjectBrief};

/// The tool's name.
pub const READ_TOOL: &str = "read_brief";

/// The product document can run to many pages; the head is what a model needs
/// to plan, and the rest stays in the store.
const MAX_PRD_BYTES: usize = 6_000;

/// `read_brief` as advertised to the model.
#[must_use]
pub fn tool_definition() -> serde_json::Value {
    serde_json::json!({
        "name": READ_TOOL,
        "description": "Read this project's brief — what it is, its goals, the PRD \
            synopsis, the PRD and the roadmap — as the intake gate sees it. Read it \
            before save_brief, so what you save adds to what is there.",
        "input_schema": { "type": "object", "properties": {} }
    })
}

/// Run `read_brief` for `workspace`.
pub fn run(workspace: &Path) -> Result<String, String> {
    match crate::brief::check(workspace) {
        Gate::Ready(brief) => Ok(render(&brief)),
        Gate::Incomplete { project, missing } => {
            let missing: Vec<&str> = missing.iter().map(|a| a.field()).collect();
            Ok(format!(
                "The brief for {project} is incomplete: {} still to write.",
                missing.join(", ")
            ))
        }
        Gate::NoBrief { workspace } => Ok(format!(
            "No brief covers {}. Gather one and record it with save_brief.",
            workspace.display()
        )),
        Gate::Unreachable { url, reason } => {
            Err(format!("the brief could not be read from {url}: {reason}"))
        }
    }
}

/// A complete brief as the model reads it.
#[must_use]
pub fn render(brief: &ProjectBrief) -> String {
    let mut out = format!("Brief for {}", brief.project);
    for (label, text) in [
        ("About", brief.about.as_str()),
        ("Goals", brief.goals.as_str()),
        ("Synopsis", brief.synopsis.as_str()),
    ] {
        if !text.trim().is_empty() {
            out.push_str(&format!("\n\n## {label}\n{}", text.trim()));
        }
    }
    if !brief.roadmap.is_empty() {
        out.push_str("\n\n## Roadmap");
        for item in &brief.roadmap {
            let status = if item.status.trim().is_empty() {
                "todo"
            } else {
                item.status.trim()
            };
            out.push_str(&format!("\n- [{status}] {}", item.title.trim()));
        }
    }
    let prd = brief.prd.trim();
    if !prd.is_empty() {
        let head = crate::text::head_bytes(prd, MAX_PRD_BYTES);
        out.push_str(&format!("\n\n## PRD\n{head}"));
        if head.len() < prd.len() {
            out.push_str(&format!(
                "\n[… the first {} of {} bytes]",
                head.len(),
                prd.len()
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_brief_is_rendered_with_its_roadmap_and_a_bounded_prd() {
        let brief = ProjectBrief {
            project: "D:/ball-game".to_string(),
            about: "a ball game".to_string(),
            goals: "fun, fair".to_string(),
            prd: "x".repeat(MAX_PRD_BYTES + 500),
            roadmap: vec![crate::brief::RoadmapItem {
                id: "b1".to_string(),
                title: "jumps".to_string(),
                status: String::new(),
            }],
            ..Default::default()
        };
        let shown = render(&brief);
        assert!(shown.starts_with("Brief for D:/ball-game"), "{shown}");
        assert!(shown.contains("## About\na ball game"), "{shown}");
        assert!(
            shown.contains("- [todo] jumps"),
            "an unset status reads as todo"
        );
        assert!(
            shown.contains(&format!("of {} bytes", MAX_PRD_BYTES + 500)),
            "the cut is said"
        );
        assert!(shown.len() < MAX_PRD_BYTES + 400, "and the PRD is bounded");
        assert!(
            !shown.contains("## Synopsis"),
            "an empty section is left out"
        );
    }

    /// The store being down is an error the model sees, not "no brief" — the
    /// two call for different things (start the service vs. write a brief).
    #[test]
    fn an_unreachable_store_is_not_read_as_no_brief() {
        let dir = tempfile::tempdir().expect("tmp");
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::brief::MEMORY_URL_ENV, "http://127.0.0.1:1");
        let read = run(dir.path());
        std::env::remove_var(crate::brief::MEMORY_URL_ENV);
        let why = read.expect_err("unreachable");
        assert!(why.contains("could not be read"), "{why}");
    }
}
