//! The team channel's tools, run in the toolbox so the guards, the permission
//! mode and the memory engine's hooks cover them like every other tool.
//!
//! The channel logic is [`crate::team_mcp::Server`]'s, reused whole: the
//! same four verbs, the same read cursor, the same untrusted-data warning.
//! Only who it speaks as comes from here — the harness's identity for the
//! task, never the model's.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde_json::Value;

/// The name a server started without an identity speaks as — the same one
/// [`crate::team_mcp::Server::identity_from_env`] falls back to.
const UNROUTED: &str = "unrouted";

/// The team channel of one workspace, as one agent speaks on it.
pub(super) struct Team {
    channel: PathBuf,
    speaking_as: String,
    /// Opened on first use: opening reads the channel file, and most turns
    /// never touch it. Rebuilt when the identity changes, so the read cursor
    /// belongs to one speaker.
    server: Mutex<Option<(String, crate::team_mcp::Server)>>,
}

impl Team {
    pub(super) fn new(workspace: &Path) -> Self {
        Self {
            channel: crate::team::path_in(workspace),
            speaking_as: UNROUTED.to_string(),
            server: Mutex::new(None),
        }
    }

    /// Speak as `role` from now on.
    pub(super) fn speak_as(&mut self, role: &str) {
        let role = role.trim();
        self.speaking_as = if role.is_empty() {
            UNROUTED.to_string()
        } else {
            role.to_string()
        };
    }

    /// Run one of the channel's tools. `None` when `name` is not one.
    pub(super) fn handle(&self, name: &str, input: &Value) -> Option<Result<String, String>> {
        if !crate::team_mcp::TOOLS.contains(&name) {
            return None;
        }
        let mut slot = self.server.lock().unwrap_or_else(PoisonError::into_inner);
        if slot
            .as_ref()
            .is_none_or(|(who, _)| *who != self.speaking_as)
        {
            let channel = crate::team::Channel::at(self.channel.clone());
            *slot = Some((
                self.speaking_as.clone(),
                crate::team_mcp::Server::new(channel, self.speaking_as.clone()),
            ));
        }
        let (_, server) = slot.as_mut()?;
        Some(server.run_tool(name, input))
    }
}

/// The channel's tools, in the toolbox's spelling (`input_schema`).
pub(super) fn definitions() -> Vec<Value> {
    crate::team_mcp::tool_definitions()
        .into_iter()
        .map(|mut def| {
            if let Some(object) = def.as_object_mut() {
                if let Some(schema) = object.remove("inputSchema") {
                    object.insert("input_schema".to_string(), schema);
                }
            }
            def
        })
        .collect()
}
