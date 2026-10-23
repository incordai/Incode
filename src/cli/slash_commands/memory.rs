//! The shared memory: `/remember`, `/forget` and `/memories`.

use crate::cli::render;
use crate::outln;

pub fn handle_remember(text: &str, workspace: &std::path::Path) {
    if text.trim().is_empty() {
        render::print_error("Usage: /remember <what to remember>");
        return;
    }
    // Saved against the workspace, so a fact about this project is recallable
    // as a fact about this project rather than as a loose note.
    match crate::knowledge::remember(text, "fact", &workspace.display().to_string()) {
        Ok(id) => render::print_info(&format!("Saved to the shared memory as {id}.")),
        Err(e) => render::print_error(&format!("NOT saved: {e}")),
    }
}

/// Delete one stored message. Two steps, and the first one shows what would go.
///
/// The identifier is `conv_id#idx`, which is what `/memories` prints beside every
/// result — so what is shown is exactly what can be typed back.
pub fn handle_forget(id: &str, confirmed: bool) {
    let Some((conv_id, idx)) = parse_memory_ref(id) else {
        render::print_error(
            "Usage: /forget <conv_id>#<idx>  (the reference /memories prints beside each result)",
        );
        return;
    };
    match crate::knowledge::forget(&conv_id, idx, confirmed) {
        Ok(crate::knowledge::Forget::NeedsConfirmation { current }) => {
            render::print_header("This would be deleted");
            outln!(
                "{current}
"
            );
            render::print_info(&format!("Run `/forget {id} confirm` to delete it."));
        }
        Ok(crate::knowledge::Forget::Deleted { conv_id, idx }) => {
            render::print_info(&format!("Deleted {conv_id}#{idx}."));
        }
        Err(e) => render::print_error(&format!("Not deleted: {e}")),
    }
}

/// Split `conv_id#idx` into its parts.
///
/// `rsplit_once`, not `split_once`: a conversation id may itself contain `#`,
/// and the index is always the last field.
#[must_use]
pub fn parse_memory_ref(id: &str) -> Option<(String, u64)> {
    let (conv_id, idx) = id.trim().rsplit_once('#')?;
    if conv_id.is_empty() {
        return None;
    }
    Some((conv_id.to_string(), idx.trim().parse().ok()?))
}

/// Search the shared memory.
pub fn handle_memories(query: Option<&str>) {
    render::print_header("Memories");

    let Some(q) = query.map(str::trim).filter(|q| !q.is_empty()) else {
        // No listing endpoint, and inventing one by searching for the empty
        // string would return whatever the index happened to rank first while
        // looking like "everything you have".
        render::print_info("Usage: /memories <what you are looking for>");
        render::print_info("The shared memory is searched by meaning, so ask a question.");
        outln!();
        return;
    };

    match crate::knowledge::search(q, 10) {
        Ok(matches) if matches.is_empty() => render::print_info("Nothing matches that."),
        Ok(matches) => {
            for m in &matches {
                outln!("  {}#{} [{}]", m.conv_id, m.idx, m.role);
                outln!("    {}", crate::text::head_bytes(&m.content, 300));
            }
            outln!();
            render::print_info("Use /forget <conv_id>#<idx> to remove one.");
        }
        Err(e) => render::print_error(&format!("Search failed: {e}")),
    }
    outln!();
}
