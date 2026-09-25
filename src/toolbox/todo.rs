//! `TodoWrite`: the model's task list for the session.
//!
//! The list is sent WHOLE on every call and replaces the last one — the shape
//! models already use, and exactly the protocol's rule for a plan, so
//! `incode --acp` forwards it as the plan without translating a delta. The
//! session keeps it ([`super::SessionTools`]); the one parser here is used by
//! the tool and by the ACP presenter alike, so what the client is shown is
//! what incode accepted.

use serde_json::{json, Value};

/// The most tasks one list may hold.
pub const MAX_TODOS: usize = 100;

/// The longest one task's text may be, in characters.
pub const MAX_TODO_CHARS: usize = 500;

/// Where a task stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// How much a task matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    High,
    Medium,
    Low,
}

/// One task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    pub content: String,
    pub status: TodoStatus,
    pub priority: Priority,
}

/// The tool as the model is told about it.
pub(super) fn definition() -> Value {
    json!({
        "name": super::TODO_WRITE,
        "description": "Keep a task list for this session. Send the WHOLE list every time: it \
            replaces the previous one. Use it for work of three or more steps; mark a task \
            in_progress when you start it and completed as soon as it is done, and keep one \
            in progress at a time. The person watching sees the list as your plan.",
        "input_schema": {
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "maxItems": MAX_TODOS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "The task, in a few words."},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                            "priority": {"type": "string", "enum": ["high", "medium", "low"], "description": "Defaults to medium."},
                            "activeForm": {"type": "string", "description": "Optional: the task as it is being done, e.g. `Running the tests`."}
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        }
    })
}

/// A task list as the tool's input carries it, or why it is not one.
///
/// Every task is checked; one bad entry refuses the whole list rather than
/// keeping the rest, because a list with a silently missing task reads as a
/// list with nothing left to do.
pub(crate) fn parse_todos(input: &str) -> Result<Vec<Todo>, String> {
    let value: Value =
        serde_json::from_str(input).map_err(|e| format!("the task list is not JSON: {e}"))?;
    parse(&value)
}

fn parse(input: &Value) -> Result<Vec<Todo>, String> {
    let items = input
        .get("todos")
        .and_then(Value::as_array)
        .ok_or("`todos` is required: the whole task list, as an array")?;
    if items.len() > MAX_TODOS {
        return Err(format!(
            "{} tasks is more than the {MAX_TODOS} a list may hold; group them",
            items.len()
        ));
    }
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let n = index + 1;
            let content = item
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .ok_or(format!("task {n} has no `content`"))?;
            if content.chars().count() > MAX_TODO_CHARS {
                return Err(format!(
                    "task {n} is longer than {MAX_TODO_CHARS} characters; say it shorter"
                ));
            }
            let status = match item.get("status").and_then(Value::as_str) {
                Some("pending") => TodoStatus::Pending,
                Some("in_progress") => TodoStatus::InProgress,
                Some("completed") => TodoStatus::Completed,
                other => {
                    return Err(format!(
                        "task {n} has status {other:?}; use pending, in_progress or completed"
                    ))
                }
            };
            let priority = match item.get("priority").and_then(Value::as_str) {
                None | Some("medium") => Priority::Medium,
                Some("high") => Priority::High,
                Some("low") => Priority::Low,
                Some(other) => {
                    return Err(format!(
                        "task {n} has priority `{other}`; use high, medium or low"
                    ))
                }
            };
            Ok(Todo {
                content: content.to_string(),
                status,
                priority,
            })
        })
        .collect()
}

/// Replace the session's list with the one in `input`, and say what it now
/// holds.
pub(super) fn write(todos: &mut Vec<Todo>, input: &Value) -> Result<String, String> {
    let list = parse(input)?;
    let count = |status: TodoStatus| list.iter().filter(|t| t.status == status).count();
    let mut out = format!(
        "Task list saved: {} task(s) — {} done, {} in progress, {} pending.",
        list.len(),
        count(TodoStatus::Completed),
        count(TodoStatus::InProgress),
        count(TodoStatus::Pending)
    );
    for todo in &list {
        let mark = match todo.status {
            TodoStatus::Completed => "[x]",
            TodoStatus::InProgress => "[>]",
            TodoStatus::Pending => "[ ]",
        };
        let priority = match todo.priority {
            Priority::High => " (high)",
            Priority::Medium => "",
            Priority::Low => " (low)",
        };
        out.push_str(&format!("\n{mark} {}{priority}", todo.content));
    }
    *todos = list;
    Ok(out)
}
