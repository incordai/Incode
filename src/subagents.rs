//! Sub-agents the operator defines, one Markdown file each.
//!
//! # Where they come from
//!
//! - `.incord/agents/*.md` in the workspace — the project's own;
//! - `~/.incord-code/agents/*.md` — the operator's, for every project.
//!
//! A project's definition wins over the operator's of the same name.
//!
//! # The file
//!
//! ```text
//! ---
//! name: reviewer
//! description: Reads a change and reports what is wrong with it.
//! role: review
//! model: opus
//! tools: [Read, Grep, Glob]
//! ---
//! You review changes. Report each problem with its file and line …
//! ```
//!
//! `name`, `description` and a non-empty body are required. `role` is one of
//! [`crate::roles::KNOWN_ROLES`] and routes the model the way the `Agent`
//! tool's own `role` does; `model` names one outright. `tools` is an
//! allow-list of toolbox tool names, written `[A, B]`, `A, B` or as a list of
//! `- A` lines: the sub-agent is offered only those, and a call to any other
//! is refused by the toolbox's own gate.
//!
//! # A bad file is reported, never fatal
//!
//! Each file is bounded ([`MAX_FILE_BYTES`]) and so is each directory
//! ([`MAX_AGENTS`]). A file that breaks a rule — too large, no front matter,
//! an unknown role or tool, a name that is not a plain word — is left out and
//! said, with its path, by `/agents` and in the trace. It never stops a
//! session.

use std::path::{Path, PathBuf};

/// Where a project keeps its sub-agents, under the workspace.
pub const PROJECT_DIR: &str = ".incord/agents";

/// The directory under the operator's data directory.
const USER_DIR: &str = "agents";

/// The most one definition file may hold. A prompt longer than this is a
/// document, not an agent's instructions, and it rides every one of its turns.
pub const MAX_FILE_BYTES: u64 = 64 * 1024;

/// The most definition files read from one directory.
pub const MAX_AGENTS: usize = 64;

/// The most characters of a description kept, in the `Agent` tool's schema.
pub const MAX_DESCRIPTION_CHARS: usize = 300;

/// The longest name a definition may have.
const MAX_NAME_CHARS: usize = 64;

/// Which directory a definition came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The workspace's `.incord/agents`.
    Project,
    /// The operator's `~/.incord-code/agents`.
    User,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

/// One sub-agent, as its file defines it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// One of the known roles, when given.
    pub role: Option<String>,
    /// A model named outright, when given.
    pub model: Option<String>,
    /// The only toolbox tools it is offered, when given.
    pub tools: Option<Vec<String>>,
    /// Its instructions: the body after the front matter.
    pub prompt: String,
    pub source: PathBuf,
    pub scope: Scope,
}

/// Every definition found, and every file left out with why.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub agents: Vec<AgentDef>,
    pub problems: Vec<String>,
}

impl Loaded {
    /// The definition called `name`, matched exactly.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&AgentDef> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// One line per agent, `name — description`, for the `Agent` tool's
    /// schema. `None` when there are none.
    #[must_use]
    pub fn schema_note(&self) -> Option<String> {
        if self.agents.is_empty() {
            return None;
        }
        let lines: Vec<String> = self
            .agents
            .iter()
            .map(|a| {
                format!(
                    "- {}: {}",
                    a.name,
                    crate::text::truncate(&a.description, MAX_DESCRIPTION_CHARS)
                )
            })
            .collect();
        Some(format!(
            "Agents defined for this workspace (pass one as `agent` to run it with its own \
             instructions and tools):\n{}",
            lines.join("\n")
        ))
    }

    /// What `/agents` prints.
    #[must_use]
    pub fn report(&self) -> String {
        let mut out = String::new();
        if self.agents.is_empty() {
            out.push_str(&format!(
                "No sub-agents are defined. Add one as a Markdown file in {PROJECT_DIR}/ (this \
                 project) or ~/.incord-code/{USER_DIR}/ (every project).\n"
            ));
        }
        for agent in &self.agents {
            out.push_str(&format!("{} ({})", agent.name, agent.scope.as_str()));
            if let Some(role) = &agent.role {
                out.push_str(&format!(" · role {role}"));
            }
            if let Some(model) = &agent.model {
                out.push_str(&format!(" · model {model}"));
            }
            out.push('\n');
            out.push_str(&format!("  {}\n", agent.description));
            match &agent.tools {
                Some(tools) => out.push_str(&format!("  tools: {}\n", tools.join(", "))),
                None => out.push_str("  tools: every tool the session offers\n"),
            }
            out.push_str(&format!("  from {}\n", agent.source.display()));
        }
        for problem in &self.problems {
            out.push_str(&format!("left out: {problem}\n"));
        }
        out
    }
}

/// Every sub-agent defined for `workspace`: the project's, then the
/// operator's that the project does not override.
#[must_use]
pub fn load(workspace: &Path) -> Loaded {
    load_from(
        &workspace.join(PROJECT_DIR),
        &crate::admin::AdminConfig::default_dir().join(USER_DIR),
    )
}

/// [`load`], from two named directories.
fn load_from(project: &Path, user: &Path) -> Loaded {
    let mut loaded = Loaded::default();
    for (dir, scope) in [(project, Scope::Project), (user, Scope::User)] {
        for found in read_dir(dir, scope, &mut loaded.problems) {
            if loaded.find(&found.name).is_some() {
                // The project's own wins; the operator's is shadowed, not an
                // error.
                crate::trace!(
                    "agents",
                    "{} is overridden by the project's own `{}`",
                    found.source.display(),
                    found.name
                );
                continue;
            }
            loaded.agents.push(found);
        }
    }
    for problem in &loaded.problems {
        crate::trace!("agents", "left out: {problem}");
    }
    loaded
}

/// The definitions in one directory, sorted by file name; problems are added
/// to `problems`.
fn read_dir(dir: &Path, scope: Scope, problems: &mut Vec<String>) -> Vec<AgentDef> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("md")))
        .collect();
    files.sort();
    if files.len() > MAX_AGENTS {
        problems.push(format!(
            "{}: {} definition files, only the first {MAX_AGENTS} are read",
            dir.display(),
            files.len()
        ));
        files.truncate(MAX_AGENTS);
    }
    let mut found: Vec<AgentDef> = Vec::new();
    for path in files {
        match read_one(&path, scope) {
            Ok(def) if found.iter().any(|d| d.name == def.name) => problems.push(format!(
                "{}: the name `{}` is already defined in this directory",
                path.display(),
                def.name
            )),
            Ok(def) => found.push(def),
            Err(why) => problems.push(format!("{}: {why}", path.display())),
        }
    }
    found
}

/// One definition file.
fn read_one(path: &Path, scope: Scope) -> Result<AgentDef, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("could not be read: {e}"))?;
    if !meta.is_file() {
        return Err("not a file".to_string());
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{} bytes, over the {MAX_FILE_BYTES}-byte limit",
            meta.len()
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("could not be read: {e}"))?;
    parse(&text, path, scope)
}

/// A definition from its text.
fn parse(text: &str, path: &Path, scope: Scope) -> Result<AgentDef, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (front, body) = split_front_matter(text).ok_or(
        "no front matter: the file must start with a `---` line and close it with another",
    )?;
    let fields = parse_fields(front)?;
    let get = |key: &str| {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };

    let name = match get("name") {
        Some(Field::One(name)) => name,
        _ => return Err("`name` is required".to_string()),
    };
    if name.is_empty()
        || name.chars().count() > MAX_NAME_CHARS
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "`name` must be letters, digits, `-` or `_`, at most {MAX_NAME_CHARS} characters, \
             not `{name}`"
        ));
    }
    let description = match get("description") {
        Some(Field::One(d)) if !d.is_empty() => d,
        _ => return Err("`description` is required".to_string()),
    };
    let role = match get("role") {
        None => None,
        Some(Field::One(role)) if role.is_empty() => None,
        Some(Field::One(role)) => {
            if !crate::roles::KNOWN_ROLES.contains(&role.as_str()) {
                return Err(format!(
                    "`role: {role}` is not a role; the roles are {}",
                    crate::roles::KNOWN_ROLES.join(", ")
                ));
            }
            Some(role)
        }
        Some(Field::Many(_)) => return Err("`role` takes one value".to_string()),
    };
    let model = match get("model") {
        None => None,
        Some(Field::One(m)) if m.is_empty() => None,
        Some(Field::One(m)) => Some(m),
        Some(Field::Many(_)) => return Err("`model` takes one value".to_string()),
    };
    let tools = match get("tools") {
        None => None,
        Some(Field::One(one)) if one.is_empty() => None,
        Some(Field::One(one)) => Some(split_list(&one)),
        Some(Field::Many(many)) => Some(many),
    };
    if let Some(tools) = &tools {
        let unknown: Vec<&String> = tools
            .iter()
            .filter(|t| crate::toolbox::reach_of(t).is_none() && !t.starts_with("mcp__"))
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "`tools` names no such tool: {}",
                unknown
                    .iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    let prompt = body.trim().to_string();
    if prompt.is_empty() {
        return Err("the instructions after the front matter are empty".to_string());
    }
    Ok(AgentDef {
        name,
        description,
        role,
        model,
        tools,
        prompt,
        source: path.to_path_buf(),
        scope,
    })
}

/// One front-matter value: a scalar, or a list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Field {
    One(String),
    Many(Vec<String>),
}

/// The front matter and the body, when the text opens with `---`.
fn split_front_matter(text: &str) -> Option<(&str, &str)> {
    let rest = text
        .strip_prefix("---\r\n")
        .or_else(|| text.strip_prefix("---\n"))?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// `key: value` lines, and `- item` lines under a key with no value.
fn parse_fields(front: &str) -> Result<Vec<(String, Field)>, String> {
    let mut fields: Vec<(String, Field)> = Vec::new();
    for raw in front.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(item) = trimmed.strip_prefix("- ") {
            match fields.last_mut() {
                Some((_, Field::Many(items))) => items.push(unquote(item.trim())),
                // A key with no value on its line, then its items below.
                Some((_, field)) if *field == Field::One(String::new()) => {
                    *field = Field::Many(vec![unquote(item.trim())]);
                }
                _ => return Err(format!("a list item with no key above it: `{trimmed}`")),
            }
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(format!("not a `key: value` line: `{trimmed}`"));
        };
        let key = key.trim().to_ascii_lowercase();
        if fields.iter().any(|(k, _)| *k == key) {
            return Err(format!("`{key}` is given twice"));
        }
        let value = value.trim();
        let field = match value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            Some(inner) => Field::Many(split_list(inner)),
            None => Field::One(unquote(value)),
        };
        fields.push((key, field));
    }
    Ok(fields)
}

/// `A, B, C` as a list, each item unquoted, empty ones dropped.
fn split_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(|item| unquote(item.trim()))
        .filter(|item| !item.is_empty())
        .collect()
}

/// A value with one pair of matching quotes taken off.
fn unquote(value: &str) -> String {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("dir");
        let path = dir.join(name);
        std::fs::write(&path, text).expect("write");
        path
    }

    const REVIEWER: &str = "---\nname: reviewer\ndescription: Reads a change and reports \
        problems.\nrole: review\nmodel: opus\ntools: [Read, Grep, \"Glob\"]\n---\nYou review \
        changes.\n";

    /// **A DEFINITION IS READ WHOLE**: every field and the body.
    #[test]
    fn a_definition_file_is_read_with_every_field() {
        let dir = tempfile::tempdir().expect("dir");
        let project = dir.path().join("p");
        write(&project, "reviewer.md", REVIEWER);
        let loaded = load_from(&project, &dir.path().join("u"));
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        let def = loaded.find("reviewer").expect("loaded");
        assert_eq!(def.description, "Reads a change and reports problems.");
        assert_eq!(def.role.as_deref(), Some("review"));
        assert_eq!(def.model.as_deref(), Some("opus"));
        assert_eq!(
            def.tools.as_deref(),
            Some(&["Read".to_string(), "Grep".to_string(), "Glob".to_string()][..])
        );
        assert_eq!(def.prompt, "You review changes.");
        assert_eq!(def.scope, Scope::Project);
    }

    /// Both list spellings, and no tools line at all meaning every tool.
    #[test]
    fn tools_may_be_a_block_list_or_absent() {
        let dir = tempfile::tempdir().expect("dir");
        let project = dir.path().join("p");
        write(
            &project,
            "a.md",
            "---\nname: a\ndescription: d\ntools:\n  - Read\n  - LS\n---\nbody",
        );
        write(
            &project,
            "b.md",
            "---\r\nname: b\r\ndescription: d\r\n---\r\nbody\r\n",
        );
        let loaded = load_from(&project, &dir.path().join("u"));
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(
            loaded.find("a").and_then(|a| a.tools.clone()),
            Some(vec!["Read".to_string(), "LS".to_string()])
        );
        assert_eq!(loaded.find("b").map(|b| b.tools.clone()), Some(None));
    }

    /// **A BAD FILE IS REPORTED, NOT A CRASH**, and the good ones still load.
    #[test]
    fn a_bad_file_is_left_out_and_said_while_the_rest_load() {
        let dir = tempfile::tempdir().expect("dir");
        let project = dir.path().join("p");
        write(&project, "good.md", REVIEWER);
        write(&project, "nofront.md", "just text");
        write(
            &project,
            "role.md",
            "---\nname: r\ndescription: d\nrole: boss\n---\nx",
        );
        write(
            &project,
            "tool.md",
            "---\nname: t\ndescription: d\ntools: Read, Hack\n---\nx",
        );
        write(
            &project,
            "name.md",
            "---\nname: bad name!\ndescription: d\n---\nx",
        );
        write(
            &project,
            "empty.md",
            "---\nname: e\ndescription: d\n---\n   \n",
        );
        write(
            &project,
            "big.md",
            &format!(
                "---\nname: big\ndescription: d\n---\n{}",
                "x".repeat(usize::try_from(MAX_FILE_BYTES).unwrap_or(0) + 1)
            ),
        );
        let loaded = load_from(&project, &dir.path().join("u"));
        assert_eq!(
            loaded
                .agents
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            ["reviewer"]
        );
        let all = loaded.problems.join("\n");
        for needle in [
            "nofront.md: no front matter",
            "role.md: `role: boss` is not a role",
            "tool.md: `tools` names no such tool: Hack",
            "name.md: `name` must be",
            "empty.md: the instructions",
            "big.md:",
        ] {
            assert!(all.contains(needle), "missing `{needle}` in:\n{all}");
        }
        assert!(loaded.report().contains("left out:"), "{}", loaded.report());
    }

    /// The project's definition wins over the operator's of the same name;
    /// the operator's others still load.
    #[test]
    fn the_projects_definition_overrides_the_operators() {
        let dir = tempfile::tempdir().expect("dir");
        let project = dir.path().join("p");
        let user = dir.path().join("u");
        write(&project, "reviewer.md", REVIEWER);
        write(
            &user,
            "reviewer.md",
            "---\nname: reviewer\ndescription: the operator's\n---\nother",
        );
        write(
            &user,
            "scout.md",
            "---\nname: scout\ndescription: finds\n---\nfind",
        );
        let loaded = load_from(&project, &user);
        assert_eq!(loaded.agents.len(), 2, "{:?}", loaded.agents);
        assert_eq!(
            loaded.find("reviewer").map(|a| a.scope),
            Some(Scope::Project)
        );
        assert_eq!(loaded.find("scout").map(|a| a.scope), Some(Scope::User));
        let note = loaded.schema_note().expect("a note");
        assert!(note.contains("- reviewer: Reads a change"), "{note}");
        assert!(note.contains("- scout: finds"), "{note}");
    }

    /// A directory with too many files is read only to its bound, and says so.
    #[test]
    fn a_directory_is_read_only_to_its_bound() {
        let dir = tempfile::tempdir().expect("dir");
        let project = dir.path().join("p");
        for i in 0..=MAX_AGENTS {
            write(
                &project,
                &format!("a{i:03}.md"),
                &format!("---\nname: a{i}\ndescription: d\n---\nx"),
            );
        }
        let loaded = load_from(&project, &dir.path().join("u"));
        assert_eq!(loaded.agents.len(), MAX_AGENTS);
        assert!(
            loaded.problems.iter().any(|p| p.contains("only the first")),
            "{:?}",
            loaded.problems
        );
    }
}
