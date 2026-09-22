//! Tool status cache — detect installed CLI tools and cache results.
//!
//! Auto-detects common developer tools (git, cargo, node, python, docker, etc.)
//! and caches their paths + versions. Persisted to `~/.incord-code/tool-status.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Status of a single CLI tool.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolStatus {
    pub installed: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    pub last_checked: u64,
}

/// Cache of tool detection results.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolStatusCache {
    tools: HashMap<String, ToolStatus>,
    #[serde(skip)]
    file_path: Option<PathBuf>,
}

/// A tool to detect (name, command, version args).
struct ToolProbe {
    name: &'static str,
    commands: &'static [&'static str],
    version_args: &'static [&'static str],
}

const TOOLS_TO_DETECT: &[ToolProbe] = &[
    ToolProbe {
        name: "git",
        commands: &["git"],
        version_args: &["--version"],
    },
    ToolProbe {
        name: "cargo",
        commands: &["cargo"],
        version_args: &["--version"],
    },
    ToolProbe {
        name: "rustc",
        commands: &["rustc"],
        version_args: &["--version"],
    },
    ToolProbe {
        name: "node",
        commands: &["node"],
        version_args: &["--version"],
    },
    ToolProbe {
        name: "npm",
        commands: &["npm"],
        version_args: &["--version"],
    },
    ToolProbe {
        name: "python",
        commands: &["python3", "python"],
        version_args: &["--version"],
    },
    ToolProbe {
        name: "pip",
        commands: &["pip3", "pip"],
        version_args: &["--version"],
    },
    ToolProbe {
        name: "docker",
        commands: &["docker"],
        version_args: &["--version"],
    },
    ToolProbe {
        name: "go",
        commands: &["go"],
        version_args: &["version"],
    },
    ToolProbe {
        name: "java",
        commands: &["java"],
        version_args: &["-version"],
    },
    ToolProbe {
        name: "gh",
        commands: &["gh"],
        version_args: &["--version"],
    },
    ToolProbe {
        name: "make",
        commands: &["make"],
        version_args: &["--version"],
    },
    ToolProbe {
        name: "cmake",
        commands: &["cmake"],
        version_args: &["--version"],
    },
    ToolProbe {
        name: "curl",
        commands: &["curl"],
        version_args: &["--version"],
    },
    ToolProbe {
        name: "ssh",
        commands: &["ssh"],
        version_args: &["-V"],
    },
];

/// Max age before re-detecting (1 hour).
const STALE_AFTER_SECS: u64 = 3600;

impl ToolStatusCache {
    /// Create an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            file_path: None,
        }
    }

    /// Load from a JSON file, or create empty if not found.
    #[must_use]
    pub fn load_or_create(path: &Path) -> Self {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(mut cache) = serde_json::from_str::<Self>(&content) {
                cache.file_path = Some(path.to_path_buf());
                return cache;
            }
        }
        Self {
            tools: HashMap::new(),
            file_path: Some(path.to_path_buf()),
        }
    }

    /// Detect all known tools, using cached results if fresh.
    pub fn detect_all(&mut self) {
        let now = unix_now();

        for probe in TOOLS_TO_DETECT {
            // Skip if cached and fresh
            if let Some(existing) = self.tools.get(probe.name) {
                if now.saturating_sub(existing.last_checked) < STALE_AFTER_SECS {
                    continue;
                }
            }

            let status = detect_tool(probe);
            self.tools.insert(probe.name.to_string(), status);
        }

        self.save();
    }

    /// Check if a specific tool is installed.
    #[must_use]
    pub fn is_installed(&self, name: &str) -> bool {
        self.tools.get(name).is_some_and(|s| s.installed)
    }

    /// Get the status of a specific tool.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ToolStatus> {
        self.tools.get(name)
    }

    /// Get all detected tools.
    #[must_use]
    pub fn all(&self) -> &HashMap<String, ToolStatus> {
        &self.tools
    }

    /// Number of installed tools.
    #[must_use]
    pub fn installed_count(&self) -> usize {
        self.tools.values().filter(|s| s.installed).count()
    }

    /// Formatted status for /status display.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let mut installed: Vec<&str> = self
            .tools
            .iter()
            .filter(|(_, s)| s.installed)
            .map(|(name, _)| name.as_str())
            .collect();
        // Sorted, because the map behind this randomises its iteration order
        // per process: the same install printed its tools in a different order
        // on every run. Nothing depended on the order, but it made two runs
        // impossible to diff — which is how a change gets misread as a
        // regression, and nearly was.
        installed.sort_unstable();

        if installed.is_empty() {
            "No CLI tools detected".to_string()
        } else {
            format!("{} tools: {}", installed.len(), installed.join(", "))
        }
    }

    /// Save to disk (fire-and-forget).
    fn save(&self) {
        if let Some(ref path) = self.file_path {
            let _ = std::fs::write(path, serde_json::to_string_pretty(self).unwrap_or_default());
        }
    }
}

impl Default for ToolStatusCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Detect a single tool.
fn detect_tool(probe: &ToolProbe) -> ToolStatus {
    let now = unix_now();

    for cmd in probe.commands {
        // Try to find path with `which`/`where`
        let found_path = find_command_path(cmd);

        // Try to get version
        let output = Command::new(cmd)
            .args(probe.version_args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .ok()
            .and_then(|child| child.wait_with_output().ok());

        if let Some(out) = output {
            if out.status.success() || !out.stdout.is_empty() || !out.stderr.is_empty() {
                let raw = if !out.stdout.is_empty() {
                    String::from_utf8_lossy(&out.stdout).to_string()
                } else {
                    String::from_utf8_lossy(&out.stderr).to_string()
                };
                let version = extract_version_string(&raw);

                return ToolStatus {
                    installed: true,
                    path: found_path,
                    version: Some(version),
                    last_checked: now,
                };
            }
        }
    }

    ToolStatus {
        installed: false,
        path: None,
        version: None,
        last_checked: now,
    }
}

/// Find the path of a command using `which` (unix) or `where` (windows).
fn find_command_path(cmd: &str) -> Option<String> {
    #[cfg(windows)]
    let which_cmd = "where";
    #[cfg(not(windows))]
    let which_cmd = "which";

    Command::new(which_cmd)
        .arg(cmd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                // `where` on Windows returns multiple lines, take the first
                let first = path.lines().next().unwrap_or("").to_string();
                if first.is_empty() {
                    None
                } else {
                    Some(first)
                }
            } else {
                None
            }
        })
}

/// Extract a clean version string from raw version output.
fn extract_version_string(raw: &str) -> String {
    let first_line = raw.lines().next().unwrap_or("").trim();
    // Cap at 80 chars
    if first_line.len() > 80 {
        format!("{}...", crate::text::head_bytes(first_line, 80))
    } else {
        first_line.to_string()
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_cache_is_empty() {
        let cache = ToolStatusCache::new();
        assert_eq!(cache.installed_count(), 0);
        assert!(!cache.is_installed("git"));
    }

    #[test]
    fn detect_all_finds_something() {
        let mut cache = ToolStatusCache::new();
        cache.detect_all();
        // At least one tool should be found on any dev machine
        assert!(!cache.all().is_empty());
    }

    #[test]
    fn the_tool_list_is_in_a_stable_order() {
        // The map randomises iteration per process, so the same install listed
        // its tools differently on every run.
        let mut cache = ToolStatusCache::default();
        for name in ["rustc", "cargo", "git", "docker", "node"] {
            cache.tools.insert(
                name.to_string(),
                ToolStatus {
                    installed: true,
                    path: None,
                    version: None,
                    last_checked: 0,
                },
            );
        }
        let line = cache.summary_line();
        let listed: Vec<&str> = line
            .split(": ")
            .nth(1)
            .expect("a list")
            .split(", ")
            .collect();
        assert_eq!(
            listed,
            vec!["cargo", "docker", "git", "node", "rustc"],
            "got: {line}"
        );
    }

    #[test]
    fn summary_line_works() {
        let mut cache = ToolStatusCache::new();
        assert_eq!(cache.summary_line(), "No CLI tools detected");
        cache.tools.insert(
            "git".to_string(),
            ToolStatus {
                installed: true,
                path: Some("/usr/bin/git".into()),
                version: Some("git version 2.40".into()),
                last_checked: 0,
            },
        );
        let summary = cache.summary_line();
        assert!(summary.contains("git"));
        assert!(summary.contains("1 tools"));
    }

    #[test]
    fn load_or_create_handles_missing_file() {
        let cache = ToolStatusCache::load_or_create(Path::new("/nonexistent/path.json"));
        assert!(cache.tools.is_empty());
    }

    #[test]
    fn extract_version_string_takes_first_line() {
        assert_eq!(
            extract_version_string("git version 2.40\nother stuff"),
            "git version 2.40"
        );
        assert_eq!(extract_version_string("v18.0.0\n"), "v18.0.0");
    }

    #[test]
    fn persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool-status.json");

        let mut cache = ToolStatusCache::load_or_create(&path);
        cache.tools.insert(
            "cargo".to_string(),
            ToolStatus {
                installed: true,
                path: None,
                version: Some("cargo 1.77".into()),
                last_checked: 12345,
            },
        );
        cache.save();

        let loaded = ToolStatusCache::load_or_create(&path);
        assert!(loaded.is_installed("cargo"));
        assert_eq!(
            loaded.get("cargo").unwrap().version.as_deref(),
            Some("cargo 1.77")
        );
    }
}
