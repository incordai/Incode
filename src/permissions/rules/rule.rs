//! One rule: `Tool` or `Tool(specifier)`, parsed into the tools it names and
//! the pattern its specifier is matched with.

use std::path::Path;

use super::path::PathPattern;

/// One parsed rule.
#[derive(Debug, Clone)]
pub struct Rule {
    /// As written.
    pub text: String,
    tool: String,
    pub(super) spec: Spec,
}

#[derive(Debug, Clone)]
pub(super) enum Spec {
    /// Every call of the tool.
    Any,
    /// A shell command pattern.
    Command(CommandPattern),
    /// A path glob.
    Path(PathPattern),
    /// `domain:<host>`.
    Domain(String),
}

/// The tools a `Read(...)` rule covers.
const READ_FAMILY: [&str; 5] = ["Read", "Glob", "Grep", "LS", "Outline"];
/// The tools an `Edit(...)` rule covers.
const EDIT_FAMILY: [&str; 5] = ["Write", "Edit", "MultiEdit", "NotebookEdit", "ApplyPatch"];

impl Rule {
    /// Parse `Tool` or `Tool(specifier)`. `home` expands a `~/` path.
    ///
    /// # Errors
    ///
    /// The rule is not of that shape, or its pattern does not compile.
    pub fn parse(text: &str, home: Option<&Path>) -> Result<Self, String> {
        let trimmed = text.trim();
        let (tool, inner) = match trimmed.find('(') {
            Some(open) => {
                let Some(body) = trimmed[open + 1..].strip_suffix(')') else {
                    return Err(format!("rule `{text}` opens `(` and does not end with `)`"));
                };
                (trimmed[..open].trim(), Some(body.trim()))
            }
            None => (trimmed, None),
        };
        if tool.is_empty()
            || !tool
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '*')
        {
            return Err(format!("rule `{text}` does not start with a tool name"));
        }
        let spec = match inner {
            None | Some("" | "*" | ":*") => Spec::Any,
            Some(body) if tool == "Bash" => Spec::Command(CommandPattern::parse(body)?),
            Some(body) if is_path_tool(tool) => Spec::Path(PathPattern::parse(body, home)?),
            Some(body) => match body.strip_prefix("domain:") {
                Some(host) if !host.trim().is_empty() => {
                    Spec::Domain(host.trim().to_ascii_lowercase())
                }
                // Said, rather than kept as a rule that never matches: a deny
                // nobody notices is not applied is worse than none.
                _ => {
                    return Err(format!(
                        "rule `{text}`: `{tool}` takes no specifier incode understands (Bash \
                         takes a command, file tools a path, WebFetch `domain:<host>`); \
                         write `{tool}` for every call"
                    ))
                }
            },
        };
        Ok(Self {
            text: trimmed.to_string(),
            tool: tool.to_string(),
            spec,
        })
    }

    /// Whether this rule names `tool` at all.
    ///
    /// Called by the rule set's `restricts` and `permits`
    /// (`permissions::rules::set`).
    pub(super) fn names(&self, tool: &str) -> bool {
        let rule = self.tool.as_str();
        if rule == "*" || rule == tool {
            return true;
        }
        if let Some(prefix) = rule.strip_suffix('*') {
            return tool.starts_with(prefix);
        }
        // `mcp__server` covers each of that server's tools.
        if rule.starts_with("mcp__") && rule.matches("__").count() == 1 {
            return tool
                .strip_prefix(rule)
                .is_some_and(|rest| rest.starts_with("__"));
        }
        (rule == "Read" && READ_FAMILY.contains(&tool))
            || (rule == "Edit" && EDIT_FAMILY.contains(&tool))
    }
}

fn is_path_tool(tool: &str) -> bool {
    READ_FAMILY.contains(&tool) || EDIT_FAMILY.contains(&tool)
}

/// A pattern over one simple command, whitespace-normalised.
#[derive(Debug, Clone)]
pub(super) struct CommandPattern {
    regex: regex::Regex,
}

impl CommandPattern {
    fn parse(body: &str) -> Result<Self, String> {
        let (body, prefix) = match body.strip_suffix(":*") {
            Some(head) => (head, true),
            None => (body, false),
        };
        let normal = collapse_whitespace(body);
        if normal.is_empty() {
            return Err("an empty command pattern".to_string());
        }
        let mut source = String::from("^");
        for (i, piece) in normal.split('*').enumerate() {
            if i > 0 {
                source.push_str(".*");
            }
            source.push_str(&regex::escape(piece));
        }
        if prefix {
            // The command itself, or it followed by arguments — never a
            // longer word: `npm test:*` does not match `npm testify`.
            source.push_str("(?: .*)?");
        }
        source.push('$');
        let regex = regex::Regex::new(&source).map_err(|e| e.to_string())?;
        Ok(Self { regex })
    }

    /// Called by the rule set's `restricts` and `permits`
    /// (`permissions::rules::set`).
    pub(super) fn matches(&self, command: &str) -> bool {
        self.regex.is_match(command)
    }
}

/// `text` with every run of whitespace made one space, and none at the ends.
///
/// Called by [`CommandPattern::parse`], and by the splitter
/// (`permissions::rules::split`) for each part of a line.
pub(super) fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
