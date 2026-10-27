//! The agent file that makes agy a model and nothing else: a custom agent
//! whose only tool of its own is `finish` and whose one MCP server is
//! `incode --tool-server`.
//!
//! Wired to: [`super::AgyBackend`]'s bridged turn (`bridged.rs`), which
//! refreshes the file before every turn, and its `mcp_config`, which reports
//! [`content`].
//!
//! # What was measured (agy 1.4.7, 2026-10-26/27, real headless turns)
//!
//! - No flag switches agy's own tools off, but a custom agent does: with
//!   `tools: [finish]` and one `mcpServers` entry, `agy --agent <name>` gives
//!   the model only `finish`, `call_mcp_tool`, `list_resources`,
//!   `read_resource` and `manage_task`. A shell request was refused by the
//!   model for want of a tool; the same request without `--agent` ran.
//! - Every name under `tools:` must be in agy's registry: `call_mcp_tool`
//!   there is refused at startup (`unknown component: tool "call_mcp_tool"
//!   not found in registry`), because it is added on its own.
//! - The server agy starts inherits agy's environment, which is how the
//!   bridge's address and token reach it. They are never written here.

use std::path::{Path, PathBuf};

/// What every incode agent's name starts with.
pub const AGENT_PREFIX: &str = "incode";

/// The agent's name for the tool server `exe`: the file is `<name>.md`, and
/// a turn runs it with `--agent <name>`.
///
/// ONE PER BINARY, named by a hash of its path (lower-cased on Windows,
/// whose paths ignore case): the folder is machine-wide, and with one shared
/// `incode.md` two incode binaries running at once rewrote it in turn, each
/// pointing the other's next turn at the wrong tool server. The same binary
/// always gets the same name, so its file is still written once.
#[must_use]
pub fn agent_name(exe: &Path) -> String {
    use sha2::Digest as _;
    let path = exe.to_string_lossy();
    let path = if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.into_owned()
    };
    let digest = sha2::Sha256::digest(path.as_bytes());
    let short: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    format!("{AGENT_PREFIX}-{short}")
}

/// What the model is told about its tools. Static: the harness's context for
/// a task changes per turn and goes in the prompt, as on agy's own route, so
/// this file only changes when the incode binary moves.
const BODY: &str = "\
You are the model for incode, a coding harness. You have no tools of your own: \
every tool you have is incode's, served by the MCP server named `incode`.

To read, search, write or edit a file, or to run a command, call `call_mcp_tool` \
with ServerName `incode`, the incode tool's name as ToolName (for example `Read`, \
`Write`, `Edit`, `Bash`) and the tool's input as Arguments; each tool's inputs are \
listed in the task under `<incode-tools>`. incode decides whether each call may run \
and hands you its answer; a refusal is final for that call.

When the work is done, give your answer and call `finish`.
";

/// Where agy looks for the user's own agents: `~/.gemini/config/agents`.
///
/// The home is agy's, not incode's [`crate::admin::home_dir`]: agy is a Go
/// program, whose home on Windows is `USERPROFILE` — a `HOME` that a Unix
/// shell set would point incode somewhere agy never reads.
#[must_use]
pub fn agents_dir() -> Option<PathBuf> {
    gemini_home().map(|g| g.join("config").join("agents"))
}

/// `~/.gemini`, in agy's sense of home (see [`agents_dir`]).
fn gemini_home() -> Option<PathBuf> {
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
    } else {
        std::env::var_os("HOME")
    };
    home.map(|h| PathBuf::from(h).join(".gemini"))
}

/// Where agy keeps a tool result too large to hand the model, for the
/// conversation `id`: `~/.gemini/antigravity-cli/brain/<id>/.system_generated/steps`
/// (measured on 1.4.7: `steps/<n>/output.txt`). The model is told to read it
/// there, and what it holds is incode's own result, so the bridged route
/// lets incode's `Read` open this folder and nothing else of agy's.
///
/// `None` for an id that is not a plain conversation id — letters, digits
/// and `-` — since it arrives on agy's output and a `..` in it would widen
/// the folder.
#[must_use]
pub fn results_dir(id: &str) -> Option<PathBuf> {
    let plain = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !plain {
        return None;
    }
    gemini_home().map(|g| {
        g.join("antigravity-cli")
            .join("brain")
            .join(id)
            .join(".system_generated")
            .join("steps")
    })
}

/// A YAML single-quoted scalar: no escapes are read inside one, so a Windows
/// path's backslashes stay as they are; a quote is doubled.
fn single_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The whole file for an agent whose tool server is `exe`.
///
/// Derived from `exe` alone, so the same binary always writes the same bytes
/// and a turn rewrites nothing. Carries no address and no token: those reach
/// the server through agy's environment, never a file another process can
/// read.
#[must_use]
pub fn content(exe: &Path) -> String {
    format!(
        "---\n\
         name: {name}\n\
         description: incode's model-only agent; every tool is incode's, through its tool bridge.\n\
         tools:\n  - finish\n\
         mcpServers:\n  - name: {key}\n    command: {command}\n    args: [{flag}]\n\
         mainAgent: true\n\
         subagent: false\n\
         ---\n{BODY}",
        name = agent_name(exe),
        key = crate::tool_bridge::CLIENT_KEY,
        command = single_quoted(&exe.to_string_lossy()),
        flag = single_quoted(crate::tool_bridge::SERVER_FLAG),
    )
}

/// Make `dir/<name>.md` ([`agent_name`]) say [`content`] for `exe`, and say
/// whether it had to be written.
///
/// Left alone when it already says it, so turns do not rewrite the file
/// under an agy that is reading it. Otherwise written whole to a file of its
/// own beside it and renamed over it, so agy never reads half of one.
///
/// # Errors
///
/// The directory could not be made, or the file written or renamed.
pub fn ensure(dir: &Path, exe: &Path) -> Result<bool, String> {
    let name = agent_name(exe);
    let path = dir.join(format!("{name}.md"));
    let want = content(exe);
    if std::fs::read_to_string(&path).is_ok_and(|have| have == want) {
        return Ok(false);
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("could not make agy's agents folder {}: {e}", dir.display()))?;
    let temp = dir.join(format!(".{name}.md.{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&temp, &want)
        .map_err(|e| format!("could not write agy's agent file {}: {e}", temp.display()))?;
    if let Err(e) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(format!(
            "could not put agy's agent file in place at {}: {e}",
            path.display()
        ));
    }
    crate::trace!("agent", "agy agent file written: {}", path.display());
    Ok(true)
}
