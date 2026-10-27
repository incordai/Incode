//! The interactive loop over a session [`super::prepare_session`] built: the
//! startup it says, one line at a time until the person leaves, and the
//! session's end.
//!
//! # Wired to
//!
//! - **Called by** [`super::CliApp::run`], through [`super::CliApp::repl`].
//! - **Dispatches** a slash command to the handlers in `commands`, which call
//!   the `slash_commands::handle_*` for what it names; a line of work goes to
//!   [`super::run_one_turn`].
//! - **Reads** lines from the pinned [`footer::Footer`] on a terminal, else
//!   from the plain [`LineEditor`].

mod commands;

use std::path::{Path, PathBuf};

use crate::config::IncordCodeConfig;
use crate::outln;
use crate::permissions::PermissionMode;
use crate::runtime::lifecycle::LifecycleHooks;
use crate::tools::git_context::GitContext;
use crate::tools::tool_status::ToolStatusCache;

use super::input::LineEditor;
use super::slash_commands::SlashCommand;
use super::{footer, notify, render, run_one_turn, ux, Prepared};

/// What the loop does once a line has been dealt with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    /// Read the next line.
    Continue,
    /// Leave the loop: `/quit`.
    Quit,
}

impl super::CliApp {
    /// The interactive loop over a session [`super::prepare_session`] built.
    ///
    /// Startup in the order the person sees it — the banner, a resumed
    /// session, the input box, the tool summary and the start hook — then
    /// one line at a time until `/quit`, Ctrl-C or Ctrl-D, then the end of
    /// the session.
    pub(super) fn repl(
        self,
        prepared: Prepared,
        cancel: crate::runtime::cancel::CancelToken,
        booted_at: std::time::Instant,
    ) -> anyhow::Result<()> {
        forward_ctrl_c(cancel.clone());
        let mut repl = Repl::new(self.config, prepared, cancel);
        repl.print_banner();
        repl.resume_from_flag();
        let mut term = Terminal::open(&repl);
        repl.announce_start();

        // REPL loop. Ends when the editor returns nothing — Ctrl-C or Ctrl-D.
        crate::trace!("startup", "first prompt after {:?}", booted_at.elapsed());
        let custom_commands = load_custom_commands(&repl.workspace);
        while let Some(line) = term.read_line() {
            if repl.on_line(line, &custom_commands, &mut term) == Next::Quit {
                break;
            }
        }
        repl.finish();
        Ok(())
    }
}

/// Ctrl-C during a turn trips `cancel`, for as long as the process runs.
///
/// The line editor handles Ctrl-C while reading a line; this covers the rest.
/// The loop is [`forward_signals`], over the process's real Ctrl-C.
fn forward_ctrl_c(cancel: crate::runtime::cancel::CancelToken) {
    tokio::spawn(forward_signals(cancel, tokio::signal::ctrl_c));
}

/// Trip `cancel` on every signal `next` delivers, until it can no longer
/// listen. Taken as a function so a test drives it with signals of its own
/// (`repl/tests.rs`); [`forward_ctrl_c`] hands it the real one.
async fn forward_signals<F, Fut>(cancel: crate::runtime::cancel::CancelToken, mut next: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<()>>,
{
    while next().await.is_ok() {
        cancel.cancel();
    }
}

/// The operator's own slash commands, the project's and the plugins', with
/// what is wrong with any of them said and the input menu told of them.
///
/// A built-in always wins; a typed custom command becomes the turn.
fn load_custom_commands(workspace: &Path) -> crate::custom_commands::Registry {
    let custom_commands = crate::custom_commands::Registry::load(workspace);
    for warning in &custom_commands.warnings {
        render::print_info(&format!("warning: {warning}"));
    }
    custom_commands.publish_menu();
    custom_commands
}

/// Everything the loop holds for the life of the session: what
/// [`Prepared`] handed over, and what the person changes as they go.
struct Repl {
    config: IncordCodeConfig,
    workspace: PathBuf,
    incode_dir: PathBuf,
    admin: crate::admin::AdminConfig,
    permission_mode: PermissionMode,
    tool_cache: ToolStatusCache,
    lifecycle_hooks: LifecycleHooks,
    git_context: Option<GitContext>,
    system_prompt: Vec<String>,
    catalogue: crate::agent::catalogue::Catalogue,
    agent: crate::agent::DetectedAgent,
    fleet: crate::fleet::Fleet,
    session: super::agent_turn::AgentSession,
    cancel: crate::runtime::cancel::CancelToken,
    /// The output style in force, by name. `/style` changes it for this
    /// session.
    output_style: Option<String>,
    /// The directories beyond the workspace. `/add-dir` adds to them.
    extra_dirs: Vec<PathBuf>,
    /// What will actually serve the work — the agent, and the model it was
    /// asked for. Naming a model nothing will answer is a header that lies.
    /// `/model` updates it when the switch actually takes effect.
    display_model: String,
    /// The session record: the id that resumes this conversation, and the
    /// memory entries that summarise it. Written on every turn because the
    /// agent only names its session after the first one.
    session_record: Option<crate::session::Record>,
    /// The session's own jobs: `/bg` and the model's background `Bash`
    /// share one table, so `/jobs` shows both and the stop hook waits on
    /// both.
    session_tools: std::sync::Arc<crate::toolbox::SessionTools>,
}

impl Repl {
    /// The loop's state over what `prepared` assembled. Prints nothing.
    fn new(
        config: IncordCodeConfig,
        prepared: Prepared,
        cancel: crate::runtime::cancel::CancelToken,
    ) -> Self {
        let Prepared {
            workspace,
            incode_dir,
            admin,
            permission_mode,
            tool_cache,
            lifecycle_hooks,
            git_context,
            system_prompt,
            output_style,
            catalogue,
            agent,
            fleet,
            session,
        } = prepared;
        let display_model = match session.model() {
            Some(model) => format!("{} via {}", model, agent.display_name),
            None => format!("{} (its own default)", agent.display_name),
        };
        let session_tools = session.session_tools();
        Self {
            extra_dirs: config.additional_dirs.clone(),
            config,
            workspace,
            incode_dir,
            admin,
            permission_mode,
            tool_cache,
            lifecycle_hooks,
            git_context,
            system_prompt,
            catalogue,
            agent,
            fleet,
            session,
            cancel,
            output_style,
            display_model,
            session_record: None,
            session_tools,
        }
    }

    /// The banner: the model and agent, the workspace, the mode and the
    /// branch.
    fn print_banner(&self) {
        render::print_banner(
            &self.display_model,
            &self.workspace.display().to_string(),
            self.permission_mode.as_str(),
            self.git_context.as_ref(),
        );
    }

    /// `--resume <id>`: the agent owns the transcript, so resuming is handing
    /// it the id back rather than replaying messages this harness stored.
    fn resume_from_flag(&mut self) {
        if let Some(id) = self.config.resume_session.clone() {
            self.session.resume(&id);
            render::print_info(&format!("Resuming agent session {id}."));
        }
    }

    /// What the session starts with, said once the input is open: the jobs
    /// table's env filter, the installed tools, and the start hook.
    fn announce_start(&self) {
        self.session_tools.jobs().env_filter_enabled = self.config.env_filter_enabled;
        if self.tool_cache.installed_count() > 0 {
            render::print_info(&self.tool_cache.summary_line());
        }
        // Run session start lifecycle hook (Feature 7)
        say_lifecycle(self.lifecycle_hooks.run_session_start(&self.workspace));
    }

    /// One line the person typed: nothing, a slash command, or a turn.
    fn on_line(
        &mut self,
        line: String,
        custom_commands: &crate::custom_commands::Registry,
        term: &mut Terminal,
    ) -> Next {
        if line.is_empty() {
            return Next::Continue;
        }
        let (line, allowed_tools) = expand_custom(custom_commands, line, &self.workspace);
        if let Some(cmd) = SlashCommand::parse(&line) {
            return self.command(cmd, term);
        }
        self.report_finished_jobs();

        // A greeting is answered here and goes no further.
        //
        // This sits at the TOP of the turn deliberately. It was first put
        // inside `run_one_turn`, which is after this call site has already
        // paid for the rules and recall lookups below — so "hi" still cost
        // two round-trips to a store that had nothing to say about it.
        // Measured at 2.07 seconds for a message that needs none of it.
        if crate::text::is_greeting(&line) {
            render::print_assistant(&crate::text::greeting_reply(&self.workspace));
            return Next::Continue;
        }
        self.turn(&line, allowed_tools, term);
        Next::Continue
    }

    /// Background jobs that finished since the last line, said before the
    /// next turn.
    fn report_finished_jobs(&self) {
        let finished = self.session_tools.jobs().poll_completed();
        for (id, output, code) in finished {
            outln!();
            if code == 0 {
                render::print_info(&format!("Job #{id} completed."));
            } else {
                render::print_info(&format!("Job #{id} finished (exit code {code})."));
            }
            if !output.is_empty() {
                render::print_dim(&format!("  {output}"));
                outln!();
            }
        }
    }

    /// One turn for the agent, then the status, the input box and the
    /// session record brought up to date.
    fn turn(&mut self, line: &str, allowed_tools: Vec<String>, term: &mut Terminal) {
        // Match CoT reasoning template + skill and inject into user message.
        // CoT = HOW to think (reasoning scaffold). Skill = WHAT to know (domain expertise).
        // Both are injected before the user's actual message.
        let mut context_parts: Vec<String> = Vec::new();

        let augmented_input = if context_parts.is_empty() {
            line.to_string()
        } else {
            context_parts.push(line.to_string());
            context_parts.join("\n\n")
        };

        // Per-turn retrieval, from the body's memory rather than a copy kept
        // here. It is semantic, so a question matches by meaning instead of
        // by shared words, and it sees what every other agent on the machine
        // has learned rather than only what this program wrote.
        //
        // Returns nothing when the store is unreachable: retrieval enriches a
        // turn, so losing it costs context, not the turn.
        //
        // The rules ride the same turn, and are fetched for the same line: the
        // service returns the always-on rules unioned with the ones that match
        // what is being asked, so which rules apply depends on the question.
        // Rules go FIRST — they are the only retrieved block the model is meant
        // to obey, and they govern how it treats the rest.
        let mut turn_context = Vec::new();
        if let Some(rules) = crate::rules::context_for_turn(line) {
            turn_context.push(rules);
        }
        // Per-turn recall is gone. Measured against the live service it
        // takes 6-8 SECONDS while the turn budget is 1.2 — so it timed out
        // every time, contributing nothing and costing 1.2s of the
        // operator's wait per message. What memory has to say is fetched
        // once per session instead, into the cached prefix.
        // A background job still running as the turn ends makes its stop
        // a wait rather than an end: the session counts them itself, at
        // that moment, from the table `/bg` and the model share.
        // Context, checkpoint, turn and streaming all live in one place
        // now, shared with the one-shot path above so the two cannot drift.
        let turn_began = std::time::Instant::now();
        // A custom command's `allowed-tools` hold the turn it starts, and
        // only that one.
        self.session.restrict_next_turn(allowed_tools);
        if let Err(e) = run_one_turn(&mut self.session, &self.workspace, &augmented_input, line) {
            render::print_error(&e);
        }
        // Lifted whether or not a turn ran: a line answered without one
        // (a reply to a question) must not pass it on to the next.
        self.session.restrict_next_turn(Vec::new());
        notify::turn_finished(turn_began.elapsed());
        term.after_turn(self);
        self.record_turn(line);
    }

    /// The session record, written after the turn.
    ///
    /// AFTER the turn, because the agent only names its session once it
    /// has opened one — asked before the first turn it answers `None`,
    /// and a one-turn session would record nothing at all.
    ///
    /// Resuming reads a cached prefix that a fresh session re-creates at
    /// roughly ten times the price, so this id is the thing worth never
    /// losing.
    fn record_turn(&mut self, line: &str) {
        let Some(id) = self.session.session_id() else {
            return;
        };
        let record = self.session_record.get_or_insert_with(|| {
            crate::session::Record::new(id, &self.workspace, &self.display_model)
        });
        record.agent_session = id.to_string();
        record.name_from(line);
        // The subagents this turn handed work to. Each is its own
        // conversation with its own id, so a run that fanned out to four
        // workers reopens as the five it really was rather than as one.
        // `spawned` ignores a repeat, so re-recording every turn costs
        // nothing and a subagent is never listed twice.
        for (child, title) in self.session.children() {
            record.spawned(&child, &title);
        }
        crate::session::save(&self.incode_dir, record);
    }

    /// The session's end: the memory engine's, the operator's end commands,
    /// the end hook, and the id that reopens the conversation.
    fn finish(&mut self) {
        // The memory engine's end of the session, bounded by the engine
        // runner, before the operator's own end-of-session command.
        self.session.end_memory("prompt_input_exit");
        for said in self.session.operator_session_end("prompt_input_exit") {
            render::print_info(&format!("[session_end hook] {said}"));
        }

        // Run session end lifecycle hook (Feature 7)
        say_lifecycle(self.lifecycle_hooks.run_session_end(&self.workspace));

        // The agent owns the transcript, so what is worth telling the user on
        // the way out is the id that reaches it again — not a file of messages
        // this harness never held, which could restore nothing.
        if let Some(id) = self.session.session_id() {
            render::print_info(&format!(
                "This conversation is agent session {id}. Reopen it with `--resume {id}`."
            ));
        }

        outln!("Goodbye!");
    }
}

/// A typed custom command expanded into the line it stands for, with the
/// tools its `allowed-tools` hold the turn to; any other line as it was.
fn expand_custom(
    custom_commands: &crate::custom_commands::Registry,
    line: String,
    workspace: &Path,
) -> (String, Vec<String>) {
    match custom_commands.expand_line(&line, workspace) {
        Some(expanded) => (
            crate::custom_commands::announce(&expanded),
            expanded.allowed_tools,
        ),
        None => (line, Vec::new()),
    }
}

/// What a lifecycle hook (Feature 7) did, when one ran.
fn say_lifecycle(result: Option<crate::runtime::lifecycle::HookResult>) {
    let Some(result) = result else {
        return;
    };
    if result.success {
        render::print_info(&format!("Lifecycle [{}]: OK", result.hook_name));
    } else {
        render::print_error(&format!(
            "Lifecycle [{}] failed: {}",
            result.hook_name,
            result.error.trim()
        ));
    }
}

/// What the input box says about this session.
///
/// The banner scrolls away within a minute; these are the facts that decide
/// whether an instruction is safe to give, so they belong where the
/// instruction is typed. Just the model — or `fallback` when the agent picks
/// its own — not the "via <agent>" the banner spells out: the rule has one
/// line and the agent does not change per turn.
fn footer_session(
    model: Option<&str>,
    fallback: &str,
    workspace: &Path,
    git: Option<&GitContext>,
) -> footer::Session {
    footer::Session {
        model: model.map_or_else(|| fallback.to_string(), str::to_string),
        workspace: workspace
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| workspace.display().to_string()),
        branch: git.map(|g| g.branch.clone()),
        dirty: git.is_some_and(|g| g.is_dirty),
    }
}

/// Where the lines come from, and what the person sees around them.
struct Terminal {
    /// The box that stays at the bottom. On a real terminal it reserves the
    /// last rows so the conversation can never scroll over it; where there
    /// is no terminal — a pipe, a CI run — there is nothing to pin and the
    /// plain editor reads the lines instead.
    footer: Option<footer::Footer>,
    editor: LineEditor,
    /// Notifications, the status line, vi mode and key bindings.
    ux: ux::Ux,
}

impl Terminal {
    /// The input box and the editor, on the history file, with the
    /// operator's preferences applied and the status line drawn.
    fn open(repl: &Repl) -> Self {
        let history_path = repl
            .incode_dir
            .join("history.txt")
            .to_string_lossy()
            .to_string();
        let mut footer = footer::Footer::open(Some(history_path.clone()));
        if let Some(f) = footer.as_mut() {
            f.set_session(footer_session(
                repl.session.model(),
                &repl.agent.display_name,
                &repl.workspace,
                repl.git_context.as_ref(),
            ));
        }
        let mut editor = LineEditor::new(Some(history_path));
        let mut ux = ux::Ux::start(&repl.admin, &repl.incode_dir, footer.as_mut(), &mut editor);
        ux.refresh_status(
            footer.as_mut(),
            &repl.session,
            &repl.workspace,
            repl.git_context.as_ref(),
        );
        Self { footer, editor, ux }
    }

    /// The next line, or `None` when the person has left (Ctrl-C, Ctrl-D).
    fn read_line(&mut self) -> Option<String> {
        match self.footer {
            Some(ref mut f) => f.read_line(),
            None => self.editor.readline("You: "),
        }
    }

    /// `/vim`: switch between vi and emacs editing for this session.
    fn toggle_vim(&mut self) {
        self.ux.toggle_vim(self.footer.as_mut(), &mut self.editor);
    }

    /// The status line and the input box redrawn after a turn, which may
    /// have changed the model or the session.
    fn after_turn(&mut self, repl: &Repl) {
        self.ux.refresh_status(
            self.footer.as_mut(),
            &repl.session,
            &repl.workspace,
            repl.git_context.as_ref(),
        );
        if let Some(f) = self.footer.as_mut() {
            f.set_session(footer_session(
                repl.session.model(),
                repl.session.backend_id(),
                &repl.workspace,
                repl.git_context.as_ref(),
            ));
        }
    }
}

#[cfg(test)]
mod tests;
