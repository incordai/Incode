//! Linux confinement with Landlock.
//!
//! The command runs through `incode --sandbox-exec`, which calls [`restrict`]
//! on itself and then execs the shell. Landlock is the kernel's own
//! unprivileged filesystem confinement: a task adds rules, calls
//! `restrict_self`, and from then on it and every child may touch only what the
//! rules allow. It needs no privilege and cannot be widened once set.
//!
//! Measured live in WSL2 (kernel 5.15, effective ABI V1, so `PartiallyEnforced`
//! rather than `FullyEnforced`): a write inside a granted root succeeds, a write
//! to `$HOME` is denied (EACCES) and the file is not created, a child's write is
//! denied too, `git`/`cc`/`ls` and `/dev/null` still work, and `curl` is
//! untouched — the filesystem rules do not cover the network.
//!
//! # The network (`sandbox_network: "deny"`)
//!
//! Landlock ABI v4 (Linux 6.7) added `LANDLOCK_ACCESS_NET_BIND_TCP` and
//! `CONNECT_TCP`, which landlock 0.4.7 exposes as `AccessNet`. Handling both
//! and adding no `NetPort` rule refuses every TCP bind and connect — loopback
//! too, because Landlock matches ports, not addresses. UDP (and so DNS) is not
//! covered by any ABI. On an older kernel the crate's best-effort level drops
//! the network rights silently, so the effective ABI is read back after
//! `restrict_self` and a deny that did not take is traced as NOT enforced;
//! [`network_enforceable`] lets the parent say so before the command runs.

use std::path::PathBuf;
use std::process::Command;

use landlock::{
    Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus, ABI,
};

use super::{SandboxMode, SandboxNetwork, SandboxPolicy};

/// The command that runs `command` under this session's sandbox: the incode
/// binary re-invoked as the `--sandbox-exec` helper. When incode cannot name
/// its own binary the helper cannot be built, so the caller runs the command
/// unconfined — [`super::Sandbox::note`] having already said the sandbox was
/// requested.
pub(super) fn wrap(policy: SandboxPolicy, roots: &[PathBuf], command: &str) -> Command {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("incode"));
    let argv = super::helper_argv(&exe, policy, roots, command);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd
}

/// Whether this kernel can refuse a command's TCP: a ruleset handling the
/// network rights is created with the HARD requirement level, which fails
/// instead of silently dropping them on a kernel older than ABI v4. Created
/// only — never applied — so asking restricts nothing.
pub(super) fn network_enforceable() -> bool {
    landlock::Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessNet::from_all(ABI::V4))
        .and_then(landlock::Ruleset::create)
        .is_ok()
}

/// Apply the confinement to the CALLING process. Best effort: a kernel without
/// Landlock, or one with an older ABI, restricts as much as it can and leaves
/// the rest — the command still runs, and the trace says what did not take.
/// Never panics: a sandbox that aborted the command would be worse than one
/// the kernel could only partly honour.
pub(super) fn restrict(mode: SandboxMode, network: SandboxNetwork, roots: &[PathBuf]) {
    if mode == SandboxMode::Off && network == SandboxNetwork::Allow {
        return;
    }
    match build(mode, network, roots) {
        Ok((status, abi)) => {
            crate::trace!("sandbox", "landlock {}: {:?}", mode.as_str(), status);
            if network == SandboxNetwork::Deny {
                if abi >= ABI::V4 {
                    crate::trace!("sandbox", "network deny enforced (Landlock ABI {abi})");
                } else {
                    crate::trace!(
                        "sandbox",
                        "network deny NOT enforced: this kernel's Landlock ABI is {abi}, \
                         and the network needs 4"
                    );
                }
            }
        }
        Err(e) => crate::trace!(
            "sandbox",
            "landlock could not be applied ({e}); the command runs unconfined"
        ),
    }
}

/// Build and apply the ruleset, returning its status and the kernel's
/// effective ABI. `ABI::V9` is the newest the crate knows; the default
/// compatibility level downgrades to whatever the running kernel supports
/// rather than failing.
fn build(
    mode: SandboxMode,
    network: SandboxNetwork,
    roots: &[PathBuf],
) -> Result<(RulesetStatus, ABI), String> {
    let abi = ABI::V9;
    let all: BitFlags<AccessFs> = AccessFs::from_all(abi);
    let read: BitFlags<AccessFs> = AccessFs::from_read(abi);

    let mut ruleset = landlock::Ruleset::default();
    if mode != SandboxMode::Off {
        ruleset = ruleset.handle_access(all).map_err(|e| e.to_string())?;
    }
    if network == SandboxNetwork::Deny {
        // Handled with NO port rule added below: every TCP bind and connect
        // is refused.
        ruleset = ruleset
            .handle_access(AccessNet::from_all(ABI::V4))
            .map_err(|e| e.to_string())?;
    }
    let mut ruleset = ruleset.create().map_err(|e| e.to_string())?;

    if mode != SandboxMode::Off {
        // Read (and traverse and execute) everywhere: the command may READ any
        // file, as both modes intend.
        ruleset = ruleset
            .add_rule(PathBeneath::new(root_fd("/")?, read))
            .map_err(|e| e.to_string())?;

        // The sinks a command legitimately writes to even under read-only.
        for sink in [
            "/dev/null",
            "/dev/zero",
            "/dev/tty",
            "/dev/full",
            "/dev/random",
        ] {
            if let Ok(fd) = PathFd::new(sink) {
                ruleset = ruleset
                    .add_rule(PathBeneath::new(fd, all))
                    .map_err(|e| e.to_string())?;
            }
        }
    }

    if mode == SandboxMode::WorkspaceWrite {
        // Full access beneath each root and the temp directory. Opened
        // explicitly (not through `path_beneath_rules`, which silently skips a
        // path it cannot open) so a root that is missing is a real error.
        let mut writable = roots.to_vec();
        writable.push(super::temp_dir());
        for dir in writable {
            let fd = PathFd::new(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, all))
                .map_err(|e| e.to_string())?;
        }
    }

    let status = ruleset.restrict_self().map_err(|e| e.to_string())?;
    Ok((status.ruleset, ABI::from(status.landlock)))
}

/// A path handle for a directory, as a rule needs it.
fn root_fd(path: &str) -> Result<PathFd, String> {
    PathFd::new(path).map_err(|e| format!("{path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrap re-invokes incode as the helper, carrying the mode and roots.
    #[test]
    fn wrap_builds_the_helper_invocation() {
        let cmd = wrap(
            SandboxPolicy {
                mode: SandboxMode::WorkspaceWrite,
                network: SandboxNetwork::Allow,
                named: false,
                engine: crate::sandbox::SandboxEngine::Builtin,
            },
            &[PathBuf::from("/ws")],
            "echo hi",
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "--sandbox-exec");
        assert_eq!(args[1], "workspace-write");
        assert!(args.iter().any(|a| a == "/ws"));
        assert_eq!(args.last().map(String::as_str), Some("echo hi"));
    }
}
