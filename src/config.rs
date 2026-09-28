//! IncordCodeConfig — model, provider, workspace path, lifecycle hooks, MCP.
//!
//! The operator's pre- and post-tool commands are not here: they are read
//! from `admin.json` (`desktop_hooks`), where they apply to every session.

use std::path::PathBuf;

/// Environment variable naming the session's model.
///
/// Lets an operator choose without editing a flag into every invocation, which
/// is the point of "the operator picks, the code does not".
pub const MODEL_ENV: &str = "INCODE_MODEL";

/// The model used when neither `--model` nor [`MODEL_ENV`] says otherwise.
///
/// A last resort, not a recommendation. It is a hardcoded model name in a
/// config, which the project's own rules argue against — kept because a harness
/// that refuses to start until an env var is set is worse for a fresh install
/// than one that starts on a stated default.
const FALLBACK_MODEL: &str = "sonnet";

/// The built-in last resort, for the admin layer to fall back to.
#[must_use]
pub fn fallback_model() -> String {
    FALLBACK_MODEL.to_string()
}

/// The model to start with, preferring the operator's choice.
#[must_use]
pub fn default_model() -> String {
    std::env::var(MODEL_ENV)
        .ok()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| FALLBACK_MODEL.to_string())
}

/// Top-level config for `incode`.
#[derive(Debug, Clone)]
pub struct IncordCodeConfig {
    /// Which model this session runs on.
    ///
    /// Resolved by the provider registry, which is data-driven — so any model a
    /// registered provider claims is valid here. The operator picks it with
    /// `--model` or [`MODEL_ENV`]; the built-in fallback below exists only so a
    /// fresh install starts, and naming a specific model in code is a wart this
    /// crate would rather not have.
    pub model: String,
    /// The model named on the command line, if one was.
    ///
    /// Kept separate from `model` so the admin layer can tell a deliberate flag
    /// from a resolved default — otherwise a flag and a fallback look identical
    /// and the flag cannot be given the precedence it earns.
    pub model_from_flag: Option<String>,
    /// Workspace root for file operations.
    pub workspace: PathBuf,
    /// Session file path for save/resume.
    /// An agent session to continue, by the id the agent assigned it.
    ///
    /// Not a path: this harness stores no transcript, so there is no file to
    /// point at. The id is what reaches the conversation again.
    pub resume_session: Option<String>,
    /// Extra directories the work may reach, beyond the workspace.
    ///
    /// Everything that protects the workspace protects these too: the sentinel
    /// watches them, the checkpoint snapshots them, and the agent's deny rules
    /// already match by pattern rather than by root. Extending reach without
    /// extending protection would let a project be made editable by its own
    /// agent just by naming the directory its tests live in.
    pub additional_dirs: Vec<std::path::PathBuf>,
    /// Permission mode string ("workspace-write", "read-only", "danger-full-access").
    pub permission_mode: String,
    /// Lifecycle hooks (on_session_start, on_session_end, on_compact).
    pub lifecycle_hooks: crate::runtime::lifecycle::LifecycleHooks,
    /// Whether to filter env vars for subprocess spawning (default: true).
    pub env_filter_enabled: bool,
}

impl Default for IncordCodeConfig {
    fn default() -> Self {
        Self {
            model: default_model(),
            model_from_flag: None,
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            resume_session: None,
            additional_dirs: Vec::new(),
            permission_mode: "workspace-write".to_string(),
            lifecycle_hooks: crate::runtime::lifecycle::LifecycleHooks::default(),
            env_filter_enabled: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The operator's choice must win over the built-in name, which is the
    /// whole reason the env var exists.
    #[test]
    fn the_environment_chooses_the_model_over_the_built_in_fallback() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        std::env::set_var(MODEL_ENV, "an-operator-chosen-model");
        let chosen = default_model();
        std::env::remove_var(MODEL_ENV);

        assert_eq!(chosen, "an-operator-chosen-model");
    }

    /// A blank or unset variable is not a choice. Treating "" as a model name
    /// would ask a provider for a model called nothing.
    #[test]
    fn a_blank_choice_falls_back_rather_than_asking_for_an_empty_model() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        std::env::set_var(MODEL_ENV, "   ");
        let blank = default_model();
        std::env::remove_var(MODEL_ENV);
        let unset = default_model();

        assert!(!blank.is_empty(), "a blank value must not become the model");
        assert_eq!(blank, unset, "blank and unset must resolve the same way");
    }
}
