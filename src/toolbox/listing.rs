//! `LS`: one directory's entries.
//!
//! No ignore list of its own: [`crate::tools::file_ops::glob_search`] applies
//! none, and a listing that hid what a search shows would contradict it. The
//! model may pass `ignore` patterns instead, and the listing is bounded so a
//! directory of build output cannot flood the context.

use std::path::Path;

use serde_json::{json, Value};

/// The most entries one listing shows. The rest are counted.
pub const MAX_LS_ENTRIES: usize = 500;

/// The tool as the model is told about it.
pub(super) fn definition() -> Value {
    json!({
        "name": super::LS,
        "description": "List one directory: its subdirectories (ending in `/`) and files with \
            their sizes. Faster than Glob when you want to see what is in one place. Pass \
            `ignore` glob patterns to leave out names, e.g. [\"target\", \"*.log\"].",
        "input_schema": {
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory to list, relative to the workspace or absolute. Defaults to the workspace."},
                "ignore": {"type": "array", "items": {"type": "string"}, "description": "Glob patterns matched against each entry's name."}
            }
        }
    })
}

/// List the directory `input` names under `root`.
pub(super) fn list(root: &Path, input: &Value) -> Result<String, String> {
    let named = input.get("path").and_then(Value::as_str).unwrap_or(".");
    let probe = json!({ "path": named }).to_string();
    let dir = crate::tools::file_ops::target_path(root, &probe).map_err(|e| e.to_string())?;
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    let ignore: Vec<glob::Pattern> = input
        .get("ignore")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(|p| glob::Pattern::new(p).map_err(|e| format!("ignore pattern `{p}`: {e}")))
        .collect::<Result<_, _>>()?;

    let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut listed: Vec<(bool, String, u64)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if ignore.iter().any(|p| p.matches(&name)) {
            continue;
        }
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().is_some_and(std::fs::Metadata::is_dir);
        let size = meta.as_ref().map_or(0, std::fs::Metadata::len);
        listed.push((is_dir, name, size));
    }
    // Directories first, then files, each alphabetical.
    listed.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let total = listed.len();
    let mut out = format!("{} ({total} entries)", dir.display());
    for (is_dir, name, size) in listed.iter().take(MAX_LS_ENTRIES) {
        if *is_dir {
            out.push_str(&format!("\n  {name}/"));
        } else {
            out.push_str(&format!("\n  {name}  ({size} bytes)"));
        }
    }
    if total > MAX_LS_ENTRIES {
        out.push_str(&format!(
            "\n  … {} more entries not shown; narrow with `ignore` or list a subdirectory",
            total - MAX_LS_ENTRIES
        ));
    }
    Ok(out)
}
