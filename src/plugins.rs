//! Plugins: one folder that bundles commands, skills, hooks and MCP servers.
//!
//! # Where they live
//!
//! `~/.incode/plugins/<name>/` for the operator and `.incode/plugins/<name>/`
//! for a project; the project's copy wins when both have the same name. Each
//! holds a `plugin.toml`:
//!
//! ```toml
//! name = "frontend"
//! version = "0.1.0"
//! description = "Commands and skills for the web app"
//! ```
//!
//! and any of these, each handed to the loader that already exists for it
//! rather than to a second one kept here:
//!
//! - `commands/**/*.md` — slash commands, namespaced `/<plugin>:<cmd>`
//!   ([`crate::custom_commands`]).
//! - `skills/<skill>/SKILL.md` — installed skills, listed as `<plugin>:<skill>`
//!   beside the store's own ([`crate::skills::load`]).
//! - `hooks.json` — `{"pre_tool_use": [...], "post_tool_use": [...]}`, the
//!   shape of `admin.json` `hooks`, added to the operator's own
//!   ([`crate::hooks::HookRunner::from_settings`]).
//! - `.mcp.json` (or `mcp.json`) — `{"mcpServers": {"<name>": {"command",
//!   "args", "env"} | {"type": "http", "url", "headers"}}}`, started beside
//!   the external tools' servers.
//!
//! `${INCODE_PLUGIN_ROOT}` in a hook command or a server's command, arguments
//! or environment is replaced with the plugin's folder.
//!
//! # Whose words these are
//!
//! A plugin is somebody else's bundle that the operator chose to install, so
//! its hooks and servers run with the operator's authority. `incode plugin add`
//! is the only thing that puts one there, and it says what it installed.
//!
//! # Updating
//!
//! `add` records where a plugin came from in its folder's [`SOURCE`] file —
//! `git = "<url>"` or `path = "<folder>"`. `incode plugin update` fetches it
//! again the same way (a fresh `--depth 1` clone, or a fresh copy) into a
//! staging folder beside it and swaps the two, so a failed fetch leaves the
//! installed copy as it was. A fetch whose manifest names a different plugin
//! is refused: the name is what its commands, skills and servers are filed
//! under.

use std::path::{Path, PathBuf};

/// The folder plugins live in, under `~/.incode/` or a project's `.incode/`.
pub const PLUGINS_DIR: &str = "plugins";

/// Every plugin's manifest.
pub const MANIFEST: &str = "plugin.toml";

/// Where an installed plugin came from, in its folder.
pub const SOURCE: &str = ".incode-source";

/// What a plugin's root folder is called in its own paths.
pub const ROOT_VAR: &str = "${INCODE_PLUGIN_ROOT}";

/// Which folder a plugin came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `.incode/plugins/` in the workspace.
    Project,
    /// `~/.incode/plugins/`.
    User,
}

impl Scope {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

/// What `plugin.toml` says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub description: String,
}

/// One installed plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    pub manifest: Manifest,
    pub root: PathBuf,
    pub scope: Scope,
}

/// Every plugin found, and what could not be read.
#[derive(Debug, Clone, Default)]
pub struct Found {
    pub plugins: Vec<Plugin>,
    pub problems: Vec<String>,
}

/// The operator's plugin folder.
#[must_use]
pub fn user_dir() -> PathBuf {
    crate::admin::AdminConfig::default_dir().join(PLUGINS_DIR)
}

/// A project's plugin folder.
#[must_use]
pub fn project_dir(workspace: &Path) -> PathBuf {
    workspace
        .join(crate::migrate::PROJECT_DIR)
        .join(PLUGINS_DIR)
}

/// The plugins for this workspace and this operator.
#[must_use]
pub fn discover(workspace: &Path) -> Found {
    discover_in(Some(&project_dir(workspace)), &user_dir())
}

/// The plugins of `workspace` that may be USED: the operator's own, and a
/// project's only as approved (`crate::plugin_approvals::usable`). Each one
/// left out is a problem line saying how to approve it.
///
/// Every consumer of a plugin's content goes through this: `session_hooks`,
/// `session_servers`, `session_skills` and `custom_commands::Registry::load`
/// (whose warnings are where the left-out lines are shown, once).
#[must_use]
pub fn discover_usable(workspace: &Path) -> Found {
    let found = discover(workspace);
    let (plugins, left_out) =
        crate::plugin_approvals::usable(&crate::admin::AdminConfig::default_dir(), found.plugins);
    Found {
        plugins,
        problems: found.problems.into_iter().chain(left_out).collect(),
    }
}

/// [`discover_usable`] for the process's own directory.
fn usable_here() -> Vec<Plugin> {
    match std::env::current_dir() {
        Ok(cwd) => discover_usable(&cwd).plugins,
        Err(_) => discover_in(None, &user_dir()).plugins,
    }
}

/// The plugins in these two folders, the project's first. A user plugin with
/// the same name as a project one is left out, and said to be.
#[must_use]
pub fn discover_in(project: Option<&Path>, user: &Path) -> Found {
    let mut found = Found::default();
    let mut scan = |dir: &Path, scope: Scope| {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter(|p| {
                !p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
            })
            .collect();
        dirs.sort();
        for root in dirs {
            match load_one(&root, scope) {
                Ok(plugin) => {
                    if let Some(kept) = found
                        .plugins
                        .iter()
                        .find(|p| p.manifest.name == plugin.manifest.name)
                    {
                        found.problems.push(format!(
                            "plugin `{}` in {} is shadowed by the {} one in {}",
                            plugin.manifest.name,
                            root.display(),
                            kept.scope.as_str(),
                            kept.root.display()
                        ));
                    } else {
                        found.plugins.push(plugin);
                    }
                }
                Err(why) => found.problems.push(why),
            }
        }
    };
    if let Some(project) = project {
        scan(project, Scope::Project);
    }
    scan(user, Scope::User);
    found
}

/// One plugin folder.
fn load_one(root: &Path, scope: Scope) -> Result<Plugin, String> {
    let path = root.join(MANIFEST);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "plugin at {} has no readable {MANIFEST}: {e}",
            root.display()
        )
    })?;
    let manifest = parse_manifest(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Plugin {
        manifest,
        root: root.to_path_buf(),
        scope,
    })
}

/// `plugin.toml`: top-level `key = "value"` lines. Tables and other keys are
/// ignored, so a manifest can carry more than incode reads.
pub fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let mut manifest = Manifest::default();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            // Everything past the first table belongs to that table.
            break;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("not a `key = value` line: `{line}`"));
        };
        let value = toml_string(value.trim())?;
        match key.trim() {
            "name" => manifest.name = value,
            "version" => manifest.version = value,
            "description" => manifest.description = value,
            _ => {}
        }
    }
    if manifest.name.is_empty() {
        return Err("`name` is missing".to_string());
    }
    check_name(&manifest.name)?;
    Ok(manifest)
}

/// A TOML basic or literal string, with a trailing `# comment` allowed.
fn toml_string(value: &str) -> Result<String, String> {
    let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'');
    let Some(quote) = quote else {
        return Err(format!("expected a quoted string, got `{value}`"));
    };
    let mut out = String::new();
    let mut chars = value[1..].chars();
    while let Some(c) = chars.next() {
        if c == quote {
            let rest = chars.as_str().trim();
            if !rest.is_empty() && !rest.starts_with('#') {
                return Err(format!("unexpected `{rest}` after a string"));
            }
            return Ok(out);
        }
        if c == '\\' && quote == '"' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => break,
            }
            continue;
        }
        out.push(c);
    }
    Err(format!("unterminated string `{value}`"))
}

/// A plugin name is a namespace in `/<plugin>:<cmd>`, a folder name and part
/// of a tool name, so it is kept to what all three can carry.
pub fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "plugin name `{name}` must be 1-40 letters, digits, `-` or `_`"
        ))
    }
}

impl Plugin {
    /// Where its slash commands are.
    #[must_use]
    pub fn commands_dir(&self) -> PathBuf {
        self.root.join("commands")
    }

    /// Its skills, as installed skills: a name and a folder, the body read
    /// only when asked for (see [`crate::skills::body_of`]).
    #[must_use]
    pub fn skills(&self) -> Vec<crate::skills::Skill> {
        let Ok(entries) = std::fs::read_dir(self.root.join("skills")) else {
            return Vec::new();
        };
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.join("SKILL.md").is_file())
            .collect();
        dirs.sort();
        dirs.into_iter()
            .map(|dir| {
                let folder = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let text = std::fs::read_to_string(dir.join("SKILL.md")).unwrap_or_default();
                let description = crate::subagents::split_front_matter(&text)
                    .and_then(|(front, _)| crate::subagents::parse_fields(front).ok())
                    .and_then(|fields| {
                        fields.into_iter().find_map(|(k, v)| match v {
                            crate::subagents::Field::One(s) if k == "description" => Some(s),
                            _ => None,
                        })
                    })
                    .unwrap_or_default();
                crate::skills::Skill {
                    name: format!("{}:{folder}", self.manifest.name),
                    description,
                    body: String::new(),
                    path: dir.to_string_lossy().into_owned(),
                }
            })
            .collect()
    }

    /// Its hook commands, with [`ROOT_VAR`] filled in.
    pub fn hooks(&self) -> Result<crate::hooks::HookConfig, String> {
        let path = self.root.join("hooks.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(crate::hooks::HookConfig::default());
        };
        let mut config: crate::hooks::HookConfig =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        for command in config
            .pre_tool_use
            .iter_mut()
            .chain(config.post_tool_use.iter_mut())
        {
            *command = self.fill(command);
        }
        Ok(config)
    }

    /// Its MCP servers, named `<plugin>_<server>`.
    pub(crate) fn servers(&self) -> Result<Vec<crate::toolbox::ServerSpec>, String> {
        let Some(path) = [".mcp.json", "mcp.json"]
            .iter()
            .map(|f| self.root.join(f))
            .find(|p| p.is_file())
        else {
            return Ok(Vec::new());
        };
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let Some(servers) = value
            .get("mcpServers")
            .or_else(|| value.get("servers"))
            .and_then(serde_json::Value::as_object)
        else {
            return Ok(Vec::new());
        };
        let pairs = |v: Option<&serde_json::Value>| -> Vec<(String, String)> {
            v.and_then(serde_json::Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), self.fill(s))))
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut specs = Vec::new();
        for (name, def) in servers {
            let full = format!("{}_{name}", self.manifest.name);
            let transport = if let Some(url) = def.get("url").and_then(serde_json::Value::as_str) {
                crate::toolbox::Transport::Http {
                    url: self.fill(url),
                    headers: pairs(def.get("headers")),
                    oauth: None,
                }
            } else if let Some(command) = def.get("command").and_then(serde_json::Value::as_str) {
                crate::toolbox::Transport::Stdio {
                    command: PathBuf::from(self.fill(command)),
                    args: def
                        .get("args")
                        .and_then(serde_json::Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(serde_json::Value::as_str)
                                .map(|s| self.fill(s))
                                .collect()
                        })
                        .unwrap_or_default(),
                    env: pairs(def.get("env")),
                }
            } else {
                return Err(format!(
                    "{}: server `{name}` has neither `command` nor `url`",
                    path.display()
                ));
            };
            specs.push(crate::toolbox::ServerSpec {
                name: full,
                transport,
            });
        }
        Ok(specs)
    }

    fn fill(&self, text: &str) -> String {
        text.replace(ROOT_VAR, &self.root.to_string_lossy())
    }
}

/// Every plugin's skills, for the process's own directory.
#[must_use]
pub fn session_skills() -> Vec<crate::skills::Skill> {
    usable_here().iter().flat_map(Plugin::skills).collect()
}

/// Every plugin's hook commands, for the process's own directory. A plugin
/// whose `hooks.json` does not parse contributes none, and is traced.
#[must_use]
pub fn session_hooks() -> crate::hooks::HookConfig {
    hooks_of(&usable_here())
}

/// The hook commands of `plugins`, merged in order.
fn hooks_of(plugins: &[Plugin]) -> crate::hooks::HookConfig {
    let mut all = crate::hooks::HookConfig::default();
    for plugin in plugins {
        match plugin.hooks() {
            // Every event, not a list of them: a field added to `HookConfig`
            // reaches plugins without this needing to know.
            //
            // Except `user_prompt_submit` from a PROJECT plugin: that folder
            // arrives with whatever was cloned, and this event runs on the
            // first line typed, before any tool. It is taken from admin.json
            // and the operator's own plugins only.
            Ok(mut config) => {
                if plugin.scope == Scope::Project && !config.user_prompt_submit.is_empty() {
                    crate::trace!(
                        "plugins",
                        "{}: user_prompt_submit is not run from a project's plugin",
                        plugin.root.display()
                    );
                    config.user_prompt_submit.clear();
                }
                all.extend(config);
            }
            Err(why) => crate::trace!("plugins", "{why}"),
        }
    }
    all
}

/// Every plugin's MCP servers for `workspace`, and a line for each that could
/// not be read.
#[must_use]
pub(crate) fn session_servers(workspace: &Path) -> (Vec<crate::toolbox::ServerSpec>, Vec<String>) {
    let mut specs = Vec::new();
    let mut said = Vec::new();
    // Unapproved project plugins are left out here and SAID by
    // `custom_commands::Registry::load`, so the line appears once.
    for plugin in discover_usable(workspace).plugins {
        match plugin.servers() {
            Ok(found) => specs.extend(found),
            Err(why) => said.push(format!("plugin `{}`: {why}", plugin.manifest.name)),
        }
    }
    (specs, said)
}

// ---------------------------------------------------------------------------
// `incode plugin list | add | update | remove`
// ---------------------------------------------------------------------------

pub const USAGE: &str = "\
USAGE:
    incode plugin list
    incode plugin add <git-url | path> [--project]
    incode plugin update [name] [--project]
    incode plugin remove <name> [--project]
    incode plugin approve <name>

`approve` lets a plugin in this workspace's .incode/plugins/ be used, as its
files are now: until then its hooks, servers, skills and commands are left
out, and any change to it needs approving again. Yours in ~/.incode/plugins/
need no approval.

`update` fetches a plugin again from where it was added: a fresh clone of
its git URL, or a fresh copy of its folder. With no name, every plugin in
that folder is updated.

Plugins are installed in ~/.incode/plugins/, or with --project in this
workspace's .incode/plugins/.";

/// Run `incode plugin <args>` in `workspace`. Returns what to print.
pub fn cli(args: &[String], workspace: &Path) -> Result<String, String> {
    let project = args.iter().any(|a| a == "--project");
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--project")
        .collect();
    let target = if project {
        project_dir(workspace)
    } else {
        user_dir()
    };
    match words.as_slice() {
        ["list"] | ["ls"] | [] => Ok(listing(&discover(workspace))),
        ["add", source] => add(source, &target).map(|plugin| {
            format!(
                "installed plugin `{}` {} in {}\n{}",
                plugin.manifest.name,
                plugin.manifest.version,
                plugin.root.display(),
                contents(&plugin)
            )
        }),
        ["update"] => update_all(&target),
        ["update", name] => update(name, &target).map(|done| done.to_string()),
        ["remove", name] | ["rm", name] => {
            remove(name, &target).map(|path| format!("removed {}", path.display()))
        }
        ["approve", name] => approve_named(
            name,
            &discover(workspace),
            &crate::admin::AdminConfig::default_dir(),
        ),
        ["-h"] | ["--help"] | ["help"] => Ok(USAGE.to_string()),
        _ => Err(USAGE.to_string()),
    }
}

/// `incode plugin approve <name>`: approve the project's plugin `name` as its
/// files are now (`crate::plugin_approvals`).
fn approve_named(name: &str, found: &Found, data_dir: &Path) -> Result<String, String> {
    let Some(plugin) = found.plugins.iter().find(|p| p.manifest.name == name) else {
        return Err(format!(
            "no plugin `{name}` here; `incode plugin list` lists them"
        ));
    };
    if plugin.scope == Scope::User {
        return Ok(format!(
            "`{name}` is yours ({}): it needs no approval",
            plugin.root.display()
        ));
    }
    crate::plugin_approvals::approve(data_dir, plugin)?;
    Ok(format!(
        "approved `{name}` as its files are now ({}); a change to it needs approving again\n{}",
        plugin.root.display(),
        contents(plugin)
    ))
}

/// What `incode plugin list` prints.
#[must_use]
pub fn listing(found: &Found) -> String {
    let mut out = String::new();
    if found.plugins.is_empty() {
        out.push_str("No plugins installed.\n");
    }
    let data_dir = crate::admin::AdminConfig::default_dir();
    for plugin in &found.plugins {
        let m = &plugin.manifest;
        let approval = match crate::plugin_approvals::state(&data_dir, plugin) {
            crate::plugin_approvals::State::Own => String::new(),
            crate::plugin_approvals::State::Approved => "    approved as it is\n".to_string(),
            crate::plugin_approvals::State::NotApproved => format!(
                "    NOT APPROVED, so not used — `incode plugin approve {}` once you have \
                 read it\n",
                m.name
            ),
            crate::plugin_approvals::State::Unknown(why) => {
                format!("    approval could not be checked: {why}\n")
            }
        };
        out.push_str(&format!(
            "{} {} ({}) — {}\n    {}\n    {}\n{approval}",
            m.name,
            if m.version.is_empty() {
                "-"
            } else {
                &m.version
            },
            plugin.scope.as_str(),
            m.description,
            plugin.root.display(),
            contents(plugin)
        ));
    }
    for problem in &found.problems {
        out.push_str(&format!("warning: {problem}\n"));
    }
    out
}

/// One line saying what a plugin brings.
fn contents(plugin: &Plugin) -> String {
    let commands =
        crate::custom_commands::scan_dir(&plugin.commands_dir(), Some(&plugin.manifest.name))
            .0
            .len();
    let skills = plugin.skills().len();
    let hooks = plugin
        .hooks()
        .map(|h| h.pre_tool_use.len() + h.post_tool_use.len())
        .unwrap_or(0);
    let servers = plugin.servers().map(|s| s.len()).unwrap_or(0);
    format!("{commands} command(s), {skills} skill(s), {hooks} hook(s), {servers} MCP server(s)")
}

/// Install a plugin from a local folder (copied) or a git URL (cloned with
/// `--depth 1`) into `target`, under the name its manifest gives.
pub fn add(source: &str, target: &Path) -> Result<Plugin, String> {
    crate::folders::create_dir_all(target)
        .map_err(|e| format!("could not create {}: {e}", target.display()))?;
    let staging = target.join(format!(".adding-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let installed = Source::of(source).fetch(&staging).and_then(|()| {
        let plugin = load_one(&staging, Scope::User)?;
        let dest = target.join(&plugin.manifest.name);
        if dest.exists() {
            return Err(format!(
                "plugin `{}` is already installed at {}; remove it first",
                plugin.manifest.name,
                dest.display()
            ));
        }
        std::fs::rename(&staging, &dest)
            .map_err(|e| format!("could not move it to {}: {e}", dest.display()))?;
        load_one(&dest, Scope::User)
    });
    let _ = std::fs::remove_dir_all(&staging);
    installed
}

/// Where a plugin is fetched from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A git URL, cloned with `--depth 1`.
    Git(String),
    /// A local folder, copied — kept absolute, so an update from another
    /// directory finds it.
    Path(PathBuf),
}

impl Source {
    /// What `incode plugin add` was given: a folder when one is there, a git
    /// URL otherwise.
    #[must_use]
    pub fn of(source: &str) -> Self {
        let local = Path::new(source);
        if local.is_dir() {
            Self::Path(std::fs::canonicalize(local).unwrap_or_else(|_| local.to_path_buf()))
        } else {
            Self::Git(source.to_string())
        }
    }

    /// Put a fresh copy into `dest`, and record there where it came from.
    fn fetch(&self, dest: &Path) -> Result<(), String> {
        match self {
            Self::Git(url) => clone(url, dest)?,
            Self::Path(from) => copy_dir(from, dest)
                .map_err(|e| format!("could not copy {}: {e}", from.display()))?,
        }
        std::fs::write(dest.join(SOURCE), self.to_toml())
            .map_err(|e| format!("could not record where it came from: {e}"))
    }

    fn to_toml(&self) -> String {
        let (key, value) = match self {
            Self::Git(url) => ("git", url.clone()),
            Self::Path(path) => ("path", path.to_string_lossy().into_owned()),
        };
        // A TOML literal string holds anything but a `'`, which a URL or a
        // path seldom has; the basic string's escapes cover the rest.
        if value.contains('\'') {
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            format!("{key} = \"{escaped}\"\n")
        } else {
            format!("{key} = '{value}'\n")
        }
    }

    /// Where the plugin in `root` came from: its [`SOURCE`] file, or — for
    /// one cloned before sources were recorded — its git `origin`.
    pub fn recorded(root: &Path) -> Result<Self, String> {
        if let Ok(text) = std::fs::read_to_string(root.join(SOURCE)) {
            for raw in text.lines() {
                let line = raw.trim();
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                let value = toml_string(value.trim())
                    .map_err(|e| format!("{}: {e}", root.join(SOURCE).display()))?;
                match key.trim() {
                    "git" => return Ok(Self::Git(value)),
                    "path" => return Ok(Self::Path(PathBuf::from(value))),
                    _ => {}
                }
            }
            return Err(format!(
                "{} names neither `git` nor `path`",
                root.join(SOURCE).display()
            ));
        }
        if root.join(".git").exists() {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["remote", "get-url", "origin"])
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|e| format!("could not run git: {e}"))?;
            let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success() && !url.is_empty() {
                return Ok(Self::Git(url));
            }
        }
        Err(format!(
            "{} does not say where it came from; remove it and add it again",
            root.display()
        ))
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Git(url) => write!(f, "{url}"),
            Self::Path(path) => write!(f, "{}", path.display()),
        }
    }
}

/// What one update did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Updated {
    pub name: String,
    pub from: String,
    pub to: String,
    pub source: Source,
}

impl std::fmt::Display for Updated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shown = |v: &str| {
            if v.is_empty() {
                "-".to_string()
            } else {
                v.to_string()
            }
        };
        if self.from == self.to {
            write!(
                f,
                "updated plugin `{}` from {} (version {} unchanged)",
                self.name,
                self.source,
                shown(&self.from)
            )
        } else {
            write!(
                f,
                "updated plugin `{}` from {}: {} → {}",
                self.name,
                self.source,
                shown(&self.from),
                shown(&self.to)
            )
        }
    }
}

/// Fetch plugin `name` in `target` again from where it came from, and swap
/// it in. The installed copy is left as it was when anything fails.
pub fn update(name: &str, target: &Path) -> Result<Updated, String> {
    check_name(name)?;
    let dest = target.join(name);
    let installed = load_one(&dest, Scope::User).map_err(|_| {
        format!(
            "no plugin `{name}` in {} (`incode plugin list` shows what is installed)",
            target.display()
        )
    })?;
    let source = Source::recorded(&dest)?;
    let staging = target.join(format!(".updating-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let swapped = source.fetch(&staging).and_then(|()| {
        let fetched = load_one(&staging, Scope::User)?;
        if fetched.manifest.name != installed.manifest.name {
            return Err(format!(
                "not updated: {source} is now plugin `{}`, not `{}`; remove `{}` and add it \
                 again if that is what you want",
                fetched.manifest.name, installed.manifest.name, installed.manifest.name
            ));
        }
        swap(&staging, &dest)?;
        Ok(Updated {
            name: installed.manifest.name.clone(),
            from: installed.manifest.version.clone(),
            to: fetched.manifest.version,
            source: source.clone(),
        })
    });
    let _ = std::fs::remove_dir_all(&staging);
    swapped
}

/// Put `fresh` where `dest` is, moving the old copy aside first and back if
/// the second move fails.
fn swap(fresh: &Path, dest: &Path) -> Result<(), String> {
    let mut old = dest.as_os_str().to_owned();
    old.push(format!(".old-{}", std::process::id()));
    let old = PathBuf::from(old);
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(dest, &old)
        .map_err(|e| format!("could not move {} aside: {e}", dest.display()))?;
    if let Err(e) = std::fs::rename(fresh, dest) {
        let _ = std::fs::rename(&old, dest);
        return Err(format!(
            "could not move the update into {}: {e}",
            dest.display()
        ));
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// [`update`] every plugin in `target`. One that fails does not stop the
/// rest; any failure makes the whole an error, with every line said.
pub fn update_all(target: &Path) -> Result<String, String> {
    let mut names: Vec<String> = std::fs::read_dir(target)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| e.path().join(MANIFEST).is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    if names.is_empty() {
        return Ok(format!("no plugins to update in {}", target.display()));
    }
    let mut lines = Vec::new();
    let mut failed = false;
    for name in names {
        match update(&name, target) {
            Ok(done) => lines.push(done.to_string()),
            Err(why) => {
                failed = true;
                lines.push(format!("plugin `{name}`: {why}"));
            }
        }
    }
    if failed {
        Err(lines.join("\n"))
    } else {
        Ok(lines.join("\n"))
    }
}

/// `git clone --depth 1 <url> <dest>`.
fn clone(url: &str, dest: &Path) -> Result<(), String> {
    let out = std::process::Command::new("git")
        .args(["clone", "--depth", "1", "--quiet", "--", url])
        .arg(dest)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git could not clone `{url}`: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Copy a folder, leaving out `.git` and anything that is not a plain file
/// or folder.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        if kind.is_dir() {
            copy_dir(&entry.path(), &to.join(&name))?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), to.join(&name))?;
        }
    }
    Ok(())
}

/// Remove an installed plugin folder from `target`.
pub fn remove(name: &str, target: &Path) -> Result<PathBuf, String> {
    check_name(name)?;
    let dest = target.join(name);
    if !dest.join(MANIFEST).is_file() {
        return Err(format!(
            "no plugin `{name}` in {} (`incode plugin list` shows what is installed)",
            target.display()
        ));
    }
    std::fs::remove_dir_all(&dest)
        .map_err(|e| format!("could not remove {}: {e}", dest.display()))?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    fn plugin(dir: &Path, name: &str, description: &str) -> PathBuf {
        let root = dir.join(name);
        write(
            &root.join(MANIFEST),
            &format!("name = \"{name}\"\nversion = \"1.0.0\"\ndescription = \"{description}\"\n"),
        );
        root
    }

    #[test]
    fn a_manifest_parses_and_ignores_what_it_does_not_know() {
        let m = parse_manifest(
            "# a plugin\nname = \"web\"  # its name\nversion = '0.2.0'\n\
             description = \"say \\\"hi\\\"\"\nauthor = \"x\"\n[extra]\nname = \"other\"\n",
        )
        .expect("parses");
        assert_eq!(m.name, "web");
        assert_eq!(m.version, "0.2.0");
        assert_eq!(m.description, "say \"hi\"");
    }

    #[test]
    fn a_manifest_without_a_usable_name_is_refused() {
        assert!(parse_manifest("version = \"1\"\n").is_err());
        assert!(parse_manifest("name = \"a/b\"\n").is_err());
        assert!(parse_manifest("name = \"a:b\"\n").is_err());
        assert!(parse_manifest("name = unquoted\n").is_err());
    }

    #[test]
    fn the_project_plugin_wins_over_the_users() {
        let dir = tempfile::tempdir().expect("dir");
        let project = dir.path().join("p");
        let user = dir.path().join("u");
        plugin(&project, "web", "project copy");
        plugin(&user, "web", "user copy");
        plugin(&user, "ops", "only the user's");
        std::fs::create_dir_all(user.join("broken")).expect("mkdir");

        let found = discover_in(Some(&project), &user);
        let names: Vec<_> = found
            .plugins
            .iter()
            .map(|p| (p.manifest.name.as_str(), p.scope))
            .collect();
        assert_eq!(names, vec![("web", Scope::Project), ("ops", Scope::User)]);
        assert_eq!(found.plugins[0].manifest.description, "project copy");
        assert_eq!(found.problems.len(), 2, "{:?}", found.problems);
        assert!(found.problems.iter().any(|p| p.contains("shadowed")));
        assert!(found.problems.iter().any(|p| p.contains("broken")));
    }

    /// **A CLONED REPOSITORY'S PLUGIN IS NOT USED UNTIL APPROVED.** Through
    /// the real gate (`discover_usable`, the data dir pointed at a temp
    /// folder): left out and said, used once `plugin approve` is run, left
    /// out again when its hooks change. The operator's own is used as is.
    #[test]
    fn a_project_plugin_is_used_only_once_approved_as_it_is() {
        let ws = tempfile::tempdir().expect("ws");
        let data = tempfile::tempdir().expect("data");
        let root = plugin(&project_dir(ws.path()), "cloned", "d");
        write(&root.join("hooks.json"), r#"{"pre_tool_use": ["check"]}"#);
        let mine = plugin(&data.path().join(PLUGINS_DIR), "mine", "d");
        write(&mine.join("hooks.json"), r#"{"pre_tool_use": ["mine"]}"#);

        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var_os(crate::admin::DATA_DIR_ENV);
        std::env::set_var(crate::admin::DATA_DIR_ENV, data.path());
        let names = |found: &Found| -> Vec<String> {
            found
                .plugins
                .iter()
                .map(|p| p.manifest.name.clone())
                .collect()
        };

        let before = discover_usable(ws.path());
        let left_out = before.problems.join("\n");
        let approved = approve_named("cloned", &discover(ws.path()), data.path());
        let after = discover_usable(ws.path());
        write(
            &root.join("hooks.json"),
            r#"{"pre_tool_use": ["curl evil"]}"#,
        );
        let changed = discover_usable(ws.path());
        let mine_needs_none = approve_named("mine", &discover(ws.path()), data.path());

        match saved {
            Some(value) => std::env::set_var(crate::admin::DATA_DIR_ENV, value),
            None => std::env::remove_var(crate::admin::DATA_DIR_ENV),
        }
        assert_eq!(names(&before), ["mine"]);
        assert!(
            left_out.contains("incode plugin approve cloned"),
            "{left_out}"
        );
        assert!(approved.expect("approve").starts_with("approved `cloned`"));
        assert_eq!(names(&after), ["cloned", "mine"]);
        assert_eq!(names(&changed), ["mine"], "a changed plugin was still used");
        assert!(mine_needs_none.expect("own").contains("needs no approval"));
        assert!(approve_named("nope", &discover(ws.path()), data.path()).is_err());
    }

    /// **A CLONED REPOSITORY'S PLUGIN DOES NOT CHECK YOUR PROMPTS.** A
    /// project plugin's `user_prompt_submit` is dropped — that folder comes
    /// with whatever was cloned, and the event runs on the first line typed —
    /// while the operator's own plugin's is kept, and the project's tool hooks
    /// are merged as before.
    #[test]
    fn a_project_plugins_prompt_hook_is_not_run_and_a_users_is() {
        let dir = tempfile::tempdir().expect("dir");
        let hooks = r#"{"pre_tool_use": ["tool-check"], "user_prompt_submit": ["prompt-check"]}"#;
        let project = plugin(&dir.path().join("project"), "cloned", "d");
        write(&project.join("hooks.json"), hooks);
        let user = plugin(&dir.path().join("user"), "mine", "d");
        write(&user.join("hooks.json"), hooks);
        let merged = hooks_of(&[
            load_one(&project, Scope::Project).expect("project plugin"),
            load_one(&user, Scope::User).expect("user plugin"),
        ]);
        assert_eq!(
            merged.user_prompt_submit,
            ["prompt-check"],
            "only the user's"
        );
        assert_eq!(merged.pre_tool_use, ["tool-check", "tool-check"]);
        let project_only = hooks_of(&[load_one(&project, Scope::Project).expect("project plugin")]);
        assert!(project_only.user_prompt_submit.is_empty());
    }

    #[test]
    fn skills_hooks_and_servers_are_read_from_the_folder() {
        let dir = tempfile::tempdir().expect("dir");
        let root = plugin(dir.path(), "web", "d");
        write(
            &root.join("skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: Ship it\n---\nbody\n",
        );
        write(
            &root.join("hooks.json"),
            r#"{"pre_tool_use": ["${INCODE_PLUGIN_ROOT}/check.sh"]}"#,
        );
        write(
            &root.join(".mcp.json"),
            r#"{"mcpServers": {"db": {"command": "${INCODE_PLUGIN_ROOT}/db", "args": ["--ro"],
                "env": {"K": "v"}}, "web": {"type": "http", "url": "http://x/mcp"}}}"#,
        );
        let p = load_one(&root, Scope::User).expect("loads");

        let skills = p.skills();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "web:deploy");
        assert_eq!(skills[0].description, "Ship it");
        assert_eq!(
            crate::skills::body_of(&skills[0]).expect("body"),
            "---\nname: deploy\ndescription: Ship it\n---\nbody\n"
        );

        let hooks = p.hooks().expect("hooks");
        assert_eq!(
            hooks.pre_tool_use,
            vec![format!("{}/check.sh", root.display())]
        );

        let servers = p.servers().expect("servers");
        let names: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["web_db", "web_web"]);
        match &servers[0].transport {
            crate::toolbox::Transport::Stdio { command, args, env } => {
                assert_eq!(command, &root.join("db"));
                assert_eq!(args, &vec!["--ro".to_string()]);
                assert_eq!(env, &vec![("K".to_string(), "v".to_string())]);
            }
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn add_copies_a_folder_under_its_manifest_name_and_remove_deletes_it() {
        let dir = tempfile::tempdir().expect("dir");
        let source = plugin(&dir.path().join("src"), "checkout-name", "d");
        // The folder name is not the plugin's name; the manifest is.
        let renamed = dir.path().join("src/whatever");
        std::fs::rename(&source, &renamed).expect("rename");
        write(&renamed.join("commands/hi.md"), "Say hi");
        write(&renamed.join(".git/HEAD"), "ref");
        let target = dir.path().join("plugins");

        let installed = add(&renamed.to_string_lossy(), &target).expect("adds");
        assert_eq!(installed.root, target.join("checkout-name"));
        assert!(installed.root.join("commands/hi.md").is_file());
        assert!(!installed.root.join(".git").exists());

        let again = add(&renamed.to_string_lossy(), &target).expect_err("already there");
        assert!(again.contains("already installed"), "{again}");
        assert!(
            std::fs::read_dir(&target)
                .expect("read")
                .filter_map(Result::ok)
                .all(|e| !e.file_name().to_string_lossy().starts_with(".adding")),
            "no staging folder is left behind"
        );

        assert!(remove("nope", &target).is_err());
        remove("checkout-name", &target).expect("removes");
        assert!(!target.join("checkout-name").exists());
    }

    /// Bump the manifest in `root` to `version`, under `name`.
    fn manifest(root: &Path, name: &str, version: &str) {
        write(
            &root.join(MANIFEST),
            &format!("name = \"{name}\"\nversion = \"{version}\"\n"),
        );
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c"])
            .arg("commit.gpgsign=false")
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    #[test]
    fn update_copies_a_folder_again_and_says_the_versions() {
        let dir = tempfile::tempdir().expect("dir");
        let source = dir.path().join("src/web");
        manifest(&source, "web", "1.0.0");
        let target = dir.path().join("plugins");
        let installed = add(&source.to_string_lossy(), &target).expect("adds");
        assert_eq!(
            Source::recorded(&installed.root).expect("recorded"),
            Source::Path(std::fs::canonicalize(&source).expect("canonical"))
        );

        manifest(&source, "web", "1.1.0");
        write(&source.join("commands/new.md"), "New");
        let done = update("web", &target).expect("updates");
        assert_eq!((done.from.as_str(), done.to.as_str()), ("1.0.0", "1.1.0"));
        assert!(done.to_string().contains("1.0.0 → 1.1.0"), "{done}");
        assert!(target.join("web/commands/new.md").is_file());
        assert!(target.join("web").join(SOURCE).is_file(), "still recorded");
        let left: Vec<String> = std::fs::read_dir(&target)
            .expect("read")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["web".to_string()], "no staging or old copy left");

        let same = update_all(&target).expect("all");
        assert!(same.contains("version 1.1.0 unchanged"), "{same}");
    }

    #[test]
    fn an_update_that_renames_the_plugin_is_refused_and_changes_nothing() {
        let dir = tempfile::tempdir().expect("dir");
        let source = dir.path().join("src/web");
        manifest(&source, "web", "1.0.0");
        let target = dir.path().join("plugins");
        add(&source.to_string_lossy(), &target).expect("adds");

        manifest(&source, "site", "2.0.0");
        let err = update("web", &target).expect_err("refused");
        assert!(err.contains("now plugin `site`"), "{err}");
        let kept = load_one(&target.join("web"), Scope::User).expect("still there");
        assert_eq!(kept.manifest.version, "1.0.0");
        assert!(!target.join("site").exists());

        let all = update_all(&target).expect_err("one failed");
        assert!(all.contains("plugin `web`"), "{all}");
    }

    #[test]
    fn update_clones_a_git_source_again() {
        let dir = tempfile::tempdir().expect("dir");
        let repo = dir.path().join("repo");
        manifest(&repo, "web", "0.1.0");
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "one"]);
        let url = format!("file://{}", repo.display());
        let target = dir.path().join("plugins");
        let installed = add(&url, &target).expect("clones");
        assert_eq!(
            Source::recorded(&installed.root).expect("recorded"),
            Source::Git(url.clone())
        );

        manifest(&repo, "web", "0.2.0");
        git(&repo, &["commit", "-q", "-am", "two"]);
        let done = update("web", &target).expect("updates");
        assert_eq!((done.from.as_str(), done.to.as_str()), ("0.1.0", "0.2.0"));
        assert_eq!(done.source, Source::Git(url));
    }

    #[test]
    fn a_plugin_without_a_recorded_source_is_not_updated() {
        let dir = tempfile::tempdir().expect("dir");
        plugin(dir.path(), "web", "d");
        let err = update("web", dir.path()).expect_err("no source");
        assert!(err.contains("add it again"), "{err}");
        assert!(update("nope", dir.path())
            .expect_err("missing")
            .contains("no plugin"));
    }

    #[test]
    fn a_source_is_recorded_so_it_reads_back() {
        let dir = tempfile::tempdir().expect("dir");
        for source in [
            Source::Git("https://example.com/a.git".to_string()),
            Source::Path(PathBuf::from("/opt/it's here")),
        ] {
            std::fs::write(dir.path().join(SOURCE), source.to_toml()).expect("write");
            assert_eq!(Source::recorded(dir.path()).expect("reads"), source);
        }
    }

    #[test]
    fn a_source_that_is_neither_a_folder_nor_a_repository_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let missing = dir.path().join("not-here");
        let err = add(&missing.to_string_lossy(), &dir.path().join("plugins")).expect_err("fails");
        assert!(!err.is_empty());
    }
}
