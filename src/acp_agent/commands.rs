//! incode's slash commands over ACP: which are offered, and running one.
//!
//! A client runs a command by sending `/name input` as ordinary prompt text,
//! so a prompt that parses as a slash command is answered here instead of
//! being handed to the agent. The command runs through the SAME handler the
//! interactive REPL calls, inside [`crate::cli::console::capture`], and what
//! the handler would have printed is the reply.
//!
//! # What is offered, and what is not
//!
//! Only a command that is dispatched here AND makes sense without a terminal
//! is announced. Left out on purpose:
//!
//! `/run` is offered, and run by the caller rather than here: it streams and
//! sends the ledger as the plan (`super::run`).
//!
//! - `/bg`, `/jobs`, `/kill` — background shell jobs. They run a command
//!   outside the agent and outside the session's permission mode, and their
//!   completion is reported by the REPL loop between lines; here nothing
//!   would report it.
//! - `/quit`, `/clear`, `/resume`, `/save`, `/sessions`, `/model`,
//!   `/add-dir` — they act on the terminal session itself; the client owns
//!   the session's lifetime, and `cwd` is the only root a client may name.
//! - `/forget` — deleting a memory is a two-step confirmation typed by a
//!   person at the terminal.
//! - `/help`, `/status`, `/compact`, `/skills`, `/mcp`, `/env`, `/guards`,
//!   `/admin` — terminal information screens, not asked for here. `/tools` is
//!   offered for the external tools alone: what is installed, and `add`,
//!   `update`, `remove`.

use std::path::{Path, PathBuf};

use agent_client_protocol_schema::v1 as acp;

use crate::cli::agent_turn::AgentSession;
use crate::cli::slash_commands::{self, SlashCommand, COMMANDS};
use crate::permissions::PermissionMode;

/// What `/agent` and `/roles` need beyond the session: the directory the
/// operator's settings live in, the agent catalogue, and this session's lead.
pub(crate) struct Settings {
    pub(crate) data_dir: PathBuf,
    pub(crate) catalogue: crate::agent::catalogue::Catalogue,
    pub(crate) lead: crate::agent::DetectedAgent,
}

/// The commands offered, by name, in the order a menu shows them. Each is in
/// [`COMMANDS`], which gives its description and its argument hint.
const OFFERED: [&str; 16] = [
    "undo", "rewind", "plan", "act", "diff", "links", "roadmap", "run", "doctor", "memories",
    "remember", "worktree", "agent", "agents", "roles", "tools",
];

/// The list `available_commands_update` carries.
pub(crate) fn available() -> Vec<acp::AvailableCommand> {
    OFFERED
        .iter()
        .filter_map(|name| {
            let (entry, description) = COMMANDS
                .iter()
                .find(|(entry, _)| slash_commands::command_name(entry).get(1..) == Some(*name))?;
            let command = acp::AvailableCommand::new(*name, *description);
            // The table writes the arguments after the name: `/links <file>`.
            Some(match entry.split_once(' ').map(|(_, hint)| hint.trim()) {
                Some(hint) if !hint.is_empty() => {
                    command.input(acp::AvailableCommandInput::Unstructured(
                        acp::UnstructuredCommandInput::new(hint),
                    ))
                }
                _ => command,
            })
        })
        .collect()
}

/// What running a command did.
pub(crate) struct Ran {
    /// What its handler said, as it would have printed it.
    pub(crate) said: String,
    /// The permission mode it switched the session to, if it did.
    pub(crate) mode: Option<PermissionMode>,
}

/// Run `prompt` when it is a slash command; `None` when it is not one and
/// goes to the agent.
///
/// A prompt that starts with `/` is a command here exactly when it is one in
/// the REPL. One that is not offered is answered with what is, rather than
/// being sent to the agent as a question.
pub(crate) fn run(
    prompt: &str,
    session: &mut AgentSession,
    workspace: &Path,
    settings: &Settings,
) -> Option<Ran> {
    let typed = prompt.trim();
    let command = SlashCommand::parse(typed)?;
    let mut mode = None;
    let ((), said) = crate::cli::console::capture(|| match command {
        SlashCommand::Undo => slash_commands::handle_undo(session, workspace),
        SlashCommand::Rewind(turns) => slash_commands::handle_rewind(session, workspace, turns),
        SlashCommand::Agents => slash_commands::handle_agents(workspace),
        SlashCommand::Plan => {
            slash_commands::handle_plan(session);
            mode = Some(PermissionMode::ReadOnly);
        }
        SlashCommand::Act => {
            slash_commands::handle_act(session);
            mode = Some(PermissionMode::WorkspaceWrite);
        }
        SlashCommand::Diff(arg) => slash_commands::handle_diff(arg.as_deref(), workspace),
        SlashCommand::Links(target) => {
            slash_commands::handle_links(&session.roots(), target.as_deref());
        }
        SlashCommand::Roadmap => slash_commands::handle_roadmap(workspace),
        SlashCommand::Doctor => slash_commands::handle_doctor(workspace),
        SlashCommand::Memories(query) => slash_commands::handle_memories(query.as_deref()),
        SlashCommand::Remember(text) => slash_commands::handle_remember(&text, workspace),
        SlashCommand::Worktree(arg) => slash_commands::handle_worktree(arg.as_deref(), workspace),
        SlashCommand::Agent(arg) => slash_commands::handle_agent(
            arg.as_deref(),
            &settings.catalogue,
            &settings.lead,
            &settings.data_dir,
        ),
        SlashCommand::Roles(arg) => {
            slash_commands::handle_roles(arg.as_deref(), &settings.data_dir)
        }
        // Only the external tools here: the list of incode's own is a
        // terminal screen.
        SlashCommand::Tools(arg) => {
            slash_commands::handle_ext_tools(arg.as_deref(), &settings.data_dir);
        }
        SlashCommand::Usage(usage) if is_offered(&usage) => slash_commands::handle_usage(&usage),
        _ => not_offered(typed),
    });
    Some(Ran {
        said: said.trim_start_matches('\n').to_string(),
        mode,
    })
}

/// Whether the command a line starts with is one offered here.
fn is_offered(line: &str) -> bool {
    let word = line.split_whitespace().next().unwrap_or_default();
    word.strip_prefix('/').is_some_and(|name| {
        OFFERED
            .iter()
            .any(|offered| offered.eq_ignore_ascii_case(name))
    })
}

/// The answer to a command that is not offered here: which ones are.
fn not_offered(typed: &str) {
    let word = typed.split_whitespace().next().unwrap_or(typed);
    crate::cli::render::print_error(&format!(
        "{word} is not a command offered here. Offered: {}",
        OFFERED.map(|name| format!("/{name}")).join(", ")
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every offered command is in the one command table, so the menu and the
    /// REPL's `/help` describe it the same way — and none is silently dropped.
    #[test]
    fn every_offered_command_is_announced_from_the_table() {
        let announced = available();
        let names: Vec<&str> = announced.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names, OFFERED,
            "an offered command is missing from COMMANDS"
        );
        for command in &announced {
            assert!(!command.description.is_empty(), "{}", command.name);
        }
        let links = announced.iter().find(|c| c.name == "links").expect("links");
        let encoded = serde_json::to_value(links).expect("encodes");
        assert_eq!(encoded["input"]["hint"], "<file>", "{encoded}");
        let undo = announced.iter().find(|c| c.name == "undo").expect("undo");
        assert!(
            undo.input.is_none(),
            "a command with no argument has no hint"
        );
    }

    /// The commands left out are left out: running one says so, and says what
    /// is offered, instead of starting it.
    #[test]
    fn a_command_that_is_not_offered_is_not_run() {
        for typed in ["/bg echo hi", "/quit", "/forget abc#1", "/nonsense"] {
            assert!(!is_offered(typed), "{typed}");
        }
        assert!(is_offered("/remember <what to remember>"));
        assert!(is_offered("/Undo"));
        assert!(is_offered("/run"));
    }
}
