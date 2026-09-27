//! The operator's control panel: which models, what ceiling, which check.
//!
//! These settings were scattered — a model name compiled into a constructor, a
//! fleet in one environment variable, a budget in another, a check detected from
//! a manifest. Each was individually defensible and together they meant nobody
//! could answer "what is this session actually going to do?" without reading the
//! source.
//!
//! This is one file the operator owns. Nothing here is a model the code chose.
//!
//! # Precedence, and why it runs this way
//!
//! A command-line flag beats an environment variable, which beats this file,
//! which beats the built-in fallback. The rule is *the more deliberate and the
//! more immediate the choice, the more it wins*: a flag is typed for one run, an
//! env var is set for one shell, the file is the standing decision.
//!
//! # Provenance is part of the answer
//!
//! Every resolved value carries where it came from. A fleet that silently
//! resolves to something unexpected is the failure this file exists to prevent,
//! and "which of the four places set this?" is the first question anyone asks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where the admin settings live, under the harness's data directory.
pub const ADMIN_FILE: &str = "admin.json";

/// Moves the whole data directory, which is what a test needs to read settings
/// without touching the operator's own.
pub const DATA_DIR_ENV: &str = "INCORD_CODE_DIR";

/// Environment variable naming which installed agent serves this session.
///
/// The value is an agent ID as [`crate::agent::registry::discover`] reports it,
/// not a path — the harness finds the binary itself and never reads a
/// credential.
pub const AGENT_ENV: &str = "INCORD_AGENT";

/// Where a setting came from, so a surprising value can be traced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A flag typed for this run.
    Flag,
    /// An environment variable set for this shell.
    Environment,
    /// The admin file — the standing decision.
    AdminFile,
    /// The built-in fallback, which is a last resort and not a recommendation.
    BuiltIn,
}

impl Source {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Environment => "env",
            Self::AdminFile => "admin file",
            Self::BuiltIn => "built-in",
        }
    }
}

/// What is said about a role naming a tier nobody defines.
///
/// One wording for `--admin` and `/roles`, so the same typo is not described
/// two ways depending on where the operator looked.
#[must_use]
pub fn unknown_tier_warning(role: &str, reference: &str) -> String {
    format!(
        "role {role} names {reference}, which no tier defines — it will run on the session model"
    )
}

/// A value and where it was decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved<T> {
    pub value: T,
    pub source: Source,
}

impl<T> Resolved<T> {
    #[must_use]
    pub fn new(value: T, source: Source) -> Self {
        Self { value, source }
    }
}

/// What the operator has decided, standing.
///
/// Every field is optional: an absent one means "I have no opinion, use the
/// layer below". A file of nulls behaves exactly as no file at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminConfig {
    /// The model this session runs on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Which installed agent serves this session, by its id.
    ///
    /// # Why this had to exist
    ///
    /// Agents are found by probing the PATH every session
    /// ([`crate::agent::registry::discover`]) and nothing was ever written down,
    /// so an operator could not see which agent had been chosen and had no way
    /// to pin one. The only way to express a preference was an `agent:model`
    /// prefix buried inside a role's candidate list — undiscoverable without
    /// reading the source.
    ///
    /// Unset means "whichever is installed", which is the behaviour before this
    /// field existed. Naming an agent that is NOT installed is reported by
    /// `--admin` and by `/doctor` rather than silently ignored: a pinned agent
    /// that quietly did not apply is the failure this field exists to end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Which agent takes the work the lead hands out, by its catalogue id.
    ///
    /// Unset means the lead's default from the built-in table beside
    /// [`crate::agent::registry::KNOWN_AGENTS`], which today is the lead
    /// itself. Written by `/agent sub <id>` and cleared by `/agent sub`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_agent: Option<String>,
    /// Services that list API agents about themselves.
    ///
    /// Each is asked for its agents when the catalogue is built — see
    /// [`crate::agent::catalogue`] for the exact request. Only the NAME of the
    /// variable holding a provider's key is kept here: the key itself is read
    /// from the environment when it is needed and never written to this file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_providers: Vec<ApiProvider>,
    /// The search service the `WebSearch` tool asks — see [`crate::web`] for
    /// the exact request and reply. Only the NAME of the variable holding
    /// its key is kept here, never the key. Unset, the tool says it is not
    /// configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<SearchProvider>,
    /// The owner's headless browser (`incord-browser`), when it is not on
    /// `PATH`. The `INCODE_BROWSER` variable wins over it. Unset and not
    /// found, the Screenshot tool is not offered and `WebFetch` runs no
    /// script — see `toolbox::browser`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_bin: Option<String>,
    /// Which program `RenderPdf` prints with. Unset is `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf_renderer: Option<PdfRenderer>,
    /// How tightly the OS confines a model's shell commands: `off`,
    /// `workspace-write` or `read-only` — see [`crate::sandbox`]. Unset ties it
    /// to the session's permission mode (read-only writes nothing, a writing
    /// mode writes the workspace, full access opts out), which is the safe
    /// default; naming one here pins it across every session and mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// Whether a sandboxed shell command may use the network: `allow` (the
    /// default, and the behaviour before this existed) or `deny` — see
    /// [`crate::sandbox`] for what each OS can enforce and what it only
    /// reports. `INCORD_SANDBOX_NETWORK` wins over it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_network: Option<String>,
    /// The capability ladder, named by the operator.
    ///
    /// A tier is a list of models that can do the same JOB — `"top"` might hold
    /// a big model from each vendor the operator has. Roles then reference a
    /// tier as `@top` instead of repeating its members, so a new model is added
    /// in ONE place rather than in every role that could use it. Vendor-neutral
    /// on purpose: a tier is a capability, and which vendor supplies it is a
    /// deployment fact that changes.
    ///
    /// Empty means no ladder, and every role's candidates are read literally —
    /// exactly the behaviour before this existed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tiers: BTreeMap<String, Vec<String>>,
    /// Which models serve which kind of delegated work, in the order to try.
    ///
    /// A candidate may carry a family tag as `model@family`, which is how the
    /// harness can tell a reviewer shares a lineage with the writer.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, Vec<String>>,
    /// Token ceiling for a run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u64>,
    /// The command that decides whether work is finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_command: Option<Vec<String>>,
    /// Where the memory service listens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_url: Option<String>,
    /// Where the memory engine's binary is, for the lifecycle hooks this
    /// harness runs itself (`hook pre-tool`, `hook permission-request`).
    ///
    /// Unset means the engine's own install location; the environment
    /// variable [`crate::hooks::engine::ENGINE_BIN_ENV`] wins over both. See
    /// [`crate::hooks::engine`] for the order and for what a missing binary
    /// does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_bin: Option<String>,
    /// Which parts of the body this harness uses.
    ///
    /// Every one is a PLUGIN, not a requirement. The harness runs without the
    /// memory service at all; what it loses is context, not the ability to work.
    /// Stated as settings rather than left implicit because "it silently did
    /// less" is indistinguishable from "it silently did nothing".
    #[serde(default)]
    pub plugins: Plugins,
    /// The operator's own commands, run around every tool call incode makes.
    ///
    /// # The name, and why here
    ///
    /// Named for the desktop, where they first ran: the desktop is the widest
    /// capability this harness hands over and the only one whose mistakes a
    /// checkpoint cannot undo. They now run around every toolbox tool (the
    /// desktop's still by its own executor), and `hooks` is accepted as the
    /// same key.
    ///
    /// In this file rather than per-workspace because it is a guard on the
    /// MACHINE — an operator who wants a confirmation before anything types
    /// into their screen wants it for every project, not for the one they
    /// remembered to configure. See [`crate::hooks`] for the contract, which
    /// includes the fact that a hook is shown the real input.
    ///
    /// Empty is the default and costs nothing.
    #[serde(
        default,
        alias = "hooks",
        skip_serializing_if = "crate::hooks::HookConfig::is_empty"
    )]
    pub desktop_hooks: crate::hooks::HookConfig,
}

/// One service that serves API agents and lists them on request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiProvider {
    /// Unique among providers; prefixes each of its agents' catalogue ids as
    /// `<name>/<agent id>`, so they cannot collide with a local agent's.
    pub name: String,
    /// The service's base URL. The agent list is read from `{url}/v1/agents`.
    pub url: String,
    /// The NAME of the environment variable holding the key — never the key.
    pub key_env: String,
}

/// Which program `RenderPdf` prints with — see `toolbox::pdf`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PdfRenderer {
    /// incord-browser when its PDFs carry text (probed once per session),
    /// else the system headless browser, else incord-browser as it is.
    #[default]
    Auto,
    /// Only the system headless browser.
    Chromium,
    /// Only incord-browser.
    IncordBrowser,
}

/// The web search service `WebSearch` asks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchProvider {
    /// The search endpoint. The query is added as `q`.
    pub url: String,
    /// The NAME of the environment variable holding its key, when it takes
    /// one — never the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
}

/// Which optional parts of the body are in use.
///
/// # Why these are opt-out rather than opt-in
///
/// The body is normally present, and a harness that ignored it by default would
/// re-ask what it has already been told — the thing this product exists to stop.
/// So each defaults to on and each can be switched off, and switching one off is
/// a decision the operator takes deliberately and can see in `/admin`.
///
/// # Why the owner channel is different from the rest
///
/// The others enrich a turn: without them the work is less informed and no less
/// correct. The owner channel decides questions a machine must not settle, and
/// turning it off does not move those decisions elsewhere — it means the runner
/// STOPS at them. That is the honest behaviour and it is not a downgrade: the
/// alternative, letting another model answer, is precisely how both earlier
/// autonomous loops failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plugins {
    /// Ask the owner (through the proxy) when the runner reaches something a
    /// machine must not settle. Off means the runner stops there instead.
    #[serde(default = "enabled")]
    pub owner: bool,
    /// Show what the code auditor has already found in this project.
    #[serde(default = "enabled")]
    pub auditor: bool,
    /// Show what this project has already been observed to solve.
    #[serde(default = "enabled")]
    pub recorder: bool,
    /// Show the owner's standing guard categories before the agent trips one.
    #[serde(default = "enabled")]
    pub guards: bool,
    /// Load the bundled expert skills.
    #[serde(default = "enabled")]
    pub skills: bool,
    /// Connect the configured MCP servers.
    #[serde(default = "enabled")]
    pub mcp: bool,
    /// Individual skills to leave out, by name.
    ///
    /// A removal list rather than a replacement list: an operator who wrote out
    /// the ones they wanted would silently lose every skill added afterwards,
    /// and would not find out until they needed one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_skills: Vec<String>,
    /// Individual MCP servers to leave out, by name. Same reasoning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_mcp: Vec<String>,
}

const fn enabled() -> bool {
    true
}

impl Default for Plugins {
    fn default() -> Self {
        Self {
            owner: true,
            auditor: true,
            recorder: true,
            guards: true,
            skills: true,
            mcp: true,
            disabled_skills: Vec::new(),
            disabled_mcp: Vec::new(),
        }
    }
}

impl Plugins {
    /// The ones switched off, for the report.
    #[must_use]
    pub fn disabled(&self) -> Vec<&'static str> {
        let mut off = Vec::new();
        if !self.owner {
            off.push("owner");
        }
        if !self.auditor {
            off.push("auditor");
        }
        if !self.recorder {
            off.push("recorder");
        }
        if !self.guards {
            off.push("guards");
        }
        if !self.skills {
            off.push("skills");
        }
        if !self.mcp {
            off.push("mcp");
        }
        off
    }

    /// Whether one named skill is loaded.
    #[must_use]
    pub fn skill_enabled(&self, name: &str) -> bool {
        self.skills
            && !self
                .disabled_skills
                .iter()
                .any(|d| d.eq_ignore_ascii_case(name))
    }

    /// Whether one named MCP server is connected.
    #[must_use]
    pub fn mcp_enabled(&self, name: &str) -> bool {
        self.mcp
            && !self
                .disabled_mcp
                .iter()
                .any(|d| d.eq_ignore_ascii_case(name))
    }
}

impl AdminConfig {
    #[must_use]
    pub fn path_in(data_dir: &Path) -> PathBuf {
        data_dir.join(ADMIN_FILE)
    }

    /// Where the settings live when nobody has said otherwise.
    ///
    /// One resolver, so a caller deep in the crate reads the same file the
    /// operator edited rather than a second one it computed for itself.
    #[must_use]
    pub fn default_dir() -> PathBuf {
        if let Some(explicit) = std::env::var_os(DATA_DIR_ENV) {
            return PathBuf::from(explicit);
        }
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map_or_else(|| PathBuf::from("."), PathBuf::from)
            .join(".incord-code")
    }

    /// Read the operator's settings from the default location.
    ///
    /// Falls back to empty settings when the file is unreadable: this exists for
    /// callers answering a different question, which have nowhere to report an
    /// error. The startup path uses `load` so a broken file is still announced.
    #[must_use]
    pub fn load_default() -> Self {
        Self::load(&Self::default_dir()).unwrap_or_default()
    }

    /// Read the operator's settings, or an empty set when there are none.
    ///
    /// A malformed file is an ERROR rather than a silent empty default: settings
    /// that quietly stop applying are worse than a harness that says the file is
    /// broken, because the run looks normal while doing something else.
    pub fn load(data_dir: &Path) -> Result<Self, String> {
        let path = Self::path_in(data_dir);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&text)
            .map_err(|e| format!("{} is not valid admin settings: {e}", path.display()))
    }

    pub fn save(&self, data_dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| format!("could not create {}: {e}", data_dir.display()))?;
        let path = Self::path_in(data_dir);
        let text =
            serde_json::to_string_pretty(self).map_err(|e| format!("could not encode: {e}"))?;
        std::fs::write(&path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
    }

    /// The session model, and where it was decided.
    ///
    /// `flag` is whatever the caller parsed from the command line, already
    /// distinguished from a default by the caller.
    #[must_use]
    pub fn resolve_model(&self, flag: Option<&str>) -> Resolved<String> {
        if let Some(model) = flag.map(str::trim).filter(|m| !m.is_empty()) {
            return Resolved::new(model.to_string(), Source::Flag);
        }
        if let Some(model) = std::env::var(crate::config::MODEL_ENV)
            .ok()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
        {
            return Resolved::new(model, Source::Environment);
        }
        if let Some(model) = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            return Resolved::new(model.to_string(), Source::AdminFile);
        }
        Resolved::new(crate::config::fallback_model(), Source::BuiltIn)
    }

    /// The `agent` line of the report, against what is actually installed.
    ///
    /// Split out so what it SAYS can be asserted without a machine that has any
    /// particular agent on it — the report itself needs a real PATH probe, and a
    /// claim about a pin that did not apply is exactly the thing worth testing.
    #[must_use]
    fn agent_line(&self) -> String {
        // DRIVABLE, not merely present, and from the same catalogue the lead
        // is chosen from — so an API agent chosen with `/agent` is reported as
        // the choice it is rather than as an agent that is not installed.
        let catalogue = crate::agent::catalogue::Catalogue::gather(&self.api_providers);
        let (drivable, present_only) = catalogue.ids_by_usability();
        Self::describe_agent(&self.resolve_agent(), &drivable, &present_only)
    }

    /// What the report says about the agent.
    ///
    /// `drivable` can actually take a turn; `present_only` was detected but has
    /// no verified protocol. Reported separately because they mean opposite
    /// things: one is a choice, the other explains why a choice is missing —
    /// "install something" and "that one is not supported yet" have completely
    /// different fixes.
    #[must_use]
    pub fn describe_agent(
        chosen: &Resolved<Option<String>>,
        drivable: &[String],
        present_only: &[String],
    ) -> String {
        let line = match &chosen.value {
            None if drivable.is_empty() => {
                "  agent          none drivable — nothing can be delegated to".to_string()
            }
            None => format!(
                "  agent          {}  <- no preference set, so whichever is drivable",
                drivable.join(", ")
            ),
            // A PIN THAT DID NOT APPLY IS SAID OUT LOUD. Silently falling back
            // to another agent is the failure this setting exists to end: the
            // operator would read the file, believe the choice took, and be
            // billed against a different subscription.
            Some(want) if !drivable.iter().any(|i| i == want) => {
                let why = if present_only.iter().any(|i| i == want) {
                    "FOUND BUT NOT DRIVABLE here - /agent says why".to_string()
                } else if drivable.is_empty() {
                    "NOT INSTALLED, and nothing else is drivable".to_string()
                } else {
                    format!("NOT INSTALLED, drivable: {}", drivable.join(", "))
                };
                format!(
                    "  agent          {want}  <- {} - {why}",
                    chosen.source.as_str()
                )
            }
            Some(want) => format!("  agent          {want}  <- {}", chosen.source.as_str()),
        };
        if present_only.is_empty() {
            return format!(
                "{line}
"
            );
        }
        // Worth saying whatever else is true: an operator staring at a short
        // drivable list needs to know the others WERE found.
        format!(
            "{line}
                 also found, not drivable: {}
",
            present_only.join(", ")
        )
    }

    /// Tier references no tier defines, with the role that names each one.
    ///
    /// A role whose `@tier` does not exist loses those candidates and falls back
    /// to the session model — which looks exactly like a role nobody configured.
    /// Returned so `--admin` can say otherwise, because the one thing an
    /// operator cannot debug is a typo that reads as an intentional absence.
    #[must_use]
    pub fn unknown_tiers(&self) -> Vec<(String, String)> {
        let mut found = Vec::new();
        for role in crate::roles::KNOWN_ROLES {
            let Some(models) = self.roles.get(role) else {
                continue;
            };
            let (_, unknown) = crate::roles::expand_tiers(models, &self.tiers);
            for reference in unknown {
                found.push(((*role).to_string(), reference));
            }
        }
        found
    }

    /// Which agent this session should use, and who decided.
    ///
    /// `None` means no preference — whichever agent is installed serves the
    /// work, which is what happened before this setting existed. The same
    /// precedence ladder as everything else: environment beats the file beats
    /// the built-in default of "no preference".
    #[must_use]
    pub fn resolve_agent(&self) -> Resolved<Option<String>> {
        if let Some(agent) = std::env::var(AGENT_ENV)
            .ok()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
        {
            return Resolved::new(Some(agent), Source::Environment);
        }
        if let Some(agent) = self
            .agent
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
        {
            return Resolved::new(Some(agent.to_string()), Source::AdminFile);
        }
        Resolved::new(None, Source::BuiltIn)
    }

    /// The fleet, and where it was decided.
    ///
    /// Environment assignments win per role rather than wholesale: an operator
    /// overriding one role for one shell should not lose the rest of the file.
    #[must_use]
    pub fn resolve_roles(&self) -> Resolved<crate::roles::RoleTable> {
        let from_env = crate::roles::RoleTable::from_env();
        let mut table = crate::roles::RoleTable::default();
        let mut used_file = false;
        let mut used_env = false;

        for role in crate::roles::KNOWN_ROLES {
            let from_env_role = from_env.candidates_for(role);
            if from_env_role.is_empty() {
                if let Some(models) = self.roles.get(role).filter(|m| !m.is_empty()) {
                    // `@top` becomes the models that tier holds. A role naming a
                    // tier nobody defined is reported by `unknown_tiers`, not
                    // silently emptied.
                    let (expanded, _) = crate::roles::expand_tiers(models, &self.tiers);
                    table = table.with_candidates(role, expanded);
                    used_file = true;
                }
            } else {
                let (expanded, _) = crate::roles::expand_tiers(from_env_role, &self.tiers);
                table = table.with_candidates(role, expanded);
                used_env = true;
            }
        }

        // The table's source is the strongest layer that contributed to it. Where
        // the layers are mixed, `role_source` is the per-role truth and this is
        // only a headline.
        let source = match (used_env, used_file) {
            (true, _) => Source::Environment,
            (false, true) => Source::AdminFile,
            (false, false) => Source::BuiltIn,
        };
        Resolved::new(table, source)
    }

    /// Which layer decided one role's candidates.
    ///
    /// The headline source on `resolve_roles` cannot answer this when an operator
    /// has overridden one role in the shell and left the rest to the file, which
    /// is the normal way this gets used.
    #[must_use]
    pub fn role_source(&self, role: &str) -> Source {
        if !crate::roles::RoleTable::from_env()
            .candidates_for(role)
            .is_empty()
        {
            Source::Environment
        } else if self.roles.get(role).is_some_and(|m| !m.is_empty()) {
            Source::AdminFile
        } else {
            Source::BuiltIn
        }
    }

    /// The token ceiling, and where it was decided.
    #[must_use]
    pub fn resolve_budget(&self) -> Resolved<crate::budget::Budget> {
        let from_env = crate::budget::Budget::from_env();
        if from_env.has_ceiling() {
            return Resolved::new(from_env, Source::Environment);
        }
        match self.budget_tokens.filter(|&c| c > 0) {
            Some(ceiling) => Resolved::new(
                crate::budget::Budget::with_ceiling(ceiling),
                Source::AdminFile,
            ),
            None => Resolved::new(crate::budget::Budget::default(), Source::BuiltIn),
        }
    }

    /// How tightly the OS confines a model's shell commands, and where the
    /// choice came from. The operator's setting wins; unset, it is tied to
    /// `permission_mode` — see [`crate::sandbox::SandboxMode::for_permission_mode`].
    /// An unrecognised setting falls back to the permission mode's default
    /// rather than running unconfined, and says which layer decided.
    #[must_use]
    pub fn resolve_sandbox(
        &self,
        permission_mode: crate::permissions::PermissionMode,
    ) -> Resolved<crate::sandbox::SandboxMode> {
        if let Ok(raw) = std::env::var(crate::sandbox::SANDBOX_ENV) {
            let name = raw.trim();
            if !name.is_empty() {
                if let Some(mode) = crate::sandbox::SandboxMode::parse(name) {
                    return Resolved::new(mode, Source::Environment);
                }
            }
        }
        match self.sandbox.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => match crate::sandbox::SandboxMode::parse(name) {
                Some(mode) => Resolved::new(mode, Source::AdminFile),
                None => Resolved::new(
                    crate::sandbox::SandboxMode::for_permission_mode(permission_mode),
                    Source::BuiltIn,
                ),
            },
            _ => Resolved::new(
                crate::sandbox::SandboxMode::for_permission_mode(permission_mode),
                Source::BuiltIn,
            ),
        }
    }

    /// Whether a sandboxed command may use the network, and where that was
    /// decided: the environment, then the settings, then `allow`. An
    /// unrecognised word is `allow` from the built-in layer — the same answer
    /// as unset, and reported as such rather than guessed at.
    #[must_use]
    pub fn resolve_sandbox_network(&self) -> Resolved<crate::sandbox::SandboxNetwork> {
        let named = |raw: &str| crate::sandbox::SandboxNetwork::parse(raw.trim());
        if let Some(network) = std::env::var(crate::sandbox::SANDBOX_NETWORK_ENV)
            .ok()
            .as_deref()
            .and_then(named)
        {
            return Resolved::new(network, Source::Environment);
        }
        match self.sandbox_network.as_deref().and_then(named) {
            Some(network) => Resolved::new(network, Source::AdminFile),
            None => Resolved::new(crate::sandbox::SandboxNetwork::Allow, Source::BuiltIn),
        }
    }

    /// The whole sandbox policy for a session in `permission_mode`: the mode,
    /// the network, and whether the operator named the mode (which Windows
    /// needs before it changes a workspace's ACL for workspace-write).
    #[must_use]
    pub fn sandbox_policy(
        &self,
        permission_mode: crate::permissions::PermissionMode,
    ) -> crate::sandbox::SandboxPolicy {
        let mode = self.resolve_sandbox(permission_mode);
        crate::sandbox::SandboxPolicy {
            mode: mode.value,
            network: self.resolve_sandbox_network().value,
            named: mode.source != Source::BuiltIn,
        }
    }

    /// The check that decides whether work is finished, and where it came from.
    #[must_use]
    pub fn resolve_verify(&self, workspace: &Path) -> Option<Resolved<Vec<String>>> {
        if let Ok(raw) = std::env::var(crate::verify::VERIFY_COMMAND_ENV) {
            let parts: Vec<String> = raw.split_whitespace().map(str::to_string).collect();
            if !parts.is_empty() {
                return Some(Resolved::new(parts, Source::Environment));
            }
        }
        if let Some(argv) = self.verify_command.as_ref().filter(|a| !a.is_empty()) {
            return Some(Resolved::new(argv.clone(), Source::AdminFile));
        }
        crate::verify::detected_command(workspace).map(|argv| Resolved::new(argv, Source::BuiltIn))
    }

    /// What the operator will actually get, laid out so a surprise is traceable.
    #[must_use]
    pub fn report(&self, workspace: &Path, flag_model: Option<&str>) -> String {
        let model = self.resolve_model(flag_model);
        let roles = self.resolve_roles();
        let budget = self.resolve_budget();

        let mut out = String::from("Admin settings (value <- where it was decided)\n");
        // Named first, because the next thing an operator does with this report
        // is edit the file it came from.
        out.push_str(&format!(
            "  settings file  {}\n",
            Self::path_in(&Self::default_dir()).display()
        ));
        out.push_str(&format!(
            "  model          {}  <- {}\n",
            model.value,
            model.source.as_str()
        ));
        out.push_str(&self.agent_line());

        if roles.value.is_empty() {
            out.push_str(
                "  roles          none assigned — delegated work runs on the session model\n",
            );
        } else {
            for role in roles.value.configured_roles() {
                out.push_str(&format!(
                    "  role {role:<9} {}  <- {}\n",
                    roles.value.candidates_for(role).join(" > "),
                    self.role_source(role).as_str()
                ));
            }
        }

        // The ladder itself, so "why did plan run on that model" is answerable
        // without opening the file.
        for (tier, models) in &self.tiers {
            out.push_str(&format!(
                "  tier {tier:<9} {}
",
                models.join(" > ")
            ));
        }
        // A REFERENCE THAT RESOLVED TO NOTHING IS NOT AN ABSENCE. Without this
        // the role simply loses its candidates and falls back to the session
        // model, which reads as "never configured" — and a typo is invisible.
        for (role, reference) in self.unknown_tiers() {
            out.push_str(&format!(
                "  !! {}\n",
                unknown_tier_warning(&role, &reference)
            ));
        }

        match budget.value.ceiling() {
            Some(ceiling) => out.push_str(&format!(
                "  budget         {ceiling} tokens  <- {}\n",
                budget.source.as_str()
            )),
            None => out.push_str("  budget         no ceiling — a run is unbounded\n"),
        }

        match self.resolve_verify(workspace) {
            Some(verify) => out.push_str(&format!(
                "  verify         {}  <- {}\n",
                verify.value.join(" "),
                verify.source.as_str()
            )),
            // WHY, not just THAT. "none" told an operator they had a problem
            // and nothing about which one, so the two cases — nothing here
            // names an ecosystem, versus two do and running one's tests would
            // not prove the other's code — read identically. They need
            // opposite responses, and `detection_note` already wrote both.
            None => out.push_str(&format!(
                "  verify         none — nothing can prove a task finished\n                 {}\n",
                crate::verify::detection_note(workspace)
            )),
        }

        // Every one of these is optional. Saying which are OFF is the point: a
        // harness quietly doing less looks exactly like one doing nothing.
        let off = self.plugins.disabled();
        if off.is_empty() {
            out.push_str(
                "  plugins        all on (owner, auditor, recorder, guards, skills, mcp)
",
            );
        } else {
            out.push_str(&format!(
                "  plugins        OFF: {}
",
                off.join(", ")
            ));
        }
        // Reported INDEPENDENTLY of the globals: an operator who removed three
        // skills while leaving skills on would otherwise see "all on", which is
        // the exact "silently doing less" this report exists to prevent.
        if !self.plugins.disabled_skills.is_empty() {
            out.push_str(&format!(
                "                 skills removed: {}
",
                self.plugins.disabled_skills.join(", ")
            ));
        }
        if !self.plugins.disabled_mcp.is_empty() {
            out.push_str(&format!(
                "                 mcp servers removed: {}
",
                self.plugins.disabled_mcp.join(", ")
            ));
        }
        if !self.plugins.owner {
            out.push_str(
                "                 with the owner channel off the runner STOPS at a decision it must not make, rather than asking
",
            );
        }

        if let Some(warning) = roles.value.review_independence_warning() {
            out.push_str(&format!("\n  ! {warning}\n"));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn clear_env() {
        std::env::remove_var(DATA_DIR_ENV);
        std::env::remove_var(crate::sandbox::SANDBOX_ENV);
        std::env::remove_var(crate::sandbox::SANDBOX_NETWORK_ENV);
        std::env::remove_var(AGENT_ENV);
        std::env::remove_var(crate::config::MODEL_ENV);
        std::env::remove_var(crate::budget::BUDGET_ENV);
        std::env::remove_var(crate::verify::VERIFY_COMMAND_ENV);
        for role in crate::roles::KNOWN_ROLES {
            std::env::remove_var(format!(
                "{}{}",
                crate::roles::ROLE_ENV_PREFIX,
                role.to_ascii_uppercase()
            ));
        }
    }

    /// **The ladder must reach the table that actually routes work.** Expanding
    /// correctly in isolation proves nothing if `resolve_roles` hands the raw
    /// `@top` string to the backend as though it were a model name.
    #[test]
    fn a_role_naming_a_tier_resolves_to_that_tiers_models() {
        let _guard = lock();
        clear_env();
        let config = AdminConfig {
            tiers: BTreeMap::from([(
                "top".to_string(),
                vec!["opus".to_string(), "other-agent:big".to_string()],
            )]),
            roles: BTreeMap::from([("plan".to_string(), vec!["@top".to_string()])]),
            ..AdminConfig::default()
        };
        assert_eq!(
            config.resolve_roles().value.candidates_for("plan"),
            ["opus".to_string(), "other-agent:big".to_string()],
            "the role must end up holding real models, not the reference"
        );
    }

    /// One ladder, many roles — the whole point. Adding a model to `top` must
    /// reach every role that names it, without editing those roles.
    #[test]
    fn one_ladder_serves_every_role_that_names_it() {
        let _guard = lock();
        clear_env();
        let config = AdminConfig {
            tiers: BTreeMap::from([("top".to_string(), vec!["opus".to_string()])]),
            roles: BTreeMap::from([
                ("plan".to_string(), vec!["@top".to_string()]),
                ("review".to_string(), vec!["@top".to_string()]),
                ("diagnose".to_string(), vec!["@top".to_string()]),
            ]),
            ..AdminConfig::default()
        };
        let table = config.resolve_roles().value;
        for role in ["plan", "review", "diagnose"] {
            assert_eq!(
                table.candidates_for(role),
                ["opus".to_string()],
                "`{role}` did not pick the ladder up"
            );
        }
    }

    /// **A dangling reference must be named.** It costs the role its candidates
    /// and falls back to the session model, which is indistinguishable from a
    /// role nobody configured — so the typo would never be found.
    #[test]
    fn a_role_naming_a_tier_that_does_not_exist_is_reported() {
        let config = AdminConfig {
            tiers: BTreeMap::from([("top".to_string(), vec!["opus".to_string()])]),
            roles: BTreeMap::from([("plan".to_string(), vec!["@tpo".to_string()])]),
            ..AdminConfig::default()
        };
        assert_eq!(
            config.unknown_tiers(),
            vec![("plan".to_string(), "@tpo".to_string())]
        );
        assert!(
            config
                .report(Path::new("."), None)
                .contains("no tier defines"),
            "the report must show it, or nothing does"
        );
    }

    /// The agent obeys the same ladder as everything else.
    #[test]
    fn the_agent_follows_the_precedence_ladder() {
        let _guard = lock();
        clear_env();
        let mut config = AdminConfig::default();

        assert_eq!(
            config.resolve_agent().value,
            None,
            "no preference by default"
        );
        assert_eq!(config.resolve_agent().source, Source::BuiltIn);

        config.agent = Some("from-file".to_string());
        let chosen = config.resolve_agent();
        assert_eq!(chosen.value.as_deref(), Some("from-file"));
        assert_eq!(chosen.source, Source::AdminFile);

        std::env::set_var(AGENT_ENV, "from-env");
        let chosen = config.resolve_agent();
        assert_eq!(
            chosen.value.as_deref(),
            Some("from-env"),
            "env beats the file"
        );
        assert_eq!(chosen.source, Source::Environment);
        clear_env();
    }

    /// **A pin that did not apply must be said out loud.** Falling back to
    /// another agent in silence is the whole failure this setting exists to
    /// end: the operator reads the file, believes the choice took, and is
    /// billed against a different subscription.
    #[test]
    fn an_agent_that_is_not_installed_is_reported_not_ignored() {
        let chosen = Resolved::new(Some("absent-agent".to_string()), Source::AdminFile);
        let line = AdminConfig::describe_agent(&chosen, &["present-agent".to_string()], &[]);
        assert!(line.contains("absent-agent"), "{line}");
        assert!(line.contains("NOT INSTALLED"), "{line}");
        assert!(
            line.contains("present-agent"),
            "it must name what IS there, or the operator cannot fix it: {line}"
        );
    }

    /// **Installed and drivable are different facts.** An agent found on the
    /// machine whose protocol has never been verified cannot take a turn, and
    /// saying "installed" about it sends the operator to fix the wrong thing.
    #[test]
    fn an_agent_that_is_installed_but_not_drivable_says_which() {
        let chosen = Resolved::new(Some("codex".to_string()), Source::AdminFile);
        let line = AdminConfig::describe_agent(
            &chosen,
            &["claude-code".to_string()],
            &["codex".to_string()],
        );
        assert!(
            line.contains("NOT DRIVABLE"),
            "it must not read as missing when it is present: {line}"
        );
        assert!(
            !line.contains("NOT INSTALLED"),
            "it IS installed — saying otherwise sends them to reinstall it: {line}"
        );
    }

    /// What was found but cannot be driven is still named, so a short drivable
    /// list is explained rather than just short.
    #[test]
    fn agents_that_were_found_but_cannot_be_driven_are_still_named() {
        let chosen = Resolved::new(None, Source::BuiltIn);
        let line = AdminConfig::describe_agent(
            &chosen,
            &["claude-code".to_string()],
            &["codex".to_string(), "gemini".to_string()],
        );
        assert!(line.contains("claude-code"), "{line}");
        assert!(
            line.contains("not drivable: codex, gemini"),
            "the others were found and that must be visible: {line}"
        );
    }

    /// A pin that DID apply reads plainly, with who decided it.
    #[test]
    fn an_installed_agent_is_reported_with_its_source() {
        let chosen = Resolved::new(Some("present-agent".to_string()), Source::Environment);
        let line = AdminConfig::describe_agent(&chosen, &["present-agent".to_string()], &[]);
        assert!(line.contains("present-agent"), "{line}");
        assert!(!line.contains("NOT INSTALLED"), "{line}");
        assert!(line.contains(Source::Environment.as_str()), "{line}");
    }

    /// No preference names what is installed, so the report answers "which
    /// agent will actually run this" rather than leaving it blank.
    #[test]
    fn no_preference_still_names_what_is_installed() {
        let chosen = Resolved::new(None, Source::BuiltIn);
        let line =
            AdminConfig::describe_agent(&chosen, &["one".to_string(), "two".to_string()], &[]);
        assert!(line.contains("one, two"), "{line}");

        let none = AdminConfig::describe_agent(&chosen, &[], &[]);
        assert!(
            none.contains("none drivable"),
            "an empty machine must say so: {none}"
        );
    }

    /// The whole precedence ladder, in one place: flag beats env beats file
    /// beats built-in.
    #[test]
    fn the_more_immediate_choice_wins() {
        let _guard = lock();
        clear_env();

        let admin = AdminConfig {
            model: Some("from-the-file".to_string()),
            ..Default::default()
        };

        let from_file = admin.resolve_model(None);
        assert_eq!(from_file.value, "from-the-file");
        assert_eq!(from_file.source, Source::AdminFile);

        std::env::set_var(crate::config::MODEL_ENV, "from-the-env");
        let from_env = admin.resolve_model(None);
        assert_eq!(from_env.value, "from-the-env");
        assert_eq!(from_env.source, Source::Environment);

        let from_flag = admin.resolve_model(Some("from-the-flag"));
        assert_eq!(from_flag.value, "from-the-flag");
        assert_eq!(from_flag.source, Source::Flag);

        clear_env();
        let nothing_set = AdminConfig::default().resolve_model(None);
        assert_eq!(nothing_set.source, Source::BuiltIn);
        assert!(!nothing_set.value.is_empty());
    }

    /// An operator overriding ONE role for one shell must not lose the rest of
    /// their standing fleet.
    #[test]
    fn an_environment_override_replaces_one_role_not_the_whole_fleet() {
        let _guard = lock();
        clear_env();

        let mut roles = BTreeMap::new();
        roles.insert("explore".to_string(), vec!["file-explorer".to_string()]);
        roles.insert("review".to_string(), vec!["file-reviewer".to_string()]);
        let admin = AdminConfig {
            roles,
            ..Default::default()
        };

        std::env::set_var(
            format!("{}EXPLORE", crate::roles::ROLE_ENV_PREFIX),
            "env-explorer",
        );
        let resolved = admin.resolve_roles();
        clear_env();

        assert_eq!(resolved.value.model_for("explore"), Some("env-explorer"));
        assert_eq!(
            resolved.value.model_for("review"),
            Some("file-reviewer"),
            "the role nobody overrode must survive"
        );
    }

    /// Settings that quietly stop applying are worse than a loud failure: the
    /// run looks normal while doing something else entirely.
    #[test]
    fn a_malformed_file_is_reported_rather_than_treated_as_no_settings() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(AdminConfig::path_in(dir.path()), "{ not json").expect("write");

        let error = AdminConfig::load(dir.path()).expect_err("must not be silently empty");
        assert!(error.contains("not valid admin settings"), "got: {error}");
    }

    #[test]
    fn a_missing_or_empty_file_behaves_as_no_opinion() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(
            AdminConfig::load(dir.path()).expect("missing is fine"),
            AdminConfig::default()
        );

        std::fs::write(AdminConfig::path_in(dir.path()), "   \n").expect("write");
        assert_eq!(
            AdminConfig::load(dir.path()).expect("empty is fine"),
            AdminConfig::default()
        );
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut roles = BTreeMap::new();
        roles.insert(
            "implement".to_string(),
            vec!["a@lab-a".to_string(), "b@lab-b".to_string()],
        );
        let admin = AdminConfig {
            model: Some("a-model".to_string()),
            // A pinned agent has to survive the file too, or the setting is one
            // an operator can write and never get back.
            agent: Some("an-agent".to_string()),
            // The sub-agent choice and the providers too — and a provider comes
            // back holding the variable's NAME, which is all it ever holds.
            sub_agent: Some("another-agent".to_string()),
            api_providers: vec![ApiProvider {
                name: "team".to_string(),
                url: "http://127.0.0.1:9".to_string(),
                key_env: "TEAM_KEY_VARIABLE".to_string(),
            }],
            // The search provider too, holding its key variable's NAME only.
            web_search: Some(SearchProvider {
                url: "https://search.invalid/q".to_string(),
                key_env: Some("SEARCH_KEY_VARIABLE".to_string()),
            }),
            // The browser and the PDF switch, which the owner flips by hand.
            browser_bin: Some("C:/tools/incord-browser.exe".to_string()),
            pdf_renderer: Some(PdfRenderer::IncordBrowser),
            sandbox: Some("read-only".to_string()),
            sandbox_network: Some("deny".to_string()),
            // The ladder has to survive the file too — a tier an operator
            // writes and cannot read back is a setting that does not exist.
            tiers: BTreeMap::from([(
                "top".to_string(),
                vec!["a-model".to_string(), "other:big".to_string()],
            )]),
            roles,
            budget_tokens: Some(50_000),
            verify_command: Some(vec!["cargo".to_string(), "test".to_string()]),
            memory_url: Some("http://127.0.0.1:9180".to_string()),
            // The engine the harness runs its own hooks through: a path the
            // operator chose must come back, or the default quietly replaces it.
            engine_bin: Some("C:\\engine\\incord-memory-service.exe".to_string()),
            plugins: Plugins::default(),
            // A guard on the desktop has to survive the file too. An operator
            // who writes one and gets it back empty has no guard and no way
            // to tell.
            desktop_hooks: crate::hooks::HookConfig {
                pre_tool_use: vec!["a-guard-command".to_string()],
                post_tool_use: vec!["a-logging-command".to_string()],
            },
        };
        admin.save(dir.path()).expect("saves");

        assert_eq!(
            AdminConfig::load(dir.path()).expect("loads"),
            admin,
            "what was written must be what comes back"
        );
    }

    /// The sandbox policy says whether the operator NAMED the mode (Windows
    /// changes an ACL for workspace-write only then) and resolves the network:
    /// unset is `allow`, a word nobody uses is `allow` from the built-in layer,
    /// and the environment wins over the file.
    #[test]
    fn the_sandbox_policy_says_whether_the_mode_was_named_and_resolves_the_network() {
        use crate::permissions::PermissionMode as P;
        use crate::sandbox::{SandboxMode, SandboxNetwork, SANDBOX_ENV, SANDBOX_NETWORK_ENV};
        let _guard = lock();
        clear_env();

        let unset = AdminConfig::default().sandbox_policy(P::WorkspaceWrite);
        assert_eq!(unset.mode, SandboxMode::WorkspaceWrite);
        assert!(!unset.named, "the permission-mode default is not named");
        assert_eq!(unset.network, SandboxNetwork::Allow);

        let admin = AdminConfig {
            sandbox: Some("workspace-write".to_string()),
            sandbox_network: Some("deny".to_string()),
            ..Default::default()
        };
        let named = admin.sandbox_policy(P::ReadOnly);
        assert_eq!(named.mode, SandboxMode::WorkspaceWrite);
        assert!(named.named);
        assert_eq!(named.network, SandboxNetwork::Deny);
        assert_eq!(admin.resolve_sandbox_network().source, Source::AdminFile);

        let typo = AdminConfig {
            sandbox_network: Some("block".to_string()),
            ..Default::default()
        };
        let resolved = typo.resolve_sandbox_network();
        assert_eq!(resolved.value, SandboxNetwork::Allow);
        assert_eq!(resolved.source, Source::BuiltIn);

        std::env::set_var(SANDBOX_NETWORK_ENV, "allow");
        std::env::set_var(SANDBOX_ENV, "read-only");
        let env = admin.sandbox_policy(P::WorkspaceWrite);
        assert_eq!(env.network, SandboxNetwork::Allow);
        assert_eq!(env.mode, SandboxMode::ReadOnly);
        assert!(env.named, "the environment names it too");
        assert_eq!(admin.resolve_sandbox_network().source, Source::Environment);
        clear_env();
    }

    /// **"NONE" IS NOT AN ANSWER AN OPERATOR CAN ACT ON.**
    ///
    /// Two workspaces get no check for opposite reasons: one names no
    /// ecosystem this harness knows, the other names two and running one's
    /// tests would not prove the other's code. The first wants a command
    /// written down; the second wants a choice made. The report said "none"
    /// for both, and `detection_note` had already written the difference and
    /// was called by nothing.
    #[test]
    fn a_report_with_no_check_says_which_kind_of_none_it_is() {
        let _guard = lock();
        clear_env();
        let admin = AdminConfig::default();

        // Nothing recognisable here.
        let empty = tempfile::tempdir().expect("temp dir");
        let bare = admin.report(empty.path(), None);
        assert!(bare.contains("nothing can prove"), "{bare}");
        assert!(
            bare.contains("names an ecosystem"),
            "the report does not say WHY there is no check: {bare}"
        );

        // Two ecosystems, which is a different problem with a different fix.
        let mixed = tempfile::tempdir().expect("temp dir");
        std::fs::write(mixed.path().join("Cargo.toml"), "[package]\n").expect("write");
        std::fs::write(mixed.path().join("package.json"), "{}").expect("write");
        let ambiguous = admin.report(mixed.path(), None);
        assert!(
            ambiguous.contains("2 ecosystems are present"),
            "the ambiguous case reads the same as the empty one: {ambiguous}"
        );
        assert!(
            ambiguous.contains("Rust") && ambiguous.contains("Node.js"),
            "the report must name them, or the operator cannot choose: {ambiguous}"
        );
    }

    /// The report is the answer to "what is this session going to do?", so every
    /// value must carry where it was decided.
    #[test]
    fn the_report_names_the_source_of_every_setting() {
        let _guard = lock();
        clear_env();
        let dir = tempfile::tempdir().expect("temp dir");

        let mut roles = BTreeMap::new();
        roles.insert("explore".to_string(), vec!["small-model".to_string()]);
        let admin = AdminConfig {
            model: Some("main-model".to_string()),
            roles,
            budget_tokens: Some(1234),
            verify_command: Some(vec!["just".to_string(), "check".to_string()]),
            ..Default::default()
        };

        let report = admin.report(dir.path(), None);
        assert!(report.contains("main-model"), "got: {report}");
        assert!(report.contains("small-model"), "got: {report}");
        assert!(report.contains("1234"), "got: {report}");
        assert!(report.contains("just check"), "got: {report}");
        assert!(
            report.matches("admin file").count() >= 3,
            "each value must say where it came from; got: {report}"
        );
    }

    /// A same-lineage reviewer must be visible in the control panel, not only at
    /// startup — this is the screen an operator looks at when setting the fleet.
    #[test]
    fn the_report_warns_when_the_reviewer_shares_the_writers_lineage() {
        let _guard = lock();
        clear_env();
        let dir = tempfile::tempdir().expect("temp dir");

        let mut roles = BTreeMap::new();
        roles.insert("implement".to_string(), vec!["w@lab-a".to_string()]);
        roles.insert("review".to_string(), vec!["r@lab-a".to_string()]);
        let admin = AdminConfig {
            roles,
            ..Default::default()
        };

        let report = admin.report(dir.path(), None);
        assert!(report.contains("same lineage"), "got: {report}");
    }

    /// No ceiling and no check are both real states an operator needs to see
    /// stated, not inferred from a blank line.
    #[test]
    fn absent_settings_are_stated_rather_than_left_blank() {
        let _guard = lock();
        clear_env();
        let dir = tempfile::tempdir().expect("temp dir");

        let report = AdminConfig::default().report(dir.path(), None);
        assert!(report.contains("no ceiling"), "got: {report}");
        assert!(report.contains("none assigned"), "got: {report}");
    }

    /// Provenance has to be per role, not per table: overriding one role in the
    /// shell is the normal way this gets used, and a headline saying "env" over
    /// a row that came from the file is a wrong answer to the only question the
    /// report exists to answer.
    #[test]
    fn each_role_reports_the_layer_that_actually_decided_it() {
        let _guard = lock();
        clear_env();
        let dir = tempfile::tempdir().expect("temp dir");

        let mut roles = BTreeMap::new();
        roles.insert("implement".to_string(), vec!["file-choice".to_string()]);
        roles.insert("review".to_string(), vec!["file-review".to_string()]);
        let admin = AdminConfig {
            roles,
            ..Default::default()
        };

        std::env::set_var(
            format!("{}REVIEW", crate::roles::ROLE_ENV_PREFIX),
            "shell-review",
        );
        let table = admin.resolve_roles().value;
        let implement_source = admin.role_source("implement");
        let review_source = admin.role_source("review");
        let report = admin.report(dir.path(), None);
        clear_env();

        assert_eq!(table.candidates_for("review"), ["shell-review".to_string()]);
        assert_eq!(
            table.candidates_for("implement"),
            ["file-choice".to_string()],
            "an override for one role must not discard the rest of the file"
        );
        assert_eq!(review_source, Source::Environment);
        assert_eq!(implement_source, Source::AdminFile);
        assert!(
            report.contains("file-choice") && report.contains("shell-review"),
            "got: {report}"
        );
        // Both layers named, each against its own row.
        assert!(report.contains("<- admin file"), "got: {report}");
        assert!(report.contains("<- env"), "got: {report}");
    }

    /// The file is only reachable if everything agrees where it lives.
    #[test]
    fn the_settings_are_read_back_from_where_they_were_written() {
        let _guard = lock();
        clear_env();
        let dir = tempfile::tempdir().expect("temp dir");

        let written = AdminConfig {
            model: Some("standing-choice".to_string()),
            budget_tokens: Some(4242),
            ..Default::default()
        };
        written.save(dir.path()).expect("save");

        std::env::set_var(DATA_DIR_ENV, dir.path());
        let loaded = AdminConfig::load_default();
        clear_env();

        assert_eq!(loaded, written);
        assert_eq!(loaded.resolve_model(None).value, "standing-choice");
        assert_eq!(loaded.resolve_budget().value.ceiling(), Some(4242));
    }

    /// **Every part of the body is a plugin.** The harness must run without any
    /// of them; what it loses is context, never the ability to work.
    #[test]
    fn all_of_the_body_can_be_switched_off() {
        let _guard = lock();
        clear_env();
        let dir = tempfile::tempdir().expect("temp dir");
        let admin = AdminConfig {
            plugins: Plugins {
                owner: false,
                auditor: false,
                recorder: false,
                guards: false,
                skills: false,
                mcp: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let report = admin.report(dir.path(), None);
        for expected in ["owner", "auditor", "recorder", "guards", "skills", "mcp"] {
            assert!(
                report.contains(expected),
                "`{expected}` must be listed: {report}"
            );
        }
        assert!(report.contains("OFF:"), "got: {report}");
    }

    /// **The owner channel is not like the others**, and the report says so.
    /// Turning it off does not move those decisions elsewhere — the runner stops
    /// at them, which is the honest behaviour and needs stating.
    #[test]
    fn switching_off_the_owner_channel_says_what_happens_instead() {
        let _guard = lock();
        clear_env();
        let dir = tempfile::tempdir().expect("temp dir");
        let admin = AdminConfig {
            plugins: Plugins {
                owner: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let report = admin.report(dir.path(), None);
        assert!(report.contains("STOPS"), "got: {report}");
        assert!(report.contains("rather than asking"), "got: {report}");
    }

    /// **Individual removals are reported even when the category is on.** An
    /// operator who dropped three skills and saw "all on" would be reading a
    /// report that lied by omission.
    #[test]
    fn removing_one_skill_is_visible_even_though_skills_are_on() {
        let _guard = lock();
        clear_env();
        let dir = tempfile::tempdir().expect("temp dir");
        let admin = AdminConfig {
            plugins: Plugins {
                disabled_skills: vec!["react-component-best-practices".to_string()],
                disabled_mcp: vec!["some-server".to_string()],
                ..Default::default()
            },
            ..Default::default()
        };
        let report = admin.report(dir.path(), None);
        assert!(
            report.contains("skills removed: react-component-best-practices"),
            "got: {report}"
        );
        assert!(
            report.contains("mcp servers removed: some-server"),
            "got: {report}"
        );
    }

    /// Removal is by name, case-insensitively, and one removal never affects
    /// another item.
    #[test]
    fn a_removal_takes_out_exactly_one_thing() {
        let plugins = Plugins {
            disabled_skills: vec!["Ponytail".to_string()],
            ..Default::default()
        };
        assert!(!plugins.skill_enabled("ponytail"), "case must not matter");
        assert!(plugins.skill_enabled("test-driven-developer"));
        assert!(
            plugins.mcp_enabled("anything"),
            "skills and mcp are separate"
        );
    }

    /// Switching a category off takes out everything in it, without needing the
    /// operator to list each one.
    #[test]
    fn switching_a_category_off_takes_out_everything_in_it() {
        let plugins = Plugins {
            skills: false,
            ..Default::default()
        };
        assert!(!plugins.skill_enabled("anything"));
        assert!(plugins.mcp_enabled("anything"));
    }

    /// Defaults are ON, because the body is normally there and a harness that
    /// ignored it would re-ask what it has already been told.
    #[test]
    fn everything_is_on_until_the_operator_says_otherwise() {
        let plugins = Plugins::default();
        assert!(plugins.disabled().is_empty());
        assert!(plugins.owner && plugins.auditor && plugins.recorder);
        assert!(plugins.guards && plugins.skills && plugins.mcp);
    }

    /// A settings file written before plugins existed must not read as "all off"
    /// — that would silently disable the whole body on upgrade.
    #[test]
    fn an_older_settings_file_keeps_everything_on() {
        let older = r#"{"model":"a-model"}"#;
        let admin: AdminConfig = serde_json::from_str(older).expect("parses");
        assert!(
            admin.plugins.disabled().is_empty(),
            "an upgrade must not switch the body off"
        );
    }
}
