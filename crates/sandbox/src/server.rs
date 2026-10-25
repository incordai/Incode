//! `sbx mcp`: sandboxes as a service an agent drives over MCP (stdio).
//!
//! An agent creates a sandbox, writes scripts and files into it, runs them,
//! reads what they produced, and throws it away. Every run happens under the
//! same policy machinery as `sbx run`: the command may write only inside its
//! sandbox's folder, sees the network only as the operator allowed, gets a
//! cleaned environment, and is killed at its time limit along with anything
//! it started.
//!
//! # Who decides what
//!
//! The OPERATOR (whoever starts `sbx mcp`) sets the ceilings: how much
//! network a sandbox may ever have, which hosts, the longest a run may take.
//! The AGENT (whoever sends the tool calls) chooses within them, and gets
//! less by default: no network, no inherited secrets. A request above a
//! ceiling is refused, not quietly lowered, so the agent knows why. This
//! machine's own ports (`network.loopback_ports`) are never reachable from a
//! sandbox made here, even when the base policy names some.
//!
//! # Files the server touches for the agent
//!
//! `sandbox_write_file`, `_read_file` and `_list` run in the server, which is
//! NOT confined, on paths the agent names. So a path must be relative, may
//! not contain `..`, and no part of it may be a symlink (checked component by
//! component): a script run earlier could otherwise have left a link that
//! points the unconfined server out of the sandbox. Runs are one at a time,
//! and after each run the whole process group is killed, so nothing the
//! agent started is still running to swap a link in between the check and
//! the write.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

use crate::policy::{FsMode, Inherit, NetMode, Policy};
use crate::Sandbox;

/// What the operator allows every sandbox, at most.
#[derive(Debug, Clone)]
pub struct Ceilings {
    /// The most network a sandbox may ask for.
    pub network: NetMode,
    /// Under `proxy`: the hosts a sandbox may name (all of them when the
    /// ceiling is `allow`).
    pub hosts: Vec<String>,
    /// The longest one run may take.
    pub max_wall_secs: u64,
    /// The policy every sandbox starts from (environment, limits).
    pub base: Policy,
    /// Where sandboxes live.
    pub root: PathBuf,
    /// Leave sandboxes on disk when the server stops.
    pub keep: bool,
    /// Run with what can be enforced when not everything can.
    pub best_effort: bool,
}

impl Ceilings {
    /// The defaults: no network, ten minutes a run, an empty environment
    /// apart from the essentials, 4 GB of memory and 1 GB files per process.
    /// On Windows there is no file size limit: Windows cannot enforce one,
    /// and a default every run is refused for would make the server useless.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        let mut base = Policy::default();
        base.env.inherit = Inherit::None;
        base.limits.memory_mb = Some(4096);
        base.limits.file_size_mb = (!cfg!(windows)).then_some(1024);
        Self {
            network: NetMode::Deny,
            hosts: Vec::new(),
            max_wall_secs: 600,
            base,
            root,
            keep: false,
            best_effort: false,
        }
    }
}

impl Ceilings {
    /// The most a sandbox under these ceilings may ask for, as one policy:
    /// what a caller checks to know whether this machine can serve them
    /// (`sbx.exe` choosing between native Windows and WSL2).
    #[must_use]
    pub fn widest_policy(&self) -> Policy {
        let mut p = self.base.clone();
        p.fs.mode = FsMode::WorkspaceWrite;
        p.fs.write = vec![self.root.clone()];
        p.network.mode = self.network;
        p.network.allow = self.hosts.clone();
        p.network.loopback_ports = Vec::new();
        p.limits.wall_secs = Some(self.max_wall_secs);
        p
    }
}

struct Box_ {
    dir: PathBuf,
    network: NetMode,
    hosts: Vec<String>,
    runs: u32,
}

/// The server's state: its ceilings and the live sandboxes.
pub struct Server {
    ceilings: Ceilings,
    boxes: BTreeMap<String, Box_>,
    next: u32,
}

const MAX_OUTPUT: usize = 64 * 1024;
const MAX_READ: usize = 256 * 1024;
const DEFAULT_TIMEOUT: u64 = 120;

impl Server {
    #[must_use]
    pub fn new(ceilings: Ceilings) -> Self {
        Self {
            ceilings,
            boxes: BTreeMap::new(),
            next: 0,
        }
    }

    /// Serve MCP over `input`/`output` (newline-delimited JSON-RPC) until the
    /// input ends; then remove the sandboxes unless asked to keep them.
    ///
    /// # Errors
    /// When the output cannot be written.
    pub fn serve(&mut self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.handle(&line) {
                writeln!(output, "{reply}")?;
                output.flush()?;
            }
        }
        if !self.ceilings.keep {
            let ids: Vec<String> = self.boxes.keys().cloned().collect();
            for id in ids {
                let _ = self.destroy(&id);
            }
        }
        Ok(())
    }

    /// One JSON-RPC message in, the reply out (none for a notification).
    pub fn handle(&mut self, line: &str) -> Option<Value> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        let id = msg.get("id").cloned()?;
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => json!({
                "protocolVersion": params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("2025-06-18"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "sbx", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            }),
            "ping" => json!({}),
            "tools/list" => json!({ "tools": tools() }),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let (text, is_error) = match self.call(name, &args) {
                    Ok(v) => (pretty(&v), false),
                    Err(e) => (e, true),
                };
                json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
            }
            other => return Some(error(id, -32601, &format!("unknown method `{other}`"))),
        };
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    fn call(&mut self, tool: &str, args: &Value) -> Result<Value, String> {
        match tool {
            "sandbox_create" => self.create(args),
            "sandbox_write_file" => self.write_file(args),
            "sandbox_read_file" => self.read_file(args),
            "sandbox_list" => self.list(args),
            "sandbox_run" => self.run(args),
            "sandbox_info" => self.info(args),
            "sandbox_destroy" => {
                let id = str_arg(args, "id")?;
                self.destroy(id)?;
                Ok(json!({ "destroyed": id }))
            }
            other => Err(format!("unknown tool `{other}`")),
        }
    }

    /// `sandbox_create`: the network the agent asked for, checked against the
    /// ceilings ([`Self::requested_network`]) and against what this machine
    /// enforces ([`Self::refuse_unenforceable`]), then a fresh folder.
    fn create(&mut self, args: &Value) -> Result<Value, String> {
        let (network, hosts) = self.requested_network(args)?;
        self.refuse_unenforceable(network)?;
        self.next += 1;
        let id = format!("sbx-{}-{}", std::process::id(), self.next);
        let dir = self.ceilings.root.join(&id);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let dir = crate::plain_path(std::fs::canonicalize(&dir).map_err(|e| e.to_string())?);
        self.boxes.insert(
            id.clone(),
            Box_ {
                dir: dir.clone(),
                network,
                hosts,
                runs: 0,
            },
        );
        let info = self.info(&json!({ "id": id }))?;
        Ok(info)
    }

    /// The network mode and hosts `args` ask for, refused when above the
    /// ceilings: a mode above the ceiling's, a host outside the ceiling's
    /// list, `proxy` without hosts, or hosts without `proxy`.
    fn requested_network(&self, args: &Value) -> Result<(NetMode, Vec<String>), String> {
        let network = match args.get("network").and_then(Value::as_str) {
            None => NetMode::Deny,
            Some(w) => NetMode::parse(w).ok_or(format!("network: unknown mode `{w}`"))?,
        };
        if rank(network) > rank(self.ceilings.network) {
            return Err(format!(
                "network `{}` is above what this server allows (`{}`)",
                network.as_str(),
                self.ceilings.network.as_str()
            ));
        }
        let hosts: Vec<String> = match args.get("allow_hosts") {
            None => Vec::new(),
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|_| "allow_hosts: expected a list of host names".to_string())?,
        };
        if network != NetMode::Proxy {
            return if hosts.is_empty() {
                Ok((network, hosts))
            } else {
                Err("allow_hosts needs network `proxy`".to_string())
            };
        }
        if hosts.is_empty() {
            return Err("network `proxy` needs allow_hosts".to_string());
        }
        if self.ceilings.network == NetMode::Proxy {
            self.refuse_hosts_above_ceiling(&hosts)?;
        }
        crate::proxy::Allowlist::new(&hosts, false)?;
        Ok((network, hosts))
    }

    /// Each host must be one the ceiling's own allowlist lets through.
    fn refuse_hosts_above_ceiling(&self, hosts: &[String]) -> Result<(), String> {
        let ceiling = crate::proxy::Allowlist::new(&self.ceilings.hosts, false)?;
        for h in hosts {
            let (name, port) = match h.rsplit_once(':') {
                Some((n, p)) => (n, p.parse().unwrap_or(443)),
                None => (h.as_str(), 443),
            };
            if !ceiling.allows(name.trim_start_matches("*."), port) {
                return Err(format!(
                    "host `{h}` is not among the hosts this server allows: {}",
                    self.ceilings.hosts.join(", ")
                ));
            }
        }
        Ok(())
    }

    /// On native Windows, where `deny` and `proxy` need `sbx setup-windows`
    /// first, say so now rather than at every run, which would each be
    /// refused anyway. (Elsewhere enforcement depends on the kernel, and each
    /// run says.)
    fn refuse_unenforceable(&self, network: NetMode) -> Result<(), String> {
        if !cfg!(windows) || self.ceilings.best_effort {
            return Ok(());
        }
        let mut probe = Policy::default();
        probe.network.mode = network;
        match crate::enforcement(&probe).network {
            crate::Status::NotEnforced(why) => Err(format!(
                "network `{}` cannot be enforced on this machine: {why}{}",
                network.as_str(),
                if rank(self.ceilings.network) >= rank(NetMode::Allow) {
                    " (this server allows `allow`, if an open network will do)"
                } else {
                    ""
                }
            )),
            _ => Ok(()),
        }
    }

    fn sandbox(&self, args: &Value) -> Result<(&str, &Box_), String> {
        let id = str_arg(args, "id")?;
        self.boxes
            .get_key_value(id)
            .map(|(k, b)| (k.as_str(), b))
            .ok_or_else(|| format!("no sandbox `{id}` (create one with sandbox_create)"))
    }

    fn policy_for(&self, b: &Box_, timeout: u64) -> Policy {
        let mut p = self.ceilings.base.clone();
        p.fs.mode = FsMode::WorkspaceWrite;
        p.fs.write = vec![b.dir.clone()];
        p.network.mode = b.network;
        p.network.allow = b.hosts.clone();
        // This machine's own ports are never an agent's to reach, whatever
        // the operator's base policy says (`network.loopback_ports`).
        p.network.loopback_ports = Vec::new();
        p.limits.wall_secs = Some(timeout);
        p
    }

    fn info(&self, args: &Value) -> Result<Value, String> {
        let (id, b) = self.sandbox(args)?;
        let policy = self.policy_for(b, self.ceilings.max_wall_secs);
        let e = crate::enforcement(&policy);
        Ok(json!({
            "id": id,
            "folder": b.dir.display().to_string(),
            "network": b.network.as_str(),
            "allow_hosts": b.hosts,
            "max_timeout_secs": self.ceilings.max_wall_secs,
            "runs": b.runs,
            "enforcement": e,
        }))
    }

    fn write_file(&mut self, args: &Value) -> Result<Value, String> {
        let (_, b) = self.sandbox(args)?;
        let rel = str_arg(args, "path")?;
        let content = str_arg(args, "content")?;
        let path = inside(&b.dir, rel, true)?;
        write_new(&path, content.as_bytes())?;
        #[cfg(unix)]
        if args.get("executable").and_then(Value::as_bool) == Some(true) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
        }
        Ok(json!({ "written": rel, "bytes": content.len() }))
    }

    fn read_file(&self, args: &Value) -> Result<Value, String> {
        let (_, b) = self.sandbox(args)?;
        let rel = str_arg(args, "path")?;
        let path = inside(&b.dir, rel, false)?;
        let bytes = read_nofollow(&path)?;
        let max = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .map_or(MAX_READ, |m| (m as usize).min(MAX_READ));
        let shown = &bytes[..bytes.len().min(max)];
        Ok(json!({
            "path": rel,
            "bytes": bytes.len(),
            "truncated": bytes.len() > shown.len(),
            "content": String::from_utf8_lossy(shown),
        }))
    }

    fn list(&self, args: &Value) -> Result<Value, String> {
        let (_, b) = self.sandbox(args)?;
        let rel = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let dir = inside(&b.dir, rel, false)?;
        let mut entries = Vec::new();
        for e in std::fs::read_dir(&dir).map_err(|e| format!("{rel}: {e}"))? {
            let e = e.map_err(|e| e.to_string())?;
            let meta = e.path().symlink_metadata().map_err(|e| e.to_string())?;
            let kind = if meta.file_type().is_symlink() {
                "symlink"
            } else if meta.is_dir() {
                "dir"
            } else {
                "file"
            };
            entries.push(json!({
                "name": e.file_name().to_string_lossy(),
                "type": kind,
                "bytes": meta.len(),
            }));
        }
        entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!({ "path": rel, "entries": entries }))
    }

    fn run(&mut self, args: &Value) -> Result<Value, String> {
        let timeout = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT.min(self.ceilings.max_wall_secs));
        if timeout == 0 || timeout > self.ceilings.max_wall_secs {
            return Err(format!(
                "timeout_secs must be 1..={} on this server",
                self.ceilings.max_wall_secs
            ));
        }
        let (id, b) = self.sandbox(args)?;
        let id = id.to_string();
        let dir = b.dir.clone();
        let policy = self.policy_for(b, timeout);
        let cwd = match args.get("cwd").and_then(Value::as_str) {
            Some(rel) => inside(&dir, rel, false)?,
            None => dir.clone(),
        };

        let argv: Vec<String> = match (args.get("command"), args.get("script")) {
            (Some(c), None) => serde_json::from_value(c.clone())
                .map_err(|_| "command: expected a list like [\"python3\", \"x.py\"]".to_string())?,
            (None, Some(s)) => {
                let script = s.as_str().ok_or("script: expected text")?;
                let interpreter = args
                    .get("interpreter")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_INTERPRETER);
                let runs = self.boxes.get(&id).map_or(0, |b| b.runs) + 1;
                let (ext, _) = interpreter_style(interpreter, cfg!(windows));
                let name = format!(".sbx/script-{runs}{ext}");
                let path = inside(&dir, &name, true)?;
                write_new(&path, script.as_bytes())?;
                script_argv(interpreter, &path.display().to_string(), cfg!(windows))
            }
            _ => return Err("give either `command` (a list) or `script` (text)".to_string()),
        };
        let (program, rest) = argv.split_first().ok_or("command is empty")?;
        let rest: Vec<OsString> = rest.iter().map(OsString::from).collect();
        let stdin = args.get("stdin").and_then(Value::as_str);

        let captured = Sandbox::new(policy)
            .strict(!self.ceilings.best_effort)
            .run_captured(program, &rest, &cwd, stdin.map(str::as_bytes), MAX_OUTPUT)?;
        if let Some(b) = self.boxes.get_mut(&id) {
            b.runs += 1;
        }
        let refused: Vec<String> = captured
            .outcome
            .decisions
            .iter()
            .filter(|d| !d.allowed)
            .map(|d| format!("{} ({})", d.target, d.reason))
            .collect();
        Ok(json!({
            "exit_code": captured.outcome.code(),
            "timed_out": captured.outcome.timed_out,
            "stdout": String::from_utf8_lossy(&captured.stdout),
            "stderr": String::from_utf8_lossy(&captured.stderr),
            "stdout_truncated": captured.stdout_total > captured.stdout.len() as u64,
            "stderr_truncated": captured.stderr_total > captured.stderr.len() as u64,
            "network_refused": refused,
            "protected_restored": captured.outcome.restored,
        }))
    }

    fn destroy(&mut self, id: &str) -> Result<(), String> {
        let b = self
            .boxes
            .remove(id)
            .ok_or_else(|| format!("no sandbox `{id}`"))?;
        // Only ever a folder this server made beneath its root.
        let root = crate::plain_path(
            std::fs::canonicalize(&self.ceilings.root).map_err(|e| e.to_string())?,
        );
        if !b.dir.starts_with(&root) || b.dir == root {
            return Err(format!("{} is not a sandbox folder", b.dir.display()));
        }
        std::fs::remove_dir_all(&b.dir).map_err(|e| format!("{}: {e}", b.dir.display()))
    }
}

/// The interpreter a `script` runs with when none is named: `sh`, or
/// PowerShell on Windows, where there is no `sh` (and Git Bash cannot start
/// under the write-restricted token).
const DEFAULT_INTERPRETER: &str = if cfg!(windows) { "powershell" } else { "sh" };

/// On Windows, how an interpreter wants its script: the file extension it
/// needs (PowerShell runs only `.ps1` files, `cmd` only `.cmd`/`.bat`) and
/// the arguments before the script's path. Elsewhere: no extension, none.
fn interpreter_style(interpreter: &str, windows: bool) -> (&'static str, &'static [&'static str]) {
    if !windows {
        return ("", &[]);
    }
    let name = interpreter
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(interpreter)
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    match name {
        "powershell" | "pwsh" => (
            ".ps1",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ],
        ),
        "cmd" => (".cmd", &["/d", "/c"]),
        "python" | "python3" | "py" => (".py", &[]),
        "node" => (".js", &[]),
        _ => ("", &[]),
    }
}

/// The argv that runs the script at `path` with `interpreter`.
fn script_argv(interpreter: &str, path: &str, windows: bool) -> Vec<String> {
    let (_, flags) = interpreter_style(interpreter, windows);
    let mut argv = vec![interpreter.to_string()];
    argv.extend(flags.iter().map(|f| (*f).to_string()));
    argv.push(path.to_string());
    argv
}

fn rank(m: NetMode) -> u8 {
    match m {
        NetMode::Deny => 0,
        NetMode::Proxy => 1,
        NetMode::Allow => 2,
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("`{key}` is required"))
}

/// `rel` beneath `root`, refused when it is absolute, climbs with `..`, or
/// passes through a symlink. With `create`, missing parent folders are made
/// (each checked the same way).
pub fn inside(root: &Path, rel: &str, create: bool) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    let mut at = root.to_path_buf();
    let parts: Vec<Component> = rel_path.components().collect();
    for (i, part) in parts.iter().enumerate() {
        match part {
            Component::Normal(name) => {
                at.push(name);
                match at.symlink_metadata() {
                    Ok(m) if m.file_type().is_symlink() => {
                        return Err(format!("`{rel}` passes through a symlink; refused"));
                    }
                    Ok(_) => {}
                    Err(_) if create && i + 1 < parts.len() => {
                        std::fs::create_dir(&at).map_err(|e| format!("{rel}: {e}"))?;
                    }
                    Err(_) => {}
                }
            }
            Component::CurDir => {}
            _ => {
                return Err(format!(
                    "`{rel}` must be a relative path inside the sandbox"
                ))
            }
        }
    }
    Ok(at)
}

/// Write `bytes` to `path`, replacing a regular file, never following a link.
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Ok(m) = path.symlink_metadata() {
        if m.file_type().is_symlink() {
            return Err(format!("{} is a symlink; refused", path.display()));
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(bytes).map_err(|e| e.to_string())
}

fn read_nofollow(path: &Path) -> Result<Vec<u8>, String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut f, &mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

const INSTRUCTIONS: &str = "Sandboxes for running code safely. sandbox_create makes one (no \
network unless asked and allowed); sandbox_write_file puts scripts and files in it; sandbox_run \
runs a command or a script inside it, confined to its folder with a time limit; \
sandbox_read_file and sandbox_list read results; sandbox_destroy removes it. Paths are relative \
to the sandbox's folder.";

fn tools() -> Value {
    let id = json!({ "type": "string", "description": "The sandbox, from sandbox_create." });
    let path = json!({ "type": "string", "description": "Relative to the sandbox's folder." });
    json!([
        {
            "name": "sandbox_create",
            "description": "Create a sandbox: a private folder where commands run confined (they can write only there, get no secrets from the host's environment, and are killed at a time limit). Network is off unless you ask for `proxy` with allow_hosts (or `allow`), within what the server permits.",
            "inputSchema": { "type": "object", "properties": {
                "network": { "type": "string", "enum": ["deny", "proxy", "allow"], "description": "Default deny." },
                "allow_hosts": { "type": "array", "items": { "type": "string" }, "description": "With proxy: hosts like pypi.org, *.npmjs.org, example.com:8443." }
            }},
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "sandbox_write_file",
            "description": "Write a file (a script, data, a config) into the sandbox, creating folders as needed. Replaces an existing file.",
            "inputSchema": { "type": "object", "required": ["id", "path", "content"], "properties": {
                "id": id, "path": path,
                "content": { "type": "string" },
                "executable": { "type": "boolean", "description": "chmod 755 after writing." }
            }},
            "annotations": { "readOnlyHint": false, "destructiveHint": true }
        },
        {
            "name": "sandbox_run",
            "description": "Run a command (`command`, an argv list) or a script (`script`, text run with `interpreter`, default sh, or powershell on Windows) inside the sandbox. Returns exit_code, stdout, stderr (64 KB each at most), timed_out, and any network requests the proxy refused. Everything the run started is stopped when it ends.",
            "inputSchema": { "type": "object", "required": ["id"], "properties": {
                "id": id,
                "command": { "type": "array", "items": { "type": "string" } },
                "script": { "type": "string" },
                "interpreter": { "type": "string", "description": "For script: sh, bash, python3, node, ... (default sh; powershell on Windows, where cmd works too)" },
                "cwd": path,
                "stdin": { "type": "string" },
                "timeout_secs": { "type": "integer", "description": "Default 120, at most the server's limit." }
            }},
            "annotations": { "readOnlyHint": false, "destructiveHint": true }
        },
        {
            "name": "sandbox_read_file",
            "description": "Read a file from the sandbox (256 KB at most).",
            "inputSchema": { "type": "object", "required": ["id", "path"], "properties": {
                "id": id, "path": path, "max_bytes": { "type": "integer" }
            }},
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "sandbox_list",
            "description": "List a folder in the sandbox (default: its top).",
            "inputSchema": { "type": "object", "required": ["id"], "properties": { "id": id, "path": path }},
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "sandbox_info",
            "description": "What a sandbox enforces on this machine, its folder, network and limits.",
            "inputSchema": { "type": "object", "required": ["id"], "properties": { "id": id }},
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "sandbox_destroy",
            "description": "Delete a sandbox and everything in it.",
            "inputSchema": { "type": "object", "required": ["id"], "properties": { "id": id }},
            "annotations": { "readOnlyHint": false, "destructiveHint": true }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_and_never_follow_links() {
        let root = tempfile::tempdir().expect("tempdir");
        let base = std::fs::canonicalize(root.path()).expect("canon");
        assert!(inside(&base, "/etc/passwd", false).is_err());
        assert!(inside(&base, "../x", false).is_err());
        assert!(inside(&base, "a/../../x", false).is_err());
        let made = inside(&base, "a/b/c.txt", true).expect("nested");
        assert!(base.join("a/b").is_dir());
        assert_eq!(made, base.join("a/b/c.txt"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", base.join("out")).expect("link");
            assert!(inside(&base, "out/passwd", false).is_err());
            std::os::unix::fs::symlink("/etc/hostname", base.join("f")).expect("link");
            assert!(inside(&base, "f", false).is_err());
        }
    }

    #[test]
    fn requests_above_the_ceilings_are_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut c = Ceilings::new(root.path().to_path_buf());
        c.network = NetMode::Proxy;
        c.hosts = vec!["pypi.org".into(), "*.pythonhosted.org".into()];
        let mut s = Server::new(c);
        assert!(s.create(&json!({"network": "allow"})).is_err());
        assert!(s
            .create(&json!({"network": "proxy", "allow_hosts": ["evil.example"]}))
            .is_err());
        // Within the ceilings. Native Windows enforces proxy and deny only
        // after `sbx setup-windows`, and without it says so at once instead.
        let mut deny = Policy::default();
        deny.network.mode = NetMode::Deny;
        let unenforced = cfg!(windows) && crate::enforcement(&deny).network.is_gap();
        let within = |r: Result<Value, String>| {
            if unenforced {
                r.is_err_and(|e| e.contains("cannot be enforced"))
            } else {
                r.is_ok()
            }
        };
        assert!(within(s.create(
            &json!({"network": "proxy", "allow_hosts": ["pypi.org", "files.pythonhosted.org"]})
        )));
        assert!(within(s.create(&json!({}))), "deny is always allowed");
    }

    #[test]
    fn scripts_get_what_their_interpreter_needs_on_windows() {
        assert_eq!(
            script_argv("sh", "/b/.sbx/script-1", false),
            ["sh", "/b/.sbx/script-1"]
        );
        assert_eq!(interpreter_style("powershell", false), ("", &[][..]));
        assert_eq!(
            script_argv("powershell", r"C:\b\.sbx\script-1.ps1", true),
            [
                "powershell",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                r"C:\b\.sbx\script-1.ps1"
            ]
        );
        assert_eq!(interpreter_style(r"C:\x\PWSH.EXE", true).0, ".ps1");
        assert_eq!(
            script_argv("cmd", r"C:\b\s.cmd", true),
            ["cmd", "/d", "/c", r"C:\b\s.cmd"]
        );
        assert_eq!(interpreter_style("python3", true), (".py", &[][..]));
        assert_eq!(interpreter_style("ruby", true), ("", &[][..]));
    }

    #[test]
    fn the_widest_policy_is_what_the_ceilings_allow() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut c = Ceilings::new(root.path().to_path_buf());
        c.network = NetMode::Proxy;
        c.hosts = vec!["pypi.org".into()];
        let p = c.widest_policy();
        assert_eq!(p.fs.mode, FsMode::WorkspaceWrite);
        assert_eq!(p.network.mode, NetMode::Proxy);
        assert_eq!(p.network.allow, ["pypi.org"]);
        assert_eq!(p.limits.wall_secs, Some(600));
        assert_eq!(p.env.inherit, Inherit::None);
    }

    /// This machine's own ports in the operator's base policy reach no
    /// sandbox an agent makes: not its run policy, not the widest one.
    #[test]
    fn a_sandbox_never_gets_this_machines_ports() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut c = Ceilings::new(root.path().to_path_buf());
        c.network = NetMode::Proxy;
        c.hosts = vec!["pypi.org".into()];
        c.base.network.loopback_ports = vec![5173];
        assert!(c.widest_policy().network.loopback_ports.is_empty());
        let server = Server::new(c);
        let b = Box_ {
            dir: root.path().to_path_buf(),
            network: NetMode::Proxy,
            hosts: vec!["pypi.org".into()],
            runs: 0,
        };
        let p = server.policy_for(&b, 60);
        assert_eq!(p.network.allow, ["pypi.org"]);
        assert!(p.network.loopback_ports.is_empty(), "{p:?}");
    }

    #[test]
    fn the_protocol_answers_initialize_and_lists_the_tools() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut s = Server::new(Ceilings::new(root.path().to_path_buf()));
        let init = s
            .handle(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#)
            .expect("reply");
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(s
            .handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none());
        let list = s
            .handle(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .expect("reply");
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert_eq!(names.len(), 7);
        assert!(names.contains(&"sandbox_run"));
        let bad = s
            .handle(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"sandbox_run","arguments":{"id":"nope"}}}"#)
            .expect("reply");
        assert_eq!(bad["result"]["isError"], true);
    }
}
