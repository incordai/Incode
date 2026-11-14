//! Parameters: text a routine is GIVEN each time it runs instead of keeping.
//!
//! # Why
//!
//! The routines file holds what a routine types, so that the person can read it
//! and so that a replay needs no model. Text typed into a password or other
//! secure field must not be on disk at all, and neither must a recording's
//! stand-in for text it redacted (replaying `[REDACTED]` into a login box is a
//! wrong password, not a shortcut). Such a step is made a PARAMETER when the
//! routine is learned: its text is stored as `{{name}}`, `routine_run` must be
//! given `params` for every one, and refuses before step 1 when one is missing.
//! The value is substituted into the arguments of the one call that types it, in
//! memory. It is not written to the routines file or the journal
//! ([`crate::mcp`]'s `describe` shows the length only), it is hidden from the
//! operator's hooks, and [`Values::scrub`] takes it back out of any message a
//! replay returns.
//!
//! # `params` are REFERENCES, never the secret
//!
//! The caller's own transcript records the arguments of `routine_run`, so a
//! literal secret in `params` would land in a place incu cannot clean. `params`
//! therefore takes only a reference to where the person keeps the value:
//! `{"password": {"env": "INCU_PARAM_PASSWORD"}}`, read from incu's own
//! environment (the person sets it where incu is launched). A literal string is
//! refused, with an error that names this form. A `keyring` reference is not
//! offered: no credential-store crate is in the build, and one is not added for
//! this; the error says so. An `env` reference must name a variable beginning
//! [`ENV_PREFIX`], so that an agent cannot have incu type out any variable of its
//! environment (an API key of the person's) into a window.
//!
//! # What makes a step a parameter
//!
//! 1. Its text is a stand-in for text that was removed: [`looks_redacted`], or a
//!    value that is not text at all (a recording's `{"redacted": true}`).
//! 2. It goes into a secure field, by the field's own name or automation id
//!    ([`is_secret_name`]), or because the desktop reports the control as a
//!    password field when the routine is saved. A typing step goes into the
//!    control the step before it named ([`Target`]).
//! 3. It goes where focus may have MOVED: after a `key` that can move it (Tab,
//!    Shift+Tab, Enter, the arrows, ...) the control typed into is unknown, and
//!    a login form's password is typed after a Tab. Such a typing step is a
//!    parameter unless it names a non-secret control of its own.
//! 4. The step already has `{{name}}` in its text: the agent made it one.
//!
//! This is a screen on what the learner can see, not a promise: a secure field
//! whose label does not say so, and that is no longer on screen when the routine
//! is saved, is not found. That is why `routine_save`, `routine_list` and
//! `routine_match` show the full text of every step: the person reads what is
//! kept.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::Step;
use crate::selector::{Selector, Text};

/// The arguments that carry typed text, by tool.
fn text_key(tool: &str) -> Option<&'static str> {
    match tool {
        "set_value" => Some("value"),
        "type_text" | "paste_text" => Some("text"),
        _ => None,
    }
}

/// The longest a parameter's value may be, in characters.
pub const MAX_PARAM_CHARS: usize = 1024;

/// What the name of an environment variable a parameter reads must begin with.
pub const ENV_PREFIX: &str = "INCU_PARAM_";

/// The most parameters one routine may take.
pub const MAX_PARAMS: usize = 16;

/// Whether `name` can be a parameter's: a lowercase letter, then lowercase
/// letters, digits and `_`, at most 32.
fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The parameter names in `text`, in order of appearance (`{{name}}`).
#[must_use]
pub fn tokens(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) if is_name(&after[..end]) => {
                found.push(after[..end].to_string());
                rest = &after[end + 2..];
            }
            _ => rest = after,
        }
    }
    found
}

/// The parameter names a step takes.
#[must_use]
pub fn names_of(step: &Step) -> Vec<String> {
    text_key(&step.tool)
        .and_then(|key| step.args.get(key))
        .and_then(Value::as_str)
        .map(tokens)
        .unwrap_or_default()
}

/// The parameter names a routine takes, each once, in order of first use.
#[must_use]
pub fn names(steps: &[Step]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    steps
        .iter()
        .flat_map(names_of)
        .filter(|n| seen.insert(n.clone()))
        .collect()
}

/// The values a run was given, by name.
#[derive(Debug, Default, Clone)]
pub struct Values(BTreeMap<String, String>);

impl Values {
    /// The values the references in `given` (the `params` argument of
    /// `routine_run`) point at, for the parameters `steps` take; the environment
    /// variables are read from this process's own.
    ///
    /// # Errors
    ///
    /// See [`Values::bind_with`].
    pub fn bind(steps: &[Step], given: Option<&Value>) -> Result<Values, String> {
        Self::bind_with(steps, given, &|var| std::env::var(var).ok())
    }

    /// [`Values::bind`] with `env` as the environment, so a test need not set
    /// the process's.
    ///
    /// # Errors
    ///
    /// A parameter has no reference; a reference is not `{"env": "INCU_PARAM_..."}`
    /// (a literal value is refused by name, because it would be in the caller's
    /// transcript); the variable is not set or is too long; or a reference is
    /// given for a name the routine does not take (a typo that would otherwise
    /// type nothing where something was meant). No error ever contains a value.
    pub fn bind_with(
        steps: &[Step],
        given: Option<&Value>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Values, String> {
        let needed = names(steps);
        if needed.len() > MAX_PARAMS {
            return Err(format!(
                "this routine takes {} parameters, more than the {MAX_PARAMS} one may; it was \
                 not made by this program",
                needed.len()
            ));
        }
        let map = match given {
            None | Some(Value::Null) => serde_json::Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(_) => {
                return Err("params must be an object of name to reference, e.g. \
                     {\"password\": {\"env\": \"INCU_PARAM_PASSWORD\"}}"
                    .to_string())
            }
        };
        let mut values = BTreeMap::new();
        for (name, reference) in &map {
            if !needed.contains(name) {
                return Err(format!(
                    "this routine takes no parameter named {name:?}{}",
                    listing(&needed)
                ));
            }
            let var = env_reference(name, reference)?;
            let value = env(&var).filter(|v| !v.is_empty()).ok_or_else(|| {
                format!(
                    "params.{name}: the environment variable {var} is not set (or is empty) in \
                     incu's environment. The person sets it where incu is launched; the value \
                     is never passed through the agent"
                )
            })?;
            if value.chars().count() > MAX_PARAM_CHARS {
                return Err(format!(
                    "params.{name}: the value of {var} is over {MAX_PARAM_CHARS} characters"
                ));
            }
            values.insert(name.clone(), value);
        }
        let missing: Vec<&String> = needed.iter().filter(|n| !values.contains_key(*n)).collect();
        if !missing.is_empty() {
            return Err(format!(
                "routine_run needs params for: {}. Nothing was done. These are the secrets \
                 and redacted texts the routine does not keep. Pass each as a REFERENCE to \
                 where the person keeps it, never the value: params: {{\"{}\": {{\"env\": \
                 \"{ENV_PREFIX}{}\"}}}}",
                missing
                    .iter()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                missing[0],
                missing[0].to_uppercase()
            ));
        }
        Ok(Values(values))
    }

    /// Whether there are no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `step` with its `{{name}}` parts replaced by the values. In one pass: a
    /// value that itself contains `{{x}}` is typed as it is.
    #[must_use]
    pub fn fill(&self, step: &Step) -> Step {
        let Some(key) = text_key(&step.tool) else {
            return step.clone();
        };
        let mut filled = step.clone();
        if let Some(text) = step.args.get(key).and_then(Value::as_str) {
            filled.args[key] = Value::String(self.render(text));
        }
        filled
    }

    fn render(&self, template: &str) -> String {
        let mut out = String::new();
        let mut rest = template;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            match after.find("}}") {
                Some(end) if is_name(&after[..end]) => {
                    let name = &after[..end];
                    match self.0.get(name) {
                        Some(value) => out.push_str(value),
                        None => out.push_str(&rest[start..start + 2 + end + 2]),
                    }
                    rest = &after[end + 2..];
                }
                _ => {
                    out.push_str("{{");
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// `text` with every value put back to its `{{name}}`, for a message that is
    /// returned or journalled. A value of any length is scrubbed (one character
    /// is a value too).
    ///
    /// A value of four characters or more is replaced wherever it stands. A
    /// shorter one only where it stands WHOLE, not inside a longer word or number
    /// (`a` in `cabin`, `12` in `2012`): taken out of the middle of a word it
    /// would eat the message, and a secret that short is not hidden by being a
    /// piece of one anyway.
    #[must_use]
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        let mut by_length: Vec<(&String, &String)> =
            self.0.iter().filter(|(_, v)| !v.is_empty()).collect();
        by_length.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
        for (name, value) in by_length {
            let placeholder = format!("{{{{{name}}}}}");
            out = if value.chars().count() >= WHOLE_ONLY_BELOW {
                out.replace(value.as_str(), &placeholder)
            } else {
                replace_whole(&out, value, &placeholder)
            };
        }
        out
    }
}

/// Values shorter than this many characters are scrubbed only as whole words.
const WHOLE_ONLY_BELOW: usize = 4;

/// `text` with each occurrence of `value` that is not part of a longer run of
/// letters and digits replaced by `with`.
fn replace_whole(text: &str, value: &str, with: &str) -> String {
    let word = |c: char| c.is_alphanumeric();
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(value) {
        let (before, after) = (&rest[..at], &rest[at + value.len()..]);
        // What stands beside it, across what is already out.
        let left_word = before
            .chars()
            .next_back()
            .or_else(|| out.chars().next_back())
            .is_some_and(word);
        let right_word = after.chars().next().is_some_and(word);
        out.push_str(before);
        if left_word && value.chars().next().is_some_and(word)
            || right_word && value.chars().next_back().is_some_and(word)
        {
            out.push_str(value);
        } else {
            out.push_str(with);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// The environment variable a parameter's reference names.
fn env_reference(name: &str, reference: &Value) -> Result<String, String> {
    let example = format!("{{\"env\": \"{ENV_PREFIX}{}\"}}", name.to_uppercase());
    let Some(map) = reference.as_object() else {
        // A string, a number, anything that is the value itself.
        return Err(format!(
            "params.{name} must be a REFERENCE, never the secret itself: the caller's own \
             transcript records the arguments of routine_run, and a secret in it cannot be \
             taken back. Use {example}, with that variable set in the environment of the \
             incu process by the person"
        ));
    };
    if map.contains_key("keyring") {
        return Err(format!(
            "params.{name}: keyring references are not available in this build (no \
             credential-store crate is compiled in). Use {example}"
        ));
    }
    let var = match (map.len(), map.get("env").and_then(Value::as_str)) {
        (1, Some(var)) => var,
        _ => return Err(format!("params.{name} must be exactly {example}")),
    };
    let well_formed = var.len() <= 128
        && var
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !well_formed || !var.starts_with(ENV_PREFIX) || var.len() == ENV_PREFIX.len() {
        return Err(format!(
            "params.{name}: the variable must be named {ENV_PREFIX}<NAME> (capitals, digits and \
             _), so that a routine can only read what the person set aside for it, never the \
             rest of incu's environment. Use {example}"
        ));
    }
    Ok(var.to_string())
}

fn listing(needed: &[String]) -> String {
    if needed.is_empty() {
        "; it takes none".to_string()
    } else {
        format!("; it takes: {}", needed.join(", "))
    }
}

/// Whether a control's name or automation id says it holds a secret.
///
/// The long words match anywhere (`PasswordBox`, `confirm_password`); the short
/// ones only as whole words, so a PIN field is found and a `Pinterest` button is
/// not.
#[must_use]
pub fn is_secret_name(text: &str) -> bool {
    const ANYWHERE: &[&str] = &[
        "password",
        "passwd",
        "passcode",
        "passphrase",
        "secret",
        "credential",
        "apikey",
        "api key",
        "api_key",
        "private key",
        "privatekey",
        "security code",
        "securitycode",
        "verification code",
        "one-time",
        "one time",
        "card number",
        "cardnumber",
        "social security",
    ];
    const AS_WORDS: &[&str] = &[
        "pin", "otp", "pwd", "cvv", "cvc", "token", "ssn", "2fa", "mfa",
    ];
    let lowered = text.to_lowercase();
    if ANYWHERE.iter().any(|w| lowered.contains(w)) {
        return true;
    }
    lowered
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| AS_WORDS.contains(&w))
}

/// Whether the control a selector names is a secret field by what the selector
/// says of it: its name or automation id.
#[must_use]
pub fn selector_is_secret(raw: &str) -> bool {
    let Ok(selector) = Selector::parse(raw) else {
        return false;
    };
    let named = match &selector.name {
        Some(Text::Exact(n) | Text::Contains(n)) => is_secret_name(n),
        None => false,
    };
    named
        || selector
            .automation_id
            .as_deref()
            .is_some_and(is_secret_name)
}

/// Whether `text` is a stand-in for text that was removed from a recording:
/// a placeholder (`[REDACTED]`, `<hidden>`, `****`, `[12 chars]`), or a digest
/// of the value (`sha256:...`, a bare 64-digit hex).
#[must_use]
pub fn looks_redacted(text: &str) -> bool {
    let trimmed = text.trim();
    let lowered = trimmed.to_lowercase();
    if lowered.is_empty() {
        return false;
    }
    if lowered.contains("redact") {
        return true;
    }
    if trimmed.chars().count() >= 3 && trimmed.chars().all(|c| "*•●∗·".contains(c)) {
        return true;
    }
    for prefix in ["sha256:", "sha-256:", "sha512:", "sha1:", "md5:", "blake3:"] {
        if lowered.starts_with(prefix) {
            return true;
        }
    }
    if lowered.len() == 64 && lowered.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    let bracketed = [('[', ']'), ('<', '>'), ('(', ')')]
        .iter()
        .any(|(open, close)| lowered.starts_with(*open) && lowered.ends_with(*close));
    if bracketed {
        let inner = lowered[1..lowered.len() - 1].trim();
        const MARKS: &[&str] = &[
            "secret",
            "hidden",
            "masked",
            "removed",
            "omitted",
            "filtered",
            "sensitive",
            "private",
            "scrubbed",
            "censored",
            "withheld",
            "elided",
            "stripped",
            "digest",
            "sha",
            "hash",
            "value",
            "password",
            "token",
        ];
        if inner.chars().count() <= 48 && MARKS.iter().any(|m| inner.contains(m)) {
            return true;
        }
        let words: Vec<&str> = inner.split_whitespace().collect();
        if words.len() == 2
            && words[0].chars().all(|c| c.is_ascii_digit())
            && matches!(
                words[1],
                "char" | "chars" | "character" | "characters" | "bytes"
            )
        {
            return true;
        }
    }
    false
}

/// Why a step was made a parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// It types into a secure field.
    SecureField,
    /// The recording kept a placeholder or a digest in place of the text.
    Redacted,
    /// It types where focus may have moved (after Tab, Enter, an arrow...), so
    /// the field is not known and may be a secure one.
    UnknownField,
}

/// The control the next typing step goes into, as far as the steps so far say.
///
/// `type_text` takes no selector: it types into whatever has focus. The last
/// step that named a control (a click, a `set_value`) gave it focus; a `key` that
/// can MOVE focus makes it unknown again.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Target {
    /// Nothing has named a control yet: typing goes to the window's own focus.
    #[default]
    Nothing,
    /// The raw selector of the control the last click or `set_value` named.
    Known(String),
    /// A key that moves focus has been pressed since: the field is not known.
    Unknown,
}

/// The keys that can move focus out of the field it was in: Tab (alone or in any
/// chord, which covers Shift+Tab), Enter, and the arrows and page keys.
const FOCUS_KEYS: &[&str] = &[
    "tab",
    "enter",
    "return",
    "kp_enter",
    "up",
    "down",
    "left",
    "right",
    "arrowup",
    "arrowdown",
    "arrowleft",
    "arrowright",
    "up_arrow",
    "down_arrow",
    "left_arrow",
    "right_arrow",
    "pageup",
    "pagedown",
    "page_up",
    "page_down",
    "home",
    "end",
];

/// Whether `step` is a `key` that can move focus.
fn moves_focus(step: &Step) -> bool {
    if step.tool != "key" {
        return false;
    }
    let named = |k: &str| FOCUS_KEYS.contains(&k.trim().to_lowercase().as_str());
    match step.args.get("keys").or_else(|| step.args.get("key")) {
        Some(Value::Array(keys)) => keys.iter().filter_map(Value::as_str).any(named),
        Some(Value::String(one)) => one.split('+').any(named),
        _ => false,
    }
}

/// The raw selector of the control `step` names, if it names one that it also
/// acts on (a click, a `set_value`, an `element_action`: not a wait).
fn control_of(step: &Step) -> Option<String> {
    if !matches!(
        step.tool.as_str(),
        "click_element" | "set_value" | "element_action"
    ) {
        return None;
    }
    step.args
        .get("selector")
        .and_then(Value::as_str)
        .filter(|raw| Selector::parse(raw).is_ok_and(|s| s.has_element_part()))
        .map(str::to_string)
}

impl Target {
    /// The target once `step` has run.
    pub fn after(&mut self, step: &Step) {
        if let Some(control) = control_of(step) {
            *self = Target::Known(control);
        } else if moves_focus(step) {
            *self = Target::Unknown;
        }
    }
}

/// Why a typing step's text must not be kept, if it must not: it is a stand-in
/// for removed text, it goes into a secure field, or it goes where focus may have
/// moved. `None` for text that is kept (and for a step that is not typing, or
/// that already is a parameter).
fn why_not_kept(step: &Step, target: &Target, is_secret: &dyn Fn(&str) -> bool) -> Option<Why> {
    let key = text_key(&step.tool)?;
    let value = step.args.get(key).cloned().unwrap_or(Value::Null);
    match &value {
        Value::String(text) if !tokens(text).is_empty() => return None,
        Value::String(text) if looks_redacted(text) => return Some(Why::Redacted),
        Value::String(_) => {}
        Value::Null => return None,
        // A recording's `{"redacted": true}` or the like: not text.
        _ => return Some(Why::Redacted),
    }
    // The control: a `set_value`'s own; for typing, one it names itself, else the
    // target the steps before it left.
    let own = control_of_any(step);
    let control = match (&own, step.tool.as_str()) {
        (Some(own), _) => Target::Known(own.clone()),
        (None, "set_value") => Target::Nothing,
        (None, _) => target.clone(),
    };
    match control {
        Target::Known(raw) if selector_is_secret(&raw) || is_secret(&raw) => Some(Why::SecureField),
        Target::Known(_) | Target::Nothing => None,
        Target::Unknown => Some(Why::UnknownField),
    }
}

/// The selector a step carries for its own control, whatever the tool.
fn control_of_any(step: &Step) -> Option<String> {
    step.args
        .get("selector")
        .and_then(Value::as_str)
        .filter(|raw| Selector::parse(raw).is_ok_and(|s| s.has_element_part()))
        .map(str::to_string)
}

/// Whether `step`, run when the typing target is `target`, puts a secret on
/// screen: it types a parameter, or text into a field whose NAME says it is
/// secret (or into one not known at all). Used to keep the replay from
/// photographing it; the desktop's own report is not asked here, a picture is
/// not worth a probe.
#[must_use]
pub fn types_a_secret(step: &Step, target: &Target) -> bool {
    !names_of(step).is_empty() || why_not_kept(step, target, &|_| false).is_some()
}

/// One step made a parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Made {
    /// The parameter's name.
    pub name: String,
    /// The step, 1-based.
    pub step: usize,
    /// Why.
    pub why: Why,
}

/// Make the steps that type a secret or a redacted text into parameters, in
/// place, and say which. `is_secret` is asked about the raw selector of a
/// control the name does not already condemn: the desktop's own report of a
/// password field.
pub fn parameterise(steps: &mut [Step], is_secret: &dyn Fn(&str) -> bool) -> Vec<Made> {
    let mut made = Vec::new();
    let mut taken: BTreeSet<String> = names(steps).into_iter().collect();
    let mut target = Target::Nothing;
    for (index, step) in steps.iter_mut().enumerate() {
        let why = why_not_kept(step, &target, is_secret);
        // The control the parameter is named after: the step's own, else the
        // target when it is known.
        let control = control_of_any(step).or(match &target {
            Target::Known(raw) => Some(raw.clone()),
            _ => None,
        });
        if let (Some(why), Some(key)) = (why, text_key(&step.tool)) {
            let name = fresh_name(
                match why {
                    Why::SecureField => "password",
                    Why::Redacted | Why::UnknownField => "text",
                },
                control.as_deref(),
                &mut taken,
            );
            if let Some(map) = step.args.as_object_mut() {
                map.insert(key.to_string(), Value::String(format!("{{{{{name}}}}}")));
            }
            made.push(Made {
                name,
                step: index + 1,
                why,
            });
        }
        target.after(step);
    }
    made
}

/// A parameter name no other has: the control's own name when it makes a good
/// one, else `fallback`, with a number to tell it from the others.
fn fresh_name(fallback: &str, control: Option<&str>, taken: &mut BTreeSet<String>) -> String {
    let from_control = control
        .and_then(|raw| Selector::parse(raw).ok())
        .and_then(|s| match s.name {
            Some(Text::Exact(n) | Text::Contains(n)) => Some(n),
            None => None,
        })
        .map(|n| {
            let mut slug: String = n
                .to_lowercase()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            slug.truncate(24);
            slug.trim_matches('_').to_string()
        })
        .filter(|s| is_name(s));
    let base = from_control.unwrap_or_else(|| fallback.to_string());
    let mut name = base.clone();
    let mut n = 2;
    while !taken.insert(name.clone()) {
        name = format!("{base}_{n}");
        n += 1;
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn no_probe(_: &str) -> bool {
        false
    }

    #[test]
    fn placeholders_are_found_only_when_they_are_well_formed_names() {
        assert_eq!(tokens("a {{password}} b {{pin_2}}"), ["password", "pin_2"]);
        assert!(tokens("{{Password}} {{1x}} {{}} {{a b}} {{ x }} {{x").is_empty());
        assert_eq!(tokens("{{{{x}}"), ["x"]);
    }

    #[test]
    fn a_routines_parameters_are_its_placeholders_once_each_in_order() {
        let steps = [
            Step::new("type_text", json!({"text": "{{user}} / {{password}}"})),
            Step::new(
                "set_value",
                json!({"value": "{{password}}", "selector": "x"}),
            ),
            Step::new("key", json!({"keys": ["enter"]})),
        ];
        assert_eq!(names(&steps), ["user", "password"]);
    }

    #[test]
    fn a_run_must_give_every_parameter_and_only_the_ones_there_are() {
        let steps = [Step::new("type_text", json!({"text": "{{password}}"}))];
        let refer = json!({"env": "INCU_PARAM_PASSWORD"});
        let missing = Values::bind_with(&steps, None, &no_env).unwrap_err();
        assert!(
            missing.contains("password") && missing.contains("Nothing was done"),
            "{missing}"
        );
        // The error shows the form to use, never a value.
        assert!(missing.contains("INCU_PARAM_PASSWORD"), "{missing}");
        let unknown = Values::bind_with(
            &steps,
            Some(&json!({"password": refer, "pasword": refer})),
            &secret_env,
        )
        .unwrap_err();
        assert!(
            unknown.contains("pasword") && unknown.contains("password"),
            "{unknown}"
        );
        assert!(Values::bind_with(&steps, Some(&json!({"password": 5})), &secret_env).is_err());
        assert!(Values::bind_with(&steps, Some(&json!("x")), &secret_env).is_err());
        let long = |_: &str| Some("x".repeat(MAX_PARAM_CHARS + 1));
        assert!(Values::bind_with(&steps, Some(&json!({"password": refer})), &long).is_err());
        let bound =
            Values::bind_with(&steps, Some(&json!({"password": refer})), &secret_env).unwrap();
        assert!(!bound.is_empty());
        // A routine with none needs none, and says so when one is offered.
        let plain = [Step::new("key", json!({"keys": ["enter"]}))];
        assert!(Values::bind_with(&plain, None, &no_env).unwrap().is_empty());
        let extra = Values::bind_with(&plain, Some(&json!({"x": refer})), &secret_env).unwrap_err();
        assert!(extra.contains("takes none"), "{extra}");
    }

    /// The environment of a test: this variable and no other.
    fn secret_env(var: &str) -> Option<String> {
        (var == "INCU_PARAM_PASSWORD").then(|| "hunter2-secret".to_string())
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// A literal secret in `params` would be in the caller's transcript: refused,
    /// naming the form to use, and never repeated.
    #[test]
    fn a_literal_value_is_refused_and_the_error_names_the_env_form() {
        let steps = [Step::new("type_text", json!({"text": "{{password}}"}))];
        let why = Values::bind_with(
            &steps,
            Some(&json!({"password": "hunter2-secret"})),
            &secret_env,
        )
        .unwrap_err();
        assert!(why.contains("REFERENCE"), "{why}");
        assert!(why.contains("{\"env\": \"INCU_PARAM_PASSWORD\"}"), "{why}");
        assert!(!why.contains("hunter2"), "the value was repeated: {why}");
        // The reference must be exactly that and nothing else.
        for bad in [
            json!({"env": "HOME"}),
            json!({"env": "PATH"}),
            json!({"env": "ANTHROPIC_API_KEY"}),
            json!({"env": "INCU_PARAM_"}),
            json!({"env": "incu_param_password"}),
            json!({"env": "INCU_PARAM_A B"}),
            json!({"env": "INCU_PARAM_PASSWORD", "value": "x"}),
            json!({"env": 5}),
            json!({}),
        ] {
            let why = Values::bind_with(&steps, Some(&json!({"password": bad})), &|_| {
                Some("leaked".to_string())
            })
            .unwrap_err();
            assert!(!why.contains("leaked"), "{bad}: {why}");
        }
        // A variable that is not set, or is empty, is said by name.
        let unset = Values::bind_with(
            &steps,
            Some(&json!({"password": {"env": "INCU_PARAM_PASSWORD"}})),
            &no_env,
        )
        .unwrap_err();
        assert!(
            unset.contains("INCU_PARAM_PASSWORD") && unset.contains("not set"),
            "{unset}"
        );
        let empty = |_: &str| Some(String::new());
        assert!(Values::bind_with(
            &steps,
            Some(&json!({"password": {"env": "INCU_PARAM_PASSWORD"}})),
            &empty
        )
        .is_err());
    }

    #[test]
    fn a_keyring_reference_is_refused_and_says_there_is_no_keyring_here() {
        let steps = [Step::new("type_text", json!({"text": "{{password}}"}))];
        let why = Values::bind_with(
            &steps,
            Some(&json!({"password": {"keyring": "vault/me"}})),
            &secret_env,
        )
        .unwrap_err();
        assert!(
            why.contains("keyring") && why.contains("not available"),
            "{why}"
        );
        assert!(why.contains("INCU_PARAM_PASSWORD"), "{why}");
    }

    #[test]
    fn a_routine_may_take_no_more_than_the_most_parameters() {
        let steps: Vec<Step> = (0..=MAX_PARAMS)
            .map(|i| Step::new("type_text", json!({"text": format!("{{{{p{i}}}}}")})))
            .collect();
        assert_eq!(names(&steps).len(), MAX_PARAMS + 1);
        let why = Values::bind_with(&steps, None, &no_env).unwrap_err();
        assert!(why.contains("more than the 16"), "{why}");
    }

    #[test]
    fn a_value_is_substituted_in_one_pass_and_scrubbed_from_what_comes_back() {
        let steps = [Step::new("type_text", json!({"text": "pw={{password}}!"}))];
        let refer = json!({"password": {"env": "INCU_PARAM_PASSWORD"}});
        let tricky = |_: &str| Some("a{{password}}b".to_string());
        let values = Values::bind_with(&steps, Some(&refer), &tricky).unwrap();
        let filled = values.fill(&steps[0]);
        assert_eq!(filled.args["text"], "pw=a{{password}}b!");
        // The stored step is untouched.
        assert_eq!(steps[0].args["text"], "pw={{password}}!");
        let message = "could not type hunter2-secret into the window";
        let values = Values::bind_with(&steps, Some(&refer), &secret_env).unwrap();
        assert_eq!(
            values.scrub(message),
            "could not type {{password}} into the window"
        );
    }

    /// A value of any length is scrubbed; a short one only where it stands whole.
    #[test]
    fn a_short_value_is_scrubbed_whole_and_never_out_of_the_middle_of_a_word() {
        let steps = [Step::new("type_text", json!({"text": "{{password}}"}))];
        let refer = json!({"password": {"env": "INCU_PARAM_PASSWORD"}});
        let with = |value: &'static str| {
            Values::bind_with(&steps, Some(&refer), &move |_| Some(value.to_string())).unwrap()
        };
        // One character.
        assert_eq!(
            with("x").scrub("typed x into box"),
            "typed {{password}} into box"
        );
        assert_eq!(
            with("x").scrub("the box"),
            "the box",
            "x inside a word stays"
        );
        assert_eq!(with("x").scrub("x"), "{{password}}");
        assert_eq!(
            with("x").scrub("x,x;(x)"),
            "{{password}},{{password}};({{password}})"
        );
        // Two and three.
        assert_eq!(with("ab").scrub("a cab ab"), "a cab {{password}}");
        assert_eq!(with("12").scrub("in 2012: 12"), "in 2012: {{password}}");
        assert_eq!(
            with("pin").scrub("the pin, not a spinner"),
            "the {{password}}, not a spinner"
        );
        // Four and more: anywhere, because the whole secret is the evidence.
        assert_eq!(
            with("hunt").scrub("it was hunter2 and hunt"),
            "it was {{password}}er2 and {{password}}"
        );
        // Longer values go first, so a short one inside a long one cannot split it.
        let two = [Step::new("type_text", json!({"text": "{{a}} {{b}}"}))];
        let refs = json!({"a": {"env": "INCU_PARAM_A"}, "b": {"env": "INCU_PARAM_B"}});
        let env = |var: &str| match var {
            "INCU_PARAM_A" => Some("secret".to_string()),
            "INCU_PARAM_B" => Some("secret-longer".to_string()),
            _ => None,
        };
        let both = Values::bind_with(&two, Some(&refs), &env).unwrap();
        assert_eq!(both.scrub("secret-longer and secret"), "{{b}} and {{a}}");
    }

    #[test]
    fn a_field_whose_name_says_secret_is_secret_and_a_lookalike_is_not() {
        for yes in [
            "Password",
            "Confirm password",
            "PasswordBox",
            "new_passcode",
            "PIN",
            "Enter your PIN",
            "OTP code",
            "API key",
            "Security code",
            "Token",
            "CVV",
        ] {
            assert!(is_secret_name(yes), "{yes}");
        }
        for no in [
            "Username",
            "Search",
            "Pinterest",
            "Spinner",
            "Subject",
            "Tokyo",
            "Body",
        ] {
            assert!(!is_secret_name(no), "{no}");
        }
        assert!(selector_is_secret(
            "app=login;window=Sign in;role=edit;name=Password"
        ));
        assert!(selector_is_secret(
            "app=login;window=Sign in;role=edit;aid=txtPassword"
        ));
        assert!(!selector_is_secret(
            "app=login;window=Sign in;role=edit;name=Email"
        ));
        assert!(!selector_is_secret("not a selector"));
    }

    #[test]
    fn what_a_recording_keeps_in_place_of_text_is_recognised() {
        for yes in [
            "[REDACTED]",
            "<redacted>",
            "***",
            "••••••••",
            "[secret]",
            "<hidden>",
            "[12 chars]",
            "<8 characters>",
            "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "[value removed]",
            "(redacted by watchman)",
        ] {
            assert!(looks_redacted(yes), "{yes}");
        }
        for no in [
            "hello",
            "",
            "ab",
            "two words",
            "[ ] todo",
            "<b>bold</b>",
            "(optional)",
            "12 characters long",
            "deadbeef",
        ] {
            assert!(!looks_redacted(no), "{no}");
        }
    }

    #[test]
    fn typing_into_a_password_field_becomes_a_parameter_and_the_secret_is_not_kept() {
        let mut steps = vec![
            Step::new(
                "set_value",
                json!({"selector": "app=login;window=Sign in;role=edit;name=Email", "value": "me@example.com"}),
            ),
            Step::new(
                "click_element",
                json!({"selector": "app=login;window=Sign in;role=edit;name=Password"}),
            ),
            Step::new(
                "type_text",
                json!({"text": "hunter2-secret", "in_window": "app=login;window=Sign in"}),
            ),
        ];
        let made = parameterise(&mut steps, &no_probe);
        assert_eq!(
            made,
            [Made {
                name: "password".to_string(),
                step: 3,
                why: Why::SecureField
            }]
        );
        assert_eq!(steps[2].args["text"], "{{password}}");
        // The email is ordinary text and stays.
        assert_eq!(steps[0].args["value"], "me@example.com");
        assert!(!serde_json::to_string(&steps).unwrap().contains("hunter2"));
        assert_eq!(names(&steps), ["password"]);
    }

    #[test]
    fn a_set_value_on_a_secret_field_is_a_parameter_named_after_it() {
        let mut steps = vec![Step::new(
            "set_value",
            json!({"selector": "app=vault;role=edit;name=Token", "value": "hunter2-secret"}),
        )];
        let made = parameterise(&mut steps, &no_probe);
        assert_eq!(made[0].name, "token");
        assert_eq!(steps[0].args["value"], "{{token}}");
    }

    #[test]
    fn the_desktops_own_report_of_a_password_field_counts_when_the_name_does_not_say() {
        let mut steps = vec![Step::new(
            "set_value",
            json!({"selector": "app=bank;window=Login;role=edit;name=Enter it here", "value": "s3cret"}),
        )];
        let seen_as_password = |raw: &str| raw.contains("Enter it here");
        let made = parameterise(&mut steps, &seen_as_password);
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].why, Why::SecureField);
        assert!(!steps[0].args.to_string().contains("s3cret"));
    }

    /// The login form: click the user name, type it, Tab, type the password,
    /// Enter. The password is in a field no selector named.
    #[test]
    fn a_password_typed_after_a_tab_is_a_parameter() {
        let window = "app=login;window=Sign in";
        let mut steps = vec![
            Step::new(
                "click_element",
                json!({"selector": format!("{window};role=edit;name=Username")}),
            ),
            Step::new("type_text", json!({"text": "alice", "in_window": window})),
            Step::new("key", json!({"keys": ["Tab"], "in_window": window})),
            Step::new("type_text", json!({"text": "hunter2", "in_window": window})),
            Step::new("key", json!({"keys": ["Enter"], "in_window": window})),
        ];
        let made = parameterise(&mut steps, &no_probe);
        assert_eq!(made.len(), 1, "{made:?}");
        assert_eq!((made[0].step, made[0].why), (4, Why::UnknownField));
        assert_eq!(steps[3].args["text"], "{{text}}");
        assert!(!serde_json::to_string(&steps).unwrap().contains("hunter2"));
        // Before the Tab the target was the user name field: its text is kept.
        assert_eq!(steps[1].args["text"], "alice");
    }

    #[test]
    fn every_key_that_can_move_focus_makes_the_next_typing_a_parameter() {
        let window = "app=form;window=F";
        let after = |keys: Value| {
            let mut steps = vec![
                Step::new(
                    "click_element",
                    json!({"selector": format!("{window};role=edit;name=Name")}),
                ),
                Step::new("key", json!({"keys": keys, "in_window": window})),
                Step::new("type_text", json!({"text": "next", "in_window": window})),
            ];
            let made = parameterise(&mut steps, &no_probe);
            made.len()
        };
        for moves in [
            json!(["tab"]),
            json!(["Tab"]),
            json!(["shift", "tab"]),
            json!(["ctrl", "shift", "Tab"]),
            json!(["enter"]),
            json!(["Return"]),
            json!(["down"]),
            json!(["Up"]),
            json!(["left"]),
            json!(["right"]),
            json!(["pagedown"]),
            json!("shift+tab"),
        ] {
            assert_eq!(after(moves.clone()), 1, "{moves}");
        }
        // A chord that does not move focus leaves the target where it was.
        for stays in [json!(["ctrl", "a"]), json!(["ctrl", "c"]), json!(["shift"])] {
            assert_eq!(after(stays.clone()), 0, "{stays}");
        }
    }

    #[test]
    fn a_typing_step_after_a_tab_that_names_a_plain_control_of_its_own_is_kept() {
        let window = "app=form;window=F";
        let mut steps = vec![
            Step::new("key", json!({"keys": ["tab"], "in_window": window})),
            Step::new(
                "type_text",
                json!({"text": "kept", "in_window": window, "selector": format!("{window};role=edit;name=City")}),
            ),
            Step::new("key", json!({"keys": ["tab"], "in_window": window})),
            Step::new(
                "type_text",
                json!({"text": "not kept", "in_window": window, "selector": format!("{window};role=edit;name=Password")}),
            ),
        ];
        let made = parameterise(&mut steps, &no_probe);
        assert_eq!(made.len(), 1);
        assert_eq!((made[0].step, made[0].why), (4, Why::SecureField));
        assert_eq!(steps[1].args["text"], "kept");
        // A click names a control again, ending the doubt.
        let mut steps = vec![
            Step::new("key", json!({"keys": ["tab"], "in_window": window})),
            Step::new(
                "click_element",
                json!({"selector": format!("{window};role=edit;name=City")}),
            ),
            Step::new(
                "type_text",
                json!({"text": "city text", "in_window": window}),
            ),
        ];
        assert!(parameterise(&mut steps, &no_probe).is_empty());
    }

    #[test]
    fn the_replay_can_tell_a_step_that_puts_a_secret_on_screen() {
        let window = "app=login;window=Sign in";
        let typing = Step::new("type_text", json!({"text": "plain", "in_window": window}));
        let secret = Target::Known(format!("{window};role=edit;name=Password"));
        let plain = Target::Known(format!("{window};role=edit;name=Username"));
        assert!(
            types_a_secret(&typing, &secret),
            "typing into a password field"
        );
        assert!(
            types_a_secret(&typing, &Target::Unknown),
            "typing where focus is not known"
        );
        assert!(!types_a_secret(&typing, &plain));
        assert!(!types_a_secret(&typing, &Target::Nothing));
        // A parameter is a secret whatever is around it.
        let param = Step::new(
            "type_text",
            json!({"text": "{{text}}", "in_window": window}),
        );
        assert!(types_a_secret(&param, &plain));
        // The target follows the steps as they run.
        let mut target = Target::Nothing;
        target.after(&Step::new(
            "click_element",
            json!({"selector": format!("{window};role=edit;name=Username")}),
        ));
        assert_eq!(target, plain);
        target.after(&Step::new(
            "key",
            json!({"keys": ["tab"], "in_window": window}),
        ));
        assert_eq!(target, Target::Unknown);
        target.after(&Step::new("wait_for", json!({"condition": "element_appears", "selector": format!("{window};role=edit;name=X")})));
        assert_eq!(target, Target::Unknown, "a wait focuses nothing");
    }

    #[test]
    fn a_redacted_recording_text_is_a_parameter_not_a_replayed_placeholder() {
        let mut steps = vec![
            Step::new(
                "set_value",
                json!({"selector": "app=mail;window~=Compose;role=edit;name=Body", "value": "[REDACTED]"}),
            ),
            Step::new(
                "type_text",
                json!({"text": {"redacted": true, "sha256": "ab"}, "in_window": "app=mail"}),
            ),
            Step::new(
                "type_text",
                json!({"text": "hello", "in_window": "app=mail"}),
            ),
        ];
        let made = parameterise(&mut steps, &no_probe);
        assert_eq!(made.len(), 2);
        assert!(made.iter().all(|m| m.why == Why::Redacted));
        assert_eq!(steps[0].args["value"], "{{body}}");
        // Named after the control it went into, numbered to stay apart.
        assert_eq!(steps[1].args["text"], "{{body_2}}");
        assert_eq!(steps[2].args["text"], "hello", "ordinary text stays");
        assert_eq!(names(&steps), ["body", "body_2"]);
    }

    #[test]
    fn two_secrets_get_two_names_and_an_agents_own_placeholder_is_left_alone() {
        let mut steps = vec![
            Step::new(
                "set_value",
                json!({"selector": "app=a;window=W;role=edit;name=Password", "value": "one"}),
            ),
            Step::new(
                "set_value",
                json!({"selector": "app=a;window=W;role=edit;name=Password", "value": "two"}),
            ),
            Step::new(
                "type_text",
                json!({"text": "{{mine}}", "in_window": "app=a"}),
            ),
        ];
        let made = parameterise(&mut steps, &no_probe);
        assert_eq!(made[0].name, "password");
        assert_eq!(made[1].name, "password_2");
        assert_eq!(made.len(), 2, "{{mine}} is the agent's, not made");
        assert_eq!(names(&steps), ["password", "password_2", "mine"]);
    }
}
