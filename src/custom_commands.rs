//! The operator's own slash commands: a markdown file is a prompt with a name.
//!
//! # Where they come from
//!
//! - `.incode/commands/**/*.md` in the workspace (the project's),
//! - `~/.incode/commands/**/*.md` (the operator's),
//! - `commands/**/*.md` in each plugin ([`crate::plugins`]), as
//!   `/<plugin>:<cmd>`.
//!
//! The name is the path under `commands/` with `/` turned into `:` and the
//! `.md` dropped, so `frontend/test.md` is `/frontend:test`. On a clash the
//! project's file wins over the operator's, and both over a plugin's. A
//! built-in command always wins over all of them, and a file that tries to
//! take a built-in's name is said to be shadowed rather than silently unused.
//!
//! # The file
//!
//! ```markdown
//! ---
//! description: Run the frontend tests for one file
//! argument-hint: <file> [pattern]
//! model: sonnet
//! allowed-tools: Bash(npm test:*), Read
//! ---
//! Run the tests in $1, filtered to `$2`. Everything typed: $ARGUMENTS
//! ```
//!
//! The front matter is optional and read by the same parser as the sub-agent
//! definitions. In the body, `$ARGUMENTS` is everything typed after the name
//! and `$1`..`$9` are its words, split the way a shell would (quotes group).
//! A body that uses neither is given what was typed on a line of its own at
//! the end, so arguments are never dropped in silence.
//!
//! # Files in the body
//!
//! `@path` in the body — at its start or after whitespace, naming something
//! with a `/` or a `.` in it (`@src/main.rs`, `@./Makefile`) — is replaced by
//! that file's contents in a fenced block headed by the path. The path is
//! resolved against the workspace and must stay inside it once `..` and
//! symlinks are resolved; an absolute path is taken only when it is inside.
//! A file is read up to [`MAX_INCLUDE_BYTES`], and all of a command's files
//! up to [`MAX_INCLUDES_TOTAL`]. What cannot be included leaves a marker the
//! model can read — `[@path: not found]`, `[@path: outside the workspace]` —
//! rather than vanishing. An email address (`a@b.com`) is left alone.
//!
//! Only the command file's own text is looked at. What was typed as
//! arguments is substituted as it is: `/review @secrets.txt` does not read
//! `secrets.txt`, and neither does a body's `@$1`. A file the person names
//! is for the model to read with its tools, under the session's rules.
//!
//! # Model and tools
//!
//! `model` is honoured by `incode -t "/name args"` when `-m` is not given; in
//! the REPL and over ACP switching the model abandons the conversation, so it
//! is said, not done.
//!
//! `allowed-tools` HOLDS THE TURN the command starts: its entries are
//! permission rules (`Bash(npm test:*)`, `Read`, `Edit(src/**)`, see
//! [`crate::permissions::rules`]), and a tool call none of them covers is
//! refused, with the list named to the model. Only that turn — its follow-up
//! and continuations included — and never the next. It only narrows: a call
//! it covers still meets the deny and ask rules, the guards, the mode and
//! the operator's hooks. It binds where incode runs the model's tools (the
//! API route, and Claude Code, whose tools are incode's); an agent CLI that
//! brings its own tools (Codex, Antigravity) is not held to it, and the turn
//! says so. A sub-agent the turn starts with `Agent` (when the list allows
//! it) runs under its own definition's tools, not this list.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The project's commands, under the workspace.
pub const PROJECT_DIR: &str = ".incode/commands";

/// The operator's commands, under `~/.incode/`.
pub const USER_DIR: &str = "commands";

/// The largest command file read. A prompt, not a document.
pub const MAX_FILE_BYTES: u64 = 64 * 1024;

/// The largest file one `@path` in a command's body brings in.
pub const MAX_INCLUDE_BYTES: u64 = 256 * 1024;

/// The most all of one command's `@path` files bring in together.
pub const MAX_INCLUDES_TOTAL: u64 = 1024 * 1024;

/// Where a command came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Project,
    User,
    Plugin(String),
}

impl Origin {
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Project => "project".to_string(),
            Self::User => "user".to_string(),
            Self::Plugin(name) => format!("plugin {name}"),
        }
    }
}

/// One command file, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    /// Without the leading `/`: `frontend:test`.
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub model: Option<String>,
    /// Permission rules the turn it starts is held to; see the module
    /// documentation. Empty: the turn is not narrowed.
    pub allowed_tools: Vec<String>,
    pub body: String,
    pub path: PathBuf,
    pub origin: Origin,
}

/// A command, filled in with what was typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expanded {
    pub name: String,
    pub prompt: String,
    pub model: Option<String>,
    pub allowed_tools: Vec<String>,
}

/// Every command found, and what was wrong with the rest.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    commands: Vec<CustomCommand>,
    pub warnings: Vec<String>,
}

impl Registry {
    /// The commands for this workspace, this operator and their plugins.
    #[must_use]
    pub fn load(workspace: &Path) -> Self {
        // Only plugins that may be used (a project's must be approved); the
        // ones left out arrive as problems and are said with the warnings.
        let plugins = crate::plugins::discover_usable(workspace);
        let mut registry = Self::load_from(
            &workspace.join(PROJECT_DIR),
            &crate::admin::AdminConfig::default_dir().join(USER_DIR),
            &plugins.plugins,
        );
        registry.warnings.extend(plugins.problems);
        registry
    }

    /// The commands in these folders, in order of precedence.
    #[must_use]
    pub fn load_from(project: &Path, user: &Path, plugins: &[crate::plugins::Plugin]) -> Self {
        let mut sources: Vec<(PathBuf, Option<String>, Origin)> = vec![
            (project.to_path_buf(), None, Origin::Project),
            (user.to_path_buf(), None, Origin::User),
        ];
        for plugin in plugins {
            sources.push((
                plugin.commands_dir(),
                Some(plugin.manifest.name.clone()),
                Origin::Plugin(plugin.manifest.name.clone()),
            ));
        }

        let mut registry = Self::default();
        for (dir, namespace, origin) in sources {
            let (found, problems) = scan_dir(&dir, namespace.as_deref());
            registry.warnings.extend(problems);
            for mut command in found {
                command.origin = origin.clone();
                let typed = format!("/{}", command.name);
                if !matches!(
                    crate::cli::slash_commands::SlashCommand::parse(&typed),
                    Some(crate::cli::slash_commands::SlashCommand::Unknown(_))
                ) {
                    registry.warnings.push(format!(
                        "custom command {typed} ({}) is shadowed by the built-in {typed}",
                        command.path.display()
                    ));
                    continue;
                }
                if let Some(kept) = registry.find(&command.name) {
                    registry.warnings.push(format!(
                        "custom command {typed} in {} is shadowed by the {} one in {}",
                        command.path.display(),
                        kept.origin.label(),
                        kept.path.display()
                    ));
                    continue;
                }
                registry.commands.push(command);
            }
        }
        registry
    }

    /// Every command, in precedence order.
    #[must_use]
    pub fn commands(&self) -> &[CustomCommand] {
        &self.commands
    }

    /// The command with this name (no `/`), ignoring case.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&CustomCommand> {
        self.commands.iter().find(|c| c.name == name).or_else(|| {
            self.commands
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(name))
        })
    }

    /// `line` filled in, when it names one of these commands and no built-in:
    /// the arguments substituted and the body's `@path` files, under
    /// `workspace`, included.
    #[must_use]
    pub fn expand_line(&self, line: &str, workspace: &Path) -> Option<Expanded> {
        let line = line.trim();
        let rest = line.strip_prefix('/')?;
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (rest, ""),
        };
        if !matches!(
            crate::cli::slash_commands::SlashCommand::parse(line),
            Some(crate::cli::slash_commands::SlashCommand::Unknown(_))
        ) {
            return None;
        }
        let command = self.find(name)?;
        Some(Expanded {
            name: command.name.clone(),
            prompt: fill(&command.body, args, Some(workspace)),
            model: command.model.clone(),
            allowed_tools: command.allowed_tools.clone(),
        })
    }

    /// The `/help` and completion entries: (`/name <hint>`, description).
    #[must_use]
    pub fn menu_entries(&self) -> Vec<(String, String)> {
        self.commands
            .iter()
            .map(|c| {
                let entry = match &c.argument_hint {
                    Some(hint) if !hint.is_empty() => format!("/{} {hint}", c.name),
                    _ => format!("/{}", c.name),
                };
                let about = if c.description.is_empty() {
                    format!("({})", c.origin.label())
                } else {
                    format!("{} ({})", c.description, c.origin.label())
                };
                (entry, about)
            })
            .collect()
    }

    /// Make these the commands `/help` and the input line offer. Once per
    /// process: the REPL is the only thing that calls it.
    pub fn publish_menu(&self) {
        let entries: Vec<(&'static str, &'static str)> = self
            .menu_entries()
            .into_iter()
            .map(|(e, d)| -> (&'static str, &'static str) {
                (Box::leak(e.into_boxed_str()), Box::leak(d.into_boxed_str()))
            })
            .collect();
        let _ = MENU.set(entries);
    }
}

static MENU: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();

/// The custom commands this session published, for `/help` and completion.
/// Empty until [`Registry::publish_menu`].
#[must_use]
pub fn menu() -> &'static [(&'static str, &'static str)] {
    MENU.get().map_or(&[], Vec::as_slice)
}

/// The built-in commands and then the custom ones, as (entry, description).
pub fn all_commands() -> impl Iterator<Item = (&'static str, &'static str)> {
    crate::cli::slash_commands::COMMANDS
        .iter()
        .copied()
        .chain(menu().iter().copied())
}

/// What a custom command became, in a line: its name, the model it asks for
/// (named rather than switched to: switching starts the conversation over),
/// and the tools the turn is held to.
#[must_use]
pub fn note(expanded: &Expanded) -> String {
    let mut note = format!("/{} expanded into the prompt", expanded.name);
    if let Some(model) = &expanded.model {
        note.push_str(&format!(
            "; it asks for model `{model}` (`/model {model}` switches, starting fresh)"
        ));
    }
    if !expanded.allowed_tools.is_empty() {
        note.push_str(&format!(
            "; this turn may use only: {}",
            expanded.allowed_tools.join(", ")
        ));
    }
    note
}

/// Say in the REPL what a custom command became, and hand back its prompt.
pub fn announce(expanded: &Expanded) -> String {
    crate::cli::render::print_info(&note(expanded));
    expanded.prompt.clone()
}

/// What the person is told when the agent serving a narrowed turn runs its
/// own tools, where incode cannot refuse a call.
#[must_use]
pub fn not_enforced_by(agent: &str, allowed: &[String]) -> String {
    crate::trace!(
        "session",
        "allowed-tools ({}) not enforced: {agent} runs its own tools",
        allowed.join(", ")
    );
    format!(
        "allowed-tools ({}) was NOT enforced this turn: {agent} runs its own tools, which \
         incode cannot refuse. It binds on the API route, Claude Code, codex with \
         codex_tools \"incode\", and agy with agy_tools \"incode\".",
        allowed.join(", ")
    )
}

/// A `-t` instruction, after a custom command is looked for in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub instruction: String,
    /// `-m` first, then the command's.
    pub model: Option<String>,
    /// The command's `allowed-tools`, which the run is held to.
    pub allowed_tools: Vec<String>,
}

/// For `incode -t "<instruction>"`: the instruction expanded when it names a
/// custom command, the model to use, and the tools the run is held to.
#[must_use]
pub fn expand_task(workspace: &Path, instruction: String, model: Option<String>) -> Task {
    let plain = |instruction, model| Task {
        instruction,
        model,
        allowed_tools: Vec::new(),
    };
    if !instruction.trim_start().starts_with('/') {
        return plain(instruction, model);
    }
    let registry = Registry::load(workspace);
    match registry.expand_line(&instruction, workspace) {
        Some(expanded) => {
            eprintln!("incode: running custom command /{}", expanded.name);
            if !expanded.allowed_tools.is_empty() {
                eprintln!(
                    "incode: this run may use only: {}",
                    expanded.allowed_tools.join(", ")
                );
            }
            Task {
                instruction: expanded.prompt,
                model: model.or(expanded.model),
                allowed_tools: expanded.allowed_tools,
            }
        }
        None => plain(instruction, model),
    }
}

/// Every `*.md` under `dir`, named by its path; `namespace` (a plugin's name)
/// goes in front. Returns the commands and what could not be read.
#[must_use]
pub fn scan_dir(dir: &Path, namespace: Option<&str>) -> (Vec<CustomCommand>, Vec<String>) {
    let mut files = Vec::new();
    collect_md(dir, &mut files);
    files.sort();
    let mut found = Vec::new();
    let mut problems = Vec::new();
    for path in files {
        let Some(name) = command_name(dir, &path) else {
            continue;
        };
        let name = match namespace {
            Some(ns) => format!("{ns}:{name}"),
            None => name,
        };
        match read_command(&path, name) {
            Ok(command) => found.push(command),
            Err(why) => problems.push(why),
        }
    }
    (found, problems)
}

fn collect_md(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if hidden {
            continue;
        }
        if kind.is_dir() {
            collect_md(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

/// `frontend/test.md` under `dir` is `frontend:test`.
#[must_use]
pub fn command_name(dir: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(dir).ok()?.with_extension("");
    let parts: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.is_empty() || parts.iter().any(|p| p.is_empty() || p.contains(' ')) {
        return None;
    }
    Some(parts.join(":"))
}

fn read_command(path: &Path, name: String) -> Result<CustomCommand, String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if size > MAX_FILE_BYTES {
        return Err(format!(
            "{}: {size} bytes is over the {MAX_FILE_BYTES}-byte limit for a command",
            path.display()
        ));
    }
    let text = crate::text::read_authored(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut command = parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    command.name = name;
    command.path = path.to_path_buf();
    Ok(command)
}

/// A command file's front matter and body. The name, path and origin are
/// left for the caller.
pub fn parse(text: &str) -> Result<CustomCommand, String> {
    use crate::subagents::Field;
    let mut command = CustomCommand {
        name: String::new(),
        description: String::new(),
        argument_hint: None,
        model: None,
        allowed_tools: Vec::new(),
        body: text.trim().to_string(),
        path: PathBuf::new(),
        origin: Origin::Project,
    };
    let Some((front, body)) = crate::subagents::split_front_matter(text) else {
        return Ok(command);
    };
    command.body = body.trim().to_string();
    for (key, field) in crate::subagents::parse_fields(front)? {
        let one = |field: Field| match field {
            Field::One(s) => s,
            Field::Many(items) => items.join(", "),
        };
        match key.as_str() {
            "description" => command.description = one(field),
            "argument-hint" | "argument_hint" => {
                // `[file]` reads as a YAML list; it is meant literally.
                command.argument_hint = Some(match field {
                    Field::One(s) => s,
                    Field::Many(items) => format!("[{}]", items.join(", ")),
                });
            }
            "model" => command.model = Some(one(field)).filter(|m| !m.is_empty()),
            "allowed-tools" | "allowed_tools" => {
                command.allowed_tools = match field {
                    Field::Many(items) => items,
                    Field::One(s) => s
                        .split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect(),
                };
            }
            _ => {}
        }
    }
    Ok(command)
}

/// The body with `$ARGUMENTS` and `$1`..`$9` filled in from `args`.
#[must_use]
pub fn substitute(body: &str, args: &str) -> String {
    fill(body, args, None)
}

/// The body with its arguments substituted and, given a workspace, its
/// `@path` files included. The two are done in one pass over the body's own
/// text, so neither an argument nor an included file is read for the other:
/// a `$1` inside a file stays as written, and an `@` typed as an argument is
/// not a file.
fn fill(body: &str, args: &str, workspace: Option<&Path>) -> String {
    let words = split_args(args);
    let mut out = String::with_capacity(body.len() + args.len());
    let mut used = false;
    let mut budget = MAX_INCLUDES_TOTAL;
    let pieces = match workspace {
        Some(_) => pieces(body),
        None => vec![Piece::Text(body)],
    };
    for piece in pieces {
        match (piece, workspace) {
            (Piece::Include(path), Some(workspace)) => {
                include(path, workspace, &mut budget, &mut out);
            }
            (Piece::Include(text) | Piece::Text(text), _) => {
                used |= substitute_into(text, args, &words, &mut out);
            }
        }
    }
    if !used && !args.is_empty() {
        out.push_str("\n\nARGUMENTS: ");
        out.push_str(args);
    }
    out
}

/// `text` onto `out` with its placeholders filled; whether it had any.
fn substitute_into(text: &str, args: &str, words: &[String], out: &mut String) -> bool {
    let mut used = false;
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            out.push_str(args);
            used = true;
            rest = tail;
        } else if let Some(d) = after.chars().next().filter(|c| ('1'..='9').contains(c)) {
            let index = d as usize - '1' as usize;
            out.push_str(words.get(index).map_or("", String::as_str));
            used = true;
            rest = &after[1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    used
}

/// A command body, cut at its `@path` includes.
#[derive(Debug, PartialEq, Eq)]
enum Piece<'a> {
    Text(&'a str),
    /// The path, without its `@`.
    Include(&'a str),
}

/// `body` as text and includes, in order.
fn pieces(body: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut text_from = 0;
    let mut search_from = 0;
    while let Some(found) = body[search_from..].find('@') {
        let at = search_from + found;
        search_from = at + 1;
        // At the start or after whitespace: `a@b.com` is an address.
        let starts_a_word = body[..at]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
        let Some(path) = include_path(&body[at + 1..]).filter(|_| starts_a_word) else {
            continue;
        };
        out.push(Piece::Text(&body[text_from..at]));
        out.push(Piece::Include(path));
        text_from = at + 1 + path.len();
        search_from = text_from;
    }
    out.push(Piece::Text(&body[text_from..]));
    out
}

/// The path `rest` starts with, when it looks like one: the characters a
/// path is written with, less a sentence's closing `.` or `:`, naming a
/// folder or an extension — so `@alice` stays a mention.
///
/// `\` separates as `/` does, and a drive (`C:\` or `C:/`) may lead, so a
/// Windows path is read whole rather than cut at its `:` — and so is checked
/// against the workspace like any other. A `:` anywhere else ends the path
/// (`@a.rs:12` is `a.rs`).
fn include_path(rest: &str) -> Option<&str> {
    let bytes = rest.as_bytes();
    let drive = bytes.len() > 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    let skip = if drive { 2 } else { 0 };
    let end = rest[skip..]
        .find(|c: char| {
            !(c.is_alphanumeric() || matches!(c, '/' | '\\' | '.' | '_' | '-' | '+' | '~'))
        })
        .map_or(rest.len(), |i| i + skip);
    let path = rest[..end].trim_end_matches(['.', ':']);
    let looks_like_one = (path.contains(['/', '\\']) || path.contains('.'))
        && path.chars().any(char::is_alphanumeric);
    looks_like_one.then_some(path)
}

/// The file at `path`, in a fenced block headed by the path, onto `out` —
/// or a marker saying why it is not there. `budget` is what the command's
/// files may still bring in.
fn include(path: &str, workspace: &Path, budget: &mut u64, out: &mut String) {
    let marker = |out: &mut String, why: &str| out.push_str(&format!("[@{path}: {why}]"));
    // Where the file really is, `..` and symlinks resolved, must be inside;
    // a path that does not exist is outside when its text already leaves.
    let root = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let named = crate::permissions::rules::normalise(&root, path);
    let Ok(real) = named.canonicalize() else {
        return marker(
            out,
            if named.starts_with(&root) {
                "not found"
            } else {
                "outside the workspace"
            },
        );
    };
    if !real.starts_with(&root) {
        return marker(out, "outside the workspace");
    }
    let size = match std::fs::metadata(&real) {
        Ok(meta) if meta.is_file() => meta.len(),
        Ok(_) => return marker(out, "not a file"),
        Err(_) => return marker(out, "not found"),
    };
    if size > MAX_INCLUDE_BYTES {
        return marker(
            out,
            &format!("{size} bytes, over the {MAX_INCLUDE_BYTES}-byte limit for one file"),
        );
    }
    if size > *budget {
        return marker(
            out,
            &format!("left out, the command's files reached {MAX_INCLUDES_TOTAL} bytes"),
        );
    }
    let Ok(text) = std::fs::read_to_string(&real) else {
        return marker(out, "not readable as text");
    };
    *budget -= size;
    // Longer than any run of backticks inside, so the file cannot close it.
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("{path}:\n{fence}\n{text}"));
    if !text.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&fence);
    out.push('\n');
}

/// Words the way a shell splits them: whitespace separates, single and double
/// quotes group, and a backslash outside single quotes takes the next
/// character as it is.
#[must_use]
pub fn split_args(args: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = args.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"') | None, '\\') => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
                in_word = true;
            }
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests;
