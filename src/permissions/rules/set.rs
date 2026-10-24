//! The rules in force, by list — every layer's deny, ask and allow rules,
//! and a turn's own allowlist — and how a call is judged against them.

use std::path::Path;

use serde::Deserialize;

use super::rule::{Rule, Spec};
use super::split::{split_command, Segment};
use super::{Effect, Layer, PathContext, RuleLists, Subject, Verdict, PROJECT_SETTINGS};

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
            match crate::permissions::managed::load() {
                crate::permissions::managed::Managed::Absent => {}
                crate::permissions::managed::Managed::Loaded(settings) => {
                    rules.add(&settings.permissions, Layer::Managed, home.as_deref());
                }
                crate::permissions::managed::Managed::Broken(why) => rules.deny_everything(&why),
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
    ///
    /// Called by [`PermissionRules::load`] for a managed file that is there
    /// and broken.
    pub(super) fn deny_everything(&mut self, why: &str) {
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
    let text = match crate::text::read_authored(&path) {
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
