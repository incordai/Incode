//! The REPL's slash commands: which area each belongs to, and what each
//! does with the session the loop holds.
//!
//! # Wired to
//!
//! - **Called by** `Repl::on_line` in `cli::repl`, for a line
//!   [`SlashCommand::parse`] recognised.
//! - **Calls** the `slash_commands::handle_*` for most commands; the ones that
//!   change what the loop holds — the directories, the model, the session
//!   record — are handled here, where that state lives.
//! - **Shared with** `crate::acp_agent::commands`, which answers the same
//!   [`SlashCommand`] over the protocol with its own handlers.

use crate::cli::slash_commands::{self, SlashCommand};
use crate::cli::{render, review_turn, run_ledger, ux};
use crate::{out, outln};

use super::{Next, Repl, Terminal};

impl Repl {
    /// One slash command, sent to the handlers for its area. Only `/quit`
    /// ends the loop.
    ///
    /// Every command is named here, so a new one does not compile until it
    /// is given an area.
    pub(super) fn command(&mut self, cmd: SlashCommand, term: &mut Terminal) -> Next {
        use SlashCommand as C;
        match cmd {
            C::Quit => return Next::Quit,
            cmd @ (C::Doctor
            | C::AddDir(_)
            | C::Links(_)
            | C::Permissions
            | C::Guards
            | C::Agents
            | C::Roadmap
            | C::Worktree(_)
            | C::PullRequest(_)
            | C::Diff(_)
            | C::Env) => self.workspace_command(cmd),
            cmd @ (C::Undo
            | C::Rewind(_)
            | C::Image(_)
            | C::Plan
            | C::Act
            | C::Style(_)
            | C::Compact(_)
            | C::Review(_)
            | C::Status
            | C::Bug(_)
            | C::Cost) => self.conversation_command(cmd),
            cmd @ (C::Run
            | C::Admin
            | C::Agent(_)
            | C::Roles(_)
            | C::Tools(_)
            | C::Skills
            | C::Mcp
            | C::Model(_)
            | C::Models) => self.agents_command(cmd),
            cmd @ (C::Clear | C::Save(_) | C::Sessions | C::Resume(_)) => self.record_command(cmd),
            cmd @ (C::Remember(_)
            | C::Forget { .. }
            | C::Memories(_)
            | C::Jobs
            | C::Kill(_)
            | C::Background(_)) => self.memory_and_jobs_command(cmd),
            C::Vim => term.toggle_vim(),
            C::Help => slash_commands::handle_help(),
            // A real command, one word short. Say what it needs — the
            // operator is closer to success than "unknown command"
            // suggests, and /help does not name the missing part.
            C::Usage(usage) => slash_commands::handle_usage(&usage),
            C::Unknown(cmd) => {
                render::print_error(&format!("Unknown command: {cmd}. Type /help for commands."));
            }
        }
        Next::Continue
    }

    /// The workspace and the directories beside it: what is there, what is
    /// protected, and how it has changed.
    fn workspace_command(&mut self, cmd: SlashCommand) {
        use SlashCommand as C;
        match cmd {
            C::Doctor => slash_commands::handle_doctor(&self.workspace),
            C::AddDir(path) => self.add_dir(path),
            C::Links(target) => {
                slash_commands::handle_links(&self.session.roots(), target.as_deref());
            }
            C::Permissions => slash_commands::handle_permissions(&self.workspace),
            C::Guards => slash_commands::handle_guards(&self.workspace),
            C::Agents => slash_commands::handle_agents(&self.workspace),
            C::Roadmap => slash_commands::handle_roadmap(&self.workspace),
            C::Worktree(arg) => slash_commands::handle_worktree(arg.as_deref(), &self.workspace),
            C::PullRequest(arg) => {
                slash_commands::handle_pull_request(arg.as_deref(), &self.workspace);
            }
            C::Diff(arg) => slash_commands::handle_diff(arg.as_deref(), &self.workspace),
            C::Env => slash_commands::handle_env(),
            other => unreachable!("{other:?} is not a workspace command"),
        }
    }

    /// `/add-dir [path]`: work in one more directory, or list the ones
    /// beyond the workspace.
    fn add_dir(&mut self, path: Option<String>) {
        let Some(raw) = path else {
            if self.extra_dirs.is_empty() {
                render::print_info("Only the workspace. Name a directory to add one.");
            } else {
                for dir in &self.extra_dirs {
                    outln!("  {}", dir.display());
                }
            }
            return;
        };
        let dir = std::path::PathBuf::from(&raw);
        if !dir.is_dir() {
            render::print_error(&format!("`{raw}` is not a directory that exists."));
            return;
        }
        // Reach and protection extend TOGETHER: the session snapshots it for
        // `/undo` and the worker's sentinel watches it. Widening one without
        // the other is how a protected file stops being protected.
        self.extra_dirs.push(dir);
        self.session.set_extra_dirs(self.extra_dirs.clone());
        render::print_info(&format!(
            "Also working in {raw}. Guards and undo cover it too."
        ));
    }

    /// This conversation: undoing it, its mode, its style, its context, a
    /// review, and what it has cost.
    fn conversation_command(&mut self, cmd: SlashCommand) {
        use SlashCommand as C;
        match cmd {
            C::Undo => slash_commands::handle_undo(&mut self.session, &self.workspace),
            C::Rewind(turns) => {
                slash_commands::handle_rewind(&mut self.session, &self.workspace, turns);
            }
            C::Image(path) => {
                slash_commands::handle_image(&mut self.session, &self.workspace, &path);
            }
            C::Plan => slash_commands::handle_plan(&mut self.session),
            C::Act => slash_commands::handle_act(&mut self.session),
            C::Style(name) => self.style(name.as_deref()),
            C::Compact(instructions) => self.compact(instructions.as_deref()),
            C::Review(base) => {
                if let Err(why) = review_turn(&mut self.session, &self.workspace, base.as_deref()) {
                    render::print_error(&why);
                }
            }
            C::Status => self.status(),
            C::Bug(description) => slash_commands::handle_bug(
                &self.display_model,
                self.permission_mode.as_str(),
                description.as_deref(),
            ),
            C::Cost => ux::print_cost(&self.session),
            other => unreachable!("{other:?} is not a conversation command"),
        }
    }

    /// `/style [name]`: list the output styles, or switch to one.
    fn style(&mut self, name: Option<&str>) {
        if let Some(block) =
            slash_commands::handle_style(name, &self.workspace, &mut self.output_style)
        {
            self.session.set_output_style(block);
        }
    }

    /// `/compact [instructions]`: summarise the conversation now.
    ///
    /// On the API route the conversation is incode's, so it is compacted
    /// here; an agent CLI compacts its own transcript, and a second
    /// compactor would be summarising a copy it does not have.
    fn compact(&mut self, instructions: Option<&str>) {
        match self.session.compact(instructions.unwrap_or("")) {
            Some(Ok(done)) => render::print_info(&done),
            Some(Err(why)) => {
                render::print_error(&format!("The conversation was not compacted: {why}"));
            }
            None => render::print_info(
                "The agent manages its own context and compacts it automatically. Use /clear to start a fresh session.",
            ),
        }
    }

    /// `/status`: the model, the workspace, the mode, and what the session
    /// has used.
    fn status(&self) {
        slash_commands::handle_agent_status(
            &self.display_model,
            &self.workspace,
            self.permission_mode.as_str(),
            self.session.session_id(),
            self.session.turns(),
            &self.session.usage(),
            self.session.quota(),
        );
    }

    /// The agents and models doing the work, the operator's settings, and the
    /// tools the session has.
    fn agents_command(&mut self, cmd: SlashCommand) {
        use SlashCommand as C;
        match cmd {
            C::Run => self.run_ledger(),
            C::Admin => {
                // The answer to "what is this session actually going to do?",
                // with each value naming which of the four layers decided it.
                out!(
                    "{}",
                    self.admin
                        .report(&self.workspace, self.config.model_from_flag.as_deref())
                );
            }
            C::Agent(arg) => slash_commands::handle_agent(
                arg.as_deref(),
                &self.catalogue,
                &self.agent,
                &self.incode_dir,
            ),
            C::Roles(arg) => slash_commands::handle_roles(arg.as_deref(), &self.incode_dir),
            C::Tools(arg) => self.tools(arg.as_deref()),
            C::Skills => slash_commands::handle_skills_from_store(&self.incode_dir),
            C::Mcp => {
                // The model is started with only incode's own servers;
                // anything else connects through incode's toolbox.
                render::print_info(
                    "MCP servers connect through incode: an ACP client's servers join the toolbox as mcp__<server>__<tool> and run through its gate. The model's own MCP settings are not loaded.",
                );
            }
            C::Model(name) => self.model(name.as_deref()),
            C::Models => self.models(),
            other => unreachable!("{other:?} is not an agents command"),
        }
    }

    /// `/run`: work the ledger until there is a reason to stop, watched on
    /// this terminal.
    fn run_ledger(&self) {
        run_ledger::run(
            &run_ledger::RunContext {
                workspace: &self.workspace,
                data_dir: &self.incode_dir,
                admin: &self.admin,
                catalogue: &self.catalogue,
                fleet: &self.fleet,
                system_prompt: &self.system_prompt,
                extra_dirs: &self.extra_dirs,
                env_filter: self.config.env_filter_enabled,
                permission_mode: self.permission_mode,
            },
            &self.cancel,
            run_ledger::Watch::Terminal,
        );
    }

    /// `/tools [arg]`: the tools incode runs for the current agent, and the
    /// external tools — or, with an argument, a change to the external tools.
    fn tools(&self, arg: Option<&str>) {
        // The tools incode runs itself for the current agent, read from the
        // backend so the list cannot drift.
        if arg.is_none() {
            slash_commands::handle_delegated_tools(
                &self.agent.display_name,
                &self.session.offered_tools(),
            );
            render::print_info(&self.tool_cache.summary_line());
        }
        slash_commands::handle_ext_tools(arg, &self.incode_dir);
    }

    /// `/model [name]`: the model in use, or switch to another.
    fn model(&mut self, name: Option<&str>) {
        let Some(name) = name else {
            render::print_info(&format!("Model: {}", self.display_model));
            return;
        };
        // Changing the model ABANDONS the agent session: a resumed session
        // carries the earlier turns, and asking a different model to continue
        // them is a request nothing can reason about.
        self.session.choose_model(name);
        self.display_model = format!("{} via {}", name, self.agent.display_name);
        render::print_info(&format!(
            "Next turn runs on {name}. The conversation starts fresh — a different model cannot continue a transcript it never had."
        ));
    }

    /// `/models`: the models the current API route's service offers.
    fn models(&self) {
        let Some(route) = self.session.api_route() else {
            render::print_info(
                "The lead is an installed agent, which names its own models; \
                 /models lists what an API route's service offers. Choose one \
                 with /agent.",
            );
            return;
        };
        match crate::agent::models::list_models(route) {
            Ok(ids) if ids.is_empty() => {
                render::print_info(&format!("{} lists no models.", route.route));
            }
            Ok(ids) => render::print_info(&format!(
                "{} offers {} model(s) — switch with /model <id>:\n  {}",
                route.route,
                ids.len(),
                ids.join("\n  ")
            )),
            Err(why) => render::print_error(&format!(
                "{}'s model list could not be read: {why}",
                route.route
            )),
        }
    }

    /// The session record: clearing the conversation, naming it, listing
    /// the ones kept, and continuing another.
    fn record_command(&mut self, cmd: SlashCommand) {
        use SlashCommand as C;
        match cmd {
            C::Clear => self.clear(),
            C::Save(name) => self.save(name.as_deref()),
            C::Sessions => self.sessions(),
            C::Resume(name) => self.resume(name),
            other => unreachable!("{other:?} is not a session-record command"),
        }
    }

    /// The record in hand saved and let go, so the next turn opens a fresh
    /// one rather than stamping a new id over this one.
    ///
    /// Returns the record that was finished, if there was one.
    fn finish_record(&mut self) -> Option<crate::session::Record> {
        let finished = self.session_record.take()?;
        crate::session::save(&self.incode_dir, &finished);
        Some(finished)
    }

    /// `/clear`: end this conversation; the next turn starts a new one.
    fn clear(&mut self) {
        // THE OLD RECORD IS FINISHED, not reused. The write after each turn
        // does `record.agent_session = id` on whatever record is in hand, so
        // carrying this one past a clear meant the next turn stamped the NEW
        // id over the only pointer to the session just ended — it vanished
        // from `/sessions` and could never be resumed. Saved, then dropped
        // so the next turn starts a fresh one.
        if let Some(finished) = self.finish_record() {
            crate::trace!(
                "session",
                "cleared: {} kept, next turn opens a new record",
                finished.agent_session
            );
        }
        // Really cleared, not merely reported as cleared: the next turn opens
        // a NEW agent session rather than resuming the one the user just
        // asked to drop.
        self.session.clear();
        render::print_info("Conversation cleared. The next turn starts a new agent session.");
    }

    /// `/save [name]`: name this conversation, and say the id that reopens
    /// it.
    ///
    /// The agent owns the transcript, so what is worth keeping is the id
    /// that reaches it again. Writing a copy of messages this harness does
    /// not have would be a file that cannot restore anything.
    ///
    /// A NAME, though, is worth keeping, and the argument used to be
    /// discarded: `/save nightly-fix` parsed the name and threw it away, so
    /// the record stayed titled by whatever happened to be typed first.
    fn save(&mut self, name: Option<&str>) {
        if let Some(wanted) = name.map(str::trim).filter(|p| !p.is_empty()) {
            match self.session_record.as_mut() {
                Some(record) => {
                    record.rename(wanted);
                    crate::session::save(&self.incode_dir, record);
                    render::print_info(&format!("Named this session `{}`.", record.title));
                }
                None => render::print_info(
                    "Nothing to name yet — the record is written after the first turn.",
                ),
            }
        }
        match self.session.session_id() {
            Some(id) => render::print_info(&format!(
                "This conversation is agent session {id}. Reopen it with `--resume {id}`."
            )),
            None => render::print_info(
                "Nothing to save yet — the agent opens a session on the first turn.",
            ),
        }
    }

    /// `/sessions`: the conversations kept for this workspace, or where the
    /// others are when it has none.
    ///
    /// This said "this harness keeps none to list", which was true when it
    /// was written and stopped being true the moment the session store
    /// landed. The agent still owns the transcripts; what is listed here is
    /// the pointers to them, which is the part that reopens one.
    fn sessions(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let records = crate::session::for_workspace(&self.incode_dir, &self.workspace);
        for line in crate::session::summary_lines(&records, now) {
            render::print_info(&line);
        }
        // The list is filtered to THIS workspace, so an empty one read as
        // "nothing was ever saved" while records for other checkouts sat in
        // the same file. Say where they are: the id is what `--resume` takes,
        // and an operator who thinks it is gone starts fresh and re-pays the
        // cached prefix.
        if records.is_empty() {
            let others = crate::session::elsewhere(&self.incode_dir, &self.workspace);
            for line in crate::session::elsewhere_lines(&others) {
                render::print_info(&line);
            }
        }
        if let Some(id) = self.session.session_id() {
            render::print_info(&format!("This conversation is {id}."));
        }
    }

    /// `/resume <id>`: the next turn continues another agent session.
    fn resume(&mut self, name: Option<String>) {
        let Some(id) = name else {
            render::print_info("Name the agent session to continue, e.g. `/resume <id>`.");
            return;
        };
        // Same reason as `/clear`: the conversation in hand is being left for
        // a different one, so its record is finished here rather than having
        // the resumed id stamped over it on the next turn.
        self.finish_record();
        self.session.resume(&id);
        crate::trace!("session", "resuming {id}");
        render::print_info(&format!("Next turn continues agent session {id}."));
    }

    /// The shared memory, and the session's background jobs.
    fn memory_and_jobs_command(&mut self, cmd: SlashCommand) {
        use SlashCommand as C;
        match cmd {
            C::Remember(text) => slash_commands::handle_remember(&text, &self.workspace),
            C::Forget { id, confirmed } => slash_commands::handle_forget(&id, confirmed),
            C::Memories(query) => slash_commands::handle_memories(query.as_deref()),
            C::Jobs => slash_commands::handle_jobs(&mut self.session_tools.jobs()),
            C::Kill(id) => self.kill(id),
            C::Background(cmd) => self.background(&cmd),
            other => unreachable!("{other:?} is not a memory or jobs command"),
        }
    }

    /// `/kill <id>`: stop a background job.
    fn kill(&self, id: u32) {
        if self.session_tools.jobs().kill(id) {
            render::print_info(&format!("Job #{id} killed."));
        } else {
            render::print_error(&format!("Job #{id} not found or already finished."));
        }
    }

    /// `/bg <command>`: start a background job.
    ///
    /// The operator's own command, not the model's: run as they typed it,
    /// unconfined.
    fn background(&self, cmd: &str) {
        match self
            .session_tools
            .jobs()
            .spawn(cmd, &self.workspace, &crate::sandbox::Sandbox::off())
        {
            Ok(id) => render::print_info(&format!("Job #{id} started: {cmd}")),
            Err(e) => render::print_error(&format!("Failed to start job: {e}")),
        }
    }
}
