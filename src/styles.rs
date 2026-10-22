//! Output styles: a named way of answering, added to the system prompt.
//!
//! A style is a Markdown file — `<workspace>/.incode/styles/<name>.md`, then
//! `~/.incode/styles/<name>.md`, the project's winning a clash — whose body is
//! told to the agent, and whose optional front matter gives a `description`.
//! None ships with incode: which ones exist is the operator's to write.
//!
//! Which one is in force: `INCODE_OUTPUT_STYLE`, else `output_style` in
//! `~/.incode/admin.json`, else none (`off` or `none` names none on purpose).
//! A name no folder has is said, never quietly dropped. `/style` lists them
//! and switches for the rest of a session.
//!
//! The block shapes how replies read; it does not loosen anything. It goes
//! after every other block of the prompt and says the rules above it win.
//!
//! # Wired to
//!
//! - `cli::prepare_session` (REPL and `--acp`) and `task_run` (`-t`) add the
//!   block; `cli::agent_turn::AgentSession::set_output_style` swaps it.
//! - `agent::budget_for_spawn` keeps it when a long prompt has to be cut: it
//!   is one of the blocks matched as high priority by [`TAG`].
//! - `admin::AdminConfig::report` names the style and where it came from.

use std::path::{Path, PathBuf};

use crate::admin::{Resolved, Source};

/// In the workspace.
pub const PROJECT_DIR: &str = ".incode/styles";
/// In incode's data directory (`~/.incode/`).
pub const USER_DIR: &str = "styles";
/// Names the style for this shell, ahead of `admin.json`.
pub const ENV: &str = "INCODE_OUTPUT_STYLE";
/// The block's tag. `agent::budget_for_spawn` matches it.
pub const TAG: &str = "<output-style";

/// A style file this big is refused: it is a prompt, not a document.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// One style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Style {
    pub name: String,
    pub description: Option<String>,
    pub body: String,
    pub source: PathBuf,
}

/// Every style found, and why any file was left out.
#[derive(Debug, Clone, Default)]
pub struct Styles {
    pub styles: Vec<Style>,
    pub problems: Vec<String>,
}

impl Styles {
    /// The style called `name`, exactly or in another letter case.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&Style> {
        self.styles.iter().find(|s| s.name == name).or_else(|| {
            self.styles
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(name))
        })
    }
}

/// The styles of `workspace` and of the operator, the project's first.
#[must_use]
pub fn load(workspace: &Path) -> Styles {
    load_dirs(&[
        workspace.join(PROJECT_DIR),
        crate::admin::AdminConfig::default_dir().join(USER_DIR),
    ])
}

/// Styles from `dirs`, earliest first; a name already found is shadowed.
fn load_dirs(dirs: &[PathBuf]) -> Styles {
    let mut found = Styles::default();
    for dir in dirs {
        for style in read_dir(dir, &mut found.problems) {
            if found.find(&style.name).is_none() {
                found.styles.push(style);
            }
        }
    }
    found
}

fn read_dir(dir: &Path, problems: &mut Vec<String>) -> Vec<Style> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            problems.push(format!("{}: {e}", dir.display()));
            return Vec::new();
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(e) => problems.push(format!("{}: {e}", dir.display())),
        }
    }
    paths.sort();
    let mut styles = Vec::new();
    for path in paths {
        let is_md = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("md"));
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_md || stem.starts_with('.') || !path.is_file() {
            continue;
        }
        match read_style(&path, stem) {
            Ok(style) => styles.push(style),
            Err(why) => problems.push(format!("{}: {why}; left out", path.display())),
        }
    }
    styles
}

fn read_style(path: &Path, name: &str) -> Result<Style, String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("a style's file name may hold only letters, digits, `-` and `_`".into());
    }
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > MAX_FILE_BYTES {
        return Err(format!(
            "{size} bytes, over the {MAX_FILE_BYTES}-byte limit"
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let (description, body) = match crate::subagents::split_front_matter(&text) {
        Some((front, body)) => {
            let mut description = None;
            for (key, value) in crate::subagents::parse_fields(front)? {
                match (key.as_str(), value) {
                    ("description", crate::subagents::Field::One(text)) => {
                        description = Some(text);
                    }
                    (other, _) => return Err(format!("unknown front-matter key `{other}`")),
                }
            }
            (description, body)
        }
        None => (None, text.as_str()),
    };
    let body = body.trim();
    if body.is_empty() {
        return Err("it says nothing to follow".into());
    }
    Ok(Style {
        name: name.to_string(),
        description,
        body: body.to_string(),
        source: path.to_path_buf(),
    })
}

/// The style asked for, and from where: the environment first, then
/// `admin.json`. `None` when neither names one, or one says `off`/`none`.
#[must_use]
pub fn chosen(env: Option<&str>, admin: Option<&str>) -> Option<Resolved<String>> {
    fn pick(value: Option<&str>) -> Option<&str> {
        value.map(str::trim).filter(|v| !v.is_empty())
    }
    let (value, source) = match (pick(env), pick(admin)) {
        (Some(v), _) => (v, Source::Environment),
        (None, Some(v)) => (v, Source::AdminFile),
        (None, None) => return None,
    };
    if value.eq_ignore_ascii_case("off") || value.eq_ignore_ascii_case("none") {
        return None;
    }
    Some(Resolved {
        value: value.to_string(),
        source,
    })
}

/// The prompt block for `style`.
#[must_use]
pub fn block(style: &Style) -> String {
    format!(
        "{TAG} name=\"{}\">\nThe person chose this way of answering. Follow it in every \
         reply. It shapes how you answer, never what you may do: everything above it \
         still holds.\n\n{}\n</output-style>",
        style.name, style.body
    )
}

/// Whether `text` is a block [`block`] made.
#[must_use]
pub fn is_block(text: &str) -> bool {
    text.trim_start().starts_with(TAG)
}

/// What a session starts with: the block, if a style is in force and found,
/// and the lines to show the person.
#[derive(Debug, Clone, Default)]
pub struct ForSession {
    /// The style in force, by its file's name.
    pub name: Option<String>,
    pub block: Option<String>,
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
}

/// The style for a session in `workspace`, `admin_choice` being
/// `output_style` in `admin.json`.
#[must_use]
pub fn for_session(workspace: &Path, admin_choice: Option<&str>) -> ForSession {
    let env = std::env::var(ENV).ok();
    let Some(chosen) = chosen(env.as_deref(), admin_choice) else {
        return ForSession::default();
    };
    let styles = load(workspace);
    resolve(&styles, &chosen)
}

fn resolve(styles: &Styles, chosen: &Resolved<String>) -> ForSession {
    let mut out = ForSession {
        warnings: styles.problems.clone(),
        ..ForSession::default()
    };
    match styles.find(&chosen.value) {
        Some(style) => {
            out.notes.push(format!(
                "output style: {} <- {}",
                style.name,
                chosen.source.as_str()
            ));
            out.name = Some(style.name.clone());
            out.block = Some(block(style));
        }
        None => out.warnings.push(format!(
            "output style `{}` (from {}) is in neither {PROJECT_DIR}/ nor ~/.incode/{USER_DIR}/; \
             none is applied",
            chosen.value,
            chosen.source.as_str()
        )),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::create_dir_all(dir).expect("dir");
        std::fs::write(dir.join(name), text).expect("write");
    }

    #[test]
    fn styles_are_found_with_their_descriptions_and_the_project_wins() {
        let project = tempfile::tempdir().expect("project");
        let user = tempfile::tempdir().expect("user");
        write(
            project.path(),
            "concise.md",
            "---\ndescription: Short answers\n---\nAnswer in three sentences or fewer.\n",
        );
        write(user.path(), "concise.md", "The operator's own concise.\n");
        write(user.path(), "teacher.md", "Explain why, then how.\n");
        write(user.path(), "notes.txt", "not a style");
        let found = load_dirs(&[project.path().into(), user.path().into()]);
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        let names: Vec<&str> = found.styles.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["concise", "teacher"]);
        let concise = found.find("CONCISE").expect("case-insensitive");
        assert_eq!(concise.description.as_deref(), Some("Short answers"));
        assert_eq!(concise.body, "Answer in three sentences or fewer.");
    }

    /// A file that cannot be a style is named, not silently skipped.
    #[test]
    fn a_file_that_cannot_be_a_style_is_reported() {
        let dir = tempfile::tempdir().expect("dir");
        write(dir.path(), "empty.md", "---\ndescription: x\n---\n   \n");
        write(dir.path(), "odd.md", "---\nmodel: big\n---\nbody\n");
        write(dir.path(), "has space.md", "body\n");
        write(
            dir.path(),
            "huge.md",
            &"x".repeat(usize::try_from(MAX_FILE_BYTES).expect("fits") + 1),
        );
        let found = load_dirs(&[dir.path().into()]);
        assert!(found.styles.is_empty(), "{:?}", found.styles);
        assert_eq!(found.problems.len(), 4, "{:?}", found.problems);
        let all = found.problems.join("\n");
        for why in [
            "says nothing",
            "unknown front-matter key `model`",
            "file name",
            "limit",
        ] {
            assert!(all.contains(why), "{why}: {all}");
        }
    }

    #[test]
    fn the_environment_beats_admin_json_and_off_names_none() {
        let env = chosen(Some("teacher"), Some("concise")).expect("one");
        assert_eq!(
            (env.value.as_str(), env.source),
            ("teacher", Source::Environment)
        );
        let admin = chosen(None, Some(" concise ")).expect("one");
        assert_eq!(
            (admin.value.as_str(), admin.source),
            ("concise", Source::AdminFile)
        );
        assert!(chosen(Some("off"), Some("concise")).is_none());
        assert!(chosen(None, Some("None")).is_none());
        assert!(chosen(Some("  "), None).is_none());
        assert!(chosen(None, None).is_none());
    }

    /// **A NAME NO FOLDER HAS IS SAID.** The operator asked for a style; a
    /// session that quietly ran without it would look like the style failed.
    #[test]
    fn a_style_asked_for_and_not_found_is_a_warning_not_a_silent_default() {
        let dir = tempfile::tempdir().expect("dir");
        write(dir.path(), "teacher.md", "Explain why.\n");
        let styles = load_dirs(&[dir.path().into()]);
        let missing = resolve(
            &styles,
            &Resolved {
                value: "concise".into(),
                source: Source::AdminFile,
            },
        );
        assert!(missing.block.is_none());
        assert!(
            missing.warnings[0].contains("`concise` (from admin file)"),
            "{:?}",
            missing.warnings
        );
        let found = resolve(
            &styles,
            &Resolved {
                value: "teacher".into(),
                source: Source::Environment,
            },
        );
        let block = found.block.expect("a block");
        assert!(is_block(&block));
        assert!(block.contains("Explain why."));
        assert!(block.contains("name=\"teacher\""));
        assert_eq!(found.notes, ["output style: teacher <- env"]);
    }

    /// The block is kept when a long prompt is cut for a command line.
    #[test]
    fn the_style_block_survives_the_command_line_budget() {
        let style = Style {
            name: "teacher".into(),
            description: None,
            body: "Explain why, then how.".into(),
            source: PathBuf::from("teacher.md"),
        };
        // Past the limit on every OS, so the cut really happens.
        let filler = "x".repeat(crate::agent::MAX_CMDLINE_PAYLOAD_LEN);
        let context = vec![filler.clone(), block(&style), filler];
        let workspace = tempfile::tempdir().expect("workspace");
        let (_, kept) =
            crate::agent::budget_for_spawn("hello", &context, workspace.path()).expect("budget");
        assert!(
            kept.len() < 2 * crate::agent::MAX_CMDLINE_PAYLOAD_LEN,
            "nothing was cut"
        );
        assert!(kept.contains("Explain why, then how."), "the style was cut");
    }
}
