//! Every tool the harness can offer a model, in one place.
//!
//! # Why this exists
//!
//! A model reached over an API brings nothing. It cannot read a file, run a
//! command or search a tree unless this program hands it the means and then
//! carries out what it asks for. Driving an installed agent CLI is the other
//! way round — that CLI supplies its own tools — but it is the convenient case,
//! not the general one, and building only for it leaves every other model
//! unable to do the work at all.
//!
//! So every route uses these: the API route calls [`Toolbox::run_call`]
//! directly, and a CLI used as a model reaches the same toolbox through
//! [`crate::tool_bridge`]. The guards, the permission mode, the memory
//! engine's hooks, the person to ask and the checkpoints all sit in
//! [`Toolbox::run_call`], so they apply to every tool on every route.
//!
//! # What was already here
//!
//! Almost all of it. `read_file`, `write_file`, `edit_file`, `glob_search`,
//! `grep_search` and `execute_bash` were written, tested and then ORPHANED when
//! the old dispatch loop was removed: implementations with no caller. In the
//! other direction `verify`, the recall pair and the memory tools each carried a
//! schema with nothing to collect it — advertisements with no audience.
//!
//! Neither half was reachable, and neither was visibly broken: `pub` keeps the
//! compiler quiet about unused code, and both halves carry their own passing
//! tests. Nineteen green tests covered code that nothing called.
//!
//! So this module writes almost no new behaviour. It joins the two halves and
//! makes the join testable, which is the part that was actually missing.
//!
//! # The invariant
//!
//! Every tool advertised can be run, and every tool that can be run is
//! advertised. Both directions are asserted, because either one failing alone is
//! silent: a schema with no implementation makes a model call something that
//! answers "unknown tool", and an implementation with no schema is what the last
//! six months of dead code looked like.
//!
//! # Reach: one table decides the mode rule and the engine's failure policy
//!
//! [`reach_of`] classifies every tool once. A read-only session runs only the
//! tools that observe; a prompting one asks before every other call; and when
//! the memory engine cannot decide, the calls that change or run something are
//! refused while observing ones run. A tool missing from the table is not a
//! tool at all — the call is refused before anything else is asked.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use serde_json::{json, Value};

use crate::agent::AgentBackend;

mod checks;
mod convert;
mod edits;
mod listing;
mod outline;
mod shell;
mod team;
mod todo;
mod validate;

pub(crate) use todo::{parse_todos, Priority, TodoStatus};

/// Read a file's contents.
pub const READ: &str = "Read";
/// Create or overwrite a file.
pub const WRITE: &str = "Write";
/// Replace a string inside a file.
pub const EDIT: &str = "Edit";
/// Several replacements to one file, all or none.
pub const MULTI_EDIT: &str = "MultiEdit";
/// A unified diff across one or more files, all or none.
pub const APPLY_PATCH: &str = "ApplyPatch";
/// Replace, insert or delete one cell of a notebook.
pub const NOTEBOOK_EDIT: &str = "NotebookEdit";
/// List one directory.
pub const LS: &str = "LS";
/// Find files by name pattern.
pub const GLOB: &str = "Glob";
/// Search file contents.
pub const GREP: &str = "Grep";
/// A source file's symbols, from its syntax tree.
pub const OUTLINE: &str = "Outline";
/// An office document to PDF, when LibreOffice is installed.
pub const CONVERT_DOCUMENT: &str = "ConvertDocument";
/// Run a shell command, in the foreground or as a background job.
pub const BASH: &str = "Bash";
/// Read what a background job has said since it was last read.
pub const BASH_OUTPUT: &str = "BashOutput";
/// Stop a background job.
pub const KILL_SHELL: &str = "KillShell";
/// The model's task list for the session.
pub const TODO_WRITE: &str = "TodoWrite";
/// Run the project's checker and report its errors.
pub const DIAGNOSTICS: &str = "Diagnostics";
/// Read one skill's text, once it turns out to apply.
pub const SKILL: &str = "Skill";
/// Hand a self-contained piece of work to a fresh agent.
pub const AGENT: &str = "Agent";
/// Put a multiple-choice question to the owner.
///
/// THIS EXACT NAME. The memory engine routes a question to the owner's app
/// only for a tool it recognises by name, so a rename here silently turns
/// every question back into a local-only one.
pub const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// What a tool can do to the world beyond being read from.
///
/// Decides the permission mode's rule and the engine's failure policy, in
/// [`Toolbox::run_call`], for every tool through one table: [`reach_of`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Reads: files, the web, the memory service, a job's output.
    Observe,
    /// Touches only the session's own state, or asks the person: the task
    /// list, a question.
    Session,
    /// Writes a shared record outside the workspace's files: the memory
    /// store, the project brief, the team channel.
    Record,
    /// Changes files in the workspace.
    Change,
    /// Runs code or stops a process.
    Run,
}

impl Reach {
    /// Whether a call runs under every mode, never asked about. Only the
    /// tools that observe, and the session's own state.
    #[must_use]
    pub fn only_observes(self) -> bool {
        matches!(self, Self::Observe | Self::Session)
    }

    /// Whether a call is REFUSED when the engine cannot decide. The owner's
    /// policy: a change is never made unguarded; a read runs, traced.
    #[must_use]
    pub fn fails_closed(self) -> bool {
        !self.only_observes()
    }

    /// The word the docs and the trace use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Session => "session",
            Self::Record => "record",
            Self::Change => "change",
            Self::Run => "run",
        }
    }
}

/// The reach of every tool this toolbox runs, or `None` for a name it does
/// not know — which is then refused as no tool at all.
#[must_use]
pub fn reach_of(name: &str) -> Option<Reach> {
    use crate::knowledge::{
        BLAST_RADIUS_TOOL, FORGET_TOOL, REMEMBER_TOOL, SEARCH_TOOL, SKELETON_TOOL, SYMBOL_TOOL,
    };
    Some(match name {
        READ | GLOB | GREP | LS | OUTLINE | BASH_OUTPUT | SKILL => Reach::Observe,
        crate::web::FETCH_TOOL | crate::web::SEARCH_TOOL => Reach::Observe,
        crate::recall::INDEX_TOOL | crate::recall::GET_TOOL => Reach::Observe,
        SEARCH_TOOL | SYMBOL_TOOL | SKELETON_TOOL | BLAST_RADIUS_TOOL => Reach::Observe,
        crate::recorder::LOOKUP_TOOL | crate::team_mcp::READ_TOOL => Reach::Observe,
        TODO_WRITE | ASK_USER_QUESTION => Reach::Session,
        REMEMBER_TOOL | FORGET_TOOL | crate::brief::SAVE_TOOL => Reach::Record,
        crate::team_mcp::ASK_TOOL | crate::team_mcp::TELL_TOOL | crate::team_mcp::ANSWER_TOOL => {
            Reach::Record
        }
        WRITE | EDIT | MULTI_EDIT | APPLY_PATCH | NOTEBOOK_EDIT => Reach::Change,
        // It writes a PDF into the workspace.
        CONVERT_DOCUMENT => Reach::Change,
        BASH | KILL_SHELL | DIAGNOSTICS | AGENT | crate::verify::VERIFY_TOOL => Reach::Run,
        _ => return None,
    })
}

/// What a session's tools keep between calls: its background jobs and the
/// model's task list.
///
/// One per session, shared by every backend the session holds — a failover
/// to a standby keeps the same jobs and the same list. The REPL's `/bg`,
/// `/jobs` and `/kill` use the same jobs, and the stop hook is held while any
/// of them runs.
pub struct SessionTools {
    jobs: Mutex<crate::tools::bash_jobs::BashJobManager>,
    todos: Mutex<Vec<todo::Todo>>,
}

impl SessionTools {
    /// Fresh state; `filter_env` gives background jobs the filtered
    /// environment.
    #[must_use]
    pub fn new(filter_env: bool) -> Arc<Self> {
        Arc::new(Self {
            jobs: Mutex::new(
                crate::tools::bash_jobs::BashJobManager::new().with_env_filter(filter_env),
            ),
            todos: Mutex::new(Vec::new()),
        })
    }

    /// The session's background jobs.
    pub fn jobs(&self) -> MutexGuard<'_, crate::tools::bash_jobs::BashJobManager> {
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many background jobs are running now.
    #[must_use]
    pub fn running_jobs(&self) -> usize {
        self.jobs().running_count()
    }

    fn todos(&self) -> MutexGuard<'_, Vec<todo::Todo>> {
        self.todos.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What the memory engine decided about one call, before it runs.
enum Gate {
    /// Run it; `context` is guidance for this action, handed to the model with
    /// the result.
    Run { context: Option<String> },
    /// Do not run it; the model is told why, as an error.
    Refused(String),
    /// A question the owner already answered in their app: the answer IS the
    /// result, and it is not an error.
    Answered(String),
}

/// How deep subagents may nest.
///
/// One level of delegation is what the pattern is for; the second is slack. A
/// subagent that can spawn subagents that can spawn subagents is a bill with no
/// ceiling and a tree nobody can read.
const MAX_DEPTH: u8 = 2;

/// What a subagent is told about its position.
///
/// Short on purpose. It inherits no conversation, so everything it needs is in
/// the task it was handed — and a long preamble here is paid on every
/// delegation.
const SUBAGENT_SYSTEM: &str = "\
    You are doing ONE piece of work handed to you by another agent, in its \
    workspace, with the same tools. You do not have its conversation and cannot \
    ask it questions. Do the work and report what you found or changed, briefly \
    and concretely — your reply is the only thing that goes back.";

/// One conversation a subagent opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawned {
    /// The subagent's own session id.
    pub session: String,
    /// What it was asked to do, trimmed.
    pub title: String,
}

/// The tools this harness offers, and the workspace they act on.
pub struct Toolbox {
    workspace: PathBuf,
    /// Whether a spawned command gets the filtered environment.
    ///
    /// Carried rather than read at the call site because the shell tool runs
    /// commands the MODEL chose: an unfiltered child inherits every variable
    /// this process holds, credentials included.
    filter_env: bool,
    /// The operator's protected paths.
    ///
    /// # Why this lives here rather than at the call site
    ///
    /// An agent CLI enforces the guards itself — the harness hands them over and
    /// the write never happens. A model driven over an API has no such layer:
    /// these functions ARE the write. Without this the protected paths would be
    /// enforced on one route and not the other, which is the same as not being
    /// enforced.
    ///
    /// Kept as a `Result` on purpose. A guards file that fails to parse must not
    /// read as "no guards" — that is the one failure mode the guards module
    /// singles out as the worst, because everything keeps working and nothing is
    /// protected. Held as an error, it refuses the write instead.
    guards: Result<crate::guards::Guards, String>,
    /// How many agents deep this one is. Zero is the one the operator started.
    depth: u8,
    /// The conversations subagents opened, so the session record holds the tree
    /// that actually ran rather than only its root.
    ///
    /// A mutex because [`Toolbox::run`] takes `&self`: running a tool is not a
    /// mutation of the toolbox from the caller's side, and threading `&mut`
    /// through every tool to record two strings would be the tail wagging the
    /// dog.
    children: Mutex<Vec<Spawned>>,
    checkpoints: Option<Arc<Mutex<crate::tools::checkpoint::CheckpointStore>>>,
    /// The memory engine, asked before every call once the operator's guards
    /// have passed it. `None` when none is named or installed.
    engine: Option<crate::hooks::engine::Engine>,
    /// The conversation these calls belong to, as the engine is told it.
    session: String,
    /// Set once the session's own id was given ([`Self::pin_session`]): from
    /// then on a backend's id for its conversation does not replace it, so
    /// every engine event of the session names the same one.
    session_pinned: bool,
    /// Directories beyond the workspace the session may work in. A file tool
    /// given an absolute path inside one acts there, under the same guards.
    extra_dirs: Vec<PathBuf>,
    /// Who can be asked when the engine wants a person — the terminal, or the
    /// ACP client. `None` when nobody is there.
    prompter: Option<Arc<dyn crate::hooks::engine::Prompter>>,
    /// Tripped when the operator stops the turn: a wait on the engine or on a
    /// person ends then rather than at its ten-minute bound.
    cancel: Option<crate::runtime::cancel::CancelToken>,
    /// How much the session lets a call do, when the session said. `None`
    /// leaves every call to the guards and the engine alone — the route whose
    /// caller has no mode to give.
    mode: Option<crate::permissions::PermissionMode>,
    /// The session's background jobs and task list; this toolbox's own until
    /// a session shares its state ([`Self::share_session`]).
    state: Arc<SessionTools>,
    /// The team channel, spoken on as the role this agent holds.
    team: team::Team,
    /// LibreOffice's `soffice`, looked for once, the first time it matters.
    /// `ConvertDocument` is offered only when it was found.
    soffice: OnceLock<Option<PathBuf>>,
}

impl Toolbox {
    /// A toolbox over one workspace, with that workspace's guards in force.
    ///
    /// The guards are loaded here rather than passed in so that the protected
    /// paths apply by DEFAULT. A safety that has to be opted into is one that
    /// some call site will eventually forget.
    #[must_use]
    pub fn new(workspace: &Path, filter_env: bool) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            filter_env,
            guards: crate::guards::Guards::load(workspace),
            depth: 0,
            children: Mutex::new(Vec::new()),
            checkpoints: None,
            engine: None,
            session: String::new(),
            session_pinned: false,
            extra_dirs: Vec::new(),
            prompter: None,
            cancel: None,
            mode: None,
            state: SessionTools::new(filter_env),
            team: team::Team::new(workspace),
            soffice: OnceLock::new(),
        }
    }

    /// LibreOffice, when it is installed where [`convert::find_soffice`]
    /// looks.
    fn soffice(&self) -> Option<&Path> {
        self.soffice
            .get_or_init(|| convert::find_soffice(&|name| std::env::var_os(name)))
            .as_deref()
    }

    /// Whether this session offers `name`. Every tool is offered except
    /// `ConvertDocument` where LibreOffice was not found.
    fn offered(&self, name: &str) -> bool {
        name != CONVERT_DOCUMENT || self.soffice().is_some()
    }

    /// This toolbox with LibreOffice found at `soffice`, or not at all,
    /// whatever this machine has installed.
    #[cfg(test)]
    pub(crate) fn with_soffice(self, soffice: Option<PathBuf>) -> Self {
        Self {
            soffice: OnceLock::from(soffice),
            ..self
        }
    }

    /// The session's permission mode, decided HERE for every call.
    ///
    /// Read-only refuses what changes the workspace or runs code; `prompt`
    /// asks the person before each of those; the writing modes leave them to
    /// the guards and the engine.
    pub fn set_permission_mode(&mut self, mode: crate::permissions::PermissionMode) {
        self.mode = Some(mode);
    }

    /// Ask this engine before every call. Opt-in here, and done by the one
    /// place that builds a backend for real work, so a toolbox built for a
    /// test never reaches the owner's engine.
    #[must_use]
    pub fn with_engine(mut self, engine: Option<crate::hooks::engine::Engine>) -> Self {
        self.engine = engine;
        self
    }

    /// The conversation id the engine is given with each call — unless the
    /// session's own id was pinned, which wins.
    pub fn set_session(&mut self, session: &str) {
        if !self.session_pinned {
            self.session = session.to_string();
        }
    }

    /// The session's own id, given to the engine with every call from now
    /// on. The engine's session-start, prompt and stop events carry it too,
    /// so a card raised here and the capture of the turn name one session.
    pub fn pin_session(&mut self, session: &str) {
        self.session = session.to_string();
        self.session_pinned = true;
    }

    /// Use the session's jobs and task list rather than this toolbox's own,
    /// so every backend the session holds sees the same ones.
    pub fn share_session(&mut self, state: Arc<SessionTools>) {
        self.state = state;
    }

    /// Who this agent speaks as on the team channel, for the task in hand.
    /// Set by the harness, never by the model: what is said there is
    /// recorded, and a name the model chose would be a claim filed as a fact.
    pub fn set_team_identity(&mut self, role: &str) {
        self.team.speak_as(role);
    }

    /// Directories beyond the workspace where the file tools may act. The
    /// operator's guards apply inside them as in the workspace.
    pub fn set_extra_dirs(&mut self, dirs: Vec<PathBuf>) {
        self.extra_dirs = dirs;
    }

    /// The root a file tool call acts under — see [`Self::root_for_path`].
    fn root_for(&self, input: &Value) -> &Path {
        match input.get("path").and_then(Value::as_str) {
            Some(path) => self.root_for_path(path),
            None => &self.workspace,
        }
    }

    /// The root `path` is acted on under: an extra directory when it is an
    /// absolute path inside one, otherwise the workspace. Containment is
    /// decided by the same check the file tools apply, so a path this picks a
    /// root for is a path the tool will accept there.
    fn root_for_path(&self, path: &str) -> &Path {
        if self.extra_dirs.is_empty() || !Path::new(path).is_absolute() {
            return &self.workspace;
        }
        let probe = json!({ "path": path }).to_string();
        let inside = |root: &Path| crate::tools::file_ops::target_path(root, &probe).is_ok();
        if inside(&self.workspace) {
            return &self.workspace;
        }
        self.extra_dirs
            .iter()
            .find(|root| inside(root))
            .map_or(self.workspace.as_path(), PathBuf::as_path)
    }

    /// Where `path` really is, resolved and confined the way every file tool
    /// resolves it: inside the workspace or an added directory, or refused.
    fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let probe = json!({ "path": path }).to_string();
        crate::tools::file_ops::target_path(self.root_for_path(path), &probe)
            .map_err(|e| e.to_string())
    }

    /// Who is asked when the engine wants a person to confirm a call.
    pub fn set_prompter(&mut self, prompter: Arc<dyn crate::hooks::engine::Prompter>) {
        self.prompter = Some(prompter);
    }

    /// The token that stops the turn, so a wait on the engine stops with it.
    pub fn set_cancel(&mut self, token: crate::runtime::cancel::CancelToken) {
        self.cancel = Some(token);
    }

    /// Attach a checkpoint store so that writes and edits snapshot files before modifying them.
    #[must_use]
    pub fn with_checkpoints(
        mut self,
        checkpoints: Arc<Mutex<crate::tools::checkpoint::CheckpointStore>>,
    ) -> Self {
        self.checkpoints = Some(checkpoints);
        self
    }

    /// The same toolbox, one level further down the tree.
    #[must_use]
    pub fn at_depth(mut self, depth: u8) -> Self {
        self.depth = depth;
        self
    }

    /// The conversations subagents opened under this one.
    #[must_use]
    pub fn children(&self) -> Vec<Spawned> {
        self.children.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Hand one piece of work to a fresh agent.
    ///
    /// # Why a subagent is worth the trouble
    ///
    /// A long search or a wide read fills the caller's context with material it
    /// will never need again. Handing that out means only the ANSWER comes back,
    /// and the reading is paid for in a conversation that is then thrown away.
    ///
    /// # The role decides the model
    ///
    /// A subagent that greps and one that works out why a bug happens are not
    /// the same job and should not cost the same. The role is looked up in the
    /// operator's own table, so which model serves which kind of work stays
    /// their decision — see [`crate::roles`].
    fn run_subagent(&self, input: &Value) -> Result<String, String> {
        if self.depth >= MAX_DEPTH {
            return Err(format!(
                "already {} agents deep, which is the limit. Do this work here \
                 rather than handing it on again.",
                self.depth
            ));
        }
        let task = input
            .get("task")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if task.is_empty() {
            return Err(
                "a subagent needs a task: say what it should do and what to report back"
                    .to_string(),
            );
        }
        let Some(mut settings) = crate::agent::api::Settings::from_env() else {
            return Err(
                "no endpoint is configured, so there is nothing to hand work to".to_string(),
            );
        };

        // The operator's routing, not ours. An unknown role leaves the model
        // alone rather than guessing at one — the parent's model is a working
        // answer, and a wrong model is a wasted call.
        let role = input
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let inherited = settings.model.clone();
        if !role.is_empty() {
            if let Some(chosen) = crate::roles::RoleTable::from_env().model_for(role) {
                settings.model = crate::roles::model_name(chosen).to_string();
            }
        }
        // WHICH WORK WENT WHERE. Routing is invisible from outside: a subagent on
        // the wrong model looks exactly like one on the right model that answered
        // badly. So the choice says itself — including when it did NOT route, and
        // why, which is the case that silently costs the most.
        if role.is_empty() {
            crate::trace!("route", "subagent: no role given, inheriting {inherited}");
        } else if settings.model == inherited {
            crate::trace!(
                "route",
                "subagent: role `{role}` is not configured, inheriting {inherited}"
            );
        } else {
            crate::trace!(
                "route",
                "subagent: role `{role}` -> {} (parent was {inherited})",
                settings.model
            );
        }
        crate::trace!(
            "route",
            "subagent spawning at depth {}",
            self.depth.saturating_add(1)
        );

        let mut child = crate::agent::api::ApiBackend::new(
            settings,
            &self.workspace,
            SUBAGENT_SYSTEM,
            self.filter_env,
        )
        .at_depth(self.depth + 1)
        // The same guard, the same person to ask, the same stop: a subagent's
        // write is a write in this workspace.
        .with_engine(self.engine.clone());
        if let Some(cp) = &self.checkpoints {
            child = child.with_checkpoints(cp.clone());
        }
        if let Some(prompter) = &self.prompter {
            child.set_prompter(Arc::clone(prompter));
        }
        if let Some(cancel) = &self.cancel {
            child.set_cancel(cancel.clone());
        }

        let mut said = String::new();
        let outcome = child
            .run_turn(task, &mut |event| {
                if let crate::agent::AgentEvent::Text(text) = event {
                    said.push_str(text);
                }
            })
            .map_err(|e| format!("the subagent could not finish: {e}"))?;

        // Recorded whatever it said, INCLUDING a failure: a subagent that ran
        // and got nowhere is still a conversation that was paid for, and the
        // session record showing it is how that becomes visible.
        if let Ok(mut children) = self.children.lock() {
            children.push(Spawned {
                session: outcome.session_id.clone(),
                title: crate::text::truncate(task, 72),
            });
        }
        if outcome.is_error {
            return Err(outcome.failure());
        }
        if said.trim().is_empty() {
            return Err("the subagent finished without reporting anything".to_string());
        }
        Ok(said)
    }

    /// Hand over one skill's text.
    fn read_skill(&self, input: &Value) -> Result<String, String> {
        let name = input.get("name").and_then(Value::as_str).unwrap_or("");
        // Checked BEFORE the fetch. An unnamed ask cannot match anything, so
        // going to the store for it spends a request — and a four-second
        // timeout — to answer a question already answered here.
        if name.trim().is_empty() {
            return Err("say which skill to read, by the name it is listed under".to_string());
        }
        let (set, _) = crate::skills::load(&crate::admin::AdminConfig::default_dir());
        let Some(skill) = crate::skills::find(&set, name) else {
            // Listing what there IS beats "not found": the name came back
            // through a model and the near miss is usually right there.
            let known: Vec<&str> = set
                .managed
                .iter()
                .chain(set.discovered.iter())
                .map(|s| s.name.as_str())
                .take(20)
                .collect();
            return Err(format!(
                "there is no skill called `{name}`. Skills available: {}",
                known.join(", ")
            ));
        };
        crate::skills::body_of(skill)
    }

    /// Record part of the project's brief.
    fn save_brief(&self, raw: &str) -> Result<String, String> {
        let update = crate::brief::parse_save(raw)?;
        // The workspace FOLDER, which is what the brief is looked up by when
        // no exact path matches — see `brief_for`. A different name here would
        // write a brief that the gate then reports as missing.
        let project = self
            .workspace
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let still_missing = crate::brief::save(&self.workspace, &project, &update)?;
        Ok(crate::brief::saved_report(&still_missing))
    }

    /// Every file a call would change, as the model named them.
    ///
    /// The one place this is worked out, so the guards, the checkpoint and
    /// the engine's code-edit event all see the same list. A patch that does
    /// not parse is an error here, before anything is asked or written.
    fn changed_paths(&self, name: &str, input: &Value) -> Result<Vec<String>, String> {
        match name {
            WRITE | EDIT | MULTI_EDIT | NOTEBOOK_EDIT => Ok(vec![input
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()]),
            APPLY_PATCH => edits::patch_paths(input),
            // The PDF it writes, not the document it reads.
            CONVERT_DOCUMENT => Ok(vec![convert::output_path(input)]),
            _ => Ok(Vec::new()),
        }
    }

    /// Why this call is refused by the operator's guards, if it is.
    fn refused(&self, name: &str, input: &Value) -> Option<String> {
        // Reads and searches change nothing, so the guards have nothing to say
        // about them: they protect against WRITES — every tool that changes a
        // file, per file, and the shell.
        let changes_files = reach_of(name) == Some(Reach::Change);
        if !changes_files && name != BASH {
            return None;
        }
        let guards = match &self.guards {
            Ok(guards) => guards,
            Err(why) => {
                return Some(format!(
                    "the guards could not be read, so nothing may be written until they are \
                     fixed: {why}"
                ))
            }
        };
        if name == BASH {
            let command = input.get("command").and_then(Value::as_str).unwrap_or("");
            return guards.refuse_command(command);
        }
        let paths = match self.changed_paths(name, input) {
            Ok(paths) => paths,
            Err(why) => return Some(why),
        };
        paths.iter().find_map(|path| self.refuse_path(guards, path))
    }

    /// Why a write to `path` is refused by the guards, if it is.
    fn refuse_path(&self, guards: &crate::guards::Guards, path: &str) -> Option<String> {
        let root = self.root_for_path(path);
        if root != self.workspace.as_path() {
            // An added directory: the operator's protected paths apply inside
            // it as they do in the workspace, matched against the path WITHIN
            // it, so a parent directory's name cannot decide. The intake gate
            // does not: it is about this workspace's own specification.
            let inside = Path::new(path)
                .strip_prefix(root)
                .map_or_else(|_| path.to_string(), |p| p.to_string_lossy().into_owned());
            return guards.refuse_write(&inside);
        }
        if let Some(reason) = guards.refuse_write(path) {
            return Some(reason);
        }
        crate::spec::refuse_source_write(&self.workspace, path)
    }

    /// Every tool this session offers, as the model is told about them: the
    /// whole [`catalogue`], less `ConvertDocument` where LibreOffice is not
    /// installed.
    #[must_use]
    pub fn definitions(&self) -> Vec<Value> {
        catalogue()
            .into_iter()
            .filter(|def| {
                def.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| self.offered(name))
            })
            .collect()
    }

    /// The name of every tool offered.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.definitions()
            .iter()
            .filter_map(|d| d.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .collect()
    }
}

/// Every tool there is, as the model is told about them, whether or not this
/// session offers it. Also the source of the schemas every call's input is
/// checked against ([`validate`]).
///
/// The file and shell schemas are written here because their
/// implementations never had any; the rest come from the modules that own
/// them, so a tool's description lives beside the code that honours it.
fn catalogue() -> Vec<Value> {
    let mut all = vec![
        json!({
            "name": READ,
            "description": "Read a file from the workspace. Returns the text with line \
                numbers. Use `offset` and `limit` to take part of a large file rather \
                than all of it. A document — PDF, Word (.docx), PowerPoint (.pptx, with \
                speaker notes), a spreadsheet (.xlsx, .xls, .ods), OpenDocument text (.odt) \
                — comes back as markdown, and `offset`/`limit` page through that. HTML, CSV \
                and TSV come back as their source unless `render` is set, which returns the \
                page as markdown or the table as a markdown table.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File to read, relative to the workspace or absolute."},
                    "offset": {"type": "integer", "minimum": 0, "description": "First line to return, counting from 0."},
                    "limit": {"type": "integer", "minimum": 1, "description": "How many lines to return."},
                    "render": {"type": "boolean", "description": "For HTML, CSV or TSV: return markdown instead of the source."}
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": WRITE,
            "description": "Create a file, or replace one entirely. When the file already \
                exists the reply includes a diff of what changed. Prefer Edit for a small \
                change to a large file.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string", "description": "The complete new contents."}
                },
                "required": ["path", "content"]
            }
        }),
        json!({
            "name": EDIT,
            "description": "Replace an exact string in a file. The old string must appear \
                exactly once, so include enough surrounding text to be unambiguous.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string", "description": "Text to replace, matched exactly."},
                    "new_string": {"type": "string", "description": "What to put in its place."}
                },
                "required": ["path", "old_string", "new_string"]
            }
        }),
    ];
    all.extend(edits::definitions());
    all.push(listing::definition());
    all.extend([
            json!({
                "name": GLOB,
                "description": "Find files whose paths match a glob. A pattern with no `/` \
                    matches file names at any depth (`*.rs`); one with a `/` matches the path \
                    from `path`, where `*` stays in one directory and `**` crosses them \
                    (`src/**/*.ts`). Skips what .gitignore, .ignore and .git/info/exclude \
                    exclude, hidden files, node_modules and build caches — the same files Grep \
                    and LS skip. Lists files only, sorted; long results are cut and counted.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string"},
                        "path": {"type": "string", "description": "Directory to search under. Defaults to the workspace."}
                    },
                    "required": ["pattern"]
                }
            }),
            json!({
                "name": GREP,
                "description": "Search file contents for a regular expression (Rust regex \
                    syntax), line by line. Skips the same files Glob and LS skip. Reports a \
                    binary file that matches without printing it, reads UTF-16 files, and stops \
                    at 100 matching lines — saying so — so narrow with `path` or `glob` when it \
                    does.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Regular expression to search for."},
                        "path": {"type": "string", "description": "Directory or file to search. Defaults to the workspace."},
                        "glob": {"type": "string", "description": "Only search files matching this, e.g. `*.rs` or `src/**/*.ts`."},
                        "context": {"type": "integer", "minimum": 0, "maximum": crate::tools::search::MAX_CONTEXT_LINES, "description": "Lines of context to show before and after each match."},
                        "case_insensitive": {"type": "boolean", "description": "Match regardless of case."}
                    },
                    "required": ["pattern"]
                }
            }),
            outline::definition(),
            convert::definition(),
            json!({
                "name": BASH,
                "description": "Run a shell command in the workspace and return its output. \
                    Output is truncated if very long, and the command is stopped if it runs \
                    past its timeout. Set `run_in_background` for a command that keeps \
                    running (a server, a watcher, a long build): it returns a job id at once, \
                    BashOutput reads what the job has said since, and KillShell stops it.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "timeout": {"type": "integer", "description": "Milliseconds before the command is stopped. Not used for a background job."},
                        "run_in_background": {"type": "boolean", "description": "Start it as a background job and return its id at once."}
                    },
                    "required": ["command"]
                }
            }),
        ]);
    all.extend(shell::definitions());
    all.push(todo::definition());
    all.push(checks::definition());
    all.extend([
            json!({
                "name": SKILL,
                "description": "Read one skill's own text. The session lists what skills exist                     and what each is for, but not their contents — ask for one BY NAME when the                     work matches its description, and follow it only after reading it.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "The skill's name, as listed."}
                    },
                    "required": ["name"]
                }
            }),
            json!({
                "name": AGENT,
                "description": "Hand one self-contained piece of work to a fresh agent and get \
                    back only its answer. Use this when the work needs a lot of reading you will \
                    not need again — a wide search, a long file, tracing a call path. The \
                    subagent has the same tools and the same workspace but NOT this \
                    conversation, so put everything it needs in `task` and say exactly what to \
                    report back. It cannot ask you anything.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "The whole job, self-contained, ending with what to report back."
                        },
                        "role": {
                            "type": "string",
                            "description": "What KIND of work this is, which decides how capable a \
                                model serves it. Omit if unsure and it uses this one's model.",
                            "enum": crate::roles::KNOWN_ROLES
                        }
                    },
                    "required": ["task"]
                }
            }),
            json!({
                "name": ASK_USER_QUESTION,
                "description": "Put a decision that is genuinely the owner's to them as options. \
                    Use it only when the answer changes what you build and you cannot choose \
                    responsibly yourself; routine judgement calls are yours. Each option needs \
                    the one line that makes it decidable. The owner may answer in their app or \
                    at the terminal; the answer comes back as this tool's result. If nobody can \
                    be asked, the result says so and nothing was chosen.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "questions": {
                            "type": "array",
                            "minItems": 1,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "question": {"type": "string", "description": "The question, ending with a question mark."},
                                    "header": {"type": "string", "description": "A very short label for it, a word or two."},
                                    "options": {
                                        "type": "array",
                                        "minItems": 2,
                                        "maxItems": crate::ask::MAX_OPTIONS,
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "label": {"type": "string", "description": "The choice, in a few words."},
                                                "description": {"type": "string", "description": "What choosing it means."}
                                            },
                                            "required": ["label", "description"]
                                        }
                                    },
                                    "multiSelect": {"type": "boolean", "description": "Whether more than one option may be chosen."}
                                },
                                "required": ["question", "header", "options"]
                            }
                        }
                    },
                    "required": ["questions"]
                }
            }),
            crate::verify::tool_definition(),
            crate::recall::index_tool_definition(),
            crate::recall::get_tool_definition(),
            crate::recorder::lookup_tool_definition(),
            // The intake gate TELLS the model to call this. Offering the gate
            // without the tool is an instruction that cannot be followed, and
            // the loop stalls on a brief that can never be saved.
            crate::brief::save_tool_definition(),
        ]);
    all.extend(crate::knowledge::tool_definitions());
    all.extend(crate::web::tool_definitions());
    all.extend(team::definitions());
    all
}

impl Toolbox {
    /// Carry out one call.
    ///
    /// A failure comes back as `Err` and belongs in the model's `tool_result`
    /// marked as an error, NOT as a crash and not as text that reads like
    /// success — a model told a write succeeded when it did not will build on
    /// the file it thinks it wrote.
    pub fn run(&self, name: &str, input: &Value) -> Result<String, String> {
        self.run_call("", name, input)
    }

    /// [`Self::run`], for the call the model numbered `call_id` — the id a
    /// person is shown if the call has to be confirmed.
    pub fn run_call(&self, call_id: &str, name: &str, input: &Value) -> Result<String, String> {
        // A name nobody classified is not a tool: refused before a guard, a
        // person or the engine is asked about it.
        let Some(reach) = reach_of(name) else {
            crate::trace!("tool", "{name} REFUSED: no such tool");
            return Err(format!("there is no tool called `{name}`"));
        };
        crate::trace!(
            "tool",
            "{name} ({}) <- {}",
            reach.as_str(),
            crate::text::truncate(&input.to_string(), 160)
        );
        // A tool this session does not offer is not run, even by a model
        // that knows its name from elsewhere.
        if !self.offered(name) {
            crate::trace!("tool", "{name} REFUSED: not offered in this session");
            return Err(format!(
                "{name} is not available: LibreOffice was not found (install it, or set {} to \
                 its soffice executable)",
                convert::SOFFICE_ENV
            ));
        }
        // The input the tool's own schema promised, before anything is asked
        // or decided about it: a malformed call runs nothing.
        if let Err(problems) = validate::check(name, input) {
            crate::trace!("tool", "{name} REFUSED: input does not match its schema");
            return Err(problems);
        }
        // BEFORE the call, not after. The sentinel that watches the tree can
        // only report a write that already happened; a protected path has to be
        // refused while refusing it still means something.
        if let Some(refusal) = self.refused(name, input) {
            crate::trace!("tool", "{name} REFUSED by guards: {refusal}");
            return Err(refusal);
        }
        // The session's mode, before the engine: it is decided here, costs
        // nothing, and a call it refuses need not cost an engine process.
        if let Err(refusal) = self.mode_allows(call_id, name, input, reach) {
            crate::trace!("tool", "{name} REFUSED by the permission mode: {refusal}");
            return Err(refusal);
        }
        // AFTER the operator's own guards, which are free: a call they refuse
        // never costs an engine process. BEFORE the snapshot and the dispatch,
        // because this is the last point where refusing still prevents.
        let context = match self.engine_gate(call_id, name, input, reach) {
            Gate::Run { context } => context,
            Gate::Refused(why) => {
                crate::trace!("tool", "{name} REFUSED by the engine: {why}");
                return Err(why);
            }
            Gate::Answered(said) => {
                crate::trace!("tool", "{name} answered in the owner's app");
                return Ok(said);
            }
        };
        let outcome = self.dispatch(name, input, reach);
        // A write that LANDED is news to the engine — it files the path under
        // the session and queues the file to be read again. Only a write that
        // happened: a refused or failed one changed nothing. Not waited on.
        if reach == Reach::Change {
            if let (Some(engine), Ok(said)) = (&self.engine, &outcome) {
                self.tell_engine_what_changed(engine, name, input, said);
            }
        }
        // The engine's guidance for THIS action travels with its result, which
        // is the one place the model reads it at the moment it applies.
        match context {
            Some(context) => outcome
                .map(|said| format!("{said}\n\n{context}"))
                .map_err(|why| format!("{why}\n\n{context}")),
            None => outcome,
        }
    }

    /// The engine's code-edit event for a change that landed: once for a
    /// one-file tool, once per file for a patch.
    fn tell_engine_what_changed(
        &self,
        engine: &crate::hooks::engine::Engine,
        name: &str,
        input: &Value,
        said: &str,
    ) {
        let at = crate::hooks::engine::Conversation {
            session: &self.session,
            cwd: &self.workspace,
            transcript: None,
        };
        if name != APPLY_PATCH && name != CONVERT_DOCUMENT {
            engine.code_edit(at, name, &self.event_input(name, input), said);
            return;
        }
        for path in self.changed_paths(name, input).unwrap_or_default() {
            let shown = json!({ "path": self.absolute(&path) });
            engine.code_edit(at, name, &shown, said);
        }
    }

    /// Whether the session's permission mode lets this call run.
    ///
    /// Decided by the call's [`Reach`]: a tool that only observes runs under
    /// every mode; everything else is refused read-only and asked about under
    /// `prompt`. A subagent is refused wherever the mode restricts anything,
    /// because the agent it would start does not carry this mode and could do
    /// what this one may not.
    fn mode_allows(
        &self,
        call_id: &str,
        name: &str,
        input: &Value,
        reach: Reach,
    ) -> Result<(), String> {
        use crate::permissions::PermissionMode;
        let Some(mode) = self.mode else {
            return Ok(());
        };
        let restricted = matches!(mode, PermissionMode::ReadOnly | PermissionMode::Prompt);
        if name == AGENT && restricted {
            return Err(format!(
                "this session is {}, and a subagent would not be held to that. Do the \
                 work here instead.",
                mode.as_str()
            ));
        }
        if reach.only_observes() {
            return Ok(());
        }
        match mode {
            PermissionMode::ReadOnly => Err(format!(
                "NOT RUN — this session is read-only, and {name} changes the workspace or runs \
                 code. Describe the change instead of making it."
            )),
            PermissionMode::Prompt => {
                let shown = self.event_input(name, input);
                let reason = format!("the session asks before every {name}");
                let ask = crate::hooks::engine::Ask {
                    call_id,
                    tool_name: name,
                    tool_input: &shown,
                    reason: &reason,
                };
                let stopped = || {
                    self.cancel
                        .as_ref()
                        .is_some_and(crate::runtime::cancel::CancelToken::is_cancelled)
                };
                match self
                    .prompter
                    .as_ref()
                    .and_then(|p| p.confirm(&ask, &stopped))
                {
                    Some(crate::hooks::engine::Verdict::Allow) => Ok(()),
                    Some(crate::hooks::engine::Verdict::Deny(why)) => Err(why),
                    None => Err(format!(
                        "NOT RUN — this session asks before every {name}, and nobody could be \
                         asked."
                    )),
                }
            }
            PermissionMode::WorkspaceWrite
            | PermissionMode::DangerFullAccess
            | PermissionMode::Allow => Ok(()),
        }
    }

    /// What the memory engine says about this call.
    ///
    /// # The failure policy (the owner's decision)
    ///
    /// When the engine cannot decide — it is missing where it was named, it
    /// crashed, it ran past its bound — a call that changes or runs something
    /// ([`Reach::fails_closed`]) is REFUSED: the guard is a safety function,
    /// and a change made because the guard was down is the one it exists to
    /// stop. A call that only observes runs, traced. The same rule as the
    /// guards file above: an unreadable guard must not read as no guard.
    fn engine_gate(&self, call_id: &str, name: &str, input: &Value, reach: Reach) -> Gate {
        let Some(engine) = &self.engine else {
            return Gate::Run { context: None };
        };
        let shown = self.event_input(name, input);
        let cancel = self.cancel.as_ref();
        match engine.pre_tool(&self.session, &self.workspace, name, &shown, cancel) {
            crate::hooks::engine::PreTool::Allow { context } => Gate::Run { context },
            // Exit 2 on a question is the owner's ANSWER, given in the app.
            crate::hooks::engine::PreTool::Block(said) if name == ASK_USER_QUESTION => {
                Gate::Answered(said)
            }
            crate::hooks::engine::PreTool::Block(reason) => Gate::Refused(reason),
            crate::hooks::engine::PreTool::Ask { reason, context } => {
                let ask = crate::hooks::engine::Ask {
                    call_id,
                    tool_name: name,
                    tool_input: &shown,
                    reason: &reason,
                };
                let prompter = self.prompter.as_deref();
                match engine.settle(&ask, &self.session, &self.workspace, prompter, cancel) {
                    crate::hooks::engine::Verdict::Allow => Gate::Run { context },
                    crate::hooks::engine::Verdict::Deny(why) => Gate::Refused(why),
                }
            }
            crate::hooks::engine::PreTool::Failed(why) if reach.fails_closed() => {
                Gate::Refused(format!(
                    "NOT RUN — the Incord guard could not decide on this {name}, and a change \
                     is not made unguarded: {why}"
                ))
            }
            crate::hooks::engine::PreTool::Failed(why) => {
                crate::trace!("tool", "{name} runs unguarded, the engine failed: {why}");
                Gate::Run { context: None }
            }
        }
    }

    /// `path` as an absolute path: a relative one is named under the
    /// workspace.
    fn absolute(&self, path: &str) -> String {
        if Path::new(path).is_relative() {
            self.workspace.join(path).to_string_lossy().into_owned()
        } else {
            path.to_string()
        }
    }

    /// The call as the engine is shown it: the model's input, with a file
    /// named relative to the workspace made absolute, because the engine keys
    /// its backups and its cards by path. A patch also carries every file it
    /// changes, as `paths`. Everything else is passed unchanged.
    fn event_input(&self, name: &str, input: &Value) -> Value {
        let mut shown = input.clone();
        if matches!(
            name,
            READ | WRITE | EDIT | MULTI_EDIT | NOTEBOOK_EDIT | LS | OUTLINE | CONVERT_DOCUMENT
        ) {
            if let Some(path) = input.get("path").and_then(Value::as_str) {
                shown["path"] = json!(self.absolute(path));
            }
        }
        if name == APPLY_PATCH || name == CONVERT_DOCUMENT {
            if let Ok(paths) = self.changed_paths(name, input) {
                let named: Vec<String> = paths.iter().map(|p| self.absolute(p)).collect();
                shown["paths"] = json!(named);
            }
        }
        shown
    }

    /// An `AskUserQuestion` the owner's app did not answer, answered here.
    ///
    /// By whoever is at the session — the terminal — and otherwise reported
    /// as unanswered, with the options, so the model does not take silence
    /// for a choice.
    fn ask_here(&self, input: &Value) -> Result<String, String> {
        let questions = crate::ask::from_tool_input(input)?;
        let answers = self.prompter.as_ref().and_then(|p| p.choose(&questions));
        if let Some(answers) = answers {
            let pairs: Vec<_> = questions.iter().cloned().zip(answers).collect();
            if let Some(reply) = crate::ask::reply(&pairs) {
                return Ok(reply);
            }
        }
        Ok(crate::ask::unattended_note(&questions)
            .unwrap_or_else(|| "Nobody could be asked, and nothing was chosen.".to_string()))
    }

    /// Snapshot every file a change is about to touch, so `/undo` can put
    /// it back. A path that does not resolve is left to the tool, which
    /// refuses it with the reason.
    fn checkpoint(&self, name: &str, input: &Value) {
        let Some(checkpoints) = &self.checkpoints else {
            return;
        };
        for path in self.changed_paths(name, input).unwrap_or_default() {
            if let Ok(target) = self.resolve(&path) {
                if let Ok(mut store) = checkpoints.lock() {
                    store.snapshot(&target);
                }
            }
        }
    }

    /// Run the tool itself: guards and the engine have already passed it.
    fn dispatch(&self, name: &str, input: &Value, reach: Reach) -> Result<String, String> {
        // The file and shell tools parse their own JSON from a string, which is
        // the interface they were written to; re-serialising here keeps that
        // contract rather than rewriting six working implementations.
        let raw = input.to_string();
        // The workspace, or the added directory the call names a path in.
        let ws = if matches!(
            name,
            READ | WRITE | EDIT | GLOB | GREP | MULTI_EDIT | NOTEBOOK_EDIT | LS | OUTLINE
        ) {
            self.root_for(input)
        } else {
            self.workspace.as_path()
        };

        // Snapshot before file-modifying operations if checkpoints are enabled.
        if reach == Reach::Change {
            self.checkpoint(name, input);
        }

        let background = input.get("run_in_background").and_then(Value::as_bool) == Some(true);
        let began = std::time::Instant::now();
        let outcome = match name {
            READ => crate::tools::file_ops::read_file(ws, &raw).map_err(|e| e.to_string()),
            WRITE => crate::tools::file_ops::write_file(ws, &raw).map_err(|e| e.to_string()),
            EDIT => crate::tools::file_ops::edit_file(ws, &raw).map_err(|e| e.to_string()),
            MULTI_EDIT => edits::multi_edit(ws, input),
            NOTEBOOK_EDIT => edits::notebook_edit(ws, input),
            APPLY_PATCH => edits::apply_patch(input, &|path| self.resolve(path)),
            LS => listing::list(ws, input),
            GLOB => crate::tools::search::glob_search(ws, input),
            GREP => crate::tools::search::grep_search(ws, input),
            OUTLINE => outline::run(ws, input),
            CONVERT_DOCUMENT => match self.soffice() {
                Some(soffice) => {
                    convert::run(soffice, input, self.filter_env, &|path| self.resolve(path))
                }
                None => Err("LibreOffice was not found".to_string()),
            },
            BASH if background => shell::start(&self.state, ws, input),
            BASH => crate::tools::bash::execute_bash(ws, &raw, self.filter_env)
                .map_err(|e| e.to_string()),
            BASH_OUTPUT => shell::output(&self.state, input),
            KILL_SHELL => shell::kill(&self.state, input),
            TODO_WRITE => todo::write(&mut self.state.todos(), input),
            DIAGNOSTICS => checks::diagnostics(ws, input, &|path| self.resolve(path)),
            crate::verify::VERIFY_TOOL => crate::verify::run_tool(ws),
            crate::recall::INDEX_TOOL => crate::recall::run_index_tool(&raw),
            crate::recall::GET_TOOL => crate::recall::run_get_tool(&raw),
            crate::recorder::LOOKUP_TOOL => crate::recorder::run_lookup_tool(ws, input),
            SKILL => self.read_skill(input),
            AGENT => self.run_subagent(input),
            ASK_USER_QUESTION => self.ask_here(input),
            crate::brief::SAVE_TOOL => self.save_brief(&raw),
            other => crate::knowledge::handle_tool(other, input)
                .or_else(|| crate::web::handle_tool(other, input))
                .or_else(|| self.team.handle(other, input))
                .unwrap_or_else(|| Err(format!("there is no tool called `{other}`"))),
        };
        // The OUTCOME, not just the attempt. A tool that ran and failed and a
        // tool that was never reached look identical from the call above.
        match &outcome {
            Ok(said) => crate::trace!(
                "tool",
                "{name} ok in {:?}, {} bytes",
                began.elapsed(),
                said.len()
            ),
            Err(why) => crate::trace!("tool", "{name} FAILED in {:?}: {why}", began.elapsed()),
        }
        outcome
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tool_tests;

#[cfg(test)]
mod reading_tests;
