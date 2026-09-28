//! The interfaces, frozen before the work is split up.
//!
//! # The failure this exists to stop
//!
//! Module A returns `{user_id}`. Module B expects `{userId}`. Neither task is
//! individually wrong, both pass their own checks, and nothing notices until
//! integration — by which time the fix touches both, and whichever agent wrote
//! the second one has long since finished and been paid for.
//!
//! It is a *multi-agent* failure specifically. One model holding the whole job
//! in its head tends to stay consistent by accident. Four models working from
//! separate task descriptions have nothing to be consistent *with* unless the
//! shapes are written down first.
//!
//! # Why this is checked and not merely documented
//!
//! A frozen interface nobody validates is a comment with a schema. The file
//! earns its place only if a mismatch fails the same exit code everything else
//! is judged by — so [`check`] runs as part of [`crate::verify`], and a task
//! that breaks a contract cannot reach `Done` no matter how green its own tests
//! are.
//!
//! # What is checked, and what is deliberately not
//!
//! Checked: every declared name appears in the code, and every field of every
//! schema appears alongside it. That catches the renamed field, the dropped
//! field, and the endpoint nobody implemented — the mismatches that cost the
//! most and are found the latest.
//!
//! Not checked: types, nullability, semantics. Verifying those means parsing
//! every language the harness might build in, and a checker that is wrong about
//! types is worse than one that says plainly what it covers. The type story
//! belongs to the project's own compiler and its own tests, which already run.

use std::collections::BTreeMap;
use std::path::Path;

/// Where the frozen interfaces live, under the workspace.
pub const CONTRACTS_FILE: &str = ".incode/contracts.json";

/// How many files to read before giving up on a search.
///
/// Bounded because this runs inside `verify`, on every task. A repository large
/// enough to exceed it is one where the check needs a real index, and silently
/// scanning forever would turn every completion into a minute of disk I/O.
const MAX_FILES_SCANNED: usize = 5_000;

/// Extensions worth searching. Everything else is data, lockfiles or artefacts.
pub const SOURCE_EXTENSIONS: [&str; 14] = [
    "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "kt", "rb", "php", "cs", "swift", "sql",
];

/// One named shape and its fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Schema {
    /// Field name → its declared type, kept as free text.
    ///
    /// The type is recorded for the humans and the agents that read this file;
    /// the checker deliberately does not interpret it. See the module header.
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

/// Read a schema's fields, in either shape a writer might use.
///
/// **Measured against a real generated file, not imagined.** Asked to write
/// contracts without being shown the shape, an agent produced
/// `fields: [{wire, column, type, required}]` — a list of objects, richer than
/// the map this crate writes and perfectly reasonable. The map-only parser read
/// that as *zero fields*, so 24 schemas checked nothing and the run reported
/// success. A checker that silently verifies nothing is worse than no checker,
/// because it is believed.
///
/// So both shapes are accepted, and anything else is an ERROR rather than an
/// empty set — see [`parse_contracts`].
fn read_fields(value: &serde_json::Value) -> Result<BTreeMap<String, String>, String> {
    let Some(fields) = value.get("fields") else {
        return Ok(BTreeMap::new());
    };
    if let Some(map) = fields.as_object() {
        return Ok(map
            .iter()
            .map(|(name, ty)| {
                let ty = ty.as_str().map_or_else(|| ty.to_string(), str::to_string);
                (name.clone(), ty)
            })
            .collect());
    }
    if let Some(list) = fields.as_array() {
        let mut out = BTreeMap::new();
        for field in list {
            // A plain string is a bare field name; an object names itself with
            // `wire` (the name that crosses the boundary), `name`, or `field`.
            let name = if let Some(name) = field.as_str() {
                name.to_string()
            } else if let Some(named) = ["wire", "name", "field"]
                .iter()
                .find_map(|key| field.get(*key).and_then(|v| v.as_str()))
            {
                named.to_string()
            } else if field.get("wire").is_some_and(serde_json::Value::is_null) {
                // `wire: null` beside a `column` is a real statement, not a gap:
                // the field exists in storage and never crosses the boundary.
                // Contracts are about what crosses, so there is nothing here for
                // another module to disagree with — skipped, not failed.
                continue;
            } else {
                return Err(format!(
                    "a field in this schema names itself in none of the ways this \
                     understands (`wire`, `name`, `field`), and does not say it is \
                     absent from the wire either: {field}"
                ));
            };
            let ty = field
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string();
            out.insert(name, ty);
        }
        return Ok(out);
    }
    Err(format!(
        "`fields` is neither an object nor a list, so nothing can be checked \
         against it: {fields}"
    ))
}

/// One endpoint across a module boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Endpoint {
    /// e.g. `POST /api/users`. Matched against the source as written.
    #[serde(default)]
    pub path: String,
    /// The schema name this returns, if it returns one.
    #[serde(default)]
    pub response: String,
    /// Error cases callers must handle, as free text.
    #[serde(default)]
    pub errors: Vec<String>,
}

/// The frozen interfaces for one project.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Contracts {
    #[serde(default)]
    pub schemas: BTreeMap<String, Schema>,
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
}

/// One thing the code and the contract disagree about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breach {
    /// What is missing, in the contract's own words.
    pub what: String,
    /// Why it matters, in words that say what to do.
    pub why: String,
}

impl Contracts {
    /// Read the project's contracts, or an empty set when there are none.
    ///
    /// A malformed file is an ERROR. Contracts that quietly stop applying are
    /// the failure this module exists to prevent, arriving one level up.
    pub fn load(workspace: &Path) -> Result<Self, String> {
        let path = workspace.join(CONTRACTS_FILE);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        parse_contracts(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn save(&self, workspace: &Path) -> Result<(), String> {
        let path = workspace.join(CONTRACTS_FILE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        let text =
            serde_json::to_string_pretty(self).map_err(|e| format!("could not encode: {e}"))?;
        std::fs::write(&path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.schemas.is_empty() && self.endpoints.is_empty()
    }

    /// Every name the code must contain for this contract to hold.
    ///
    /// A field is only required where its schema is: looking for `email`
    /// anywhere in the tree would pass on any project that happens to mention
    /// it, which is every project.
    #[must_use]
    pub fn required_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for name in self.schemas.keys() {
            names.push(name.clone());
        }
        for endpoint in &self.endpoints {
            if !endpoint.path.trim().is_empty() {
                names.push(endpoint.path.clone());
            }
        }
        names
    }
}

/// Read a contracts file, in any shape this understands.
///
/// **A shape it does not understand is an ERROR, never an empty set.** That is
/// the whole lesson of this function: a contracts file the parser cannot read
/// silently checks nothing, and a check that verifies nothing while reporting
/// success is worse than having no check at all.
pub fn parse_contracts(text: &str) -> Result<Contracts, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;

    let mut schemas = BTreeMap::new();
    if let Some(declared) = value.get("schemas") {
        let object = declared
            .as_object()
            .ok_or_else(|| "`schemas` must be an object keyed by shape name".to_string())?;
        for (name, schema) in object {
            let fields = read_fields(schema).map_err(|e| format!("schema `{name}`: {e}"))?;
            schemas.insert(name.clone(), Schema { fields });
        }
    }

    // Endpoints arrive either as a bare list, or wrapped in an object with the
    // list under `routes` — the second is what a generator produced when left to
    // choose, and it reads better for a person. Both are accepted; anything else
    // is refused rather than read as "no endpoints".
    let mut endpoints = Vec::new();
    if let Some(declared) = value.get("endpoints") {
        let list = if let Some(list) = declared.as_array() {
            list.clone()
        } else if let Some(routes) = declared.get("routes").and_then(|r| r.as_array()) {
            routes.clone()
        } else {
            return Err(
                "`endpoints` must be a list, or an object with a `routes` list inside it"
                    .to_string(),
            );
        };
        for route in list {
            // The path may be one field, or split into method and path.
            let path = route
                .get("path")
                .and_then(|p| p.as_str())
                .map(str::to_string)
                .or_else(|| {
                    let method = route.get("method").and_then(|m| m.as_str())?;
                    let route_path = route.get("route").and_then(|r| r.as_str())?;
                    Some(format!("{method} {route_path}"))
                })
                .unwrap_or_default();
            endpoints.push(Endpoint {
                path,
                response: route
                    .get("response")
                    .and_then(|r| r.as_str())
                    .unwrap_or_default()
                    .to_string(),
                errors: route
                    .get("errors")
                    .and_then(|e| e.as_array())
                    .map(|list| {
                        list.iter()
                            .map(|e| e.as_str().map_or_else(|| e.to_string(), str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            });
        }
    }

    Ok(Contracts { schemas, endpoints })
}

/// Hold the code against the contract.
///
/// Returns every breach rather than the first: an agent given one mismatch fixes
/// one mismatch and comes back, and each round costs a full task attempt.
pub fn check(workspace: &Path, contracts: &Contracts) -> Result<Vec<Breach>, String> {
    if contracts.is_empty() {
        return Ok(Vec::new());
    }
    let sources = read_sources(workspace)?;
    let mut breaches = Vec::new();

    for (name, schema) in &contracts.schemas {
        let Some(hits) = files_naming(&sources, name) else {
            breaches.push(Breach {
                what: format!("schema `{name}`"),
                why: format!(
                    "the contract declares `{name}` and no source file mentions it. \
                     Either it has not been built yet, or it was built under a \
                     different name — which is the mismatch that costs the most, \
                     because both sides pass their own tests."
                ),
            });
            continue;
        };
        // Fields are searched only in the files that name the schema, so a
        // field is credited to the shape it belongs to and not to a coincidence
        // somewhere else in the tree.
        let context = hits.join("\n");
        for field in schema.fields.keys() {
            if !mentions(&context, field) {
                breaches.push(Breach {
                    what: format!("field `{name}.{field}`"),
                    why: format!(
                        "`{name}` exists but nothing near it mentions `{field}`. A \
                         renamed or dropped field is invisible to the module that \
                         reads it until integration."
                    ),
                });
            }
        }
    }

    for endpoint in &contracts.endpoints {
        let path = endpoint.path.trim();
        if path.is_empty() {
            continue;
        }
        // The route as written, and the route without its method — a source file
        // rarely contains the string `POST /api/users` verbatim.
        let route = path.split_whitespace().last().unwrap_or(path);
        if files_naming(&sources, route).is_none() {
            breaches.push(Breach {
                what: format!("endpoint `{path}`"),
                why: format!(
                    "the contract declares `{path}` and no source file mentions \
                     `{route}`. A caller written against this contract will get a \
                     404 from a service that believes it is finished."
                ),
            });
        }
    }

    Ok(breaches)
}

/// The contents of every file that names `needle`, or `None` when none do.
fn files_naming<'a>(sources: &'a [(String, String)], needle: &str) -> Option<Vec<&'a str>> {
    let hits: Vec<&str> = sources
        .iter()
        .filter(|(_, text)| mentions(text, needle))
        .map(|(_, text)| text.as_str())
        .collect();
    (!hits.is_empty()).then_some(hits)
}

/// Checks whether `needle_segments` matches a route pattern with parameters in `text_lower`.
fn matches_param_route(text_lower: &str, needle_segments: &[&str]) -> bool {
    for quote in ['"', '\'', '`'] {
        let mut rest = text_lower;
        while let Some(open) = rest.find(quote) {
            let after = &rest[open + 1..];
            let Some(close) = after.find(quote) else {
                break;
            };
            let candidate = after[..close].trim().trim_matches('/');
            let cand_segments: Vec<&str> = candidate.split('/').collect();
            if cand_segments.len() == needle_segments.len() {
                let all_match = needle_segments
                    .iter()
                    .zip(cand_segments.iter())
                    .all(|(n, c)| {
                        let n_is_param =
                            n.starts_with('{') || n.starts_with(':') || n.starts_with('<');
                        let c_is_param =
                            c.starts_with('{') || c.starts_with(':') || c.starts_with('<');
                        if n_is_param || c_is_param {
                            true
                        } else {
                            n.eq_ignore_ascii_case(c)
                        }
                    });
                if all_match {
                    return true;
                }
            }
            rest = &after[close + 1..];
        }
    }
    false
}

/// Matches an endpoint route needle against text with boundary awareness.
fn matches_route(text: &str, needle: &str) -> bool {
    let route = needle.trim().trim_start_matches('/').trim_end_matches('/');
    if route.is_empty() {
        return false;
    }
    let needle_lower = route.to_ascii_lowercase();
    let text_lower = text.to_ascii_lowercase();

    let mut from = 0;
    while let Some(idx) = text_lower[from..].find(&needle_lower) {
        let start = from + idx;
        let end = start + needle_lower.len();

        let before_ok = if start == 0 {
            true
        } else {
            let prev = text[..start].chars().last().unwrap();
            !prev.is_alphanumeric() && prev != '_' && prev != '-'
        };

        let after_ok = if end >= text.len() {
            true
        } else {
            let next = text[end..].chars().next().unwrap();
            !next.is_alphanumeric() && next != '_' && next != '-'
        };

        if before_ok && after_ok {
            return true;
        }

        from = start + 1;
        if from >= text_lower.len() {
            break;
        }
    }

    let needle_segments: Vec<&str> = route.split('/').collect();
    if needle_segments
        .iter()
        .any(|s| s.starts_with('{') || s.starts_with(':') || s.starts_with('<'))
        && matches_param_route(&text_lower, &needle_segments)
    {
        return true;
    }

    false
}

/// Whether a body of text names something.
///
/// Case-insensitive and punctuation-tolerant across house styles (`user_id`,
/// `userId`, `UserId`, `user-id`), but enforces token boundaries so short field
/// names like `id` or `key` do not match substrings inside unrelated words
/// like `valid`, `guid`, or `keyboard`.
#[must_use]
pub fn mentions(text: &str, needle: &str) -> bool {
    if needle.contains('/') {
        return matches_route(text, needle);
    }

    let needle_flat: String = needle
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    if needle_flat.is_empty() {
        return false;
    }

    // Split source code into identifier tokens delimited by non-identifier punctuation
    for token in text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-') {
        let trimmed = token.trim_matches(['_', '-']);
        if trimmed.is_empty() {
            continue;
        }
        let token_flat: String = trimmed
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        if token_flat == needle_flat {
            return true;
        }
    }
    false
}

/// Every source file under the workspace, bounded.
fn read_sources(workspace: &Path) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut stack = vec![workspace.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let name_lower = name.to_ascii_lowercase();
            // Build output, virtualenvs, dependencies, and VCS dwarf the source
            // and contain copies of it, which would make a deleted name look present.
            if name.starts_with('.')
                || matches!(
                    name_lower.as_str(),
                    "target"
                        | "node_modules"
                        | "dist"
                        | "build"
                        | "out"
                        | "vendor"
                        | "venv"
                        | "env"
                        | "__pycache__"
                )
            {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let is_source = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| SOURCE_EXTENSIONS.contains(&e));
            if !is_source {
                continue;
            }
            if out.len() >= MAX_FILES_SCANNED {
                // Stated, not silent: a truncated scan that reported "no
                // breaches" would be a pass nobody earned.
                return Err(format!(
                    "more than {MAX_FILES_SCANNED} source files: the contract check \
                     stopped rather than report a result it did not finish computing"
                ));
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                out.push((path.display().to_string(), text));
            }
        }
    }
    Ok(out)
}

/// The breaches as one report for a person or a model to act on.
#[must_use]
pub fn report(breaches: &[Breach]) -> String {
    if breaches.is_empty() {
        return "Every declared contract is present in the code.".to_string();
    }
    let mut out = format!(
        "{} contract breach(es). These are interfaces that were frozen before the \
         work was split up, so a mismatch here means two pieces were built against \
         different shapes:\n",
        breaches.len()
    );
    for breach in breaches {
        out.push_str(&format!("\n- MISSING {} — {}\n", breach.what, breach.why));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        for (path, body) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, body).expect("write");
        }
        dir
    }

    fn user_contract() -> Contracts {
        let mut fields = BTreeMap::new();
        fields.insert("id".to_string(), "uuid".to_string());
        fields.insert("email".to_string(), "string".to_string());
        fields.insert("created_at".to_string(), "timestamp".to_string());
        let mut schemas = BTreeMap::new();
        schemas.insert("User".to_string(), Schema { fields });
        Contracts {
            schemas,
            endpoints: vec![Endpoint {
                path: "POST /api/users".to_string(),
                response: "User".to_string(),
                errors: vec!["409 email_taken".to_string()],
            }],
        }
    }

    /// **The exact failure this file exists for.** Module A writes `user_id`,
    /// module B expects `userId` — and both are the same field under two house
    /// styles, so the check must NOT fire on that.
    #[test]
    fn the_same_field_in_two_house_styles_is_the_same_field() {
        assert!(mentions("struct User { user_id: Uuid }", "userId"));
        assert!(mentions("interface User { userId: string }", "user_id"));
        assert!(mentions("class User { UserId }", "user_id"));
        // And a genuinely different name is still different.
        assert!(!mentions("struct User { account: Uuid }", "user_id"));
    }

    /// A field the contract declares and the code never mentions is the breach
    /// that survives to integration.
    #[test]
    fn a_dropped_field_is_caught() {
        let dir = project(&[(
            "src/user.rs",
            "pub struct User { pub id: Uuid, pub email: String }",
        )]);
        let breaches = check(dir.path(), &user_contract()).expect("checked");
        let names: Vec<&str> = breaches.iter().map(|b| b.what.as_str()).collect();
        assert!(
            names.iter().any(|n| n.contains("created_at")),
            "the dropped field must be named: {names:?}"
        );
    }

    /// A complete implementation passes, or the check is noise nobody trusts.
    #[test]
    fn a_faithful_implementation_reports_nothing() {
        let dir = project(&[
            (
                "src/user.rs",
                "pub struct User { pub id: Uuid, pub email: String, pub created_at: i64 }",
            ),
            ("src/api.rs", r#"route("/api/users", post(create_user))"#),
        ]);
        let breaches = check(dir.path(), &user_contract()).expect("checked");
        assert!(breaches.is_empty(), "unexpected breaches: {breaches:?}");
        assert!(report(&breaches).contains("Every declared contract is present"));
    }

    /// An endpoint nobody implemented is a 404 from a service that believes it
    /// is finished.
    #[test]
    fn an_unimplemented_endpoint_is_caught() {
        let dir = project(&[(
            "src/user.rs",
            "pub struct User { pub id: Uuid, pub email: String, pub created_at: i64 }",
        )]);
        let breaches = check(dir.path(), &user_contract()).expect("checked");
        assert!(
            breaches.iter().any(|b| b.what.contains("/api/users")),
            "got: {breaches:?}"
        );
    }

    /// A schema nobody built at all.
    #[test]
    fn a_missing_schema_is_caught_before_its_fields_are() {
        let dir = project(&[("src/other.rs", "fn unrelated() {}")]);
        let breaches = check(dir.path(), &user_contract()).expect("checked");
        assert!(breaches.iter().any(|b| b.what.contains("schema `User`")));
        // The fields are NOT then reported one by one: four lines about a shape
        // that does not exist buries the one fact that matters.
        assert!(
            !breaches.iter().any(|b| b.what.contains("User.email")),
            "got: {breaches:?}"
        );
    }

    /// Fields are credited to the file that holds the shape, not to a
    /// coincidence elsewhere in the tree.
    #[test]
    fn a_field_mentioned_in_an_unrelated_file_does_not_count() {
        let dir = project(&[
            ("src/user.rs", "pub struct User { pub id: Uuid }"),
            (
                "src/mailer.rs",
                "// email templates live here, created_at too",
            ),
        ]);
        let breaches = check(dir.path(), &user_contract()).expect("checked");
        assert!(
            breaches.iter().any(|b| b.what.contains("User.email")),
            "a mention in an unrelated file must not satisfy the contract: {breaches:?}"
        );
    }

    /// Every breach at once. One at a time costs a task attempt per round.
    #[test]
    fn all_breaches_are_reported_together() {
        let dir = project(&[("src/user.rs", "pub struct User { pub id: Uuid }")]);
        let breaches = check(dir.path(), &user_contract()).expect("checked");
        assert!(breaches.len() >= 3, "got: {breaches:?}");
        let rendered = report(&breaches);
        assert!(rendered.contains("email"));
        assert!(rendered.contains("created_at"));
        assert!(rendered.contains("/api/users"));
    }

    /// Build output holds copies of the source; scanning it would make a deleted
    /// name look present, which is the one direction this must never fail in.
    #[test]
    fn build_output_is_not_searched() {
        let dir = project(&[
            ("src/user.rs", "pub struct Nothing {}"),
            (
                "target/debug/old.rs",
                "pub struct User { id: Uuid, email: String, created_at: i64 }",
            ),
            (
                "node_modules/pkg/index.js",
                "class User { id; email; created_at; }",
            ),
        ]);
        let breaches = check(dir.path(), &user_contract()).expect("checked");
        assert!(
            breaches.iter().any(|b| b.what.contains("schema `User`")),
            "a stale copy in build output must not satisfy a contract: {breaches:?}"
        );
    }

    /// No contracts is a normal state, not a failure — most projects start there.
    #[test]
    fn a_project_with_no_contracts_passes() {
        let dir = project(&[("src/main.rs", "fn main() {}")]);
        assert!(check(dir.path(), &Contracts::default())
            .expect("checked")
            .is_empty());
    }

    /// A malformed contracts file is loud. Contracts that quietly stop applying
    /// are this module's own failure mode, one level up.
    #[test]
    fn a_broken_contracts_file_is_an_error() {
        let dir = project(&[(".incode/contracts.json", "{ not json")]);
        assert!(Contracts::load(dir.path()).is_err());
    }

    #[test]
    fn contracts_survive_a_save_and_load() {
        let dir = tempfile::tempdir().expect("temp dir");
        let written = user_contract();
        written.save(dir.path()).expect("save");
        assert_eq!(Contracts::load(dir.path()).expect("load"), written);
    }

    /// **The shape a generator actually produced when not told one.** Asked to
    /// write contracts with no schema given, an agent emitted fields as a LIST
    /// of objects and endpoints wrapped in an object with `routes` inside —
    /// richer than the map this crate writes, and perfectly reasonable.
    ///
    /// The map-only parser read that file as 24 schemas with ZERO fields and
    /// ZERO endpoints: it checked nothing and reported success. Trimmed from the
    /// real 51 KB file, including the `wire: null` case.
    const REAL_GENERATED: &str = r#"{
      "$schemaVersion": "1.0",
      "project": "symptom-intake",
      "schemas": {
        "Tenant": {
          "table": "tenant",
          "tenantScoped": false,
          "fields": [
            {"wire": "id", "column": "id", "type": "uuid", "required": true},
            {"wire": "name", "column": "name", "type": "string", "required": true}
          ]
        },
        "RateLimitCounter": {
          "fields": [
            {"wire": null, "column": "key", "type": "string", "required": true},
            {"wire": "count", "column": "count", "type": "int", "required": true}
          ]
        }
      },
      "endpoints": {
        "description": "Route handlers under module `web`.",
        "basePath": "/api",
        "routes": [
          {"path": "POST /encounters", "response": "Encounter", "errors": ["409 taken"]},
          {"path": "GET /emergency", "response": "text/html"}
        ]
      }
    }"#;

    #[test]
    fn fields_written_as_a_list_are_read_as_fields() {
        let contracts = parse_contracts(REAL_GENERATED).expect("the observed file parses");
        let tenant = contracts.schemas.get("Tenant").expect("Tenant is declared");
        assert_eq!(
            tenant.fields.keys().collect::<Vec<_>>(),
            vec!["id", "name"],
            "a list of field objects must read as fields, not as nothing"
        );
        assert_eq!(tenant.fields.get("id").map(String::as_str), Some("uuid"));
    }

    /// **`wire: null` is a statement, not a gap.** The field exists in storage
    /// and never crosses a boundary, so there is nothing for another module to
    /// disagree with — but a field that names itself in NO way is still an error.
    #[test]
    fn a_field_that_never_crosses_the_boundary_is_skipped_but_a_nameless_one_is_not() {
        let contracts = parse_contracts(REAL_GENERATED).expect("parses");
        let counter = contracts.schemas.get("RateLimitCounter").expect("declared");
        assert_eq!(
            counter.fields.keys().collect::<Vec<_>>(),
            vec!["count"],
            "the wire:null column is not part of the contract surface"
        );

        let nameless = r#"{"schemas":{"X":{"fields":[{"type":"string"}]}}}"#;
        assert!(
            parse_contracts(nameless).is_err(),
            "a field with no name and no statement about the wire must fail loudly"
        );
    }

    /// Endpoints wrapped in an object with `routes` read the same as a bare list.
    #[test]
    fn endpoints_wrapped_in_an_object_are_still_endpoints() {
        let contracts = parse_contracts(REAL_GENERATED).expect("parses");
        assert_eq!(contracts.endpoints.len(), 2);
        assert_eq!(contracts.endpoints[0].path, "POST /encounters");
        assert_eq!(contracts.endpoints[0].response, "Encounter");
    }

    /// **A shape this cannot read is an ERROR, never an empty set.** That is the
    /// whole lesson: a contracts file the parser silently skips checks nothing,
    /// and a check that verifies nothing while reporting success is worse than
    /// no check, because it is believed.
    #[test]
    fn an_unreadable_shape_fails_rather_than_checking_nothing() {
        for bad in [
            r#"{"schemas": [{"name":"User"}]}"#,
            r#"{"schemas":{"User":{"fields":"id, email"}}}"#,
            r#"{"endpoints": "POST /users"}"#,
        ] {
            assert!(
                parse_contracts(bad).is_err(),
                "this must not read as an empty contract: {bad}"
            );
        }
    }

    /// Both shapes coexist: the map form this crate writes still works.
    #[test]
    fn the_map_form_this_crate_writes_still_reads() {
        let written = r#"{"schemas":{"User":{"fields":{"id":"uuid","email":"string"}}},
            "endpoints":[{"path":"POST /api/users","response":"User"}]}"#;
        let contracts = parse_contracts(written).expect("parses");
        assert_eq!(contracts.schemas["User"].fields.len(), 2);
        assert_eq!(contracts.endpoints.len(), 1);
    }

    #[test]
    fn substring_false_positives_do_not_match_fields() {
        assert!(!mentions("fn validate(text: &str) -> bool", "id"));
        assert!(!mentions("struct User { pub guid: Uuid }", "id"));
        assert!(!mentions("struct User { pub widget_count: u32 }", "id"));
        assert!(!mentions("struct Config { filename: String }", "name"));
        assert!(!mentions("struct Keyboard { keycode: u32 }", "key"));
        assert!(!mentions("let superuser = true;", "User"));
    }

    #[test]
    fn legitimate_tokens_and_styles_match_fields() {
        assert!(mentions("pub id: Uuid", "id"));
        assert!(mentions("ID: string", "id"));
        assert!(mentions("_id: String", "id"));
        assert!(mentions("user.id", "id"));
        assert!(mentions(r#"{"id": 1}"#, "id"));
        assert!(mentions("user-id: 123", "user_id"));
    }

    #[test]
    fn routes_match_across_parameter_styles() {
        assert!(mentions(
            r#"app.get("/api/users/:id", handler)"#,
            "/api/users/{id}"
        ));
        assert!(mentions(r#"#[get("/api/users/{id}")]"#, "/api/users/{id}"));
    }

    #[test]
    fn route_variations_with_different_suffixes_do_not_match() {
        assert!(!mentions(
            r#"route("/api/users_archive", handler)"#,
            "/api/users"
        ));
        assert!(!mentions(r#"route("/api/user", handler)"#, "/api/users"));
    }
}
