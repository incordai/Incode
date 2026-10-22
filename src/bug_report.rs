//! `/bug`: what a bug report needs, and a link that opens a prefilled GitHub
//! issue for it.
//!
//! **Nothing is sent.** The report is printed and the link is printed; the
//! person reads both and decides whether to open it. No browser is started
//! here — `mcp_oauth::open_browser` is for login addresses only.
//!
//! What goes in: incode's version, the OS and architecture, the model and
//! permission mode in force, and what the person typed after `/bug`. Typed
//! text is scrubbed by [`crate::agent::stderr_tail::redact`] (bearer tokens,
//! `key=value` secrets, `sk-…`, GitHub and Slack tokens, AWS keys, JWTs), and
//! the home folder in a path becomes `~`. No settings file, transcript or
//! trace is read: none of them is redacted (`transcript.rs`), and a bug
//! report is public.
//!
//! Called by `cli::slash_commands::handle_bug`. The field ids in the link
//! are the ids of `.github/ISSUE_TEMPLATE/bug.yml`, which the tests hold to
//! each other.

use reqwest::Url;

/// Where a new issue is opened.
pub const NEW_ISSUE: &str = "https://github.com/incordai/Incode/issues/new";

/// The issue form the link fills in, in `.github/ISSUE_TEMPLATE/`.
pub const TEMPLATE: &str = "bug.yml";

/// The longest link made, in bytes. GitHub refuses an address of about 8 KB;
/// the description is cut to fit, by its ENCODED size — `é` is six bytes
/// once percent-encoded, so no character count bounds the link.
pub const MAX_URL_BYTES: usize = 7000;

/// What the session knows about itself.
#[derive(Debug, Clone, Copy)]
pub struct Facts<'a> {
    pub model: &'a str,
    pub permission_mode: &'a str,
    /// What was typed after `/bug`, as typed.
    pub description: &'a str,
    /// The home folder, replaced by `~` wherever it appears.
    pub home: Option<&'a str>,
}

/// The report, as printed, and the link that prefills it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub text: String,
    pub url: String,
    /// Whether the description was cut to fit the link.
    pub cut: bool,
}

/// `incode <version> on <os> <arch>`.
#[must_use]
pub fn version_line() -> String {
    format!(
        "incode {} on {} {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Typed text made fit to publish: secrets out, home folder as `~`.
#[must_use]
pub fn scrub(text: &str, home: Option<&str>) -> String {
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            let line = match home.filter(|h| h.len() > 1) {
                Some(home) => replace_home(line, home),
                None => line.to_string(),
            };
            // `redact`, not `clean`: `clean` also cuts a line to 512 bytes,
            // for stored error output; the length is bounded here instead.
            crate::agent::stderr_tail::redact(&line)
        })
        .collect();
    lines.join("\n")
}

/// `home` replaced by `~`, with either separator and in any letter case
/// (`C:\Users\me` and `c:/users/me`).
fn replace_home(line: &str, home: &str) -> String {
    let mut out = line.to_string();
    for spelling in [home.replace('\\', "/"), home.replace('/', "\\")] {
        // ASCII-only lowering keeps every byte offset the same in both.
        let needle = spelling.to_ascii_lowercase();
        let lower = out.to_ascii_lowercase();
        let mut result = String::new();
        let mut rest = 0;
        while let Some(at) = lower[rest..].find(&needle) {
            let start = rest + at;
            result.push_str(&out[rest..start]);
            result.push('~');
            rest = start + needle.len();
        }
        result.push_str(&out[rest..]);
        out = result;
    }
    out
}

/// How many bytes `c` takes in a query once form-encoded: letters, digits,
/// `*-._` and space (as `+`) one each; anything else `%XX` per UTF-8 byte.
fn encoded_len(c: char) -> usize {
    if c.is_ascii_alphanumeric() || matches!(c, '*' | '-' | '.' | '_' | ' ') {
        1
    } else {
        3 * c.len_utf8()
    }
}

/// The longest start of `text` whose encoding fits `room` bytes, with `…`
/// when it was cut; and whether it was.
fn fit(text: &str, room: usize) -> (String, bool) {
    let total: usize = text.chars().map(encoded_len).sum();
    if total <= room + encoded_len('…') {
        return (text.to_string(), false);
    }
    let mut used = 0;
    let mut out = String::new();
    for c in text.chars() {
        used += encoded_len(c);
        if used > room {
            break;
        }
        out.push(c);
    }
    out.push('…');
    (out, true)
}

/// The report for `facts`.
#[must_use]
pub fn report(facts: &Facts<'_>) -> Report {
    let version = version_line();
    let setup = format!(
        "model: {}; permission mode: {}",
        scrub(facts.model, facts.home),
        scrub(facts.permission_mode, facts.home)
    );
    let described = scrub(facts.description.trim(), facts.home);
    let title = crate::text::truncate(described.lines().next().unwrap_or("").trim(), 80);

    let mut url = Url::parse(NEW_ISSUE).expect("a constant address");
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("template", TEMPLATE)
            .append_pair("version", &version)
            .append_pair("setup", &setup);
        if !title.is_empty() {
            query.append_pair("title", &title);
        }
    }
    // What is left for `&description=…`, the cut's `…` included.
    let room =
        MAX_URL_BYTES.saturating_sub(url.as_str().len() + "&description=".len() + encoded_len('…'));
    let (description, cut) = fit(&described, room);
    if !description.is_empty() {
        url.query_pairs_mut()
            .append_pair("description", &description);
    }
    // In full: when the link's copy is cut, the rest is pasted from here.
    let shown = if described.is_empty() {
        "(none — add one after /bug, or in the issue)".to_string()
    } else {
        described
    };
    Report {
        text: format!("{version}\n{setup}\ndescription: {shown}"),
        url: url.into(),
        cut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(description: &str) -> Facts<'_> {
        Facts {
            model: "claude / opus",
            permission_mode: "workspace-write",
            description,
            home: Some(r"C:\Users\someone"),
        }
    }

    fn field(url: &str, name: &str) -> Option<String> {
        Url::parse(url)
            .expect("a URL")
            .query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    }

    #[test]
    fn the_link_opens_the_bug_form_with_the_facts_filled_in() {
        let report = report(&facts("the REPL froze after /compact\nsteps: …"));
        assert!(report.url.starts_with(NEW_ISSUE), "{}", report.url);
        assert_eq!(field(&report.url, "template").as_deref(), Some(TEMPLATE));
        let version = field(&report.url, "version").expect("version");
        assert!(version.starts_with("incode ") && version.contains(std::env::consts::OS));
        assert_eq!(
            field(&report.url, "setup").as_deref(),
            Some("model: claude / opus; permission mode: workspace-write")
        );
        assert_eq!(
            field(&report.url, "title").as_deref(),
            Some("the REPL froze after /compact")
        );
        assert!(report.text.contains(&version));
        assert!(!report.cut);
    }

    /// **A BUG REPORT IS PUBLIC.** A token pasted into the description, or a
    /// path under the home folder, never reaches the link or the text. The
    /// token-shaped values are assembled here so no real-looking secret sits
    /// in the source.
    #[test]
    fn secrets_and_the_home_folder_are_scrubbed_before_anything_is_shown() {
        let openai = format!("{}-proj-{}", "sk", "0123456789abcdefghijklmnop");
        let github = format!("{}_{}", "ghp", "0123456789abcdefghijklmnopqrstuvwxyz");
        let typed = format!(
            "failed with Authorization: Bearer abc.def.ghi\n\
             OPENAI_API_KEY={openai}\n\
             token {github} in C:\\Users\\someone\\proj and c:/users/someone/x"
        );
        let report = report(&facts(&typed));
        let description = field(&report.url, "description").expect("description");
        for leaked in ["abc.def.ghi", openai.as_str(), github.as_str(), "someone"] {
            assert!(!description.contains(leaked), "{leaked} in {description}");
            assert!(!report.text.contains(leaked), "{leaked} in {}", report.text);
            assert!(!report.url.contains(leaked), "{leaked} in the link");
        }
        assert!(description.contains(r"~\proj"), "{description}");
        assert!(description.contains("~/x"), "{description}");
    }

    /// **THE LINK'S SIZE IS BOUNDED, NOT THE CHARACTER COUNT.** ASCII, accents
    /// and emoji encode to 1, 6 and 12 bytes; each long description is cut so
    /// the whole link fits, and what is kept is the start of it.
    #[test]
    fn a_long_description_is_cut_to_keep_the_link_openable() {
        for unit in ["a", "é", "🦀", "a b%&"] {
            // Past the budget whatever the encoding: plain `a` is one byte.
            let long = unit.repeat(MAX_URL_BYTES + 1);
            let report = report(&facts(&long));
            assert!(report.cut, "{unit}");
            assert!(
                report.url.len() <= MAX_URL_BYTES,
                "{unit}: {} bytes",
                report.url.len()
            );
            let description = field(&report.url, "description").expect("description");
            let kept = description.strip_suffix('…').expect("marked as cut");
            assert!(long.starts_with(kept), "{unit}");
            assert!(kept.len() > 500, "{unit}: kept only {} bytes", kept.len());
        }
        let short = report(&facts("a short one"));
        assert!(!short.cut);
        assert_eq!(
            field(&short.url, "description").as_deref(),
            Some("a short one")
        );
    }

    #[test]
    fn with_nothing_typed_the_link_has_no_title_or_description() {
        let report = report(&facts("   "));
        assert_eq!(field(&report.url, "title"), None);
        assert_eq!(field(&report.url, "description"), None);
        assert!(report.text.contains("(none"));
    }

    /// The link fills the form's fields by id: every id it sends is one the
    /// form has, so GitHub drops nothing.
    #[test]
    fn every_field_in_the_link_is_one_the_issue_form_has() {
        let form = include_str!("../.github/ISSUE_TEMPLATE/bug.yml");
        let report = report(&facts("something broke"));
        for (name, _) in Url::parse(&report.url).expect("a URL").query_pairs() {
            if name == "template" || name == "title" {
                continue;
            }
            assert!(
                form.contains(&format!("id: {name}")),
                "the form has no field `{name}`"
            );
        }
    }
}
