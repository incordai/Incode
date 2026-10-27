//! Keeping a prompt and its context within what an OS command line can
//! carry, for the agent CLIs that are handed both as arguments.
//!
//! Wired to: the CLI backends that pass the prompt on a command line —
//! `agy.rs` and `codex/mod.rs` (`full_prompt`) and `claude_code/turn.rs`
//! (`run_once`) — through [`budget_for_spawn`].

use super::AgentError;

/// Maximum length of payload (prompt + system context) passed to external CLI
/// processes via command-line flags.
///
/// On Windows, Win32 `CreateProcessW` imposes a hard 32,767 character limit on
/// the entire command line (`lpCommandLine`). Exceeding this triggers OS error 206
/// (`ERROR_FILENAME_EXCED_RANGE: The filename or extension is too long`).
/// Keeping the combined payload at or below 28,000 characters leaves ample room
/// (~4,700 chars) for binary paths, flags, arguments, quotes, and whitespace.
#[cfg(windows)]
pub const MAX_CMDLINE_PAYLOAD_LEN: usize = 28_000;
#[cfg(not(windows))]
pub const MAX_CMDLINE_PAYLOAD_LEN: usize = 120_000;

/// Prompt budget ceiling when total payload exceeds [`MAX_CMDLINE_PAYLOAD_LEN`].
pub const MAX_PROMPT_BUDGET: usize = 12_000;

/// Minimum context budget preserved when budgeting payload.
pub const MIN_CONTEXT_BUDGET: usize = 4_000;

/// What a block cut to fit ends with.
const CUT_MARK: &str = "\n\n[...truncated for OS command line limit...]";

/// A high-priority block is cut to fit only when more than this is left;
/// otherwise it is dropped.
const HIGH_PRIORITY_CUT_FLOOR: usize = 100;

/// The same floor for a low-priority block.
const LOW_PRIORITY_CUT_FLOOR: usize = 120;

/// Compacts / budgets prompt and system context so external agent CLI processes
/// spawned via `Command::new` stay safely within OS command-line limits.
///
/// When the total payload exceeds [`MAX_CMDLINE_PAYLOAD_LEN`], a prompt over
/// [`MAX_PROMPT_BUDGET`] is written whole to a file in `workspace` and the
/// agent is told to read it ([`spill`]); nothing of it is dropped. Then
/// high-priority context blocks (guards, standing rules, intake, asking,
/// roles) are preserved, while large informational blocks (discovered skills
/// catalog, auditor findings, recorder handoff) are truncated to fit the
/// remaining budget. An error only when the spill file cannot be written:
/// the turn is not started on a prompt the agent would never see whole.
///
/// Called by the CLI backends that pass the prompt on a command line:
/// `agy.rs` and `codex/mod.rs` (`full_prompt`) and `claude_code/turn.rs`
/// (`run_once`).
pub fn budget_for_spawn(
    prompt: &str,
    system_context: &[String],
    workspace: &std::path::Path,
) -> Result<(String, String), AgentError> {
    let unbudgeted_context = system_context.join("\n\n");
    let total_len = prompt.len() + unbudgeted_context.len();
    if total_len <= MAX_CMDLINE_PAYLOAD_LEN {
        return Ok((prompt.to_string(), unbudgeted_context));
    }

    // NO PART OF THE PROMPT IS DROPPED. It used to keep the first 2,000 and
    // the last ~9,900 bytes and cut the middle, on the theory that the
    // instruction is always short and last. Measured on Insi's lesson writer:
    // its instruction comes FIRST and its prompt runs to 20 KB, so the model
    // got the preamble's head and the tail of a diff, and answered by asking
    // what it was meant to do — four lessons of fifteen lost.
    let budgeted_prompt = if prompt.len() > MAX_PROMPT_BUDGET {
        spill(prompt, workspace)?
    } else {
        prompt.to_string()
    };

    let context_budget = MAX_CMDLINE_PAYLOAD_LEN
        .saturating_sub(budgeted_prompt.len())
        .max(MIN_CONTEXT_BUDGET);

    if unbudgeted_context.len() <= context_budget {
        return Ok((budgeted_prompt, unbudgeted_context));
    }

    let (high_priority, low_priority) = by_priority(system_context);
    let mut kept = Kept::default();
    kept.take(&high_priority, context_budget, HIGH_PRIORITY_CUT_FLOOR);
    kept.take(&low_priority, context_budget, LOW_PRIORITY_CUT_FLOOR);
    Ok((budgeted_prompt, kept.blocks.join("\n\n")))
}

/// Whether `prompt` and `system_context` together fit on one command line.
///
/// A backend that can take the prompt on stdin asks this first, and sends a
/// prompt that does not fit THERE rather than spilling it to a file the model
/// must be told to read (`spill`) — measured: with the memory engine's context
/// riding the turn, a two-line request arrived as a 16 KB file with the
/// person's words on its last line. Called by `claude_code/turn.rs`
/// `run_once`.
#[must_use]
pub fn fits_command_line(prompt: &str, system_context: &[String]) -> bool {
    prompt.len() + system_context.join("\n\n").len() <= MAX_CMDLINE_PAYLOAD_LEN
}

/// The system context alone, kept within one command line — for a turn whose
/// prompt goes on stdin, so the whole command line is the context's. Blocks
/// are kept by the same priority [`budget_for_spawn`] uses.
#[must_use]
pub fn budget_context(system_context: &[String]) -> String {
    let joined = system_context.join("\n\n");
    if joined.len() <= MAX_CMDLINE_PAYLOAD_LEN {
        return joined;
    }
    let (high_priority, low_priority) = by_priority(system_context);
    let mut kept = Kept::default();
    kept.take(
        &high_priority,
        MAX_CMDLINE_PAYLOAD_LEN,
        HIGH_PRIORITY_CUT_FLOOR,
    );
    kept.take(
        &low_priority,
        MAX_CMDLINE_PAYLOAD_LEN,
        LOW_PRIORITY_CUT_FLOOR,
    );
    kept.blocks.join("\n\n")
}

/// The context's non-blank blocks, trimmed, split by priority in their
/// order:
/// high-priority: guards, standing rules, intake task, question prompt, roles;
/// low-priority: discovered skills index, auditor findings, recorder handoff.
///
/// Called by [`budget_for_spawn`].
fn by_priority(system_context: &[String]) -> (Vec<&str>, Vec<&str>) {
    let mut high_priority = Vec::new();
    let mut low_priority = Vec::new();
    for block in system_context {
        let trimmed = block.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_high_priority(trimmed) {
            high_priority.push(trimmed);
        } else {
            low_priority.push(trimmed);
        }
    }
    (high_priority, low_priority)
}

/// Whether a block is one the agent must keep: its guards, the owner's
/// rules and instructions, the task's intake, how to ask, the roles, the
/// output style and the tools it may not use.
///
/// Called by [`by_priority`].
fn is_high_priority(block: &str) -> bool {
    let lower = block.to_ascii_lowercase();
    lower.contains("<guards>")
        || lower.contains("standing rules")
        || lower.contains("<intake>")
        || lower.contains("<asking>")
        || lower.contains("<roles>")
        || lower.contains("<project-instructions")
        || lower.contains(crate::styles::TAG)
        || lower.contains("disallowedtools")
}

/// The blocks kept so far, and the bytes they take joined.
#[derive(Default)]
struct Kept {
    blocks: Vec<String>,
    /// Each block's length plus its separator.
    used: usize,
}

impl Kept {
    /// Keep `blocks` in order while each fits in `budget`; the first that
    /// does not is cut to what is left — when more than `floor` is — and
    /// nothing after it in `blocks` is kept.
    ///
    /// Called by [`budget_for_spawn`], for the high-priority blocks and then
    /// the low.
    fn take(&mut self, blocks: &[&str], budget: usize, floor: usize) {
        for block in blocks {
            let block_len = block.len() + 2;
            if self.used + block_len <= budget {
                self.blocks.push((*block).to_string());
                self.used += block_len;
                continue;
            }
            let remaining = budget.saturating_sub(self.used);
            if remaining > floor {
                let cutoff = crate::text::head_bytes(block, remaining.saturating_sub(80));
                let truncated = format!("{cutoff}{CUT_MARK}");
                self.used += truncated.len() + 2;
                self.blocks.push(truncated);
            }
            break;
        }
    }
}

/// Write the whole of a prompt too long for a command line to the context
/// folder in `workspace` — the folder incode's memory hooks spill their
/// blocks to and prune (`crate::hooks::memory::CONTEXT`) — and return what the
/// agent is told instead: where the file is, and to read all of it first.
/// The name is the prompt's hash, so the same prompt is one file.
fn spill(prompt: &str, workspace: &std::path::Path) -> Result<String, AgentError> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    prompt.hash(&mut hasher);
    let name = format!("task-{:016x}.md", hasher.finish());
    let dir = workspace.join(crate::hooks::memory::CONTEXT);
    let path = dir.join(&name);
    std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&path, prompt))
        .map_err(|e| {
            AgentError::new(format!(
                "the task is {} bytes, more than a command line can carry, and could not \
                 be written to {} for the agent to read: {e}; the turn was not started",
                prompt.len(),
                path.display()
            ))
        })?;
    Ok(format!(
        "This task is {} bytes, more than a command line can carry, so the whole of it \
         is in the file `{}/{name}` in your working folder ({}). Read that file in \
         full, from its first line to its last, before doing anything else: it is the \
         task, and what it asks is what you do.",
        prompt.len(),
        crate::hooks::memory::CONTEXT,
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_for_spawn_preserves_under_limit() {
        let prompt = "Implement feature X";
        let context = vec![
            "<guards>no-write tests</guards>".to_string(),
            "standing rules: follow style guide".to_string(),
        ];
        let dir = tempfile::tempdir().expect("dir");
        let (p, c) = budget_for_spawn(prompt, &context, dir.path()).expect("budgeted");
        assert_eq!(p, prompt);
        assert_eq!(c, context.join("\n\n"));
        assert!(
            !dir.path().join(crate::hooks::memory::CONTEXT).exists(),
            "a prompt that fits writes no file"
        );
    }

    #[test]
    fn budget_for_spawn_bounds_large_context_preserving_guards() {
        let prompt = "Fix bug";
        let large_skills = "Skills on this machine:\n".to_string() + &"x".repeat(35_000);
        let context = vec![
            "<guards>protected</guards>".to_string(),
            "The owner's standing rules:\n1. do not delete".to_string(),
            large_skills,
        ];
        let dir = tempfile::tempdir().expect("dir");
        let (p, c) = budget_for_spawn(prompt, &context, dir.path()).expect("budgeted");
        assert_eq!(p, prompt);
        assert!(p.len() + c.len() <= MAX_CMDLINE_PAYLOAD_LEN);
        assert!(c.contains("<guards>protected</guards>"));
        assert!(c.contains("standing rules"));
    }

    /// The file an over-budget prompt was written to, read from what the
    /// agent was told.
    fn spilled(told: &str, workspace: &std::path::Path) -> String {
        let at = told.find("task-").expect("names a task file");
        let name = &told[at..at + "task-".len() + 16 + ".md".len()];
        std::fs::read_to_string(workspace.join(crate::hooks::memory::CONTEXT).join(name))
            .expect("the task file exists")
    }

    /// **AN OVER-BUDGET PROMPT REACHES THE AGENT WHOLE.** Both shapes a
    /// harness sends: the instruction LAST (a task after its context) and
    /// the instruction FIRST, then a long body (Insi's lesson writer, whose
    /// instruction the old middle cut dropped). Each is written to a file in
    /// full, the agent is told where it is and to read all of it, and the
    /// command line stays within the OS limit.
    #[test]
    fn budget_for_spawn_spills_an_oversized_prompt_whole() {
        let body = "context é ".repeat(MAX_CMDLINE_PAYLOAD_LEN / 10);
        let last = format!("<guards>g</guards>\n\n{body}\n\nNow do this:\n\ncreate a.txt with hi");
        let first =
            format!("Write one lesson as JSON with an issue field.\n\n{body}\nVERIFIED_FIX>>>");
        let context = vec!["<guards>safe</guards>".to_string(), "x".repeat(20_000)];
        for prompt in [last, first] {
            let dir = tempfile::tempdir().expect("dir");
            let (p, c) = budget_for_spawn(&prompt, &context, dir.path()).expect("budgeted");
            assert!(
                p.len() + c.len() <= MAX_CMDLINE_PAYLOAD_LEN,
                "{}",
                p.len() + c.len()
            );
            assert!(
                p.len() < 1_000,
                "the agent is told where, not sent a cut: {p}"
            );
            assert!(p.contains("Read that file in full"), "{p}");
            assert_eq!(
                spilled(&p, dir.path()),
                prompt,
                "nothing of the prompt is dropped"
            );
            assert!(c.contains("<guards>safe</guards>"));
        }
    }

    /// **A PROMPT THAT CANNOT BE WRITTEN DOWN IS NOT SENT AT ALL.** The
    /// workspace is a file, so the context folder cannot be made: the turn
    /// stops with the reason instead of starting on a prompt the agent would
    /// never see whole.
    #[test]
    fn budget_for_spawn_refuses_when_the_task_file_cannot_be_written() {
        let dir = tempfile::tempdir().expect("dir");
        let not_a_folder = dir.path().join("plain-file");
        std::fs::write(&not_a_folder, "x").expect("file");
        let prompt = "y".repeat(MAX_CMDLINE_PAYLOAD_LEN + 1);
        let why = budget_for_spawn(&prompt, &[], &not_a_folder).expect_err("refused");
        let why = why.to_string();
        assert!(
            why.contains("could not be written") && why.contains("not started"),
            "{why}"
        );
    }
}
