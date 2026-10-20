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
//! `model` is honoured by `incode -t "/name args"` when `-m` is not given; in
//! the REPL switching the model abandons the conversation, so it is said, not
//! done. `allowed-tools` is ADVISORY: it is parsed, shown and carried on the
//! expansion, but no tool is refused because of it — the guards, the
//! permission mode and the operator's hooks are what restrict tools.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The project's commands, under the workspace.
pub const PROJECT_DIR: &str = ".incode/commands";

/// The operator's commands, under `~/.incode/`.
pub const USER_DIR: &str = "commands";

/// The largest command file read. A prompt, not a document.
pub const MAX_FILE_BYTES: u64 = 64 * 1024;

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
    /// Advisory; see the module documentation.
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
        let plugins = crate::plugins::discover(workspace);
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

    /// `line` filled in, when it names one of these commands and no built-in.
    #[must_use]
    pub fn expand_line(&self, line: &str) -> Option<Expanded> {
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
            prompt: substitute(&command.body, args),
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

/// Say in the REPL what a custom command became, and hand back its prompt.
/// The model it asks for is named rather than switched to: switching starts
/// the conversation over.
pub fn announce(expanded: &Expanded) -> String {
    let mut note = format!("/{} expanded into the prompt", expanded.name);
    if let Some(model) = &expanded.model {
        note.push_str(&format!(
            "; it asks for model `{model}` (`/model {model}` switches, starting fresh)"
        ));
    }
    if !expanded.allowed_tools.is_empty() {
        note.push_str(&format!(
            "; allowed-tools ({}) is advisory",
            expanded.allowed_tools.join(", ")
        ));
    }
    crate::cli::render::print_info(&note);
    expanded.prompt.clone()
}

/// For `incode -t "<instruction>"`: the instruction expanded when it names a
/// custom command, and the model to use (`-m` first, then the command's).
#[must_use]
pub fn expand_task(
    workspace: &Path,
    instruction: String,
    model: Option<String>,
) -> (String, Option<String>) {
    if !instruction.trim_start().starts_with('/') {
        return (instruction, model);
    }
    let registry = Registry::load(workspace);
    match registry.expand_line(&instruction) {
        Some(expanded) => {
            eprintln!("incode: running custom command /{}", expanded.name);
            if !expanded.allowed_tools.is_empty() {
                eprintln!(
                    "incode: allowed-tools ({}) is advisory and not enforced",
                    expanded.allowed_tools.join(", ")
                );
            }
            (expanded.prompt, model.or(expanded.model))
        }
        None => (instruction, model),
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
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
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
    let words = split_args(args);
    let mut out = String::with_capacity(body.len() + args.len());
    let mut used = false;
    let mut rest = body;
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
    if !used && !args.is_empty() {
        out.push_str("\n\nARGUMENTS: ");
        out.push_str(args);
    }
    out
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
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    #[test]
    fn names_come_from_the_path_with_colons() {
        let dir = Path::new("/c");
        assert_eq!(
            command_name(dir, Path::new("/c/frontend/test.md")).as_deref(),
            Some("frontend:test")
        );
        assert_eq!(
            command_name(dir, Path::new("/c/a/b/c.md")).as_deref(),
            Some("a:b:c")
        );
        assert_eq!(
            command_name(dir, Path::new("/c/review.md")).as_deref(),
            Some("review")
        );
        assert_eq!(command_name(dir, Path::new("/c/has space.md")), None);
    }

    #[test]
    fn front_matter_is_read_and_the_body_kept() {
        let c = parse(
            "---\ndescription: Run the tests\nargument-hint: [file] [pattern]\nmodel: sonnet\n\
             allowed-tools: Bash(npm test:*), Read\n---\n\nTest $1\n",
        )
        .expect("parses");
        assert_eq!(c.description, "Run the tests");
        assert_eq!(c.argument_hint.as_deref(), Some("[file] [pattern]"));
        assert_eq!(c.model.as_deref(), Some("sonnet"));
        assert_eq!(c.allowed_tools, vec!["Bash(npm test:*)", "Read"]);
        assert_eq!(c.body, "Test $1");
    }

    #[test]
    fn a_hint_in_brackets_alone_is_kept_literally() {
        let c = parse("---\nargument-hint: [file]\nallowed-tools: [Read, Grep]\n---\nx")
            .expect("parses");
        assert_eq!(c.argument_hint.as_deref(), Some("[file]"));
        assert_eq!(c.allowed_tools, vec!["Read", "Grep"]);
    }

    #[test]
    fn no_front_matter_is_all_body() {
        let c = parse("Just do it.\n").expect("parses");
        assert_eq!(c.body, "Just do it.");
        assert!(c.description.is_empty());
        assert!(parse("---\nnot a field\n---\nx").is_err());
    }

    #[test]
    fn arguments_split_like_a_shell() {
        assert_eq!(
            split_args(r#"a "b c" 'd "e"' f\ g """#),
            vec!["a", "b c", "d \"e\"", "f g", ""]
        );
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn arguments_are_substituted() {
        assert_eq!(
            substitute("Fix $1 in $2; all: $ARGUMENTS", r#"bug "src/a b.rs""#),
            r#"Fix bug in src/a b.rs; all: bug "src/a b.rs""#
        );
        assert_eq!(
            substitute("missing [$3] cost $$5", "x"),
            "missing [] cost $"
        );
        assert_eq!(substitute("$0 and $ alone", ""), "$0 and $ alone");
        assert_eq!(substitute("No placeholders.", ""), "No placeholders.");
        assert_eq!(
            substitute("No placeholders.", "extra words"),
            "No placeholders.\n\nARGUMENTS: extra words"
        );
    }

    #[test]
    fn project_beats_user_and_builtins_beat_both() {
        let dir = tempfile::tempdir().expect("dir");
        let project = dir.path().join("p");
        let user = dir.path().join("u");
        write(&project.join("deploy.md"), "project deploy");
        write(&user.join("deploy.md"), "user deploy");
        write(
            &user.join("frontend/test.md"),
            "---\ndescription: t\n---\nrun $1",
        );
        write(&user.join("help.md"), "not the real help");
        write(&user.join("notes.txt"), "ignored");

        let r = Registry::load_from(&project, &user, &[]);
        let names: Vec<_> = r.commands().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["deploy", "frontend:test"]);
        assert_eq!(r.find("deploy").expect("found").body, "project deploy");
        assert_eq!(r.find("deploy").expect("found").origin, Origin::Project);
        assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
        assert!(r.warnings.iter().any(|w| w.contains("built-in /help")));
        assert!(r.warnings.iter().any(|w| w.contains("/deploy")));

        // A built-in is never expanded, even with a file of its name.
        assert_eq!(r.expand_line("/help"), None);
        assert_eq!(r.expand_line("/nothing here"), None);
        assert_eq!(r.expand_line("not a command"), None);
        let e = r.expand_line("/frontend:test app.tsx").expect("expands");
        assert_eq!(e.prompt, "run app.tsx");
        assert_eq!(e.name, "frontend:test");
        // Names are matched without regard to case, as the parser lowercases.
        assert!(r.expand_line("/Frontend:Test").is_some());

        let menu = r.menu_entries();
        assert_eq!(
            menu[1],
            ("/frontend:test".to_string(), "t (user)".to_string())
        );
    }

    #[test]
    fn plugin_commands_are_namespaced_and_lose_to_the_users() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("plugins/web");
        write(&root.join(crate::plugins::MANIFEST), "name = \"web\"\n");
        write(&root.join("commands/deploy.md"), "deploy from plugin");
        write(&root.join("commands/ci/run.md"), "run ci");
        let user = dir.path().join("u");
        write(&user.join("web/deploy.md"), "deploy from user");

        let found = crate::plugins::discover_in(None, &dir.path().join("plugins"));
        let r = Registry::load_from(&dir.path().join("p"), &user, &found.plugins);
        let names: Vec<_> = r.commands().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["web:deploy", "web:ci:run"]);
        assert_eq!(r.find("web:deploy").expect("found").origin, Origin::User);
        assert_eq!(
            r.find("web:ci:run").expect("found").origin,
            Origin::Plugin("web".to_string())
        );
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
    }

    #[test]
    fn an_oversized_file_is_reported_not_read() {
        let dir = tempfile::tempdir().expect("dir");
        write(
            &dir.path().join("big.md"),
            &"x".repeat(MAX_FILE_BYTES as usize + 1),
        );
        let (found, problems) = scan_dir(dir.path(), None);
        assert!(found.is_empty());
        assert_eq!(problems.len(), 1);
    }
}
