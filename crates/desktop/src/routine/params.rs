//! Parameters: text a routine is GIVEN each time it runs instead of keeping.
//!
//! # Why
//!
//! The routines file holds what a routine types, so that the person can read it
//! and so that a replay needs no model. Text typed into a password or other
//! secure field must not be on disk at all, and neither must a recording's
//! stand-in for text it redacted (replaying `[REDACTED]` into a login box is a
//! wrong password, not a shortcut). Such a step is made a PARAMETER when the
//! routine is learned: its text is stored as `{{name}}`, and `routine_run`
//! refuses before step 1 unless every one is bound. The value is substituted into
//! the arguments of the one call that types it, in memory. It is not written to
//! the routines file or the journal ([`crate::mcp`]'s `describe` shows the length
//! only), it is hidden from the operator's hooks, and [`Values::scrub`] takes it
//! back out of any message a replay returns.
//!
//! # The PERSON binds parameters, not the agent
//!
//! `routine_run` takes no `params`. Each `{{param}}` is read from the environment
//! variable the person bound it to with `incu routine bind <phrase> <param>
//! <ENV_VAR>` at a terminal. Only the variable's NAME is stored (in the routine,
//! with a [`digest`] of its steps at the time), never a value. Had the agent
//! named the variable, it could name any `INCU_PARAM_*` variable the person set
//! for some other routine and have it typed into a window of its choosing; and a
//! literal value would land in the agent's own transcript. Saving different steps
//! drops every binding (as it drops the kept flag), and a routine whose steps are
//! not the ones that were bound is refused. Variable names must begin
//! [`ENV_PREFIX`], so a parameter can never be bound to `PATH` or an API key.
//!
//! # What makes a step a parameter
//!
//! **The rule is inverted: typed text is stored literally only when the step
//! itself names a control that is positively not secret** ([`why_not_kept`]): a
//! selector of its own with a window part, role `edit` or `combobox`, no
//! secret-looking name or automation id ([`is_secret_name`]), and not marked as a
//! password field by the desktop when the routine is saved. Everything else is a
//! parameter:
//!
//! - a typing step with no selector of its own (it types where focus is; focus
//!   moves without a key, to a sign-in dialog a click opened, a first-step sudo
//!   or KeePass prompt, the address bar by Ctrl+L, Alt+D or F6, and no list of
//!   such moves is complete);
//! - one that names a button or other control that is not a text box, or a secure
//!   one, or one that could not be checked;
//! - text a recording replaced with a placeholder or a digest
//!   ([`looks_redacted`]), or a value that is not text at all;
//! - text that already has `{{name}}` in it: the agent made it one.
//!
//! This is a screen on what the learner can see, not a promise (a `key` that
//! types a password a letter at a time is not looked at), which is why
//! `routine_save`, `routine_list` and `routine_match` show the full text of every
//! step: the person reads what is kept.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::{Routine, Step};
use crate::selector::{Selector, Text};
use crate::uia::Role;

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
    /// The values of the parameters `routine` takes, read from the environment
    /// variables the PERSON bound them to (`incu routine bind`), in this
    /// process's own environment.
    ///
    /// # Errors
    ///
    /// See [`Values::from_routine_with`].
    pub fn from_routine(routine: &Routine) -> Result<Values, String> {
        Self::from_routine_with(routine, &|var| std::env::var(var).ok())
    }

    /// [`Values::from_routine`] with `env` as the environment, so a test need not
    /// set the process's.
    ///
    /// **The agent does not choose where a value comes from.** `routine_run` takes
    /// no `params`: each `{{param}}` is read from the variable the person bound it
    /// to, so an agent cannot name some other variable of incu's environment and
    /// have it typed into a window of its choosing. The binding is stored with a
    /// digest of the routine's steps at the time ([`digest`]), and a routine whose
    /// steps are not the ones that were bound is refused.
    ///
    /// # Errors
    ///
    /// A parameter is not bound (the error names the command that binds it); the
    /// steps are not the ones that were bound; a bound variable name is not one
    /// this program accepts ([`check_env_name`], as the file may have been edited);
    /// or the variable is not set, is empty, or is too long. All before any step
    /// runs. No error contains a value.
    pub fn from_routine_with(
        routine: &Routine,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Values, String> {
        let needed = names(&routine.steps);
        if needed.len() > MAX_PARAMS {
            return Err(format!(
                "this routine takes {} parameters, more than the {MAX_PARAMS} one may; it was \
                 not made by this program",
                needed.len()
            ));
        }
        if needed.is_empty() {
            return Ok(Values::default());
        }
        let unbound: Vec<&String> = needed
            .iter()
            .filter(|n| !routine.bindings.contains_key(*n))
            .collect();
        if !unbound.is_empty() {
            return Err(format!(
                "routine_run cannot start: {} {} not bound to a value. Nothing was done. These \
                 are the secrets and redacted texts the routine does not keep, and only the \
                 PERSON binds them, at a terminal, to an environment variable of incu's own: \
                 `incu routine bind \"{}\" {} {ENV_PREFIX}{}`",
                unbound
                    .iter()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                if unbound.len() == 1 { "is" } else { "are" },
                routine.phrase,
                unbound[0],
                unbound[0].to_uppercase()
            ));
        }
        if routine.bound_steps != digest(&routine.steps) {
            return Err(format!(
                "routine_run cannot start: the steps of \"{}\" are not the ones its parameters \
                 were bound for (the file was changed since). Nothing was done. The person \
                 reads the routine again (`incu routine list`) and binds again with \
                 `incu routine bind`",
                routine.phrase
            ));
        }
        let mut values = BTreeMap::new();
        for name in &needed {
            let var = &routine.bindings[name];
            check_env_name(var).map_err(|e| format!("the binding of {name} is not usable: {e}"))?;
            let value = env(var).filter(|v| !v.is_empty()).ok_or_else(|| {
                format!(
                    "routine_run cannot start: the environment variable {var} that {name} is \
                     bound to is not set (or is empty) in incu's environment. The person sets \
                     it where incu is launched. Nothing was done"
                )
            })?;
            if value.chars().count() > MAX_PARAM_CHARS {
                return Err(format!(
                    "the value of {var} is over {MAX_PARAM_CHARS} characters"
                ));
            }
            values.insert(name.clone(), value);
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

/// Whether `var` is a name this program will read a parameter from: capitals,
/// digits and `_`, beginning [`ENV_PREFIX`] with something after it, at most 128.
///
/// The prefix is what keeps a binding to what the person set aside for routines:
/// a parameter can never be bound to `PATH`, `HOME` or an API key of the person's
/// that incu inherited, however the file came to say so.
///
/// # Errors
///
/// It is not.
pub fn check_env_name(var: &str) -> Result<(), String> {
    let well_formed = var.len() <= 128
        && var
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if well_formed && var.starts_with(ENV_PREFIX) && var.len() > ENV_PREFIX.len() {
        Ok(())
    } else {
        Err(format!(
            "the variable must be named {ENV_PREFIX}<NAME> (capitals, digits and _), so that a \
             routine can only read what the person set aside for it, never the rest of incu's \
             environment"
        ))
    }
}

/// A digest of a routine's steps: SHA-256 of their JSON, in hex. Stored with a
/// routine's bindings, so a binding is only honoured for the steps it was made
/// for.
#[must_use]
pub fn digest(steps: &[Step]) -> String {
    use sha2::{Digest, Sha256};
    let json = serde_json::to_string(steps).unwrap_or_default();
    Sha256::digest(json.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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
    /// It types into a secure field: the name or automation id of the control the
    /// step names says so, or the desktop reports it as a password field.
    SecureField,
    /// The recording kept a placeholder or a digest in place of the text.
    Redacted,
    /// Nothing positively says the control is a plain text field: the step names
    /// none of its own (it types where focus happens to be: the focus may have
    /// moved without a key, to a sign-in dialog, a prompt or the address bar), or
    /// names one that is not an edit or combo box, or one that could not be
    /// checked on the desktop when the routine was saved.
    NotKnownPlain,
}

/// What the desktop says of the control a selector names, at the moment a routine
/// is saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// It is there and is marked as a password or other secure field.
    Secret,
    /// It is there and is not so marked.
    Plain,
    /// It could not be checked: the window or control is not on screen (or no
    /// desktop is there to ask). Not the same as `Plain`.
    Unknown,
}

/// The roles a control may have for its text to be kept: a text box or a combo
/// box. (The selector vocabulary has no document role; a rich-text area is
/// `edit` or `pane`, and only `edit` is taken.)
fn is_plain_role(role: Option<Role>) -> bool {
    matches!(role, Some(Role::Edit | Role::ComboBox))
}

/// Why a typing step's text must not be kept, if it must not.
///
/// **The rule is inverted: typed text is kept only when the step itself names a
/// control that is positively not secret.** A step's text is stored literally
/// only when the step carries a selector of its own that
///
/// 1. has a window part (so it says which window),
/// 2. names a control of role `edit` or `combobox`,
/// 3. has no secret-looking name or automation id ([`is_secret_name`]), and
/// 4. is not marked as a password or secure field by the desktop at save time
///    (`probe`; a control that could not be checked does not pass).
///
/// Everything else becomes a parameter: no selector of its own, an inherited
/// target (the last click, which may have opened a dialog), a button, a secure
/// control, text a recording redacted. The earlier rule followed focus through
/// the steps and kept the rest; focus moves without a key (a click that opens a
/// sign-in dialog, a first-step prompt, Ctrl+L, Alt+D, F6), and no list of such
/// moves is complete. `None` for a step that is not typing, or already a
/// parameter.
fn why_not_kept(step: &Step, probe: &dyn Fn(&str) -> Probe) -> Option<Why> {
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
    let Some(raw) = control_of_any(step) else {
        return Some(Why::NotKnownPlain);
    };
    let Ok(selector) = Selector::parse(&raw) else {
        return Some(Why::NotKnownPlain);
    };
    if !selector.has_window_part() {
        return Some(Why::NotKnownPlain);
    }
    if selector_is_secret(&raw) {
        return Some(Why::SecureField);
    }
    if !is_plain_role(selector.role) {
        return Some(Why::NotKnownPlain);
    }
    match probe(&raw) {
        Probe::Plain => None,
        Probe::Secret => Some(Why::SecureField),
        Probe::Unknown => Some(Why::NotKnownPlain),
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

/// Whether `step` puts a secret on screen, as far as the replay can tell without
/// asking the desktop: it types a parameter, or text into a control whose NAME or
/// automation id says it is secret. Used to keep the replay from photographing it.
#[must_use]
pub fn types_a_secret(step: &Step) -> bool {
    !names_of(step).is_empty()
        || (text_key(&step.tool).is_some()
            && control_of_any(step).is_some_and(|raw| selector_is_secret(&raw)))
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

/// Make every typing step whose text is not positively safe to keep ([`why_not_kept`])
/// a parameter, in place, and say which. `probe` is asked about the raw selector
/// of a control a step names: the desktop's own report, at save time, of whether
/// it is a password field.
pub fn parameterise(steps: &mut [Step], probe: &dyn Fn(&str) -> Probe) -> Vec<Made> {
    let mut made = Vec::new();
    let mut taken: BTreeSet<String> = names(steps).into_iter().collect();
    for (index, step) in steps.iter_mut().enumerate() {
        let (Some(why), Some(key)) = (why_not_kept(step, probe), text_key(&step.tool)) else {
            continue;
        };
        // Named after the control the step names, when it names one.
        let control = control_of_any(step);
        let name = fresh_name(
            match why {
                Why::SecureField => "password",
                Why::Redacted | Why::NotKnownPlain => "text",
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

    /// A desktop with nothing on it: no control can be checked.
    fn no_probe(_: &str) -> Probe {
        Probe::Unknown
    }

    /// A desktop that says every control it is asked about is a plain text box.
    fn plain_probe(_: &str) -> Probe {
        Probe::Plain
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

    /// A routine that types `{{password}}`, with the bindings the person made.
    fn routine_with(steps: Vec<Step>, bind: &[(&str, &str)]) -> Routine {
        let mut routine = Routine {
            phrase: "unlock the vault".to_string(),
            steps,
            date: String::new(),
            last_used_ms: 0,
            taught: false,
            bindings: BTreeMap::new(),
            bound_steps: String::new(),
        };
        for (param, var) in bind {
            routine
                .bindings
                .insert((*param).to_string(), (*var).to_string());
        }
        if !bind.is_empty() {
            routine.bound_steps = digest(&routine.steps);
        }
        routine
    }

    fn typing_password() -> Vec<Step> {
        vec![Step::new("type_text", json!({"text": "{{password}}"}))]
    }

    /// The environment of a test: this variable and no other.
    fn secret_env(var: &str) -> Option<String> {
        (var == "INCU_PARAM_PASSWORD").then(|| "hunter2-secret".to_string())
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// A value, for the tests of what is done with one.
    fn values(pairs: &[(&str, &str)]) -> Values {
        Values(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }

    #[test]
    fn a_run_needs_every_parameter_bound_by_the_person_for_these_steps() {
        let bound = routine_with(typing_password(), &[("password", "INCU_PARAM_PASSWORD")]);
        // Bound, for these steps, and the variable is set: read from it.
        let got = Values::from_routine_with(&bound, &secret_env).unwrap();
        assert!(!got.is_empty());
        assert_eq!(got.fill(&bound.steps[0]).args["text"], "hunter2-secret");
        // Not bound: refused, naming the command and never a value.
        let unbound = routine_with(typing_password(), &[]);
        let why = Values::from_routine_with(&unbound, &secret_env).unwrap_err();
        assert!(
            why.contains("not bound") && why.contains("Nothing was done"),
            "{why}"
        );
        assert!(
            why.contains("incu routine bind \"unlock the vault\" password INCU_PARAM_PASSWORD"),
            "{why}"
        );
        assert!(!why.contains("hunter2"), "{why}");
        // Some bound, some not: the unbound ones are named.
        let two = vec![Step::new(
            "type_text",
            json!({"text": "{{user}} {{password}}"}),
        )];
        let half = routine_with(two, &[("password", "INCU_PARAM_PASSWORD")]);
        let why = Values::from_routine_with(&half, &secret_env).unwrap_err();
        assert!(why.contains("user") && !why.contains("password,"), "{why}");
        // The variable is not set, or empty, or too long: said by name.
        let why = Values::from_routine_with(&bound, &no_env).unwrap_err();
        assert!(
            why.contains("INCU_PARAM_PASSWORD") && why.contains("not set"),
            "{why}"
        );
        let empty = |_: &str| Some(String::new());
        assert!(Values::from_routine_with(&bound, &empty).is_err());
        let long = |_: &str| Some("x".repeat(MAX_PARAM_CHARS + 1));
        assert!(Values::from_routine_with(&bound, &long).is_err());
        // A routine with none needs none.
        let plain = routine_with(vec![Step::new("key", json!({"keys": ["enter"]}))], &[]);
        assert!(Values::from_routine_with(&plain, &no_env)
            .unwrap()
            .is_empty());
    }

    /// The steps changed since the person bound: a binding is for steps it was
    /// made for, and is not honoured for others (a hand-edited file).
    #[test]
    fn a_routine_whose_steps_changed_since_they_were_bound_is_refused() {
        let mut routine = routine_with(typing_password(), &[("password", "INCU_PARAM_PASSWORD")]);
        assert!(Values::from_routine_with(&routine, &secret_env).is_ok());
        routine
            .steps
            .push(Step::new("key", json!({"keys": ["enter"]})));
        let why = Values::from_routine_with(&routine, &secret_env).unwrap_err();
        assert!(
            why.contains("not the ones its parameters were bound for"),
            "{why}"
        );
        assert!(why.contains("incu routine bind"), "{why}");
        // The digest is of the steps and nothing else: same steps, same digest.
        assert_eq!(digest(&typing_password()), digest(&typing_password()));
        assert_ne!(digest(&typing_password()), digest(&routine.steps));
        assert_eq!(digest(&typing_password()).len(), 64);
    }

    /// Only a variable the person set aside for routines can be bound: the prefix
    /// is the line, in the command and again when the file is read.
    #[test]
    fn a_binding_can_name_only_an_incu_param_variable() {
        for good in [
            "INCU_PARAM_PASSWORD",
            "INCU_PARAM_A",
            "INCU_PARAM_VAULT_PIN_2",
        ] {
            assert!(check_env_name(good).is_ok(), "{good}");
        }
        for bad in [
            "HOME",
            "PATH",
            "ANTHROPIC_API_KEY",
            "INCU_PARAM_",
            "incu_param_password",
            "INCU_PARAM_A B",
            "INCU_PARAM_A=B",
            "XINCU_PARAM_A",
            "",
        ] {
            assert!(check_env_name(bad).is_err(), "{bad:?}");
        }
        // A file edited to bind a parameter to something else is refused at run
        // time, before the variable is read.
        let mut edited = routine_with(typing_password(), &[("password", "HOME")]);
        edited.bound_steps = digest(&edited.steps);
        let read = std::cell::Cell::new(false);
        let spy = |_: &str| {
            read.set(true);
            Some("leaked".to_string())
        };
        let why = Values::from_routine_with(&edited, &spy).unwrap_err();
        assert!(
            why.contains("INCU_PARAM_") && !why.contains("leaked"),
            "{why}"
        );
        assert!(!read.get(), "the variable was read");
    }

    #[test]
    fn a_routine_may_take_no_more_than_the_most_parameters() {
        let steps: Vec<Step> = (0..=MAX_PARAMS)
            .map(|i| Step::new("type_text", json!({"text": format!("{{{{p{i}}}}}")})))
            .collect();
        assert_eq!(names(&steps).len(), MAX_PARAMS + 1);
        let bound: Vec<(String, String)> = (0..=MAX_PARAMS)
            .map(|i| (format!("p{i}"), format!("INCU_PARAM_P{i}")))
            .collect();
        let pairs: Vec<(&str, &str)> = bound
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let routine = routine_with(steps, &pairs);
        let why = Values::from_routine_with(&routine, &|_| Some("x".to_string())).unwrap_err();
        assert!(why.contains("more than the 16"), "{why}");
    }

    #[test]
    fn a_value_is_substituted_in_one_pass_and_scrubbed_from_what_comes_back() {
        let step = Step::new("type_text", json!({"text": "pw={{password}}!"}));
        let tricky = values(&[("password", "a{{password}}b")]);
        let filled = tricky.fill(&step);
        assert_eq!(filled.args["text"], "pw=a{{password}}b!");
        // The stored step is untouched.
        assert_eq!(step.args["text"], "pw={{password}}!");
        let message = "could not type hunter2-secret into the window";
        assert_eq!(
            values(&[("password", "hunter2-secret")]).scrub(message),
            "could not type {{password}} into the window"
        );
    }

    /// A value of any length is scrubbed; a short one only where it stands whole.
    #[test]
    fn a_short_value_is_scrubbed_whole_and_never_out_of_the_middle_of_a_word() {
        let with = |value: &'static str| values(&[("password", value)]);
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
        let both = values(&[("a", "secret"), ("b", "secret-longer")]);
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

    /// A typing step that names a plain text box of its own, in a window, that
    /// the desktop confirms is not a password field: the only text that is kept.
    fn typing_in(selector: &str, text: &str) -> Step {
        Step::new(
            "type_text",
            json!({"text": text, "in_window": "app=browser;window~=Home", "selector": selector}),
        )
    }

    const SEARCH_BOX: &str = "app=browser;window~=Home;role=edit;name=Search";

    /// The inverted rule: a plain named search box keeps its literal text.
    #[test]
    fn a_plain_named_search_box_keeps_its_literal_text() {
        let mut steps = vec![
            typing_in(SEARCH_BOX, "rust lifetimes"),
            Step::new(
                "set_value",
                json!({"selector": "app=browser;window~=Home;role=combobox;name=Language", "value": "English"}),
            ),
        ];
        let made = parameterise(&mut steps, &plain_probe);
        assert!(made.is_empty(), "{made:?}");
        assert_eq!(steps[0].args["text"], "rust lifetimes");
        assert_eq!(steps[1].args["value"], "English");
        assert!(names(&steps).is_empty());
    }

    /// Everything that does not positively say "plain text box" is a parameter.
    #[test]
    fn a_text_is_kept_only_for_a_control_positively_known_to_be_a_plain_text_box() {
        let in_window = "app=browser;window~=Home";
        type Case<'a> = (&'a str, Value, &'a dyn Fn(&str) -> Probe, Why);
        let cases: &[Case] = &[
            // The desktop could not check it (the window is gone, or no desktop).
            (
                "unchecked",
                typing_in(SEARCH_BOX, "x").args,
                &no_probe,
                Why::NotKnownPlain,
            ),
            // The desktop says it is a password field, whatever its name.
            (
                "marked secure",
                typing_in(SEARCH_BOX, "x").args,
                &|_| Probe::Secret,
                Why::SecureField,
            ),
            // A name that says secret.
            (
                "secret name",
                typing_in(&format!("{in_window};role=edit;name=Password"), "x").args,
                &plain_probe,
                Why::SecureField,
            ),
            (
                "secret automation id",
                typing_in(&format!("{in_window};role=edit;aid=txtPassword"), "x").args,
                &plain_probe,
                Why::SecureField,
            ),
            // Not a text box.
            (
                "a button",
                typing_in(&format!("{in_window};role=button;name=Sign in"), "x").args,
                &plain_probe,
                Why::NotKnownPlain,
            ),
            (
                "a text label",
                typing_in(&format!("{in_window};role=text;name=Hello"), "x").args,
                &plain_probe,
                Why::NotKnownPlain,
            ),
            (
                "no role",
                typing_in(&format!("{in_window};name=Search"), "x").args,
                &plain_probe,
                Why::NotKnownPlain,
            ),
            // Does not say which window.
            (
                "no window part",
                typing_in("role=edit;name=Search", "x").args,
                &plain_probe,
                Why::NotKnownPlain,
            ),
            // No selector of its own at all: it types where focus is.
            (
                "no selector",
                json!({"text": "x", "in_window": in_window}),
                &plain_probe,
                Why::NotKnownPlain,
            ),
        ];
        for (what, args, probe, why) in cases {
            let mut steps = vec![Step::new("type_text", args.clone())];
            let made = parameterise(&mut steps, probe);
            assert_eq!(made.len(), 1, "{what}: kept as text");
            assert_eq!(made[0].why, *why, "{what}");
            assert!(
                steps[0].args["text"].as_str().unwrap().starts_with("{{"),
                "{what}"
            );
        }
    }

    /// A click that opens a sign-in dialog: the password goes into a field no
    /// step named, and the focus moved without any key.
    #[test]
    fn a_password_typed_after_a_click_that_opened_a_sign_in_dialog_is_a_parameter() {
        let mut steps = vec![
            Step::new(
                "click_element",
                json!({"selector": "app=editor;window=Doc;role=button;name=Sign in"}),
            ),
            Step::new(
                "type_text",
                json!({"text": "hunter2", "in_window": "app=editor;window~=Sign in"}),
            ),
        ];
        // Even a desktop that calls every control plain does not save it: the
        // step does not name the field.
        let made = parameterise(&mut steps, &plain_probe);
        assert_eq!(made.len(), 1);
        assert!(!serde_json::to_string(&steps).unwrap().contains("hunter2"));
    }

    /// A first-step prompt: nothing has named a control yet (sudo, KeePass).
    #[test]
    fn a_secret_typed_as_the_first_step_is_a_parameter() {
        let mut steps = vec![
            Step::new(
                "type_text",
                json!({"text": "correct horse", "in_window": "app=terminal;window~=sudo"}),
            ),
            Step::new(
                "key",
                json!({"keys": ["enter"], "in_window": "app=terminal"}),
            ),
        ];
        let made = parameterise(&mut steps, &plain_probe);
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].step, 1);
        assert!(!serde_json::to_string(&steps).unwrap().contains("horse"));
    }

    /// Focus moves with no key at all, or with one no list of keys would hold:
    /// Ctrl+L, Alt+D, F6 put it in the address bar.
    #[test]
    fn text_typed_after_ctrl_l_or_alt_d_or_f6_is_a_parameter() {
        let window = "app=browser;window~=Home";
        for keys in [json!(["ctrl", "l"]), json!(["alt", "d"]), json!(["f6"])] {
            let mut steps = vec![
                Step::new(
                    "click_element",
                    json!({"selector": format!("{window};role=edit;name=Search")}),
                ),
                Step::new("key", json!({"keys": keys, "in_window": window})),
                Step::new(
                    "type_text",
                    json!({"text": "secret-url", "in_window": window}),
                ),
            ];
            let made = parameterise(&mut steps, &plain_probe);
            assert_eq!(made.len(), 1, "{keys}");
            assert_eq!(made[0].step, 3);
        }
    }

    /// The login form, now: every text is a parameter, the user name too, because
    /// no step names a field.
    #[test]
    fn the_login_form_with_a_tab_keeps_no_text() {
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
        let made = parameterise(&mut steps, &plain_probe);
        assert_eq!(made.len(), 2, "{made:?}");
        let kept = serde_json::to_string(&steps).unwrap();
        assert!(
            !kept.contains("hunter2") && !kept.contains("alice"),
            "{kept}"
        );
        // The two get two names.
        assert_eq!(names(&steps), ["text", "text_2"]);
    }

    #[test]
    fn typing_into_a_password_field_becomes_a_parameter_named_after_it() {
        let mut steps = vec![
            typing_in(
                "app=login;window~=Home;role=edit;name=Email",
                "me@example.com",
            ),
            typing_in(
                "app=login;window~=Home;role=edit;name=Password",
                "hunter2-secret",
            ),
        ];
        let made = parameterise(&mut steps, &plain_probe);
        assert_eq!(
            made,
            [Made {
                name: "password".to_string(),
                step: 2,
                why: Why::SecureField
            }]
        );
        assert_eq!(steps[1].args["text"], "{{password}}");
        // The email goes into a named plain field: it stays.
        assert_eq!(steps[0].args["text"], "me@example.com");
        assert!(!serde_json::to_string(&steps).unwrap().contains("hunter2"));
    }

    #[test]
    fn a_set_value_follows_the_same_rule_through_its_own_selector() {
        let mut steps = vec![
            Step::new(
                "set_value",
                json!({"selector": "app=vault;window=V;role=edit;name=Token", "value": "hunter2-secret"}),
            ),
            // No window part: not kept.
            Step::new(
                "set_value",
                json!({"selector": "role=edit;name=Title", "value": "a title"}),
            ),
            Step::new(
                "set_value",
                json!({"selector": "app=vault;window=V;role=edit;name=Title", "value": "a title"}),
            ),
        ];
        let made = parameterise(&mut steps, &plain_probe);
        assert_eq!(made[0].name, "token");
        assert_eq!(steps[0].args["value"], "{{token}}");
        assert_eq!(made.len(), 2, "{made:?}");
        assert_eq!(made[1].step, 2);
        assert_eq!(steps[2].args["value"], "a title");
    }

    #[test]
    fn the_desktops_own_report_of_a_password_field_counts_when_the_name_does_not_say() {
        let mut steps = vec![Step::new(
            "set_value",
            json!({"selector": "app=bank;window=Login;role=edit;name=Enter it here", "value": "s3cret"}),
        )];
        let seen_as_password = |raw: &str| {
            if raw.contains("Enter it here") {
                Probe::Secret
            } else {
                Probe::Plain
            }
        };
        let made = parameterise(&mut steps, &seen_as_password);
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].why, Why::SecureField);
        assert!(!steps[0].args.to_string().contains("s3cret"));
    }

    #[test]
    fn the_replay_can_tell_a_step_that_puts_a_secret_on_screen() {
        let window = "app=login;window=Sign in";
        // A parameter is a secret, whatever is around it.
        let param = Step::new(
            "type_text",
            json!({"text": "{{text}}", "in_window": window}),
        );
        assert!(types_a_secret(&param));
        // Text into a field whose name says secret.
        assert!(types_a_secret(&typing_in(
            &format!("{window};role=edit;name=Password"),
            "plain"
        )));
        // Plain text into a plain field, and steps that type nothing.
        assert!(!types_a_secret(&typing_in(SEARCH_BOX, "plain")));
        assert!(!types_a_secret(&Step::new("key", json!({"keys": ["tab"]}))));
        assert!(!types_a_secret(&Step::new(
            "click_element",
            json!({"selector": format!("{window};role=edit;name=Password")})
        )));
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
            typing_in("app=mail;window~=Compose;role=edit;name=Subject", "hello"),
        ];
        let made = parameterise(&mut steps, &plain_probe);
        assert_eq!(made.len(), 2);
        assert!(made.iter().all(|m| m.why == Why::Redacted));
        assert_eq!(steps[0].args["value"], "{{body}}");
        // No control of its own: named plainly, numbered to stay apart.
        assert_eq!(steps[1].args["text"], "{{text}}");
        assert_eq!(
            steps[2].args["text"], "hello",
            "ordinary text in a named plain field stays"
        );
        assert_eq!(names(&steps), ["body", "text"]);
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
        let made = parameterise(&mut steps, &plain_probe);
        assert_eq!(made[0].name, "password");
        assert_eq!(made[1].name, "password_2");
        assert_eq!(made.len(), 2, "{{mine}} is the agent's, not made");
        assert_eq!(names(&steps), ["password", "password_2", "mine"]);
    }
}
