//! One `session/prompt`, in named steps: the prompt read and the operator's
//! command expanded, the client's choices followed, `/run` and the slash
//! commands run here, the turn planned, run and streamed, and what the
//! session holds afterwards said and recorded.
//!
//! # Why these are steps, in a file of their own
//!
//! Root cause: `run_prompt` in `acp_agent/mod.rs` was 216 lines, over the
//! code auditor's 100-line function threshold, because it mixed reading the
//! prompt, slash-command dispatch, turn planning, running the turn and the
//! bookkeeping after it in one body. Each of those is now a named step
//! function here, called in the order the one body ran them, with no change
//! in behaviour: every update, answer and refusal reaches the client in the
//! same order as before. The steps live in this file rather than in
//! `mod.rs`, which holds the reader and the session's thread and is over
//! three thousand lines with its tests.
//!
//! # Wired to
//!
//! - **Called by** [`super::serve_prompts`], once per prompt and inside its
//!   panic guard, through [`run_prompt`].
//! - **Calls** [`crate::cli::plan_turn`] and
//!   [`crate::cli::agent_turn::AgentSession::turn_observed`] for the turn, as
//!   the REPL does; [`super::config`] and [`super::history`] for the pickers
//!   and the session record; [`super::commands::run`] for a slash command and
//!   [`super::run::run`] for `/run`.

use std::path::Path;
use std::sync::Arc;

use agent_client_protocol_schema::v1 as acp;

use crate::agent::{AgentEvent, TurnOutcome};
use crate::cli::agent_turn::TurnStart;
use crate::cli::slash_commands::SlashCommand;

use super::transport::Outbox;
use super::{
    commands, config, connecting_note, history, internal, invalid_params, present, run, say,
    workers, Held, Shared,
};

/// One prompt: the same turn an interactive line runs, streamed as updates —
/// or, when the prompt is a slash command, that command.
///
/// The prompt is read by [`read_prompt`] and the client's choices put in
/// force by [`follow_the_client`]; `/run` goes to [`run::run`] and any other
/// slash command to [`run_command`]; [`plan`] decides whether a turn runs,
/// [`run_turn`] runs it and [`after_turn`] says and records what it left.
pub(super) fn run_prompt(
    held: &mut Held,
    outbox: &Arc<Outbox>,
    session_id: &acp::SessionId,
    shared: &Shared,
    blocks: &[acp::ContentBlock],
) -> Result<acp::PromptResponse, acp::Error> {
    let workspace = held.workspace.clone();
    let workspace = workspace.as_path();
    let asked = read_prompt(held, blocks, workspace)?;
    follow_the_client(held, shared, outbox, session_id);

    if shared.cancelled() {
        return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
    }

    // `/run` works the ledger for as long as it takes, streaming, with the
    // ledger as the plan, and its lanes as sub-agents.
    if matches!(
        SlashCommand::parse(asked.text.trim()),
        Some(SlashCommand::Run)
    ) {
        return Ok(acp::PromptResponse::new(run::run(
            held.runs.as_mut(),
            workspace,
            held.applied,
            outbox,
            session_id,
            shared,
            &held.watch,
        )));
    }

    if let Some(note) = &asked.expanded {
        say_text(outbox, session_id, &format!("{note}\n\n"));
    }
    if let Some(stop) = run_command(held, outbox, session_id, shared, &asked.text, workspace) {
        return Ok(acp::PromptResponse::new(stop));
    }
    let turn = match plan(
        held,
        outbox,
        session_id,
        shared,
        workspace,
        &asked.text,
        asked.images,
    )? {
        Planned::Turn(turn) => turn,
        Planned::Answered(stop) => return Ok(acp::PromptResponse::new(stop)),
    };
    let (who, outcome) = run_turn(
        held,
        outbox,
        session_id,
        shared,
        workspace,
        &turn,
        asked.allowed_tools,
    );
    after_turn(held, outbox, session_id, shared, &asked.typed);

    // A cancel decides the answer whatever the turn did with it: ACP requires
    // `cancelled` once the client has asked, even if stopping surfaced as an
    // error underneath.
    if shared.cancelled() {
        return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
    }
    match outcome {
        Ok(outcome) => Ok(
            acp::PromptResponse::new(present::stop_reason(&outcome.stop_reason))
                .meta(workers::prompt_meta(&who)),
        ),
        Err(why) => Err(internal(why)),
    }
}

/// A prompt, once read.
struct Asked {
    /// The prompt's text as the person typed it, before any command of the
    /// operator's was expanded: what the session record is named from.
    typed: String,
    /// What the prompt asks: its text, or what the operator's command
    /// expanded it into.
    text: String,
    /// The pictures it carries.
    images: Vec<crate::toolbox::Image>,
    /// The expanded command's `allowed-tools`, which hold the turn it
    /// starts; empty when no command was expanded.
    allowed_tools: Vec<String>,
    /// The note that says a command was expanded.
    expanded: Option<String>,
}

/// The prompt's text and pictures, with the operator's own command expanded;
/// refused when it has no text, or brings pictures to a slash command.
///
/// Called by [`run_prompt`], before anything else is done with the prompt.
fn read_prompt(
    held: &Held,
    blocks: &[acp::ContentBlock],
    workspace: &Path,
) -> Result<Asked, acp::Error> {
    let present::Prompted { text, images } =
        present::prompt_parts(blocks, workspace).map_err(invalid_params)?;
    if text.trim().is_empty() {
        return Err(invalid_params("the prompt has no text"));
    }
    // The operator's own command becomes the prompt, as it does in the REPL:
    // before anything asks whether the prompt is a slash command, so it is
    // never answered as one that is not offered. Its `allowed-tools` hold
    // the turn it starts.
    let typed = text.clone();
    let (text, allowed_tools, expanded) = match held.custom.expand_line(&text, workspace) {
        Some(expanded) => (
            expanded.prompt.clone(),
            expanded.allowed_tools.clone(),
            Some(crate::custom_commands::note(&expanded)),
        ),
        None => (text, Vec::new(), None),
    };
    // Pictures go to the model with a question; a command runs here and has
    // nowhere to put them.
    if !images.is_empty() && SlashCommand::parse(text.trim()).is_some() {
        return Err(invalid_params(
            "pictures go with a prompt to the model, not with a slash command",
        ));
    }
    Ok(Asked {
        typed,
        text,
        images,
        allowed_tools,
        expanded,
    })
}

/// The mode, agent and role the client last chose, put in force.
///
/// Called by [`run_prompt`] before the prompt runs, because a running
/// agent's mode was fixed when it was started.
fn follow_the_client(
    held: &mut Held,
    shared: &Shared,
    outbox: &Outbox,
    session_id: &acp::SessionId,
) {
    // The mode the client chose, sent to the thing that does the writing —
    // as `-p` is at startup.
    let wanted = shared.state().mode;
    if let Some(wanted) = wanted {
        if wanted != held.applied {
            held.session.set_permission_mode(wanted.agent_mode());
            held.applied = wanted;
        }
    }
    // And the agent and role the client picked, for the same reason.
    config::apply(held, shared, outbox, session_id);
}

/// A slash command, run here as the REPL runs it: how the prompt stops, or
/// `None` when `text` is not one.
///
/// Called by [`run_prompt`]. The command never reaches the agent. It runs to
/// the end: a cancel that lands meanwhile decides the answer, as it does for
/// a turn.
fn run_command(
    held: &mut Held,
    outbox: &Outbox,
    session_id: &acp::SessionId,
    shared: &Shared,
    text: &str,
    workspace: &Path,
) -> Option<acp::StopReason> {
    let ran = commands::run(text, &mut held.session, workspace, &held.settings)?;
    say_text(outbox, session_id, &ran.said);
    if let Some(mode) = ran.mode {
        // The command switched the mode; the client's picker, and the
        // mode the next prompt asks for, follow it.
        held.applied = mode;
        shared.state().mode = Some(mode);
        say(
            outbox,
            session_id,
            acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(mode.as_str())),
        );
    }
    Some(if shared.cancelled() {
        acp::StopReason::Cancelled
    } else {
        acp::StopReason::EndTurn
    })
}

/// A turn about to run: what the agent is asked, and the context that goes
/// before it.
struct Turn {
    instruction: String,
    context: Vec<String>,
}

/// How a prompt goes on once it is planned.
enum Planned {
    /// An agent turn runs.
    Turn(Turn),
    /// No turn runs; the prompt stops so.
    Answered(acp::StopReason),
}

/// Whether a turn runs for `text`, and what it carries: the pictures
/// attached to the agent that leads, the plan made as the REPL makes it,
/// and, in its context, a loaded session's earlier conversation and the note
/// about the client's servers still connecting. When no turn runs, the
/// pictures are taken back off so they do not ride the next one.
///
/// Called by [`run_prompt`], once the prompt is known not to be a command.
fn plan(
    held: &mut Held,
    outbox: &Outbox,
    session_id: &acp::SessionId,
    shared: &Shared,
    workspace: &Path,
    text: &str,
    images: Vec<crate::toolbox::Image>,
) -> Result<Planned, acp::Error> {
    let session = &mut held.session;
    // The pictures go to the agent that leads now, before anything else is
    // done with the prompt: an agent that cannot take them refuses the
    // prompt whole, before the engine or the transcript have heard it.
    let pictures = !images.is_empty();
    if pictures {
        session.attach_images(images).map_err(invalid_params)?;
    }
    let (instruction, mut context) = match crate::cli::plan_turn(session, workspace, text, text) {
        crate::cli::TurnPlan::NextQuestion(reply) | crate::cli::TurnPlan::Greeting(reply) => {
            if pictures {
                // No turn runs: the pictures must not ride the next one.
                let _ = session.attach_images(Vec::new());
            }
            say_text(outbox, session_id, &reply);
            return Ok(Planned::Answered(acp::StopReason::EndTurn));
        }
        crate::cli::TurnPlan::Refused(why) => {
            if pictures {
                let _ = session.attach_images(Vec::new());
            }
            say_text(
                outbox,
                session_id,
                &format!("Not sent: the operator's user_prompt_submit hook refused it. {why}"),
            );
            return Ok(Planned::Answered(acp::StopReason::EndTurn));
        }
        crate::cli::TurnPlan::Agent {
            instruction,
            context,
        } => (instruction, context),
    };
    // A loaded session whose model could not resume its own conversation is
    // given it here, once, ahead of everything else the turn carries.
    if let Some(earlier) = held.replay.take() {
        context.insert(0, earlier);
    }
    // A turn does not wait for the client's MCP servers still connecting: it
    // runs without their tools, and the model is told so, by name.
    if let Some(note) = connecting_note(&session.session_tools().client_servers_connecting()) {
        context.push(note);
    }

    // Looked at again AFTER the plan, which can wait on the memory service:
    // a cancel that landed during that wait is honoured here, before an agent
    // is started, rather than being cleared when the turn resets its token.
    if shared.cancelled() {
        if pictures {
            let _ = session.attach_images(Vec::new());
        }
        return Ok(Planned::Answered(acp::StopReason::Cancelled));
    }
    Ok(Planned::Turn(Turn {
        instruction,
        context,
    }))
}

/// The turn, streamed to the client: its text as agent message chunks, its
/// steps through the session's watch. Who served it, and how it ended.
///
/// Called by [`run_prompt`], after every way out of the prompt that runs no
/// turn.
fn run_turn(
    held: &mut Held,
    outbox: &Outbox,
    session_id: &acp::SessionId,
    shared: &Shared,
    workspace: &Path,
    turn: &Turn,
    allowed_tools: Vec<String>,
) -> (workers::Who, Result<TurnOutcome, String>) {
    let session = &mut held.session;
    // Who serves this turn, for every step's `_meta` and the answer's.
    let who = workers::Who {
        role: Some(held.role.clone()),
        agent: Some(held.lead.clone()),
        model: session.model().map(str::to_string),
        lane: None,
    };
    held.watch.serve_as(who.clone());
    let mut presenter = held.watch.presenter(workspace);
    // Set here, after every way out of the prompt that runs no turn, so it
    // is taken by this turn and no other.
    session.restrict_next_turn(allowed_tools);
    let outcome = session.turn_observed(
        workspace,
        &turn.instruction,
        &turn.context,
        &mut |fragment: &str| {
            shared.hold_cancel();
            say_text(outbox, session_id, fragment);
        },
        &mut |_status| {},
        Some(&mut |event: &AgentEvent, start: &TurnStart<'_>| {
            shared.hold_cancel();
            for update in presenter.updates(event, &|path| start.before(path)) {
                say(outbox, session_id, update);
            }
        }),
    );
    (who, outcome)
}

/// What the session holds for the person once the turn is over — a
/// question the agent asked, a guard the sentinel caught — said after the
/// reply, as the REPL prints these after the status line stops. Then which
/// conversation the model holds now, so the session can be loaded and
/// resumed; and the pickers, when the turn changed what serves the session
/// (a standby took over) or what model it asks for.
///
/// Called by [`run_prompt`], after the turn, whatever it did.
fn after_turn(
    held: &mut Held,
    outbox: &Outbox,
    session_id: &acp::SessionId,
    shared: &Shared,
    typed: &str,
) {
    for notice in held.session.take_notices() {
        say_text(outbox, session_id, &format!("\n{notice}\n"));
    }
    history::remember(held, session_id, typed);
    config::report(held, shared, outbox, session_id);
}

/// Text for the person, as an agent message chunk; nothing when it is empty.
fn say_text(outbox: &Outbox, session_id: &acp::SessionId, text: &str) {
    if !text.is_empty() {
        say(
            outbox,
            session_id,
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::from(
                text,
            ))),
        );
    }
}
