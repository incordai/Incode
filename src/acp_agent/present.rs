//! What crosses between the protocol's shapes and incode's own: a prompt's
//! content blocks become the instruction a turn is given, and a turn's agent
//! events become `session/update` payloads.
//!
//! Every payload here is a type from `agent-client-protocol-schema`, built with
//! its own constructors, so the wire shape is the schema's and never a copy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use agent_client_protocol_schema::v1 as acp;
use serde_json::Value;

use crate::cli::agent_turn::Before;
use crate::cli::edits::Asked;

/// The most of one tool's output sent to the client, in bytes.
///
/// A tool result is the agent's own record — a whole file read, a full test
/// log — and the client shows it as a preview beside the call. Sixty-four KiB
/// is several screens of text, far more than any preview draws, while keeping
/// one `tool_call_update` line a size every client reads comfortably. The
/// whole output stays in the agent's transcript; only the copy sent here is
/// cut, and the cut says so.
pub(crate) const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;

/// The most of EACH side of an edit's diff sent to the client, in bytes.
///
/// Half of [`MAX_TOOL_OUTPUT_BYTES`], so one `tool_call_update` carrying a
/// diff is no larger than one carrying a tool's output. A side over it is cut
/// the way output is, and the cut says so.
pub(crate) const MAX_DIFF_TEXT_BYTES: usize = MAX_TOOL_OUTPUT_BYTES / 2;

/// What one prompt carries: the instruction its blocks make, and its
/// pictures.
#[derive(Debug)]
pub(crate) struct Prompted {
    pub(crate) text: String,
    pub(crate) images: Vec<crate::toolbox::Image>,
}

/// The instruction a prompt's content blocks make, in the client's order, and
/// the pictures it carries.
///
/// # What is accepted
///
/// Text, and — as the protocol requires of every agent — resource links. An
/// embedded TEXT resource is accepted too, because `embeddedContext` is
/// advertised, and images, because `image` is: each a PNG or a JPEG inside the
/// toolbox's image bounds (`toolbox::image`), at most
/// [`crate::toolbox::MAX_IMAGES_PER_RESULT`] of them, checked from their own
/// bytes whatever the client called them. Audio and binary attachments are
/// not advertised, so one arriving is refused rather than silently dropped: a
/// prompt that loses part of what the person attached reads to them as a
/// prompt the agent ignored. Whether the agent can take the pictures is
/// decided when they are handed to it.
///
/// # Paths from the client
///
/// A `file:` link is passed on only when it resolves INSIDE the workspace.
/// This agent never reads it — the agent CLI does, with its own tools and its
/// own guards — but naming a path outside the workspace to it would be an
/// invitation to go there, and the client is not trusted to decide that.
pub(crate) fn prompt_parts(
    blocks: &[acp::ContentBlock],
    workspace: &Path,
) -> Result<Prompted, String> {
    let mut parts: Vec<String> = Vec::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            acp::ContentBlock::Text(text) => parts.push(text.text.clone()),
            acp::ContentBlock::ResourceLink(link) => parts.push(linked(link, workspace)?),
            acp::ContentBlock::Resource(embedded) => {
                parts.push(embedded_text(embedded, workspace)?)
            }
            acp::ContentBlock::Image(image) => {
                if images.len() >= crate::toolbox::MAX_IMAGES_PER_RESULT {
                    return Err(format!(
                        "at most {} pictures go with one prompt",
                        crate::toolbox::MAX_IMAGES_PER_RESULT
                    ));
                }
                images.push(picture(image)?);
            }
            acp::ContentBlock::Audio(_) => {
                return Err("audio is not accepted: this agent advertises `audio: false`".into())
            }
            _ => return Err("a kind of content this agent does not accept".into()),
        }
    }
    Ok(Prompted {
        text: parts.join("\n\n"),
        images,
    })
}

/// One picture from the client, inside the bounds or refused with the bound
/// it broke.
fn picture(image: &acp::ImageContent) -> Result<crate::toolbox::Image, String> {
    if !matches!(image.mime_type.as_str(), "image/png" | "image/jpeg") {
        return Err(format!(
            "a picture of type `{}` was sent; only PNG and JPEG are accepted",
            image.mime_type
        ));
    }
    crate::toolbox::Image::from_base64(&image.data)
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
    let title = title_of(name, input);
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

/// A call's title as the client shows it: the tool and what it acts on.
pub(crate) fn title_of(name: &str, input: &str) -> String {
    match crate::cli::panel::target(input) {
        Some(target) => format!("{name} {target}"),
        None => name.to_string(),
    }
}

/// `text`, cut to `limit` bytes with the cut said — never silently.
fn bounded(text: &str, limit: usize, rest_is: &str) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let head = crate::text::head_bytes(text, limit);
    format!(
        "{head}\n[… {} more bytes not shown here; {rest_is}]",
        text.len() - head.len()
    )
}

/// What a tool returned, as an update to its call.
pub(crate) fn tool_result(id: &str, output: &str, is_error: bool) -> acp::ToolCallUpdate {
    let shown = bounded(
        output,
        MAX_TOOL_OUTPUT_BYTES,
        "the agent has the whole output",
    );
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

/// An edit that landed, as an update to its call: the change as a diff, which
/// the client draws red and green and lists under the files changed.
pub(crate) fn tool_diff(id: &str, diff: acp::Diff) -> acp::ToolCallUpdate {
    acp::ToolCallUpdate::new(
        id.to_string(),
        acp::ToolCallUpdateFields::new()
            .status(acp::ToolCallStatus::Completed)
            .content(vec![acp::ToolCallContent::from(diff)]),
    )
}

/// Follows one prompt's edit calls, and turns each one that LANDED into a
/// diff.
///
/// # Where the two sides come from
///
/// The calls and results are incode's own: on the API route and on the
/// bridged CLI route (see [`crate::tool_bridge`]) incode's toolbox ran them and
/// reported them. On a route whose CLI still edits with its own tools they are
/// that CLI's reports. Either way what is in hand is the call, its result, and
/// the snapshot the turn took before it started ([`TurnStart`]):
///
/// - a SPLICE — part of a file replaced — is shown as the call carries it:
///   the text taken out and the text put in. Exactly what changed, with no
///   read of the file that could race the agent.
/// - a WHOLE-FILE write shows the body written against what the file held
///   before: the snapshot's copy for the first edit of that file in the
///   prompt, or what the file held after the edit before it. A file the
///   snapshot shows did not exist has no old text — the protocol's "new
///   file". A file whose before is not known gets no diff at all, only its
///   output, rather than being called new.
///
/// Only a SUCCESSFUL result makes a diff: a write can be refused, and a diff
/// for it would claim a change that never happened. Only a path that is
/// inside the workspace once resolved is named, and it is named absolutely.
pub(crate) struct Edits {
    workspace: PathBuf,
    /// Edit calls seen, keyed by call id, until their result arrives.
    pending: HashMap<String, Asked>,
    /// What each file held after the last edit of it this prompt saw land;
    /// `None` when that is not known. Keyed by the absolute path sent.
    latest: HashMap<PathBuf, Option<String>>,
}

impl Edits {
    pub(crate) fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            pending: HashMap::new(),
            latest: HashMap::new(),
        }
    }

    /// A tool was called; remembered when it is an edit.
    pub(crate) fn called(&mut self, id: &str, input: &str) {
        if let Some(asked) = crate::cli::edits::asked_in(input) {
            self.pending.insert(id.to_string(), asked);
        }
    }

    /// The diff a result makes, or `None` when it shows as ordinary output.
    ///
    /// `before` says what a file held when the work began: the turn's
    /// snapshot for the session's own turn
    /// ([`crate::cli::agent_turn::TurnStart::before`]), nothing
    /// known for a worker, which has none.
    pub(crate) fn landed(
        &mut self,
        id: &str,
        is_error: bool,
        before: &dyn Fn(&Path) -> Before,
    ) -> Option<acp::Diff> {
        let asked = self.pending.remove(id)?;
        if is_error {
            return None;
        }
        match asked {
            Asked::Splice { path, old, new } => {
                let path = self.inside(&path)?;
                // What the file holds now, for a later whole write's "before".
                let now = std::fs::read_to_string(&path).ok();
                self.latest.insert(path.clone(), now);
                Some(acp::Diff::new(path, side(&new)).old_text(side(&old)))
            }
            Asked::Whole { path, body } => {
                let path = self.inside(&path)?;
                let before = match self.latest.get(&path) {
                    Some(Some(text)) => Before::Held(text.clone()),
                    Some(None) => Before::Unknown,
                    None => before(&path),
                };
                let diff = match before {
                    Before::Held(old) => {
                        Some(acp::Diff::new(&path, side(&body)).old_text(side(&old)))
                    }
                    Before::Absent => Some(acp::Diff::new(&path, side(&body))),
                    Before::Unknown => None,
                };
                self.latest.insert(path, Some(body));
                diff
            }
        }
    }

    /// `named` as an absolute path inside the workspace, or `None`.
    ///
    /// Resolved the way prompt links are ([`within`]), so `..` and links
    /// cannot walk out. The workspace part keeps the session's own spelling,
    /// which is how the turn's snapshot names the same file.
    fn inside(&self, named: &str) -> Option<PathBuf> {
        let given = Path::new(named);
        let absolute = if given.is_absolute() {
            given.to_path_buf()
        } else {
            self.workspace.join(given)
        };
        within(&absolute, &self.workspace).map(|relative| self.workspace.join(relative))
    }
}

/// One side of a diff, cut to [`MAX_DIFF_TEXT_BYTES`].
fn side(text: &str) -> String {
    bounded(text, MAX_DIFF_TEXT_BYTES, "the file on disk has all of it")
}

/// What kind of work a tool does, from its name.
///
/// Only a hint for the client's icon. A name that matches nothing is
/// [`acp::ToolKind::Other`], never a guess.
pub(crate) fn tool_kind(name: &str) -> acp::ToolKind {
    match name.to_ascii_lowercase().as_str() {
        "read" | "notebookread" | "read_file" | "bashoutput" | "outline" => acp::ToolKind::Read,
        "edit" | "multiedit" | "write" | "notebookedit" | "apply_patch" | "applypatch"
        | "write_file" | "convertdocument" | "renderpdf" => acp::ToolKind::Edit,
        "bash" | "powershell" | "shell" | "exec_command" | "run_shell_command" | "killshell"
        | "diagnostics" => acp::ToolKind::Execute,
        "grep" | "glob" | "ls" | "search" | "toolsearch" | "watchman_lookup" => {
            acp::ToolKind::Search
        }
        "webfetch" | "websearch" | "web_fetch" | "web_search" => acp::ToolKind::Fetch,
        "todowrite" | "think" => acp::ToolKind::Think,
        _ => acp::ToolKind::Other,
    }
}

/// Follows one prompt's task-list calls — incode's own `TodoWrite` — and
/// turns each one incode ACCEPTED into the plan.
///
/// The list arrives whole each time, which is exactly ACP's rule for a plan:
/// every update replaces the last. It is sent on the call's successful
/// RESULT, not on the call: a list the toolbox refused was never the
/// session's list, and a plan built from it would show one that does not
/// exist.
pub(crate) struct Plans {
    /// Task-list calls seen, keyed by call id, until their result arrives.
    pending: HashMap<String, String>,
}

impl Plans {
    pub(crate) fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }

    /// A tool was called; remembered when it is the task list.
    pub(crate) fn called(&mut self, id: &str, name: &str, input: &str) {
        if name == crate::toolbox::TODO_WRITE {
            self.pending.insert(id.to_string(), input.to_string());
        }
    }

    /// The plan a result makes, or `None` when it makes none.
    pub(crate) fn landed(&mut self, id: &str, is_error: bool) -> Option<acp::Plan> {
        let input = self.pending.remove(id)?;
        if is_error {
            return None;
        }
        plan_of(&input)
    }
}

/// A task list as an ACP plan, read by the same parser incode's `TodoWrite`
/// accepted it with — its priority included. `None` for an input that parser
/// refuses, rather than a plan with invented entries.
pub(crate) fn plan_of(input: &str) -> Option<acp::Plan> {
    use crate::toolbox::{Priority, TodoStatus};
    let todos = crate::toolbox::parse_todos(input).ok()?;
    let entries: Vec<acp::PlanEntry> = todos
        .into_iter()
        .map(|todo| {
            let status = match todo.status {
                TodoStatus::Pending => acp::PlanEntryStatus::Pending,
                TodoStatus::InProgress => acp::PlanEntryStatus::InProgress,
                TodoStatus::Completed => acp::PlanEntryStatus::Completed,
            };
            let priority = match todo.priority {
                Priority::High => acp::PlanEntryPriority::High,
                Priority::Medium => acp::PlanEntryPriority::Medium,
                Priority::Low => acp::PlanEntryPriority::Low,
            };
            acp::PlanEntry::new(todo.content, priority, status)
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
pub(super) mod tests {
    use super::*;

    fn text(t: &str) -> acp::ContentBlock {
        acp::ContentBlock::from(t)
    }

    fn prompt_text(blocks: &[acp::ContentBlock], workspace: &Path) -> Result<String, String> {
        prompt_parts(blocks, workspace).map(|prompted| prompted.text)
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

    /// A PNG of `width` x `height`, as base64.
    pub(crate) fn png_base64(width: u32, height: u32) -> String {
        use base64::Engine as _;
        let mut bytes = Vec::new();
        image::RgbImage::from_pixel(width, height, image::Rgb([200, 30, 30]))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encode a png");
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// **A PICTURE IS TAKEN INSIDE THE BOUNDS, AND REFUSED OUTSIDE THEM —
    /// NEVER DROPPED.** A PNG arrives beside the text; one past the edge
    /// bound, one that is not a picture, one of another type and a fifth one
    /// are each refused with the reason.
    #[test]
    fn a_picture_is_taken_within_the_bounds_and_refused_past_them() {
        let dir = tempfile::tempdir().expect("temp dir");
        let small = acp::ContentBlock::Image(acp::ImageContent::new(png_base64(4, 3), "image/png"));
        let prompted = prompt_parts(&[text("look"), small.clone()], dir.path()).expect("taken");
        assert_eq!(prompted.text, "look");
        assert_eq!(prompted.images.len(), 1);
        assert_eq!(prompted.images[0].width(), 4);

        let wide = acp::ContentBlock::Image(acp::ImageContent::new(
            png_base64(crate::toolbox::MAX_IMAGE_EDGE + 1, 1),
            "image/png",
        ));
        let err = prompt_parts(&[text("look"), wide], dir.path()).expect_err("too wide");
        assert!(err.contains("pixels on a side"), "{err}");

        let junk = acp::ContentBlock::Image(acp::ImageContent::new("AAAA", "image/png"));
        assert!(prompt_parts(&[text("look"), junk], dir.path()).is_err());

        let gif = acp::ContentBlock::Image(acp::ImageContent::new(png_base64(2, 2), "image/gif"));
        let err = prompt_parts(&[text("look"), gif], dir.path()).expect_err("gif");
        assert!(err.contains("only PNG and JPEG"), "{err}");

        let five = vec![small; crate::toolbox::MAX_IMAGES_PER_RESULT + 1];
        let err = prompt_parts(&five, dir.path()).expect_err("too many");
        assert!(err.contains("at most"), "{err}");
    }

    /// **THE PLAN IS INCODE'S TASK LIST**: sent on the successful result of
    /// its own `TodoWrite`, with the priority the model gave; nothing for a
    /// refused call or for another tool.
    #[test]
    fn a_task_list_becomes_a_plan_and_nothing_else_does() {
        let input = r#"{"todos":[{"content":"read","status":"completed","priority":"high","activeForm":"reading"},{"content":"fix","status":"in_progress","priority":"low"}]}"#;
        let mut plans = Plans::new();
        plans.called("t1", crate::toolbox::TODO_WRITE, input);
        let plan = plans.landed("t1", false).expect("a plan");
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[0].priority, acp::PlanEntryPriority::High);
        assert_eq!(plan.entries[1].priority, acp::PlanEntryPriority::Low);
        assert_eq!(plan.entries[1].status, acp::PlanEntryStatus::InProgress);

        plans.called("t2", crate::toolbox::TODO_WRITE, input);
        assert!(
            plans.landed("t2", true).is_none(),
            "a refused list is no plan"
        );
        plans.called("t3", "Read", input);
        assert!(plans.landed("t3", false).is_none(), "not the task list");
    }

    /// Each side of a diff is bounded like tool output, and the cut is said.
    #[test]
    fn a_huge_diff_side_is_cut_and_says_so() {
        let shown = side(&"y".repeat(MAX_DIFF_TEXT_BYTES + 5));
        assert!(shown.len() < MAX_DIFF_TEXT_BYTES + 200, "{}", shown.len());
        assert!(shown.contains("5 more bytes"), "the cut must be named");
        assert_eq!(side("small\n"), "small\n");
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
