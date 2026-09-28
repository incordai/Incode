//! `Diagnostics`: run the project's own checker and hand back its errors.
//!
//! The checker is the one [`crate::tools::diagnostics`] already chooses —
//! the operator's `INCODE_CHECK_CMD`, or the manifest's (`cargo check`,
//! `tsc --noEmit`) — and it runs once per call. Given files, the report is
//! cut to the lines about them; given none, it is every error and warning,
//! bounded. Running a checker runs build scripts, so this is a tool that
//! RUNS code and is governed like the shell.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::tools::diagnostics::{
    command_from_env, detect_command, problems_in, report_from_output, DiagnosticsRunner,
    CHECK_COMMAND_ENV, PROJECT_CHECK_TIMEOUT,
};

/// The most files one call may ask about.
pub const MAX_DIAGNOSTIC_FILES: usize = 50;

/// The tool as the model is told about it.
pub(super) fn definition() -> Value {
    json!({
        "name": super::DIAGNOSTICS,
        "description": "Run this project's type checker / compiler check (e.g. `cargo check`, \
            `tsc --noEmit`) and get back its errors and warnings. Pass `files` to see only \
            what it says about those files; omit it for the whole project. Use it after an \
            edit to find out whether the code still compiles. Slower than reading, faster \
            than the full test suite.",
        "input_schema": {
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "items": {"type": "string"},
                    "maxItems": MAX_DIAGNOSTIC_FILES,
                    "description": "Files to report on, relative to the workspace or absolute."
                }
            }
        }
    })
}

/// Run the checker in `root` and report on `input`'s files, or on all.
pub(super) fn diagnostics(
    root: &Path,
    input: &Value,
    resolve: &dyn Fn(&str) -> Result<PathBuf, String>,
) -> Result<String, String> {
    let named: Vec<&str> = input
        .get("files")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if named.len() > MAX_DIAGNOSTIC_FILES {
        return Err(format!(
            "{} files is more than the {MAX_DIAGNOSTIC_FILES} one call may ask about; omit \
             `files` to check the whole project",
            named.len()
        ));
    }
    let files: Vec<PathBuf> = named.iter().map(|f| resolve(f)).collect::<Result<_, _>>()?;

    let Some(command) = command_from_env().or_else(|| detect_command(root)) else {
        return Err(format!(
            "no checker is known for this project: it has no Cargo.toml or tsconfig.json, and \
             {CHECK_COMMAND_ENV} is not set. What was detected: {}",
            crate::verify::detection_note(root)
        ));
    };
    let shown = command.join(" ");
    let runner = DiagnosticsRunner::new(root, command);
    let (output, passed) = runner.run_within(PROJECT_CHECK_TIMEOUT)?;

    if files.is_empty() {
        let report = problems_in(&output);
        return Ok(match report.rendered() {
            None if passed => format!("`{shown}` passed with nothing to report."),
            None => format!(
                "`{shown}` failed, and printed no line naming an error or warning. Its \
                 output:\n{}",
                crate::text::truncate(&output, 2_000)
            ),
            Some(found) => format!(
                "`{shown}` {}:{}",
                if passed { "passed" } else { "failed" },
                found.trim_start_matches("\n\nDiagnostics for this file:")
            ),
        });
    }

    let mut out = format!("`{shown}` {}.", if passed { "passed" } else { "failed" });
    for file in &files {
        let report = report_from_output(&output, file, root);
        match report.rendered() {
            None => out.push_str(&format!("\n{}: nothing reported.", file.display())),
            Some(found) => out.push_str(&format!(
                "\n{}:{}",
                file.display(),
                found.trim_start_matches("\n\nDiagnostics for this file:")
            )),
        }
    }
    Ok(out)
}
