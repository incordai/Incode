//! Real document files, built in memory for tests: a PDF, a workbook, a Word
//! document, a presentation and an OpenDocument text. Each is the smallest
//! file its format's reader accepts, written by hand so a test controls every
//! byte it asserts on.

use std::io::Write;

/// A zip archive of `entries`, deflated.
pub(crate) fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in entries {
        writer.start_file(*name, options).expect("entry");
        writer.write_all(body).expect("body");
    }
    writer.finish().expect("finish").into_inner()
}

/// A PDF with one page per inner slice, each line drawn in Helvetica.
pub(crate) fn pdf(pages: &[&[&str]]) -> Vec<u8> {
    let page_count = pages.len();
    // Objects: 1 catalog, 2 pages, 3 font, then a page and its content per page.
    let mut objects: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        String::new(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_string(),
    ];
    let mut kids = Vec::new();
    for (i, lines) in pages.iter().enumerate() {
        let page_id = 4 + 2 * i;
        let content_id = page_id + 1;
        kids.push(format!("{page_id} 0 R"));
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 \
             R >> >> /Contents {content_id} 0 R >>"
        ));
        let mut stream = String::from("BT\n/F1 14 Tf\n72 720 Td\n");
        for (n, line) in lines.iter().enumerate() {
            if n > 0 {
                stream.push_str("0 -20 Td\n");
            }
            let escaped = line
                .replace('\\', "\\\\")
                .replace('(', "\\(")
                .replace(')', "\\)");
            stream.push_str(&format!("({escaped}) Tj\n"));
        }
        stream.push_str("ET\n");
        objects.push(format!(
            "<< /Length {} >>\nstream\n{stream}endstream",
            stream.len()
        ));
    }
    objects[1] = format!(
        "<< /Type /Pages /Kids [{}] /Count {page_count} >>",
        kids.join(" ")
    );

    let mut out = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref_at = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f\r\n");
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n\r\n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// A cell in [`xlsx`]: text or a number.
pub(crate) enum Cell {
    Text(&'static str),
    Number(f64),
}

/// A workbook with one sheet per entry.
pub(crate) fn xlsx(sheets: &[(&str, Vec<Vec<Cell>>)]) -> Vec<u8> {
    let mut workbook_sheets = String::new();
    let mut rels = String::new();
    let mut parts: Vec<(String, String)> = Vec::new();
    for (i, (name, rows)) in sheets.iter().enumerate() {
        let n = i + 1;
        workbook_sheets.push_str(&format!(
            r#"<sheet name="{name}" sheetId="{n}" r:id="rId{n}"/>"#
        ));
        rels.push_str(&format!(
            r#"<Relationship Id="rId{n}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{n}.xml"/>"#
        ));
        let mut data = String::new();
        for (r, row) in rows.iter().enumerate() {
            data.push_str(&format!(r#"<row r="{}">"#, r + 1));
            for (c, cell) in row.iter().enumerate() {
                let reference = format!("{}{}", char::from(b'A' + c as u8), r + 1);
                match cell {
                    Cell::Text(t) => data.push_str(&format!(
                        r#"<c r="{reference}" t="inlineStr"><is><t>{t}</t></is></c>"#
                    )),
                    Cell::Number(v) => {
                        data.push_str(&format!(r#"<c r="{reference}"><v>{v}</v></c>"#));
                    }
                }
            }
            data.push_str("</row>");
        }
        parts.push((
            format!("xl/worksheets/sheet{n}.xml"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{data}</sheetData></worksheet>"#
            ),
        ));
    }
    let content_types = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
    let workbook = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>{workbook_sheets}</sheets></workbook>"#
    );
    let workbook_rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#
    );
    let mut entries: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", content_types.as_bytes()),
        ("_rels/.rels", root_rels.as_bytes()),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", workbook_rels.as_bytes()),
    ];
    for (name, body) in &parts {
        entries.push((name.as_str(), body.as_bytes()));
    }
    zip(&entries)
}

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

/// A Word document whose body is `body` (WordprocessingML, `w:` prefix).
pub(crate) fn docx(body: &str) -> Vec<u8> {
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="{W}"><w:body>{body}</w:body></w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
    zip(&[
        ("[Content_Types].xml", content_types.as_bytes()),
        ("word/document.xml", document.as_bytes()),
    ])
}

/// The Word body the document tests assert on: a heading, a paragraph in
/// three runs with an entity, a list item whose properties carry a tab stop,
/// and a table with a `|` in a cell.
pub(crate) const DOCX_BODY: &str = r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Quarterly Report</w:t></w:r></w:p><w:p><w:r><w:t xml:space="preserve">Revenue grew </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>strongly</w:t></w:r><w:r><w:t xml:space="preserve"> &amp; fast.</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr><w:tabs><w:tab w:val="left" w:pos="720"/></w:tabs></w:pPr><w:r><w:t>First point</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>Name</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Value</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:p><w:r><w:t>alpha</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>4|2</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#;

const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

fn shape(placeholder: &str, text: &str) -> String {
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="s"/><p:cNvSpPr/><p:nvPr>{placeholder}</p:nvPr></p:nvSpPr><p:txBody><a:bodyPr/><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>"#
    )
}

/// One slide of [`pptx`]: its title, body text and speaker notes.
pub(crate) struct Slide {
    pub title: &'static str,
    pub body: &'static str,
    pub notes: Option<&'static str>,
}

/// A presentation. `order` lists the slide files' numbers in the order the
/// presentation shows them, so a test can make that differ from the names.
pub(crate) fn pptx(slides: &[Slide], order: &[usize]) -> Vec<u8> {
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut ids = String::new();
    let mut rels = String::new();
    for (position, n) in order.iter().enumerate() {
        ids.push_str(&format!(
            r#"<p:sldId id="{}" r:id="rId{n}"/>"#,
            256 + position
        ));
    }
    for (i, slide) in slides.iter().enumerate() {
        let n = i + 1;
        rels.push_str(&format!(
            r#"<Relationship Id="rId{n}" Type="{R}/slide" Target="slides/slide{n}.xml"/>"#
        ));
        let tree = format!(
            "{}{}",
            shape(r#"<p:ph type="title"/>"#, slide.title),
            shape(r#"<p:ph idx="1"/>"#, slide.body)
        );
        parts.push((
            format!("ppt/slides/slide{n}.xml"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><p:sld xmlns:p="{P}" xmlns:a="{A}" xmlns:r="{R}"><p:cSld><p:spTree>{tree}</p:spTree></p:cSld></p:sld>"#
            ),
        ));
        if let Some(notes) = slide.notes {
            parts.push((
                format!("ppt/slides/_rels/slide{n}.xml.rels"),
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="{R}/notesSlide" Target="../notesSlides/notesSlide{n}.xml"/></Relationships>"#
                ),
            ));
            // The slide image and the slide number are placeholders too; only
            // the body is the speaker's notes.
            let tree = format!(
                "{}{}{}",
                shape(r#"<p:ph type="sldImg"/>"#, ""),
                shape(r#"<p:ph type="body" idx="1"/>"#, notes),
                shape(r#"<p:ph type="sldNum" idx="5"/>"#, &n.to_string())
            );
            parts.push((
                format!("ppt/notesSlides/notesSlide{n}.xml"),
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?><p:notes xmlns:p="{P}" xmlns:a="{A}" xmlns:r="{R}"><p:cSld><p:spTree>{tree}</p:spTree></p:cSld></p:notes>"#
                ),
            ));
        }
    }
    let presentation = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><p:presentation xmlns:p="{P}" xmlns:r="{R}"><p:sldIdLst>{ids}</p:sldIdLst></p:presentation>"#
    );
    let presentation_rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#
    );
    let mut entries: Vec<(&str, &[u8])> = vec![
        ("ppt/presentation.xml", presentation.as_bytes()),
        (
            "ppt/_rels/presentation.xml.rels",
            presentation_rels.as_bytes(),
        ),
    ];
    for (name, body) in &parts {
        entries.push((name.as_str(), body.as_bytes()));
    }
    zip(&entries)
}

/// An OpenDocument text with a heading, a paragraph with a span and a run of
/// spaces, a list item and a table.
pub(crate) fn odt() -> Vec<u8> {
    let content = r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" office:version="1.2"><office:body><office:text><text:h text:outline-level="2">ODT Title</text:h><text:p>Hello from <text:span>OpenDocument</text:span><text:s text:c="2"/>text.</text:p><text:list><text:list-item><text:p>item one</text:p></text:list-item></text:list><table:table table:name="T"><table:table-row><table:table-cell><text:p>Item</text:p></table:table-cell><table:table-cell><text:p>Cost</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell><text:p>Disk</text:p></table:table-cell><table:table-cell><text:p>79.5</text:p></table:table-cell></table:table-row></table:table></office:text></office:body></office:document-content>"#;
    zip(&[
        (
            "mimetype",
            b"application/vnd.oasis.opendocument.text".as_slice(),
        ),
        ("content.xml", content.as_bytes()),
    ])
}
