//! CLI module — interactive REPL for `incord code`.

pub mod activity;
pub mod agent_turn;
pub mod console;
pub mod edits;
pub mod footer;
pub mod input;
pub mod logo;
pub mod markdown;
pub mod paint;
pub mod panel;
pub mod prompt;
pub mod render;
pub mod slash_commands;
pub mod viewport;

use crate::{out, outln};
use std::path::Path;

use crate::config::IncordCodeConfig;
use crate::permissions::PermissionMode;
use crate::runtime::lifecycle::LifecycleHooks;
use crate::tools::git_context;
use crate::tools::tool_status::ToolStatusCache;

use input::LineEditor;
use slash_commands::SlashCommand;

// ---------------------------------------------------------------------------
// TerminalPrompter
// ---------------------------------------------------------------------------

/// Time one startup step, when the operator asks to see where the time goes.
///
/// Reports under `INCORD_TRACE` in the `startup` area. A startup that takes
/// seventeen seconds with five candidate causes is not diagnosable by reading;
/// this makes it name itself.
fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
    crate::trace::timed("startup", label, f)
}

/// Cumulative elapsed at a named point in startup, under `INCORD_TRACE`.
fn mark(label: &str, since: std::time::Instant) {
    crate::trace!("startup", "@{label} at {:?}", since.elapsed());
}

// ---------------------------------------------------------------------------
// CliApp
// ---------------------------------------------------------------------------

/// The main REPL application for `incord code`.
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

/// Everything [`prepare_session`] assembles, handed to whoever drives the
/// session: the interactive REPL here, or `incode --acp` in
/// [`crate::acp_agent`].
pub(crate) struct Prepared {
    pub(crate) workspace: std::path::PathBuf,
    pub(crate) incord_code_dir: std::path::PathBuf,
    pub(crate) admin: crate::admin::AdminConfig,
    pub(crate) permission_mode: PermissionMode,
    pub(crate) tool_cache: ToolStatusCache,
    pub(crate) lifecycle_hooks: LifecycleHooks,
    pub(crate) git_context: Option<git_context::GitContext>,
    pub(crate) system_prompt: Vec<String>,
    pub(crate) catalogue: crate::agent::catalogue::Catalogue,
    pub(crate) agent: crate::agent::DetectedAgent,
    pub(crate) fleet: crate::fleet::Fleet,
    pub(crate) session: crate::cli::agent_turn::AgentSession,
}

/// Build a session the way every front end must: the intake gate, the
/// operator's settings, the project context the agent is told, the lead from
/// the catalogue, the guards, the standby agents, the permission mode and the
/// model.
///
/// # Why this is one function
///
/// The REPL built all of this inline, and `incode --acp` needs exactly the same
/// session. A second copy of this sequence would drift from the first the way
/// the run backend's system context did — twice — so both front ends call this.
///
/// `cancel` is the token the caller trips to stop a turn: Ctrl-C in the REPL,
/// `session/cancel` over the protocol.
///
/// Everything it prints goes through [`render`], i.e. the console. Under
/// `--acp` the process's stdout is already pointed at stderr by the time this
/// runs (see `acp_agent::stdio`), so none of it can reach the protocol channel.
///
/// `session_id` is incode's id for the session — the ACP session's own id, or
/// one the REPL made for itself — and the memory engine files the session
/// under it.
pub(crate) fn prepare_session(
    config: &IncordCodeConfig,
    cancel: crate::runtime::cancel::CancelToken,
    booted_at: std::time::Instant,
    session_id: &str,
) -> anyhow::Result<Prepared> {
    let workspace = config.workspace.clone();
    // The intake gate. Nothing is blocked by it: an incomplete brief makes
    // completing the brief the session's first job, not a reason to exit.
    // The work still happens here, in this session, with the owner present
    // to answer what only they can answer.
    // HEAL THE BELIEF BEFORE REPORTING IT MISSING.
    //
    // The brief lives in the proxy, and the proxy's write endpoint replaces
    // a whole document with no version to write against — so a belief CAN be
    // lost to a concurrent writer, and one was, live: the project key
    // survived with every field blank.
    //
    // The documents are the durable copy. Deriving here means a lost brief
    // is rebuilt by the next session that opens the project, rather than
    // waiting for someone to type `/run` — which was the gap when this ran
    // only in the run handler: the brief stayed empty and the session merely
    // complained about it.
    //
    // Nothing is overwritten: `from_docs` carries only what the document
    // says, and `patch_config` writes only the fields an update carries.
    let gate = timed("brief::check", || {
        let gate = crate::brief::check(&workspace);
        if let crate::brief::Gate::Ready(ref brief) = gate {
            // THE OTHER DIRECTION. A belief with no document behind it is
            // one clobber from gone, because healing rebuilds from the
            // document and there is none. Writing it down means the truth
            // lives in two places and neither is the only copy.
            //
            // An existing document is never touched: it is the source, and a
            // drifted brief overwriting it would make the drift the truth.
            match crate::brief::write_prd_to_docs(&workspace, brief) {
                    Ok(true) => render::print_info(&format!(
                        "Wrote {} from the brief — this project had a belief and no document behind it.",
                        crate::brief::PRD_PATH
                    )),
                    Ok(false) => {}
                    Err(e) => crate::trace!("brief", "document not written: {e}"),
                }
            return gate;
        }
        match crate::brief::derive_from_docs(&workspace) {
            Ok(_) => {
                crate::trace!("brief", "derived from {}", crate::brief::PRD_PATH);
                crate::brief::check(&workspace)
            }
            // No document to derive from is the ordinary case for a new
            // project. The gate below then asks for the brief, which is the
            // useful thing to say — this error is not.
            Err(e) => {
                crate::trace!("brief", "not derived: {e}");
                gate
            }
        }
    });
    // Two intake findings, gathered together so the session is told
    // everything that needs writing before it starts, rather than one thing
    // now and another after the first is fixed.
    //
    // The dependency check needs the workspace AND the document, which only
    // meet here: `research` reads the manifest the build reads, and the PRD
    // comes from the brief.
    let mut intake_notes: Vec<String> = gate.intake_task().into_iter().collect();
    mark("intake_task", booted_at);
    if let crate::brief::Gate::Ready(ref brief) = gate {
        if let Some(note) =
            crate::research::intake_note(&crate::research::unvetted(&workspace, &brief.prd))
        {
            intake_notes.push(note);
        }
    }
    // The rest of the spec set — architecture, contracts, conventions,
    // verification, assumptions. The loop reads these; a PRD on its own
    // leaves every later task guessing at names and shapes.
    if let Some(note) = crate::spec::intake_note(&workspace) {
        intake_notes.push(note);
    }
    mark("spec-note", booted_at);
    let intake_task = (!intake_notes.is_empty()).then(|| intake_notes.join("\n\n"));
    mark("intake-joined", booted_at);
    if let Some(ref task) = intake_task {
        render::print_info(task);
    }

    let permission_mode = PermissionMode::from_str_lossy(&config.permission_mode);
    // PUBLISHED FOR THE REST OF THE PROCESS, so a backend built somewhere
    // that cannot see this config still runs under the mode the operator
    // chose. See `PERMISSION_MODE_ENV` for why this is a variable rather
    // than an argument, and why it stops at this process boundary.
    std::env::set_var(PERMISSION_MODE_ENV, permission_mode.as_str());

    // Incord Code data directory (history, sessions, memory)
    // One resolver for the data directory, shared with every caller that
    // reads settings from it — two answers here means the operator edits one
    // file and the harness reads another.
    let incord_code_dir = crate::admin::AdminConfig::default_dir();
    let _ = std::fs::create_dir_all(&incord_code_dir);

    // The operator's standing decisions. A flag beats an env var beats this
    // file beats the built-in fallback, and every value can say which of the
    // four decided it — see `/admin`.
    let admin = match timed("admin", || {
        mark("admin", booted_at);
        crate::admin::AdminConfig::load(&incord_code_dir)
    }) {
        Ok(admin) => admin,
        Err(e) => {
            // Settings that quietly stop applying are worse than a loud
            // failure: the run looks normal while doing something else.
            render::print_error(&e);
            crate::admin::AdminConfig::default()
        }
    };

    // START THE ADVISORY LOOKUPS NOW; READ THEM WHEN THE PROMPT IS BUILT.
    //
    // Both of these used to run where their answers are used, on the
    // [`crate::recall::WAITING_TIMEOUT`] of 1200ms, and both were silently
    // losing to it: measured from the service's own request log,
    // `/v1/memory/watchman/handoff` exceeded 1200ms on 198 of 506 calls —
    // 39.1% — every one of them answering `status=200`. The store was
    // working and the harness had already stopped listening, so two sessions
    // in five started believing a project with real recorded history had
    // none.
    //
    // Started here they overlap the six-odd seconds of startup that follow —
    // tool probing, git, plugins, agent discovery — and are read at the end
    // of it. The deadline is not raised on the waiting path, because the
    // reason it is 1200ms there has not changed: an 8.08s lookup once left
    // the operator staring at a terminal that looked dead. Nothing waits on
    // these, so nothing can be held up by them.
    //
    // Gated on the same plugin switches as before, so an operator who turned
    // one off still pays nothing for it.
    // The project's stable id, taken from the brief the gate already
    // resolved — no extra call. Empty when the service has assigned none,
    // and `recorder::key_for` then falls back to the workspace path.
    let recorder_scope = match gate {
        crate::brief::Gate::Ready(ref brief) if !brief.scope.is_empty() => brief.scope.clone(),
        _ => {
            let norm = workspace.display().to_string().replace('\\', "/");
            if norm.ends_with("/incord-code") || norm == "incord-code" {
                "proj-incordcode".to_string()
            } else {
                String::new()
            }
        }
    };
    let recorder_prefetch = admin.plugins.recorder.then(|| {
        let workspace = workspace.clone();
        let scope = recorder_scope.clone();
        std::thread::spawn(move || {
            crate::recorder::context_for_session_within(
                &workspace,
                Some(scope.as_str()),
                crate::recall::PREFETCH_TIMEOUT,
            )
        })
    });
    let auditor_prefetch = admin.plugins.auditor.then(|| {
        let workspace = workspace.clone();
        std::thread::spawn(move || {
            crate::auditor::context_for_session_within(&workspace, crate::recall::PREFETCH_TIMEOUT)
        })
    });

    // Tool status cache (Feature 3: detect installed CLI tools)
    let tool_cache_path = incord_code_dir.join("tool-status.json");
    let mut tool_cache = timed("tool probe", || {
        mark("tools", booted_at);
        ToolStatusCache::load_or_create(&tool_cache_path)
    });
    tool_cache.detect_all();

    // Lifecycle hooks (Feature 7: session start/end scripts)
    let lifecycle_hooks = if config.lifecycle_hooks.has_any() {
        config.lifecycle_hooks.clone()
    } else {
        LifecycleHooks::discover(&workspace).unwrap_or_default()
    };

    // Load skills (72 bundled + user skills from workspace)
    let _skill_count = 0usize;

    let git_context = timed("git", || git_context::detect_git_context(&workspace));
    mark("git", booted_at);

    // What the agent is told about this project, beyond each instruction.
    //
    // ONLY what this harness knows and the agent does not. It used to also
    // carry a role ("You are Incord Code"), a methodology, a thinking style
    // and a manual for tools — all written to drive a raw model. Appended to
    // a working agent's own prompt, that is a competing identity and a
    // manual for tools it does not have, which is worse than sending
    // nothing. The agent is already a coding agent; it needs the PROJECT.
    let mut system_prompt: Vec<String> = Vec::new();
    // The blocks below that the memory engine's session-start block carries
    // too — the rules, the recorder's handoff, the auditor's findings. Kept
    // in the prompt, and named here, so that when the engine answers they
    // are dropped rather than sent twice (see `AgentSession::start_memory`).
    let mut native: Vec<String> = Vec::new();

    // How to put a decision to the owner as OPTIONS rather than an open
    // question. Enumerating the choices is most of the work of deciding,
    // and the agent has just done it — an open question throws it away.
    system_prompt.push(crate::ask::prompt_block());

    // The project's own instruction files. An agent CLI no longer reads them
    // for itself — it runs as a model only — and no other model ever did.
    if let Some(block) = crate::instructions::project_block(&workspace) {
        system_prompt.push(block);
    }

    // The intake task rides the system prompt rather than one turn's
    // context: it is not finished until the brief is, which may take several
    // exchanges, and a task that fell out of context after one turn would be
    // abandoned halfway through gathering.
    if let Some(ref task) = intake_task {
        system_prompt.push(format!(
            "<intake>\nThis project cannot be worked on until its brief is \
                 complete. That is your first job, ahead of any code.\n\n{task}\n</intake>"
        ));
    }

    // What this project has already been observed to have solved, from the
    // recorder in the body's memory. Once at session start rather than per
    // turn: it describes the project, not the question, so re-fetching it
    // every turn would pay for the same block over and over — which is the
    // exact waste the recorder exists to stop.
    //
    // Silent when the recorder is unreachable. It enriches a session; losing
    // it costs context, not the session.
    // What the owner has reserved for their own decision. Read from the body,
    // where the categories are the owner's standing policy across every
    // project and every agent — not restated here, and not classified here
    // either: the service's own hook does that for any agent on this machine.
    // Telling the model in advance saves the turn it would otherwise spend
    // discovering a stop.
    mark("before-plugins", booted_at);
    // The rules that apply to every action, fetched once. They were parsed
    // and dropped before this, so the agent never saw them at all; in the
    // cached prefix they cost almost nothing after the first turn.
    if let Some(block) = timed("rules (always)", crate::rules::session_context) {
        native.push(block.clone());
        system_prompt.push(block);
    }

    // The skills this machine has, as an INDEX: what exists and what each is
    // for, never the bodies. The store holds the operator's own skills and
    // the bundles installed under other agents' folders, so this is the same
    // set every agent here sees rather than a copy kept per checkout.
    //
    // Bodies are deliberately left out. Measured against the running store,
    // fifty-five installed skills came to 21,323 characters of names and
    // descriptions alone; carrying their text as well would spend the budget
    // on skills the work never touches. One is read when it is asked for.
    if let Some(block) = timed("skills", || {
        crate::skills::session_context(&incord_code_dir)
    }) {
        system_prompt.push(block);
    }

    if admin.plugins.guards {
        if let Some(block) = timed("guards", crate::guards::context_for_session) {
            system_prompt.push(block);
        }
    }

    // What the auditor already knows is wrong here. Same shape as the
    // recorder block and for the same reason: rediscovering a filed bug
    // costs a turn, and the finding is already written down.
    if let Some(prefetch) = auditor_prefetch {
        // Started before the tool probe; collected here. A panicked thread
        // is treated as no findings, never as an excuse to fetch again on
        // the waiting path — that is the deadline this exists to escape.
        if let Some(block) = timed("auditor", || prefetch.join().ok().flatten()) {
            render::print_info(&format!(
                "auditor: {} open finding(s) in this project",
                block.lines().count().saturating_sub(1)
            ));
            native.push(block.clone());
            system_prompt.push(block);
        }
    }

    if let Some(prefetch) = recorder_prefetch {
        if let Some(block) = timed("recorder", || prefetch.join().ok().flatten()) {
            // Said out loud, for the same reason the fleet and the ceiling
            // are: context the operator cannot see is context they cannot
            // correct.
            if block.contains("Already solved in this project") {
                render::print_info(&format!(
                    "recorder: {} already solved in this project",
                    block.lines().count().saturating_sub(1)
                ));
            } else {
                render::print_info(
                        "recorder: no past handoff recorded yet (live session facts capture real-time state)",
                    );
            }
            native.push(block.clone());
            system_prompt.push(block);
        }
    }

    // Read BEFORE the provider client is built. These lines tell the
    // operator what the fleet resolved to and warn about a reviewer
    // sharing the writer's lineage — and a diagnostic printed after a
    // failed client build is one nobody ever sees.
    // Which model serves which kind of delegated work. Read once here so the
    // fleet is visible at startup rather than inferred from a bill later.
    let roles = admin.resolve_roles().value;
    if !roles.is_empty() {
        render::print_info(&roles.summary_line());
    }
    // A ceiling nobody can see is a ceiling nobody plans around.
    // A reviewer sharing the writer's lineage reads as independent and is
    // not. Warned loudly rather than refused: the operator may know
    // something the harness does not, but they should know they did it.
    if let Some(warning) = roles.review_independence_warning() {
        render::print_error(&warning);
    }
    let budget = admin.resolve_budget().value;
    if let Some(line) = budget.summary_line() {
        render::print_info(&line);
    }
    // The orchestrator cannot choose a role it has not been told about. This
    // block carries both the choices and the rule that a role never settles
    // whether the work is correct.
    if let Some(block) = roles.prompt_block() {
        system_prompt.push(block);
    }
    // The agent that will do the work.
    //
    // TWO ROUTES, and this harness keeps the judging on both — the ledger,
    // the check, the guards, the contracts, the budget, the owner channel.
    //
    // An installed agent on the operator's own subscription brings its own
    // tools and needs no credential here. A model over a configured
    // endpoint brings nothing, so the harness supplies the tools itself
    // (see `crate::toolbox`) and the endpoint comes from the environment.
    //
    // NEITHER means no way to do anything, which is fatal and said plainly
    // rather than discovered at the first prompt.
    //
    // Both routes are in ONE catalogue, read once here and kept for
    // `/agent` and `/run`: the lead is the operator's choice when usable,
    // else the first usable local agent, else a usable API agent.
    let catalogue = timed("agent catalogue", || {
        crate::agent::catalogue::Catalogue::gather(&admin.api_providers)
    });
    let agent = catalogue
        .lead(crate::agent::registry::preferred_agent().as_deref())
        .map_err(|e| anyhow::anyhow!(e))?;
    // Everything installed and drivable. With more than one, roles can route
    // between them and work can move when one runs out.
    let fleet = timed("agent discovery", || {
        mark("fleet", booted_at);
        crate::fleet::Fleet::new(crate::agent::discover())
    });
    render::print_info(&fleet.summary_line());

    // The protected paths reach the agent as deny rules it enforces itself,
    // so a write to one never happens — prevention, with the sentinel in
    // `agent::worker` behind it for whatever the rules did not cover.
    mark("before-guards", booted_at);
    let session_guards = crate::guards::Guards::load(&workspace).map_err(|e| {
            anyhow::anyhow!(
                "the protected-path policy could not be read ({e}). Running without it would leave the check editable by the thing being judged."
            )
        })?;

    let standby: Vec<Box<dyn crate::agent::AgentBackend>> = fleet
        .agents()
        .iter()
        .filter(|a| a.id != agent.id)
        .map(|other| {
            run_backend(
                &workspace,
                &other.id,
                &other.command,
                &session_guards,
                &system_prompt,
            )
        })
        .collect();
    if !standby.is_empty() {
        render::print_info(&format!(
            "{} standby agent(s) available if this one cannot do the work.",
            standby.len()
        ));
    }

    let mut session = crate::cli::agent_turn::AgentSession::new(
        workspace.clone(),
        &agent.id,
        agent.command.clone(),
        &session_guards,
        // Everything assembled above — the intake task, the guard rules, the
        // auditor's findings, the recorder's handoff, the role table. This
        // was built and DROPPED when the harness stopped owning the loop:
        // it compiled, the tests passed, and the agent silently stopped
        // being told any of it.
        system_prompt.clone(),
    )
    .with_standby(standby)
    .with_cancel(cancel.clone())
    .with_extra_dirs(config.additional_dirs.clone());
    if !config.additional_dirs.is_empty() {
        render::print_info(&format!(
            "Also working in {} additional director(ies) — guards and undo cover them.",
            config.additional_dirs.len()
        ));
    }
    // The mode the operator asked for, sent to the thing that does the
    // writing. Shown in the banner and never sent, `-p read-only` let an
    // operator believe nothing could be written while the agent wrote.
    session.set_permission_mode(permission_mode.agent_mode());

    // The memory engine's session, opened by incode: the agent CLI used to
    // fire these hooks from its own settings, and it runs as a model only now.
    // Its session-start context replaces the native blocks it overlaps.
    let memory = crate::hooks::memory::MemoryHooks::new(
        crate::hooks::engine::Engine::locate(),
        &workspace,
        session_id,
    );
    let source = if config.resume_session.is_some() {
        "resume"
    } else {
        "startup"
    };
    timed("memory session-start", || {
        session.start_memory(memory, native, source);
    });

    // `-m` first, then the role table. A model named on the command line is
    // the most specific thing the operator said, and it was previously
    // reported by `/admin` while the session ran on something else.
    match config.model_from_flag.as_deref() {
        Some(model) => session.set_models(&[model.to_string()]),
        None => {
            // Role routing applies to interactive work too: the model the
            // session opens on is the one chosen for conversation.
            let session_models = roles.candidates_for("plan");
            if !session_models.is_empty() {
                session.set_models(session_models);
            }
        }
    }

    Ok(Prepared {
        workspace,
        incord_code_dir,
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
    })
}

impl CliApp {
    /// The interactive loop over a session [`prepare_session`] built.
    fn repl(
        self,
        prepared: Prepared,
        cancel: crate::runtime::cancel::CancelToken,
        booted_at: std::time::Instant,
    ) -> anyhow::Result<()> {
        let Prepared {
            workspace,
            incord_code_dir,
            admin,
            permission_mode,
            tool_cache,
            lifecycle_hooks,
            git_context,
            system_prompt,
            catalogue,
            agent,
            fleet,
            mut session,
        } = prepared;
        let turn_cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    break;
                }
                turn_cancel.cancel();
            }
        });
        let mut extra_dirs = self.config.additional_dirs.clone();

        // Tell the memory store which conversation and workspace it is serving.
        // Done here rather than at load time because the conversation id only
        // exists once the session has been built or resumed above. Without it
        // the store is a single file shared by every project on the machine,
        // and retrieval keyed on a file name matches other projects' notes
        // about their own `mod.rs`.
        // Print banner (show friendly model name for local models)
        // Mutable: `/model` updates it when the switch actually takes effect.
        // What will actually serve the work — the agent, and the model it was
        // asked for. Naming a model nothing will answer is a header that lies.
        let mut display_model = match session.model() {
            Some(model) => format!("{} via {}", model, agent.display_name),
            None => format!("{} (its own default)", agent.display_name),
        };
        render::print_banner(
            &display_model,
            &workspace.display().to_string(),
            permission_mode.as_str(),
            git_context.as_ref(),
        );
        // History file
        let history_path = incord_code_dir
            .join("history.txt")
            .to_string_lossy()
            .to_string();

        // The agent owns the transcript, so resuming is handing it the id back
        // rather than replaying messages this harness stored.
        if let Some(id) = self.config.resume_session.clone() {
            session.resume(&id);
            render::print_info(&format!("Resuming agent session {id}."));
        }

        // The box that stays at the bottom. On a real terminal it reserves the
        // last rows so the conversation can never scroll over it; where there
        // is no terminal — a pipe, a CI run — there is nothing to pin and the
        // plain editor reads the lines instead.
        let mut footer = footer::Footer::open(Some(history_path.clone()));
        // What the box says about this session. The banner scrolls away within
        // a minute; these are the facts that decide whether an instruction is
        // safe to give, so they belong where the instruction is typed.
        if let Some(f) = footer.as_mut() {
            f.set_session(footer::Session {
                // Just the model, not the "via <agent>" the banner spells out —
                // the rule has one line and the agent does not change per turn.
                model: session
                    .model()
                    .map(str::to_string)
                    .unwrap_or_else(|| agent.display_name.clone()),
                workspace: workspace
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| workspace.display().to_string()),
                branch: git_context.as_ref().map(|g| g.branch.clone()),
                dirty: git_context.as_ref().is_some_and(|g| g.is_dirty),
            });
        }
        let mut editor = LineEditor::new(Some(history_path));
        // The session's own jobs: `/bg` and the model's background `Bash`
        // share one table, so `/jobs` shows both and the stop hook waits on
        // both.
        let session_tools = session.session_tools();
        session_tools.jobs().env_filter_enabled = self.config.env_filter_enabled;

        // Print tool status summary
        if tool_cache.installed_count() > 0 {
            render::print_info(&tool_cache.summary_line());
        }

        // Run session start lifecycle hook (Feature 7)
        if let Some(result) = lifecycle_hooks.run_session_start(&workspace) {
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

        // REPL loop. Ends when the editor returns nothing — Ctrl-C or Ctrl-D.
        crate::trace!("startup", "first prompt after {:?}", booted_at.elapsed());
        // The session record: the id that resumes this conversation, and the
        // memory entries that summarise it. Written on every turn because the
        // agent only names its session after the first one.
        let mut session_record: Option<crate::session::Record> = None;

        while let Some(line) = match footer {
            Some(ref mut f) => f.read_line(),
            None => editor.readline("You: "),
        } {
            if line.is_empty() {
                continue;
            }

            // Check for slash commands
            if let Some(cmd) = SlashCommand::parse(&line) {
                match cmd {
                    SlashCommand::Quit => break,
                    SlashCommand::Doctor => {
                        slash_commands::handle_doctor(&workspace);
                    }
                    SlashCommand::AddDir(path) => match path {
                        Some(raw) => {
                            let dir = std::path::PathBuf::from(&raw);
                            if dir.is_dir() {
                                // Reach and protection extend TOGETHER: the
                                // session snapshots it for `/undo` and the
                                // worker's sentinel watches it. Widening one
                                // without the other is how a protected file
                                // stops being protected.
                                extra_dirs.push(dir);
                                session = session.with_extra_dirs(extra_dirs.clone());
                                render::print_info(&format!(
                                    "Also working in {raw}. Guards and undo cover it too."
                                ));
                            } else {
                                render::print_error(&format!(
                                    "`{raw}` is not a directory that exists."
                                ));
                            }
                        }
                        None => {
                            if extra_dirs.is_empty() {
                                render::print_info(
                                    "Only the workspace. Name a directory to add one.",
                                );
                            } else {
                                for dir in &extra_dirs {
                                    outln!("  {}", dir.display());
                                }
                            }
                        }
                    },
                    SlashCommand::Links(target) => {
                        slash_commands::handle_links(&session.roots(), target.as_deref());
                    }
                    SlashCommand::Guards => {
                        slash_commands::handle_guards(&workspace);
                    }
                    SlashCommand::Undo => {
                        slash_commands::handle_undo(&mut session, &workspace);
                    }
                    SlashCommand::Roadmap => {
                        slash_commands::handle_roadmap(&workspace);
                    }
                    SlashCommand::Run => {
                        // The loop is where an unspecified project costs the
                        // most: it runs unattended, so nobody is present to
                        // notice it building the wrong thing. Refused outright
                        // rather than started and stopped at the first write.
                        if let Some(note) = crate::spec::intake_note(&workspace) {
                            render::print_error(
                                "The autonomous loop will not start on a project that is not specified yet.",
                            );
                            render::print_info(&note);
                            continue;
                        }
                        // The autonomous loop. Built from the roadmap the owner
                        // approved, so the harness can only work on what was
                        // actually planned.
                        // Planning is for a ledger that has just been derived
                        // from the roadmap. One resumed from disk was planned on
                        // the run that created it, and re-planning it would
                        // spend a turn to overwrite work already in progress.
                        let mut freshly_derived = false;
                        let mut ledger = match crate::ledger::Ledger::load(&workspace) {
                            Ok(existing) if !existing.tasks.is_empty() => existing,
                            Ok(_) => {
                                // THE DOCUMENT ALREADY SAYS WHAT THIS IS.
                                // The spec gate refused to let a line be
                                // written until docs/PRD.md existed, so by
                                // the time a run is possible the belief is
                                // already written down. Asking a person to
                                // type it again into a different store is
                                // asking them to keep two copies of one
                                // truth in step by hand.
                                //
                                // Only what the document does not cover is
                                // then asked for, and a brief a person wrote
                                // is never overwritten: `from_docs` carries
                                // only the fields the document actually has.
                                // Asked as the QUESTION rather than matched as
                                // a shape. `Gate::may_code` is the one place
                                // that decides whether work may start, and it
                                // had no caller — so this read as "is it the
                                // Ready variant", which is true today and is
                                // not the rule. A variant added later would
                                // fall through this match silently.
                                if !crate::brief::check(&workspace).may_code() {
                                    match crate::brief::derive_from_docs(&workspace) {
                                        Ok(still_missing) if still_missing.is_empty() => {
                                            render::print_info(
                                                "Brief derived from this project's own documents.",
                                            );
                                        }
                                        Ok(still_missing) => render::print_info(&format!(
                                            "Derived what the documents cover; still missing: {}",
                                            still_missing
                                                .iter()
                                                .map(|a| a.field())
                                                .collect::<Vec<_>>()
                                                .join(", ")
                                        )),
                                        // Not fatal: the gate below says what
                                        // is missing and how to write it,
                                        // which is more use than this error.
                                        Err(e) => crate::trace!("brief", "not derived: {e}"),
                                    }
                                }
                                match crate::brief::check(&workspace) {
                                    crate::brief::Gate::Ready(brief) => {
                                        freshly_derived = true;
                                        crate::ledger::Ledger::from_roadmap(&brief.roadmap)
                                    }
                                    other => {
                                        render::print_error(&other.intake_task().unwrap_or_else(
                                            || "no roadmap to work from".to_string(),
                                        ));
                                        continue;
                                    }
                                }
                            }
                            Err(e) => {
                                render::print_error(&e);
                                continue;
                            }
                        };

                        if ledger.tasks.is_empty() {
                            render::print_info("Nothing to run: the roadmap has no open items.");
                            continue;
                        }
                        render::print_info(&format!(
                            "Running {} task(s). Ctrl-C stops at the next task.",
                            ledger.tasks.len()
                        ));

                        let stop_flag = cancel.clone();
                        // A Ctrl-C pressed at the prompt, or a stop that ended
                        // an earlier run, left the flag set; without a reset
                        // this run would stop before its first task.
                        cancel.reset();
                        // The interface's stop button: `--serve` is another
                        // process, so it writes a request and this watches for
                        // it for as long as the run lasts.
                        let stop_watch = cancel
                            .cancel_on_request(crate::runtime::cancel::stop_path_in(&workspace));
                        if stop_watch.cleared_stale() {
                            render::print_info(
                                "An old stop request from the interface was cleared: it was \
                                 made while nothing was running.",
                            );
                        }
                        // The ceiling the operator set, from wherever they set it
                        // — not the environment alone, which would ignore the
                        // admin file and run unbounded while `/admin` claimed a
                        // limit was in force.
                        let mut run_budget = admin.resolve_budget().value;
                        // A question the runner must not answer itself goes to
                        // the owner's queue, where the proxy answers it as them
                        // or escalates to the person.
                        // A plugin like the rest. Switched off, the runner stops
                        // at a question instead of asking — deliberately NOT
                        // "another model decides", which is how both earlier
                        // loops failed.
                        let mut proxy_owner = crate::runner::MemoryOwner::default();
                        let mut no_owner = crate::runner::NoOwner;
                        let owner: &mut dyn crate::runner::OwnerChannel = if admin.plugins.owner {
                            &mut proxy_owner
                        } else {
                            &mut no_owner
                        };
                        // An installed, already-authenticated agent is used when
                        // there is one: it owns its own loop and its own tools,
                        // and it runs on the operator's subscription rather than
                        // on an API credential this harness would have to hold.
                        //
                        // The harness still runs `verify` itself, in its own
                        // process, so the law is unchanged. What it does NOT get
                        // is `guards` — the agent's file tools are its own. See
                        // `agent::worker`.
                        // Finished work is written to the shared store under
                        // the AGENT's id, so "what has this worker done?" is a
                        // lookup rather than a scan of prose. A store that is
                        // down costs the note and never the work — see
                        // `runner::WorkJournal`.
                        let mut journal = crate::runner::MemoryJournal;
                        // The lead from the session's catalogue, by the choice
                        // as it stands NOW — so a `/agent` made in this session
                        // applies from the next `/run`.
                        let agent = match catalogue
                            .lead(crate::agent::registry::preferred_agent().as_deref())
                        {
                            Ok(found) => found,
                            Err(why) => {
                                render::print_error(&why);
                                continue;
                            }
                        };
                        // THE AGENT THAT WORKS THE TASKS: the lead's sub-agent,
                        // which is the lead itself unless the operator chose
                        // another (`/agent sub <id>`) or the built-in table
                        // pairs it with one. Read fresh for the same reason.
                        let sub = catalogue.sub_agent(
                            &agent,
                            crate::admin::AdminConfig::load_default()
                                .sub_agent
                                .as_deref(),
                        );
                        if let Some(note) = &sub.note {
                            render::print_info(note);
                        }
                        let worker_agent = sub.agent;
                        // THE ROLES AS THEY STAND NOW, read fresh for the same
                        // reason: `admin` was loaded at startup, so a `/roles`
                        // saved in this session would otherwise not reach the
                        // run. A file broken since startup is said out loud,
                        // and the settings the session started with are used.
                        let run_roles = match crate::admin::AdminConfig::load(&incord_code_dir) {
                            Ok(fresh) => fresh.resolve_roles().value,
                            Err(e) => {
                                render::print_error(&e);
                                admin.resolve_roles().value
                            }
                        };
                        render::print_info(&format!(
                            "Working the ledger with {} — its own loop, on your subscription. The check is still run by this harness, in its own process.",
                            worker_agent.display_name
                        ));
                        // The protected paths go to the agent as deny rules it
                        // enforces ITSELF, so a write to one never happens. The
                        // sentinel in `agent::worker` stays behind it, catching
                        // whatever the rules did not.
                        //
                        // Defaulted rather than fatal HERE only because the
                        // worker loads the same policy and refuses to delegate
                        // at all if it cannot be parsed — a malformed file stops
                        // the attempt there, it does not become "no guards".
                        let run_guards =
                            crate::guards::Guards::load(&workspace).unwrap_or_default();
                        // The same choice the interactive session makes, for
                        // the same reason: an unattended run is the same work.
                        // EVERYTHING THE SESSION KNOWS, ON THE RUN PATH TOO.
                        //
                        // The interactive backend has been given this since it
                        // was written — the intake task, the owner's rules, the
                        // auditor's findings, the recorder's handoff, the role
                        // table. The run backend was built without it, so every
                        // ledger task was worked by an agent that had been told
                        // none of it: it rediscovered what the recorder had
                        // already solved, and re-derived conventions the spec
                        // set states outright.
                        //
                        // This is the SECOND time this exact context has gone
                        // missing on a path that compiled and passed its tests —
                        // see the note where the interactive session is built.
                        // It is one argument, and leaving it off is invisible
                        // from the outside, which is why it is worth a comment
                        // rather than a shrug.
                        let backend: Box<dyn crate::agent::AgentBackend> = route_backend(
                            &workspace,
                            &worker_agent.id,
                            &worker_agent.command,
                            &run_guards,
                            &system_prompt,
                            self.config.env_filter_enabled,
                        );
                        // Planning is the lead's job. With the sub-agent the
                        // lead itself, the one backend does both, as it always
                        // has; only a different sub-agent needs a second.
                        let mut lead_planner: Option<Box<dyn crate::agent::AgentBackend>> =
                            (worker_agent.id != agent.id).then(|| {
                                route_backend(
                                    &workspace,
                                    &agent.id,
                                    &agent.command,
                                    &run_guards,
                                    &system_prompt,
                                    self.config.env_filter_enabled,
                                )
                            });
                        // Every OTHER installed agent becomes somewhere the
                        // work can move when this one runs out of room. Two
                        // subscriptions are two separate allowances, and a
                        // five-hour limit on one is not a reason to stop
                        // working for five hours.
                        // Standby exists because two installed CLIs are two
                        // separate allowances. A configured endpoint has no
                        // such sibling to move to, so it gets none rather than
                        // a list that could never help.
                        //
                        // The standby backs up the agent WORKING the tasks.
                        let standby: Vec<Box<dyn crate::agent::AgentBackend>> = fleet
                            .agents()
                            .iter()
                            .filter(|a| a.id != worker_agent.id)
                            .map(|other| {
                                run_backend(
                                    &workspace,
                                    &other.id,
                                    &other.command,
                                    &run_guards,
                                    &system_prompt,
                                )
                            })
                            .collect();
                        if !standby.is_empty() {
                            render::print_info(&format!(
                                // NOT "if this one hits a limit" any more: a
                                // limit is one of several agent-side failures
                                // that hand the work over, and the line named
                                // the rarest of them. Measured live, the standby
                                // took over four times in one run for a broken
                                // stream and once for a session limit.
                                "{} standby agent(s) available if this one cannot do the work.",
                                standby.len()
                            ));
                        }
                        // THE PLANNING PASS. A roadmap item is a thing to
                        // achieve, not a kind of work, so it carries no role —
                        // and a task with no role runs on the session model
                        // however cheap the work turns out to be. This is where
                        // an item becomes steps that each name one.
                        //
                        // Bounded by the roadmap: one planning turn per open
                        // item, in `plan` mode so the planner cannot start doing
                        // the work instead of splitting it.
                        //
                        // An item the planner returns nothing for is KEPT AS IT
                        // WAS. A planner having a bad turn must not delete work
                        // the owner approved; the item simply runs unrouted, as
                        // it did before any of this existed.
                        let mut backend = backend;
                        if freshly_derived {
                            let roles = run_roles.clone();
                            // THE CHECK EVERY PLANNED STEP IS JUDGED BY. The
                            // operator's, resolved the same way it is for any
                            // other task — never named by the model, which
                            // would let it pick something that always passes.
                            //
                            // None means this project has no check the harness
                            // can find. The steps are still planned and still
                            // routed; the runner then sets them aside rather
                            // than marking unprovable work done, and says so.
                            let verify = admin
                                .resolve_verify(&workspace)
                                .map(|resolved| resolved.value)
                                .unwrap_or_default();
                            if verify.is_empty() {
                                render::print_error(
                                    "No check is configured for this project, so no planned step can be proved finished.                                      Set one with INCORD_VERIFY_CMD or `verify_command` in admin.json.",
                                );
                            }
                            let mut planned: Vec<crate::ledger::Task> = Vec::new();
                            for task in &ledger.tasks {
                                let planner = match lead_planner.as_mut() {
                                    Some(lead) => lead.as_mut(),
                                    None => backend.as_mut(),
                                };
                                let steps = crate::plan::split(planner, &roles, &task.title);
                                if steps.is_empty() {
                                    planned.push(task.clone());
                                    continue;
                                }
                                render::print_info(&format!(
                                    "{} -> {} step(s): {}",
                                    task.roadmap_id,
                                    steps.len(),
                                    steps
                                        .iter()
                                        .map(|s| s.role.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ));
                                planned.extend(
                                    crate::ledger::Ledger::from_plan(
                                        &task.roadmap_id,
                                        &steps,
                                        &verify,
                                        &workspace,
                                    )
                                    .tasks,
                                );
                            }
                            ledger.tasks = planned;
                        }

                        // The role table applies here too: the CLI takes
                        // `--model` per invocation, so cheap work runs cheap.
                        let worker = crate::agent::worker::AgentBackendWorker::in_workspace(
                            backend,
                            workspace.clone(),
                        )
                        .with_roles(run_roles.clone())
                        .with_standby(standby)
                        .with_extra_dirs(extra_dirs.clone())
                        // Its OWN instance rather than the runner's: the channel
                        // is plain config, and sharing one would mean handing the
                        // same `&mut` to the runner and the worker for the whole
                        // run. Same plugin switch, so "no owner" stays one
                        // setting rather than two that can disagree.
                        .with_owner(if admin.plugins.owner {
                            Box::new(crate::runner::MemoryOwner::default())
                        } else {
                            Box::new(crate::runner::NoOwner) as Box<dyn crate::runner::OwnerChannel>
                        });
                        // A bar the whole time the loop runs, because a run
                        // works unattended for minutes and "still going" is the
                        // question a person actually has.
                        let run_status = render::LiveStatus::start(
                            std::io::IsTerminal::is_terminal(&std::io::stdout()),
                        );
                        // AND WHAT IT IS CHANGING, as it changes it. "Still
                        // going" is only the first question; the next one is
                        // which file and what part of it, and until now that
                        // could only be answered after the run by reading the
                        // transcript. Written through the bar's OWN lock — two
                        // writers on one terminal is the bug `LiveStatus` was
                        // built to prevent.
                        let edit_writer = run_status.writer();
                        let mut worker = worker.showing_edits(Box::new(move |line: &str| {
                            edit_writer.write_text(line);
                        }));
                        let run_width = crate::cli::activity::terminal_width();
                        let mut watcher = RunWatcher {
                            status: &run_status,
                            width: run_width,
                        };
                        // A SECOND OPINION WHEN A GOAL DEFEATS ITS FIRST PLAN.
                        // Bounded at `runner::MAX_REPLANS`, and every failure is
                        // written to the project's shared facts BEFORE the
                        // re-plan — so the next attempt, the next session and
                        // the next project are all told what already failed.
                        //
                        // Its own backend, for the reason given on
                        // `LiveReplanner`: the worker holds the run's for the
                        // whole loop, and a planning turn is one cheap turn.
                        let titles: std::collections::HashMap<String, String> =
                            match crate::brief::check(&workspace) {
                                crate::brief::Gate::Ready(brief) => brief
                                    .roadmap
                                    .iter()
                                    .map(|item| (item.id.clone(), item.title.clone()))
                                    .collect(),
                                // No roadmap to plan against means no second
                                // opinion worth having; the run behaves as it
                                // did before this existed.
                                _ => std::collections::HashMap::new(),
                            };
                        let mut no_replan = crate::runner::NoReplan;
                        let mut live_replan = LiveReplanner {
                            workspace: workspace.clone(),
                            // THE SAME CONSTRUCTOR THE RUN ITSELF USES. This
                            // built one vendor's backend directly and handed it
                            // the SELECTED agent's binary — so under a second
                            // agent a re-plan drove that binary with the wrong
                            // protocol and could only fail. Routing by id is
                            // `run_backend`'s whole job; there must not be a
                            // second place that decides it.
                            backend: run_backend(
                                &workspace,
                                &agent.id,
                                &agent.command,
                                &run_guards,
                                &system_prompt,
                            ),
                            roles: run_roles.clone(),
                            verify: admin
                                .resolve_verify(&workspace)
                                .map(|resolved| resolved.value)
                                .unwrap_or_default(),
                            titles,
                        };
                        let replanner: &mut dyn crate::runner::Replanner =
                            if live_replan.titles.is_empty() {
                                &mut no_replan
                            } else {
                                &mut live_replan
                            };
                        let outcome = crate::runner::run_watched(
                            &mut ledger,
                            &mut worker,
                            &workspace,
                            &mut run_budget,
                            &|| stop_flag.is_cancelled(),
                            owner,
                            &mut journal,
                            &mut watcher,
                            replanner,
                            // THE FINAL TESTING. The orchestrator's own last
                            // act: every roadmap item's check, run once the
                            // queue has drained. Without it a completed plan
                            // and a reached goal are the same report.
                            &mut crate::runner::BriefGoals,
                            // MORE THAN ONE AGENT AT A TIME, when the operator
                            // asked for it (`INCORD_LANES`) and the workspace
                            // is a git repository. One lane — the default —
                            // never reaches any of this.
                            &crate::cli::CliLaneWorkers {
                                // Lanes work tasks, so they run the sub-agent.
                                agent_id: worker_agent.id.clone(),
                                command: worker_agent.command.clone(),
                                guards: run_guards.clone(),
                                system_prompt: system_prompt.clone(),
                                roles: run_roles,
                                extra_dirs: extra_dirs.clone(),
                                desktop: desktop_enabled_from_env(),
                                permission_mode,
                            },
                        );
                        run_status.stop();

                        // Always reported, never summarised away: a run that
                        // stops without saying why reads as one that crashed.
                        render::print_info(&outcome.stop.message());
                        render::print_info(&format!(
                            "{} completed, {} attempt(s) made.",
                            outcome.completed.len(),
                            outcome.attempted
                        ));

                        // WHICH AGENT SPENT IT. The ceiling says whether there
                        // is budget left; with several models on several
                        // subscriptions the operator's next question is which
                        // one is consuming it, and a single total cannot say.
                        if let Some(report) = outcome.spend_report() {
                            render::print_info(&report);
                        }

                        // WHAT ONE AGENT ASKED ANOTHER AND NOBODY ANSWERED.
                        //
                        // An ask does not block, so the agent carried on with
                        // a guess — which is correct, and only safe if the
                        // question is seen afterwards. Unshown, this is where
                        // guesses quietly become decisions.
                        let unanswered = crate::runner::unanswered_questions(&workspace);
                        if !unanswered.is_empty() {
                            render::print_info(&format!(
                                "{} question(s) between agents went unanswered — each one is \
                                 a guess somebody made:",
                                unanswered.len()
                            ));
                            for question in &unanswered {
                                outln!("  {question}");
                            }
                        }

                        outln!("{}", ledger.summary());

                        // WHAT THE RUN ACTUALLY ACHIEVED, not what it attempted.
                        //
                        // The lines above report TASKS — how many were tried and
                        // how the loop stopped. A run can finish every task it
                        // planned and still not reach the goal, because the plan
                        // was a guess; and until this call existed the belief was
                        // never told either way, so an unattended run left the
                        // proxy — and the owner's phone with it — reading `todo`
                        // for goals that had been passing for days.
                        //
                        // The SAME call `/roadmap` makes, deliberately: it runs
                        // the checks, prints the board, ticks what a command
                        // proved, and names checks that match no item. A second
                        // copy here would be a second thing to keep true.
                        slash_commands::handle_roadmap(&workspace);
                    }
                    SlashCommand::Admin => {
                        // The answer to "what is this session actually going to
                        // do?", with each value naming which of the four layers
                        // decided it.
                        out!(
                            "{}",
                            admin.report(&workspace, self.config.model_from_flag.as_deref())
                        );
                    }
                    SlashCommand::Agent(arg) => {
                        slash_commands::handle_agent(
                            arg.as_deref(),
                            &catalogue,
                            &agent,
                            &incord_code_dir,
                        );
                    }
                    SlashCommand::Roles(arg) => {
                        slash_commands::handle_roles(arg.as_deref(), &incord_code_dir);
                    }
                    SlashCommand::Help => slash_commands::handle_help(),
                    SlashCommand::Status => {
                        slash_commands::handle_agent_status(
                            &display_model,
                            &workspace,
                            permission_mode.as_str(),
                            session.session_id(),
                            session.turns(),
                            &session.usage(),
                            session.quota(),
                        );
                    }
                    SlashCommand::Compact => {
                        // The agent compacts its own transcript — it owns the
                        // conversation now, and a second compactor here would be
                        // summarising a copy it does not have.
                        render::print_info(
                            "The agent manages its own context and compacts it automatically. Use /clear to start a fresh session.",
                        );
                    }
                    SlashCommand::Tools => {
                        // The agent brings its own tools and runs its own loop,
                        // so listing this harness's would name things nothing
                        // uses. What IS this harness's business is stated.
                        slash_commands::handle_delegated_tools(&agent.display_name);
                        render::print_info(&tool_cache.summary_line());
                    }
                    SlashCommand::Skills => {
                        slash_commands::handle_skills_from_store(&incord_code_dir);
                    }
                    SlashCommand::Mcp => {
                        // MCP servers are configured for the agent, in the
                        // agent's own settings — not here. Saying so beats
                        // printing an empty list this harness would never use.
                        render::print_info(&format!(
                            "MCP servers are configured in {}'s own settings and connect there, not in this harness.",
                            agent.display_name
                        ));
                    }
                    SlashCommand::Model(new_model) => {
                        match new_model.as_deref() {
                            Some(name) => {
                                // Changing the model ABANDONS the agent session:
                                // a resumed session carries the earlier turns,
                                // and asking a different model to continue them
                                // is a request nothing can reason about.
                                session.set_models(&[name.to_string()]);
                                display_model = format!("{} via {}", name, agent.display_name);
                                render::print_info(&format!(
                                    "Next turn runs on {name}. The conversation starts fresh — a different model cannot continue a transcript it never had."
                                ));
                            }
                            None => render::print_info(&format!("Model: {display_model}")),
                        }
                    }
                    SlashCommand::Clear => {
                        // THE OLD RECORD IS FINISHED, not reused. The write
                        // after each turn does `record.agent_session = id` on
                        // whatever record is in hand, so carrying this one past
                        // a clear meant the next turn stamped the NEW id over
                        // the only pointer to the session just ended — it
                        // vanished from `/sessions` and could never be resumed.
                        // Saved, then dropped so the next turn starts a fresh
                        // one.
                        if let Some(finished) = session_record.take() {
                            crate::session::save(&incord_code_dir, &finished);
                            crate::trace!(
                                "session",
                                "cleared: {} kept, next turn opens a new record",
                                finished.agent_session
                            );
                        }
                        // Really cleared, not merely reported as cleared: the
                        // next turn opens a NEW agent session rather than
                        // resuming the one the user just asked to drop.
                        session.clear();
                        render::print_info(
                            "Conversation cleared. The next turn starts a new agent session.",
                        );
                    }
                    SlashCommand::Save(path) => {
                        // The agent owns the transcript, so what is worth
                        // keeping is the id that reaches it again. Writing a
                        // copy of messages this harness does not have would be
                        // a file that cannot restore anything.
                        //
                        // A NAME, though, is worth keeping, and the argument
                        // used to be discarded: `/save nightly-fix` parsed the
                        // name and threw it away, so the record stayed titled by
                        // whatever happened to be typed first.
                        if let Some(wanted) =
                            path.as_deref().map(str::trim).filter(|p| !p.is_empty())
                        {
                            match session_record.as_mut() {
                                Some(record) => {
                                    record.rename(wanted);
                                    crate::session::save(&incord_code_dir, record);
                                    render::print_info(&format!(
                                        "Named this session `{}`.",
                                        record.title
                                    ));
                                }
                                None => render::print_info(
                                    "Nothing to name yet — the record is written after the first turn.",
                                ),
                            }
                        }
                        match session.session_id() {
                            Some(id) => render::print_info(&format!(
                                "This conversation is agent session {id}. Reopen it with `--resume {id}`."
                            )),
                            None => render::print_info(
                                "Nothing to save yet — the agent opens a session on the first turn.",
                            ),
                        }
                    }
                    SlashCommand::Sessions => {
                        // This said "this harness keeps none to list", which was
                        // true when it was written and stopped being true the
                        // moment the session store landed. The agent still owns
                        // the transcripts; what is listed here is the pointers
                        // to them, which is the part that reopens one.
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_secs());
                        let records = crate::session::for_workspace(&incord_code_dir, &workspace);
                        for line in crate::session::summary_lines(&records, now) {
                            render::print_info(&line);
                        }
                        // The list is filtered to THIS workspace, so an empty one
                        // read as "nothing was ever saved" while records for other
                        // checkouts sat in the same file. Say where they are: the
                        // id is what `--resume` takes, and an operator who thinks
                        // it is gone starts fresh and re-pays the cached prefix.
                        if records.is_empty() {
                            let others = crate::session::elsewhere(&incord_code_dir, &workspace);
                            for line in crate::session::elsewhere_lines(&others) {
                                render::print_info(&line);
                            }
                        }
                        if let Some(id) = session.session_id() {
                            render::print_info(&format!("This conversation is {id}."));
                        }
                    }
                    SlashCommand::Resume(name) => match name {
                        Some(id) => {
                            // Same reason as `/clear`: the conversation in hand
                            // is being left for a different one, so its record
                            // is finished here rather than having the resumed
                            // id stamped over it on the next turn.
                            if let Some(finished) = session_record.take() {
                                crate::session::save(&incord_code_dir, &finished);
                            }
                            session.resume(&id);
                            crate::trace!("session", "resuming {id}");
                            render::print_info(&format!("Next turn continues agent session {id}."));
                        }
                        None => render::print_info(
                            "Name the agent session to continue, e.g. `/resume <id>`.",
                        ),
                    },
                    SlashCommand::Remember(text) => {
                        slash_commands::handle_remember(&text, &workspace);
                    }
                    SlashCommand::Forget { id, confirmed } => {
                        slash_commands::handle_forget(&id, confirmed);
                    }
                    SlashCommand::Memories(query) => {
                        slash_commands::handle_memories(query.as_deref());
                    }
                    SlashCommand::Plan => slash_commands::handle_plan(&mut session),
                    SlashCommand::Act => slash_commands::handle_act(&mut session),
                    SlashCommand::Jobs => {
                        slash_commands::handle_jobs(&mut session_tools.jobs());
                    }
                    SlashCommand::Kill(id) => {
                        if session_tools.jobs().kill(id) {
                            render::print_info(&format!("Job #{id} killed."));
                        } else {
                            render::print_error(&format!(
                                "Job #{id} not found or already finished."
                            ));
                        }
                    }
                    SlashCommand::Background(cmd) => {
                        match session_tools.jobs().spawn(&cmd, &workspace) {
                            Ok(id) => {
                                render::print_info(&format!("Job #{id} started: {cmd}"));
                            }
                            Err(e) => {
                                render::print_error(&format!("Failed to start job: {e}"));
                            }
                        }
                    }
                    SlashCommand::Worktree(arg) => {
                        slash_commands::handle_worktree(arg.as_deref(), &workspace);
                    }
                    SlashCommand::Diff(arg) => {
                        slash_commands::handle_diff(arg.as_deref(), &workspace);
                    }
                    SlashCommand::Env => {
                        slash_commands::handle_env();
                    }
                    // A real command, one word short. Say what it needs — the
                    // operator is closer to success than "unknown command"
                    // suggests, and /help does not name the missing part.
                    SlashCommand::Usage(usage) => slash_commands::handle_usage(&usage),
                    SlashCommand::Unknown(cmd) => {
                        render::print_error(&format!(
                            "Unknown command: {cmd}. Type /help for commands."
                        ));
                    }
                }
                continue;
            }

            // Poll background jobs for completion notifications
            let finished = session_tools.jobs().poll_completed();
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

            // A greeting is answered here and goes no further.
            //
            // This sits at the TOP of the turn deliberately. It was first put
            // inside `run_one_turn`, which is after this call site has already
            // paid for the rules and recall lookups below — so "hi" still cost
            // two round-trips to a store that had nothing to say about it.
            // Measured at 2.07 seconds for a message that needs none of it.
            if crate::text::is_greeting(&line) {
                render::print_assistant(&crate::text::greeting_reply(&workspace));
                continue;
            }

            // Match CoT reasoning template + skill and inject into user message.
            // CoT = HOW to think (reasoning scaffold). Skill = WHAT to know (domain expertise).
            // Both are injected before the user's actual message.
            let mut context_parts: Vec<String> = Vec::new();

            let augmented_input = if context_parts.is_empty() {
                line.clone()
            } else {
                context_parts.push(line.clone());
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
            if let Some(rules) = crate::rules::context_for_turn(&line) {
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
            if let Err(e) = run_one_turn(&mut session, &workspace, &augmented_input, &line) {
                render::print_error(&e);
            }

            if let Some(f) = footer.as_mut() {
                f.set_session(footer::Session {
                    model: session
                        .model()
                        .map(str::to_string)
                        .unwrap_or_else(|| session.backend_id().to_string()),
                    workspace: workspace
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| workspace.display().to_string()),
                    branch: git_context.as_ref().map(|g| g.branch.clone()),
                    dirty: git_context.as_ref().is_some_and(|g| g.is_dirty),
                });
            }

            // AFTER the turn, because the agent only names its session once it
            // has opened one — asked before the first turn it answers `None`,
            // and a one-turn session would record nothing at all.
            //
            // Resuming reads a cached prefix that a fresh session re-creates at
            // roughly ten times the price, so this id is the thing worth never
            // losing.
            if let Some(id) = session.session_id() {
                let record = session_record.get_or_insert_with(|| {
                    crate::session::Record::new(id, &workspace, &display_model)
                });
                record.agent_session = id.to_string();
                record.name_from(&line);
                // The subagents this turn handed work to. Each is its own
                // conversation with its own id, so a run that fanned out to four
                // workers reopens as the five it really was rather than as one.
                // `spawned` ignores a repeat, so re-recording every turn costs
                // nothing and a subagent is never listed twice.
                for (child, title) in session.children() {
                    record.spawned(&child, &title);
                }
                crate::session::save(&incord_code_dir, record);
            }
        }

        // The memory engine's end of the session, bounded by the engine
        // runner, before the operator's own end-of-session command.
        session.end_memory("prompt_input_exit");

        // Run session end lifecycle hook (Feature 7)
        if let Some(result) = lifecycle_hooks.run_session_end(&workspace) {
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

        // The agent owns the transcript, so what is worth telling the user on
        // the way out is the id that reaches it again — not a file of messages
        // this harness never held, which could restore nothing.
        if let Some(id) = session.session_id() {
            render::print_info(&format!(
                "This conversation is agent session {id}. Reopen it with `--resume {id}`."
            ));
        }

        outln!("Goodbye!");
        Ok(())
    }
}

/// Discover the most recent session file (Feature 6: session discovery).
/// Returns (session_name, message_count) or None.
/// Draws the ledger's progress while a run works.
///
/// Only here, and not for a conversation: `/run` has a plan to measure against,
/// and an interactive turn does not. A bar that invented a denominator would be
/// a number the reader could not use.
struct RunWatcher<'a> {
    status: &'a render::LiveStatus,
    width: usize,
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
/// Set `INCORD_DESKTOP=1` to turn it on. Empty does NOT count, for the same
/// reason `INCORD_TRACE=` in a shell profile must not turn tracing on for
/// everyone (see [`crate::trace`]).
pub(crate) const DESKTOP_ENV: &str = "INCORD_DESKTOP";

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
pub(crate) const PERMISSION_MODE_ENV: &str = "INCORD_PERMISSION_MODE";

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

/// Builds one agent worker per lane, each in its own checkout.
///
/// # Why it holds settings and not a backend
///
/// [`crate::agent::AgentBackend`] is not `Send` — it owns callbacks into the
/// terminal — so a backend cannot be handed to a lane's thread. What CAN
/// cross is the settings it is built from: strings, paths and tables. So the
/// backend is constructed inside the lane, from the same
/// [`run_backend_with_desktop`] the sequential path uses, and no second place
/// decides which agent serves the work.
///
/// A process per lane is not waste: the worker already drops the agent's
/// conversation between tasks, so a reused one would start from nothing
/// anyway.
pub(crate) struct CliLaneWorkers {
    pub agent_id: String,
    pub command: String,
    pub guards: crate::guards::Guards,
    pub system_prompt: Vec<String>,
    pub roles: crate::roles::RoleTable,
    pub extra_dirs: Vec<std::path::PathBuf>,
    pub desktop: bool,
    /// Carried rather than read from the environment, because a lane runs on
    /// its own thread and a capability decision should not depend on what a
    /// process-wide variable happens to hold when that thread gets there.
    pub permission_mode: crate::permissions::PermissionMode,
}

impl crate::runner::LaneWorkers for CliLaneWorkers {
    fn spawn_for(&self, workspace: &Path) -> Option<Box<dyn crate::runner::TaskWorker + Send>> {
        Some(Box::new(LaneAgent {
            workspace: workspace.to_path_buf(),
            agent_id: self.agent_id.clone(),
            command: self.command.clone(),
            guards: self.guards.clone(),
            system_prompt: self.system_prompt.clone(),
            roles: self.roles.clone(),
            extra_dirs: self.extra_dirs.clone(),
            desktop: self.desktop,
            permission_mode: self.permission_mode,
        }))
    }
}

/// One lane's agent, built where it runs.
struct LaneAgent {
    workspace: std::path::PathBuf,
    agent_id: String,
    command: String,
    guards: crate::guards::Guards,
    system_prompt: Vec<String>,
    roles: crate::roles::RoleTable,
    extra_dirs: Vec<std::path::PathBuf>,
    desktop: bool,
    permission_mode: crate::permissions::PermissionMode,
}

impl crate::runner::TaskWorker for LaneAgent {
    fn work(&mut self, task: &crate::ledger::Task) -> Result<String, String> {
        // Built HERE, on the lane's own thread, because the backend cannot
        // cross a thread boundary. Every argument is the one the sequential
        // path passes, except the workspace — which is the whole point.
        let backend = run_backend_with_desktop(
            &self.workspace,
            &self.agent_id,
            &self.command,
            &self.guards,
            &self.system_prompt,
            self.desktop,
            self.permission_mode,
        );
        let mut worker =
            crate::agent::worker::AgentBackendWorker::in_workspace(backend, self.workspace.clone())
                .with_roles(self.roles.clone())
                .with_extra_dirs(self.extra_dirs.clone());

        let report = worker.work(task);
        // The conversation this lane opened is on the vendor's side and is
        // billed on every later turn. Nothing else will clear it: the worker
        // is dropped here.
        crate::runner::TaskWorker::release(&mut worker);
        report
    }
}

/// The backend for one catalogue route: the configured API when the route is
/// [`crate::agent::registry::API_ROUTE`], otherwise the installed CLI.
///
/// # The bug this closes
///
/// The API backend used to be chosen whenever the API variables were SET,
/// whatever agent had been chosen — so exporting two variables replaced every
/// installed agent, and a session that announced one agent drove another. The
/// id decides now, and the catalogue lists that id as usable only when the
/// same variables are present ([`crate::agent::api::Settings::from_lookup`]).
pub(crate) fn route_backend(
    workspace: &Path,
    agent_id: &str,
    binary: &str,
    guards: &crate::guards::Guards,
    system_prompt: &[String],
    filter_env: bool,
) -> Box<dyn crate::agent::AgentBackend> {
    let api = if agent_id == crate::agent::registry::API_ROUTE {
        crate::agent::api::Settings::from_env()
    } else {
        None
    };
    match api {
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
            // tools runs.
            .with_engine(crate::hooks::engine::Engine::locate())
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
    match agent_id {
        // The registry ID, not the command name. `agy` is the binary; the agent
        // is `antigravity`. Matching the binary name here compiled, passed a
        // unit test that had invented the same wrong id on both sides, and
        // could never once have fired in a real run — the live check is what
        // caught it.
        crate::agent::registry::ANTIGRAVITY => Box::new(
            crate::agent::agy::AgyBackend::new(workspace.to_path_buf())
                .with_binary(binary.to_string())
                // No guards argument: this agent has no flag to enforce them.
                // The policy still applies — the sentinel checks it, and the
                // worker reports the gap on every task — but claiming it here
                // by handing over rules nothing would render is the pretence
                // this whole split exists to prevent.
                .with_system_context(system_prompt.to_vec()),
        ),
        crate::agent::registry::CODEX => {
            let mut backend = crate::agent::codex::CodexBackend::new(workspace.to_path_buf())
                .with_binary(binary.to_string())
                .with_system_context(system_prompt.to_vec());

            // Codex spells registration as `-c key=value` TOML overrides rather
            // than a JSON flag — verified from `codex mcp add --help`, whose own
            // example passes an array.
            let overrides =
                mcp_overrides_for(desktop, std::env::current_exe().ok(), workspace, mode);
            if !overrides.is_empty() {
                backend = backend.with_mcp_overrides(overrides);
            }

            Box::new(backend)
        }
        _ => {
            let mut backend =
                crate::agent::claude_code::ClaudeCodeBackend::new(workspace.to_path_buf())
                    .with_binary(binary.to_string())
                    .with_guards(guards)
                    .streaming(true)
                    .with_system_context(system_prompt.to_vec())
                    // The model's tools are incode's own on this route, so the
                    // memory engine's guard is asked before each, as it is on
                    // the API route.
                    .with_engine(crate::hooks::engine::Engine::locate());

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

/// Plans a roadmap item again after its first plan failed.
///
/// # Wired to
///
/// - **Implements** [`crate::runner::Replanner`]; called from the run loop in
///   `runner::run_watched` when a task uses up its attempts.
/// - **Calls** [`crate::plan::split`] and [`crate::ledger::Ledger::from_plan`] —
///   the SAME two calls that build the first plan, so a re-plan cannot drift
///   into producing a different shape of task than the original.
///
/// # Why it owns a backend instead of borrowing the run's
///
/// The worker owns the run's backend for the whole loop. Lending it out would
/// mean handing the same `&mut` to two places, so this keeps its own — which is
/// affordable because a planning turn is ONE turn, and it is the cheap half of
/// the split by design: the planner reads no files and writes no code.
struct LiveReplanner {
    /// The tree this project lives in — the key its lessons are filed under.
    workspace: std::path::PathBuf,
    backend: Box<dyn crate::agent::AgentBackend>,
    roles: crate::roles::RoleTable,
    /// The check every re-planned step is judged by — the operator's, never the
    /// model's, for the same reason as the first plan.
    verify: Vec<String>,
    /// Roadmap id to its title, from the brief. A re-plan needs the GOAL, not
    /// the failed task's wording: planning against the task that just failed
    /// would inherit the framing that failed with it.
    titles: std::collections::HashMap<String, String>,
}

impl crate::runner::Replanner for LiveReplanner {
    /// Write the mistake down, whether or not a re-plan follows.
    ///
    /// A store that will not take it must NOT stop the run: losing the note
    /// costs the next run some context, losing the run costs the owner the work.
    fn failed(&mut self, roadmap_id: &str, why: &str) {
        match crate::lessons::record(&self.workspace, roadmap_id, why) {
            Ok(true) => crate::trace!("lessons", "{roadmap_id}: written down"),
            Ok(false) => crate::trace!("lessons", "{roadmap_id}: already known"),
            Err(e) => crate::trace!("lessons", "{roadmap_id}: not written: {e}"),
        }
    }

    fn replan(&mut self, roadmap_id: &str, why: &str) -> Vec<crate::ledger::Task> {
        // Read back from the STORE rather than from this run: another session
        // may have learned something about this project since, and a shared
        // memory that is only ever written is not shared.
        let lessons = crate::lessons::known(&self.workspace);

        // An item with no title in the brief is one this run cannot describe,
        // and planning against an id alone would produce noise.
        let Some(title) = self.titles.get(roadmap_id).cloned() else {
            crate::trace!("plan", "{roadmap_id}: no roadmap title, not re-planning");
            return Vec::new();
        };

        let mut instruction = format!(
            "{title}\n\nA previous plan for this was attempted and failed. What \
             went wrong, as an OBSERVATION rather than an instruction:\n{}\n\n\
             Plan a DIFFERENT approach. Repeating the same shape of plan spends \
             the owner's remaining attempts on the thing that already failed.",
            why.trim()
        );
        if let Some(block) = crate::lessons::prompt_block(&lessons) {
            instruction.push_str("\n\n");
            instruction.push_str(&block);
        }

        let steps = crate::plan::split(self.backend.as_mut(), &self.roles, &instruction);
        if steps.is_empty() {
            return Vec::new();
        }
        crate::ledger::Ledger::from_plan(roadmap_id, &steps, &self.verify, &self.workspace).tasks
    }
}

impl crate::runner::Progress for RunWatcher<'_> {
    fn advanced(&mut self, done: usize, total: usize) {
        let snapshot = crate::cli::activity::Status {
            activity: crate::cli::activity::Activity::THINKING,
            elapsed: 0,
            tokens: 0,
            progress: Some((done, total)),
        };
        self.status
            .set(&snapshot.render(render::FRAME_SLOT, self.width));
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
            // The answers are the person's line for this turn: the engine
            // hears it, and its context for it rides the turn.
            let context = session.memory_prompt(&instruction).into_iter().collect();
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
    let mut context = Vec::new();
    match session.memory_prompt(line) {
        Some(block) => context.push(block),
        None => {
            if let Some(rules) = crate::rules::context_for_turn(line) {
                context.push(rules);
            }
        }
    }

    TurnPlan::Agent {
        instruction: augmented.to_string(),
        context,
    }
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
mod tests {
    use super::*;

    /// The mode a session runs in when the operator says nothing.
    ///
    /// Named so the tests below read as being about registration rather than
    /// about permissions — the one test that IS about permissions passes a
    /// different mode on purpose and is meant to stand out.
    const ORDINARY: crate::permissions::PermissionMode =
        crate::permissions::PermissionMode::WorkspaceWrite;

    /// **THE REGRESSION THAT HAPPENED TWICE.**
    ///
    /// The run backend shipped built without `with_system_context`, so every
    /// ledger task was worked by an agent told none of the intake task, the
    /// owner's rules, the auditor's findings, the recorder's handoff or the role
    /// table — while the interactive backend beside it carried all of them. Both
    /// times it compiled and the suite stayed green; the only visible symptom
    /// was a missing `--append-system-prompt` on the spawned child, which is not
    /// something a test suite notices.
    ///
    /// Drop the `.with_system_context(..)` from [`run_backend`] and this fails.
    #[test]
    fn the_run_backend_carries_the_sessions_system_context() {
        let blocks = vec![
            "<guards>the paths the agent may not write</guards>".to_string(),
            "Already solved in this project, from the recorder".to_string(),
        ];
        // EVERY agent, not just the one the regression happened on. The second
        // backend has no flag to append a system prompt at all, so it is the
        // one most likely to lose this quietly — checking only the first would
        // leave exactly that hole open.
        for (agent_id, binary) in [
            ("claude-code", "claude"),
            (crate::agent::registry::ANTIGRAVITY, "agy"),
            (crate::agent::registry::CODEX, "codex"),
        ] {
            let backend = run_backend(
                Path::new("."),
                agent_id,
                binary,
                &crate::guards::Guards::default(),
                &blocks,
            );

            assert_eq!(
                backend.system_context().len(),
                2,
                "every block the session assembled must reach the {agent_id} \
                 backend, not just some: {:?}",
                backend.system_context()
            );
            assert!(
                backend
                    .system_context()
                    .iter()
                    .any(|b| b.contains("from the recorder")),
                "including the recorder handoff, which is the one that went \
                 missing, on {agent_id}: {:?}",
                backend.system_context()
            );
        }
    }

    /// **The desktop is OFF unless asked for.**
    ///
    /// The switch decides whether an agent can click and type into whatever is
    /// on the screen. Defaulting it on would widen every existing run as a side
    /// effect of an upgrade — the kind of change nobody notices until it has
    /// already happened.
    #[test]
    fn the_desktop_is_not_handed_over_unless_it_is_switched_on() {
        let exe = Some(std::path::PathBuf::from("C:/bin/incode.exe"));

        let off = desktop_setup_for(false, exe.clone(), Path::new("C:/work"));
        assert_eq!(
            off, None,
            "the desktop must not be offered when the switch is off"
        );

        let setup =
            desktop_setup_for(true, exe, Path::new("C:/work")).expect("switched on, so a setup");
        assert_eq!(setup.exe, std::path::PathBuf::from("C:/bin/incode.exe"));
        // The journal is what makes the work watchable; without it the agent
        // drives a desktop nobody can see. Its stop file sits beside it.
        assert!(
            setup.journal.ends_with("desktop.jsonl"),
            "{}",
            setup.journal.display()
        );
    }

    /// No path to this binary means no desktop — on either route.
    ///
    /// A desktop child that cannot be started is a tool that can only fail,
    /// which is a much harder thing to diagnose than simply not offering it.
    #[test]
    fn an_unknown_executable_path_offers_nothing() {
        assert_eq!(desktop_setup_for(true, None, Path::new("C:/work")), None);
        assert!(mcp_overrides_for(true, None, Path::new("C:/work"), ORDINARY).is_empty());
    }

    /// An empty value does not count as on.
    ///
    /// Same reasoning as `INCORD_TRACE`: `INCORD_DESKTOP=` left in a shell
    /// profile must not silently grant every run a desktop.
    #[test]
    fn the_desktop_switch_ignores_an_empty_value() {
        // The parsing rule, checked directly rather than by mutating the
        // process environment — which is global, and would race the other
        // tests running beside this one.
        let reads_as_on = |value: &str| !value.trim().is_empty();

        assert!(!reads_as_on(""));
        assert!(!reads_as_on("   "));
        assert!(reads_as_on("1"));
        assert!(reads_as_on("true"));
    }

    /// **THE FLAG MUST SURVIVE `run_backend`.**
    ///
    /// The sibling of `the_run_backend_carries_the_sessions_system_context`,
    /// and written for the same measured reason: a capability assembled inside
    /// a spawn is invisible from outside, so it goes missing quietly. The
    /// system context went missing twice this way.
    ///
    /// Drop the `.with_desktop(..)` from [`run_backend_with_desktop`] and
    /// this fails.
    ///
    /// Both states are driven in one process, so the assertion does not depend
    /// on what `INCORD_DESKTOP` happens to say on the machine running it. The
    /// earlier shape of this test read the environment and asserted the pairing,
    /// which on an unset machine reduced to "off, and nothing was handed over" —
    /// true with every registration line deleted.
    ///
    /// The desktop reaches the model as incode's own tools now, through the
    /// bridge: no MCP server is handed over beside it, on or off.
    #[test]
    fn the_run_backend_hands_the_desktop_to_the_agent_when_it_is_on() {
        let on = run_backend_with_desktop(
            Path::new("."),
            "claude-code",
            "claude",
            &crate::guards::Guards::default(),
            &[],
            true,
            ORDINARY,
        );
        let tools = on.offered_tools();
        for tool in ["screenshot", "click", "ui_tree"] {
            assert!(
                tools.iter().any(|t| t == tool),
                "the desktop is on, so the model gets {tool}: {tools:?}"
            );
        }
        assert_eq!(on.mcp_config(), None, "no desktop server beside the bridge");

        let off = run_backend_with_desktop(
            Path::new("."),
            "claude-code",
            "claude",
            &crate::guards::Guards::default(),
            &[],
            false,
            ORDINARY,
        );
        let tools = off.offered_tools();
        assert!(
            !tools.iter().any(|t| t == "screenshot"),
            "handed a desktop nobody asked for: {tools:?}"
        );
        assert!(tools.iter().any(|t| t == "Read"), "{tools:?}");
        assert_eq!(off.mcp_config(), None);
    }

    /// agy has NO per-invocation MCP flag, so it must never be handed one.
    ///
    /// Measured from `agy --help`: it offers only `mcp add/remove/list`, which
    /// edits the operator's persistent configuration. Registering a desktop
    /// there would be a lasting change to their machine, not a spawn argument,
    /// and that is a decision for them.
    #[test]
    fn agy_is_not_handed_a_flag_it_does_not_have() {
        let backend = run_backend_with_desktop(
            Path::new("."),
            crate::agent::registry::ANTIGRAVITY,
            "agy",
            &crate::guards::Guards::default(),
            &[],
            // Switched ON, which is the case that matters: the point is that
            // agy stays empty-handed even then.
            true,
            ORDINARY,
        );
        assert_eq!(backend.mcp_config(), None);
    }

    /// Codex takes the same registration in its own spelling.
    ///
    /// Drop the `.with_mcp_overrides(..)` from [`run_backend_with_desktop`] and
    /// this fails on the `expect`.
    #[test]
    fn codex_gets_the_desktop_as_toml_overrides_when_it_is_on() {
        let on = run_backend_with_desktop(
            Path::new("."),
            crate::agent::registry::CODEX,
            "codex",
            &crate::guards::Guards::default(),
            &[],
            true,
            ORDINARY,
        );
        // TOML, not JSON — the difference that makes it codex's dialect.
        let config = on
            .mcp_config()
            .expect("the desktop is on, so codex gets it");
        assert!(
            config.contains("mcp_servers.desktop.command="),
            "got: {config}"
        );
        assert!(
            config.contains("mcp_servers.team.command="),
            "the desktop overrides swallowed the team server: {config}"
        );

        let off = run_backend_with_desktop(
            Path::new("."),
            crate::agent::registry::CODEX,
            "codex",
            &crate::guards::Guards::default(),
            &[],
            false,
            ORDINARY,
        );
        let quiet = off.mcp_config().expect("the team channel is not optional");
        assert!(
            !quiet.contains("mcp_servers.desktop."),
            "handed a desktop nobody asked for: {quiet}"
        );
        assert!(quiet.contains("mcp_servers.team."), "got: {quiet}");
    }

    /// A Windows path must survive TOML's escape rules.
    ///
    /// `C:\Users\…` inside a TOML basic string is an invalid escape, so the
    /// separators are doubled. Without this the override is rejected by codex
    /// at spawn time, which surfaces as an MCP server that never starts.
    #[test]
    fn a_windows_path_is_escaped_for_toml() {
        let overrides = crate::desktop::mcp::codex_config_overrides(
            std::path::Path::new(r"C:\Users\someone\.cargo\bin\incode.exe"),
            Some(std::path::Path::new(r"C:\work\.incord\desktop.jsonl")),
            ORDINARY,
        );
        assert!(
            overrides[0].contains(r"C:\\Users\\someone"),
            "backslashes must be doubled: {}",
            overrides[0]
        );
        // The JOURNAL path needs the same treatment — it is a Windows path in
        // the same TOML string syntax, and escaping only the exe would ship a
        // half-fixed bug that fails exactly where nobody looks.
        assert!(
            overrides[1].contains(r"C:\\work\\.incord"),
            "the journal path must be escaped too: {}",
            overrides[1]
        );
        assert_eq!(
            overrides[1],
            r#"mcp_servers.desktop.args=["--desktop-mcp","--desktop-journal","C:\\work\\.incord\\desktop.jsonl","-p","workspace-write"]"#
        );
    }

    /// The configured agent decides WHICH backend the run loop drives.
    ///
    /// Without this the second adapter is unreachable: it compiles, its tests
    /// pass, and the run loop still builds the first backend every time.
    #[test]
    fn the_configured_agent_selects_the_backend_that_drives_it() {
        let guards = crate::guards::Guards::default();
        let claude = run_backend(Path::new("."), "claude-code", "claude", &guards, &[]);
        let agy = run_backend(
            Path::new("."),
            crate::agent::registry::ANTIGRAVITY,
            "agy",
            &guards,
            &[],
        );
        let codex = run_backend(
            Path::new("."),
            crate::agent::registry::CODEX,
            "codex",
            &guards,
            &[],
        );

        assert_eq!(claude.id(), "claude-code");
        assert_eq!(agy.id(), "agy", "the backend reports the BINARY it drives");
        assert_eq!(codex.id(), "codex");
        // And they do NOT claim the same enforcement, which is the whole point
        // of asking each backend rather than assuming one answer.
        assert_ne!(claude.enforces(), agy.enforces());
        assert_ne!(claude.enforces(), codex.enforces());
    }

    /// An empty session context is not an error — a project with no brief, no
    /// findings and no history has nothing to say — but it must not become a
    /// blank block that reads to the agent as an empty instruction.
    #[test]
    fn a_session_with_nothing_to_say_sends_no_empty_blocks() {
        for (agent_id, binary) in [
            ("claude-code", "claude"),
            (crate::agent::registry::ANTIGRAVITY, "agy"),
            (crate::agent::registry::CODEX, "codex"),
        ] {
            let backend = run_backend(
                Path::new("."),
                agent_id,
                binary,
                &crate::guards::Guards::default(),
                &[String::new(), "   ".to_string()],
            );
            assert!(
                backend.system_context().is_empty(),
                "blank blocks are dropped rather than sent, on {agent_id}: {:?}",
                backend.system_context()
            );
        }
    }
}
