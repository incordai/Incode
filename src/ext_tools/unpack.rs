//! Unpacking a checked release archive, and nothing outside its folder.
//!
//! zip (the `zip` crate) and tar.gz (`flate2` and `tar`). Each entry's path is
//! read as written in the archive and must be relative and made only of plain
//! names ([`super::inside_path`]): an absolute path, a drive, a `..` anywhere,
//! or (on Windows) a `:` stream name is refused, and so is the whole archive.
//! Links — symbolic or hard — are refused, since a link is how a later entry
//! is written somewhere else. Files are written by this module, never by the
//! archive crates' own "extract", so every byte is counted:
//! [`MAX_ENTRIES`] entries and [`MAX_UNPACKED_BYTES`] in all, measured as the
//! bytes come out, not taken from the headers.

use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};

/// The most entries an archive may hold.
pub(crate) const MAX_ENTRIES: usize = 10_000;
/// The most bytes an archive may unpack to.
pub(crate) const MAX_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// What an archive has unpacked so far, against its bounds.
struct Budget {
    entries: usize,
    bytes: u64,
    max_entries: usize,
    max_bytes: u64,
}

impl Budget {
    fn entry(&mut self) -> Result<(), String> {
        self.entries += 1;
        if self.entries > self.max_entries {
            return Err(format!(
                "the archive holds more than {} entries",
                self.max_entries
            ));
        }
        Ok(())
    }
}

/// Unpack `archive`, named `asset`, into `into` (created). The kind is taken
/// from the name: `.zip`, or `.tar.gz` / `.tgz`.
pub(crate) fn unpack(archive: &Path, asset: &str, into: &Path) -> Result<(), String> {
    unpack_within(archive, asset, into, MAX_ENTRIES, MAX_UNPACKED_BYTES)
}

/// [`unpack`], with the bounds given.
pub(crate) fn unpack_within(
    archive: &Path,
    asset: &str,
    into: &Path,
    max_entries: usize,
    max_bytes: u64,
) -> Result<(), String> {
    std::fs::create_dir_all(into).map_err(|e| format!("could not make {}: {e}", into.display()))?;
    let mut budget = Budget {
        entries: 0,
        bytes: 0,
        max_entries,
        max_bytes,
    };
    let lower = asset.to_ascii_lowercase();
    let file =
        std::fs::File::open(archive).map_err(|e| format!("could not open the download: {e}"))?;
    if lower.ends_with(".zip") {
        unzip(file, into, &mut budget)
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        untar(flate2::read::GzDecoder::new(file), into, &mut budget)
    } else {
        Err(format!(
            "`{asset}` is neither a .zip nor a .tar.gz; incode unpacks only those"
        ))
    }
    .map_err(|why| format!("{asset} was not unpacked: {why}"))
}

/// Where an entry named `name` goes under `into`, or why it may not.
fn destination(into: &Path, name: &str) -> Result<PathBuf, String> {
    if name.contains('\0') {
        return Err("an entry's name holds a NUL byte".to_string());
    }
    let relative = super::inside_path(name).map_err(|why| format!("entry {why}"))?;
    if cfg!(windows)
        && relative
            .components()
            .any(|c| c.as_os_str().to_string_lossy().contains(':'))
    {
        return Err(format!("entry `{name}` names a stream (`:`)"));
    }
    Ok(into.join(relative))
}

/// Copy `from` into a new file at `to`, counting against `budget`.
fn write_file(from: &mut dyn Read, to: &Path, budget: &mut Budget) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not make {}: {e}", parent.display()))?;
    }
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
        .map_err(|e| format!("could not write {}: {e}", to.display()))?;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = from
            .read(&mut buffer)
            .map_err(|e| format!("the archive could not be read: {e}"))?;
        if n == 0 {
            break;
        }
        budget.bytes += n as u64;
        if budget.bytes > budget.max_bytes {
            return Err(format!(
                "it unpacks to more than {} bytes",
                budget.max_bytes
            ));
        }
        out.write_all(&buffer[..n])
            .map_err(|e| format!("could not write {}: {e}", to.display()))?;
    }
    Ok(())
}

/// Mark `path` executable when the archive said it was (Unix only).
fn set_mode(path: &Path, mode: Option<u32>) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let executable = mode.is_some_and(|m| m & 0o111 != 0);
        let bits = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits))
            .map_err(|e| format!("could not set the mode of {}: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

fn unzip(file: std::fs::File, into: &Path, budget: &mut Budget) -> Result<(), String> {
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("it is not a readable zip: {e}"))?;
    if archive.len() > budget.max_entries {
        return Err(format!(
            "the archive holds {} entries, more than {}",
            archive.len(),
            budget.max_entries
        ));
    }
    for index in 0..archive.len() {
        budget.entry()?;
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("entry {index} could not be read: {e}"))?;
        let name = entry.name().to_string();
        // Both rules: ours, and the crate's own.
        let to = destination(into, &name)?;
        if entry.enclosed_name().is_none() {
            return Err(format!("entry `{name}` would land outside the folder"));
        }
        if entry.is_symlink() {
            return Err(format!("entry `{name}` is a link, which is not unpacked"));
        }
        if entry.is_dir() {
            std::fs::create_dir_all(&to)
                .map_err(|e| format!("could not make {}: {e}", to.display()))?;
            continue;
        }
        let mode = entry.unix_mode();
        write_file(&mut entry, &to, budget)?;
        set_mode(&to, mode)?;
    }
    Ok(())
}

fn untar(reader: impl Read, into: &Path, budget: &mut Budget) -> Result<(), String> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|e| format!("it is not a readable tar.gz: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("an entry could not be read: {e}"))?;
        let kind = entry.header().entry_type();
        // A pax global header carries settings, not a file.
        if kind.is_pax_global_extensions() {
            continue;
        }
        budget.entry()?;
        let raw = entry.path_bytes().into_owned();
        let name =
            String::from_utf8(raw).map_err(|_| "an entry's name is not UTF-8".to_string())?;
        let to = destination(into, &name)?;
        if kind.is_symlink() || kind.is_hard_link() {
            return Err(format!("entry `{name}` is a link, which is not unpacked"));
        }
        if kind.is_dir() {
            std::fs::create_dir_all(&to)
                .map_err(|e| format!("could not make {}: {e}", to.display()))?;
            continue;
        }
        if !kind.is_file() {
            return Err(format!(
                "entry `{name}` is neither a file nor a folder, which is not unpacked"
            ));
        }
        let mode = entry.header().mode().ok();
        write_file(&mut entry, &to, budget)?;
        set_mode(&to, mode)?;
    }
    Ok(())
}
