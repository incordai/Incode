//! Taking this process's standard streams for the protocol.
//!
//! # Why the handles are moved, not just avoided
//!
//! Under `--acp`, stdout IS the protocol: one stray byte there is a parse error
//! at the client, and the session is gone. Keeping every print off it by
//! discipline alone does not hold — this crate has well over a hundred
//! `print!`/`println!` sites, a console that writes to stdout by design, and
//! child processes (probes, hooks, agent CLIs) that inherit whatever the
//! standard handles point at when they are spawned.
//!
//! So the protocol takes the REAL stdin and stdout as private handles, and the
//! process-wide standard handles are repointed: stdout at stderr, stdin at the
//! null device. After [`claim`], anything that writes "to stdout" — a render
//! call, a forgotten `println!`, an inherited child — lands on stderr, which
//! ACP leaves free for logging. Anything that reads stdin, including a child
//! that inherits it, reads end-of-file instead of eating the client's messages.

use std::fs::File;

/// The protocol's own ends of the pipe.
pub(crate) struct Claimed {
    /// What the client writes: requests and notifications.
    pub(crate) input: File,
    /// What the client reads. Nothing but protocol messages may go here.
    pub(crate) output: File,
}

/// Take stdin and stdout for the protocol, and repoint the process-wide
/// standard handles at the null device (input) and stderr (output).
///
/// Call once, before anything else in the process has printed.
///
/// # Errors
///
/// When the standard handles cannot be read or replaced. `--acp` must not
/// start in that case: running with stdout still shared is exactly the failure
/// this exists to prevent.
#[cfg(windows)]
pub(crate) fn claim() -> std::io::Result<Claimed> {
    use std::os::windows::io::{FromRawHandle, IntoRawHandle};
    use windows_sys::Win32::Foundation::{
        SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    let usable = |handle: HANDLE| !handle.is_null() && handle != INVALID_HANDLE_VALUE;

    // SAFETY: GetStdHandle only reads the process's handle table.
    let (input, output, error) = unsafe {
        (
            GetStdHandle(STD_INPUT_HANDLE),
            GetStdHandle(STD_OUTPUT_HANDLE),
            GetStdHandle(STD_ERROR_HANDLE),
        )
    };
    if !usable(input) || !usable(output) {
        return Err(std::io::Error::other(
            "this process has no stdin or stdout to speak the protocol on",
        ));
    }

    // The null device becomes the process's stdin for the rest of its life,
    // so the handle is deliberately never closed.
    let null = std::fs::OpenOptions::new().read(true).open("NUL")?;
    let null: HANDLE = null.into_raw_handle();

    // Output first: from this call on, nothing more can land on the protocol.
    // A missing stderr (a launcher that gave none) repoints stdout at nothing,
    // which the standard library treats as a sink — suppressed, not leaked.
    // SAFETY: both arguments are handles this process owns.
    if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, error) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { SetStdHandle(STD_INPUT_HANDLE, null) } == 0 {
        return Err(std::io::Error::last_os_error());
    }

    // Keep the protocol ends out of every child process. An inherited copy of
    // the write end would hold the client's pipe open after this process has
    // exited, so the client would never see it go. Best effort: a handle that
    // refuses leaves only that lingering-pipe case, not a leak onto stdout.
    // SAFETY: both are valid handles owned by this process.
    unsafe {
        SetHandleInformation(input, HANDLE_FLAG_INHERIT, 0);
        SetHandleInformation(output, HANDLE_FLAG_INHERIT, 0);
    }

    // SAFETY: the two handles are open, owned by this process, and no longer
    // reachable through the standard-handle table, so these Files are their
    // only owners.
    let (input, output) = unsafe { (File::from_raw_handle(input), File::from_raw_handle(output)) };
    Ok(Claimed { input, output })
}

/// See the Windows version above; the same move, with file descriptors.
#[cfg(unix)]
pub(crate) fn claim() -> std::io::Result<Claimed> {
    use std::os::fd::{AsRawFd, FromRawFd};

    // Private copies of the real stdin and stdout, close-on-exec so no child
    // inherits the protocol ends. 3 is the lowest descriptor they may take,
    // keeping them clear of 0, 1 and 2.
    // SAFETY: fcntl on descriptors 0 and 1 has no memory-safety preconditions.
    let input = unsafe { libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 3) };
    if input < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    let output = unsafe { libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3) };
    if output < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `input` and `output` were just returned by fcntl and are owned
    // by nothing else.
    let (input, output) = unsafe { (File::from_raw_fd(input), File::from_raw_fd(output)) };

    let null = File::open("/dev/null")?;
    // Output first, for the same reason as on Windows.
    // SAFETY: dup2 onto the standard descriptors; all are open.
    if unsafe { libc::dup2(2, 1) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::dup2(null.as_raw_fd(), 0) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Claimed { input, output })
}

/// No standard-handle move is known for this platform, so `--acp` refuses to
/// start rather than run with stdout shared.
#[cfg(not(any(windows, unix)))]
pub(crate) fn claim() -> std::io::Result<Claimed> {
    Err(std::io::Error::other(
        "incode --acp cannot isolate stdout on this platform",
    ))
}
