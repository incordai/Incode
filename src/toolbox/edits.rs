//! The tools that change files beyond one `Write` or `Edit`: `MultiEdit`,
//! `ApplyPatch` and `NotebookEdit`.
//!
//! # All or nothing, each of them
//!
//! Every change is worked out in memory first. Nothing is written until the
//! whole of it is known to apply, and a write that fails part-way through a
//! patch puts back the files it had already written. A half-applied change is
//! a tree that matches neither the model's picture nor the file's history.
//!
//! The guards, the checkpoint and the engine's hooks are not here: they sit
//! in [`super::Toolbox::run_call`], which asks them for every path these
//! tools will touch before any of them runs.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::tools::diff::{apply_hunks, parse_unified, unified_diff};

/// The most replacements one `MultiEdit` may carry.
pub const MAX_EDITS: usize = 100;

/// The most files one patch may touch.
pub const MAX_PATCH_FILES: usize = 50;

/// How much of a change's diff is handed back. The file on disk has the rest.
pub const MAX_CHANGE_REPORT_CHARS: usize = 30_000;

/// The largest notebook that is edited. Past it the notebook is refused
/// rather than parsed whole.
pub const MAX_NOTEBOOK_BYTES: u64 = 32 * 1024 * 1024;

/// The tools as the model is told about them.
pub(super) fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": super::MULTI_EDIT,
            "description": "Make several exact-string replacements in ONE file, applied in \
                order, all or none. Each old_string must appear exactly once in the file as it \
                stands after the edits before it (or set replace_all). If any edit does not \
                apply, the file is left untouched. Prefer this to several Edit calls on one \
                file.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_EDITS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": {"type": "string", "description": "Text to replace, matched exactly."},
                                "new_string": {"type": "string", "description": "What to put in its place."},
                                "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of requiring exactly one."}
                            },
                            "required": ["old_string", "new_string"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }
        }),
        json!({
            "name": super::APPLY_PATCH,
            "description": "Apply a unified diff to one or more files: modify (`--- a/x` / \
                `+++ b/x`), create (`--- /dev/null`) or delete (`+++ /dev/null`), with \
                `@@ -l,c +l,c @@` hunks and at least one line of context. Every hunk is \
                checked before anything is written; if one does not fit, no file is \
                changed. Paths are relative to the workspace.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "patch": {"type": "string", "description": "The unified diff."}
                },
                "required": ["patch"]
            }
        }),
        json!({
            "name": super::NOTEBOOK_EDIT,
            "description": "Edit one cell of a Jupyter notebook (.ipynb): replace its source, \
                insert a new cell, or delete one. Name the cell by `cell_id`, or by \
                `cell_number` counting from 0. An insert goes after `cell_id`, or at \
                `cell_number`. The notebook stays valid JSON.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "The .ipynb file."},
                    "cell_id": {"type": "string"},
                    "cell_number": {"type": "integer", "description": "The cell's index, from 0."},
                    "new_source": {"type": "string", "description": "The cell's new source. Required to replace or insert."},
                    "cell_type": {"type": "string", "enum": ["code", "markdown"], "description": "For an insert (default code), or to change a replaced cell's type."},
                    "edit_mode": {"type": "string", "enum": ["replace", "insert", "delete"], "description": "Defaults to replace."}
                },
                "required": ["path"]
            }
        }),
    ]
}

/// `path` resolved under `root` the way every file tool resolves it.
fn resolve_under(root: &Path, path: &str) -> Result<PathBuf, String> {
    let probe = json!({ "path": path }).to_string();
    crate::tools::file_ops::target_path(root, &probe).map_err(|e| e.to_string())
}

/// `text`, cut to [`MAX_CHANGE_REPORT_CHARS`] with the cut said.
fn bounded(mut text: String) -> String {
    crate::text::truncate_in_place(
        &mut text,
        MAX_CHANGE_REPORT_CHARS,
        "\n... (diff truncated; the file on disk has all of it)",
    );
    text
}

// ---------------------------------------------------------------------------
// MultiEdit
// ---------------------------------------------------------------------------

/// Apply every replacement in `input` to one file, or none of them.
pub(super) fn multi_edit(root: &Path, input: &Value) -> Result<String, String> {
    let named = input
        .get("path")
        .and_then(Value::as_str)
        .ok_or("`path` is required")?;
    let edits = input
        .get("edits")
        .and_then(Value::as_array)
        .filter(|e| !e.is_empty())
        .ok_or("`edits` is required: at least one {old_string, new_string}")?;
    if edits.len() > MAX_EDITS {
        return Err(format!(
            "{} edits is more than the {MAX_EDITS} one call may carry",
            edits.len()
        ));
    }
    let path = resolve_under(root, named)?;
    let original = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;

    let mut text = original.clone();
    for (index, edit) in edits.iter().enumerate() {
        let n = index + 1;
        let old = edit
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or(format!("edit {n} has no `old_string`"))?;
        let new = edit
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or(format!("edit {n} has no `new_string`"))?;
        if old.is_empty() {
            return Err(format!("edit {n}: `old_string` is empty; nothing to find"));
        }
        if old == new {
            return Err(format!(
                "edit {n}: `old_string` and `new_string` are the same"
            ));
        }
        let replace_all = edit
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let count = text.matches(old).count();
        if count == 0 {
            return Err(format!(
                "edit {n}: `old_string` was not found in {} (after the edits before it). \
                 Nothing was changed.",
                path.display()
            ));
        }
        if count > 1 && !replace_all {
            return Err(format!(
                "edit {n}: `old_string` matches {count} times in {} — it must be unique, or \
                 set replace_all. Nothing was changed.",
                path.display()
            ));
        }
        text = if replace_all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
    }

    std::fs::write(&path, &text).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    let diff = unified_diff(&original, &text, named);
    Ok(bounded(format!(
        "Applied {} edit(s) to {}\n\n{diff}",
        edits.len(),
        path.display()
    )))
}

// ---------------------------------------------------------------------------
// ApplyPatch
// ---------------------------------------------------------------------------

/// Every file `input`'s patch would change, as the patch names them.
///
/// # Errors
///
/// The patch is missing or does not parse — refused before anything else is
/// asked about it.
pub(super) fn patch_paths(input: &Value) -> Result<Vec<String>, String> {
    let text = input
        .get("patch")
        .and_then(Value::as_str)
        .ok_or("`patch` is required: a unified diff")?;
    let patches = parse_unified(text).map_err(|e| format!("the patch could not be read: {e}"))?;
    if patches.len() > MAX_PATCH_FILES {
        return Err(format!(
            "the patch touches {} files, more than the {MAX_PATCH_FILES} one call may",
            patches.len()
        ));
    }
    Ok(patches.iter().map(|p| p.target().to_string()).collect())
}

/// One file's change, worked out and not yet written.
struct Planned {
    path: PathBuf,
    named: String,
    /// What it holds now; `None` when it does not exist.
    before: Option<String>,
    /// What it will hold; `None` when it is deleted.
    after: Option<String>,
}

/// Apply `input`'s patch, every file or none. `resolve` confines each path
/// the way the file tools do.
pub(super) fn apply_patch(
    input: &Value,
    resolve: &dyn Fn(&str) -> Result<PathBuf, String>,
) -> Result<String, String> {
    let text = input
        .get("patch")
        .and_then(Value::as_str)
        .ok_or("`patch` is required: a unified diff")?;
    let patches = parse_unified(text).map_err(|e| format!("the patch could not be read: {e}"))?;
    if patches.len() > MAX_PATCH_FILES {
        return Err(format!(
            "the patch touches {} files, more than the {MAX_PATCH_FILES} one call may",
            patches.len()
        ));
    }

    // EVERY HUNK, EVERY FILE, BEFORE ANY WRITE.
    let mut plan: Vec<Planned> = Vec::new();
    for patch in &patches {
        let named = patch.target().to_string();
        let path = resolve(&named)?;
        // Two spellings of one file would each be worked out against the
        // same original, and the second write would silently undo the first.
        if let Some(earlier) = plan.iter().find(|p| p.path == path) {
            return Err(format!(
                "`{named}` and `{}` are the same file; put all its hunks under one header. \
                 No file was changed.",
                earlier.named
            ));
        }
        let before = if path.exists() {
            Some(
                std::fs::read_to_string(&path)
                    .map_err(|e| format!("{named}: could not be read as text: {e}"))?,
            )
        } else {
            None
        };
        let refuse = |why: String| format!("{named}: {why}. No file was changed.");
        let after = match (&patch.old_path, &patch.new_path, &before) {
            (None, Some(_), Some(_)) => {
                return Err(refuse("the patch creates it, but it already exists".into()))
            }
            (Some(_), _, None) => {
                return Err(refuse("the patch changes it, but it does not exist".into()))
            }
            (Some(old), Some(new), _) if old != new => {
                return Err(refuse(format!(
                    "the patch renames `{old}` to `{new}`, which is not applied"
                )))
            }
            (_, None, Some(current)) => {
                let left = apply_hunks(current, &patch.hunks).map_err(refuse)?;
                if !left.trim().is_empty() {
                    return Err(refuse(
                        "the patch deletes it, but its hunks do not remove every line".into(),
                    ));
                }
                None
            }
            (_, Some(_), current) => {
                Some(apply_hunks(current.as_deref().unwrap_or(""), &patch.hunks).map_err(refuse)?)
            }
            (None, None, _) => return Err(refuse("the patch names no file".into())),
        };
        plan.push(Planned {
            path,
            named,
            before,
            after,
        });
    }

    // Written in order; a failure puts back every file already written.
    for (done, step) in plan.iter().enumerate() {
        if let Err(why) = write_step(step) {
            // EVERY step is put back, even after one fails: `all` would stop
            // at the first failure and leave the rest written.
            let mut restored = true;
            for written in plan[..done].iter().rev() {
                if !undo_step(written) {
                    restored = false;
                }
            }
            return Err(format!(
                "{}: {why}. {}",
                step.named,
                if restored {
                    "The files written before it were put back; nothing is changed."
                } else {
                    "Putting back the files written before it FAILED for at least one; check \
                     them."
                }
            ));
        }
    }

    let mut report = format!("Applied the patch to {} file(s):", plan.len());
    let mut diffs = String::new();
    for step in &plan {
        let (verb, old, new) = match (&step.before, &step.after) {
            (None, Some(new)) => ("created", "", new.as_str()),
            (Some(old), None) => ("deleted", old.as_str(), ""),
            (Some(old), Some(new)) => ("modified", old.as_str(), new.as_str()),
            (None, None) => ("unchanged", "", ""),
        };
        report.push_str(&format!("\n  {verb} {}", step.named));
        diffs.push_str(&unified_diff(old, new, &step.named));
    }
    Ok(bounded(format!("{report}\n\n{diffs}")))
}

fn write_step(step: &Planned) -> Result<(), String> {
    match &step.after {
        Some(text) => {
            if let Some(parent) = step.path.parent() {
                crate::folders::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&step.path, text).map_err(|e| e.to_string())
        }
        None => std::fs::remove_file(&step.path).map_err(|e| e.to_string()),
    }
}

/// Put one written step back. `true` when it was.
fn undo_step(step: &Planned) -> bool {
    match &step.before {
        Some(text) => std::fs::write(&step.path, text).is_ok(),
        None => std::fs::remove_file(&step.path).is_ok(),
    }
}

// ---------------------------------------------------------------------------
// NotebookEdit
// ---------------------------------------------------------------------------

/// Replace, insert or delete one cell of the notebook `input` names.
pub(super) fn notebook_edit(root: &Path, input: &Value) -> Result<String, String> {
    let named = input
        .get("path")
        .and_then(Value::as_str)
        .ok_or("`path` is required")?;
    if !named.to_ascii_lowercase().ends_with(".ipynb") {
        return Err(format!(
            "`{named}` is not a notebook: NotebookEdit edits .ipynb files; use Edit for others"
        ));
    }
    let path = resolve_under(root, named)?;
    let size = std::fs::metadata(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?
        .len();
    if size > MAX_NOTEBOOK_BYTES {
        return Err(format!(
            "{} is {size} bytes, larger than the {MAX_NOTEBOOK_BYTES} a notebook edit reads",
            path.display()
        ));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let mut notebook: Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid notebook JSON: {e}", path.display()))?;
    let with_ids = notebook_uses_ids(&notebook);
    let cells = notebook
        .get_mut("cells")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| format!("{} has no `cells` list", path.display()))?;

    let mode = input
        .get("edit_mode")
        .and_then(Value::as_str)
        .unwrap_or("replace");
    let source = input.get("new_source").and_then(Value::as_str);
    let cell_type = input.get("cell_type").and_then(Value::as_str);
    if let Some(kind) = cell_type {
        if !matches!(kind, "code" | "markdown") {
            return Err(format!("cell_type `{kind}` is not code or markdown"));
        }
    }
    let by_id = input.get("cell_id").and_then(Value::as_str);
    let by_number = input
        .get("cell_number")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok());
    let located = || -> Result<usize, String> {
        match (by_id, by_number) {
            (Some(id), _) => cells
                .iter()
                .position(|c| c.get("id").and_then(Value::as_str) == Some(id))
                .ok_or_else(|| {
                    format!(
                        "no cell has id `{id}`; the notebook has {} cells",
                        cells.len()
                    )
                }),
            (None, Some(n)) if n < cells.len() => Ok(n),
            (None, Some(n)) => Err(format!(
                "cell_number {n} is past the end: the notebook has {} cells",
                cells.len()
            )),
            (None, None) => Err("name the cell with `cell_id` or `cell_number`".to_string()),
        }
    };

    let said = match mode {
        "replace" => {
            let at = located()?;
            let source = source.ok_or("`new_source` is required to replace a cell")?;
            let cell = &mut cells[at];
            let kind = cell_type
                .or_else(|| cell.get("cell_type").and_then(Value::as_str))
                .unwrap_or("code")
                .to_string();
            cell["cell_type"] = json!(kind);
            cell["source"] = source_lines(source);
            shape_for(cell, &kind);
            format!("Replaced cell {at} ({kind})")
        }
        "insert" => {
            let source = source.ok_or("`new_source` is required to insert a cell")?;
            let at = match (by_id, by_number) {
                (Some(_), _) => located()? + 1,
                (None, Some(n)) if n <= cells.len() => n,
                (None, Some(n)) => {
                    return Err(format!(
                        "cell_number {n} is past the end: the notebook has {} cells",
                        cells.len()
                    ))
                }
                (None, None) => 0,
            };
            let kind = cell_type.unwrap_or("code");
            let mut cell = json!({
                "cell_type": kind,
                "metadata": {},
                "source": source_lines(source),
            });
            if with_ids {
                cell["id"] = json!(uuid::Uuid::new_v4().simple().to_string()[..8].to_string());
            }
            shape_for(&mut cell, kind);
            cells.insert(at, cell);
            format!("Inserted a {kind} cell at {at}")
        }
        "delete" => {
            let at = located()?;
            cells.remove(at);
            format!("Deleted cell {at}")
        }
        other => {
            return Err(format!(
                "edit_mode `{other}` is not replace, insert or delete"
            ))
        }
    };

    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut writer = serde_json::Serializer::with_formatter(&mut out, formatter);
    serde::Serialize::serialize(&notebook, &mut writer)
        .map_err(|e| format!("the notebook could not be written back: {e}"))?;
    out.push(b'\n');
    std::fs::write(&path, &out).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(format!("{said} of {}.", path.display()))
}

/// Whether this notebook's cells carry ids (nbformat 4.5 and later).
fn notebook_uses_ids(notebook: &Value) -> bool {
    let number = |key: &str| notebook.get(key).and_then(Value::as_u64).unwrap_or(0);
    let (major, minor) = (number("nbformat"), number("nbformat_minor"));
    major > 4 || (major == 4 && minor >= 5)
}

/// A cell's source as notebooks store it: lines, each keeping its newline.
fn source_lines(source: &str) -> Value {
    json!(source.split_inclusive('\n').collect::<Vec<_>>())
}

/// A code cell has outputs and an execution count; a markdown cell has
/// neither. A replaced code cell's old outputs no longer describe it.
fn shape_for(cell: &mut Value, kind: &str) {
    let Some(object) = cell.as_object_mut() else {
        return;
    };
    if kind == "code" {
        object.insert("outputs".to_string(), json!([]));
        object.insert("execution_count".to_string(), Value::Null);
    } else {
        object.remove("outputs");
        object.remove("execution_count");
    }
}
