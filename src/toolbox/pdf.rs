//! `RenderPdf`: a web page or a workspace HTML file, printed to PDF.
//!
//! # Which program prints it
//!
//! The owner's `incord-browser`, over DevTools `Page.printToPDF`, when its
//! PDFs carry text. Whether they do is MEASURED, once per session, never read
//! off a version: [`super::browser::print_ability`] prints a probe page and
//! reads its text back the way Read reads a PDF. A build from before
//! incordai/incord-browser#11 was fixed printed pictures of the page, with no
//! text to select, search or extract; a build with the fix prints a text
//! layer over the page, laid out again for the paper as a system browser
//! does (measured: the same two Letter pages as Edge for one article, 1864
//! characters extracted where Edge gives 1884).
//!
//! Otherwise a system headless browser that prints to PDF from the command
//! line: Edge or Chrome. Looked for like LibreOffice is: the
//! `INCODE_PDF_BROWSER` override (and ONLY there when it is set), then `PATH`,
//! then each platform's standard install locations.
//!
//! `admin.json` `pdf_renderer` picks: `"auto"` (the default: incord-browser
//! when its PDFs carry text, else the system browser, else incord-browser
//! with a note that its PDF is image-only), `"chromium"` or
//! `"incord-browser"` (only that one). Whenever incord-browser printed a PDF
//! the probe found no text in, the result says so.
//!
//! # How it is run
//!
//! Headless, against a private profile in a fresh temporary directory, the
//! PDF written there and copied into place only when it exists — a failed
//! print leaves nothing half-written in the workspace. A hard deadline ends
//! the whole process tree. The OUTPUT path is what the guards and the
//! checkpoint see.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use super::browser::{self, Browsers, Page, PrintAbility};
use crate::admin::PdfRenderer;

/// Names the system headless browser to print with, overriding the search.
pub const PDF_BROWSER_ENV: &str = "INCODE_PDF_BROWSER";
/// How long one print may run before its process tree is killed.
pub const PDF_DEADLINE: Duration = Duration::from_secs(120);
/// What the result says when incord-browser printed and its probe page came
/// back without text.
const IMAGE_ONLY: &str = "NOTE: printed by incord-browser, whose PDF is IMAGE-ONLY — a probe \
    page it printed this session had no text layer, so this page is pictures and its text \
    cannot be selected, searched or extracted. A build with incordai/incord-browser#11 fixed \
    prints text.";

/// The program a print goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Printer {
    /// A system headless browser.
    Chromium(PathBuf),
    /// The owner's headless browser, whatever its PDFs are.
    IncordBrowser(PathBuf),
    /// `auto` with both installed: the owner's browser when its PDFs carry
    /// text, else the system browser — decided by the session's probe.
    Either { browser: PathBuf, chromium: PathBuf },
}

/// Which printer the setting picks from what is installed, or `None` when
/// the one it names is missing — and then `RenderPdf` is not offered.
pub(super) fn choose(
    setting: PdfRenderer,
    chromium: Option<PathBuf>,
    browser: Option<PathBuf>,
) -> Option<Printer> {
    match setting {
        PdfRenderer::Auto => match (browser, chromium) {
            (Some(browser), Some(chromium)) => Some(Printer::Either { browser, chromium }),
            (Some(browser), None) => Some(Printer::IncordBrowser(browser)),
            (None, chromium) => chromium.map(Printer::Chromium),
        },
        PdfRenderer::Chromium => chromium.map(Printer::Chromium),
        PdfRenderer::IncordBrowser => browser.map(Printer::IncordBrowser),
    }
}

/// Whether `auto` prints with the owner's browser, given what its probe
/// measured: only when its PDFs carry text. A probe that failed says nothing
/// good about the browser, so the system browser prints.
fn owner_first(ability: &Result<PrintAbility, String>) -> bool {
    matches!(ability, Ok(PrintAbility { text: true, .. }))
}

/// The note a print by the owner's browser carries, from its probe: none
/// when its PDFs carry text.
fn owner_note(ability: &Result<PrintAbility, String>) -> Option<String> {
    match ability {
        Ok(PrintAbility { text: true, .. }) => None,
        Ok(_) => Some(IMAGE_ONLY.to_string()),
        Err(why) => Some(format!(
            "NOTE: printed by incord-browser, whose PDFs could not be checked for a text layer \
             this session ({why}); this PDF may be image-only."
        )),
    }
}

/// Where a system headless browser is looked for, in order.
fn candidates(env: &dyn Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    if let Some(named) = env(PDF_BROWSER_ENV).filter(|v| !v.is_empty()) {
        return vec![PathBuf::from(named)];
    }
    let names: &[&str] = if cfg!(windows) {
        &["msedge.exe", "chrome.exe"]
    } else {
        &[
            "microsoft-edge",
            "microsoft-edge-stable",
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
        ]
    };
    let mut out = Vec::new();
    if let Some(path) = env("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in names {
                out.push(dir.join(name));
            }
        }
    }
    if cfg!(windows) {
        // The x86 directory first: it is where this browser installs even on
        // a 64-bit system.
        for var in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
            if let Some(base) = env(var) {
                let base = Path::new(&base);
                out.push(base.join(r"Microsoft\Edge\Application\msedge.exe"));
                out.push(base.join(r"Google\Chrome\Application\chrome.exe"));
            }
        }
    } else if cfg!(target_os = "macos") {
        for fixed in [
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ] {
            out.push(fixed.into());
        }
    } else {
        for fixed in [
            "/usr/bin/microsoft-edge",
            "/usr/bin/google-chrome",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/snap/bin/chromium",
        ] {
            out.push(fixed.into());
        }
    }
    out
}

/// The system headless browser to print with, or `None`. Traced either way.
pub(super) fn find_chromium(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let tried = candidates(env);
    let found = tried.iter().find(|p| p.is_file()).cloned();
    match &found {
        Some(path) => crate::trace!("tool", "PDF browser found at {}", path.display()),
        None => crate::trace!(
            "tool",
            "no PDF browser found ({} places tried)",
            tried.len()
        ),
    }
    found
}

/// The tool as the model is told about it.
pub(super) fn definition() -> Value {
    json!({
        "name": super::RENDER_PDF,
        "description": "Print a web page, or an HTML or SVG file in the workspace, to a PDF file \
            in the workspace, as a browser prints it. Name exactly one of `url` or `path`, and \
            the `output` file (ending in .pdf), which is replaced if it exists. A private or \
            local address is refused. To READ a PDF use Read; to convert an office document \
            use ConvertDocument.",
        "input_schema": {
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "An http:// or https:// address on the public web."},
                "path": {"type": "string", "description": "An .html, .htm, .xhtml or .svg file in the workspace."},
                "output": {"type": "string", "description": "The PDF to write, relative to the workspace or absolute; must end in .pdf."}
            },
            "required": ["output"]
        }
    })
}

/// The file a print writes, as the model named it, so the guards and the
/// checkpoint see the path the tool writes.
pub(super) fn output_path(input: &Value) -> String {
    input
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Print what `input` names with `printer`.
pub(super) fn run(
    printer: &Printer,
    browsers: &Mutex<Browsers>,
    input: &Value,
    resolve: &dyn Fn(&str) -> Result<PathBuf, String>,
    filter_env: bool,
) -> Result<String, String> {
    let named = output_path(input);
    if !named.to_ascii_lowercase().ends_with(".pdf") {
        return Err(format!("`output` must be a .pdf file, not `{named}`"));
    }
    let page = Page::from_input(input, resolve)?;
    let target = resolve(&named)?;
    let printed = match printer {
        Printer::Chromium(bin) => with_chromium(bin, &page, filter_env)?,
        Printer::IncordBrowser(bin) => with_owner(browsers, bin, &page, filter_env)?,
        Printer::Either { browser, chromium } => {
            let ability = browser::print_ability(browsers, browser, filter_env);
            if owner_first(&ability) {
                // The owner's browser first; the system browser when it
                // cannot print this page, and the result says why.
                match with_owner(browsers, browser, &page, filter_env) {
                    Ok(printed) => printed,
                    Err(why) => {
                        let mut printed = with_chromium(chromium, &page, filter_env)?;
                        printed.note = Some(format!(
                            "NOTE: incord-browser could not print it ({why}), so {} did.",
                            printed.by
                        ));
                        printed
                    }
                }
            } else {
                with_chromium(chromium, &page, filter_env)?
            }
        }
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(&target, &printed.bytes)
        .map_err(|e| format!("could not write {}: {e}", target.display()))?;
    let mut said = format!(
        "Printed {} to {} ({} bytes) with {}.",
        page_name(&page),
        target.display(),
        printed.bytes.len(),
        printed.by
    );
    if let Some(note) = printed.note {
        said.push(' ');
        said.push_str(&note);
    }
    Ok(said)
}

/// A PDF, which program printed it, and what the result must add.
struct Printed {
    bytes: Vec<u8>,
    by: String,
    note: Option<String>,
}

/// Anything that is not a PDF is refused, whoever printed it.
fn checked(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    if bytes.starts_with(b"%PDF") {
        Ok(bytes)
    } else {
        Err("the browser produced something that is not a PDF".to_string())
    }
}

/// A print by the system browser, named by its program.
fn with_chromium(bin: &Path, page: &Page, filter_env: bool) -> Result<Printed, String> {
    let bytes = checked(print_with_chromium(bin, page, filter_env)?)?;
    let by = bin
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| bin.display().to_string());
    Ok(Printed {
        bytes,
        by,
        note: None,
    })
}

/// A print by the owner's browser, with the page's own `@page` size when the
/// probe found it honoured, and the note its probe calls for.
fn with_owner(
    browsers: &Mutex<Browsers>,
    bin: &Path,
    page: &Page,
    filter_env: bool,
) -> Result<Printed, String> {
    let ability = browser::print_ability(browsers, bin, filter_env);
    let css_page_size = matches!(
        ability,
        Ok(PrintAbility {
            css_page_size: true,
            ..
        })
    );
    let bytes = checked(browser::print_pdf(
        browsers,
        bin,
        page,
        filter_env,
        css_page_size,
    )?)?;
    Ok(Printed {
        bytes,
        by: "incord-browser".to_string(),
        note: owner_note(&ability),
    })
}

fn page_name(page: &Page) -> String {
    match page {
        Page::Url(url) => url.clone(),
        Page::File(path) => path.display().to_string(),
    }
}

/// One headless print with the system browser, into a scratch directory.
fn print_with_chromium(bin: &Path, page: &Page, filter_env: bool) -> Result<Vec<u8>, String> {
    let scratch = std::env::temp_dir().join(format!("incode-pdf-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&scratch)
        .map_err(|e| format!("could not make a scratch directory: {e}"))?;
    let printed = print_into(bin, page, filter_env, &scratch);
    // The private profile and the scratch PDF go, whatever happened.
    let _ = std::fs::remove_dir_all(&scratch);
    printed
}

fn print_into(
    bin: &Path,
    page: &Page,
    filter_env: bool,
    scratch: &Path,
) -> Result<Vec<u8>, String> {
    let out = scratch.join("out.pdf");
    let address = page.address();
    let mut command = std::process::Command::new(bin);
    command
        .args(["--headless", "--disable-gpu", "--no-pdf-header-footer"])
        .arg(format!(
            "--user-data-dir={}",
            scratch.join("profile").display()
        ))
        .arg(format!("--print-to-pdf={}", out.display()))
        .arg(&address)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if filter_env {
        command.env_clear();
        for (key, value) in crate::tools::env_filter::filtered_env() {
            command.env(key, value);
        }
    }
    crate::agent::process_tree::prepare(&mut command);
    let child = command
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", bin.display()))?;
    let output = crate::tools::bash::wait_with_timeout(child, PDF_DEADLINE).map_err(|e| {
        format!("the browser did not finish printing: {e}; its processes were stopped")
    })?;
    if out.is_file() {
        return std::fs::read(&out).map_err(|e| format!("the printed PDF could not be read: {e}"));
    }
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Err(format!(
        "the browser exited with {} and printed no PDF: {}",
        output.status,
        crate::text::truncate(said.trim(), 600)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, OsString)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        }
    }

    /// **THE SWITCH PICKS THE PRINTER, AND A MISSING ONE IS NOT SUBSTITUTED.**
    /// `auto` with both installed defers to the probe.
    #[test]
    fn the_switch_picks_the_printer() {
        let edge = Some(PathBuf::from("edge"));
        let ours = Some(PathBuf::from("obscura"));
        assert_eq!(
            choose(PdfRenderer::Auto, edge.clone(), ours.clone()),
            Some(Printer::Either {
                browser: "obscura".into(),
                chromium: "edge".into()
            })
        );
        assert_eq!(
            choose(PdfRenderer::Auto, edge.clone(), None),
            Some(Printer::Chromium("edge".into()))
        );
        assert_eq!(
            choose(PdfRenderer::Auto, None, ours.clone()),
            Some(Printer::IncordBrowser("obscura".into()))
        );
        assert_eq!(
            choose(PdfRenderer::IncordBrowser, edge.clone(), ours),
            Some(Printer::IncordBrowser("obscura".into()))
        );
        assert_eq!(choose(PdfRenderer::IncordBrowser, edge.clone(), None), None);
        assert_eq!(choose(PdfRenderer::Chromium, None, Some("o".into())), None);
        assert_eq!(choose(PdfRenderer::Auto, None, None), None);
    }

    /// **`AUTO` PRINTS WITH THE OWNER'S BROWSER ONLY WHEN ITS PROBE FOUND
    /// TEXT, AND THE IMAGE-ONLY NOTE GOES WITH EXACTLY THE OTHER CASES.**
    #[test]
    fn the_probe_decides_the_order_and_the_note() {
        let text = Ok(PrintAbility {
            text: true,
            css_page_size: true,
        });
        let pictures = Ok(PrintAbility {
            text: false,
            css_page_size: false,
        });
        let failed: Result<PrintAbility, String> = Err("did not start".to_string());
        assert!(owner_first(&text));
        assert!(!owner_first(&pictures));
        assert!(!owner_first(&failed));
        assert_eq!(owner_note(&text), None);
        let image_only = owner_note(&pictures).expect("a note");
        assert!(image_only.contains("IMAGE-ONLY"), "{image_only}");
        let unknown = owner_note(&failed).expect("a note");
        assert!(unknown.contains("did not start"), "{unknown}");
        assert!(unknown.contains("image-only"), "{unknown}");
    }

    #[test]
    fn the_setting_is_spelled_as_the_owner_writes_it() {
        for (text, want) in [
            ("\"auto\"", PdfRenderer::Auto),
            ("\"chromium\"", PdfRenderer::Chromium),
            ("\"incord-browser\"", PdfRenderer::IncordBrowser),
        ] {
            assert_eq!(serde_json::from_str::<PdfRenderer>(text).expect(text), want);
        }
    }

    /// The override alone when set; otherwise PATH and the standard places,
    /// including the x86 program directory on Windows.
    #[test]
    fn the_system_browser_is_found_like_libreoffice_is() {
        let dir = tempfile::tempdir().expect("tmp");
        let named = dir.path().join("my-browser");
        std::fs::write(&named, "").expect("seed");
        let env = [(PDF_BROWSER_ENV, named.clone().into_os_string())];
        assert_eq!(find_chromium(&env_of(&env)), Some(named));
        let wrong = [
            (PDF_BROWSER_ENV, OsString::from(dir.path().join("gone"))),
            ("PATH", dir.path().as_os_str().to_os_string()),
        ];
        assert_eq!(find_chromium(&env_of(&wrong)), None);
        if cfg!(windows) {
            let base = tempfile::tempdir().expect("tmp");
            let install = base.path().join(r"Microsoft\Edge\Application");
            std::fs::create_dir_all(&install).expect("dir");
            std::fs::write(install.join("msedge.exe"), "").expect("seed");
            let env = [("ProgramFiles(x86)", base.path().as_os_str().to_os_string())];
            assert_eq!(
                find_chromium(&env_of(&env)),
                Some(install.join("msedge.exe"))
            );
        }
    }

    #[test]
    fn the_output_must_be_a_pdf_and_nothing_runs_otherwise() {
        let ws = tempfile::tempdir().expect("ws");
        std::fs::write(ws.path().join("p.html"), "<p>x</p>").expect("seed");
        let resolve = |p: &str| Ok(ws.path().join(p));
        let why = run(
            &Printer::Chromium("definitely-not-a-program".into()),
            &Mutex::new(Browsers::default()),
            &json!({"path": "p.html", "output": "p.txt"}),
            &resolve,
            true,
        )
        .expect_err("not a pdf");
        assert!(why.contains(".pdf"), "{why}");
        assert!(!ws.path().join("p.txt").exists());
    }
}
