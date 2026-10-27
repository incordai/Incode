//! The gates every `tools/call` passes, as the named steps
//! `Server::call_tool` runs in order: the operator's stop file, the session's
//! permission mode, the operator's pre-tool hook, the decision whether the
//! tool runs at all, the post-tool hook, and the journal line — with
//! [`Server::describe`], the redaction boundary for that line.
//!
//! # Why these are steps, in a file of their own
//!
//! Root cause: `call_tool` in `desktop/mcp.rs` was 131 lines, over the code
//! auditor's 100-line function threshold, because it checked the stop file,
//! authorised the call, asked the pre-hook for a veto, ran the tool with its
//! `screenshot_after`, told the post-hook and wrote the journal in one body;
//! and the file was 2,375 lines, over its 1,000-line threshold. Each of those
//! is a step here with its body and its comments unchanged, and
//! `call_tool` calls them in the order the one body ran them:
//!
//! stop file → permission → pre-hook (only when permitted and not stopped)
//! → outcome (permission refusal, then stop, then veto, else the tool) →
//! post-hook (only when the tool ran) → journal (every call).
//!
//! # Wired to
//!
//! - **Called by** `Server::call_tool` in [`super`], once per `tools/call`.
//! - **Calls** [`crate::desktop::safety`] for the stop file and the stop,
//!   [`crate::permissions::PermissionPolicy::authorize`] for the mode,
//!   [`crate::hooks::HookRunner`] for the operator's commands,
//!   [`Server::run_and_look`] in [`super::tools`] for the tool, and
//!   [`crate::desktop::journal::Journal::record`] for the journal.

use serde_json::Value;

use super::tools::window_arg;
use super::Server;

impl Server {
    /// The operator's stop file, turned into a stop of this server.
    ///
    /// Called by `Server::call_tool` first, before anything else is asked.
    pub(super) fn catch_stop_file(&self) {
        // CHECKED BEFORE THE TOOL RUNS, not after, and here rather than in the
        // stdio loop: `run_stdio` blocks in `read_line`, so a stop noticed
        // there would land only after the agent's NEXT request — which is one
        // action too late, every time. This is the last point before the
        // desktop is touched.
        //
        // The cost is one `exists()` per tool call, beside work that takes a
        // screenshot or walks an accessibility tree.
        if let Some(path) = self.stop_file.as_deref() {
            if crate::desktop::safety::stop_requested(path) {
                self.safety.stop();
            }
        }
    }

    /// Whether this session's permission mode allows `name`, with the
    /// mode's reason when it does not.
    ///
    /// Called by `Server::call_tool` after [`Server::catch_stop_file`].
    pub(super) fn authorise(&self, name: &str, args: &Value) -> Result<(), String> {
        // WHAT THIS SESSION IS ALLOWED TO DO, asked before the stop check is
        // asked about anything and before a single pixel is read.
        //
        // The mode reached the agent (`PermissionMode::agent_mode`) and
        // governed its file writing, but the desktop was outside it entirely:
        // a session started `-p read-only` could not edit a file and could
        // type into a login form. The policy that was written for this and
        // never called now runs here, at the one point every tool passes.
        //
        // No prompter is passed, and there is none to pass: stdin and stdout
        // are the protocol. `Prompt` therefore denies, which is the right
        // answer for an unattended server rather than a gap.
        match self.permissions.authorize(name, &args.to_string(), None) {
            crate::permissions::PermissionOutcome::Allow => Ok(()),
            crate::permissions::PermissionOutcome::Deny { reason } => Err(reason),
        }
    }

    /// The operator's pre-tool hook's refusal, if it refused.
    ///
    /// Called by `Server::call_tool` after [`Server::authorise`]; runs no
    /// hook unless the call was permitted and the server is not stopped.
    pub(super) fn hook_veto(
        &self,
        permitted: &Result<(), String>,
        name: &str,
        raw_args: &str,
    ) -> Option<String> {
        // THE OPERATOR'S OWN VETO, asked last of the three and only if the
        // first two allowed. Ordering matters: a hook is a process spawn, so
        // running it before a call that a mode or a stop would refuse anyway
        // would pay for it to be told about work that is not going to happen.
        //
        // A hook sees the REAL arguments, not `describe`'s redacted line. That
        // is the one place the redaction boundary is deliberately crossed, and
        // [`crate::hooks`] says why: a hook that cannot see what is about to
        // be typed cannot refuse on it.
        match (permitted, self.safety.is_stopped(), self.hooks.as_ref()) {
            (Ok(()), false, Some(hooks)) => {
                let result = hooks.run_pre_tool_use(name, raw_args);
                if result.is_denied() {
                    let said = result.messages().join("; ");
                    Some(if said.is_empty() {
                        format!("refused: a hook denied `{name}` and said nothing about why")
                    } else {
                        format!("refused by a hook: {said}")
                    })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// What the call comes to: the permission refusal, else the stop, else
    /// the hook's veto, else the tool's own result.
    ///
    /// Called by `Server::call_tool` after [`Server::hook_veto`]. The stop is
    /// read again here, not carried over from the veto, as the one body did.
    pub(super) fn outcome(
        &mut self,
        permitted: &Result<(), String>,
        vetoed: Option<String>,
        name: &str,
        args: &Value,
    ) -> Result<Vec<Value>, String> {
        // A STOP HALTS READS TOO, not only the tools that move something.
        //
        // The acting tools already consult `Safety` inside the input thread, so
        // before this the switch left `screen_info`, `screenshot` and `ui_tree`
        // working — a stopped agent that goes on photographing a desktop signed
        // into real accounts. That is the surveillance an operator reaches for
        // the switch to end, so the refusal is applied to every tool here.
        //
        // Deliberately `is_stopped` and not the full `check`: a stop is an
        // explicit, permanent decision. A LAPSED HEARTBEAT is not — it means
        // nobody is supervising right now, it clears on its own, and refusing
        // to answer "what does the screen look like" during it would take away
        // the one tool for finding out why.
        match (permitted, self.safety.is_stopped(), vetoed) {
            (Err(reason), _, _) => Err(reason.clone()),
            (Ok(()), true, _) => Err(crate::desktop::safety::Refusal::Stopped.message()),
            (Ok(()), false, Some(why)) => Err(why),
            (Ok(()), false, None) => self.run_and_look(name, args),
        }
    }

    /// Tell the operator's post-tool hook about a call that ran.
    ///
    /// Called by `Server::call_tool` after [`Server::outcome`].
    pub(super) fn tell_post_hooks(
        &self,
        name: &str,
        raw_args: &str,
        outcome: &Result<Vec<Value>, String>,
    ) {
        // AFTER, and only for a call that actually ran. A post hook told about
        // a refused click would be recording something that did not happen.
        // It cannot deny: the action is already done, and reporting a refusal
        // for it would be a lie to the agent.
        if let Some(hooks) = self.hooks.as_ref() {
            if outcome.is_ok() {
                let said = hooks.run_post_tool_use(name, raw_args, "", false);
                for message in said.messages() {
                    // stderr, never stdout: stdout is the protocol channel.
                    eprintln!("hook after {name}: {message}");
                }
            }
        }
    }

    /// Write the call, ran or refused, to the operator's journal.
    ///
    /// Called by `Server::call_tool` last, after [`Server::tell_post_hooks`].
    pub(super) fn record_call(
        &mut self,
        name: &str,
        args: &Value,
        outcome: &Result<Vec<Value>, String>,
    ) {
        // Journalled HERE rather than inside `run_tool`, because this is the
        // one place every tool passes through with its outcome known. Putting
        // it in the arms would mean twelve call sites and a thirteenth tool
        // added later that nobody remembers to instrument.
        if let Some(journal) = self.journal.as_mut() {
            let (t, detail, ok) = match outcome {
                Ok(_) => (name, Self::describe(name, args), true),
                Err(why) => ("refused", format!("{name}: {why}"), false),
            };
            // A journal that cannot be written is a lost view of the work, not
            // a reason to fail the work the agent actually asked for.
            let _ = journal.record(t, &detail, ok);
        }
    }

    /// One line describing a call, for the operator's journal.
    ///
    /// Called by [`Server::record_call`] for a call that ran.
    ///
    /// # The redaction boundary
    ///
    /// **This is the only place that decides what leaves the process.** The
    /// journal is written to disk and pushed to every browser holding the
    /// stream open, and the agent will be working in real accounts (decision
    /// 7e). So `type_text` and `paste_text` are described by their LENGTH, never
    /// their content: a password typed into a login form must not become a line
    /// in a file and a row in a web page. The default arm is deliberately a
    /// bare tool name rather than a dump of `args` — a tool added later is
    /// under-described until someone thinks about it, which is the safe
    /// direction to fail.
    pub(super) fn describe(name: &str, args: &Value) -> String {
        let s = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let n = |key: &str| args.get(key).and_then(Value::as_i64).unwrap_or_default();

        match name {
            // The id, because that is genuinely all this call carries — the
            // schema is `{id, button}`. The element's role and name live in the
            // `ui_tree` the agent read to get the id, which is itself a
            // journalled call, so an operator can follow `e16` back to it.
            "click_element" => format!("element e{}", n("id")),
            // LENGTH ONLY, like typing: a value set may be a password.
            "set_value" => format!(
                "element e{}, {} characters",
                n("id"),
                s("value").chars().count()
            ),
            "element_action" => format!("element e{} {}", n("id"), s("action")),
            "focus_window" => format!("window {}", window_arg(args).unwrap_or_default()),
            "click" | "move_mouse" => format!("({}, {})", n("x"), n("y")),
            "drag" => format!(
                "({}, {}) -> ({}, {})",
                n("from_x"),
                n("from_y"),
                n("to_x"),
                n("to_y")
            ),
            "scroll" => format!("dx {} dy {}", n("dx"), n("dy")),
            // An ARRAY of key names, and a closed vocabulary at that
            // (`thread::key_from_name`), so it cannot carry typed content.
            "key" => args
                .get("keys")
                .and_then(Value::as_array)
                .map(|keys| {
                    keys.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("+")
                })
                .unwrap_or_default(),
            // LENGTH ONLY. See the redaction note above.
            "type_text" | "paste_text" => {
                format!("{} characters", s("text").chars().count())
            }
            other => other.to_string(),
        }
    }
}
