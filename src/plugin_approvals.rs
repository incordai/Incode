//! Approval for a project's plugins: a plugin in `<workspace>/.incode/plugins/`
//! arrives with whatever was cloned, and its hooks run commands, its servers
//! start programs and its skills and commands are told to the agent. So none
//! of it is used until the operator approves that plugin, as it is now.
//!
//! The operator's own plugins (`~/.incode/plugins/`) need no approval: they
//! put them there.
//!
//! # What an approval is
//!
//! The plugin's name and a fingerprint of its folder ([`fingerprint`]): every
//! file's path and bytes, `.git` left out. Any change to the plugin — a new
//! hook, an edited server command — makes a new fingerprint, and the plugin is
//! left out again until it is approved again. The same rule as a project's
//! `.mcp.json` servers (`mcp_config::approval_key`).
//!
//! Kept in `<data dir>/plugin-approvals.json` as `{"approved": [keys]}`.
//!
//! # Wired to
//!
//! - [`usable`] is the one gate: `plugins::session_hooks`,
//!   `plugins::session_servers`, `plugins::session_skills` and
//!   `custom_commands::Registry::load` take their plugins through it.
//! - `incode plugin approve <name>` (`plugins::cli`) calls [`approve`];
//!   `incode plugin list` shows [`state`].

use std::collections::BTreeSet;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::plugins::{Plugin, Scope};

/// The store, in incode's data directory.
pub const FILE: &str = "plugin-approvals.json";

/// The folder left out of a fingerprint: its history, not its content.
const SKIPPED: &str = ".git";

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Store {
    #[serde(default)]
    approved: BTreeSet<String>,
}

/// A fingerprint of everything in `root`: each entry's path relative to it,
/// sorted, with a file's bytes, length-framed so no two folders hash alike by
/// accident. `.git` is left out.
///
/// # Errors
///
/// A file or folder could not be read, or one is a symbolic link — whose
/// target could change without the plugin changing, so it cannot be approved.
pub fn fingerprint(root: &Path) -> Result<String, String> {
    let mut entries = Vec::new();
    collect(root, root, &mut entries)?;
    entries.sort();
    let mut hasher = Sha256::new();
    for (relative, kind, content) in &entries {
        for part in [relative.as_bytes(), kind.as_bytes(), content.as_slice()] {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Every entry under `dir`, as (path relative to `root` with `/`, kind,
/// content). A symbolic link anywhere is an error: see the check below.
fn collect(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, &'static str, Vec<u8>)>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        if entry.file_name() == SKIPPED {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let meta =
            std::fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        // A link is refused, not fingerprinted: its target can sit outside
        // the plugin and change without the plugin changing, so an approval
        // could not cover what the loaders would read through it.
        if meta.file_type().is_symlink() {
            return Err(format!(
                "{} is a symbolic link; a project's plugin may not contain one",
                path.display()
            ));
        } else if meta.is_dir() {
            out.push((relative, "dir", Vec::new()));
            collect(root, &path, out)?;
        } else {
            let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            out.push((relative, "file", bytes));
        }
    }
    Ok(())
}

/// What `plugin` is approved as: its name and its folder's fingerprint.
///
/// # Errors
///
/// The folder could not be read.
pub fn key(plugin: &Plugin) -> Result<String, String> {
    Ok(format!(
        "{}:{}",
        plugin.manifest.name,
        fingerprint(&plugin.root)?
    ))
}

/// The approvals in `data_dir`. A missing file is none; one that cannot be
/// read or parsed is an error, said by the caller — never quietly none.
fn read(data_dir: &Path) -> Result<Store, String> {
    let path = data_dir.join(FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Approve `plugin` as it is now.
///
/// # Errors
///
/// The folder or the store could not be read, or the store not written.
pub fn approve(data_dir: &Path, plugin: &Plugin) -> Result<(), String> {
    approve_key(data_dir, &key(plugin)?)
}

/// Record `key` as approved. The store holds every kind of project content
/// the operator approved to run: a plugin ([`key`]), and a project's
/// lifecycle commands (`runtime::lifecycle::approval_key`).
///
/// # Errors
///
/// The store could not be read, or not written.
pub fn approve_key(data_dir: &Path, key: &str) -> Result<(), String> {
    let mut store = read(data_dir)?;
    store.approved.insert(key.to_string());
    std::fs::create_dir_all(data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    let path = data_dir.join(FILE);
    let text = serde_json::to_string_pretty(&store).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Whether `key` is approved. A store that cannot be read is an error, never
/// quietly "not approved" or "approved".
///
/// # Errors
///
/// The store could not be read or parsed.
pub fn is_approved(data_dir: &Path, key: &str) -> Result<bool, String> {
    Ok(read(data_dir)?.approved.contains(key))
}

/// Where `plugin` stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// The operator's own: no approval needed.
    Own,
    /// A project's, approved as it is now.
    Approved,
    /// A project's, not approved — or changed since it was.
    NotApproved,
    /// A project's whose approval could not be checked: why.
    Unknown(String),
}

/// Where `plugin` stands, against the approvals in `data_dir`.
#[must_use]
pub fn state(data_dir: &Path, plugin: &Plugin) -> State {
    if plugin.scope == Scope::User {
        return State::Own;
    }
    match (read(data_dir), key(plugin)) {
        (Ok(store), Ok(key)) if store.approved.contains(&key) => State::Approved,
        (Ok(_), Ok(_)) => State::NotApproved,
        (Err(why), _) | (_, Err(why)) => State::Unknown(why),
    }
}

/// The plugins that may be used: the operator's own, and a project's only
/// when approved as they are now. Each one left out is named, with how to
/// approve it.
#[must_use]
pub fn usable(data_dir: &Path, plugins: Vec<Plugin>) -> (Vec<Plugin>, Vec<String>) {
    let mut kept = Vec::new();
    let mut left_out = Vec::new();
    for plugin in plugins {
        match state(data_dir, &plugin) {
            State::Own | State::Approved => kept.push(plugin),
            State::NotApproved => left_out.push(format!(
                "plugin `{}` (this project's) is not used: it is not approved, or changed since \
                 it was. Look at {} and run `incode plugin approve {}` to use it",
                plugin.manifest.name,
                plugin.root.display(),
                plugin.manifest.name
            )),
            State::Unknown(why) => left_out.push(format!(
                "plugin `{}` (this project's) is not used: its approval could not be checked \
                 ({why})",
                plugin.manifest.name
            )),
        }
    }
    (kept, left_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, text).expect("write");
    }

    fn plugin(dir: &Path, scope: Scope) -> Plugin {
        let root = dir.join("cloned");
        write(
            &root.join("plugin.toml"),
            "name = \"cloned\"\nversion = \"1\"\n",
        );
        write(&root.join("hooks.json"), r#"{"pre_tool_use": ["check"]}"#);
        Plugin {
            manifest: crate::plugins::Manifest {
                name: "cloned".into(),
                version: "1".into(),
                description: String::new(),
            },
            root,
            scope,
        }
    }

    /// **A PROJECT'S PLUGIN IS NOT USED UNTIL APPROVED, AND NOT AFTER IT
    /// CHANGES.** The operator's own needs nothing.
    #[test]
    fn a_project_plugin_is_used_only_as_it_was_approved() {
        let dir = tempfile::tempdir().expect("dir");
        let data = tempfile::tempdir().expect("data");
        let project = plugin(dir.path(), Scope::Project);

        let (kept, left_out) = usable(data.path(), vec![project.clone()]);
        assert!(kept.is_empty());
        assert!(
            left_out[0].contains("incode plugin approve cloned"),
            "{left_out:?}"
        );

        approve(data.path(), &project).expect("approve");
        assert_eq!(state(data.path(), &project), State::Approved);
        assert_eq!(usable(data.path(), vec![project.clone()]).0.len(), 1);

        // A new hook after the approval: left out again.
        write(
            &project.root.join("hooks.json"),
            r#"{"pre_tool_use": ["curl evil"]}"#,
        );
        assert_eq!(state(data.path(), &project), State::NotApproved);
        assert!(usable(data.path(), vec![project]).0.is_empty());

        let own = plugin(&dir.path().join("mine"), Scope::User);
        assert_eq!(state(data.path(), &own), State::Own);
    }

    /// The fingerprint covers every file and its bytes, and leaves out `.git`.
    #[test]
    fn the_fingerprint_follows_content_not_history() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        write(&root.join("a.txt"), "one");
        let first = fingerprint(root).expect("fingerprint");
        write(&root.join(".git/HEAD"), "ref: x");
        assert_eq!(
            fingerprint(root).expect("fingerprint"),
            first,
            ".git counted"
        );
        write(&root.join("sub/b.txt"), "two");
        let second = fingerprint(root).expect("fingerprint");
        assert_ne!(second, first, "a new file was not counted");
        write(&root.join("sub/b.txt"), "TWO");
        assert_ne!(
            fingerprint(root).expect("fingerprint"),
            second,
            "an edit was not counted"
        );
    }

    /// **A LINK IS NOT APPROVABLE.** A plugin file pointing outside the plugin
    /// could change without the fingerprint changing, so a plugin holding one
    /// cannot be approved and is left out, saying why.
    #[test]
    fn a_plugin_with_a_symbolic_link_cannot_be_approved() {
        let dir = tempfile::tempdir().expect("dir");
        let data = tempfile::tempdir().expect("data");
        let outside = dir.path().join("outside.json");
        std::fs::write(&outside, r#"{"pre_tool_use": ["check"]}"#).expect("outside");
        let project = plugin(dir.path(), Scope::Project);
        std::fs::remove_file(project.root.join("hooks.json")).expect("remove");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, project.root.join("hooks.json"));
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&outside, project.root.join("hooks.json"));
        if let Err(e) = linked {
            eprintln!(
                "a_plugin_with_a_symbolic_link_cannot_be_approved: SKIPPED (no symlink: {e})"
            );
            return;
        }
        assert!(fingerprint(&project.root)
            .expect_err("a link was fingerprinted")
            .contains("symbolic link"));
        assert!(approve(data.path(), &project).is_err());
        let (kept, left_out) = usable(data.path(), vec![project]);
        assert!(kept.is_empty());
        assert!(left_out[0].contains("symbolic link"), "{left_out:?}");
    }

    /// A store that cannot be read is said, never taken as "nothing approved"
    /// silently, nor as "everything".
    #[test]
    fn a_broken_store_is_reported_not_trusted() {
        let dir = tempfile::tempdir().expect("dir");
        let data = tempfile::tempdir().expect("data");
        std::fs::write(data.path().join(FILE), "{not json").expect("broken");
        let project = plugin(dir.path(), Scope::Project);
        assert!(matches!(state(data.path(), &project), State::Unknown(_)));
        let (kept, left_out) = usable(data.path(), vec![project]);
        assert!(kept.is_empty());
        assert!(left_out[0].contains("could not be checked"), "{left_out:?}");
        assert!(approve(data.path(), &plugin(dir.path(), Scope::Project)).is_err());
    }
}
