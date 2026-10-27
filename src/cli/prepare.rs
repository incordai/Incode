//! Building a session the way every front end must, in named steps: the
//! workspace, the intake gate, the operator's settings, the advisory lookups,
//! the project context the agent is told, the lead and its fleet, the guards,
//! and the session itself.
//!
//! # Wired to
//!
//! - **Called by** [`super::CliApp::run`] (the REPL) and
//!   `crate::acp_agent` (`session/new`), both through [`prepare_session`],
//!   which `crate::cli` re-exports.
//! - **Calls** [`super::run_backend`] for the standby agents, and
//!   [`super::timed`] / [`super::mark`] for the startup trace.
//! - **Publishes** [`super::PERMISSION_MODE_ENV`] for the backends built
//!   later in this process.

use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use crate::config::IncordCodeConfig;
use crate::permissions::PermissionMode;
use crate::runtime::lifecycle::LifecycleHooks;
use crate::tools::git_context;
use crate::tools::tool_status::ToolStatusCache;

use super::{mark, render, run_backend, timed, PERMISSION_MODE_ENV};

/// Everything [`prepare_session`] assembles, handed to whoever drives the
/// session: the interactive REPL here, or `incode --acp` in
/// [`crate::acp_agent`].
pub(crate) struct Prepared {
    pub(crate) workspace: std::path::PathBuf,
    pub(crate) incode_dir: std::path::PathBuf,
    pub(crate) admin: crate::admin::AdminConfig,
    pub(crate) permission_mode: PermissionMode,
    pub(crate) tool_cache: ToolStatusCache,
    pub(crate) lifecycle_hooks: LifecycleHooks,
    pub(crate) git_context: Option<git_context::GitContext>,
    pub(crate) system_prompt: Vec<String>,
    /// The output style the session started with, by name.
    pub(crate) output_style: Option<String>,
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
///
/// Each step below runs in the order it always did: several print, several
/// start a lookup on its own thread, and the prompt's blocks keep their places.
pub(crate) fn prepare_session(
    config: &IncordCodeConfig,
    cancel: crate::runtime::cancel::CancelToken,
    booted_at: std::time::Instant,
    session_id: &str,
) -> anyhow::Result<Prepared> {
    let workspace = open_workspace(&config.workspace)?;
    let gate = timed("brief::check", || intake_gate(&workspace));
    let intake_notes = intake_notes(&gate, &workspace, booted_at);
    let spec_prefetch = prefetch_spec_note(&workspace);
    let permission_mode = publish_permission_mode(&config.permission_mode);
    let incode_dir = data_dir();
    let admin = load_admin(&incode_dir, booted_at);
    let lookups = Lookups::start(&admin, &workspace, &gate, spec_prefetch);
    let tool_cache = probe_tools(&incode_dir, booted_at);
    let lifecycle_hooks = lifecycle_hooks(config, &workspace);
    let git_context = timed("git", || git_context::detect_git_context(&workspace));
    mark("git", booted_at);

    let mut prompt = Prompt::begin(&workspace, &incode_dir, booted_at);
    prompt.add_findings(lookups.auditor, lookups.recorder);
    let roles = announce_roles(&admin, &mut prompt);
    let style = apply_style(&workspace, &admin, &mut prompt);
    let (catalogue, agent, fleet) = find_agents(&admin, booted_at)?;
    prompt.place_guards(lookups.guards);
    prompt.place_intake(intake_notes, lookups.spec, booted_at);

    let session_guards = load_guards(&workspace, booted_at)?;
    let mut session = open_session(
        config,
        &workspace,
        &agent,
        &fleet,
        &session_guards,
        &prompt.blocks,
        cancel,
    );
    // The mode the operator asked for, sent to the thing that does the
    // writing. Shown in the banner and never sent, `-p read-only` let an
    // operator believe nothing could be written while the agent wrote.
    session.set_permission_mode(permission_mode.agent_mode());
    start_memory(&mut session, config, &workspace, session_id, prompt.native);
    choose_model(&mut session, config, &roles);

    Ok(Prepared {
        workspace,
        incode_dir,
        admin,
        permission_mode,
        tool_cache,
        lifecycle_hooks,
        git_context,
        system_prompt: prompt.blocks,
        output_style: style.name,
        catalogue,
        agent,
        fleet,
        session,
    })
}

/// The workspace, absolute and canonical, with incode's state in it brought
/// up to date before anything reads it.
fn open_workspace(given: &Path) -> anyhow::Result<PathBuf> {
    // Absolute and canonical ONCE, here, so every tool, guard, snapshot and
    // path the session reports names the same directory however it was given
    // (`-w .` included).
    let workspace =
        crate::tools::file_ops::workspace_root(given).map_err(|e| anyhow::anyhow!("{e:#}"))?;
    // incode's state left in the old `.incord/` moves to `.incode/` before
    // anything below reads it. Here rather than at process start because a
    // protocol session names its own workspace.
    for line in crate::migrate::project_report(&workspace) {
        render::print_info(&line);
    }
    // A `.incode/` made before incode wrote its `.gitignore`, or one missing
    // a line a newer incode ignores, is brought up to date — added to, never
    // rewritten.
    if let Some(line) = crate::folders::heal(&workspace) {
        render::print_info(&line);
    }
    Ok(workspace)
}

/// The intake gate, with the brief healed from its document first.
///
/// Nothing is blocked by the gate: an incomplete brief makes
/// completing the brief the session's first job, not a reason to exit.
/// The work still happens here, in this session, with the owner present
/// to answer what only they can answer.
///
/// # Heal the belief before reporting it missing
///
/// The brief lives in the proxy, and the proxy's write endpoint replaces
/// a whole document with no version to write against — so a belief CAN be
/// lost to a concurrent writer, and one was, live: the project key
/// survived with every field blank.
///
/// The documents are the durable copy. Deriving here means a lost brief
/// is rebuilt by the next session that opens the project, rather than
/// waiting for someone to type `/run` — which was the gap when this ran
/// only in the run handler: the brief stayed empty and the session merely
/// complained about it.
///
/// Nothing is overwritten: `from_docs` carries only what the document
/// says, and `patch_config` writes only the fields an update carries.
fn intake_gate(workspace: &Path) -> crate::brief::Gate {
    let gate = crate::brief::check(workspace);
    if let crate::brief::Gate::Ready(ref brief) = gate {
        write_brief_document(workspace, brief);
        return gate;
    }
    match crate::brief::derive_from_docs(workspace) {
        Ok(_) => {
            crate::trace!("brief", "derived from {}", crate::brief::PRD_PATH);
            crate::brief::check(workspace)
        }
        // No document to derive from is the ordinary case for a new
        // project. The gate below then asks for the brief, which is the
        // useful thing to say — this error is not.
        Err(e) => {
            crate::trace!("brief", "not derived: {e}");
            gate
        }
    }
}

/// A ready brief with no document behind it gets one.
///
/// THE OTHER DIRECTION. A belief with no document behind it is
/// one clobber from gone, because healing rebuilds from the
/// document and there is none. Writing it down means the truth
/// lives in two places and neither is the only copy.
///
/// An existing document is never touched: it is the source, and a
/// drifted brief overwriting it would make the drift the truth.
fn write_brief_document(workspace: &Path, brief: &crate::brief::ProjectBrief) {
    match crate::brief::write_prd_to_docs(workspace, brief) {
        Ok(true) => render::print_info(&format!(
            "Wrote {} from the brief — this project had a belief and no document behind it.",
            crate::brief::PRD_PATH
        )),
        Ok(false) => {}
        Err(e) => crate::trace!("brief", "document not written: {e}"),
    }
}

/// What the gate and the dependency check say needs writing before work
/// starts.
///
/// Two intake findings, gathered together so the session is told
/// everything that needs writing before it starts, rather than one thing
/// now and another after the first is fixed.
///
/// The dependency check needs the workspace AND the document, which only
/// meet here: `research` reads the manifest the build reads, and the PRD
/// comes from the brief.
fn intake_notes(
    gate: &crate::brief::Gate,
    workspace: &Path,
    booted_at: std::time::Instant,
) -> Vec<String> {
    let mut notes: Vec<String> = gate.intake_task().into_iter().collect();
    mark("intake_task", booted_at);
    if let crate::brief::Gate::Ready(brief) = gate {
        if let Some(note) =
            crate::research::intake_note(&crate::research::unvetted(workspace, &brief.prd))
        {
            notes.push(note);
        }
    }
    notes
}

/// The rest of the spec set's note, started on its own thread.
///
/// The rest of the spec set — architecture, contracts, conventions,
/// verification, assumptions. The loop reads these; a PRD on its own
/// leaves every later task guessing at names and shapes.
///
/// Started here, collected once the agents are found: the note searches
/// memory for leads on the waiting deadline, and waited in line for it —
/// measured at 1.2 s of `session/new`, the whole of its deadline. On its
/// own thread it overlaps the rest of startup instead.
fn prefetch_spec_note(workspace: &Path) -> JoinHandle<Option<String>> {
    let workspace = workspace.to_path_buf();
    std::thread::spawn(move || crate::spec::intake_note(&workspace))
}

/// The session's permission mode, published for the rest of the process.
///
/// PUBLISHED FOR THE REST OF THE PROCESS, so a backend built somewhere
/// that cannot see this config still runs under the mode the operator
/// chose. See `PERMISSION_MODE_ENV` for why this is a variable rather
/// than an argument, and why it stops at this process boundary.
fn publish_permission_mode(asked: &str) -> PermissionMode {
    let permission_mode = PermissionMode::from_str_lossy(asked);
    std::env::set_var(PERMISSION_MODE_ENV, permission_mode.as_str());
    permission_mode
}

/// incode's data directory (history, sessions, memory), made if missing.
///
/// One resolver for the data directory, shared with every caller that
/// reads settings from it — two answers here means the operator edits one
/// file and the harness reads another.
fn data_dir() -> PathBuf {
    let incode_dir = crate::admin::AdminConfig::default_dir();
    let _ = std::fs::create_dir_all(&incode_dir);
    incode_dir
}

/// The operator's standing decisions, or the built-in ones said loudly.
///
/// A flag beats an env var beats this file beats the built-in fallback, and
/// every value can say which of the four decided it — see `/admin`.
fn load_admin(incode_dir: &Path, booted_at: std::time::Instant) -> crate::admin::AdminConfig {
    match timed("admin", || {
        mark("admin", booted_at);
        crate::admin::AdminConfig::load(incode_dir)
    }) {
        Ok(admin) => admin,
        Err(e) => {
            // Settings that quietly stop applying are worse than a loud
            // failure: the run looks normal while doing something else.
            render::print_error(&e);
            crate::admin::AdminConfig::default()
        }
    }
}

/// One advisory lookup running on its own thread, or none when its plugin
/// is switched off.
type Prefetch = Option<JoinHandle<Option<String>>>;

/// The lookups started early and collected once the rest of startup has run.
///
/// # Start the advisory lookups now; read them when the prompt is built
///
/// The recorder and auditor lookups used to run where their answers are
/// used, on the [`crate::recall::WAITING_TIMEOUT`] of 1200ms, and both were
/// silently losing to it: measured from the service's own request log,
/// `/v1/memory/watchman/handoff` exceeded 1200ms on 198 of 506 calls —
/// 39.1% — every one of them answering `status=200`. The store was
/// working and the harness had already stopped listening, so two sessions
/// in five started believing a project with real recorded history had
/// none.
///
/// Started here they overlap the six-odd seconds of startup that follow —
/// tool probing, git, plugins, agent discovery — and are read at the end
/// of it. The deadline is not raised on the waiting path, because the
/// reason it is 1200ms there has not changed: an 8.08s lookup once left
/// the operator staring at a terminal that looked dead. Nothing waits on
/// these, so nothing can be held up by them.
///
/// Gated on the same plugin switches as before, so an operator who turned
/// one off still pays nothing for it.
struct Lookups {
    /// The spec set's intake note ([`prefetch_spec_note`]).
    spec: JoinHandle<Option<String>>,
    /// The owner's reserved categories.
    guards: Prefetch,
    /// What this project has already been observed to have solved.
    recorder: Prefetch,
    /// What the auditor already knows is wrong here.
    auditor: Prefetch,
}

impl Lookups {
    /// Start the guards, recorder and auditor lookups, in that order, beside
    /// the spec note already running.
    fn start(
        admin: &crate::admin::AdminConfig,
        workspace: &Path,
        gate: &crate::brief::Gate,
        spec: JoinHandle<Option<String>>,
    ) -> Self {
        // The owner's reserved categories: requested here and collected once the
        // agents are found, for the same reason as the spec note — on the
        // waiting deadline in line it was up to 1.2 s of `session/new`.
        let guards = admin
            .plugins
            .guards
            .then(|| std::thread::spawn(crate::guards::context_for_session));
        let scope = recorder_scope(gate, workspace);
        let recorder = admin.plugins.recorder.then(|| {
            let workspace = workspace.to_path_buf();
            std::thread::spawn(move || {
                crate::recorder::context_for_session_within(
                    &workspace,
                    Some(scope.as_str()),
                    crate::recall::PREFETCH_TIMEOUT,
                )
            })
        });
        let auditor = admin.plugins.auditor.then(|| {
            let workspace = workspace.to_path_buf();
            std::thread::spawn(move || {
                crate::auditor::context_for_session_within(
                    &workspace,
                    crate::recall::PREFETCH_TIMEOUT,
                )
            })
        });
        Self {
            spec,
            guards,
            recorder,
            auditor,
        }
    }
}

/// The project's stable id for the recorder.
///
/// Taken from the brief the gate already resolved — no extra call. Empty
/// when the service has assigned none, and `recorder::key_for` then falls
/// back to the workspace path.
fn recorder_scope(gate: &crate::brief::Gate, workspace: &Path) -> String {
    match gate {
        crate::brief::Gate::Ready(brief) if !brief.scope.is_empty() => brief.scope.clone(),
        _ => {
            let norm = workspace.display().to_string().replace('\\', "/");
            if norm.ends_with("/incord-code") || norm == "incord-code" {
                "proj-incordcode".to_string()
            } else {
                String::new()
            }
        }
    }
}

/// The installed CLI tools, from the cache and probed again (Feature 3).
fn probe_tools(incode_dir: &Path, booted_at: std::time::Instant) -> ToolStatusCache {
    let tool_cache_path = incode_dir.join("tool-status.json");
    let mut tool_cache = timed("tool probe", || {
        mark("tools", booted_at);
        ToolStatusCache::load_or_create(&tool_cache_path)
    });
    tool_cache.detect_all();
    tool_cache
}

/// The session start/end scripts (Feature 7): the configured ones, else the
/// project's.
///
/// A project's run only once the operator approved them as they are
/// (`runtime::lifecycle::usable`): they arrive with whatever was cloned.
fn lifecycle_hooks(config: &IncordCodeConfig, workspace: &Path) -> LifecycleHooks {
    if config.lifecycle_hooks.has_any() {
        return config.lifecycle_hooks.clone();
    }
    let (hooks, note) =
        crate::runtime::lifecycle::usable(workspace, &crate::admin::AdminConfig::default_dir());
    if let Some(note) = note {
        render::print_info(&note);
    }
    hooks
}

/// What the agent is told about this project, beyond each instruction.
///
/// ONLY what this harness knows and the agent does not. It used to also
/// carry a role ("You are Incord Code"), a methodology, a thinking style
/// and a manual for tools — all written to drive a raw model. Appended to
/// a working agent's own prompt, that is a competing identity and a
/// manual for tools it does not have, which is worse than sending
/// nothing. The agent is already a coding agent; it needs the PROJECT.
struct Prompt {
    /// The blocks, in the order the agent reads them.
    blocks: Vec<String>,
    /// The blocks below that the memory engine's session-start block carries
    /// too — the rules, the recorder's handoff, the auditor's findings. Kept
    /// in the prompt, and named here, so that when the engine answers they
    /// are dropped rather than sent twice (see `AgentSession::start_memory`).
    native: Vec<String>,
    /// Where the intake task goes, once the spec note is collected.
    intake_at: usize,
    /// Where the guards block goes, once its lookup is collected.
    guards_at: usize,
}

impl Prompt {
    /// The blocks known before any lookup is collected: how to ask the
    /// owner, the project's instructions, the rules and the skills index —
    /// with the places the intake task and the guards block will go.
    fn begin(workspace: &Path, incode_dir: &Path, booted_at: std::time::Instant) -> Self {
        let mut blocks: Vec<String> = Vec::new();
        let mut native: Vec<String> = Vec::new();

        // How to put a decision to the owner as OPTIONS rather than an open
        // question. Enumerating the choices is most of the work of deciding,
        // and the agent has just done it — an open question throws it away.
        blocks.push(crate::ask::prompt_block());

        // The project's own instruction files. An agent CLI no longer reads them
        // for itself — it runs as a model only — and no other model ever did.
        if let Some(block) = crate::instructions::project_block(workspace) {
            blocks.push(block);
        }

        // The intake task rides the system prompt rather than one turn's
        // context: it is not finished until the brief is, which may take several
        // exchanges, and a task that fell out of context after one turn would be
        // abandoned halfway through gathering. Put in HERE, at this place in the
        // prompt, once the spec note is collected (after the agents are found).
        let intake_at = blocks.len();

        mark("before-plugins", booted_at);
        // The rules that apply to every action, fetched once. They were parsed
        // and dropped before this, so the agent never saw them at all; in the
        // cached prefix they cost almost nothing after the first turn.
        if let Some(block) = timed("rules (always)", crate::rules::session_context) {
            native.push(block.clone());
            blocks.push(block);
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
        if let Some(block) = timed("skills", || crate::skills::session_context(incode_dir)) {
            blocks.push(block);
        }

        // The guards block goes HERE, collected with the spec note below.
        let guards_at = blocks.len();
        Self {
            blocks,
            native,
            intake_at,
            guards_at,
        }
    }

    /// The auditor's findings, then the recorder's handoff, each said out
    /// loud when it arrives.
    ///
    /// What the auditor already knows is wrong here, and what this project
    /// has already been observed to have solved. Once at session start rather
    /// than per turn: they describe the project, not the question, so
    /// re-fetching them every turn would pay for the same block over and
    /// over — which is the exact waste the recorder exists to stop.
    /// Rediscovering a filed bug costs a turn, and the finding is already
    /// written down.
    ///
    /// Silent when a lookup is unreachable. It enriches a session; losing it
    /// costs context, not the session. A panicked thread is treated as
    /// nothing found, never as an excuse to fetch again on the waiting path —
    /// that is the deadline the prefetch exists to escape.
    fn add_findings(&mut self, auditor: Prefetch, recorder: Prefetch) {
        if let Some(block) = auditor.and_then(|p| timed("auditor", || p.join().ok().flatten())) {
            render::print_info(&format!(
                "auditor: {} open finding(s) in this project",
                block.lines().count().saturating_sub(1)
            ));
            self.push_native(block);
        }
        if let Some(block) = recorder.and_then(|p| timed("recorder", || p.join().ok().flatten())) {
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
            self.push_native(block);
        }
    }

    /// A block the memory engine's session-start context also carries.
    fn push_native(&mut self, block: String) {
        self.native.push(block.clone());
        self.blocks.push(block);
    }

    /// The owner's reserved categories, where they always stood: after the
    /// skills index, ahead of the findings.
    ///
    /// What the owner has reserved for their own decision. Read from the body,
    /// where the categories are the owner's standing policy across every
    /// project and every agent — not restated here, and not classified here
    /// either: the service's own hook does that for any agent on this machine.
    /// Telling the model in advance saves the turn it would otherwise spend
    /// discovering a stop.
    fn place_guards(&mut self, guards: Prefetch) {
        if let Some(block) = guards.and_then(|p| timed("guards", || p.join().ok().flatten())) {
            self.blocks.insert(self.guards_at, block);
        }
    }

    /// The intake task — the gate's notes and the spec note — said to the
    /// operator and put where it always stood in the prompt.
    ///
    /// Collected after the guards, since the guards block comes after the
    /// intake and inserting it first keeps `intake_at` where it was.
    fn place_intake(
        &mut self,
        mut notes: Vec<String>,
        spec: JoinHandle<Option<String>>,
        booted_at: std::time::Instant,
    ) {
        let spec_note = timed("spec note", || spec.join().ok().flatten());
        notes.extend(spec_note);
        mark("spec-note", booted_at);
        if let Some(task) = intake_task(&notes) {
            render::print_info(&task);
            self.blocks.insert(
                self.intake_at,
                format!(
                    "<intake>\nThis project cannot be worked on until its brief is \
                     complete. That is your first job, ahead of any code.\n\n{task}\n</intake>"
                ),
            );
        }
    }
}

/// The intake notes as the one task the session is given, or none.
fn intake_task(notes: &[String]) -> Option<String> {
    (!notes.is_empty()).then(|| notes.join("\n\n"))
}

/// Which model serves which kind of delegated work, said at startup, and the
/// role table for the prompt.
///
/// Read BEFORE the provider client is built. These lines tell the
/// operator what the fleet resolved to and warn about a reviewer
/// sharing the writer's lineage — and a diagnostic printed after a
/// failed client build is one nobody ever sees. Read once here so the
/// fleet is visible at startup rather than inferred from a bill later.
fn announce_roles(
    admin: &crate::admin::AdminConfig,
    prompt: &mut Prompt,
) -> crate::roles::RoleTable {
    let roles = admin.resolve_roles().value;
    if !roles.is_empty() {
        render::print_info(&roles.summary_line());
    }
    // A reviewer sharing the writer's lineage reads as independent and is
    // not. Warned loudly rather than refused: the operator may know
    // something the harness does not, but they should know they did it.
    if let Some(warning) = roles.review_independence_warning() {
        render::print_error(&warning);
    }
    // A ceiling nobody can see is a ceiling nobody plans around.
    let budget = admin.resolve_budget().value;
    if let Some(line) = budget.summary_line() {
        render::print_info(&line);
    }
    // The orchestrator cannot choose a role it has not been told about. This
    // block carries both the choices and the rule that a role never settles
    // whether the work is correct.
    if let Some(block) = roles.prompt_block() {
        prompt.blocks.push(block);
    }
    roles
}

/// The answering style the person chose (`crate::styles`), said and added to
/// the prompt.
///
/// Pushed LAST and kept last — the guards and the intake are inserted
/// earlier — after every block it is weighed against: it shapes how replies
/// read, and its own words say the rules above it win.
fn apply_style(
    workspace: &Path,
    admin: &crate::admin::AdminConfig,
    prompt: &mut Prompt,
) -> crate::styles::ForSession {
    let style = crate::styles::for_session(workspace, admin.output_style.as_deref());
    for note in &style.notes {
        render::print_info(note);
    }
    for warning in &style.warnings {
        render::print_error(warning);
    }
    if let Some(block) = &style.block {
        prompt.blocks.push(block.clone());
    }
    style
}

/// The agent catalogue, the lead it names, and the fleet of everything
/// installed and drivable.
///
/// TWO ROUTES, and this harness keeps the judging on both — the ledger,
/// the check, the guards, the contracts, the budget, the owner channel.
///
/// An installed agent on the operator's own subscription brings its own
/// tools and needs no credential here. A model over a configured
/// endpoint brings nothing, so the harness supplies the tools itself
/// (see `crate::toolbox`) and the endpoint comes from the environment.
///
/// NEITHER means no way to do anything, which is fatal and said plainly
/// rather than discovered at the first prompt.
///
/// Both routes are in ONE catalogue, read once here and kept for
/// `/agent` and `/run`: the lead is the operator's choice when usable,
/// else the first usable local agent, else a usable API agent.
fn find_agents(
    admin: &crate::admin::AdminConfig,
    booted_at: std::time::Instant,
) -> anyhow::Result<(
    crate::agent::catalogue::Catalogue,
    crate::agent::DetectedAgent,
    crate::fleet::Fleet,
)> {
    let catalogue = timed("agent catalogue", || {
        crate::agent::catalogue::Catalogue::gather(admin)
    });
    let agent = catalogue
        .lead(crate::agent::registry::preferred_agent().as_deref())
        .map_err(|e| anyhow::anyhow!(e))?;
    // Everything installed and drivable. With more than one, roles can route
    // between them and work can move when one runs out. The catalogue has
    // just probed this machine for the same agents: probing again cost
    // another 0.7 s of `session/new` and could only find the same ones.
    let fleet = timed("agent discovery", || {
        mark("fleet", booted_at);
        crate::fleet::Fleet::new(catalogue.found().to_vec())
    });
    render::print_info(&fleet.summary_line());
    Ok((catalogue, agent, fleet))
}

/// The protected-path policy, which the session refuses to run without.
///
/// The protected paths reach the agent as deny rules it enforces itself,
/// so a write to one never happens — prevention, with the sentinel in
/// `agent::worker` behind it for whatever the rules did not cover.
fn load_guards(
    workspace: &Path,
    booted_at: std::time::Instant,
) -> anyhow::Result<crate::guards::Guards> {
    mark("before-guards", booted_at);
    crate::guards::Guards::load(workspace).map_err(|e| {
        anyhow::anyhow!(
            "the protected-path policy could not be read ({e}). Running without it would leave the check editable by the thing being judged."
        )
    })
}

/// The session on the lead, with every other agent in the fleet standing by,
/// the operator's hooks set and their `session_start` commands run.
fn open_session(
    config: &IncordCodeConfig,
    workspace: &Path,
    agent: &crate::agent::DetectedAgent,
    fleet: &crate::fleet::Fleet,
    guards: &crate::guards::Guards,
    system_prompt: &[String],
    cancel: crate::runtime::cancel::CancelToken,
) -> crate::cli::agent_turn::AgentSession {
    let standby = standby_agents(workspace, agent, fleet, guards, system_prompt);
    let mut session = crate::cli::agent_turn::AgentSession::new(
        workspace.to_path_buf(),
        &agent.id,
        agent.command.clone(),
        guards,
        // Everything assembled above — the intake task, the guard rules, the
        // auditor's findings, the recorder's handoff, the role table. This
        // was built and DROPPED when the harness stopped owning the loop:
        // it compiled, the tests passed, and the agent silently stopped
        // being told any of it.
        system_prompt.to_vec(),
    )
    .with_standby(standby)
    .with_cancel(cancel)
    .with_extra_dirs(config.additional_dirs.clone());
    // The operator's commands for the events the session fires itself (a
    // person's line); the toolbox has its own for the tool events.
    session.set_operator_hooks(crate::hooks::HookRunner::from_settings());
    // The operator's `session_start` commands, now the session exists — the
    // REPL and `--acp` both come through here. Shown, never asked.
    for said in session.operator_session_start("startup") {
        render::print_info(&format!("[session_start hook] {said}"));
    }
    if !config.additional_dirs.is_empty() {
        render::print_info(&format!(
            "Also working in {} additional director(ies) — guards and undo cover them.",
            config.additional_dirs.len()
        ));
    }
    session
}

/// A backend for every agent in the fleet but the lead, said when there is
/// one.
fn standby_agents(
    workspace: &Path,
    lead: &crate::agent::DetectedAgent,
    fleet: &crate::fleet::Fleet,
    guards: &crate::guards::Guards,
    system_prompt: &[String],
) -> Vec<Box<dyn crate::agent::AgentBackend>> {
    let standby: Vec<Box<dyn crate::agent::AgentBackend>> = fleet
        .agents()
        .iter()
        .filter(|a| a.id != lead.id)
        .map(|other| run_backend(workspace, &other.id, &other.command, guards, system_prompt))
        .collect();
    if !standby.is_empty() {
        render::print_info(&format!(
            "{} standby agent(s) available if this one cannot do the work.",
            standby.len()
        ));
    }
    standby
}

/// The memory engine's session, opened by incode, and checkpoints kept on
/// disk under this session's id.
///
/// The agent CLI used to fire these hooks from its own settings, and it runs
/// as a model only now. Its session-start context replaces the `native`
/// blocks it overlaps. The checkpoints are what make `/undo` and `/rewind`
/// work after a restart.
fn start_memory(
    session: &mut crate::cli::agent_turn::AgentSession,
    config: &IncordCodeConfig,
    workspace: &Path,
    session_id: &str,
    native: Vec<String>,
) {
    let memory = crate::hooks::memory::MemoryHooks::new(
        crate::hooks::engine::Engine::locate(),
        workspace,
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
    session.keep_checkpoints(session_id);
}

/// The model the session opens on: `-m` first, then the role table.
///
/// A model named on the command line is the most specific thing the
/// operator said, and it was previously reported by `/admin` while the
/// session ran on something else.
fn choose_model(
    session: &mut crate::cli::agent_turn::AgentSession,
    config: &IncordCodeConfig,
    roles: &crate::roles::RoleTable,
) {
    match config.model_from_flag.as_deref() {
        Some(model) => session.choose_model(model),
        None => {
            // Role routing applies to interactive work too: the model the
            // session opens on is the one chosen for conversation.
            let session_models = roles.candidates_for("plan");
            if !session_models.is_empty() {
                session.set_models(session_models);
            }
        }
    }
}

#[cfg(test)]
mod tests;
