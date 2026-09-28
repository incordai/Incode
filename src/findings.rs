//! Work that produces INFORMATION, and the artifact that proves it happened.
//!
//! # The problem this exists to solve
//!
//! [`crate::roles::changes_code`] records a real failure: a live run marked
//! `explore: locate the ball entity` as `Done` because the project's check,
//! `node --check game.js`, passed — for reasons that had nothing to do with the
//! step. The fix at the time was to stop making such steps tasks at all:
//! [`crate::ledger::Ledger::from_plan`] folded them into the prose of the
//! change they served.
//!
//! That was honest, and it cost the thing the roles were built for. A folded
//! step is not a task, so it is never routed: the whole point of
//! [`crate::roles`] is that finding where something lives should run on a
//! cheap model and deciding whether a design is sound should not, and a
//! research step that only exists as a sentence at the top of an `implement`
//! task runs on whatever model got the implement task. The routing table was
//! mostly decoration.
//!
//! # What changed
//!
//! A research step becomes a task again, but it is no longer judged by the
//! project's build. It is judged by whether it produced its DELIVERABLE: a
//! findings file, at a path fixed before the agent ran, holding more than a
//! shrug. The check is a command that exits zero, exactly like every other
//! check — so the completion law is untouched.
//!
//! # What this does NOT prove, stated plainly
//!
//! That the findings are CORRECT. Nothing here reads them for sense, and a
//! confident, wrong answer of sufficient length passes. This check proves one
//! thing: the step produced the artifact it was asked for. That is strictly
//! more than the fold proved — which was nothing — and strictly less than the
//! build proves about code. What actually catches wrong findings is the task
//! that depends on them: an `implement` task built on a bad diagnosis fails
//! its own check, which is a real command over real code.

use std::path::{Path, PathBuf};

/// Where findings live, relative to the workspace.
pub const DIR: &str = ".incode/findings";

/// The smallest answer this will accept, in non-whitespace characters.
///
/// # Why a length at all
///
/// The failure being guarded is an agent that writes "done" — or an empty
/// file — and moves on. A length is a crude test and it is the only one
/// available without reading the content for sense, which would mean a model
/// judging a model and is exactly what [`crate::verify`] exists to refuse.
///
/// 200 is chosen as "shorter than any real answer to any of these roles". A
/// paragraph naming a file and a line number clears it comfortably; a single
/// sentence of assurance does not.
pub const MIN_CHARS: usize = 200;

/// The file a research task must write.
#[must_use]
pub fn path_for(task_id: &str) -> String {
    // Forward slashes deliberately, on every platform: this string is stored
    // in the ledger and printed into a prompt, and Windows accepts it in both
    // places. A separator that changed with the machine would make the same
    // plan produce two different ledgers.
    format!("{DIR}/{}.md", safe_name(task_id))
}

/// A task id reduced to something safe to use as a filename.
///
/// Ids are generated (`r-3.2`), not typed, so this is a guard rather than a
/// sanitiser: anything that is not a letter, digit, dot, dash or underscore
/// becomes a dash rather than reaching the filesystem.
fn safe_name(id: &str) -> String {
    let mut name: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // A dot is kept because ids really contain them (`r-3.2`), and two dots
    // in a row are the one sequence that walks out of the directory. Collapsed
    // rather than rejected: this is a guard on generated input, and failing a
    // plan over a task id would be a worse outcome than a slightly odd
    // filename.
    while name.contains("..") {
        name = name.replace("..", ".");
    }
    name
}

/// Resolve a findings path against a workspace.
#[must_use]
pub fn resolve(workspace: &Path, path: &str) -> PathBuf {
    let given = Path::new(path);
    if given.is_absolute() {
        given.to_path_buf()
    } else {
        workspace.join(given)
    }
}

/// Decide whether a findings file counts as delivered.
///
/// `Ok` carries what to print as evidence; `Err` carries what the agent got
/// wrong, in words it can act on. Nothing here is a warning: this is the
/// command whose exit code decides a task, so an ambiguous outcome is a
/// failure.
pub fn check(workspace: &Path, path: &str) -> Result<String, String> {
    let full = resolve(workspace, path);
    let text = std::fs::read_to_string(&full).map_err(|e| {
        format!(
            "no findings at {}: {e}. This task's deliverable is that file — \
             write what you found there.",
            full.display()
        )
    })?;

    let solid = text.chars().filter(|c| !c.is_whitespace()).count();
    if solid < MIN_CHARS {
        return Err(format!(
            "{} holds {solid} characters of content; this task needs at least \
             {MIN_CHARS}. Write what you actually found — the files, the line \
             numbers, and what they mean — not a summary of having looked.",
            full.display()
        ));
    }

    Ok(format!(
        "{} — {solid} characters, {} lines",
        full.display(),
        text.lines().count()
    ))
}

// There is deliberately no `read` here. The task that waited on findings is
// given the PATH, in its own instructions, and reads the file with the tools
// it already has. Loading the text into the prompt instead would put every
// research answer into every prompt after it, which is the context cost the
// roles were introduced to avoid.

/// Make sure the findings directory exists before an agent is asked to write
/// into it.
///
/// Best effort. An agent that can create a file can create its parent, and a
/// failure here shows up as a failed check with a message that names the path
/// — which is a better place to discover it than a run that refused to start.
pub fn prepare(workspace: &Path) {
    let _ = crate::folders::create_dir_all(&workspace.join(DIR));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        prepare(dir.path());
        dir
    }

    /// **A MISSING DELIVERABLE IS A FAILURE, NOT AN EMPTY RESULT.**
    #[test]
    fn findings_that_were_never_written_do_not_pass() {
        let dir = workspace();
        let err = check(dir.path(), &path_for("r-1.1")).expect_err("nothing was written");
        assert!(err.contains("no findings at"), "{err}");
    }

    /// **"DONE" IS NOT AN ANSWER.**
    ///
    /// The whole reason this check exists is the agent that reports having
    /// looked instead of reporting what it saw. A file-exists test would pass
    /// that, which would be the old bug with a new name.
    #[test]
    fn a_one_word_file_does_not_count_as_findings() {
        let dir = workspace();
        let path = path_for("r-1.1");
        std::fs::write(resolve(dir.path(), &path), "Done.\n").expect("write");

        let err = check(dir.path(), &path).expect_err("six characters is not findings");
        assert!(err.contains("at least"), "{err}");
    }

    /// Whitespace is not content, however much of it there is.
    #[test]
    fn padding_a_short_answer_with_blank_lines_does_not_help() {
        let dir = workspace();
        let path = path_for("r-1.1");
        let padded = format!("Done.{}", "\n".repeat(MIN_CHARS * 2));
        std::fs::write(resolve(dir.path(), &path), padded).expect("write");

        assert!(
            check(dir.path(), &path).is_err(),
            "blank lines were counted as an answer"
        );
    }

    /// A real answer passes, and the evidence names the file.
    #[test]
    fn a_written_answer_passes_and_says_where_it_is() {
        let dir = workspace();
        let path = path_for("r-1.1");
        std::fs::write(resolve(dir.path(), &path), "x".repeat(MIN_CHARS + 1)).expect("write");

        let ok = check(dir.path(), &path).expect("a real answer");
        assert!(ok.contains("r-1.1.md"), "{ok}");
    }

    /// An id with a separator in it must not escape the findings directory.
    #[test]
    fn a_path_cannot_be_smuggled_through_a_task_id() {
        let path = path_for("../../etc/passwd");
        assert!(!path.contains(".."), "{path}");
        assert!(path.starts_with(DIR), "{path}");
    }
}
