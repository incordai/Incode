use super::fixtures::{self, Cell, Slide};
use super::*;

fn written(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("fixture");
    path
}

fn convert(dir: &Path, name: &str, bytes: &[u8]) -> Result<String, String> {
    let path = written(dir, name, bytes);
    let kind = Kind::of(&path).expect("a document");
    to_markdown(&path, kind)
}

#[test]
fn formats_are_known_by_extension_and_source_formats_are_not_converted_by_default() {
    for (name, kind) in [
        ("a.PDF", Kind::Pdf),
        ("a.xlsx", Kind::Xlsx),
        ("a.xls", Kind::Xls),
        ("a.ods", Kind::Ods),
        ("a.csv", Kind::Csv),
        ("a.docx", Kind::Docx),
        ("a.pptx", Kind::Pptx),
        ("a.odt", Kind::Odt),
        ("a.htm", Kind::Html),
    ] {
        assert_eq!(Kind::of(Path::new(name)), Some(kind), "{name}");
    }
    assert_eq!(Kind::of(Path::new("a.rs")), None);
    assert!(Kind::Pdf.converts_by_default());
    assert!(!Kind::Html.converts_by_default());
    assert!(!Kind::Csv.converts_by_default());
}

/// **A PDF's TEXT COMES BACK, PAGE BY PAGE.**
#[test]
fn a_pdf_is_read_page_by_page() {
    let dir = tempfile::tempdir().expect("tmp");
    let pdf = fixtures::pdf(&[
        &["Quarterly Report", "Revenue grew strongly."],
        &["Appendix", "Numbers: 42, 1000"],
    ]);
    let md = convert(dir.path(), "r.pdf", &pdf).expect("reads");
    let one = md.find("## Page 1").expect("page 1");
    let two = md.find("## Page 2").expect("page 2");
    let report = md.find("Quarterly Report").expect("page one's text");
    let appendix = md.find("Appendix").expect("page two's text");
    assert!(one < report && report < two && two < appendix, "{md}");
    assert!(md.contains("Revenue grew strongly."), "{md}");
}

/// **A MALFORMED PDF FAILS THE CALL, NOT THE PROCESS.** The reader panics on
/// some broken files; the panic is caught.
#[test]
fn a_broken_pdf_is_an_error_not_a_crash() {
    let dir = tempfile::tempdir().expect("tmp");
    let pdf = fixtures::pdf(&[&["Some text on a page"]]);
    for (name, bytes) in [
        ("cut.pdf", pdf[..pdf.len() * 6 / 10].to_vec()),
        (
            "junk.pdf",
            b"%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF"
                .to_vec(),
        ),
        ("empty.pdf", Vec::new()),
    ] {
        match convert(dir.path(), name, &bytes) {
            Err(why) => assert!(why.contains("PDF"), "{name}: {why}"),
            Ok(md) => assert!(!md.contains("Some text"), "{name}: {md}"),
        }
    }
}

/// The deadline: a reader past it is abandoned and the call says so.
#[test]
fn a_pdf_reader_past_its_deadline_is_abandoned() {
    let why = pdf_pages(Vec::new(), Duration::from_millis(50), |_| {
        std::thread::sleep(Duration::from_secs(2));
        Ok(Vec::new())
    })
    .expect_err("past the deadline");
    assert!(why.contains("abandoned"), "{why}");
}

/// **A READER THAT PANICS FAILS THE CALL, NOT THE PROCESS**, and says it
/// was the reader. The broken files above do not reliably make the reader
/// panic, so the panic is put there directly.
#[test]
fn a_panicking_pdf_reader_is_caught_and_named() {
    let why = pdf_pages(b"%PDF".to_vec(), PDF_DEADLINE, |_| {
        panic!("a malformed cross-reference table")
    })
    .expect_err("the panic is an error");
    assert!(why.contains("the PDF reader failed on this file"), "{why}");
}

#[test]
fn a_file_over_the_size_bound_is_refused_before_it_is_read() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("huge.pdf");
    let file = std::fs::File::create(&path).expect("create");
    file.set_len(MAX_DOCUMENT_BYTES + 1).expect("sparse");
    let why = to_markdown(&path, Kind::Pdf).expect_err("too large");
    assert!(why.contains("are not read"), "{why}");
}

/// **EVERY SHEET COMES BACK AS A MARKDOWN TABLE.**
#[test]
fn a_workbook_is_one_table_per_sheet() {
    let dir = tempfile::tempdir().expect("tmp");
    let book = fixtures::xlsx(&[
        (
            "Sales",
            vec![
                vec![Cell::Text("Region"), Cell::Text("Q1")],
                vec![Cell::Text("North"), Cell::Number(120.5)],
            ],
        ),
        ("Notes", vec![vec![Cell::Text("Second sheet text")]]),
    ]);
    let md = convert(dir.path(), "b.xlsx", &book).expect("reads");
    assert!(md.contains("## Sheet: Sales (2 rows × 2 columns)"), "{md}");
    assert!(md.contains("| Region | Q1 |"), "{md}");
    assert!(md.contains("| --- | --- |"), "{md}");
    assert!(md.contains("| North | 120.5 |"), "{md}");
    assert!(md.contains("## Sheet: Notes"), "{md}");
    assert!(md.contains("Second sheet text"), "{md}");
}

#[test]
fn a_long_sheet_is_cut_and_the_rest_counted() {
    let dir = tempfile::tempdir().expect("tmp");
    let rows: Vec<Vec<Cell>> = (0..MAX_TABLE_ROWS + 25)
        .map(|n| vec![Cell::Number(n as f64)])
        .collect();
    let md = convert(dir.path(), "long.xlsx", &fixtures::xlsx(&[("Big", rows)])).expect("reads");
    assert!(
        md.contains("… 25 more rows not shown"),
        "{}",
        &md[md.len() - 200..]
    );
    assert!(
        !md.contains(&format!("| {} |", MAX_TABLE_ROWS + 1)),
        "past the bound"
    );
}

#[test]
fn a_file_that_is_not_a_workbook_says_so() {
    let dir = tempfile::tempdir().expect("tmp");
    let why = convert(dir.path(), "fake.xlsx", b"not a zip at all").expect_err("junk");
    assert!(why.contains("not a readable archive"), "{why}");
}

/// **A WORD DOCUMENT KEEPS ITS SHAPE**: heading, runs joined, entity decoded,
/// list item marked, a tab stop in the properties NOT turned into text, and
/// the table as a table with its `|` escaped.
#[test]
fn a_word_document_becomes_structured_markdown() {
    let dir = tempfile::tempdir().expect("tmp");
    let md = convert(dir.path(), "d.docx", &fixtures::docx(fixtures::DOCX_BODY)).expect("reads");
    assert!(md.contains("# Quarterly Report\n"), "{md}");
    assert!(md.contains("Revenue grew strongly & fast."), "{md}");
    assert!(md.contains("- First point\n"), "{md}");
    assert!(!md.contains("\tFirst point"), "{md:?}");
    assert!(md.contains("| Name | Value |"), "{md}");
    assert!(md.contains("| alpha | 4\\|2 |"), "{md}");
}

#[test]
fn a_zip_that_is_not_a_word_document_says_so() {
    let dir = tempfile::tempdir().expect("tmp");
    let not_word = fixtures::zip(&[("readme.txt", b"hello".as_slice())]);
    let why = convert(dir.path(), "x.docx", &not_word).expect_err("no document part");
    assert!(why.contains("word/document.xml is missing"), "{why}");
}

/// **SLIDES IN THE PRESENTATION'S ORDER, EACH WITH ITS SPEAKER NOTES** —
/// and only the notes body, not the slide-number placeholder.
#[test]
fn a_presentation_is_slides_in_order_with_their_notes() {
    let dir = tempfile::tempdir().expect("tmp");
    let deck = fixtures::pptx(
        &[
            Slide {
                title: "Risks",
                body: "Binary size growth",
                notes: Some("Speaker note here"),
            },
            Slide {
                title: "Roadmap 2027",
                body: "Ship the reader",
                notes: None,
            },
        ],
        // The presentation shows slide2.xml FIRST.
        &[2, 1],
    );
    let md = convert(dir.path(), "p.pptx", &deck).expect("reads");
    let roadmap = md.find("### Roadmap 2027").expect("title as heading");
    let risks = md.find("### Risks").expect("second slide");
    assert!(roadmap < risks, "presentation order, not file order: {md}");
    assert!(md.contains("Ship the reader"), "{md}");
    assert!(
        md.contains("**Speaker notes:**\n\n> Speaker note here"),
        "{md}"
    );
    assert!(!md.contains("> 1"), "the slide number is not a note: {md}");
    assert_eq!(md.matches("Speaker notes").count(), 1, "{md}");
}

#[test]
fn an_opendocument_text_becomes_markdown() {
    let dir = tempfile::tempdir().expect("tmp");
    let md = convert(dir.path(), "o.odt", &fixtures::odt()).expect("reads");
    assert!(md.contains("## ODT Title"), "{md}");
    assert!(md.contains("Hello from OpenDocument  text."), "{md}");
    assert!(md.contains("- item one"), "{md}");
    assert!(md.contains("| Item | Cost |"), "{md}");
    assert!(md.contains("| Disk | 79.5 |"), "{md}");
}

/// **A ZIP BOMB IS REFUSED ON WHAT IT INFLATES TO**, not what it claims.
#[test]
fn an_archive_inflating_past_its_budget_is_refused() {
    let zeros = vec![0u8; 2 * 1024 * 1024];
    let bytes = fixtures::zip(&[("word/document.xml", zeros.as_slice())]);
    assert!(bytes.len() < 64 * 1024, "the fixture really is compressed");
    // The stated size is honest here, so the stated check catches it...
    let why = Archive::bounded(&bytes, 10, 1024 * 1024)
        .err()
        .expect("stated size over budget");
    assert!(why.contains("zip bomb"), "{why}");
    // ...and with the stated check passed, inflation still stops at the budget.
    let mut archive = Archive::bounded(&bytes, 10, 3 * 1024 * 1024).expect("opens");
    archive.budget = 1024 * 1024;
    // Not `expect_err`: a failure must not print two MiB of zeros.
    let why = match archive.bytes("word/document.xml") {
        Err(why) => why,
        Ok(_) => panic!("inflated past the budget"),
    };
    assert!(why.contains("zip bomb"), "{why}");
    let mut archive = Archive::bounded(&bytes, 10, 3 * 1024 * 1024).expect("opens");
    archive.budget = 1024 * 1024;
    assert!(archive.inflate_all().is_err_and(|w| w.contains("zip bomb")));
}

/// A size field that LIES is caught by the production bound on stated size.
#[test]
fn an_archive_claiming_a_huge_size_is_refused_by_the_production_bound() {
    let dir = tempfile::tempdir().expect("tmp");
    let mut bytes = fixtures::docx(fixtures::DOCX_BODY);
    // The central directory's uncompressed-size field sits 24 bytes into each
    // record (signature 50 4B 01 02).
    let at = bytes
        .windows(4)
        .position(|w| w == [0x50, 0x4B, 0x01, 0x02])
        .expect("central directory");
    bytes[at + 24..at + 28].copy_from_slice(&0xF000_0000u32.to_le_bytes());
    let why = convert(dir.path(), "liar.docx", &bytes).expect_err("claims 3.75 GiB");
    assert!(why.contains("zip bomb"), "{why}");
}

#[test]
fn an_archive_with_too_many_entries_is_refused() {
    let names: Vec<String> = (0..12).map(|n| format!("f{n}")).collect();
    let entries: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), b"".as_slice())).collect();
    let bytes = fixtures::zip(&entries);
    let why = Archive::bounded(&bytes, 10, 1024).err().expect("too many");
    assert!(why.contains("12 entries"), "{why}");
}

#[test]
fn csv_becomes_a_table_with_quoted_fields_kept_whole() {
    let md = delimited_to_markdown(
        "name,qty\n\"Widget, large\",3\n\"say \"\"hi\"\"\",7\n",
        b',',
    );
    assert!(md.contains("| name | qty |"), "{md}");
    assert!(md.contains("| Widget, large | 3 |"), "{md}");
    assert!(md.contains("| say \"hi\" | 7 |"), "{md}");
    let tsv = delimited_to_markdown("a\tb\n1\t2\n", b'\t');
    assert!(tsv.contains("| 1 | 2 |"), "{tsv}");
}

#[test]
fn a_long_csv_is_cut_and_counted() {
    let body: String = std::iter::once("n\n".to_string())
        .chain((0..MAX_TABLE_ROWS + 10).map(|n| format!("{n}\n")))
        .collect();
    let md = delimited_to_markdown(&body, b',');
    assert!(
        md.contains("… 11 more rows not shown"),
        "{}",
        &md[md.len() - 100..]
    );
}

const ARTICLE: &str = r#"<!doctype html><html><head><title>How Ownership Works | DevBlog</title></head><body>
<nav class="site-nav"><a href="/">Home</a> | <a href="/blog">Blog</a></nav>
<div class="ad-banner sponsored"><a href="https://ads.example/click">BUY CHEAP HOSTING NOW</a></div>
<main><article><h1>How Ownership Works</h1>
<p>Ownership is the most distinctive feature of the language. Every value has a single <strong>owner</strong>, and when the owner goes out of scope the value is dropped. This guarantees memory safety without a garbage collector, which is one of the reasons it is chosen for systems work.</p>
<h2>Borrowing</h2><p>Instead of transferring ownership you can <em>borrow</em> a value by reference. The borrow checker enforces that you have either one mutable reference or any number of immutable references at a time, and it does so at compile time.</p>
<ul><li>Each value has one owner</li><li>Ownership can move</li></ul>
<p>With these rules in mind, most compile errors about lifetimes become much easier to read and fix, and you will find yourself writing safer code by default across your whole project.</p></article></main>
<footer><p>Copyright 2026 DevBlog.</p></footer><script>trackUser();</script></body></html>"#;

/// **AN ARTICLE COMES BACK AS ITS MARKDOWN**, without the navigation, the
/// advert or the script.
#[test]
fn an_html_article_is_read_as_markdown() {
    let md = html_to_markdown(ARTICLE, Some("https://example.com/post"));
    assert!(md.contains("Borrowing"), "{md}");
    assert!(md.contains("**owner**"), "{md}");
    assert!(md.contains("Each value has one owner"), "{md}");
    for noise in ["BUY CHEAP HOSTING", "trackUser", "Copyright 2026"] {
        assert!(!md.contains(noise), "{noise} kept: {md}");
    }
}

/// A page that is not an article still comes back, as the whole page.
#[test]
fn a_page_that_is_not_an_article_falls_back_to_the_whole_page() {
    let page = "<html><body><nav><a href='/'>Home</a></nav><h1>API index</h1><ul><li><code>read()</code></li><li><code>write()</code></li></ul><script>x()</script></body></html>";
    let md = html_to_markdown(page, None);
    assert!(md.contains("# API index"), "{md}");
    assert!(md.contains("read()"), "{md}");
    assert!(!md.contains("x()"), "{md}");
    assert!(!md.contains("Home"), "{md}");
}

#[test]
fn a_page_is_numbered_bounded_and_says_how_to_read_on() {
    let markdown: String = (0..5000)
        .map(|n| format!("line {n} {}\n", "x".repeat(40)))
        .collect();
    let shown = page(Path::new("big.pdf"), Kind::Pdf, &markdown, 0, None);
    assert!(
        shown.len() <= MAX_DOCUMENT_READ_CHARS + 200,
        "{}",
        shown.len()
    );
    assert!(
        shown.contains("read on with offset="),
        "{}",
        &shown[shown.len() - 200..]
    );
    let later = page(Path::new("big.pdf"), Kind::Pdf, &markdown, 4990, Some(5));
    assert!(later.contains("  4991\tline 4990"), "{later}");
    assert!(!later.contains("read on"), "{later}");
}
