//! Tests for the search engine, Outline, document reading, ConvertDocument
//! and input validation — each through [`Toolbox::run`], the path every route
//! takes, so the mode, the guards and the engine apply as they do for real.

use super::*;
use crate::hooks::engine::fake::{self, Reply};
use crate::permissions::PermissionMode;
use crate::tools::documents::fixtures;
use std::time::Duration;

fn toolbox(root: &Path) -> Toolbox {
    Toolbox::new(root, true).with_soffice(None)
}

fn read_only(root: &Path) -> Toolbox {
    let mut tools = toolbox(root);
    tools.set_permission_mode(PermissionMode::ReadOnly);
    tools
}

// ---------------------------------------------------------------------------
// Grep, Glob and LS agree
// ---------------------------------------------------------------------------

/// **THE THREE SEARCH TOOLS AGREE ON WHAT IS IGNORED**, and LS names what
/// it left out rather than dropping it in silence.
#[test]
fn grep_glob_and_ls_skip_the_same_entries_and_ls_names_them() {
    let dir = tempfile::tempdir().expect("tmp");
    let root = dir.path();
    std::fs::create_dir_all(root.join("node_modules/pkg")).expect("dir");
    std::fs::create_dir_all(root.join("target")).expect("dir");
    std::fs::write(
        root.join("target/CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55",
    )
    .expect("seed");
    std::fs::write(root.join("target/built.rs"), "needle").expect("seed");
    std::fs::write(root.join("node_modules/pkg/i.rs"), "needle").expect("seed");
    std::fs::write(root.join(".gitignore"), "skip.rs\n").expect("seed");
    std::fs::write(root.join("skip.rs"), "needle").expect("seed");
    std::fs::write(root.join(".secret.rs"), "needle").expect("seed");
    std::fs::write(root.join("keep.rs"), "needle").expect("seed");

    let tools = read_only(root);
    let grep = tools
        .run(GREP, &json!({"pattern": "needle"}))
        .expect("grep observes");
    let glob = tools
        .run(GLOB, &json!({"pattern": "*.rs"}))
        .expect("glob observes");
    for out in [&grep, &glob] {
        assert!(out.contains("keep.rs"), "{out}");
        for skipped in ["built.rs", "i.rs", "skip.rs", ".secret.rs"] {
            assert!(!out.contains(skipped), "{skipped} in: {out}");
        }
    }
    let ls = tools.run(LS, &json!({})).expect("ls observes");
    assert!(ls.contains("keep.rs"), "{ls}");
    assert!(!ls.contains("  skip.rs  ("), "{ls}");
    assert!(
        ls.contains("not listed — ignored, hidden or build output:"),
        "{ls}"
    );
    for named in [
        ".gitignore",
        ".secret.rs",
        "node_modules",
        "skip.rs",
        "target",
    ] {
        assert!(ls.contains(named), "{named} not named: {ls}");
    }
}

// ---------------------------------------------------------------------------
// Read, for documents
// ---------------------------------------------------------------------------

/// **READ RETURNS A DOCUMENT AS MARKDOWN** — a PDF, a workbook, a Word
/// document and a presentation — and does so in a read-only session.
#[test]
fn read_returns_documents_as_markdown_even_read_only() {
    let dir = tempfile::tempdir().expect("tmp");
    let root = dir.path();
    std::fs::write(
        root.join("r.pdf"),
        fixtures::pdf(&[&["Quarterly Report", "Revenue grew strongly."]]),
    )
    .expect("seed");
    std::fs::write(root.join("d.docx"), fixtures::docx(fixtures::DOCX_BODY)).expect("seed");
    std::fs::write(
        root.join("b.xlsx"),
        fixtures::xlsx(&[(
            "Sales",
            vec![
                vec![fixtures::Cell::Text("Region")],
                vec![fixtures::Cell::Text("North")],
            ],
        )]),
    )
    .expect("seed");
    std::fs::write(
        root.join("p.pptx"),
        fixtures::pptx(
            &[fixtures::Slide {
                title: "Roadmap",
                body: "Ship it",
                notes: Some("Say it slowly"),
            }],
            &[1],
        ),
    )
    .expect("seed");

    let tools = read_only(root);
    let pdf = tools.run(READ, &json!({"path": "r.pdf"})).expect("pdf");
    assert!(pdf.starts_with("[PDF "), "{pdf}");
    assert!(pdf.contains("## Page 1"), "{pdf}");
    assert!(pdf.contains("Revenue grew strongly."), "{pdf}");
    let docx = tools.run(READ, &json!({"path": "d.docx"})).expect("docx");
    assert!(docx.contains("\t# Quarterly Report"), "{docx}");
    let xlsx = tools.run(READ, &json!({"path": "b.xlsx"})).expect("xlsx");
    assert!(xlsx.contains("| North |"), "{xlsx}");
    let pptx = tools.run(READ, &json!({"path": "p.pptx"})).expect("pptx");
    assert!(pptx.contains("### Roadmap"), "{pptx}");
    assert!(pptx.contains("> Say it slowly"), "{pptx}");

    // Paging works on the markdown.
    let paged = tools
        .run(READ, &json!({"path": "d.docx", "offset": 2, "limit": 1}))
        .expect("paged");
    assert_eq!(paged.lines().count(), 2, "header and one line: {paged}");
    assert!(paged.contains("     3\t"), "{paged}");
}

/// HTML and CSV stay source unless `render` is asked for: Edit needs the
/// exact text of a file someone is working on.
#[test]
fn html_and_csv_are_source_by_default_and_markdown_when_rendered() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(
        dir.path().join("i.html"),
        "<html><body><h1>Title</h1><p>Body text</p></body></html>",
    )
    .expect("seed");
    std::fs::write(dir.path().join("t.csv"), "a,b\n1,2\n").expect("seed");
    let tools = toolbox(dir.path());
    let source = tools.run(READ, &json!({"path": "i.html"})).expect("html");
    assert!(source.contains("<h1>Title</h1>"), "{source}");
    let rendered = tools
        .run(READ, &json!({"path": "i.html", "render": true}))
        .expect("html");
    assert!(rendered.contains("# Title"), "{rendered}");
    assert!(!rendered.contains("<h1>"), "{rendered}");
    let csv = tools.run(READ, &json!({"path": "t.csv"})).expect("csv");
    assert!(csv.contains("a,b"), "{csv}");
    let table = tools
        .run(READ, &json!({"path": "t.csv", "render": true}))
        .expect("csv");
    assert!(table.contains("| 1 | 2 |"), "{table}");
}

#[test]
fn a_broken_document_is_an_error_the_model_can_read() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(dir.path().join("x.docx"), b"PK\x03\x04 not really").expect("seed");
    let why = toolbox(dir.path())
        .run(READ, &json!({"path": "x.docx"}))
        .expect_err("broken");
    assert!(why.contains("not a readable archive"), "{why}");
}

// ---------------------------------------------------------------------------
// Outline
// ---------------------------------------------------------------------------

#[test]
fn outline_runs_read_only_and_through_the_toolbox() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(dir.path().join("m.py"), "def f(x):\n    return x\n").expect("seed");
    let out = read_only(dir.path())
        .run(OUTLINE, &json!({"path": "m.py"}))
        .expect("outline observes");
    assert!(out.contains("def f [L1-2]: def f(x)"), "{out}");
    assert_eq!(reach_of(OUTLINE), Some(Reach::Observe));
}

// ---------------------------------------------------------------------------
// ConvertDocument
// ---------------------------------------------------------------------------

/// **NOT OFFERED WITHOUT LIBREOFFICE**, and not runnable by a model that
/// names it anyway.
#[test]
fn convert_document_is_not_offered_or_run_without_libreoffice() {
    let dir = tempfile::tempdir().expect("tmp");
    let tools = toolbox(dir.path());
    assert!(!tools.names().iter().any(|n| n == CONVERT_DOCUMENT));
    assert!(tools.names().iter().any(|n| n == OUTLINE));
    let why = tools
        .run(CONVERT_DOCUMENT, &json!({"path": "a.docx"}))
        .expect_err("not offered");
    assert!(why.contains("LibreOffice was not found"), "{why}");
}

/// Offered when found, as a CHANGE: refused read-only, and its OUTPUT path
/// is what the guards check.
#[test]
fn convert_document_is_offered_when_found_and_guarded_as_a_change() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(dir.path().join("tests")).expect("dir");
    std::fs::write(dir.path().join("tests/spec.docx"), "x").expect("seed");
    let fake = dir.path().join("soffice.exe");
    let tools = Toolbox::new(dir.path(), true).with_soffice(Some(fake.clone()));
    assert!(tools.names().iter().any(|n| n == CONVERT_DOCUMENT));
    assert_eq!(reach_of(CONVERT_DOCUMENT), Some(Reach::Change));

    let refused = tools
        .run(CONVERT_DOCUMENT, &json!({"path": "tests/spec.docx"}))
        .expect_err("writes tests/spec.pdf");
    assert!(refused.contains("protected"), "{refused}");
    assert!(
        refused.contains("spec.pdf"),
        "the output is what is checked: {refused}"
    );

    let mut locked = Toolbox::new(dir.path(), true).with_soffice(Some(fake));
    locked.set_permission_mode(PermissionMode::ReadOnly);
    let refused = locked
        .run(CONVERT_DOCUMENT, &json!({"path": "a.docx"}))
        .expect_err("read-only");
    assert!(refused.contains("read-only"), "{refused}");
}

/// A fake `soffice` that writes its own argument list as the "PDF", so the
/// test sees exactly what it was run with.
fn fake_soffice(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let path = dir.join("soffice.cmd");
        std::fs::write(
            &path,
            "@echo off\r\nsetlocal\r\n:loop\r\nif \"%~1\"==\"\" goto done\r\n\
             if \"%~1\"==\"--outdir\" (set \"OUT=%~2\" & shift)\r\n\
             set \"LAST=%~1\"\r\nshift\r\ngoto loop\r\n:done\r\n\
             for %%F in (\"%LAST%\") do set \"STEM=%%~nF\"\r\n\
             echo %* > \"%OUT%\\%STEM%.pdf\"\r\n",
        )
        .expect("fake");
        path
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("soffice");
        std::fs::write(
            &path,
            "#!/bin/sh\nall=\"$*\"\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = --outdir ]; then out=\"$2\"; shift; fi\n  last=\"$1\"; shift\ndone\nstem=$(basename \"$last\"); stem=${stem%.*}\necho \"$all\" > \"$out/$stem.pdf\"\n",
        )
        .expect("fake");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }
}

/// The run itself, against a FAKE soffice: the flags, a private profile,
/// the PDF copied into place. LibreOffice itself is not installed here, so
/// a real conversion is not what this proves.
#[test]
fn convert_document_runs_soffice_headless_with_a_private_profile() {
    let dir = tempfile::tempdir().expect("tmp");
    let tools_dir = tempfile::tempdir().expect("tools");
    std::fs::create_dir_all(dir.path().join("docs")).expect("dir");
    std::fs::write(dir.path().join("docs/report.docx"), "x").expect("seed");
    let fake = fake_soffice(tools_dir.path());
    let tools = Toolbox::new(dir.path(), false).with_soffice(Some(fake));
    let said = tools
        .run(
            CONVERT_DOCUMENT,
            &json!({"path": "docs/report.docx", "output_dir": "out"}),
        )
        .expect("converts");
    assert!(said.contains("Converted"), "{said}");
    let args = std::fs::read_to_string(dir.path().join("out/report.pdf")).expect("the PDF");
    for flag in [
        "--headless",
        "--norestore",
        "--nologo",
        "--nodefault",
        "--nolockcheck",
        "-env:UserInstallation=file:///",
        "--convert-to pdf",
        "--outdir",
    ] {
        assert!(args.contains(flag), "{flag} missing: {args}");
    }
    assert!(
        !dir.path().join("docs/report.pdf").exists(),
        "written to output_dir only"
    );
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// **A MALFORMED CALL RUNS NOTHING**: the file is not written, and every
/// violation is listed with its path.
#[test]
fn an_input_that_breaks_its_schema_runs_nothing() {
    let dir = tempfile::tempdir().expect("tmp");
    let why = toolbox(dir.path())
        .run(WRITE, &json!({"path": "made.txt", "content": 42}))
        .expect_err("content must be a string");
    assert!(why.contains("NOT RUN"), "{why}");
    assert!(why.contains("at `/content`"), "{why}");
    assert!(!dir.path().join("made.txt").exists(), "nothing was written");
}

/// Checked BEFORE the guards and the engine: a malformed call to a
/// protected path gets the schema's answer, and the engine is never started.
#[test]
fn validation_comes_before_the_guards_and_the_engine() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    let bin = fake::write(engine.path(), &[("pre-tool", Reply::exit(0))]);
    let tools =
        toolbox(ws.path()).with_engine(Some(fake::engine(bin).capped(Duration::from_secs(30))));
    let why = tools
        .run(WRITE, &json!({"path": "tests/it.rs"}))
        .expect_err("content missing");
    assert!(why.contains("\"content\" is a required property"), "{why}");
    assert!(
        !why.contains("protected"),
        "the guards were asked first: {why}"
    );
    assert_eq!(
        fake::calls(engine.path(), "pre-tool"),
        0,
        "the engine was started"
    );
}
