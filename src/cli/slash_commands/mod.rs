//! Slash commands — /help, /status, /compact, /review, /tools, /skills, /mcp, /model, /clear, /sessions, /resume.
//!
//! # Wired to
//!
//! - **Called by** `crate::cli::run`'s REPL dispatch and
//!   `crate::acp_agent::commands`, which parse a typed line with
//!   [`SlashCommand::parse`] and call the `handle_*` for what it names; by
//!   `crate::cli::input` and `crate::cli::footer`, which complete from
//!   [`COMMANDS`]; and by `crate::custom_commands`, which refuses a custom
//!   command that would shadow a built-in one.
//! - **Split by command family**, one file each, and every handler is
//!   re-exported here so a caller names `slash_commands::handle_*` without
//!   knowing which file holds it:
//!   `parse` (the line to a [`SlashCommand`]), `session` (this conversation:
//!   undo, modes, model, style, image, bug), `status` (`/status`, `/roadmap`),
//!   `memory` (the shared memory), `agents` (`/agent`, `/roles`), `workspace`
//!   (diff, worktree, pull request, links, guards, permissions, doctor) and
//!   `tools` (skills, tools, jobs, env).

mod agents;
mod memory;
mod parse;
mod session;
mod status;
mod tools;
mod workspace;

pub use self::agents::*;
pub use self::memory::*;
pub use self::session::*;
pub use self::status::*;
pub use self::tools::*;
pub use self::workspace::*;

use crate::cli::render;
use crate::outln;

// ---------------------------------------------------------------------------
// SlashCommand
// ---------------------------------------------------------------------------

/// Parsed slash command from user input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Status,
    /// Summarise the conversation now, steered by the optional instructions.
    Compact(Option<String>),
    /// Review the change against a base (default: where the branch forked).
    Review(Option<String>),
    /// A prefilled GitHub issue link for what happened; sends nothing.
    Bug(Option<String>),
    /// List the output styles, or switch to one (`off`: none).
    Style(Option<String>),
    /// The tools incode runs, or with an argument the external tools:
    /// `add <name>…`, `update [name]`, `remove <name>`.
    Tools(Option<String>),
    Skills,
    Mcp,
    Model(Option<String>),
    /// The models the current API route's service offers.
    Models,
    Clear,
    Save(Option<String>),
    Sessions,
    Resume(Option<String>),
    Remember(String),
    Forget {
        id: String,
        confirmed: bool,
    },
    Memories(Option<String>),
    Plan,
    Act,
    /// Put the files from the last change back as they were.
    Undo,
    /// Put the files back as they were this many changed turns ago.
    Rewind(usize),
    /// The sub-agents defined for this workspace and for the operator.
    Agents,
    /// Can this session actually work?
    Doctor,
    /// What the agent is not allowed to write.
    Guards,
    /// The settings' allow / ask / deny rules in force.
    Permissions,
    /// Also work in another directory.
    AddDir(Option<String>),
    /// What else touches this file.
    Links(Option<String>),
    /// Work the ledger until there is a reason to stop.
    Run,
    /// The roadmap, with what a command has actually proven.
    Roadmap,
    /// Show which models, ceiling and check this session resolved to.
    Admin,
    /// List the agent catalogue, or choose the lead (`<id>`) or the
    /// sub-agents (`sub <id>`).
    Agent(Option<String>),
    /// List which models serve each role, or set (`<role> <model|@tier>...`)
    /// or clear (`<role> default`) one.
    Roles(Option<String>),
    Jobs,
    Kill(u32),
    Background(String),
    Worktree(Option<String>),
    /// Push a branch and open (or find) its GitHub pull request.
    PullRequest(Option<String>),
    /// Diff code or workspace against older versions.
    Diff(Option<String>),
    /// Send a picture with the next message.
    Image(String),
    Env,
    /// Tokens and estimated dollars for this session, per model.
    Cost,
    /// Switch between vi and emacs editing.
    Vim,
    Quit,
    /// A REAL command, typed without what it needs. Carries the usage line.
    ///
    /// These used to be [`Self::Unknown`], so typing `/remember` on its own
    /// answered "Unknown command: /remember (missing text). Type /help for
    /// commands." — which is wrong twice over: `/remember` is a known command,
    /// and `/help` does not tell you what it is missing. The operator is one
    /// word away from success and is told to go and look somewhere else.
    Usage(String),
    Unknown(String),
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

/// Every slash command, with what it does.
///
/// One table, because two would drift: `/help` prints it AND the input line
/// completes from it. A command that exists but is not listed here is a command
/// nobody can discover, which is the same as one that does not exist.
pub const COMMANDS: &[(&str, &str)] = &[
    ("/help", "Show this help message"),
    ("/status", "Session info, token usage, model"),
    ("/cost", "Tokens and estimated cost, per model"),
    (
        "/style [name|off]",
        "List the output styles (.incode/styles, ~/.incode/styles), or answer in one from now on",
    ),
    (
        "/bug [what happened]",
        "A prefilled GitHub issue link: version, OS, model, your words scrubbed. Sends nothing",
    ),
    ("/vim", "Toggle vi editing for the input box"),
    (
        "/compact",
        "Summarise older turns now [instructions] (API route; a CLI agent compacts its own)",
    ),
    (
        "/review",
        "Review the change against [base] (default: where the branch forked)",
    ),
    (
        "/undo",
        "Put the files from the last change back as they were",
    ),
    (
        "/rewind <n>",
        "Put the files back as they were n changed turns ago",
    ),
    (
        "/agents",
        "List the sub-agents defined in .incode/agents and ~/.incode/agents",
    ),
    (
        "/doctor",
        "Check that this session can work: agents, guards, specs, ledger, memory",
    ),
    (
        "/guards",
        "Show the paths the agent is not allowed to write",
    ),
    (
        "/permissions",
        "Show the allow / ask / deny rules in force, and where each came from",
    ),
    (
        "/add-dir <path>",
        "Also work in this directory — guards and undo extend to it too",
    ),
    (
        "/links <file>",
        "What else uses what this file defines, and what it depends on",
    ),
    (
        "/tools [add|update|remove <name>]",
        "List incode's tools and the external ones; install, update or remove an external tool",
    ),
    (
        "/skills",
        "List the skills this session loaded from the shared store",
    ),
    (
        "/mcp",
        "Say where MCP servers connect — through incode's toolbox",
    ),
    (
        "/model [name]",
        "Show or switch the model the agent runs on",
    ),
    (
        "/models",
        "List the models the current API route's service offers",
    ),
    ("/clear", "Start a new agent session on the next turn"),
    (
        "/save",
        "Show the agent session id, to reopen this conversation",
    ),
    (
        "/sessions",
        "Show the agent session id for this conversation",
    ),
    ("/resume <id>", "Continue an agent session by id"),
    (
        "/remember <text>",
        "Save a fact to the shared long-term memory",
    ),
    (
        "/forget <ref> [confirm]",
        "Delete one memory — shows it first, deletes only on confirm",
    ),
    ("/memories <query>", "Search the shared memory by meaning"),
    (
        "/plan",
        "Plan mode — the agent proposes changes without making them",
    ),
    (
        "/act",
        "Act mode — the agent applies changes, within the guards",
    ),
    (
        "/image <path>",
        "Send a PNG or JPEG with your next message to the agent",
    ),
    ("/bg <command>", "Run a bash command in the background"),
    ("/jobs", "List background jobs"),
    ("/kill <id>", "Kill a background job"),
    (
        "/worktree [cmd]",
        "Git worktree management (create/list/remove)",
    ),
    (
        "/pr [branch] [--draft]",
        "Push a branch (default: the current one) and open its GitHub pull request",
    ),
    (
        "/pr merge [branch] [--squash|--rebase]",
        "Merge the branch's open pull request, only at its reviewed head with no failing or running check",
    ),
    (
        "/diff [file] [rev]",
        "Show git/file diff against older versions",
    ),
    ("/env", "Show filtered environment variables"),
    (
        "/admin",
        "Show the operator's settings and which layer decided each",
    ),
    (
        "/agent [id | sub [id]]",
        "List every agent, or choose the lead or the sub-agents",
    ),
    (
        "/roles [role <model|@tier>... | role default]",
        "Show which models serve each kind of work, or set or clear one",
    ),
    (
        "/roadmap",
        "What the roadmap says, and which goals a command has proven",
    ),
    (
        "/run",
        "Work the roadmap ledger until it is empty or something needs a person",
    ),
    ("/quit", "Exit incode"),
];

/// The bare command word, without the `<args>` the table shows after it.
#[must_use]
pub fn command_name(entry: &str) -> &str {
    entry.split(' ').next().unwrap_or(entry)
}

/// A real command typed without what it needs: say what it needs.
pub fn handle_usage(usage: &str) {
    render::print_error(&format!("Usage: {usage}"));
}

pub fn handle_help() {
    render::print_header("incode Commands");
    for (cmd, desc) in COMMANDS {
        outln!("  {cmd:<20} {desc}");
    }
    let custom = crate::custom_commands::menu();
    if !custom.is_empty() {
        outln!();
        render::print_header("Custom Commands");
        for (cmd, desc) in custom {
            outln!("  {cmd:<20} {desc}");
        }
    }
    outln!();
}
