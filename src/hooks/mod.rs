//! Run an operator's own shell commands around a tool call, and before a
//! person's line is sent ([`HookRunner::user_prompt_submit`]: exit 2 refuses
//! it, stdout is added to the turn, the line is never changed).
//!
//! # Where these run
//!
//! Around EVERY tool incode runs: [`crate::toolbox::Toolbox::run_call`] runs
//! the pre-tool commands after the guards and the permission mode and before
//! the memory engine, and the post-tool commands once the tool has returned.
//! The desktop's tools are the one exception there: their executor, the
//! desktop MCP server (`desktop::mcp::call_tool`), runs the same commands
//! itself, as it always has — also for the agent that reaches the desktop
//! server directly — so they are not run twice.
//!
//! The commands come from `admin.json` `desktop_hooks` (also accepted as
//! `hooks`); the name predates their running for every tool.
//!
//! # The contract with the operator's command
//!
//! - **stdin** carries the event as one JSON object, then is closed:
//!   `hook_event_name` (`PreToolUse` / `PostToolUse`), `tool_name`,
//!   `tool_input`, `session_id` and `cwd`, and after the tool `tool_response`
//!   and `is_error` — the shape an agent CLI's own hooks are given. It is a
//!   pipe, never an inherited stdin: under `incode --acp` that is the
//!   protocol stream.
//! - `HOOK_TOOL_NAME` and `HOOK_TOOL_INPUT` are also set in its environment
//!   (the input cut at [`MAX_ENV_INPUT_BYTES`]; the whole of it is on stdin).
//! - **Exit code 2 denies the call**, before it runs; stderr is the reason
//!   the model is given (stdout when stderr is empty). Any other code allows
//!   it. A post-tool command cannot deny anything.
//! - Anything on stdout is passed back to the agent as a message.
//! - It runs in the workspace, and is killed with everything it started at
//!   [`HOOK_TIMEOUT`]; a command that could not run or ran past it allows
//!   the call, with a message saying so.
//!
//! # `HOOK_TOOL_INPUT` is the REAL input, not the redacted description
//!
//! `desktop::mcp::Server::describe` exists so a typed password becomes
//! "8 characters" in the journal and in the browser stream. A hook is
//! deliberately outside that boundary: it is a command the operator wrote,
//! running on the operator's machine, and inspecting what is about to be typed
//! is the entire reason a pre-tool hook exists. A hook that cannot see the
//! input cannot refuse on it.
//!
//! The redaction still applies to everything else. What a hook does with what
//! it is shown is the operator's decision, made when they wrote the command.

pub mod engine;
pub mod memory;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

/// The most of a call's input put in `HOOK_TOOL_INPUT`, in bytes. An
/// environment block is bounded (32 767 characters on Windows) and a `Write`
/// carries a whole file; the full input is always on stdin.
pub const MAX_ENV_INPUT_BYTES: usize = 8 * 1024;

/// Why a hook process gave no answer.
#[derive(Debug)]
pub(crate) enum Piped {
    /// It could not be started.
    Start(std::io::Error),
    /// It ran past its bound, was abandoned, or its wait failed.
    Wait(String),
}

/// Run `command` the way every hook is run: `input` written to a PIPED stdin
/// that is then closed, stdout and stderr piped, the process tree contained,
/// and the wait bounded at `bound` or ended when `abandon` turns true — when
/// the tree is killed.
///
/// One implementation for the memory engine's hooks and the operator's.
pub(crate) fn run_piped(
    mut command: Command,
    input: Vec<u8>,
    bound: Duration,
    abandon: &dyn Fn() -> bool,
) -> Result<std::process::Output, Piped> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::agent::process_tree::prepare(&mut command);
    let mut child = command.spawn().map_err(Piped::Start)?;
    // Written on a thread of its own and CLOSED when that thread ends, so the
    // child sees end-of-input. Not written inline: a large event and a child
    // that prints before it reads would each wait on the other.
    if let Some(mut stdin) = child.stdin.take() {
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    crate::tools::bash::wait_with_timeout_or(child, bound, abandon)
        .map_err(|e| Piped::Wait(e.to_string()))
}

/// How long a hook may take before it is killed and treated as allowing.
///
/// A hook runs between the agent asking and the desktop moving, so the whole
/// session waits on it. Without a bound, one command that waits on input
/// hangs the desktop with no way to tell from the outside whether the agent
/// or the hook is stuck.
///
/// Ten seconds is long for a guard command and short enough that a hung one is
/// noticed rather than lived with.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// HookConfig
// ---------------------------------------------------------------------------

/// The operator's pre- and post-tool commands.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct HookConfig {
    /// Commands run BEFORE the tool. Exit code 2 refuses the call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_tool_use: Vec<String>,
    /// Commands run AFTER the tool, whatever it returned.
    ///
    /// These cannot refuse anything — the action has already happened, and a
    /// deny here would report a refusal for something that was done.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_tool_use: Vec<String>,
    /// Commands run when a person's line is about to go to the agent. Exit
    /// code 2 refuses the line (the reason is shown, nothing is sent); stdout
    /// is added to that turn's context. The line itself is never changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_prompt_submit: Vec<String>,
}

impl HookConfig {
    /// Whether there is anything to run at all.
    ///
    /// Checked before a call so the ordinary case — no hooks configured —
    /// costs nothing beyond this.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.user_prompt_submit.is_empty()
    }

    /// `other`'s commands after these, event by event (a plugin's after the
    /// operator's).
    pub fn extend(&mut self, other: Self) {
        self.pre_tool_use.extend(other.pre_tool_use);
        self.post_tool_use.extend(other.post_tool_use);
        self.user_prompt_submit.extend(other.user_prompt_submit);
    }
}

// ---------------------------------------------------------------------------
// HookRunResult
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRunResult {
    denied: bool,
    messages: Vec<String>,
}

impl HookRunResult {
    #[must_use]
    pub fn allow(messages: Vec<String>) -> Self {
        Self {
            denied: false,
            messages,
        }
    }

    #[must_use]
    pub fn is_denied(&self) -> bool {
        self.denied
    }

    #[must_use]
    pub fn messages(&self) -> &[String] {
        &self.messages
    }
}

// ---------------------------------------------------------------------------
// HookRunner
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HookRunner {
    config: HookConfig,
}

/// One call, as a hook is told it.
#[derive(Debug, Clone, Copy)]
pub struct HookCall<'a> {
    pub tool_name: &'a str,
    pub tool_input: &'a Value,
    /// The session the call belongs to; empty when there is none.
    pub session: &'a str,
    /// Where the command runs; the process's own directory when `None`.
    pub cwd: Option<&'a Path>,
}

/// Which side of the call a hook runs on.
#[derive(Debug, Clone, Copy)]
enum Side<'a> {
    Pre,
    Post { output: &'a str, is_error: bool },
}

impl HookRunner {
    #[must_use]
    pub fn new(config: HookConfig) -> Self {
        Self { config }
    }

    /// The operator's commands from `admin.json`, when there are any. Read by
    /// the code that builds a backend for real work, so a toolbox built for a
    /// test never runs the operator's commands.
    #[must_use]
    pub fn from_settings() -> Option<Self> {
        let mut config = crate::admin::AdminConfig::load_default().desktop_hooks;
        config.extend(crate::plugins::session_hooks());
        (!config.is_empty()).then(|| Self::new(config))
    }

    /// The pre-tool commands, for a caller holding the input as text — the
    /// desktop server, which has no workspace or session.
    #[must_use]
    pub fn run_pre_tool_use(&self, tool_name: &str, tool_input: &str) -> HookRunResult {
        let input = parsed(tool_input);
        self.pre_tool(&HookCall {
            tool_name,
            tool_input: &input,
            session: "",
            cwd: None,
        })
    }

    /// The post-tool commands, for a caller holding the input as text.
    #[must_use]
    pub fn run_post_tool_use(
        &self,
        tool_name: &str,
        tool_input: &str,
        tool_output: &str,
        is_error: bool,
    ) -> HookRunResult {
        let input = parsed(tool_input);
        let call = HookCall {
            tool_name,
            tool_input: &input,
            session: "",
            cwd: None,
        };
        self.post_tool(&call, tool_output, is_error)
    }

    /// The pre-tool commands for `call`. Exit 2 from any of them denies it,
    /// and the rest are not run.
    #[must_use]
    pub fn pre_tool(&self, call: &HookCall<'_>) -> HookRunResult {
        self.run_commands(&self.config.pre_tool_use, call, Side::Pre)
    }

    /// The post-tool commands for `call`, which returned `output`. They
    /// cannot deny: the call already happened.
    #[must_use]
    pub fn post_tool(&self, call: &HookCall<'_>, output: &str, is_error: bool) -> HookRunResult {
        let mut ran = self.run_commands(
            &self.config.post_tool_use,
            call,
            Side::Post { output, is_error },
        );
        ran.denied = false;
        ran
    }

    fn run_commands(
        &self,
        commands: &[String],
        call: &HookCall<'_>,
        side: Side<'_>,
    ) -> HookRunResult {
        if commands.is_empty() {
            return HookRunResult::allow(Vec::new());
        }
        let mut event = json!({
            "hook_event_name": match side { Side::Pre => "PreToolUse", Side::Post { .. } => "PostToolUse" },
            "tool_name": call.tool_name,
            "tool_input": call.tool_input,
            "session_id": call.session,
            "cwd": call.cwd.map(|p| p.to_string_lossy().into_owned()),
        });
        if let Side::Post { output, is_error } = side {
            event["tool_response"] = json!(output);
            event["is_error"] = json!(is_error);
        }
        let mut payload = event.to_string().into_bytes();
        payload.push(b'\n');

        let input = match call.tool_input {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let env = [
            ("HOOK_TOOL_NAME", call.tool_name.to_string()),
            (
                "HOOK_TOOL_INPUT",
                crate::text::head_bytes(&input, MAX_ENV_INPUT_BYTES).to_string(),
            ),
        ];
        let mut messages = Vec::new();
        for command in commands {
            match run_shell_command(command, &env, call.cwd, &payload) {
                Ok((code, stdout, stderr)) => {
                    if code == 2 && matches!(side, Side::Pre) {
                        // Exit 2 denies; stderr is the reason, as the
                        // engine's hooks give theirs.
                        let reason = if stderr.is_empty() { stdout } else { stderr };
                        messages.push(if reason.is_empty() {
                            format!("the operator's hook `{command}` refused this call")
                        } else {
                            reason
                        });
                        return HookRunResult {
                            denied: true,
                            messages,
                        };
                    }
                    if !stdout.is_empty() {
                        messages.push(stdout);
                    }
                }
                Err(e) => {
                    messages.push(format!("Hook `{command}` failed: {e}"));
                }
            }
        }

        HookRunResult::allow(messages)
    }

    /// The `user_prompt_submit` commands for a person's `prompt`, before it
    /// goes to the agent. Exit 2 from any of them refuses it — the rest are
    /// not run — and the reason is stderr (stdout when stderr is empty).
    /// Otherwise each one's stdout is a message for the turn's context. The
    /// prompt is shown to them, never taken back from them.
    ///
    /// Called by `cli::agent_turn::AgentSession::prompt_hooks` (the REPL and
    /// `--acp`, through `cli::plan_turn`) and by `task_run` (`-t`).
    #[must_use]
    pub fn user_prompt_submit(
        &self,
        prompt: &str,
        session: &str,
        cwd: Option<&Path>,
    ) -> HookRunResult {
        let commands = &self.config.user_prompt_submit;
        if commands.is_empty() {
            return HookRunResult::allow(Vec::new());
        }
        let mut payload = json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": prompt,
            "session_id": session,
            "cwd": cwd.map(|p| p.to_string_lossy().into_owned()),
        })
        .to_string()
        .into_bytes();
        payload.push(b'\n');
        let mut messages = Vec::new();
        for command in commands {
            match run_shell_command(command, &[], cwd, &payload) {
                Ok((2, stdout, stderr)) => {
                    let reason = if stderr.is_empty() { stdout } else { stderr };
                    return HookRunResult {
                        denied: true,
                        messages: vec![if reason.is_empty() {
                            format!("the operator's hook `{command}` refused this prompt")
                        } else {
                            reason
                        }],
                    };
                }
                Ok((_, stdout, _)) => {
                    if !stdout.is_empty() {
                        messages.push(stdout);
                    }
                }
                Err(e) => messages.push(format!("Hook `{command}` failed: {e}")),
            }
        }
        HookRunResult::allow(messages)
    }
}

/// A call's input from its text: the JSON it holds, or the text itself.
fn parsed(tool_input: &str) -> Value {
    serde_json::from_str(tool_input).unwrap_or_else(|_| json!(tool_input))
}

/// One operator command, run through the shell in `cwd` with `env` set and the
/// event on its stdin: its exit code, stdout and stderr, each trimmed.
fn run_shell_command(
    command: &str,
    env: &[(&str, String)],
    cwd: Option<&Path>,
    payload: &[u8],
) -> std::io::Result<(i32, String, String)> {
    // Verbatim on Windows, as the Bash tool passes its command: `Command::arg`
    // would escape a `"` the operator wrote as `\"`, which `cmd /C` does not
    // read that way.
    #[cfg(windows)]
    let mut child = {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        c.arg("/C").raw_arg(command);
        c
    };

    #[cfg(not(windows))]
    let mut child = {
        let mut c = Command::new("sh");
        c.arg("-lc").arg(command);
        c
    };

    for (name, value) in env {
        child.env(name, value);
    }
    if let Some(cwd) = cwd {
        child.current_dir(cwd);
    }

    // BOUNDED, through the same runner the memory engine's hooks use: a
    // piped stdin that is written and closed, the tree contained, the wait
    // capped. This used to be `child.output()` with the stdin inherited,
    // which waits forever: a hook that blocks on input would have hung the
    // call between the agent asking and anything happening.
    let output =
        run_piped(child, payload.to_vec(), HOOK_TIMEOUT, &|| false).map_err(|e| match e {
            Piped::Start(e) => e,
            Piped::Wait(why) => std::io::Error::other(why),
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let code = output.status.code().unwrap_or(-1);
    Ok((code, stdout, stderr))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hook command that saves its stdin next to it, prints `said`, and
    /// exits with `code` after writing `why` to stderr.
    fn hook(dir: &Path, code: i32, why: &str) -> String {
        #[cfg(windows)]
        {
            let script = dir.join(format!("hook{code}.cmd"));
            std::fs::write(
                &script,
                format!(
                    "@echo off\r\nmore > \"%~dp0event.json\"\r\necho said\r\n\
                     echo {why} 1>&2\r\nexit /b {code}\r\n"
                ),
            )
            .expect("hook");
            // Quoted as an operator would: the command reaches `cmd /C`
            // verbatim.
            format!("\"{}\"", script.display())
        }
        #[cfg(not(windows))]
        {
            let script = dir.join(format!("hook{code}.sh"));
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\ncat > \"$(dirname \"$0\")/event.json\"\necho said\n\
                     echo '{why}' >&2\nexit {code}\n"
                ),
            )
            .expect("hook");
            format!("sh '{}'", script.display())
        }
    }

    fn call<'a>(input: &'a Value, cwd: &'a Path) -> HookCall<'a> {
        HookCall {
            tool_name: "Write",
            tool_input: input,
            session: "sess-9",
            cwd: Some(cwd),
        }
    }

    /// **THE EVENT ARRIVES ON A PIPED STDIN THAT IS CLOSED, AND EXIT 2 DENIES
    /// WITH STDERR AS THE REASON.** A hook that reads stdin to its end would
    /// hang on an inherited one.
    #[test]
    fn a_pre_hook_reads_the_event_on_stdin_and_exit_two_denies_with_its_stderr() {
        let dir = tempfile::tempdir().expect("dir");
        let runner = HookRunner::new(HookConfig {
            pre_tool_use: vec![hook(dir.path(), 2, "no writes today")],
            post_tool_use: Vec::new(),
            user_prompt_submit: Vec::new(),
        });
        let input = json!({"path": "a.txt", "content": "hi"});
        let ran = runner.pre_tool(&call(&input, dir.path()));
        assert!(ran.is_denied(), "{ran:?}");
        assert_eq!(ran.messages(), ["no writes today"], "{ran:?}");
        let event: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("event.json")).expect("the hook saved it"),
        )
        .expect("one JSON object");
        assert_eq!(event["hook_event_name"], "PreToolUse");
        assert_eq!(event["tool_name"], "Write");
        assert_eq!(event["tool_input"]["path"], "a.txt");
        assert_eq!(event["session_id"], "sess-9");
    }

    /// Any other exit allows, with what the hook printed; a post hook cannot
    /// deny, and is told the result.
    #[test]
    fn other_exits_allow_and_a_post_hook_cannot_deny() {
        let dir = tempfile::tempdir().expect("dir");
        let runner = HookRunner::new(HookConfig {
            pre_tool_use: vec![hook(dir.path(), 0, "fine")],
            post_tool_use: vec![hook(dir.path(), 2, "too late")],
            user_prompt_submit: Vec::new(),
        });
        let input = json!({"path": "a.txt"});
        let pre = runner.pre_tool(&call(&input, dir.path()));
        assert!(!pre.is_denied());
        assert_eq!(pre.messages(), ["said"]);
        let post = runner.post_tool(&call(&input, dir.path()), "wrote it", false);
        assert!(!post.is_denied(), "a post hook cannot deny: {post:?}");
        let event: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("event.json")).expect("saved"),
        )
        .expect("json");
        assert_eq!(event["hook_event_name"], "PostToolUse");
        assert_eq!(event["tool_response"], "wrote it");
    }

    fn prompt_runner(commands: Vec<String>) -> HookRunner {
        HookRunner::new(HookConfig {
            user_prompt_submit: commands,
            ..HookConfig::default()
        })
    }

    /// **EXIT 2 REFUSES THE PROMPT, WITH STDERR AS THE REASON**, and the
    /// commands after it are not run.
    #[test]
    fn a_prompt_hook_exiting_2_refuses_the_prompt_with_its_reason() {
        let dir = tempfile::tempdir().expect("dir");
        let later = tempfile::tempdir().expect("later");
        let runner = prompt_runner(vec![
            hook(dir.path(), 2, "no secrets in prompts"),
            hook(later.path(), 0, "ran"),
        ]);
        let ran = runner.user_prompt_submit("paste my key here", "sess-1", Some(dir.path()));
        assert!(ran.is_denied());
        assert_eq!(ran.messages(), ["no secrets in prompts"]);
        assert!(
            !later.path().join("event.json").exists(),
            "a command after the refusal ran"
        );
    }

    /// Any other exit lets the prompt through; stdout is the turn's context,
    /// and the event — the prompt, the session, the folder — is on stdin.
    #[test]
    fn a_prompt_hook_adds_context_and_is_told_the_prompt() {
        let dir = tempfile::tempdir().expect("dir");
        let runner = prompt_runner(vec![hook(dir.path(), 0, "")]);
        let ran = runner.user_prompt_submit("fix the parser", "sess-1", Some(dir.path()));
        assert!(!ran.is_denied());
        assert_eq!(ran.messages(), ["said"]);
        let event: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("event.json")).expect("saved"),
        )
        .expect("json");
        assert_eq!(event["hook_event_name"], "UserPromptSubmit");
        assert_eq!(event["prompt"], "fix the parser");
        assert_eq!(event["session_id"], "sess-1");
    }

    /// The tool hooks do not run for a prompt, nor the prompt hooks for a
    /// tool: each list is its own event's.
    #[test]
    fn prompt_hooks_and_tool_hooks_are_separate_events() {
        let dir = tempfile::tempdir().expect("dir");
        let tool_only = HookRunner::new(HookConfig {
            pre_tool_use: vec![hook(dir.path(), 2, "tool hook")],
            ..HookConfig::default()
        });
        assert!(!tool_only
            .user_prompt_submit("hello", "", Some(dir.path()))
            .is_denied());
        let prompt_only = prompt_runner(vec![hook(dir.path(), 2, "prompt hook")]);
        let input = json!({"path": "a"});
        assert!(!prompt_only.pre_tool(&call(&input, dir.path())).is_denied());
        assert!(!HookConfig {
            user_prompt_submit: vec!["x".into()],
            ..HookConfig::default()
        }
        .is_empty());
    }
}
