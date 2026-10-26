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
//!
//! # A worker is given its memory, like a person's session
//!
//! The engine's session-start block rides each new task — every task is a
//! fresh agent conversation (`TaskWorker::brief`), so a block given only to
//! the first would leave the rest without it; a retry of the same task keeps
//! its conversation and is not given it again. Each task's instruction goes
//! through the engine's prompt hook — the past work,
//! topics and code it is about — or, when that hook says nothing, the
//! engine's context pack ([`crate::turn_memory::for_turn`]). Both used to be
//! dropped: the start block was fetched and discarded, the prompt hook never
//! fired, so a `/run` worker started every task knowing nothing memory held.
//! They ride the same per-task channel as the operator's prompt-hook output
//! (`TaskWorker::operator_context`).

use crate::hooks::memory::{MemoryHooks, Speaker};
use crate::ledger::Task;
use crate::runner::TaskWorker;

/// A worker whose work is filed with the memory engine as one session.
pub(crate) struct Remembered<W: TaskWorker> {
    inner: W,
    memory: MemoryHooks,
    /// The operator's hooks; their `user_prompt_submit` sees each task's
    /// instruction, as it sees a person's line.
    operator_hooks: Option<crate::hooks::HookRunner>,
    /// The engine's session-start block, for every new task.
    opening: Option<String>,
    /// The task the opening was last given to: a retry of it keeps its
    /// conversation (`AgentBackendWorker::work`), so it is not given again.
    opened_for: Option<String>,
}

impl<W: TaskWorker> Remembered<W> {
    /// `inner`, as the session `memory` names — whose id the caller has
    /// already given the worker's agents, so their calls are filed under it.
    /// The session starts here; its block rides each new task.
    pub(crate) fn new(inner: W, mut memory: MemoryHooks) -> Self {
        let opening = memory.start("startup");
        Self {
            inner,
            memory,
            operator_hooks: None,
            opening,
            opened_for: None,
        }
    }

    /// With the operator's hooks: each task's instruction goes through their
    /// `user_prompt_submit` before the worker or the engine hears it. Given
    /// by `cli::run_ledger` for the run's worker and for every lane's.
    pub(crate) fn with_operator_hooks(mut self, hooks: Option<crate::hooks::HookRunner>) -> Self {
        self.operator_hooks = hooks;
        self
    }

    /// The worker inside.
    pub(crate) fn inner_mut(&mut self) -> &mut W {
        &mut self.inner
    }
}

impl<W: TaskWorker> TaskWorker for Remembered<W> {
    fn work(&mut self, task: &Task) -> Result<String, String> {
        let instruction = crate::runner::task_instruction(task);
        // The session's opening, for each new task: each is a conversation of
        // its own. Not again for a retry, which resumes the one it had.
        let new_task = self.opened_for.as_deref() != Some(task.id.as_str());
        let mut blocks: Vec<String> = if new_task {
            self.opened_for = Some(task.id.clone());
            self.opening.clone().into_iter().collect()
        } else {
            Vec::new()
        };
        // The operator's `user_prompt_submit` first, as for a person's line:
        // one that refuses the instruction fails this attempt before the
        // worker or the engine hears it; what they print rides the turn.
        if let Some(hooks) = &self.operator_hooks {
            let ran = hooks.user_prompt_submit(
                &instruction,
                self.memory.session(),
                Some(self.memory.cwd()),
            );
            if ran.is_denied() {
                return Err(format!(
                    "not run: the operator's user_prompt_submit hook refused it. {}",
                    ran.messages().join("\n")
                ));
            }
            blocks.extend(
                ran.messages()
                    .iter()
                    .map(|said| crate::hooks::prompt_context_block(said)),
            );
        }
        // The engine hears the instruction as the session's next line — which
        // writes it to the transcript — and hands back what memory holds for
        // it; the context pack when its hook says nothing.
        match self.memory.prompt(&instruction) {
            Some(block) => blocks.push(block),
            None if self.memory.has_engine() => blocks.extend(crate::turn_memory::for_turn(
                &instruction,
                self.memory.cwd(),
                self.memory.session(),
            )),
            None => {}
        }
        if !blocks.is_empty() {
            self.inner.operator_context(&blocks);
        }
        let report = self.inner.work(task);
        // What its tools did, whether or not the task then passed: the engine's
        // recorder learns from it.
        let steps = self.inner.take_steps();
        self.memory.tools_ran(&steps);
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

    /// Writes down whether it worked and what operator context it was given.
    #[derive(Default)]
    struct Heard {
        worked: usize,
        context: Vec<String>,
    }

    impl TaskWorker for Heard {
        fn work(&mut self, _task: &Task) -> Result<String, String> {
            self.worked += 1;
            Ok("done".to_string())
        }
        fn operator_context(&mut self, blocks: &[String]) {
            self.context = blocks.to_vec();
        }
        /// One tool call per task, as an agent worker's would be.
        fn take_steps(&mut self) -> crate::hooks::memory::Said {
            use crate::agent::AgentEvent;
            let mut steps = crate::hooks::memory::Said::default();
            steps.hear(&AgentEvent::ToolUse {
                id: format!("call-{}", self.worked),
                name: "Read".to_string(),
                input: r#"{"path":"a.txt"}"#.to_string(),
            });
            steps.hear(&AgentEvent::ToolResult {
                id: format!("call-{}", self.worked),
                output: "hi".to_string(),
                is_error: false,
            });
            steps
        }
    }

    fn context(text: &str, event: &str) -> String {
        serde_json::json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}})
            .to_string()
    }

    /// **A `/run` WORKER IS GIVEN ITS MEMORY.** The session's opening rides
    /// every new task — each is a fresh conversation — but not a retry, which
    /// resumes its own; every task's instruction goes through the engine's
    /// prompt hook, whose block rides that task. Both were dropped, so a
    /// worker started every task knowing nothing memory held. And what the
    /// worker's tools did reaches the transcript the recorder learns from.
    #[test]
    fn a_worker_is_given_the_opening_and_each_tasks_memory() {
        let dir = tempfile::tempdir().expect("dir");
        let bin = fake::write(
            dir.path(),
            &[
                (
                    "session-start",
                    Reply::exit(0).stdout(&context("OPENING-BLOCK", "SessionStart")),
                ),
                (
                    "user-prompt",
                    Reply::exit(0).stdout(&context("TASK-MEMORY", "UserPromptSubmit")),
                ),
                ("capture", Reply::exit(0)),
                ("session-end", Reply::exit(0)),
            ],
        );
        let engine = fake::engine(bin).capped(std::time::Duration::from_secs(20));
        let memory = MemoryHooks::new(Some(engine), dir.path(), "worker-3");
        let mut worker = Remembered::new(Heard::default(), memory);
        let task = Task::new("t-1", "r-1", "do it");

        assert_eq!(worker.work(&task).as_deref(), Ok("done"));
        let first = worker.inner_mut().context.clone();
        assert!(
            first.iter().any(|b| b.contains("OPENING-BLOCK")),
            "{first:?}"
        );
        assert!(first.iter().any(|b| b.contains("TASK-MEMORY")), "{first:?}");

        // A retry of the same task resumes its conversation: no opening again.
        assert_eq!(worker.work(&task).as_deref(), Ok("done"));
        let retry = worker.inner_mut().context.clone();
        assert!(
            !retry.iter().any(|b| b.contains("OPENING-BLOCK")),
            "a retry already has the opening: {retry:?}"
        );
        assert!(retry.iter().any(|b| b.contains("TASK-MEMORY")), "{retry:?}");

        // A NEW task is a new conversation, and is given the opening again.
        let next = Task::new("t-2", "r-1", "do the next thing");
        assert_eq!(worker.work(&next).as_deref(), Ok("done"));
        let second = worker.inner_mut().context.clone();
        assert!(
            second.iter().any(|b| b.contains("OPENING-BLOCK")),
            "a new task starts a new conversation and needs the opening: {second:?}"
        );

        // The engine heard each instruction as the session's next line.
        let prompts = fake::events(dir.path(), "user-prompt");
        assert_eq!(prompts.len(), 3);
        assert!(prompts[0]["prompt"]
            .as_str()
            .is_some_and(|p| p.contains("do it")));

        // Each task's tool calls are in the transcript, paired by id.
        let transcript =
            std::fs::read_to_string(crate::hooks::memory::transcript_for(dir.path(), "worker-3"))
                .expect("the transcript");
        for n in 1..=3 {
            assert!(
                transcript.contains(&format!("\"id\":\"call-{n}\"")),
                "{transcript}"
            );
            assert!(
                transcript.contains(&format!("\"tool_use_id\":\"call-{n}\"")),
                "{transcript}"
            );
        }
    }

    fn prompt_hook(out: &str, code: i32) -> crate::hooks::HookRunner {
        let command = if cfg!(windows) {
            format!("echo {out}& exit /b {code}")
        } else {
            format!("echo '{out}'; exit {code}")
        };
        crate::hooks::HookRunner::new(crate::hooks::HookConfig {
            user_prompt_submit: vec![command],
            ..crate::hooks::HookConfig::default()
        })
    }

    /// **A `/run` TASK GOES THROUGH THE OPERATOR'S PROMPT HOOK.** Exit 2
    /// fails the attempt with the reason and the worker never hears the
    /// task; otherwise what the hook printed reaches the worker as an
    /// `<operator-hook>` block for that task's turn.
    #[test]
    fn a_task_refused_by_the_prompt_hook_is_not_worked_and_an_allowed_one_carries_its_context() {
        let dir = tempfile::tempdir().expect("dir");
        let task = Task::new("t-1", "r-1", "do it");

        let mut refused = Remembered::new(Heard::default(), MemoryHooks::off(dir.path()))
            .with_operator_hooks(Some(prompt_hook("no deploys today", 2)));
        let why = refused
            .work(&task)
            .expect_err("exit 2 did not refuse the task");
        assert!(why.contains("no deploys today"), "{why}");
        assert_eq!(refused.inner_mut().worked, 0, "a refused task was worked");

        let mut allowed = Remembered::new(Heard::default(), MemoryHooks::off(dir.path()))
            .with_operator_hooks(Some(prompt_hook("ticket ABC-1", 0)));
        assert_eq!(allowed.work(&task).as_deref(), Ok("done"));
        let heard = allowed.inner_mut();
        assert_eq!(heard.worked, 1);
        assert_eq!(heard.context.len(), 1, "{:?}", heard.context);
        assert!(
            heard.context[0].starts_with("<operator-hook")
                && heard.context[0].contains("ticket ABC-1"),
            "{:?}",
            heard.context
        );
    }
}
