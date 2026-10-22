//! Task lifecycle hooks — run user-defined scripts at session boundaries.
//!
//! Configurable via IncordCodeConfig:
//! - `on_session_start`: Run when a new session begins (e.g. "npm install")
//! - `on_session_end`: Run when session ends (e.g. "cargo fmt")
//! - `on_compact`: Run after compaction (e.g. custom cleanup)
//!
//! Superseded by the operator's `session_start` / `session_end` hooks
//! (`crate::hooks::HookConfig`); kept so an existing `lifecycle.json` still
//! runs.

use std::path::Path;
use std::process::Command;

/// Lifecycle hook configuration.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct LifecycleHooks {
    /// Script to run when a session starts.
    #[serde(default)]
    pub on_session_start: Option<String>,
    /// Script to run when a session ends.
    #[serde(default)]
    pub on_session_end: Option<String>,
    /// Script to run after compaction.
    #[serde(default)]
    pub on_compact: Option<String>,
}

/// Result of running a lifecycle hook.
#[derive(Debug, Clone)]
pub struct HookResult {
    pub hook_name: String,
    pub command: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub output: String,
    pub error: String,
}

impl LifecycleHooks {
    /// Check if any hooks are configured.
    #[must_use]
    pub fn has_any(&self) -> bool {
        self.on_session_start.is_some()
            || self.on_session_end.is_some()
            || self.on_compact.is_some()
    }

    /// Run the session start hook (if configured).
    pub fn run_session_start(&self, workspace: &Path) -> Option<HookResult> {
        self.on_session_start
            .as_deref()
            .map(|cmd| run_hook("on_session_start", cmd, workspace))
    }

    /// Run the session end hook (if configured).
    pub fn run_session_end(&self, workspace: &Path) -> Option<HookResult> {
        self.on_session_end
            .as_deref()
            .map(|cmd| run_hook("on_session_end", cmd, workspace))
    }

    /// Run the compaction hook (if configured).
    pub fn run_on_compact(&self, workspace: &Path) -> Option<HookResult> {
        self.on_compact
            .as_deref()
            .map(|cmd| run_hook("on_compact", cmd, workspace))
    }

    /// Load from a JSON file (e.g. `.incode/lifecycle.json`).
    pub fn load_from_file(path: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Load from workspace `.incode.json` or `.incode/lifecycle.json`.
    pub fn discover(workspace: &Path) -> Option<Self> {
        // Try workspace/.incode/lifecycle.json
        let lifecycle_path = workspace.join(".incode").join("lifecycle.json");
        if let Some(hooks) = Self::load_from_file(&lifecycle_path) {
            if hooks.has_any() {
                return Some(hooks);
            }
        }

        // Try workspace/.incode.json (lifecycle section)
        let config_path = workspace.join(".incode.json");
        if let Ok(content) = std::fs::read_to_string(config_path) {
            if let Ok(config) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(lifecycle) = config.get("lifecycle") {
                    if let Ok(hooks) = serde_json::from_value::<Self>(lifecycle.clone()) {
                        if hooks.has_any() {
                            return Some(hooks);
                        }
                    }
                }
            }
        }

        None
    }
}

/// Run a lifecycle hook command.
fn run_hook(name: &str, command: &str, workspace: &Path) -> HookResult {
    let cmd_str = command.to_string();

    #[cfg(windows)]
    let result = Command::new("cmd")
        .args(["/C", command])
        .current_dir(workspace)
        .output();

    #[cfg(not(windows))]
    let result = Command::new("sh")
        .args(["-c", command])
        .current_dir(workspace)
        .output();

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let truncated_output = if stdout.len() > 500 {
                format!("{}...", crate::text::head_bytes(&stdout, 500))
            } else {
                stdout
            };

            HookResult {
                hook_name: name.to_string(),
                command: cmd_str,
                success: output.status.success(),
                exit_code: output.status.code(),
                output: truncated_output,
                error: stderr,
            }
        }
        Err(e) => HookResult {
            hook_name: name.to_string(),
            command: cmd_str,
            success: false,
            exit_code: None,
            output: String::new(),
            error: format!("Failed to spawn: {e}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_hooks_has_none() {
        let hooks = LifecycleHooks::default();
        assert!(!hooks.has_any());
        assert!(hooks.run_session_start(Path::new(".")).is_none());
        assert!(hooks.run_session_end(Path::new(".")).is_none());
        assert!(hooks.run_on_compact(Path::new(".")).is_none());
    }

    #[test]
    fn hooks_with_start() {
        let hooks = LifecycleHooks {
            on_session_start: Some("echo hello".into()),
            on_session_end: None,
            on_compact: None,
        };
        assert!(hooks.has_any());
        let result = hooks.run_session_start(Path::new(".")).unwrap();
        assert!(result.success);
        assert!(result.output.contains("hello"));
        assert_eq!(result.hook_name, "on_session_start");
    }

    #[test]
    fn hooks_with_failing_command() {
        let hooks = LifecycleHooks {
            on_session_start: Some("exit 1".into()),
            on_session_end: None,
            on_compact: None,
        };
        let result = hooks.run_session_start(Path::new(".")).unwrap();
        assert!(!result.success);
    }

    #[test]
    fn discover_returns_none_for_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(LifecycleHooks::discover(dir.path()).is_none());
    }

    #[test]
    fn discover_from_lifecycle_json() {
        let dir = tempfile::tempdir().unwrap();
        let incode_dir = dir.path().join(".incode");
        std::fs::create_dir_all(&incode_dir).unwrap();
        std::fs::write(
            incode_dir.join("lifecycle.json"),
            r#"{"on_session_start": "echo start"}"#,
        )
        .unwrap();

        let hooks = LifecycleHooks::discover(dir.path()).unwrap();
        assert_eq!(hooks.on_session_start.as_deref(), Some("echo start"));
    }

    #[test]
    fn load_from_incode_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".incode.json"),
            r#"{"lifecycle": {"on_session_end": "cargo fmt"}}"#,
        )
        .unwrap();

        let hooks = LifecycleHooks::discover(dir.path()).unwrap();
        assert_eq!(hooks.on_session_end.as_deref(), Some("cargo fmt"));
    }

    #[test]
    fn serialization_roundtrip() {
        let hooks = LifecycleHooks {
            on_session_start: Some("npm install".into()),
            on_session_end: Some("cargo fmt".into()),
            on_compact: None,
        };
        let json = serde_json::to_string(&hooks).unwrap();
        let loaded: LifecycleHooks = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.on_session_start, hooks.on_session_start);
        assert_eq!(loaded.on_session_end, hooks.on_session_end);
    }
}
