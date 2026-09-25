//! The project's own instruction files, read by incode for every model.
//!
//! An agent CLI used to read `CLAUDE.md` for itself. It is now started as a
//! model only, with its own instruction files switched off, and codex, agy and
//! a model over an API never read that file at all. So incode reads the
//! project's instruction files and hands them to whichever model does the work,
//! as part of the session's standing context.
//!
//! Only the workspace root is read, and each file is bounded: the context rides
//! a command line for a CLI model, and a file past its budget is cut with a
//! note saying where the whole of it is, so the model can read the rest.

use std::path::Path;

/// The files read, in this order.
pub const FILES: [&str; 2] = ["CLAUDE.md", "AGENTS.md"];

/// The most of each file given inline, in bytes.
pub const MAX_BYTES: usize = 8_000;

/// The project's instruction files as one context block, or `None` when the
/// workspace has none (or only empty ones).
#[must_use]
pub fn project_block(workspace: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for name in FILES {
        let Ok(text) = std::fs::read_to_string(workspace.join(name)) else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let head = crate::text::head_bytes(text, MAX_BYTES);
        let cut = if head.len() < text.len() {
            format!(
                "\n[… {} more bytes — read `{name}` for the rest.]",
                text.len() - head.len()
            )
        } else {
            String::new()
        };
        parts.push(format!("<file name=\"{name}\">\n{head}{cut}\n</file>"));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "<project-instructions>\nThe project's own instruction files, from the workspace \
         root. Follow them as the project's standing instructions.\n\n{}\n</project-instructions>",
        parts.join("\n\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **BOTH FILES REACH THE CONTEXT**, in order and marked by name.
    #[test]
    fn both_instruction_files_are_read() {
        let dir = tempfile::tempdir().expect("scratch");
        std::fs::write(dir.path().join("CLAUDE.md"), "CANARY-CLAUDE: use tabs").expect("write");
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "CANARY-AGENTS: run cargo test",
        )
        .expect("write");
        let block = project_block(dir.path()).expect("a block");
        assert!(block.starts_with("<project-instructions>"), "{block}");
        let claude = block.find("CANARY-CLAUDE").expect("CLAUDE.md");
        let agents = block.find("CANARY-AGENTS").expect("AGENTS.md");
        assert!(claude < agents, "{block}");
        assert!(block.contains("name=\"CLAUDE.md\""), "{block}");
    }

    #[test]
    fn a_workspace_without_them_adds_nothing() {
        let dir = tempfile::tempdir().expect("scratch");
        assert_eq!(project_block(dir.path()), None);
        std::fs::write(dir.path().join("AGENTS.md"), "  \n").expect("write");
        assert_eq!(
            project_block(dir.path()),
            None,
            "an empty file is not a block"
        );
    }

    /// A long file is cut to its budget and says where the rest is.
    #[test]
    fn a_long_file_is_bounded() {
        let dir = tempfile::tempdir().expect("scratch");
        std::fs::write(dir.path().join("CLAUDE.md"), "é".repeat(MAX_BYTES)).expect("write");
        let block = project_block(dir.path()).expect("a block");
        assert!(block.len() < MAX_BYTES + 600, "{} bytes", block.len());
        assert!(block.contains("read `CLAUDE.md` for the rest"), "{block}");
    }
}
