//! Background shell jobs as tools: `Bash` with `run_in_background`,
//! `BashOutput` and `KillShell`, over the session's
//! [`crate::tools::bash_jobs::BashJobManager`].

use std::path::Path;

use serde_json::{json, Value};

use super::SessionTools;

/// The tools as the model is told about them.
pub(super) fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": super::BASH_OUTPUT,
            "description": "Read what a background job (started with Bash and \
                `run_in_background`) has printed since you last read it, and whether it is \
                still running or has exited.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "bash_id": {"type": "string", "description": "The job id Bash returned."}
                },
                "required": ["bash_id"]
            }
        }),
        json!({
            "name": super::KILL_SHELL,
            "description": "Stop a background job, and everything it started.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "shell_id": {"type": "string", "description": "The job id Bash returned."}
                },
                "required": ["shell_id"]
            }
        }),
    ]
}

/// A job id as the model sent it: a number, or a number as text.
fn job_id(input: &Value, key: &str) -> Result<u32, String> {
    let raw = input.get(key);
    raw.and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| {
            raw.and_then(Value::as_str)
                .and_then(|s| s.trim().trim_start_matches('#').parse().ok())
        })
        .ok_or_else(|| format!("`{key}` must be the job id Bash returned, e.g. \"1\""))
}

/// Start `input`'s command as a background job of the session.
pub(super) fn start(state: &SessionTools, root: &Path, input: &Value) -> Result<String, String> {
    let command = input
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or("`command` is required")?;
    let id = state
        .jobs()
        .spawn(command, root)
        .map_err(|e| format!("{e:#}"))?;
    Ok(format!(
        "Started background job {id}. Read its output with BashOutput {{\"bash_id\": \"{id}\"}}; \
         stop it with KillShell {{\"shell_id\": \"{id}\"}}."
    ))
}

/// What job `bash_id` has said since it was last read.
pub(super) fn output(state: &SessionTools, input: &Value) -> Result<String, String> {
    let id = job_id(input, "bash_id")?;
    let read = state
        .jobs()
        .read_new(id)
        .ok_or_else(|| format!("there is no background job {id} in this session"))?;
    let mut out = format!("Job {id}: {}.", read.state);
    if read.lost > 0 {
        out.push_str(&format!(
            "\n[{} bytes were printed and dropped before this read: the job keeps the last {} \
             bytes of each stream]",
            read.lost,
            crate::tools::bash_jobs::MAX_JOB_BUFFER_BYTES
        ));
    }
    if read.stdout.is_empty() && read.stderr.is_empty() {
        out.push_str("\n(no new output)");
    }
    if !read.stdout.is_empty() {
        out.push('\n');
        out.push_str(&read.stdout);
    }
    if !read.stderr.is_empty() {
        out.push_str("\nSTDERR:\n");
        out.push_str(&read.stderr);
    }
    if read.more > 0 {
        out.push_str(&format!(
            "\n[{} more bytes are waiting: read again for them]",
            read.more
        ));
    }
    Ok(out)
}

/// Stop job `shell_id`.
pub(super) fn kill(state: &SessionTools, input: &Value) -> Result<String, String> {
    let id = job_id(input, "shell_id")?;
    if state.jobs().kill(id) {
        Ok(format!("Job {id} was stopped."))
    } else {
        Err(format!(
            "job {id} is not running in this session: it has ended already, or there is no \
             such job"
        ))
    }
}
