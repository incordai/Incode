//! CLI module — interactive REPL for `incode`.
//!
//! The session is built in `prepare` ([`prepare_session`], shared with
//! `incode --acp`) and driven by the loop in `repl`. What stays here is what
//! both front ends and the run loop share: the backend for an agent, and what
//! one typed line becomes.

pub mod activity;
pub mod agent_turn;
pub mod console;
pub mod edits;
pub mod footer;
pub mod input;
pub mod keys;
pub mod logo;
pub mod markdown;
pub mod notify;
pub mod paint;
pub mod panel;
mod prepare;
pub mod prompt;
pub mod render;
mod repl;
pub(crate) mod run_ledger;
pub(crate) mod run_memory;
pub mod slash_commands;
pub mod statusline;
mod ux;
pub mod viewport;

use crate::out;
use std::path::Path;

use crate::config::IncordCodeConfig;

pub(crate) use prepare::{prepare_session, Prepared};

// ---------------------------------------------------------------------------
// TerminalPrompter
// ---------------------------------------------------------------------------

/// Time one startup step, when the operator asks to see where the time goes.
///
/// Reports under `INCODE_TRACE` in the `startup` area. A startup that takes
/// seventeen seconds with five candidate causes is not diagnosable by reading;
/// this makes it name itself.
fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
    crate::trace::timed("startup", label, f)
}

/// Cumulative elapsed at a named point in startup, under `INCODE_TRACE`.
fn mark(label: &str, since: std::time::Instant) {
    crate::trace!("startup", "@{label} at {:?}", since.elapsed());
}

// ---------------------------------------------------------------------------
// CliApp
// ---------------------------------------------------------------------------

/// The main REPL application for `incode`.
pub struct CliApp {
    config: IncordCodeConfig,
}

impl CliApp {
    #[must_use]
    pub fn new(config: IncordCodeConfig) -> Self {
        Self { config }
    }

    /// Run the interactive REPL. Blocks until the user quits.
    pub fn run(self) -> anyhow::Result<()> {
        let booted_at = std::time::Instant::now();
        // Ctrl-C during a turn. The line editor handles it while reading a
        // line; this covers the rest. The agent runs as a child process, so
        // stopping it means killing that process — the token is checked between
        // output lines and the child is killed when it is set.
        let cancel = crate::runtime::cancel::CancelToken::new();
        // The REPL's own id for this session, made the way `incode --acp`
        // makes one for each `session/new`.
        let session_id = uuid::Uuid::new_v4().to_string();
        let mut prepared = prepare_session(&self.config, cancel.clone(), booted_at, &session_id)?;
        crate::telemetry::session_started("repl");
        connect_ext_tools(&prepared);
        // Someone is at the keyboard only when both ends are a terminal; piped,
        // a mid-turn question would wait on input nobody can give.
        if std::io::IsTerminal::is_terminal(&std::io::stdin())
            && std::io::IsTerminal::is_terminal(&std::io::stdout())
        {
            prepared
                .session
                .set_prompter(std::sync::Arc::new(prompt::Terminal));
        }
        self.repl(prepared, cancel, booted_at)
    }
}

/// Start the owner's installed external tools as MCP servers for the REPL's
/// session, in the background — their tools join the toolbox once each has
/// listed them — and say which configured tools are not installed. What
/// became of each server is traced, never printed over the prompt.
fn connect_ext_tools(prepared: &Prepared) {
    let (mut specs, mut said) = crate::ext_tools::session_servers(
        &crate::ext_tools::in_effect(&prepared.admin),
        &prepared.incode_dir,
    );
    let (plugin_specs, plugin_said) = crate::plugins::session_servers(&prepared.workspace);
    specs.extend(plugin_specs);
    said.extend(plugin_said);
    // `.mcp.json` and `~/.incode/mcp.json`. A project's server not yet
    // approved is put to the person at the terminal, before the prompt.
    let at_terminal = std::io::IsTerminal::is_terminal(&std::io::stdin())
        && std::io::IsTerminal::is_terminal(&std::io::stdout());
    let ask = |name: &str, entry: &crate::mcp_config::ServerEntry| {
        render::print_info(&format!(
            "This project's {} wants to start the MCP server `{name}`: {}",
            crate::mcp_config::PROJECT_FILE,
            entry.summary()
        ));
        out!("Start it now and remember the approval? [y/N] ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).is_ok()
            && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    };
    let asker: Option<crate::mcp_config::Asker<'_>> = if at_terminal { Some(&ask) } else { None };
    let (from_files, about_files) =
        crate::mcp_config::session_servers(&prepared.workspace, &prepared.incode_dir, asker);
    specs.extend(from_files);
    said.extend(about_files);
    for line in said {
        render::print_info(&line);
    }
    let on_connected: crate::toolbox::OnConnected =
        std::sync::Arc::new(|line| crate::trace!("tool", "{line}"));
    for line in prepared.session.session_tools().connect_client_servers(
        specs,
        &prepared.workspace,
        &on_connected,
    ) {
        crate::trace!("tool", "{line}");
    }
}

/// The agent the run loop drives, built with everything the session knows.
///
/// # Wired to
///
/// - **Called by** the `/run` handler in [`CliApp::run`], for the installed-agent
///   path (an operator with a configured API endpoint gets `ApiBackend` instead).
/// - **Calls** [`crate::agent::claude_code::ClaudeCodeBackend`]'s builders.
/// - **Sibling to** the interactive backend built through
///   `crate::cli::agent_turn::AgentSession`, which must carry the SAME context.
///
/// # Why this is a function and not four lines at the call site
///
/// It was four lines at the call site, and the system context went missing from
/// them TWICE. The run backend shipped built as
/// `ClaudeCodeBackend::new(..).with_binary(..).with_guards(..)` — no
/// `with_system_context` — so every ledger task was worked by an agent told none
/// of the intake task, the owner's rules, the auditor's findings, the recorder's
/// handoff or the role table, while the interactive backend beside it had all of
/// them. It compiled, the suite stayed green, and the only visible symptom was a
/// child process missing one flag.
///
/// A call site cannot be tested; this can. See
/// `the_run_backend_carries_the_sessions_system_context`.
/// The switch that gives an agent this machine's screen.
///
/// OFF unless explicitly set. Computer use lets an agent click and type into
/// whatever is on the display, which is the widest capability the harness can
/// hand over and the only one whose mistakes cannot be undone by a checkpoint.
/// Making it the default would widen every existing run as a side effect of an
/// upgrade, which is not a thing to discover afterwards.
///
/// Set `INCODE_DESKTOP=1` to turn it on. Empty does NOT count, for the same
/// reason `INCODE_TRACE=` in a shell profile must not turn tracing on for
/// everyone (see [`crate::trace`]).
pub(crate) const DESKTOP_ENV: &str = "INCODE_DESKTOP";

/// What the session's `-p` was, for the parts of this file that build a
/// backend without holding the session.
///
/// # Why an environment variable and not a parameter
///
/// The mode is resolved once, at session start, and never changes for the
/// life of the process — while `run_backend` is reached from eight places,
/// several of which (an interactive turn, a slash command) have no access to
/// the session config. Threading it through all of them would add a parameter
/// that is the same value every time it is passed.
///
/// It carries the mode WITHIN this process only. What reaches the desktop
/// server is an explicit `-p` on its argv, because that is a different
/// process and an environment it inherits is not a thing to rely on for a
/// capability decision.
///
/// Unset means the default mode, which is what a session without `-p` runs as.
pub(crate) const PERMISSION_MODE_ENV: &str = "INCODE_PERMISSION_MODE";

/// The session's mode, or the default when nothing set it.
///
/// Uses [`crate::permissions::PermissionMode::from_str_lossy`], which fails
/// CLOSED: a value nobody recognises lands on `read-only` rather than being
/// treated as permission to act.
fn permission_mode_from_env() -> crate::permissions::PermissionMode {
    match std::env::var(PERMISSION_MODE_ENV) {
        Ok(raw) if !raw.trim().is_empty() => {
            crate::permissions::PermissionMode::from_str_lossy(raw.trim())
        }
        _ => crate::permissions::PermissionMode::WorkspaceWrite,
    }
}

/// Where the desktop tools are started from, for a model whose tools are
/// incode's own (the bridged CLI and the API route), or `None` when the
/// desktop is off.
///
/// The desktop tools are toolbox tools there: offered by the toolbox, held to
/// the permission mode and the memory engine, and executed by the desktop
/// child the toolbox starts (`toolbox::desktop`). No desktop server is handed
/// to the model beside the bridge. It stays opt-in: a desktop is a capability
/// over the operator's machine.
pub(crate) fn desktop_setup_for(
    desktop: bool,
    exe: Option<std::path::PathBuf>,
    workspace: &Path,
) -> Option<crate::toolbox::DesktopSetup> {
    // No path to this binary means the desktop child cannot be started at
    // all, and a tool that can only fail is not offered.
    let exe = exe?;
    desktop.then(|| crate::toolbox::DesktopSetup {
        exe,
        journal: crate::desktop::journal::path_in(workspace),
    })
}

/// The same set, in Codex's `-c key=value` spelling.
pub(crate) fn mcp_overrides_for(
    desktop: bool,
    exe: Option<std::path::PathBuf>,
    workspace: &Path,
    mode: crate::permissions::PermissionMode,
) -> Vec<String> {
    let Some(exe) = exe else {
        return Vec::new();
    };
    let mut out = crate::team_mcp::codex_config_overrides(&exe, &crate::team::path_in(workspace));
    if desktop {
        out.extend(crate::desktop::mcp::codex_config_overrides(
            &exe,
            Some(&crate::desktop::journal::path_in(workspace)),
            mode,
        ));
    }
    out
}

fn desktop_enabled_from_env() -> bool {
    std::env::var(DESKTOP_ENV).is_ok_and(|v| !v.trim().is_empty())
}

/// How the OS confines a model's shell commands, from the settings and the
/// session's permission mode — the same resolution `/admin` reports.
fn sandbox_for(mode: crate::permissions::PermissionMode) -> crate::sandbox::SandboxPolicy {
    crate::admin::AdminConfig::load_default().sandbox_policy(mode)
}

/// The backend for one catalogue route: an API route when the id names one
/// ([`crate::agent::models::settings_for`]), otherwise the installed CLI.
///
/// # The bug this closes
///
/// The API backend used to be chosen whenever the API variables were SET,
/// whatever agent had been chosen — so exporting two variables replaced every
/// installed agent, and a session that announced one agent drove another. The
/// id decides now, and the catalogue lists that id as usable by the same
/// resolution ([`crate::agent::models`]).
pub(crate) fn route_backend(
    workspace: &Path,
    agent_id: &str,
    binary: &str,
    guards: &crate::guards::Guards,
    system_prompt: &[String],
    filter_env: bool,
) -> Box<dyn crate::agent::AgentBackend> {
    match crate::agent::models::settings_for(agent_id) {
        Some(settings) => Box::new(
            crate::agent::api::ApiBackend::new(
                settings,
                workspace,
                // The same project context the CLI gets, as the standing
                // instructions rather than repeated on every turn.
                &system_prompt.join("\n\n"),
                filter_env,
            )
            // The memory engine's guard, asked before each of the harness's own
            // tools runs, and the operator's own commands around each.
            .with_engine(crate::hooks::engine::Engine::locate())
            .with_code_edits(crate::toolbox::CodeEdits::locate())
            .with_operator_hooks(crate::hooks::HookRunner::from_settings())
            .with_sandbox(sandbox_for(permission_mode_from_env()))
            .with_desktop(desktop_setup_for(
                desktop_enabled_from_env(),
                std::env::current_exe().ok(),
                workspace,
            )),
        ),
        None => run_backend(workspace, agent_id, binary, guards, system_prompt),
    }
}

pub(crate) fn run_backend(
    workspace: &Path,
    agent_id: &str,
    binary: &str,
    guards: &crate::guards::Guards,
    system_prompt: &[String],
) -> Box<dyn crate::agent::AgentBackend> {
    run_backend_with_desktop(
        workspace,
        agent_id,
        binary,
        guards,
        system_prompt,
        desktop_enabled_from_env(),
        permission_mode_from_env(),
    )
}

/// [`run_backend`] with the desktop switch passed in rather than read.
///
/// The switch is an environment variable, and a test that reads one can only
/// assert "whatever the environment says" — which passes with the wiring
/// deleted. Taking it as an argument is what lets a test drive BOTH states in
/// one process and actually reach the registration lines below.
fn run_backend_with_desktop(
    workspace: &Path,
    agent_id: &str,
    binary: &str,
    guards: &crate::guards::Guards,
    system_prompt: &[String],
    desktop: bool,
    mode: crate::permissions::PermissionMode,
) -> Box<dyn crate::agent::AgentBackend> {
    // By the PROTOCOL the agent table gives the id — not by the id itself, so
    // an agent admin.json adds (`cli_agents`) is driven by the backend for the
    // protocol it names. Looked up by the registry ID, never the command name:
    // `agy` is the binary, `antigravity` the agent, and matching the binary
    // once compiled, passed a test that invented the same wrong id on both
    // sides, and never fired in a real run.
    let protocol =
        crate::agent::registry::protocol_of(&crate::agent::registry::configured_table(), agent_id);
    backend_for_protocol(
        protocol,
        workspace,
        binary,
        guards,
        system_prompt,
        desktop,
        mode,
    )
}

/// The CLI backend for a protocol: each verified protocol has its own, and
/// anything else is driven as the stream-json one, as before this was keyed
/// on the protocol.
fn backend_for_protocol(
    protocol: Option<crate::agent::Protocol>,
    workspace: &Path,
    binary: &str,
    guards: &crate::guards::Guards,
    system_prompt: &[String],
    desktop: bool,
    mode: crate::permissions::PermissionMode,
) -> Box<dyn crate::agent::AgentBackend> {
    match protocol {
        Some(crate::agent::Protocol::AgyStreamJson) => Box::new(
            crate::agent::agy::AgyBackend::new(workspace.to_path_buf())
                .with_binary(binary.to_string())
                // No guards argument: this agent has no flag to enforce them.
                // The policy still applies — the sentinel checks it, and the
                // worker reports the gap on every task — but claiming it here
                // by handing over rules nothing would render is the pretence
                // this whole split exists to prevent.
                .with_system_context(system_prompt.to_vec()),
        ),
        Some(crate::agent::Protocol::CodexJson) => Box::new(codex_backend(
            workspace,
            binary,
            system_prompt,
            desktop,
            mode,
            crate::admin::AdminConfig::load_default()
                .resolve_codex_tools()
                .value,
        )),
        _ => {
            let mut backend =
                crate::agent::claude_code::ClaudeCodeBackend::new(workspace.to_path_buf())
                    .with_binary(binary.to_string())
                    .with_guards(guards)
                    .streaming(true)
                    .with_system_context(system_prompt.to_vec())
                    // The model's tools are incode's own on this route, so the
                    // memory engine's guard is asked before each, as it is on
                    // the API route, and the operator's own commands run
                    // around each.
                    .with_engine(crate::hooks::engine::Engine::locate())
                    .with_code_edits(crate::toolbox::CodeEdits::locate())
                    .with_operator_hooks(crate::hooks::HookRunner::from_settings())
                    .with_sandbox(sandbox_for(mode));

            // The desktop, as toolbox tools reached through the bridge — not
            // a server of its own beside it. The desktop child is told the
            // session's mode by the toolbox, which holds it.
            if let Some(setup) = desktop_setup_for(desktop, std::env::current_exe().ok(), workspace)
            {
                backend = backend.with_desktop(setup);
            }

            Box::new(backend)
        }
    }
}

/// The codex backend, on its own tools or on incode's ([`crate::agent::codex::CodexTools`]).
///
/// The choice is an argument, not read here, for the reason
/// [`run_backend_with_desktop`] takes the desktop switch: a test can then
/// build both routes in one process and reach the lines of each.
fn codex_backend(
    workspace: &Path,
    binary: &str,
    system_prompt: &[String],
    desktop: bool,
    mode: crate::permissions::PermissionMode,
    tools: crate::agent::codex::CodexTools,
) -> crate::agent::codex::CodexBackend {
    let backend = crate::agent::codex::CodexBackend::new(workspace.to_path_buf())
        .with_binary(binary.to_string())
        .with_system_context(system_prompt.to_vec());
    match tools {
        // Codex as a model only. The toolbox gets what the Claude route's
        // does — the engine's guard before each call, the operator's own
        // commands around each, the OS sandbox on its shell — and the desktop
        // and the team channel are toolbox tools reached through the bridge,
        // so no server of their own is registered beside it.
        crate::agent::codex::CodexTools::Incode => {
            let mut backend = backend
                .with_tools(tools)
                .with_engine(crate::hooks::engine::Engine::locate())
                .with_code_edits(crate::toolbox::CodeEdits::locate())
                .with_operator_hooks(crate::hooks::HookRunner::from_settings())
                .with_sandbox(sandbox_for(mode));
            if let Some(setup) = desktop_setup_for(desktop, std::env::current_exe().ok(), workspace)
            {
                backend = backend.with_desktop(setup);
            }
            backend
        }
        crate::agent::codex::CodexTools::Own => {
            // Codex spells registration as `-c key=value` TOML overrides rather
            // than a JSON flag — verified from `codex mcp add --help`, whose own
            // example passes an array.
            let overrides =
                mcp_overrides_for(desktop, std::env::current_exe().ok(), workspace, mode);
            if overrides.is_empty() {
                backend
            } else {
                backend.with_mcp_overrides(overrides)
            }
        }
    }
}

/// Run one turn and print it, streaming the reply as it is generated.
///
/// `augmented` is what the agent is sent — the instruction plus whatever skills
/// or reasoning templates matched. `line` is what the PERSON typed, and is what
/// labels the checkpoint, so `/undo` says which change it is putting back.
fn run_one_turn(
    session: &mut crate::cli::agent_turn::AgentSession,
    workspace: &Path,
    augmented: &str,
    line: &str,
) -> Result<(), String> {
    match plan_turn(session, workspace, augmented, line) {
        TurnPlan::NextQuestion(menu) => {
            render::print_info(&menu);
            Ok(())
        }
        TurnPlan::Greeting(reply) => {
            render::print_assistant(&reply);
            Ok(())
        }
        TurnPlan::Refused(why) => {
            render::print_error(&format!(
                "Not sent: the operator's user_prompt_submit hook refused it. {why}"
            ));
            Ok(())
        }
        TurnPlan::Agent {
            instruction,
            context,
        } => one_turn_with_context(session, workspace, &instruction, context),
    }
}

/// What one line of input becomes, decided before any agent is started.
///
/// Shared by the REPL and `incode --acp` so the two cannot disagree about what
/// a line MEANS — an answer to a pending question, a greeting, or work.
pub(crate) enum TurnPlan {
    /// The line answered a question and another is now showing. Nothing is
    /// sent to the agent yet; this is the next question's menu.
    NextQuestion(String),
    /// A bare greeting, answered here without starting an agent.
    Greeting(String),
    /// The operator's `user_prompt_submit` hook refused the line: why. Nothing
    /// is sent, and the memory engine does not hear it either.
    Refused(String),
    /// A turn for the agent: what it is asked, and the context that goes
    /// before it.
    Agent {
        instruction: String,
        context: Vec<String>,
    },
}

/// Decide what `line` is. `augmented` is what the agent is sent for ordinary
/// work; `line` is what the person typed.
pub(crate) fn plan_turn(
    session: &mut crate::cli::agent_turn::AgentSession,
    workspace: &Path,
    augmented: &str,
    line: &str,
) -> TurnPlan {
    // A question was put to the owner and this line is the reply. Read as the
    // ANSWER rather than as a fresh instruction: "b" typed after a menu means
    // the second option, and forwarding it verbatim would send the agent a
    // single letter with no idea what it referred to.
    match session.take_answer(line) {
        crate::cli::agent_turn::Answered::NothingPending => {}
        // Another question is showing. Nothing is sent yet: the agent asked
        // two things and is owed both answers, and a turn per answer would pay
        // for the same conversation twice.
        crate::cli::agent_turn::Answered::AskedTheNext(menu) => {
            return TurnPlan::NextQuestion(menu);
        }
        crate::cli::agent_turn::Answered::Ready(instruction) => {
            // The answers are the person's line for this turn: the operator's
            // hooks see it first, then the engine hears it, and both their
            // contexts ride the turn.
            let mut context = match session.prompt_hooks(&instruction) {
                Ok(added) => added,
                Err(why) => return TurnPlan::Refused(why),
            };
            context.extend(session.memory_prompt(&instruction));
            return TurnPlan::Agent {
                instruction,
                context,
            };
        }
    }

    // What the harness knows and the agent does not: the guard rules in force
    // and whatever the shared memory has on this question.
    //
    // SKIPPED for a greeting. Both are network calls, and "hi" has nothing to
    // guard and nothing to recall — measured at 12 seconds of dead wait in
    // front of the one reply where a person is most likely watching the clock.
    // Only a bare greeting qualifies: "hi, fix the parser" is work and gets its
    // full context.
    // A greeting is answered here too, for callers that do not come through
    // the REPL loop (which checks first, before its own context lookups).
    //
    // The first attempt at this only skipped the two context lookups and still
    // ran a full turn: process spawn, the entire system prompt — which in a
    // directory with no brief carries the whole intake preamble — inference,
    // and streaming. Measured at eighteen seconds and upwards to say hello, and
    // reported three times as the program hanging.
    //
    // Skipping the lookups was the wrong fix. Nothing about "hi" needs a model:
    // there is no question in it, no file to read and no decision to make. So
    // the harness answers it itself, instantly and for nothing, and the agent
    // is started when there is actually work.
    if crate::text::is_greeting(line) {
        return TurnPlan::Greeting(crate::text::greeting_reply(workspace));
    }

    // The memory engine's context for this line, which carries the rules
    // itself: when it answers, incode's own per-turn rules are not fetched as
    // well, so the rules are never sent twice. When it does not — no engine,
    // or nothing to say — incode's own are.
    //
    // The operator's `user_prompt_submit` hooks first: one that refuses the
    // line stops it here, before the engine or the agent hear it.
    let mut context = match session.prompt_hooks(line) {
        Ok(added) => added,
        Err(why) => return TurnPlan::Refused(why),
    };
    // Its length to the operator's collector; its text only when they asked
    // for prompts (`crate::telemetry`).
    crate::telemetry::user_prompt(line);
    match session.memory_prompt(line) {
        Some(block) => context.push(block),
        // The engine's hook said nothing: the rules this conversation has not
        // been given, then what memory holds for the line and the code it is
        // about — not the rules alone, which is all such a turn used to carry.
        None => {
            context.extend(session.turn_rules(line));
            context.extend(session.turn_memory(line));
        }
    }

    TurnPlan::Agent {
        instruction: augmented.to_string(),
        context,
    }
}

/// `/review [base]`: the change in `workspace` against `base`, as one turn
/// for the agent. The operator's `user_prompt_submit` hooks see the review
/// prompt first, as they see a typed line: one that refuses it stops it here,
/// and what they print rides the turn. The memory engine is not told it — it
/// is incode's prompt, not the person's line.
///
/// Called by the REPL's `/review` (`--acp` does not offer it).
pub(crate) fn review_turn(
    session: &mut crate::cli::agent_turn::AgentSession,
    workspace: &Path,
    base: Option<&str>,
) -> Result<(), String> {
    let change = crate::review::collect(workspace, base)?;
    if change.is_empty() {
        render::print_info(&format!("No changes to review against {}.", change.base));
        return Ok(());
    }
    let ask = crate::review::prompt(&change);
    let context = session.prompt_hooks(&ask).map_err(|why| {
        format!("Not sent: the operator's user_prompt_submit hook refused it. {why}")
    })?;
    one_turn_with_context(session, workspace, &ask, context)
}

/// The turn itself: spinner, streaming, and what the outcome means.
fn one_turn_with_context(
    session: &mut crate::cli::agent_turn::AgentSession,
    workspace: &Path,
    augmented: &str,
    context: Vec<String>,
) -> Result<(), String> {
    // One line under the output, rewritten in place: what the agent is doing,
    // how long it has been at it, and what the turn has cost. It only draws on
    // a terminal — piped, the frames would be hundreds of lines.
    let cancel_token = session.cancel_token();
    let status = render::LiveStatus::start_with_cancel(
        std::io::IsTerminal::is_terminal(&std::io::stdout()),
        cancel_token,
    );
    status.set(&format!("{} Thinking", crate::cli::render::FRAME_SLOT));

    let mut streamed = false;
    let width = crate::cli::activity::terminal_width();
    let outcome = session.turn(
        workspace,
        augmented,
        &context,
        &mut |fragment: &str| {
            // The reply and the status line share one terminal. This wipes
            // the line and RETURNS ONCE IT IS GONE — an asynchronous clear
            // leaves a frame in which the reply starts at whatever column the
            // status left the cursor, which is how a reply came out as
            // "Thinking · 2m 36st by getting oriented" and an error as
            // "Error: t". The next status event restores the line.
            // Through the SAME lock the status draws under, so the two can
            // never be writing at once.
            status.write_text(fragment);
            streamed = true;
        },
        &mut |snapshot: &crate::cli::activity::Status| {
            // Text has landed since the last draw, so the status starts on its
            // OWN line rather than on the end of the reply.
            status.set(&snapshot.render(crate::cli::render::FRAME_SLOT, width));
        },
    );
    status.stop();
    if streamed {
        render::finish_streaming();
    }

    // Printed only once the status line has stopped: two writers on one line is
    // how a reply came out as "Thinking · 2m 36st by getting oriented".
    for notice in session.take_notices() {
        render::print_info(&notice);
    }

    let outcome = outcome?;
    if outcome.is_error {
        return Err(outcome.failure());
    }
    if !streamed {
        render::print_dim("(the agent finished without saying anything)");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
