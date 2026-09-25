//! Documents read as markdown: PDF, spreadsheets, Word, PowerPoint,
//! OpenDocument text, CSV and HTML.
//!
//! # Why Read converts
//!
//! A model cannot read a `.docx`. Handed the bytes it sees a zip archive; told
//! the file is binary it has nothing. Markdown is what it reads best, so a
//! document is turned into markdown here, once, and paged exactly like a text
//! file.
//!
//! Text formats a person also EDITS — HTML, CSV — are left as source by
//! default, because Edit needs the exact text; `render` asks for the markdown.
//!
//! # Everything is bounded, because every input is hostile
//!
//! The file is size-checked before it is read. An archive is refused when it
//! holds too many entries or expands past its budget — counted on the bytes
//! actually inflated, not the sizes the archive claims, since those are
//! exactly what a zip bomb lies about. The PDF reader can panic on a
//! malformed file, so every call to it is caught and run on its own thread
//! with a deadline. What is shown is cut at a stated bound, and the rest is
//! counted.

use std::io::{Cursor, Read};
use std::path::Path;
use std::time::Duration;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

/// The largest document file read at all.
pub const MAX_DOCUMENT_BYTES: u64 = 32 * 1024 * 1024;
/// The most bytes an archive may inflate to, across all its entries.
pub const MAX_UNZIPPED_BYTES: u64 = 96 * 1024 * 1024;
/// The most entries an archive may hold.
pub const MAX_ZIP_ENTRIES: usize = 10_000;
/// The most rows shown per sheet or CSV table; the rest are counted.
pub const MAX_TABLE_ROWS: usize = 200;
/// The most columns shown per table; the rest are counted.
pub const MAX_TABLE_COLUMNS: usize = 40;
/// The most sheets read from one workbook.
pub const MAX_SHEETS: usize = 30;
/// The most slides read from one presentation.
pub const MAX_SLIDES: usize = 500;
/// The most characters of markdown one Read returns; page on with `offset`.
pub const MAX_DOCUMENT_READ_CHARS: usize = 60_000;
/// The largest HTML parsed, in bytes; a longer page is cut first.
pub const MAX_HTML_BYTES: usize = 8 * 1024 * 1024;
/// How long the PDF reader may run before it is abandoned.
const PDF_DEADLINE: Duration = Duration::from_secs(60);
/// A cell longer than this is cut.
const MAX_CELL_CHARS: usize = 200;

/// A document format Read knows how to turn into markdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Pdf,
    Xlsx,
    Xlsb,
    Xls,
    Ods,
    Csv,
    Tsv,
    Docx,
    Pptx,
    Odt,
    Html,
}

impl Kind {
    /// The format `path` names by its extension, if it is a document.
    pub fn of(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
        Some(match ext.as_str() {
            "pdf" => Self::Pdf,
            "xlsx" | "xlsm" => Self::Xlsx,
            "xlsb" => Self::Xlsb,
            "xls" => Self::Xls,
            "ods" => Self::Ods,
            "csv" => Self::Csv,
            "tsv" => Self::Tsv,
            "docx" => Self::Docx,
            "pptx" => Self::Pptx,
            "odt" => Self::Odt,
            "html" | "htm" | "xhtml" => Self::Html,
            _ => return None,
        })
    }

    /// Whether Read converts it without being asked. A format that is also
    /// source someone edits comes back as its text unless `render` is set.
    pub fn converts_by_default(self) -> bool {
        !matches!(self, Self::Csv | Self::Tsv | Self::Html)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Pdf => "PDF",
            Self::Xlsx | Self::Xlsb | Self::Xls | Self::Ods => "spreadsheet",
            Self::Csv => "CSV",
            Self::Tsv => "TSV",
            Self::Docx => "Word document",
            Self::Pptx => "presentation",
            Self::Odt => "OpenDocument text",
            Self::Html => "HTML page",
        }
    }
}

/// The document at `path` as markdown, whole. See [`page`] for what a model
/// is shown of it.
pub fn to_markdown(path: &Path, kind: Kind) -> Result<String, String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if size > MAX_DOCUMENT_BYTES {
        return Err(format!(
            "{} is {} MiB; documents over {} MiB are not read",
            path.display(),
            size / (1024 * 1024),
            MAX_DOCUMENT_BYTES / (1024 * 1024)
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    match kind {
        Kind::Pdf => pdf_to_markdown(bytes),
        Kind::Xlsx | Kind::Xlsb | Kind::Xls | Kind::Ods => sheets_to_markdown(&bytes, kind),
        Kind::Csv => Ok(delimited_to_markdown(
            &String::from_utf8_lossy(&bytes),
            b',',
        )),
        Kind::Tsv => Ok(delimited_to_markdown(
            &String::from_utf8_lossy(&bytes),
            b'\t',
        )),
        Kind::Docx => docx_to_markdown(&bytes),
        Kind::Pptx => pptx_to_markdown(&bytes),
        Kind::Odt => odt_to_markdown(&bytes),
        Kind::Html => Ok(html_to_markdown(&String::from_utf8_lossy(&bytes), None)),
    }
}

/// What a model is shown of a converted document: numbered lines from
/// `offset`, at most `limit` of them, cut at [`MAX_DOCUMENT_READ_CHARS`] with
/// the way on stated.
pub fn page(
    path: &Path,
    kind: Kind,
    markdown: &str,
    offset: usize,
    limit: Option<usize>,
) -> String {
    let lines: Vec<&str> = markdown.lines().collect();
    let total = lines.len();
    let start = offset.min(total);
    let end = limit.map_or(total, |l| start.saturating_add(l)).min(total);
    let mut out = format!(
        "[{} {}, read as markdown: {total} lines]\n",
        kind.label(),
        path.display()
    );
    let mut shown_to = start;
    for (i, line) in lines[start..end].iter().enumerate() {
        let numbered = format!("{:>6}\t{}\n", start + i + 1, line);
        if out.len() + numbered.len() > MAX_DOCUMENT_READ_CHARS {
            break;
        }
        out.push_str(&numbered);
        shown_to = start + i + 1;
    }
    if shown_to < end {
        out.push_str(&format!(
            "[… shown lines {}–{shown_to} of {total}; read on with offset={shown_to}]\n",
            start + 1
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// PDF
// ---------------------------------------------------------------------------

/// A PDF's text, page by page. The reader runs on its own thread, under
/// `catch_unwind`, with a deadline: it can panic on a malformed file and can
/// take very long on a pathological one.
fn pdf_to_markdown(bytes: Vec<u8>) -> Result<String, String> {
    let pages = pdf_pages(bytes, PDF_DEADLINE, |pdf| {
        pdf_extract::extract_text_from_mem_by_pages(pdf).map_err(|e| e.to_string())
    })?;
    let mut out = String::new();
    let mut any_text = false;
    for (i, text) in pages.iter().enumerate() {
        let text = tidy_blank_lines(text);
        any_text |= !text.is_empty();
        out.push_str(&format!("## Page {}\n\n{text}\n\n", i + 1));
    }
    if !any_text {
        out.push_str(
            "(No text layer was found: the PDF may be scanned images, which this reader does \
             not recognise.)\n",
        );
    }
    Ok(out)
}

/// The text of every page as `extract` reads it, or why there is none.
/// `extract` is the PDF reader; a test hands in one that panics.
fn pdf_pages<F>(bytes: Vec<u8>, deadline: Duration, extract: F) -> Result<Vec<String>, String>
where
    F: FnOnce(&[u8]) -> Result<Vec<String>, String> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("pdf-reader".to_string())
        .spawn(move || {
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| extract(&bytes)));
            let _ = tx.send(outcome);
        })
        .map_err(|e| format!("could not start the PDF reader: {e}"))?;
    match rx.recv_timeout(deadline) {
        Ok(Ok(Ok(pages))) => Ok(pages),
        Ok(Ok(Err(e))) => Err(format!("the PDF could not be read: {e}")),
        Ok(Err(_)) => Err(
            "the PDF reader failed on this file (it is malformed, or uses a feature the reader \
             does not support); nothing was extracted"
                .to_string(),
        ),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "the PDF reader was still working after {} s and was abandoned",
            deadline.as_secs()
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err("the PDF reader stopped without an answer".to_string())
        }
    }
}

/// Runs of blank lines collapsed to one, ends trimmed.
fn tidy_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0usize;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank == 1 && !out.is_empty() {
                out.push('\n');
            }
            continue;
        }
        blank = 0;
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// Archives
// ---------------------------------------------------------------------------

/// An archive opened for reading, with its inflation budget.
struct Archive<'a> {
    zip: zip::ZipArchive<Cursor<&'a [u8]>>,
    budget: u64,
}

impl<'a> Archive<'a> {
    /// Open `bytes` as a zip under the production bounds.
    fn open(bytes: &'a [u8]) -> Result<Self, String> {
        Self::bounded(bytes, MAX_ZIP_ENTRIES, MAX_UNZIPPED_BYTES)
    }

    /// Open `bytes` as a zip, refusing one with more than `max_entries`
    /// entries or whose stated sizes already exceed `budget`.
    fn bounded(bytes: &'a [u8], max_entries: usize, budget: u64) -> Result<Self, String> {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| format!("not a readable archive: {e}"))?;
        if zip.len() > max_entries {
            return Err(format!(
                "the archive holds {} entries; more than {max_entries} is refused",
                zip.len()
            ));
        }
        let mut stated = 0u64;
        for i in 0..zip.len() {
            if let Ok(entry) = zip.by_index_raw(i) {
                stated = stated.saturating_add(entry.size());
            }
        }
        if stated > budget {
            return Err(bomb(budget, stated));
        }
        Ok(Self { zip, budget })
    }

    /// One entry's bytes, or `None` when there is no such entry. Inflation
    /// is stopped at the remaining budget, whatever the entry claims.
    fn bytes(&mut self, name: &str) -> Result<Option<Vec<u8>>, String> {
        let limit = self.budget;
        let entry = match self.zip.by_name(name) {
            Ok(entry) => entry,
            Err(zip::result::ZipError::FileNotFound) => return Ok(None),
            Err(e) => return Err(format!("{name}: {e}")),
        };
        let mut out = Vec::new();
        entry
            .take(limit.saturating_add(1))
            .read_to_end(&mut out)
            .map_err(|e| format!("{name}: {e}"))?;
        let used = out.len() as u64;
        if used > limit {
            return Err(bomb(limit, used));
        }
        self.budget -= used;
        Ok(Some(out))
    }

    /// One entry as text, lossily decoded.
    fn text(&mut self, name: &str) -> Result<Option<String>, String> {
        Ok(self
            .bytes(name)?
            .map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    /// Inflate every entry against the budget and throw the bytes away:
    /// the check run before a reader that does its own unzipping.
    fn inflate_all(&mut self) -> Result<(), String> {
        for i in 0..self.zip.len() {
            let limit = self.budget;
            let entry = self
                .zip
                .by_index(i)
                .map_err(|e| format!("archive entry {i}: {e}"))?;
            let used = std::io::copy(
                &mut entry.take(limit.saturating_add(1)),
                &mut std::io::sink(),
            )
            .map_err(|e| format!("archive entry {i}: {e}"))?;
            if used > limit {
                return Err(bomb(limit, used));
            }
            self.budget -= used;
        }
        Ok(())
    }

    fn names(&self) -> Vec<String> {
        self.zip.file_names().map(str::to_string).collect()
    }
}

/// Why an archive past its budget is refused. `seen` is what was stated or
/// inflated before stopping, so it is a floor, not the archive's real size.
fn bomb(budget: u64, seen: u64) -> String {
    format!(
        "the archive expands past its {} KiB budget ({} KiB seen before stopping); refused as a \
         possible zip bomb",
        budget / 1024,
        seen / 1024
    )
}

// ---------------------------------------------------------------------------
// Spreadsheets and delimited text
// ---------------------------------------------------------------------------

/// Every sheet as a markdown table, bounded in sheets, rows and columns.
fn sheets_to_markdown(bytes: &[u8], kind: Kind) -> Result<String, String> {
    if matches!(kind, Kind::Xlsx | Kind::Xlsb | Kind::Ods) {
        Archive::open(bytes)?.inflate_all()?;
    }
    // Caught: a malformed workbook must fail this call, not the session.
    std::panic::catch_unwind(|| read_sheets(bytes, kind)).unwrap_or_else(|_| {
        Err("the spreadsheet reader failed on this file; nothing was extracted".to_string())
    })
}

fn read_sheets(bytes: &[u8], kind: Kind) -> Result<String, String> {
    use calamine::Reader as _;
    let cursor = Cursor::new(bytes);
    let fail = |e: &dyn std::fmt::Display| format!("the spreadsheet could not be read: {e}");
    let mut tables: Vec<(String, Result<calamine::Range<calamine::Data>, String>)> = Vec::new();
    macro_rules! collect {
        ($book:ty) => {{
            let mut book: $book = calamine::open_workbook_from_rs(cursor).map_err(|e| fail(&e))?;
            for name in book.sheet_names().into_iter().take(MAX_SHEETS + 1) {
                let range = book.worksheet_range(&name).map_err(|e| e.to_string());
                tables.push((name, range));
            }
        }};
    }
    match kind {
        Kind::Xlsx => collect!(calamine::Xlsx<_>),
        Kind::Xlsb => collect!(calamine::Xlsb<_>),
        Kind::Xls => collect!(calamine::Xls<_>),
        _ => collect!(calamine::Ods<_>),
    }
    let mut out = String::new();
    for (name, range) in tables.iter().take(MAX_SHEETS) {
        match range {
            Ok(range) => {
                let (height, width) = range.get_size();
                out.push_str(&format!(
                    "## Sheet: {name} ({height} rows × {width} columns)\n\n"
                ));
                let rows: Vec<Vec<String>> = range
                    .rows()
                    .take(MAX_TABLE_ROWS + 1)
                    .map(|row| row.iter().map(|c| c.to_string()).collect())
                    .collect();
                out.push_str(&markdown_table(&rows, height, width));
            }
            Err(why) => out.push_str(&format!("## Sheet: {name}\n\n(not readable: {why})\n\n")),
        }
    }
    if tables.len() > MAX_SHEETS {
        out.push_str(&format!("(only the first {MAX_SHEETS} sheets are shown)\n"));
    }
    if out.is_empty() {
        out.push_str("(the workbook has no sheets)\n");
    }
    Ok(out)
}

/// `rows` as a markdown table, the first row its header. `height` and
/// `width` are the table's whole size, so what was cut can be counted.
fn markdown_table(rows: &[Vec<String>], height: usize, width: usize) -> String {
    if rows.is_empty() || width == 0 {
        return "(empty)\n\n".to_string();
    }
    let columns = width.min(MAX_TABLE_COLUMNS);
    let cell = |row: &Vec<String>, c: usize| {
        let raw = row.get(c).map_or("", String::as_str);
        let flat = raw.replace(['\r', '\n'], " ").replace('|', "\\|");
        crate::text::truncate(flat.trim(), MAX_CELL_CHARS)
    };
    let line = |row: &Vec<String>| {
        let cells: Vec<String> = (0..columns).map(|c| cell(row, c)).collect();
        format!("| {} |\n", cells.join(" | "))
    };
    let mut out = line(&rows[0]);
    out.push_str(&format!("|{}\n", " --- |".repeat(columns)));
    for row in rows.iter().skip(1).take(MAX_TABLE_ROWS.saturating_sub(1)) {
        out.push_str(&line(row));
    }
    if height > MAX_TABLE_ROWS {
        out.push_str(&format!(
            "\n(… {} more rows not shown)\n",
            height - MAX_TABLE_ROWS
        ));
    }
    if width > MAX_TABLE_COLUMNS {
        out.push_str(&format!(
            "\n(… {} more columns not shown)\n",
            width - MAX_TABLE_COLUMNS
        ));
    }
    out.push('\n');
    out
}

/// CSV or TSV as a markdown table. Quoted fields may hold the delimiter, a
/// doubled quote, or a line break.
fn delimited_to_markdown(text: &str, delimiter: u8) -> String {
    let rows = parse_delimited(text, char::from(delimiter), MAX_TABLE_ROWS + 1);
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let height = count_records(text, char::from(delimiter));
    markdown_table(&rows, height, width)
}

/// At most `max` records of delimited text.
fn parse_delimited(text: &str, delimiter: char, max: usize) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            c if c == delimiter => row.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                if rows.len() >= max {
                    return rows;
                }
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// How many records the text holds, counted without keeping them.
fn count_records(text: &str, delimiter: char) -> usize {
    let mut quoted = false;
    let mut records = 0usize;
    let mut pending = false;
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                pending = true;
            }
            '\n' if !quoted => {
                records += 1;
                pending = false;
            }
            '\r' => {}
            c if c == delimiter || !c.is_whitespace() => pending = true,
            _ => {}
        }
    }
    records + usize::from(pending)
}

// ---------------------------------------------------------------------------
// Word, PowerPoint and OpenDocument text
// ---------------------------------------------------------------------------

/// Which vocabulary an XML part speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// `word/document.xml`: `w:p`, `w:t`, `w:tbl`.
    Word,
    /// A slide: `a:p`, `a:t`, `a:tbl`; a title placeholder is a heading.
    Slide,
    /// A notes page: only its body placeholder's text.
    Notes,
    /// `content.xml` of an OpenDocument text: `text:p`, `text:h`, `table:table`.
    Odf,
}

/// A table being read, row by row.
#[derive(Default)]
struct TableAcc {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
}

/// The walker's state over one XML part.
struct Walk {
    dialect: Dialect,
    out: String,
    line: String,
    text_depth: usize,
    heading: Option<usize>,
    bullet: bool,
    tables: Vec<TableAcc>,
    cells: usize,
    /// A slide or notes shape: whether it is a title, and whether it is a
    /// notes body.
    shape_title: bool,
    shape_body: bool,
}

impl Walk {
    fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            out: String::new(),
            line: String::new(),
            text_depth: 0,
            heading: None,
            bullet: false,
            tables: Vec::new(),
            cells: 0,
            shape_title: false,
            shape_body: false,
        }
    }

    fn is_text(&self, name: &[u8]) -> bool {
        match self.dialect {
            Dialect::Word | Dialect::Slide | Dialect::Notes => name == b"t",
            Dialect::Odf => name == b"p" || name == b"h",
        }
    }

    fn ends_paragraph(&self, name: &[u8]) -> bool {
        match self.dialect {
            Dialect::Odf => name == b"p" || name == b"h",
            _ => name == b"p",
        }
    }

    fn is_table(&self, name: &[u8]) -> bool {
        match self.dialect {
            Dialect::Odf => name == b"table",
            _ => name == b"tbl",
        }
    }

    fn is_row(&self, name: &[u8]) -> bool {
        match self.dialect {
            Dialect::Odf => name == b"table-row",
            _ => name == b"tr",
        }
    }

    fn is_cell(&self, name: &[u8]) -> bool {
        match self.dialect {
            Dialect::Odf => name == b"table-cell" || name == b"covered-table-cell",
            _ => name == b"tc",
        }
    }

    fn open(&mut self, e: &BytesStart<'_>, empty: bool) {
        let local = e.local_name();
        let name = local.as_ref();
        if !empty && self.is_text(name) {
            self.text_depth += 1;
        }
        if !empty && self.is_table(name) {
            self.tables.push(TableAcc::default());
        }
        if !empty && self.is_row(name) {
            if let Some(table) = self.tables.last_mut() {
                table.row.clear();
            }
        }
        if !empty && self.is_cell(name) {
            self.cells += 1;
            if let Some(table) = self.tables.last_mut() {
                table.row.push(String::new());
            }
        }
        match (self.dialect, name) {
            (Dialect::Word, b"pStyle") => {
                if let Some(style) = attribute(e, b"val") {
                    if let Some(level) = style.strip_prefix("Heading") {
                        self.heading = level.parse().ok();
                    } else if style == "Title" {
                        self.heading = Some(1);
                    } else if style == "Subtitle" {
                        self.heading = Some(2);
                    } else if style.starts_with("List") {
                        self.bullet = true;
                    }
                }
            }
            (Dialect::Word, b"numPr") => self.bullet = true,
            // A tab STOP in the paragraph's properties lands here too, before
            // any text; the paragraph is trimmed, so it never shows.
            (Dialect::Word, b"tab") => self.line.push('\t'),
            (Dialect::Word, b"br" | b"cr") => self.line.push(' '),
            (Dialect::Slide | Dialect::Notes, b"sp") if !empty => {
                self.shape_title = false;
                self.shape_body = false;
            }
            (Dialect::Slide | Dialect::Notes, b"ph") => {
                let kind = attribute(e, b"type").unwrap_or_default();
                self.shape_title = kind == "title" || kind == "ctrTitle";
                // A placeholder with no type is the body, by the format's rule.
                self.shape_body = kind.is_empty() || kind == "body";
            }
            (Dialect::Slide | Dialect::Notes, b"br") => self.line.push(' '),
            (Dialect::Odf, b"h") if !empty => {
                let level = attribute(e, b"outline-level")
                    .and_then(|l| l.parse().ok())
                    .unwrap_or(1);
                self.heading = Some(level);
            }
            (Dialect::Odf, b"list-item") => self.bullet = true,
            (Dialect::Odf, b"s") => {
                let count = attribute(e, b"c")
                    .and_then(|c| c.parse::<usize>().ok())
                    .unwrap_or(1)
                    .min(64);
                self.line.push_str(&" ".repeat(count));
            }
            (Dialect::Odf, b"tab") => self.line.push('\t'),
            (Dialect::Odf, b"line-break") => self.line.push(' '),
            _ => {}
        }
    }

    fn close(&mut self, name: &[u8]) {
        if self.is_text(name) {
            self.text_depth = self.text_depth.saturating_sub(1);
        }
        if self.ends_paragraph(name) {
            self.flush();
        }
        if self.is_cell(name) {
            self.cells = self.cells.saturating_sub(1);
        }
        if self.is_row(name) {
            if let Some(table) = self.tables.last_mut() {
                let row = std::mem::take(&mut table.row);
                table.rows.push(row);
            }
        }
        if self.is_table(name) {
            if let Some(table) = self.tables.pop() {
                let width = table.rows.iter().map(Vec::len).max().unwrap_or(0);
                if self.tables.is_empty() {
                    let height = table.rows.len();
                    self.out
                        .push_str(&markdown_table(&table.rows, height, width));
                } else {
                    // A table inside a cell is flattened into that cell.
                    let flat: Vec<String> = table.rows.iter().map(|r| r.join(" / ")).collect();
                    self.append_to_cell(&flat.join("; "));
                }
            }
        }
        if matches!(self.dialect, Dialect::Slide | Dialect::Notes) && name == b"sp" {
            self.shape_title = false;
            self.shape_body = false;
        }
    }

    fn append_to_cell(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(cell) = self.tables.last_mut().and_then(|t| t.row.last_mut()) {
            if !cell.is_empty() {
                cell.push(' ');
            }
            cell.push_str(text);
        }
    }

    /// The paragraph just ended, into a cell or onto the page.
    fn flush(&mut self) {
        let text = std::mem::take(&mut self.line).trim().to_string();
        let heading = self.heading.take();
        let bullet = std::mem::take(&mut self.bullet);
        if self.dialect == Dialect::Notes && !self.shape_body {
            return;
        }
        if self.cells > 0 && !self.tables.is_empty() {
            self.append_to_cell(&text);
            return;
        }
        if text.is_empty() {
            return;
        }
        if let Some(level) = heading {
            self.out.push_str(&"#".repeat(level.clamp(1, 6)));
            self.out.push(' ');
        } else if self.dialect == Dialect::Slide && self.shape_title {
            self.out.push_str("### ");
        } else if bullet {
            self.out.push_str("- ");
        }
        self.out.push_str(&text);
        self.out.push_str("\n\n");
    }
}

/// An attribute's value by local name.
fn attribute(e: &BytesStart<'_>, local: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == local)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

/// One XML part of an office document as markdown.
fn xml_to_markdown(xml: &str, dialect: Dialect) -> Result<String, String> {
    let mut reader = Reader::from_str(xml);
    let mut walk = Walk::new(dialect);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => walk.open(&e, false),
            Ok(Event::Empty(e)) => walk.open(&e, true),
            Ok(Event::End(e)) => walk.close(e.local_name().as_ref()),
            Ok(Event::Text(t)) if walk.text_depth > 0 => {
                if let Ok(text) = t.xml10_content() {
                    walk.line.push_str(&text);
                }
            }
            Ok(Event::CData(t)) if walk.text_depth > 0 => {
                walk.line.push_str(&String::from_utf8_lossy(&t));
            }
            Ok(Event::GeneralRef(r)) if walk.text_depth > 0 => {
                if let Ok(Some(c)) = r.resolve_char_ref() {
                    walk.line.push(c);
                } else if let Ok(name) = r.decode() {
                    if let Some(s) = quick_xml::escape::resolve_predefined_entity(&name) {
                        walk.line.push_str(s);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(format!(
                    "the document's XML is malformed near byte {}: {e}",
                    reader.buffer_position()
                ))
            }
            _ => {}
        }
    }
    Ok(walk.out)
}

fn docx_to_markdown(bytes: &[u8]) -> Result<String, String> {
    let mut archive = Archive::open(bytes)?;
    let xml = archive
        .text("word/document.xml")?
        .ok_or("not a Word document: word/document.xml is missing")?;
    xml_to_markdown(&xml, Dialect::Word)
}

fn odt_to_markdown(bytes: &[u8]) -> Result<String, String> {
    let mut archive = Archive::open(bytes)?;
    let xml = archive
        .text("content.xml")?
        .ok_or("not an OpenDocument file: content.xml is missing")?;
    xml_to_markdown(&xml, Dialect::Odf)
}

/// A relationship file's entries: `(id, type, target)`.
fn relationships(xml: &str) -> Vec<(String, String, String)> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == b"Relationship" => {
                out.push((
                    attribute(&e, b"Id").unwrap_or_default(),
                    attribute(&e, b"Type").unwrap_or_default(),
                    attribute(&e, b"Target").unwrap_or_default(),
                ));
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// `target` resolved against the archive directory `dir`, as a relationship
/// names it: relative with `..`, or absolute from the archive's root.
fn resolve_part(dir: &str, target: &str) -> String {
    if let Some(absolute) = target.strip_prefix('/') {
        return absolute.to_string();
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for piece in target.split('/') {
        match piece {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// The slides in presentation order: from the presentation's own list when
/// it can be read, else by the number in each slide's name.
fn slide_order(archive: &mut Archive<'_>) -> Result<Vec<String>, String> {
    let listed = (|| -> Result<Option<Vec<String>>, String> {
        let Some(presentation) = archive.text("ppt/presentation.xml")? else {
            return Ok(None);
        };
        let Some(rels) = archive.text("ppt/_rels/presentation.xml.rels")? else {
            return Ok(None);
        };
        let targets = relationships(&rels);
        let mut reader = Reader::from_str(&presentation);
        let mut order = Vec::new();
        loop {
            match reader.read_event() {
                Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == b"sldId" => {
                    // The relationship id is the PREFIXED `id` (`r:id`); the
                    // bare `id` beside it is a number.
                    let rid = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.local_name().as_ref() == b"id" && a.key.prefix().is_some())
                        .map(|a| String::from_utf8_lossy(&a.value).into_owned());
                    if let Some((_, _, target)) =
                        rid.and_then(|rid| targets.iter().find(|(id, _, _)| *id == rid))
                    {
                        order.push(resolve_part("ppt", target));
                    }
                }
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
        Ok((!order.is_empty()).then_some(order))
    })()?;
    if let Some(order) = listed {
        return Ok(order);
    }
    let number = |n: &str| {
        n.strip_prefix("ppt/slides/slide")
            .and_then(|s| s.strip_suffix(".xml"))
            .and_then(|s| s.parse::<u32>().ok())
    };
    let mut slides: Vec<(u32, String)> = archive
        .names()
        .into_iter()
        .filter_map(|n| number(&n).map(|i| (i, n)))
        .collect();
    slides.sort();
    Ok(slides.into_iter().map(|(_, n)| n).collect())
}

/// Every slide's text, in order, each with its speaker notes.
fn pptx_to_markdown(bytes: &[u8]) -> Result<String, String> {
    let mut archive = Archive::open(bytes)?;
    let order = slide_order(&mut archive)?;
    if order.is_empty() {
        return Err("not a presentation: no slides were found".to_string());
    }
    let mut out = String::new();
    for (i, slide) in order.iter().take(MAX_SLIDES).enumerate() {
        out.push_str(&format!("## Slide {}\n\n", i + 1));
        let Some(xml) = archive.text(slide)? else {
            out.push_str("(this slide's part is missing)\n\n");
            continue;
        };
        out.push_str(&xml_to_markdown(&xml, Dialect::Slide)?);
        if let Some(notes) = notes_for(&mut archive, slide)? {
            if !notes.trim().is_empty() {
                let quoted: Vec<String> = notes
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| format!("> {l}"))
                    .collect();
                out.push_str(&format!(
                    "**Speaker notes:**\n\n{}\n\n",
                    quoted.join("\n>\n")
                ));
            }
        }
    }
    if order.len() > MAX_SLIDES {
        out.push_str(&format!(
            "(… {} more slides not shown)\n",
            order.len() - MAX_SLIDES
        ));
    }
    Ok(out)
}

/// The notes page a slide's relationships point at, as markdown.
fn notes_for(archive: &mut Archive<'_>, slide: &str) -> Result<Option<String>, String> {
    let (dir, file) = slide.rsplit_once('/').unwrap_or(("", slide));
    let Some(rels) = archive.text(&format!("{dir}/_rels/{file}.rels"))? else {
        return Ok(None);
    };
    let Some((_, _, target)) = relationships(&rels)
        .into_iter()
        .find(|(_, kind, _)| kind.ends_with("/notesSlide"))
    else {
        return Ok(None);
    };
    let Some(xml) = archive.text(&resolve_part(dir, &target))? else {
        return Ok(None);
    };
    xml_to_markdown(&xml, Dialect::Notes).map(Some)
}

// ---------------------------------------------------------------------------
// HTML
// ---------------------------------------------------------------------------

/// Tags a whole-page conversion leaves out: they never read as prose.
const SKIPPED_TAGS: &[&str] = &[
    "script", "style", "noscript", "svg", "head", "nav", "footer", "iframe", "form", "button",
];

/// An HTML page as markdown: the article when the page reads as one and
/// readability extracts it, else the whole page with navigation, forms and
/// scripts left out.
///
/// The article test comes first because readability, asked about a page that
/// is not an article, will still pick SOMETHING — measured: an index page came
/// back as its navigation bar.
pub fn html_to_markdown(html: &str, url: Option<&str>) -> String {
    let html = crate::text::head_bytes(html, MAX_HTML_BYTES);
    let document = dom_query::Document::from(html);
    if dom_smoothie::is_probably_readable(&document, None, None) {
        let config = dom_smoothie::Config {
            text_mode: dom_smoothie::TextMode::Markdown,
            ..Default::default()
        };
        let article = dom_smoothie::Readability::with_document(document, url, Some(config))
            .and_then(|mut reader| reader.parse());
        if let Ok(article) = article {
            let body = article.text_content.trim();
            if !body.is_empty() {
                let title = article.title.trim();
                return if title.is_empty() || body.starts_with('#') {
                    body.to_string()
                } else {
                    format!("# {title}\n\n{body}")
                };
            }
        }
        // Readability took the document; the whole page is read afresh.
        return whole_page(&dom_query::Document::from(html));
    }
    whole_page(&document)
}

/// The whole page as markdown, less what never reads as prose.
fn whole_page(document: &dom_query::Document) -> String {
    tidy_blank_lines(&document.md(Some(SKIPPED_TAGS)))
}

#[cfg(test)]
pub(crate) mod fixtures;

#[cfg(test)]
mod tests;
