//! Where the steps of a routine come from.
//!
//! A small trait, so the crate works with no memory engine at all.
//! [`super::watchman::WatchmanSource`] reads Watchman's records from the local
//! engine; [`MemorySource`] holds them in memory, which is what tests use and
//! what a caller with its own recorder can feed. A routine can also be saved
//! from an explicit step list, which needs no source.
//!
//! A source hands back [`Call`]s exactly as recorded, failures and observing
//! calls included. Deciding what to keep is [`super::learn`]'s job, so the rule
//! lives in one place whichever source fed it.

use std::collections::HashMap;

use serde_json::Value;

/// One recorded tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// The tool's name as it was recorded (for an MCP tool, with its
    /// `mcp__<server>__` prefix).
    pub tool: String,
    /// What it was called with.
    pub args: Value,
    /// Set when the call failed: Watchman records a tool result flagged
    /// `is_error` as the span's `error`.
    pub error: Option<String>,
    /// What it answered, as text, when Watchman kept it (it keeps at most 4 KiB,
    /// and nothing for an image).
    pub output: Option<String>,
}

impl Call {
    /// A call that succeeded, with no recorded output.
    #[must_use]
    pub fn ok(tool: &str, args: Value) -> Self {
        Self {
            tool: tool.to_string(),
            args,
            error: None,
            output: None,
        }
    }

    /// The same call, failed with `why`.
    #[must_use]
    pub fn failed(mut self, why: &str) -> Self {
        self.error = Some(why.to_string());
        self
    }

    /// The same call, having answered `text`.
    #[must_use]
    pub fn answering(mut self, text: &str) -> Self {
        self.output = Some(text.to_string());
        self
    }
}

/// One recorded run: the tool calls of one turn, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// The record's id (`w-<24 hex>` in Watchman).
    pub id: String,
    /// When it started, Unix milliseconds. A task spans several records, one
    /// per assistant turn; they are joined in this order.
    pub started_at_ms: i64,
    /// Its tool calls, in order.
    pub calls: Vec<Call>,
}

/// Where records come from. `Send` because the server that holds one is built
/// on one thread and may be handed to another.
pub trait StepSource: Send {
    /// The record `id`.
    ///
    /// # Errors
    ///
    /// There is no such record, or the source cannot be reached. The text says
    /// which, and is shown to the agent as it stands.
    fn record(&self, id: &str) -> Result<Record, String>;
}

/// Records held in memory.
#[derive(Debug, Default)]
pub struct MemorySource {
    records: HashMap<String, Record>,
}

impl MemorySource {
    /// An empty source.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a record, answered to by its id.
    #[must_use]
    pub fn with(mut self, record: Record) -> Self {
        self.records.insert(record.id.clone(), record);
        self
    }
}

impl StepSource for MemorySource {
    fn record(&self, id: &str) -> Result<Record, String> {
        self.records
            .get(id)
            .cloned()
            .ok_or_else(|| format!("no record {id:?}"))
    }
}

/// A source that is never there: the engine is not installed or not running.
/// Saving from an explicit step list still works without it.
#[derive(Debug, Default)]
pub struct NoSource;

impl StepSource for NoSource {
    fn record(&self, id: &str) -> Result<Record, String> {
        Err(format!(
            "cannot read record {id:?}: no memory engine is configured. Pass `steps` to \
             routine_save instead"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_memory_source_answers_for_the_records_it_holds_and_only_those() {
        let source = MemorySource::new().with(Record {
            id: "w-1".to_string(),
            started_at_ms: 5,
            calls: vec![Call::ok("key", json!({"keys": ["ctrl", "s"]}))],
        });
        assert_eq!(source.record("w-1").unwrap().calls.len(), 1);
        assert!(source.record("w-2").unwrap_err().contains("w-2"));
    }

    #[test]
    fn no_source_says_what_to_do_instead() {
        let why = NoSource.record("w-1").unwrap_err();
        assert!(why.contains("steps"), "{why}");
    }
}
