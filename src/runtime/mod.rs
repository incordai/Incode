//! What outlived the loop.
//!
//! This module used to hold `ConversationRuntime` — the harness's own agentic
//! loop, which sent messages to a provider, dispatched tools and streamed
//! tokens back. That loop is gone: the work is delegated to an installed agent
//! that runs its own loop with its own tools, on the operator's subscription.
//!
//! What remains is what the rest of the crate genuinely shares: the two error
//! types, the token counter that bounds a run, the cancel flag that stops one,
//! and the session lifecycle hooks.

use std::fmt::{Display, Formatter};

pub mod cancel;
pub mod http;
pub mod lifecycle;
pub mod usage;

/// Something went wrong while doing the work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    message: String,
}

impl RuntimeError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for RuntimeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RuntimeError {}

/// A tool could not do what it was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    message: String,
}

impl ToolError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for ToolError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ToolError {}
