//! Resource limits applied in the helper, just before it execs the command.
//!
//! `setrlimit` limits are per process and inherited, so each process the
//! command starts gets the same ceiling (not a shared budget). The wall-clock
//! limit is the parent's: it kills the whole process group.

use crate::policy::Limits;
use crate::Status;

#[must_use]
pub fn status(limits: &Limits) -> Status {
    let per_process = limits.cpu_secs.is_some()
        || limits.memory_mb.is_some()
        || limits.file_size_mb.is_some()
        || limits.open_files.is_some();
    if !per_process && limits.wall_secs.is_none() {
        return Status::NotRequested;
    }
    if per_process && !cfg!(unix) {
        return Status::NotEnforced(
            "per-process limits (cpu, memory, file size, open files) need setrlimit; on this \
             platform only wall_secs is enforced"
                .into(),
        );
    }
    let mut what = Vec::new();
    if limits.wall_secs.is_some() {
        what.push("wall clock (process group killed)");
    }
    if per_process {
        what.push("setrlimit per process");
    }
    Status::Enforced(what.join(", "))
}

/// Apply the per-process limits to the calling process.
///
/// # Errors
/// When the kernel refuses a limit. On macOS `RLIMIT_AS` is commonly
/// refused; that is reported, and the caller decides (strict refuses).
#[cfg(unix)]
pub(crate) fn apply(limits: &Limits) -> Result<(), String> {
    const MB: u64 = 1024 * 1024;
    let set = |resource, value: u64, name: &str| -> Result<(), String> {
        let lim = libc::rlimit {
            rlim_cur: value as libc::rlim_t,
            rlim_max: value as libc::rlim_t,
        };
        // SAFETY: setrlimit reads a valid, initialised rlimit struct.
        let rc = unsafe { libc::setrlimit(resource, &lim) };
        if rc == 0 {
            Ok(())
        } else {
            Err(format!(
                "setrlimit {name} = {value}: {}",
                std::io::Error::last_os_error()
            ))
        }
    };
    if let Some(s) = limits.cpu_secs {
        set(libc::RLIMIT_CPU, s, "cpu")?;
    }
    if let Some(mb) = limits.memory_mb {
        set(libc::RLIMIT_AS, mb.saturating_mul(MB), "memory")?;
    }
    if let Some(mb) = limits.file_size_mb {
        set(libc::RLIMIT_FSIZE, mb.saturating_mul(MB), "file size")?;
    }
    if let Some(n) = limits.open_files {
        set(libc::RLIMIT_NOFILE, n, "open files")?;
    }
    Ok(())
}
