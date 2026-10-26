//! Custom commands: names from paths, front matter, arguments, the layers
//! that override each other, and the files a command includes.

use super::*;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

#[test]
fn names_come_from_the_path_with_colons() {
    let dir = Path::new("/c");
    assert_eq!(
        command_name(dir, Path::new("/c/frontend/test.md")).as_deref(),
        Some("frontend:test")
    );
    assert_eq!(
        command_name(dir, Path::new("/c/a/b/c.md")).as_deref(),
        Some("a:b:c")
    );
    assert_eq!(
        command_name(dir, Path::new("/c/review.md")).as_deref(),
        Some("review")
    );
    assert_eq!(command_name(dir, Path::new("/c/has space.md")), None);
}

#[test]
fn front_matter_is_read_and_the_body_kept() {
    let c = parse(
        "---\ndescription: Run the tests\nargument-hint: [file] [pattern]\nmodel: sonnet\n\
         allowed-tools: Bash(npm test:*), Read\n---\n\nTest $1\n",
    )
    .expect("parses");
    assert_eq!(c.description, "Run the tests");
    assert_eq!(c.argument_hint.as_deref(), Some("[file] [pattern]"));
    assert_eq!(c.model.as_deref(), Some("sonnet"));
    assert_eq!(c.allowed_tools, vec!["Bash(npm test:*)", "Read"]);
    assert_eq!(c.body, "Test $1");
}

/// **A command file that starts with a byte-order mark keeps its
/// `allowed-tools`.** The mark hid the opening `---`, so the front matter
/// was taken as body and the command ran with every tool, saying nothing.
#[test]
fn a_byte_order_mark_does_not_hide_the_front_matter() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(
        dir.path().join("review.md"),
        b"\xEF\xBB\xBF---\r\ndescription: Read only\r\nallowed-tools: Read\r\n---\r\nLook at $1\r\n",
    )
    .expect("write");
    let (found, problems) = scan_dir(dir.path(), None);
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].allowed_tools, vec!["Read"]);
    assert_eq!(found[0].description, "Read only");
    assert_eq!(found[0].body, "Look at $1");
}

#[test]
fn a_hint_in_brackets_alone_is_kept_literally() {
    let c =
        parse("---\nargument-hint: [file]\nallowed-tools: [Read, Grep]\n---\nx").expect("parses");
    assert_eq!(c.argument_hint.as_deref(), Some("[file]"));
    assert_eq!(c.allowed_tools, vec!["Read", "Grep"]);
}

#[test]
fn no_front_matter_is_all_body() {
    let c = parse("Just do it.\n").expect("parses");
    assert_eq!(c.body, "Just do it.");
    assert!(c.description.is_empty());
    assert!(parse("---\nnot a field\n---\nx").is_err());
}

#[test]
fn arguments_split_like_a_shell() {
    assert_eq!(
        split_args(r#"a "b c" 'd "e"' f\ g """#),
        vec!["a", "b c", "d \"e\"", "f g", ""]
    );
    assert!(split_args("   ").is_empty());
}

#[test]
fn arguments_are_substituted() {
    assert_eq!(
        substitute("Fix $1 in $2; all: $ARGUMENTS", r#"bug "src/a b.rs""#),
        r#"Fix bug in src/a b.rs; all: bug "src/a b.rs""#
    );
    assert_eq!(
        substitute("missing [$3] cost $$5", "x"),
        "missing [] cost $"
    );
    assert_eq!(substitute("$0 and $ alone", ""), "$0 and $ alone");
    assert_eq!(substitute("No placeholders.", ""), "No placeholders.");
    assert_eq!(
        substitute("No placeholders.", "extra words"),
        "No placeholders.\n\nARGUMENTS: extra words"
    );
}

#[test]
fn project_beats_user_and_builtins_beat_both() {
    let dir = tempfile::tempdir().expect("dir");
    let project = dir.path().join("p");
    let user = dir.path().join("u");
    write(&project.join("deploy.md"), "project deploy");
    write(&user.join("deploy.md"), "user deploy");
    write(
        &user.join("frontend/test.md"),
        "---\ndescription: t\n---\nrun $1",
    );
    write(&user.join("help.md"), "not the real help");
    write(&user.join("notes.txt"), "ignored");

    let r = Registry::load_from(&project, &user, &[]);
    let names: Vec<_> = r.commands().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["deploy", "frontend:test"]);
    assert_eq!(r.find("deploy").expect("found").body, "project deploy");
    assert_eq!(r.find("deploy").expect("found").origin, Origin::Project);
    assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
    assert!(r.warnings.iter().any(|w| w.contains("built-in /help")));
    assert!(r.warnings.iter().any(|w| w.contains("/deploy")));

    // A built-in is never expanded, even with a file of its name.
    assert_eq!(r.expand_line("/help", dir.path()), None);
    assert_eq!(r.expand_line("/nothing here", dir.path()), None);
    assert_eq!(r.expand_line("not a command", dir.path()), None);
    let e = r
        .expand_line("/frontend:test app.tsx", dir.path())
        .expect("expands");
    assert_eq!(e.prompt, "run app.tsx");
    assert_eq!(e.name, "frontend:test");
    // Names are matched without regard to case, as the parser lowercases.
    assert!(r.expand_line("/Frontend:Test", dir.path()).is_some());

    let menu = r.menu_entries();
    assert_eq!(
        menu[1],
        ("/frontend:test".to_string(), "t (user)".to_string())
    );
}

#[test]
fn plugin_commands_are_namespaced_and_lose_to_the_users() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("plugins/web");
    write(&root.join(crate::plugins::MANIFEST), "name = \"web\"\n");
    write(&root.join("commands/deploy.md"), "deploy from plugin");
    write(&root.join("commands/ci/run.md"), "run ci");
    let user = dir.path().join("u");
    write(&user.join("web/deploy.md"), "deploy from user");

    let found = crate::plugins::discover_in(None, &dir.path().join("plugins"));
    let r = Registry::load_from(&dir.path().join("p"), &user, &found.plugins);
    let names: Vec<_> = r.commands().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["web:deploy", "web:ci:run"]);
    assert_eq!(r.find("web:deploy").expect("found").origin, Origin::User);
    assert_eq!(
        r.find("web:ci:run").expect("found").origin,
        Origin::Plugin("web".to_string())
    );
    assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
}

#[test]
fn an_oversized_file_is_reported_not_read() {
    let dir = tempfile::tempdir().expect("dir");
    write(
        &dir.path().join("big.md"),
        &"x".repeat(MAX_FILE_BYTES as usize + 1),
    );
    let (found, problems) = scan_dir(dir.path(), None);
    assert!(found.is_empty());
    assert_eq!(problems.len(), 1);
}

#[test]
fn an_include_is_an_at_starting_a_word_and_naming_a_path() {
    assert_eq!(
        pieces("Read @src/a.rs, then @./Makefile."),
        vec![
            Piece::Text("Read "),
            Piece::Include("src/a.rs"),
            Piece::Text(", then "),
            Piece::Include("./Makefile"),
            Piece::Text("."),
        ]
    );
    assert_eq!(
        pieces("@a.md"),
        vec![Piece::Text(""), Piece::Include("a.md"), Piece::Text("")]
    );
    // An address, a mention, and an `@` alone are text.
    for text in [
        "mail a@b.com now",
        "ask @alice",
        "at @ noon",
        "x@./y",
        "@...",
    ] {
        assert_eq!(pieces(text), vec![Piece::Text(text)], "{text}");
    }
}

#[test]
fn a_file_is_included_fenced_under_its_path() {
    let dir = tempfile::tempdir().expect("dir");
    write(&dir.path().join("src/a.rs"), "fn a() {}\n");
    write(&dir.path().join("notes.md"), "has ``` inside");
    let said = fill(
        "Review @src/a.rs and @notes.md please; mail a@b.com",
        "",
        Some(dir.path()),
    );
    assert_eq!(
        said,
        "Review \nsrc/a.rs:\n```\nfn a() {}\n```\n and \nnotes.md:\n````\nhas ``` \
         inside\n````\n please; mail a@b.com"
    );
}

/// A Windows path is read whole, drive and backslashes included, so it
/// is judged like any other; a `:` that is not a drive still ends one.
#[test]
fn a_windows_path_is_one_include() {
    assert_eq!(
        include_path(r"C:\Users\me\secret.txt and more"),
        Some(r"C:\Users\me\secret.txt")
    );
    assert_eq!(include_path("C:/x/y.txt"), Some("C:/x/y.txt"));
    assert_eq!(include_path(r"src\a.rs, then"), Some(r"src\a.rs"));
    assert_eq!(include_path("a.rs:12"), Some("a.rs"));
    assert_eq!(include_path("alice"), None);
}

#[test]
fn what_cannot_be_included_leaves_a_marker() {
    let root = tempfile::tempdir().expect("dir");
    let ws = root.path().join("ws");
    write(&ws.join("inside.txt"), "ok");
    write(&root.path().join("secret.txt"), "SECRET");
    write(&ws.join("sub/x.txt"), "x");
    let outside = root.path().join("secret.txt");
    let said = fill(
        &format!(
            "@missing.txt @../secret.txt @sub/../../secret.txt @{} @sub/ @sub/../inside.txt",
            outside.display()
        ),
        "",
        Some(&ws),
    );
    assert!(said.contains("[@missing.txt: not found]"), "{said}");
    assert!(
        said.contains("[@../secret.txt: outside the workspace]"),
        "{said}"
    );
    assert!(
        said.contains("[@sub/../../secret.txt: outside the workspace]"),
        "{said}"
    );
    assert!(
        said.contains(&format!("[@{}: outside the workspace]", outside.display())),
        "{said}"
    );
    assert!(said.contains("[@sub/: not a file]"), "{said}");
    assert!(said.contains("sub/../inside.txt:\n```\nok\n```"), "{said}");
    assert!(!said.contains("SECRET"), "{said}");
    // An absolute path inside the workspace is taken.
    let inside = ws.join("inside.txt");
    let said = fill(&format!("@{}", inside.display()), "", Some(&ws));
    assert!(said.contains("```\nok\n```"), "{said}");
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_workspace_is_not_followed() {
    let root = tempfile::tempdir().expect("dir");
    let ws = root.path().join("ws");
    write(&root.path().join("secret.txt"), "SECRET");
    std::fs::create_dir_all(&ws).expect("mkdir");
    std::os::unix::fs::symlink(root.path().join("secret.txt"), ws.join("link.txt"))
        .expect("symlink");
    let said = fill("@link.txt", "", Some(&ws));
    assert_eq!(said, "[@link.txt: outside the workspace]");
}

#[test]
fn includes_are_capped_per_file_and_in_total() {
    let dir = tempfile::tempdir().expect("dir");
    let big = MAX_INCLUDE_BYTES as usize;
    write(&dir.path().join("huge.txt"), &"x".repeat(big + 1));
    for name in ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"] {
        write(&dir.path().join(name), &"y".repeat(big - 10));
    }
    let said = fill(
        "@huge.txt @a.txt @b.txt @c.txt @d.txt @e.txt",
        "",
        Some(dir.path()),
    );
    assert!(said.contains("[@huge.txt:"), "per file");
    assert!(said.contains("over the 262144-byte limit"));
    for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        assert!(said.contains(&format!("{name}:\n```")), "{name} included");
    }
    assert!(
        said.contains("[@e.txt: left out, the command's files reached 1048576 bytes]"),
        "the fifth is past the total"
    );
}

/// Only the body is looked at: an `@` in what was typed, or one a
/// placeholder fills in, is not read; and a placeholder inside an included
/// file is not filled.
#[test]
fn arguments_are_never_included_and_files_never_substituted() {
    let dir = tempfile::tempdir().expect("dir");
    write(&dir.path().join("secret.txt"), "SECRET");
    write(&dir.path().join("t.md"), "uses $1 literally");
    let said = fill(
        "Look at $ARGUMENTS and @$1",
        "@secret.txt",
        Some(dir.path()),
    );
    assert_eq!(said, "Look at @secret.txt and @@secret.txt");
    let said = fill("@t.md for $1", "x", Some(dir.path()));
    assert_eq!(
        said,
        "\nt.md:\n```\nuses $1 literally\n```\n for x".trim_start()
    );
}

#[test]
fn a_command_expands_its_includes_against_the_workspace() {
    let dir = tempfile::tempdir().expect("dir");
    let project = dir.path().join(PROJECT_DIR);
    write(&project.join("explain.md"), "Explain @src/lib.rs to $1");
    write(&dir.path().join("src/lib.rs"), "pub fn x() {}");
    let r = Registry::load_from(&project, &dir.path().join("u"), &[]);
    let e = r
        .expand_line("/explain newcomers", dir.path())
        .expect("expands");
    assert_eq!(
        e.prompt,
        "Explain \nsrc/lib.rs:\n```\npub fn x() {}\n```\n to newcomers"
    );
}
