//! Git worktree management — create, list, claim, and cleanup isolated work branches.
//!
//! Inspired by emdash's WorktreePoolService: pre-create reserve worktrees for
//! instant task startup, claim them when tasks begin, clean up when done.

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// A managed git worktree.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub name: String,
    pub branch: String,
    pub path: PathBuf,
    pub created_at: u64,
}

/// What [`WorktreeManager::remove`] did with the branch behind a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub branch: String,
    /// `None` when the branch was deleted; otherwise why it was kept.
    pub branch_kept: Option<String>,
}

/// Whether two paths name the same place on disk.
///
/// Compared canonically when both exist, so separator style, `.`/`..` and
/// case on case-insensitive filesystems do not decide it.
fn same_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Worktree manager — create, list, and manage git worktrees.
pub struct WorktreeManager {
    /// The main repository root.
    repo_root: PathBuf,
    /// Directory where worktrees are stored (repo_root/../worktrees/).
    worktree_dir: PathBuf,
    /// Branch prefix (default: "incode").
    branch_prefix: String,
}

impl WorktreeManager {
    /// Create a new worktree manager for the given repo.
    #[must_use]
    pub fn new(repo_root: PathBuf) -> Self {
        let worktree_dir = repo_root.parent().unwrap_or(&repo_root).join("worktrees");
        Self {
            repo_root,
            worktree_dir,
            branch_prefix: "incode".to_string(),
        }
    }

    /// Set the branch prefix (default: "incode").
    #[must_use]
    pub fn with_branch_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.branch_prefix = prefix.into();
        self
    }

    /// Create a new worktree for a task.
    ///
    /// Creates `worktrees/<slug>-<hash>/` with branch `incode/<slug>-<hash>`.
    pub fn create(&self, task_name: &str, base_ref: Option<&str>) -> Result<Worktree, String> {
        // Ensure worktree directory exists
        std::fs::create_dir_all(&self.worktree_dir)
            .map_err(|e| format!("create worktree dir: {e}"))?;

        let slug = slugify(task_name);
        let hash = short_hash();
        let name = format!("{slug}-{hash}");
        let branch = format!("{}/{name}", self.branch_prefix);
        let wt_path = self.worktree_dir.join(&name);
        let base = base_ref.unwrap_or("HEAD");

        // git worktree add -b <branch> <path> <base>
        let output = Command::new("git")
            .args(["worktree", "add", "-b", &branch])
            .arg(&wt_path)
            .arg(base)
            .current_dir(&self.repo_root)
            .output()
            .map_err(|e| format!("spawn git: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git worktree add failed: {stderr}"));
        }

        Ok(Worktree {
            name,
            branch,
            path: wt_path,
            created_at: unix_now(),
        })
    }
}

/// Parse `git worktree list --porcelain` output into a list of Worktrees.
/// Supports both branch checkouts and detached HEAD worktrees.
pub fn parse_worktree_list(stdout: &str) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in stdout.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path));
            current_branch = None;
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            current_branch = Some(branch.to_string());
        } else if line.trim() == "detached" {
            current_branch = Some("(detached)".to_string());
        } else if line.is_empty() {
            if let Some(path) = current_path.take() {
                let branch = current_branch
                    .take()
                    .unwrap_or_else(|| "(detached)".to_string());
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                worktrees.push(Worktree {
                    name,
                    branch,
                    path,
                    created_at: 0,
                });
            }
            current_path = None;
            current_branch = None;
        }
    }
    // Handle last entry (might not have trailing blank line)
    if let Some(path) = current_path {
        let branch = current_branch.unwrap_or_else(|| "(detached)".to_string());
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        worktrees.push(Worktree {
            name,
            branch,
            path,
            created_at: 0,
        });
    }

    worktrees
}

impl WorktreeManager {
    /// List all worktrees (parsed from `git worktree list`).
    pub fn list(&self) -> Result<Vec<Worktree>, String> {
        let output = Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&self.repo_root)
            .output()
            .map_err(|e| format!("spawn git: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git worktree list failed: {stderr}"));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_worktree_list(&stdout))
    }

    /// List only worktrees managed by incode (matching our branch prefix).
    pub fn list_managed(&self) -> Result<Vec<Worktree>, String> {
        let all = self.list()?;
        Ok(all
            .into_iter()
            .filter(|wt| wt.branch.starts_with(&format!("{}/", self.branch_prefix)))
            .collect())
    }

    /// Remove a worktree by name (or path), and its branch only if nothing is
    /// lost by doing so.
    ///
    /// # Nothing here destroys work
    ///
    /// This used to run `git worktree remove --force` and then `git branch -D`
    /// unconditionally. The first wiped uncommitted work in the checkout; the
    /// second deleted the branch that [`crate::parallel`] had just promised the
    /// operator was keeping a conflicted lane's work. Now:
    ///
    /// - the checkout is removed WITHOUT `--force`, so git refuses one that
    ///   still holds uncommitted changes, and that refusal is returned;
    /// - the branch is deleted only when [`Self::branch_is_landed`] proves its
    ///   work is already in `HEAD`. Otherwise it is kept and the returned
    ///   [`Removed::branch_kept`] says so.
    ///
    /// # Errors
    ///
    /// No such managed worktree, or git refused or failed to remove it.
    pub fn remove(&self, name_or_path: &str) -> Result<Removed, String> {
        let managed = self.list_managed()?;
        // Paths are compared as the filesystem sees them, not as strings: git
        // prints `D:/x/y` where a `PathBuf` built on Windows holds `D:\x\y`, so
        // a string match found nothing and every lane checkout was left behind.
        let wanted = std::path::Path::new(name_or_path);
        let wt = managed
            .iter()
            .find(|w| w.name == name_or_path || same_path(&w.path, wanted))
            .ok_or_else(|| format!("worktree '{name_or_path}' not found"))?;

        let branch = wt.branch.clone();
        let output = Command::new("git")
            .args(["worktree", "remove"])
            .arg(&wt.path)
            .current_dir(&self.repo_root)
            .output()
            .map_err(|e| format!("spawn git: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git worktree remove failed: {}", stderr.trim()));
        }

        let branch_kept = match self.branch_is_landed(&branch) {
            Ok(true) => {
                // `-d`, not `-D`: git makes the same check again and refuses
                // if it disagrees, so a mistake here cannot cost a commit.
                let deleted = Command::new("git")
                    .args(["branch", "-d", &branch])
                    .current_dir(&self.repo_root)
                    .output()
                    .map_err(|e| format!("spawn git: {e}"))?;
                if deleted.status.success() {
                    None
                } else {
                    Some(format!(
                        "git would not delete it: {}",
                        String::from_utf8_lossy(&deleted.stderr).trim()
                    ))
                }
            }
            Ok(false) => Some("it holds work that is not in HEAD".to_string()),
            Err(why) => Some(format!("could not tell whether its work landed: {why}")),
        };
        Ok(Removed {
            branch,
            branch_kept,
        })
    }

    /// Whether everything on `branch` is already in `HEAD`, proved by git:
    /// the branch is an ancestor of `HEAD` — merged, or never moved from its
    /// base. Then deleting it loses nothing.
    ///
    /// # Errors
    ///
    /// git could not answer. The caller must then keep the branch.
    pub fn branch_is_landed(&self, branch: &str) -> Result<bool, String> {
        // Answered by the exit code: 0 yes, 1 no, anything else is git
        // failing to answer at all — which must not be read as either.
        let ancestor = Command::new("git")
            .args(["merge-base", "--is-ancestor", branch, "HEAD"])
            .current_dir(&self.repo_root)
            .output()
            .map_err(|e| format!("spawn git: {e}"))?;
        match ancestor.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(String::from_utf8_lossy(&ancestor.stderr).trim().to_string()),
        }
    }

    /// Formatted list for display.
    pub fn display_list(&self) -> String {
        match self.list_managed() {
            Ok(worktrees) if !worktrees.is_empty() => {
                let mut lines = Vec::new();
                for (i, wt) in worktrees.iter().enumerate() {
                    lines.push(format!(
                        "  {:>3}. {:<30} branch: {} ({})",
                        i + 1,
                        wt.name,
                        wt.branch,
                        wt.path.display()
                    ));
                }
                lines.join("\n")
            }
            Ok(_) => "  No managed worktrees. Use /worktree create <name> to create one.".into(),
            Err(e) => format!("  Error listing worktrees: {e}"),
        }
    }
}

/// Slugify a task name: lowercase, alphanumeric + hyphens, max 30 chars.
fn slugify(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();

    // Collapse multiple hyphens and trim
    let mut result = String::new();
    let mut prev_hyphen = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_hyphen && !result.is_empty() {
                result.push(c);
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }

    // Trim trailing hyphen and cap length
    let trimmed = result.trim_end_matches('-');
    if trimmed.len() > 30 {
        crate::text::head(trimmed, 30)
            .trim_end_matches('-')
            .to_string()
    } else {
        trimmed.to_string()
    }
}

/// Width of the part of [`short_hash`] that tells processes apart. Fixed, so
/// the counter after it can never shift into it: six base-36 characters are
/// about 2.2 billion values, which keeps two runs sharing a `worktrees/`
/// directory — whose kept lane branches outlive the run that made them — from
/// meeting by chance, where four (1.7 million) gives a fleet of a hundred runs
/// a fraction of a percent.
const PROCESS_PART: usize = 6;

/// The counter is padded to two characters only so a run's names line up
/// for their first 1,296; past that it grows a character rather than
/// wrapping, because wrapping would repeat a name.
const COUNTER_PART: usize = 2;

/// A suffix that makes a worktree name unique.
///
/// # Within one process, unique by construction
///
/// The suffix is a fixed-width part chosen once per process, followed by
/// this process's own counter written out in full. Two calls in the same
/// process differ in the counter, and because the part before it is a fixed
/// width the counter cannot run into it — so no two names from one process
/// can be equal, however many threads ask in the same clock tick.
///
/// # Between processes, separated by chance, and it says so
///
/// The leading part is a hash of the process id and the clock at the first
/// call. Two runs sharing a `worktrees/` directory meet only if those hashes
/// agree to all [`PROCESS_PART`] characters.
///
/// # What this replaced
///
/// First `nanos % 46656`, which gave two lanes opened in the same tick the
/// same name. Then the counter, the pid and the clock were ADDED into one
/// seed and 30 bits of it printed: counter `c` at time `t` and counter `c+1`
/// at `t-31` produced the same seed, and 1 name in 1600 collided in roughly
/// two runs out of five of the concurrency test below — while the comment
/// here claimed repeats were impossible. A nanosecond clock meets that often;
/// Windows' 100 ns clock only when the counters are 100 apart, which is why
/// it passed where it was written. Adding the counter into the clock is what
/// lost it; it has to survive verbatim.
fn short_hash() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    static PROCESS: OnceLock<String> = OnceLock::new();

    let process = PROCESS.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let mixed = splitmix64((u64::from(std::process::id()) << 32) ^ nanos);
        base36(mixed % 36u64.pow(PROCESS_PART as u32), PROCESS_PART)
    });
    let counter = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{process}{}", base36(counter, COUNTER_PART))
}

/// SplitMix64's finaliser: neighbouring pids and clock readings land far
/// apart, so the few characters kept from it are not just the clock's low
/// digits, which two runs started together would share.
fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `value` in lowercase base 36, zero-padded to at least `width` characters.
/// Lowercase alphanumerics only, because the result goes into a branch name
/// and a directory name on case-insensitive filesystems.
fn base36(mut value: u64, width: usize) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    while out.len() < width {
        out.push(b'0');
    }
    out.iter().rev().map(|&b| b as char).collect()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Fix Login Bug"), "fix-login-bug");
        assert_eq!(slugify("add_feature_123"), "add-feature-123");
        assert_eq!(slugify("  spaces  "), "spaces");
    }

    #[test]
    fn slugify_max_length() {
        let long = "a".repeat(50);
        assert!(slugify(&long).len() <= 30);
    }

    /// A task name is typed by the user, so it can contain any character.
    /// `char::is_alphanumeric` keeps accented and CJK letters, so they reach
    /// the 30-character cap — which used to slice at byte 30 and panic with
    /// "byte index 30 is not a char boundary" the moment someone named a
    /// worktree in their own language.
    #[test]
    fn slugify_does_not_panic_on_a_long_non_ascii_name() {
        // 40 two-byte characters: byte 30 lands inside the 16th one.
        let accented = "é".repeat(40);
        let slug = slugify(&accented);
        assert!(
            slug.chars().count() <= 30,
            "the cap is in characters; got {} chars",
            slug.chars().count()
        );
        assert!(accented.starts_with(&slug), "the slug must be a prefix");

        // 3-byte characters, so no cut point is a multiple of the width.
        let cjk = "中".repeat(40);
        assert!(cjk.starts_with(&slugify(&cjk)));

        // Mixed widths, cut at every length around the cap.
        for n in 1..40 {
            let mixed = "aé中🙂".repeat(n);
            let slug = slugify(&mixed);
            assert!(
                std::str::from_utf8(slug.as_bytes()).is_ok(),
                "slug must stay valid UTF-8"
            );
        }
    }

    #[test]
    fn slugify_special_chars() {
        assert_eq!(slugify("feat: add OAuth2!"), "feat-add-oauth2");
        assert_eq!(slugify("bug/fix#123"), "bug-fix-123");
    }

    /// The suffix lands in a branch name and a directory name. It is at least
    /// the process part plus a padded counter, and longer once the counter
    /// outgrows its padding — never shorter, and never anything git or a
    /// case-insensitive filesystem would read differently.
    #[test]
    fn a_name_suffix_is_lowercase_alphanumeric_and_at_least_its_fixed_width() {
        let h = short_hash();
        assert!(
            h.len() >= PROCESS_PART + COUNTER_PART,
            "`{h}` is shorter than the process part plus the padded counter"
        );
        assert!(
            h.chars()
                .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()),
            "`{h}` must be lowercase base 36 to be safe in a branch and a path"
        );
    }

    /// The counter has to come through verbatim, at whatever length it
    /// reaches: a padded counter that was truncated, or a process part that
    /// was not fixed-width, would let the 1297th name repeat an earlier one.
    #[test]
    fn base36_pads_short_values_and_never_truncates_long_ones() {
        assert_eq!(base36(0, 2), "00", "zero still fills its width");
        assert_eq!(base36(35, 2), "0z");
        assert_eq!(base36(36, 2), "10");
        assert_eq!(
            base36(36 * 36, 2),
            "100",
            "a value wider than the padding must keep every digit"
        );
        assert_eq!(base36(u64::MAX, 1), "3w5e11264sgsf");
    }

    /// **TWO WORKTREES MADE BACK TO BACK MUST NOT SHARE A NAME.**
    ///
    /// The old suffix was `nanos % 46656`. A lane opens one per task in a
    /// tight loop, so two landed in the same clock tick and the second
    /// `git worktree add` failed on a path that already existed — an
    /// intermittent lane failure with no obvious cause.
    ///
    /// Asked from several threads at once, which is how lanes ask, and which
    /// is what defeats a name derived only from the clock: two threads read
    /// the same tick and produce the same string.
    #[test]
    fn names_asked_for_concurrently_are_all_different() {
        const THREADS: usize = 8;
        const EACH: usize = 200;

        let made: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| scope.spawn(|| (0..EACH).map(|_| short_hash()).collect::<Vec<_>>()))
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap_or_default())
                .collect()
        });

        let unique: std::collections::BTreeSet<&String> = made.iter().collect();
        assert_eq!(
            unique.len(),
            made.len(),
            "{} of {} names collided — a second `git worktree add` would fail on a \
             path that already exists",
            made.len() - unique.len(),
            made.len()
        );
    }

    #[test]
    fn worktree_manager_creation() {
        let mgr = WorktreeManager::new(PathBuf::from("/tmp/repo"));
        assert_eq!(mgr.repo_root, PathBuf::from("/tmp/repo"));
        assert_eq!(mgr.branch_prefix, "incode");
    }

    #[test]
    fn worktree_manager_custom_prefix() {
        let mgr = WorktreeManager::new(PathBuf::from("/tmp/repo")).with_branch_prefix("feature");
        assert_eq!(mgr.branch_prefix, "feature");
    }

    #[test]
    fn parse_worktree_list_handles_branches_and_detached_head() {
        let porcelain = "\
worktree /repos/project
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /repos/worktrees/incode-task1-abc
HEAD 2222222222222222222222222222222222222222
branch refs/heads/incode/task1-abc

worktree /repos/worktrees/detached-task2-def
HEAD 3333333333333333333333333333333333333333
detached
";
        let parsed = parse_worktree_list(porcelain);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].name, "project");
        assert_eq!(parsed[0].branch, "main");
        assert_eq!(parsed[1].name, "incode-task1-abc");
        assert_eq!(parsed[1].branch, "incode/task1-abc");
        assert_eq!(parsed[2].name, "detached-task2-def");
        assert_eq!(parsed[2].branch, "(detached)");
    }
}
