//! WSL2: closing the door to Windows.
//!
//! Inside WSL2, a Linux program can start a Windows program
//! (`/mnt/c/Windows/System32/cmd.exe`, `powershell.exe`, any `.exe`), and
//! that program runs on Windows, outside Linux altogether. None of the
//! sandbox's rules reach it. It goes three ways, and each is closed when the
//! filesystem is confined (`fs.block_wsl_interop`, on by default):
//!
//! - **Running a `.exe`.** The kernel hands it, through `binfmt_misc`, to
//!   WSL's interpreter (`/init`, named in
//!   `/proc/sys/fs/binfmt_misc/WSLInterop`). The interpreter is added to
//!   `deny_read`, so the sandbox can't execute it and no `.exe` starts.
//! - **The interop socket.** The interpreter reaches Windows over a Unix
//!   socket named by `WSL_INTEROP`, under `/run/WSL`. Landlock can't refuse a
//!   connect to a Unix socket by path, so the variable is removed and
//!   `/run/WSL` is hidden: nothing in the sandbox can find the socket.
//! - **vsock.** The seccomp filter refuses `AF_VSOCK` whenever the
//!   filesystem is confined (see `seccomp`).
//!
//! Hiding `/init` and `/run/WSL` uses the same carving as `deny_read`, so
//! listing `/` or `/run` is refused inside the sandbox; files beneath them
//! are still readable.

use std::path::{Path, PathBuf};

const BINFMT: &str = "/proc/sys/fs/binfmt_misc";
const INTEROP_SOCKETS: &str = "/run/WSL";

/// Whether this Linux is WSL.
#[must_use]
pub fn detected() -> bool {
    let version = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    is_wsl(&version, !interop_entries().is_empty())
}

/// WSL by its kernel release (`…-microsoft-standard-WSL2`) or by a
/// registered interop handler.
#[must_use]
pub fn is_wsl(kernel_release: &str, interop_registered: bool) -> bool {
    let r = kernel_release.to_ascii_lowercase();
    interop_registered || r.contains("microsoft") || r.contains("wsl")
}

/// The `binfmt_misc` entries WSL registers for `.exe` files.
fn interop_entries() -> Vec<PathBuf> {
    std::fs::read_dir(BINFMT)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("WSLInterop"))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The interpreter a `binfmt_misc` entry names (its `interpreter` line).
#[must_use]
pub fn interpreter(entry_text: &str) -> Option<PathBuf> {
    entry_text
        .lines()
        .find_map(|l| l.strip_prefix("interpreter "))
        .map(|p| PathBuf::from(p.trim()))
}

/// The paths to hide so no Windows program can be started: every interop
/// interpreter (`/init` by default) and the interop sockets' folder. Only
/// paths that exist, canonicalised.
#[must_use]
pub fn interop_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = interop_entries()
        .iter()
        .filter_map(|e| std::fs::read_to_string(e).ok())
        .filter_map(|t| interpreter(&t))
        .collect();
    paths.push(PathBuf::from("/init"));
    paths.push(PathBuf::from(INTEROP_SOCKETS));
    let mut out = Vec::new();
    for p in paths {
        if let Ok(real) = std::fs::canonicalize(&p) {
            if !out.contains(&real) && real != Path::new("/") {
                out.push(real);
            }
        }
    }
    out
}

/// The variable that tells WSL's interpreter where the interop socket is.
pub const INTEROP_VAR: &str = "WSL_INTEROP";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_is_recognised_by_kernel_or_interop() {
        assert!(is_wsl("6.6.87.2-microsoft-standard-WSL2", false));
        assert!(is_wsl("5.15.167.4-microsoft-standard-WSL2", false));
        assert!(is_wsl("6.18.0-custom", true));
        assert!(!is_wsl("6.18.44-fc-v139", false));
    }

    #[test]
    fn the_interpreter_is_read_from_the_binfmt_entry() {
        let entry = "enabled\ninterpreter /init\nflags: PF\noffset 0\nmagic 4d5a\n";
        assert_eq!(interpreter(entry), Some(PathBuf::from("/init")));
        assert_eq!(interpreter("enabled\nflags: F\n"), None);
    }
}
