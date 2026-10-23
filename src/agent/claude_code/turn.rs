//! One run of the CLI for a turn: spawned with the tool bridge open, its
//! tool calls answered by incode's toolbox, its stream read to the end.

#[cfg(test)]
use std::io::BufRead;
use std::io::BufReader;
use std::process::{Child, Command, Stdio};

use super::command::{servers_for, user_message};
use super::wire::map_line;
use super::{ClaudeCodeBackend, CANCELLED};
use crate::agent::process_tree::Tree;
use crate::agent::stderr_tail::StderrTail;
use crate::agent::{AgentError, AgentEvent, TurnOutcome};
use crate::tool_bridge::Incoming;

/// Where to write the agent's raw output, if the operator asked for it.
///
/// `INCODE_DEBUG_STREAM=<path>` appends every line the agent sends. It is the
/// only way to answer "why did this turn fail" without guessing.
#[must_use]
pub fn debug_stream_path() -> Option<String> {
    std::env::var("INCODE_DEBUG_STREAM")
        .ok()
        .filter(|p| !p.is_empty())
}

/// The turn itself: the CLI's output mapped to events, and every tool call
/// the model makes run by `tools`, until the CLI finishes or the turn is
/// stopped. The loop is the bridge's ([`crate::tool_bridge::drive`]); what a
/// line of THIS CLI means is decided here.
///
/// Split from the spawn plumbing so the loop that actually matters — event
/// mapping, session capture for `--resume`, and outcome extraction — is
/// testable over canned bytes without launching a process.
///
/// `session` is updated in place as soon as the session id appears, so a turn
/// that fails partway still leaves the backend able to resume.
pub(super) fn consume(
    from: &std::sync::mpsc::Receiver<Incoming>,
    session: &mut Option<String>,
    on_event: &mut dyn FnMut(&AgentEvent),
    cancel: Option<&crate::runtime::cancel::CancelToken>,
    tools: &mut crate::toolbox::Toolbox,
    calls: &mut u64,
) -> Result<Option<TurnOutcome>, AgentError> {
    let mut outcome = None;
    crate::tool_bridge::drive(from, cancel, tools, calls, on_event, &mut |line, tools| {
        // Every raw line, when asked for. A turn that fails for a reason the
        // harness cannot explain — "stopped without finishing: stop_sequence" —
        // is unanswerable without the bytes the agent actually sent, and
        // guessing at them from a screenshot is how an afternoon disappears.
        if let Some(path) = debug_stream_path() {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                use std::io::Write as _;
                let _ = writeln!(file, "{line}");
            }
        }
        if line.trim().is_empty() {
            return Vec::new();
        }
        let events = map_line(&line, &mut outcome);
        for event in &events {
            if let AgentEvent::SessionStarted { session_id, .. } = event {
                if !session_id.is_empty() {
                    *session = Some(session_id.clone());
                    // The engine files its cards and backups under the
                    // conversation, so it is told this one.
                    tools.set_session(session_id);
                }
            }
        }
        events
    })?;
    Ok(outcome)
}

/// [`consume`] over a finished stream of bytes, for the tests that replay
/// captured output.
#[cfg(test)]
pub(super) fn consume_stream<R: BufRead>(
    reader: R,
    session: &mut Option<String>,
    on_event: &mut dyn FnMut(&AgentEvent),
    cancel: Option<&crate::runtime::cancel::CancelToken>,
) -> Result<Option<TurnOutcome>, AgentError> {
    let (to, from) = std::sync::mpsc::channel();
    for line in reader.lines() {
        let message = match line {
            Ok(line) => Incoming::Line(line),
            Err(e) => Incoming::ReadFailed(e.to_string()),
        };
        let _ = to.send(message);
    }
    let _ = to.send(Incoming::Ended);
    let mut tools = crate::toolbox::Toolbox::new(std::path::Path::new("."), true);
    consume(&from, session, on_event, cancel, &mut tools, &mut 0)
}

impl ClaudeCodeBackend {
    /// One run of the CLI for a turn: spawned, its tool calls answered, its
    /// stream read to the end.
    ///
    /// Called by `run_turn` (`mod.rs`), and once more when the CLI refused the
    /// thinking flag.
    pub(super) fn run_once(
        &mut self,
        prompt: &str,
        images: Vec<crate::toolbox::Image>,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError> {
        let (budgeted_prompt, budgeted_context) =
            crate::agent::budget_for_spawn(prompt, &self.system_context, &self.workspace)?;

        // THE TOOLS ARE incode's. The CLI's own are switched off below and the
        // model is handed incode's toolbox through the bridge, which this
        // process answers for the whole turn. Without the bridge there would
        // be no tools but the CLI's, so a bridge that cannot open stops the
        // turn rather than falling back to them.
        let crate::tool_bridge::Turn {
            exe,
            bridge,
            to: to_loop,
            from: from_cli,
        } = crate::tool_bridge::Turn::open(self.cancel.clone())?;
        let (servers, allowed) = servers_for(&exe);

        let mut cmd = self.command(
            images.is_empty().then_some(budgeted_prompt.as_str()),
            &budgeted_context,
            &servers,
            &allowed,
        );
        // How the tool server finds this process: through the CLI's
        // environment, which the servers it starts inherit — never its argv.
        for (key, value) in bridge.env() {
            cmd.env(key, value);
        }
        self.trace_spawn(&cmd);
        self.dump_argv(&cmd);

        let (mut child, tree) = self.spawn(&mut cmd, &budgeted_prompt, &images)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::new("child stdout was not piped"))?;
        // Drained from now, not after `wait()`: an agent that fills the pipe
        // blocks on the write and never exits. See `stderr_tail`.
        let stderr = StderrTail::capture(child.stderr.take());

        crate::tool_bridge::pump(BufReader::new(stdout), to_loop);
        let outcome = consume(
            &from_cli,
            &mut self.session_id,
            on_event,
            self.cancel.as_ref(),
            &mut self.tools,
            &mut self.calls,
        );
        // Closed as soon as the turn's loop is done: a call that arrives from
        // here on is refused rather than left waiting on a loop that is gone.
        drop(bridge);
        self.finish(outcome, &mut child, &tree, stderr)
    }

    /// WHAT WAS ACTUALLY SPAWNED, on the ordinary trace. The flags decide the
    /// whole turn — which model, which profile, which deny rules — and a
    /// failure is unreadable without them. The full argv, including the
    /// appended system prompt, still goes to the stream dump
    /// ([`Self::dump_argv`]); this is the one-line version that belongs beside
    /// everything else that happened.
    ///
    /// Called by [`Self::run_once`], before the spawn.
    fn trace_spawn(&self, cmd: &Command) {
        if !crate::trace::on() {
            return;
        }
        let flags: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .filter(|a| a.starts_with("--"))
            .collect();
        crate::trace!(
            "agent",
            "spawning {} model={:?} resume={:?} flags={}",
            self.binary,
            self.model,
            self.session_id,
            flags.join(" ")
        );
    }

    /// The exact command, when the operator asks for it.
    ///
    /// A turn that fails cannot be investigated from outside the harness
    /// without the argv that produced it: the flags, the deny rules and the
    /// whole appended system prompt are assembled here and were, until now,
    /// invisible. Written alongside the raw stream so a failure can be
    /// replayed by hand and the flags bisected.
    ///
    /// Called by [`Self::run_once`], before the spawn.
    fn dump_argv(&self, cmd: &Command) {
        let Some(path) = debug_stream_path() else {
            return;
        };
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            use std::io::Write as _;
            let argv: Vec<String> = std::iter::once(self.binary.clone())
                .chain(cmd.get_args().map(|a| a.to_string_lossy().into_owned()))
                .collect();
            // As JSON, so an argument containing spaces or newlines — the
            // system prompt does — survives being read back.
            let _ = writeln!(
                file,
                "{}",
                serde_json::json!({ "type": "incode_argv", "argv": argv })
            );
        }
    }

    /// The CLI started in the workspace with its output piped and everything
    /// it starts taken in; a turn with pictures has its one message written to
    /// stdin.
    ///
    /// Called by [`Self::run_once`].
    fn spawn(
        &self,
        cmd: &mut Command,
        prompt: &str,
        images: &[crate::toolbox::Image],
    ) -> Result<(Child, Tree), AgentError> {
        // stdin must be closed explicitly, or the CLI waits ~3s for piped input
        // on every invocation. A turn with pictures writes its one message
        // there instead, and closes it.
        crate::agent::process_tree::prepare(cmd);
        let mut child = cmd
            .current_dir(&self.workspace)
            .stdin(if images.is_empty() {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| AgentError::new(format!("failed to spawn `{}`: {e}", self.binary)))?;
        let tree = Tree::adopt(&child);
        if let Some(mut stdin) = child.stdin.take() {
            // On a thread of its own: the message can be megabytes, and a
            // pipe the CLI is not reading yet must not stall this loop.
            let message = user_message(prompt, images);
            std::thread::spawn(move || {
                use std::io::Write as _;
                let _ = stdin.write_all(message.as_bytes());
                // Dropped here: the CLI ends its input, and its turn, on EOF.
            });
        }
        Ok((child, tree))
    }

    /// The turn's end: the child stopped if the turn was, waited for, and the
    /// outcome kept — or, with no result line, the error that says what the
    /// CLI wrote on stderr.
    ///
    /// Called by [`Self::run_once`] once the stream is read.
    fn finish(
        &mut self,
        outcome: Result<Option<TurnOutcome>, AgentError>,
        child: &mut Child,
        tree: &Tree,
        stderr: StderrTail,
    ) -> Result<TurnOutcome, AgentError> {
        // Whatever happened, the child must not outlive this call. A cancelled
        // turn that left the agent running would keep writing files with
        // nobody reading its output — and that includes whatever it started.
        let cancelled = matches!(outcome, Err(ref e) if e.to_string() == CANCELLED);
        if cancelled {
            tree.kill(child);
        }
        let outcome = outcome?;

        let status = child
            .wait()
            .map_err(|e| AgentError::new(format!("waiting for agent: {e}")))?;

        match outcome {
            Some(o) => {
                if !o.session_id.is_empty() {
                    self.session_id = Some(o.session_id.clone());
                }
                Ok(o)
            }
            None => Err(AgentError::new(format!(
                "agent exited with {} before producing a result: {}",
                status.code().unwrap_or(-1),
                stderr.finish()
            ))),
        }
    }
}
