//! The memory engine's lifecycle hooks, run by this harness for its own tools.
//!
//! # Why the harness runs them
//!
//! The engine's guard — backups checked before a write, secrets scanned, a
//! card raised for the owner, a question routed to the owner's app — is
//! delivered as hook subcommands of the engine binary: `hook pre-tool`,
//! `hook permission-request` and the rest. Something has to call them at the
//! moment a tool is about to run. For the tools in [`crate::toolbox`] that
//! something is this harness, because the Toolbox IS the tool: nothing else
//! stands between the model asking and the file changing.
//!
//! # The contract, per call
//!
//! ```text
//! spawn:  <engine> hook <event>        (no agent flag: the engine's default contract)
//! cwd:    the workspace
//! stdin:  PIPED — the event JSON is written, then the pipe is closed
//! stdout: piped, read per event
//! stderr: piped — for `pre-tool` exit 2 it IS the reason
//! wait:   bounded, per event, never shorter than the engine registers itself
//! ```
//!
//! stdin is never inherited. Under `incode --acp` the inherited stdin is the
//! protocol stream (see [`crate::acp_agent`]); a child that read it would eat
//! the client's messages.
//!
//! # Where the binary is
//!
//! 1. [`ENGINE_BIN_ENV`], when set.
//! 2. `engine_bin` in the operator's settings ([`crate::admin::AdminConfig`]).
//! 3. The engine's install location. On Windows that is where its installer
//!    runs from, `%LOCALAPPDATA%\Incord Memory\incord-memory-service.exe` —
//!    the path it writes into the hooks of every agent it installs for — and
//!    then the spaceless copy it makes at `%USERPROFILE%\.incord\` for agents
//!    whose shell cannot quote a path. No default is known elsewhere.
//!
//! A path the operator NAMED is used even when it does not exist: the spawn
//! then fails, and that failure is the guard's failure policy (see
//! [`crate::toolbox`]) — an operator who asked for the guard is not quietly
//! run without it. When nothing was named and nothing is installed, the hooks
//! are skipped: the engine is a plugin, and the harness works without it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{json, Value};

use crate::runtime::cancel::CancelToken;

/// Names the engine binary, ahead of the settings and the install location.
pub const ENGINE_BIN_ENV: &str = "INCORD_ENGINE_BIN";

/// Added to every bound, so the engine's own deadline fires before ours and it
/// gets to say what it decided rather than being cut off mid-answer.
const MARGIN: Duration = Duration::from_secs(10);

/// `hook pre-tool`. The engine registers it for 600 s, and waits up to 500 s
/// inside it for the owner to answer a question.
pub const PRE_TOOL_BOUND: Duration = Duration::from_secs(600 + MARGIN.as_secs());

/// `hook permission-request`. Registered for 600 s; the engine itself holds
/// the prompt for at most 540 s.
pub const PERMISSION_REQUEST_BOUND: Duration = Duration::from_secs(600 + MARGIN.as_secs());

/// `hook capture`, the stop channel. Registered for 600 s: the engine may hold
/// a stop open while the owner decides whether the agent carries on.
pub const CAPTURE_BOUND: Duration = Duration::from_secs(600 + MARGIN.as_secs());

/// `hook session-end`. Registered for 120 s: it may write a handoff for the
/// session, which is a model call.
pub const SESSION_END_BOUND: Duration = Duration::from_secs(120 + MARGIN.as_secs());

/// `hook session-start`, `hook user-prompt` and `hook code-edit`. The engine
/// registers no timeout for these three, so the agent's default applies; its
/// own hooks file for the one agent that needs a number says 60.
pub const CONTEXT_BOUND: Duration = Duration::from_secs(60 + MARGIN.as_secs());

/// One hook this harness runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Before a tool runs: allow, block, or ask a person.
    PreTool,
    /// A person is being asked about a call: the owner's app may answer first.
    PermissionRequest,
    /// A session opened, or was cleared: the engine hands back its context.
    SessionStart,
    /// A person sent a line: the engine hands back the context for it.
    UserPrompt,
    /// A file was written or edited, and the write landed.
    CodeEdit,
    /// The turn is ending: the engine records it, and may say to carry on.
    Capture,
    /// The session is over.
    SessionEnd,
}

impl Event {
    fn subcommand(self) -> &'static str {
        match self {
            Self::PreTool => "pre-tool",
            Self::PermissionRequest => "permission-request",
            Self::SessionStart => "session-start",
            Self::UserPrompt => "user-prompt",
            Self::CodeEdit => "code-edit",
            Self::Capture => "capture",
            Self::SessionEnd => "session-end",
        }
    }

    /// The name the event carries, as the engine's default contract spells it.
    fn name(self) -> &'static str {
        match self {
            Self::PreTool => "PreToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::SessionStart => "SessionStart",
            Self::UserPrompt => "UserPromptSubmit",
            Self::CodeEdit => "PostToolUse",
            Self::Capture => "Stop",
            Self::SessionEnd => "SessionEnd",
        }
    }

    fn bound(self) -> Duration {
        match self {
            Self::PreTool => PRE_TOOL_BOUND,
            Self::PermissionRequest => PERMISSION_REQUEST_BOUND,
            Self::Capture => CAPTURE_BOUND,
            Self::SessionEnd => SESSION_END_BOUND,
            Self::SessionStart | Self::UserPrompt | Self::CodeEdit => CONTEXT_BOUND,
        }
    }
}

/// The conversation a lifecycle event belongs to, as every event names it.
#[derive(Debug, Clone, Copy)]
pub struct Conversation<'a> {
    /// incode's id for the session — the same one the engine's pre-tool
    /// events carry, so its cards, its capture and its context agree.
    pub session: &'a str,
    /// The workspace.
    pub cwd: &'a Path,
    /// The session's transcript, in the shape the engine reads, when one is
    /// being written. The engine records the conversation from it.
    pub transcript: Option<&'a Path>,
}

/// The engine binary, once found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Engine {
    bin: PathBuf,
    /// A shorter wait than the event's own. Only a test sets it: the real
    /// bounds are ten minutes.
    cap: Option<Duration>,
}

/// What one hook process did.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ran {
    Exited {
        code: i32,
        stdout: String,
        stderr: String,
    },
    /// It could not be started, ran past its bound, was abandoned, or ended
    /// without an exit code.
    Failed(String),
}

/// The engine's answer before a tool runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreTool {
    /// Run it. `context` is guidance for this action, for the model to read.
    Allow { context: Option<String> },
    /// A person must confirm it first. `reason` is what to show them.
    Ask {
        reason: String,
        context: Option<String>,
    },
    /// Do not run it. The text is the engine's reason (exit 2, stderr).
    Block(String),
    /// The engine could not decide. What that means is the caller's policy.
    Failed(String),
}

/// The engine's answer to a pending confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permission {
    Allow,
    Deny(String),
    /// The owner's app did not answer; the prompt is the person's to answer.
    NoAnswer,
}

/// One call that needs a person's confirmation.
pub struct Ask<'a> {
    /// The model's id for the call, when there is one.
    pub call_id: &'a str,
    pub tool_name: &'a str,
    /// Exactly what the engine was shown, so the answer is about this call.
    pub tool_input: &'a Value,
    /// Why the engine wants a person: what to show them.
    pub reason: &'a str,
}

/// What was decided about a call that needed confirming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Refused, with the words the model is given.
    Deny(String),
}

/// Someone who can be asked, where the session runs.
///
/// The REPL asks at the terminal; `incode --acp` asks the client. A run with
/// nobody there has no prompter at all, and a call that needs confirming is
/// then decided by the owner's app or refused.
pub trait Prompter: Send + Sync {
    /// Ask whether this call may run.
    ///
    /// `settled` turns true once the answer has come from somewhere else — the
    /// owner's app — or the turn was stopped. Stop asking then and return
    /// `None`. `None` also means nobody here could answer.
    fn confirm(&self, ask: &Ask<'_>, settled: &dyn Fn() -> bool) -> Option<Verdict>;

    /// Put these questions to the person, one answer each, in order. `None`
    /// when nobody here can answer them.
    fn choose(&self, questions: &[crate::ask::Question]) -> Option<Vec<crate::ask::Answer>>;
}

impl Engine {
    /// The engine this harness should run its hooks through, if any.
    ///
    /// Resolved once per backend; the result — including "none" — is traced,
    /// so a session that quietly ran without the guard can be told apart from
    /// one the guard allowed.
    #[must_use]
    pub fn locate() -> Option<Self> {
        let named = std::env::var(ENGINE_BIN_ENV).ok();
        let configured = crate::admin::AdminConfig::load_default().engine_bin;
        locate_from(named, configured, &default_locations())
    }

    /// The engine at `bin`.
    fn at(bin: PathBuf) -> Self {
        Self { bin, cap: None }
    }

    /// The same engine, never waiting longer than `cap`.
    #[cfg(test)]
    pub(crate) fn capped(mut self, cap: Duration) -> Self {
        self.cap = Some(cap);
        self
    }

    /// Ask the engine about a call before it runs.
    #[must_use]
    pub fn pre_tool(
        &self,
        session: &str,
        cwd: &Path,
        tool_name: &str,
        tool_input: &Value,
        cancel: Option<&CancelToken>,
    ) -> PreTool {
        let payload = payload(Event::PreTool, session, cwd, tool_name, tool_input);
        let abandon = || cancel.is_some_and(CancelToken::is_cancelled);
        read_pre_tool(self.run(Event::PreTool, &payload, cwd, &abandon))
    }

    /// Decide a call the engine said needs a person.
    ///
    /// `hook permission-request` runs for the same call — the engine answers
    /// it the moment the owner answers the card `pre-tool` raised — while the
    /// `prompter`, if there is one, asks here. The first answer wins, and the
    /// loser is stopped: the hook's process tree is killed, the prompt told to
    /// stand down.
    ///
    /// With no prompter, or one that could not ask, only the hook can answer;
    /// no answer is a refusal carrying the engine's reason. A stopped turn is
    /// a refusal too.
    #[must_use]
    pub fn settle(
        &self,
        ask: &Ask<'_>,
        session: &str,
        cwd: &Path,
        prompter: Option<&dyn Prompter>,
        cancel: Option<&CancelToken>,
    ) -> Verdict {
        let event = payload(
            Event::PermissionRequest,
            session,
            cwd,
            ask.tool_name,
            ask.tool_input,
        );
        let stop = Arc::new(AtomicBool::new(false));
        let slot: Arc<Mutex<Option<Permission>>> = Arc::default();
        let hook = {
            let engine = self.clone();
            let cwd = cwd.to_path_buf();
            let stop = Arc::clone(&stop);
            let slot = Arc::clone(&slot);
            let cancel = cancel.cloned();
            std::thread::spawn(move || {
                let abandon = || {
                    stop.load(Ordering::SeqCst)
                        || cancel.as_ref().is_some_and(CancelToken::is_cancelled)
                };
                let answer =
                    read_permission(engine.run(Event::PermissionRequest, &event, &cwd, &abandon));
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(answer);
            })
        };

        let cancelled = || cancel.is_some_and(CancelToken::is_cancelled);
        let hook_decided = || {
            matches!(
                *slot.lock().unwrap_or_else(PoisonError::into_inner),
                Some(Permission::Allow | Permission::Deny(_))
            )
        };
        let local = prompter.and_then(|p| p.confirm(ask, &|| hook_decided() || cancelled()));
        if local.is_some() || cancelled() {
            stop.store(true, Ordering::SeqCst);
        }
        // Prompt once stopped: the waiter looks at the flag every 50 ms. With
        // no answer here yet, this is the wait for the owner's app.
        let _ = hook.join();

        if cancelled() {
            return Verdict::Deny(
                "NOT RUN — the turn was stopped before this call was confirmed.".to_string(),
            );
        }
        if let Some(verdict) = local {
            crate::trace!("engine", "{} confirmed here: {verdict:?}", ask.tool_name);
            return verdict;
        }
        let answer = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
        crate::trace!(
            "engine",
            "{} answered by the owner's app: {answer:?}",
            ask.tool_name
        );
        match answer {
            Some(Permission::Allow) => Verdict::Allow,
            Some(Permission::Deny(message)) => Verdict::Deny(message),
            _ => Verdict::Deny(format!(
                "NOT RUN — this call needs a person's confirmation and nobody gave it: {}",
                ask.reason
            )),
        }
    }

    /// The context the engine hands a session as it opens: `source` is
    /// `startup`, `resume` or `clear`. `None` when it had nothing to add, or
    /// could not be asked — a session starts either way.
    #[must_use]
    pub fn session_start(&self, at: Conversation<'_>, source: &str) -> Option<String> {
        let event = lifecycle(Event::SessionStart, at, json!({ "source": source }));
        read_context(self.run(Event::SessionStart, &event, at.cwd, &|| false))
    }

    /// The context the engine hands this turn, for the line the person sent.
    #[must_use]
    pub fn user_prompt(&self, at: Conversation<'_>, prompt: &str) -> Option<String> {
        let event = lifecycle(Event::UserPrompt, at, json!({ "prompt": prompt }));
        read_context(self.run(Event::UserPrompt, &event, at.cwd, &|| false))
    }

    /// Tell the engine a write landed, and do not wait for it.
    ///
    /// Bookkeeping, not a decision: the engine files the path under the
    /// session and queues the file to be read again. It runs on a thread of its
    /// own, bounded like the rest, so the model's next call is not held up by
    /// it and a stuck engine is still killed at the bound.
    pub fn code_edit(&self, at: Conversation<'_>, tool_name: &str, tool_input: &Value, said: &str) {
        let event = lifecycle(
            Event::CodeEdit,
            at,
            json!({
                "tool_name": tool_name,
                "tool_input": tool_input,
                "tool_response": said,
            }),
        );
        let engine = self.clone();
        let cwd = at.cwd.to_path_buf();
        std::thread::spawn(move || {
            let _ = engine.run(Event::CodeEdit, &event, &cwd, &|| false);
        });
    }

    /// The turn is ending: the engine records it, and may answer that the
    /// agent should carry on — with the words it is to carry on with.
    ///
    /// `active` is true on a stop reached after a continuation this hook
    /// ordered, which is the engine's own brake. A stopped turn abandons the
    /// wait: a person who pressed stop is not held for ten minutes.
    #[must_use]
    pub fn stop(
        &self,
        at: Conversation<'_>,
        last_message: &str,
        active: bool,
        cancel: Option<&CancelToken>,
    ) -> Option<String> {
        let event = lifecycle(
            Event::Capture,
            at,
            json!({
                "last_assistant_message": last_message,
                "stop_hook_active": active,
            }),
        );
        let abandon = || cancel.is_some_and(CancelToken::is_cancelled);
        read_stop(self.run(Event::Capture, &event, at.cwd, &abandon))
    }

    /// The session is over. `reason` is why: `clear`, `prompt_input_exit`,
    /// `other`.
    pub fn session_end(&self, at: Conversation<'_>, reason: &str) {
        let event = lifecycle(Event::SessionEnd, at, json!({ "reason": reason }));
        let _ = self.run(Event::SessionEnd, &event, at.cwd, &|| false);
    }

    fn run(&self, event: Event, payload: &Value, cwd: &Path, abandon: &dyn Fn() -> bool) -> Ran {
        let bound = self.cap.unwrap_or_else(|| event.bound());
        let began = std::time::Instant::now();
        let ran = self.run_within(event, payload, cwd, bound, abandon);
        crate::trace!(
            "engine",
            "hook {} in {:?}: {}",
            event.subcommand(),
            began.elapsed(),
            match &ran {
                Ran::Exited { code, .. } => format!("exit {code}"),
                Ran::Failed(why) => format!("FAILED: {why}"),
            }
        );
        ran
    }

    fn run_within(
        &self,
        event: Event,
        payload: &Value,
        cwd: &Path,
        bound: Duration,
        abandon: &dyn Fn() -> bool,
    ) -> Ran {
        let mut command = Command::new(&self.bin);
        command.arg("hook").arg(event.subcommand()).current_dir(cwd);
        #[cfg(test)]
        fake::note_spawn(&self.bin, event.subcommand());
        // The event on a piped stdin that is then closed — a Write carries
        // the whole file — through the runner the operator's hooks share.
        let mut bytes = payload.to_string().into_bytes();
        bytes.push(b'\n');
        match super::run_piped(command, bytes, bound, abandon) {
            Ok(output) => match output.status.code() {
                Some(code) => Ran::Exited {
                    code,
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                },
                None => Ran::Failed("the engine ended without an exit code".to_string()),
            },
            Err(super::Piped::Start(e)) => {
                Ran::Failed(format!("could not start `{}`: {e}", self.bin.display()))
            }
            Err(super::Piped::Wait(why)) => Ran::Failed(why),
        }
    }
}

/// The resolution order, with the environment and the settings handed in.
fn locate_from(
    named: Option<String>,
    configured: Option<String>,
    defaults: &[PathBuf],
) -> Option<Engine> {
    let chosen = |value: Option<String>| {
        value
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    if let Some(bin) = chosen(named) {
        crate::trace!("engine", "engine from {ENGINE_BIN_ENV}: {bin}");
        return Some(Engine::at(PathBuf::from(bin)));
    }
    if let Some(bin) = chosen(configured) {
        crate::trace!("engine", "engine from the settings: {bin}");
        return Some(Engine::at(PathBuf::from(bin)));
    }
    match defaults.iter().find(|path| path.is_file()) {
        Some(found) => {
            crate::trace!(
                "engine",
                "engine at its install location: {}",
                found.display()
            );
            Some(Engine::at(found.clone()))
        }
        None => {
            crate::trace!(
                "engine",
                "no engine binary named or installed (looked at {defaults:?}); its hooks are skipped"
            );
            None
        }
    }
}

/// Where the engine installs itself, most authoritative first.
fn default_locations() -> Vec<PathBuf> {
    let mut out = Vec::new();
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            out.push(
                PathBuf::from(local)
                    .join("Incord Memory")
                    .join("incord-memory-service.exe"),
            );
        }
        if let Some(home) = std::env::var_os("USERPROFILE") {
            out.push(
                PathBuf::from(home)
                    .join(".incord")
                    .join("incord-memory-service.exe"),
            );
        }
    }
    out
}

/// The event JSON: the fields every event carries, then the call.
fn payload(event: Event, session: &str, cwd: &Path, tool_name: &str, tool_input: &Value) -> Value {
    json!({
        "hook_event_name": event.name(),
        "session_id": session,
        "cwd": cwd.to_string_lossy(),
        "tool_name": tool_name,
        "tool_input": tool_input,
    })
}

/// A lifecycle event: the fields every event carries, then its own.
fn lifecycle(event: Event, at: Conversation<'_>, own: Value) -> Value {
    let mut out = json!({
        "hook_event_name": event.name(),
        "session_id": at.session,
        "cwd": at.cwd.to_string_lossy(),
    });
    if let Some(path) = at.transcript {
        out["transcript_path"] = json!(path.to_string_lossy());
    }
    if let (Some(fields), Value::Object(own)) = (out.as_object_mut(), own) {
        fields.extend(own);
    }
    out
}

/// The `additionalContext` a session-start or user-prompt hook printed.
/// Anything else — nothing, not JSON, a failure — is no context.
fn read_context(ran: Ran) -> Option<String> {
    let Ran::Exited {
        code: 0, stdout, ..
    } = ran
    else {
        return None;
    };
    let reply: Value = serde_json::from_str(stdout.trim()).ok()?;
    text_at(&reply["hookSpecificOutput"], "additionalContext").map(str::to_string)
}

/// `{"decision":"block","reason":R}` is the engine saying the agent should
/// carry on with `R`. Anything else lets the turn end: a stop hook never
/// keeps a turn alive by failing.
fn read_stop(ran: Ran) -> Option<String> {
    let Ran::Exited {
        code: 0, stdout, ..
    } = ran
    else {
        return None;
    };
    let reply: Value = serde_json::from_str(stdout.trim()).ok()?;
    if reply.get("decision").and_then(Value::as_str) != Some("block") {
        return None;
    }
    text_at(&reply, "reason").map(str::to_string)
}

/// A non-blank string field.
fn text_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn read_pre_tool(ran: Ran) -> PreTool {
    match ran {
        Ran::Failed(why) => PreTool::Failed(why),
        Ran::Exited {
            code: 2,
            stdout,
            stderr,
        } => {
            let reason = [stderr.trim(), stdout.trim()]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or("the Incord guard refused this call and gave no reason");
            PreTool::Block(reason.to_string())
        }
        Ran::Exited {
            code: 0, stdout, ..
        } => {
            let said = stdout.trim();
            if said.is_empty() {
                return PreTool::Allow { context: None };
            }
            let Ok(reply) = serde_json::from_str::<Value>(said) else {
                // Plain text on success carries no decision in the default
                // contract. Kept in the trace, not handed to the model.
                crate::trace!("engine", "pre-tool said something that is not JSON: {said}");
                return PreTool::Allow { context: None };
            };
            let out = reply.get("hookSpecificOutput").unwrap_or(&Value::Null);
            let context = text_at(out, "additionalContext").map(str::to_string);
            let reason = text_at(out, "permissionDecisionReason");
            match out.get("permissionDecision").and_then(Value::as_str) {
                Some("ask") => PreTool::Ask {
                    reason: reason
                        .unwrap_or("the Incord guard wants a person to confirm this call")
                        .to_string(),
                    context,
                },
                Some("deny") => PreTool::Block(
                    reason
                        .unwrap_or("the Incord guard refused this call and gave no reason")
                        .to_string(),
                ),
                _ => PreTool::Allow { context },
            }
        }
        Ran::Exited { code, stderr, .. } => PreTool::Failed(format!(
            "the engine's pre-tool hook exited {code}: {}",
            stderr.trim()
        )),
    }
}

/// Only stdout decides: exit 2 is not part of this hook's contract.
fn read_permission(ran: Ran) -> Permission {
    let stdout = match ran {
        Ran::Exited { stdout, .. } => stdout,
        Ran::Failed(_) => return Permission::NoAnswer,
    };
    let Ok(reply) = serde_json::from_str::<Value>(stdout.trim()) else {
        return Permission::NoAnswer;
    };
    let decision = &reply["hookSpecificOutput"]["decision"];
    match decision.get("behavior").and_then(Value::as_str) {
        Some("allow") => Permission::Allow,
        Some("deny") => Permission::Deny(
            text_at(decision, "message")
                .unwrap_or("the owner refused this call")
                .to_string(),
        ),
        _ => Permission::NoAnswer,
    }
}

/// A scripted stand-in for the engine binary, for the tests here and in the
/// Toolbox and the ACP agent.
///
/// A batch file on Windows and a shell script elsewhere. For each subcommand it
/// appends the event it was given on stdin to `<subcommand>.stdin`, a line to
/// `<subcommand>.calls`, prints `<subcommand>.out` and `<subcommand>.err`, and
/// exits with the code it was given. Reading stdin to its end is what proves
/// the pipe was closed: left open, the script waits and the call times out.
#[cfg(test)]
pub(crate) mod fake {
    use std::path::{Path, PathBuf};

    /// What the fake does for one subcommand.
    pub(crate) struct Reply {
        pub(crate) stdout: String,
        pub(crate) stderr: String,
        pub(crate) code: i32,
        /// Seconds to wait before answering.
        pub(crate) sleep: u32,
    }

    impl Reply {
        pub(crate) fn exit(code: i32) -> Self {
            Self {
                stdout: String::new(),
                stderr: String::new(),
                code,
                sleep: 0,
            }
        }

        pub(crate) fn stdout(mut self, text: &str) -> Self {
            self.stdout = text.to_string();
            self
        }

        pub(crate) fn stderr(mut self, text: &str) -> Self {
            self.stderr = text.to_string();
            self
        }

        pub(crate) fn after(mut self, seconds: u32) -> Self {
            self.sleep = seconds;
            self
        }
    }

    /// Write the fake into `dir`; returns the path to run.
    pub(crate) fn write(dir: &Path, replies: &[(&str, Reply)]) -> PathBuf {
        for (sub, reply) in replies {
            std::fs::write(dir.join(format!("{sub}.out")), &reply.stdout).expect("fake stdout");
            std::fs::write(dir.join(format!("{sub}.err")), &reply.stderr).expect("fake stderr");
        }
        #[cfg(windows)]
        {
            let mut script = String::from("@echo off\r\n");
            for (sub, reply) in replies {
                let wait = if reply.sleep > 0 {
                    format!("ping -n {} 127.0.0.1 >nul\r\n", reply.sleep + 1)
                } else {
                    String::new()
                };
                script.push_str(&format!(
                    "if \"%2\"==\"{sub}\" (\r\n\
                     echo x>>\"%~dp0{sub}.calls\"\r\n\
                     findstr \"^\" >>\"%~dp0{sub}.stdin\"\r\n\
                     {wait}\
                     type \"%~dp0{sub}.out\"\r\n\
                     type \"%~dp0{sub}.err\" 1>&2\r\n\
                     exit /b {code}\r\n\
                     )\r\n",
                    code = reply.code
                ));
            }
            script.push_str("exit /b 0\r\n");
            let path = dir.join("fake-engine.cmd");
            std::fs::write(&path, script).expect("write the fake engine");
            path
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut script = String::from("#!/bin/sh\nhere=\"$(dirname \"$0\")\"\n");
            for (sub, reply) in replies {
                script.push_str(&format!(
                    "if [ \"$2\" = \"{sub}\" ]; then\n\
                     echo x >> \"$here/{sub}.calls\"\n\
                     cat >> \"$here/{sub}.stdin\"\n\
                     sleep {sleep}\n\
                     cat \"$here/{sub}.out\"\n\
                     cat \"$here/{sub}.err\" 1>&2\n\
                     exit {code}\n\
                     fi\n",
                    sleep = reply.sleep,
                    code = reply.code
                ));
            }
            script.push_str("exit 0\n");
            let path = dir.join("fake-engine.sh");
            std::fs::write(&path, script).expect("write the fake engine");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("make it runnable");
            path
        }
    }

    /// Every engine process the runner started, as (binary, subcommand).
    static SPAWNS: std::sync::Mutex<Vec<(PathBuf, &'static str)>> =
        std::sync::Mutex::new(Vec::new());

    /// Noted by the runner just before it starts a process.
    pub(crate) fn note_spawn(bin: &Path, sub: &'static str) {
        SPAWNS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((bin.to_path_buf(), sub));
    }

    /// How many times the runner STARTED the fake in `dir` for `sub` —
    /// counted before the process exists, so a process stopped before its
    /// first line still counts. [`calls`] cannot say that.
    pub(crate) fn spawned(dir: &Path, sub: &str) -> usize {
        SPAWNS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(bin, s)| *s == sub && bin.parent() == Some(dir))
            .count()
    }

    /// How many times the fake ran `sub`.
    pub(crate) fn calls(dir: &Path, sub: &str) -> usize {
        std::fs::read_to_string(dir.join(format!("{sub}.calls")))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    /// Every event the fake was given for `sub`, raw, one per line.
    pub(crate) fn events_raw(dir: &Path, sub: &str) -> Vec<String> {
        std::fs::read_to_string(dir.join(format!("{sub}.stdin")))
            .unwrap_or_default()
            .lines()
            .map(|line| line.trim_end_matches('\r').to_string())
            .filter(|line| !line.trim().is_empty())
            .collect()
    }

    /// Every event the fake was given for `sub`, parsed.
    pub(crate) fn events(dir: &Path, sub: &str) -> Vec<serde_json::Value> {
        events_raw(dir, sub)
            .iter()
            .map(|line| serde_json::from_str(line).expect("the event is JSON"))
            .collect()
    }

    /// The engine at `bin`, for a test.
    pub(crate) fn engine(bin: PathBuf) -> super::Engine {
        super::Engine::at(bin)
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{self, Reply};
    use super::*;
    use std::time::Instant;

    const ASK: &str = r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"git push needs the owner","additionalContext":"RULE-7"}}"#;

    /// **NEVER SHORTER THAN THE ENGINE.** The engine registers these three for
    /// 600 s and does its own waiting inside them — 500 s for an answer to a
    /// question, 540 s for a permission. A harness bound below that cuts an
    /// answer the owner is in the middle of giving.
    #[test]
    fn the_long_hooks_are_bounded_no_shorter_than_the_engine_registers_them() {
        for (name, bound) in [
            ("pre-tool", PRE_TOOL_BOUND),
            ("capture", CAPTURE_BOUND),
            ("permission-request", PERMISSION_REQUEST_BOUND),
        ] {
            assert!(
                bound >= Duration::from_secs(600),
                "{name} is bounded at {bound:?}"
            );
        }
        assert_eq!(Event::PreTool.bound(), PRE_TOOL_BOUND);
        assert_eq!(Event::PermissionRequest.bound(), PERMISSION_REQUEST_BOUND);
        // Not the operator hooks' ten seconds, which would cut every answer.
        assert!(PRE_TOOL_BOUND > crate::hooks::HOOK_TIMEOUT);
    }

    /// **THE EVENT ARRIVES ON STDIN, AND STDIN IS CLOSED.** The fake reads its
    /// input to the end before it answers; an open pipe would leave it waiting
    /// until the bound, and this would time out instead of returning.
    #[test]
    fn the_event_is_written_to_stdin_and_the_pipe_is_closed() {
        let dir = tempfile::tempdir().expect("scratch");
        let bin = fake::write(dir.path(), &[("pre-tool", Reply::exit(0))]);
        let engine = fake::engine(bin).capped(Duration::from_secs(20));
        let started = Instant::now();
        let answer = engine.pre_tool(
            "api-123",
            dir.path(),
            "Bash",
            &json!({"command": "cargo test"}),
            None,
        );
        assert_eq!(answer, PreTool::Allow { context: None });
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the fake waited on stdin: {:?}",
            started.elapsed()
        );
        let events = fake::events(dir.path(), "pre-tool");
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["hook_event_name"], "PreToolUse");
        assert_eq!(events[0]["session_id"], "api-123");
        assert_eq!(events[0]["tool_input"]["command"], "cargo test");
    }

    /// **A HOOK PAST ITS BOUND IS KILLED — WITH EVERYTHING IT STARTED.** The
    /// engine here starts a grandchild that ticks into a file; once the call
    /// returns, the file must stop growing.
    #[test]
    fn a_hook_past_its_bound_is_killed_with_its_whole_tree() {
        let dir = tempfile::tempdir().expect("scratch");
        let (command, ticks) =
            crate::agent::process_tree::a_parent_with_a_ticking_grandchild(dir.path());
        #[cfg(windows)]
        let bin = PathBuf::from(command.get_program());
        #[cfg(unix)]
        let bin = {
            // The shared helper is `sh -c <script>`; as an engine it has to be
            // one runnable file.
            use std::os::unix::fs::PermissionsExt;
            let script = command
                .get_args()
                .nth(1)
                .map(|a| a.to_string_lossy().into_owned())
                .expect("the script");
            let path = dir.path().join("slow-engine.sh");
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("write");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            path
        };
        let engine = fake::engine(bin).capped(Duration::from_secs(3));

        let started = Instant::now();
        let answer = engine.pre_tool("s", dir.path(), "Read", &json!({"path": "a"}), None);
        let took = started.elapsed();
        assert!(
            matches!(&answer, PreTool::Failed(why) if why.contains("timed out")),
            "{answer:?}"
        );
        assert!(
            took < Duration::from_secs(20),
            "the bound did not hold: {took:?}"
        );

        std::thread::sleep(Duration::from_millis(1500));
        let size = || std::fs::metadata(&ticks).map(|m| m.len()).unwrap_or(0);
        let after = size();
        std::thread::sleep(Duration::from_millis(2500));
        assert_eq!(size(), after, "the engine's grandchild outlived the bound");
    }

    /// Exit 2 is a block, and its reason is on stderr.
    #[test]
    fn exit_two_is_a_block_with_the_reason_from_stderr() {
        let dir = tempfile::tempdir().expect("scratch");
        let bin = fake::write(
            dir.path(),
            &[("pre-tool", Reply::exit(2).stderr("no backup of src/lib.rs"))],
        );
        let answer = fake::engine(bin).capped(Duration::from_secs(20)).pre_tool(
            "s",
            dir.path(),
            "Write",
            &json!({"path": "x"}),
            None,
        );
        assert_eq!(
            answer,
            PreTool::Block("no backup of src/lib.rs".to_string())
        );
    }

    #[test]
    fn the_reply_shapes_are_read_as_the_engine_writes_them() {
        let exited = |code: i32, stdout: &str| Ran::Exited {
            code,
            stdout: stdout.to_string(),
            stderr: String::new(),
        };
        assert_eq!(
            read_pre_tool(exited(0, ASK)),
            PreTool::Ask {
                reason: "git push needs the owner".to_string(),
                context: Some("RULE-7".to_string())
            }
        );
        assert_eq!(
            read_pre_tool(exited(
                0,
                r#"{"hookSpecificOutput":{"permissionDecision":"allow","permissionDecisionReason":"fine","additionalContext":"RULE-1"}}"#
            )),
            PreTool::Allow {
                context: Some("RULE-1".to_string())
            }
        );
        // Context alone, with no decision, is an allow that carries rules.
        assert_eq!(
            read_pre_tool(exited(
                0,
                r#"{"hookSpecificOutput":{"additionalContext":"RULE-2"}}"#
            )),
            PreTool::Allow {
                context: Some("RULE-2".to_string())
            }
        );
        assert!(matches!(read_pre_tool(exited(1, "")), PreTool::Failed(_)));

        let allow = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;
        let deny = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"no"}}}"#;
        assert_eq!(read_permission(exited(0, allow)), Permission::Allow);
        assert_eq!(
            read_permission(exited(0, deny)),
            Permission::Deny("no".to_string())
        );
        assert_eq!(read_permission(exited(0, "")), Permission::NoAnswer);
        // Exit 2 is not this hook's contract: only stdout decides.
        assert_eq!(read_permission(exited(2, "")), Permission::NoAnswer);
    }

    /// The order: the environment, then the settings, then an installed copy.
    /// A NAMED path is kept even when it is missing — its spawn failure is the
    /// guard's failure policy, not a quiet fallback.
    #[test]
    fn the_binary_is_found_in_the_documented_order() {
        let dir = tempfile::tempdir().expect("scratch");
        let installed = dir.path().join("installed.exe");
        std::fs::write(&installed, "").expect("write");
        let missing = dir.path().join("missing.exe");
        let defaults = [missing.clone(), installed.clone()];

        let bin = |e: Option<Engine>| e.map(|e| e.bin);
        assert_eq!(
            bin(locate_from(
                Some("env.exe".into()),
                Some("cfg.exe".into()),
                &defaults
            )),
            Some(PathBuf::from("env.exe"))
        );
        assert_eq!(
            bin(locate_from(
                Some("  ".into()),
                Some("cfg.exe".into()),
                &defaults
            )),
            Some(PathBuf::from("cfg.exe"))
        );
        assert_eq!(bin(locate_from(None, None, &defaults)), Some(installed));
        assert_eq!(bin(locate_from(None, None, &[missing])), None);
    }
}
