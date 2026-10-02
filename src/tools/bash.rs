//! Bash tool — execute shell commands with timeout.
//!
//! The shell is [`super::shell_choice`]'s: `sh -c` on macOS and Linux, and on
//! Windows Git Bash, else PowerShell — never cmd.exe.

use std::path::Path;
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
///
/// Each piece of output is handed to `on_output` as the command prints it —
/// stdout and stderr in the order they arrived — and the command, with
/// everything it started, is stopped as soon as `stopped` answers true. What
/// was streamed is a view of the run, not a second copy of its answer: the
/// result is the whole output, bounded.
///
/// # Errors
///
/// The input names no command, the shell could not start, the command ran
/// past its timeout, or it was stopped.
pub fn execute_bash_watched(
    workspace: &Path,
    input: &str,
    filter_env: bool,
    sandbox: &crate::sandbox::Sandbox,
    stopped: &dyn Fn() -> bool,
    on_output: &mut dyn FnMut(&[u8]),
) -> Result<String> {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or_else(|_| serde_json::json!({"command": input}));

    let command = parsed["command"]
        .as_str()
        .context("missing 'command' field")?;
    let timeout_ms = parsed["timeout"].as_u64().unwrap_or(DEFAULT_TIMEOUT_MS);

    // The command runs under the session's OS sandbox where the platform can
    // confine it, and through the plain shell where it cannot — `wrap` decides,
    // and traces which. The shell is chosen for the sandbox: on Windows the
    // token that confines a command cannot start Git Bash.
    if !sandbox.is_off() {
        crate::trace!("sandbox", "{}", sandbox.note());
    }
    let shell = super::shell_choice::for_sandbox(sandbox).map_err(|why| anyhow::anyhow!(why))?;
    let mut child = sandbox.wrap(&shell, command);

    // Nothing is typed into a tool's command: its stdin is the null device,
    // so one that waits for input sees end of input at once instead of
    // holding the terminal the REPL reads from until its timeout.
    child
        .current_dir(workspace)
        .stdin(std::process::Stdio::null())
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

    // Wait with timeout, handing on what it prints as it prints it.
    let output = wait_streaming(
        spawned,
        Duration::from_millis(timeout_ms),
        &Stop {
            now: stopped,
            said: "the turn was stopped, so the command was stopped too",
        },
        on_output,
    )?;

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
    child: std::process::Child,
    timeout: Duration,
    abandon: &dyn Fn() -> bool,
) -> Result<std::process::Output> {
    wait_streaming(
        child,
        timeout,
        &Stop {
            now: abandon,
            said: "stopped before it finished: the answer came from elsewhere",
        },
        &mut |_| {},
    )
}

/// When a wait is given up early, and what that is called.
struct Stop<'a> {
    now: &'a dyn Fn() -> bool,
    said: &'a str,
}

/// How long the waiter sleeps between looks at the child when nothing
/// arrives: a stop or a timeout lands within this.
const WAIT_POLL: Duration = Duration::from_millis(50);

/// Which pipe a piece of output came from.
enum Piece {
    Out(Vec<u8>),
    Err(Vec<u8>),
    /// One pipe reached its end.
    Closed,
}

/// Read `pipe` to its end on a thread of its own, sending each piece as it
/// arrives.
fn drain(
    pipe: Option<Box<dyn std::io::Read + Send>>,
    to: std::sync::mpsc::Sender<Piece>,
    wrap: fn(Vec<u8>) -> Piece,
) {
    std::thread::spawn(move || {
        if let Some(mut pipe) = pipe {
            let mut buf = [0u8; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if to.send(wrap(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
        }
        let _ = to.send(Piece::Closed);
    });
}

/// The one bounded waiter: [`wait_with_timeout_or`], also handing each piece
/// of output to `on_output` as it arrives.
///
/// The pipes are read while waiting (see [`wait_with_timeout`]); after the
/// child exits both are read to their end, as before, so nothing it printed
/// is lost.
fn wait_streaming(
    mut child: std::process::Child,
    timeout: Duration,
    stop: &Stop<'_>,
    on_output: &mut dyn FnMut(&[u8]),
) -> Result<std::process::Output> {
    let tree = crate::agent::process_tree::Tree::adopt(&child);
    let (to, arriving) = std::sync::mpsc::channel::<Piece>();
    drain(
        child.stdout.take().map(|p| Box::new(p) as _),
        to.clone(),
        Piece::Out,
    );
    drain(
        child.stderr.take().map(|p| Box::new(p) as _),
        to,
        Piece::Err,
    );
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut open = 2;
    // `None` is time passing with nothing new: handed on as an empty piece,
    // so a watcher holding output back can still show it.
    let mut take = |piece: Option<Piece>, open: &mut i32| match piece {
        Some(Piece::Out(bytes)) => {
            on_output(&bytes);
            stdout.extend_from_slice(&bytes);
        }
        Some(Piece::Err(bytes)) => {
            on_output(&bytes);
            stderr.extend_from_slice(&bytes);
        }
        Some(Piece::Closed) => *open -= 1,
        None => on_output(&[]),
    };

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if (stop.now)() {
                    tree.kill(&mut child);
                    let _ = child.wait();
                    anyhow::bail!("{}", stop.said);
                }
                if start.elapsed() > timeout {
                    tree.kill(&mut child);
                    let _ = child.wait();
                    // The readers are not waited for: something outside the
                    // tree could still hold a pipe, and a timeout must not
                    // become a hang. They end when the pipes close.
                    anyhow::bail!("command timed out after {}s", timeout.as_secs());
                }
                // Waiting on the output rather than sleeping: what arrives is
                // handed on at once, and a quiet child is looked at again
                // within the poll.
                match arriving.recv_timeout(WAIT_POLL) {
                    Ok(piece) => {
                        take(Some(piece), &mut open);
                        while let Ok(more) = arriving.try_recv() {
                            take(Some(more), &mut open);
                        }
                    }
                    // Both pipes are closed and the child still runs: nothing
                    // more can arrive, so this is an ordinary wait.
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        std::thread::sleep(WAIT_POLL);
                        take(None, &mut open);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => take(None, &mut open),
                }
            }
            Err(e) => return Err(e.into()),
        }
    };
    // Everything the child printed, to the end of both pipes.
    while open > 0 {
        match arriving.recv() {
            Ok(piece) => take(Some(piece), &mut open),
            Err(_) => break,
        }
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No sandbox: these tests are about the shell tool, not confinement.
    fn no_sandbox() -> crate::sandbox::Sandbox {
        crate::sandbox::Sandbox::off()
    }

    /// Whether the tool's shell here is a POSIX one (Git Bash on Windows, `sh`
    /// elsewhere) rather than the PowerShell fallback.
    fn posix() -> bool {
        crate::tools::shell_choice::for_sandbox(&no_sandbox())
            .is_ok_and(|s| s.flavor() == crate::tools::shell_choice::Flavor::Posix)
    }

    /// `posix` in a POSIX shell, `powershell` in PowerShell.
    fn either(posix: &str, powershell: &str) -> String {
        if self::posix() { posix } else { powershell }.to_string()
    }

    /// **THE BUG THIS TOOL HAD**: `mkdir -p a/b && echo $((1+2))` ran through
    /// cmd.exe on Windows, which made a directory literally called `-p` and
    /// printed the arithmetic unexpanded. Through a real shell it makes
    /// `a/b`, prints 3, and no `-p` appears.
    #[test]
    fn a_posix_command_runs_as_posix() {
        let dir = tempfile::tempdir().expect("dir");
        let input = serde_json::json!({ "command": "mkdir -p a/b && echo $((1+2))" }).to_string();
        let said = execute_bash_watched(
            dir.path(),
            &input,
            true,
            &no_sandbox(),
            &|| false,
            &mut |_| {},
        )
        .expect("runs");
        assert_eq!(said.trim(), "3", "{said}");
        assert!(dir.path().join("a").join("b").is_dir(), "a/b was not made");
        assert!(!dir.path().join("-p").exists(), "a `-p` directory appeared");
    }

    /// A line, two seconds of nothing, another line.
    fn slow_two_lines() -> String {
        either(
            "echo first; sleep 2; echo second",
            "echo first; Start-Sleep 2; echo second",
        )
    }

    /// **OUTPUT IS HANDED ON AS THE COMMAND PRINTS IT**, not when it ends:
    /// the first line arrives while the command is still sleeping, and the
    /// result is still the whole output.
    #[test]
    fn output_is_handed_on_while_the_command_runs() {
        let dir = tempfile::tempdir().expect("dir");
        let started = std::time::Instant::now();
        let mut first_at = None;
        let mut seen = Vec::new();
        let input = serde_json::json!({ "command": slow_two_lines() }).to_string();
        let said = execute_bash_watched(
            dir.path(),
            &input,
            false,
            &no_sandbox(),
            &|| false,
            &mut |piece| {
                seen.extend_from_slice(piece);
                if first_at.is_none() && String::from_utf8_lossy(&seen).contains("first") {
                    first_at = Some(started.elapsed());
                }
            },
        )
        .expect("runs");
        let whole = started.elapsed();
        let first_at = first_at.expect("the first line was handed on");
        assert!(
            first_at + Duration::from_millis(1000) < whole,
            "the first line came at {first_at:?}, the end at {whole:?}"
        );
        assert!(said.contains("first") && said.contains("second"), "{said}");
    }

    /// **A STOPPED TURN STOPS ITS COMMAND**, and everything it started, at
    /// once rather than at the command's own timeout.
    #[test]
    fn a_stop_ends_the_command_at_once() {
        let dir = tempfile::tempdir().expect("dir");
        let asked = std::time::Instant::now();
        let long = either("sleep 30", "Start-Sleep 30");
        let input = serde_json::json!({ "command": long }).to_string();
        let stop_at = asked + Duration::from_millis(300);
        let err = execute_bash_watched(
            dir.path(),
            &input,
            false,
            &no_sandbox(),
            &|| std::time::Instant::now() > stop_at,
            &mut |_| {},
        )
        .expect_err("stopped");
        assert!(err.to_string().contains("stopped"), "{err}");
        assert!(
            asked.elapsed() < Duration::from_secs(10),
            "{:?}",
            asked.elapsed()
        );
    }

    /// A shell command that prints `lines` lines of about 70 bytes each.
    fn loud(lines: u32) -> std::process::Command {
        use std::process::Command;
        #[cfg(windows)]
        {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(format!(
                "for /L %i in (1,1,{lines}) do @echo line %i padding padding padding padding \
                 padding padding padding"
            ));
            c
        }
        #[cfg(not(windows))]
        {
            let mut c = Command::new("sh");
            c.arg("-c").arg(format!(
                "i=1; while [ $i -le {lines} ]; do echo \"line $i padding padding padding \
                 padding padding padding padding\"; i=$((i+1)); done"
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

    /// Echo a variable in the tool's shell.
    fn echo_var(name: &str) -> String {
        either(&format!("echo ${name}"), &format!("echo $env:{name}"))
    }

    fn run(command: &str, filter_env: bool) -> String {
        let input = serde_json::json!({ "command": command }).to_string();
        execute_bash_watched(
            std::path::Path::new("."),
            &input,
            filter_env,
            &no_sandbox(),
            &|| false,
            &mut |_| {},
        )
        .expect("the shell command should run")
    }

    /// A secret in this process's environment must not reach a command the
    /// model chose. The bash tool previously inherited the full parent
    /// environment, so `env_filter_enabled` had no effect on it at all.
    #[test]
    fn filtered_shell_cannot_see_an_unlisted_secret() {
        // Deliberately NOT prefixed `INCODE_` or `INCORD_`: env_filter.rs
        // allowlists both whole prefixes, so such a canary would be passed
        // through by design and the test would prove nothing.
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
        let dir = std::env::temp_dir().join("incode-bash-truncation-test");
        std::fs::create_dir_all(&dir).expect("test fixture directory");
        let path = dir.join("wide.txt");
        let mut content = String::from("a");
        content.push_str(&"\u{4e2d}".repeat(20_000));
        assert!(
            content.len() > MAX_OUTPUT_CHARS,
            "the fixture must exceed the cap or the test proves nothing"
        );
        std::fs::write(&path, &content).expect("write fixture");

        let command = either(
            &format!("cat \"{}\"", path.display()),
            &format!("Get-Content -Raw -Encoding UTF8 \"{}\"", path.display()),
        );

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

    /// Set only in the child [`a_command_that_reads_input_is_not_handed_ours`]
    /// starts.
    const STDIN_CHILD: &str = "INCODE_BASH_STDIN_CHILD";

    /// The child half: runs a command that reads a line, with this process's
    /// stdin a pipe its parent holds open and never writes, and prints what
    /// the tool said. Returns at once — passing — unless started as the
    /// child.
    #[test]
    fn bash_stdin_child() {
        if std::env::var_os(STDIN_CHILD).is_none() {
            return;
        }
        let command = either(
            "read x; echo got",
            "[Console]::In.ReadLine() | Out-Null; echo got",
        );
        let input = serde_json::json!({ "command": command, "timeout": 15_000 }).to_string();
        let said = execute_bash_watched(
            Path::new("."),
            &input,
            false,
            &no_sandbox(),
            &|| false,
            &mut |_| {},
        );
        println!("TOOL-SAID: {said:?}");
        std::process::exit(0);
    }

    /// **A COMMAND THAT WAITS FOR INPUT IS NOT HANDED THE TERMINAL.** In the
    /// REPL this process's stdin is the person's terminal: a foreground
    /// command that read from it hung until its timeout. Its stdin is the null
    /// device, so `read` sees end of input at once. The child's stdin here is
    /// a pipe held open and never written — what a terminal nobody types into
    /// looks like to `read`.
    #[test]
    fn a_command_that_reads_input_is_not_handed_ours() {
        use std::io::Read as _;
        let mut child = std::process::Command::new(std::env::current_exe().expect("exe"))
            .args([
                "--exact",
                "tools::bash::tests::bash_stdin_child",
                "--nocapture",
                "--test-threads=1",
                "-q",
            ])
            .env(STDIN_CHILD, "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn");
        // Held open, never written, until the child is done.
        let held = child.stdin.take();
        let began = std::time::Instant::now();
        let mut out = String::new();
        child
            .stdout
            .take()
            .expect("stdout")
            .read_to_string(&mut out)
            .expect("read");
        let _ = child.wait();
        drop(held);
        let took = began.elapsed();
        assert!(out.contains("TOOL-SAID: Ok(\"got"), "{out}");
        assert!(took < Duration::from_secs(10), "took {took:?}: {out}");
    }
}
