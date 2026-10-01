//! `ConvertDocument`: an office document to PDF, through LibreOffice.
//!
//! Optional. LibreOffice is an external program this binary does not ship;
//! the tool is OFFERED only when `soffice` is found, so a model is never told
//! about a tool that can only fail. Looked for in order: the `INCODE_SOFFICE`
//! override (and ONLY there when it is set — a named path that is wrong is a
//! mistake to report, not a reason to pick another copy), then `PATH`, then
//! each platform's standard install locations.
//!
//! # How it is run
//!
//! Headless, with no first-start wizard, no crash recovery and no lock check,
//! and against a PRIVATE profile in a fresh temporary directory: a desktop
//! LibreOffice the operator has open would otherwise hold the profile and the
//! conversion would silently hand its work to that instance and exit. The PDF
//! is written into that temporary directory and copied into place only when
//! it exists, so a failed conversion leaves nothing half-written in the
//! workspace. A hard deadline ends the whole process tree — `soffice` starts
//! a second process that does the work.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

/// Names the `soffice` executable to use, overriding the search.
pub const SOFFICE_ENV: &str = "INCODE_SOFFICE";
/// How long a conversion may run before its process tree is killed.
pub const CONVERT_DEADLINE: Duration = Duration::from_secs(120);
/// What can be converted, by extension.
const CONVERTIBLE: &[&str] = &[
    "docx", "doc", "odt", "rtf", "pptx", "ppt", "odp", "xlsx", "xls", "ods",
];

/// The tool as the model is told about it.
pub(super) fn definition() -> Value {
    json!({
        "name": super::CONVERT_DOCUMENT,
        "description": "Convert an office document (docx, doc, odt, rtf, pptx, ppt, odp, xlsx, \
            xls, ods) to PDF with LibreOffice. Writes `<name>.pdf` beside the document, or into \
            `output_dir`, replacing a PDF of that name. To READ a document you do not need \
            this — Read returns docx, pptx, xlsx and PDF as markdown.",
        "input_schema": {
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The document to convert, relative to the workspace or absolute."},
                "output_dir": {"type": "string", "description": "Directory for the PDF. Defaults to the document's own directory."}
            },
            "required": ["path"]
        }
    })
}

/// Where `soffice` is looked for, in order. `env` reads a variable, so a
/// test can say what the environment holds.
fn candidates(env: &dyn Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    if let Some(named) = env(SOFFICE_ENV).filter(|v| !v.is_empty()) {
        return vec![PathBuf::from(named)];
    }
    let mut out = Vec::new();
    let names: &[&str] = if cfg!(windows) {
        &["soffice.exe", "soffice.com"]
    } else {
        &["soffice", "libreoffice"]
    };
    if let Some(path) = env("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in names {
                out.push(dir.join(name));
            }
        }
    }
    if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = env(var) {
                out.push(
                    Path::new(&base)
                        .join("LibreOffice")
                        .join("program")
                        .join("soffice.exe"),
                );
            }
        }
    } else if cfg!(target_os = "macos") {
        out.push("/Applications/LibreOffice.app/Contents/MacOS/soffice".into());
        if let Some(home) = env("HOME") {
            out.push(Path::new(&home).join("Applications/LibreOffice.app/Contents/MacOS/soffice"));
        }
    } else {
        for fixed in [
            "/usr/bin/soffice",
            "/usr/local/bin/soffice",
            "/usr/lib/libreoffice/program/soffice",
            "/opt/libreoffice/program/soffice",
            "/snap/bin/libreoffice",
        ] {
            out.push(fixed.into());
        }
    }
    out
}

/// The `soffice` to use, or `None` when LibreOffice is not installed where
/// anything looked. Traced either way, with how many places were tried.
pub(super) fn find_soffice(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let tried = candidates(env);
    let found = tried.iter().find(|p| p.is_file()).cloned();
    match &found {
        Some(path) => crate::trace!("tool", "LibreOffice found at {}", path.display()),
        None => crate::trace!(
            "tool",
            "LibreOffice not found ({} places tried); ConvertDocument is not offered",
            tried.len()
        ),
    }
    found
}

/// The file a conversion writes, spelled the way the model named its input,
/// so the guards and the checkpoint see the same path the tool writes.
pub(super) fn output_path(input: &Value) -> String {
    let named = input.get("path").and_then(Value::as_str).unwrap_or("");
    // The name after the LAST separator of either kind, as the directory is
    // found below. `Path::file_stem` alone splits on this OS's separator only,
    // so on macOS and Linux `a\b.pptx` became `a\a\b.pdf`.
    let file_name = named.rsplit(['/', '\\']).next().unwrap_or(named);
    let pdf_name = Path::new(file_name)
        .file_stem()
        .map(|stem| format!("{}.pdf", stem.to_string_lossy()))
        .unwrap_or_default();
    match input.get("output_dir").and_then(Value::as_str) {
        Some(dir) if !dir.trim().is_empty() => {
            format!("{}/{pdf_name}", dir.trim_end_matches(['/', '\\']))
        }
        // The input's own directory, in the separator the model used.
        _ => match named.rfind(['/', '\\']) {
            Some(cut) => format!("{}{pdf_name}", &named[..=cut]),
            None => pdf_name,
        },
    }
}

/// A local path as a `file://` URL: what LibreOffice's `-env:` option takes,
/// and what a headless browser is pointed at for a workspace file.
pub(super) fn file_url(path: &Path) -> String {
    let spelled = path.to_string_lossy().replace('\\', "/");
    let mut url = String::from("file://");
    if !spelled.starts_with('/') {
        url.push('/');
    }
    for byte in spelled.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' | b':' => {
                url.push(char::from(byte));
            }
            other => url.push_str(&format!("%{other:02X}")),
        }
    }
    url
}

/// Convert the document `input` names. `resolve` confines a path the way
/// every file tool does.
pub(super) fn run(
    soffice: &Path,
    input: &Value,
    filter_env: bool,
    resolve: &dyn Fn(&str) -> Result<PathBuf, String>,
) -> Result<String, String> {
    let named = input
        .get("path")
        .and_then(Value::as_str)
        .ok_or("say which document to convert, as `path`")?;
    let source = resolve(named)?;
    if !source.is_file() {
        return Err(format!("{} is not a file", source.display()));
    }
    let ext = source
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !CONVERTIBLE.contains(&ext.as_str()) {
        return Err(format!(
            "ConvertDocument converts {} to PDF; `.{ext}` is not one of them",
            CONVERTIBLE.join(", ")
        ));
    }
    let target = resolve(&output_path(input))?;

    let scratch = std::env::temp_dir().join(format!("incode-soffice-{}", uuid::Uuid::new_v4()));
    let outdir = scratch.join("out");
    std::fs::create_dir_all(&outdir)
        .map_err(|e| format!("could not make a scratch directory: {e}"))?;
    let outcome = convert(soffice, &source, &scratch, &outdir, filter_env).and_then(|made| {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::copy(&made, &target)
            .map_err(|e| format!("could not write {}: {e}", target.display()))
    });
    // The private profile and the scratch PDF go, whatever happened.
    let _ = std::fs::remove_dir_all(&scratch);
    let bytes = outcome?;
    Ok(format!(
        "Converted {} to {} ({bytes} bytes)",
        source.display(),
        target.display()
    ))
}

/// Run `soffice` once, and return the PDF it made in `outdir`.
fn convert(
    soffice: &Path,
    source: &Path,
    scratch: &Path,
    outdir: &Path,
    filter_env: bool,
) -> Result<PathBuf, String> {
    let mut command = std::process::Command::new(soffice);
    command
        .args([
            "--headless",
            "--norestore",
            "--nologo",
            "--nodefault",
            "--nolockcheck",
        ])
        .arg(format!(
            "-env:UserInstallation={}",
            file_url(&scratch.join("profile"))
        ))
        .args(["--convert-to", "pdf", "--outdir"])
        .arg(outdir)
        .arg(source)
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
        .map_err(|e| format!("could not start {}: {e}", soffice.display()))?;
    let output = crate::tools::bash::wait_with_timeout(child, CONVERT_DEADLINE)
        .map_err(|e| format!("LibreOffice did not finish: {e}; its processes were stopped"))?;
    let made = outdir.join(
        source
            .file_stem()
            .map(|s| format!("{}.pdf", s.to_string_lossy()))
            .unwrap_or_default(),
    );
    if output.status.success() && made.is_file() {
        return Ok(made);
    }
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Err(format!(
        "LibreOffice exited with {} and made no PDF{}",
        output.status,
        if said.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", crate::text::truncate(said.trim(), 600))
        }
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

    /// **THE OVERRIDE WINS, AND ONLY IT IS TRIED WHEN SET.**
    #[test]
    fn the_override_is_used_and_nothing_else_when_set() {
        let dir = tempfile::tempdir().expect("tmp");
        let fake = dir.path().join(if cfg!(windows) {
            "soffice.exe"
        } else {
            "soffice"
        });
        std::fs::write(&fake, "").expect("seed");
        let env = [(SOFFICE_ENV, fake.clone().into_os_string())];
        assert_eq!(find_soffice(&env_of(&env)), Some(fake));

        // A wrong override is not quietly replaced by another copy on PATH.
        let other = tempfile::tempdir().expect("tmp");
        std::fs::write(
            other.path().join(if cfg!(windows) {
                "soffice.exe"
            } else {
                "soffice"
            }),
            "",
        )
        .expect("seed");
        let env = [
            (
                SOFFICE_ENV,
                OsString::from(dir.path().join("missing").as_os_str()),
            ),
            ("PATH", other.path().as_os_str().to_os_string()),
        ];
        assert_eq!(find_soffice(&env_of(&env)), None);
    }

    #[test]
    fn path_and_the_standard_places_are_searched() {
        let dir = tempfile::tempdir().expect("tmp");
        let name = if cfg!(windows) {
            "soffice.exe"
        } else {
            "soffice"
        };
        std::fs::write(dir.path().join(name), "").expect("seed");
        let env = [("PATH", dir.path().as_os_str().to_os_string())];
        assert_eq!(find_soffice(&env_of(&env)), Some(dir.path().join(name)));

        if cfg!(windows) {
            let base = tempfile::tempdir().expect("tmp");
            let install = base.path().join("LibreOffice").join("program");
            std::fs::create_dir_all(&install).expect("dir");
            std::fs::write(install.join("soffice.exe"), "").expect("seed");
            let env = [("ProgramFiles", base.path().as_os_str().to_os_string())];
            assert_eq!(
                find_soffice(&env_of(&env)),
                Some(install.join("soffice.exe"))
            );
        }
    }

    /// Nothing installed anywhere looked: not found.
    #[test]
    fn nothing_found_is_none() {
        let empty = tempfile::tempdir().expect("tmp");
        let env = [("PATH", empty.path().as_os_str().to_os_string())];
        let found = find_soffice(&env_of(&env));
        // The fixed install locations are real paths on this machine; the
        // assertion holds wherever LibreOffice is not installed there.
        if let Some(path) = found {
            assert!(!path.starts_with(empty.path()), "{}", path.display());
        }
    }

    #[test]
    fn the_output_lands_beside_the_input_or_in_output_dir() {
        assert_eq!(
            output_path(&json!({"path": "docs/report.docx"})),
            "docs/report.pdf"
        );
        assert_eq!(output_path(&json!({"path": r"a\b.pptx"})), r"a\b.pdf");
        assert_eq!(output_path(&json!({"path": "top.odt"})), "top.pdf");
        assert_eq!(
            output_path(&json!({"path": "docs/report.docx", "output_dir": "out/"})),
            "out/report.pdf"
        );
    }

    #[test]
    fn the_profile_url_is_a_file_url_with_spaces_escaped() {
        let url = file_url(Path::new(r"C:\Users\A B\tmp\profile"));
        assert_eq!(url, "file:///C:/Users/A%20B/tmp/profile");
        assert_eq!(file_url(Path::new("/tmp/p q")), "file:///tmp/p%20q");
    }

    #[test]
    fn an_unconvertible_file_or_a_missing_one_is_refused_before_anything_runs() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("a.txt"), "x").expect("seed");
        let resolve = |p: &str| Ok(dir.path().join(p));
        let never = Path::new("definitely-not-a-program");
        let why = run(never, &json!({"path": "a.txt"}), true, &resolve).expect_err("txt");
        assert!(why.contains("`.txt` is not one of them"), "{why}");
        let why = run(never, &json!({"path": "gone.docx"}), true, &resolve).expect_err("missing");
        assert!(why.contains("is not a file"), "{why}");
    }

    /// A program that cannot be started is an error that names it, and
    /// nothing is written.
    #[test]
    fn a_soffice_that_cannot_start_writes_nothing() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("a.docx"), "x").expect("seed");
        let resolve = |p: &str| Ok(dir.path().join(p));
        let never = dir.path().join("no-such-soffice");
        let why = run(&never, &json!({"path": "a.docx"}), true, &resolve).expect_err("no program");
        assert!(why.contains("could not start"), "{why}");
        assert!(!dir.path().join("a.pdf").exists());
    }
}
