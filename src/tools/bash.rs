//! Bash tool — execute shell commands with timeout.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};

const DEFAULT_TIMEOUT_MS: u64 = 120_000; // 2 minutes
const MAX_OUTPUT_CHARS: usize = 30_000;

/// Execute a bash command in the workspace directory.
///
/// `filter_env` applies the [`super::env_filter`] allowlist to the child's
/// environment. This tool runs commands the model chose, so an unfiltered child
/// inherits every variable this process holds — including credentials and
/// internal URLs that have nothing to do with the command. The background-job
/// path already filtered; this one did not, which meant `env_filter_enabled`
/// had no effect on the tool the model actually calls.
pub fn execute_bash(workspace: &Path, input: &str, filter_env: bool) -> Result<String> {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or_else(|_| serde_json::json!({"command": input}));

    let command = parsed["command"]
        .as_str()
        .context("missing 'command' field")?;
    let timeout_ms = parsed["timeout"].as_u64().unwrap_or(DEFAULT_TIMEOUT_MS);

    // `Command::arg` applies MSVC-style quoting on Windows: a `"` in the
    // argument is escaped to `\"`. `cmd.exe /C` does not parse its argument
    // that way, so every quote the model wrote arrived literally backslashed
    // and `echo "hello world"` printed `\"hello world\"`. Redirects, paths with
    // spaces and any quoted grep pattern were all corrupted. `raw_arg` passes
    // the command through verbatim, which is what a shell string needs.
    #[cfg(windows)]
    let mut child = {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        c.arg("/C").raw_arg(command);
        c
    };

    #[cfg(not(windows))]
    let mut child = {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };

    child
        .current_dir(workspace)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if filter_env {
        child.env_clear();
        for (key, value) in super::env_filter::filtered_env() {
            child.env(key, value);
        }
    }

    let spawned = child.spawn().context("failed to spawn shell")?;

    // Wait with timeout
    let output = wait_with_timeout(spawned, Duration::from_millis(timeout_ms))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output.status.code().unwrap_or(-1);

    let mut result = String::new();

    if !stdout.is_empty() {
        result.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str("STDERR:\n");
        result.push_str(&stderr);
    }

    if code != 0 {
        result.push_str(&format!("\n(exit code: {code})"));
    }

    // Truncate if too long. `String::truncate` panics on a non-character
    // boundary, and this is shell output — arbitrary bytes from any command.
    crate::text::truncate_in_place(&mut result, MAX_OUTPUT_CHARS, "\n... (output truncated)");

    Ok(result)
}

pub(crate) fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<std::process::Output> {
    // Simple approach: use wait_with_output with a thread-based timeout
    let start = std::time::Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child.wait_with_output().context("failed to collect output");
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    anyhow::bail!("command timed out after {}s", timeout.as_secs());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Echo a variable in a way both shells understand.
    fn echo_var(name: &str) -> String {
        #[cfg(windows)]
        return format!("echo %{name}%");
        #[cfg(not(windows))]
        return format!("echo ${name}");
    }

    fn run(command: &str, filter_env: bool) -> String {
        let input = serde_json::json!({ "command": command }).to_string();
        execute_bash(std::path::Path::new("."), &input, filter_env)
            .expect("the shell command should run")
    }

    /// A secret in this process's environment must not reach a command the
    /// model chose. The bash tool previously inherited the full parent
    /// environment, so `env_filter_enabled` had no effect on it at all.
    #[test]
    fn filtered_shell_cannot_see_an_unlisted_secret() {
        // Deliberately NOT prefixed `INCORD_`: env_filter.rs:163 allowlists that
        // whole prefix, so an `INCORD_*` canary would be passed through by design
        // and the test would prove nothing.
        let name = "ZZ_UNLISTED_TEST_VAR";
        let canary = "CANARY-MUST-NOT-REACH-THE-CHILD";
        std::env::set_var(name, canary);

        let out = run(&echo_var(name), true);
        std::env::remove_var(name);

        assert!(
            !out.contains(canary),
            "an unlisted variable must not reach the child; got: {out}"
        );
    }

    /// Filtering must not break ordinary work — the allowlisted essentials
    /// still have to reach the child, or every build command fails.
    #[test]
    fn filtered_shell_still_receives_allowlisted_essentials() {
        let out = run(&echo_var("PATH"), true);
        assert!(
            out.trim().len() > 3 && !out.contains("%PATH%") && !out.trim().is_empty(),
            "PATH is allowlisted and must still reach the child; got: {out}"
        );
    }

    /// A quoted argument must reach the shell as the user wrote it. On Windows
    /// `Command::arg` escapes `"` to `\"` for MSVC-style parsing, which
    /// `cmd /C` does not undo, so `echo "hello world"` printed
    /// `\"hello world\"` — corrupting every quoted grep pattern, redirect and
    /// spaced path the model produced.
    #[test]
    fn quotes_in_a_command_reach_the_shell_intact() {
        let out = run("echo \"hello world\"", false);
        assert!(
            out.contains("hello world"),
            "the quoted text must survive; got: {out}"
        );
        assert!(
            !out.contains('\\'),
            "no backslash escaping may be introduced; got: {out}"
        );
    }

    /// Shell output is arbitrary bytes, and the 30 000-byte cap used to be
    /// applied with `String::truncate`, which panics when that index falls
    /// inside a character. Any command printing enough non-ASCII text — a
    /// compiler error quoting source, a directory of CJK filenames — took the
    /// whole CLI down.
    ///
    /// The shell prints one ASCII byte then 20 000 three-byte characters, so
    /// the cap at byte 30 000 lands 29 999 bytes into a run of 3-byte
    /// characters and cannot be a boundary. The content comes from a file so
    /// the command itself stays ASCII — passing non-ASCII through `cmd` is
    /// subject to the console code page and would not reliably arrive.
    #[test]
    fn oversized_non_ascii_output_is_truncated_without_panicking() {
        let dir = std::env::temp_dir().join("incord-bash-truncation-test");
        std::fs::create_dir_all(&dir).expect("test fixture directory");
        let path = dir.join("wide.txt");
        let mut content = String::from("a");
        content.push_str(&"\u{4e2d}".repeat(20_000));
        assert!(
            content.len() > MAX_OUTPUT_CHARS,
            "the fixture must exceed the cap or the test proves nothing"
        );
        std::fs::write(&path, &content).expect("write fixture");

        #[cfg(windows)]
        let command = format!("type \"{}\"", path.display());
        #[cfg(not(windows))]
        let command = format!("cat \"{}\"", path.display());

        let out = run(&command, false);
        let _ = std::fs::remove_file(&path);

        assert!(
            out.contains("(output truncated)"),
            "the output should have exceeded the cap and been marked; got {} bytes: {out}",
            out.len()
        );
        assert!(
            std::str::from_utf8(out.as_bytes()).is_ok(),
            "truncation must leave valid UTF-8"
        );
        assert!(
            out.starts_with('a') && out.contains('\u{4e2d}'),
            "the kept prefix must be the real output"
        );

        // Prove the cut point was genuinely mid-character, so this test would
        // have hit the panic rather than passing by luck: the kept prefix has
        // to be SHORTER than the cap, which only happens when the boundary
        // search had to walk backwards.
        let kept = out.len() - "\n... (output truncated)".len();
        assert!(
            kept < MAX_OUTPUT_CHARS,
            "the cap must have landed inside a character for this to test \
             anything; kept {kept} of {MAX_OUTPUT_CHARS}"
        );
        assert!(
            kept >= MAX_OUTPUT_CHARS - 3,
            "it must give up at most one character's worth; kept {kept}"
        );
    }

    /// The flag must actually control the behaviour, in both directions —
    /// otherwise the config option is decorative.
    #[test]
    fn disabling_the_filter_passes_the_environment_through() {
        let name = "ZZ_PASSTHROUGH_TEST_VAR";
        let marker = "PASSTHROUGH-MARKER-VALUE";
        std::env::set_var(name, marker);

        let unfiltered = run(&echo_var(name), false);
        let filtered = run(&echo_var(name), true);
        std::env::remove_var(name);

        assert!(
            unfiltered.contains(marker),
            "with filtering off the variable should pass through; got: {unfiltered}"
        );
        assert!(
            !filtered.contains(marker),
            "with filtering on it must not; got: {filtered}"
        );
    }
}
