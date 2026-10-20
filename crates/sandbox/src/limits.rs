//! Resource limits applied in the helper, just before it execs the command.
//!
//! `setrlimit` limits are per process and inherited, so each process the
//! command starts gets the same ceiling (not a shared budget). The wall-clock
//! limit is the parent's: it kills the whole process group.
//!
//! On Windows the helper's job object carries them instead (see
//! [`windows_status`]): the per-process limits that have a job-object
//! equivalent, and the wall clock by the parent killing the helper, whose
//! job then takes the whole tree with it.

use crate::policy::Limits;
use crate::Status;

/// What this platform enforces of `limits`.
#[must_use]
pub fn status(limits: &Limits) -> Status {
    if cfg!(windows) {
        windows_status(limits)
    } else {
        rlimit_status(limits)
    }
}

/// What native Windows enforces of `limits`, through a job object.
///
/// `memory_mb` becomes `JOB_OBJECT_LIMIT_PROCESS_MEMORY`, which caps the
/// memory a process COMMITS (not its address space, as `RLIMIT_AS` does):
/// an allocation past it fails. `cpu_secs` becomes
/// `JOB_OBJECT_LIMIT_PROCESS_TIME`, user-mode time per process: the process
/// is ended when it runs out. `file_size_mb` and `open_files` have no Windows
/// counterpart, so asking for either is reported not enforced, naming what
/// would still be.
#[must_use]
pub fn windows_status(limits: &Limits) -> Status {
    let mut what = Vec::new();
    if limits.wall_secs.is_some() {
        what.push("wall clock (the job is killed with everything in it)");
    }
    if limits.cpu_secs.is_some() {
        what.push("job object: user-mode CPU time per process");
    }
    if limits.memory_mb.is_some() {
        what.push("job object: committed memory per process");
    }
    let mut missing = Vec::new();
    if limits.file_size_mb.is_some() {
        missing.push("file_size_mb");
    }
    if limits.open_files.is_some() {
        missing.push("open_files");
    }
    if !missing.is_empty() {
        let enforced = if what.is_empty() {
            String::new()
        } else {
            format!("; enforced: {}", what.join(", "))
        };
        return Status::NotEnforced(format!(
            "{} not enforced: Windows has no per-process limit for {}{enforced}",
            missing.join(" and "),
            if missing.len() == 1 { "it" } else { "them" },
        ));
    }
    if what.is_empty() {
        return Status::NotRequested;
    }
    Status::Enforced(what.join(", "))
}

fn rlimit_status(limits: &Limits) -> Status {
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
    // Never a core dump: it would write the process's memory (tokens,
    // keys) to disk, outside what the command was meant to produce.
    set(libc::RLIMIT_CORE, 0, "core dumps")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_reports_what_its_job_object_enforces_and_what_it_cannot() {
        assert_eq!(windows_status(&Limits::default()), Status::NotRequested);
        let ok = Limits {
            wall_secs: Some(5),
            cpu_secs: Some(2),
            memory_mb: Some(512),
            ..Limits::default()
        };
        assert!(
            matches!(windows_status(&ok), Status::Enforced(s) if s.contains("committed memory") && s.contains("CPU") && s.contains("wall"))
        );
        let gap = Limits {
            memory_mb: Some(512),
            file_size_mb: Some(1),
            open_files: Some(10),
            ..Limits::default()
        };
        match windows_status(&gap) {
            Status::NotEnforced(s) => {
                assert!(s.contains("file_size_mb and open_files"), "{s}");
                assert!(s.contains("enforced: job object: committed memory"), "{s}");
            }
            other => panic!("{other:?}"),
        }
    }
}
