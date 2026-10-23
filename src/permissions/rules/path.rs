//! Path rules: a `Read(...)` or `Edit(...)` specifier compiled into a
//! gitignore-style glob, anchored at a name, the workspace, or the root, and
//! matched against the path a call names.

use std::path::{Component, Path, PathBuf};

use super::PathContext;

#[derive(Debug, Clone)]
pub(super) struct PathPattern {
    matcher: globset::GlobMatcher,
    anchor: Anchor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Anchor {
    /// No `/`: a file name, at any depth, anywhere.
    Name,
    /// From the workspace root.
    Workspace,
    /// An absolute path.
    Absolute,
}

impl PathPattern {
    /// Called by [`super::Rule::parse`] for a file tool's specifier.
    pub(super) fn parse(body: &str, home: Option<&Path>) -> Result<Self, String> {
        let mut pattern = body.replace('\\', "/");
        let anchor = if let Some(rest) = pattern.strip_prefix("~/") {
            let home = home.ok_or_else(|| format!("`{body}`: no home directory to expand"))?;
            pattern = format!(
                "{}/{rest}",
                home.to_string_lossy()
                    .replace('\\', "/")
                    .trim_end_matches('/')
            );
            Anchor::Absolute
        } else if let Some(rest) = pattern.strip_prefix("//") {
            pattern = format!("/{rest}");
            Anchor::Absolute
        } else if is_windows_absolute(&pattern) {
            Anchor::Absolute
        } else {
            // `/x` and `./x` are anchored at the workspace root; so is any
            // pattern with a `/` inside it.
            let rooted = pattern.starts_with('/') || pattern.starts_with("./");
            let rest = pattern
                .trim_start_matches("./")
                .trim_start_matches('/')
                .to_string();
            pattern = rest;
            if rooted || pattern.trim_end_matches('/').contains('/') {
                Anchor::Workspace
            } else {
                // A name: what is inside a directory of that name is matched
                // through its parents, so the trailing `/` adds nothing.
                pattern = pattern.trim_end_matches('/').to_string();
                Anchor::Name
            }
        };
        if pattern.ends_with('/') {
            pattern.push_str("**");
        }
        if pattern.is_empty() {
            return Err("an empty path pattern".to_string());
        }
        let glob = globset::GlobBuilder::new(&pattern)
            .literal_separator(true)
            .case_insensitive(cfg!(any(windows, target_os = "macos")))
            .build()
            .map_err(|e| format!("`{body}` is not a valid path pattern: {e}"))?;
        Ok(Self {
            matcher: glob.compile_matcher(),
            anchor,
        })
    }

    /// Whether `path`, or a directory it is in, matches.
    ///
    /// Called by the rule set's `restricts` and `permits`
    /// (`permissions::rules::set`).
    pub(super) fn matches(&self, path: &str, ctx: &PathContext) -> bool {
        let absolute = normalise(&ctx.workspace, path);
        let workspace = normalise(&ctx.workspace, "");
        let mut current: Option<&Path> = Some(&absolute);
        while let Some(at) = current {
            let hit = match self.anchor {
                Anchor::Name => at
                    .file_name()
                    .is_some_and(|name| self.matcher.is_match(Path::new(name))),
                Anchor::Workspace => at
                    .strip_prefix(&workspace)
                    .ok()
                    .filter(|rel| !rel.as_os_str().is_empty())
                    .is_some_and(|rel| self.matcher.is_match(slashed(rel))),
                Anchor::Absolute => self.matcher.is_match(slashed(at)),
            };
            if hit {
                return true;
            }
            current = at.parent();
        }
        false
    }
}

fn is_windows_absolute(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

fn slashed(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().replace('\\', "/"))
}

/// `path` made absolute against `base`, with `.` and `..` resolved by text.
#[must_use]
pub fn normalise(base: &Path, path: &str) -> PathBuf {
    let joined = if path.is_empty() {
        base.to_path_buf()
    } else if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{normalise, PathPattern};
    use crate::permissions::rules::PathContext;

    #[test]
    fn dot_and_dot_dot_are_resolved_by_text() {
        assert_eq!(
            normalise(Path::new("/ws"), "./src/../.env"),
            PathBuf::from("/ws/.env")
        );
        assert_eq!(normalise(Path::new("/ws"), ""), PathBuf::from("/ws"));
    }

    /// A name matches at any depth; a pattern with a `/` from the workspace.
    #[test]
    fn a_pattern_is_anchored_by_its_shape() {
        let ctx = PathContext {
            workspace: PathBuf::from("/ws"),
            home: None,
        };
        let name = PathPattern::parse(".env", None).expect("parses");
        assert!(name.matches("app/config/.env", &ctx));
        let rooted = PathPattern::parse("src/**", None).expect("parses");
        assert!(rooted.matches("src/a/b.rs", &ctx));
        assert!(!rooted.matches("lib/src/x.rs", &ctx));
        assert!(
            PathPattern::parse("~/x", None).is_err(),
            "no home to expand"
        );
    }
}
