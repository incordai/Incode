//! The dependency gate: nothing gets built on a library the document never
//! argued for.
//!
//! The intake standards say every technology named in a PRD is version-pinned,
//! free or self-hostable, and feasibility-checked before it enters the plan.
//! Enforcing "every technology" needs an opinion about what counts as one, and
//! an opinion is exactly what a gate must not have.
//!
//! So this checks the half a machine can check without guessing: **the project's
//! own manifest is the list of technologies it actually depends on**, and every
//! entry on it should appear in the document that is supposed to have justified
//! it. A dependency in `Cargo.toml` that the PRD never mentions is a decision
//! nobody recorded — and it is the cheapest kind to acquire, because adding a
//! line to a manifest takes ten seconds and reviewing it takes an afternoon.
//!
//! # Why the manifest and not the prose
//!
//! Scanning a document for "technology names" means keeping a list of what
//! names look like, which is wrong the moment someone adopts something not on
//! the list — and silently wrong, which is worse. The manifest is the ground
//! truth about what is linked in, it is machine-readable, and it cannot drift
//! from reality because the build reads the same file.

use std::path::Path;

/// One manifest this understands, and how to read a dependency list out of it.
///
/// A function pointer rather than an enum because the only thing that varies is
/// the parse, and a table keeps the question "is this language covered?"
/// answerable by reading one list instead of tracing a chain of ifs.
struct Manifest {
    file: &'static str,
    parse: fn(&str) -> Vec<String>,
}

/// Manifests this understands, in the order they are looked for.
///
/// # Why hand-written parsers and not a crate per format
///
/// Adding a TOML crate to read a dependency list "would be its own joke", and
/// the same argument covers XML and YAML. These parsers read ONE list out of a
/// narrow, fixed shape. When a shape defeats one, it returns fewer names — and
/// the failure mode is a dependency that goes unchecked, never a build that
/// breaks or a false accusation against the PRD.
const MANIFEST_KINDS: &[Manifest] = &[
    Manifest {
        file: "Cargo.toml",
        parse: cargo_dependencies,
    },
    Manifest {
        file: "package.json",
        parse: npm_dependencies,
    },
    Manifest {
        file: "composer.json",
        parse: composer_dependencies,
    },
    Manifest {
        file: "requirements.txt",
        parse: requirements_dependencies,
    },
    Manifest {
        file: "pyproject.toml",
        parse: pyproject_dependencies,
    },
    Manifest {
        file: "go.mod",
        parse: go_mod_dependencies,
    },
    Manifest {
        file: "Gemfile",
        parse: gemfile_dependencies,
    },
    Manifest {
        file: "pom.xml",
        parse: maven_dependencies,
    },
    Manifest {
        file: "build.gradle",
        parse: gradle_dependencies,
    },
    Manifest {
        file: "build.gradle.kts",
        parse: gradle_dependencies,
    },
    Manifest {
        file: "pubspec.yaml",
        parse: pubspec_dependencies,
    },
    Manifest {
        file: "mix.exs",
        parse: mix_dependencies,
    },
];

/// Manifest file names this understands.
#[must_use]
pub fn manifests() -> Vec<&'static str> {
    MANIFEST_KINDS.iter().map(|m| m.file).collect()
}

/// Every dependency the project actually pulls in.
///
/// Returns nothing when there is no manifest this understands — a project in a
/// language nobody has taught this about must not be reported as having zero
/// dependencies, which would read as "all clear".
///
/// **Every matching manifest is read, not just the first.** A project can
/// genuinely be two things — a PHP application with a JavaScript front end is
/// the ordinary case, not the exotic one — and stopping at the first match
/// would report the front end's libraries as the whole dependency list and
/// give the back end's a clean bill nobody checked.
#[must_use]
pub fn declared_dependencies(workspace: &Path) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut add = |found: Vec<String>| {
        for name in found {
            if !names.iter().any(|n| n == &name) {
                names.push(name);
            }
        }
    };

    for kind in MANIFEST_KINDS {
        let path = workspace.join(kind.file);
        if !path.is_file() {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            add((kind.parse)(&text));
        }
    }

    // .NET names its project file after the project, so it cannot be a fixed
    // entry in the table above.
    if let Ok(entries) = std::fs::read_dir(workspace) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let is_project = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e, "csproj" | "fsproj" | "vbproj"));
            if is_project {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    add(dotnet_dependencies(&text));
                }
            }
        }
    }

    names
}

/// Dependency names from a Cargo manifest.
///
/// Hand-parsed rather than adding a TOML dependency to read a dependency list,
/// which would be its own joke. The shape is narrow and fixed: a `[dependencies]`
/// table, one `name = ...` per line, ending at the next table header.
#[must_use]
pub fn cargo_dependencies(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut inside = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // Dev-dependencies are test scaffolding, not product decisions, and
            // holding a PRD to them would flood the report with noise.
            inside = trimmed == "[dependencies]"
                || (trimmed.starts_with("[target.") && trimmed.ends_with(".dependencies]"));
            continue;
        }
        if !inside || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = trimmed.split_once('=') {
            let name = name.trim().trim_matches('"');
            if !name.is_empty() && !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Dependency names from a Node manifest.
#[must_use]
pub fn npm_dependencies(manifest: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(manifest) else {
        return Vec::new();
    };
    value
        .get("dependencies")
        .and_then(|d| d.as_object())
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default()
}

/// The first single- or double-quoted run in a line, if any.
fn first_quoted(line: &str) -> Option<&str> {
    for quote in ['"', '\''] {
        if let Some(open) = line.find(quote) {
            let rest = &line[open + 1..];
            if let Some(close) = rest.find(quote) {
                return Some(&rest[..close]);
            }
        }
    }
    None
}

/// The text between `<tag>` and `</tag>` on one line.
fn tag_value<'a>(line: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = line.find(&open)? + open.len();
    let end = line[start..].find(&close)? + start;
    Some(line[start..end].trim())
}

/// A name with any version specifier, extras or comment stripped off.
fn bare_name(raw: &str) -> String {
    let cut = raw
        .find(|c: char| "=<>~!^*;,[( \t#@".contains(c))
        .unwrap_or(raw.len());
    raw[..cut]
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string()
}

/// Dependency names from a Composer manifest (PHP).
///
/// `require`, not `dependencies` — and `require-dev` is left out for the same
/// reason dev-dependencies are everywhere else: test scaffolding is not a
/// product decision.
///
/// `php` itself and the `ext-*` entries are dropped. They are platform
/// requirements, not libraries someone chose, and a PRD that had to argue for
/// "php" to build a PHP project would be arguing with itself.
#[must_use]
pub fn composer_dependencies(manifest: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(manifest) else {
        return Vec::new();
    };
    value
        .get("require")
        .and_then(|d| d.as_object())
        .map(|map| {
            map.keys()
                .filter(|k| k.as_str() != "php" && !k.starts_with("ext-"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Dependency names from a pip requirements file.
#[must_use]
pub fn requirements_dependencies(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        // `-r other.txt`, `--index-url ...` and comments are not dependencies.
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            continue;
        }
        let name = bare_name(trimmed);
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Dependency names from a `pyproject.toml`.
///
/// Reads both shapes in use: PEP 621's `dependencies = [...]` array and
/// Poetry's `[tool.poetry.dependencies]` table.
#[must_use]
pub fn pyproject_dependencies(manifest: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut in_poetry_table = false;
    let mut in_array = false;

    let push = |raw: &str, names: &mut Vec<String>| {
        let name = bare_name(raw);
        // `python` is the interpreter, not a library someone chose.
        if !name.is_empty() && name != "python" && !names.contains(&name) {
            names.push(name);
        }
    };

    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_poetry_table = trimmed == "[tool.poetry.dependencies]";
            in_array = false;
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if in_array {
            if trimmed.starts_with(']') {
                in_array = false;
                continue;
            }
            push(trimmed.trim_start_matches(['"', '\'']), &mut names);
            continue;
        }

        if in_poetry_table {
            if let Some((name, _)) = trimmed.split_once('=') {
                push(name, &mut names);
            }
            continue;
        }

        if let Some((key, rest)) = trimmed.split_once('=') {
            if key.trim() == "dependencies" {
                let rest = rest.trim();
                // Either the whole array on one line, or an open bracket.
                if let Some(inner) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                    for item in inner.split(',') {
                        push(item.trim().trim_start_matches(['"', '\'']), &mut names);
                    }
                } else if rest == "[" {
                    in_array = true;
                }
            }
        }
    }
    names
}

/// Module paths from a `go.mod`.
///
/// Indirect requirements are transitive — nobody chose them, so holding a PRD
/// to them would flood the report with libraries the author never saw.
#[must_use]
pub fn go_mod_dependencies(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_block = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.contains("// indirect") {
            continue;
        }
        if trimmed.starts_with("require (") {
            in_block = true;
            continue;
        }
        if in_block && trimmed.starts_with(')') {
            in_block = false;
            continue;
        }
        let candidate = if in_block {
            trimmed
        } else if let Some(rest) = trimmed.strip_prefix("require ") {
            rest
        } else {
            continue;
        };
        if candidate.is_empty() || candidate.starts_with("//") {
            continue;
        }
        let name = candidate
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Gem names from a `Gemfile`.
#[must_use]
pub fn gemfile_dependencies(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || !trimmed.starts_with("gem ") {
            continue;
        }
        if let Some(name) = first_quoted(trimmed) {
            let name = name.to_string();
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// Artifact ids from a Maven `pom.xml`.
///
/// Only inside a `<dependency>` element. The project's own `<artifactId>` and
/// its parent's sit at the top level of the same file, and reporting those
/// would accuse a PRD of failing to justify the project itself.
#[must_use]
pub fn maven_dependencies(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut depth = 0_u32;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.contains("<dependency>") {
            depth += 1;
        }
        if depth > 0 {
            if let Some(name) = tag_value(trimmed, "artifactId") {
                let name = name.to_string();
                if !name.is_empty() && !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        if trimmed.contains("</dependency>") {
            depth = depth.saturating_sub(1);
        }
    }
    names
}

/// Artifact names from a Gradle build script, Groovy or Kotlin.
///
/// The artifact segment of `group:artifact:version`, because that is what a
/// person writes in a document. Test configurations are skipped, like
/// dev-dependencies everywhere else.
#[must_use]
pub fn gradle_dependencies(manifest: &str) -> Vec<String> {
    const CONFIGS: [&str; 5] = [
        "implementation",
        "api",
        "compileOnly",
        "runtimeOnly",
        "annotationProcessor",
    ];
    let mut names = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        let first = trimmed
            .split(|c: char| c.is_whitespace() || c == '(')
            .next()
            .unwrap_or("");
        if !CONFIGS.contains(&first) {
            continue;
        }
        let Some(coordinate) = first_quoted(trimmed) else {
            continue;
        };
        // group:artifact:version -> artifact. A coordinate with no colon is
        // already a bare name.
        let mut parts = coordinate.split(':');
        let name = match (parts.next(), parts.next()) {
            (Some(_), Some(artifact)) if !artifact.is_empty() => artifact.to_string(),
            _ => coordinate.to_string(),
        };
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Package names from a Dart or Flutter `pubspec.yaml`.
#[must_use]
pub fn pubspec_dependencies(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_block = false;
    for line in manifest.lines() {
        if line.trim_end() == "dependencies:" {
            in_block = true;
            continue;
        }
        // Any other unindented key ends the block.
        if in_block && !line.starts_with([' ', '\t']) && !line.trim().is_empty() {
            in_block = false;
        }
        if !in_block {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Only the block's own keys, not the nested lines under one (`sdk:`).
        let indent = line.len() - line.trim_start().len();
        if indent > 2 {
            continue;
        }
        if let Some((key, _)) = trimmed.split_once(':') {
            let name = key.trim().to_string();
            // The Flutter SDK itself is the platform, not a chosen library.
            if !name.is_empty() && name != "flutter" && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// Package names from an Elixir `mix.exs`, written `{:name, "~> 1.0"}`.
#[must_use]
pub fn mix_dependencies(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    for (index, _) in manifest.match_indices("{:") {
        let rest = &manifest[index + 2..];
        let end = rest
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        let name = rest[..end].to_string();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Package names from a .NET project file.
#[must_use]
pub fn dotnet_dependencies(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if !trimmed.contains("<PackageReference") {
            continue;
        }
        let Some(start) = trimmed.find("Include=") else {
            continue;
        };
        if let Some(name) = first_quoted(&trimmed[start..]) {
            let name = name.to_string();
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// Dependencies the document never mentions.
///
/// Case-insensitive substring, which is deliberately generous: the aim is to
/// catch what was never discussed at all, not to police how it was written. A
/// gate that flagged a library because the PRD spelled it differently would be
/// ignored within a week, and an ignored gate protects nothing.
#[must_use]
pub fn undocumented(dependencies: &[String], prd: &str) -> Vec<String> {
    if prd.trim().is_empty() {
        // Nothing to check against. The missing PRD is the finding.
        return Vec::new();
    }
    let haystack = prd.to_lowercase();
    dependencies
        .iter()
        .filter(|name| !haystack.contains(&name.to_lowercase()))
        .cloned()
        .collect()
}

/// The whole check for one project.
#[must_use]
pub fn unvetted(workspace: &Path, prd: &str) -> Vec<String> {
    undocumented(&declared_dependencies(workspace), prd)
}

/// What to tell the session when a dependency was never written down.
///
/// Phrased as work, like every other intake finding: the answer is to argue for
/// the dependency or remove it, and both are things this session can do.
#[must_use]
pub fn intake_note(unvetted: &[String]) -> Option<String> {
    if unvetted.is_empty() {
        return None;
    }
    Some(format!(
        "{} dependency(ies) are linked into this project but appear nowhere in \
         the PRD:\n{}\n\nEach one is a decision nobody recorded. For each: name \
         the exact version in use, confirm it is free or self-hostable, say what \
         it is for and what was considered instead — then write that into the \
         PRD with `save_brief`. If it cannot be justified, remove it instead. Do \
         not state a version, a licence or a running cost you have not verified.",
        unvetted.len(),
        unvetted
            .iter()
            .map(|d| format!("  - {d}"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
[package]
name = "thing"
version = "0.1.0"

[dependencies]
serde = { version = "1", features = ["derive"] }
# A comment about the next one.
reqwest = "0.12"
tokio = { version = "1", features = ["full"] }

[dev-dependencies]
tempfile = "3"

[features]
default = []
"#;

    #[test]
    fn a_cargo_manifest_yields_its_real_dependencies() {
        let deps = cargo_dependencies(MANIFEST);
        assert_eq!(deps, vec!["serde", "reqwest", "tokio"]);
    }

    /// **Dev-dependencies are not product decisions.** Including them would
    /// flood the report with test scaffolding and train everyone to ignore it.
    #[test]
    fn test_only_dependencies_are_not_held_against_the_document() {
        assert!(!cargo_dependencies(MANIFEST).contains(&"tempfile".to_string()));
    }

    /// Neither are features, package metadata, or comments.
    #[test]
    fn nothing_but_dependencies_is_read_as_a_dependency() {
        let deps = cargo_dependencies(MANIFEST);
        for not_a_dep in ["name", "version", "default", "package"] {
            assert!(
                !deps.contains(&not_a_dep.to_string()),
                "`{not_a_dep}` is not a dependency"
            );
        }
    }

    #[test]
    fn a_node_manifest_yields_its_runtime_dependencies() {
        let manifest = r#"{"name":"x","dependencies":{"react":"^18"},
            "devDependencies":{"vitest":"^1"}}"#;
        assert_eq!(npm_dependencies(manifest), vec!["react".to_string()]);
    }

    /// The point of the whole module: something linked in that the document
    /// never argued for.
    #[test]
    fn a_dependency_the_document_never_mentions_is_reported() {
        let deps = cargo_dependencies(MANIFEST);
        let prd = "We use serde for the wire format and tokio for the runtime.";
        assert_eq!(undocumented(&deps, prd), vec!["reqwest".to_string()]);
    }

    /// Generous on purpose: a gate that flagged a library over spelling would be
    /// switched off, and a gate nobody runs protects nothing.
    #[test]
    fn a_mention_in_any_form_counts_as_a_mention() {
        let deps = vec!["Reqwest".to_string()];
        assert!(undocumented(&deps, "the HTTP client is reqwest 0.12").is_empty());
    }

    /// With no PRD there is nothing to check against, and reporting every
    /// dependency would bury the one thing that needs writing.
    #[test]
    fn a_missing_prd_produces_no_dependency_findings() {
        assert!(undocumented(&["serde".to_string()], "   ").is_empty());
    }

    /// A language nobody taught this about must not read as "no dependencies",
    /// which is indistinguishable from all-clear.
    #[test]
    fn an_unknown_project_layout_reports_nothing_rather_than_all_clear() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(declared_dependencies(dir.path()).is_empty());
        assert!(unvetted(dir.path(), "a document").is_empty());
    }

    /// The note has to say what to DO. "Unvetted dependency" alone teaches
    /// nobody what the gate wants.
    #[test]
    fn the_note_says_what_the_session_should_do_about_it() {
        let note = intake_note(&["reqwest".to_string()]).expect("a note");
        assert!(note.contains("reqwest"));
        assert!(note.contains("exact version"), "got: {note}");
        assert!(note.contains("free or self-hostable"), "got: {note}");
        assert!(
            note.contains("not state a version, a licence or a running cost you have not verified"),
            "the gate must not invite invented answers: {note}"
        );
        assert!(intake_note(&[]).is_none());
    }

    /// End to end against a real manifest on disk.
    #[test]
    fn the_check_reads_the_manifest_the_build_reads() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("Cargo.toml"), MANIFEST).expect("write");
        let loose = unvetted(dir.path(), "serde and tokio are discussed at length");
        assert_eq!(loose, vec!["reqwest".to_string()]);
    }

    /// **PHP — the language this gate was blind to.**
    ///
    /// `require`, not `dependencies`; and the platform entries are not
    /// libraries anyone chose, so a PRD must not be accused of failing to
    /// argue for "php" in a PHP project.
    #[test]
    fn composer_reads_require_and_drops_the_platform() {
        let names = composer_dependencies(
            r#"{
              "require": {
                "php": "^8.2",
                "ext-json": "*",
                "laravel/framework": "^11.0",
                "guzzlehttp/guzzle": "^7.8"
              },
              "require-dev": { "phpunit/phpunit": "^11.0" }
            }"#,
        );
        assert!(names.contains(&"laravel/framework".to_string()));
        assert!(names.contains(&"guzzlehttp/guzzle".to_string()));
        assert!(
            !names.iter().any(|n| n == "php"),
            "the interpreter is not a dependency: {names:?}"
        );
        assert!(!names.iter().any(|n| n.starts_with("ext-")), "{names:?}");
        assert!(
            !names.iter().any(|n| n.contains("phpunit")),
            "require-dev is test scaffolding, not a product decision: {names:?}"
        );
    }

    /// One realistic snippet per ecosystem, asserting the name a PRD would use.
    #[test]
    fn every_manifest_kind_yields_the_names_a_document_would_name() {
        assert_eq!(
            requirements_dependencies("# comment\n-r base.txt\nrequests>=2.0\nflask==2.0  # web\n"),
            vec!["requests".to_string(), "flask".to_string()]
        );

        let poetry = pyproject_dependencies(
            "[tool.poetry.dependencies]\npython = \"^3.11\"\nrequests = \"^2.31\"\n",
        );
        assert_eq!(
            poetry,
            vec!["requests".to_string()],
            "the interpreter is not a dependency"
        );

        let pep621 = pyproject_dependencies(
            "[project]\ndependencies = [\n  \"httpx>=0.27\",\n  \"pydantic\",\n]\n",
        );
        assert_eq!(pep621, vec!["httpx".to_string(), "pydantic".to_string()]);

        let go = go_mod_dependencies(
            "module example.com/m\n\nrequire (\n\tgithub.com/gin-gonic/gin v1.9.1\n\tgithub.com/x/y v0.1.0 // indirect\n)\n",
        );
        assert_eq!(
            go,
            vec!["github.com/gin-gonic/gin".to_string()],
            "an indirect requirement is transitive; nobody chose it"
        );

        assert_eq!(
            gemfile_dependencies(
                "source 'https://rubygems.org'\ngem 'rails', '~> 7.1'\ngem \"pg\"\n"
            ),
            vec!["rails".to_string(), "pg".to_string()]
        );

        let maven = maven_dependencies(
            "<project>\n<artifactId>my-app</artifactId>\n<dependencies>\n<dependency>\n<groupId>org.springframework.boot</groupId>\n<artifactId>spring-boot-starter-web</artifactId>\n</dependency>\n</dependencies>\n</project>",
        );
        assert_eq!(
            maven,
            vec!["spring-boot-starter-web".to_string()],
            "the project's own artifactId must not be reported as its dependency"
        );

        let gradle = gradle_dependencies(
            "dependencies {\n  implementation 'com.squareup.okhttp3:okhttp:4.12.0'\n  api(\"org.jetbrains.kotlinx:kotlinx-coroutines-core:1.8.0\")\n  testImplementation 'junit:junit:4.13'\n}\n",
        );
        assert_eq!(
            gradle,
            vec!["okhttp".to_string(), "kotlinx-coroutines-core".to_string()],
            "test configurations are scaffolding: {gradle:?}"
        );

        let pubspec = pubspec_dependencies(
            "name: app\ndependencies:\n  flutter:\n    sdk: flutter\n  http: ^1.2.0\n  provider: ^6.1.0\n\ndev_dependencies:\n  test: any\n",
        );
        assert_eq!(
            pubspec,
            vec!["http".to_string(), "provider".to_string()],
            "the SDK is the platform, and dev_dependencies end the block: {pubspec:?}"
        );

        assert_eq!(
            mix_dependencies("defp deps do\n  [\n    {:phoenix, \"~> 1.7\"},\n    {:ecto_sql, \"~> 3.11\"}\n  ]\nend\n"),
            vec!["phoenix".to_string(), "ecto_sql".to_string()]
        );

        assert_eq!(
            dotnet_dependencies(
                "<Project>\n<ItemGroup>\n<PackageReference Include=\"Newtonsoft.Json\" Version=\"13.0.3\" />\n</ItemGroup>\n</Project>"
            ),
            vec!["Newtonsoft.Json".to_string()]
        );
    }

    /// **A PROJECT CAN BE TWO THINGS, AND BOTH LISTS MUST BE CHECKED.**
    ///
    /// A PHP application with a JavaScript front end is the ordinary case.
    /// Stopping at the first manifest would report the front end's libraries
    /// as the whole list and hand the back end's a clean bill nobody checked.
    #[test]
    fn a_project_with_two_manifests_has_both_read() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("composer.json"),
            r#"{"require":{"laravel/framework":"^11.0"}}"#,
        )
        .expect("write");
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies":{"vue":"^3.4.0"}}"#,
        )
        .expect("write");

        let found = declared_dependencies(dir.path());
        assert!(
            found.contains(&"laravel/framework".to_string()),
            "{found:?}"
        );
        assert!(found.contains(&"vue".to_string()), "{found:?}");

        // And the gate then catches whichever of them the PRD never argued for.
        let missing = unvetted(dir.path(), "The back end uses laravel/framework.");
        assert_eq!(missing, vec!["vue".to_string()]);
    }

    /// A language nobody taught this about reports nothing, not "all clear".
    #[test]
    fn an_unknown_ecosystem_reports_no_dependencies_rather_than_none_found() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("Makefile"), "all:\n\tcc main.c").expect("write");
        assert!(declared_dependencies(dir.path()).is_empty());
        // The point: an empty list must not be read as a passed check. It is
        // the same answer as "no manifest", and `unvetted` therefore flags
        // nothing rather than certifying anything.
        assert!(unvetted(dir.path(), "").is_empty());
    }
}
