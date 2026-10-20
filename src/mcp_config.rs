//! MCP servers named in configuration files: a project's `.mcp.json` and the
//! user's `~/.incode/mcp.json`.
//!
//! # The format
//!
//! The one other agents already read, so a repository's file works as is:
//!
//! ```json
//! { "mcpServers": {
//!     "github": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
//!                 "env": { "GITHUB_TOKEN": "${GITHUB_TOKEN}" } },
//!     "docs":   { "type": "http", "url": "https://mcp.example.com/mcp",
//!                 "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" } } } }
//! ```
//!
//! `${VAR}` and `${VAR:-default}` are expanded from incode's environment in
//! `command`, `args`, `env` values, `url` and `headers` values, when the
//! session starts — never written back. A variable that is unset and has no
//! default stops that server, and says which.
//!
//! # How they connect
//!
//! Exactly as the installed external tools and an ACP client's servers do:
//! handed to `toolbox::client_servers`, started in the background, their tools
//! offered as `mcp__<server>__<tool>` and every call run through the
//! toolbox's one gate (guards, permission mode and rules, the memory engine,
//! hooks). Stdio servers are children of incode; `http` servers are spoken to
//! over MCP Streamable HTTP (`toolbox::mcp_http`: JSON-RPC POSTs, the
//! `Mcp-Session-Id` sent back, https anywhere and plain http only to this
//! machine). `sse` is accepted and tried over Streamable HTTP; a server that
//! speaks only the legacy SSE transport will not connect, and says so.
//!
//! # OAuth
//!
//! An `http` server that needs OAuth is logged in to once, with `incode mcp
//! login <name>` ([`crate::mcp_oauth`]): the browser opens, and the tokens are
//! kept in `~/.incode/mcp-tokens.json`, refreshed as they expire. An entry
//! may say more under `"oauth"` — a `client_id` (and `client_secret`) for an
//! authorization server without dynamic registration, the `scopes`, a fixed
//! `callback_port`:
//!
//! ```json
//! "docs": { "url": "https://mcp.example.com/mcp",
//!           "oauth": { "client_id": "abc", "scopes": ["read"] } }
//! ```
//!
//! A session never waits on a browser: a server with an `"oauth"` entry and no
//! token is not started, and one that answers `401` with no token to renew is
//! dropped — each with the command that logs in. A server given its own
//! `Authorization` header is sent that, and no OAuth token.
//!
//! # A project's servers are asked about first
//!
//! A `.mcp.json` arrives with a `git clone`, and a server is a program incode
//! would run. So a project's server starts only once the user has approved
//! THAT server: its name and a sha256 of everything that decides what runs
//! (type, command, args, env, url, headers — as written, before expansion).
//! Approvals are kept in `~/.incode/mcp-approvals.json`; a repository that
//! changes its server's command needs approving again. The REPL asks at
//! start; `--acp` asks the client once the session is open
//! (`session/request_permission`: allow once, allow always, reject — see
//! `acp_agent::permission`). Under `-t`, or when the client cannot answer, an
//! unapproved server is skipped, with the command that approves it:
//! `incode mcp approve <name>`. The user's own
//! `~/.incode/mcp.json` needs no approval — the user wrote it.
//!
//! A project's server overrides the user's of the same name, and still needs
//! approving.
//!
//! # Managed policy
//!
//! The administrator's `[mcp] allow` / `deny` lists
//! ([`crate::permissions::managed`]) are applied where every server is
//! connected, whatever named it — see [`apply_managed_policy`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::toolbox::{ServerSpec, Transport};

/// A project's servers, at the workspace root.
pub const PROJECT_FILE: &str = ".mcp.json";
/// The user's servers, in incode's data directory.
pub const USER_FILE: &str = "mcp.json";
/// The project servers the user approved, in incode's data directory.
pub const APPROVALS_FILE: &str = "mcp-approvals.json";

/// One server, as a file writes it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerEntry {
    /// `stdio` (the default with a `command`), `http` (the default with only
    /// a `url`), `streamable-http` or `sse`.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// How to log in to an `http` server that needs OAuth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<crate::mcp_oauth::OAuthConfig>,
    /// Whatever else the file says about this server, kept when it is
    /// written back.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// A whole `.mcp.json` / `mcp.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpFile {
    #[serde(rename = "mcpServers", default)]
    pub servers: BTreeMap<String, ServerEntry>,
    /// Whatever else the file holds, kept when it is written back.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// Which file a server is named in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
}

impl Scope {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }

    fn parse(word: &str) -> Result<Self, String> {
        match word {
            "user" | "global" => Ok(Self::User),
            "project" => Ok(Self::Project),
            other => Err(format!("unknown scope `{other}`: user or project")),
        }
    }

    /// The file this scope's servers are in.
    #[must_use]
    pub fn path(self, workspace: &Path, data_dir: &Path) -> PathBuf {
        match self {
            Self::User => data_dir.join(USER_FILE),
            Self::Project => workspace.join(PROJECT_FILE),
        }
    }
}

/// One configured server, with where it was named.
#[derive(Debug, Clone, PartialEq)]
pub struct Configured {
    pub name: String,
    pub scope: Scope,
    pub entry: ServerEntry,
}

// ---------------------------------------------------------------------------
// Reading and writing
// ---------------------------------------------------------------------------

/// Parse a file's text.
///
/// # Errors
///
/// It is not JSON of this shape.
pub fn parse(text: &str) -> Result<McpFile, String> {
    if text.trim().is_empty() {
        return Ok(McpFile::default());
    }
    serde_json::from_str(text).map_err(|e| e.to_string())
}

/// Read `path`; an absent file is an empty one.
///
/// # Errors
///
/// It could not be read, or is not valid.
pub fn read(path: &Path) -> Result<McpFile, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map_err(|e| format!("{} is not valid: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(McpFile::default()),
        Err(e) => Err(format!("could not read {}: {e}", path.display())),
    }
}

fn write(path: &Path, file: &McpFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(file).map_err(|e| e.to_string())?;
    std::fs::write(path, text + "\n")
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Every configured server: the user's, then the project's over them by
/// name. A file that cannot be read costs its own servers, and is said.
#[must_use]
pub fn configured(workspace: &Path, data_dir: &Path) -> (Vec<Configured>, Vec<String>) {
    let mut by_name: BTreeMap<String, Configured> = BTreeMap::new();
    let mut said = Vec::new();
    for scope in [Scope::User, Scope::Project] {
        match read(&scope.path(workspace, data_dir)) {
            Ok(file) => {
                for (name, entry) in file.servers {
                    by_name.insert(name.clone(), Configured { name, scope, entry });
                }
            }
            Err(why) => said.push(why),
        }
    }
    (by_name.into_values().collect(), said)
}

// ---------------------------------------------------------------------------
// Expansion
// ---------------------------------------------------------------------------

/// `${VAR}` and `${VAR:-default}` in `text`, from `lookup`. A `$` not
/// followed by `{` is left as it is.
///
/// # Errors
///
/// A variable is unset with no default, or a `${` is not closed.
pub fn expand(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("${") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let Some(close) = after.find('}') else {
            return Err(format!("`${{` without a closing `}}` in `{text}`"));
        };
        let inside = &after[..close];
        let (name, default) = match inside.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (inside, None),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("`${{{inside}}}` does not name a variable"));
        }
        match lookup(name)
            .filter(|v| !v.is_empty())
            .or(default.map(str::to_string))
        {
            Some(value) => out.push_str(&value),
            None => return Err(format!("the environment variable `{name}` is not set")),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn from_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

impl ServerEntry {
    /// The transport this entry names: `stdio` or `http`.
    fn transport_kind(&self) -> Result<&'static str, String> {
        let kind = match self.kind.as_deref() {
            Some(kind) => kind,
            None if self.command.is_none() && self.url.is_some() => "http",
            None => "stdio",
        };
        match kind {
            "stdio" => Ok("stdio"),
            "http" | "streamable-http" | "streamableHttp" | "sse" => Ok("http"),
            other => Err(format!("unknown server type `{other}`: stdio, http or sse")),
        }
    }

    /// What it starts, for a person: no header or variable values.
    #[must_use]
    pub fn summary(&self) -> String {
        match self.transport_kind() {
            Ok("stdio") => {
                let mut said = format!(
                    "stdio: {}",
                    std::iter::once(self.command.as_deref().unwrap_or("<no command>"))
                        .chain(self.args.iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                if !self.env.is_empty() {
                    let names: Vec<&str> = self.env.keys().map(String::as_str).collect();
                    said.push_str(&format!(" (env: {})", names.join(", ")));
                }
                said
            }
            Ok(_) => {
                let mut said = format!(
                    "{}: {}",
                    self.kind.as_deref().unwrap_or("http"),
                    self.url.as_deref().unwrap_or("<no url>")
                );
                if !self.headers.is_empty() {
                    let names: Vec<&str> = self.headers.keys().map(String::as_str).collect();
                    said.push_str(&format!(" (headers: {})", names.join(", ")));
                }
                if self.oauth.is_some() {
                    said.push_str(" (OAuth)");
                }
                said
            }
            Err(why) => why,
        }
    }

    /// This entry as the toolbox connects it, every `${VAR}` expanded.
    ///
    /// # Errors
    ///
    /// It names no command or url for its type, or a variable is unset.
    pub(crate) fn to_spec(
        &self,
        name: &str,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<ServerSpec, String> {
        let transport = match self.transport_kind()? {
            "stdio" => {
                let command = self
                    .command
                    .as_deref()
                    .filter(|c| !c.trim().is_empty())
                    .ok_or("a stdio server needs a `command`")?;
                Transport::Stdio {
                    command: PathBuf::from(expand(command, lookup)?),
                    args: self
                        .args
                        .iter()
                        .map(|a| expand(a, lookup))
                        .collect::<Result<_, _>>()?,
                    env: self
                        .env
                        .iter()
                        .map(|(k, v)| Ok((k.clone(), expand(v, lookup)?)))
                        .collect::<Result<_, String>>()?,
                }
            }
            _ => {
                let url = self
                    .url
                    .as_deref()
                    .filter(|u| !u.trim().is_empty())
                    .ok_or("an http server needs a `url`")?;
                Transport::Http {
                    url: expand(url, lookup)?,
                    headers: self
                        .headers
                        .iter()
                        .map(|(k, v)| Ok((k.clone(), expand(v, lookup)?)))
                        .collect::<Result<_, String>>()?,
                    oauth: None,
                }
            }
        };
        Ok(ServerSpec {
            name: name.to_string(),
            transport,
        })
    }
}

// ---------------------------------------------------------------------------
// Approval
// ---------------------------------------------------------------------------

/// `<name>:<sha256>` of everything that decides what this server runs, as
/// written — so a changed command, argument, variable, address or header is
/// a different server and asked about again.
#[must_use]
pub fn approval_key(name: &str, entry: &ServerEntry) -> String {
    // Each field framed by its length, so no two entries share a hash by
    // moving text from one field into the next.
    let mut hasher = Sha256::new();
    let mut field = |text: &str| {
        hasher.update((text.len() as u64).to_le_bytes());
        hasher.update(text.as_bytes());
    };
    field(name);
    field(entry.transport_kind().unwrap_or("?"));
    field(entry.command.as_deref().unwrap_or(""));
    field(&entry.args.len().to_string());
    for arg in &entry.args {
        field(arg);
    }
    field(&entry.env.len().to_string());
    for (k, v) in &entry.env {
        field(k);
        field(v);
    }
    field(entry.url.as_deref().unwrap_or(""));
    field(&entry.headers.len().to_string());
    for (k, v) in &entry.headers {
        field(k);
        field(v);
    }
    // Only when present, so approvals of entries without it stay valid: a
    // changed client or wider scopes is a different server to approve.
    if let Some(oauth) = &entry.oauth {
        field("oauth");
        field(&serde_json::to_string(oauth).unwrap_or_default());
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{name}:{hex}")
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Approvals {
    #[serde(default)]
    approved: BTreeSet<String>,
}

fn read_approvals(data_dir: &Path) -> Approvals {
    std::fs::read_to_string(data_dir.join(APPROVALS_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Whether the user approved this project server, as it is now written.
#[must_use]
pub fn is_approved(data_dir: &Path, name: &str, entry: &ServerEntry) -> bool {
    read_approvals(data_dir)
        .approved
        .contains(&approval_key(name, entry))
}

/// Remember that the user approved this project server, as it is written.
///
/// # Errors
///
/// The approvals file could not be written.
pub fn approve(data_dir: &Path, name: &str, entry: &ServerEntry) -> Result<(), String> {
    let mut approvals = read_approvals(data_dir);
    approvals.approved.insert(approval_key(name, entry));
    std::fs::create_dir_all(data_dir)
        .map_err(|e| format!("could not create {}: {e}", data_dir.display()))?;
    let path = data_dir.join(APPROVALS_FILE);
    let text = serde_json::to_string_pretty(&approvals).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// A session's servers
// ---------------------------------------------------------------------------

/// Asked whether a project server not yet approved may start: its name and
/// entry. `true` approves it, and the approval is remembered.
pub type Asker<'a> = &'a dyn Fn(&str, &ServerEntry) -> bool;

/// The configured servers a session starts, and a line for each one that is
/// not started and why. `ask` is the person at the terminal, if there is one.
pub(crate) fn session_servers(
    workspace: &Path,
    data_dir: &Path,
    ask: Option<Asker<'_>>,
) -> (Vec<ServerSpec>, Vec<String>) {
    let (specs, mut said, unapproved) = session_servers_asking_later(workspace, data_dir, ask);
    said.extend(
        unapproved
            .iter()
            .map(|server| not_approved(&server.name, &server.entry)),
    );
    (specs, said)
}

/// [`session_servers`], with the project servers not yet approved — and not
/// asked about, with no `ask` — handed back instead of said: for a caller
/// that asks about them once the session is open (`--acp`).
pub(crate) fn session_servers_asking_later(
    workspace: &Path,
    data_dir: &Path,
    ask: Option<Asker<'_>>,
) -> (Vec<ServerSpec>, Vec<String>, Vec<Configured>) {
    let (servers, mut said) = configured(workspace, data_dir);
    let mut specs = Vec::new();
    let mut unapproved = Vec::new();
    for server in servers {
        if server.scope == Scope::Project && !is_approved(data_dir, &server.name, &server.entry) {
            match ask {
                Some(ask) if ask(&server.name, &server.entry) => {
                    if let Err(why) = approve(data_dir, &server.name, &server.entry) {
                        said.push(approved_once(&server.name, &why));
                    }
                }
                Some(_) => {
                    said.push(refused(&server.name));
                    continue;
                }
                None => {
                    unapproved.push(server);
                    continue;
                }
            }
        }
        match server.spec(data_dir) {
            Ok(spec) => specs.push(spec),
            Err(why) => said.push(why),
        }
    }
    (specs, said, unapproved)
}

impl Configured {
    /// This server as the toolbox connects it, or why it cannot start.
    ///
    /// # Errors
    ///
    /// It names no command or url for its type, or a variable is unset.
    /// An http server is bound to its stored OAuth token from `data_dir`
    /// ([`with_oauth`]); one that needs a login it lacks cannot start.
    pub(crate) fn spec(&self, data_dir: &Path) -> Result<ServerSpec, String> {
        let spec = self.entry.to_spec(&self.name, &from_env).map_err(|why| {
            format!(
                "MCP server `{}` ({} settings) is not started: {why}",
                self.name,
                self.scope.as_str()
            )
        })?;
        with_oauth(spec, &self.entry, data_dir)
    }
}

/// What is said of a project server skipped for want of an approval.
#[must_use]
pub(crate) fn not_approved(name: &str, entry: &ServerEntry) -> String {
    format!(
        "MCP server `{name}` from {PROJECT_FILE} is not started: it has not been approved. \
         `incode mcp approve {name}` approves it ({})",
        entry.summary()
    )
}

/// What is said of a project server the person refused.
#[must_use]
pub(crate) fn refused(name: &str) -> String {
    format!("MCP server `{name}` from {PROJECT_FILE} was not approved and is not started")
}

/// What is said when an approval could not be remembered.
#[must_use]
pub(crate) fn approved_once(name: &str, why: &str) -> String {
    format!("MCP server `{name}` approved for this session only: {why}")
}

/// Whether `name: value` is an `Authorization` header with no credential in
/// it — `Bearer ${TOKEN:-}` with the variable unset.
fn is_blank_authorization(name: &str, value: &str) -> bool {
    name.eq_ignore_ascii_case("authorization") && value.split_whitespace().nth(1).is_none()
}

/// Whether `headers` carries an `Authorization` with a credential in it.
fn has_authorization(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("authorization") && !is_blank_authorization(name, value)
    })
}

/// `spec`, an `http` server, bound to its OAuth token in `data_dir` — unless
/// it sends an `Authorization` header of its own (a blank one is dropped, and
/// OAuth used instead). A server whose entry says it
/// needs OAuth and that has no token is refused, with the command that logs
/// in: a session does not wait on a browser.
fn with_oauth(
    mut spec: ServerSpec,
    entry: &ServerEntry,
    data_dir: &Path,
) -> Result<ServerSpec, String> {
    let Transport::Http {
        url,
        headers,
        oauth,
    } = &mut spec.transport
    else {
        return Ok(spec);
    };
    if has_authorization(headers) {
        return Ok(spec);
    }
    let store = data_dir.join(crate::mcp_oauth::TOKENS_FILE);
    if entry.oauth.is_some() && crate::mcp_oauth::load(&store, &spec.name, url).is_none() {
        return Err(format!(
            "MCP server `{0}` needs OAuth and is not logged in, so it is not started: run \
             `incode mcp login {0}`",
            spec.name
        ));
    }
    headers.retain(|(name, value)| !is_blank_authorization(name, value));
    *oauth = Some(crate::mcp_oauth::Binding {
        name: spec.name.clone(),
        store,
    });
    Ok(spec)
}

/// `specs` less those the administrator's `[mcp]` lists refuse, and a line
/// for each one refused.
pub(crate) fn apply_managed_policy(
    policy: &crate::permissions::managed::McpPolicy,
    specs: Vec<ServerSpec>,
) -> (Vec<ServerSpec>, Vec<String>) {
    let mut kept = Vec::new();
    let mut said = Vec::new();
    for spec in specs {
        let command = match &spec.transport {
            Transport::Stdio { command, .. } => Some(command.to_string_lossy().into_owned()),
            Transport::Http { .. } => None,
        };
        let url = match &spec.transport {
            Transport::Http { url, .. } => Some(url.as_str()),
            Transport::Stdio { .. } => None,
        };
        let facts = crate::permissions::managed::ServerFacts {
            name: &spec.name,
            command: command.as_deref(),
            url,
        };
        match policy.refuses(facts) {
            Some(why) => said.push(format!("MCP server `{}` is not started: {why}", spec.name)),
            None => kept.push(spec),
        }
    }
    (kept, said)
}

// ---------------------------------------------------------------------------
// `incode mcp …`
// ---------------------------------------------------------------------------

/// What `incode mcp` prints for help.
pub const USAGE: &str = "\
incode mcp — the MCP servers incode starts for its sessions

USAGE:
    incode mcp list
    incode mcp get <name>
    incode mcp add [-s user|project] [-t stdio|http|sse] [-e KEY=VALUE]...
                   [-H 'Name: value']... <name> [--] <command-or-url> [args...]
    incode mcp remove [-s user|project] <name>
    incode mcp approve <name>
    incode mcp login <name>
    incode mcp logout <name>

Servers live in ~/.incode/mcp.json (user, the default) and ./.mcp.json
(project). A project's server starts only once approved; adding one here
approves it. `${VAR}` and `${VAR:-default}` are expanded when a session starts.
`login` opens the browser to authorize an http server that needs OAuth; its
tokens are kept in ~/.incode/mcp-tokens.json.";

/// Run `incode mcp <args>` and return what to print.
///
/// # Errors
///
/// The arguments are wrong, or a file could not be read or written.
pub fn command(args: &[String], workspace: &Path, data_dir: &Path) -> Result<String, String> {
    let Some((verb, rest)) = args.split_first() else {
        return Ok(USAGE.to_string());
    };
    match verb.as_str() {
        "list" | "ls" => Ok(list(workspace, data_dir)),
        "get" => {
            let [name] = rest else {
                return Err("usage: incode mcp get <name>".to_string());
            };
            get(workspace, data_dir, name)
        }
        "add" => add(rest, workspace, data_dir),
        "remove" | "rm" => remove(rest, workspace, data_dir),
        "approve" => {
            let [name] = rest else {
                return Err("usage: incode mcp approve <name>".to_string());
            };
            let file = read(&Scope::Project.path(workspace, data_dir))?;
            let entry = file
                .servers
                .get(name)
                .ok_or_else(|| format!("no server `{name}` in {PROJECT_FILE}"))?;
            approve(data_dir, name, entry)?;
            Ok(format!("approved `{name}`: {}", entry.summary()))
        }
        "login" => {
            let [name] = rest else {
                return Err("usage: incode mcp login <name>".to_string());
            };
            login(workspace, data_dir, name)
        }
        "logout" => {
            let [name] = rest else {
                return Err("usage: incode mcp logout <name>".to_string());
            };
            let store = data_dir.join(crate::mcp_oauth::TOKENS_FILE);
            Ok(if crate::mcp_oauth::forget(&store, name)? {
                format!("logged out of `{name}`; its tokens are forgotten")
            } else {
                format!("`{name}` was not logged in")
            })
        }
        "-h" | "--help" | "help" => Ok(USAGE.to_string()),
        other => Err(format!("unknown `incode mcp {other}`\n\n{USAGE}")),
    }
}

/// `incode mcp login <name>`: the browser flow, its address printed too.
fn login(workspace: &Path, data_dir: &Path, name: &str) -> Result<String, String> {
    let (servers, _) = configured(workspace, data_dir);
    let server = servers
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("no MCP server `{name}` is configured"))?;
    let Transport::Http { url, .. } = server.entry.to_spec(name, &from_env)?.transport else {
        return Err(format!(
            "`{name}` is a stdio server; OAuth is for http servers"
        ));
    };
    let mut config = server.entry.oauth.clone().unwrap_or_default();
    // A client's secret belongs in the environment, not in a file a
    // repository carries.
    for field in [&mut config.client_id, &mut config.client_secret] {
        if let Some(text) = field.as_mut() {
            *text = expand(text, &from_env)?;
        }
    }
    // `${SECRET:-}` with the variable unset: a public client.
    config.client_secret = config.client_secret.filter(|s| !s.is_empty());
    crate::mcp_oauth::login(
        name,
        &url,
        &config,
        &data_dir.join(crate::mcp_oauth::TOKENS_FILE),
        &|line| println!("{line}"),
        &crate::mcp_oauth::open_browser,
    )
}

fn status(data_dir: &Path, server: &Configured) -> String {
    let policy = crate::permissions::managed::mcp_policy();
    let lookup = |name: &str| from_env(name);
    let mut auth = String::new();
    if let Ok(spec) = server.entry.to_spec(&server.name, &lookup) {
        if let Transport::Http { url, headers, .. } = &spec.transport {
            if !has_authorization(headers) {
                let store = data_dir.join(crate::mcp_oauth::TOKENS_FILE);
                let configured = server.entry.oauth.is_some();
                if let Some(said) = crate::mcp_oauth::status(&store, &server.name, url, configured)
                {
                    auth = format!("; {said}");
                }
            }
        }
        let (_, refused) = apply_managed_policy(&policy, vec![spec]);
        if let Some(why) = refused.first() {
            return format!("blocked — {why}");
        }
    }
    let trust = match server.scope {
        Scope::User => "trusted (user settings)".to_string(),
        Scope::Project if is_approved(data_dir, &server.name, &server.entry) => {
            "approved".to_string()
        }
        Scope::Project => format!("needs approval: incode mcp approve {}", server.name),
    };
    trust + &auth
}

fn list(workspace: &Path, data_dir: &Path) -> String {
    let (servers, said) = configured(workspace, data_dir);
    let mut lines: Vec<String> = said;
    if servers.is_empty() {
        lines.push(format!(
            "no MCP servers configured (looked in {} and {})",
            Scope::User.path(workspace, data_dir).display(),
            Scope::Project.path(workspace, data_dir).display()
        ));
    }
    for server in &servers {
        lines.push(format!(
            "{} [{}] {} — {}",
            server.name,
            server.scope.as_str(),
            server.entry.summary(),
            status(data_dir, server)
        ));
    }
    lines.join("\n")
}

fn get(workspace: &Path, data_dir: &Path, name: &str) -> Result<String, String> {
    let (servers, _) = configured(workspace, data_dir);
    let server = servers
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("no MCP server `{name}` is configured"))?;
    let mut lines = vec![
        name.to_string(),
        format!(
            "  scope:   {} ({})",
            server.scope.as_str(),
            server.scope.path(workspace, data_dir).display()
        ),
        format!("  {}", server.entry.summary()),
        format!("  status:  {}", status(data_dir, server)),
    ];
    if server.scope == Scope::Project {
        lines.push(format!(
            "  key:     {}",
            approval_key(&server.name, &server.entry)
        ));
    }
    Ok(lines.join("\n"))
}

/// The value after a flag, or an error naming the flag.
fn value<'a>(args: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<&'a String, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn add(args: &[String], workspace: &Path, data_dir: &Path) -> Result<String, String> {
    let mut scope = Scope::User;
    let mut kind: Option<String> = None;
    let mut env = BTreeMap::new();
    let mut headers = BTreeMap::new();
    let mut positional: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        // Once the name and the command are named, the rest are the
        // command's own arguments, flags or not.
        if positional.len() >= 2 {
            positional.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "-s" | "--scope" => scope = Scope::parse(value(&mut iter, arg)?)?,
            "-t" | "--transport" => kind = Some(value(&mut iter, arg)?.clone()),
            "-e" | "--env" => {
                let pair = value(&mut iter, arg)?;
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("`{pair}`: -e takes KEY=VALUE"))?;
                env.insert(k.to_string(), v.to_string());
            }
            "-H" | "--header" => {
                let pair = value(&mut iter, arg)?;
                let (k, v) = pair
                    .split_once(':')
                    .ok_or_else(|| format!("`{pair}`: -H takes 'Name: value'"))?;
                headers.insert(k.trim().to_string(), v.trim().to_string());
            }
            "--" => positional.extend(iter.by_ref().cloned()),
            _ => positional.push(arg.clone()),
        }
    }
    let mut positional = positional.into_iter();
    let (Some(name), Some(target)) = (positional.next(), positional.next()) else {
        return Err(format!(
            "usage: incode mcp add [options] <name> <command-or-url> [args...]\n\n{USAGE}"
        ));
    };
    let rest: Vec<String> = positional.collect();
    if name.trim().is_empty() {
        return Err("a server needs a name".to_string());
    }
    let is_http = match kind.as_deref() {
        Some("stdio") => false,
        Some("http" | "sse" | "streamable-http") => true,
        Some(other) => return Err(format!("unknown transport `{other}`: stdio, http or sse")),
        None => target.starts_with("http://") || target.starts_with("https://"),
    };
    let entry = if is_http {
        if !env.is_empty() || !rest.is_empty() {
            return Err("an http server takes a url and headers, not env or arguments".to_string());
        }
        ServerEntry {
            kind: Some(kind.unwrap_or_else(|| "http".to_string())),
            url: Some(target),
            headers,
            ..ServerEntry::default()
        }
    } else {
        if !headers.is_empty() {
            return Err("a stdio server takes env, not headers".to_string());
        }
        ServerEntry {
            command: Some(target),
            args: rest,
            env,
            ..ServerEntry::default()
        }
    };
    let path = scope.path(workspace, data_dir);
    let mut file = read(&path)?;
    let replaced = file.servers.insert(name.clone(), entry.clone()).is_some();
    write(&path, &file)?;
    // The user typed it: a project server added here is approved as written.
    if scope == Scope::Project {
        approve(data_dir, &name, &entry)?;
    }
    Ok(format!(
        "{} MCP server `{name}` in {}: {}",
        if replaced { "replaced" } else { "added" },
        path.display(),
        entry.summary()
    ))
}

fn remove(args: &[String], workspace: &Path, data_dir: &Path) -> Result<String, String> {
    let mut scope: Option<Scope> = None;
    let mut name: Option<&String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-s" | "--scope" => scope = Some(Scope::parse(value(&mut iter, arg)?)?),
            _ if name.is_none() => name = Some(arg),
            other => {
                return Err(format!(
                    "unexpected `{other}`: incode mcp remove [-s scope] <name>"
                ))
            }
        }
    }
    let name = name.ok_or("usage: incode mcp remove [-s user|project] <name>")?;
    let scopes = match scope {
        Some(scope) => vec![scope],
        None => vec![Scope::User, Scope::Project],
    };
    let mut removed = Vec::new();
    for scope in scopes {
        let path = scope.path(workspace, data_dir);
        let mut file = read(&path)?;
        if file.servers.remove(name).is_some() {
            write(&path, &file)?;
            removed.push(path.display().to_string());
        }
    }
    if removed.is_empty() {
        return Err(format!("no MCP server `{name}` to remove"));
    }
    Ok(format!("removed `{name}` from {}", removed.join(" and ")))
}

#[cfg(test)]
#[path = "mcp_config_tests.rs"]
mod tests;
