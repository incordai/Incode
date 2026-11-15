//! The permission gates a call passes before it runs: the settings' rules, the
//! session's mode, the person to ask, and the memory engine.
//!
//! Wired to: every function here is called from [`Toolbox::decide_and_run`]
//! (`toolbox/mod.rs`), in that order — the rules' deny first, then the mode
//! (with the rules' ask and allow), then the operator's pre-tool hook, then
//! the engine. Siblings that also refuse a call, by other routes and in the
//! same function: the agent's tool list (`Toolbox::allows`), the schema check
//! (`toolbox/validate.rs`), the command's allowed-tools
//! (`SessionTools::turn_allowlist`), the guards on protected paths
//! (`Toolbox::refused` and `Toolbox::ask_protected`) and the operator's hooks
//! (`Toolbox::operator_hook_before`), all in `toolbox/mod.rs`.

use super::*;

/// What the memory engine decided about one call, before it runs.
pub(super) enum Gate {
    /// Run it; `context` is guidance for this action, handed to the model with
    /// the result.
    Run { context: Option<String> },
    /// Do not run it; the model is told why, as an error.
    Refused(String),
    /// A question the owner already answered in their app: the answer IS the
    /// result, and it is not an error.
    Answered(String),
    /// The recorder already ran this exact call and served its recorded
    /// output ([`crate::hooks::engine::is_served`]): that output IS the
    /// result. It arrives shaped like a refusal, and handed back as one it
    /// read as a failed call — so the model retried, or worked around a tool
    /// that had in fact answered.
    Served(String),
}

impl Toolbox {
    /// What the settings' permission rules say about this call, if anything.
    pub(super) fn rule_verdict(
        &self,
        name: &str,
        input: &Value,
    ) -> Option<crate::permissions::rules::Verdict> {
        if self.rules.is_empty() {
            return None;
        }
        self.rules
            .decide(name, &self.rule_subject(name, input), &self.rule_context())
    }

    /// What a rule's specifier is matched against for this call: the command,
    /// the paths or the address it names.
    pub(super) fn rule_subject(
        &self,
        name: &str,
        input: &Value,
    ) -> crate::permissions::rules::Subject {
        use crate::permissions::rules::Subject;
        let text = |key: &str| {
            input
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        match name {
            BASH => Subject::Command(text("command")),
            // A search or a listing with no `path` covers the workspace.
            READ | WRITE | EDIT | MULTI_EDIT | NOTEBOOK_EDIT | LS | OUTLINE | GLOB | GREP => {
                let path = text("path");
                Subject::Paths(vec![if path.is_empty() {
                    ".".to_string()
                } else {
                    path
                }])
            }
            APPLY_PATCH | CONVERT_DOCUMENT | RENDER_PDF => {
                let mut paths = self.changed_paths(name, input).unwrap_or_default();
                if let Some(read) = input.get("path").and_then(Value::as_str) {
                    paths.push(read.to_string());
                }
                Subject::Paths(paths)
            }
            crate::web::FETCH_TOOL => Subject::Url(text("url")),
            _ => Subject::None,
        }
    }

    /// Where a rule's relative paths are resolved, and what `~` is.
    pub(super) fn rule_context(&self) -> crate::permissions::rules::PathContext {
        crate::permissions::rules::PathContext {
            workspace: self.workspace.clone(),
            home: crate::admin::home_dir(),
        }
    }

    /// Whether the session's permission mode, and the settings' `ask` and
    /// `allow` rules, let this call run (a `deny` rule was applied before).
    ///
    /// Decided by the call's [`Reach`]: a tool that only observes runs under
    /// every mode; everything else is refused read-only and asked about under
    /// `prompt`. A subagent is refused wherever the mode restricts anything,
    /// because the agent it would start does not carry this mode and could do
    /// what this one may not.
    ///
    /// The rules come after those two refusals and before the rest: read-only
    /// is a promise that nothing changes, which no allow rule breaks. Past it,
    /// an `ask` rule asks the person, whatever the mode — even for a tool that
    /// only observes — and an `allow` rule runs the call without the `prompt`
    /// mode's question.
    pub(super) fn mode_allows(
        &self,
        call_id: &str,
        name: &str,
        input: &Value,
        reach: Reach,
        ruled: Option<&crate::permissions::rules::Verdict>,
    ) -> Result<(), String> {
        use crate::permissions::rules::Effect;
        use crate::permissions::PermissionMode;
        if let Some(mode) = self.mode {
            let restricted = matches!(mode, PermissionMode::ReadOnly | PermissionMode::Prompt);
            if name == AGENT && restricted {
                return Err(format!(
                    "this session is {}, and a subagent would not be held to that. Do the \
                     work here instead.",
                    mode.as_str()
                ));
            }
            if mode == PermissionMode::ReadOnly && !reach.only_observes() {
                return Err(format!(
                    "NOT RUN — this session is read-only, and {name} changes the workspace or \
                     runs code. Describe the change instead of making it."
                ));
            }
        }
        if let Some(verdict) = ruled {
            match verdict.effect {
                Effect::Ask => {
                    let reason = format!("the {} asks before this {name}", verdict.describe());
                    return self.confirm_with_person(call_id, name, input, &reason);
                }
                Effect::Allow => {
                    crate::trace!("tool", "{name} allowed by {}", verdict.describe());
                    return Ok(());
                }
                // Applied before this was asked.
                Effect::Deny => {}
            }
        }
        if reach.only_observes() {
            return Ok(());
        }
        match self.mode {
            Some(PermissionMode::Prompt) => {
                let reason = format!("the session asks before every {name}");
                self.confirm_with_person(call_id, name, input, &reason)
            }
            Some(
                PermissionMode::ReadOnly
                | PermissionMode::WorkspaceWrite
                | PermissionMode::DangerFullAccess
                | PermissionMode::Allow,
            )
            | None => Ok(()),
        }
    }

    /// Put this call to the person at the session, for `reason`. Refused when
    /// nobody can be asked.
    pub(super) fn confirm_with_person(
        &self,
        call_id: &str,
        name: &str,
        input: &Value,
        reason: &str,
    ) -> Result<(), String> {
        let shown = self.event_input(name, input);
        let ask = crate::hooks::engine::Ask {
            call_id,
            tool_name: name,
            tool_input: &shown,
            reason,
        };
        let stopped = || {
            self.cancel
                .as_ref()
                .is_some_and(crate::runtime::cancel::CancelToken::is_cancelled)
        };
        let answer = self
            .prompter
            .as_ref()
            .and_then(|p| p.confirm(&ask, &stopped));
        crate::telemetry::permission_prompt(
            name,
            match &answer {
                Some(crate::hooks::engine::Verdict::Allow) => "allow",
                Some(crate::hooks::engine::Verdict::Deny(_)) => "deny",
                None => "unanswered",
            },
        );
        match answer {
            Some(crate::hooks::engine::Verdict::Allow) => Ok(()),
            Some(crate::hooks::engine::Verdict::Deny(why)) => Err(why),
            None => Err(format!("NOT RUN — {reason}, and nobody could be asked.")),
        }
    }

    /// What the memory engine says about this call.
    ///
    /// # The failure policy (the owner's decision)
    ///
    /// When the engine cannot decide — it is missing where it was named, it
    /// crashed, it ran past its bound — a call that changes or runs something
    /// ([`Reach::fails_closed`]) is REFUSED: the guard is a safety function,
    /// and a change made because the guard was down is the one it exists to
    /// stop. A call that only observes runs, traced. The same rule as the
    /// guards file (`Toolbox::refused`, `toolbox/mod.rs`): an unreadable guard
    /// must not read as no guard.
    pub(super) fn engine_gate(
        &self,
        call_id: &str,
        name: &str,
        input: &Value,
        reach: Reach,
    ) -> Gate {
        let gate = self.engine_gate_inner(call_id, name, input, reach);
        if self.engine.is_some() {
            let (allowed, reason) = match &gate {
                Gate::Run { .. } => (true, None),
                Gate::Refused(why) => (false, Some(why.clone())),
                // The owner answered a question in the app, or the recorder
                // served the call: nothing was refused, and that is the result.
                Gate::Answered(_) | Gate::Served(_) => (true, None),
            };
            self.tell_decided(
                call_id,
                crate::watch::Decision {
                    by: crate::watch::Decider::Engine,
                    allowed,
                    rule: None,
                    path: None,
                    reason: reason
                        .map(|why| crate::text::truncate(&why, MAX_DECISION_REASON_CHARS)),
                },
            );
        }
        gate
    }

    /// [`Self::engine_gate`]'s decision, before it is told.
    fn engine_gate_inner(&self, call_id: &str, name: &str, input: &Value, reach: Reach) -> Gate {
        let Some(engine) = &self.engine else {
            return Gate::Run { context: None };
        };
        let shown = self.event_input(name, input);
        let cancel = self.cancel.as_ref();
        match engine.pre_tool(&self.session, &self.workspace, name, &shown, cancel) {
            crate::hooks::engine::PreTool::Allow { context } => Gate::Run { context },
            // Exit 2 on a question is the owner's ANSWER, given in the app.
            crate::hooks::engine::PreTool::Block(said) if name == ASK_USER_QUESTION => {
                Gate::Answered(said)
            }
            // Never for a write: the engine serves only reads (`Read`, recall,
            // and a one-file `cat`/`head`/`tail`/`sed -n` through the shell),
            // and a "served" answer to a write would read as a write that
            // landed when none was made.
            crate::hooks::engine::PreTool::Block(recorded)
                if reach != Reach::Change && crate::hooks::engine::is_served(&recorded) =>
            {
                Gate::Served(recorded)
            }
            crate::hooks::engine::PreTool::Block(reason) => Gate::Refused(reason),
            crate::hooks::engine::PreTool::Ask { reason, context } => {
                let ask = crate::hooks::engine::Ask {
                    call_id,
                    tool_name: name,
                    tool_input: &shown,
                    reason: &reason,
                };
                let prompter = self.prompter.as_deref();
                match engine.settle(&ask, &self.session, &self.workspace, prompter, cancel) {
                    crate::hooks::engine::Verdict::Allow => Gate::Run { context },
                    crate::hooks::engine::Verdict::Deny(why) => Gate::Refused(why),
                }
            }
            crate::hooks::engine::PreTool::Failed(why) if reach.fails_closed() => {
                Gate::Refused(format!(
                    "NOT RUN — the Incord guard could not decide on this {name}, and a change \
                     is not made unguarded: {why}"
                ))
            }
            crate::hooks::engine::PreTool::Failed(why) => {
                crate::trace!("tool", "{name} runs unguarded, the engine failed: {why}");
                Gate::Run { context: None }
            }
        }
    }
}
