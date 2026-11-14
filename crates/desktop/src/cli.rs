//! What `incu` does, as functions both binaries call: serve the MCP server over
//! stdio, and stop a running one.
//!
//! # Where this sits
//!
//! The server is a separate PROCESS on purpose. Windows per-monitor DPI
//! awareness is set once, before anything reads the screen, and cannot be
//! changed afterwards, so it needs a startup it owns; and a crash in desktop
//! control must not take down a live harness session. Two entry points start
//! that process:
//!
//! - `incu` (`src/main.rs`), which parses its own argv with [`parse`].
//! - incode's `--desktop-mcp` (`src/main.rs` in incode), a thin wrapper that
//!   keeps every existing agent config and installer working: it reads its own
//!   `admin.json`, then calls [`serve`] with what it found.
//!
//! # `-p` is an alias of `--tier`
//!
//! incode has always passed `-p <mode>` on the argv of the child it launches,
//! always and never omitted when it is the default, because the child cannot see
//! the flag the session was started with. `-p read-only` therefore keeps
//! working here, through [`Tier::from_mode_word`]; `--tier observe|act` is the
//! crate's own spelling of the same two rungs ([`crate::permission`]).
//!
//! # Nothing on stdout
//!
//! Stdout is the protocol channel, and a banner there is a parse error at the
//! client. Everything [`serve`] says goes to stderr.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::hooks::{HookConfig, Hooks, ShellHooks};
use crate::limits::DesktopLimits;
use crate::mcp::Server;
use crate::permission::Tier;

/// The usage text of `incu`.
pub const USAGE: &str = "\
incu - computer use for an agent, as an MCP server over stdio

USAGE:
    incu serve [OPTIONS]      speak MCP on stdin/stdout (the default command)
    incu stop [OPTIONS]       halt the desktop a running server drives
    incu --help | --version

OPTIONS for serve:
    --tier <observe|act>      what the agent may do (default: act).
                              observe = look at the screen, never touch it
    -p, --permission-mode <M> an alias of --tier that takes incode's mode words:
                              read-only = observe; workspace-write,
                              danger-full-access, allow = act; prompt = nothing
    --journal <PATH>          append what the desktop did to this file, and
                              watch for the stop file beside it
    --config <FILE>           limits, display and hooks (JSON; incode's
                              admin.json is read as it stands). Default:
                              $INCU_CONFIG, else the platform config folder
    --display <SPEC>          Linux: \":N\" or \"xvfb[:WxH]\" (overrides the config)

ENVIRONMENT for serve:
    INCU_ROUTINES             the file learned routines are kept in (else the
                              config's `routines`, else routines.json in the
                              per-user data folder)
    INCORD_MEMORY_URL         the local memory engine routine_save reads
                              Watchman records from (default
                              http://127.0.0.1:9180; loopback only)

OPTIONS for stop:
    --journal <PATH>          stop the server journalling to PATH
    --workspace <DIR>         stop the server for DIR's .incode/ journal
                              (default: the current directory)

Nothing but the protocol is written to stdout.";

/// A command line, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Print this and exit successfully.
    Print(String),
    /// Run the server.
    Serve(ServeArgs),
    /// Ask a running server to stop.
    Stop(StopArgs),
}

/// The flags of `incu serve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeArgs {
    /// What the agent may do. [`Tier::Act`] when no flag named one.
    pub tier: Tier,
    /// Where to append what the desktop did; `None` means nobody is watching.
    pub journal: Option<PathBuf>,
    /// The config file named on the command line, if any.
    pub config: Option<PathBuf>,
    /// The Linux display named on the command line, if any.
    pub display: Option<String>,
}

/// The flags of `incu stop`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopArgs {
    /// The journal whose stop file to write, if one was named.
    pub journal: Option<PathBuf>,
    /// The workspace whose journal it is, when no journal was named.
    pub workspace: Option<PathBuf>,
}

/// Parse `incu`'s arguments (without the program name).
///
/// No subcommand and a leading flag means `serve`, so a client configured with
/// bare flags starts the server.
///
/// # Errors
///
/// An unknown flag or subcommand, a flag missing its value, or a mode that
/// names nothing: a typo in a security flag is reported, never resolved to
/// something.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command, String> {
    let mut args: Vec<String> = args.into_iter().collect();
    match args.first().map(String::as_str) {
        None | Some("serve") => {
            if !args.is_empty() {
                args.remove(0);
            }
            parse_serve(args)
        }
        Some("stop") => {
            args.remove(0);
            parse_stop(args)
        }
        Some("-h" | "--help" | "help") => Ok(Command::Print(USAGE.to_string())),
        Some("--version" | "-V") => Ok(Command::Print(format!(
            "incu {}",
            env!("CARGO_PKG_VERSION")
        ))),
        Some(flag) if flag.starts_with('-') => parse_serve(args),
        Some(other) => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
}

fn value(flag: &str, it: &mut std::vec::IntoIter<String>) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn parse_serve(args: Vec<String>) -> Result<Command, String> {
    let mut tier: Option<Tier> = None;
    let mut journal = None;
    let mut config = None;
    let mut display = None;
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "-h" | "--help" => return Ok(Command::Print(USAGE.to_string())),
            "--tier" | "-p" | "--permission-mode" => {
                let word = value(&flag, &mut it)?;
                let named = if flag == "--tier" {
                    Tier::parse(&word)
                        .ok_or_else(|| format!("--tier takes `observe` or `act`, not `{word}`"))?
                } else {
                    Tier::from_mode_word(&word).ok_or_else(|| {
                        format!(
                            "{flag} does not know the mode `{word}` \
                             (read-only, workspace-write, danger-full-access, allow or prompt)"
                        )
                    })?
                };
                if tier.is_some_and(|already| already != named) {
                    return Err("--tier and -p name different tiers".to_string());
                }
                tier = Some(named);
            }
            "--journal" => journal = Some(PathBuf::from(value(&flag, &mut it)?)),
            "--config" => config = Some(PathBuf::from(value(&flag, &mut it)?)),
            "--display" => display = Some(value(&flag, &mut it)?),
            other => return Err(format!("unknown option `{other}` for serve\n\n{USAGE}")),
        }
    }
    Ok(Command::Serve(ServeArgs {
        tier: tier.unwrap_or(Tier::Act),
        journal,
        config,
        display,
    }))
}

fn parse_stop(args: Vec<String>) -> Result<Command, String> {
    let mut journal = None;
    let mut workspace = None;
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "-h" | "--help" => return Ok(Command::Print(USAGE.to_string())),
            "--journal" => journal = Some(PathBuf::from(value(&flag, &mut it)?)),
            "--workspace" => workspace = Some(PathBuf::from(value(&flag, &mut it)?)),
            other => return Err(format!("unknown option `{other}` for stop\n\n{USAGE}")),
        }
    }
    if journal.is_some() && workspace.is_some() {
        return Err("stop takes --journal or --workspace, not both".to_string());
    }
    Ok(Command::Stop(StopArgs { journal, workspace }))
}

/// Everything [`serve`] needs, already resolved.
pub struct ServeOptions {
    /// What the agent may do.
    pub tier: Tier,
    /// Where to append what the desktop did, and the stop file beside it.
    pub journal: Option<PathBuf>,
    /// The limits in force once resolved; refused if a safety limit is loosened.
    pub limits: DesktopLimits,
    /// Linux: the display to drive (see [`crate::linux::attach`]). Unused
    /// elsewhere.
    pub display: Option<String>,
    /// The operator's commands around every call, if any.
    pub hooks: Option<Box<dyn Hooks>>,
    /// Where routines are kept; `None` is the default
    /// ([`crate::routine::store::Store::default_path`]).
    pub routines: Option<PathBuf>,
    /// What the startup lines on stderr call this server (`incu`, or
    /// `incode-desktop` for incode's wrapper).
    pub name: &'static str,
}

impl ServeOptions {
    /// What `incu serve` runs with: the flags, over the config file they name
    /// (or the default one).
    ///
    /// # Errors
    ///
    /// The config file cannot be read or is malformed; the desktop will not
    /// start without the guard the operator wrote.
    pub fn from_args(args: ServeArgs) -> Result<Self, String> {
        let config = match &args.config {
            Some(path) => Config::load(path)?,
            None => Config::load_default()?,
        };
        let Config {
            limits,
            display,
            hooks,
            routines,
        } = config;
        Ok(Self {
            tier: args.tier,
            journal: args.journal,
            limits,
            display: args.display.or(display),
            hooks: shell_hooks(hooks),
            routines,
            name: "incu",
        })
    }
}

/// The routines kit `incu` runs with: the file at `path`, else the default, and
/// Watchman's records from the local memory engine as the source. `None` when
/// there is no place to keep the file.
///
/// The engine is only asked when a routine is saved from record ids, and an
/// engine that is not running is an error at that moment, not at startup: the
/// crate works without it.
fn routine_kit(path: Option<PathBuf>) -> Option<crate::mcp::RoutineKit> {
    let path = path.or_else(crate::routine::store::Store::default_path)?;
    Some(crate::mcp::RoutineKit::new(
        crate::routine::store::Store::at(path),
        Box::new(crate::routine::watchman::WatchmanSource::from_env()),
    ))
}

/// [`ShellHooks`] for `config`, or none when it has nothing to run.
fn shell_hooks(config: HookConfig) -> Option<Box<dyn Hooks>> {
    (!config.is_empty()).then(|| Box::new(ShellHooks::new(config)) as Box<dyn Hooks>)
}

/// Run the MCP server on stdio until the client goes away.
///
/// Synchronous on this thread, deliberately: the protocol is a line-oriented
/// pipe and the input thread is its own OS thread, so an async runtime would add
/// a scheduler to a program that has nothing to schedule.
///
/// The steps, in the order they have always run: install the limits (refusing
/// one that loosens a safety limit), on Linux attach the display FIRST while
/// this process is still one thread (attaching sets `DISPLAY`, which the input
/// thread reads when it connects, and the display is held until the server
/// returns so a private Xvfb lives exactly as long as the desktop it backs),
/// start the input thread, build the [`Server`], then [`crate::mcp::run_stdio`].
///
/// # Errors
///
/// The limits are refused, the display cannot be attached, the input thread
/// cannot start, or reading stdin fails. The text is what the operator is shown.
pub fn serve(options: ServeOptions) -> Result<(), String> {
    let ServeOptions {
        tier,
        journal,
        limits,
        display,
        hooks,
        routines,
        name,
    } = options;
    crate::limits::set(
        limits
            .resolve()
            .map_err(|e| format!("the desktop will not start: {e}"))?,
    );
    #[cfg(target_os = "linux")]
    let _display = {
        let attached = crate::linux::attach(display.as_deref()).map_err(|e| format!("{e}"))?;
        eprintln!(
            "{name} on display {}{}",
            attached.name(),
            if attached.is_private() {
                " (a private Xvfb, stopped when this server ends)"
            } else {
                ""
            }
        );
        attached
    };
    // Only Linux has a display to name; the others drive the one they are in.
    #[cfg(not(target_os = "linux"))]
    let _ = display;
    let input = crate::thread::InputThread::start()
        .map_err(|e| format!("could not start desktop input: {e}"))?;
    let mut server = Server::new(input).with_tier(tier);
    if let Some(hooks) = hooks {
        let before = hooks.pre_tool_count();
        if before > 0 {
            eprintln!("{name}: {before} pre-tool hook(s) will run before every action");
        }
        server = server.with_hook_runner(hooks);
    }
    if let Some(path) = journal {
        eprintln!("{name} journalling to {}", path.display());
        server = server.with_journal(path);
    }
    server = match routine_kit(routines) {
        Some(kit) => {
            eprintln!("{name}: routines in {}", kit.path().display());
            server.with_routines(kit)
        }
        None => {
            eprintln!(
                "{name}: routines are off: no data folder to keep them in (set INCU_ROUTINES)"
            );
            server
        }
    };
    // Named on stderr so an operator who asked for `read-only` can see that it
    // arrived. A capability restriction nobody can observe is one nobody trusts.
    eprintln!("{name} MCP server ready on stdio ({})", tier.label());
    crate::mcp::run_stdio(server).map_err(|e| e.to_string())
}

/// Ask the server whose stop file is `path` to stop, and say so on stdout.
///
/// REPORTED, never swallowed: a stop request that silently failed is worse than
/// no stop button at all, because the operator walks away believing the desktop
/// is halted.
///
/// # Errors
///
/// The stop file could not be written.
pub fn stop(path: &Path) -> Result<(), String> {
    crate::safety::request_stop(path)
        .map_err(|e| format!("could not request a stop at {}: {e}", path.display()))?;
    println!("desktop stop requested: {}", path.display());
    println!(
        "It takes effect before the desktop's next action, and is permanent \
         for that session. Delete the file to allow a NEW session to act."
    );
    Ok(())
}

/// Run a parsed `incu` command. The `incu` binary's whole body.
///
/// # Errors
///
/// Whatever [`serve`] or [`stop`] refuses with.
pub fn run(command: Command) -> Result<(), String> {
    match command {
        Command::Print(text) => {
            println!("{text}");
            Ok(())
        }
        Command::Serve(args) => serve(ServeOptions::from_args(args)?),
        Command::Stop(args) => {
            let path = match (args.journal, args.workspace) {
                (Some(journal), _) => crate::safety::stop_file_beside(&journal),
                (None, Some(workspace)) => crate::safety::stop_path_in(&workspace),
                (None, None) => crate::safety::stop_path_in(
                    &std::env::current_dir().map_err(|e| format!("no current directory: {e}"))?,
                ),
            };
            stop(&path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    fn serve_args(words: &[&str]) -> ServeArgs {
        match parse(argv(words)).expect("parses") {
            Command::Serve(args) => args,
            other => panic!("expected serve, got {other:?}"),
        }
    }

    #[test]
    fn serve_is_the_default_command_and_act_the_default_tier() {
        assert_eq!(serve_args(&[]).tier, Tier::Act);
        assert_eq!(serve_args(&["serve"]).tier, Tier::Act);
        // Bare flags start the server too.
        assert_eq!(serve_args(&["--tier", "observe"]).tier, Tier::Observe);
    }

    /// `-p` is the argv incode has always passed to the child it launches.
    #[test]
    fn dash_p_is_an_alias_of_tier_and_takes_incodes_mode_words() {
        assert_eq!(serve_args(&["-p", "read-only"]).tier, Tier::Observe);
        assert_eq!(serve_args(&["-p", "workspace-write"]).tier, Tier::Act);
        assert_eq!(serve_args(&["serve", "-p", "prompt"]).tier, Tier::Deny);
        assert_eq!(
            serve_args(&["--permission-mode", "danger-full-access"]).tier,
            Tier::Act
        );
        // The exact argv incode's client config emits, minus the flag that
        // selects incode's own server mode.
        let args = serve_args(&["--journal", "/w/.incode/desktop.jsonl", "-p", "read-only"]);
        assert_eq!(args.tier, Tier::Observe);
        assert_eq!(
            args.journal.as_deref(),
            Some(Path::new("/w/.incode/desktop.jsonl"))
        );
    }

    #[test]
    fn a_typo_in_a_security_flag_is_refused_not_guessed() {
        assert!(parse(argv(&["-p", "wrokspace-write"])).is_err());
        assert!(parse(argv(&["--tier", "read-only"])).is_err());
        assert!(parse(argv(&["--tier"])).is_err());
    }

    #[test]
    fn two_tiers_that_disagree_are_refused() {
        assert!(parse(argv(&["--tier", "observe", "-p", "workspace-write"])).is_err());
        assert!(parse(argv(&["--tier", "act", "-p", "workspace-write"])).is_ok());
    }

    #[test]
    fn stop_takes_a_journal_or_a_workspace() {
        assert!(matches!(
            parse(argv(&["stop", "--journal", "j.jsonl"])),
            Ok(Command::Stop(_))
        ));
        assert!(parse(argv(&["stop", "--journal", "a", "--workspace", "b"])).is_err());
    }

    #[test]
    fn unknown_commands_and_options_are_refused() {
        assert!(parse(argv(&["frobnicate"])).is_err());
        assert!(parse(argv(&["serve", "--frobnicate"])).is_err());
    }

    #[test]
    fn help_and_version_print_instead_of_serving() {
        assert_eq!(
            parse(argv(&["--help"])),
            Ok(Command::Print(USAGE.to_string()))
        );
        assert!(matches!(
            parse(argv(&["--version"])),
            Ok(Command::Print(v)) if v.starts_with("incu ")
        ));
    }

    /// The usage must name every flag the parser takes, or an operator cannot
    /// find the one that matters.
    #[test]
    fn the_usage_names_every_flag() {
        for flag in [
            "--tier",
            "-p",
            "--permission-mode",
            "--journal",
            "--config",
            "--display",
            "--workspace",
        ] {
            assert!(USAGE.contains(flag), "{flag} is not in the usage");
        }
    }
}
