//! An exclusive OS lock on a file, held for as long as the value lives.
//!
//! # Why this and not `std::fs::File::lock`
//!
//! The standard library has had `File::try_lock` since Rust 1.89, and it is the
//! natural thing to use. This crate's declared floor is 1.88 (`rust-version`,
//! measured against the whole dependency tree and written into `Cargo.toml`,
//! `README.md` and `CONTRIBUTING.md`), so using it would make that floor false,
//! and moving the floor is a decision for the whole project, not for one module.
//! `fd-lock` 4.0.4 is the same thing (`flock` on Unix, `LockFileEx` on Windows),
//! it is already in the lock file (rustyline uses it), and it adds no package.
//! Swapping this module for `File::try_lock` when the floor moves is a change
//! to this file alone.
//!
//! # What the lock is
//!
//! The OS holds it exactly as long as the handle that took it is open and drops
//! it the instant the holder exits or crashes. That is the property that makes it
//! right for a lock file: no file "left behind" can be stale, so nothing has to
//! guess at a dead holder from the age of a file (which is how a holder merely
//! slower than the guess got robbed).
//!
//! `fd-lock` hands out a guard that unlocks when dropped and borrows the lock.
//! This module does not need either: it takes the lock, forgets the guard, and
//! lets the OS release it when the file is closed, which happens when the
//! [`OsLock`] is dropped. So a held lock is just a value that can be stored.

use std::fs::File;
use std::io;
use std::time::{Duration, Instant};

/// A file that may be locked, and holds the lock once it has been taken.
pub struct OsLock {
    inner: fd_lock::RwLock<File>,
}

impl std::fmt::Debug for OsLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OsLock")
    }
}

impl OsLock {
    /// A lock on `file`. Nothing is locked until [`OsLock::try_acquire`].
    #[must_use]
    pub fn new(file: File) -> Self {
        Self {
            inner: fd_lock::RwLock::new(file),
        }
    }

    /// Try once to take the lock for this process: `Ok(true)` when it is held
    /// now, `Ok(false)` when another holder has it.
    ///
    /// # Errors
    ///
    /// The OS refused for another reason.
    pub fn try_acquire(&mut self) -> io::Result<bool> {
        match self.inner.try_write() {
            Ok(guard) => {
                // Kept: the OS releases it when the file is closed, with `self`.
                std::mem::forget(guard);
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Take the lock, trying again every 10 ms for up to `wait`. `Err(None)` is
    /// "still held when the time ran out"; `Err(Some(e))` is the OS refusing for
    /// another reason.
    ///
    /// # Errors
    ///
    /// As above.
    pub fn acquire_within(&mut self, wait: Duration) -> Result<(), Option<io::Error>> {
        let started = Instant::now();
        loop {
            match self.try_acquire() {
                Ok(true) => return Ok(()),
                Ok(false) if started.elapsed() >= wait => return Err(None),
                Ok(false) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => return Err(Some(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> (std::path::PathBuf, File) {
        let dir = std::env::temp_dir().join(format!("incu-oslock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lock");
        let handle = open(&path);
        (dir, handle)
    }

    fn open(path: &std::path::Path) -> File {
        File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap()
    }

    #[test]
    fn a_lock_is_held_until_its_handle_goes_and_a_second_handle_cannot_have_it() {
        let (dir, first) = file("held");
        let mut first = OsLock::new(first);
        assert!(first.try_acquire().unwrap());
        let mut second = OsLock::new(open(&dir.join("lock")));
        assert!(!second.try_acquire().unwrap(), "two holders");
        // The holder goes without unlocking, as a crash does: the OS lets go.
        drop(first);
        assert!(
            second.try_acquire().unwrap(),
            "the lock outlived its holder"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn waiting_ends_when_the_holder_lets_go_or_when_the_time_is_up() {
        let (dir, first) = file("wait");
        let mut first = OsLock::new(first);
        assert!(first.try_acquire().unwrap());
        let mut waiter = OsLock::new(open(&dir.join("lock")));
        // Held throughout: gives up, saying so (not an OS error).
        let started = Instant::now();
        assert!(matches!(
            waiter.acquire_within(Duration::from_millis(150)),
            Err(None)
        ));
        assert!(started.elapsed() >= Duration::from_millis(150));
        // Let go partway: the waiter gets it.
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(first);
        });
        waiter.acquire_within(Duration::from_secs(5)).unwrap();
        releaser.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
