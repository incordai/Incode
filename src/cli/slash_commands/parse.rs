//! A typed line to a [`SlashCommand`]: which word names which command, and
//! how each command reads what was typed after it.

use super::SlashCommand;

/// How one command word turns what was typed after it into a command.
type Parser = fn(Option<String>) -> SlashCommand;

/// Every spelling a command answers to, and how it reads its argument.
///
/// A table rather than one `match` arm per word: the words are data, and each
/// command that needs more than "keep the argument" gets a named helper below
/// instead of a branch in [`SlashCommand::parse`]. Every word appears once —
/// `no_spelling_is_listed_twice` holds that, as an unreachable `match` arm
/// used to.
///
/// Called by [`SlashCommand::parse`].
const SPELLINGS: &[(&[&str], Parser)] = &[
    (&["/help", "/h", "/?", "/"], |_| SlashCommand::Help),
    (&["/status", "/s"], |_| SlashCommand::Status),
    (&["/compact"], |arg| SlashCommand::Compact(non_empty(arg))),
    (&["/review"], |arg| SlashCommand::Review(non_empty(arg))),
    (&["/bug", "/feedback"], |arg| {
        SlashCommand::Bug(non_empty(arg))
    }),
    (&["/style"], |arg| SlashCommand::Style(non_empty(arg))),
    (&["/cost"], |_| SlashCommand::Cost),
    (&["/vim"], |_| SlashCommand::Vim),
    (&["/undo", "/u"], |_| SlashCommand::Undo),
    (&["/rewind"], rewind),
    (&["/agents"], |_| SlashCommand::Agents),
    (&["/doctor", "/dr"], |_| SlashCommand::Doctor),
    (&["/guards", "/g"], |_| SlashCommand::Guards),
    (&["/permissions", "/perms"], |_| SlashCommand::Permissions),
    (&["/add-dir"], SlashCommand::AddDir),
    (&["/links", "/l"], SlashCommand::Links),
    (&["/run"], |_| SlashCommand::Run),
    (&["/roadmap"], |_| SlashCommand::Roadmap),
    (&["/admin"], |_| SlashCommand::Admin),
    (&["/agent"], SlashCommand::Agent),
    (&["/roles"], SlashCommand::Roles),
    (&["/tools", "/t"], |arg| SlashCommand::Tools(non_empty(arg))),
    (&["/skills", "/sk"], |_| SlashCommand::Skills),
    (&["/mcp"], |_| SlashCommand::Mcp),
    (&["/model", "/m"], SlashCommand::Model),
    (&["/models"], |_| SlashCommand::Models),
    (&["/clear"], |_| SlashCommand::Clear),
    (&["/save"], SlashCommand::Save),
    (&["/sessions", "/ls"], |_| SlashCommand::Sessions),
    (&["/resume", "/load"], SlashCommand::Resume),
    (&["/remember", "/rem"], remember),
    (&["/forget"], forget),
    (&["/memories", "/mem"], SlashCommand::Memories),
    (&["/plan"], |_| SlashCommand::Plan),
    (&["/act"], |_| SlashCommand::Act),
    (&["/jobs"], |_| SlashCommand::Jobs),
    (&["/kill"], kill),
    (&["/bg"], background),
    (&["/worktree", "/wt"], SlashCommand::Worktree),
    (&["/pr"], SlashCommand::PullRequest),
    (&["/diff", "/df"], SlashCommand::Diff),
    (&["/image", "/img"], image),
    (&["/env"], |_| SlashCommand::Env),
    (&["/quit", "/exit", "/q"], |_| SlashCommand::Quit),
];

impl SlashCommand {
    /// Parse a line that starts with `/`.
    pub fn parse(line: &str) -> Option<Self> {
        if !line.starts_with('/') {
            return None;
        }

        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        let cmd = parts[0].to_lowercase();
        let arg = parts.get(1).map(|s| s.trim().to_string());

        let parser = SPELLINGS
            .iter()
            .find(|(words, _)| words.contains(&cmd.as_str()))
            .map(|(_, parser)| *parser);
        Some(match parser {
            Some(parser) => parser(arg),
            None => Self::Unknown(cmd),
        })
    }
}

/// The argument, unless nothing was typed after the space.
///
/// Called by the [`SPELLINGS`] entries for `/compact`, `/review`, `/bug`,
/// `/style` and `/tools`, and by [`image`] — the commands that treat an empty
/// argument as none.
fn non_empty(arg: Option<String>) -> Option<String> {
    arg.filter(|a| !a.is_empty())
}

/// `/rewind <n>`: a count of changed turns, from one.
///
/// Called by [`SPELLINGS`] for `/rewind`.
fn rewind(arg: Option<String>) -> SlashCommand {
    match arg.as_deref().map(str::parse::<usize>) {
        Some(Ok(turns)) if turns > 0 => SlashCommand::Rewind(turns),
        _ => SlashCommand::Usage("/rewind <n>  (how many changed turns back, from 1)".to_string()),
    }
}

/// `/remember <text>`: the text, kept as typed — even empty, which
/// `handle_remember` answers with its own usage line.
///
/// Called by [`SPELLINGS`] for `/remember` and `/rem`.
fn remember(arg: Option<String>) -> SlashCommand {
    match arg {
        Some(text) => SlashCommand::Remember(text),
        None => SlashCommand::Usage("/remember <what to remember>".to_string()),
    }
}

/// `/forget <conv_id>#<idx> [confirm]`.
///
/// `confirm` is typed by a person, deliberately, as a second word — the same
/// two-step the tool and the service enforce.
///
/// Called by [`SPELLINGS`] for `/forget`.
fn forget(arg: Option<String>) -> SlashCommand {
    match arg {
        Some(rest) => {
            let (id, tail) = rest.split_once(' ').unwrap_or((rest.as_str(), ""));
            SlashCommand::Forget {
                id: id.to_string(),
                confirmed: tail.trim().eq_ignore_ascii_case("confirm"),
            }
        }
        None => SlashCommand::Usage("/forget <conv_id>#<idx> [confirm]".to_string()),
    }
}

/// `/kill <id>`: a job number from `/jobs`.
///
/// Called by [`SPELLINGS`] for `/kill`.
fn kill(arg: Option<String>) -> SlashCommand {
    match arg.as_deref().map(str::parse::<u32>) {
        Some(Ok(id)) => SlashCommand::Kill(id),
        _ => SlashCommand::Usage("/kill <id>  (a job number from /jobs)".to_string()),
    }
}

/// `/bg <command>`: the shell command, kept as typed.
///
/// Called by [`SPELLINGS`] for `/bg`.
fn background(arg: Option<String>) -> SlashCommand {
    match arg {
        Some(cmd) => SlashCommand::Background(cmd),
        None => SlashCommand::Usage(
            "/bg <command>  (a shell command to run in the background)".to_string(),
        ),
    }
}

/// `/image <path>`: a path, and an empty one is none.
///
/// Called by [`SPELLINGS`] for `/image` and `/img`.
fn image(arg: Option<String>) -> SlashCommand {
    match non_empty(arg) {
        Some(path) => SlashCommand::Image(path),
        None => SlashCommand::Usage("/image <path to a PNG or JPEG>".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{command_name, COMMANDS};
    use super::*;

    /// **Every command in the table must be one `parse` actually accepts**, or
    /// the help lists something that cannot be typed and completion offers it.
    ///
    /// A bare command that still needs an argument is fine — it is
    /// [`SlashCommand::Usage`], which names what is missing. What is NOT fine is
    /// `Unknown`, which tells the operator the command does not exist.
    ///
    /// This test found five: `/remember`, `/forget`, `/kill` (twice) and `/bg`
    /// all answered "Unknown command" when typed without their argument.
    #[test]
    fn every_listed_command_parses_to_something_real() {
        for (entry, _) in COMMANDS {
            let word = command_name(entry);
            let parsed = SlashCommand::parse(word)
                .unwrap_or_else(|| panic!("`{word}` is listed in /help but does not parse"));
            assert!(
                !matches!(parsed, SlashCommand::Unknown(_)),
                "`{word}` is listed in /help yet parses as Unknown — the operator is told a \
                 real command does not exist"
            );
        }
    }

    /// **A spelling listed twice would answer to whichever entry comes
    /// first**, silently — the `match` this table replaced made the compiler
    /// call that an unreachable pattern. Every word is listed once, in lower
    /// case (what `parse` compares against) and starting with `/`.
    #[test]
    fn no_spelling_is_listed_twice() {
        let mut seen = std::collections::BTreeSet::new();
        for (words, _) in SPELLINGS {
            for word in *words {
                assert!(seen.insert(*word), "`{word}` is listed twice");
                assert!(word.starts_with('/'), "`{word}` does not start with /");
                assert_eq!(*word, word.to_lowercase(), "`{word}` can never match");
            }
        }
    }

    /// A command missing its argument names what it needs, and never claims the
    /// command itself is unknown.
    #[test]
    fn a_command_without_its_argument_is_told_what_it_needs() {
        for (typed, must_mention) in [
            ("/remember", "what to remember"),
            ("/forget", "conv_id"),
            ("/kill", "/jobs"),
            ("/bg", "command"),
        ] {
            match SlashCommand::parse(typed) {
                Some(SlashCommand::Usage(usage)) => assert!(
                    usage.contains(must_mention),
                    "`{typed}` must say what it needs — expected `{must_mention}` in `{usage}`"
                ),
                other => panic!("`{typed}` alone should ask for its argument, got {other:?}"),
            }
        }
    }

    /// A genuinely unknown command is still unknown — the new variant must not
    /// swallow the case it was split off from.
    #[test]
    fn a_command_that_does_not_exist_is_still_unknown() {
        assert_eq!(
            SlashCommand::parse("/definitely-not-a-command"),
            Some(SlashCommand::Unknown(
                "/definitely-not-a-command".to_string()
            ))
        );
    }

    /// `/kill` with a non-numeric id is a usage problem, not an unknown command.
    #[test]
    fn kill_with_a_bad_id_asks_for_a_job_number() {
        assert!(matches!(
            SlashCommand::parse("/kill not-a-number"),
            Some(SlashCommand::Usage(_))
        ));
    }

    #[test]
    fn parses_slash_commands() {
        assert_eq!(SlashCommand::parse("/help"), Some(SlashCommand::Help));
        assert_eq!(SlashCommand::parse("/status"), Some(SlashCommand::Status));
        assert_eq!(
            SlashCommand::parse("/compact"),
            Some(SlashCommand::Compact(None))
        );
        assert_eq!(
            SlashCommand::parse("/compact keep the API notes"),
            Some(SlashCommand::Compact(Some("keep the API notes".into())))
        );
        assert_eq!(SlashCommand::parse("/bug"), Some(SlashCommand::Bug(None)));
        assert_eq!(
            SlashCommand::parse("/style"),
            Some(SlashCommand::Style(None))
        );
        assert_eq!(
            SlashCommand::parse("/style teacher"),
            Some(SlashCommand::Style(Some("teacher".into())))
        );
        assert_eq!(
            SlashCommand::parse("/feedback the footer flickers"),
            Some(SlashCommand::Bug(Some("the footer flickers".into())))
        );
        assert_eq!(
            SlashCommand::parse("/review"),
            Some(SlashCommand::Review(None))
        );
        assert_eq!(
            SlashCommand::parse("/review origin/main"),
            Some(SlashCommand::Review(Some("origin/main".into())))
        );
        assert_eq!(
            SlashCommand::parse("/tools"),
            Some(SlashCommand::Tools(None))
        );
        assert_eq!(
            SlashCommand::parse("/tools add browser"),
            Some(SlashCommand::Tools(Some("add browser".to_string())))
        );
        assert_eq!(SlashCommand::parse("/quit"), Some(SlashCommand::Quit));
        assert_eq!(
            SlashCommand::parse("/model opus"),
            Some(SlashCommand::Model(Some("opus".to_string())))
        );
        assert_eq!(
            SlashCommand::parse("/model"),
            Some(SlashCommand::Model(None))
        );
        assert_eq!(SlashCommand::parse("hello"), None);
    }

    #[test]
    fn parses_session_commands() {
        assert_eq!(
            SlashCommand::parse("/sessions"),
            Some(SlashCommand::Sessions)
        );
        assert_eq!(SlashCommand::parse("/ls"), Some(SlashCommand::Sessions));
        assert_eq!(
            SlashCommand::parse("/resume myfile"),
            Some(SlashCommand::Resume(Some("myfile".to_string())))
        );
        assert_eq!(
            SlashCommand::parse("/load myfile"),
            Some(SlashCommand::Resume(Some("myfile".to_string())))
        );
        assert_eq!(
            SlashCommand::parse("/resume"),
            Some(SlashCommand::Resume(None))
        );
        assert_eq!(
            SlashCommand::parse("/save myname"),
            Some(SlashCommand::Save(Some("myname".to_string())))
        );
    }

    /// The command is the only way a user reaches undo, so both spellings are
    /// pinned — and `/u` must not be swallowed by some other prefix match.
    /// `/image` takes a path, and without one says what it needs.
    #[test]
    fn parses_image() {
        assert_eq!(
            SlashCommand::parse("/image shots/a.png"),
            Some(SlashCommand::Image("shots/a.png".to_string()))
        );
        assert!(matches!(
            SlashCommand::parse("/image"),
            Some(SlashCommand::Usage(usage)) if usage.starts_with("/image <path")
        ));
    }

    #[test]
    fn parses_undo() {
        assert_eq!(SlashCommand::parse("/run"), Some(SlashCommand::Run));
        assert_eq!(SlashCommand::parse("/admin"), Some(SlashCommand::Admin));
        assert_eq!(SlashCommand::parse("/undo"), Some(SlashCommand::Undo));
        assert_eq!(SlashCommand::parse("/u"), Some(SlashCommand::Undo));
        assert_eq!(SlashCommand::parse("/UNDO"), Some(SlashCommand::Undo));
    }

    #[test]
    fn parses_agent() {
        assert_eq!(
            SlashCommand::parse("/agent"),
            Some(SlashCommand::Agent(None))
        );
        assert_eq!(
            SlashCommand::parse("/agent sub codex"),
            Some(SlashCommand::Agent(Some("sub codex".to_string())))
        );
    }

    #[test]
    fn parses_roles() {
        assert_eq!(
            SlashCommand::parse("/roles"),
            Some(SlashCommand::Roles(None))
        );
        assert_eq!(
            SlashCommand::parse("/roles review model-a @top"),
            Some(SlashCommand::Roles(Some("review model-a @top".to_string())))
        );
    }

    #[test]
    fn parses_unknown_command() {
        assert_eq!(
            SlashCommand::parse("/foobar"),
            Some(SlashCommand::Unknown("/foobar".to_string()))
        );
    }

    // EVERY SPELLING OF EVERY COMMAND, with and without its argument, and the
    // exact usage text of each one that needs one — in the three tables below,
    // one per kind of argument. Pinned so a change to how `parse` is built
    // cannot change what it accepts: the edges matter — an empty argument is
    // kept for some commands and dropped for others, `/rewind 0` is refused,
    // and `/kill +5` is a job number.

    /// `Some(text)`, for the tables below.
    fn arg(text: &str) -> Option<String> {
        Some(text.to_string())
    }

    /// `SlashCommand::Usage(text)`, for the tables below.
    fn usage(text: &str) -> SlashCommand {
        SlashCommand::Usage(text.to_string())
    }

    /// Every line in `table` parses to exactly what is beside it.
    ///
    /// Called by the `*_parse_as_pinned` tests below.
    fn assert_parses(table: Vec<(&str, Option<SlashCommand>)>) {
        for (typed, expected) in table {
            assert_eq!(SlashCommand::parse(typed), expected, "parsing `{typed:?}`");
        }
    }

    /// What is not a command, the spelling rules every command shares, and
    /// the commands whose empty argument counts as none.
    #[test]
    fn spellings_and_dropped_empty_arguments_parse_as_pinned() {
        use SlashCommand as C;
        assert_parses(vec![
            // Not a command at all.
            ("hello", None),
            ("", None),
            (" /help", None),
            // Help, and the spelling rules every command shares.
            ("/help", Some(C::Help)),
            ("/h", Some(C::Help)),
            ("/?", Some(C::Help)),
            ("/", Some(C::Help)),
            ("/HELP", Some(C::Help)),
            ("/help ignored words", Some(C::Help)),
            ("/status", Some(C::Status)),
            ("/s", Some(C::Status)),
            // Optional text, where an empty one counts as none.
            ("/compact", Some(C::Compact(None))),
            ("/compact ", Some(C::Compact(None))),
            (
                "/compact   keep the notes  ",
                Some(C::Compact(arg("keep the notes"))),
            ),
            ("/review", Some(C::Review(None))),
            ("/review ", Some(C::Review(None))),
            ("/review main", Some(C::Review(arg("main")))),
            ("/bug", Some(C::Bug(None))),
            ("/bug ", Some(C::Bug(None))),
            ("/feedback it broke", Some(C::Bug(arg("it broke")))),
            ("/style", Some(C::Style(None))),
            ("/style ", Some(C::Style(None))),
            ("/style teacher", Some(C::Style(arg("teacher")))),
            ("/tools", Some(C::Tools(None))),
            ("/t ", Some(C::Tools(None))),
            ("/t add browser", Some(C::Tools(arg("add browser")))),
        ]);
    }

    /// The commands that take no argument, and those that keep an empty one
    /// as typed.
    #[test]
    fn bare_commands_and_kept_empty_arguments_parse_as_pinned() {
        use SlashCommand as C;
        assert_parses(vec![
            // No argument.
            ("/cost", Some(C::Cost)),
            ("/vim", Some(C::Vim)),
            ("/undo", Some(C::Undo)),
            ("/u", Some(C::Undo)),
            ("/agents", Some(C::Agents)),
            ("/doctor", Some(C::Doctor)),
            ("/dr", Some(C::Doctor)),
            ("/guards", Some(C::Guards)),
            ("/g", Some(C::Guards)),
            ("/permissions", Some(C::Permissions)),
            ("/perms", Some(C::Permissions)),
            ("/run", Some(C::Run)),
            ("/roadmap", Some(C::Roadmap)),
            ("/admin", Some(C::Admin)),
            ("/skills", Some(C::Skills)),
            ("/sk", Some(C::Skills)),
            ("/mcp", Some(C::Mcp)),
            ("/models", Some(C::Models)),
            ("/clear", Some(C::Clear)),
            ("/sessions", Some(C::Sessions)),
            ("/ls", Some(C::Sessions)),
            ("/plan", Some(C::Plan)),
            ("/act", Some(C::Act)),
            ("/jobs", Some(C::Jobs)),
            ("/env", Some(C::Env)),
            ("/quit", Some(C::Quit)),
            ("/exit", Some(C::Quit)),
            ("/q", Some(C::Quit)),
            // Optional text, where an empty one is kept as typed.
            ("/add-dir", Some(C::AddDir(None))),
            ("/add-dir ", Some(C::AddDir(arg("")))),
            ("/add-dir ../other", Some(C::AddDir(arg("../other")))),
            ("/links", Some(C::Links(None))),
            ("/l src/a.rs", Some(C::Links(arg("src/a.rs")))),
            ("/agent", Some(C::Agent(None))),
            ("/agent ", Some(C::Agent(arg("")))),
            ("/agent sub codex", Some(C::Agent(arg("sub codex")))),
            ("/roles", Some(C::Roles(None))),
            ("/roles plan a", Some(C::Roles(arg("plan a")))),
            ("/model", Some(C::Model(None))),
            ("/m ", Some(C::Model(arg("")))),
            ("/m opus", Some(C::Model(arg("opus")))),
            ("/save", Some(C::Save(None))),
            ("/save name", Some(C::Save(arg("name")))),
            ("/resume", Some(C::Resume(None))),
            ("/load id", Some(C::Resume(arg("id")))),
            ("/memories", Some(C::Memories(None))),
            ("/mem what", Some(C::Memories(arg("what")))),
            ("/worktree", Some(C::Worktree(None))),
            ("/wt create x", Some(C::Worktree(arg("create x")))),
            ("/pr", Some(C::PullRequest(None))),
            (
                "/pr merge --squash",
                Some(C::PullRequest(arg("merge --squash"))),
            ),
            ("/diff", Some(C::Diff(None))),
            ("/df a.rs HEAD~1", Some(C::Diff(arg("a.rs HEAD~1")))),
        ]);
    }

    /// The commands that need their argument — each refusal's exact usage
    /// text — and what stays unknown.
    #[test]
    fn required_arguments_and_unknown_words_parse_as_pinned() {
        use SlashCommand as C;
        let rewind = "/rewind <n>  (how many changed turns back, from 1)";
        let kill = "/kill <id>  (a job number from /jobs)";
        let bg = "/bg <command>  (a shell command to run in the background)";
        assert_parses(vec![
            // A count of turns, from one.
            ("/rewind 3", Some(C::Rewind(3))),
            ("/rewind", Some(usage(rewind))),
            ("/rewind ", Some(usage(rewind))),
            ("/rewind 0", Some(usage(rewind))),
            ("/rewind -1", Some(usage(rewind))),
            ("/rewind two", Some(usage(rewind))),
            // Required text.
            ("/remember", Some(usage("/remember <what to remember>"))),
            ("/remember ", Some(C::Remember(String::new()))),
            (
                "/rem the port is 8080",
                Some(C::Remember("the port is 8080".into())),
            ),
            ("/bg", Some(usage(bg))),
            ("/bg ", Some(C::Background(String::new()))),
            ("/bg cargo build", Some(C::Background("cargo build".into()))),
            ("/image", Some(usage("/image <path to a PNG or JPEG>"))),
            ("/img ", Some(usage("/image <path to a PNG or JPEG>"))),
            ("/img a.png", Some(C::Image("a.png".into()))),
            // A job number.
            ("/kill 7", Some(C::Kill(7))),
            ("/kill +5", Some(C::Kill(5))),
            ("/kill", Some(usage(kill))),
            ("/kill -1", Some(usage(kill))),
            ("/kill seven", Some(usage(kill))),
            // A memory reference, and the deliberate second word.
            ("/forget", Some(usage("/forget <conv_id>#<idx> [confirm]"))),
            (
                "/forget ",
                Some(C::Forget {
                    id: String::new(),
                    confirmed: false,
                }),
            ),
            (
                "/forget c#1",
                Some(C::Forget {
                    id: "c#1".into(),
                    confirmed: false,
                }),
            ),
            (
                "/forget c#1 CONFIRM",
                Some(C::Forget {
                    id: "c#1".into(),
                    confirmed: true,
                }),
            ),
            (
                "/forget c#1   confirm  ",
                Some(C::Forget {
                    id: "c#1".into(),
                    confirmed: true,
                }),
            ),
            (
                "/forget c#1 confirm now",
                Some(C::Forget {
                    id: "c#1".into(),
                    confirmed: false,
                }),
            ),
            // Unknown, lower-cased as typed; only a space separates.
            ("/FooBar", Some(C::Unknown("/foobar".into()))),
            ("//help", Some(C::Unknown("//help".into()))),
            ("/compact\tnow", Some(C::Unknown("/compact\tnow".into()))),
        ]);
    }
}
