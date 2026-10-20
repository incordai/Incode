//! Windows paths as WSL sees them, for `sbx.exe` forwarding a command into
//! WSL2 (where the Linux sandbox does the confining).
//!
//! `C:\work\app` is `/mnt/c/work/app` in WSL's default mount layout
//! (`[automount] root = /mnt/`). A UNC path to a WSL distribution
//! (`\\wsl$\Ubuntu\home\me`, `\\wsl.localhost\Ubuntu\home\me`) is that
//! distribution's own `/home/me`. Anything else (another UNC share, a
//! relative path) has no WSL spelling and is refused rather than guessed.

/// The WSL spelling of a Windows path, or `None` when there is none.
#[must_use]
pub fn to_wsl(path: &str) -> Option<String> {
    let p = path.replace('\\', "/");
    for prefix in ["//wsl$/", "//wsl.localhost/"] {
        if let Some(rest) = p
            .get(..prefix.len())
            .filter(|h| h.eq_ignore_ascii_case(prefix))
            .map(|_| &p[prefix.len()..])
        {
            // Drop the distribution's name: inside it, the rest is absolute.
            let inner = rest.split_once('/').map_or("", |(_, r)| r);
            return Some(format!("/{inner}"));
        }
    }
    let b = p.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let drive = (b[0] as char).to_ascii_lowercase();
        let rest = p[2..].trim_start_matches('/');
        return Some(if rest.is_empty() {
            format!("/mnt/{drive}")
        } else {
            format!("/mnt/{drive}/{}", rest.trim_end_matches('/'))
        });
    }
    None
}

/// The options of `sbx` whose value is a path.
pub const PATH_OPTIONS: &[&str] = &[
    "-w",
    "--write",
    "--deny-read",
    "--cwd",
    "-p",
    "--policy",
    "--root",
];

/// `sbx` arguments rewritten for WSL: each path option's value, and any
/// argument after `--` that is a drive path (`C:\...`), translated.
///
/// # Errors
/// When a path option's value has no WSL spelling.
pub fn translate_args(args: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(args.len());
    let mut after_dashdash = false;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if after_dashdash {
            out.push(if is_drive_path(a) {
                to_wsl(a).unwrap_or_else(|| a.clone())
            } else {
                a.clone()
            });
        } else if a == "--" {
            after_dashdash = true;
            out.push(a.clone());
        } else if PATH_OPTIONS.contains(&a.as_str()) {
            out.push(a.clone());
            if let Some(v) = args.get(i + 1) {
                out.push(path_value(v)?);
                i += 1;
            }
        } else {
            out.push(a.clone());
        }
        i += 1;
    }
    Ok(out)
}

fn path_value(v: &str) -> Result<String, String> {
    let absolute = is_drive_path(v) || v.starts_with("\\\\") || v.starts_with("//");
    if !absolute {
        // Relative (or `~`): resolved inside WSL against the forwarded --cd.
        return Ok(v.replace('\\', "/"));
    }
    to_wsl(v)
        .ok_or_else(|| format!("`{v}` has no path inside WSL (use a drive path or \\\\wsl$\\...)"))
}

fn is_drive_path(a: &str) -> bool {
    let b = a.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_and_wsl_unc_paths_translate() {
        assert_eq!(to_wsl(r"C:\work\app").as_deref(), Some("/mnt/c/work/app"));
        assert_eq!(to_wsl("d:/data/").as_deref(), Some("/mnt/d/data"));
        assert_eq!(to_wsl(r"E:\").as_deref(), Some("/mnt/e"));
        assert_eq!(
            to_wsl(r"\\wsl$\Ubuntu\home\me\proj").as_deref(),
            Some("/home/me/proj")
        );
        assert_eq!(
            to_wsl(r"\\WSL.localhost\Ubuntu-24.04\tmp").as_deref(),
            Some("/tmp")
        );
        assert_eq!(to_wsl(r"\\server\share\x"), None);
        assert_eq!(to_wsl(r"relative\dir"), None);
    }

    #[test]
    fn path_options_and_command_paths_are_rewritten() {
        let args: Vec<String> = [
            "run",
            "-w",
            r"C:\work\app",
            "--deny-read",
            r"..\private",
            "--net",
            "deny",
            "--",
            "python3",
            r"C:\work\app\main.py",
            "--flag=C:x",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            translate_args(&args).expect("translates"),
            [
                "run",
                "-w",
                "/mnt/c/work/app",
                "--deny-read",
                "../private",
                "--net",
                "deny",
                "--",
                "python3",
                "/mnt/c/work/app/main.py",
                "--flag=C:x",
            ]
        );
        let bad: Vec<String> = ["run", "-w", r"\\server\share"].map(String::from).to_vec();
        assert!(translate_args(&bad).is_err());
    }
}
