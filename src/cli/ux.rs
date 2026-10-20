//! The REPL's operator preferences, in one place: notifications, the status
//! line, the editing mode and key bindings, and `/cost`.
//!
//! Kept out of the REPL loop so the loop only says WHEN each happens.

use crate::outln;

use super::footer::Footer;
use super::input::LineEditor;
use super::keys::{EditingMode, Keymap};
use super::statusline::{Input, StatusLine};

/// The preferences in force for this REPL session.
pub struct Ux {
    editing: EditingMode,
    statusline: Option<StatusLine>,
}

impl Ux {
    /// Read the settings and apply them to whichever input is in use.
    pub fn start(
        admin: &crate::admin::AdminConfig,
        data_dir: &std::path::Path,
        footer: Option<&mut Footer>,
        editor: &mut LineEditor,
    ) -> Self {
        super::notify::configure(super::notify::Settings::from_admin(admin));
        let editing = EditingMode::parse(admin.editing_mode.as_deref());
        let (keymap, problems) = Keymap::load(data_dir);
        for problem in problems {
            super::render::print_error(&format!(
                "{}: {problem} (skipped)",
                data_dir.join(super::keys::KEYBINDINGS_FILE).display()
            ));
        }
        editor.set_editing_mode(editing);
        if let Some(f) = footer {
            f.set_keymap(keymap);
            f.set_editing_mode(editing);
        }
        Self {
            editing,
            statusline: StatusLine::from_admin(admin),
        }
    }

    /// `/vim`: switch between vi and emacs editing for this session.
    pub fn toggle_vim(&mut self, footer: Option<&mut Footer>, editor: &mut LineEditor) {
        self.editing = self.editing.toggled();
        editor.set_editing_mode(self.editing);
        if let Some(f) = footer {
            f.set_editing_mode(self.editing);
        }
        super::render::print_info(&match self.editing {
            EditingMode::Vi => "Vi editing on: Esc for normal mode, i to type. Set \"editing_mode\": \"vi\" in admin.json to keep it.".to_string(),
            EditingMode::Emacs => "Vi editing off.".to_string(),
        });
    }

    /// Re-run the status line command, when there is one, and show its line.
    pub fn refresh_status(
        &mut self,
        footer: Option<&mut Footer>,
        session: &super::agent_turn::AgentSession,
        workspace: &std::path::Path,
        git: Option<&crate::tools::git_context::GitContext>,
    ) {
        let (Some(status), Some(f)) = (self.statusline.as_mut(), footer) else {
            return;
        };
        let input = status_input(session, workspace, git);
        let line = status.refresh(&input, workspace).map(str::to_string);
        f.set_status_line(line);
    }
}

/// What the status line command is told about this session.
#[must_use]
pub fn status_input(
    session: &super::agent_turn::AgentSession,
    workspace: &std::path::Path,
    git: Option<&crate::tools::git_context::GitContext>,
) -> Input {
    Input {
        cwd: workspace.display().to_string(),
        workspace: workspace
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        model: session
            .model()
            .map_or_else(|| session.backend_id().to_string(), str::to_string),
        session_id: session.session_id().map(str::to_string),
        git_branch: git.map(|g| g.branch.clone()),
        git_dirty: git.is_some_and(|g| g.is_dirty),
        ..Input::default()
    }
    .with_usage(session.usage_by_model())
}

/// `/cost`: the session's tokens and estimated dollars, per model.
pub fn print_cost(session: &super::agent_turn::AgentSession) {
    super::render::print_header("Cost");
    for line in crate::runtime::usage::cost_report(session.usage_by_model()) {
        outln!("  {line}");
    }
}
