//! Background shell jobs — spawn, read, list and kill commands that outlive
//! the call that started them.
//!
//! # Output is read as it arrives
//!
//! Each stream is drained by its own thread into a bounded buffer, so a job
//! can be asked what it has said SO FAR — the `BashOutput` tool — and not only
//! once it has ended. A pipe nobody reads also fills and blocks the child, so
//! draining as it goes is what lets a chatty job keep running at all.
//!
//! # The jobs belong to the session
//!
//! One manager per session, shared by every route the session drives (see
//! [`crate::toolbox::SessionTools`]). Dropping it ends every job still running
//! — process tree and all — so nothing a session started outlives it unseen.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use anyhow::{Context, Result};

use super::env_filter;

/// How much of each stream one job keeps. Past it the OLDEST output is
/// dropped, and the count of what was dropped is kept and reported.
pub const MAX_JOB_BUFFER_BYTES: usize = 1024 * 1024;

/// How much new output one read hands back. The rest waits for the next read.
pub const MAX_JOB_READ_BYTES: usize = 30_000;

/// How many jobs may run at once in one session.
pub const MAX_RUNNING_JOBS: usize = 8;

/// How much a pipe is read at a time.
const CHUNK: usize = 8 * 1024;

/// How long a job that has ended is given for its last output to arrive.
const READER_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// One stream's output, bounded, with where the last reader got to.
#[derive(Debug, Default)]
struct Stream {
    bytes: Vec<u8>,
    /// Bytes dropped from the front to stay under [`MAX_JOB_BUFFER_BYTES`].
    dropped: usize,
    /// How far into the stream (counting dropped bytes) a reader has been
    /// shown.
    read_to: usize,
}

impl Stream {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > MAX_JOB_BUFFER_BYTES {
            let excess = self.bytes.len() - MAX_JOB_BUFFER_BYTES;
            self.bytes.drain(..excess);
            self.dropped += excess;
        }
    }

    fn all(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// What arrived since the last read, at most `limit` bytes, and how many
    /// unread bytes were lost to the bound before anyone read them.
    fn take_new(&mut self, limit: usize) -> (String, usize) {
        let lost = self.dropped.saturating_sub(self.read_to);
        let start = self.read_to.max(self.dropped) - self.dropped;
        let end = (start + limit).min(self.bytes.len());
        let text = String::from_utf8_lossy(&self.bytes[start..end]).into_owned();
        self.read_to = self.dropped + end;
        (text, lost)
    }

    fn unread(&self) -> usize {
        (self.dropped + self.bytes.len()).saturating_sub(self.read_to.max(self.dropped))
    }
}

type Shared = Arc<Mutex<Stream>>;

fn lock(stream: &Shared) -> std::sync::MutexGuard<'_, Stream> {
    stream.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Drain `pipe` into `into` until it closes.
fn pump(mut pipe: impl Read + Send + 'static, into: Shared) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = vec![0u8; CHUNK];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => lock(&into).push(&chunk[..n]),
            }
        }
    })
}

/// A background job.
pub struct BashJob {
    pub id: u32,
    pub command: String,
    child: Child,
    pub started_at: Instant,
    pub finished: bool,
    pub exit_code: Option<i32>,
    /// Whether it was stopped by a kill rather than ending on its own.
    killed: bool,
    /// Whether its end has been reported by [`BashJobManager::poll_completed`].
    reported: bool,
    stdout: Shared,
    stderr: Shared,
    readers: Vec<std::thread::JoinHandle<()>>,
    /// What the job's shell started, so a kill ends the command and not just
    /// the shell around it.
    tree: crate::agent::process_tree::Tree,
    /// The sbx engine's proxy, temp directory and `.git` snapshot, released
    /// (and any `.git` change undone) as soon as the job is seen to end.
    sandbox: Option<crate::sandbox::sbx::Keep>,
}

impl BashJob {
    /// Everything it said, stdout then stderr.
    fn output(&self) -> String {
        let out = lock(&self.stdout).all();
        let err = lock(&self.stderr).all();
        if err.is_empty() {
            out
        } else {
            format!("{out}\nSTDERR:\n{err}")
        }
    }

    /// Look once whether it has ended, and settle it if so.
    fn refresh(&mut self) {
        if self.finished {
            return;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.finished = true;
                self.exit_code = status.code();
                // The pipes close when the process ends, and the readers then
                // put the last bytes in the buffer. Waited on for a moment
                // only: something the job started in the background can hold
                // a pipe open for ever, and a read must not hang on it.
                let deadline = Instant::now() + READER_GRACE;
                while Instant::now() < deadline && self.readers.iter().any(|r| !r.is_finished()) {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                self.readers.clear();
                self.sandbox = None;
            }
            Ok(None) => {}
            Err(_) => {
                self.finished = true;
                self.exit_code = Some(-1);
                self.sandbox = None;
            }
        }
    }

    /// Where the job stands, in words.
    fn state(&self) -> String {
        match (self.finished, self.killed, self.exit_code) {
            (false, _, _) => format!("running for {}s", self.started_at.elapsed().as_secs()),
            (true, true, _) => "killed".to_string(),
            (true, false, Some(code)) => format!("exited with code {code}"),
            (true, false, None) => "ended without an exit code".to_string(),
        }
    }

    fn kill(&mut self) {
        self.tree.kill(&mut self.child);
        // `kill` only sends the signal. Without reaping the child it stays a
        // zombie in the process table for as long as the session runs.
        let _ = self.child.wait();
        self.finished = true;
        self.killed = true;
        self.exit_code = Some(-9);
    }
}

/// What one read of a job found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRead {
    pub stdout: String,
    pub stderr: String,
    /// Where the job stands after the read.
    pub state: String,
    pub running: bool,
    /// Unread bytes that were dropped to the buffer's bound before this read.
    pub lost: usize,
    /// Unread bytes still waiting after this read.
    pub more: usize,
}

/// Manages the background jobs of one session.
pub struct BashJobManager {
    jobs: Vec<BashJob>,
    next_id: u32,
    /// Whether to filter environment variables for subprocesses.
    pub env_filter_enabled: bool,
}

impl BashJobManager {
    pub fn new() -> Self {
        Self {
            jobs: Vec::new(),
            next_id: 1,
            env_filter_enabled: true,
        }
    }

    /// Spawn a new background job. Returns the job ID.
    ///
    /// # Errors
    ///
    /// [`MAX_RUNNING_JOBS`] are already running, or the shell could not start.
    pub fn spawn(
        &mut self,
        command: &str,
        workspace: &Path,
        sandbox: &crate::sandbox::Sandbox,
    ) -> Result<u32> {
        let running = self.running_count();
        if running >= MAX_RUNNING_JOBS {
            anyhow::bail!(
                "{running} background jobs are already running, which is the limit; \
                 wait for one or stop one first"
            );
        }

        // Under the session's OS sandbox where the platform can confine it,
        // through the plain shell where it cannot — `wrap` decides. The shell
        // is the Bash tool's own, chosen for the sandbox.
        let shell =
            super::shell_choice::for_sandbox(sandbox).map_err(|why| anyhow::anyhow!(why))?;
        let env = self.env_filter_enabled.then(env_filter::filtered_env);
        let crate::sandbox::Wrapped {
            command: mut cmd_builder,
            keep,
        } = sandbox
            .wrap(&shell, command, env.as_ref())
            .map_err(|why| anyhow::anyhow!(why))?;

        cmd_builder
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        crate::agent::process_tree::prepare(&mut cmd_builder);
        let mut child = cmd_builder
            .spawn()
            .context("failed to spawn background job")?;
        let tree = crate::agent::process_tree::Tree::adopt(&child);

        let stdout: Shared = Arc::default();
        let stderr: Shared = Arc::default();
        let mut readers = Vec::new();
        if let Some(pipe) = child.stdout.take() {
            readers.push(pump(pipe, Arc::clone(&stdout)));
        }
        if let Some(pipe) = child.stderr.take() {
            readers.push(pump(pipe, Arc::clone(&stderr)));
        }

        let id = self.next_id;
        self.next_id += 1;
        self.jobs.push(BashJob {
            id,
            command: command.to_string(),
            child,
            started_at: Instant::now(),
            finished: false,
            exit_code: None,
            killed: false,
            reported: false,
            stdout,
            stderr,
            readers,
            tree,
            sandbox: Some(keep),
        });
        Ok(id)
    }

    /// Settle every job that has ended since the last look.
    fn refresh(&mut self) {
        for job in &mut self.jobs {
            job.refresh();
        }
    }

    /// Jobs that ended since the last call: `(id, output snippet, exit code)`.
    pub fn poll_completed(&mut self) -> Vec<(u32, String, i32)> {
        self.refresh();
        let mut newly_done = Vec::new();
        for job in &mut self.jobs {
            if !job.finished || job.reported {
                continue;
            }
            job.reported = true;
            let output = job.output();
            let snippet = if output.len() > 200 {
                format!("{}...", crate::text::head_bytes(&output, 200))
            } else {
                output
            };
            newly_done.push((job.id, snippet, job.exit_code.unwrap_or(-1)));
        }
        newly_done
    }

    /// What job `id` has said since it was last read, and where it stands.
    ///
    /// `None` when no job has that id.
    pub fn read_new(&mut self, id: u32) -> Option<JobRead> {
        let job = self.jobs.iter_mut().find(|j| j.id == id)?;
        job.refresh();
        let (stdout, lost_out) = lock(&job.stdout).take_new(MAX_JOB_READ_BYTES);
        let room = MAX_JOB_READ_BYTES.saturating_sub(stdout.len());
        let (stderr, lost_err) = lock(&job.stderr).take_new(room);
        let more = lock(&job.stdout).unread() + lock(&job.stderr).unread();
        Some(JobRead {
            stdout,
            stderr,
            state: job.state(),
            running: !job.finished,
            lost: lost_out + lost_err,
            more,
        })
    }

    /// Kill a running job. Returns true if the job was found and killed.
    pub fn kill(&mut self, id: u32) -> bool {
        self.refresh();
        match self.jobs.iter_mut().find(|j| j.id == id && !j.finished) {
            Some(job) => {
                job.kill();
                true
            }
            None => false,
        }
    }

    /// List all jobs: (id, command, running, elapsed_secs).
    pub fn list(&mut self) -> Vec<(u32, &str, bool, u64)> {
        self.refresh();
        self.jobs
            .iter()
            .map(|j| {
                (
                    j.id,
                    j.command.as_str(),
                    !j.finished,
                    j.started_at.elapsed().as_secs(),
                )
            })
            .collect()
    }

    /// Number of jobs running now — looked at, not remembered: a job that
    /// ended a moment ago is not counted.
    pub fn running_count(&mut self) -> usize {
        self.refresh();
        self.jobs.iter().filter(|j| !j.finished).count()
    }

    /// Set whether env filtering is enabled.
    #[must_use]
    pub fn with_env_filter(mut self, enabled: bool) -> Self {
        self.env_filter_enabled = enabled;
        self
    }
}

impl Default for BashJobManager {
    fn default() -> Self {
        Self::new()
    }
}

/// The session is over: nothing it started keeps running unseen.
impl Drop for BashJobManager {
    fn drop(&mut self) {
        self.refresh();
        for job in self.jobs.iter_mut().filter(|j| !j.finished) {
            job.kill();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A command that keeps running for about `secs` seconds.
    fn long_running(secs: u32) -> String {
        if cfg!(windows) {
            format!("ping -n {} 127.0.0.1 > nul", secs + 1)
        } else {
            format!("sleep {secs}")
        }
    }

    /// No sandbox: these tests are about the job manager, not confinement.
    fn off() -> crate::sandbox::Sandbox {
        crate::sandbox::Sandbox::off()
    }

    fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn job_ids_increment() {
        let mut mgr = BashJobManager::new();
        let id1 = mgr.spawn("echo hello", Path::new("."), &off()).unwrap();
        let id2 = mgr.spawn("echo world", Path::new("."), &off()).unwrap();
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn spawn_and_poll_completes() {
        let mut mgr = BashJobManager::new();
        let _id = mgr
            .spawn("echo test_output", Path::new("."), &off())
            .unwrap();
        let mut completed = Vec::new();
        assert!(wait_until(|| {
            completed.extend(mgr.poll_completed());
            !completed.is_empty()
        }));
        let (id, output, code) = &completed[0];
        assert_eq!(*id, 1);
        assert!(output.contains("test_output"));
        assert_eq!(*code, 0);
        assert!(mgr.poll_completed().is_empty(), "reported once");
    }

    #[test]
    fn list_shows_jobs() {
        let mut mgr = BashJobManager::new();
        mgr.spawn("echo a", Path::new("."), &off()).unwrap();
        let jobs = mgr.list();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].0, 1);
        assert_eq!(jobs[0].1, "echo a");
    }

    #[test]
    fn running_count() {
        let mut mgr = BashJobManager::new();
        assert_eq!(mgr.running_count(), 0);
    }

    /// **A READ SHOWS ONLY WHAT IS NEW**, and a job's end is reported with
    /// its code.
    #[test]
    fn a_read_returns_new_output_once() {
        let mut mgr = BashJobManager::new();
        let id = mgr
            .spawn("echo first_line", Path::new("."), &off())
            .unwrap();
        assert!(wait_until(|| mgr.running_count() == 0));
        let first = mgr.read_new(id).expect("the job exists");
        assert!(first.stdout.contains("first_line"), "{first:?}");
        assert!(!first.running);
        assert_eq!(first.state, "exited with code 0");
        let second = mgr.read_new(id).expect("the job exists");
        assert!(second.stdout.is_empty(), "shown twice: {second:?}");
        assert!(mgr.read_new(999).is_none());
    }

    /// **A KILL ENDS A RUNNING JOB**, and the count falls with it.
    #[test]
    fn a_running_job_is_counted_and_can_be_killed() {
        let mut mgr = BashJobManager::new();
        let id = mgr
            .spawn(&long_running(30), Path::new("."), &off())
            .unwrap();
        assert_eq!(mgr.running_count(), 1);
        assert!(mgr.kill(id));
        assert_eq!(mgr.running_count(), 0);
        assert_eq!(mgr.read_new(id).expect("exists").state, "killed");
        assert!(!mgr.kill(id), "a finished job cannot be killed again");
    }

    /// The bound holds and the loss is counted, never silent.
    #[test]
    fn a_stream_past_its_bound_drops_the_oldest_and_counts_it() {
        let mut stream = Stream::default();
        stream.push(&vec![b'a'; MAX_JOB_BUFFER_BYTES]);
        stream.push(b"tail");
        assert_eq!(stream.bytes.len(), MAX_JOB_BUFFER_BYTES);
        let (text, lost) = stream.take_new(MAX_JOB_READ_BYTES);
        assert_eq!(lost, 4, "the four oldest unread bytes were dropped");
        assert_eq!(text.len(), MAX_JOB_READ_BYTES);
        assert_eq!(stream.unread(), MAX_JOB_BUFFER_BYTES - MAX_JOB_READ_BYTES);
    }

    #[test]
    fn no_more_than_the_limit_run_at_once() {
        let mut mgr = BashJobManager::new();
        for _ in 0..MAX_RUNNING_JOBS {
            mgr.spawn(&long_running(30), Path::new("."), &off())
                .unwrap();
        }
        let refused = mgr
            .spawn("echo one_too_many", Path::new("."), &off())
            .expect_err("over the limit");
        assert!(refused.to_string().contains("limit"), "{refused}");
    }
}
