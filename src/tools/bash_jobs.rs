//! Background bash job manager — spawn, poll, list, and kill background tasks.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result};

use super::env_filter;

// ---------------------------------------------------------------------------
// BashJob
// ---------------------------------------------------------------------------

/// A background bash job.
pub struct BashJob {
    pub id: u32,
    pub command: String,
    pub child: Child,
    pub started_at: Instant,
    pub finished: bool,
    pub exit_code: Option<i32>,
    pub output: String,
    stdout_handle: Option<std::thread::JoinHandle<Vec<u8>>>,
    stderr_handle: Option<std::thread::JoinHandle<Vec<u8>>>,
    /// What the job's shell started, so `/kill` ends the command and not
    /// just the shell around it.
    tree: crate::agent::process_tree::Tree,
}

// ---------------------------------------------------------------------------
// BashJobManager
// ---------------------------------------------------------------------------

/// Manages background bash jobs.
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
    pub fn spawn(&mut self, command: &str, workspace: &Path) -> Result<u32> {
        let id = self.next_id;
        self.next_id += 1;

        #[cfg(windows)]
        let mut cmd_builder = {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(command);
            c
        };

        #[cfg(not(windows))]
        let mut cmd_builder = {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            c
        };

        cmd_builder
            .current_dir(workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Apply env var filtering if enabled
        if self.env_filter_enabled {
            cmd_builder.env_clear();
            for (key, value) in env_filter::filtered_env() {
                cmd_builder.env(key, value);
            }
        }

        crate::agent::process_tree::prepare(&mut cmd_builder);
        let mut child = cmd_builder
            .spawn()
            .context("failed to spawn background job")?;
        let tree = crate::agent::process_tree::Tree::adopt(&child);

        let stdout = child.stdout.take();
        let stdout_handle = stdout.map(|mut r| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(&mut r, &mut buf);
                buf
            })
        });

        let stderr = child.stderr.take();
        let stderr_handle = stderr.map(|mut r| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(&mut r, &mut buf);
                buf
            })
        });

        self.jobs.push(BashJob {
            id,
            command: command.to_string(),
            child,
            started_at: Instant::now(),
            finished: false,
            exit_code: None,
            output: String::new(),
            stdout_handle,
            stderr_handle,
            tree,
        });

        Ok(id)
    }

    /// Poll all jobs. Returns a list of (id, just_finished, output_snippet) for newly completed jobs.
    pub fn poll_completed(&mut self) -> Vec<(u32, String, i32)> {
        let mut newly_done = Vec::new();

        for job in &mut self.jobs {
            if job.finished {
                continue;
            }

            match job.child.try_wait() {
                Ok(Some(status)) => {
                    job.finished = true;
                    job.exit_code = status.code();

                    let stdout_buf = job
                        .stdout_handle
                        .take()
                        .and_then(|h| h.join().ok())
                        .unwrap_or_default();
                    let stderr_buf = job
                        .stderr_handle
                        .take()
                        .and_then(|h| h.join().ok())
                        .unwrap_or_default();
                    let stdout_str = String::from_utf8_lossy(&stdout_buf);
                    let stderr_str = String::from_utf8_lossy(&stderr_buf);
                    job.output = if stderr_str.is_empty() {
                        stdout_str.to_string()
                    } else {
                        format!("{stdout_str}\nSTDERR:\n{stderr_str}")
                    };

                    let snippet = if job.output.len() > 200 {
                        format!("{}...", crate::text::head_bytes(&job.output, 200))
                    } else {
                        job.output.clone()
                    };

                    newly_done.push((job.id, snippet, job.exit_code.unwrap_or(-1)));
                }
                Ok(None) => {} // still running
                Err(_) => {
                    job.finished = true;
                    job.exit_code = Some(-1);
                    let _ = job.stdout_handle.take();
                    let _ = job.stderr_handle.take();
                }
            }
        }

        newly_done
    }

    /// Get the output of a completed job.
    pub fn get_output(&self, id: u32) -> Option<&str> {
        self.jobs
            .iter()
            .find(|j| j.id == id && j.finished)
            .map(|j| j.output.as_str())
    }

    /// Kill a running job. Returns true if the job was found and killed.
    pub fn kill(&mut self, id: u32) -> bool {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id && !j.finished) {
            job.tree.kill(&mut job.child);
            // `kill` only sends the signal. Without reaping the child it stays
            // a zombie in the process table for as long as the CLI runs, and a
            // long session that kills jobs leaks one entry per job.
            let _ = job.child.wait();
            let _ = job.stdout_handle.take();
            let _ = job.stderr_handle.take();
            job.finished = true;
            job.exit_code = Some(-9);
            job.output = "(killed)".to_string();
            true
        } else {
            false
        }
    }

    /// List all jobs: (id, command, running, elapsed_secs).
    pub fn list(&self) -> Vec<(u32, &str, bool, u64)> {
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

    /// Number of currently running jobs.
    pub fn running_count(&self) -> usize {
        self.jobs.iter().filter(|j| !j.finished).count()
    }
}

impl Default for BashJobManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BashJobManager {
    /// Set whether env filtering is enabled.
    #[must_use]
    pub fn with_env_filter(mut self, enabled: bool) -> Self {
        self.env_filter_enabled = enabled;
        self
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_ids_increment() {
        let mut mgr = BashJobManager::new();
        let id1 = mgr.spawn("echo hello", Path::new(".")).unwrap();
        let id2 = mgr.spawn("echo world", Path::new(".")).unwrap();
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn spawn_and_poll_completes() {
        let mut mgr = BashJobManager::new();
        let _id = mgr.spawn("echo test_output", Path::new(".")).unwrap();

        // Wait a bit for the process to complete
        std::thread::sleep(std::time::Duration::from_millis(500));

        let completed = mgr.poll_completed();
        assert!(!completed.is_empty());
        let (id, output, code) = &completed[0];
        assert_eq!(*id, 1);
        assert!(output.contains("test_output"));
        assert_eq!(*code, 0);
    }

    #[test]
    fn list_shows_jobs() {
        let mut mgr = BashJobManager::new();
        mgr.spawn("echo a", Path::new(".")).unwrap();
        let jobs = mgr.list();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].0, 1);
        assert_eq!(jobs[0].1, "echo a");
    }

    #[test]
    fn running_count() {
        let mgr = BashJobManager::new();
        assert_eq!(mgr.running_count(), 0);
    }
}
