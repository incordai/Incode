//! Linux: Landlock for the filesystem and TCP, `setrlimit` for the limits.
//!
//! Landlock is the kernel's own unprivileged confinement: the helper process
//! adds its rules, calls `restrict_self`, then execs the command, and from
//! then on the command and every process it starts can touch only what the
//! rules allow. It cannot be widened again, and `no_new_privs` is set with it,
//! so a setuid binary cannot step outside either.
//!
//! What each kernel covers, by Landlock ABI:
//! - v1 (5.13): reads, writes, creating and removing files and directories.
//! - v2 (5.19): `refer`, so a file cannot be linked or renamed from a
//!   read-only tree into a writable one and then written there.
//! - v3 (6.2): `truncate`.
//! - v4 (6.7): TCP `bind` and `connect`, by port (not by address).
//! - v5 (6.10): device ioctls.
//!
//! UDP is not covered by any ABI, nor is any socket that is not TCP. Under
//! `network = "deny"` or `"proxy"` a seccomp filter (`seccomp.rs`) refuses
//! to create those sockets, so UDP, and with it DNS, is closed too. Under
//! `proxy` the command never needed DNS: the proxy resolves names itself.

use std::path::{Path, PathBuf};

use landlock::{
    Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, NetPort, PathBeneath, PathFd,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus, ABI,
};

use crate::policy::{FsMode, NetMode};
use crate::{EnterSpec, Status};

/// The newest Landlock ABI this crate knows; the kernel downgrades it.
const NEWEST: ABI = ABI::V5;

/// The highest Landlock ABI whose filesystem rights this kernel enforces in
/// full, or `None` when it has no Landlock at all.
#[must_use]
pub fn fs_abi() -> Option<ABI> {
    [ABI::V5, ABI::V3, ABI::V2, ABI::V1]
        .into_iter()
        .find(|abi| hard_ruleset_fs(*abi))
}

fn hard_ruleset_fs(abi: ABI) -> bool {
    landlock::Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .and_then(landlock::Ruleset::create)
        .is_ok()
}

/// Whether this kernel can confine TCP (Landlock ABI v4).
#[must_use]
pub fn net_supported() -> bool {
    landlock::Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessNet::from_all(ABI::V4))
        .and_then(landlock::Ruleset::create)
        .is_ok()
}

pub(crate) fn fs_status(mode: FsMode) -> Status {
    if mode == FsMode::Off {
        return Status::NotRequested;
    }
    match fs_abi() {
        None => Status::NotEnforced(
            "this kernel has no Landlock (Linux 5.13+ with landlock in the LSM list)".into(),
        ),
        Some(ABI::V1) => Status::Enforced(
            "Landlock ABI v1: writes confined, but a file can be hard-linked or renamed \
             out of a read-only tree (needs v2, Linux 5.19)"
                .into(),
        ),
        Some(abi) => Status::Enforced(format!("Landlock (filesystem ABI {})", abi_number(abi))),
    }
}

pub(crate) fn net_status(mode: NetMode) -> Status {
    if mode == NetMode::Allow {
        return Status::NotRequested;
    }
    if !net_supported() {
        return Status::NotEnforced(
            "this kernel's Landlock predates ABI v4 (Linux 6.7), the first to cover TCP".into(),
        );
    }
    // Landlock alone leaves UDP open, so without the filter the policy is
    // not what it says: a gap, refused in strict mode.
    let sockets = match crate::seccomp::status() {
        Status::Enforced(how) => how,
        gap => return gap,
    };
    Status::Enforced(match mode {
        NetMode::Proxy => format!(
            "Landlock ABI v4: TCP connect only to the proxy's port, binds refused \
             (Landlock matches ports, not addresses); {sockets}"
        ),
        _ => format!(
            "Landlock ABI v4: every TCP bind and connect refused, loopback included; \
             {sockets} (DNS included)"
        ),
    })
}

fn abi_number(abi: ABI) -> u8 {
    match abi {
        ABI::V1 => 1,
        ABI::V2 => 2,
        ABI::V3 => 3,
        ABI::V4 => 4,
        _ => 5,
    }
}

/// Confine the CALLING process (the helper, just before it execs).
///
/// # Errors
/// When the ruleset cannot be built or applied, or when `strict` and the
/// kernel enforced less than was asked.
pub(crate) fn restrict(spec: &EnterSpec) -> Result<(), String> {
    let wants_fs = spec.fs_mode != FsMode::Off;
    let wants_net = spec.net_mode != NetMode::Allow;
    if !wants_fs && !wants_net {
        return Ok(());
    }
    let all: BitFlags<AccessFs> = AccessFs::from_all(NEWEST);
    let read: BitFlags<AccessFs> = AccessFs::from_read(NEWEST);

    let mut ruleset = landlock::Ruleset::default();
    if wants_fs {
        ruleset = ruleset.handle_access(all).map_err(|e| e.to_string())?;
    }
    if wants_net {
        ruleset = ruleset
            .handle_access(AccessNet::from_all(ABI::V4))
            .map_err(|e| e.to_string())?;
    }
    let mut ruleset = ruleset.create().map_err(|e| e.to_string())?;

    if wants_fs {
        for dir in read_grants(&spec.deny_read) {
            // A path that vanished since it was listed is simply not granted.
            if let Ok(f) = PathFd::new(&dir) {
                ruleset = ruleset
                    .add_rule(PathBeneath::new(f, read))
                    .map_err(|e| e.to_string())?;
            }
        }
        for sink in [
            "/dev/null",
            "/dev/zero",
            "/dev/full",
            "/dev/random",
            "/dev/urandom",
            "/dev/tty",
            "/dev/ptmx",
            "/dev/pts",
        ] {
            if let Ok(f) = PathFd::new(sink) {
                ruleset = ruleset
                    .add_rule(PathBeneath::new(f, all))
                    .map_err(|e| e.to_string())?;
            }
        }
        let mut writable: Vec<PathBuf> = spec.roots.clone();
        writable.push(spec.temp.clone());
        for dir in &writable {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd(dir)?, all))
                .map_err(|e| e.to_string())?;
        }
    }
    if spec.net_mode == NetMode::Proxy {
        let port = spec
            .proxy_port
            .ok_or("network proxy requested but no proxy port was handed over")?;
        ruleset = ruleset
            .add_rule(NetPort::new(port, AccessNet::ConnectTcp))
            .map_err(|e| e.to_string())?;
    }

    let status = ruleset.restrict_self().map_err(|e| e.to_string())?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err("the kernel enforced none of the Landlock rules".into());
    }
    if spec.strict && wants_net && ABI::from(status.landlock) < ABI::V4 {
        return Err("the network rules did not take (Landlock ABI below v4)".into());
    }
    Ok(())
}

/// The directories to grant read access beneath so that everything is
/// readable except what lies under `denied` (canonical paths).
///
/// Landlock only ever grants, and a grant covers a whole subtree, so a
/// denied path is carved out by granting its siblings instead of its
/// parent, level by level from `/`: to hide `/a/b/c`, grant every entry of
/// `/` but `a`, every entry of `/a` but `b`, every entry of `/a/b` but `c`.
/// Path lookup is not restricted by Landlock, so `/a/x` stays reachable
/// through the ungranted `/a`; only listing `/a` itself is refused.
///
/// The siblings are listed once, when the command starts: an entry created
/// later in `/a` is not readable. An entry that is a symlink is granted only
/// when its target neither lies inside nor contains a denied path, so a link
/// cannot lead back into what was hidden. What this cannot see: a hard link
/// to a hidden FILE made elsewhere beforehand, or the same directory
/// mounted at a second path.
pub(crate) fn read_grants(denied: &[PathBuf]) -> Vec<PathBuf> {
    let mut grants = vec![PathBuf::from("/")];
    for hidden in denied {
        grants = grants
            .into_iter()
            .flat_map(|g| {
                if hidden.starts_with(&g) {
                    carve(&g, hidden)
                } else if g.starts_with(hidden) {
                    Vec::new()
                } else {
                    vec![g]
                }
            })
            .collect();
    }
    // Siblings are never on the way to a hidden path, so only a symlink
    // can resolve into (or above) one.
    grants.retain(|g| {
        let real = std::fs::canonicalize(g).unwrap_or_else(|_| g.clone());
        !denied
            .iter()
            .any(|d| real.starts_with(d) || d.starts_with(&real))
    });
    grants
}

/// The entries beneath `dir` that cover everything except `hidden`.
fn carve(dir: &Path, hidden: &Path) -> Vec<PathBuf> {
    if dir == hidden {
        return Vec::new();
    }
    let Ok(rest) = hidden.strip_prefix(dir) else {
        return vec![dir.to_path_buf()];
    };
    let Some(next) = rest.components().next() else {
        return Vec::new();
    };
    let toward = dir.join(next);
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| *p != toward)
                .collect()
        })
        .unwrap_or_default();
    out.extend(carve(&toward, hidden));
    out
}

fn fd(path: &Path) -> Result<PathFd, String> {
    PathFd::new(path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_cover_everything_but_the_hidden_branch() {
        let root = tempfile::tempdir().expect("tempdir");
        let base = std::fs::canonicalize(root.path()).expect("canon");
        for d in ["a/b/hidden/deep", "a/b/sibling", "a/other", "z"] {
            std::fs::create_dir_all(base.join(d)).expect("mkdir");
        }
        std::os::unix::fs::symlink(base.join("a/b/hidden"), base.join("z/link-in"))
            .expect("symlink");
        std::os::unix::fs::symlink(&base, base.join("z/link-above")).expect("symlink");
        let hidden = base.join("a/b/hidden");
        let grants = read_grants(std::slice::from_ref(&hidden));

        let covered = |p: &Path| grants.iter().any(|g| p.starts_with(g));
        assert!(
            !covered(&hidden.join("deep")),
            "the hidden branch is granted"
        );
        assert!(covered(&base.join("a/b/sibling")));
        assert!(covered(&base.join("a/other")));
        assert!(covered(Path::new("/usr")) || !Path::new("/usr").exists());
        assert!(!grants.contains(&base.join("z/link-in")), "a link into it");
        assert!(
            !grants.contains(&base.join("z/link-above")),
            "a link above it"
        );
        assert!(!grants.contains(&PathBuf::from("/")));
    }

    #[test]
    fn no_hidden_path_grants_the_root() {
        assert_eq!(read_grants(&[]), vec![PathBuf::from("/")]);
    }
}
