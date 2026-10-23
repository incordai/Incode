//! What the session can use: skills, incode's tools and the external ones,
//! background jobs, and the environment subprocesses see.

use crate::cli::render;
use crate::outln;
use crate::tools::tool_status::ToolStatusCache;

/// The skills this session actually loaded, from the store every agent shares.
///
/// # Why this exists beside the feature-gated one below
///
/// The gated handler reads an external crate that is not present on this
/// machine, so the branch that actually ships was a message saying skills were
/// "not compiled in". That was false in the way that matters: [`crate::skills`]
/// loads them from the store and puts the index into EVERY prompt
/// (`cli/mod.rs`), so the command denied something the session was already
/// doing — and a command that contradicts the harness teaches the operator to
/// distrust the rest of them.
///
/// Bodies are not printed, for the same reason they are not put in the prompt:
/// the index is what decides whether a body is wanted.
pub fn handle_skills_from_store(root: &std::path::Path) {
    render::print_header("Skills");
    let (set, source) = crate::skills::load(root);
    for line in skills_lines(&set, source) {
        outln!("{line}");
    }
    outln!();
}

/// What `/skills` prints, as data.
///
/// Returned rather than printed for the same reason
/// [`crate::session::summary_lines`] is: the command's whole job is *what it
/// says*, and a handler that writes straight to the terminal can only be
/// checked by a person looking at a screen. That is exactly how this command
/// came to deny that skills existed while the same session was loading them —
/// nothing executed the sentence, so nothing contradicted it.
#[must_use]
pub fn skills_lines(set: &crate::skills::Skillset, source: crate::skills::Source) -> Vec<String> {
    let mut out = Vec::new();

    // SAY WHERE IT CAME FROM. A cached list that reads exactly like a live one
    // turns an unreachable store into a silent wrong answer.
    match source {
        crate::skills::Source::Store => {}
        crate::skills::Source::Cache(age) => out.push(format!(
            "  The skill store did not answer — this is what it last said, {age}s ago."
        )),
        crate::skills::Source::Nothing if set.is_empty() => {
            out.push(
                "  The skill store did not answer and nothing was kept. This session is \
                 running without skills."
                    .to_string(),
            );
            return out;
        }
        crate::skills::Source::Nothing => {
            out.push("  The skill store did not answer; only plugin skills are listed.".to_string())
        }
    }

    if set.is_empty() {
        out.push("  The store holds no skills for this machine.".to_string());
        return out;
    }

    if !set.managed.is_empty() {
        out.push("  The operator's own, with their text:".to_string());
        for skill in &set.managed {
            out.push(format!("    {}  —  {}", skill.name, skill.description));
        }
    }
    if !set.discovered.is_empty() {
        if !set.managed.is_empty() {
            out.push(String::new());
        }
        out.push("  Installed folders, read on request:".to_string());
        for skill in &set.discovered {
            out.push(format!("    {}  —  {}", skill.name, skill.description));
        }
    }
    out.push(String::new());
    out.push(format!(
        "  {} skill(s): {} written by you, {} installed. The index rides every prompt;",
        set.managed.len() + set.discovered.len(),
        set.managed.len(),
        set.discovered.len()
    ));
    out.push("  a body is read only when the agent asks for that skill by name.".to_string());
    out
}

/// Where the tools live now.
///
/// The agent runs its own loop with its own tools, so listing this harness's
/// would name things nothing calls. What the harness still does is said plainly
/// instead of implied by an empty list.
/// `/tools` and its verbs for the external tools configured in `admin.json`
/// under `data_dir`: what is installed, or install, update or remove one.
pub fn handle_ext_tools(arg: Option<&str>, data_dir: &std::path::Path) {
    render::print_header("External tools");
    for line in crate::ext_tools::command(arg.unwrap_or_default(), data_dir) {
        outln!("  {line}");
    }
}

/// `/tools`: the tools incode runs itself for this agent, or — for an agent
/// whose CLI still uses its own tools — that plainly.
pub fn handle_delegated_tools(agent: &str, offered: &[String]) {
    render::print_header("Tools");
    if offered.is_empty() {
        outln!("  {agent} still runs its own tools on this route; incode checks the");
        outln!("  result afterwards (protected paths, the check, the ledger).");
    } else {
        outln!("  incode runs every tool {agent} calls, through its own gate");
        outln!("  (schema, guards, the memory engine, the permission mode):");
        for chunk in offered.chunks(6) {
            outln!("    {}", chunk.join(", "));
        }
    }
    outln!();
    outln!("  Installed on this machine:");
}

// ---------------------------------------------------------------------------
// Background job handlers
// ---------------------------------------------------------------------------

pub fn handle_jobs(job_mgr: &mut crate::tools::bash_jobs::BashJobManager) {
    render::print_header("Background Jobs");
    let jobs = job_mgr.list();
    if jobs.is_empty() {
        render::print_info("No background jobs. Use /bg <command> to start one.");
    } else {
        for (id, cmd, running, elapsed) in &jobs {
            let status = if *running { "running" } else { "done" };
            let cmd_preview = if cmd.len() > 60 {
                format!("{}...", crate::text::head_bytes(cmd, 60))
            } else {
                cmd.to_string()
            };
            outln!("  #{id:<4} [{status}] {elapsed}s  {cmd_preview}");
        }
        outln!();
        outln!(
            "  {} job(s), {} running",
            jobs.len(),
            job_mgr.running_count()
        );
    }
    outln!();
}

// ---------------------------------------------------------------------------
// Env filter handler
// ---------------------------------------------------------------------------

pub fn handle_env() {
    render::print_header("Environment Filter");
    let filtered = crate::tools::env_filter::filtered_env();
    let mut keys: Vec<&String> = filtered.keys().collect();
    keys.sort();
    for key in &keys {
        let value = &filtered[*key];
        let display_value = if value.len() > 60 {
            format!("{}...", crate::text::head_bytes(value, 60))
        } else {
            value.clone()
        };
        outln!("  {key}={display_value}");
    }
    outln!();
    render::print_info(&format!(
        "{} env vars passed to subprocesses ({} blocked).",
        filtered.len(),
        std::env::vars().count().saturating_sub(filtered.len())
    ));
    outln!();
}

// ---------------------------------------------------------------------------
// Tool status handler
// ---------------------------------------------------------------------------

pub fn handle_tool_status(cache: &ToolStatusCache) {
    render::print_header("CLI Tool Status");
    let all = cache.all();
    let mut entries: Vec<(&String, &crate::tools::tool_status::ToolStatus)> = all.iter().collect();
    entries.sort_by_key(|(name, _)| name.to_string());
    for (name, status) in &entries {
        let icon = if status.installed { "+" } else { "-" };
        let version = status.version.as_deref().unwrap_or("n/a");
        outln!("  [{icon}] {name:<12} {version}");
    }
    outln!();
    outln!(
        "  {} installed, {} not found",
        cache.installed_count(),
        entries.len() - cache.installed_count()
    );
    outln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str) -> crate::skills::Skill {
        crate::skills::Skill {
            name: name.to_string(),
            description: format!("what {name} is for"),
            body: String::new(),
            path: String::new(),
        }
    }

    /// **The command must not deny what the session is doing.** `/skills` used
    /// to print "Skills are not compiled in. Rebuild with `--features
    /// agent-tools`" on the branch that actually ships, while
    /// `skills::session_context` was putting these very skills into every
    /// prompt. Nothing executed that sentence, so nothing contradicted it.
    #[test]
    fn the_skills_that_were_loaded_are_the_skills_that_are_listed() {
        let set = crate::skills::Skillset {
            managed: vec![skill("Test Driven Developer")],
            discovered: vec![skill("Ponytail"), skill("Reviewer Council")],
        };
        let lines = skills_lines(&set, crate::skills::Source::Store);
        let all = lines.join("\n");

        for name in ["Test Driven Developer", "Ponytail", "Reviewer Council"] {
            assert!(all.contains(name), "`{name}` must be listed; got:\n{all}");
        }
        assert!(
            !all.contains("not compiled in") && !all.contains("agent-tools"),
            "it must never deny skills it was just handed; got:\n{all}"
        );
        assert!(
            all.contains("3 skill(s): 1 written by you, 2 installed"),
            "the counts must be the real ones; got:\n{all}"
        );
    }

    /// **A cached list must not read like a live one.** An unreachable store
    /// that answers silently from cache is a wrong answer nobody can see.
    #[test]
    fn a_cached_answer_says_it_is_cached() {
        let set = crate::skills::Skillset {
            managed: vec![skill("Ponytail")],
            discovered: Vec::new(),
        };
        let all = skills_lines(&set, crate::skills::Source::Cache(90)).join("\n");
        assert!(all.contains("did not answer"), "got:\n{all}");
        assert!(
            all.contains("90s ago"),
            "the age must be stated; got:\n{all}"
        );
        assert!(all.contains("Ponytail"), "it still lists what it has");
    }

    /// Nothing at all is said plainly, and the list is not implied to be empty
    /// for some other reason.
    #[test]
    fn an_unreachable_store_with_no_cache_says_so() {
        let all = skills_lines(
            &crate::skills::Skillset::default(),
            crate::skills::Source::Nothing,
        )
        .join("\n");
        assert!(all.contains("running without skills"), "got:\n{all}");
    }

    /// A store that answered, with nothing in it, is a different fact from a
    /// store that did not answer.
    #[test]
    fn an_empty_store_is_not_reported_as_a_failure() {
        let all = skills_lines(
            &crate::skills::Skillset::default(),
            crate::skills::Source::Store,
        )
        .join("\n");
        assert!(all.contains("holds no skills"), "got:\n{all}");
        assert!(!all.contains("did not answer"), "got:\n{all}");
    }
}
