//! `incode update`: replace this program with the latest release.
//!
//! # What it trusts
//!
//! The release's own `SHA256SUMS`, published beside the archives by
//! `.github/workflows/release.yml`. The archive is hashed as it downloads and
//! compared before anything is unpacked; a release with no `SHA256SUMS`, or no
//! line in it for this platform's archive, is refused rather than installed
//! unchecked. Fetching goes through [`crate::ext_tools::fetch`], so the same
//! rules hold as for an external tool: https only, bounded in time and size,
//! and nothing about the operator sent.
//!
//! # How the program is replaced
//!
//! The archive is unpacked into a folder BESIDE the running program, so the
//! final step is a rename within one filesystem and a crash can never leave a
//! half-written program behind.
//!
//! - Unix: the new file is renamed over the old one. The running process
//!   keeps the old inode until it exits.
//! - Windows: a running `.exe` cannot be overwritten, but it can be renamed.
//!   The old one becomes `incode.exe.old`, the new one moves in, and the
//!   `.old` is removed by the next update (it is still in use by this one).
//!
//! `sbx`, the standalone sandbox, ships in the same archive. It is replaced
//! too when it sits beside `incode` — where the installers put it — and left
//! alone otherwise.
//!
//! # Installed by a package manager
//!
//! Homebrew, Scoop and winget keep track of the files they install. Replaced
//! underneath them, the manager would still list the old version and its next
//! upgrade would undo the update, so a program one of them installed is left
//! as it is and the manager's own upgrade command is named instead
//! ([`package_manager`]).

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::ext_tools::{fetch, unpack};

/// Where releases are published.
pub const REPO: &str = "incordai/incode";
/// The checksum file every release carries.
const SUMS_ASSET: &str = "SHA256SUMS";
/// How long the release lookup and the checksum file may take.
const API_BOUND: Duration = Duration::from_secs(30);
/// The most of a release description read.
const MAX_API_BYTES: usize = 4 * 1024 * 1024;
/// The most of `SHA256SUMS` read: a line per platform, a few hundred bytes.
const MAX_SUMS_BYTES: usize = 64 * 1024;

/// The winget package. The same identifier is in
/// `scripts/package_manifests.py` (`WINGET_ID`), which writes the manifest.
const WINGET_ID: &str = "IncordAI.Incode";

/// A package manager that owns this program's files.
///
/// Replacing them under it would leave the manager believing the old version
/// is installed, and its next upgrade would undo the update. So `incode
/// update` names the manager's own command instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Homebrew,
    Scoop,
    Winget,
}

impl Manager {
    fn name(self) -> &'static str {
        match self {
            Self::Homebrew => "Homebrew",
            Self::Scoop => "Scoop",
            Self::Winget => "winget",
        }
    }

    fn upgrade_command(self) -> String {
        match self {
            Self::Homebrew => "brew upgrade incode".to_string(),
            Self::Scoop => "scoop update incode".to_string(),
            Self::Winget => format!("winget upgrade {WINGET_ID}"),
        }
    }
}

/// Which package manager installed the program at `exe` (its resolved path),
/// if one did, by where each puts it — as `scripts/package_manifests.py`'s
/// manifests install it, measured by `.github/workflows/package-check.yml`:
///
/// - Homebrew: `<prefix>/Cellar/incode/<version>/bin/incode` (`bin` links
///   there, and the path is resolved).
/// - Scoop: `<root>\apps\incode\<version or current>\incode.exe`, the root
///   being `scoop` under the profile or `ProgramData` unless `SCOOP` moved it.
/// - winget: `<...>\WinGet\Packages\IncordAI.Incode_<source>\incode.exe`
///   (`WinGet\Links` holds a link to it).
///
/// Read as text, split on either separator and without regard to case, so the
/// same rules hold for a path from any platform.
///
/// Called by [`run`].
#[must_use]
pub fn package_manager(exe: &Path) -> Option<Manager> {
    let text = exe.to_string_lossy().to_lowercase();
    let parts: Vec<&str> = text.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    let after = |i: usize, name: &str| parts.get(i + 1).is_some_and(|p| *p == name);
    for (i, part) in parts.iter().enumerate() {
        if *part == "cellar" && after(i, "incode") {
            return Some(Manager::Homebrew);
        }
        if *part == "apps"
            && after(i, "incode")
            && i > 0
            && (parts[i - 1] == "scoop" || scoop_root(&parts[..i]))
        {
            return Some(Manager::Scoop);
        }
        if *part == "winget"
            && after(i, "packages")
            && parts
                .get(i + 2)
                .is_some_and(|p| p.starts_with(&WINGET_ID.to_lowercase()))
        {
            return Some(Manager::Winget);
        }
    }
    None
}

/// Whether `parts` is the Scoop root a `SCOOP` or `SCOOP_GLOBAL` variable
/// names, for a Scoop moved away from its default folder.
fn scoop_root(parts: &[&str]) -> bool {
    ["SCOOP", "SCOOP_GLOBAL"].iter().any(|var| {
        std::env::var(var).is_ok_and(|root| {
            let root = root.to_lowercase();
            let root: Vec<&str> = root.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
            !root.is_empty() && root == parts
        })
    })
}

/// The Rust target this program was built for, as release archives name it,
/// or `None` on a platform no release is built for.
pub fn target_triple() -> Option<&'static str> {
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        return None;
    };
    match (arch, std::env::consts::OS) {
        ("x86_64", "linux") => Some("x86_64-unknown-linux-gnu"),
        ("aarch64", "linux") => Some("aarch64-unknown-linux-gnu"),
        ("x86_64", "macos") => Some("x86_64-apple-darwin"),
        ("aarch64", "macos") => Some("aarch64-apple-darwin"),
        ("x86_64", "windows") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

/// The archive a release ships for `triple`: `.zip` for Windows, `.tar.gz`
/// for the rest.
pub(crate) fn asset_name(triple: &str) -> String {
    let ext = if triple.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("incode-{triple}.{ext}")
}

/// A release version: `major.minor.patch`, an optional `-pre.release`, and
/// build metadata (`+...`), which is ignored. A leading `v` is allowed, since
/// tags carry one and `CARGO_PKG_VERSION` does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Version {
    core: [u64; 3],
    pre: Vec<String>,
}

impl Version {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let text = text.split('+').next().unwrap_or_default();
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut numbers = core.split('.').map(|n| n.parse::<u64>().ok());
        let parsed = [numbers.next()??, numbers.next()??, numbers.next()??];
        if numbers.next().is_some() {
            return None;
        }
        let pre = match pre {
            Some(pre) if pre.is_empty() || pre.split('.').any(str::is_empty) => return None,
            Some(pre) => pre.split('.').map(str::to_string).collect(),
            None => Vec::new(),
        };
        Some(Self { core: parsed, pre })
    }
}

impl Ord for Version {
    /// Semver precedence: the numbers first; then a pre-release sorts BEFORE
    /// the release it leads to, and two pre-releases compare part by part,
    /// numbers numerically and below words.
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(a), Ok(b)) => a.cmp(&b),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if order != Ordering::Equal {
                            return order;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The sha256 `sums` gives for `asset`, lowercase, or `None` when it names
/// none. `sums` is `sha256sum` output: `<hex>  <name>`, or `<hex> *<name>`
/// from binary mode. A line whose digest is not 64 hex digits is skipped.
pub(crate) fn checksum_for(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (digest, name) = line.trim().split_once(char::is_whitespace)?;
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        let well_formed = digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit());
        (name == asset && well_formed).then(|| digest.to_ascii_lowercase())
    })
}

/// What the latest release says about itself.
struct Release {
    tag: String,
    /// Each asset's name, download address and declared size.
    assets: Vec<(String, String, Option<u64>)>,
}

impl Release {
    fn asset(&self, name: &str) -> Option<&(String, String, Option<u64>)> {
        self.assets.iter().find(|(n, _, _)| n == name)
    }
}

fn latest_release() -> Result<Release, String> {
    let url = format!("{}/repos/{REPO}/releases/latest", fetch::GITHUB_API);
    let body = fetch::get_bounded(
        &url,
        "application/vnd.github+json",
        API_BOUND,
        MAX_API_BYTES,
    )
    .map_err(|why| {
        // GitHub answers 404 for a repository with no (non-pre-)release yet.
        if why.contains("404") {
            format!("{REPO} has no published release yet ({why})")
        } else {
            format!("the latest release of {REPO} could not be read: {why}")
        }
    })?;
    let json: Value = serde_json::from_slice(&body)
        .map_err(|e| format!("the latest release of {REPO} did not parse: {e}"))?;
    let tag = json
        .get("tag_name")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("the latest release of {REPO} names no tag"))?
        .to_string();
    let assets = json
        .get("assets")
        .and_then(Value::as_array)
        .map(|assets| {
            assets
                .iter()
                .filter_map(|a| {
                    Some((
                        a.get("name")?.as_str()?.to_string(),
                        a.get("browser_download_url")?.as_str()?.to_string(),
                        a.get("size").and_then(Value::as_u64),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release { tag, assets })
}

/// What `incode update` does, once the latest release is known.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Nothing to do, or `--check`: say this, change nothing.
    Say(String),
    /// Stop without changing anything, for this reason.
    Refuse(String),
    /// Download and put the release in place.
    Install,
}

/// The decision, apart from the network and the files: this program's
/// version, the latest release's tag, the package manager that installed it
/// (if any), and whether only to check.
///
/// Called by [`run`].
fn next_step(current: &str, tag: &str, managed: Option<Manager>, check_only: bool) -> Step {
    let (Some(have), Some(latest)) = (Version::parse(current), Version::parse(tag)) else {
        return Step::Refuse(format!(
            "the latest release is tagged `{tag}`, which is not a version incode can compare \
             with its own {current}"
        ));
    };
    if latest <= have {
        return Step::Say(format!("incode {current} is up to date (latest: {tag})"));
    }
    if check_only {
        let how = managed.map_or_else(
            || "`incode update`".to_string(),
            |m| format!("`{}`", m.upgrade_command()),
        );
        return Step::Say(format!(
            "incode {tag} is available (this is {current}); run {how} to install it"
        ));
    }
    match managed {
        Some(manager) => Step::Refuse(format!(
            "incode was installed by {}, which keeps track of its files; run `{}` to update \
             it to {tag}. Nothing was changed",
            manager.name(),
            manager.upgrade_command()
        )),
        None => Step::Install,
    }
}

/// Check for, and unless `check_only` install, the latest release. `Ok` is
/// what to tell the operator; `Err` says what stopped it, and that the
/// program in place was left as it was.
pub fn run(check_only: bool) -> Result<String, String> {
    let current = env!("CARGO_PKG_VERSION");
    let release = latest_release()?;
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| format!("could not find this program's own file: {e}"));
    let managed = exe.as_deref().ok().and_then(package_manager);
    match next_step(current, &release.tag, managed, check_only) {
        Step::Say(said) => return Ok(said),
        Step::Refuse(why) => return Err(why),
        Step::Install => {}
    }
    let triple = target_triple()
        .ok_or_else(|| "no release is built for this platform; build from source".to_string())?;
    let exe = exe?;
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no folder", exe.display()))?
        .to_path_buf();
    // Unpacked beside the program, so the swap is a rename on one filesystem.
    let staging = dir.join(format!(".incode-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir(&staging).map_err(|e| {
        format!(
            "could not write in {} ({e}); re-run with permission to write there, or \
             reinstall with the install script",
            dir.display()
        )
    })?;
    let outcome = install(&release, triple, &exe, &dir, &staging);
    let _ = std::fs::remove_dir_all(&staging);
    outcome.map(|replaced| {
        format!(
            "incode updated {current} -> {} ({}, sha256 checked): {}",
            release.tag,
            asset_name(triple),
            replaced
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// Download, check and unpack the archive for `triple` into `staging`, then
/// put its programs in place. Returns the files replaced.
fn install(
    release: &Release,
    triple: &str,
    exe: &Path,
    dir: &Path,
    staging: &Path,
) -> Result<Vec<PathBuf>, String> {
    let wanted = asset_name(triple);
    let (_, url, size) = release
        .asset(&wanted)
        .ok_or_else(|| format!("release {} has no `{wanted}`", release.tag))?;
    let (_, sums_url, _) = release.asset(SUMS_ASSET).ok_or_else(|| {
        format!(
            "release {} has no {SUMS_ASSET}, so `{wanted}` cannot be checked; nothing was \
             installed",
            release.tag
        )
    })?;
    let sums = fetch::get_bounded(
        sums_url,
        "application/octet-stream",
        API_BOUND,
        MAX_SUMS_BYTES,
    )
    .map_err(|why| format!("{SUMS_ASSET} could not be read: {why}"))?;
    let expected = checksum_for(&String::from_utf8_lossy(&sums), &wanted)
        .ok_or_else(|| format!("{SUMS_ASSET} of {} has no line for `{wanted}`", release.tag))?;

    let archive = staging.join(&wanted);
    let got = fetch::download(url, *size, &archive)?;
    if got != expected {
        return Err(format!(
            "refused — {wanted} has sha256 {got}, not the {expected} {SUMS_ASSET} gives; \
             nothing was installed"
        ));
    }
    let unpacked = staging.join("unpacked");
    unpack::unpack(&archive, &wanted, &unpacked)?;

    let suffix = std::env::consts::EXE_SUFFIX;
    let new_incode = unpacked.join(format!("incode{suffix}"));
    if !new_incode.is_file() {
        return Err(format!(
            "{wanted} holds no incode{suffix}; nothing was installed"
        ));
    }
    replace(&new_incode, exe)?;
    let mut replaced = vec![exe.to_path_buf()];
    // sbx only where it already is: an install that never had it gets none.
    let new_sbx = unpacked.join(format!("sbx{suffix}"));
    let sbx = dir.join(format!("sbx{suffix}"));
    if new_sbx.is_file() && sbx.is_file() {
        match replace(&new_sbx, &sbx) {
            Ok(()) => replaced.push(sbx),
            Err(why) => return Err(format!("incode was updated, but sbx was not: {why}")),
        }
    }
    Ok(replaced)
}

/// Put `new` where `target` is, in one rename. `new` must be on the same
/// filesystem (it is: the staging folder sits beside the program).
fn replace(new: &Path, target: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(new, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("could not mark {} executable: {e}", new.display()))?;
        std::fs::rename(new, target)
            .map_err(|e| format!("could not replace {}: {e}", target.display()))
    }
    #[cfg(windows)]
    {
        let mut old = target.as_os_str().to_owned();
        old.push(".old");
        let old = PathBuf::from(old);
        // Left by the previous update, whose program has since exited.
        let _ = std::fs::remove_file(&old);
        std::fs::rename(target, &old)
            .map_err(|e| format!("could not move {} aside: {e}", target.display()))?;
        if let Err(e) = std::fs::rename(new, target) {
            // Put the old one back, so a failed update leaves a working program.
            let _ = std::fs::rename(&old, target);
            return Err(format!("could not replace {}: {e}", target.display()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("`{text}` should parse"))
    }

    /// **A package manager's install is recognised from where it puts
    /// incode**, on paths from every platform, so `incode update` names its
    /// command instead of replacing files it keeps track of.
    #[test]
    fn a_package_managed_install_is_recognised_by_its_folder() {
        for (path, want) in [
            (
                "/opt/homebrew/Cellar/incode/0.1.0/bin/incode",
                Manager::Homebrew,
            ),
            (
                "/usr/local/Cellar/incode/0.1.0/bin/incode",
                Manager::Homebrew,
            ),
            (
                "/home/linuxbrew/.linuxbrew/Cellar/incode/0.1.0/bin/incode",
                Manager::Homebrew,
            ),
            (
                r"C:\Users\me\scoop\apps\incode\0.1.0\incode.exe",
                Manager::Scoop,
            ),
            (
                r"C:\ProgramData\scoop\apps\incode\current\incode.exe",
                Manager::Scoop,
            ),
            (
                r"\\?\C:\Users\me\AppData\Local\Microsoft\WinGet\Packages\IncordAI.Incode_Microsoft.Winget.Source_8wekyb3d8bbwe\incode.exe",
                Manager::Winget,
            ),
            (
                r"C:\Program Files\WinGet\Packages\IncordAI.Incode_Microsoft.Winget.Source_8wekyb3d8bbwe\incode.exe",
                Manager::Winget,
            ),
        ] {
            assert_eq!(package_manager(Path::new(path)), Some(want), "{path}");
        }
    }

    /// **Everything else updates itself**: the installer scripts' folder,
    /// `cargo install`, a build, and paths that only look alike.
    #[test]
    fn an_install_no_manager_owns_is_left_to_update_itself() {
        for path in [
            r"C:\Users\me\AppData\Local\incode\bin\incode.exe",
            "/home/me/.local/bin/incode",
            "/home/me/.cargo/bin/incode",
            r"D:\Projects\incord-code\target\release\incode.exe",
            "/opt/homebrew/Cellar/other/1.0/bin/incode",
            r"D:\work\apps\incode\incode.exe",
            r"C:\Users\me\AppData\Local\Microsoft\WinGet\Packages\Other.Tool_x\incode.exe",
        ] {
            assert_eq!(package_manager(Path::new(path)), None, "{path}");
        }
    }

    /// **A newer release over a package-managed install is refused with the
    /// manager's command**, and nothing is installed; `--check` names that
    /// command too. Without a manager the update goes ahead as before, and an
    /// up-to-date program is told so whoever installed it.
    #[test]
    fn a_managed_install_is_pointed_at_its_manager_and_left_alone() {
        let refused = next_step("0.1.0", "v0.2.0", Some(Manager::Homebrew), false);
        assert_eq!(
            refused,
            Step::Refuse(
                "incode was installed by Homebrew, which keeps track of its files; run \
                 `brew upgrade incode` to update it to v0.2.0. Nothing was changed"
                    .to_string()
            )
        );
        assert_eq!(
            next_step("0.1.0", "v0.2.0", Some(Manager::Winget), true),
            Step::Say(
                "incode v0.2.0 is available (this is 0.1.0); run `winget upgrade \
                 IncordAI.Incode` to install it"
                    .to_string()
            )
        );
        assert_eq!(next_step("0.1.0", "v0.2.0", None, false), Step::Install);
        assert_eq!(
            next_step("0.1.0", "v0.2.0", None, true),
            Step::Say(
                "incode v0.2.0 is available (this is 0.1.0); run `incode update` to install it"
                    .to_string()
            )
        );
        assert_eq!(
            next_step("0.2.0", "v0.2.0", Some(Manager::Scoop), false),
            Step::Say("incode 0.2.0 is up to date (latest: v0.2.0)".to_string())
        );
        assert!(matches!(
            next_step("0.1.0", "latest", None, false),
            Step::Refuse(_)
        ));
    }

    /// The command each manager is told to use.
    #[test]
    fn each_manager_names_its_own_upgrade_command() {
        assert_eq!(Manager::Homebrew.upgrade_command(), "brew upgrade incode");
        assert_eq!(Manager::Scoop.upgrade_command(), "scoop update incode");
        assert_eq!(
            Manager::Winget.upgrade_command(),
            "winget upgrade IncordAI.Incode"
        );
    }

    #[test]
    fn versions_parse_with_or_without_a_v() {
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert_eq!(v("1.2.3+build.7"), v("1.2.3"), "build metadata is ignored");
        for bad in [
            "",
            "v",
            "1.2",
            "1.2.3.4",
            "1.x.3",
            "1.2.3-",
            "1.2.3-a..b",
            "latest",
        ] {
            assert_eq!(Version::parse(bad), None, "`{bad}` must not parse");
        }
    }

    #[test]
    fn versions_order_by_semver_precedence() {
        assert!(v("0.1.10") > v("0.1.9"), "numerically, not as text");
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(
            v("1.0.0") > v("1.0.0-rc.1"),
            "a pre-release precedes its release"
        );
        assert!(v("1.0.0-rc.2") > v("1.0.0-rc.1"));
        assert!(v("1.0.0-rc.10") > v("1.0.0-rc.2"));
        assert!(v("1.0.0-beta") > v("1.0.0-alpha"));
        assert!(v("1.0.0-alpha.1") > v("1.0.0-alpha"));
        assert!(v("1.0.0-alpha") > v("1.0.0-1"), "numbers sort below words");
        assert_eq!(v("v2.0.0").cmp(&v("2.0.0")), Ordering::Equal);
    }

    #[test]
    fn this_build_has_a_comparable_version() {
        assert!(Version::parse(env!("CARGO_PKG_VERSION")).is_some());
    }

    /// **sbx SHIPS UNDER incode's VERSION.** One release tag carries both
    /// programs, and release.yml refuses a tag that either crate's version
    /// disagrees with; this catches the drift on a pull request instead of at
    /// release time.
    #[test]
    fn sbx_is_versioned_with_incode() {
        let sandbox = include_str!("../crates/sandbox/Cargo.toml");
        let version = sandbox
            .lines()
            .find_map(|line| line.trim().strip_prefix("version = \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("crates/sandbox/Cargo.toml names a version");
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn asset_names_match_the_release_workflow() {
        assert_eq!(
            asset_name("x86_64-unknown-linux-gnu"),
            "incode-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_name("aarch64-apple-darwin"),
            "incode-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            asset_name("x86_64-pc-windows-msvc"),
            "incode-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn this_platform_names_a_built_target() {
        // Every platform CI runs on is one the release builds for.
        let triple = target_triple().expect("a release target");
        assert!(triple.starts_with(std::env::consts::ARCH));
    }

    #[test]
    fn checksums_are_found_by_exact_name() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!(
            "{a}  incode-x86_64-unknown-linux-gnu.tar.gz\n\
             {b} *incode-x86_64-pc-windows-msvc.zip\n\
             nothex  incode-aarch64-apple-darwin.tar.gz\n"
        );
        assert_eq!(
            checksum_for(&sums, "incode-x86_64-unknown-linux-gnu.tar.gz"),
            Some(a)
        );
        assert_eq!(
            checksum_for(&sums, "incode-x86_64-pc-windows-msvc.zip"),
            Some(b.to_ascii_lowercase()),
            "binary-mode `*` and uppercase hex"
        );
        assert_eq!(
            checksum_for(&sums, "incode-aarch64-apple-darwin.tar.gz"),
            None,
            "a malformed digest is not a checksum"
        );
        assert_eq!(checksum_for(&sums, "incode-x86_64-unknown-linux-gnu"), None);
        assert_eq!(checksum_for("", "anything"), None);
    }
}
