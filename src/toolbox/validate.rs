//! Every call's input, checked against the schema its tool advertises,
//! before anything else about the call is decided.
//!
//! # Why here, and why first
//!
//! The schema is a promise made to the model: "call this with these fields".
//! Nothing held the model to it. A `limit` sent as a string, a `path` left
//! out, an `edits` array of the wrong shape each reached the implementation,
//! which then failed in its own words — or did not fail and did something
//! nobody asked for. Checked here, a bad call is refused with every violation
//! and the JSON path it sits at, and the model can fix all of them at once.
//!
//! The schemas are compiled ONCE per process, from the full catalogue, and
//! looked up by name. A schema that does not compile is a defect in this
//! crate: the call is refused and says so, rather than running unchecked.
//!
//! No `$ref` can fetch anything: the validator is built without its network
//! and file resolvers.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::Value;

/// The most violations listed for one call. The rest are counted.
const MAX_VIOLATIONS: usize = 20;

type Compiled = HashMap<String, Result<jsonschema::Validator, String>>;

/// Every tool's compiled input schema, keyed by name.
fn compiled() -> &'static Compiled {
    static SCHEMAS: OnceLock<Compiled> = OnceLock::new();
    SCHEMAS.get_or_init(|| {
        super::catalogue()
            .into_iter()
            .filter_map(|def| {
                let name = def.get("name")?.as_str()?.to_string();
                let schema = def.get("input_schema")?;
                let validator = jsonschema::validator_for(schema).map_err(|e| e.to_string());
                Some((name, validator))
            })
            .collect()
    })
}

/// Whether `input` is what tool `name` said it takes. `Err` lists every
/// violation with its JSON path; nothing may run then.
pub(super) fn check(name: &str, input: &Value) -> Result<(), String> {
    let Some(entry) = compiled().get(name) else {
        return Err(format!(
            "NOT RUN — `{name}` has no input schema, so its input cannot be checked"
        ));
    };
    let validator = entry
        .as_ref()
        .map_err(|why| format!("NOT RUN — the input schema of `{name}` does not compile: {why}"))?;
    let mut problems = Vec::new();
    let mut total = 0usize;
    for error in validator.iter_errors(input) {
        total += 1;
        if problems.len() < MAX_VIOLATIONS {
            let at = error.instance_path().to_string();
            let at = if at.is_empty() { "/".to_string() } else { at };
            problems.push(format!("- at `{at}`: {error}"));
        }
    }
    if total == 0 {
        return Ok(());
    }
    if total > problems.len() {
        problems.push(format!("- … {} more", total - problems.len()));
    }
    Err(format!(
        "NOT RUN — the input to {name} does not match its schema ({total} problem{}):\n{}\nFix \
         the input and call again.",
        if total == 1 { "" } else { "s" },
        problems.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// **EVERY ADVERTISED SCHEMA COMPILES.** One that did not would refuse
    /// every call to its tool.
    #[test]
    fn every_schema_in_the_catalogue_compiles() {
        let all = compiled();
        assert!(
            all.len() >= super::super::catalogue().len(),
            "{}",
            all.len()
        );
        for (name, validator) in all {
            assert!(validator.is_ok(), "{name}: {:?}", validator.as_ref().err());
        }
    }

    #[test]
    fn each_violation_is_listed_with_its_json_path() {
        let why = check(super::super::READ, &json!({"offset": "ten", "limit": -1.5}))
            .expect_err("three problems");
        assert!(why.contains("NOT RUN"), "{why}");
        assert!(why.contains("\"path\" is a required property"), "{why}");
        assert!(why.contains("at `/offset`"), "{why}");
        assert!(
            why.contains("at `/limit`: -1.5 is not of type \"integer\""),
            "{why}"
        );
        assert!(
            why.contains("at `/limit`: -1.5 is less than the minimum of 1"),
            "{why}"
        );
        assert!(why.contains("4 problems"), "{why}");
    }

    #[test]
    fn a_nested_violation_names_the_nested_path() {
        let why = check(
            super::super::MULTI_EDIT,
            &json!({"path": "f.txt", "edits": [{"old_string": "a"}]}),
        )
        .expect_err("new_string missing in edits[0]");
        assert!(why.contains("at `/edits/0`"), "{why}");
        assert!(why.contains("new_string"), "{why}");
    }

    #[test]
    fn a_valid_input_passes_and_an_unknown_name_is_refused() {
        assert!(check(super::super::READ, &json!({"path": "a.txt", "limit": 5})).is_ok());
        let why = check("Teleport", &json!({})).expect_err("no schema");
        assert!(why.contains("has no input schema"), "{why}");
    }
}
