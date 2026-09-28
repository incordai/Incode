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
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock};

use serde_json::{json, Value};

mod browser;
mod cdp;
mod checks;
mod client_servers;
mod convert;
mod desktop;
mod edits;
mod image;
mod listing;
mod mcp_client;
mod mcp_http;
mod outline;
mod pdf;
mod shell;
mod subagent;
mod team;
mod todo;
mod validate;

pub(crate) use client_servers::{OnConnected, ServerSpec, Transport};
pub use desktop::DesktopSetup;
pub use image::{Image, ImageFormat, ToolOutput, MAX_IMAGES_PER_RESULT, MAX_IMAGE_EDGE};
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
/// A picture of a web page or a workspace HTML file, when the headless
/// browser is installed.
pub const SCREENSHOT: &str = "Screenshot";
/// A web page or a workspace HTML file printed to PDF, when a browser that
/// can print is installed.
pub const RENDER_PDF: &str = "RenderPdf";
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
        // It reads the page it is pointed at, held to WebFetch's address rules.
        SCREENSHOT => Reach::Observe,
        WRITE | EDIT | MULTI_EDIT | APPLY_PATCH | NOTEBOOK_EDIT => Reach::Change,
        // Each writes a PDF into the workspace.
        CONVERT_DOCUMENT | RENDER_PDF => Reach::Change,
        BASH | KILL_SHELL | DIAGNOSTICS | AGENT | crate::verify::VERIFY_TOOL => Reach::Run,
        // The desktop: looking at the screen observes; everything that moves
        // the mouse or types acts on a machine signed into real accounts.
        other if desktop::OBSERVING.contains(&other) => Reach::Observe,
        other if desktop::ACTING.contains(&other) => Reach::Run,
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
    /// The headless browser's `serve` processes, started on first use and
    /// killed, with their trees, when the session's tools are dropped.
    browsers: Mutex<browser::Browsers>,
    /// The desktop child, started on the first desktop call and stopped —
    /// EOF, then its tree — when the session's tools are dropped.
    desktop: Mutex<Option<desktop::DesktopClient>>,
    /// The MCP servers the session's client handed it, each added once it
    /// has connected in the background — stopped, EOF and then their trees,
    /// when the session's tools are dropped. See [`client_servers`].
    client: Arc<RwLock<client_servers::ClientServers>>,
    /// Whether a process these tools start gets the filtered environment.
    filter_env: bool,
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
            browsers: Mutex::new(browser::Browsers::default()),
            desktop: Mutex::new(None),
            client: Arc::default(),
            filter_env,
        })
    }

    /// Start the MCP servers the session's client named, in `cwd`, in the
    /// background, and offer each one's tools through every toolbox sharing
    /// this state as soon as it has listed them. Returns at once, with a line
    /// per server started or refused; what became of each server later is
    /// handed to `on_connected`. Once per session: a second call starts
    /// nothing.
    pub(crate) fn connect_client_servers(
        &self,
        specs: Vec<ServerSpec>,
        cwd: &Path,
        on_connected: &OnConnected,
    ) -> Vec<String> {
        self.connect_client_servers_within(
            specs,
            cwd,
            client_servers::Bounds::DEFAULT,
            on_connected,
        )
    }

    /// [`Self::connect_client_servers`], with each kind of server given
    /// `bounds` to start.
    fn connect_client_servers_within(
        &self,
        specs: Vec<ServerSpec>,
        cwd: &Path,
        bounds: client_servers::Bounds,
        on_connected: &OnConnected,
    ) -> Vec<String> {
        if specs.is_empty() {
            return Vec::new();
        }
        client_servers::ClientServers::connect_in_background(
            &self.client,
            specs,
            cwd,
            self.filter_env,
            bounds,
            on_connected,
        )
    }

    /// The client's servers started and still connecting, by name — for the
    /// model, which is told their tools are not offered yet.
    pub(crate) fn client_servers_connecting(&self) -> Vec<String> {
        self.client().connecting()
    }

    /// The client's servers, as they stand now.
    fn client(&self) -> std::sync::RwLockReadGuard<'_, client_servers::ClientServers> {
        self.client.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `name` is a tool of one of the client's servers.
    fn is_client_tool(&self, name: &str) -> bool {
        name.starts_with(client_servers::PREFIX) && self.client().has(name)
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

/// The most of the engine's reason a decision carries to a watch, in
/// characters. The whole reason is still the call's result.
const MAX_DECISION_REASON_CHARS: usize = 500;

/// Why the operator's guards refused a call: the reason the model is told,
/// and the protected pattern and the path when one matched.
struct Refusal {
    why: String,
    rule: Option<String>,
    path: Option<String>,
}

impl Refusal {
    /// A refusal with no pattern behind it — the guards unreadable, a path
    /// that could not be worked out.
    fn said(why: String) -> Self {
        Self {
            why,
            rule: None,
            path: None,
        }
    }
}

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
    /// The headless browser, looked for once. Screenshot is offered, and
    /// `WebFetch` runs script, only when it was found.
    browser: OnceLock<Option<browser::Found>>,
    /// What `RenderPdf` prints with, chosen once; not offered when `None`.
    printer: OnceLock<Option<pdf::Printer>>,
    /// Where the desktop is started from, when the operator switched it on.
    /// The desktop tools are offered only then.
    desktop: Option<DesktopSetup>,
    /// Who is told what a call does while it runs — its output, what the
    /// gate decided, the workers it starts. See [`crate::watch`].
    watch: Option<Arc<dyn crate::watch::Watch>>,
    /// The agent CLI this toolbox's model runs in, when it runs in one: the
    /// `Agent` tool starts its sub-agent there when no model endpoint is
    /// configured. See [`subagent`].
    cli_agent: Option<String>,
    /// The only tools this toolbox offers and runs, when a sub-agent's
    /// definition named them ([`crate::subagents`]). `None` is every tool.
    allowed: Option<Vec<String>>,
    /// The operator's own pre- and post-tool commands ([`crate::hooks`]),
    /// run around every call. `None` when none are configured, and in every
    /// toolbox a test builds.
    hooks: Option<crate::hooks::HookRunner>,
    /// How tightly the OS confines the shell commands the model runs
    /// ([`crate::sandbox`]). `Off` by default, and in every toolbox a test
    /// builds; set for real work from the session's mode and the settings. The
    /// writable roots are the workspace and the extra directories, taken when a
    /// command runs so a directory added mid-session is covered.
    sandbox: crate::sandbox::SandboxPolicy,
    /// What the sandbox holds for this session — on Windows workspace-write,
    /// the ACE granted on each root — released when the toolbox drops.
    sandbox_grant: crate::sandbox::SessionGrant,
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
            browser: OnceLock::new(),
            printer: OnceLock::new(),
            desktop: None,
            watch: None,
            cli_agent: None,
            allowed: None,
            hooks: None,
            sandbox: crate::sandbox::SandboxPolicy::OFF,
            sandbox_grant: crate::sandbox::SessionGrant::default(),
        }
    }

    /// Confine the model's shell commands to `policy` from now on — see
    /// [`crate::sandbox`]. Opt-in, and given by the code that builds a backend
    /// for real work, so a toolbox built for a test runs commands unconfined.
    #[must_use]
    pub fn with_sandbox(mut self, policy: crate::sandbox::SandboxPolicy) -> Self {
        self.sandbox = policy;
        self
    }

    /// The sandbox for a shell command now: the policy, over the workspace and
    /// every added directory (plus the temp dir, added by the sandbox itself),
    /// with whatever the session must hold for it taken first.
    ///
    /// # Errors
    ///
    /// When the sandbox the operator named could not be made ready (a Windows
    /// workspace-write grant refused or failed). The command must not run.
    fn sandbox(&self) -> Result<crate::sandbox::Sandbox, String> {
        if self.sandbox == crate::sandbox::SandboxPolicy::OFF {
            return Ok(crate::sandbox::Sandbox::off());
        }
        let mut roots = vec![self.workspace.clone()];
        roots.extend(self.extra_dirs.iter().cloned());
        crate::sandbox::Sandbox::new(self.sandbox, roots)
            .prepared(&self.sandbox_grant)
            .map_err(|e| {
                format!("the shell sandbox could not be set up, so the command was not run: {e}")
            })
    }

    /// Run the operator's own commands around every call from now on — see
    /// [`crate::hooks`]. Opt-in, like the engine, and given by the code that
    /// builds a backend for real work.
    #[must_use]
    pub fn with_operator_hooks(mut self, hooks: Option<crate::hooks::HookRunner>) -> Self {
        self.hooks = hooks;
        self
    }

    /// Offer and run only `names` from now on — narrowed further when a list
    /// is already in force, never widened. A call to any other tool is refused
    /// by [`Self::run_call`]'s gate before anything else is asked.
    pub fn restrict_tools(&mut self, names: Vec<String>) {
        self.allowed = Some(match self.allowed.take() {
            Some(already) => names.into_iter().filter(|n| already.contains(n)).collect(),
            None => names,
        });
    }

    /// Whether this toolbox may run `name` at all.
    fn allows(&self, name: &str) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|names| names.iter().any(|n| n == name))
    }

    /// Tell `watch` what calls do while they run, from now on.
    pub fn set_watch(&mut self, watch: Arc<dyn crate::watch::Watch>) {
        self.watch = Some(watch);
    }

    /// The agent CLI (`binary`) this toolbox's model runs in: the `Agent`
    /// tool starts its sub-agent on it when there is no model endpoint.
    pub fn run_subagents_on(&mut self, binary: &str) {
        self.cli_agent = Some(binary.to_string());
    }

    /// Tell the watch, if there is one, what was decided about `call_id`.
    fn tell_decided(&self, call_id: &str, decision: crate::watch::Decision) {
        if let Some(watch) = &self.watch {
            watch.decided(call_id, &decision);
        }
    }

    /// Whether the stop was asked for.
    fn stopped(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(crate::runtime::cancel::CancelToken::is_cancelled)
    }

    /// LibreOffice, when it is installed where [`convert::find_soffice`]
    /// looks.
    fn soffice(&self) -> Option<&Path> {
        self.soffice
            .get_or_init(|| convert::find_soffice(&|name| std::env::var_os(name)))
            .as_deref()
    }

    /// The headless browser, when it is named or installed where
    /// [`browser::find`] looks.
    fn browser(&self) -> Option<&Path> {
        self.found_browser().map(|f| f.path.as_path())
    }

    /// The headless browser and the name a result calls it by.
    fn found_browser(&self) -> Option<&browser::Found> {
        self.browser.get_or_init(Self::detect_browser).as_ref()
    }

    /// [`browser::find`] over this process's environment, `admin.json`'s
    /// `browser_bin`, and the installed tool that provides `browser`.
    fn detect_browser() -> Option<browser::Found> {
        let data_dir = crate::admin::AdminConfig::default_dir();
        let admin = crate::admin::AdminConfig::load_default();
        let provided = crate::ext_tools::provider("browser", &admin.tools, &data_dir);
        browser::find(
            &|name| std::env::var_os(name),
            admin.browser_bin.as_deref(),
            provided,
        )
    }

    /// What `RenderPdf` prints with, per `admin.json` `pdf_renderer`.
    fn printer(&self) -> Option<&pdf::Printer> {
        self.printer
            .get_or_init(|| {
                let setting = crate::admin::AdminConfig::load_default()
                    .pdf_renderer
                    .unwrap_or_default();
                pdf::choose(
                    setting,
                    pdf::find_chromium(&|name| std::env::var_os(name)),
                    self.found_browser().cloned(),
                )
            })
            .as_ref()
    }

    /// Whether this session offers `name`. Every tool is offered except the
    /// ones whose program is not installed — `ConvertDocument` without
    /// LibreOffice, Screenshot without the headless browser, `RenderPdf`
    /// without a browser that can print — and the desktop's, unless the
    /// operator switched the desktop on.
    fn offered(&self, name: &str) -> bool {
        match name {
            CONVERT_DOCUMENT => self.soffice().is_some(),
            SCREENSHOT => self.browser().is_some(),
            RENDER_PDF => self.printer().is_some(),
            other if desktop::is_desktop(other) => self.desktop.is_some(),
            _ => true,
        }
    }

    /// Why `name` is not offered here, for a model that called it anyway.
    fn unavailable(name: &str) -> String {
        match name {
            CONVERT_DOCUMENT => format!(
                "{name} is not available: LibreOffice was not found (install it, or set {} to \
                 its soffice executable)",
                convert::SOFFICE_ENV
            ),
            SCREENSHOT => format!(
                "{name} is not available: no headless browser is installed. Add an entry to \
                 `tools` in admin.json with `\"provides\": \"browser\"` and install it with \
                 `/tools add <name>` (or `incode --install-tools`); for development, {} or \
                 `browser_bin` names one directly",
                browser::BROWSER_ENV
            ),
            RENDER_PDF => format!(
                "{name} is not available: no browser that can print was found (set {}, or \
                 install the headless browser)",
                pdf::PDF_BROWSER_ENV
            ),
            other => {
                format!("{other} is not available: the desktop is not switched on for this session")
            }
        }
    }

    /// Offer the desktop tools, run by the desktop child started from
    /// `setup` — see [`desktop`] for why it is a child.
    pub fn enable_desktop(&mut self, setup: DesktopSetup) {
        self.desktop = Some(setup);
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

    /// This toolbox with the headless browser at `browser`, or none —
    /// whatever this machine has installed — and no system browser to print
    /// with, so `RenderPdf` prints with `browser` when there is one. The
    /// browser is called by its file stem, as a named executable is.
    #[cfg(test)]
    pub(crate) fn with_browser(self, browser: Option<PathBuf>) -> Self {
        let found = browser.clone().map(browser::Found::by_stem);
        let printer = pdf::choose(crate::admin::PdfRenderer::Auto, None, found);
        self.with_printer(browser, printer)
    }

    /// This toolbox with the headless browser at `browser`, or none, and
    /// `RenderPdf` printing with `printer`, or not offered.
    #[cfg(test)]
    fn with_printer(self, browser: Option<PathBuf>, printer: Option<pdf::Printer>) -> Self {
        Self {
            browser: OnceLock::from(browser.map(browser::Found::by_stem)),
            printer: OnceLock::from(printer),
            ..self
        }
    }

    /// This toolbox with a desktop that answers `screenshot` with `picture`
    /// (a PNG or JPEG) and counts in `calls` the calls that reach it — for a
    /// test elsewhere in the crate to send a picture through.
    #[cfg(test)]
    pub(crate) fn with_desktop_picture(
        self,
        picture: Vec<u8>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        self.with_desktop_client(picture_tests::fake_desktop(picture, calls))
    }

    /// The desktop, served by `client` rather than a child this toolbox
    /// starts.
    #[cfg(test)]
    fn with_desktop_client(mut self, client: desktop::DesktopClient) -> Self {
        self.desktop = Some(DesktopSetup {
            exe: PathBuf::from("not-started-in-a-test"),
            journal: self.workspace.join(".incode").join("desktop.jsonl"),
        });
        *self
            .state
            .desktop
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(client);
        self
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
        let mut update = crate::brief::parse_save(raw)?;
        // A project with no code builds its layout first, whoever wrote the
        // plan — see `crate::scaffold`. Said in the result, so the model knows
        // the roadmap it saved is not exactly the one it sent.
        let scaffolded = crate::scaffold::applies(&self.workspace)
            && update
                .roadmap
                .as_mut()
                .is_some_and(crate::scaffold::put_first);
        // The workspace FOLDER, which is what the brief is looked up by when
        // no exact path matches — see `brief_for`. A different name here would
        // write a brief that the gate then reports as missing.
        let project = self
            .workspace
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let still_missing = crate::brief::save(&self.workspace, &project, &update)?;
        let report = crate::brief::saved_report(&still_missing);
        if scaffolded {
            return Ok(format!(
                "{report}\n\nThis project has no code yet, so incode put `{} {}` first in \
                 the roadmap. Give it a check in {}: {}",
                crate::scaffold::ID,
                crate::scaffold::TITLE,
                crate::goal::VERIFICATION_PATH,
                crate::scaffold::check_line()
            ));
        }
        Ok(report)
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
            // The PDF it writes, not the page it prints.
            RENDER_PDF => Ok(vec![pdf::output_path(input)]),
            _ => Ok(Vec::new()),
        }
    }

    /// Whether the operator's guards have anything to say about `name`.
    ///
    /// Reads and searches change nothing, so the guards have nothing to say
    /// about them: they protect against WRITES — every tool that changes a
    /// file, per file, and the shell.
    fn guarded(name: &str) -> bool {
        reach_of(name) == Some(Reach::Change) || name == BASH
    }

    /// Why this call is refused by the operator's guards, if it is.
    fn refused(&self, name: &str, input: &Value) -> Option<Refusal> {
        if !Self::guarded(name) {
            return None;
        }
        let guards = match &self.guards {
            Ok(guards) => guards,
            Err(why) => {
                return Some(Refusal::said(format!(
                    "the guards could not be read, so nothing may be written until they are \
                     fixed: {why}"
                )))
            }
        };
        if name == BASH {
            let command = input.get("command").and_then(Value::as_str).unwrap_or("");
            return guards.refuse_command(command).map(|why| Refusal {
                why,
                rule: guards.protecting_in_command(command),
                path: None,
            });
        }
        let paths = match self.changed_paths(name, input) {
            Ok(paths) => paths,
            Err(why) => return Some(Refusal::said(why)),
        };
        paths.iter().find_map(|path| self.refuse_path(guards, path))
    }

    /// Why a write to `path` is refused by the guards, if it is.
    fn refuse_path(&self, guards: &crate::guards::Guards, path: &str) -> Option<Refusal> {
        let root = self.root_for_path(path);
        let named = Some(path.to_string());
        if root != self.workspace.as_path() {
            // An added directory: the operator's protected paths apply inside
            // it as they do in the workspace, matched against the path WITHIN
            // it, so a parent directory's name cannot decide. The intake gate
            // does not: it is about this workspace's own specification.
            let inside = Path::new(path)
                .strip_prefix(root)
                .map_or_else(|_| path.to_string(), |p| p.to_string_lossy().into_owned());
            return guards.refuse_write(&inside).map(|why| Refusal {
                why,
                rule: guards.protecting(&inside),
                path: named,
            });
        }
        if let Some(why) = guards.refuse_write(path) {
            return Some(Refusal {
                why,
                rule: guards.protecting(path),
                path: named,
            });
        }
        crate::spec::refuse_source_write(&self.workspace, path).map(|why| Refusal {
            why,
            rule: None,
            path: named,
        })
    }

    /// Every tool this session offers, as the model is told about them: the
    /// whole [`catalogue`], less what [`Self::offered`] leaves out — and
    /// `WebFetch` without its `render_js` option where no headless browser
    /// is installed, so it reads exactly as it did before there was one.
    #[must_use]
    pub fn definitions(&self) -> Vec<Value> {
        let browser = self.browser().is_some();
        let mut all: Vec<Value> = catalogue()
            .into_iter()
            .filter(|def| {
                def.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| self.offered(name) && self.allows(name))
            })
            .map(|mut def| {
                // The sub-agents defined for this workspace, named where the
                // model decides whether to delegate — read now, so a file
                // added mid-session is offered from the next turn.
                if def.get("name").and_then(Value::as_str) == Some(AGENT) {
                    if let Some(note) = crate::subagents::load(&self.workspace).schema_note() {
                        if let Some(text) = def.get("description").and_then(Value::as_str) {
                            def["description"] = json!(format!("{text}\n\n{note}"));
                        }
                    }
                }
                // The shell the command will actually run in, named, so the
                // model writes that shell's syntax: chosen once per process,
                // and PowerShell where the Windows sandbox cannot start bash.
                if def.get("name").and_then(Value::as_str) == Some(BASH) {
                    let sandbox = crate::sandbox::Sandbox::new(self.sandbox, Vec::new());
                    let shell = crate::tools::shell_choice::describe(&sandbox);
                    if let Some(text) = def.get("description").and_then(Value::as_str) {
                        def["description"] = json!(format!("{text} {shell}"));
                    }
                }
                if !browser
                    && def.get("name").and_then(Value::as_str) == Some(crate::web::FETCH_TOOL)
                {
                    if let Some(properties) = def
                        .pointer_mut("/input_schema/properties")
                        .and_then(Value::as_object_mut)
                    {
                        properties.remove("render_js");
                    }
                }
                def
            })
            .collect();
        // The tools of the MCP servers the session's client handed it, run
        // through the same gate as the rest — read now, so a server that
        // finished connecting since the last list is offered.
        let client = self.state.client().definitions();
        all.extend(client.into_iter().filter(|def| {
            def.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| self.allows(name))
        }));
        all
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
                        },
                        "agent": {
                            "type": "string",
                            "description": "The name of an agent defined for this workspace, when \
                                the list below names one that fits: it works with that \
                                definition's instructions, role, model and tools."
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
    all.push(browser::definition());
    all.push(pdf::definition());
    all.extend(team::definitions());
    all.extend(desktop::definitions());
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
        self.run_call("", name, input).map(|out| out.shown())
    }

    /// [`Self::run`], for the call the model numbered `call_id` — the id a
    /// person is shown if the call has to be confirmed — with the answer's
    /// pictures kept, for a caller that can send them on.
    pub fn run_call(&self, call_id: &str, name: &str, input: &Value) -> Result<ToolOutput, String> {
        // A name nobody classified is not a tool: refused before a guard, a
        // person or the engine is asked about it. A tool of the client's MCP
        // servers RUNS something nobody here can inspect, so it is `run`:
        // refused read-only, asked under prompt, refused when the engine
        // cannot decide.
        let client_tool = self.state.is_client_tool(name);
        let reach = if client_tool {
            Some(Reach::Run)
        } else {
            reach_of(name)
        };
        let Some(reach) = reach else {
            crate::trace!("tool", "{name} REFUSED: no such tool");
            return Err(format!("there is no tool called `{name}`"));
        };
        crate::trace!(
            "tool",
            "{name} ({}) <- {}",
            reach.as_str(),
            crate::text::truncate(&input.to_string(), 160)
        );
        // A sub-agent's definition named the tools it may use: any other is
        // refused here, before its input is looked at or anyone is asked.
        if !self.allows(name) {
            crate::trace!("tool", "{name} REFUSED: not in this agent's tools");
            return Err(format!(
                "{name} is not one of the tools this agent may use: {}",
                self.allowed.as_deref().unwrap_or_default().join(", ")
            ));
        }
        // A tool this session does not offer is not run, even by a model
        // that knows its name from elsewhere.
        if !self.offered(name) {
            crate::trace!("tool", "{name} REFUSED: not offered in this session");
            return Err(Self::unavailable(name));
        }
        // The input the tool's own schema promised, before anything is asked
        // or decided about it: a malformed call runs nothing.
        let checked = if client_tool {
            self.state.client().check(name, input)
        } else {
            validate::check(name, input)
        };
        if let Err(problems) = checked {
            crate::trace!("tool", "{name} REFUSED: input does not match its schema");
            return Err(problems);
        }
        // BEFORE the call, not after. The sentinel that watches the tree can
        // only report a write that already happened; a protected path has to be
        // refused while refusing it still means something.
        if let Some(refusal) = self.refused(name, input) {
            crate::trace!("tool", "{name} REFUSED by guards: {}", refusal.why);
            self.tell_decided(
                call_id,
                crate::watch::Decision {
                    by: crate::watch::Decider::Guards,
                    allowed: false,
                    rule: refusal.rule,
                    path: refusal.path,
                    reason: None,
                },
            );
            return Err(refusal.why);
        }
        // The session's mode, before the engine: it is decided here, costs
        // nothing, and a call it refuses need not cost an engine process.
        if let Err(refusal) = self.mode_allows(call_id, name, input, reach) {
            crate::trace!("tool", "{name} REFUSED by the permission mode: {refusal}");
            return Err(refusal);
        }
        // The operator's own pre-tool commands: after the free checks, before
        // the engine, so a call they deny never costs an engine process.
        let said_before = match self.operator_hook_before(call_id, name, input) {
            Ok(said) => said,
            Err(refusal) => {
                crate::trace!("tool", "{name} REFUSED by the operator's hook: {refusal}");
                return Err(refusal);
            }
        };
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
                return Ok(ToolOutput::from(said));
            }
        };
        // With no engine, the guards had the last word on a call they look
        // at, and it was yes.
        if self.engine.is_none() && Self::guarded(name) && self.guards.is_ok() {
            self.tell_decided(
                call_id,
                crate::watch::Decision {
                    by: crate::watch::Decider::Guards,
                    allowed: true,
                    rule: None,
                    path: None,
                    reason: None,
                },
            );
        }
        let outcome = self.dispatch(call_id, name, input, reach);
        // A write that LANDED is news to the engine — it files the path under
        // the session and queues the file to be read again. Only a write that
        // happened: a refused or failed one changed nothing. Not waited on.
        if reach == Reach::Change {
            if let (Some(engine), Ok(said)) = (&self.engine, &outcome) {
                self.tell_engine_what_changed(engine, name, input, &said.shown());
            }
        }
        // The operator's post-tool commands, told what came back; what they
        // and the pre-tool commands printed travels with the result.
        let mut said_hooks = said_before;
        said_hooks.extend(self.operator_hook_after(name, input, &outcome));
        let context = match (context, said_hooks.is_empty()) {
            (context, true) => context,
            (context, false) => {
                let hooks = said_hooks
                    .iter()
                    .map(|line| format!("[operator hook] {line}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(match context {
                    Some(context) => format!("{context}\n\n{hooks}"),
                    None => hooks,
                })
            }
        };
        // The engine's guidance for THIS action travels with its result, which
        // is the one place the model reads it at the moment it applies.
        match context {
            Some(context) => outcome
                .map(|mut said| {
                    said.text = format!("{}\n\n{context}", said.text);
                    said
                })
                .map_err(|why| format!("{why}\n\n{context}")),
            None => outcome,
        }
    }

    /// The call as the operator's hooks are told it.
    fn hook_call<'a>(&'a self, name: &'a str, shown: &'a Value) -> crate::hooks::HookCall<'a> {
        crate::hooks::HookCall {
            tool_name: name,
            tool_input: shown,
            session: &self.session,
            cwd: Some(&self.workspace),
        }
    }

    /// The operator's pre-tool commands for this call: what they printed, or
    /// the refusal when one exited 2. The desktop's tools are left to their
    /// executor, which runs the same commands itself.
    fn operator_hook_before(
        &self,
        call_id: &str,
        name: &str,
        input: &Value,
    ) -> Result<Vec<String>, String> {
        let Some(hooks) = &self.hooks else {
            return Ok(Vec::new());
        };
        if desktop::is_desktop(name) {
            return Ok(Vec::new());
        }
        let shown = self.event_input(name, input);
        let ran = hooks.pre_tool(&self.hook_call(name, &shown));
        if !ran.is_denied() {
            return Ok(ran.messages().to_vec());
        }
        let reason = ran.messages().join("; ");
        self.tell_decided(
            call_id,
            crate::watch::Decision {
                by: crate::watch::Decider::Hook,
                allowed: false,
                rule: None,
                path: None,
                reason: Some(crate::text::truncate(&reason, MAX_DECISION_REASON_CHARS)),
            },
        );
        Err(format!(
            "NOT RUN — refused by the operator's hook: {reason}"
        ))
    }

    /// The operator's post-tool commands for a call that returned `outcome`:
    /// what they printed. They cannot refuse — the call already happened.
    fn operator_hook_after(
        &self,
        name: &str,
        input: &Value,
        outcome: &Result<ToolOutput, String>,
    ) -> Vec<String> {
        let Some(hooks) = &self.hooks else {
            return Vec::new();
        };
        if desktop::is_desktop(name) {
            return Vec::new();
        }
        let shown = self.event_input(name, input);
        let (said, is_error) = match outcome {
            Ok(out) => (out.shown(), false),
            Err(why) => (why.clone(), true),
        };
        hooks
            .post_tool(&self.hook_call(name, &shown), &said, is_error)
            .messages()
            .to_vec()
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
        if !matches!(name, APPLY_PATCH | CONVERT_DOCUMENT | RENDER_PDF) {
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
        let gate = self.engine_gate_inner(call_id, name, input, reach);
        if self.engine.is_some() {
            let (allowed, reason) = match &gate {
                Gate::Run { .. } => (true, None),
                Gate::Refused(why) => (false, Some(why.clone())),
                // The owner answered a question in the app: nothing was
                // refused, and the answer is the result.
                Gate::Answered(_) => (true, None),
            };
            self.tell_decided(
                call_id,
                crate::watch::Decision {
                    by: crate::watch::Decider::Engine,
                    allowed,
                    rule: None,
                    path: None,
                    reason: reason
                        .map(|why| crate::text::truncate(&why, MAX_DECISION_REASON_CHARS)),
                },
            );
        }
        gate
    }

    /// [`Self::engine_gate`]'s decision, before it is told.
    fn engine_gate_inner(&self, call_id: &str, name: &str, input: &Value, reach: Reach) -> Gate {
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
        if matches!(name, APPLY_PATCH | CONVERT_DOCUMENT | RENDER_PDF) {
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
    fn dispatch(
        &self,
        call_id: &str,
        name: &str,
        input: &Value,
        reach: Reach,
    ) -> Result<ToolOutput, String> {
        if reach == Reach::Change {
            self.checkpoint(name, input);
        }
        let began = std::time::Instant::now();
        let resolve = |path: &str| self.resolve(path);
        let outcome = match name {
            SCREENSHOT => match self.browser() {
                Some(bin) => {
                    browser::screenshot(&self.state.browsers, bin, input, &resolve, self.filter_env)
                }
                None => Err(Self::unavailable(name)),
            },
            RENDER_PDF => match self.printer() {
                Some(printer) => pdf::run(
                    printer,
                    &self.state.browsers,
                    input,
                    &resolve,
                    self.filter_env,
                )
                .map(ToolOutput::from),
                None => Err(Self::unavailable(name)),
            },
            other if desktop::is_desktop(other) => self.run_desktop(other, input),
            other if self.state.is_client_tool(other) => self.run_client_tool(other, input),
            other => self
                .dispatch_text(call_id, other, input)
                .map(ToolOutput::from),
        };
        // The OUTCOME, not just the attempt. A tool that ran and failed and a
        // tool that was never reached look identical from the call above.
        match &outcome {
            Ok(said) => crate::trace!(
                "tool",
                "{name} ok in {:?}, {} bytes, {} picture(s)",
                began.elapsed(),
                said.text.len(),
                said.images.len()
            ),
            Err(why) => crate::trace!("tool", "{name} FAILED in {:?}: {why}", began.elapsed()),
        }
        outcome
    }

    /// One desktop tool, run by the session's desktop child — started now
    /// when there is none, or when the one running was started for a
    /// different mode or has failed.
    fn run_desktop(&self, name: &str, input: &Value) -> Result<ToolOutput, String> {
        let Some(setup) = &self.desktop else {
            return Err(Self::unavailable(name));
        };
        let read_only = self.mode == Some(crate::permissions::PermissionMode::ReadOnly);
        let mut slot = self
            .state
            .desktop
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !slot.as_ref().is_some_and(|c| c.serves(read_only)) {
            // Dropped FIRST: its kill switch ends it before another starts.
            *slot = None;
            *slot = Some(desktop::DesktopClient::start(setup, read_only)?);
        }
        match slot.as_mut() {
            Some(client) => client.call(name, input),
            None => Err("the desktop could not be started".to_string()),
        }
    }

    /// One tool of the client's MCP servers, run by its server — and given up
    /// on at once when the turn is stopped.
    fn run_client_tool(&self, name: &str, input: &Value) -> Result<ToolOutput, String> {
        let stopped = || {
            self.cancel
                .as_ref()
                .is_some_and(crate::runtime::cancel::CancelToken::is_cancelled)
        };
        client_servers::call(&self.state.client, name, input, &stopped)
    }

    /// A foreground shell command: its output shown as it prints when there
    /// is a watch, and the command stopped — with everything it started — as
    /// soon as the turn is.
    fn run_shell(&self, call_id: &str, root: &Path, raw: &str) -> Result<String, String> {
        let stopped = || self.stopped();
        let sandbox = self.sandbox()?;
        let ran = match &self.watch {
            Some(watch) => {
                let mut live = crate::watch::LiveOutput::new(watch.as_ref(), call_id);
                crate::tools::bash::execute_bash_watched(
                    root,
                    raw,
                    self.filter_env,
                    &sandbox,
                    &stopped,
                    &mut |piece| live.push(piece),
                )
            }
            None => crate::tools::bash::execute_bash_watched(
                root,
                raw,
                self.filter_env,
                &sandbox,
                &stopped,
                &mut |_| {},
            ),
        };
        ran.map_err(|e| e.to_string())
    }

    /// The tools whose answer is text alone.
    fn dispatch_text(&self, call_id: &str, name: &str, input: &Value) -> Result<String, String> {
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

        let background = input.get("run_in_background").and_then(Value::as_bool) == Some(true);
        // The page's script, run by the headless browser, for a WebFetch
        // that needs it — when the browser is installed.
        let render = |url: &str| match self.browser() {
            Some(bin) => browser::render_html(bin, url, self.filter_env),
            None => Err("no headless browser is installed".to_string()),
        };
        let render: Option<crate::web::Render<'_>> = self.browser().is_some().then_some(&render);
        match name {
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
            BASH if background => shell::start(&self.state, ws, input, &self.sandbox()?),
            BASH => self.run_shell(call_id, ws, &raw),
            BASH_OUTPUT => shell::output(&self.state, input),
            KILL_SHELL => shell::kill(&self.state, input),
            TODO_WRITE => todo::write(&mut self.state.todos(), input),
            DIAGNOSTICS => checks::diagnostics(ws, input, &|path| self.resolve(path)),
            crate::verify::VERIFY_TOOL => crate::verify::run_tool(ws),
            crate::recall::INDEX_TOOL => crate::recall::run_index_tool(&raw),
            crate::recall::GET_TOOL => crate::recall::run_get_tool(&raw),
            crate::recorder::LOOKUP_TOOL => {
                crate::recorder::run_lookup_tool(ws, input).map(|found| {
                    if let Some(watch) = &self.watch {
                        watch.recorded(call_id, found.hit, found.observations);
                    }
                    found.text
                })
            }
            SKILL => self.read_skill(input),
            AGENT => self.run_subagent(input),
            ASK_USER_QUESTION => self.ask_here(input),
            crate::brief::SAVE_TOOL => self.save_brief(&raw),
            other => crate::knowledge::handle_tool(other, input)
                .or_else(|| crate::web::handle_tool(other, input, render))
                .or_else(|| self.team.handle(other, input))
                .unwrap_or_else(|| Err(format!("there is no tool called `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tool_tests;

#[cfg(test)]
mod reading_tests;

#[cfg(test)]
mod picture_tests;

#[cfg(test)]
mod client_tool_tests;

#[cfg(test)]
mod hook_tests;

#[cfg(test)]
pub(crate) use client_servers::tests::{
    calls_in as client_server_calls, fake_server, slow_fake_server,
};

#[cfg(test)]
pub(crate) use mcp_http::tests::FakeHttp;
