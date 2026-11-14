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
//! given `params: {"name": "..."}` for every one, and refuses before step 1 when
//! one is missing. The value is substituted into the arguments of the one call
//! that types it, in memory. It is not written to the routines file or the
//! journal ([`crate::mcp`]'s `describe` shows the length only), and
//! [`Values::scrub`] takes it back out of any message a replay returns.
//!
//! # What makes a step a parameter
//!
//! 1. Its text is a stand-in for text that was removed: [`looks_redacted`], or a
//!    value that is not text at all (a recording's `{"redacted": true}`).
//! 2. It goes into a secure field, by the field's own name or automation id
//!    ([`is_secret_name`]), or because the desktop reports the control as a
//!    password field when the routine is saved. A typing step goes into the
//!    control the step before it named.
//! 3. The step already has `{{name}}` in its text: the agent made it one.
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
    /// The values `given` (the `params` argument of `routine_run`) holds for the
    /// parameters `steps` take.
    ///
    /// # Errors
    ///
    /// A parameter has no value, a value is not text or is too long, or a value
    /// is given for a name the routine does not take (a typo that would
    /// otherwise type nothing where something was meant).
    pub fn bind(steps: &[Step], given: Option<&Value>) -> Result<Values, String> {
        let needed = names(steps);
        let map = match given {
            None | Some(Value::Null) => serde_json::Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(_) => return Err("params must be an object of name to text".to_string()),
        };
        let mut values = BTreeMap::new();
        for (name, value) in &map {
            if !needed.contains(name) {
                return Err(format!(
                    "this routine takes no parameter named {name:?}{}",
                    listing(&needed)
                ));
            }
            let Some(text) = value.as_str() else {
                return Err(format!("params.{name} must be text"));
            };
            if text.chars().count() > MAX_PARAM_CHARS {
                return Err(format!(
                    "params.{name} is over {MAX_PARAM_CHARS} characters"
                ));
            }
            values.insert(name.clone(), text.to_string());
        }
        let missing: Vec<&String> = needed.iter().filter(|n| !values.contains_key(*n)).collect();
        if !missing.is_empty() {
            return Err(format!(
                "routine_run needs params for: {}. Nothing was done. These are the secrets \
                 and redacted texts the routine does not keep; pass them as \
                 params: {{\"name\": \"...\"}}",
                missing
                    .iter()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
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

    /// `text` with every value (of three characters or more) put back to its
    /// `{{name}}`, for a message that is returned or journalled.
    #[must_use]
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        let mut by_length: Vec<(&String, &String)> = self
            .0
            .iter()
            .filter(|(_, v)| v.chars().count() >= 3)
            .collect();
        by_length.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
        for (name, value) in by_length {
            out = out.replace(value.as_str(), &format!("{{{{{name}}}}}"));
        }
        out
    }
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
    // The control the step before named, for a typing step that has none of its
    // own (`type_text` goes to whatever has focus, which the last click gave it).
    let mut last_control: Option<String> = None;
    for (index, step) in steps.iter_mut().enumerate() {
        let own = step
            .args
            .get("selector")
            .and_then(Value::as_str)
            .filter(|raw| Selector::parse(raw).is_ok_and(|s| s.has_element_part()))
            .map(str::to_string);
        let control = if step.tool == "set_value" {
            own.clone()
        } else {
            last_control.clone()
        };
        if let Some(own) = own {
            last_control = Some(own);
        }
        let Some(key) = text_key(&step.tool) else {
            continue;
        };
        let value = step.args.get(key).cloned().unwrap_or(Value::Null);
        let why = match &value {
            Value::String(text) if !tokens(text).is_empty() => continue,
            Value::String(text) if looks_redacted(text) => Some(Why::Redacted),
            Value::String(_) => control
                .as_deref()
                .is_some_and(|raw| selector_is_secret(raw) || is_secret(raw))
                .then_some(Why::SecureField),
            Value::Null => None,
            // A recording's `{"redacted": true}` or the like: not text.
            _ => Some(Why::Redacted),
        };
        let Some(why) = why else { continue };
        let name = fresh_name(
            match why {
                Why::SecureField => "password",
                Why::Redacted => "text",
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
        let missing = Values::bind(&steps, None).unwrap_err();
        assert!(
            missing.contains("password") && missing.contains("Nothing was done"),
            "{missing}"
        );
        let unknown =
            Values::bind(&steps, Some(&json!({"password": "a", "pasword": "b"}))).unwrap_err();
        assert!(
            unknown.contains("pasword") && unknown.contains("password"),
            "{unknown}"
        );
        assert!(Values::bind(&steps, Some(&json!({"password": 5}))).is_err());
        assert!(Values::bind(&steps, Some(&json!("x"))).is_err());
        let long = "x".repeat(MAX_PARAM_CHARS + 1);
        assert!(Values::bind(&steps, Some(&json!({"password": long}))).is_err());
        let bound = Values::bind(&steps, Some(&json!({"password": "hunter2"}))).unwrap();
        assert!(!bound.is_empty());
        // A routine with none needs none, and says so when one is offered.
        let plain = [Step::new("key", json!({"keys": ["enter"]}))];
        assert!(Values::bind(&plain, None).unwrap().is_empty());
        let extra = Values::bind(&plain, Some(&json!({"x": "y"}))).unwrap_err();
        assert!(extra.contains("takes none"), "{extra}");
    }

    #[test]
    fn a_value_is_substituted_in_one_pass_and_scrubbed_from_what_comes_back() {
        let steps = [Step::new("type_text", json!({"text": "pw={{password}}!"}))];
        let values = Values::bind(&steps, Some(&json!({"password": "a{{password}}b"}))).unwrap();
        let filled = values.fill(&steps[0]);
        assert_eq!(filled.args["text"], "pw=a{{password}}b!");
        // The stored step is untouched.
        assert_eq!(steps[0].args["text"], "pw={{password}}!");
        let message = "could not type hunter2 into the window";
        let values = Values::bind(&steps, Some(&json!({"password": "hunter2"}))).unwrap();
        assert_eq!(
            values.scrub(message),
            "could not type {{password}} into the window"
        );
        // Two characters are not scrubbed: they would eat the message.
        let short = Values::bind(&steps, Some(&json!({"password": "ab"}))).unwrap();
        assert_eq!(short.scrub("a cab"), "a cab");
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
