//! Credentials and personal data in what is about to leave the machine.
//!
//! `/pr` pushes a branch and opens a pull request that other people read.
//! Before anything is pushed, the lines the branch ADDS and its commit
//! messages are checked for shapes that are credentials, and files that are
//! credentials by name; the added lines are also checked for personal data
//! ([`pii_in`]). A finding names the file and line and what it looked like —
//! never the value.
//!
//! Personal data is held to the same precision: a card number written like
//! one that passes Luhn, a US social security number in an issued range, an
//! email address on a real domain (not `example.*`, `.test`, `.invalid`,
//! `noreply`, or a `git@` remote). Commit messages are not checked for it: a
//! `Signed-off-by` trailer carries an email on purpose.
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

/// What personal data `line` holds the shape of, if any.
pub fn pii_in(line: &str) -> Option<&'static str> {
    if card_number(line) {
        Some("a payment card number")
    } else if us_ssn(line) {
        Some("a US social security number")
    } else if personal_email(line) {
        Some("an email address")
    } else {
        None
    }
}

fn luhn(digits: &[u32]) -> bool {
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| match (i % 2 == 1, d * 2) {
            (true, doubled) if doubled > 9 => doubled - 9,
            (true, doubled) => doubled,
            (false, _) => d,
        })
        .sum();
    sum.is_multiple_of(10)
}

/// Four groups of four digits, or a bare 15–16 digits with an issuer's
/// prefix (4, 51–55, 2221–2720, 34, 37, 6011, 65) — passing Luhn either way.
fn card_number(line: &str) -> bool {
    static RUN: OnceLock<Regex> = OnceLock::new();
    let run = RUN.get_or_init(|| {
        Regex::new(
            r"(?:^|[^0-9A-Za-z])(\d{4}[ -]\d{4}[ -]\d{4}[ -]\d{4}|\d{15,16})(?:$|[^0-9A-Za-z])",
        )
        .expect("a valid pattern")
    });
    run.captures_iter(line).any(|c| {
        let text = &c[1];
        let digits: Vec<u32> = text.chars().filter_map(|ch| ch.to_digit(10)).collect();
        let prefix = |n: usize| text[..n].parse::<u32>().unwrap_or(0);
        let issuer = text.contains([' ', '-'])
            || text.starts_with('4')
            || (51..=55).contains(&prefix(2))
            || (2221..=2720).contains(&prefix(4))
            || [34, 37, 65].contains(&prefix(2))
            || prefix(4) == 6011;
        let same = digits.windows(2).all(|w| w[0] == w[1]);
        issuer && !same && luhn(&digits)
    })
}

/// `ddd-dd-dddd` alone, in a range that is issued: not area 000, 666 or
/// 900–999, not group 00, not serial 0000.
fn us_ssn(line: &str) -> bool {
    static SSN: OnceLock<Regex> = OnceLock::new();
    let ssn = SSN.get_or_init(|| {
        Regex::new(r"(?:^|[^0-9A-Za-z-])(\d{3})-(\d{2})-(\d{4})(?:$|[^0-9A-Za-z-])")
            .expect("a valid pattern")
    });
    ssn.captures_iter(line).any(|c| {
        let area: u32 = c[1].parse().unwrap_or(0);
        area != 0 && area != 666 && area < 900 && &c[2] != "00" && &c[3] != "0000"
    })
}

/// An email address on a real domain: not a reserved or placeholder one,
/// not a `noreply` address, not a `git@host` remote.
fn personal_email(line: &str) -> bool {
    static EMAIL: OnceLock<Regex> = OnceLock::new();
    let email = EMAIL.get_or_init(|| {
        Regex::new(r"([A-Za-z0-9._%+-]+)@([A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.([A-Za-z]{2,}))\b")
            .expect("a valid pattern")
    });
    email.captures_iter(line).any(|c| {
        let local = c[1].to_ascii_lowercase();
        let domain = c[2].to_ascii_lowercase();
        let tld = c[3].to_ascii_lowercase();
        let reserved = ["example.com", "example.org", "example.net"]
            .iter()
            .any(|d| domain == *d || domain.ends_with(&format!(".{d}")))
            || ["example", "invalid", "test", "localhost", "local"].contains(&tld.as_str())
            || domain.ends_with("users.noreply.github.com");
        let placeholder = local == "git" || local.contains("noreply") || local.contains("no-reply");
        !reserved && !placeholder
    })
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
            if let Some(what) = shape_in(added).or_else(|| pii_in(added)) {
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

    /// **PERSONAL DATA IN AN ADDED LINE IS FOUND; WHAT ONLY LOOKS LIKE IT
    /// IS NOT.** Test card numbers (Luhn-valid, never issued), an
    /// issued-range SSN and a real-domain email are found; epoch
    /// milliseconds, versions, dates, placeholder emails, git remotes and
    /// npm specs are not; and a commit message's sign-off is not scanned.
    #[test]
    fn personal_data_is_found_and_lookalikes_are_not() {
        let visa = ["4111", "1111", "1111", "1111"];
        for (line, what) in [
            (format!("card: {}", visa.join(" ")), "a payment card number"),
            (format!("pan={}", visa.concat()), "a payment card number"),
            (
                format!("ssn {}-{}-{}", "123", "45", "6789"),
                "a US social security number",
            ),
            (
                format!("mail {}@{}", "jane.doe", "gmail.com"),
                "an email address",
            ),
        ] {
            assert_eq!(pii_in(&line), Some(what), "{line}");
        }
        for line in [
            "const now = 1696212345678;",
            "id = 4111111111111112",
            "let n = 1234 5678 9012 3456;",
            "date 2026-10-02",
            "ssn: 000-12-3456",
            "author: Jo <jo@example.com>",
            "Co-Authored-By: bot <noreply@anthropic.com>",
            "url = git@github.com:o/r.git",
            "npm i @scope/pkg@1.2.3",
            "send to admin@service.test",
        ] {
            assert_eq!(pii_in(line), None, "{line}");
        }
        let diff = format!(
            "+++ b/fixtures/users.json\n@@ -0,0 +1 @@\n+{{\"email\": \"{}@{}\"}}\n",
            "jane", "gmail.com"
        );
        assert_eq!(scan_diff(&diff)[0].what, "an email address");
        assert!(scan_messages("fix\n\nSigned-off-by: Jo <jo@gmail.com>").is_empty());
    }
}
