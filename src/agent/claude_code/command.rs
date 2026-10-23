//! The command one turn of the CLI is started with: the flags that make it a
//! model and nothing else, the one MCP server it is handed, and the stdin
//! message that carries a turn's pictures.

use std::process::Command;

use super::profiles::{usable_profile, PROFILE_ENV};
use super::ClaudeCodeBackend;

/// The flag that asks the CLI for a readable summary of the model's
/// thinking. Undocumented; see [`rejects_thinking_display`].
pub(super) const THINKING_DISPLAY_FLAG: &str = "--thinking-display";

/// Whether a failed start is the CLI refusing [`THINKING_DISPLAY_FLAG`]:
/// its option parser names the flag with "unknown option" (or a close
/// spelling) and exits before any output.
pub(super) fn rejects_thinking_display(failure: &str) -> bool {
    let said = failure.to_ascii_lowercase();
    said.contains("thinking-display")
        && ["unknown option", "unknown argument", "unrecognized option"]
            .iter()
            .any(|phrase| said.contains(phrase))
}

impl ClaudeCodeBackend {
    /// The CLI's command for one turn, as a model and nothing else, with
    /// `servers` as its only MCP servers and `allowed` the tool names it may
    /// send without asking. The bridge's address and token are added by the
    /// caller, which holds the bridge.
    ///
    /// `prompt` is `None` for a turn carrying pictures, whose prompt goes on
    /// stdin instead, as one stream-json user message ([`user_message`]): `-p`
    /// with no prompt and `--input-format stream-json`. Measured on 2.1.283: a
    /// message with a base64 PNG block was described correctly, and the CLI
    /// ended when stdin closed.
    pub(super) fn command(
        &self,
        prompt: Option<&str>,
        context: &str,
        servers: &str,
        allowed: &[String],
    ) -> Command {
        let mut cmd = Command::new(&self.binary);
        crate::agent::parent_session::detach_from_parent_session(&mut cmd);
        cmd.arg("-p");
        match prompt {
            Some(prompt) => {
                cmd.arg(prompt);
            }
            None => {
                cmd.arg("--input-format").arg("stream-json");
            }
        }
        cmd.arg("--output-format")
            .arg("stream-json")
            .args(MODEL_ONLY_FLAGS)
            .arg("--mcp-config")
            .arg(servers)
            .arg("--allowedTools")
            .args(allowed);
        for (key, value) in MODEL_ONLY_ENV {
            cmd.env(key, value);
        }

        // Which model serves this turn. Absent, the CLI uses its own default,
        // which is the right behaviour for a caller with no opinion.
        if let Some(model) = &self.model {
            cmd.arg("--model").arg(model);
        }
        if self.partial_messages {
            cmd.arg("--include-partial-messages");
            // The model's reasoning as text a person can read, streamed as
            // `thinking_delta`s. Not in `--help` on 2.1.283 (it is in the
            // CLI's option table, choices summarized / omitted / highlights);
            // measured: without it every thinking delta arrived EMPTY, with it
            // the summary arrived word by word, beside an unchanged reply.
            // Left off once this CLI has refused it (`thinking_display`).
            if self.thinking_display {
                cmd.arg(THINKING_DISPLAY_FLAG).arg("summarized");
            }
        }
        if !context.is_empty() {
            cmd.arg("--append-system-prompt").arg(context);
        }
        if !self.fallbacks.is_empty() {
            // One comma-separated list, tried in order — the CLI's own shape for
            // the ordered chain the role table produces.
            cmd.arg("--fallback-model").arg(self.fallbacks.join(","));
        }

        // Continue the existing session rather than starting a new one. A fresh
        // session re-pays the entire prompt prefix; resuming does not.
        if let Some(sid) = &self.session_id {
            cmd.arg("--resume").arg(sid);
        }

        // Point the agent at a profile whose session is actually live.
        //
        // Skipped entirely when the operator has already said which profile to
        // use: an explicit choice is a decision, and silently overriding it
        // would be worse than the fallback this replaces.
        if std::env::var_os(PROFILE_ENV).is_none() {
            if let Some(profile) = usable_profile() {
                cmd.env(PROFILE_ENV, &profile.dir);
            }
        }

        // No team server identity here: on this route the team channel's
        // tools are incode's own (`toolbox::team`), and who they speak as is
        // set on the toolbox by `set_team_identity`.
        cmd
    }
}

/// One stream-json user message: the prompt as a text block, then each
/// picture as a base64 image block — the shape the CLI reads on stdin with
/// `--input-format stream-json`, one message per line.
pub(super) fn user_message(prompt: &str, images: &[crate::toolbox::Image]) -> String {
    let mut content = vec![serde_json::json!({"type": "text", "text": prompt})];
    content.extend(images.iter().map(crate::agent::api::image_block));
    let message = serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": content},
    });
    format!("{message}\n")
}

/// The flags that make the CLI a model and nothing else. Each was proved on
/// the installed CLI before it was relied on — see `docs/ARCHITECTURE.md`,
/// "Models only: incode runs every tool".
///
/// - `--tools ""`: none of its built-in tools. Its session then lists only the
///   MCP tools incode hands it.
/// - `--strict-mcp-config`: only the servers in `--mcp-config`, so nothing the
///   operator configured for the CLI is reachable.
/// - `--setting-sources ""`: no user, project or local settings — and with
///   them none of their hooks, permission rules or default mode. Measured:
///   thirteen hook events on a control run, none with this flag.
/// - `--disable-slash-commands`: no skills of its own.
///
/// `--bare` would do most of this in one flag, and cannot be used: it reads
/// only an API key, and a subscription login answers "Not logged in".
const MODEL_ONLY_FLAGS: [&str; 7] = [
    "--tools",
    "",
    "--strict-mcp-config",
    "--setting-sources",
    "",
    "--disable-slash-commands",
    "--verbose",
];

/// Environment that keeps the CLI's own memory out: no `CLAUDE.md` files and
/// no auto-memory. Measured: a project canary in `CLAUDE.md` was repeated by a
/// control run and unknown to a run with these set.
pub(super) const MODEL_ONLY_ENV: [(&str, &str); 2] = [
    ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
];

/// The one MCP server the CLI gets: the tool bridge — as an `--mcp-config`
/// document, and the tool names its calls may use without the CLI asking
/// anyone. The team channel and the desktop are toolbox tools reached
/// through it, so no server of their own is handed over.
///
/// `--strict-mcp-config` goes with it: the CLI loads ONLY what incode hands
/// it. Memory is reached through incode's own tools, not through a server
/// the operator happened to configure for the CLI.
pub(super) fn servers_for(exe: &std::path::Path) -> (String, Vec<String>) {
    let mut servers = serde_json::Map::new();
    servers.insert(
        crate::tool_bridge::CLIENT_KEY.to_string(),
        crate::tool_bridge::client_entry(exe),
    );
    // Allowed by server, not by tool: the CLI would otherwise refuse each call
    // for want of a person to ask. Whether a call may run is decided by incode
    // when it arrives, not by the CLI before it is sent.
    let allowed = servers.keys().map(|key| format!("mcp__{key}")).collect();
    let config = serde_json::json!({ "mcpServers": servers }).to_string();
    (config, allowed)
}
