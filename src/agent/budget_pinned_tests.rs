//! Pins on [`crate::agent::budget_for_spawn`], taken from the code before it
//! was broken into named steps: what it returns at every size around the
//! limits, which blocks it keeps first, where it cuts, and the file it
//! writes for a prompt too long to send. Each expectation is written out by
//! construction from the limits, so it holds on Windows and Unix alike.

use crate::agent::{budget_for_spawn, MAX_CMDLINE_PAYLOAD_LEN as MAX, MAX_PROMPT_BUDGET};

/// What a cut block ends with.
const CUT: &str = "\n\n[...truncated for OS command line limit...]";

fn budget(prompt: &str, context: &[String]) -> (String, String, Vec<String>) {
    let dir = tempfile::tempdir().expect("dir");
    let (p, c) = budget_for_spawn(prompt, context, dir.path()).expect("budgeted");
    let spilled = std::fs::read_dir(dir.path().join(crate::hooks::memory::CONTEXT))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    (p, c, spilled)
}

fn blocks(list: &[&str]) -> Vec<String> {
    list.iter().map(|b| (*b).to_string()).collect()
}

#[test]
fn nothing_at_all_is_nothing() {
    assert_eq!(budget("", &[]), (String::new(), String::new(), Vec::new()));
}

/// Under the limit every block is joined as given — blank and padded ones
/// too: only an over-limit payload is trimmed.
#[test]
fn under_the_limit_everything_is_joined_untouched() {
    let (p, c, files) = budget("p", &blocks(&["a", "  ", " b \n"]));
    assert_eq!(p, "p");
    assert_eq!(c, "a\n\n  \n\n b \n");
    assert!(files.is_empty());

    let prompt = "p".repeat(MAX - 10);
    let context = blocks(&["c".repeat(10).as_str()]);
    let (p, c, files) = budget(&prompt, &context);
    assert_eq!((p.len(), c.as_str()), (MAX - 10, "cccccccccc"));
    assert!(files.is_empty(), "exactly at the limit is under it");
}

/// One byte over: a single high-priority block is cut to the budget less
/// 80, and marked.
#[test]
fn one_byte_over_cuts_the_block_and_marks_it() {
    let block = format!("<guards>{}", "g".repeat(MAX - 8));
    let (p, c, files) = budget("p", std::slice::from_ref(&block));
    assert_eq!(p, "p");
    assert_eq!(c, format!("{}{CUT}", &block[..MAX - 81]));
    assert!(files.is_empty());
}

/// High-priority blocks go first whatever their place; blocks are trimmed and
/// blank ones dropped; the low block that does not fit is cut to what is
/// left less 80.
#[test]
fn high_priority_blocks_go_first_and_low_ones_are_cut() {
    let big = "z".repeat(MAX);
    let context = blocks(&["  low one \n", "   ", "<ROLES>r</ROLES>", big.as_str()]);
    let (p, c, _) = budget("p", &context);
    assert_eq!(p, "p");
    let kept = "<ROLES>r</ROLES>\n\nlow one\n\n";
    let left = (MAX - 1) - (16 + 2) - (7 + 2);
    assert_eq!(c, format!("{kept}{}{CUT}", "z".repeat(left - 80)));
}

/// Every marker of a high-priority block, in any case.
#[test]
fn every_high_priority_marker_is_recognised() {
    let highs = [
        "<guards>g</guards>",
        "The owner's standing rules: 1",
        "<intake>i</intake>",
        "<asking>a</asking>",
        "<roles>r</roles>",
        "<project-instructions path=\"x\">p</project-instructions>",
        "<output-style name=\"s\">o</output-style>",
        "disallowedTools: Bash",
        "STANDING RULES IN CAPS",
    ];
    let low = "l".repeat(MAX);
    let mut context = vec![low];
    context.extend(highs.iter().map(|h| (*h).to_string()));
    let (_, c, _) = budget("p", &context);
    let used: usize = highs.iter().map(|h| h.len() + 2).sum();
    let left = (MAX - 1) - used;
    assert_eq!(
        c,
        format!("{}\n\n{}{CUT}", highs.join("\n\n"), "l".repeat(left - 80))
    );
}

/// A high block that had to be cut still leaves room counted after it, and
/// a small low block that fits there is kept.
#[test]
fn a_low_block_still_fits_after_a_cut_high_one() {
    let high = format!("<guards>{}", "g".repeat(MAX));
    let context = vec!["tiny low".to_string(), high.clone()];
    let (_, c, _) = budget("p", &context);
    assert_eq!(c, format!("{}{CUT}\n\ntiny low", &high[..MAX - 81]));
}

/// A low block is cut only when MORE than 120 bytes are left.
#[test]
fn a_low_block_is_cut_only_past_120_left() {
    let low = "l".repeat(500);
    for (left, want_low) in [(120, None), (121, Some(41))] {
        let high = format!("<guards>{}", "g".repeat(MAX - 3 - left - 8));
        let (_, c, _) = budget("p", &[high.clone(), low.clone()]);
        let want = match want_low {
            None => high.clone(),
            Some(n) => format!("{high}\n\n{}{CUT}", "l".repeat(n)),
        };
        assert_eq!(c, want, "left={left}");
    }
}

/// A high block is cut only when MORE than 100 bytes are left; one that is
/// not cut is dropped, and the low blocks still get their turn.
#[test]
fn a_high_block_is_cut_only_past_100_left() {
    let second = format!("<asking>{}", "a".repeat(500));
    for (left, want_second) in [(100, None), (101, Some(21))] {
        let first = format!("<guards>{}", "g".repeat(MAX - 3 - left - 8));
        let context = vec![first.clone(), second.clone(), "x".to_string()];
        let (_, c, _) = budget("p", &context);
        let want = match want_second {
            None => format!("{first}\n\nx"),
            Some(n) => format!("{first}\n\n{}{CUT}\n\nx", &second[..n]),
        };
        assert_eq!(c, want, "left={left}");
    }
}

/// The cut never splits a character.
#[test]
fn the_cut_lands_on_a_character_boundary() {
    let block = format!("<guards>{}", "é".repeat(MAX));
    let (_, c, _) = budget("p", std::slice::from_ref(&block));
    let at = (0..=MAX - 81)
        .rev()
        .find(|&i| block.is_char_boundary(i))
        .expect("a boundary");
    assert_eq!(c, format!("{}{CUT}", &block[..at]));
}

/// A prompt of exactly the prompt budget is sent whole, even over the limit,
/// and the context takes the rest.
#[test]
fn a_prompt_at_the_prompt_budget_is_not_spilled() {
    let prompt = "q".repeat(MAX_PROMPT_BUDGET);
    let context = vec!["c".repeat(MAX - MAX_PROMPT_BUDGET + 1)];
    let (p, c, files) = budget(&prompt, &context);
    assert_eq!(p, prompt);
    assert_eq!(
        c,
        format!("{}{CUT}", "c".repeat(MAX - MAX_PROMPT_BUDGET - 80))
    );
    assert!(files.is_empty());
}

/// One byte over the prompt budget, over the limit: the prompt goes to a
/// file named by its hash, the agent is told where in these exact words, and
/// a context that fits beside the notice is untouched. The same prompt is the
/// same file.
#[test]
fn an_over_budget_prompt_is_spilled_with_the_same_notice() {
    let prompt = "q".repeat(MAX_PROMPT_BUDGET + 1);
    let context = vec!["c".repeat(MAX - MAX_PROMPT_BUDGET)];
    let dir = tempfile::tempdir().expect("dir");
    let (p, c) = budget_for_spawn(&prompt, &context, dir.path()).expect("budgeted");
    assert_eq!(c, context[0]);
    let folder = dir.path().join(crate::hooks::memory::CONTEXT);
    let names: Vec<String> = std::fs::read_dir(&folder)
        .expect("the context folder")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    let name = &names[0];
    assert!(
        name.len() == "task-".len() + 16 + ".md".len()
            && name.starts_with("task-")
            && name.ends_with(".md"),
        "{name}"
    );
    assert_eq!(
        std::fs::read_to_string(folder.join(name)).expect("the file"),
        prompt
    );
    assert_eq!(
        p,
        format!(
            "This task is {} bytes, more than a command line can carry, so the whole of it \
             is in the file `{}/{name}` in your working folder ({}). Read that file in \
             full, from its first line to its last, before doing anything else: it is the \
             task, and what it asks is what you do.",
            prompt.len(),
            crate::hooks::memory::CONTEXT,
            folder.join(name).display()
        )
    );
    let (again, _) = budget_for_spawn(&prompt, &context, dir.path()).expect("budgeted");
    assert_eq!(again, p, "the same prompt is the same file");
}
