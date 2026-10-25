//! External tools: programs incode downloads from a configured release and
//! runs, never from `PATH` and never from a folder named on one machine.
//!
//! # The entry
//!
//! Each tool is one entry of `admin.json` `tools`:
//!
//! ```json
//! { "name": "browser", "release": "<owner>/<repo>", "version": "latest",
//!   "asset": "<prefix>-{arch}-{os}.{ext}", "run": ["<program>", "mcp"],
//!   "provides": "browser" }
//! ```
//!
//! `{os}` is `windows`, `macos` or `linux`; `{arch}` is `x86_64` or
//! `aarch64`; `{ext}` is `zip` on Windows and `tar.gz` elsewhere. A repo that
//! names its assets otherwise gives an `assets` map from `<arch>-<os>` to the
//! file name. A source that is not a GitHub release gives `url` instead of
//! `release`: a template over the same names and `{version}`.
//!
//! # Built-in entries
//!
//! `defaults/tools.json` ([`defaults`]) holds entries offered beside the
//! owner's: today the headless browser. Each is left out when the owner has
//! an entry of the same name or one that `provides` the same capability, and
//! all of them when `admin.json` says `"default_tools": false`
//! ([`in_effect`]). They are installed exactly like the
//! owner's, through the same checks.
//!
//! # Installing
//!
//! Only when asked: `/tools add <name>`, `/tools update [name]`,
//! `/tools remove <name>` (the REPL and ACP), or `incode --install-tools` for
//! every entry, which the installers (`scripts/install.ps1`,
//! `scripts/install.sh`) run at the end of an install. A session that finds
//! an entry not installed says how to install it, and downloads nothing.
//!
//! 1. The release is resolved through GitHub's API ([`fetch::GITHUB_API`]):
//!    `releases/latest`, or `releases/tags/<version>`.
//! 2. The asset's sha256 is known BEFORE it is downloaded: GitHub's `digest`
//!    for the asset (`sha256:<hex>`), or a `sha256` pinned in the entry. With
//!    neither the install is refused; with both they must agree.
//! 3. It is downloaded over https (plain http only to this machine), no
//!    redirect followed to anything else, bounded in size and time
//!    ([`fetch`]), and hashed as it arrives. A mismatch is refused and nothing
//!    is unpacked, so an unverified program is never run.
//! 4. It is unpacked — zip, or tar.gz — into a staging folder, refusing any
//!    entry that would land outside it, links, and archives past their bounds
//!    ([`unpack`]); then moved to `<data dir>/tools/<name>/<version>/`, and a
//!    manifest (`installed.json`) records the version, the asset and its sha.
//!
//! # Running
//!
//! `run[0]` is resolved inside the tool's installed folder, never on `PATH`.
//! Each installed tool is started as an MCP server on stdio, at session start,
//! in the background, in every mode: [`session_servers`] hands the specs to
//! the same connection code an ACP client's servers use
//! (`toolbox::client_servers`), so its tools are `mcp__<name>__<tool>` and go
//! through the toolbox's one gate like any other.
//!
//! An entry with `provides: "browser"` is also the headless browser incode's
//! own Screenshot, RenderPdf and WebFetch-with-script run ([`provider`]).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

pub(crate) mod fetch;
pub(crate) mod unpack;

/// What the manifest of an installed tool is called, in its folder.
const MANIFEST: &str = "installed.json";
/// The longest tool name accepted. It becomes a folder name and part of every
/// tool name the model sees (`mcp__<name>__<tool>`, at most 64 characters).
const MAX_NAME: usize = 24;

/// One external tool, as `admin.json` `tools` names it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEntry {
    /// The tool's name: its folder, its MCP server's name, and what `/tools`
    /// commands take. Letters, digits, `-` and `_`.
    pub name: String,
    /// A GitHub release source, as `<owner>/<repo>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// Any other https source: a template over `{version}`, `{os}`, `{arch}`
    /// and `{ext}`. Needs `sha256`, since nothing else can vouch for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `latest`, or a release tag. Unset is `latest`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The asset's file name, as a template over `{version}`, `{os}`,
    /// `{arch}` and `{ext}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    /// The asset's file name per target (`x86_64-windows`, `aarch64-macos`,
    /// …), for a repo whose names do not follow one template. Wins over
    /// `asset` for the targets it names.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub assets: BTreeMap<String, String>,
    /// The asset's sha256, pinned: one hex digest, or a map from target to
    /// digest. Checked with GitHub's own digest when both exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<Pinned>,
    /// The program and its arguments. `run[0]` is a path inside the unpacked
    /// release, never a name looked up on `PATH`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub run: Vec<String>,
    /// A capability incode's own tools take from this one: `browser`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provides: Option<String>,
}

/// A pinned sha256: one for every target, or one per target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Pinned {
    One(String),
    PerTarget(BTreeMap<String, String>),
}

/// The platform a release asset is chosen for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) os: &'static str,
    pub(crate) arch: &'static str,
}

impl Target {
    /// This machine, or `None` on a platform no release is built for.
    pub(crate) fn current() -> Option<Self> {
        let os = if cfg!(windows) {
            "windows"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            return None;
        };
        let arch = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else {
            return None;
        };
        Some(Self { os, arch })
    }

    /// The archive a release ships for this OS.
    pub(crate) fn ext(self) -> &'static str {
        if self.os == "windows" {
            "zip"
        } else {
            "tar.gz"
        }
    }

    /// `<arch>-<os>`, the key of an `assets` or `sha256` map.
    pub(crate) fn key(self) -> String {
        format!("{}-{}", self.arch, self.os)
    }

    /// `template` with `{os}`, `{arch}`, `{ext}` and `{version}` filled in.
    pub(crate) fn expand(self, template: &str, version: &str) -> String {
        template
            .replace("{os}", self.os)
            .replace("{arch}", self.arch)
            .replace("{ext}", self.ext())
            .replace("{version}", version)
    }
}

/// What `installed.json` records about an installed tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) name: String,
    /// The release tag, or the version the entry named.
    pub(crate) version: String,
    /// The folder under the tool's folder holding this version.
    pub(crate) dir: String,
    /// The asset's file name.
    pub(crate) asset: String,
    /// The sha256 the download was checked against, and matched.
    pub(crate) sha256: String,
    /// Where it came from: `<owner>/<repo>`, or the address it was fetched
    /// from.
    pub(crate) source: String,
}

impl ToolEntry {
    /// Whether the name is 1 to [`MAX_NAME`] letters, digits, `-` or `_`.
    fn name_ok(&self) -> bool {
        !self.name.is_empty()
            && self.name.len() <= MAX_NAME
            && self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }

    /// Whether the entry can be installed and run at all; `Err` says why not.
    /// Its name, then its source ([`Self::check_source`]), its version
    /// ([`Self::check_version`]) and the program it runs.
    pub(crate) fn check(&self) -> Result<(), String> {
        let name = &self.name;
        if !self.name_ok() {
            return Err(format!(
                "tool `{name}`: a name is 1 to {MAX_NAME} letters, digits, `-` or `_`"
            ));
        }
        self.check_source()?;
        self.check_version()?;
        let Some(program) = self.run.first() else {
            return Err(format!(
                "tool `{name}`: `run` names the program inside the release, and its arguments"
            ));
        };
        inside_path(program).map_err(|why| format!("tool `{name}`: `run[0]` {why}"))?;
        Ok(())
    }

    /// Exactly one source: `release` as `<owner>/<repo>` of plain names, or
    /// an http(s) `url`.
    fn check_source(&self) -> Result<(), String> {
        let name = &self.name;
        match (&self.release, &self.url) {
            (Some(release), None) => {
                let part_ok = |p: &str| {
                    !p.is_empty()
                        && p != "."
                        && p != ".."
                        && p.chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                };
                let parts: Vec<&str> = release.split('/').collect();
                if parts.len() == 2 && parts.iter().all(|p| part_ok(p)) {
                    Ok(())
                } else {
                    Err(format!(
                        "tool `{name}`: `release` is `<owner>/<repo>`, not `{release}`"
                    ))
                }
            }
            (None, Some(url)) if url.starts_with("https://") || url.starts_with("http://") => {
                Ok(())
            }
            (None, Some(_)) => Err(format!("tool `{name}`: `url` must be an https address")),
            (Some(_), Some(_)) => Err(format!("tool `{name}`: give `release` or `url`, not both")),
            (None, None) => Err(format!(
                "tool `{name}`: needs `release` (`<owner>/<repo>`) or `url`"
            )),
        }
    }

    /// `latest` or a tag of letters, digits, `.`, `-`, `_` or `+`; and a
    /// `url` that names `{version}` needs a real one.
    fn check_version(&self) -> Result<(), String> {
        let name = &self.name;
        let version = self.version();
        let plain = !version.is_empty()
            && version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'));
        if !plain {
            return Err(format!(
                "tool `{name}`: `version` is `latest` or a tag of letters, digits, `.`, `-`, `_` \
                 or `+`"
            ));
        }
        if self.url.is_some() && version == "latest" && self.url_template_uses_version() {
            return Err(format!(
                "tool `{name}`: a `url` source cannot resolve `latest`; name the version"
            ));
        }
        Ok(())
    }

    fn url_template_uses_version(&self) -> bool {
        self.url.as_deref().is_some_and(|u| u.contains("{version}"))
    }

    /// The version asked for: `latest` when unset.
    pub(crate) fn version(&self) -> &str {
        self.version
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or("latest")
    }

    /// The asset's file name for `target`, once the version is known.
    pub(crate) fn asset_for(&self, target: Target, version: &str) -> Result<String, String> {
        if let Some(named) = self.assets.get(&target.key()) {
            return Ok(target.expand(named, version));
        }
        match &self.asset {
            Some(template) => Ok(target.expand(template, version)),
            None if self.url.is_some() => {
                // A url source names the file in the address itself.
                let url = target.expand(self.url.as_deref().unwrap_or_default(), version);
                Ok(url
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .split('?')
                    .next()
                    .unwrap_or_default()
                    .to_string())
            }
            None => Err(format!(
                "tool `{}`: no asset is named for {} (give `asset`, or `assets.{}`)",
                self.name,
                target.key(),
                target.key()
            )),
        }
    }

    /// The sha256 pinned for `target`, lowercase, if any.
    pub(crate) fn pinned_for(&self, target: Target) -> Option<String> {
        let pinned = match self.sha256.as_ref()? {
            Pinned::One(hex) => hex.clone(),
            Pinned::PerTarget(map) => map.get(&target.key())?.clone(),
        };
        Some(
            pinned
                .trim()
                .trim_start_matches("sha256:")
                .to_ascii_lowercase(),
        )
    }
}

/// Where `path` points inside an installed folder: `Ok` when it is relative
/// and made only of plain names, so it can never leave the folder.
pub(crate) fn inside_path(path: &str) -> Result<PathBuf, String> {
    let as_path = Path::new(path);
    if path.is_empty() {
        return Err("is empty".to_string());
    }
    let mut out = PathBuf::new();
    for component in as_path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => {
                return Err(format!(
                    "`{path}` must be a path inside the release, with no `..`, root or drive"
                ))
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(format!("`{path}` names no file"));
    }
    Ok(out)
}

/// The tools a session runs and an install installs: the owner's `tools`,
/// then each built-in entry ([`defaults`]) the owner has not replaced — one
/// whose name the owner already uses, or whose capability an owner's entry
/// already `provides`, is left out. None of them when `default_tools` is
/// `false`.
///
/// Read by every consumer of the tool list: [`install_all`] and [`command`]
/// here, the REPL's and a task run's `connect_ext_tools` (`cli/mod.rs`,
/// `task_run.rs`), the ACP agent's `tool_servers` (`acp_agent/mod.rs`), and
/// `Toolbox::detect_browser` (`toolbox/mod.rs`). A consumer reading
/// `admin.tools` directly would silently lose the built-in entries.
#[must_use]
pub fn in_effect(admin: &crate::admin::AdminConfig) -> Vec<ToolEntry> {
    let mut tools = admin.tools.clone();
    if admin.default_tools == Some(false) {
        return tools;
    }
    for default in defaults() {
        let replaced = admin.tools.iter().any(|own| {
            own.name == default.name
                || (default.provides.is_some() && own.provides == default.provides)
        });
        if !replaced {
            tools.push(default);
        }
    }
    tools
}

/// The built-in entries: `defaults/tools.json`, compiled in. Data, like
/// `defaults/agents`, so no program is named in code. Joined to the owner's
/// `tools` only by [`in_effect`].
pub(crate) fn defaults() -> Vec<ToolEntry> {
    static DEFAULTS: std::sync::OnceLock<Vec<ToolEntry>> = std::sync::OnceLock::new();
    DEFAULTS
        .get_or_init(|| {
            // The file is part of this build and a test parses and checks
            // every entry in it, so a malformed one never ships.
            serde_json::from_str(include_str!("../../defaults/tools.json"))
                .expect("defaults/tools.json is a list of tool entries")
        })
        .clone()
}

/// Where every tool is installed: `<data dir>/tools`.
pub(crate) fn tools_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("tools")
}

/// The manifest of `name`'s installed version, if it is installed.
pub(crate) fn manifest(data_dir: &Path, name: &str) -> Option<Manifest> {
    let text = std::fs::read_to_string(tools_dir(data_dir).join(name).join(MANIFEST)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The program `entry` runs, inside its installed folder: `None` when it is
/// not installed, or the file is missing. On Windows `run[0]` may leave off
/// `.exe`. `PATH` is never consulted.
pub(crate) fn executable(entry: &ToolEntry, data_dir: &Path) -> Option<PathBuf> {
    let installed = manifest(data_dir, &entry.name)?;
    let folder = tools_dir(data_dir)
        .join(&entry.name)
        .join(inside_path(&installed.dir).ok()?);
    program_in(&folder, entry.run.first()?)
}

/// `program` inside `folder`, as a file that exists.
fn program_in(folder: &Path, program: &str) -> Option<PathBuf> {
    let path = folder.join(inside_path(program).ok()?);
    if path.is_file() {
        return Some(path);
    }
    if cfg!(windows) && path.extension().is_none() {
        let exe = path.with_extension("exe");
        if exe.is_file() {
            return Some(exe);
        }
    }
    None
}

/// The installed tool that provides `capability`, if any: its `name` as the
/// owner wrote it, and its executable.
pub(crate) fn provider(
    capability: &str,
    tools: &[ToolEntry],
    data_dir: &Path,
) -> Option<(String, PathBuf)> {
    tools
        .iter()
        .filter(|t| t.provides.as_deref() == Some(capability))
        .find_map(|t| executable(t, data_dir).map(|program| (t.name.clone(), program)))
}

/// The MCP servers a session starts for the installed tools, and a line for
/// each configured tool that is not installed (how to install it) or cannot
/// be used (why). Nothing is downloaded here.
pub(crate) fn session_servers(
    tools: &[ToolEntry],
    data_dir: &Path,
) -> (Vec<crate::toolbox::ServerSpec>, Vec<String>) {
    let mut specs = Vec::new();
    let mut said = Vec::new();
    for entry in tools {
        if let Err(why) = entry.check() {
            said.push(format!("{why}; it is not started"));
            continue;
        }
        match executable(entry, data_dir) {
            Some(command) => specs.push(crate::toolbox::ServerSpec {
                name: entry.name.clone(),
                transport: crate::toolbox::Transport::Stdio {
                    command,
                    args: entry.run.iter().skip(1).cloned().collect(),
                    env: Vec::new(),
                },
            }),
            None => said.push(format!(
                "tool `{0}` is configured but not installed: `/tools add {0}` (or `incode \
                 --install-tools`) downloads and checks it",
                entry.name
            )),
        }
    }
    (specs, said)
}

/// What an install did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Installed {
    /// Downloaded, checked and unpacked.
    Fresh(Manifest),
    /// This version, with this sha, was already there.
    Already(Manifest),
}

/// Install `entry`'s release, or its newer one, under `data_dir`. `api` is
/// GitHub's API root ([`fetch::GITHUB_API`]; a test serves its own).
///
/// The steps, in order: [`resolve_release`] (what to fetch), then
/// [`expected_sha256`] (what it must hash to, known BEFORE fetching), then
/// [`download_and_unpack`] into a staging folder, [`place_version`] to swap
/// it in, [`write_manifest`], and [`remove_old_versions`].
pub(crate) fn install(entry: &ToolEntry, data_dir: &Path, api: &str) -> Result<Installed, String> {
    entry.check()?;
    let target =
        Target::current().ok_or_else(|| "no release is built for this platform".to_string())?;
    let name = &entry.name;
    let resolved = resolve_release(entry, target, api)?;
    let expected = expected_sha256(entry, &resolved, target)?;
    let dir = safe_dir(&resolved.version);
    if let Some(have) = manifest(data_dir, name) {
        if have.version == resolved.version
            && have.sha256 == expected
            && executable(entry, data_dir).is_some()
        {
            return Ok(Installed::Already(have));
        }
    }
    let home = tools_dir(data_dir).join(name);
    std::fs::create_dir_all(&home)
        .map_err(|e| format!("could not make {}: {e}", home.display()))?;
    let part = home.join(format!(".download-{}", uuid::Uuid::new_v4()));
    let staging = home.join(format!(".staging-{}", uuid::Uuid::new_v4()));
    let result = download_and_unpack(entry, &resolved, &expected, &part, &staging);
    // The download goes whatever happened; the staging folder goes unless it
    // became the installed version below.
    let _ = std::fs::remove_file(&part);
    if let Err(why) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!("tool `{name}`: {why}"));
    }
    place_version(name, &staging, &home.join(&dir))?;
    let written = Manifest {
        name: name.clone(),
        version: resolved.version.clone(),
        dir: dir.clone(),
        asset: resolved.asset.clone(),
        sha256: expected,
        source: resolved.source.clone(),
    };
    write_manifest(&home, &written)?;
    remove_old_versions(&home, &dir);
    Ok(Installed::Fresh(written))
}

/// What to fetch for `entry` on `target`: from GitHub's API for a `release`
/// entry, or the `url` template filled in (no digest: the entry must pin
/// one).
fn resolve_release(
    entry: &ToolEntry,
    target: Target,
    api: &str,
) -> Result<fetch::Resolved, String> {
    if let Some(release) = &entry.release {
        return fetch::resolve_github(api, release, entry, target);
    }
    let version = entry.version().to_string();
    let url = target.expand(entry.url.as_deref().unwrap_or_default(), &version);
    Ok(fetch::Resolved {
        asset: entry.asset_for(target, &version)?,
        version,
        source: url.clone(),
        url,
        digest: None,
        size: None,
    })
}

/// The sha256 the download must have: the release's digest or the entry's
/// pin, which must agree when both exist. Neither, or a value that is not
/// 64 hex digits, is refused before anything is fetched.
fn expected_sha256(
    entry: &ToolEntry,
    resolved: &fetch::Resolved,
    target: Target,
) -> Result<String, String> {
    let name = &entry.name;
    let expected = match (&resolved.digest, &entry.pinned_for(target)) {
        (Some(released), Some(pinned)) if released != pinned => {
            return Err(format!(
                "tool `{name}`: refused — the release says sha256 {released} for {}, the \
                 entry pins {pinned}; nothing was downloaded",
                resolved.asset
            ))
        }
        (Some(sha), _) | (None, Some(sha)) => sha.clone(),
        (None, None) => {
            return Err(format!(
                "tool `{name}`: refused — {} has no sha256 to check it against (the release \
                 publishes no digest for it, and the entry pins none); nothing was downloaded",
                resolved.asset
            ))
        }
    };
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "tool `{name}`: `{expected}` is not a sha256 (64 hex digits)"
        ));
    }
    Ok(expected)
}

/// Swap the checked `staging` folder in as `folder`, replacing a copy of the
/// same version. On any failure the staging folder is removed and nothing
/// half-installed is left.
fn place_version(name: &str, staging: &Path, folder: &Path) -> Result<(), String> {
    if folder.exists() {
        std::fs::remove_dir_all(folder).map_err(|e| {
            let _ = std::fs::remove_dir_all(staging);
            format!(
                "tool `{name}`: {} is in use or locked ({e}); stop what runs it and try again",
                folder.display()
            )
        })?;
    }
    std::fs::rename(staging, folder).map_err(|e| {
        let _ = std::fs::remove_dir_all(staging);
        format!(
            "tool `{name}`: could not move it into {}: {e}",
            folder.display()
        )
    })
}

/// Record what was installed in `home`'s `installed.json`.
fn write_manifest(home: &Path, written: &Manifest) -> Result<(), String> {
    let name = &written.name;
    let text = serde_json::to_string_pretty(written)
        .map_err(|e| format!("tool `{name}`: the manifest could not be encoded: {e}"))?;
    std::fs::write(home.join(MANIFEST), text)
        .map_err(|e| format!("tool `{name}`: the manifest could not be written: {e}"))
}

/// Remove every version folder in `home` but `keep`. One still running
/// (Windows keeps its files locked) stays until the next update.
fn remove_old_versions(home: &Path, keep: &str) {
    let Ok(read) = std::fs::read_dir(home) else {
        return;
    };
    for old in read.flatten() {
        let is_old_version =
            old.file_type().is_ok_and(|t| t.is_dir()) && old.file_name().to_string_lossy() != keep;
        if is_old_version {
            let _ = std::fs::remove_dir_all(old.path());
        }
    }
}

/// Download the asset to `part`, check it against `expected`, and unpack it
/// into `staging`, where `run[0]` must then be.
fn download_and_unpack(
    entry: &ToolEntry,
    resolved: &fetch::Resolved,
    expected: &str,
    part: &Path,
    staging: &Path,
) -> Result<(), String> {
    let got = fetch::download(&resolved.url, resolved.size, part)?;
    if got != expected {
        return Err(format!(
            "refused — {} has sha256 {got}, not the {expected} it must have; it was deleted \
             and nothing was unpacked",
            resolved.asset
        ));
    }
    unpack::unpack(part, &resolved.asset, staging)?;
    let program = entry.run.first().map(String::as_str).unwrap_or_default();
    if program_in(staging, program).is_none() {
        return Err(format!(
            "{} holds no `{program}` (the entry's `run[0]`), so nothing was installed",
            resolved.asset
        ));
    }
    Ok(())
}

/// A version as a folder name: anything but letters, digits, `.`, `-`, `_`
/// and `+` becomes `_`, and a name of dots only is never produced.
fn safe_dir(version: &str) -> String {
    let dir: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if dir.chars().all(|c| c == '.') {
        format!("v{}", dir.replace('.', "_"))
    } else {
        dir
    }
}

/// Remove `name`'s installed files. Its entry in `admin.json` stays.
pub(crate) fn remove(data_dir: &Path, name: &str) -> Result<String, String> {
    let named = ToolEntry {
        name: name.to_string(),
        ..ToolEntry::default()
    };
    if !named.name_ok() {
        return Err(format!("`{name}` is not a tool name"));
    }
    let home = tools_dir(data_dir).join(name);
    if !home.exists() {
        return Ok(format!("tool `{name}` is not installed"));
    }
    std::fs::remove_dir_all(&home).map_err(|e| {
        format!(
            "tool `{name}` could not be removed ({e}); a session that is running it keeps its \
             files open — end it and try again"
        )
    })?;
    Ok(format!(
        "tool `{name}` removed; its entry in admin.json stays, so a session will say it is \
         not installed until the entry is deleted"
    ))
}

/// One line for an install's outcome.
fn said(outcome: &Result<Installed, String>) -> String {
    match outcome {
        Ok(Installed::Fresh(m)) => format!(
            "tool `{}` {} installed from {} ({}, sha256 {} checked); its tools are offered from \
             the next session",
            m.name, m.version, m.source, m.asset, m.sha256
        ),
        Ok(Installed::Already(m)) => format!(
            "tool `{}` {} is already installed (sha256 {})",
            m.name, m.version, m.sha256
        ),
        Err(why) => why.clone(),
    }
}

/// Install every configured tool: `incode --install-tools`. Returns a line
/// per tool, and whether every one is installed.
#[must_use]
pub fn install_all(data_dir: &Path) -> (Vec<String>, bool) {
    let tools = match crate::admin::AdminConfig::load(data_dir) {
        Ok(admin) => in_effect(&admin),
        Err(why) => return (vec![why], false),
    };
    if tools.is_empty() {
        return (vec![no_tools_line()], true);
    }
    let mut ok = true;
    let lines = tools
        .iter()
        .map(|entry| {
            let outcome = install(entry, data_dir, fetch::GITHUB_API);
            ok &= outcome.is_ok();
            said(&outcome)
        })
        .collect();
    (lines, ok)
}

fn no_tools_line() -> String {
    "no tools are configured: add an entry to `tools` in admin.json (name, release, version, \
     asset, run, provides), then `/tools add <name>`"
        .to_string()
}

/// `/tools add <name>…`, `/tools update [name]`, `/tools remove <name>`, or
/// with no argument the configured tools and what is installed. Returns what
/// to say, a line at a time.
pub(crate) fn command(arg: &str, data_dir: &Path) -> Vec<String> {
    let mut words = arg.split_whitespace();
    let verb = words.next().unwrap_or_default().to_ascii_lowercase();
    let names: Vec<&str> = words.collect();
    let tools = match crate::admin::AdminConfig::load(data_dir) {
        Ok(admin) => in_effect(&admin),
        Err(why) => return vec![why],
    };
    let find = |name: &str| tools.iter().find(|t| t.name == name);
    let unknown = |name: &str| {
        let known: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        if known.is_empty() {
            format!("no tool `{name}`: {}", no_tools_line())
        } else {
            format!(
                "no tool `{name}` in admin.json `tools` (configured: {})",
                known.join(", ")
            )
        }
    };
    match verb.as_str() {
        "" | "list" => status(&tools, data_dir),
        "add" | "install" if names.is_empty() => {
            vec!["usage: /tools add <name> [<name>…]".to_string()]
        }
        "add" | "install" | "update" => {
            let chosen: Vec<&ToolEntry> = if names.is_empty() {
                tools.iter().collect()
            } else {
                names.iter().filter_map(|n| find(n)).collect()
            };
            let mut lines: Vec<String> = names
                .iter()
                .filter(|n| find(n).is_none())
                .map(|n| unknown(n))
                .collect();
            if chosen.is_empty() && lines.is_empty() {
                lines.push(no_tools_line());
            }
            lines.extend(
                chosen
                    .into_iter()
                    .map(|entry| said(&install(entry, data_dir, fetch::GITHUB_API))),
            );
            lines
        }
        "remove" | "uninstall" if names.is_empty() => {
            vec!["usage: /tools remove <name>".to_string()]
        }
        "remove" | "uninstall" => names
            .iter()
            .map(|name| remove(data_dir, name).unwrap_or_else(|why| why))
            .collect(),
        other => vec![format!(
            "unknown `/tools {other}`: /tools, /tools add <name>, /tools update [name], \
             /tools remove <name>"
        )],
    }
}

/// A line per configured tool: installed (which version, which sha) or not.
fn status(tools: &[ToolEntry], data_dir: &Path) -> Vec<String> {
    if tools.is_empty() {
        return vec![no_tools_line()];
    }
    tools
        .iter()
        .map(|entry| {
            let provides = entry
                .provides
                .as_deref()
                .map(|p| format!(", provides {p}"))
                .unwrap_or_default();
            match (manifest(data_dir, &entry.name), executable(entry, data_dir)) {
                (Some(m), Some(program)) => format!(
                    "tool `{}`: {} installed ({}, sha256 {}){provides} — runs {}",
                    entry.name,
                    m.version,
                    m.asset,
                    m.sha256,
                    program.display()
                ),
                _ => format!(
                    "tool `{0}`: not installed{provides} — `/tools add {0}`",
                    entry.name
                ),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
