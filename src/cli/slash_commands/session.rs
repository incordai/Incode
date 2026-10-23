//! The commands that act on this conversation: undo and rewind, plan and act,
//! the model, the output style, a picture for the next message, the sub-agent
//! list, and a bug report.

use crate::cli::render;
use crate::{out, outln};

/// `/undo`: put back the files the last turn changed.
pub fn handle_undo(
    session: &mut crate::cli::agent_turn::AgentSession,
    workspace: &std::path::Path,
) {
    match session.undo(workspace) {
        Ok(outcome) => say_undone(&outcome),
        Err(e) => render::print_error(&e),
    }
}

/// What one undone turn put back, and what it could not.
fn say_undone(outcome: &crate::tools::checkpoint::UndoOutcome) {
    outln!("Undid: {}", outcome.label);
    for path in &outcome.restored {
        outln!("  restored {path}");
    }
    // Never swallowed: a partial undo the user believes was complete is
    // worse than none.
    for failure in &outcome.failed {
        render::print_error(&format!("  could NOT restore {failure}"));
    }
}

/// `/rewind <n>`: put the files back as they were `turns` changed turns ago,
/// undoing the newest first. Stops at the first turn that cannot be undone,
/// and says how far it got.
pub fn handle_rewind(
    session: &mut crate::cli::agent_turn::AgentSession,
    workspace: &std::path::Path,
    turns: usize,
) {
    let (undone, stopped) = session.rewind(workspace, turns);
    for outcome in &undone {
        say_undone(outcome);
    }
    match stopped {
        Some(why) => render::print_error(&format!(
            "Rewound {} of {turns} turn(s), then stopped: {why}",
            undone.len()
        )),
        None => outln!("Rewound {turns} turn(s)."),
    }
}

/// `/agents`: the sub-agents defined for this workspace and for the
/// operator, and every definition file left out with why.
pub fn handle_agents(workspace: &std::path::Path) {
    out!("{}", crate::subagents::load(workspace).report());
}

/// The most bytes a picture file `/image` reads may have. Past the image
/// bounds it is shrunk to fit (`toolbox::image`); this bounds what is read
/// before that is tried.
const MAX_IMAGE_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// `/image <path>`: a PNG or JPEG sent with the next message, made to fit the
/// image bounds as a screenshot is. One picture: a second `/image` before
/// the message replaces the first. An agent that cannot take pictures says
/// so here, before anything is sent.
pub fn handle_image(
    session: &mut crate::cli::agent_turn::AgentSession,
    workspace: &std::path::Path,
    path: &str,
) {
    let path = path.trim().trim_matches('"');
    let file = workspace.join(path);
    let read = std::fs::metadata(&file)
        .map_err(|e| format!("`{path}` could not be read: {e}"))
        .and_then(|meta| {
            if meta.len() > MAX_IMAGE_FILE_BYTES {
                Err(format!(
                    "`{path}` is {} bytes, past {MAX_IMAGE_FILE_BYTES}",
                    meta.len()
                ))
            } else {
                std::fs::read(&file).map_err(|e| format!("`{path}` could not be read: {e}"))
            }
        })
        .and_then(|bytes| {
            crate::toolbox::Image::fitted(bytes).map_err(|e| format!("`{path}`: {e}"))
        });
    let (picture, note) = match read {
        Ok(fitted) => fitted,
        Err(why) => return render::print_error(&why),
    };
    let shown = picture.placeholder();
    match session.attach_images(vec![picture]) {
        Ok(()) => {
            outln!("Attached {shown}: it goes with your next message.");
            if let Some(note) = note {
                outln!("  ({note})");
            }
        }
        Err(why) => render::print_error(&why),
    }
}

/// `/plan`: the agent proposes changes without making them.
///
/// The agent enforces this itself, for its own tools — which is the only place
/// it can be enforced now.
pub fn handle_plan(session: &mut crate::cli::agent_turn::AgentSession) {
    session.set_permission_mode(crate::cli::agent_turn::PLAN_MODE);
    render::print_info("Plan mode: the agent will propose changes without making them.");
}

/// `/act`: the agent applies changes, within the guards.
pub fn handle_act(session: &mut crate::cli::agent_turn::AgentSession) {
    session.set_permission_mode(crate::cli::agent_turn::ACT_MODE);
    render::print_info("Act mode: the agent applies changes, within the guards.");
}

/// Report the outcome of a `/model` request.
///
/// `switched` is what the client actually did. This used to print "switching
/// to X" unconditionally while changing nothing, so every later turn quietly
/// went to the original model at the original price.
pub fn handle_model(current: &str, new_model: Option<&str>, switched: bool) {
    match new_model {
        Some(name) if switched => {
            render::print_info(&format!("Model switched to: {name}"));
        }
        Some(name) => {
            render::print_error(&format!(
                "Cannot switch to '{name}' — this session's provider does not serve it. \
                 Still using {current}."
            ));
        }
        None => {
            render::print_info(&format!("Current model: {current}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Output style handler
// ---------------------------------------------------------------------------

/// `/style [name|off]`: with no name, list the styles and the one in force;
/// with one, switch to it. Returns the session's new style block when it
/// changes (`Some(None)`: none); the caller hands it to
/// `AgentSession::set_output_style`. `current` is the style's name.
pub fn handle_style(
    arg: Option<&str>,
    workspace: &std::path::Path,
    current: &mut Option<String>,
) -> Option<Option<String>> {
    let styles = crate::styles::load(workspace);
    for problem in &styles.problems {
        render::print_error(problem);
    }
    let Some(name) = arg.map(str::trim) else {
        render::print_header("Output styles");
        if styles.styles.is_empty() {
            outln!(
                "  none yet: write one as a Markdown file in {}/ or ~/.incode/{}/",
                crate::styles::PROJECT_DIR,
                crate::styles::USER_DIR
            );
        }
        for style in &styles.styles {
            let mark = if current.as_deref() == Some(style.name.as_str()) {
                "*"
            } else {
                " "
            };
            // Said beside each: a project's style came with the repository.
            let from = match style.origin {
                crate::styles::Origin::Operator => "yours",
                crate::styles::Origin::Project => "this project",
            };
            outln!(
                "  {mark} {:<16} {:<13} {}",
                style.name,
                from,
                style.description.as_deref().unwrap_or("")
            );
        }
        outln!();
        render::print_info("/style <name> to answer in one, /style off for none.");
        return None;
    };
    if name.eq_ignore_ascii_case("off") || name.eq_ignore_ascii_case("none") {
        *current = None;
        render::print_info("No output style from the next reply.");
        return Some(None);
    }
    match styles.find(name) {
        Some(style) => {
            *current = Some(style.name.clone());
            render::print_info(&format!(
                "Output style `{}` from the next reply, for this session.",
                style.name
            ));
            Some(Some(crate::styles::block(style)))
        }
        None => {
            render::print_error(&format!(
                "No style `{name}` in {}/ or ~/.incode/{}/. /style lists them.",
                crate::styles::PROJECT_DIR,
                crate::styles::USER_DIR
            ));
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Bug report handler
// ---------------------------------------------------------------------------

/// `/bug [what happened]`: print what a report would hold and the link that
/// prefills it ([`crate::bug_report`]). Sends nothing and opens nothing.
pub fn handle_bug(model: &str, permission_mode: &str, description: Option<&str>) {
    let home = crate::admin::home_dir().map(|h| h.to_string_lossy().into_owned());
    let report = crate::bug_report::report(&crate::bug_report::Facts {
        model,
        permission_mode,
        description: description.unwrap_or(""),
        home: home.as_deref(),
    });
    render::print_header("Bug report");
    for line in report.text.lines() {
        outln!("  {line}");
    }
    if report.cut {
        render::print_info(
            "The description in the link is cut to keep the link openable; paste the rest in \
             the issue.",
        );
    }
    outln!();
    render::print_info(
        "Nothing has been sent. Open this link to file it — check the fields first; tokens \
         and your home folder are removed, but not everything private looks like a secret:",
    );
    outln!("  {}", report.url);
    outln!();
}
