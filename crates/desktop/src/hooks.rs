//! The operator's own commands, run around every desktop tool call.
//!
//! # Where this sits
//!
//! The desktop server is the one place a click or a keystroke passes through,
//! so it is where an operator's veto is worth the most: a click reaches a
//! machine they are signed into and no checkpoint undoes it.
//! [`crate::mcp::Server`] asks a [`Hooks`] before the tool runs
//! ([`Hooks::run_pre_tool_use`]) and tells it after
//! ([`Hooks::run_post_tool_use`]).
//!
//! Two things implement [`Hooks`]:
//!
//! - [`ShellHooks`], here, for the `incu` binary and any embedder with no hook
//!   engine of its own. Built from a [`HookConfig`] read out of the
//!   configuration file ([`crate::config::Config`]).
//! - incode's own `hooks::HookRunner`, through an adapter in `main.rs`, so
//!   `incode --desktop-mcp` keeps running the operator's commands exactly as
//!   it did before the desktop became its own crate (its process-tree
//!   containment, its POSIX-shell choice on Windows). This module does not
//!   replace that engine; it is what the desktop needs of one.
//!
//! # The contract with the operator's command
//!
//! It is the contract incode's hooks documented, kept identical so an
//! operator's existing scripts work under either binary:
//!
//! - **stdin** carries the event as one JSON object, then is closed:
//!   `hook_event_name` (`PreToolUse` / `PostToolUse`), `tool_name`,
//!   `tool_input`, `session_id` and `cwd`, and after the tool `tool_response`
//!   and `is_error`. It is a pipe, never an inherited stdin: the server's own
//!   stdin is the MCP protocol.
//! - `HOOK_TOOL_NAME` and `HOOK_TOOL_INPUT` are also set in its environment
//!   (the input cut at [`MAX_ENV_INPUT_BYTES`]; the whole of it is on stdin).
//! - **Exit code 2 denies the call**, before it runs; stderr is the reason the
//!   agent is given (stdout when stderr is empty). Any other code allows it. A
//!   post-tool command cannot deny anything.
//! - Anything on stdout is passed back as a message.
//! - It is killed with everything it started at [`HOOK_TIMEOUT`]; a command
//!   that could not run or ran past it allows the call, with a message saying
//!   so.
//! - A command with a `matcher` runs only for the tools it names: a regular
//!   expression matched against the WHOLE tool name; none, `""` or `*` is every
//!   tool. A matcher that is not a regular expression runs the command for no
//!   tool, and says so.
//!
//! # A hook is shown the REAL input
//!
//! [`crate::mcp::Server`] describes a typed password to the journal as "8
//! characters". A hook is deliberately outside that boundary: it is a command
//! the operator wrote, running on the operator's machine, and inspecting what
//! is about to be typed is the entire reason a pre-tool hook exists. A hook that
//! cannot see the input cannot refuse on it.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The most of a call's input put in `HOOK_TOOL_INPUT`, in bytes. An
/// environment block is bounded (32 767 characters on Windows) and `type_text`
/// can carry a great deal; the full input is always on stdin.
pub const MAX_ENV_INPUT_BYTES: usize = 8 * 1024;

/// How long a hook may take before it is killed and treated as allowing.
///
/// A hook runs between the agent asking and the desktop moving, so the whole
/// session waits on it. Without a bound, one command that waits on input hangs
/// the desktop with no way to tell from the outside whether the agent or the
/// hook is stuck.
///
/// Ten seconds is long for a guard command and short enough that a hung one is
/// noticed rather than lived with.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for a finished hook's output pipes to close. A command that
/// started a background process which kept them open must not hold the call.
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// What a hook run comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOutcome {
    denied: bool,
    messages: Vec<String>,
}

impl HookOutcome {
    /// An outcome that lets the call go ahead, with whatever the commands said.
    #[must_use]
    pub fn allow(messages: Vec<String>) -> Self {
        Self {
            denied: false,
            messages,
        }
    }

    /// An outcome that refuses the call, with the reason(s) to give the agent.
    #[must_use]
    pub fn deny(messages: Vec<String>) -> Self {
        Self {
            denied: true,
            messages,
        }
    }

    /// Whether a pre-tool command refused the call (exit code 2).
    #[must_use]
    pub fn is_denied(&self) -> bool {
        self.denied
    }

    /// What the commands said: the refusal's reason, or their stdout.
    #[must_use]
    pub fn messages(&self) -> &[String] {
        &self.messages
    }
}

/// What the desktop server needs of an operator's hook engine.
///
/// Held by [`crate::mcp::Server`] (`with_hook_runner`, or `with_hooks` for the
/// built-in [`ShellHooks`]) and called from its gates in `mcp/gate.rs`
/// (`Server::hook_veto`, `Server::tell_post_hooks`). `Send` because the server
/// is built on one thread and may be handed to another.
pub trait Hooks: Send {
    /// How many pre-tool commands there are, for the startup line that tells the
    /// operator their guard is armed ([`crate::cli::serve`]). The default,
    /// nothing, only means the line is not printed.
    fn pre_tool_count(&self) -> usize {
        0
    }

    /// The pre-tool commands, for a caller holding the input as text. A denied
    /// outcome refuses the call before it reaches the desktop.
    fn run_pre_tool_use(&self, tool_name: &str, tool_input: &str) -> HookOutcome;

    /// The post-tool commands, for a call that ran and returned `tool_output`.
    /// They cannot deny: the action already happened.
    fn run_post_tool_use(
        &self,
        tool_name: &str,
        tool_input: &str,
        tool_output: &str,
        is_error: bool,
    ) -> HookOutcome;
}

/// One hook command: a command line, or `{"command", "matcher"}`.
///
/// Written the way incode's `admin.json` writes it, so one file serves both
/// binaries ([`crate::config::Config`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "RawHookCommand", into = "RawHookCommand")]
pub struct HookCommand {
    /// The command line, run through the platform shell (`cmd /C` on Windows,
    /// `sh -lc` elsewhere).
    pub command: String,
    /// The tool names the command runs for; see the module header.
    pub matcher: Option<String>,
}

/// How a [`HookCommand`] is written.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RawHookCommand {
    Plain(String),
    Matched {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        matcher: Option<String>,
    },
}

impl From<RawHookCommand> for HookCommand {
    fn from(raw: RawHookCommand) -> Self {
        match raw {
            RawHookCommand::Plain(command) => command.into(),
            RawHookCommand::Matched { command, matcher } => Self { command, matcher },
        }
    }
}

impl From<HookCommand> for RawHookCommand {
    fn from(hook: HookCommand) -> Self {
        match hook.matcher {
            None => Self::Plain(hook.command),
            matcher => Self::Matched {
                command: hook.command,
                matcher,
            },
        }
    }
}

impl From<String> for HookCommand {
    fn from(command: String) -> Self {
        Self {
            command,
            matcher: None,
        }
    }
}

impl From<&str> for HookCommand {
    fn from(command: &str) -> Self {
        command.to_string().into()
    }
}

impl std::fmt::Display for HookCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.command)
    }
}

impl HookCommand {
    /// Whether this command runs for the tool `tool_name`, and why not when its
    /// matcher cannot be read.
    ///
    /// # Errors
    ///
    /// The matcher is not a regular expression.
    pub fn matches(&self, tool_name: &str) -> Result<bool, String> {
        match self.matcher.as_deref().map(str::trim) {
            None | Some("" | "*") => Ok(true),
            Some(pattern) => regex::Regex::new(&format!("^(?:{pattern})$"))
                .map(|re| re.is_match(tool_name))
                .map_err(|e| format!("its matcher `{pattern}` is not a regular expression: {e}")),
        }
    }
}

/// The operator's pre- and post-tool commands.
///
/// Only the two events the desktop has. `admin.json` carries seven more for
/// incode's own sessions; they are ignored here (no `deny_unknown_fields`), so
/// the same file can be read by either binary.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HookConfig {
    /// Commands run BEFORE the tool. Exit code 2 refuses the call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_tool_use: Vec<HookCommand>,
    /// Commands run AFTER the tool, whatever it returned. These cannot refuse
    /// anything: a deny here would report a refusal for something that was
    /// done.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_tool_use: Vec<HookCommand>,
}

impl HookConfig {
    /// Whether there is anything to run at all.
    ///
    /// Checked once at startup ([`crate::mcp::Server::with_hooks`]) so the
    /// ordinary case, no hooks configured, costs nothing per call.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty() && self.post_tool_use.is_empty()
    }
}

/// [`Hooks`] that runs the operator's commands itself, from a [`HookConfig`].
///
/// What `incu` uses. It spawns each command with a piped stdin that is written
/// and closed, bounds the wait at [`HOOK_TIMEOUT`] and kills the command's
/// whole process tree when the bound passes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShellHooks {
    config: HookConfig,
}

impl ShellHooks {
    /// Run `config`'s commands.
    #[must_use]
    pub fn new(config: HookConfig) -> Self {
        Self { config }
    }

    /// The commands of one event that take this tool, then each one run in
    /// turn. A pre-tool command exiting 2 stops the rest.
    fn run_commands(
        &self,
        commands: &[HookCommand],
        tool_name: &str,
        tool_input: &str,
        after: Option<(&str, bool)>,
    ) -> HookOutcome {
        let mut messages = Vec::new();
        // Only the commands whose matcher takes this tool; one whose matcher
        // cannot be read runs for none, and says so.
        let commands: Vec<&HookCommand> = commands
            .iter()
            .filter(|hook| match hook.matches(tool_name) {
                Ok(runs) => runs,
                Err(why) => {
                    messages.push(format!("Hook `{hook}` was not run: {why}"));
                    false
                }
            })
            .collect();
        if commands.is_empty() {
            return HookOutcome::allow(messages);
        }
        let input = parsed(tool_input);
        let mut event = json!({
            "hook_event_name": if after.is_some() { "PostToolUse" } else { "PreToolUse" },
            "tool_name": tool_name,
            "tool_input": input,
            "session_id": "",
            "cwd": Value::Null,
        });
        if let Some((output, is_error)) = after {
            event["tool_response"] = json!(output);
            event["is_error"] = json!(is_error);
        }
        let mut payload = event.to_string().into_bytes();
        payload.push(b'\n');

        let env_input = match &input {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let env = [
            ("HOOK_TOOL_NAME", tool_name.to_string()),
            (
                "HOOK_TOOL_INPUT",
                head_bytes(&env_input, MAX_ENV_INPUT_BYTES).to_string(),
            ),
        ];
        for command in commands {
            match run_shell_command(command, &env, &payload) {
                Ok((code, stdout, stderr)) => {
                    if code == 2 && after.is_none() {
                        // Exit 2 denies; stderr is the reason.
                        let reason = if stderr.is_empty() { stdout } else { stderr };
                        messages.push(if reason.is_empty() {
                            format!("the operator's hook `{command}` refused this call")
                        } else {
                            reason
                        });
                        return HookOutcome::deny(messages);
                    }
                    if !stdout.is_empty() {
                        messages.push(stdout);
                    }
                }
                Err(e) => messages.push(format!("Hook `{command}` failed: {e}")),
            }
        }
        HookOutcome::allow(messages)
    }
}

impl Hooks for ShellHooks {
    fn pre_tool_count(&self) -> usize {
        self.config.pre_tool_use.len()
    }

    fn run_pre_tool_use(&self, tool_name: &str, tool_input: &str) -> HookOutcome {
        self.run_commands(&self.config.pre_tool_use, tool_name, tool_input, None)
    }

    fn run_post_tool_use(
        &self,
        tool_name: &str,
        tool_input: &str,
        tool_output: &str,
        is_error: bool,
    ) -> HookOutcome {
        // A post-tool command cannot deny: the action already happened.
        let ran = self.run_commands(
            &self.config.post_tool_use,
            tool_name,
            tool_input,
            Some((tool_output, is_error)),
        );
        HookOutcome::allow(ran.messages)
    }
}

/// A call's input from its text: the JSON it holds, or the text itself.
fn parsed(tool_input: &str) -> Value {
    serde_json::from_str(tool_input).unwrap_or_else(|_| json!(tool_input))
}

/// The longest prefix of `s` within `max_bytes`, not splitting a character.
fn head_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// One operator command, run through the shell with `env` set and the event on
/// its stdin: its exit code, stdout and stderr, each trimmed.
///
/// # Errors
///
/// The command could not be started, or ran past [`HOOK_TIMEOUT`] (and was
/// killed with everything it had started).
fn run_shell_command(
    hook: &HookCommand,
    env: &[(&str, String)],
    payload: &[u8],
) -> std::io::Result<(i32, String, String)> {
    // Verbatim on Windows, as a shell would be given it: `Command::arg` would
    // escape a `"` the operator wrote as `\"`, which `cmd /C` does not read
    // that way.
    #[cfg(windows)]
    let mut child = {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        c.arg("/C").raw_arg(&hook.command);
        c
    };
    #[cfg(not(windows))]
    let mut child = {
        use std::os::unix::process::CommandExt;
        let mut c = Command::new("sh");
        // Its own process group, so a timeout can take the whole tree.
        c.arg("-lc").arg(&hook.command).process_group(0);
        c
    };
    for (name, value) in env {
        child.env(name, value);
    }
    child
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = child.spawn()?;

    // Written on a thread of its own and CLOSED when that thread ends, so the
    // command sees end-of-input. Not written inline: a large event and a
    // command that prints before it reads would each wait on the other.
    if let Some(mut stdin) = child.stdin.take() {
        let payload = payload.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&payload);
        });
    }
    let stdout = collect(child.stdout.take());
    let stderr = collect(child.stderr.take());

    let deadline = Instant::now() + HOOK_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_tree(&mut child);
            return Err(std::io::Error::other(format!(
                "it ran past {} seconds and was stopped",
                HOOK_TIMEOUT.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let said = |rx: mpsc::Receiver<Vec<u8>>| {
        let bytes = rx.recv_timeout(PIPE_GRACE).unwrap_or_default();
        String::from_utf8_lossy(&bytes).trim().to_string()
    };
    Ok((status.code().unwrap_or(-1), said(stdout), said(stderr)))
}

/// Read `pipe` to its end on a thread of its own, delivering what it held.
fn collect<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        let _ = tx.send(bytes);
    });
    rx
}

/// Stop `child` and everything it started.
///
/// Unix: the child leads its own process group, so `killpg` reaches every
/// descendant. Windows: `taskkill /T` walks the tree from the child's pid.
/// Either way the child itself is killed and reaped last, so a failure to reach
/// the tree still stops the command.
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        if let Ok(group) = i32::try_from(child.id()) {
            // SAFETY: `killpg` takes plain integers and has no memory effects.
            unsafe {
                libc::killpg(group, libc::SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command that prints `said` and exits with `code`.
    fn says(said: &str, code: i32) -> HookCommand {
        if cfg!(windows) {
            format!("cmd /C echo {said}& exit {code}").into()
        } else {
            format!("echo {said}; exit {code}").into()
        }
    }

    fn shell(pre: Vec<HookCommand>) -> ShellHooks {
        ShellHooks::new(HookConfig {
            pre_tool_use: pre,
            post_tool_use: Vec::new(),
        })
    }

    #[test]
    fn a_pre_hook_exiting_two_denies_and_its_output_is_the_reason() {
        let ran = shell(vec![says("no", 2)]).run_pre_tool_use("click", "{}");
        assert!(ran.is_denied(), "{ran:?}");
        assert_eq!(ran.messages(), ["no"]);
    }

    #[test]
    fn any_other_exit_allows_and_its_stdout_is_a_message() {
        let ran = shell(vec![says("fyi", 1)]).run_pre_tool_use("click", "{}");
        assert!(!ran.is_denied(), "{ran:?}");
        assert_eq!(ran.messages(), ["fyi"]);
    }

    #[test]
    fn a_post_hook_cannot_deny() {
        let hooks = ShellHooks::new(HookConfig {
            pre_tool_use: Vec::new(),
            post_tool_use: vec![says("done", 2)],
        });
        let ran = hooks.run_post_tool_use("click", "{}", "ok", false);
        assert!(!ran.is_denied(), "{ran:?}");
    }

    #[test]
    fn a_hook_is_told_the_tool_in_its_environment() {
        let echo = if cfg!(windows) {
            "cmd /C echo %HOOK_TOOL_NAME%"
        } else {
            "echo $HOOK_TOOL_NAME"
        };
        let ran = shell(vec![echo.into()]).run_pre_tool_use("move_mouse", "{}");
        assert_eq!(ran.messages(), ["move_mouse"]);
    }

    #[test]
    fn a_matcher_runs_the_command_only_for_the_tools_it_names() {
        let only_type = HookCommand {
            command: says("blocked", 2).command,
            matcher: Some("type_text|paste_text".to_string()),
        };
        let hooks = shell(vec![only_type]);
        assert!(!hooks.run_pre_tool_use("click", "{}").is_denied());
        assert!(hooks.run_pre_tool_use("paste_text", "{}").is_denied());
        // Matched against the whole name, not a part of it.
        assert!(!hooks.run_pre_tool_use("type_text_x", "{}").is_denied());
    }

    #[test]
    fn a_matcher_that_is_not_a_regex_runs_nothing_and_says_so() {
        let broken = HookCommand {
            command: says("blocked", 2).command,
            matcher: Some("(".to_string()),
        };
        let ran = shell(vec![broken]).run_pre_tool_use("click", "{}");
        assert!(!ran.is_denied(), "{ran:?}");
        assert!(ran.messages()[0].contains("not a regular expression"));
    }

    #[test]
    fn a_command_that_cannot_run_allows_with_a_message() {
        let ran = shell(vec!["exit 0".into()]).run_pre_tool_use("click", "{}");
        assert!(!ran.is_denied(), "{ran:?}");
    }

    #[test]
    fn a_hook_is_written_as_a_line_or_with_its_matcher() {
        let config: HookConfig = serde_json::from_str(
            r#"{"pre_tool_use": ["one", {"command": "two", "matcher": "key"}],
                "user_prompt_submit": ["ignored here"]}"#,
        )
        .expect("a config with an event the desktop does not have");
        assert_eq!(config.pre_tool_use[0], "one".into());
        assert_eq!(config.pre_tool_use[1].matcher.as_deref(), Some("key"));
        assert!(config.post_tool_use.is_empty());
    }

    #[test]
    fn the_environment_copy_of_the_input_is_cut_on_a_character_boundary() {
        // "中" is three bytes: a two-byte budget keeps none of it.
        assert_eq!(head_bytes("中文", 2), "");
        assert_eq!(head_bytes("中文", 3), "中");
        assert_eq!(head_bytes("short", 100), "short");
    }
}
