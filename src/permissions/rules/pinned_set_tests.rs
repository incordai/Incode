//! What the rules decide beyond splitting a command, as it was recorded
//! before they were restructured: the grammar of a rule and the tools it
//! names, path patterns and `normalise`, domain rules, a project's ignored
//! `allow` list, a broken managed file, and the order of the lists across
//! the layers.
//!
//! Every input is lower case, has forward slashes and no drive letter in a
//! path matched, so the recording holds on every platform: matching is case
//! insensitive only on Windows and macOS, and `C:/x` is absolute only on
//! Windows.

use std::path::{Component, Path, PathBuf};

use super::{normalise, Allowlist, Layer, PathContext, PermissionRules, Rule, RuleLists, Subject};
use crate::parser_pins::{assert_pinned, esc, list};

const RECORDING: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/pinned/permissions_rules_set.txt"
));

const HOME: &str = "/home/me";

fn ctx() -> PathContext {
    PathContext {
        workspace: PathBuf::from("/ws"),
        home: Some(PathBuf::from(HOME)),
    }
}

fn owned(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|s| (*s).to_string()).collect()
}

/// One layer's lists added to `rules`.
fn add(rules: &mut PermissionRules, layer: Layer, allow: &[&str], ask: &[&str], deny: &[&str]) {
    let lists = RuleLists {
        allow: owned(allow),
        ask: owned(ask),
        deny: owned(deny),
    };
    rules.add(&lists, layer, Some(Path::new(HOME)));
}

fn verdict(rules: &PermissionRules, tool: &str, subject: &Subject) -> String {
    rules
        .decide(tool, subject, &ctx())
        .map_or_else(|| "none".to_string(), |v| v.describe())
}

fn command(text: &str) -> Subject {
    Subject::Command(text.to_string())
}

fn paths(path: &str) -> Subject {
    Subject::Paths(vec![path.to_string()])
}

fn url(text: &str) -> Subject {
    Subject::Url(text.to_string())
}

/// Rules as written: parsed with a home directory and without one.
const RULES: &[&str] = &[
    "",
    "Bash",
    "  Bash  ",
    "Bash()",
    "Bash(*)",
    "Bash(:*)",
    "Bash(   )",
    "Bash(npm test:*)",
    "Bash(npm   test)",
    "Bash(:*",
    "Bash(npm test",
    "Bash(ls))",
    "Bash((ls)",
    "(x)",
    "Ba sh",
    "Bash!",
    "*",
    "mcp__*",
    "mcp__github",
    "mcp__github__create_issue",
    "Read",
    "Read(.env)",
    "Read([)",
    "Read(a(b))",
    "Read(~/.ssh/**)",
    "Read(//etc/shadow)",
    "Read(C:/Windows/**)",
    r"Read(c:\Users\me\**)",
    "Read(/)",
    "Read(./)",
    "Read(~/)",
    "Edit(secrets/)",
    "Write(x.txt)",
    "Grep(src/**)",
    "WebFetch(domain:Example.COM)",
    "WebFetch(domain:  )",
    "WebFetch(domain:)",
    "WebFetch(example.com)",
    "Task(x)",
];

fn parsing() -> Vec<String> {
    let mut lines = Vec::new();
    for text in RULES {
        for home in [Some(Path::new(HOME)), None] {
            let parsed = match Rule::parse(text, home) {
                Ok(rule) => format!("ok {}", esc(&rule.text)),
                Err(why) => format!("err {}", esc(&why)),
            };
            let with = if home.is_some() { "home" } else { "no-home" };
            lines.push(format!("parse {} {with} => {parsed}", esc(text)));
        }
    }
    lines
}

/// Tool names, matched by every rule that names tools without a specifier.
const TOOLS: &[&str] = &[
    "Bash",
    "BashX",
    "Read",
    "Glob",
    "Grep",
    "LS",
    "Outline",
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "ApplyPatch",
    "WebFetch",
    "WebSearch",
    "Task",
    "mcp__github",
    "mcp__github__create_issue",
    "mcp__githubx__thing",
    "mcp__fs__write_file",
];

fn naming() -> Vec<String> {
    let mut lines = Vec::new();
    for rule in [
        "*",
        "Bash",
        "Bash*",
        "mcp__*",
        "mcp__github",
        "mcp__github__*",
        "mcp__github__create_issue",
        "Read",
        "Edit",
        "Write",
        "Web*",
        "*Edit",
    ] {
        let mut rules = PermissionRules::none();
        add(&mut rules, Layer::User, &[], &[], &[rule]);
        let named: Vec<&str> = TOOLS
            .iter()
            .copied()
            .filter(|tool| verdict(&rules, tool, &Subject::None) != "none")
            .collect();
        lines.push(format!("names {} => {}", esc(rule), list(&named)));
    }
    lines
}

const PATTERNS: &[&str] = &[
    ".env",
    "secrets",
    "secrets/",
    "/build",
    "./src/**",
    "src/**",
    "src/*.rs",
    "~/.ssh/**",
    "//etc/shadow",
    "/ws/abs",
    "c:/windows/**",
    "*.pem",
    "**/*.key",
    "a/../b/**",
    "../out/**",
    ".",
];

const PATHS: &[&str] = &[
    ".env",
    "app/.env",
    "/elsewhere/.env",
    ".env.example",
    "secrets",
    "secrets/k",
    "a/secrets/b",
    "build/o",
    "x/build/o",
    "src/a.rs",
    "src/d/a.rs",
    "lib/src/a.rs",
    "/ws/src/a.rs",
    "/home/me/.ssh/id",
    "~/.ssh/id",
    "/etc/shadow",
    "etc/shadow",
    "k.pem",
    "d/k.key",
    "../out/x",
    "/out/x",
    "src/../.env",
    "./src/./../secrets/k",
    "b/c",
    "a/../b/c",
    "/ws/abs",
    "ws/abs",
    "",
];

fn path_patterns() -> Vec<String> {
    let mut lines = Vec::new();
    for pattern in PATTERNS {
        let rule = format!("Read({pattern})");
        let mut rules = PermissionRules::none();
        add(&mut rules, Layer::User, &[], &[], &[rule.as_str()]);
        let matched: Vec<&str> = PATHS
            .iter()
            .copied()
            .filter(|path| verdict(&rules, "Read", &paths(path)) != "none")
            .collect();
        lines.push(format!(
            "read {} warnings {} => {}",
            esc(pattern),
            list(rules.warnings()),
            list(&matched)
        ));
    }
    lines
}

/// A path as its components, the root spelled `/` on every platform.
fn components(path: &Path) -> Vec<String> {
    path.components()
        .map(|part| match part {
            Component::RootDir => "/".to_string(),
            other => other.as_os_str().to_string_lossy().into_owned(),
        })
        .collect()
}

fn normalising() -> Vec<String> {
    let mut lines = Vec::new();
    for base in ["/ws", "/", "/ws/sub"] {
        for path in [
            "",
            ".",
            "..",
            "../..",
            "../../../x",
            "a/./b/../c",
            "/abs/../x",
            "./",
            "a//b",
            "~/x",
            "a/b/",
            "..a/b..",
        ] {
            lines.push(format!(
                "normalise {} {} => {}",
                esc(base),
                esc(path),
                list(&components(&normalise(Path::new(base), path)))
            ));
        }
    }
    lines
}

const URLS: &[&str] = &[
    "https://example.com/a",
    "https://EXAMPLE.com/",
    "https://docs.example.com/a",
    "https://a.b.example.com",
    "https://example.com:8443/x",
    "http://user:pw@example.com/",
    "https://example.com./",
    "https://example.com.evil.net/",
    "https://notexample.com/",
    "https://evil.com/",
    "https://sub.evil.com/x",
    "ftp://example.com/",
    "example.com",
    "not a url",
    "https://",
    "https://[::1]/",
    "https://127.0.0.1/",
    "file:///etc/passwd",
];

fn domains() -> Vec<String> {
    let mut lines = Vec::new();
    for rule in [
        "WebFetch(domain:example.com)",
        "WebFetch(domain:Example.COM)",
        "WebFetch(domain:evil.com)",
        "WebFetch",
    ] {
        let mut allowed = PermissionRules::none();
        add(&mut allowed, Layer::User, &[rule], &[], &[]);
        let mut denied = PermissionRules::none();
        add(&mut denied, Layer::User, &["WebFetch"], &[], &[rule]);
        for (list_name, rules) in [("allow", &allowed), ("deny", &denied)] {
            let verdicts: Vec<String> = URLS
                .iter()
                .map(|u| verdict(rules, "WebFetch", &url(u)))
                .collect();
            lines.push(format!("{list_name} {} => {}", esc(rule), list(&verdicts)));
        }
    }
    lines
}

/// The calls each rule set below is asked about.
fn calls() -> Vec<(&'static str, Subject)> {
    vec![
        ("Bash", command("ls")),
        ("Bash", command("git push origin")),
        ("Bash", command("rm -rf /")),
        ("Read", paths(".env")),
        ("Read", paths("a.txt")),
        ("Write", paths("src/a.rs")),
        ("WebFetch", url("https://example.com/")),
        ("mcp__x__y", Subject::None),
    ]
}

fn summary(name: &str, rules: &PermissionRules) -> String {
    let verdicts: Vec<String> = calls()
        .iter()
        .map(|(tool, subject)| verdict(rules, tool, subject))
        .collect();
    format!(
        "{name} => warnings {} | describe {} | empty {} | {}",
        list(rules.warnings()),
        list(&rules.describe()),
        rules.is_empty(),
        list(&verdicts)
    )
}

fn special_sets() -> Vec<String> {
    let mut project = PermissionRules::none();
    add(
        &mut project,
        Layer::Project,
        &["Bash", "Read"],
        &["Bash(git push:*)"],
        &["Read(.env)", "Bash(oops"],
    );
    let mut project_no_allow = PermissionRules::none();
    add(
        &mut project_no_allow,
        Layer::Project,
        &[],
        &[],
        &["Read(.env)"],
    );
    let mut broken = PermissionRules::none();
    broken.deny_everything("bad toml");
    let mut broken_after_allow = PermissionRules::none();
    add(
        &mut broken_after_allow,
        Layer::User,
        &["Bash", "Read"],
        &[],
        &[],
    );
    broken_after_allow.deny_everything("bad toml");
    let mut user = PermissionRules::none();
    add(
        &mut user,
        Layer::User,
        &[
            "Bash",
            "Read",
            "Edit(src/**)",
            "WebFetch(domain:example.com)",
        ],
        &["Bash(git push:*)"],
        &["Read(.env)", "Bash(rm -rf:*)"],
    );
    vec![
        summary("none", &PermissionRules::none()),
        summary("project", &project),
        summary("project-no-allow", &project_no_allow),
        summary("deny-everything", &broken),
        summary("deny-everything-after-allow", &broken_after_allow),
        summary("user", &user),
    ]
}

/// Where a rule goes: a layer and one of its lists.
const PLACES: [(Layer, &str); 9] = [
    (Layer::Managed, "deny"),
    (Layer::Managed, "ask"),
    (Layer::Managed, "allow"),
    (Layer::User, "deny"),
    (Layer::User, "ask"),
    (Layer::User, "allow"),
    (Layer::Project, "deny"),
    (Layer::Project, "ask"),
    (Layer::Project, "allow"),
];

/// Two rules placed in every pair of places, the layers added in `order`.
fn layering(order: [Layer; 3]) -> Vec<String> {
    let rules_placed = ["Bash(git push:*)", "Bash"];
    let mut lines = Vec::new();
    for first in PLACES {
        for second in PLACES {
            let mut rules = PermissionRules::none();
            for layer in order {
                let mut allow = Vec::new();
                let mut ask = Vec::new();
                let mut deny = Vec::new();
                for (rule, (at, list_name)) in rules_placed.iter().zip([first, second]) {
                    if at == layer {
                        match list_name {
                            "deny" => deny.push(*rule),
                            "ask" => ask.push(*rule),
                            _ => allow.push(*rule),
                        }
                    }
                }
                add(&mut rules, layer, &allow, &ask, &deny);
            }
            lines.push(format!(
                "layers {:?} {}:{} {}:{} => {} | {} | warnings {}",
                order.map(Layer::as_str),
                first.0.as_str(),
                first.1,
                second.0.as_str(),
                second.1,
                verdict(&rules, "Bash", &command("git push origin")),
                verdict(&rules, "Bash", &command("ls")),
                rules.warnings().len()
            ));
        }
    }
    lines
}

fn allowlists() -> Vec<String> {
    let texts = owned(&[
        "Bash(npm test:*)",
        "Read",
        "Edit(src/**)",
        "Bash(oops",
        "WebFetch(domain:example.com)",
        "mcp__github",
    ]);
    let (allowlist, warnings) = Allowlist::parse(&texts, Some(Path::new(HOME)));
    let checks: Vec<String> = [
        ("Bash", command("npm test")),
        ("Bash", command("npm test && git status")),
        ("Bash", command("npm test $(curl x)")),
        ("Bash", command("")),
        ("Grep", paths(".")),
        ("Write", paths("src/a.rs")),
        ("Write", paths("Cargo.toml")),
        ("ApplyPatch", Subject::Paths(vec![])),
        ("WebFetch", url("https://docs.example.com/")),
        ("WebFetch", url("https://evil.com/")),
        ("mcp__github__x", Subject::None),
        ("Task", Subject::None),
    ]
    .iter()
    .map(|(tool, subject)| allowlist.permits(tool, subject, &ctx()).to_string())
    .collect();
    vec![format!(
        "allowlist warnings {} listed {} => {}",
        list(&warnings),
        list(allowlist.listed()),
        list(&checks)
    )]
}

/// **NO RESTRUCTURING CHANGES WHAT A RULE MEANS.** Parsing, the tools a rule
/// names, path and domain matching, the special rule sets and the order of
/// the lists across layers decide exactly as they did when recorded.
#[test]
fn every_pinned_rule_set_decides_as_recorded() {
    let mut lines = parsing();
    lines.extend(naming());
    lines.extend(path_patterns());
    lines.extend(normalising());
    lines.extend(domains());
    lines.extend(special_sets());
    lines.extend(layering([Layer::Managed, Layer::User, Layer::Project]));
    lines.extend(layering([Layer::Project, Layer::User, Layer::Managed]));
    lines.extend(allowlists());
    assert_pinned(RECORDING, &lines);
}
