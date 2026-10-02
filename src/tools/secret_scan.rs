//! Credentials in what is about to leave the machine.
//!
//! `/pr` pushes a branch and opens a pull request that other people read.
//! Before anything is pushed, the lines the branch ADDS and its commit
//! messages are checked for shapes that are credentials, and files that are
//! credentials by name. A finding names the file and line and what it looked
//! like — never the value.
//!
//! Only high-precision shapes are used: a prefixed key, a cloud key id, a
//! JWT, a private-key block, a bearer token. The generic `token: <value>`
//! shape that `crate::agent::stderr_tail` redacts in error output is left
//! out on purpose: in source code it matches `token: String` on every other
//! line, and a check that cries wolf gets overridden.

use std::sync::OnceLock;

use regex::Regex;

/// One credential-shaped thing in a diff or a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The file, or `commit message`.
    pub place: String,
    /// The line in the new file (1-based); 0 for a whole file or a message.
    pub line: usize,
    /// What it looked like: `a GitHub token`, `a private key file`.
    pub what: &'static str,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            write!(f, "{}: {}", self.place, self.what)
        } else {
            write!(f, "{}:{}: {}", self.place, self.line, self.what)
        }
    }
}

/// Credential shapes, each with what it is called in a finding.
const SHAPES: &[(&str, &str)] = &[
    (r"\bsk-[A-Za-z0-9_-]{20,}", "an API key (sk-…)"),
    (r"\bgh[pousr]_[A-Za-z0-9]{20,}", "a GitHub token"),
    (r"\bgithub_pat_[A-Za-z0-9_]{20,}", "a GitHub token"),
    (r"\bxox[abprs]-[A-Za-z0-9-]{10,}", "a Slack token"),
    (r"\bAKIA[0-9A-Z]{16}\b", "a cloud access key id"),
    (r"\bAIza[0-9A-Za-z_-]{35}\b", "a Google API key"),
    (
        r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
        "a JSON web token",
    ),
    (r"-----BEGIN [A-Z ]*PRIVATE KEY-----", "a private key"),
    (r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{20,}", "a bearer token"),
];

fn shapes() -> &'static [(Regex, &'static str)] {
    static COMPILED: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        SHAPES
            .iter()
            .filter_map(|(p, what)| Regex::new(p).ok().map(|re| (re, *what)))
            .collect()
    })
}

/// What a file is, when its name alone makes it a credential.
fn credential_file(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    // `.env`, `.env.production` — but the templates people commit on
    // purpose are not credentials.
    if lower == ".env"
        || (lower.starts_with(".env.")
            && ![".env.example", ".env.sample", ".env.template", ".env.dist"]
                .contains(&lower.as_str()))
    {
        return Some("an environment file");
    }
    if ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"].contains(&lower.as_str()) {
        return Some("a private key file");
    }
    if [".pem", ".p12", ".pfx"]
        .iter()
        .any(|ext| lower.ends_with(ext))
    {
        return Some("a key or certificate store");
    }
    None
}

fn shape_in(text: &str) -> Option<&'static str> {
    shapes()
        .iter()
        .find(|(re, _)| re.is_match(text))
        .map(|(_, what)| *what)
}

/// Every credential in what a unified diff ADDS: its `+` lines, read with
/// their line numbers in the new file, and every file it adds whose name
/// makes it a credential. Removed lines are not looked at: deleting a
/// secret is the fix, not the leak.
pub fn scan_diff(diff: &str) -> Vec<Finding> {
    let mut found = Vec::new();
    let mut file = String::new();
    let mut new_line = 0usize;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").unwrap_or(path).to_string();
            if file != "/dev/null" {
                if let Some(what) = credential_file(&file) {
                    found.push(Finding {
                        place: file.clone(),
                        line: 0,
                        what,
                    });
                }
            }
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("diff --git") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            // `@@ -a,b +c,d @@`: the new file's lines start at c.
            new_line = rest
                .split_whitespace()
                .find_map(|part| part.strip_prefix('+'))
                .and_then(|start| start.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            continue;
        }
        if let Some(added) = line.strip_prefix('+') {
            if let Some(what) = shape_in(added) {
                found.push(Finding {
                    place: file.clone(),
                    line: new_line,
                    what,
                });
            }
            new_line += 1;
        } else if line.starts_with(' ') {
            new_line += 1;
        }
    }
    found
}

/// Credentials in commit messages, which become the pull request's title
/// and body.
pub fn scan_messages(messages: &str) -> Vec<Finding> {
    shape_in(messages)
        .map(|what| Finding {
            place: "a commit message".to_string(),
            line: 0,
            what,
        })
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each fixture is SHAPED like a credential and is not one: the right
    // prefix and length, built at run time, so nothing real is written
    // down and no scanner reading this file is fooled.
    fn fake(prefix: &str, n: usize) -> String {
        format!("{prefix}{}", "Q".repeat(n))
    }

    #[test]
    fn every_shape_compiles() {
        assert_eq!(shapes().len(), SHAPES.len());
    }

    /// **A KEY IN AN ADDED LINE IS FOUND, AT ITS LINE.** The finding names
    /// the file, the line in the new file and the kind — never the value.
    #[test]
    fn an_added_credential_is_found_with_its_file_and_line() {
        let token = fake("ghp_", 36);
        let diff = format!(
            "diff --git a/src/config.ts b/src/config.ts\n\
             --- a/src/config.ts\n\
             +++ b/src/config.ts\n\
             @@ -10,2 +10,3 @@ export const config = {{\n\
             \x20 name: \"app\",\n\
             +  github: \"{token}\",\n\
             \x20 debug: false,\n"
        );
        let found = scan_diff(&diff);
        assert_eq!(
            found,
            [Finding {
                place: "src/config.ts".into(),
                line: 11,
                what: "a GitHub token"
            }]
        );
        assert!(!found[0].to_string().contains(&token), "the value leaked");
    }

    /// **WHAT IS NOT A LEAK IS NOT FLAGGED.** A removed key (the fix), code
    /// that merely names a token, and an `.env.example` template.
    #[test]
    fn removals_ordinary_code_and_templates_are_not_flagged() {
        let token = fake("sk-", 32);
        let diff = format!(
            "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,3 +1,2 @@\n\
             -let key = \"{token}\";\n\
             +let token: String = read_token();\n\
             +let password = std::env::var(\"PASSWORD\")?;\n\
             --- /dev/null\n+++ b/.env.example\n@@ -0,0 +1 @@\n+API_KEY=\n"
        );
        assert_eq!(scan_diff(&diff), []);
    }

    #[test]
    fn credential_files_and_messages_are_found() {
        let diff = "--- /dev/null\n+++ b/.env\n@@ -0,0 +1 @@\n+MODE=dev\n\
                    --- /dev/null\n+++ b/deploy/id_ed25519\n@@ -0,0 +1 @@\n+x\n\
                    --- /dev/null\n+++ b/certs/server.pem\n@@ -0,0 +1 @@\n+x\n";
        let places: Vec<String> = scan_diff(diff).into_iter().map(|f| f.place).collect();
        assert_eq!(places, [".env", "deploy/id_ed25519", "certs/server.pem"]);

        let pem = format!("-----BEGIN {}PRIVATE KEY-----", "RSA ");
        assert_eq!(
            scan_diff(&format!("+++ b/k.txt\n@@ -0,0 +1 @@\n+{pem}\n"))[0].what,
            "a private key"
        );
        assert_eq!(
            scan_messages(&format!("fix auth\n\nused {}", fake("AKIA", 16)))[0].what,
            "a cloud access key id"
        );
        assert!(scan_messages("fix: read the token from the keychain").is_empty());
    }
}
