//! The workspace and its repository: diff, worktree, pull request, links,
//! and the checks on what the agent may do here — doctor, guards and
//! permissions.

use crate::cli::render;
use crate::tools::worktree::WorktreeManager;
use crate::{out, outln};

/// Diff code or workspace against older versions.
pub fn handle_diff(arg: Option<&str>, workspace: &std::path::Path) {
    match arg {
        Some(target) => {
            let parts: Vec<&str> = target.split_whitespace().collect();
            if parts.len() >= 2 {
                let p1 = workspace.join(parts[0]);
                let p2 = workspace.join(parts[1]);
                if p1.is_file() && p2.is_file() {
                    match crate::tools::diff::diff_files(&p1, &p2) {
                        Ok(diff) if diff.is_empty() => render::print_info("No differences found."),
                        Ok(diff) => {
                            let (added, removed) = crate::tools::diff::diff_stats(&diff);
                            render::print_header(&format!(
                                "Diff: {} -> {} (+{} -{})",
                                parts[0], parts[1], added, removed
                            ));
                            outln!("{diff}");
                        }
                        Err(e) => render::print_error(&e),
                    }
                    return;
                }
            }
            let file = parts[0];
            let rev = parts.get(1).copied();
            match crate::tools::diff::git_diff_file(workspace, file, rev) {
                Ok(diff) if diff.is_empty() => {
                    render::print_info(&format!("No changes detected for {file}."));
                }
                Ok(diff) => {
                    let (added, removed) = crate::tools::diff::diff_stats(&diff);
                    render::print_header(&format!("Diff: {file} (+{} -{})", added, removed));
                    outln!("{diff}");
                }
                Err(e) => render::print_error(&e),
            }
        }
        None => match crate::tools::diff::git_diff_workspace(workspace, None) {
            Ok(diff) if diff.is_empty() => {
                render::print_info("Working tree clean — no diff against HEAD.");
            }
            Ok(diff) => {
                let (added, removed) = crate::tools::diff::diff_stats(&diff);
                render::print_header(&format!("Workspace Diff (+{} -{})", added, removed));
                outln!("{diff}");
            }
            Err(e) => render::print_error(&format!("Diff failed: {e}")),
        },
    }
}

/// Whether this session can actually do the work.
pub fn handle_doctor(workspace: &std::path::Path) {
    render::print_header("Doctor");
    let checks = crate::doctor::run(workspace);
    out!("{}", crate::doctor::report(&checks));
    match crate::doctor::worst(&checks) {
        crate::doctor::Health::Ok => render::print_info("Everything needed is in place."),
        crate::doctor::Health::Warn => {
            render::print_info("Workable, with the notes above.");
        }
        crate::doctor::Health::Fail => {
            render::print_error("Something above will stop work. Fix it before running.");
        }
    }
}

/// What the agent may not write.
///
/// A guard nobody can see is a guard nobody can check — and this one decides
/// whether the thing being judged can edit its own exam, so it is worth being
/// able to read without opening a JSON file.
pub fn handle_permissions(workspace: &std::path::Path) {
    render::print_header("Permission rules (deny > ask > allow > mode)");
    let rules = crate::permissions::rules::PermissionRules::load(workspace);
    let lines = rules.describe();
    if lines.is_empty() {
        outln!("  None. Add `permissions` to ~/.incode/admin.json or .incode/settings.json.");
    }
    for line in lines {
        outln!("  {line}");
    }
    for warning in rules.warnings() {
        render::print_error(warning);
    }
}

pub fn handle_guards(workspace: &std::path::Path) {
    render::print_header("Protected paths");
    match crate::guards::Guards::load(workspace) {
        Ok(guards) => {
            let paths = guards.effective();
            if paths.is_empty() {
                render::print_error(
                    "Nothing is protected — every default has been switched off in .incode/guards.json.",
                );
                return;
            }
            for path in &paths {
                outln!("  {path}");
            }
            outln!();
            outln!(
                "  {} pattern(s). The agent enforces these itself as deny rules, and a",
                paths.len()
            );
            outln!("  sentinel checks the files again after every delegated task.");
            if !guards.allow_writes_to.is_empty() {
                outln!();
                outln!("  Switched off by you:");
                for exempt in &guards.allow_writes_to {
                    outln!("    {exempt}");
                }
            }
        }
        Err(e) => render::print_error(&format!("the policy could not be read: {e}")),
    }
}

/// What else touches this file.
///
/// The question an agent should ask before changing a function and does not: a
/// backend edit that never reaches its API layer, its frontend and its tests
/// leaves three files written against something that no longer exists.
pub fn handle_links(roots: &[std::path::PathBuf], target: Option<&str>) {
    let map = crate::links::LinkMap::build(roots);

    let Some(target) = target else {
        render::print_header("Links");
        outln!("  {} source file(s) indexed.", map.len());
        if map.is_truncated() {
            render::print_error(
                "The index stopped early — this tree is larger than the walk limit, so what is shown is partial.",
            );
        }
        outln!("  Name a file to see what touches it, e.g. `/links src/thing.rs`.");
        return;
    };

    // Matched by suffix so a person can type what they would say, rather than
    // the absolute path the index happens to store.
    let wanted = target.replace(char::from(92), "/");
    let Some(file) = map.files_matching(&wanted).into_iter().next() else {
        render::print_error(&format!(
            "No indexed source file matches `{target}`. It may be outside the workspace, a type this index does not read, or generated."
        ));
        return;
    };

    render::print_header(&format!("Links — {file}"));

    let dependents = map.dependents(&file);
    if dependents.is_empty() {
        outln!("  Nothing indexed uses what this file defines.");
    } else {
        outln!("  Used by (change this file, check these):");
        for (dependent, symbol) in &dependents {
            outln!("    {dependent}  (uses `{symbol}`)");
        }
    }

    let dependencies = map.dependencies(&file);
    if !dependencies.is_empty() {
        outln!();
        outln!("  Depends on:");
        for (dependency, symbol) in dependencies.iter().take(20) {
            outln!("    {dependency}  (defines `{symbol}`)");
        }
        if dependencies.len() > 20 {
            outln!("    …and {} more", dependencies.len() - 20);
        }
    }

    outln!();
    outln!("  A text index, not a compiler: it can link files that merely share a");
    outln!("  word, and miss a call made through a string. A place to look.");
}

// ---------------------------------------------------------------------------
// Worktree handler
// ---------------------------------------------------------------------------

pub fn handle_worktree(arg: Option<&str>, workspace: &std::path::Path) {
    let mgr = WorktreeManager::new(workspace.to_path_buf());

    match arg {
        Some(sub) if sub.starts_with("create ") => {
            let name = sub.strip_prefix("create ").unwrap_or("task").trim();
            match mgr.create(name, None) {
                Ok(wt) => {
                    render::print_info(&format!(
                        "Worktree created: {} (branch: {}, path: {})",
                        wt.name,
                        wt.branch,
                        wt.path.display()
                    ));
                }
                Err(e) => render::print_error(&format!("Worktree create failed: {e}")),
            }
        }
        Some(sub) if sub.starts_with("remove ") => {
            let name = sub.strip_prefix("remove ").unwrap_or("").trim();
            match mgr.remove(name) {
                Ok(removed) => match removed.branch_kept {
                    None => render::print_info(&format!(
                        "Worktree '{name}' removed, and branch {} with it.",
                        removed.branch
                    )),
                    Some(why) => render::print_info(&format!(
                        "Worktree '{name}' removed. Branch {} was KEPT: {why}.",
                        removed.branch
                    )),
                },
                Err(e) => render::print_error(&format!("Worktree remove failed: {e}")),
            }
        }
        Some("list") | None => {
            render::print_header("Git Worktrees");
            outln!("{}", mgr.display_list());
            outln!();
        }
        Some(sub) => {
            render::print_error(&format!("Unknown worktree subcommand: {sub}"));
            render::print_info("Usage: /worktree [list | create <name> | remove <name>]");
        }
    }
    outln!();
}

// ---------------------------------------------------------------------------
// Pull request handler
// ---------------------------------------------------------------------------

/// `/pr [branch] [--draft]`: push the branch and open its pull request, or
/// report the one already open.
pub fn handle_pull_request(arg: Option<&str>, workspace: &std::path::Path) {
    let words: Vec<&str> = arg.unwrap_or("").split_whitespace().collect();
    if words.first() == Some(&"merge") {
        handle_pull_request_merge(&words[1..], workspace);
        return;
    }
    let draft = words.contains(&"--draft");
    let named: Vec<&str> = words.iter().copied().filter(|w| *w != "--draft").collect();
    if named.len() > 1 {
        render::print_error("Usage: /pr [branch] [--draft]");
        return;
    }
    let forge = crate::tools::pull_request::GitHubCli;
    match crate::tools::pull_request::open(workspace, named.first().copied(), draft, &forge) {
        Ok(opened) if opened.created => render::print_info(&format!(
            "Pushed {} and opened a pull request into {}: {}",
            opened.branch, opened.base, opened.url
        )),
        Ok(opened) => render::print_info(&format!(
            "Pushed {}; its pull request was already open: {}",
            opened.branch, opened.url
        )),
        Err(why) => render::print_error(&format!("/pr: {why}")),
    }
    outln!();
}

/// `/pr merge [branch] [--squash|--rebase]`: merge the branch's open pull
/// request, only as it was reviewed and checked.
fn handle_pull_request_merge(words: &[&str], workspace: &std::path::Path) {
    use crate::tools::pull_request::MergeMethod;
    let mut method = MergeMethod::Merge;
    let mut named = Vec::new();
    for word in words {
        match *word {
            "--squash" => method = MergeMethod::Squash,
            "--rebase" => method = MergeMethod::Rebase,
            "--merge" => method = MergeMethod::Merge,
            other => named.push(other),
        }
    }
    if named.len() > 1 {
        render::print_error("Usage: /pr merge [branch] [--squash|--rebase]");
        return;
    }
    let forge = crate::tools::pull_request::GitHubCli;
    match crate::tools::pull_request::merge(workspace, named.first().copied(), method, &forge) {
        Ok(merged) => {
            render::print_info(&format!(
                "Merged pull request #{} ({}) as {}: {}",
                merged.number, merged.branch, merged.commit, merged.url
            ));
            if merged.checks_passed == 0 {
                render::print_info("No checks ran on it: this merge rests on your review alone.");
            }
        }
        Err(why) => render::print_error(&format!("/pr merge: {why}")),
    }
    outln!();
}
