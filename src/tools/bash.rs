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

    crate::agent::process_tree::prepare(&mut child);
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

/// Wait for `child` to finish, or kill it — and everything it started — at
/// `timeout`. The one bounded waiter in the crate: the bash tool, `verify` and
/// hooks all come through here.
///
/// Spawn `child` from a command passed through
/// [`crate::agent::process_tree::prepare`], or on Linux and macOS its
/// descendants cannot be reached.
///
/// # Why the pipes are read while waiting
///
/// This used to poll `try_wait` and read stdout and stderr only after exit. A
/// command that printed more than a pipe holds blocked on the write and never
/// exited, so a verbose check was killed at the deadline and reported as a
/// hang. Measured: 4,000 lines of output, "command timed out after 30s".
pub(crate) fn wait_with_timeout(
    child: std::process::Child,
    timeout: Duration,
) -> Result<std::process::Output> {
    wait_with_timeout_or(child, timeout, &|| false)
}

/// [`wait_with_timeout`], which also kills the tree as soon as `abandon`
/// answers true.
///
/// For a wait whose answer can arrive from somewhere else first — the engine's
/// permission hook racing the person at the prompt — so the losing side is
/// ended at once instead of running to its bound.
pub(crate) fn wait_with_timeout_or(
    mut child: std::process::Child,
    timeout: Duration,
    abandon: &dyn Fn() -> bool,
) -> Result<std::process::Output> {
    let tree = crate::agent::process_tree::Tree::adopt(&child);
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if abandon() {
                    tree.kill(&mut child);
                    let _ = child.wait();
                    anyhow::bail!("stopped before it finished: the answer came from elsewhere");
                }
                if start.elapsed() > timeout {
                    tree.kill(&mut child);
                    let _ = child.wait();
                    // The readers are not joined: something outside the tree
                    // could still hold a pipe, and a timeout must not become
                    // a hang. They end when the pipes close.
                    anyhow::bail!("command timed out after {}s", timeout.as_secs());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.into()),
        }
    };
    Ok(std::process::Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shell command that prints `lines` lines of about 70 bytes each.
    fn loud(lines: u32) -> Command {
        #[cfg(windows)]
        {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(format!(
                "for /L %i in (1,1,{lines}) do @echo line %i padding padding padding padding padding"
            ));
            c
        }
        #[cfg(not(windows))]
        {
            let mut c = Command::new("sh");
            c.arg("-c").arg(format!(
                "i=0; while [ $i -lt {lines} ]; do echo \"line $i padding padding padding \
                 padding padding\"; i=$((i+1)); done"
            ));
            c
        }
    }

    /// **A CHECK THAT PRINTS A LOT IS NOT MISTAKEN FOR ONE THAT HANGS.**
    ///
    /// The waiter polled `try_wait` and only read the pipes after exit. A
    /// command that printed more than a pipe holds blocked on the write,
    /// never exited, and was killed at the deadline as a "timeout" — a
    /// verbose test suite reported as a hung one.
    #[test]
    fn a_command_with_a_lot_of_output_finishes_and_keeps_it() {
        let mut cmd = loud(4000);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        crate::agent::process_tree::prepare(&mut cmd);
        let child = cmd.spawn().expect("spawn");

        let output = wait_with_timeout(child, Duration::from_secs(30))
            .expect("a command that only printed a lot was treated as hung");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains("line 4000 "),
            "the end of the output was lost"
        );
        assert!(
            output.stdout.len() > 200_000,
            "{} bytes",
            output.stdout.len()
        );
    }

    /// **A TIMED-OUT COMMAND TAKES WHAT IT STARTED WITH IT.**
    ///
    /// A check is a shell that starts a build tool that starts test binaries.
    /// Killing only the shell left the rest running after the run moved on.
    #[test]
    fn a_timed_out_command_stops_its_children_too() {
        let dir = tempfile::tempdir().expect("scratch");
        let (mut cmd, ticks) =
            crate::agent::process_tree::a_parent_with_a_ticking_grandchild(dir.path());
        crate::agent::process_tree::prepare(&mut cmd);
        let child = cmd.spawn().expect("spawn");

        let result = wait_with_timeout(child, Duration::from_secs(4));
        assert!(
            result.is_err(),
            "a command that sleeps 60s did not time out"
        );

        let size = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        std::thread::sleep(Duration::from_millis(1500));
        let after = size(&ticks);
        assert!(
            after > 0,
            "the grandchild never ran, so this proves nothing"
        );
        std::thread::sleep(Duration::from_millis(2500));
        assert_eq!(
            size(&ticks),
            after,
            "the command's child kept running after the timeout"
        );
    }

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
