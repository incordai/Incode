//! The harness: a folder of instructions, routing and sub-agents that Insi's
//! improvement loop changes, named for one run by [`HARNESS_ENV`].
//!
//! Insi scores a candidate harness by running incode with this variable set
//! to the candidate's folder. What incode takes from it:
//!
//! | File | What it changes |
//! | --- | --- |
//! | `instructions.md` | Standing instructions in the system context, after the project's own `CLAUDE.md`/`AGENTS.md` |
//! | `roles.json` | `{"roles": {...}, "tiers": {...}}`: per role and per tier, over `admin.json`; an `INCODE_ROLE_*` variable still wins |
//! | `agents/*.md` | Sub-agent definitions, between the project's and the operator's |
//!
//! Applied where they are READ, never folded into the loaded admin config: a
//! `/roles` change saves `admin.json`, and a harness must not end up written
//! into the operator's own settings. With the variable unset, nothing here
//! does anything.
//!
//! A harness file that is present but unreadable is an error, not an empty
//! file: a candidate whose routing cannot be read must fail its attempt, not
//! be scored as if it were the default ([`check`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The variable naming the harness folder.
pub const HARNESS_ENV: &str = "INSI_HARNESS";
/// Standing instructions.
pub const INSTRUCTIONS_FILE: &str = "instructions.md";
/// Role and tier routing.
pub const ROUTING_FILE: &str = "roles.json";
/// Sub-agent definitions.
pub const AGENTS_DIR: &str = "agents";
/// The most of `instructions.md` given inline, as for the project's own
/// instruction files.
pub const MAX_BYTES: usize = crate::instructions::MAX_BYTES;

/// The harness folder this run was given, if any.
#[must_use]
pub fn dir() -> Option<PathBuf> {
    std::env::var_os(HARNESS_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Routing from a harness: per role and per tier, over `admin.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Routing {
    #[serde(default)]
    pub roles: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub tiers: BTreeMap<String, Vec<String>>,
}

/// The routing in `dir`; `Ok(None)` when it has no routing file.
pub fn routing_in(dir: &Path) -> Result<Option<Routing>, String> {
    let path = dir.join(ROUTING_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// This run's harness routing, when there is a harness with routing that
/// reads. A problem is traced; [`check`] is what refuses a run over it.
#[must_use]
pub fn routing() -> Option<Routing> {
    let dir = dir()?;
    match routing_in(&dir) {
        Ok(routing) => routing,
        Err(why) => {
            crate::trace!("harness", "routing left out: {why}");
            None
        }
    }
}

/// `roles` and `tiers` with the harness's entries over them, key by key.
#[must_use]
pub fn overlay(
    roles: &BTreeMap<String, Vec<String>>,
    tiers: &BTreeMap<String, Vec<String>>,
    harness: Option<&Routing>,
) -> (BTreeMap<String, Vec<String>>, BTreeMap<String, Vec<String>>) {
    let mut roles = roles.clone();
    let mut tiers = tiers.clone();
    if let Some(h) = harness {
        roles.extend(h.roles.iter().map(|(k, v)| (k.clone(), v.clone())));
        tiers.extend(h.tiers.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    (roles, tiers)
}

/// The harness's standing instructions as one context block, or `None`
/// when there is no harness or no instructions in it.
pub fn instructions_in(dir: &Path) -> Result<Option<String>, String> {
    let path = dir.join(INSTRUCTIONS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let head = crate::text::head_bytes(text, MAX_BYTES);
    let cut = if head.len() < text.len() {
        format!(
            "\n[… {} more bytes — read `{}` for the rest.]",
            text.len() - head.len(),
            path.display()
        )
    } else {
        String::new()
    };
    Ok(Some(format!(
        "<harness-instructions>\nStanding instructions for how to work, from the harness \
         this run was given.\n\n{head}{cut}\n</harness-instructions>"
    )))
}

/// This run's harness instructions, if any; a problem is traced.
#[must_use]
pub fn instructions_block() -> Option<String> {
    let dir = dir()?;
    instructions_in(&dir).unwrap_or_else(|why| {
        crate::trace!("harness", "instructions left out: {why}");
        None
    })
}

/// Whether this run's harness, if it was given one, can be used: the folder
/// exists and every file incode reads from it reads. A run refuses to start
/// on a harness it would have to half-ignore.
pub fn check() -> Result<(), String> {
    let Some(dir) = dir() else {
        return Ok(());
    };
    if !dir.is_dir() {
        return Err(format!(
            "{HARNESS_ENV} names {}, which is not a folder",
            dir.display()
        ));
    }
    routing_in(&dir)?;
    instructions_in(&dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder() -> tempfile::TempDir {
        tempfile::tempdir().expect("dir")
    }

    /// **THE HARNESS'S ROUTING WINS PER KEY, AND ONLY WHERE IT SAYS.**
    #[test]
    fn harness_routing_overlays_the_admin_file_key_by_key() {
        let dir = folder();
        std::fs::write(
            dir.path().join(ROUTING_FILE),
            r#"{"roles": {"plan": ["@fast"]}, "tiers": {"fast": ["model-b"]}}"#,
        )
        .expect("write");
        let routing = routing_in(dir.path()).expect("reads").expect("present");
        let mut roles = BTreeMap::new();
        roles.insert("plan".to_string(), vec!["model-a".to_string()]);
        roles.insert("review".to_string(), vec!["model-r".to_string()]);
        let (roles, tiers) = overlay(&roles, &BTreeMap::new(), Some(&routing));
        assert_eq!(roles["plan"], ["@fast"]);
        assert_eq!(
            roles["review"],
            ["model-r"],
            "a role it does not name is kept"
        );
        assert_eq!(tiers["fast"], ["model-b"]);
        assert_eq!(routing_in(folder().path()).expect("absent"), None);
    }

    /// **A HARNESS FILE THAT DOES NOT READ IS AN ERROR, NOT A DEFAULT.**
    #[test]
    fn a_broken_harness_file_is_reported() {
        let dir = folder();
        std::fs::write(dir.path().join(ROUTING_FILE), "{not json").expect("write");
        assert!(routing_in(dir.path()).is_err());
        std::fs::write(dir.path().join(ROUTING_FILE), r#"{"rolez": {}}"#).expect("write");
        assert!(
            routing_in(dir.path()).is_err(),
            "a misspelt key is refused, not ignored"
        );
    }

    #[test]
    fn instructions_are_wrapped_and_bounded() {
        let dir = folder();
        assert_eq!(instructions_in(dir.path()).expect("absent"), None);
        std::fs::write(dir.path().join(INSTRUCTIONS_FILE), "  \n").expect("write");
        assert_eq!(instructions_in(dir.path()).expect("blank"), None);
        std::fs::write(dir.path().join(INSTRUCTIONS_FILE), "Run the tests first.").expect("w");
        let block = instructions_in(dir.path())
            .expect("reads")
            .expect("present");
        assert!(block.starts_with("<harness-instructions>"));
        assert!(block.contains("Run the tests first."));
        std::fs::write(
            dir.path().join(INSTRUCTIONS_FILE),
            "x".repeat(MAX_BYTES + 10),
        )
        .expect("w");
        let block = instructions_in(dir.path())
            .expect("reads")
            .expect("present");
        assert!(
            block.contains("10 more bytes"),
            "{}",
            &block[block.len() - 200..]
        );
    }
}
