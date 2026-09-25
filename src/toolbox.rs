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

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::agent::AgentBackend;

/// Read a file's contents.
pub const READ: &str = "Read";
/// Create or overwrite a file.
pub const WRITE: &str = "Write";
/// Replace a string inside a file.
pub const EDIT: &str = "Edit";
/// Find files by name pattern.
pub const GLOB: &str = "Glob";
/// Search file contents.
pub const GREP: &str = "Grep";
/// Run a shell command.
pub const BASH: &str = "Bash";
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
    children: std::sync::Mutex<Vec<Spawned>>,
    checkpoints:
        Option<std::sync::Arc<std::sync::Mutex<crate::tools::checkpoint::CheckpointStore>>>,
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
    prompter: Option<std::sync::Arc<dyn crate::hooks::engine::Prompter>>,
    /// Tripped when the operator stops the turn: a wait on the engine or on a
    /// person ends then rather than at its ten-minute bound.
    cancel: Option<crate::runtime::cancel::CancelToken>,
    /// How much the session lets a call do, when the session said. `None`
    /// leaves every call to the guards and the engine alone — the route whose
    /// caller has no mode to give.
    mode: Option<crate::permissions::PermissionMode>,
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
            children: std::sync::Mutex::new(Vec::new()),
            checkpoints: None,
            engine: None,
            session: String::new(),
            session_pinned: false,
            extra_dirs: Vec::new(),
            prompter: None,
            cancel: None,
            mode: None,
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

    /// Directories beyond the workspace where the file tools may act. The
    /// operator's guards apply inside them as in the workspace.
    pub fn set_extra_dirs(&mut self, dirs: Vec<PathBuf>) {
        self.extra_dirs = dirs;
    }

    /// The root a file tool call acts under: an extra directory when the call
    /// names an absolute path inside one, otherwise the workspace. Containment
    /// is decided by the same check the file tools apply, so a path this
    /// picks a root for is a path the tool will accept there.
    fn root_for(&self, input: &Value) -> &Path {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return &self.workspace;
        };
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

    /// Who is asked when the engine wants a person to confirm a call.
    pub fn set_prompter(&mut self, prompter: std::sync::Arc<dyn crate::hooks::engine::Prompter>) {
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
        checkpoints: std::sync::Arc<std::sync::Mutex<crate::tools::checkpoint::CheckpointStore>>,
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
            child.set_prompter(std::sync::Arc::clone(prompter));
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

    /// Why this call is refused by the operator's guards, if it is.
    fn refused(&self, name: &str, input: &Value) -> Option<String> {
        // Reads and searches change nothing, so the guards have nothing to say
        // about them: they protect against WRITES.
        if !matches!(name, WRITE | EDIT | BASH) {
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
        let path = input.get("path").and_then(Value::as_str).unwrap_or("");
        let root = self.root_for(input);
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

    /// Every tool, as the model is told about them.
    ///
    /// The file and shell schemas are written here because their
    /// implementations never had any; the rest come from the modules that own
    /// them, so a tool's description lives beside the code that honours it.
    #[must_use]
    pub fn definitions(&self) -> Vec<Value> {
        let mut all = vec![
            json!({
                "name": READ,
                "description": "Read a file from the workspace. Returns the text with line \
                    numbers. Use `offset` and `limit` to take part of a large file rather \
                    than all of it.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File to read, relative to the workspace or absolute."},
                        "offset": {"type": "integer", "description": "First line to return, counting from 0."},
                        "limit": {"type": "integer", "description": "How many lines to return."}
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
            json!({
                "name": GLOB,
                "description": "Find files whose paths match a pattern, such as `**/*.rs`.",
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
                "description": "Search file contents for a regular expression.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Regular expression to search for."},
                        "path": {"type": "string", "description": "Directory to search under. Defaults to the workspace."},
                        "glob": {"type": "string", "description": "Only search files matching this, e.g. `*.rs`."}
                    },
                    "required": ["pattern"]
                }
            }),
            json!({
                "name": BASH,
                "description": "Run a shell command in the workspace and return its output. \
                    Output is truncated if very long, and the command is stopped if it runs \
                    past its timeout.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "timeout": {"type": "integer", "description": "Milliseconds before the command is stopped."}
                    },
                    "required": ["command"]
                }
            }),
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
            // The intake gate TELLS the model to call this. Offering the gate
            // without the tool is an instruction that cannot be followed, and
            // the loop stalls on a brief that can never be saved.
            crate::brief::save_tool_definition(),
        ];
        all.extend(crate::knowledge::tool_definitions());
        all.extend(crate::web::tool_definitions());
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
        crate::trace!(
            "tool",
            "{name} <- {}",
            crate::text::truncate(&input.to_string(), 160)
        );
        // BEFORE the call, not after. The sentinel that watches the tree can
        // only report a write that already happened; a protected path has to be
        // refused while refusing it still means something.
        if let Some(refusal) = self.refused(name, input) {
            crate::trace!("tool", "{name} REFUSED by guards: {refusal}");
            return Err(refusal);
        }
        // The session's mode, before the engine: it is decided here, costs
        // nothing, and a call it refuses need not cost an engine process.
        if let Err(refusal) = self.mode_allows(call_id, name, input) {
            crate::trace!("tool", "{name} REFUSED by the permission mode: {refusal}");
            return Err(refusal);
        }
        // AFTER the operator's own guards, which are free: a call they refuse
        // never costs an engine process. BEFORE the snapshot and the dispatch,
        // because this is the last point where refusing still prevents.
        let context = match self.engine_gate(call_id, name, input) {
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
        let outcome = self.dispatch(name, input);
        // A write that LANDED is news to the engine — it files the path under
        // the session and queues the file to be read again. Only a write that
        // happened: a refused or failed one changed nothing. Not waited on.
        if matches!(name, WRITE | EDIT) {
            if let (Some(engine), Ok(said)) = (&self.engine, &outcome) {
                let at = crate::hooks::engine::Conversation {
                    session: &self.session,
                    cwd: &self.workspace,
                    transcript: None,
                };
                engine.code_edit(at, name, &self.event_input(name, input), said);
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

    /// Whether the session's permission mode lets this call run.
    ///
    /// Only the calls that change the workspace or run code are governed:
    /// writing, editing, the shell, the project's check and the brief. Reading
    /// and searching run under every mode. A subagent is refused wherever the
    /// mode restricts anything, because the agent it would start does not
    /// carry this mode and could do what this one may not.
    fn mode_allows(&self, call_id: &str, name: &str, input: &Value) -> Result<(), String> {
        use crate::permissions::PermissionMode;
        let Some(mode) = self.mode else {
            return Ok(());
        };
        let changes = matches!(name, WRITE | EDIT | BASH)
            || name == crate::verify::VERIFY_TOOL
            || name == crate::brief::SAVE_TOOL;
        let restricted = matches!(mode, PermissionMode::ReadOnly | PermissionMode::Prompt);
        if name == AGENT && restricted {
            return Err(format!(
                "this session is {}, and a subagent would not be held to that. Do the \
                 work here instead.",
                mode.as_str()
            ));
        }
        if !changes {
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
    /// crashed, it ran past its bound — a `Write`, `Edit` or `Bash` is
    /// REFUSED: the guard is a safety function, and a change made because the
    /// guard was down is the one it exists to stop. Every other tool changes
    /// nothing and runs, traced. The same rule as the guards file above: an
    /// unreadable guard must not read as no guard.
    fn engine_gate(&self, call_id: &str, name: &str, input: &Value) -> Gate {
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
            crate::hooks::engine::PreTool::Failed(why) if matches!(name, WRITE | EDIT | BASH) => {
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

    /// The call as the engine is shown it: the model's input, with a file
    /// named relative to the workspace made absolute, because the engine keys
    /// its backups and its cards by path. Everything else is passed unchanged.
    fn event_input(&self, name: &str, input: &Value) -> Value {
        let mut shown = input.clone();
        if matches!(name, READ | WRITE | EDIT) {
            if let Some(path) = input.get("path").and_then(Value::as_str) {
                if Path::new(path).is_relative() {
                    shown["path"] = json!(self.workspace.join(path).to_string_lossy());
                }
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

    /// Run the tool itself: guards and the engine have already passed it.
    fn dispatch(&self, name: &str, input: &Value) -> Result<String, String> {
        // The file and shell tools parse their own JSON from a string, which is
        // the interface they were written to; re-serialising here keeps that
        // contract rather than rewriting six working implementations.
        let raw = input.to_string();
        // The workspace, or the added directory the call names a path in.
        let ws = if matches!(name, READ | WRITE | EDIT | GLOB | GREP) {
            self.root_for(input)
        } else {
            self.workspace.as_path()
        };

        // Snapshot before file-modifying operations if checkpoints are enabled.
        if matches!(name, WRITE | EDIT) {
            if let Some(checkpoints) = &self.checkpoints {
                if let Ok(target) = crate::tools::file_ops::target_path(ws, &raw) {
                    if let Ok(mut store) = checkpoints.lock() {
                        store.snapshot(&target);
                    }
                }
            }
        }

        let began = std::time::Instant::now();
        let outcome = match name {
            READ => crate::tools::file_ops::read_file(ws, &raw).map_err(|e| e.to_string()),
            WRITE => crate::tools::file_ops::write_file(ws, &raw).map_err(|e| e.to_string()),
            EDIT => crate::tools::file_ops::edit_file(ws, &raw).map_err(|e| e.to_string()),
            GLOB => crate::tools::file_ops::glob_search(ws, &raw).map_err(|e| e.to_string()),
            GREP => crate::tools::file_ops::grep_search(ws, &raw).map_err(|e| e.to_string()),
            BASH => crate::tools::bash::execute_bash(ws, &raw, self.filter_env)
                .map_err(|e| e.to_string()),
            crate::verify::VERIFY_TOOL => crate::verify::run_tool(ws),
            crate::recall::INDEX_TOOL => crate::recall::run_index_tool(&raw),
            crate::recall::GET_TOOL => crate::recall::run_get_tool(&raw),
            SKILL => self.read_skill(input),
            AGENT => self.run_subagent(input),
            ASK_USER_QUESTION => self.ask_here(input),
            crate::brief::SAVE_TOOL => self.save_brief(&raw),
            other => crate::knowledge::handle_tool(other, input)
                .or_else(|| crate::web::handle_tool(other, input))
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
mod tests {
    use super::*;

    fn toolbox(root: &Path) -> Toolbox {
        Toolbox::new(root, true)
    }

    #[test]
    fn every_advertised_tool_can_actually_be_run() {
        // THE INVARIANT, in the direction that costs a model a wasted turn: a
        // schema whose name reaches no implementation answers "there is no tool
        // called X" to a model that was told there was.
        let dir = tempfile::tempdir().expect("tmp");
        let box_ = toolbox(dir.path());
        for name in box_.names() {
            let answer = box_.run(&name, &json!({}));
            // The call is expected to FAIL here — it is given no arguments. What
            // must never happen is failing because nothing is wired to the name.
            if let Err(e) = answer {
                assert!(
                    !e.contains("there is no tool called"),
                    "advertised but not wired: {name} -> {e}"
                );
            }
        }
    }

    #[test]
    fn an_unknown_tool_is_an_error_the_model_can_read() {
        let dir = tempfile::tempdir().expect("tmp");
        let answer = toolbox(dir.path()).run("Teleport", &json!({}));
        let message = answer.expect_err("an unknown tool cannot succeed");
        assert!(message.contains("Teleport"), "{message}");
    }

    #[test]
    fn the_control_for_that_invariant_works() {
        // Proves the check above can fail at all: an unwired name really does
        // produce the string the assertion looks for. Without this the loop
        // could be passing because the message changed.
        let dir = tempfile::tempdir().expect("tmp");
        let e = toolbox(dir.path())
            .run("NotATool", &json!({}))
            .expect_err("must fail");
        assert!(e.contains("there is no tool called"), "{e}");
    }

    #[test]
    fn every_schema_is_shaped_the_way_a_model_expects() {
        let dir = tempfile::tempdir().expect("tmp");
        for def in toolbox(dir.path()).definitions() {
            let name = def.get("name").and_then(Value::as_str);
            assert!(name.is_some_and(|n| !n.is_empty()), "unnamed tool: {def}");
            let described = def
                .get("description")
                .and_then(Value::as_str)
                .is_some_and(|d| d.len() > 20);
            assert!(described, "{name:?} needs a description a model can act on");
            let schema = def.get("input_schema").and_then(|s| s.get("type"));
            assert_eq!(
                schema.and_then(Value::as_str),
                Some("object"),
                "{name:?} input_schema"
            );
        }
    }

    #[test]
    fn no_tool_is_advertised_twice() {
        // Two entries under one name means the second is unreachable, and which
        // one a provider honours is not ours to decide.
        let dir = tempfile::tempdir().expect("tmp");
        let mut names = toolbox(dir.path()).names();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), before, "duplicated: {names:?}");
    }

    #[test]
    fn the_file_schema_names_the_field_the_implementation_reads() {
        // A schema saying `file_path` over an implementation reading `path`
        // fails on every single call, and nothing in either half would show it:
        // both are correct alone. This is the join, which is what was missing.
        let dir = tempfile::tempdir().expect("tmp");
        let defs = toolbox(dir.path()).definitions();
        let read = defs
            .iter()
            .find(|d| d["name"] == READ)
            .expect("Read is offered");
        let props = &read["input_schema"]["properties"];
        assert!(
            props.get("path").is_some(),
            "the implementation reads `path`: {props}"
        );
        assert!(
            props.get("file_path").is_none(),
            "the implementation does NOT read `file_path`: {props}"
        );
    }

    #[test]
    fn reading_a_file_works_through_the_toolbox() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("hello.txt"), "first\nsecond\n").expect("write");
        let out = toolbox(dir.path())
            .run(READ, &json!({"path": "hello.txt"}))
            .expect("read");
        assert!(out.contains("first"), "{out}");
        assert!(out.contains("second"), "{out}");
    }

    #[test]
    fn writing_then_reading_returns_what_was_written() {
        // The round trip, not a return code: a write that reports success and
        // leaves nothing on disk is the failure worth catching.
        let dir = tempfile::tempdir().expect("tmp");
        let box_ = toolbox(dir.path());
        box_.run(
            WRITE,
            &json!({"path": "new.txt", "content": "the contents"}),
        )
        .expect("write");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.txt")).expect("on disk"),
            "the contents",
            "the file must actually hold it"
        );
        let back = box_.run(READ, &json!({"path": "new.txt"})).expect("read");
        assert!(back.contains("the contents"), "{back}");
    }

    #[test]
    fn editing_changes_the_file_on_disk() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("e.txt"), "keep OLD keep").expect("write");
        toolbox(dir.path())
            .run(
                EDIT,
                &json!({"path": "e.txt", "old_string": "OLD", "new_string": "NEW"}),
            )
            .expect("edit");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("e.txt")).expect("read"),
            "keep NEW keep"
        );
    }

    #[test]
    fn a_failing_call_reports_the_failure_rather_than_inventing_a_result() {
        // A model told a read succeeded when the file is missing will build on
        // contents that were never there.
        let dir = tempfile::tempdir().expect("tmp");
        let answer = toolbox(dir.path()).run(READ, &json!({"path": "absent.txt"}));
        assert!(answer.is_err(), "got: {answer:?}");
    }

    #[test]
    fn searching_finds_a_file_that_is_there_and_not_one_that_is_not() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("found.rs"), "fn marker() {}").expect("write");
        let box_ = toolbox(dir.path());
        let hits = box_
            .run(GLOB, &json!({"pattern": "**/*.rs"}))
            .expect("glob");
        assert!(hits.contains("found.rs"), "{hits}");
        let none = box_
            .run(GLOB, &json!({"pattern": "**/*.nope"}))
            .expect("glob");
        assert!(!none.contains("found.rs"), "control: {none}");
    }

    #[test]
    fn the_gate_that_demands_a_brief_offers_the_tool_that_saves_one() {
        // Watched live: the intake gate tells the model to "Save each part with
        // save_brief" while that tool was offered to nobody. An instruction to
        // call a tool that does not exist is a loop with no way out of it.
        let dir = tempfile::tempdir().expect("tmp");
        let names = toolbox(dir.path()).names();
        assert!(
            names.iter().any(|n| n == crate::brief::SAVE_TOOL),
            "the gate's own tool is missing: {names:?}"
        );
    }

    #[test]
    fn asking_for_a_skill_without_naming_one_costs_no_request() {
        // An unnamed ask cannot match anything, so going to the store for it
        // spends a request and a four-second timeout to answer a question that
        // was already answered.
        let dir = tempfile::tempdir().expect("tmp");
        let started = std::time::Instant::now();
        let refused = toolbox(dir.path())
            .run(SKILL, &json!({}))
            .expect_err("nothing to look up");
        assert!(refused.contains("say which skill"), "{refused}");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "it went to the store anyway: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn delegation_stops_at_the_depth_limit() {
        // A subagent that can spawn subagents that can spawn subagents is a bill
        // with no ceiling. The refusal happens at the tool, because that is where
        // the new agent would be made.
        let dir = tempfile::tempdir().expect("tmp");
        let deep = Toolbox::new(dir.path(), true).at_depth(MAX_DEPTH);
        let refused = deep
            .run(AGENT, &json!({"task": "go and do a thing"}))
            .expect_err("past the limit");
        assert!(refused.contains("agents deep"), "{refused}");
        assert!(
            deep.children().is_empty(),
            "and nothing was spawned to record"
        );
    }

    #[test]
    fn a_subagent_with_no_task_is_refused_before_anything_is_spawned() {
        // An empty task cannot be done, and finding that out by paying for a
        // conversation is the expensive way to learn it.
        let dir = tempfile::tempdir().expect("tmp");
        let box_ = toolbox(dir.path());
        let refused = box_
            .run(AGENT, &json!({"task": "   "}))
            .expect_err("nothing to do");
        assert!(refused.contains("needs a task"), "{refused}");
        assert!(box_.children().is_empty(), "nothing was spawned");
    }

    #[test]
    fn the_control_for_the_depth_limit_works() {
        // Proves the limit is what refused, not the task check above it: the
        // SAME call one level shallower gets past the depth gate and fails
        // later, on something else.
        let dir = tempfile::tempdir().expect("tmp");
        let shallow = Toolbox::new(dir.path(), true).at_depth(MAX_DEPTH - 1);
        let refused = shallow
            .run(AGENT, &json!({"task": "go and do a thing"}))
            .expect_err("no endpoint is configured in a test");
        assert!(
            !refused.contains("agents deep"),
            "the depth gate fired at the wrong depth: {refused}"
        );
    }

    #[test]
    fn a_write_to_a_protected_path_is_refused() {
        // An agent CLI enforces this itself. Over an API these functions ARE
        // the write, so unenforced here means unenforced entirely.
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir.path().join("tests")).expect("dir");
        let box_ = toolbox(dir.path());
        let refused = box_
            .run(
                WRITE,
                &json!({"path": "tests/it.rs", "content": "assert!(true)"}),
            )
            .expect_err("a protected path must be refused");
        assert!(refused.contains("protected"), "{refused}");
        // BEFORE the write, not after: the file must not exist at all.
        assert!(
            !dir.path().join("tests/it.rs").exists(),
            "the refusal came too late — the file was written"
        );
        // The control: an ordinary path in the same workspace still works once
        // the spec set exists, or this proves only that writing is broken.
        for f in &crate::spec::SPEC_SET {
            let full = dir.path().join(f.path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(
                full,
                if f.path.ends_with(".json") {
                    "{}"
                } else {
                    "ok"
                },
            )
            .unwrap();
        }
        std::fs::create_dir_all(dir.path().join("src")).expect("dir");
        box_.run(
            WRITE,
            &json!({"path": "src/it.rs", "content": "fn main() {}"}),
        )
        .expect("an unprotected path is fine");
        assert!(dir.path().join("src/it.rs").exists());
    }

    #[test]
    fn writing_source_without_spec_is_refused_by_toolbox() {
        let dir = tempfile::tempdir().expect("tmp");
        let box_ = toolbox(dir.path());
        let refused = box_
            .run(
                WRITE,
                &json!({"path": "src/it.rs", "content": "fn main() {}"}),
            )
            .expect_err("source write without spec set must be refused");
        assert!(refused.contains("not specified yet"), "{refused}");
    }

    #[test]
    fn editing_a_protected_path_is_refused_too() {
        // Write and Edit both change a file; guarding one and not the other
        // leaves the door open beside the locked one.
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir.path().join("tests")).expect("dir");
        std::fs::write(dir.path().join("tests/it.rs"), "OLD").expect("write");
        let refused = toolbox(dir.path())
            .run(
                EDIT,
                &json!({"path": "tests/it.rs", "old_string": "OLD", "new_string": "NEW"}),
            )
            .expect_err("refused");
        assert!(refused.contains("protected"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tests/it.rs")).expect("read"),
            "OLD",
            "the file is untouched"
        );
    }

    #[test]
    fn reading_a_protected_path_is_still_allowed() {
        // The guards protect against writes. An agent that cannot READ the
        // check it must satisfy cannot satisfy it.
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir.path().join("tests")).expect("dir");
        std::fs::write(dir.path().join("tests/it.rs"), "the check").expect("write");
        let out = toolbox(dir.path())
            .run(READ, &json!({"path": "tests/it.rs"}))
            .expect("reading is not writing");
        assert!(out.contains("the check"), "{out}");
    }

    #[test]
    fn unreadable_guards_stop_writes_rather_than_permitting_them() {
        // The failure the guards module calls the worst one: protections that
        // quietly stop applying while everything keeps working.
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir.path().join(".incord")).expect("dir");
        std::fs::write(dir.path().join(crate::guards::GUARDS_FILE), "{ not json").expect("write");
        let refused = toolbox(dir.path())
            .run(WRITE, &json!({"path": "anything.txt", "content": "x"}))
            .expect_err("a broken guards file must not read as no guards");
        assert!(refused.contains("could not be read"), "{refused}");
        assert!(
            !dir.path().join("anything.txt").exists(),
            "and nothing was written"
        );
    }

    #[test]
    fn the_shell_tool_runs_a_command_and_returns_its_output() {
        let dir = tempfile::tempdir().expect("tmp");
        let out = toolbox(dir.path())
            .run(BASH, &json!({"command": "echo toolbox_probe"}))
            .expect("bash");
        assert!(out.contains("toolbox_probe"), "{out}");
    }

    #[test]
    fn the_orphaned_tools_are_all_reachable_again() {
        // Named individually rather than counted: a count passes while the
        // wrong six are present.
        let dir = tempfile::tempdir().expect("tmp");
        let names = toolbox(dir.path()).names();
        for expected in [
            READ,
            WRITE,
            EDIT,
            GLOB,
            GREP,
            BASH,
            crate::verify::VERIFY_TOOL,
            crate::recall::INDEX_TOOL,
            crate::recall::GET_TOOL,
            crate::knowledge::REMEMBER_TOOL,
            crate::knowledge::SEARCH_TOOL,
            crate::knowledge::FORGET_TOOL,
            SKILL,
            AGENT,
            crate::brief::SAVE_TOOL,
            crate::knowledge::SYMBOL_TOOL,
            crate::web::FETCH_TOOL,
            crate::web::SEARCH_TOOL,
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "{expected} is not offered; have {names:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // The memory engine's gate: pre-tool, the ask, and questions.
    // -----------------------------------------------------------------------

    use crate::hooks::engine::fake::{self, Reply};
    use crate::hooks::engine::{Ask, Prompter, Verdict};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const ASK_JSON: &str = r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"git push needs the owner"}}"#;
    const ALLOW_JSON: &str = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;
    const DENY_JSON: &str = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"no"}}}"#;

    /// A toolbox over `root` whose engine is the fake in `engine`.
    fn guarded(root: &Path, engine: &Path, replies: &[(&str, Reply)], cap: u64) -> Toolbox {
        let bin = fake::write(engine, replies);
        let mut toolbox = Toolbox::new(root, true)
            .with_engine(Some(fake::engine(bin).capped(Duration::from_secs(cap))));
        toolbox.set_session("api-test-session");
        toolbox
    }

    /// Someone at the session, scripted.
    #[derive(Default)]
    struct Person {
        /// What they answer to a confirmation; `None` waits for the answer to
        /// come from elsewhere instead.
        confirm: Option<Verdict>,
        choose: Option<Vec<crate::ask::Answer>>,
        asked: AtomicUsize,
        chose: AtomicUsize,
    }

    impl Prompter for Person {
        fn confirm(&self, _ask: &Ask<'_>, settled: &dyn Fn() -> bool) -> Option<Verdict> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            if self.confirm.is_some() {
                return self.confirm.clone();
            }
            let deadline = Instant::now() + Duration::from_secs(30);
            while !settled() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            None
        }

        fn choose(&self, _questions: &[crate::ask::Question]) -> Option<Vec<crate::ask::Answer>> {
            self.chose.fetch_add(1, Ordering::SeqCst);
            self.choose.clone()
        }
    }

    /// §3 test 1. **EXIT 2 IS A REFUSAL, AND NOTHING IS WRITTEN.**
    #[test]
    fn a_blocked_write_is_not_made_and_the_model_is_told_why() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        std::fs::write(ws.path().join("notes.txt"), "before").expect("seed");
        let toolbox = guarded(
            ws.path(),
            engine.path(),
            &[("pre-tool", Reply::exit(2).stderr("no backup of notes.txt"))],
            30,
        );
        let refused = toolbox
            .run(WRITE, &json!({"path": "notes.txt", "content": "after"}))
            .expect_err("a block is an error");
        assert!(refused.contains("no backup"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(ws.path().join("notes.txt")).expect("read"),
            "before",
            "the file is unchanged on disk"
        );
    }

    /// §3 test 2. **SILENCE IS AN ALLOW.**
    #[test]
    fn an_engine_that_says_nothing_lets_the_tool_run() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        let toolbox = guarded(
            ws.path(),
            engine.path(),
            &[("pre-tool", Reply::exit(0))],
            30,
        );
        toolbox
            .run(WRITE, &json!({"path": "made.txt", "content": "made"}))
            .expect("allowed");
        assert_eq!(
            std::fs::read_to_string(ws.path().join("made.txt")).expect("the tool ran"),
            "made"
        );
        assert_eq!(fake::calls(engine.path(), "pre-tool"), 1);
    }

    /// The rules the engine attaches to an allowed action reach the model,
    /// with the result it acts on.
    #[test]
    fn the_engines_guidance_travels_with_the_result() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        std::fs::write(ws.path().join("r.txt"), "the contents").expect("seed");
        let toolbox = guarded(
            ws.path(),
            engine.path(),
            &[(
                "pre-tool",
                Reply::exit(0).stdout(
                    r#"{"hookSpecificOutput":{"permissionDecision":"allow","additionalContext":"RULE-7: read before you write"}}"#,
                ),
            )],
            30,
        );
        let said = toolbox.run(READ, &json!({"path": "r.txt"})).expect("read");
        assert!(said.contains("the contents"), "{said}");
        assert!(said.contains("RULE-7"), "{said}");
    }

    /// §3 test 3 and §6's unattended case. **AN ASK WITH NOBODY TO ASK IS A
    /// REFUSAL**, when the owner's app does not answer either — and the
    /// engine's reason reaches the model.
    #[test]
    fn an_ask_nobody_answers_is_refused_with_the_reason() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        let toolbox = guarded(
            ws.path(),
            engine.path(),
            &[
                ("pre-tool", Reply::exit(0).stdout(ASK_JSON)),
                ("permission-request", Reply::exit(0)),
            ],
            30,
        );
        let refused = toolbox
            .run(WRITE, &json!({"path": "asked.txt", "content": "x"}))
            .expect_err("nobody confirmed it");
        assert!(refused.contains("git push needs the owner"), "{refused}");
        assert!(
            !ws.path().join("asked.txt").exists(),
            "the tool did not run"
        );
        assert_eq!(fake::calls(engine.path(), "permission-request"), 1);
    }

    /// §3 test 4. **A CALL THE OPERATOR'S GUARDS REFUSE NEVER STARTS THE
    /// ENGINE.**
    #[test]
    fn a_native_refusal_never_spawns_the_engine() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        std::fs::create_dir_all(ws.path().join("tests")).expect("dir");
        let toolbox = guarded(
            ws.path(),
            engine.path(),
            &[("pre-tool", Reply::exit(0))],
            30,
        );
        let refused = toolbox
            .run(WRITE, &json!({"path": "tests/it.rs", "content": "x"}))
            .expect_err("protected");
        assert!(refused.contains("protected"), "{refused}");
        assert_eq!(
            fake::calls(engine.path(), "pre-tool"),
            0,
            "the engine was spawned for a call already refused"
        );
    }

    /// The owner's failure policy: an engine that cannot decide stops a
    /// change, and lets a read through.
    #[test]
    fn an_engine_that_cannot_decide_stops_writes_and_lets_reads_through() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        std::fs::write(ws.path().join("r.txt"), "readable").expect("seed");
        let toolbox = guarded(
            ws.path(),
            engine.path(),
            &[("pre-tool", Reply::exit(1))],
            30,
        );
        let refused = toolbox
            .run(BASH, &json!({"command": "echo should-not-run > ran.txt"}))
            .expect_err("fail closed");
        assert!(refused.contains("could not decide"), "{refused}");
        assert!(!ws.path().join("ran.txt").exists());
        let read = toolbox
            .run(READ, &json!({"path": "r.txt"}))
            .expect("fail open");
        assert!(read.contains("readable"), "{read}");
    }

    /// The same policy for an engine that runs past its bound: killed, and
    /// the write refused.
    #[test]
    fn an_engine_past_its_bound_stops_the_write() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        let toolbox = guarded(
            ws.path(),
            engine.path(),
            &[("pre-tool", Reply::exit(0).after(20))],
            2,
        );
        let started = Instant::now();
        let refused = toolbox
            .run(WRITE, &json!({"path": "late.txt", "content": "x"}))
            .expect_err("timed out");
        assert!(refused.contains("timed out"), "{refused}");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "{:?}",
            started.elapsed()
        );
        assert!(!ws.path().join("late.txt").exists());
    }

    /// A named engine that is not there is the same failure, not a quiet
    /// "no guard".
    #[test]
    fn a_named_engine_that_is_missing_stops_the_write() {
        let ws = tempfile::tempdir().expect("ws");
        let toolbox = Toolbox::new(ws.path(), true)
            .with_engine(Some(fake::engine(ws.path().join("no-such-engine.exe"))));
        let refused = toolbox
            .run(WRITE, &json!({"path": "x.txt", "content": "x"}))
            .expect_err("missing engine");
        assert!(refused.contains("could not start"), "{refused}");
        assert!(!ws.path().join("x.txt").exists());
    }

    /// §6 test 1. **THE OWNER'S APP ANSWERS FIRST**: allowed there, the tool
    /// runs, and the person at the session did not have to answer.
    #[test]
    fn an_allow_from_the_owners_app_runs_the_tool_with_no_local_answer() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        let mut toolbox = guarded(
            ws.path(),
            engine.path(),
            &[
                ("pre-tool", Reply::exit(0).stdout(ASK_JSON)),
                ("permission-request", Reply::exit(0).stdout(ALLOW_JSON)),
            ],
            30,
        );
        let person = Arc::new(Person::default());
        toolbox.set_prompter(person.clone());
        toolbox
            .run(WRITE, &json!({"path": "ok.txt", "content": "ok"}))
            .expect("allowed in the app");
        assert!(ws.path().join("ok.txt").exists());
        assert_eq!(
            person.asked.load(Ordering::SeqCst),
            1,
            "the prompt was shown"
        );
    }

    /// §6 test 2. A refusal in the app is the model's error, in the app's
    /// words.
    #[test]
    fn a_deny_from_the_owners_app_is_the_error() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        let toolbox = guarded(
            ws.path(),
            engine.path(),
            &[
                ("pre-tool", Reply::exit(0).stdout(ASK_JSON)),
                ("permission-request", Reply::exit(0).stdout(DENY_JSON)),
            ],
            30,
        );
        let refused = toolbox
            .run(WRITE, &json!({"path": "no.txt", "content": "x"}))
            .expect_err("denied");
        assert_eq!(refused, "no");
        assert!(!ws.path().join("no.txt").exists());
    }

    /// §6 test 4. **THE PERMISSION EVENT NAMES THE SAME CALL**: its
    /// `tool_input` is byte-identical to the pre-tool event's, which is how the
    /// engine finds the card the pre-tool raised.
    #[test]
    fn the_permission_event_carries_the_pre_tool_events_input() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        let toolbox = guarded(
            ws.path(),
            engine.path(),
            &[
                ("pre-tool", Reply::exit(0).stdout(ASK_JSON)),
                ("permission-request", Reply::exit(0).stdout(DENY_JSON)),
            ],
            30,
        );
        let _ = toolbox.run(
            EDIT,
            &json!({"path": "e.txt", "old_string": "a", "new_string": "b"}),
        );
        let pre = fake::events(engine.path(), "pre-tool");
        let permission = fake::events(engine.path(), "permission-request");
        assert_eq!(pre.len(), 1);
        assert_eq!(permission.len(), 1);
        assert_eq!(permission[0]["hook_event_name"], "PermissionRequest");
        assert_eq!(
            pre[0]["tool_input"].to_string(),
            permission[0]["tool_input"].to_string()
        );
        // And the file is named absolutely, since the engine keys by path.
        let named = pre[0]["tool_input"]["path"].as_str().expect("a path");
        assert!(Path::new(named).is_absolute(), "{named}");
    }

    /// The race, the terminal's way round: the person answers first, and the
    /// hook still waiting on the owner's app is stopped rather than left to
    /// run out its ten minutes.
    #[test]
    fn a_local_answer_wins_and_stops_the_hook() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        let mut toolbox = guarded(
            ws.path(),
            engine.path(),
            &[
                ("pre-tool", Reply::exit(0).stdout(ASK_JSON)),
                (
                    "permission-request",
                    Reply::exit(0).stdout(DENY_JSON).after(20),
                ),
            ],
            60,
        );
        toolbox.set_prompter(Arc::new(Person {
            confirm: Some(Verdict::Allow),
            ..Person::default()
        }));
        let started = Instant::now();
        toolbox
            .run(WRITE, &json!({"path": "mine.txt", "content": "x"}))
            .expect("allowed here");
        assert!(ws.path().join("mine.txt").exists());
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "waited on the losing hook: {:?}",
            started.elapsed()
        );
    }

    /// §4 test 4. **THE EXACT NAME**: the engine recognises a question tool by
    /// it, so a rename turns routing off without a sound.
    #[test]
    fn the_question_tool_is_offered_under_its_exact_name() {
        assert_eq!(ASK_USER_QUESTION, "AskUserQuestion");
        let dir = tempfile::tempdir().expect("tmp");
        assert!(toolbox(dir.path())
            .names()
            .iter()
            .any(|n| n == "AskUserQuestion"));
    }

    fn database_question() -> Value {
        json!({"questions": [{
            "question": "Which database should this use?",
            "header": "Database",
            "options": [
                {"label": "Postgres", "description": "relational; the document implies joins"},
                {"label": "SQLite", "description": "one file, nothing to operate"}
            ],
            "multiSelect": false
        }]})
    }

    /// §4 test 1. **AN ANSWER GIVEN IN THE APP IS THE RESULT, NOT AN ERROR**,
    /// and nobody is asked again here.
    #[test]
    fn a_question_answered_in_the_app_returns_the_answer() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        let answered = "The owner answered your question: Postgres\nContinue with that answer — do not ask it again.";
        let mut toolbox = guarded(
            ws.path(),
            engine.path(),
            &[("pre-tool", Reply::exit(2).stderr(answered))],
            30,
        );
        let person = Arc::new(Person::default());
        toolbox.set_prompter(person.clone());
        let said = toolbox
            .run(ASK_USER_QUESTION, &database_question())
            .expect("an answer is not an error");
        assert_eq!(said, answered);
        assert_eq!(person.chose.load(Ordering::SeqCst), 0, "no local prompt");
    }

    /// §4 test 2. Not answered in the app, and nobody here who can answer —
    /// the ACP client, which has no question request, or a run with nobody at
    /// it: the model is told so, with the options.
    #[test]
    fn a_question_nobody_can_answer_says_so_with_its_options() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        let mut toolbox = guarded(
            ws.path(),
            engine.path(),
            &[("pre-tool", Reply::exit(0))],
            30,
        );
        toolbox.set_prompter(Arc::new(Person::default()));
        let said = toolbox
            .run(ASK_USER_QUESTION, &database_question())
            .expect("a note, not an error");
        assert!(said.contains("nobody is at the prompt"), "{said}");
        assert!(
            said.contains("Postgres") && said.contains("SQLite"),
            "{said}"
        );
    }

    /// Answered at the terminal: the pick goes back as the result.
    #[test]
    fn a_question_answered_here_returns_the_pick() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        let mut toolbox = guarded(
            ws.path(),
            engine.path(),
            &[("pre-tool", Reply::exit(0))],
            30,
        );
        toolbox.set_prompter(Arc::new(Person {
            choose: Some(vec![crate::ask::Answer::Chose("SQLite".to_string())]),
            ..Person::default()
        }));
        let said = toolbox
            .run(ASK_USER_QUESTION, &database_question())
            .expect("answered");
        assert!(said.contains("A: SQLite"), "{said}");
    }

    /// §4 test 3. The labels reach the engine unchanged: it turns them into
    /// the buttons in the owner's app.
    #[test]
    fn the_questions_labels_reach_the_engine_unchanged() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        let toolbox = guarded(
            ws.path(),
            engine.path(),
            &[("pre-tool", Reply::exit(0))],
            30,
        );
        let _ = toolbox.run(ASK_USER_QUESTION, &database_question());
        let events = fake::events(engine.path(), "pre-tool");
        assert_eq!(events[0]["tool_name"], "AskUserQuestion");
        assert_eq!(
            events[0]["tool_input"]["questions"][0]["options"],
            database_question()["questions"][0]["options"]
        );
    }

    /// **READ-ONLY IS DECIDED HERE.** A write is refused and nothing lands; a
    /// read still runs. The control is the same write with no mode set.
    #[test]
    fn a_read_only_session_refuses_changes_and_allows_reads() {
        use crate::permissions::PermissionMode;
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("r.txt"), "readable").expect("seed");
        let mut box_ = toolbox(dir.path());
        box_.set_permission_mode(PermissionMode::ReadOnly);

        let refused = box_
            .run(WRITE, &json!({"path": "w.txt", "content": "x"}))
            .expect_err("read-only refuses a write");
        assert!(refused.contains("read-only"), "{refused}");
        assert!(!dir.path().join("w.txt").exists(), "nothing was written");
        let refused = box_
            .run(BASH, &json!({"command": "echo ran > ran.txt"}))
            .expect_err("read-only refuses the shell");
        assert!(refused.contains("read-only"), "{refused}");
        assert!(!dir.path().join("ran.txt").exists());
        assert!(box_
            .run(READ, &json!({"path": "r.txt"}))
            .expect("reading is not changing")
            .contains("readable"));

        // The control: no mode, the same write lands.
        toolbox(dir.path())
            .run(WRITE, &json!({"path": "w.txt", "content": "x"}))
            .expect("no mode, no refusal");
        assert!(dir.path().join("w.txt").exists());
    }

    /// **`prompt` ASKS THE PERSON**, and their answer decides.
    #[test]
    fn a_prompt_session_asks_before_a_write_and_obeys_the_answer() {
        use crate::permissions::PermissionMode;
        let dir = tempfile::tempdir().expect("tmp");

        let yes = Arc::new(Person {
            confirm: Some(Verdict::Allow),
            ..Person::default()
        });
        let mut box_ = toolbox(dir.path());
        box_.set_permission_mode(PermissionMode::Prompt);
        box_.set_prompter(yes.clone());
        box_.run(WRITE, &json!({"path": "yes.txt", "content": "x"}))
            .expect("allowed by the person");
        assert!(dir.path().join("yes.txt").exists());
        assert_eq!(yes.asked.load(Ordering::SeqCst), 1, "asked once");

        let no = Arc::new(Person {
            confirm: Some(Verdict::Deny("not now".to_string())),
            ..Person::default()
        });
        box_.set_prompter(no.clone());
        let refused = box_
            .run(WRITE, &json!({"path": "no.txt", "content": "x"}))
            .expect_err("refused by the person");
        assert_eq!(refused, "not now");
        assert!(!dir.path().join("no.txt").exists());

        // A read is not asked about.
        let _ = box_.run(GLOB, &json!({"pattern": "*.txt"}));
        assert_eq!(
            no.asked.load(Ordering::SeqCst),
            1,
            "only the write was asked"
        );
    }

    /// A subagent would not carry the mode, so a restricted session keeps
    /// the work.
    #[test]
    fn a_restricted_session_does_not_delegate() {
        use crate::permissions::PermissionMode;
        let dir = tempfile::tempdir().expect("tmp");
        let mut box_ = toolbox(dir.path());
        box_.set_permission_mode(PermissionMode::ReadOnly);
        let refused = box_
            .run(AGENT, &json!({"task": "go and do a thing"}))
            .expect_err("no delegation");
        assert!(refused.contains("subagent"), "{refused}");
        assert!(box_.children().is_empty());
    }

    #[test]
    fn writes_and_edits_via_toolbox_are_snapshotted_and_undoable() {
        let dir = tempfile::tempdir().expect("tmp");
        let ws = dir.path();
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::tools::checkpoint::CheckpointStore::new(),
        ));
        let box_ = toolbox(ws).with_checkpoints(store.clone());

        store.lock().unwrap().begin("create and modify a file");

        box_.run(
            WRITE,
            &json!({"path": "sample.txt", "content": "initial line\n"}),
        )
        .expect("write");
        assert_eq!(
            std::fs::read_to_string(ws.join("sample.txt")).unwrap(),
            "initial line\n"
        );

        box_.run(
            EDIT,
            &json!({
                "path": "sample.txt",
                "old_string": "initial line\n",
                "new_string": "modified line\n"
            }),
        )
        .expect("edit");
        assert_eq!(
            std::fs::read_to_string(ws.join("sample.txt")).unwrap(),
            "modified line\n"
        );

        let outcome = store.lock().unwrap().undo_last(ws).expect("undo");
        assert_eq!(outcome.restored.len(), 1);
        assert!(outcome.failed.is_empty());
        assert!(!ws.join("sample.txt").exists());
    }

    #[test]
    fn pre_existing_file_modified_via_toolbox_is_restored_on_undo() {
        let dir = tempfile::tempdir().expect("tmp");
        let ws = dir.path();
        std::fs::write(ws.join("existing.txt"), "before edit\n").expect("write");

        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::tools::checkpoint::CheckpointStore::new(),
        ));
        let box_ = toolbox(ws).with_checkpoints(store.clone());

        store.lock().unwrap().begin("edit existing");

        box_.run(
            EDIT,
            &json!({
                "path": "existing.txt",
                "old_string": "before edit\n",
                "new_string": "after edit\n"
            }),
        )
        .expect("edit");
        assert_eq!(
            std::fs::read_to_string(ws.join("existing.txt")).unwrap(),
            "after edit\n"
        );

        let outcome = store.lock().unwrap().undo_last(ws).expect("undo");
        assert_eq!(outcome.restored.len(), 1);
        assert_eq!(
            std::fs::read_to_string(ws.join("existing.txt")).unwrap(),
            "before edit\n"
        );
    }

    /// How many whole events the fake was given for `sub`, waiting up to
    /// `bound` for `want` — the code-edit hook is not waited on by the
    /// toolbox, so it lands a moment after the call returns.
    fn calls_within(dir: &Path, sub: &str, want: usize, bound: Duration) -> usize {
        let deadline = Instant::now() + bound;
        loop {
            let whole = fake::events_raw(dir, sub)
                .iter()
                .filter(|line| serde_json::from_str::<Value>(line).is_ok())
                .count();
            if whole >= want || Instant::now() >= deadline {
                return whole;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// **CODE-EDIT FIRES ONLY FOR A WRITE THAT LANDED.** One spawn for a
    /// successful Write, carrying the session and the absolute path; none for
    /// a Write the guards refused, an Edit that failed, or a Read.
    #[test]
    fn code_edit_fires_only_for_a_successful_write() {
        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("engine"),
        );
        let tools = guarded(
            ws.path(),
            engine.path(),
            &[("pre-tool", Reply::exit(0)), ("code-edit", Reply::exit(0))],
            20,
        );
        tools
            .run(WRITE, &json!({"path": "notes.txt", "content": "hi"}))
            .expect("the write lands");
        assert_eq!(
            calls_within(engine.path(), "code-edit", 1, Duration::from_secs(20)),
            1
        );
        let event = &fake::events(engine.path(), "code-edit")[0];
        assert_eq!(event["hook_event_name"], "PostToolUse");
        assert_eq!(event["session_id"], "api-test-session");
        assert_eq!(event["tool_name"], WRITE);
        assert_eq!(
            event["tool_input"]["path"],
            ws.path().join("notes.txt").to_string_lossy().as_ref()
        );

        // Refused by the guards, failed in the tool, and a read: none of them
        // changed a file, so none of them is news.
        assert!(tools
            .run(WRITE, &json!({"path": "tests/x.txt", "content": "no"}))
            .is_err());
        assert!(tools
            .run(
                EDIT,
                &json!({"path": "notes.txt", "old_string": "absent", "new_string": "x"})
            )
            .is_err());
        tools
            .run(READ, &json!({"path": "notes.txt"}))
            .expect("the read works");
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(
            fake::calls(engine.path(), "code-edit"),
            1,
            "only the write that landed"
        );
    }

    /// **AN ADDED DIRECTORY IS WRITABLE, AND GUARDED.** A write inside it by
    /// absolute path lands there; a protected path inside it is refused as it
    /// would be in the workspace, and nothing is written; a directory nobody
    /// added stays out of reach.
    #[test]
    fn an_extra_directory_is_writable_under_the_same_guards() {
        let (ws, extra, elsewhere) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("extra"),
            tempfile::tempdir().expect("elsewhere"),
        );
        let mut tools = toolbox(ws.path());
        tools.set_extra_dirs(vec![extra.path().to_path_buf()]);

        let allowed = extra.path().join("notes").join("plan.txt");
        tools
            .run(
                WRITE,
                &json!({"path": allowed.to_string_lossy(), "content": "added root"}),
            )
            .expect("a write inside an added directory lands");
        assert_eq!(
            std::fs::read_to_string(&allowed).expect("written"),
            "added root"
        );
        let read = tools
            .run(READ, &json!({"path": allowed.to_string_lossy()}))
            .expect("and reads back");
        assert!(read.contains("added root"), "{read}");

        let guarded_path = extra.path().join("tests").join("check.txt");
        let refused = tools
            .run(
                WRITE,
                &json!({"path": guarded_path.to_string_lossy(), "content": "rewritten"}),
            )
            .expect_err("a protected path in an added directory is refused");
        assert!(refused.contains("protected"), "{refused}");
        assert!(!guarded_path.exists(), "nothing was written");

        let outside = elsewhere.path().join("x.txt");
        assert!(tools
            .run(
                WRITE,
                &json!({"path": outside.to_string_lossy(), "content": "no"})
            )
            .is_err());
        assert!(!outside.exists());
    }
}
