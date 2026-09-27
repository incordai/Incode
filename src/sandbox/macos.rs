//! macOS confinement with `sandbox-exec` (Seatbelt).
//!
//! The command runs as `sandbox-exec -D WS=… -D TMP=… -p <profile> /bin/sh -c
//! <command>`. The profile allows everything by default, then denies all writes
//! and re-allows the exceptions — the last matching rule wins, so the deny is
//! stated before the allows.
//!
//! Validated live on macOS 26.6.2: a write inside the workspace succeeds, a
//! write to `$HOME` is refused ("Operation not permitted") and the file is not
//! created, a child process is refused too, `git`/`cc`/`ls` and `mktemp` still
//! run, and `curl` still reaches the network (that profile did not restrict it).
//! `sandbox-exec` is marked deprecated by Apple but present and silent.
//!
//! `sandbox_network: "deny"` adds `(deny network*)` and re-allows loopback and
//! Unix sockets ([`super::seatbelt_profile`]). That profile is unit-tested on
//! every platform but has NOT been run on a Mac: the Mac was offline when it
//! was written.
//!
//! Two things the profile depends on, both measured:
//! - **Real paths only.** Seatbelt matches resolved paths, so the roots and the
//!   temp directory are canonicalised before they are passed.
//! - **`-D` parameters, never interpolation.** A path with a quote or a
//!   backslash breaks a profile built by string concatenation; passed as a
//!   parameter and matched with `(param "WS")` it needs no escaping.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{SandboxMode, SandboxPolicy};

/// Where the tool is. A fixed path: it is a system binary, and looking it up on
/// `PATH` would let a workspace shadow it.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Whether this machine has `sandbox-exec`.
pub(super) fn available() -> bool {
    Path::new(SANDBOX_EXEC).exists()
}

/// The confined command, or `None` when `sandbox-exec` is missing. The
/// network half of the policy is in the same profile
/// ([`super::seatbelt_profile`]), so one `sandbox-exec` carries both.
pub(super) fn wrap(
    policy: SandboxPolicy,
    roots: &[PathBuf],
    shell: &crate::tools::shell_choice::Shell,
    command: &str,
) -> Option<Command> {
    if !available() {
        return None;
    }
    let temp = real(&super::temp_dir());
    let mut cmd = Command::new(SANDBOX_EXEC);
    // The temp directory is writable in BOTH modes: too many tools fail
    // without it, and it is not the workspace.
    cmd.arg("-D").arg(format!("TMP={}", temp.display()));
    let writable_roots: Vec<PathBuf> = if policy.mode == SandboxMode::WorkspaceWrite {
        roots.iter().map(|r| real(r)).collect()
    } else {
        Vec::new()
    };
    for (i, root) in writable_roots.iter().enumerate() {
        cmd.arg("-D").arg(format!("WS{i}={}", root.display()));
    }
    cmd.arg("-p").arg(super::seatbelt_profile(
        policy.mode,
        writable_roots.len(),
        policy.network,
    ));
    // The shell is honoured, so a caller that runs another one still does.
    cmd.args(shell.argv(command));
    Some(cmd)
}

/// A path as Seatbelt matches it: resolved. An unresolvable path is left as it
/// is, so a rule for it simply will not match rather than crashing the run.
fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
