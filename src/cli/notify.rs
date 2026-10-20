//! Telling the operator a long turn finished, or that the agent is waiting.
//!
//! A turn that runs for minutes is a turn the operator walks away from. When
//! it ends — or stops to ask permission — nothing on screen changes in a way
//! anyone in another window would notice. This says so, in the way the
//! operator chose in admin.json:
//!
//! - `auto` (default): OSC 9 on a terminal known to show it as a desktop
//!   notification (iTerm2, WezTerm, Windows Terminal, kitty, Ghostty), a bell
//!   everywhere else. Whether the window has focus is not knowable here, so it
//!   is sent either way; terminals generally show it only when unfocused.
//! - `bell`: the terminal bell.
//! - `desktop`: the platform's notifier — `notify-send` on Linux, `osascript`
//!   on macOS, a PowerShell toast on Windows — with a bell when it cannot run.
//! - `off`: nothing.
//!
//! `notify_command`, when set, runs as well, with `INCODE_EVENT` and
//! `INCODE_MESSAGE` in its environment.
//!
//! # Never in the way
//!
//! Every process is spawned and left to a thread that reaps it. Nothing here
//! waits on a child, and every error is dropped: a notification that failed is
//! not worth a turn.

use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::time::Duration;

/// The default for `notify_after_secs`.
pub const DEFAULT_AFTER: Duration = Duration::from_secs(10);

/// How a notification is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Auto,
    Bell,
    Desktop,
    Off,
}

impl Mode {
    /// Read `notify`. Unset or unrecognised is `auto`: a typo should not
    /// silence notifications nor invent a new kind.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(|r| r.trim().to_ascii_lowercase()).as_deref() {
            Some("bell") => Self::Bell,
            Some("desktop") => Self::Desktop,
            Some("off" | "none" | "false") => Self::Off,
            _ => Self::Auto,
        }
    }
}

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A turn finished after running longer than the threshold.
    Done,
    /// The agent is waiting on the operator: permission, or a question.
    Input,
}

impl Event {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Input => "input",
        }
    }
}

/// The operator's notification settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub mode: Mode,
    pub command: Option<String>,
    pub after: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Auto,
            command: None,
            after: DEFAULT_AFTER,
        }
    }
}

impl Settings {
    #[must_use]
    pub fn from_admin(admin: &crate::admin::AdminConfig) -> Self {
        Self {
            mode: Mode::parse(admin.notify.as_deref()),
            command: admin
                .notify_command
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_string),
            after: admin
                .notify_after_secs
                .map_or(DEFAULT_AFTER, Duration::from_secs),
        }
    }

    /// Whether a turn that took `took` is worth telling anyone about.
    #[must_use]
    pub fn turn_is_worth_it(&self, took: Duration) -> bool {
        took >= self.after
    }
}

/// The settings in force. `None` until the REPL configures them, so a path
/// that never set them (`-t`, `--acp`, a test) notifies nobody.
static SETTINGS: Mutex<Option<Settings>> = Mutex::new(None);

/// Put the operator's settings in force for this process.
pub fn configure(settings: Settings) {
    if let Ok(mut held) = SETTINGS.lock() {
        *held = Some(settings);
    }
}

fn current() -> Option<Settings> {
    SETTINGS.lock().ok().and_then(|held| held.clone())
}

/// A turn ended after `took`. Notifies only past the threshold.
pub fn turn_finished(took: Duration) {
    let Some(settings) = current() else { return };
    if !settings.turn_is_worth_it(took) {
        return;
    }
    let message = format!("Turn finished after {}", human(took));
    deliver(&settings, Event::Done, &message);
}

/// The agent is waiting on the operator.
pub fn waiting_for_input(message: &str) {
    let Some(settings) = current() else { return };
    deliver(&settings, Event::Input, message);
}

fn human(took: Duration) -> String {
    let secs = took.as_secs();
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn deliver(settings: &Settings, event: Event, message: &str) {
    if let Some(command) = &settings.command {
        spawn_shell(command, &command_env(event, message));
    }
    match settings.mode {
        Mode::Off => {}
        Mode::Bell => write_terminal(BELL),
        Mode::Auto => {
            if osc9_supported(|key| std::env::var(key).ok()) {
                write_terminal(&osc9(message));
            } else {
                write_terminal(BELL);
            }
        }
        Mode::Desktop => match desktop_command(std::env::consts::OS, "incode", message) {
            Some((program, args)) => {
                if !spawn_detached(&program, &args, &[]) {
                    write_terminal(BELL);
                }
            }
            None => write_terminal(BELL),
        },
    }
}

const BELL: &str = "\x07";

/// OSC 9: a desktop notification on terminals that support it. Control
/// characters are removed so the message cannot end the sequence early.
#[must_use]
pub fn osc9(message: &str) -> String {
    let clean: String = message.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]9;{clean}\x07")
}

/// Whether the terminal is one known to show OSC 9, from its environment.
///
/// Taken as a lookup so the decision can be tested without the process's own
/// environment.
#[must_use]
pub fn osc9_supported(env: impl Fn(&str) -> Option<String>) -> bool {
    if env("WT_SESSION").is_some() || env("KITTY_WINDOW_ID").is_some() {
        return true;
    }
    matches!(
        env("TERM_PROGRAM").as_deref(),
        Some("iTerm.app" | "WezTerm" | "ghostty")
    )
}

/// The environment a `notify_command` runs with.
#[must_use]
pub fn command_env(event: Event, message: &str) -> Vec<(String, String)> {
    vec![
        ("INCODE_EVENT".to_string(), event.as_str().to_string()),
        ("INCODE_MESSAGE".to_string(), message.to_string()),
    ]
}

/// The platform notifier for `os` (as [`std::env::consts::OS`] names it), or
/// `None` where there is none to try.
#[must_use]
pub fn desktop_command(os: &str, title: &str, message: &str) -> Option<(String, Vec<String>)> {
    match os {
        "linux" | "freebsd" | "openbsd" | "netbsd" => Some((
            "notify-send".to_string(),
            vec![title.to_string(), message.to_string()],
        )),
        "macos" => Some((
            "osascript".to_string(),
            vec![
                "-e".to_string(),
                format!(
                    "display notification \"{}\" with title \"{}\"",
                    applescript_escape(message),
                    applescript_escape(title)
                ),
            ],
        )),
        "windows" => Some((
            "powershell".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                windows_toast_script(title, message),
            ],
        )),
        _ => None,
    }
}

/// A string safe inside an AppleScript double-quoted literal.
#[must_use]
pub fn applescript_escape(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// A string safe inside a PowerShell single-quoted literal.
#[must_use]
pub fn powershell_quote(text: &str) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    format!("'{}'", clean.replace('\'', "''"))
}

/// BurntToast when it is installed, the WinRT toast API otherwise, and the
/// console bell when neither works.
fn windows_toast_script(title: &str, message: &str) -> String {
    let t = powershell_quote(title);
    let m = powershell_quote(message);
    format!(
        "try {{ Import-Module BurntToast -ErrorAction Stop; New-BurntToastNotification -Text {t},{m} }} \
         catch {{ try {{ \
         [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; \
         $x = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
         $s = $x.GetElementsByTagName('text'); $s.Item(0).AppendChild($x.CreateTextNode({t})) > $null; \
         $s.Item(1).AppendChild($x.CreateTextNode({m})) > $null; \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('incode').Show([Windows.UI.Notifications.ToastNotification]::new($x)) \
         }} catch {{ [console]::beep() }} }}"
    )
}

/// Write an escape to the terminal, wherever one is attached.
fn write_terminal(seq: &str) {
    let mut out = std::io::stdout();
    if out.is_terminal() {
        let _ = out.write_all(seq.as_bytes());
        let _ = out.flush();
        return;
    }
    let mut err = std::io::stderr();
    if err.is_terminal() {
        let _ = err.write_all(seq.as_bytes());
        let _ = err.flush();
    }
}

fn spawn_shell(command: &str, env: &[(String, String)]) {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    spawn_detached(shell, &[flag.to_string(), command.to_string()], env);
}

/// Start a process with no stdio and reap it on a thread. `false` when it
/// could not be started.
fn spawn_detached(program: &str, args: &[String], env: &[(String, String)]) -> bool {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for (key, value) in env {
        cmd.env(key, value);
    }
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mode_reads_every_spelling_and_defaults_to_auto() {
        assert_eq!(Mode::parse(None), Mode::Auto);
        assert_eq!(Mode::parse(Some("auto")), Mode::Auto);
        assert_eq!(Mode::parse(Some(" Bell ")), Mode::Bell);
        assert_eq!(Mode::parse(Some("desktop")), Mode::Desktop);
        assert_eq!(Mode::parse(Some("off")), Mode::Off);
        assert_eq!(Mode::parse(Some("loud")), Mode::Auto);
    }

    #[test]
    fn settings_come_from_admin_json() {
        let admin: crate::admin::AdminConfig = serde_json::from_str(
            r#"{"notify":"bell","notify_command":"  say done ","notify_after_secs":3}"#,
        )
        .expect("valid");
        let s = Settings::from_admin(&admin);
        assert_eq!(s.mode, Mode::Bell);
        assert_eq!(s.command.as_deref(), Some("say done"));
        assert!(s.turn_is_worth_it(Duration::from_secs(3)));
        assert!(!s.turn_is_worth_it(Duration::from_secs(2)));

        let none = Settings::from_admin(&crate::admin::AdminConfig::default());
        assert_eq!(none, Settings::default());
        assert!(!none.turn_is_worth_it(Duration::from_secs(9)));
    }

    #[test]
    fn the_command_hears_the_event_and_the_message() {
        let env = command_env(Event::Input, "Bash needs your confirmation");
        assert_eq!(
            env,
            vec![
                ("INCODE_EVENT".to_string(), "input".to_string()),
                (
                    "INCODE_MESSAGE".to_string(),
                    "Bash needs your confirmation".to_string()
                ),
            ]
        );
        assert_eq!(command_env(Event::Done, "x")[0].1, "done");
    }

    #[test]
    fn osc9_cannot_be_ended_early_by_the_message() {
        assert_eq!(osc9("hi\x07there\n"), "\x1b]9;hithere\x07");
    }

    #[test]
    fn osc9_is_used_only_where_the_terminal_is_known_to_show_it() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| (*v).to_string())
            }
        };
        assert!(osc9_supported(env(&[("TERM_PROGRAM", "iTerm.app")])));
        assert!(osc9_supported(env(&[("WT_SESSION", "x")])));
        assert!(!osc9_supported(env(&[("TERM_PROGRAM", "Apple_Terminal")])));
        assert!(!osc9_supported(env(&[])));
    }

    #[test]
    fn each_platform_gets_its_own_notifier_with_the_message_quoted() {
        let (prog, args) = desktop_command("linux", "incode", "done").unwrap();
        assert_eq!(prog, "notify-send");
        assert_eq!(args, vec!["incode", "done"]);

        let (prog, args) = desktop_command("macos", "incode", "say \"hi\"").unwrap();
        assert_eq!(prog, "osascript");
        assert_eq!(
            args[1],
            "display notification \"say \\\"hi\\\"\" with title \"incode\""
        );

        let (prog, args) = desktop_command("windows", "incode", "it's done").unwrap();
        assert_eq!(prog, "powershell");
        assert!(args[3].contains("'it''s done'"), "{}", args[3]);
        assert!(args[3].contains("BurntToast"));

        assert!(desktop_command("plan9", "incode", "x").is_none());
    }

    #[test]
    fn durations_read_as_a_person_would_say_them() {
        assert_eq!(human(Duration::from_secs(42)), "42s");
        assert_eq!(human(Duration::from_secs(125)), "2m 5s");
    }
}
