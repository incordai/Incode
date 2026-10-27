//! A turn's steps, driven through [`AgentSession::turn`] with a stand-in
//! agent and no terminal behind it: what the stream draws for a tool's
//! result ([`super::TurnStream`]).

use crate::agent::{AgentBackend, AgentEvent, TurnOutcome};
use crate::cli::agent_turn::AgentSession;

/// A backend that starts its reply, reads a file with one of its own
/// tools, and repeats the reply as whole text.
struct Reading;

impl AgentBackend for Reading {
    fn id(&self) -> &str {
        "claude-code"
    }
    fn run_turn(
        &mut self,
        _prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, crate::agent::AgentError> {
        on_event(&AgentEvent::TextDelta("reading it".to_string()));
        on_event(&AgentEvent::ToolUse {
            id: "t1".to_string(),
            name: "Read".to_string(),
            input: r#"{"file_path":"notes.txt"}"#.to_string(),
        });
        on_event(&AgentEvent::ToolResult {
            id: "t1".to_string(),
            output: "line one".to_string(),
            is_error: false,
        });
        on_event(&AgentEvent::Text("reading it".to_string()));
        Ok(TurnOutcome {
            session_id: String::new(),
            stop_reason: "end_turn".to_string(),
            is_error: false,
            detail: None,
            served_by: Vec::new(),
        })
    }
    fn session_id(&self) -> Option<&str> {
        None
    }
}

/// **A TOOL'S RESULT IS DRAWN ON ROWS OF ITS OWN, NAMED BY ITS CALL**,
/// on a terminal: the reply's unfinished row is ended first, the panel
/// carries the tool and what it acted on, the call is reported as the
/// turn's activity, and whole text after streamed deltas is not repeated.
#[test]
fn a_tool_result_is_drawn_on_rows_of_its_own_named_by_its_call() {
    let ws = tempfile::tempdir().expect("ws");
    let mut s = AgentSession::new(
        ws.path().to_path_buf(),
        "claude-code",
        "claude".to_string(),
        &crate::guards::Guards::default(),
        Vec::new(),
    )
    .with_backend(Box::new(Reading));
    let mut shown = String::new();
    let mut statuses = 0;
    let outcome = s.turn(
        ws.path(),
        "go",
        &[],
        &mut |t| shown.push_str(t),
        &mut |_| statuses += 1,
    );
    assert!(outcome.is_ok(), "{outcome:?}");
    let lines: Vec<&str> = shown.lines().collect();
    let head = lines
        .iter()
        .position(|line| line.contains("Read  notes.txt"))
        .expect("the result panel names its call");
    assert!(head > 0 && lines[head - 1] == "reading it", "{shown:?}");
    assert!(shown.contains("line one"), "{shown:?}");
    assert_eq!(shown.matches("reading it").count(), 1, "{shown:?}");
    assert_eq!(statuses, 1, "the call is reported once");
}
