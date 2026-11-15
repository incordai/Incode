//! What the crate's tests share off Windows: writing a script a test then
//! runs.
//!
//! Used by every test that writes a stand-in program on Unix: the agents'
//! (`agent/agy_tests.rs`, `agent/agy/bridged_tests.rs`,
//! `agent/claude_code/tests.rs`, `agent/codex/tests.rs`), the engine's
//! (`hooks/engine.rs`) and the document reader's (`toolbox/reading_tests.rs`).
//! Test code only; `self_update.rs` makes its downloaded binary runnable in
//! its own way. On Windows the same tests write `.cmd` files and do not use
//! this.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// Write `body` to `path` as a runnable script, for a test to start.
///
/// The file is written by a short-lived `sh`, not by this process. A test
/// binary runs its tests on many threads at once, and a program another
/// thread starts while this process holds a file open for writing is handed
/// that open file until it has started; starting the script in that moment
/// fails with "Text file busy" (ETXTBSY) — a test that fails one run in
/// many, for no reason in the code under test. A file this process never
/// opens cannot be handed on.
///
/// Panics, as a test's setup does, when the script cannot be written.
pub(crate) fn write_script(path: &Path, body: &str) {
    let mut sh = Command::new("sh")
        .arg("-c")
        .arg(r#"cat > "$1" && chmod 755 "$1""#)
        .arg("sh")
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("start sh to write the script");
    sh.stdin
        .take()
        .expect("sh's stdin")
        .write_all(body.as_bytes())
        .expect("hand the script to sh");
    let status = sh.wait().expect("sh ends");
    assert!(status.success(), "sh could not write {}", path.display());
}
