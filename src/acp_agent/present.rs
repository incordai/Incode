//! What crosses between the protocol's shapes and incode's own: a prompt's
//! content blocks become the instruction a turn is given, and a turn's agent
//! events become `session/update` payloads.
//!
//! Every payload here is a type from `agent-client-protocol-schema`, built with
//! its own constructors, so the wire shape is the schema's and never a copy.

use std::path::{Path, PathBuf};

use agent_client_protocol_schema::v1 as acp;
use serde_json::Value;

/// The most of one tool's output sent to the client, in bytes.
///
/// A tool result is the agent's own record — a whole file read, a full test
/// log — and the client shows it as a preview beside the call. Sixty-four KiB
/// is several screens of text, far more than any preview draws, while keeping
/// one `tool_call_update` line a size every client reads comfortably. The
/// whole output stays in the agent's transcript; only the copy sent here is
/// cut, and the cut says so.
pub(crate) const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;

/// The tool whose input is the agent's task list, reported as a plan.
const TASK_LIST_TOOL: &str = "TodoWrite";

/// The instruction a prompt's content blocks make, in the client's order.
///
/// # What is accepted
///
/// Text, and — as the protocol requires of every agent — resource links. An
/// embedded TEXT resource is accepted too, because `embeddedContext` is
/// advertised. Images, audio and binary attachments are not advertised, so
/// one arriving is refused rather than silently dropped: a prompt that loses
/// part of what the person attached reads to them as a prompt the agent
/// ignored.
///
/// # Paths from the client
///
/// A `file:` link is passed on only when it resolves INSIDE the workspace.
/// This agent never reads it — the agent CLI does, with its own tools and its
/// own guards — but naming a path outside the workspace to it would be an
/// invitation to go there, and the client is not trusted to decide that.
pub(crate) fn prompt_text(
    blocks: &[acp::ContentBlock],
    workspace: &Path,
) -> Result<String, String> {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            acp::ContentBlock::Text(text) => parts.push(text.text.clone()),
            acp::ContentBlock::ResourceLink(link) => parts.push(linked(link, workspace)?),
            acp::ContentBlock::Resource(embedded) => {
                parts.push(embedded_text(embedded, workspace)?)
            }
            acp::ContentBlock::Image(_) => {
                return Err("images are not accepted: this agent advertises `image: false`".into())
            }
            acp::ContentBlock::Audio(_) => {
                return Err("audio is not accepted: this agent advertises `audio: false`".into())
            }
            _ => return Err("a kind of content this agent does not accept".into()),
        }
    }
    Ok(parts.join("\n\n"))
}

fn linked(link: &acp::ResourceLink, workspace: &Path) -> Result<String, String> {
    match scheme_of(&link.uri).as_deref() {
        Some("file") => {}
        // A web address names nothing on this machine, so there is nothing to
        // confine; it is passed on as the reference it is.
        Some("http" | "https") => return Ok(format!("[link: {} <{}>]", link.name, link.uri)),
        // Anything else — another scheme, or a bare path such as `C:\x` that
        // would reach the agent unexamined — is refused.
        _ => {
            return Err(format!(
                "`{}` is neither a file: URI inside the workspace nor a web address",
                link.name
            ))
        }
    }
    let path = file_uri_path(&link.uri)
        .ok_or_else(|| format!("`{}` is not a file URI this agent can read", link.uri))?;
    match within(&path, workspace) {
        Some(relative) => Ok(format!("[attached file: {}]", relative.display())),
        None => Err(format!(
            "`{}` is not inside this session's workspace, so it was not passed on",
            link.name
        )),
    }
}

fn embedded_text(embedded: &acp::EmbeddedResource, workspace: &Path) -> Result<String, String> {
    match &embedded.resource {
        acp::EmbeddedResourceResource::TextResourceContents(text) => {
            // The content came from the client, inline; only the LABEL is a
            // path, and a path outside the workspace is not repeated.
            let label = file_uri_path(&text.uri)
                .and_then(|path| within(&path, workspace))
                .map_or_else(|| "attached text".to_string(), |p| p.display().to_string());
            Ok(format!(
                "<context source=\"{label}\">\n{}\n</context>",
                text.text
            ))
        }
        acp::EmbeddedResourceResource::BlobResourceContents(_) => {
            Err("binary attachments are not accepted".into())
        }
        _ => Err("a kind of attachment this agent does not accept".into()),
    }
}

/// A URI's scheme, lower-cased, when it has one of at least two characters.
///
/// Schemes are case-insensitive, so `FILE:` is `file:`. One character is not a
/// scheme but a drive letter: `C:\x` is a path, not a URI.
fn scheme_of(uri: &str) -> Option<String> {
    let (scheme, _) = uri.split_once(':')?;
    let valid = scheme.len() >= 2
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then(|| scheme.to_ascii_lowercase())
}

/// The local path a `file:` URI names, percent-decoded.
///
/// Only this machine: an empty authority or `localhost`. A path that decodes
/// to a network share (`//host/share`, `\\host\share`) is refused before
/// anything touches it — resolving one would connect to that host.
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    if scheme_of(uri).as_deref() != Some("file") {
        return None;
    }
    let rest = uri.get("file:".len()..)?.strip_prefix("//")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let decoded = percent_decode(rest)?;
    if decoded.starts_with("//") || decoded.contains('\\') {
        return None;
    }
    // `file:///C:/work` carries the drive after a slash.
    #[cfg(windows)]
    let decoded = match decoded.as_bytes() {
        [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => decoded[1..].to_string(),
        _ => decoded,
    };
    Some(PathBuf::from(decoded))
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `path` relative to the workspace, when it is really inside it.
///
/// Both sides are resolved first, so `..` and links cannot walk out: a path
/// that does not exist, or resolves elsewhere, is not inside.
fn within(path: &Path, workspace: &Path) -> Option<PathBuf> {
    let root = workspace.canonicalize().ok()?;
    let resolved = path.canonicalize().ok()?;
    resolved.strip_prefix(&root).ok().map(Path::to_path_buf)
}

/// A tool call as the client shows it: named, classified, in progress.
pub(crate) fn tool_call(id: &str, name: &str, input: &str) -> acp::ToolCall {
    let raw: Option<Value> = serde_json::from_str(input).ok();
    let title = match crate::cli::panel::target(input) {
        Some(target) => format!("{name} {target}"),
        None => name.to_string(),
    };
    let locations: Vec<acp::ToolCallLocation> = raw
        .as_ref()
        .and_then(|value| {
            ["file_path", "notebook_path", "path"]
                .iter()
                .find_map(|key| value.get(*key).and_then(Value::as_str))
        })
        .map(|path| vec![acp::ToolCallLocation::new(path)])
        .unwrap_or_default();
    acp::ToolCall::new(id.to_string(), title)
        .kind(tool_kind(name))
        .status(acp::ToolCallStatus::InProgress)
        .locations(locations)
        .raw_input(raw)
}

/// What a tool returned, as an update to its call.
pub(crate) fn tool_result(id: &str, output: &str, is_error: bool) -> acp::ToolCallUpdate {
    let shown = if output.len() > MAX_TOOL_OUTPUT_BYTES {
        format!(
            "{}\n[… {} more bytes not shown here; the agent has the whole output]",
            crate::text::head_bytes(output, MAX_TOOL_OUTPUT_BYTES),
            output.len() - crate::text::head_bytes(output, MAX_TOOL_OUTPUT_BYTES).len()
        )
    } else {
        output.to_string()
    };
    let status = if is_error {
        acp::ToolCallStatus::Failed
    } else {
        acp::ToolCallStatus::Completed
    };
    acp::ToolCallUpdate::new(
        id.to_string(),
        acp::ToolCallUpdateFields::new()
            .status(status)
            .content(vec![acp::ToolCallContent::from(shown)]),
    )
}

/// What kind of work a tool does, from its name.
///
/// Only a hint for the client's icon. A name that matches nothing is
/// [`acp::ToolKind::Other`], never a guess.
pub(crate) fn tool_kind(name: &str) -> acp::ToolKind {
    match name.to_ascii_lowercase().as_str() {
        "read" | "notebookread" | "read_file" => acp::ToolKind::Read,
        "edit" | "multiedit" | "write" | "notebookedit" | "apply_patch" | "write_file" => {
            acp::ToolKind::Edit
        }
        "bash" | "powershell" | "shell" | "exec_command" | "run_shell_command" => {
            acp::ToolKind::Execute
        }
        "grep" | "glob" | "ls" | "search" | "toolsearch" => acp::ToolKind::Search,
        "webfetch" | "websearch" | "web_fetch" | "web_search" => acp::ToolKind::Fetch,
        "todowrite" | "think" => acp::ToolKind::Think,
        _ => acp::ToolKind::Other,
    }
}

/// The agent's task list, when this tool call is one, as an ACP plan.
///
/// The list arrives as the input of the agent's task-list tool, whole each
/// time — which is exactly ACP's rule for a plan: every update replaces the
/// last. `None` for every other tool, and for an input that is not the shape
/// this reads, rather than a plan with invented entries.
pub(crate) fn plan_of(name: &str, input: &str) -> Option<acp::Plan> {
    if name != TASK_LIST_TOOL {
        return None;
    }
    let value: Value = serde_json::from_str(input).ok()?;
    let todos = value.get("todos")?.as_array()?;
    let entries: Vec<acp::PlanEntry> = todos
        .iter()
        .filter_map(|todo| {
            let content = todo.get("content")?.as_str()?;
            let status = match todo.get("status")?.as_str()? {
                "pending" => acp::PlanEntryStatus::Pending,
                "in_progress" => acp::PlanEntryStatus::InProgress,
                "completed" => acp::PlanEntryStatus::Completed,
                _ => return None,
            };
            // The task list carries no priority, and ACP requires one. Medium
            // is the protocol's own middle, not a claim about the task.
            Some(acp::PlanEntry::new(
                content,
                acp::PlanEntryPriority::Medium,
                status,
            ))
        })
        .collect();
    Some(acp::Plan::new(entries))
}

/// How a completed turn ended, in the protocol's words.
///
/// Only a turn that SUCCEEDED reaches this; a failed one is a JSON-RPC error
/// and a cancelled one is decided by the cancel, not by what the agent said.
pub(crate) fn stop_reason(agent_said: &str) -> acp::StopReason {
    match agent_said {
        "max_tokens" => acp::StopReason::MaxTokens,
        "refusal" => acp::StopReason::Refusal,
        "max_turns" | "max_turn_requests" => acp::StopReason::MaxTurnRequests,
        _ => acp::StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &str) -> acp::ContentBlock {
        acp::ContentBlock::from(t)
    }

    fn uri_of(path: &Path) -> String {
        let shown = path.to_string_lossy().replace('\\', "/");
        if shown.starts_with('/') {
            format!("file://{shown}")
        } else {
            format!("file:///{shown}")
        }
    }

    #[test]
    fn text_blocks_join_in_the_clients_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let got = prompt_text(&[text("first"), text("second")], dir.path()).expect("text");
        assert_eq!(got, "first\n\nsecond");
    }

    /// **A PATH OUTSIDE THE WORKSPACE IS NOT PASSED ON.**
    ///
    /// The client names the file; this agent does not read it, the agent CLI
    /// would. Handing it a path outside the workspace is handing it a reason
    /// to go there, and the client is not the one who decides that.
    #[test]
    fn a_link_outside_the_workspace_is_refused() {
        let workspace = tempfile::tempdir().expect("workspace");
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let outside = elsewhere.path().join("secret.txt");
        std::fs::write(&outside, "x").expect("write");
        let link =
            acp::ContentBlock::ResourceLink(acp::ResourceLink::new("secret.txt", uri_of(&outside)));
        let err = prompt_text(&[link], workspace.path()).expect_err("must be refused");
        assert!(err.contains("not inside"), "got: {err}");
    }

    /// `..` cannot walk a link out of the workspace: both sides are resolved
    /// before they are compared.
    #[test]
    fn dot_dot_does_not_escape_the_workspace() {
        let parent = tempfile::tempdir().expect("parent");
        let workspace = parent.path().join("ws");
        std::fs::create_dir(&workspace).expect("ws");
        std::fs::write(parent.path().join("outside.txt"), "x").expect("write");
        let sneaky = workspace.join("..").join("outside.txt");
        let link =
            acp::ContentBlock::ResourceLink(acp::ResourceLink::new("outside.txt", uri_of(&sneaky)));
        assert!(prompt_text(&[link], &workspace).is_err());
    }

    #[test]
    fn a_link_inside_the_workspace_is_named_relative_to_it() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir(workspace.path().join("src")).expect("src");
        let file = workspace.path().join("src").join("my file.rs");
        std::fs::write(&file, "fn main() {}").expect("write");
        let uri = uri_of(&file).replace(' ', "%20");
        let link = acp::ContentBlock::ResourceLink(acp::ResourceLink::new("my file.rs", uri));
        let got = prompt_text(&[link], workspace.path()).expect("inside");
        assert!(got.contains("my file.rs"), "got: {got}");
        assert!(got.starts_with("[attached file: src"), "got: {got}");
    }

    /// A scheme in capitals, a bare path, a network share and an unknown
    /// scheme are all refused; none may reach the agent unexamined.
    #[test]
    fn links_that_dodge_the_file_check_are_refused() {
        let workspace = tempfile::tempdir().expect("workspace");
        for uri in [
            "FILE:///C:/Users/someone/.ssh/id_rsa",
            r"C:\Users\someone\.ssh\id_rsa",
            "file:////attacker.example/share/x",
            "file://attacker.example/share/x",
            "ssh://host/x",
        ] {
            let link = acp::ContentBlock::ResourceLink(acp::ResourceLink::new("x", uri));
            assert!(
                prompt_text(&[link], workspace.path()).is_err(),
                "`{uri}` must be refused"
            );
        }
        let web = acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
            "docs",
            "https://example.com/page",
        ));
        assert!(prompt_text(&[web], workspace.path()).is_ok());
    }

    #[test]
    fn an_image_is_refused_not_dropped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let image = acp::ContentBlock::Image(acp::ImageContent::new("AAAA", "image/png"));
        assert!(prompt_text(&[text("look"), image], dir.path()).is_err());
    }

    #[test]
    fn a_task_list_becomes_a_plan_and_nothing_else_does() {
        let input = r#"{"todos":[{"content":"read","status":"completed","activeForm":"reading"},{"content":"fix","status":"in_progress","activeForm":"fixing"}]}"#;
        let plan = plan_of(TASK_LIST_TOOL, input).expect("a plan");
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[1].status, acp::PlanEntryStatus::InProgress);
        assert!(plan_of("Read", input).is_none());
    }

    /// The client gets a bounded copy; the cut is said, not silent.
    #[test]
    fn a_huge_tool_result_is_cut_and_says_so() {
        let update = tool_result("t1", &"x".repeat(MAX_TOOL_OUTPUT_BYTES + 10), false);
        let encoded = serde_json::to_string(&update).expect("encodes");
        assert!(
            encoded.len() < MAX_TOOL_OUTPUT_BYTES + 1024,
            "{}",
            encoded.len()
        );
        assert!(encoded.contains("10 more bytes"), "the cut must be named");
    }
}
