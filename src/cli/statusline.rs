//! The operator's own status line, on the input box's rule.
//!
//! `statusline_command` in admin.json names a shell command. It is run with
//! the session as one JSON object on stdin and the first line it prints
//! replaces the facts the rule normally shows. It runs at the start and after
//! each turn, under a short timeout; a command that is slow, fails or prints
//! nothing leaves the last good line (or the built-in facts) in place.
//!
//! The JSON:
//!
//! ```json
//! {"cwd": "/work/app", "workspace": "app", "model": "claude-opus-5-5",
//!  "session_id": "…", "git_branch": "main", "git_dirty": false,
//!  "tokens": {"input": 0, "output": 0, "cache_write": 0, "cache_read": 0, "total": 0},
//!  "cost_usd": 0.0}
//! ```
//!
//! `cost_usd` is `null` for a model the price table does not know.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use serde::Serialize;

/// How long the command may take before its output is given up on.
pub const TIMEOUT: Duration = Duration::from_millis(1500);

/// Tokens, as the status line command reads them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub total: u64,
}

/// What the status line command is told about the session.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Input {
    pub cwd: String,
    pub workspace: String,
    pub model: String,
    pub session_id: Option<String>,
    pub git_branch: Option<String>,
    pub git_dirty: bool,
    pub tokens: Tokens,
    pub cost_usd: Option<f64>,
}

impl Input {
    /// Build from a session's spend, pricing each model it used.
    #[must_use]
    pub fn with_usage(mut self, by_model: &[(String, crate::runtime::usage::TokenUsage)]) -> Self {
        let mut tokens = Tokens::default();
        let mut cost = Some(0.0);
        for (model, used) in by_model {
            tokens.input += u64::from(used.input_tokens);
            tokens.output += u64::from(used.output_tokens);
            tokens.cache_write += u64::from(used.cache_creation_input_tokens);
            tokens.cache_read += u64::from(used.cache_read_input_tokens);
            cost = match (cost, used.estimate_cost_usd(Some(model))) {
                (Some(sum), Some(this)) => Some(sum + this),
                _ => None,
            };
        }
        tokens.total = tokens.input + tokens.output + tokens.cache_write + tokens.cache_read;
        self.tokens = tokens;
        self.cost_usd = cost;
        self
    }

    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// The line the rule shows: the first non-blank line of the output, with
/// terminal escapes and control characters removed (the rule measures
/// characters, and an escape would count as several that take no room).
#[must_use]
pub fn first_line(output: &str) -> Option<String> {
    let line = output.lines().find(|l| !l.trim().is_empty())?;
    let plain = strip_escapes(line);
    let plain = plain.trim();
    (!plain.is_empty()).then(|| plain.to_string())
}

fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                // CSI: up to and including a final byte in @..~
                Some('[') => {
                    chars.next();
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ST
                Some(']') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// Run `command` with `input` on stdin; the first line it printed, or `None`
/// on failure, timeout or silence.
#[must_use]
pub fn run(
    command: &str,
    input: &str,
    workdir: &std::path::Path,
    timeout: Duration,
) -> Option<String> {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut child = std::process::Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(workdir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // Written on a thread: a command that never reads its stdin must not
    // block this one on a full pipe.
    if let Some(mut stdin) = child.stdin.take() {
        let body = input.to_string();
        std::thread::spawn(move || {
            let _ = stdin.write_all(body.as_bytes());
        });
    }
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let began = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if began.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    first_line(&reader.join().ok()?)
}

/// The command and the last line it gave.
#[derive(Debug, Clone)]
pub struct StatusLine {
    command: String,
    cached: Option<String>,
}

impl StatusLine {
    /// `None` when no command is configured.
    #[must_use]
    pub fn from_admin(admin: &crate::admin::AdminConfig) -> Option<Self> {
        let command = admin.statusline_command.as_deref()?.trim();
        (!command.is_empty()).then(|| Self {
            command: command.to_string(),
            cached: None,
        })
    }

    /// Run the command again; keep the last good line when this run gives none.
    pub fn refresh(&mut self, input: &Input, workdir: &std::path::Path) -> Option<&str> {
        if let Some(line) = run(&self.command, &input.to_json(), workdir, TIMEOUT) {
            self.cached = Some(line);
        }
        self.cached.as_deref()
    }

    #[must_use]
    pub fn cached(&self) -> Option<&str> {
        self.cached.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::usage::TokenUsage;

    #[test]
    fn the_json_carries_every_field_a_status_line_needs() {
        let used = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 10,
            cache_creation_input_tokens: 20,
            cache_read_input_tokens: 30,
        };
        let input = Input {
            cwd: "/w/app".to_string(),
            workspace: "app".to_string(),
            model: "claude-sonnet-5-5".to_string(),
            session_id: Some("s1".to_string()),
            git_branch: Some("main".to_string()),
            ..Input::default()
        }
        .with_usage(&[("claude-sonnet-5-5".to_string(), used)]);
        let v: serde_json::Value = serde_json::from_str(&input.to_json()).unwrap();
        assert_eq!(v["cwd"], "/w/app");
        assert_eq!(v["model"], "claude-sonnet-5-5");
        assert_eq!(v["session_id"], "s1");
        assert_eq!(v["git_branch"], "main");
        assert_eq!(v["tokens"]["input"], 1_000_000);
        assert_eq!(v["tokens"]["total"], 1_000_060);
        assert!(v["cost_usd"].as_f64().unwrap() > 1.99);
    }

    #[test]
    fn an_unpriced_model_makes_the_cost_null_not_partial() {
        let used = TokenUsage {
            input_tokens: 5,
            ..TokenUsage::default()
        };
        let input = Input::default().with_usage(&[
            ("claude-opus-5-5".to_string(), used),
            ("gemini-3".to_string(), used),
        ]);
        assert_eq!(input.cost_usd, None);
        assert_eq!(Input::default().with_usage(&[]).cost_usd, Some(0.0));
    }

    #[test]
    fn only_the_first_non_blank_line_is_shown_without_escapes() {
        assert_eq!(
            first_line("\n  \x1b[32mmain\x1b[0m $0.12\nsecond\n").as_deref(),
            Some("main $0.12")
        );
        assert_eq!(
            first_line("\x1b]8;;http://x\x07link\x1b]8;;\x07").as_deref(),
            Some("link")
        );
        assert_eq!(first_line(""), None);
        assert_eq!(first_line("\n \n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_command_reads_the_json_and_a_slow_one_is_given_up_on() {
        let dir = std::env::temp_dir();
        let got = run("head -c 7", r#"{"cwd":"/x"}"#, &dir, TIMEOUT);
        assert_eq!(got.as_deref(), Some(r#"{"cwd":"#));
        let slow = run("sleep 5; echo late", "{}", &dir, Duration::from_millis(100));
        assert_eq!(slow, None);
        assert_eq!(run("exit 0", "{}", &dir, TIMEOUT), None);
    }
}
