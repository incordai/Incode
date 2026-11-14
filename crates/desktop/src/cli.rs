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
    incu routine list         show every learned routine, each step in full
    incu routine keep <WORDS>     keep a routine from expiring (only you can)
    incu routine unkeep <WORDS>   let it expire again, thirty days from now
    incu routine bind <WORDS> <PARAM> <ENV_VAR>
                              read a routine's secret {PARAM} from the
                              environment variable ENV_VAR (named INCU_PARAM_...)
                              of incu's own environment, for the steps as they are
                              now. Only you bind; the variable's NAME is stored.
                              Binding stops accidental or stale use. It does NOT
                              stop an agent that can write your app-data folder or
                              run incu itself: run agents sandboxed (insbx), where
                              that folder and the incu binary are out of reach.
    incu routine unbind <WORDS> <PARAM>   remove that binding
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

OPTIONS for routine:
    --file <PATH>             the routines file (else the config's, else the
                              one serve uses)
    --config <FILE>           the config to find it in

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
    /// List, keep or unkeep learned routines: what only the person does.
    Routine(RoutineArgs),
}

/// What `incu routine` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineAction {
    /// Show every routine, with every step in full.
    List,
    /// Keep a routine from expiring.
    Keep,
    /// Let a kept routine expire again (thirty days from now).
    Unkeep,
    /// Bind a parameter of a routine to an environment variable of incu's own.
    Bind,
    /// Remove such a binding.
    Unbind,
}

/// The arguments of `incu routine`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineArgs {
    /// What to do.
    pub action: RoutineAction,
    /// The phrase naming the routine, for keep, unkeep, bind and unbind. Matched
    /// as a request is: by its words, in any order, filler ignored.
    pub phrase: String,
    /// The parameter, for bind and unbind.
    pub param: String,
    /// The environment variable (its NAME), for bind.
    pub var: String,
    /// The routines file, when named; else the config's, else the default.
    pub file: Option<PathBuf>,
    /// The config file named on the command line, if any.
    pub config: Option<PathBuf>,
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
        Some("routine") => {
            args.remove(0);
            parse_routine(args)
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

fn parse_routine(args: Vec<String>) -> Result<Command, String> {
    let mut it = args.into_iter();
    let action = match it.next().as_deref() {
        Some("list") => RoutineAction::List,
        Some("keep") => RoutineAction::Keep,
        Some("unkeep") => RoutineAction::Unkeep,
        Some("bind") => RoutineAction::Bind,
        Some("unbind") => RoutineAction::Unbind,
        Some("-h" | "--help") => return Ok(Command::Print(USAGE.to_string())),
        Some(other) => {
            return Err(format!(
                "unknown routine command `{other}` (list, keep, unkeep, bind or unbind)\n\n{USAGE}"
            ))
        }
        None => {
            return Err(format!(
                "routine needs list, keep, unkeep, bind or unbind\n\n{USAGE}"
            ))
        }
    };
    let mut words = Vec::new();
    let mut file = None;
    let mut config = None;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Print(USAGE.to_string())),
            "--file" => file = Some(PathBuf::from(value(&arg, &mut it)?)),
            "--config" => config = Some(PathBuf::from(value(&arg, &mut it)?)),
            flag if flag.starts_with('-') => {
                return Err(format!("unknown option `{flag}` for routine\n\n{USAGE}"))
            }
            _ => words.push(arg),
        }
    }
    // Bind and unbind end with the parameter (and, for bind, the variable); what
    // comes before is the words that name the routine.
    let (mut param, mut var) = (String::new(), String::new());
    match action {
        RoutineAction::Bind => {
            if words.len() < 3 {
                return Err(
                    "incu routine bind <the words that ask for the routine> <param> \
                            <ENV_VAR>"
                        .to_string(),
                );
            }
            var = words.pop().unwrap_or_default();
            param = words.pop().unwrap_or_default();
        }
        RoutineAction::Unbind => {
            if words.len() < 2 {
                return Err(
                    "incu routine unbind <the words that ask for the routine> <param>".to_string(),
                );
            }
            param = words.pop().unwrap_or_default();
        }
        _ => {}
    }
    let phrase = words.join(" ");
    match (action, phrase.trim().is_empty()) {
        (RoutineAction::List, false) => Err("routine list takes no phrase".to_string()),
        (
            RoutineAction::Keep
            | RoutineAction::Unkeep
            | RoutineAction::Bind
            | RoutineAction::Unbind,
            true,
        ) => Err("name the routine: incu routine keep <the words that ask for it>".to_string()),
        _ => Ok(Command::Routine(RoutineArgs {
            action,
            phrase,
            param,
            var,
            file,
            config,
        })),
    }
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

/// The routines file `incu routine` works on: `--file`, else the config's
/// `routines`, else `$INCU_ROUTINES`, else the per-user data folder: the file
/// `incu serve` uses.
///
/// # Errors
///
/// The config cannot be read, or there is no place to keep routines.
pub fn routine_store(
    file: Option<PathBuf>,
    config: Option<PathBuf>,
) -> Result<crate::routine::store::Store, String> {
    use crate::routine::store::Store;
    let path = match file {
        Some(path) => Some(path),
        None => {
            let config = match &config {
                Some(path) => Config::load(path)?,
                None => Config::load_default()?,
            };
            config.routines.or_else(Store::default_path)
        }
    };
    path.map(Store::at)
        .ok_or_else(|| "no place to keep routines: set INCU_ROUTINES or pass --file".to_string())
}

/// `incu routine`: what only the person does to the routines an agent saved.
///
/// **Why this is a command and not a tool.** A routine a person has kept never
/// expires. If an agent could say "keep this", it could exempt its own saves from
/// the thirty days that exist so that nothing it learned lives on unreviewed. So
/// keeping is done here, at a terminal, by the person, and no tool has an
/// argument for it.
///
/// # Errors
///
/// The file cannot be read or written, or no routine matches the phrase.
pub fn run_routine(args: RoutineArgs) -> Result<String, String> {
    let store = routine_store(args.file, args.config)?;
    let now = crate::routine::now_ms();
    match args.action {
        RoutineAction::List => {
            let file = store.load()?;
            let mut out = format!("routines in {}\n", store.path().display());
            let mut any = false;
            for routine in file.live(now) {
                any = true;
                let status = match routine.days_left(now) {
                    Some(days) => format!("expires in {days} days unless used"),
                    None => "kept: never expires".to_string(),
                };
                out.push_str(&format!("\n\"{}\" ({status})\n", routine.phrase));
                for (i, step) in routine.steps.iter().enumerate() {
                    out.push_str(&format!("  {}. {}\n", i + 1, step.review()));
                }
                // What it needs the person to bind, and what is bound: the
                // variable's NAME and whether it is set here, never its value.
                let needs = crate::routine::params::names(&routine.steps);
                let stale = !routine.bindings.is_empty()
                    && routine.bound_steps != crate::routine::params::digest(&routine.steps);
                for param in &needs {
                    match routine.bindings.get(param) {
                        Some(var) if stale => out.push_str(&format!(
                            "  {{{{{param}}}}} bound to {var}, but the steps are not the ones it \
                             was bound for: bind it again\n"
                        )),
                        Some(var) => out.push_str(&format!(
                            "  {{{{{param}}}}} bound to {var} ({})\n",
                            if std::env::var_os(var).is_some_and(|v| !v.is_empty()) {
                                "set in this environment"
                            } else {
                                "NOT set in this environment"
                            }
                        )),
                        None => out.push_str(&format!(
                            "  {{{{{param}}}}} NOT BOUND: incu routine bind \"{}\" {param} {}{}\n",
                            routine.phrase,
                            crate::routine::params::ENV_PREFIX,
                            param.to_uppercase()
                        )),
                    }
                }
            }
            if !any {
                out.push_str("\n(none)\n");
            }
            Ok(out.trim_end().to_string())
        }
        RoutineAction::Bind => {
            let (phrase, param, var) = (args.phrase, args.param, args.var);
            store.update(|file| {
                file.bind(&phrase, &param, &var, now)?;
                Ok(format!(
                    "bound {{{{{param}}}}} of \"{phrase}\" to {var}. Set {var} in the \
                     environment where incu is launched: {}",
                    if std::env::var_os(&var).is_some_and(|v| !v.is_empty()) {
                        "it is set in this environment"
                    } else {
                        "it is NOT set in this environment"
                    }
                ))
            })
        }
        RoutineAction::Unbind => {
            let (phrase, param) = (args.phrase, args.param);
            store.update(|file| {
                if file.unbind(&phrase, &param, now) {
                    Ok(format!(
                        "{{{{{param}}}}} of \"{phrase}\" is no longer bound"
                    ))
                } else {
                    Err(format!(
                        "\"{phrase}\" has no binding for {param:?}; `incu routine list` shows \
                         what is bound"
                    ))
                }
            })
        }
        RoutineAction::Keep | RoutineAction::Unkeep => {
            let keep = args.action == RoutineAction::Keep;
            let phrase = args.phrase;
            store.update(|file| {
                if file.set_taught(&phrase, keep, now) {
                    Ok(if keep {
                        format!("kept \"{phrase}\": it will not expire")
                    } else {
                        format!("\"{phrase}\" will expire after thirty days unused, from now")
                    })
                } else {
                    Err(format!(
                        "no routine matches \"{phrase}\"; `incu routine list` shows what is saved"
                    ))
                }
            })
        }
    }
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
        Command::Routine(args) => {
            println!("{}", run_routine(args)?);
            Ok(())
        }
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

    fn routine_args(words: &[&str]) -> RoutineArgs {
        match parse(argv(words)).expect("parses") {
            Command::Routine(args) => args,
            other => panic!("expected routine, got {other:?}"),
        }
    }

    #[test]
    fn routine_keep_and_unkeep_take_the_words_that_ask_for_the_routine() {
        let keep = routine_args(&["routine", "keep", "open", "the", "weekly", "report"]);
        assert_eq!(keep.action, RoutineAction::Keep);
        assert_eq!(keep.phrase, "open the weekly report");
        let unkeep = routine_args(&["routine", "unkeep", "report", "--file", "r.json"]);
        assert_eq!(unkeep.action, RoutineAction::Unkeep);
        assert_eq!(unkeep.file.as_deref(), Some(Path::new("r.json")));
        assert_eq!(
            routine_args(&["routine", "list"]).action,
            RoutineAction::List
        );
        // A routine is named, a list is not, and a typo is not guessed.
        assert!(parse(argv(&["routine", "keep"])).is_err());
        assert!(parse(argv(&["routine", "list", "extra"])).is_err());
        assert!(parse(argv(&["routine", "teach", "x"])).is_err());
        assert!(parse(argv(&["routine"])).is_err());
        assert!(parse(argv(&["routine", "keep", "x", "--taught"])).is_err());
    }

    /// Only this command sets "kept"; keeping stops the expiry and unkeeping
    /// starts it again.
    #[test]
    fn keeping_a_routine_is_done_here_and_stops_it_expiring() {
        use crate::routine::store::{Routines, Store};
        let dir = std::env::temp_dir().join(format!("incu-cli-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("routines.json");
        let store = Store::at(file.clone());
        let mut routines = Routines::default();
        routines
            .learn(
                "open notes",
                vec![crate::routine::Step::new(
                    "key",
                    serde_json::json!({"keys": ["ctrl", "n"], "in_window": "app=notes"}),
                )],
                false,
                crate::routine::now_ms(),
            )
            .unwrap();
        store.save(&routines).unwrap();
        let args = |action, phrase: &str| RoutineArgs {
            action,
            phrase: phrase.to_string(),
            param: String::new(),
            var: String::new(),
            file: Some(file.clone()),
            config: None,
        };
        assert!(store.load().unwrap().routines[0]
            .days_left(crate::routine::now_ms())
            .is_some());
        let said = run_routine(args(RoutineAction::Keep, "notes open")).unwrap();
        assert!(said.contains("will not expire"), "{said}");
        let kept = store.load().unwrap();
        assert!(kept.routines[0].taught);
        assert_eq!(kept.routines[0].days_left(crate::routine::now_ms()), None);
        let listed = run_routine(args(RoutineAction::List, "")).unwrap();
        assert!(listed.contains("kept: never expires"), "{listed}");
        assert!(
            listed.contains("ctrl"),
            "every step is shown in full: {listed}"
        );
        run_routine(args(RoutineAction::Unkeep, "open notes")).unwrap();
        assert!(!store.load().unwrap().routines[0].taught);
        let missing = run_routine(args(RoutineAction::Keep, "open mail")).unwrap_err();
        assert!(missing.contains("no routine matches"), "{missing}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn routine_bind_takes_the_routine_words_then_the_param_then_the_variable() {
        let bind = routine_args(&[
            "routine",
            "bind",
            "unlock",
            "the",
            "vault",
            "password",
            "INCU_PARAM_VAULT",
            "--file",
            "r.json",
        ]);
        assert_eq!(bind.action, RoutineAction::Bind);
        assert_eq!(bind.phrase, "unlock the vault");
        assert_eq!(
            (bind.param.as_str(), bind.var.as_str()),
            ("password", "INCU_PARAM_VAULT")
        );
        let unbind = routine_args(&["routine", "unbind", "unlock", "vault", "password"]);
        assert_eq!(unbind.action, RoutineAction::Unbind);
        assert_eq!(unbind.phrase, "unlock vault");
        assert_eq!(unbind.param, "password");
        // Too few words: a routine, a param and a variable are all needed.
        assert!(parse(argv(&["routine", "bind", "vault", "password"])).is_err());
        assert!(parse(argv(&["routine", "bind"])).is_err());
        assert!(parse(argv(&["routine", "unbind", "password"])).is_err());
    }

    /// The person binds; the file keeps the variable's NAME and a digest of the
    /// steps, never a value.
    #[test]
    fn binding_is_the_persons_command_and_stores_a_name_not_a_value() {
        use crate::routine::store::{Routines, Store};
        let dir = std::env::temp_dir().join(format!("incu-cli-bind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("routines.json");
        let store = Store::at(file.clone());
        let mut routines = Routines::default();
        routines
            .learn(
                "unlock the vault",
                vec![crate::routine::Step::new(
                    "type_text",
                    serde_json::json!({"text": "{{password}}", "in_window": "app=vault"}),
                )],
                false,
                crate::routine::now_ms(),
            )
            .unwrap();
        store.save(&routines).unwrap();
        let args = |action, param: &str, var: &str| RoutineArgs {
            action,
            phrase: "vault unlock".to_string(),
            param: param.to_string(),
            var: var.to_string(),
            file: Some(file.clone()),
            config: None,
        };
        std::env::set_var("INCU_PARAM_CLI_TEST_BIND", "hunter2-secret");
        let said = run_routine(args(
            RoutineAction::Bind,
            "password",
            "INCU_PARAM_CLI_TEST_BIND",
        ))
        .unwrap();
        assert!(said.contains("INCU_PARAM_CLI_TEST_BIND"), "{said}");
        assert!(!said.contains("hunter2"), "{said}");
        let on_disk = std::fs::read_to_string(&file).unwrap();
        assert!(on_disk.contains("INCU_PARAM_CLI_TEST_BIND"), "{on_disk}");
        assert!(
            !on_disk.contains("hunter2"),
            "a value reached the file: {on_disk}"
        );
        let routine = &store.load().unwrap().routines[0];
        assert_eq!(routine.bindings["password"], "INCU_PARAM_CLI_TEST_BIND");
        assert_eq!(
            routine.bound_steps,
            crate::routine::params::digest(&routine.steps)
        );
        // The listing says what is bound and whether it is set, with no value.
        let listed = run_routine(args(RoutineAction::List, "", "")).unwrap();
        assert!(
            listed.contains("bound to INCU_PARAM_CLI_TEST_BIND (set in this environment)"),
            "{listed}"
        );
        assert!(!listed.contains("hunter2"), "{listed}");
        // Refused: another variable of the environment, a param it does not take,
        // a routine that is not there.
        let home = run_routine(args(RoutineAction::Bind, "password", "HOME")).unwrap_err();
        assert!(home.contains("INCU_PARAM_"), "{home}");
        let nope = run_routine(args(RoutineAction::Bind, "pasword", "INCU_PARAM_X")).unwrap_err();
        assert!(nope.contains("takes: password"), "{nope}");
        let mut elsewhere = args(RoutineAction::Bind, "password", "INCU_PARAM_X");
        elsewhere.phrase = "open mail".to_string();
        assert!(run_routine(elsewhere)
            .unwrap_err()
            .contains("no routine matches"));
        // Unbinding removes it.
        run_routine(args(RoutineAction::Unbind, "password", "")).unwrap();
        assert!(store.load().unwrap().routines[0].bindings.is_empty());
        assert!(store.load().unwrap().routines[0].bound_steps.is_empty());
        let again = run_routine(args(RoutineAction::Unbind, "password", "")).unwrap_err();
        assert!(again.contains("no binding"), "{again}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What binding is and is not, said where the person reads about the command.
    #[test]
    fn the_bind_help_says_it_is_not_a_boundary_against_an_agent_that_can_write_files() {
        let help = USAGE.split("OPTIONS for serve").next().unwrap();
        let bind = help.split("incu routine bind").nth(1).unwrap();
        let bind = bind.split("incu routine unbind").next().unwrap();
        for said in [
            "stops accidental or stale use",
            "does NOT",
            "app-data folder",
            "insbx",
        ] {
            assert!(
                bind.contains(said),
                "the bind help does not say {said:?}: {bind}"
            );
        }
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
