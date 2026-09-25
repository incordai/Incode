//! What the harness does to a workspace itself.
//!
//! These used to be TOOLS — things offered to a model, dispatched by this
//! crate's own loop and gated by a permission tier. That loop is gone: the
//! delegated agent brings its own tools and its own permission layer.
//!
//! So these are no longer offered to anything. They are ordinary functions the
//! harness calls for its own work: running the check that settles whether a
//! task is done, snapshotting a tree so a turn can be undone, reading the git
//! context that rides a prompt, and reporting what is installed.

pub mod bash;
pub mod bash_jobs;
pub mod checkpoint;
pub mod diagnostics;
pub mod diff;
pub mod documents;
pub mod env_filter;
pub mod file_ops;
pub mod git_context;
pub mod search;
pub mod tool_status;
pub mod worktree;
