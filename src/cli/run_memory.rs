//! A `/run` worker as a memory-engine session of its own.
//!
//! A person's session is started, stopped after each turn and ended by
//! [`crate::hooks::memory::MemoryHooks`]; a ledger worker is the same kind of
//! session, driven by the runner instead of a person. [`Remembered`] brackets
//! a worker: `session-start` when it is made, the task's instruction and the
//! worker's report written to the transcript, `capture` (the stop) after each
//! task, and `session-end` when it is dropped.
//!
//! A `capture` answer to carry on is recorded and NOT followed: whether a
//! task is finished is decided by its check, and a failed task goes round the
//! runner's own retry — a second way to extend a task would be a second judge.

use crate::hooks::memory::{MemoryHooks, Speaker};
use crate::ledger::Task;
use crate::runner::TaskWorker;

/// A worker whose work is filed with the memory engine as one session.
pub(crate) struct Remembered<W: TaskWorker> {
    inner: W,
    memory: MemoryHooks,
}

impl<W: TaskWorker> Remembered<W> {
    /// `inner`, as the session `memory` names — whose id the caller has
    /// already given the worker's agents, so their calls are filed under it.
    /// The session starts here.
    pub(crate) fn new(inner: W, mut memory: MemoryHooks) -> Self {
        // The start block is the engine's context for a person's session;
        // a task's context is the runner's to assemble, so it is not sent.
        let _ = memory.start("startup");
        Self { inner, memory }
    }

    /// The worker inside.
    pub(crate) fn inner_mut(&mut self) -> &mut W {
        &mut self.inner
    }
}

impl<W: TaskWorker> TaskWorker for Remembered<W> {
    fn work(&mut self, task: &Task) -> Result<String, String> {
        self.memory
            .said(Speaker::Person, &crate::runner::task_instruction(task));
        let report = self.inner.work(task);
        if let Ok(said) = &report {
            self.memory.said(Speaker::Agent, said);
            if let Some(reason) = self.memory.stop(said, None) {
                crate::trace!(
                    "memory",
                    "{}: the engine asked to carry on ({reason}); not followed — the task's \
                     check decides",
                    task.id
                );
            }
        }
        report
    }

    fn unenforced(&self) -> &[String] {
        self.inner.unenforced()
    }

    fn handovers(&self) -> &[String] {
        self.inner.handovers()
    }

    fn release(&mut self) {
        self.inner.release();
    }

    fn brief(&mut self, handoffs: &[crate::runner::Handoff]) {
        self.inner.brief(handoffs);
    }

    fn attribution(&self, task: &Task) -> crate::identity::Attribution {
        self.inner.attribution(task)
    }

    fn spent_since_last_call(&mut self) -> crate::runtime::usage::TokenUsage {
        self.inner.spent_since_last_call()
    }
}

impl<W: TaskWorker> Drop for Remembered<W> {
    fn drop(&mut self) {
        self.memory.end("other");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::engine::fake::{self, Reply};

    struct Says;

    impl TaskWorker for Says {
        fn work(&mut self, _task: &Task) -> Result<String, String> {
            Ok("did the thing".to_string())
        }
    }

    /// **A LEDGER WORKER IS A SESSION**: started when it is made, a stop for
    /// each task it works, ended when it is dropped — each under its own id.
    #[test]
    fn a_ledger_worker_gets_session_start_a_stop_per_task_and_session_end() {
        let dir = tempfile::tempdir().expect("dir");
        let bin = fake::write(
            dir.path(),
            &[
                ("session-start", Reply::exit(0)),
                ("capture", Reply::exit(0)),
                ("session-end", Reply::exit(0)),
            ],
        );
        let engine = fake::engine(bin).capped(std::time::Duration::from_secs(20));
        let memory = MemoryHooks::new(Some(engine), dir.path(), "worker-1");
        {
            let mut worker = Remembered::new(Says, memory);
            let task = Task::new("t-1", "r-1", "do it");
            assert_eq!(worker.work(&task).as_deref(), Ok("did the thing"));
            assert_eq!(worker.work(&task).as_deref(), Ok("did the thing"));
        }
        let calls = |sub: &str| fake::calls(dir.path(), sub);
        assert_eq!(calls("session-start"), 1);
        assert_eq!(calls("capture"), 2);
        assert_eq!(calls("session-end"), 1);
        let transcript =
            std::fs::read_to_string(crate::hooks::memory::transcript_for(dir.path(), "worker-1"))
                .expect("the transcript the engine reads");
        assert!(transcript.contains("did the thing"), "{transcript}");
    }

    /// A carry-on answer is recorded and not followed: the task is worked
    /// once.
    #[test]
    fn a_carry_on_answer_is_not_followed() {
        let dir = tempfile::tempdir().expect("dir");
        let bin = fake::write(
            dir.path(),
            &[(
                "capture",
                Reply::exit(0).stdout(r#"{"decision":"block","reason":"keep going"}"#),
            )],
        );
        let engine = fake::engine(bin).capped(std::time::Duration::from_secs(20));
        let mut worker =
            Remembered::new(Says, MemoryHooks::new(Some(engine), dir.path(), "worker-2"));
        let task = Task::new("t-1", "r-1", "do it");
        assert_eq!(worker.work(&task).as_deref(), Ok("did the thing"));
        assert_eq!(fake::calls(dir.path(), "capture"), 1);
        let _ = worker.inner_mut();
    }
}
