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
//! untouched — Landlock does not cover the network.

use std::path::PathBuf;
use std::process::Command;

use landlock::{
    Access, AccessFs, BitFlags, PathBeneath, PathFd, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus, ABI,
};

use super::SandboxMode;

/// The command that runs `command` under this session's sandbox: the incode
/// binary re-invoked as the `--sandbox-exec` helper. When incode cannot name
/// its own binary the helper cannot be built, so the caller runs the command
/// unconfined — [`super::Sandbox::note`] having already said the sandbox was
/// requested.
pub(super) fn wrap(mode: SandboxMode, roots: &[PathBuf], command: &str) -> Command {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("incode"));
    let argv = super::helper_argv(&exe, mode, roots, command);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd
}

/// Apply the confinement to the CALLING process. Best effort: a kernel without
/// Landlock, or one with an older ABI, restricts as much as it can and leaves
/// the rest — the command still runs. Never panics: a sandbox that aborted the
/// command would be worse than one the kernel could only partly honour.
pub(super) fn restrict(mode: SandboxMode, roots: &[PathBuf]) {
    if mode == SandboxMode::Off {
        return;
    }
    match build(mode, roots) {
        Ok(status) => crate::trace!("sandbox", "landlock {}: {:?}", mode.as_str(), status),
        Err(e) => crate::trace!(
            "sandbox",
            "landlock could not be applied ({e}); the command runs unconfined"
        ),
    }
}

/// Build and apply the ruleset. `ABI::V9` is the newest the crate knows; the
/// default compatibility level downgrades to whatever the running kernel
/// supports rather than failing.
fn build(mode: SandboxMode, roots: &[PathBuf]) -> Result<RulesetStatus, String> {
    let abi = ABI::V9;
    let all: BitFlags<AccessFs> = AccessFs::from_all(abi);
    let read: BitFlags<AccessFs> = AccessFs::from_read(abi);

    let mut ruleset = landlock::Ruleset::default()
        .handle_access(all)
        .map_err(|e| e.to_string())?
        .create()
        .map_err(|e| e.to_string())?
        // Read (and traverse and execute) everywhere: the command may READ any
        // file, as both modes intend.
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
    Ok(status.ruleset)
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
            SandboxMode::WorkspaceWrite,
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
