//! Computer use, as incode sees it: the `incord-desktop` crate (`crates/desktop`,
//! binary `incu`) under its old path, and the few things that name incode.
//!
//! # Where this sits
//!
//! The desktop began as this module and moved out whole, for the reasons in the
//! `incord-desktop` crate header: it does not depend on incode, and incode
//! depends on it as it does on `incord-sandbox`. `pub use incord_desktop::*`
//! keeps every `crate::desktop::…` path in incode (`serve::http`, `runtime::
//! cancel`, `migrate`, `admin`, `cli`, the toolbox) as it was; rewriting them to
//! name the crate is a later step.
//!
//! Three things are not re-exported as they are, because they are incode's:
//!
//! - [`mcp::client_config`], [`mcp::client_config_json`] and
//!   [`mcp::codex_config_overrides`] build the registration an agent CLI needs
//!   to launch `incode --desktop-mcp`. They name incode's flags and take
//!   incode's `PermissionMode`, so they stayed.
//! - [`safety::request_stop`] is the crate's, behind incode's own
//!   `folders::create_dir_all`, which also writes the `.gitignore` of a fresh
//!   `.incode/`. The crate uses plain `std::fs::create_dir_all`.
//! - [`serve`] is `incode --desktop-mcp`: a thin wrapper that hands the crate's
//!   [`cli::serve`] the settings from `admin.json` and the operator's hook
//!   engine ([`crate::hooks::HookRunner`], so the commands run exactly as they
//!   always have, in the shell and with the process-tree containment incode's
//!   hooks use).
//!
//! # Wired to
//!
//! - **Called by** `main.rs` (`Parsed::DesktopMcp`) for [`serve`];
//!   `cli::agent_session` and `agent::codex` for [`mcp::codex_config_overrides`].
//! - **Calls** `incord_desktop` for everything else.

use std::path::PathBuf;

pub use incord_desktop::*;

use incord_desktop::hooks::{HookOutcome, Hooks};
use incord_desktop::permission::Tier;

use crate::admin::AdminConfig;
use crate::permissions::PermissionMode;

pub mod mcp {
    //! The server (the crate's), and the registration of it that names incode.

    pub use incord_desktop::mcp::*;

    use serde_json::{json, Value};

    use crate::permissions::PermissionMode;

    /// The name a client will see this server under.
    pub const CLIENT_KEY: &str = "desktop";

    /// The MCP configuration a client needs in order to launch this server.
    ///
    /// Verified against the installed Claude CLI 2.1.273: `--mcp-config <configs...>`
    /// takes JSON, and `--strict-mcp-config` confines the agent to only what is
    /// passed there. Codex takes the same information as
    /// `-c mcp_servers.<key>.…` instead (`src/agent/codex/argv.rs`), which is why
    /// this returns the SHAPE rather than a command line — each backend spells it
    /// differently, and the thing they agree on is what to launch.
    ///
    /// `exe` is the path to this binary. Taken as an argument rather than read from
    /// [`std::env::current_exe`] inside, so it is testable and so a caller can
    /// register a server it is not itself running.
    /// `journal` is where the launched server should record what it did, so
    /// `--serve` can stream it (`COMPUTER_USE.md` 14c). `None` means nobody is
    /// watching and the server writes nowhere.
    pub fn client_config(
        exe: &std::path::Path,
        journal: Option<&std::path::Path>,
        mode: PermissionMode,
    ) -> Value {
        let mut args = vec!["--desktop-mcp".to_string()];
        if let Some(path) = journal {
            args.push("--desktop-journal".to_string());
            args.push(path.to_string_lossy().to_string());
        }
        // ALWAYS PASSED, never omitted when it happens to be the default. This
        // server is a separate process and cannot see the flag the session was
        // started with; leaving it off would mean the operator's `-p read-only`
        // stopped at the process boundary, which is exactly the bug that made a
        // read-only session able to type.
        args.push("-p".to_string());
        args.push(mode.as_str().to_string());
        json!({
            "mcpServers": {
                CLIENT_KEY: {
                    "command": exe.to_string_lossy(),
                    "args": args,
                }
            }
        })
    }

    /// [`client_config`] as the compact JSON string a `--mcp-config` flag takes.
    pub fn client_config_json(
        exe: &std::path::Path,
        journal: Option<&std::path::Path>,
        mode: PermissionMode,
    ) -> String {
        client_config(exe, journal, mode).to_string()
    }

    /// The same registration as `-c key=value` overrides, which is how Codex takes
    /// it.
    ///
    /// Verified against the installed CLI: `codex mcp add --help` documents `-c`
    /// as "Override a configuration value… The `value` portion is parsed as TOML",
    /// and its own example passes an array —
    /// `-c 'sandbox_permissions=["disk-full-read-access"]'` — so the `args` list
    /// below is TOML, not JSON, and not a guess.
    ///
    /// Returned as pairs rather than a command line because the caller knows how
    /// its own `Command` wants them.
    pub fn codex_config_overrides(
        exe: &std::path::Path,
        journal: Option<&std::path::Path>,
        mode: PermissionMode,
    ) -> Vec<String> {
        // TOML basic strings take backslash escapes, so a Windows path must have
        // its separators doubled or `C:\Users\…` becomes an invalid escape. This
        // applies to the JOURNAL path too, which is also a Windows path — the
        // exe is not a special case, the string syntax is.
        let toml = |p: &std::path::Path| p.to_string_lossy().replace('\\', "\\\\");
        let args = match journal {
            Some(path) => format!(
                "[\"--desktop-mcp\",\"--desktop-journal\",\"{}\",\"-p\",\"{}\"]",
                toml(path),
                mode.as_str()
            ),
            None => format!("[\"--desktop-mcp\",\"-p\",\"{}\"]", mode.as_str()),
        };
        vec![
            format!("mcp_servers.{CLIENT_KEY}.command=\"{}\"", toml(exe)),
            format!("mcp_servers.{CLIENT_KEY}.args={args}"),
        ]
    }
}

pub mod safety {
    //! The kill switch (the crate's), with incode's `.incode/` folder handling on
    //! the one function that creates a file.

    pub use incord_desktop::safety::*;

    /// Ask the desktop at this path to stop.
    ///
    /// [`incord_desktop::safety::request_stop`] behind incode's
    /// `folders::create_dir_all`, so a stop file written into a fresh `.incode/`
    /// brings that folder's `.gitignore` with it, as it did when this code was
    /// in incode. Called by `serve::http`, `runtime::cancel` and `main.rs`
    /// (`--desktop-stop`).
    ///
    /// # Errors
    ///
    /// Returns the I/O error. A caller must REPORT it rather than swallow it: a
    /// stop request that silently failed is worse than no stop button at all.
    pub fn request_stop(path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            crate::folders::create_dir_all(parent)?;
        }
        incord_desktop::safety::request_stop(path)
    }
}

/// incode's hook engine, as the desktop server's [`Hooks`].
///
/// The commands, their matchers, the shell they run in and the way they are
/// stopped at the time limit are all `crate::hooks::HookRunner`'s, unchanged.
struct OperatorHooks {
    runner: crate::hooks::HookRunner,
    pre_tool: usize,
}

impl OperatorHooks {
    fn outcome(ran: crate::hooks::HookRunResult) -> HookOutcome {
        let messages = ran.messages().to_vec();
        if ran.is_denied() {
            HookOutcome::deny(messages)
        } else {
            HookOutcome::allow(messages)
        }
    }
}

impl Hooks for OperatorHooks {
    fn pre_tool_count(&self) -> usize {
        self.pre_tool
    }

    fn run_pre_tool_use(&self, tool_name: &str, tool_input: &str) -> HookOutcome {
        Self::outcome(self.runner.run_pre_tool_use(tool_name, tool_input))
    }

    fn run_post_tool_use(
        &self,
        tool_name: &str,
        tool_input: &str,
        tool_output: &str,
        is_error: bool,
    ) -> HookOutcome {
        Self::outcome(
            self.runner
                .run_post_tool_use(tool_name, tool_input, tool_output, is_error),
        )
    }
}

/// `incode --desktop-mcp`: serve the desktop on stdio under `mode`, with the
/// limits, display and hooks `admin` holds, until the client goes away.
///
/// Called by `main.rs` (`Parsed::DesktopMcp`) once it has loaded `admin.json`.
/// The whole of the work is [`cli::serve`]; what this adds is incode's:
///
/// - the session's [`PermissionMode`] becomes the crate's [`Tier`] through the
///   word `-p` has always carried ([`Tier::from_mode_word`]);
/// - the operator's hooks are run by incode's own engine ([`OperatorHooks`]);
/// - the journal's folder is made with `folders::create_dir_all` first, so a
///   fresh `.incode/` gets its `.gitignore` before the server writes into it.
///
/// # Errors
///
/// What [`cli::serve`] refuses with, as text.
pub fn serve(
    journal: Option<PathBuf>,
    mode: PermissionMode,
    admin: AdminConfig,
) -> Result<(), String> {
    if let Some(parent) = journal.as_deref().and_then(std::path::Path::parent) {
        // Not fatal: a journal that cannot be written is a lost view of the
        // work, not a reason to refuse it (`Journal::record`'s own contract).
        let _ = crate::folders::create_dir_all(parent);
    }
    let hooks = admin.desktop_hooks;
    let hooks: Option<Box<dyn Hooks>> = (!hooks.is_empty()).then(|| {
        Box::new(OperatorHooks {
            pre_tool: hooks.pre_tool_use.len(),
            runner: crate::hooks::HookRunner::new(hooks),
        }) as Box<dyn Hooks>
    });
    cli::serve(cli::ServeOptions {
        // Every mode word incode emits is one the crate knows; failing closed
        // costs nothing if that ever stops being true.
        tier: Tier::from_mode_word(mode.as_str()).unwrap_or(Tier::Deny),
        journal,
        limits: admin.desktop_limits,
        display: admin.desktop_display,
        hooks,
        // incode's `admin.json` has no setting for it: `$INCU_ROUTINES`, else
        // the crate's per-user data folder, the same file `incu` uses.
        routines: None,
        name: "incode-desktop",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    /// The config has to name the flag that actually starts the server.
    ///
    /// If these two ever drift, the client launches the binary in its
    /// NORMAL mode — which starts the interactive harness on a pipe, prints
    /// human text to stdout, and looks to the client like a server speaking
    /// gibberish. That is a long debugging session for a one-word mistake.
    #[test]
    fn the_client_config_launches_this_binary_in_server_mode() {
        let config = mcp::client_config(
            std::path::Path::new("C:/bin/incode.exe"),
            None,
            PermissionMode::WorkspaceWrite,
        );
        let server = &config["mcpServers"][mcp::CLIENT_KEY];

        assert_eq!(server["command"], "C:/bin/incode.exe");
        assert_eq!(
            server["args"],
            json!(["--desktop-mcp", "-p", "workspace-write"]),
            "the config must pass the flag that selects the server mode"
        );

        // With a journal, the path is passed as its own argument — one the
        // server can only receive this way, since it holds no workspace.
        let watched = mcp::client_config(
            std::path::Path::new("C:/bin/incode.exe"),
            Some(std::path::Path::new("C:/work/.incode/desktop.jsonl")),
            PermissionMode::ReadOnly,
        );
        assert_eq!(
            watched["mcpServers"][mcp::CLIENT_KEY]["args"],
            json!([
                "--desktop-mcp",
                "--desktop-journal",
                "C:/work/.incode/desktop.jsonl",
                "-p",
                "read-only"
            ]),
            "the session's mode must reach the child process on its argv"
        );

        // The same string main.rs matches on, so a rename breaks here
        // rather than at a client six months later.
        assert!(
            USAGE_FLAG_IN_MAIN.contains("--desktop-mcp"),
            "the flag in the config no longer matches the one main.rs parses"
        );
    }

    /// The flag `src/main.rs` parses. Kept beside the config that emits it.
    const USAGE_FLAG_IN_MAIN: &str = "--desktop-mcp";

    #[test]
    fn the_config_is_valid_json_for_a_command_line() {
        let text = mcp::client_config_json(
            std::path::Path::new("/usr/local/bin/incode"),
            None,
            PermissionMode::WorkspaceWrite,
        );
        let parsed: Value = serde_json::from_str(&text).expect("must be valid JSON");
        assert_eq!(
            parsed["mcpServers"]["desktop"]["command"],
            "/usr/local/bin/incode"
        );
        // Compact, because it is passed as one shell argument.
        assert!(!text.contains('\n'), "a multi-line config breaks the flag");
    }

    /// **The crate's tier and incode's policy must say the same thing.** The
    /// server now runs the tier; `PermissionPolicy::for_desktop` stays as incode's
    /// statement of the same table, and this is what keeps the two from
    /// drifting: every mode, every one of the 23 tools, the same verdict and the
    /// same words in a refusal.
    #[test]
    fn the_crates_tier_agrees_with_the_desktop_permission_policy() {
        use crate::permissions::{PermissionOutcome, PermissionPolicy};
        // The tables walked below are the tools the server offers: one added
        // to the server and to neither table would be compared with nothing.
        let offered: Vec<String> = incord_desktop::mcp::tool_definitions()
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect();
        assert_eq!(offered.len(), 23, "{offered:?}");
        for name in &offered {
            assert!(
                permission::OBSERVING.contains(&name.as_str())
                    || permission::ACTING.contains(&name.as_str()),
                "{name} is offered by the desktop server but has no tier"
            );
        }
        // `wait_for` and `watch` only look, and `routine_list` and
        // `routine_match` only read the routines file: a read-only session may
        // use them. Saving, forgetting or running a routine acts.
        for looking in ["wait_for", "watch", "routine_list", "routine_match"] {
            assert!(permission::OBSERVING.contains(&looking), "{looking}");
        }
        for acting in ["routine_save", "routine_run", "routine_forget"] {
            assert!(permission::ACTING.contains(&acting), "{acting}");
        }
        let modes = [
            PermissionMode::ReadOnly,
            PermissionMode::WorkspaceWrite,
            PermissionMode::DangerFullAccess,
            PermissionMode::Prompt,
            PermissionMode::Allow,
        ];
        for mode in modes {
            let tier = Tier::from_mode_word(mode.as_str()).expect("a mode word");
            let policy = PermissionPolicy::for_desktop(mode);
            for tool in permission::OBSERVING.iter().chain(permission::ACTING) {
                let from_policy = match policy.authorize(tool, "{}", None) {
                    PermissionOutcome::Allow => Ok(()),
                    PermissionOutcome::Deny { reason } => Err(reason),
                };
                assert_eq!(
                    tier.authorise(tool),
                    from_policy,
                    "{} on {tool}",
                    mode.as_str()
                );
            }
        }
    }

    /// incode's hook engine reaches the server through the adapter with its
    /// verdict intact: exit 2 refuses, and what the command printed is the reason.
    #[test]
    fn the_operators_hook_engine_refuses_through_the_adapter() {
        let deny = if cfg!(windows) {
            "cmd /C echo blocked: %HOOK_TOOL_NAME% & exit 2"
        } else {
            "echo \"blocked: $HOOK_TOOL_NAME\"; exit 2"
        };
        let hooks = OperatorHooks {
            pre_tool: 1,
            runner: crate::hooks::HookRunner::new(crate::hooks::HookConfig {
                pre_tool_use: vec![deny.into()],
                ..Default::default()
            }),
        };
        let ran = hooks.run_pre_tool_use("click", "{}");
        assert!(ran.is_denied(), "{ran:?}");
        assert_eq!(ran.messages(), ["blocked: click"]);
        assert_eq!(hooks.pre_tool_count(), 1);
    }
}
