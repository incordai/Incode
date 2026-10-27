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
pub const DATA_DIR_ENV: &str = "INCODE_DIR";

/// The home directory the data directory sits under when [`DATA_DIR_ENV`] is
/// not set: `HOME`, else `USERPROFILE`.
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Environment variable naming which installed agent serves this session.
///
/// The value is an agent ID as [`crate::agent::registry::discover`] reports it,
/// not a path — the harness finds the binary itself and never reads a
/// credential.
pub const AGENT_ENV: &str = "INCODE_AGENT";

/// Where a setting came from, so a surprising value can be traced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A flag typed for this run.
    Flag,
    /// An environment variable set for this shell.
    Environment,
    /// The harness folder this run was given (`INSI_HARNESS`), over the
    /// admin file for this run only.
    Harness,
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
            Self::Harness => "harness",
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
    /// Unset means the lead's `default_sub_agent` in
    /// [`crate::agent::registry::agent_table`], which today is the lead
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
    /// Models reached over an API with a key, each chosen by its own name —
    /// see [`crate::agent::models`]. Only the NAME of the variable holding a
    /// model's key is kept here, never the key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_models: Vec<ApiModel>,
    /// Agent CLIs: changes to the built-in ones, and more of them. Local
    /// agents are still FOUND, never listed by hand — an entry here only
    /// says what to look for and how to drive it. See
    /// [`crate::agent::registry::agent_table`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cli_agents: Vec<CliAgent>,
    /// The search service the `WebSearch` tool asks — see [`crate::web`] for
    /// the exact request and reply. Only the NAME of the variable holding
    /// its key is kept here, never the key. Unset, the tool says it is not
    /// configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<SearchProvider>,
    /// The headless browser's executable, for development: the
    /// `INCODE_BROWSER` variable wins over it, and it wins over an installed
    /// tool that provides `browser` (`tools`). None of the three, and the
    /// Screenshot tool is not offered and `WebFetch` runs no script — see
    /// `toolbox::browser`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_bin: Option<String>,
    /// External tools, each downloaded from a configured release when the
    /// owner asks (`/tools add <name>`, `incode --install-tools`), checked
    /// against its sha256, and started as an MCP server in every session —
    /// see [`crate::ext_tools`]. Exactly what the owner wrote: the built-in
    /// defaults join it only in [`crate::ext_tools::in_effect`], so a save
    /// never writes them into the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<crate::ext_tools::ToolEntry>,
    /// Whether the built-in tools (`defaults/tools.json`) are offered beside
    /// `tools`. Unset is yes; `false` offers only the owner's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools: Option<bool>,
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
    /// reports. `INCODE_SANDBOX_NETWORK` wins over it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_network: Option<String>,
    /// What confines the commands: `builtin` (the default) or `sbx`, the
    /// standalone sandbox — see [`crate::sandbox::sbx`].
    /// `INCODE_SANDBOX_ENGINE` wins over it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_engine: Option<String>,
    /// Under the sbx engine: the only hosts a command may reach, when the
    /// network is not denied. Empty leaves it open.
    /// `INCODE_SANDBOX_ALLOW_HOSTS` (comma-separated) wins over it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sandbox_allow_hosts: Vec<String>,
    /// Under the sbx engine: this machine's own ports a command may reach
    /// when the network is not denied (a dev server a browser test loads),
    /// on `localhost`, `127.0.0.1` or `::1` only; nothing else private opens.
    /// Like `sandbox_allow_hosts` it puts the network behind sbx's proxy.
    /// Set here only, by the owner: no variable overrides it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sandbox_loopback_ports: Vec<u16>,
    /// Under the sbx engine on Windows: `native`, `wsl` or `auto` (the
    /// default). `INCODE_SANDBOX_BACKEND` wins over it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_backend: Option<String>,
    /// The shell the `Bash` tool runs commands in on Windows, as a path or a
    /// program on `PATH` — see `tools::shell_choice`. `INCODE_SHELL` wins over
    /// it. Unset, Git for Windows' bash is used, then PowerShell; never
    /// cmd.exe, and never the WSL launcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Whose tools a codex model runs: `own` (the default — codex's, watched
    /// afterwards by the Sentinel) or `incode` (incode's toolbox through the
    /// tool bridge, codex's own switched off) — see
    /// [`crate::agent::codex::CodexTools`]. `INCODE_CODEX_TOOLS` wins over it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_tools: Option<String>,
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
    /// The generator the owner wants a new project scaffolded with, per stack:
    /// `{"rust": "cargo new --vcs none app", "web": "npm create vite@latest"}`.
    ///
    /// A PREFERENCE handed to the model when it writes `## Layout` for a
    /// project with no code yet — see [`crate::scaffold`]. incode carries no
    /// templates of its own and runs nothing from this map itself; the chosen
    /// command is run by the scaffold roadmap item like any other work, and its
    /// build proves it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scaffold: BTreeMap<String, String>,
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

    /// Linux: the X display computer use drives. `":N"` attaches to a display
    /// that is already running; `"xvfb"` (or `"xvfb:WIDTHxHEIGHT"`) starts a
    /// private one for the desktop server's lifetime and stops it after.
    ///
    /// Unset is a refusal, not a fallback to `$DISPLAY`: an agent that quietly
    /// lands on the operator's own desktop is the failure the isolation
    /// decision exists to prevent (`docs/COMPUTER_USE.md` 7e).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_display: Option<String>,

    /// The desktop's limits — rate cap, heartbeat, tree size, journal size,
    /// timeouts. Unset fields are the defaults; the two safety limits may
    /// only be made stricter. See [`crate::desktop::limits`].
    #[serde(
        default,
        skip_serializing_if = "crate::desktop::limits::DesktopLimits::is_empty"
    )]
    pub desktop_limits: crate::desktop::limits::DesktopLimits,

    /// How the REPL says a long turn finished or that it is waiting on you:
    /// `auto` (the default: OSC 9 where the terminal is known to show it, a
    /// bell otherwise), `bell`, `desktop`, or `off`. See [`crate::cli::notify`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<String>,
    /// A shell command run on each notification, with `INCODE_EVENT`
    /// (`done` | `input`) and `INCODE_MESSAGE` in its environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_command: Option<String>,
    /// How long a turn must run before its end is worth a notification.
    /// Default 10 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_after_secs: Option<u64>,
    /// A shell command whose first line of output replaces the facts on the
    /// input box's rule. It reads the session as JSON on stdin. See
    /// [`crate::cli::statusline`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statusline_command: Option<String>,
    /// `emacs` (the default) or `vi`: how the input box edits. `/vim`
    /// toggles it for the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editing_mode: Option<String>,
    /// The output style every session starts with: a file in
    /// `.incode/styles/` or `~/.incode/styles/` ([`crate::styles`]).
    /// `INCODE_OUTPUT_STYLE` overrides it; `/style` switches it per session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_style: Option<String>,
    /// Per-command permission rules — `allow`, `ask`, `deny` lists of `Tool`
    /// or `Tool(specifier)` — applied to every tool call the toolbox runs.
    /// Unioned with the managed and project rules; deny beats ask beats allow.
    /// See [`crate::permissions::rules`].
    #[serde(
        default,
        skip_serializing_if = "crate::permissions::rules::RuleLists::is_empty"
    )]
    pub permissions: crate::permissions::rules::RuleLists,
    /// Export usage to an OpenTelemetry collector ([`crate::telemetry`]):
    /// `true` turns it on (as `INCODE_TELEMETRY=1` does), given an
    /// `OTEL_EXPORTER_OTLP_ENDPOINT`. Off when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<bool>,
}

mod entries;
pub use entries::{ApiModel, ApiProvider, CliAgent, PdfRenderer, Plugins, SearchProvider};

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
        home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(crate::migrate::GLOBAL_DIR)
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
        let text = crate::text::read_authored(&path)
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
        let catalogue = crate::agent::catalogue::Catalogue::gather(self);
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
        let harness = crate::harness::routing();
        let (roles, tiers) = crate::harness::overlay(&self.roles, &self.tiers, harness.as_ref());
        let mut table = crate::roles::RoleTable::default();
        let mut used_file = false;
        let mut used_harness = false;
        let mut used_env = false;

        for role in crate::roles::KNOWN_ROLES {
            let from_env_role = from_env.candidates_for(role);
            if from_env_role.is_empty() {
                if let Some(models) = roles.get(role).filter(|m| !m.is_empty()) {
                    // `@top` becomes the models that tier holds. A role naming a
                    // tier nobody defined is reported by `unknown_tiers`, not
                    // silently emptied.
                    let (expanded, _) = crate::roles::expand_tiers(models, &tiers);
                    table = table.with_candidates(role, expanded);
                    if harness.as_ref().is_some_and(|h| h.roles.contains_key(role)) {
                        used_harness = true;
                    } else {
                        used_file = true;
                    }
                }
            } else {
                let (expanded, _) = crate::roles::expand_tiers(from_env_role, &tiers);
                table = table.with_candidates(role, expanded);
                used_env = true;
            }
        }

        // The table's source is the strongest layer that contributed to it. Where
        // the layers are mixed, `role_source` is the per-role truth and this is
        // only a headline.
        let source = match (used_env, used_harness, used_file) {
            (true, ..) => Source::Environment,
            (false, true, _) => Source::Harness,
            (false, false, true) => Source::AdminFile,
            (false, false, false) => Source::BuiltIn,
        };
        Resolved::new(table, source)
    }

    /// `roles` and `tiers` as this run uses them: the admin file's, with the
    /// harness's over them key by key when the run was given one. Never
    /// saved: the harness is this run's, the file is the operator's.
    #[must_use]
    pub fn effective_routing(
        &self,
    ) -> (BTreeMap<String, Vec<String>>, BTreeMap<String, Vec<String>>) {
        crate::harness::overlay(&self.roles, &self.tiers, crate::harness::routing().as_ref())
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
        } else if crate::harness::routing()
            .is_some_and(|h| h.roles.get(role).is_some_and(|m| !m.is_empty()))
        {
            Source::Harness
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
            engine: self.resolve_sandbox_engine().value,
        }
    }

    /// What confines the commands, and where that was decided: the
    /// environment, then the settings, then `builtin`.
    #[must_use]
    pub fn resolve_sandbox_engine(&self) -> Resolved<crate::sandbox::SandboxEngine> {
        let named = |raw: &str| crate::sandbox::SandboxEngine::parse(raw.trim());
        if let Some(engine) = std::env::var(crate::sandbox::SANDBOX_ENGINE_ENV)
            .ok()
            .as_deref()
            .and_then(named)
        {
            return Resolved::new(engine, Source::Environment);
        }
        match self.sandbox_engine.as_deref().and_then(named) {
            Some(engine) => Resolved::new(engine, Source::AdminFile),
            None => Resolved::new(crate::sandbox::SandboxEngine::Builtin, Source::BuiltIn),
        }
    }

    /// Whose tools a codex model runs, and where that was decided: the
    /// environment, then the settings, then `incode` ([`CodexTools`]'s
    /// default, so the two cannot disagree). A word nobody uses is the
    /// built-in default and is reported as such rather than guessed at.
    ///
    /// [`CodexTools`]: crate::agent::codex::CodexTools
    #[must_use]
    pub fn resolve_codex_tools(&self) -> Resolved<crate::agent::codex::CodexTools> {
        self.resolve_codex_tools_with(
            std::env::var(crate::agent::codex::CODEX_TOOLS_ENV)
                .ok()
                .as_deref(),
        )
    }

    /// [`Self::resolve_codex_tools`] with the environment's value passed in,
    /// so a test drives every layer without setting a variable that the
    /// backend tests running beside it read.
    fn resolve_codex_tools_with(
        &self,
        env: Option<&str>,
    ) -> Resolved<crate::agent::codex::CodexTools> {
        use crate::agent::codex::CodexTools;
        if let Some(tools) = env.and_then(CodexTools::parse) {
            return Resolved::new(tools, Source::Environment);
        }
        match self.codex_tools.as_deref().and_then(CodexTools::parse) {
            Some(tools) => Resolved::new(tools, Source::AdminFile),
            None => Resolved::new(CodexTools::default(), Source::BuiltIn),
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
        out.push_str(&self.roles_report(&roles));
        out.push_str(&self.budget_and_verify_report(workspace));

        let codex_tools = self.resolve_codex_tools();
        out.push_str(&format!(
            "  codex tools    {}  <- {}\n",
            codex_tools.value.as_str(),
            codex_tools.source.as_str()
        ));
        out.push_str(&format!(
            "  telemetry      {}\n",
            crate::telemetry::describe(self.telemetry)
        ));
        out.push_str(&self.output_style_line(workspace));
        out.push_str(&self.plugins_report());

        if let Some(warning) = roles.value.review_independence_warning() {
            out.push_str(&format!("\n  ! {warning}\n"));
        }
        out
    }

    /// The roles and the tier ladder they draw on, for [`Self::report`], with
    /// a warning per role that names a tier nobody defined.
    fn roles_report(&self, roles: &Resolved<crate::roles::RoleTable>) -> String {
        let mut out = String::new();
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
            out.push_str(&format!("  tier {tier:<9} {}\n", models.join(" > ")));
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
        out
    }

    /// The token budget and the verify command, for [`Self::report`].
    fn budget_and_verify_report(&self, workspace: &Path) -> String {
        let budget = self.resolve_budget();
        let mut out = match budget.value.ceiling() {
            Some(ceiling) => format!(
                "  budget         {ceiling} tokens  <- {}\n",
                budget.source.as_str()
            ),
            None => "  budget         no ceiling — a run is unbounded\n".to_string(),
        };
        out.push_str(&match self.resolve_verify(workspace) {
            Some(verify) => format!(
                "  verify         {}  <- {}\n",
                verify.value.join(" "),
                verify.source.as_str()
            ),
            // WHY, not just THAT. "none" told an operator they had a problem
            // and nothing about which one, so the two cases — nothing here
            // names an ecosystem, versus two do and running one's tests would
            // not prove the other's code — read identically. They need
            // opposite responses, and `detection_note` already wrote both.
            None => format!(
                "  verify         none — nothing can prove a task finished\n                 {}\n",
                crate::verify::detection_note(workspace)
            ),
        });
        out
    }

    /// The output style in force and the file it comes from, for
    /// [`Self::report`].
    fn output_style_line(&self, workspace: &Path) -> String {
        let env_style = std::env::var(crate::styles::ENV).ok();
        match crate::styles::chosen(env_style.as_deref(), self.output_style.as_deref()) {
            Some(style) => format!(
                "  output style   {}  <- {}{}\n",
                style.value,
                style.source.as_str(),
                match crate::styles::load(workspace).find(&style.value) {
                    Some(found) => format!("  ({})", found.source.display()),
                    None => "  (no such file: none is applied)".to_string(),
                }
            ),
            None => "  output style   none\n".to_string(),
        }
    }

    /// Which plugins are off, for [`Self::report`]. Every one of these is
    /// optional; saying which are OFF is the point, since a harness quietly
    /// doing less looks exactly like one doing nothing.
    fn plugins_report(&self) -> String {
        let off = self.plugins.disabled();
        let mut out = if off.is_empty() {
            "  plugins        all on (owner, auditor, recorder, guards, skills, mcp)\n".to_string()
        } else {
            format!("  plugins        OFF: {}\n", off.join(", "))
        };
        // Reported INDEPENDENTLY of the globals: an operator who removed three
        // skills while leaving skills on would otherwise see "all on", which is
        // the exact "silently doing less" this report exists to prevent.
        if !self.plugins.disabled_skills.is_empty() {
            out.push_str(&format!(
                "                 skills removed: {}\n",
                self.plugins.disabled_skills.join(", ")
            ));
        }
        if !self.plugins.disabled_mcp.is_empty() {
            out.push_str(&format!(
                "                 mcp servers removed: {}\n",
                self.plugins.disabled_mcp.join(", ")
            ));
        }
        if !self.plugins.owner {
            out.push_str(
                "                 with the owner channel off the runner STOPS at a decision it must \
                 not make, rather than asking\n",
            );
        }
        out
    }
}

#[cfg(test)]
mod tests;
