//! Stopping a process stops everything it started.
//!
//! # Why `Child::kill` is not enough
//!
//! It ends the one process it was given. An agent CLI is rarely one process:
//! on Windows it is often a `.cmd` shim that starts `node`; a check command is
//! a shell that starts `cargo`, which starts the test binaries. Killing the
//! top of that tree left the rest running — an agent "cancelled" while it was
//! still writing files, a check "timed out" while `cargo test` carried on.
//!
//! # How, per platform
//!
//! - **Windows**: the child is put in a job object created with
//!   `KILL_ON_JOB_CLOSE`. Everything it starts afterwards is in the job, so
//!   [`Tree::kill`] ends the lot — and when the job's handle closes (the
//!   `Tree` drops, or incode itself dies), Windows ends whatever is left.
//! - **Linux and macOS**: [`prepare`] starts the child in a process group of
//!   its own, and [`Tree::kill`] signals the whole group. There is no portable
//!   "die with your parent" here, so descendants are ended on a kill, not when
//!   incode exits.
//! - Anything else: the direct child only, as before.
//!
//! On Unix the group is signalled only while the leader is unreaped — every
//! caller kills before `wait()` — so the group id cannot have been reused by an
//! unrelated process.
//!
//! On Windows the child runs briefly before it is assigned to the job (std
//! gives no way to start it suspended). A descendant started in those
//! microseconds would escape; an agent CLI takes far longer than that to start
//! its first child.

use std::process::{Child, Command};

/// Get `cmd` ready to be contained. Call before `spawn`.
pub fn prepare(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A group of its own, led by the child: its id is the group id.
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// Everything a spawned child has started, or will start.
pub struct Tree {
    #[cfg(windows)]
    job: Option<job::Job>,
    #[cfg(unix)]
    group: i32,
}

impl Tree {
    /// Take in `child`, freshly spawned from a [`prepare`]d command.
    ///
    /// Never fails: a tree that cannot be contained falls back to killing the
    /// child alone, and says so on stderr, because refusing to run the agent
    /// over it would be the worse trade.
    #[must_use]
    pub fn adopt(child: &Child) -> Tree {
        #[cfg(windows)]
        {
            let job = match job::Job::containing(child) {
                Ok(job) => Some(job),
                Err(why) => {
                    eprintln!(
                        "could not contain process {} ({why}); stopping it will stop it alone",
                        child.id()
                    );
                    None
                }
            };
            Tree { job }
        }
        #[cfg(unix)]
        {
            // `process_group(0)` made the child its group's leader, so the
            // group id is its pid. A pid always fits: pid_t is i32.
            Tree {
                group: i32::try_from(child.id()).unwrap_or(0),
            }
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = child;
            Tree {}
        }
    }

    /// End `child` and every descendant still running. Call BEFORE `wait()`.
    pub fn kill(&self, child: &mut Child) {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
        #[cfg(unix)]
        if self.group > 0 {
            // SAFETY: killpg takes plain integers and touches no memory. The
            // group is the one `prepare` created for this child, whose leader
            // has not been reaped yet (see the module note), so the id still
            // names that group and no other.
            unsafe {
                libc::killpg(self.group, libc::SIGKILL);
            }
        }
        // The direct child as well: it is the one process every platform can
        // reach, and killing an already-ended process is harmless.
        let _ = child.kill();
    }
}

#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// The exit code the job's processes are given when it is terminated.
    const KILLED: u32 = 1;

    /// An owned job object handle, closed on drop — which, with
    /// `KILL_ON_JOB_CLOSE`, ends every process still in it.
    ///
    /// Held as an integer so the owner stays `Send`: a kernel handle is not
    /// tied to the thread that opened it.
    pub struct Job(isize);

    impl Job {
        pub fn containing(child: &Child) -> Result<Job, String> {
            // SAFETY: both pointers are documented as optional (no security
            // attributes, no name). A null return is checked before use.
            let handle: HANDLE = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(last_error("CreateJobObjectW"));
            }
            // Owned from here, so every early return below closes it.
            let job = Job(handle as isize);

            // SAFETY: an all-zero JOBOBJECT_EXTENDED_LIMIT_INFORMATION is the
            // documented "no limits" value; only the one flag is then set.
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let size = u32::try_from(std::mem::size_of_val(&limits))
                .map_err(|_| "job limit struct size".to_string())?;
            // SAFETY: the pointer and length describe `limits`, which lives
            // for the duration of the call, and the class matches its type.
            let set = unsafe {
                SetInformationJobObject(
                    job.handle(),
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&limits).cast(),
                    size,
                )
            };
            if set == 0 {
                return Err(last_error("SetInformationJobObject"));
            }

            // SAFETY: `child` owns a live process handle for as long as the
            // borrow lasts; the job handle was checked above.
            let assigned =
                unsafe { AssignProcessToJobObject(job.handle(), child.as_raw_handle().cast()) };
            if assigned == 0 {
                return Err(last_error("AssignProcessToJobObject"));
            }
            Ok(job)
        }

        pub fn terminate(&self) {
            // SAFETY: the handle is owned and open until `drop`.
            unsafe {
                TerminateJobObject(self.handle(), KILLED);
            }
        }

        fn handle(&self) -> HANDLE {
            self.0 as HANDLE
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: owned, opened once in `containing`, closed only here.
            unsafe {
                CloseHandle(self.handle());
            }
        }
    }

    fn last_error(call: &str) -> String {
        // SAFETY: reads the calling thread's last-error value; no arguments.
        let code = unsafe { GetLastError() };
        format!("{call} failed with error {code}")
    }
}

/// A parent that starts a grandchild appending to `ticks.txt` about every
/// half second, then sleeps for a minute. Returns the command and the ticks
/// file — a file that stops growing is a grandchild that stopped.
///
/// Shared with the other tests that kill a tree (`tools::bash`).
#[cfg(test)]
pub(crate) fn a_parent_with_a_ticking_grandchild(
    dir: &std::path::Path,
) -> (Command, std::path::PathBuf) {
    let ticks = dir.join("ticks.txt");
    #[cfg(windows)]
    {
        std::fs::write(
            dir.join("tick.cmd"),
            "@echo off\r\n:loop\r\necho x>>\"%~dp0ticks.txt\"\r\n\
             ping -n 2 -w 500 127.0.0.1 >nul\r\ngoto loop\r\n",
        )
        .expect("write the grandchild");
        let parent = dir.join("parent.cmd");
        std::fs::write(
            &parent,
            "@echo off\r\nstart \"\" /B cmd /C \"%~dp0tick.cmd\"\r\n\
             ping -n 60 127.0.0.1 >nul\r\n",
        )
        .expect("write the parent");
        (Command::new(parent), ticks)
    }
    #[cfg(unix)]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!(
            "(while true; do echo x >> '{}'; sleep 0.3; done) & sleep 60",
            ticks.display()
        ));
        (cmd, ticks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn size(path: &Path) -> u64 {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }

    /// **KILLING THE CHILD KILLS WHAT IT STARTED.**
    ///
    /// Measured by behaviour, not by process lists: the grandchild writes to a
    /// file while it lives, so a file that stops growing is a grandchild that
    /// stopped.
    #[test]
    fn killing_the_tree_stops_the_grandchild() {
        let dir = tempfile::tempdir().expect("scratch");
        let (mut cmd, ticks) = a_parent_with_a_ticking_grandchild(dir.path());
        prepare(&mut cmd);
        let mut child = cmd.spawn().expect("spawn the parent");
        let tree = Tree::adopt(&child);

        let deadline = Instant::now() + Duration::from_secs(15);
        while size(&ticks) < 4 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(size(&ticks) >= 4, "the grandchild never started ticking");

        tree.kill(&mut child);
        let _ = child.wait();
        // Let a tick already in flight land, then watch for new ones.
        std::thread::sleep(Duration::from_millis(1500));
        let after_kill = size(&ticks);
        std::thread::sleep(Duration::from_millis(2500));
        assert_eq!(
            size(&ticks),
            after_kill,
            "the grandchild kept running after the tree was killed"
        );
    }
}
