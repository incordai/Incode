//! Slash commands — /help, /status, /compact, /tools, /skills, /mcp, /model, /clear, /sessions, /resume.

use crate::cli::render;
use crate::tools::tool_status::ToolStatusCache;
use crate::tools::worktree::WorktreeManager;
use crate::{out, outln};

// ---------------------------------------------------------------------------
// SlashCommand
// ---------------------------------------------------------------------------

/// Parsed slash command from user input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Status,
    Compact,
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

impl SlashCommand {
    /// Parse a line that starts with `/`.
    pub fn parse(line: &str) -> Option<Self> {
        if !line.starts_with('/') {
            return None;
        }

        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        let cmd = parts[0].to_lowercase();
        let arg = parts.get(1).map(|s| s.trim().to_string());

        Some(match cmd.as_str() {
            "/help" | "/h" | "/?" | "/" => Self::Help,
            "/status" | "/s" => Self::Status,
            "/compact" => Self::Compact,
            "/undo" | "/u" => Self::Undo,
            "/rewind" => match arg.as_deref().map(str::parse::<usize>) {
                Some(Ok(turns)) if turns > 0 => Self::Rewind(turns),
                _ => Self::Usage("/rewind <n>  (how many changed turns back, from 1)".to_string()),
            },
            "/agents" => Self::Agents,
            "/doctor" | "/dr" => Self::Doctor,
            "/guards" | "/g" => Self::Guards,
            "/add-dir" => Self::AddDir(arg),
            "/links" | "/l" => Self::Links(arg),
            "/run" => Self::Run,
            "/roadmap" => Self::Roadmap,
            "/admin" => Self::Admin,
            "/agent" => Self::Agent(arg),
            "/roles" => Self::Roles(arg),
            "/tools" | "/t" => Self::Tools(arg.filter(|a| !a.is_empty())),
            "/skills" | "/sk" => Self::Skills,
            "/mcp" => Self::Mcp,
            "/model" | "/m" => Self::Model(arg),
            "/models" => Self::Models,
            "/clear" => Self::Clear,
            "/save" => Self::Save(arg),
            "/sessions" | "/ls" => Self::Sessions,
            "/resume" | "/load" => Self::Resume(arg),
            "/remember" | "/rem" => {
                if let Some(text) = arg {
                    Self::Remember(text)
                } else {
                    Self::Usage("/remember <what to remember>".to_string())
                }
            }
            "/forget" => match arg {
                // `confirm` is typed by a person, deliberately, as a second
                // word — the same two-step the tool and the service enforce.
                Some(rest) => {
                    let (id, tail) = rest.split_once(' ').unwrap_or((rest.as_str(), ""));
                    Self::Forget {
                        id: id.to_string(),
                        confirmed: tail.trim().eq_ignore_ascii_case("confirm"),
                    }
                }
                None => Self::Usage("/forget <conv_id>#<idx> [confirm]".to_string()),
            },
            "/memories" | "/mem" => Self::Memories(arg),
            "/plan" => Self::Plan,
            "/act" => Self::Act,
            "/jobs" => Self::Jobs,
            "/kill" => {
                if let Some(id_str) = arg {
                    if let Ok(id) = id_str.parse::<u32>() {
                        Self::Kill(id)
                    } else {
                        Self::Usage("/kill <id>  (a job number from /jobs)".to_string())
                    }
                } else {
                    Self::Usage("/kill <id>  (a job number from /jobs)".to_string())
                }
            }
            "/bg" => {
                if let Some(cmd) = arg {
                    Self::Background(cmd)
                } else {
                    Self::Usage(
                        "/bg <command>  (a shell command to run in the background)".to_string(),
                    )
                }
            }
            "/worktree" | "/wt" => Self::Worktree(arg),
            "/pr" => Self::PullRequest(arg),
            "/diff" | "/df" => Self::Diff(arg),
            "/image" | "/img" => match arg.filter(|path| !path.is_empty()) {
                Some(path) => Self::Image(path),
                None => Self::Usage("/image <path to a PNG or JPEG>".to_string()),
            },
            "/env" => Self::Env,
            "/quit" | "/exit" | "/q" => Self::Quit,
            _ => Self::Unknown(cmd),
        })
    }
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
    (
        "/compact",
        "Say where context is managed — the agent compacts its own",
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

/// `/undo`: put back the files the last turn changed.
pub fn handle_undo(
    session: &mut crate::cli::agent_turn::AgentSession,
    workspace: &std::path::Path,
) {
    match session.undo(workspace) {
        Ok(outcome) => say_undone(&outcome),
        Err(e) => render::print_error(&e),
    }
}

/// What one undone turn put back, and what it could not.
fn say_undone(outcome: &crate::tools::checkpoint::UndoOutcome) {
    outln!("Undid: {}", outcome.label);
    for path in &outcome.restored {
        outln!("  restored {path}");
    }
    // Never swallowed: a partial undo the user believes was complete is
    // worse than none.
    for failure in &outcome.failed {
        render::print_error(&format!("  could NOT restore {failure}"));
    }
}

/// `/rewind <n>`: put the files back as they were `turns` changed turns ago,
/// undoing the newest first. Stops at the first turn that cannot be undone,
/// and says how far it got.
pub fn handle_rewind(
    session: &mut crate::cli::agent_turn::AgentSession,
    workspace: &std::path::Path,
    turns: usize,
) {
    let (undone, stopped) = session.rewind(workspace, turns);
    for outcome in &undone {
        say_undone(outcome);
    }
    match stopped {
        Some(why) => render::print_error(&format!(
            "Rewound {} of {turns} turn(s), then stopped: {why}",
            undone.len()
        )),
        None => outln!("Rewound {turns} turn(s)."),
    }
}

/// `/agents`: the sub-agents defined for this workspace and for the
/// operator, and every definition file left out with why.
pub fn handle_agents(workspace: &std::path::Path) {
    out!("{}", crate::subagents::load(workspace).report());
}

/// The most bytes a picture file `/image` reads may have. Past the image
/// bounds it is shrunk to fit (`toolbox::image`); this bounds what is read
/// before that is tried.
const MAX_IMAGE_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// `/image <path>`: a PNG or JPEG sent with the next message, made to fit the
/// image bounds as a screenshot is. One picture: a second `/image` before
/// the message replaces the first. An agent that cannot take pictures says
/// so here, before anything is sent.
pub fn handle_image(
    session: &mut crate::cli::agent_turn::AgentSession,
    workspace: &std::path::Path,
    path: &str,
) {
    let path = path.trim().trim_matches('"');
    let file = workspace.join(path);
    let read = std::fs::metadata(&file)
        .map_err(|e| format!("`{path}` could not be read: {e}"))
        .and_then(|meta| {
            if meta.len() > MAX_IMAGE_FILE_BYTES {
                Err(format!(
                    "`{path}` is {} bytes, past {MAX_IMAGE_FILE_BYTES}",
                    meta.len()
                ))
            } else {
                std::fs::read(&file).map_err(|e| format!("`{path}` could not be read: {e}"))
            }
        })
        .and_then(|bytes| {
            crate::toolbox::Image::fitted(bytes).map_err(|e| format!("`{path}`: {e}"))
        });
    let (picture, note) = match read {
        Ok(fitted) => fitted,
        Err(why) => return render::print_error(&why),
    };
    let shown = picture.placeholder();
    match session.attach_images(vec![picture]) {
        Ok(()) => {
            outln!("Attached {shown}: it goes with your next message.");
            if let Some(note) = note {
                outln!("  ({note})");
            }
        }
        Err(why) => render::print_error(&why),
    }
}

/// `/plan`: the agent proposes changes without making them.
///
/// The agent enforces this itself, for its own tools — which is the only place
/// it can be enforced now.
pub fn handle_plan(session: &mut crate::cli::agent_turn::AgentSession) {
    session.set_permission_mode(crate::cli::agent_turn::PLAN_MODE);
    render::print_info("Plan mode: the agent will propose changes without making them.");
}

/// `/act`: the agent applies changes, within the guards.
pub fn handle_act(session: &mut crate::cli::agent_turn::AgentSession) {
    session.set_permission_mode(crate::cli::agent_turn::ACT_MODE);
    render::print_info("Act mode: the agent applies changes, within the guards.");
}

pub fn handle_help() {
    render::print_header("incode Commands");
    for (cmd, desc) in COMMANDS {
        outln!("  {cmd:<20} {desc}");
    }
    outln!();
}

/// The skills this session actually loaded, from the store every agent shares.
///
/// # Why this exists beside the feature-gated one below
///
/// The gated handler reads an external crate that is not present on this
/// machine, so the branch that actually ships was a message saying skills were
/// "not compiled in". That was false in the way that matters: [`crate::skills`]
/// loads them from the store and puts the index into EVERY prompt
/// (`cli/mod.rs`), so the command denied something the session was already
/// doing — and a command that contradicts the harness teaches the operator to
/// distrust the rest of them.
///
/// Bodies are not printed, for the same reason they are not put in the prompt:
/// the index is what decides whether a body is wanted.
pub fn handle_skills_from_store(root: &std::path::Path) {
    render::print_header("Skills");
    let (set, source) = crate::skills::load(root);
    for line in skills_lines(&set, source) {
        outln!("{line}");
    }
    outln!();
}

/// What `/skills` prints, as data.
///
/// Returned rather than printed for the same reason
/// [`crate::session::summary_lines`] is: the command's whole job is *what it
/// says*, and a handler that writes straight to the terminal can only be
/// checked by a person looking at a screen. That is exactly how this command
/// came to deny that skills existed while the same session was loading them —
/// nothing executed the sentence, so nothing contradicted it.
#[must_use]
pub fn skills_lines(set: &crate::skills::Skillset, source: crate::skills::Source) -> Vec<String> {
    let mut out = Vec::new();

    // SAY WHERE IT CAME FROM. A cached list that reads exactly like a live one
    // turns an unreachable store into a silent wrong answer.
    match source {
        crate::skills::Source::Store => {}
        crate::skills::Source::Cache(age) => out.push(format!(
            "  The skill store did not answer — this is what it last said, {age}s ago."
        )),
        crate::skills::Source::Nothing => {
            out.push(
                "  The skill store did not answer and nothing was kept. This session is \
                 running without skills."
                    .to_string(),
            );
            return out;
        }
    }

    if set.is_empty() {
        out.push("  The store holds no skills for this machine.".to_string());
        return out;
    }

    if !set.managed.is_empty() {
        out.push("  The operator's own, with their text:".to_string());
        for skill in &set.managed {
            out.push(format!("    {}  —  {}", skill.name, skill.description));
        }
    }
    if !set.discovered.is_empty() {
        if !set.managed.is_empty() {
            out.push(String::new());
        }
        out.push("  Installed folders, read on request:".to_string());
        for skill in &set.discovered {
            out.push(format!("    {}  —  {}", skill.name, skill.description));
        }
    }
    out.push(String::new());
    out.push(format!(
        "  {} skill(s): {} written by you, {} installed. The index rides every prompt;",
        set.managed.len() + set.discovered.len(),
        set.managed.len(),
        set.discovered.len()
    ));
    out.push("  a body is read only when the agent asks for that skill by name.".to_string());
    out
}

/// Diff code or workspace against older versions.
pub fn handle_diff(arg: Option<&str>, workspace: &std::path::Path) {
    match arg {
        Some(target) => {
            let parts: Vec<&str> = target.split_whitespace().collect();
            if parts.len() >= 2 {
                let p1 = workspace.join(parts[0]);
                let p2 = workspace.join(parts[1]);
                if p1.is_file() && p2.is_file() {
                    match crate::tools::diff::diff_files(&p1, &p2) {
                        Ok(diff) if diff.is_empty() => render::print_info("No differences found."),
                        Ok(diff) => {
                            let (added, removed) = crate::tools::diff::diff_stats(&diff);
                            render::print_header(&format!(
                                "Diff: {} -> {} (+{} -{})",
                                parts[0], parts[1], added, removed
                            ));
                            outln!("{diff}");
                        }
                        Err(e) => render::print_error(&e),
                    }
                    return;
                }
            }
            let file = parts[0];
            let rev = parts.get(1).copied();
            match crate::tools::diff::git_diff_file(workspace, file, rev) {
                Ok(diff) if diff.is_empty() => {
                    render::print_info(&format!("No changes detected for {file}."));
                }
                Ok(diff) => {
                    let (added, removed) = crate::tools::diff::diff_stats(&diff);
                    render::print_header(&format!("Diff: {file} (+{} -{})", added, removed));
                    outln!("{diff}");
                }
                Err(e) => render::print_error(&e),
            }
        }
        None => match crate::tools::diff::git_diff_workspace(workspace, None) {
            Ok(diff) if diff.is_empty() => {
                render::print_info("Working tree clean — no diff against HEAD.");
            }
            Ok(diff) => {
                let (added, removed) = crate::tools::diff::diff_stats(&diff);
                render::print_header(&format!("Workspace Diff (+{} -{})", added, removed));
                outln!("{diff}");
            }
            Err(e) => render::print_error(&format!("Diff failed: {e}")),
        },
    }
}

// ---------------------------------------------------------------------------
// Memory command handlers
// ---------------------------------------------------------------------------

/// Save a quick memory (defaults to static/high).
/// What this session is and what it has spent.
///
/// Reports the AGENT's numbers, because the agent is what ran — a count of
/// messages this harness held would be a count of nothing.
pub fn handle_agent_status(
    model: &str,
    workspace: &std::path::Path,
    permission: &str,
    session_id: Option<&str>,
    turns: usize,
    usage: &crate::runtime::usage::TokenUsage,
    quota: Option<(&str, i64)>,
) {
    render::print_header("Session");
    outln!("  Model:            {model}");
    outln!("  Workspace:        {}", workspace.display());
    outln!("  Permission:       {permission}");
    outln!(
        "  Agent session:    {}",
        session_id.unwrap_or("not opened yet")
    );
    outln!("  Turns:            {turns}");
    outln!("  {}", usage.summary_line(None));
    // The number that decides whether work MOVES to another agent. Shown so a
    // failover is something an operator can see coming rather than discover.
    if let Some((status, resets_at)) = quota {
        let room = if crate::agent::is_limit_status(status) {
            "OUT OF ROOM — work will move to a standby agent"
        } else {
            "room to work"
        };
        outln!("  Quota:            {status} ({room})");
        if resets_at > 0 {
            outln!("  Resets at:        {resets_at} (unix time)");
        }
    }

    // The work, not just the session.
    //
    // The ledger is the answer to "how is it going", and it used to be shown
    // only once — after a run ENDED. A person watching a loop had no way to ask
    // mid-flight, which is exactly when they want to know.
    //
    // Read from disk rather than from the runner: the runner owns it during a
    // run and this is the same file it saves after every task, so what is shown
    // is what was actually recorded.
    render::print_header("Work");
    match crate::ledger::Ledger::load(workspace) {
        Ok(ledger) => outln!("{}", ledger.progress()),
        // A ledger that cannot be read is said out loud rather than shown as
        // "no work planned" — those are different facts.
        Err(e) => render::print_error(&format!("the ledger could not be read: {e}")),
    }
}

/// The roadmap, with what a command has actually proven.
///
/// # Wired to
///
/// - **Called by** `crate::cli::run`'s dispatch for [`SlashCommand::Roadmap`],
///   and after a run ends.
/// - **Calls** [`crate::brief::check`] for the items and
///   [`crate::goal::standings`] for what is proven.
/// - **Sibling to** [`handle_agent_status`], which shows the LEDGER — the tasks.
///   This shows the ROADMAP — the goals those tasks were meant to reach. They
///   disagree exactly when a plan was wrong, which is the case worth seeing.
///
/// Running the checks costs a command per item, which is why it is a command the
/// owner types rather than something on every prompt.
pub fn handle_roadmap(workspace: &std::path::Path) {
    render::print_header("Roadmap");
    // FOUR OUTCOMES, FOUR MESSAGES. A first cut printed "no brief covers this
    // project" for all of them, and the very first live run hit it with the
    // service mid-restart: the board reported an absent brief for a project
    // whose brief was fine. That is the same conflation of "could not ask" with
    // "the answer is nothing" that cost this codebase a day elsewhere, and
    // `Gate` already draws the distinction — the handler was throwing it away.
    let brief = match crate::brief::check(workspace) {
        crate::brief::Gate::Ready(brief) => brief,
        crate::brief::Gate::Unreachable { url, reason } => {
            render::print_error(&format!(
                "The roadmap could not be read because the memory service did not \
                 answer ({url}): {reason}\nThis is NOT an empty roadmap — start \
                 the service and ask again."
            ));
            return;
        }
        crate::brief::Gate::Incomplete { project, missing } => {
            render::print_error(&format!(
                "The brief for {project} is incomplete, so its roadmap is not \
                 settled yet. Missing: {}",
                missing
                    .iter()
                    .map(|a| crate::brief::Artifact::field(*a))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            return;
        }
        crate::brief::Gate::NoBrief { workspace } => {
            render::print_error(&format!(
                "No brief covers {} yet, so there is no roadmap to show. The \
                 goals live in the brief.",
                workspace.display()
            ));
            return;
        }
    };

    let items: Vec<(String, String)> = brief
        .roadmap
        .iter()
        .map(|item| (item.id.clone(), item.title.clone()))
        .collect();
    let checks = crate::goal::checks_from_docs(workspace);
    // The ledger is the other half of the cross-check: a goal may not advance on
    // its own command alone while the tasks planned for it are unfinished. An
    // unreadable ledger is an EMPTY one here, which is the conservative reading
    // — no task claims to be done, so nothing is ticked on its say-so.
    let ledger = crate::ledger::Ledger::load(workspace).unwrap_or_default();
    let standings = crate::goal::standings(&items, &checks, workspace, &ledger.tasks);
    out!("{}", crate::goal::board(&items, &standings));

    // TELL THE BELIEF WHAT THE COMMANDS PROVED.
    //
    // Until this existed the roadmap status was written once and never touched
    // by execution, so the proxy answered "todo" for goals that had been passing
    // for days — and the proxy is what reaches the owner's phone. Verified on
    // the live service: all three ball-game items read `status: 'todo'` while
    // `/roadmap` proved all three passed.
    //
    // Only a REACHED standing ticks an item, and a standing is only reached when
    // a command exited zero. Nothing here can move an item back — see
    // `brief::mark_done_in_config` for why that asymmetry is deliberate.
    let proven: Vec<String> = items
        .iter()
        .zip(standings.iter())
        .filter(|(_, standing)| standing.is_reached())
        .map(|((id, _), _)| id.clone())
        .collect();
    match crate::brief::mark_roadmap_done(&brief.project, &proven) {
        Ok(0) => {}
        Ok(n) => render::print_info(&format!(
            "{n} roadmap item(s) marked done in the brief — the proxy and \
             anything reading it now agree with the checks."
        )),
        // A belief that cannot be updated is worth saying out loud: the board
        // above is right and the API is stale, which is exactly the drift this
        // is meant to end.
        Err(e) => render::print_error(&format!(
            "The board is correct but the brief could not be updated, so the \
             API still shows the old status: {e}"
        )),
    }

    // A check that matches no item never runs and never complains. Named here
    // because it is indistinguishable from "covered" everywhere else.
    let orphans = crate::goal::orphans(&items, &checks);
    if !orphans.is_empty() {
        outln!(
            "\n  {} check(s) in {} name an item this roadmap does not have, so \
             they never run:",
            orphans.len(),
            crate::goal::VERIFICATION_PATH
        );
        for orphan in orphans {
            outln!("    - {} ({})", orphan.title, orphan.roadmap_id);
        }
    }

    // Said once, here, rather than repeated per line: an owner who has written
    // no checks needs to know HOW, not that thirty items are unproven.
    if checks.is_empty() && !items.is_empty() {
        outln!(
            "  Nothing here can be settled by a machine yet. Give an item a \
             command in {} and it will be proven instead of assumed:",
            crate::goal::VERIFICATION_PATH
        );
        outln!("    - **{}** — `the command that proves it`", items[0].0);
    }
}

/// Where the tools live now.
///
/// The agent runs its own loop with its own tools, so listing this harness's
/// would name things nothing calls. What the harness still does is said plainly
/// instead of implied by an empty list.
/// `/tools` and its verbs for the external tools configured in `admin.json`
/// under `data_dir`: what is installed, or install, update or remove one.
pub fn handle_ext_tools(arg: Option<&str>, data_dir: &std::path::Path) {
    render::print_header("External tools");
    for line in crate::ext_tools::command(arg.unwrap_or_default(), data_dir) {
        outln!("  {line}");
    }
}

/// `/tools`: the tools incode runs itself for this agent, or — for an agent
/// whose CLI still uses its own tools — that plainly.
pub fn handle_delegated_tools(agent: &str, offered: &[String]) {
    render::print_header("Tools");
    if offered.is_empty() {
        outln!("  {agent} still runs its own tools on this route; incode checks the");
        outln!("  result afterwards (protected paths, the check, the ledger).");
    } else {
        outln!("  incode runs every tool {agent} calls, through its own gate");
        outln!("  (schema, guards, the memory engine, the permission mode):");
        for chunk in offered.chunks(6) {
            outln!("    {}", chunk.join(", "));
        }
    }
    outln!();
    outln!("  Installed on this machine:");
}

/// Whether this session can actually do the work.
pub fn handle_doctor(workspace: &std::path::Path) {
    render::print_header("Doctor");
    let checks = crate::doctor::run(workspace);
    out!("{}", crate::doctor::report(&checks));
    match crate::doctor::worst(&checks) {
        crate::doctor::Health::Ok => render::print_info("Everything needed is in place."),
        crate::doctor::Health::Warn => {
            render::print_info("Workable, with the notes above.");
        }
        crate::doctor::Health::Fail => {
            render::print_error("Something above will stop work. Fix it before running.");
        }
    }
}

/// What the agent may not write.
///
/// A guard nobody can see is a guard nobody can check — and this one decides
/// whether the thing being judged can edit its own exam, so it is worth being
/// able to read without opening a JSON file.
pub fn handle_guards(workspace: &std::path::Path) {
    render::print_header("Protected paths");
    match crate::guards::Guards::load(workspace) {
        Ok(guards) => {
            let paths = guards.effective();
            if paths.is_empty() {
                render::print_error(
                    "Nothing is protected — every default has been switched off in .incode/guards.json.",
                );
                return;
            }
            for path in &paths {
                outln!("  {path}");
            }
            outln!();
            outln!(
                "  {} pattern(s). The agent enforces these itself as deny rules, and a",
                paths.len()
            );
            outln!("  sentinel checks the files again after every delegated task.");
            if !guards.allow_writes_to.is_empty() {
                outln!();
                outln!("  Switched off by you:");
                for exempt in &guards.allow_writes_to {
                    outln!("    {exempt}");
                }
            }
        }
        Err(e) => render::print_error(&format!("the policy could not be read: {e}")),
    }
}

/// What else touches this file.
///
/// The question an agent should ask before changing a function and does not: a
/// backend edit that never reaches its API layer, its frontend and its tests
/// leaves three files written against something that no longer exists.
pub fn handle_links(roots: &[std::path::PathBuf], target: Option<&str>) {
    let map = crate::links::LinkMap::build(roots);

    let Some(target) = target else {
        render::print_header("Links");
        outln!("  {} source file(s) indexed.", map.len());
        if map.is_truncated() {
            render::print_error(
                "The index stopped early — this tree is larger than the walk limit, so what is shown is partial.",
            );
        }
        outln!("  Name a file to see what touches it, e.g. `/links src/thing.rs`.");
        return;
    };

    // Matched by suffix so a person can type what they would say, rather than
    // the absolute path the index happens to store.
    let wanted = target.replace(char::from(92), "/");
    let Some(file) = map.files_matching(&wanted).into_iter().next() else {
        render::print_error(&format!(
            "No indexed source file matches `{target}`. It may be outside the workspace, a type this index does not read, or generated."
        ));
        return;
    };

    render::print_header(&format!("Links — {file}"));

    let dependents = map.dependents(&file);
    if dependents.is_empty() {
        outln!("  Nothing indexed uses what this file defines.");
    } else {
        outln!("  Used by (change this file, check these):");
        for (dependent, symbol) in &dependents {
            outln!("    {dependent}  (uses `{symbol}`)");
        }
    }

    let dependencies = map.dependencies(&file);
    if !dependencies.is_empty() {
        outln!();
        outln!("  Depends on:");
        for (dependency, symbol) in dependencies.iter().take(20) {
            outln!("    {dependency}  (defines `{symbol}`)");
        }
        if dependencies.len() > 20 {
            outln!("    …and {} more", dependencies.len() - 20);
        }
    }

    outln!();
    outln!("  A text index, not a compiler: it can link files that merely share a");
    outln!("  word, and miss a call made through a string. A place to look.");
}

pub fn handle_remember(text: &str, workspace: &std::path::Path) {
    if text.trim().is_empty() {
        render::print_error("Usage: /remember <what to remember>");
        return;
    }
    // Saved against the workspace, so a fact about this project is recallable
    // as a fact about this project rather than as a loose note.
    match crate::knowledge::remember(text, "fact", &workspace.display().to_string()) {
        Ok(id) => render::print_info(&format!("Saved to the shared memory as {id}.")),
        Err(e) => render::print_error(&format!("NOT saved: {e}")),
    }
}

/// Delete one stored message. Two steps, and the first one shows what would go.
///
/// The identifier is `conv_id#idx`, which is what `/memories` prints beside every
/// result — so what is shown is exactly what can be typed back.
pub fn handle_forget(id: &str, confirmed: bool) {
    let Some((conv_id, idx)) = parse_memory_ref(id) else {
        render::print_error(
            "Usage: /forget <conv_id>#<idx>  (the reference /memories prints beside each result)",
        );
        return;
    };
    match crate::knowledge::forget(&conv_id, idx, confirmed) {
        Ok(crate::knowledge::Forget::NeedsConfirmation { current }) => {
            render::print_header("This would be deleted");
            outln!(
                "{current}
"
            );
            render::print_info(&format!("Run `/forget {id} confirm` to delete it."));
        }
        Ok(crate::knowledge::Forget::Deleted { conv_id, idx }) => {
            render::print_info(&format!("Deleted {conv_id}#{idx}."));
        }
        Err(e) => render::print_error(&format!("Not deleted: {e}")),
    }
}

/// Split `conv_id#idx` into its parts.
///
/// `rsplit_once`, not `split_once`: a conversation id may itself contain `#`,
/// and the index is always the last field.
#[must_use]
pub fn parse_memory_ref(id: &str) -> Option<(String, u64)> {
    let (conv_id, idx) = id.trim().rsplit_once('#')?;
    if conv_id.is_empty() {
        return None;
    }
    Some((conv_id.to_string(), idx.trim().parse().ok()?))
}

/// Search the shared memory.
pub fn handle_memories(query: Option<&str>) {
    render::print_header("Memories");

    let Some(q) = query.map(str::trim).filter(|q| !q.is_empty()) else {
        // No listing endpoint, and inventing one by searching for the empty
        // string would return whatever the index happened to rank first while
        // looking like "everything you have".
        render::print_info("Usage: /memories <what you are looking for>");
        render::print_info("The shared memory is searched by meaning, so ask a question.");
        outln!();
        return;
    };

    match crate::knowledge::search(q, 10) {
        Ok(matches) if matches.is_empty() => render::print_info("Nothing matches that."),
        Ok(matches) => {
            for m in &matches {
                outln!("  {}#{} [{}]", m.conv_id, m.idx, m.role);
                outln!("    {}", crate::text::head_bytes(&m.content, 300));
            }
            outln!();
            render::print_info("Use /forget <conv_id>#<idx> to remove one.");
        }
        Err(e) => render::print_error(&format!("Search failed: {e}")),
    }
    outln!();
}

// ---------------------------------------------------------------------------
// Code graph command handler
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Background job handlers
// ---------------------------------------------------------------------------

pub fn handle_jobs(job_mgr: &mut crate::tools::bash_jobs::BashJobManager) {
    render::print_header("Background Jobs");
    let jobs = job_mgr.list();
    if jobs.is_empty() {
        render::print_info("No background jobs. Use /bg <command> to start one.");
    } else {
        for (id, cmd, running, elapsed) in &jobs {
            let status = if *running { "running" } else { "done" };
            let cmd_preview = if cmd.len() > 60 {
                format!("{}...", crate::text::head_bytes(cmd, 60))
            } else {
                cmd.to_string()
            };
            outln!("  #{id:<4} [{status}] {elapsed}s  {cmd_preview}");
        }
        outln!();
        outln!(
            "  {} job(s), {} running",
            jobs.len(),
            job_mgr.running_count()
        );
    }
    outln!();
}

// ---------------------------------------------------------------------------
// Session command handlers
// ---------------------------------------------------------------------------

/// Report the outcome of a `/model` request.
///
/// `switched` is what the client actually did. This used to print "switching
/// to X" unconditionally while changing nothing, so every later turn quietly
/// went to the original model at the original price.
pub fn handle_model(current: &str, new_model: Option<&str>, switched: bool) {
    match new_model {
        Some(name) if switched => {
            render::print_info(&format!("Model switched to: {name}"));
        }
        Some(name) => {
            render::print_error(&format!(
                "Cannot switch to '{name}' — this session's provider does not serve it. \
                 Still using {current}."
            ));
        }
        None => {
            render::print_info(&format!("Current model: {current}"));
        }
    }
}

/// `/agent`: list the catalogue, or choose the lead or the sub-agents.
///
/// A choice is saved to admin.json and applies from the next `/run` and the
/// next session. This conversation's agent is not swapped mid-flight: its
/// transcript lives inside the agent that holds it.
pub fn handle_agent(
    arg: Option<&str>,
    catalogue: &crate::agent::catalogue::Catalogue,
    lead: &crate::agent::DetectedAgent,
    data_dir: &std::path::Path,
) {
    match agent_command(arg, catalogue, lead, data_dir) {
        Ok(text) => outln!("{}", text.trim_end()),
        Err(why) => render::print_error(&why),
    }
}

/// What `/agent` does with its argument, and what it says.
///
/// - nothing: the catalogue, the lead, and the sub-agents with where they
///   came from;
/// - `<id>`: saves the lead;
/// - `sub <id>`: saves the sub-agents; `sub` or `sub same` clears that back
///   to the lead's default.
///
/// An id must be in the catalogue and usable, or nothing is written. The
/// settings file is READ before it is written, and a malformed one is refused
/// rather than replaced by a file holding only this one choice.
fn agent_command(
    arg: Option<&str>,
    catalogue: &crate::agent::catalogue::Catalogue,
    lead: &crate::agent::DetectedAgent,
    data_dir: &std::path::Path,
) -> Result<String, String> {
    let mut admin = crate::admin::AdminConfig::load(data_dir)?;
    let Some(arg) = arg.map(str::trim).filter(|a| !a.is_empty()) else {
        let sub = catalogue.sub_agent(lead, admin.sub_agent.as_deref());
        return Ok(catalogue.report(lead, &sub, admin.resolve_agent().value.as_deref()));
    };
    let (word, rest) = arg
        .split_once(char::is_whitespace)
        .map_or((arg, ""), |(word, rest)| (word, rest.trim()));
    const APPLIES: &str = "Applies from the next /run and the next session.";

    if word.eq_ignore_ascii_case("sub") {
        if rest.is_empty() || rest.eq_ignore_ascii_case("same") {
            admin.sub_agent = None;
            admin.save(data_dir)?;
            return Ok(format!(
                "Sub-agents follow the lead's default again. {APPLIES}"
            ));
        }
        let chosen = catalogue.route_for(rest)?;
        admin.sub_agent = Some(chosen.id.clone());
        admin.save(data_dir)?;
        return Ok(format!(
            "Sub-agents: {} (your choice), saved to admin.json. {APPLIES}",
            chosen.id
        ));
    }
    if !rest.is_empty() {
        return Err("/agent [id | sub [id]]  (one id at a time)".to_string());
    }

    let chosen = catalogue.route_for(word)?;
    admin.agent = Some(chosen.id.clone());
    admin.save(data_dir)?;
    let mut said = if chosen.id == lead.id {
        format!(
            "Lead: {} saved to admin.json — already this session's lead.",
            chosen.id
        )
    } else {
        format!(
            "Lead: {} saved to admin.json. {APPLIES} This conversation stays on {}.",
            chosen.id, lead.id
        )
    };
    if crate::admin::AdminConfig::default().resolve_agent().source
        == crate::admin::Source::Environment
    {
        said.push_str(&format!(
            "\n{} is set in this shell and overrides the saved choice until it is unset.",
            crate::admin::AGENT_ENV
        ));
    }
    Ok(said)
}

/// `/roles`: which models serve each kind of delegated work, or set one.
pub fn handle_roles(arg: Option<&str>, data_dir: &std::path::Path) {
    match roles_command(arg, data_dir) {
        Ok(text) => outln!("{}", text.trim_end()),
        Err(why) => render::print_error(&why),
    }
}

/// What `/roles` does with its argument, and what it says.
///
/// - nothing: every role, what it resolves to, and which layer decided it;
/// - `<role> <model|@tier>...`: saves that role's candidates, in order;
/// - `<role> default` (or `clear`): removes the role from the file, so it runs
///   on the session model unless the shell says otherwise.
///
/// The role must be a known one and every `@tier` must exist, or nothing is
/// written. The settings file is READ before it is written, and a malformed
/// one is refused rather than replaced by a file holding only this role.
fn roles_command(arg: Option<&str>, data_dir: &std::path::Path) -> Result<String, String> {
    use crate::roles::{KNOWN_ROLES, MAX_CANDIDATES, TIER_PREFIX};
    const USAGE: &str = "/roles [role <model|@tier>... | role default]";
    const APPLIES: &str = "Applies from the next /run and the next session.";

    let mut admin = crate::admin::AdminConfig::load(data_dir)?;
    let Some(arg) = arg.map(str::trim).filter(|a| !a.is_empty()) else {
        return Ok(roles_listing(&admin));
    };
    let (typed_role, rest) = arg
        .split_once(char::is_whitespace)
        .map_or((arg, ""), |(word, rest)| (word, rest.trim()));
    let role = typed_role.to_ascii_lowercase();
    if !KNOWN_ROLES.contains(&role.as_str()) {
        return Err(format!(
            "'{typed_role}' is not a role. The roles are: {}",
            KNOWN_ROLES.join(", ")
        ));
    }
    if rest.is_empty() {
        return Err(format!("{USAGE}  (name the models for {role})"));
    }

    // Spaces and commas both separate, so `a b` and `a, b` mean the same.
    let joined = rest.split_whitespace().collect::<Vec<_>>().join(",");
    let given = joined.split(',').filter(|m| !m.trim().is_empty()).count();
    if given > MAX_CANDIDATES {
        return Err(format!(
            "{role} lists {given} candidates; a role may hold at most {MAX_CANDIDATES}."
        ));
    }
    let candidates = crate::roles::parse_candidates(&joined);

    let clears =
        |word: &str| word.eq_ignore_ascii_case("default") || word.eq_ignore_ascii_case("clear");
    let overridden = crate::admin::AdminConfig::default().role_source(&role)
        == crate::admin::Source::Environment;
    let env_note = |said: &mut String| {
        if overridden {
            said.push_str(&format!(
                "\n{}{} is set in this shell and overrides the saved choice until it is unset.",
                crate::roles::ROLE_ENV_PREFIX,
                role.to_ascii_uppercase()
            ));
        }
    };

    if candidates.iter().any(|c| clears(c)) {
        if candidates.len() > 1 {
            return Err(format!(
                "'default' clears {role} and takes nothing after it. {USAGE}"
            ));
        }
        admin.roles.remove(&role);
        admin.save(data_dir)?;
        let mut said =
            format!("{role}: removed from admin.json — it runs on the session model. {APPLIES}");
        env_note(&mut said);
        return Ok(said);
    }

    let unknown: Vec<&str> = candidates
        .iter()
        .map(String::as_str)
        .filter(|c| {
            c.strip_prefix(TIER_PREFIX)
                .is_some_and(|name| !admin.tiers.contains_key(name.trim()))
        })
        .collect();
    if !unknown.is_empty() {
        let defined = if admin.tiers.is_empty() {
            "no tiers are defined in admin.json".to_string()
        } else {
            format!(
                "the tiers are: {}",
                admin
                    .tiers
                    .keys()
                    .map(|t| format!("{TIER_PREFIX}{t}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return Err(format!(
            "{} names no tier; {defined}. Nothing was saved.",
            unknown.join(", ")
        ));
    }

    admin.roles.insert(role.clone(), candidates.clone());
    admin.save(data_dir)?;
    let mut said = format!(
        "{role}: {} (your choice), saved to admin.json. {APPLIES}",
        candidates.join(" > ")
    );
    env_note(&mut said);
    Ok(said)
}

/// One line per role, in the order the harness knows them.
///
/// Built from the same resolution the run uses (`resolve_roles`,
/// `role_source`, `unknown_tiers`), so this cannot say one thing while the
/// run does another.
fn roles_listing(admin: &crate::admin::AdminConfig) -> String {
    use crate::admin::Source;
    let table = admin.resolve_roles().value;
    let mut out = String::from("Roles (candidates, tried in order <- where they came from)\n");
    for role in crate::roles::KNOWN_ROLES {
        let candidates = table.candidates_for(role);
        let models = if candidates.is_empty() {
            "session model".to_string()
        } else {
            candidates.join(" > ")
        };
        let source = match admin.role_source(role) {
            Source::Environment => {
                let var = format!(
                    "{}{}",
                    crate::roles::ROLE_ENV_PREFIX,
                    role.to_ascii_uppercase()
                );
                if admin.roles.get(role).is_some_and(|m| !m.is_empty()) {
                    format!("{var}, over your choice")
                } else {
                    var
                }
            }
            Source::AdminFile => "your choice".to_string(),
            Source::Flag | Source::BuiltIn => "not set".to_string(),
        };
        let purpose = crate::roles::role_purpose(role)
            .map(short_purpose)
            .unwrap_or_default();
        out.push_str(&format!(
            "  {role:<10} {models}  <- {source}  ({purpose})\n"
        ));
    }
    for (role, reference) in admin.unknown_tiers() {
        out.push_str(&format!(
            "  !! {}\n",
            crate::admin::unknown_tier_warning(&role, &reference)
        ));
    }
    if admin.tiers.is_empty() {
        out.push_str("  tiers      none defined\n");
    } else {
        for (tier, models) in &admin.tiers {
            let name = format!("@{tier}");
            out.push_str(&format!("  {name:<10} {}  (tier)\n", models.join(" > ")));
        }
    }
    out.push_str("Set one with /roles <role> <model|@tier>...; /roles <role> default clears it.\n");
    out
}

/// The opening phrase of a role's purpose, short enough to share its line.
fn short_purpose(purpose: &str) -> String {
    const MAX: usize = 32;
    let phrase = purpose
        .split(['—', ',', '.', ':'])
        .next()
        .unwrap_or(purpose)
        .trim()
        .to_lowercase();
    if phrase.chars().count() <= MAX {
        return phrase;
    }
    let mut short = String::new();
    for word in phrase.split_whitespace() {
        if short.chars().count() + word.chars().count() + 1 > MAX {
            break;
        }
        if !short.is_empty() {
            short.push(' ');
        }
        short.push_str(word);
    }
    format!("{short}…")
}

// ---------------------------------------------------------------------------
// Provider registry handler
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// MCP sync + catalog handlers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Worktree handler
// ---------------------------------------------------------------------------

pub fn handle_worktree(arg: Option<&str>, workspace: &std::path::Path) {
    let mgr = WorktreeManager::new(workspace.to_path_buf());

    match arg {
        Some(sub) if sub.starts_with("create ") => {
            let name = sub.strip_prefix("create ").unwrap_or("task").trim();
            match mgr.create(name, None) {
                Ok(wt) => {
                    render::print_info(&format!(
                        "Worktree created: {} (branch: {}, path: {})",
                        wt.name,
                        wt.branch,
                        wt.path.display()
                    ));
                }
                Err(e) => render::print_error(&format!("Worktree create failed: {e}")),
            }
        }
        Some(sub) if sub.starts_with("remove ") => {
            let name = sub.strip_prefix("remove ").unwrap_or("").trim();
            match mgr.remove(name) {
                Ok(removed) => match removed.branch_kept {
                    None => render::print_info(&format!(
                        "Worktree '{name}' removed, and branch {} with it.",
                        removed.branch
                    )),
                    Some(why) => render::print_info(&format!(
                        "Worktree '{name}' removed. Branch {} was KEPT: {why}.",
                        removed.branch
                    )),
                },
                Err(e) => render::print_error(&format!("Worktree remove failed: {e}")),
            }
        }
        Some("list") | None => {
            render::print_header("Git Worktrees");
            outln!("{}", mgr.display_list());
            outln!();
        }
        Some(sub) => {
            render::print_error(&format!("Unknown worktree subcommand: {sub}"));
            render::print_info("Usage: /worktree [list | create <name> | remove <name>]");
        }
    }
    outln!();
}

// ---------------------------------------------------------------------------
// Pull request handler
// ---------------------------------------------------------------------------

/// `/pr [branch] [--draft]`: push the branch and open its pull request, or
/// report the one already open.
pub fn handle_pull_request(arg: Option<&str>, workspace: &std::path::Path) {
    let words: Vec<&str> = arg.unwrap_or("").split_whitespace().collect();
    let draft = words.contains(&"--draft");
    let named: Vec<&str> = words.iter().copied().filter(|w| *w != "--draft").collect();
    if named.len() > 1 {
        render::print_error("Usage: /pr [branch] [--draft]");
        return;
    }
    let forge = crate::tools::pull_request::GitHubCli;
    match crate::tools::pull_request::open(workspace, named.first().copied(), draft, &forge) {
        Ok(opened) if opened.created => render::print_info(&format!(
            "Pushed {} and opened a pull request into {}: {}",
            opened.branch, opened.base, opened.url
        )),
        Ok(opened) => render::print_info(&format!(
            "Pushed {}; its pull request was already open: {}",
            opened.branch, opened.url
        )),
        Err(why) => render::print_error(&format!("/pr: {why}")),
    }
    outln!();
}

// ---------------------------------------------------------------------------
// SSH handler
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Env filter handler
// ---------------------------------------------------------------------------

pub fn handle_env() {
    render::print_header("Environment Filter");
    let filtered = crate::tools::env_filter::filtered_env();
    let mut keys: Vec<&String> = filtered.keys().collect();
    keys.sort();
    for key in &keys {
        let value = &filtered[*key];
        let display_value = if value.len() > 60 {
            format!("{}...", crate::text::head_bytes(value, 60))
        } else {
            value.clone()
        };
        outln!("  {key}={display_value}");
    }
    outln!();
    render::print_info(&format!(
        "{} env vars passed to subprocesses ({} blocked).",
        filtered.len(),
        std::env::vars().count().saturating_sub(filtered.len())
    ));
    outln!();
}

// ---------------------------------------------------------------------------
// Tool status handler
// ---------------------------------------------------------------------------

pub fn handle_tool_status(cache: &ToolStatusCache) {
    render::print_header("CLI Tool Status");
    let all = cache.all();
    let mut entries: Vec<(&String, &crate::tools::tool_status::ToolStatus)> = all.iter().collect();
    entries.sort_by_key(|(name, _)| name.to_string());
    for (name, status) in &entries {
        let icon = if status.installed { "+" } else { "-" };
        let version = status.version.as_deref().unwrap_or("n/a");
        outln!("  [{icon}] {name:<12} {version}");
    }
    outln!();
    outln!(
        "  {} installed, {} not found",
        cache.installed_count(),
        entries.len() - cache.installed_count()
    );
    outln!();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str) -> crate::skills::Skill {
        crate::skills::Skill {
            name: name.to_string(),
            description: format!("what {name} is for"),
            body: String::new(),
            path: String::new(),
        }
    }

    /// **The command must not deny what the session is doing.** `/skills` used
    /// to print "Skills are not compiled in. Rebuild with `--features
    /// agent-tools`" on the branch that actually ships, while
    /// `skills::session_context` was putting these very skills into every
    /// prompt. Nothing executed that sentence, so nothing contradicted it.
    #[test]
    fn the_skills_that_were_loaded_are_the_skills_that_are_listed() {
        let set = crate::skills::Skillset {
            managed: vec![skill("Test Driven Developer")],
            discovered: vec![skill("Ponytail"), skill("Reviewer Council")],
        };
        let lines = skills_lines(&set, crate::skills::Source::Store);
        let all = lines.join("\n");

        for name in ["Test Driven Developer", "Ponytail", "Reviewer Council"] {
            assert!(all.contains(name), "`{name}` must be listed; got:\n{all}");
        }
        assert!(
            !all.contains("not compiled in") && !all.contains("agent-tools"),
            "it must never deny skills it was just handed; got:\n{all}"
        );
        assert!(
            all.contains("3 skill(s): 1 written by you, 2 installed"),
            "the counts must be the real ones; got:\n{all}"
        );
    }

    /// **A cached list must not read like a live one.** An unreachable store
    /// that answers silently from cache is a wrong answer nobody can see.
    #[test]
    fn a_cached_answer_says_it_is_cached() {
        let set = crate::skills::Skillset {
            managed: vec![skill("Ponytail")],
            discovered: Vec::new(),
        };
        let all = skills_lines(&set, crate::skills::Source::Cache(90)).join("\n");
        assert!(all.contains("did not answer"), "got:\n{all}");
        assert!(
            all.contains("90s ago"),
            "the age must be stated; got:\n{all}"
        );
        assert!(all.contains("Ponytail"), "it still lists what it has");
    }

    /// Nothing at all is said plainly, and the list is not implied to be empty
    /// for some other reason.
    #[test]
    fn an_unreachable_store_with_no_cache_says_so() {
        let all = skills_lines(
            &crate::skills::Skillset::default(),
            crate::skills::Source::Nothing,
        )
        .join("\n");
        assert!(all.contains("running without skills"), "got:\n{all}");
    }

    /// A store that answered, with nothing in it, is a different fact from a
    /// store that did not answer.
    #[test]
    fn an_empty_store_is_not_reported_as_a_failure() {
        let all = skills_lines(
            &crate::skills::Skillset::default(),
            crate::skills::Source::Store,
        )
        .join("\n");
        assert!(all.contains("holds no skills"), "got:\n{all}");
        assert!(!all.contains("did not answer"), "got:\n{all}");
    }

    /// **Every command in the table must be one `parse` actually accepts**, or
    /// the help lists something that cannot be typed and completion offers it.
    ///
    /// A bare command that still needs an argument is fine — it is
    /// [`SlashCommand::Usage`], which names what is missing. What is NOT fine is
    /// `Unknown`, which tells the operator the command does not exist.
    ///
    /// This test found five: `/remember`, `/forget`, `/kill` (twice) and `/bg`
    /// all answered "Unknown command" when typed without their argument.
    #[test]
    fn every_listed_command_parses_to_something_real() {
        for (entry, _) in COMMANDS {
            let word = command_name(entry);
            let parsed = SlashCommand::parse(word)
                .unwrap_or_else(|| panic!("`{word}` is listed in /help but does not parse"));
            assert!(
                !matches!(parsed, SlashCommand::Unknown(_)),
                "`{word}` is listed in /help yet parses as Unknown — the operator is told a \
                 real command does not exist"
            );
        }
    }

    /// A command missing its argument names what it needs, and never claims the
    /// command itself is unknown.
    #[test]
    fn a_command_without_its_argument_is_told_what_it_needs() {
        for (typed, must_mention) in [
            ("/remember", "what to remember"),
            ("/forget", "conv_id"),
            ("/kill", "/jobs"),
            ("/bg", "command"),
        ] {
            match SlashCommand::parse(typed) {
                Some(SlashCommand::Usage(usage)) => assert!(
                    usage.contains(must_mention),
                    "`{typed}` must say what it needs — expected `{must_mention}` in `{usage}`"
                ),
                other => panic!("`{typed}` alone should ask for its argument, got {other:?}"),
            }
        }
    }

    /// A genuinely unknown command is still unknown — the new variant must not
    /// swallow the case it was split off from.
    #[test]
    fn a_command_that_does_not_exist_is_still_unknown() {
        assert_eq!(
            SlashCommand::parse("/definitely-not-a-command"),
            Some(SlashCommand::Unknown(
                "/definitely-not-a-command".to_string()
            ))
        );
    }

    /// `/kill` with a non-numeric id is a usage problem, not an unknown command.
    #[test]
    fn kill_with_a_bad_id_asks_for_a_job_number() {
        assert!(matches!(
            SlashCommand::parse("/kill not-a-number"),
            Some(SlashCommand::Usage(_))
        ));
    }

    #[test]
    fn parses_slash_commands() {
        assert_eq!(SlashCommand::parse("/help"), Some(SlashCommand::Help));
        assert_eq!(SlashCommand::parse("/status"), Some(SlashCommand::Status));
        assert_eq!(SlashCommand::parse("/compact"), Some(SlashCommand::Compact));
        assert_eq!(
            SlashCommand::parse("/tools"),
            Some(SlashCommand::Tools(None))
        );
        assert_eq!(
            SlashCommand::parse("/tools add browser"),
            Some(SlashCommand::Tools(Some("add browser".to_string())))
        );
        assert_eq!(SlashCommand::parse("/quit"), Some(SlashCommand::Quit));
        assert_eq!(
            SlashCommand::parse("/model opus"),
            Some(SlashCommand::Model(Some("opus".to_string())))
        );
        assert_eq!(
            SlashCommand::parse("/model"),
            Some(SlashCommand::Model(None))
        );
        assert_eq!(SlashCommand::parse("hello"), None);
    }

    #[test]
    fn parses_session_commands() {
        assert_eq!(
            SlashCommand::parse("/sessions"),
            Some(SlashCommand::Sessions)
        );
        assert_eq!(SlashCommand::parse("/ls"), Some(SlashCommand::Sessions));
        assert_eq!(
            SlashCommand::parse("/resume myfile"),
            Some(SlashCommand::Resume(Some("myfile".to_string())))
        );
        assert_eq!(
            SlashCommand::parse("/load myfile"),
            Some(SlashCommand::Resume(Some("myfile".to_string())))
        );
        assert_eq!(
            SlashCommand::parse("/resume"),
            Some(SlashCommand::Resume(None))
        );
        assert_eq!(
            SlashCommand::parse("/save myname"),
            Some(SlashCommand::Save(Some("myname".to_string())))
        );
    }

    /// The command is the only way a user reaches undo, so both spellings are
    /// pinned — and `/u` must not be swallowed by some other prefix match.
    /// `/image` takes a path, and without one says what it needs.
    #[test]
    fn parses_image() {
        assert_eq!(
            SlashCommand::parse("/image shots/a.png"),
            Some(SlashCommand::Image("shots/a.png".to_string()))
        );
        assert!(matches!(
            SlashCommand::parse("/image"),
            Some(SlashCommand::Usage(usage)) if usage.starts_with("/image <path")
        ));
    }

    #[test]
    fn parses_undo() {
        assert_eq!(SlashCommand::parse("/run"), Some(SlashCommand::Run));
        assert_eq!(SlashCommand::parse("/admin"), Some(SlashCommand::Admin));
        assert_eq!(SlashCommand::parse("/undo"), Some(SlashCommand::Undo));
        assert_eq!(SlashCommand::parse("/u"), Some(SlashCommand::Undo));
        assert_eq!(SlashCommand::parse("/UNDO"), Some(SlashCommand::Undo));
    }

    fn agent_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn installed(id: &str, protocol: crate::agent::Protocol) -> crate::agent::DetectedAgent {
        crate::agent::DetectedAgent {
            id: id.to_string(),
            display_name: id.to_string(),
            command: id.to_string(),
            path: None,
            version: Some("1.0".to_string()),
            protocol,
        }
    }

    /// Two usable local agents and one that is found but cannot be driven.
    fn two_agents_and_one_unverified() -> crate::agent::catalogue::Catalogue {
        crate::agent::catalogue::Catalogue::of_local(vec![
            installed("claude-code", crate::agent::Protocol::ClaudeStreamJson),
            installed(
                crate::agent::registry::CODEX,
                crate::agent::Protocol::CodexJson,
            ),
            installed("cursor", crate::agent::Protocol::Unverified),
        ])
    }

    fn session_lead() -> crate::agent::DetectedAgent {
        installed("claude-code", crate::agent::Protocol::ClaudeStreamJson)
    }

    #[test]
    fn parses_agent() {
        assert_eq!(
            SlashCommand::parse("/agent"),
            Some(SlashCommand::Agent(None))
        );
        assert_eq!(
            SlashCommand::parse("/agent sub codex"),
            Some(SlashCommand::Agent(Some("sub codex".to_string())))
        );
    }

    /// **`/agent <unknown>` is refused, and nothing is written.** A choice that
    /// saved a name nothing answers to would fall back in silence next session.
    #[test]
    fn an_unknown_agent_is_refused_and_nothing_is_written() {
        let _guard = agent_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        let said = agent_command(
            Some("no-such-agent"),
            &two_agents_and_one_unverified(),
            &session_lead(),
            dir.path(),
        );
        assert!(
            said.as_ref()
                .is_err_and(|why| why.contains("not in the catalogue") && why.contains("codex")),
            "it must refuse and name what exists: {said:?}"
        );
        assert!(
            !crate::admin::AdminConfig::path_in(dir.path()).exists(),
            "a refused choice wrote the settings file"
        );
    }

    /// A known agent that cannot be used is refused with the reason, and
    /// nothing is written either.
    #[test]
    fn an_agent_that_cannot_be_used_is_refused_with_the_reason() {
        let _guard = agent_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        let said = agent_command(
            Some("cursor"),
            &two_agents_and_one_unverified(),
            &session_lead(),
            dir.path(),
        );
        assert!(
            said.as_ref()
                .is_err_and(|why| why.contains("cannot be used") && why.contains("protocol")),
            "{said:?}"
        );
        assert!(!crate::admin::AdminConfig::path_in(dir.path()).exists());
    }

    /// **`/agent <known>` persists the lead to admin.json**, by its catalogue
    /// id, and keeps every other setting already in the file.
    #[test]
    fn a_known_agent_is_saved_as_the_lead_in_admin_json() {
        let _guard = agent_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        crate::admin::AdminConfig {
            model: Some("kept-model".to_string()),
            ..Default::default()
        }
        .save(dir.path())
        .expect("seed");

        let said = agent_command(
            Some("CODEX"),
            &two_agents_and_one_unverified(),
            &session_lead(),
            dir.path(),
        );
        assert!(
            said.as_ref()
                .is_ok_and(|text| text.contains("next session")),
            "{said:?}"
        );
        let saved = crate::admin::AdminConfig::load(dir.path()).expect("load");
        assert_eq!(saved.agent.as_deref(), Some(crate::agent::registry::CODEX));
        assert_eq!(saved.model.as_deref(), Some("kept-model"));
    }

    /// **`/agent sub <id>` saves the sub-agents; `/agent sub` puts them back
    /// to the lead's default** — and bare `/agent` says which it is.
    #[test]
    fn the_sub_agent_is_saved_shown_and_cleared() {
        let _guard = agent_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        let catalogue = two_agents_and_one_unverified();
        let lead = session_lead();

        agent_command(Some("sub codex"), &catalogue, &lead, dir.path()).expect("saves");
        let saved = crate::admin::AdminConfig::load(dir.path()).expect("load");
        assert_eq!(
            saved.sub_agent.as_deref(),
            Some(crate::agent::registry::CODEX)
        );
        assert_eq!(
            saved.agent, None,
            "choosing sub-agents must not move the lead"
        );
        let listing = agent_command(None, &catalogue, &lead, dir.path()).expect("lists");
        assert!(
            listing.contains("sub-agents   codex (your choice)"),
            "{listing}"
        );

        assert!(agent_command(Some("sub cursor"), &catalogue, &lead, dir.path()).is_err());
        assert_eq!(
            crate::admin::AdminConfig::load(dir.path())
                .expect("load")
                .sub_agent
                .as_deref(),
            Some(crate::agent::registry::CODEX),
            "a refused choice must leave the saved one alone"
        );

        agent_command(Some("sub same"), &catalogue, &lead, dir.path()).expect("clears");
        assert_eq!(
            crate::admin::AdminConfig::load(dir.path())
                .expect("load")
                .sub_agent,
            None
        );
        let listing = agent_command(None, &catalogue, &lead, dir.path()).expect("lists");
        assert!(
            listing.contains("sub-agents   claude-code (default for claude-code)"),
            "{listing}"
        );
    }

    /// A settings file that does not parse is refused, not replaced by one
    /// holding only this choice — that would erase every other setting.
    #[test]
    fn a_malformed_settings_file_is_not_overwritten() {
        let _guard = agent_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = crate::admin::AdminConfig::path_in(dir.path());
        std::fs::write(&path, "{ not json").expect("write");
        let said = agent_command(
            Some("codex"),
            &two_agents_and_one_unverified(),
            &session_lead(),
            dir.path(),
        );
        assert!(said.is_err(), "{said:?}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "{ not json");
    }

    fn clear_role_env() {
        for role in crate::roles::KNOWN_ROLES {
            std::env::remove_var(role_env(role));
        }
    }

    fn role_env(role: &str) -> String {
        format!(
            "{}{}",
            crate::roles::ROLE_ENV_PREFIX,
            role.to_ascii_uppercase()
        )
    }

    /// A settings file with one tier, `top`, and nothing else.
    fn seed_with_a_tier(dir: &std::path::Path) {
        let mut tiers = std::collections::BTreeMap::new();
        tiers.insert(
            "top".to_string(),
            vec!["model-top-1".to_string(), "model-top-2".to_string()],
        );
        crate::admin::AdminConfig {
            model: Some("kept-model".to_string()),
            tiers,
            ..Default::default()
        }
        .save(dir)
        .expect("seed");
    }

    #[test]
    fn parses_roles() {
        assert_eq!(
            SlashCommand::parse("/roles"),
            Some(SlashCommand::Roles(None))
        );
        assert_eq!(
            SlashCommand::parse("/roles review model-a @top"),
            Some(SlashCommand::Roles(Some("review model-a @top".to_string())))
        );
    }

    /// **`/roles <unknown> ...` is refused, and nothing is written.** A role
    /// nothing asks for would sit in the file looking configured forever.
    #[test]
    fn an_unknown_role_is_refused_and_nothing_is_written() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        let said = roles_command(Some("architect model-a"), dir.path());
        assert!(
            said.as_ref()
                .is_err_and(|why| why.contains("architect") && why.contains("explore")),
            "it must refuse and name the roles that exist: {said:?}"
        );
        assert!(
            !crate::admin::AdminConfig::path_in(dir.path()).exists(),
            "a refused role wrote the settings file"
        );
    }

    /// **A tier nobody defines is refused, and the file is left exactly as it
    /// was.** Saved, it would resolve to nothing and the role would quietly
    /// run on the session model.
    #[test]
    fn an_unknown_tier_is_refused_and_nothing_is_written() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        seed_with_a_tier(dir.path());
        let path = crate::admin::AdminConfig::path_in(dir.path());
        let before = std::fs::read_to_string(&path).expect("read");

        let said = roles_command(Some("review model-a @nope"), dir.path());
        assert!(
            said.as_ref()
                .is_err_and(|why| why.contains("@nope") && why.contains("@top")),
            "it must refuse, name the bad tier and the ones that exist: {said:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            before,
            "a refused tier changed the settings file"
        );
    }

    /// More candidates than a role may hold is refused rather than silently
    /// cut short — the ones dropped would be the operator's last fallbacks.
    #[test]
    fn too_many_candidates_are_refused_and_nothing_is_written() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        let many: Vec<String> = (0..=crate::roles::MAX_CANDIDATES)
            .map(|i| format!("model-{i}"))
            .collect();
        let said = roles_command(Some(&format!("plan {}", many.join(" "))), dir.path());
        assert!(
            said.as_ref()
                .is_err_and(|why| why.contains(&crate::roles::MAX_CANDIDATES.to_string())),
            "{said:?}"
        );
        assert!(!crate::admin::AdminConfig::path_in(dir.path()).exists());
    }

    /// **A role is saved lower-case, in order, and read back** — and every
    /// other setting in the file survives the write.
    #[test]
    fn a_role_is_saved_and_read_back() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        seed_with_a_tier(dir.path());

        let said = roles_command(Some("REVIEW model-a, @top"), dir.path());
        assert!(
            said.as_ref().is_ok_and(|text| text.contains("next /run")),
            "{said:?}"
        );
        let saved = crate::admin::AdminConfig::load(dir.path()).expect("load");
        assert_eq!(
            saved.roles.get("review"),
            Some(&vec!["model-a".to_string(), "@top".to_string()])
        );
        assert!(!saved.roles.contains_key("REVIEW"));
        assert_eq!(saved.model.as_deref(), Some("kept-model"));
        assert!(saved.tiers.contains_key("top"), "the ladder was lost");

        let listing = roles_command(None, dir.path()).expect("lists");
        let line = listing
            .lines()
            .find(|l| l.trim_start().starts_with("review"))
            .unwrap_or_default();
        assert!(
            line.contains("model-a > model-top-1 > model-top-2") && line.contains("your choice"),
            "{listing}"
        );
    }

    /// **`default` (or `clear`) removes the role**, so it runs on the session
    /// model again, and leaves the other roles alone.
    #[test]
    fn default_clears_the_role() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        roles_command(Some("plan model-a"), dir.path()).expect("saves plan");
        roles_command(Some("grunt model-b"), dir.path()).expect("saves grunt");

        roles_command(Some("Plan default"), dir.path()).expect("clears plan");
        let saved = crate::admin::AdminConfig::load(dir.path()).expect("load");
        assert!(!saved.roles.contains_key("plan"), "{:?}", saved.roles);
        assert_eq!(saved.roles.get("grunt"), Some(&vec!["model-b".to_string()]));

        roles_command(Some("grunt clear"), dir.path()).expect("clears grunt");
        let saved = crate::admin::AdminConfig::load(dir.path()).expect("load");
        assert!(saved.roles.is_empty(), "{:?}", saved.roles);
    }

    /// A settings file that does not parse is refused, not replaced by one
    /// holding only this role — that would erase every other setting.
    #[test]
    fn roles_leaves_a_malformed_settings_file_alone() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = crate::admin::AdminConfig::path_in(dir.path());
        std::fs::write(&path, "{ not json").expect("write");
        let said = roles_command(Some("plan model-a"), dir.path());
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "{ not json",
            "the malformed settings file was overwritten"
        );
        assert!(said.is_err(), "{said:?}");
    }

    /// **The listing names where every role came from**: the file, the shell,
    /// or nowhere — and a tier that does not exist is called out, not shown as
    /// a role nobody configured.
    #[test]
    fn the_listing_shows_where_each_role_came_from() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        let mut roles = std::collections::BTreeMap::new();
        roles.insert("implement".to_string(), vec!["model-a".to_string()]);
        roles.insert("diagnose".to_string(), vec!["@missing".to_string()]);
        crate::admin::AdminConfig {
            roles,
            ..Default::default()
        }
        .save(dir.path())
        .expect("seed");
        std::env::set_var(role_env("plan"), "model-env");

        let listing = roles_command(None, dir.path());
        clear_role_env();
        let listing = listing.expect("lists");

        let line_for = |role: &str| {
            listing
                .lines()
                .find(|l| l.trim_start().starts_with(role))
                .unwrap_or_default()
                .to_string()
        };
        let implement = line_for("implement");
        assert!(
            implement.contains("model-a") && implement.contains("your choice"),
            "{listing}"
        );
        let plan = line_for("plan");
        assert!(
            plan.contains("model-env") && plan.contains("INCODE_ROLE_PLAN"),
            "{listing}"
        );
        let explore = line_for("explore");
        assert!(
            explore.contains("session model") && explore.contains("not set"),
            "{listing}"
        );
        for role in crate::roles::KNOWN_ROLES {
            assert!(
                listing.lines().any(|l| l.trim_start().starts_with(role)),
                "`{role}` is missing from the listing:\n{listing}"
            );
        }
        assert!(
            listing.contains("diagnose names @missing, which no tier defines"),
            "an unknown tier must be flagged:\n{listing}"
        );
    }

    /// Saving a role the shell overrides still saves it, and says the saved
    /// choice will not apply while the variable is set.
    #[test]
    fn a_role_the_shell_overrides_is_saved_with_a_warning() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        std::env::set_var(role_env("explore"), "model-env");
        let said = roles_command(Some("explore model-a"), dir.path());
        clear_role_env();

        assert!(
            said.as_ref().is_ok_and(
                |text| text.contains("INCODE_ROLE_EXPLORE") && text.contains("overrides")
            ),
            "{said:?}"
        );
        let saved = crate::admin::AdminConfig::load(dir.path()).expect("load");
        assert_eq!(
            saved.roles.get("explore"),
            Some(&vec!["model-a".to_string()])
        );
    }

    #[test]
    fn parses_unknown_command() {
        assert_eq!(
            SlashCommand::parse("/foobar"),
            Some(SlashCommand::Unknown("/foobar".to_string()))
        );
    }
}
