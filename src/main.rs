//! `incode` — entry point for the coding harness CLI.
//!
//! The command is `incode`; the crate is still `incord-code`. A crate is named
//! once, a command is typed all day.
//!
//! The crate had no binary for the CLI itself, only the memory MCP server, so
//! everything here could be built and tested but never actually run.

use std::path::PathBuf;

use anyhow::Context;
use incord_code::{IncordCodeConfig, PermissionMode};

const USAGE: &str = "\
incode — autonomous coding harness

USAGE:
    incode [OPTIONS]

OPTIONS:
    -m, --model <NAME>        Model to use (or set INCODE_MODEL)
    -w, --workspace <PATH>    Workspace root (default: current directory)
    -p, --permission-mode <M> read-only | workspace-write | danger-full-access | prompt | allow
        --no-env-filter       Pass the full environment to spawned commands
        --admin               Print the operator's settings and where each came from
        --serve               Watch this workspace and serve what it is doing,
                              read-only, on 127.0.0.1 (see --port)
        --port <N>            Pin --serve to this port. Without it a free one is
                              chosen and written to .incode/serve.json
        --desktop-mcp         Speak MCP on stdin/stdout so an agent can see and
                              use this machine's screen. Launched BY an MCP
                              client, not by hand; stdout is the protocol.
        --desktop-journal <P> With --desktop-mcp, append what the desktop did to
                              this file so --serve can stream it to a watcher
        --acp                 Speak the Agent Client Protocol on stdin/stdout, as
                              the agent an editor drives. Launched BY an ACP
                              client, not by hand; stdout is the protocol.
                              Takes -p (read-only | workspace-write |
                              danger-full-access), -m and --no-env-filter
        --team-mcp            Speak MCP on stdin/stdout so a running agent can
                              reach its teammates. Needs --team-channel
        --team-channel <P>    The conversation --team-mcp joins
        --sandbox-exec        Apply this platform's OS sandbox to this process,
                              then exec the shell command after `--`. Launched BY
                              incode's Bash tool on Unix, not by hand
        --install-tools       Download, check (sha256) and unpack every tool in
                              admin.json `tools`, then exit
        --tool-server         Speak MCP on stdin/stdout, offering incode's tools to
                              an agent CLI that incode started. Runs nothing
                              itself: every call goes back to that incode.
                              Launched BY the agent CLI, not by hand
        --check-findings <P>  Exit 0 if P holds real findings, non-zero if not.
                              This is the CHECK for a research task, run by the
                              harness against its own deliverable — see
                              docs/RUNNER.md
        --desktop-stop        Halt the desktop driving this workspace (see -w).
                              Takes effect before its next action, permanently
                              for that session. Works while the harness is busy
    -t, --task <TEXT>         Run one instruction with an installed agent CLI
                              (uses its subscription; needs no API credential)
        --output-format <F>   With -t: text (default) | stream-json (one JSON
                              event per line: init, text, thinking, tool_use,
                              tool_result, usage, result) | json (the result
                              object alone). In the JSON forms stdout is JSON
                              only; everything else goes to stderr
    -r, --resume <ID>         Continue an agent session by its id
        --add-dir <PATH>      Also work in this directory (repeatable)
    -h, --help                Print this help
    -V, --version             Print version
";

fn main() -> anyhow::Result<()> {
    // BEFORE ANYTHING ELSE, and before any thread starts: this mode applies the
    // OS sandbox to THIS process and then execs the command, and an early
    // Landlock ruleset binds only the thread that set it. On success it never
    // returns; it runs only on Unix (Windows launches a confined child
    // instead), so the argument is simply passed through to the ordinary
    // parser elsewhere.
    {
        let raw: Vec<String> = std::env::args().skip(1).collect();
        if raw.first().map(String::as_str) == Some("--sandbox-exec") {
            #[cfg(unix)]
            {
                incord_code::sandbox::exec(&raw[1..])?;
                unreachable!("sandbox::exec replaces this process on success");
            }
            #[cfg(windows)]
            {
                let code = incord_code::sandbox::run_windows_helper(&raw[1..])?;
                std::process::exit(code);
            }
        }
    }
    // NOTE: the crate emits `tracing` events (MCP connection failures, tool
    // errors) but no subscriber is installed, so they currently go nowhere.
    // Installing one needs a new dependency, which is a decision to take
    // deliberately rather than as a side effect of adding this entry point.
    let parsed = parse_args(std::env::args().skip(1))?;
    // Before anything reads `~/.incode/` or a project's `.incode/`: state left
    // under the old names (`~/.incord-code/`, `.incord/`) is carried over.
    // stderr, never stdout: several modes speak a protocol on stdout.
    let mut migrated = Vec::new();
    if parsed.reads_data_dir() {
        migrated.extend(incord_code::migrate::global_report());
        // The reviewers incode ships, written once into the operator's own
        // agents folder and theirs from then on.
        migrated.extend(incord_code::subagents::seed_report());
    }
    if let Some(workspace) = parsed.workspace() {
        migrated.extend(incord_code::migrate::project_report(workspace));
        migrated.extend(incord_code::folders::heal(workspace));
    }
    for line in migrated {
        eprintln!("{line}");
    }
    let config = match parsed {
        Parsed::Run(config) => config,
        Parsed::Exit(message) => {
            println!("{message}");
            return Ok(());
        }
        Parsed::Task {
            workspace,
            instruction,
            model,
            format,
        } => {
            return on_a_runtime(|| {
                incord_code::task_run::run_task_as(
                    &workspace,
                    &instruction,
                    model.as_deref(),
                    format,
                )
            })?
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!(e));
        }
        Parsed::DesktopMcp {
            journal,
            permission_mode,
        } => {
            // Synchronous on this thread, deliberately: the protocol is a
            // line-oriented pipe and the input thread is its own OS thread, so
            // a tokio runtime would add a scheduler to a program that has
            // nothing to schedule.
            //
            // Nothing is printed to stdout — it is the protocol channel, and a
            // banner there is a parse error at the client.
            let input = incord_code::desktop::thread::InputThread::start()
                .map_err(|e| anyhow::anyhow!("could not start desktop input: {e}"))?;
            // The operator's settings, read from `~/.incode/admin.json`.
            // This process holds no workspace, so a machine-wide file is the
            // only place its configuration can come from — and a guard on the
            // desktop is a machine-wide concern anyway.
            let hooks = incord_code::admin::AdminConfig::load_default().desktop_hooks;
            if !hooks.pre_tool_use.is_empty() {
                eprintln!(
                    "incode-desktop: {} pre-tool hook(s) will run before every action",
                    hooks.pre_tool_use.len()
                );
            }
            let mut server = incord_code::desktop::mcp::Server::new(input)
                .with_permission_mode(permission_mode)
                .with_hooks(hooks);
            if let Some(path) = journal {
                eprintln!("incode-desktop journalling to {}", path.display());
                server = server.with_journal(path);
            }

            // Named on stderr so an operator who asked for `read-only` can see
            // that it arrived. A capability restriction nobody can observe is
            // one nobody trusts.
            eprintln!(
                "incode-desktop MCP server ready on stdio ({})",
                permission_mode.as_str()
            );
            incord_code::desktop::mcp::run_stdio(server)?;
            return Ok(());
        }
        Parsed::Acp(config) => {
            // Its own threads, no runtime around them: the protocol is a
            // line-oriented pipe, and the HTTP helpers bring their own runtime
            // when none is ambient. Nothing may reach stdout — `run_stdio`
            // moves the standard handles before anything is printed.
            return incord_code::acp_agent::run_stdio(*config);
        }
        Parsed::InstallTools => {
            let (lines, all) = incord_code::ext_tools::install_all(
                &incord_code::admin::AdminConfig::default_dir(),
            );
            for line in lines {
                println!("{line}");
            }
            if !all {
                anyhow::bail!("not every configured tool is installed");
            }
            return Ok(());
        }
        Parsed::ToolServer => {
            // stdout is the protocol channel; this process prints nothing
            // else there, and runs no tool itself.
            incord_code::tool_bridge::run_stdio()?;
            return Ok(());
        }
        Parsed::TeamMcp { channel } => {
            let speaking_as = incord_code::team_mcp::Server::identity_from_env();
            // stderr, never stdout: stdout is the protocol channel and a
            // banner there is a parse error at the client.
            eprintln!("incode-team MCP server ready on stdio, speaking as `{speaking_as}`");
            let server = incord_code::team_mcp::Server::new(
                incord_code::team::Channel::at(channel),
                speaking_as,
            );
            incord_code::team_mcp::run_stdio(server)?;
            return Ok(());
        }
        Parsed::CheckFindings { workspace, path } => {
            // stdout on a pass, an error on a failure, because that is how
            // `verify` reads a check: the exit code decides and the text is
            // the evidence either way. The failure text is written for the
            // AGENT that reads it on its next attempt, so it says what to do
            // rather than merely that something is wrong.
            let found = incord_code::findings::check(&workspace, &path)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("findings delivered: {found}");
            return Ok(());
        }
        Parsed::DesktopStop { workspace } => {
            let path = incord_code::desktop::safety::stop_path_in(&workspace);
            // REPORTED, never swallowed. A stop request that silently failed is
            // worse than no stop button at all, because the operator walks away
            // believing the desktop is halted.
            incord_code::desktop::safety::request_stop(&path).map_err(|e| {
                anyhow::anyhow!("could not request a stop at {}: {e}", path.display())
            })?;
            println!("desktop stop requested: {}", path.display());
            println!(
                "It takes effect before the desktop's next action, and is permanent \
                 for that session. Delete the file to allow a NEW session to act."
            );
            return Ok(());
        }
        Parsed::Serve { workspace, port } => {
            // The banner is printed by `listen`, AFTER the bind, because until
            // then the port is not known — asking the OS for a free one is the
            // default. Announcing an address before it is held would print a
            // URL that may never answer.
            // AWAITED on its own runtime, not run inside one.
            //
            // `on_a_runtime` exists for the harness's SYNCHRONOUS work — a
            // readline loop whose callees need a runtime nearby — so it calls
            // its closure from within the runtime. This mode is itself the
            // async work, and calling `block_on` from in there panics with
            // "Cannot start a runtime from within a runtime". Measured, on the
            // first run of this flag.
            let runtime = tokio::runtime::Runtime::new()?;
            return runtime
                .block_on(incord_code::serve::http::listen(workspace, port))
                .map_err(anyhow::Error::from);
        }
    };

    on_a_runtime(|| incord_code::cli::CliApp::new(*config).run())?
}

/// Run the CLI's blocking work with a tokio runtime available underneath it.
///
/// The harness itself is synchronous — a readline prompt driving turns — but
/// what it calls underneath is not. [`incord_code::agent::api`] looks up the
/// ambient runtime with `Handle::try_current()` before it can stream, and then
/// calls `tokio::task::block_in_place` to await the streaming task; the HTTP
/// calls into the memory service ([`incord_code::brief`],
/// [`incord_code::recall`]) are async for the same reason. Nothing in the crate
/// ever built a runtime, so that lookup always failed and the model path could
/// not run from this binary at all.
///
/// (This used to name `provider/anthropic.rs:397`. That module went with the
/// harness's own agentic loop — the reference outlived the file it pointed at,
/// which is exactly the kind of comment that sends the next reader looking for
/// code that is not there.)
///
/// It has to be MULTI-THREAD: `block_in_place` panics outright on a
/// current-thread runtime, so a smaller one would trade the error for a crash.
fn on_a_runtime<T>(work: impl FnOnce() -> T) -> anyhow::Result<T> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not start the async runtime the provider needs")?;

    // `block_on` drives the future on this thread inside the runtime's context,
    // which is exactly what `#[tokio::main]` expands to — and what the two call
    // sites above are written against.
    Ok(runtime.block_on(async move { work() }))
}

/// What the arguments asked for.
#[derive(Debug)]
enum Parsed {
    Run(Box<IncordCodeConfig>),
    /// One instruction, run by an installed agent rather than a model.
    Task {
        workspace: PathBuf,
        instruction: String,
        model: Option<String>,
        /// How what happens is reported: text, or JSON for a script.
        format: incord_code::task_run::OutputFormat,
    },
    Exit(String),
    /// Serve what the run is doing to a local interface, and nothing else.
    ///
    /// Its own mode rather than a flag on a run: this process does no work and
    /// drives no agent. It reads the ledger and the transcripts a run leaves
    /// behind — which is what lets it watch a run started from another terminal
    /// rather than needing to be the thing that started it.
    Serve {
        workspace: PathBuf,
        /// `None` means "any free port", published to `.incode/serve.json`.
        port: Option<u16>,
    },
    /// Speak MCP over stdin/stdout so a delegated agent can drive the desktop.
    ///
    /// Its own mode, and its own PROCESS, for the reasons in
    /// `docs/COMPUTER_USE.md` 4c: Windows per-monitor DPI awareness must be set
    /// before anything reads the screen and cannot be changed afterwards, so it
    /// needs a startup it owns. Keeping it separate also means a crash in
    /// desktop control cannot take down a live harness session.
    ///
    /// Carries no workspace: this process edits nothing except the one journal
    /// file it is handed, if it is handed one.
    DesktopMcp {
        /// Where to append the operator's view of what the desktop did. `None`
        /// means nobody is watching — a server started by hand for a protocol
        /// test writes nowhere.
        journal: Option<std::path::PathBuf>,
        /// What this desktop is allowed to do.
        ///
        /// Passed on the argv by whoever launched the server, because this is
        /// a separate process: the session's `-p` cannot reach it any other
        /// way, and for a while it did not reach it at all — a `read-only`
        /// session got a desktop that could type. See
        /// [`incord_code::permissions::PermissionPolicy::for_desktop`].
        permission_mode: incord_code::PermissionMode,
    },

    /// Halt the desktop driving this workspace.
    ///
    /// Its own mode rather than a flag on a run, because it must work when the
    /// harness is not running, when the workspace is half-set-up, and while
    /// something else holds the terminal. That is what "out-of-band" means in
    /// `COMPUTER_USE.md` 7b.
    DesktopStop {
        workspace: std::path::PathBuf,
    },

    /// Judge a research task's deliverable, and nothing else.
    ///
    /// # Why the harness checks itself
    ///
    /// A task that produces INFORMATION has no build to judge it — that is the
    /// whole reason such steps used to be folded into the change they served
    /// (see [`incord_code::findings`]). Giving them a check means giving them
    /// a COMMAND, because a command exiting zero is the only thing
    /// [`incord_code::verify`] accepts, and the only command that knows what a
    /// findings file must contain is this one. So the runner spawns its own
    /// binary as an ordinary subprocess: the law is untouched, and the exit
    /// code is as real as `cargo test`'s.
    CheckFindings {
        workspace: std::path::PathBuf,
        path: String,
    },

    /// Speak MCP on stdio so a running agent can reach its teammates.
    ///
    /// Carries the channel and nothing else: WHO it speaks for comes from
    /// [`incord_code::team_mcp::IDENTITY_ENV`], which the harness sets on the
    /// agent this is a child of. An identity taken from the command line would
    /// be fixed for the life of the agent CLI, and the role changes per task.
    TeamMcp {
        channel: std::path::PathBuf,
    },

    /// Offer incode's tools to the agent CLI incode started, over MCP.
    ///
    /// Carries nothing: which incode to answer to, and the turn's token, come
    /// from the environment that incode gave the CLI and the CLI passed on.
    ToolServer,

    /// Install every tool `admin.json` `tools` names, then exit.
    InstallTools,

    /// Be an Agent Client Protocol agent on stdio.
    ///
    /// Carries the config for the flags that apply to every session it opens
    /// (`-p`, `-m`, `--no-env-filter`). The workspace does NOT come from here:
    /// each `session/new` names its own `cwd`.
    Acp(Box<IncordCodeConfig>),
}

impl Parsed {
    /// The project this mode works in, when it names one up front. The REPL
    /// and `--acp` are left out: each session migrates its own workspace as
    /// it starts (`cli::prepare_session`), and under `--acp` that is the
    /// session's `cwd`, not this process's.
    fn workspace(&self) -> Option<&std::path::Path> {
        match self {
            Self::Task { workspace, .. }
            | Self::Serve { workspace, .. }
            | Self::DesktopStop { workspace }
            | Self::CheckFindings { workspace, .. } => Some(workspace),
            _ => None,
        }
    }

    /// Whether this mode reads incode's data directory (`~/.incode/`), and so
    /// must find the old one's contents there. The rest never open it, and
    /// `--check-findings` in particular runs as a subprocess of checks and of
    /// this crate's tests — copying the operator's settings is not its job.
    fn reads_data_dir(&self) -> bool {
        matches!(
            self,
            Self::Run(_)
                | Self::Acp(_)
                | Self::Task { .. }
                | Self::DesktopMcp { .. }
                | Self::InstallTools
                | Self::ToolServer
        )
    }
}

/// Parse command-line arguments into a config.
///
/// An unknown flag or an unparseable value is an error rather than a silent
/// default: a mistyped `--permission-mode` must never quietly downgrade to
/// something the user did not ask for.
fn parse_args(args: impl Iterator<Item = String>) -> anyhow::Result<Parsed> {
    let mut config = IncordCodeConfig::default();
    let mut args = args.peekable();
    // Answered after the loop, not inside it: the report is about a workspace,
    // and `--admin -w some/path` must describe the path that was named.
    let mut admin_only = false;
    let mut serve = false;
    let mut desktop_mcp = false;
    let mut desktop_journal: Option<std::path::PathBuf> = None;
    let mut desktop_stop = false;
    let mut check_findings: Option<String> = None;
    let mut team_mcp = false;
    let mut team_channel: Option<std::path::PathBuf> = None;
    let mut acp = false;
    let mut tool_server = false;
    let mut install_tools = false;
    // `None` asks the OS for a free port and publishes the answer, so nothing
    // collides with whatever else this machine is already running.
    let mut port: Option<u16> = None;
    let mut task: Option<String> = None;
    let mut output_format: Option<incord_code::task_run::OutputFormat> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Exit(USAGE.to_string())),
            "-V" | "--version" => {
                return Ok(Parsed::Exit(format!(
                    "incode {}",
                    env!("CARGO_PKG_VERSION")
                )))
            }
            "-m" | "--model" => {
                let chosen = next_value(&mut args, &arg)?;
                // Recorded as a flag as well as a value: the admin layer gives a
                // deliberate flag precedence over every other source, and cannot
                // do that if a flag and a fallback look the same.
                config.model_from_flag = Some(chosen.clone());
                config.model = chosen;
            }
            "-w" | "--workspace" => {
                config.workspace = PathBuf::from(next_value(&mut args, &arg)?);
            }
            "-p" | "--permission-mode" => {
                let value = next_value(&mut args, &arg)?;
                // Reject rather than fall back: this flag decides what the
                // agent is allowed to do to the machine.
                if PermissionMode::parse(&value).is_none() {
                    anyhow::bail!(
                        "unknown permission mode `{value}`. Valid: read-only, \
                         workspace-write, danger-full-access, prompt, allow"
                    );
                }
                config.permission_mode = value;
            }
            "--check-findings" => check_findings = Some(next_value(&mut args, &arg)?),
            "--admin" => admin_only = true,
            // Answered after the loop, like `--admin`, so `--serve -w path`
            // watches the path that was named rather than the default.
            "--serve" => serve = true,
            "--port" => {
                let value = next_value(&mut args, &arg)?;
                // Refused rather than defaulted: a mistyped port that silently
                // became 9181 would leave the interface talking to a server
                // watching a different project.
                port = Some(
                    value
                        .parse()
                        .map_err(|_| anyhow::anyhow!("`--port` needs a number, got `{value}`"))?,
                );
            }
            // One instruction, run by an installed agent on its own
            // subscription. No provider credential is read or needed.
            "-t" | "--task" => task = Some(next_value(&mut args, &arg)?),
            // Refused rather than defaulted: a script that asked for JSON and
            // got text would parse a person's prose.
            "--output-format" => {
                let value = next_value(&mut args, &arg)?;
                output_format = Some(
                    incord_code::task_run::OutputFormat::parse(&value).ok_or_else(|| {
                        anyhow::anyhow!(
                            "unknown output format `{value}`. Valid: text, stream-json, json"
                        )
                    })?,
                );
            }
            // The id the agent assigned, which every session prints on the way
            // out. Promised by that message, so it has to exist.
            "-r" | "--resume" => config.resume_session = Some(next_value(&mut args, &arg)?),
            // Repeatable: each occurrence adds a root. Guards, the sentinel and
            // the checkpoint all extend to cover it.
            "--add-dir" => config
                .additional_dirs
                .push(std::path::PathBuf::from(next_value(&mut args, &arg)?)),
            "--no-env-filter" => config.env_filter_enabled = false,
            // Answered after the loop like `--serve` and `--admin`, though
            // unlike them it ignores the workspace entirely.
            "--desktop-mcp" => desktop_mcp = true,
            // Where to write the operator's view of what the desktop did.
            // A PATH, not a workspace: this process is told the one file it may
            // append to, which is what keeps "edits nothing" true in spirit
            // while still making the work watchable.
            "--desktop-journal" => {
                desktop_journal = Some(std::path::PathBuf::from(next_value(&mut args, &arg)?));
            }
            // The operator's half of the kill switch. Answered after the loop,
            // where the workspace is known.
            "--desktop-stop" => desktop_stop = true,
            "--team-mcp" => team_mcp = true,
            // Answered after the loop, before the workspace check.
            "--acp" => acp = true,
            "--tool-server" => tool_server = true,
            "--install-tools" => install_tools = true,
            // Which conversation. A PATH, like --desktop-journal: this process
            // is told the one file it may append to rather than given a
            // workspace to find its own way around.
            "--team-channel" => {
                team_channel = Some(std::path::PathBuf::from(next_value(&mut args, &arg)?));
            }
            other => anyhow::bail!("unknown argument `{other}`\n\n{USAGE}"),
        }
    }

    // Before everything, and alone: the agent CLI starts this with whatever
    // cwd it has, and a flag beside it would be one this mode ignores.
    if tool_server {
        let alone = !acp
            && !install_tools
            && task.is_none()
            && !serve
            && !admin_only
            && !desktop_mcp
            && !desktop_stop
            && !team_mcp
            && check_findings.is_none()
            && desktop_journal.is_none()
            && team_channel.is_none();
        if !alone {
            anyhow::bail!(
                "`--tool-server` is its own mode and cannot be combined with another\n\n{USAGE}"
            );
        }
        return Ok(Parsed::ToolServer);
    }

    // Alone, and before the workspace check: it installs into the data
    // directory and touches no workspace.
    if install_tools {
        let alone = !acp
            && task.is_none()
            && !serve
            && !admin_only
            && !desktop_mcp
            && !desktop_stop
            && !team_mcp
            && check_findings.is_none()
            && desktop_journal.is_none()
            && team_channel.is_none();
        if !alone {
            anyhow::bail!(
                "`--install-tools` is its own mode and cannot be combined with another\n\n{USAGE}"
            );
        }
        return Ok(Parsed::InstallTools);
    }

    // FIRST, and before the workspace check: an ACP client launches this from
    // whatever directory it has, and every session names its own `cwd`.
    if acp {
        let other_mode = task.is_some()
            || serve
            || admin_only
            || desktop_mcp
            || desktop_stop
            || team_mcp
            || check_findings.is_some()
            || desktop_journal.is_some()
            || team_channel.is_some();
        if other_mode {
            anyhow::bail!("`--acp` is its own mode and cannot be combined with another\n\n{USAGE}");
        }
        // Each is refused rather than ignored: the client picks the workspace
        // and the conversation per session, and a flag that silently did
        // nothing would be believed.
        if config.resume_session.is_some() || !config.additional_dirs.is_empty() {
            anyhow::bail!(
                "`--resume` and `--add-dir` do not apply to `--acp`: the client names each \
                 session's workspace\n\n{USAGE}"
            );
        }
        // The modes a client can be offered. `prompt` and `allow` have no ACP
        // mode to show as current, so a session could not say what it runs as.
        if !matches!(
            PermissionMode::parse(&config.permission_mode),
            Some(
                PermissionMode::ReadOnly
                    | PermissionMode::WorkspaceWrite
                    | PermissionMode::DangerFullAccess
            )
        ) {
            anyhow::bail!(
                "`--acp` runs as read-only, workspace-write or danger-full-access, not `{}`",
                config.permission_mode
            );
        }
        return Ok(Parsed::Acp(Box::new(config)));
    }

    // BEFORE the workspace check, unlike every other mode. This process edits
    // nothing, and a client that launches it from a directory that has since
    // been removed would otherwise be refused with a message about a workspace
    // it never asked for.
    // BEFORE the workspace check, like `--desktop-mcp` and for the same
    // reason: an MCP client launches this with whatever cwd it happens to
    // have, and this process touches only the channel file it is handed.
    if team_mcp {
        let Some(channel) = team_channel else {
            anyhow::bail!("`--team-mcp` needs `--team-channel <PATH>`: there is no conversation to join without one\n\n{USAGE}");
        };
        return Ok(Parsed::TeamMcp { channel });
    }
    if let Some(path) = team_channel {
        anyhow::bail!(
            "`--team-channel {}` only applies with `--team-mcp`\n\n{USAGE}",
            path.display()
        );
    }

    if desktop_mcp {
        if desktop_stop {
            anyhow::bail!("`--desktop-stop` stops a desktop; it cannot also start one\n\n{USAGE}");
        }
        return Ok(Parsed::DesktopMcp {
            journal: desktop_journal,
            // `-p` was validated by the loop above, so this cannot silently
            // fall back — a typo was already refused with a message.
            permission_mode: incord_code::PermissionMode::from_str_lossy(&config.permission_mode),
        });
    }

    // Also before the workspace check. An operator reaching for the kill switch
    // must not be turned away because the folder is missing a document — this
    // is the one command whose whole purpose is to work when things are wrong.
    if desktop_stop {
        return Ok(Parsed::DesktopStop {
            workspace: config.workspace,
        });
    }

    // Also before the workspace check, and for a plainer reason than the
    // others: this reads one file and exits. The runner invokes it as a check
    // against a lane's own checkout, which has no brief and no settings of its
    // own, and refusing there would fail the task for the wrong reason.
    if let Some(path) = check_findings {
        return Ok(Parsed::CheckFindings {
            workspace: config.workspace,
            path,
        });
    }

    // A journal with nothing to journal is a typo worth naming rather than
    // ignoring: the operator asked to watch a desktop and would get silence.
    if let Some(path) = desktop_journal {
        anyhow::bail!(
            "`--desktop-journal {}` only applies with `--desktop-mcp`\n\n{USAGE}",
            path.display()
        );
    }

    if !config.workspace.is_dir() {
        anyhow::bail!(
            "workspace `{}` is not a directory",
            config.workspace.display()
        );
    }

    if let Some(instruction) = task {
        // Before anything that needs a provider, for the same reason `--admin`
        // is: this mode exists precisely for machines with no credential.
        return Ok(Parsed::Task {
            workspace: config.workspace,
            instruction,
            model: config.model_from_flag,
            format: output_format.unwrap_or_default(),
        });
    }
    // Only a one-shot task has an output to format; ignored anywhere else it
    // would be believed.
    if output_format.is_some() {
        anyhow::bail!("`--output-format` only applies with `-t`/`--task`\n\n{USAGE}");
    }

    if serve {
        // Before the provider-shaped paths, for the same reason `--admin` is:
        // watching a run needs no credential of its own.
        return Ok(Parsed::Serve {
            workspace: config.workspace,
            port,
        });
    }

    if admin_only {
        // Deliberately before anything that needs a provider: an operator
        // checking which model a run will use must not need a working
        // credential for that model to find out.
        let admin =
            incord_code::admin::AdminConfig::load(&incord_code::admin::AdminConfig::default_dir())
                .map_err(|e| anyhow::anyhow!(e))?;
        let report = admin.report(&config.workspace, config.model_from_flag.as_deref());
        return Ok(Parsed::Exit(report));
    }

    Ok(Parsed::Run(Box::new(config)))
}

fn next_value(
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
    flag: &str,
) -> anyhow::Result<String> {
    args.next()
        .ok_or_else(|| anyhow::anyhow!("`{flag}` requires a value"))
}

#[cfg(test)]
mod tests {

    use super::*;

    /// **Reach is repeatable.** A person with three checkouts should not have
    /// to choose which two matter.
    #[test]
    fn add_dir_can_be_given_more_than_once() {
        match parse(&["--add-dir", "one", "--add-dir", "two"]).expect("valid") {
            Parsed::Run(config) => assert_eq!(
                config.additional_dirs,
                vec![
                    std::path::PathBuf::from("one"),
                    std::path::PathBuf::from("two")
                ]
            ),
            other => panic!("got {other:?}"),
        }
    }

    /// With none given, the workspace is the only root — the behaviour before
    /// the flag existed.
    #[test]
    fn no_add_dir_means_the_workspace_alone() {
        match parse(&["-w", "."]).expect("valid") {
            Parsed::Run(config) => assert!(config.additional_dirs.is_empty()),
            other => panic!("got {other:?}"),
        }
    }

    /// A flag with no value is refused rather than silently adding nothing.
    #[test]
    fn add_dir_without_a_path_is_refused() {
        assert!(parse(&["--add-dir"]).is_err());
    }

    /// **The CLI promises `--resume <id>` on the way out of every session.** A
    /// flag that is printed as the way back in must exist, or the message is an
    /// instruction to run something that errors.
    #[test]
    fn resume_takes_an_agent_session_id() {
        match parse(&["--resume", "845ef1d1-5bcc-49fb-ab40-e5d0fb539afa"]).expect("valid") {
            Parsed::Run(config) => assert_eq!(
                config.resume_session.as_deref(),
                Some("845ef1d1-5bcc-49fb-ab40-e5d0fb539afa")
            ),
            other => panic!("--resume must start a session, got {other:?}"),
        }
    }

    /// The short form is the one people type.
    #[test]
    fn resume_has_a_short_form() {
        match parse(&["-r", "abc"]).expect("valid") {
            Parsed::Run(config) => assert_eq!(config.resume_session.as_deref(), Some("abc")),
            other => panic!("got {other:?}"),
        }
    }

    /// A flag with no value is an error, not a silent no-op.
    #[test]
    fn resume_without_an_id_is_refused() {
        assert!(parse(&["--resume"]).is_err());
    }

    #[test]
    fn desktop_mcp_is_its_own_mode() {
        match parse(&["--desktop-mcp"]).expect("valid") {
            Parsed::DesktopMcp {
                journal,
                permission_mode,
            } => {
                assert_eq!(journal, None, "nobody asked for a journal");
                assert_eq!(
                    permission_mode,
                    incord_code::PermissionMode::WorkspaceWrite,
                    "a desktop started with no `-p` must run as an ordinary session does"
                );
            }
            other => panic!("--desktop-mcp must be its own mode, got {other:?}"),
        }
    }

    /// **THE SESSION'S `-p` MUST CROSS THE PROCESS BOUNDARY.**
    ///
    /// The desktop server is a child process with its own argv. For a while
    /// the mode stopped at the boundary, so `-p read-only` governed the
    /// agent's file writing and left it free to type into whatever was on the
    /// screen. The flag is the only way the restriction gets there.
    #[test]
    fn the_desktop_runs_under_the_mode_it_was_given() {
        match parse(&["--desktop-mcp", "-p", "read-only"]).expect("valid") {
            Parsed::DesktopMcp {
                permission_mode, ..
            } => assert_eq!(permission_mode, incord_code::PermissionMode::ReadOnly),
            other => panic!("got {other:?}"),
        }
    }

    /// **THE JOURNAL PATH MUST SURVIVE THE PARSE.**
    ///
    /// This is the whole cross-process seam: the harness cannot hand the
    /// desktop process a channel, only an argument. Drop it here and the
    /// server starts, works, and streams nothing — a viewer that is silently
    /// empty rather than visibly broken.
    #[test]
    fn the_desktop_takes_a_journal_path() {
        match parse(&["--desktop-mcp", "--desktop-journal", "/tmp/d.jsonl"]).expect("valid") {
            Parsed::DesktopMcp { journal, .. } => {
                assert_eq!(journal, Some(std::path::PathBuf::from("/tmp/d.jsonl")));
            }
            other => panic!("got {other:?}"),
        }
    }

    /// A journal without a desktop is a typo, and a silent one would leave the
    /// operator watching a file nothing ever writes.
    #[test]
    fn a_journal_without_the_desktop_is_refused() {
        let err = parse(&["--desktop-journal", "/tmp/d.jsonl", "-t", "do a thing"])
            .expect_err("must be refused");
        assert!(
            err.to_string().contains("--desktop-mcp"),
            "the error must say what it needs: {err}"
        );
    }

    /// A flag with no value is an error, not a silent no-op.
    #[test]
    fn the_journal_flag_needs_a_path() {
        assert!(parse(&["--desktop-mcp", "--desktop-journal"]).is_err());
    }

    /// **THE KILL SWITCH MUST NOT NEED A HEALTHY WORKSPACE.**
    ///
    /// Every other mode refuses a missing directory. This one cannot: an
    /// operator reaching for the stop is, by definition, in a situation where
    /// something is wrong, and being turned away over a missing folder is the
    /// one failure this command exists to not have.
    #[test]
    fn the_stop_works_even_when_the_workspace_does_not_exist() {
        match parse(&["--desktop-stop", "-w", "/no/such/directory/anywhere"]).expect("valid") {
            Parsed::DesktopStop { workspace } => {
                assert_eq!(
                    workspace,
                    std::path::PathBuf::from("/no/such/directory/anywhere")
                );
            }
            other => panic!("got {other:?}"),
        }
    }

    /// Starting a desktop and stopping one are opposite requests.
    ///
    /// Silently doing one of them would be the worst outcome: an operator who
    /// typed both means to stop, and a server that started instead is a desktop
    /// they believe is halted.
    #[test]
    fn starting_and_stopping_in_one_command_is_refused() {
        assert!(parse(&["--desktop-mcp", "--desktop-stop"]).is_err());
    }

    #[test]
    fn the_stop_appears_in_the_usage() {
        assert!(USAGE.contains("--desktop-stop"));
    }

    /// **THE CHANNEL PATH MUST SURVIVE THE PARSE.**
    ///
    /// The whole cross-process seam: the harness cannot hand a child a
    /// channel object, only an argument. Drop it and the server starts, works,
    /// and talks to nobody.
    #[test]
    fn the_team_server_takes_a_channel() {
        match parse(&["--team-mcp", "--team-channel", "/tmp/t.jsonl"]).expect("valid") {
            Parsed::TeamMcp { channel } => {
                assert_eq!(channel, std::path::PathBuf::from("/tmp/t.jsonl"));
            }
            other => panic!("got {other:?}"),
        }
    }

    /// A team server with no channel is refused, not started silently.
    ///
    /// Started anyway it would answer every tool call successfully and write
    /// to nothing — the worst failure shape there is, because both ends
    /// report success.
    #[test]
    fn a_team_server_without_a_channel_is_refused() {
        let err = parse(&["--team-mcp"]).expect_err("must be refused");
        assert!(err.to_string().contains("--team-channel"), "got: {err}");
    }

    /// A channel without the server is a typo worth naming.
    #[test]
    fn a_channel_without_the_team_server_is_refused() {
        assert!(parse(&["--team-channel", "/tmp/t.jsonl", "-t", "do a thing"]).is_err());
    }

    /// It must not care about the workspace, for the same reason
    /// `--desktop-mcp` does not: an MCP client launches it from wherever.
    #[test]
    fn the_team_server_starts_even_when_the_workspace_does_not_exist() {
        assert!(parse(&[
            "--team-mcp",
            "--team-channel",
            "/tmp/t.jsonl",
            "-w",
            "/no/such/directory/anywhere",
        ])
        .is_ok());
    }

    #[test]
    fn the_team_server_appears_in_the_usage() {
        assert!(USAGE.contains("--team-mcp"));
    }

    /// The tool server is its own mode, needs no workspace, and takes no
    /// other flag: one it ignored would be believed.
    #[test]
    fn the_tool_server_is_its_own_mode() {
        assert!(matches!(
            parse(&["--tool-server", "-w", "/no/such/directory/anywhere"]).expect("valid"),
            Parsed::ToolServer
        ));
        assert!(parse(&["--tool-server", "--acp"]).is_err());
        assert!(parse(&["--tool-server", "--team-mcp"]).is_err());
        assert!(USAGE.contains("--tool-server"));
    }

    /// `--install-tools` installs and exits: its own mode, before the
    /// workspace check, and refused beside another mode.
    #[test]
    fn install_tools_is_its_own_mode() {
        assert!(matches!(
            parse(&["--install-tools", "-w", "/no/such/directory/anywhere"]).expect("valid"),
            Parsed::InstallTools
        ));
        assert!(parse(&["--install-tools", "--acp"]).is_err());
        assert!(parse(&["--install-tools", "--tool-server"]).is_err());
        assert!(parse(&["--install-tools", "-t", "x"]).is_err());
        assert!(USAGE.contains("--install-tools"));
    }

    /// It must not care about the workspace.
    ///
    /// An MCP client launches this process with whatever cwd it happens to
    /// have. Refusing to start because a directory it never touches is missing
    /// would be a baffling failure at exactly the moment nobody is watching.
    #[test]
    fn desktop_mcp_starts_even_when_the_workspace_does_not_exist() {
        match parse(&["--desktop-mcp", "-w", "/no/such/directory/anywhere"]).expect("valid") {
            Parsed::DesktopMcp { .. } => {}
            other => panic!("got {other:?}"),
        }

        // The contrast that makes the point: every other mode still refuses.
        assert!(
            parse(&["--serve", "-w", "/no/such/directory/anywhere"]).is_err(),
            "--serve must still validate its workspace"
        );
    }

    /// The help text has to mention it, or nobody can find it.
    #[test]
    fn desktop_mcp_appears_in_the_usage() {
        assert!(USAGE.contains("--desktop-mcp"));
        // And warn that stdout is the protocol, because printing to it is the
        // easy mistake and it breaks the client rather than the server.
        assert!(USAGE.contains("stdout is the protocol"));
    }

    fn parse(args: &[&str]) -> anyhow::Result<Parsed> {
        parse_args(args.iter().map(|s| (*s).to_string()))
    }

    /// **THE OPERATOR'S SETTINGS ARE COPIED ONLY BY A MODE THAT READS THEM.**
    /// `--check-findings` is run as a subprocess by checks — and by this
    /// crate's own tests, with the real home directory — so it must never
    /// copy `~/.incord-code`. It did, once, before this was split out.
    #[test]
    fn only_a_mode_that_reads_the_settings_migrates_them() {
        let findings = parse(&["--check-findings", "f.md", "-w", "."]).expect("valid");
        assert!(!findings.reads_data_dir());
        assert!(findings.workspace().is_some(), "its project still migrates");
        let stop = parse(&["--desktop-stop", "-w", "."]).expect("valid");
        assert!(!stop.reads_data_dir());

        let repl = parse(&["-w", "."]).expect("valid");
        assert!(repl.reads_data_dir());
        assert!(repl.workspace().is_none(), "each session migrates its own");
        assert!(parse(&["--install-tools"]).expect("valid").reads_data_dir());
    }

    /// The two things the async work actually needs from the entry point, both
    /// executed rather than described. Before this, neither held: nothing in
    /// the crate built a runtime, so `Handle::try_current()` always failed and
    /// the CLI could never reach the model at all.
    ///
    /// A current-thread runtime would satisfy the first and PANIC on the
    /// second, which is why the flavour is asserted this way round.
    #[test]
    fn the_cli_runs_with_the_runtime_the_provider_requires() {
        let (found_handle, blocked_in_place) = on_a_runtime(|| {
            let found_handle = tokio::runtime::Handle::try_current().is_ok();
            let blocked_in_place = tokio::task::block_in_place(|| "ran");
            (found_handle, blocked_in_place)
        })
        .expect("the runtime must build");

        assert!(
            found_handle,
            "the provider looks up the ambient runtime before streaming and must find one"
        );
        assert_eq!(
            blocked_in_place, "ran",
            "block_in_place must be usable from the thread the CLI runs on"
        );
    }

    /// **THE CHECK MUST PARSE THE WAY THE LEDGER WRITES IT.**
    ///
    /// A research task's check is `incode --check-findings <PATH>`, written
    /// into the ledger by [`incord_code::ledger::Task::delivering_findings`]
    /// and run by [`incord_code::verify`] as an ordinary subprocess. If this
    /// flag stops parsing, every research task fails for a reason that has
    /// nothing to do with the research — and the run still looks orderly,
    /// because a failing check is a normal outcome.
    #[test]
    fn the_findings_check_parses_the_argv_the_ledger_writes() {
        let task = incord_code::ledger::Task::new("r-1.1", "r-1", "look")
            .delivering_findings(".incode/findings/r-1.1.md");
        let (program, args) = task.verify.split_first().expect("a check");
        assert_eq!(program, incord_code::verify::SELF_PROGRAM);

        match parse_args(args.iter().cloned()).expect("the flag parses") {
            Parsed::CheckFindings { path, .. } => {
                assert_eq!(path, ".incode/findings/r-1.1.md");
            }
            other => panic!("expected a findings check, got {other:?}"),
        }
    }

    /// What the CLI returns has to survive the runtime, or a failing run would
    /// report success.
    #[test]
    fn the_result_of_the_work_is_passed_back_out() {
        let out = on_a_runtime(|| 42).expect("the runtime must build");
        assert_eq!(out, 42);
    }

    fn config_of(parsed: Parsed) -> IncordCodeConfig {
        match parsed {
            Parsed::Run(config) => *config,
            Parsed::Exit(_)
            | Parsed::Task { .. }
            | Parsed::Serve { .. }
            | Parsed::DesktopMcp { .. }
            | Parsed::DesktopStop { .. }
            | Parsed::CheckFindings { .. }
            | Parsed::TeamMcp { .. }
            | Parsed::ToolServer
            | Parsed::InstallTools
            | Parsed::Acp(_) => {
                panic!("expected a runnable config")
            }
        }
    }

    /// **`--acp` IS ITS OWN MODE, AND IT DOES NOT CARE WHERE IT WAS STARTED.**
    ///
    /// An ACP client launches the agent from whatever directory it has; each
    /// session names its own `cwd`. So a missing `-w` directory must not stop
    /// it, and the flags that apply to every session must survive the parse.
    #[test]
    fn acp_is_its_own_mode_and_carries_the_session_flags() {
        match parse(&[
            "--acp",
            "-p",
            "read-only",
            "-m",
            "opus",
            "-w",
            "/no/such/directory/anywhere",
        ])
        .expect("valid")
        {
            Parsed::Acp(config) => {
                assert_eq!(config.permission_mode, "read-only");
                assert_eq!(config.model_from_flag.as_deref(), Some("opus"));
            }
            other => panic!("--acp must be its own mode, got {other:?}"),
        }
    }

    /// A mode no ACP session can show as current is refused, not mapped.
    #[test]
    fn acp_refuses_a_permission_mode_it_cannot_offer() {
        for mode in ["prompt", "allow"] {
            assert!(
                parse(&["--acp", "-p", mode]).is_err(),
                "`-p {mode}` must be refused under --acp"
            );
        }
    }

    /// Combined with another mode, one of them would be silently dropped.
    #[test]
    fn acp_cannot_be_combined_with_another_mode() {
        assert!(parse(&["--acp", "--task", "do a thing"]).is_err());
        assert!(parse(&["--acp", "--serve"]).is_err());
        assert!(parse(&["--acp", "--resume", "abc"]).is_err());
        assert!(parse(&["--acp", "--add-dir", "."]).is_err());
    }

    #[test]
    fn acp_appears_in_the_usage() {
        assert!(USAGE.contains("--acp"));
    }

    /// **`--serve` names no port, so nothing can collide.**
    ///
    /// The machine this runs on already has a service one port away. A default
    /// number would have been a collision waiting for the day something else
    /// took it; `None` means the OS is asked and the answer published.
    #[test]
    fn serving_without_a_port_asks_for_any_free_one() {
        match parse(&["--serve"]).expect("--serve is valid on its own") {
            Parsed::Serve { port, .. } => assert_eq!(
                port, None,
                "no port must be assumed — see serve::http::ADDRESS_FILE"
            ),
            other => panic!("expected a serve mode, got {other:?}"),
        }
    }

    /// A NAMED port is honoured exactly, because the operator chose it.
    #[test]
    fn a_named_port_is_carried_through_unchanged() {
        match parse(&["--serve", "--port", "9999"]).expect("valid") {
            Parsed::Serve { port, .. } => assert_eq!(port, Some(9999)),
            other => panic!("expected a serve mode, got {other:?}"),
        }
    }

    /// **A mistyped port is refused, never silently defaulted.**
    ///
    /// Falling back would leave an interface pointed confidently at a server
    /// watching some other project.
    #[test]
    fn a_port_that_is_not_a_number_is_an_error() {
        let err = parse(&["--serve", "--port", "nine-thousand"])
            .expect_err("a non-numeric port must not be accepted");
        assert!(
            err.to_string().contains("--port"),
            "the message must name the flag: {err}"
        );
    }

    #[test]
    fn defaults_are_used_when_no_arguments_are_given() {
        let config = config_of(parse(&[]).expect("no arguments is valid"));
        assert_eq!(config.model, "sonnet");
        assert_eq!(config.permission_mode, "workspace-write");
        assert!(config.env_filter_enabled);
    }

    #[test]
    fn flags_are_applied() {
        let config =
            config_of(parse(&["--model", "opus", "--no-env-filter"]).expect("valid flags"));
        assert_eq!(config.model, "opus");
        assert!(!config.env_filter_enabled);
    }

    /// A mistyped security flag must stop the program, not silently pick a
    /// mode. `from_str_lossy` would resolve this to read-only, which is safe
    /// but not what the user asked for — and they would never be told.
    #[test]
    fn a_mistyped_permission_mode_is_rejected() {
        let error = parse(&["--permission-mode", "wrokspace-write"])
            .expect_err("a typo must not be accepted");
        let message = error.to_string();
        assert!(
            message.contains("unknown permission mode"),
            "the error must name the problem; got: {message}"
        );
        assert!(
            message.contains("workspace-write"),
            "the error must list the valid modes; got: {message}"
        );
    }

    #[test]
    fn every_valid_permission_mode_is_accepted() {
        for mode in [
            "read-only",
            "workspace-write",
            "danger-full-access",
            "prompt",
            "allow",
        ] {
            let config = config_of(
                parse(&["--permission-mode", mode]).unwrap_or_else(|e| panic!("{mode}: {e}")),
            );
            assert_eq!(config.permission_mode, mode);
        }
    }

    #[test]
    fn an_unknown_flag_is_rejected_rather_than_ignored() {
        let error = parse(&["--defnitely-not-a-flag"]).expect_err("unknown flags must fail");
        assert!(error.to_string().contains("unknown argument"));
    }

    #[test]
    fn a_flag_missing_its_value_is_rejected() {
        let error = parse(&["--model"]).expect_err("a dangling flag must fail");
        assert!(error.to_string().contains("requires a value"));
    }

    #[test]
    fn a_workspace_that_is_not_a_directory_is_rejected() {
        let error = parse(&["--workspace", "./definitely-not-a-real-directory-xyz"])
            .expect_err("a missing workspace must fail");
        assert!(error.to_string().contains("not a directory"));
    }

    /// **`--task` must not need a credential.** It is the door for a machine
    /// that has an agent installed and no API key, so it has to be answered
    /// during argument parsing, before anything builds a provider client.
    #[test]
    fn a_task_is_parsed_without_touching_a_provider() {
        match parse(&["--task", "write the brief", "-w", "."]).expect("valid") {
            Parsed::Task {
                instruction,
                workspace,
                model,
                format,
            } => {
                assert_eq!(instruction, "write the brief");
                assert_eq!(workspace, PathBuf::from("."));
                assert_eq!(model, None);
                assert_eq!(format, incord_code::task_run::OutputFormat::Text);
            }
            other => panic!("--task must not start a session, got {other:?}"),
        }
    }

    #[test]
    fn a_task_with_model_flag_carries_model() {
        match parse(&[
            "--task",
            "write the brief",
            "-w",
            ".",
            "-m",
            "gpt-5.6-terra",
        ])
        .expect("valid")
        {
            Parsed::Task {
                instruction,
                workspace,
                model,
                ..
            } => {
                assert_eq!(instruction, "write the brief");
                assert_eq!(workspace, PathBuf::from("."));
                assert_eq!(model.as_deref(), Some("gpt-5.6-terra"));
            }
            other => panic!("--task must not start a session, got {other:?}"),
        }
    }

    /// **A SCRIPT ASKS FOR JSON AND GETS IT, OR IS REFUSED.** The format
    /// rides the task; a name nobody knows, or the flag without `-t`, is an
    /// error rather than text a script would then try to parse.
    #[test]
    fn the_output_format_rides_a_task_and_is_refused_elsewhere() {
        for (name, want) in [
            (
                "stream-json",
                incord_code::task_run::OutputFormat::StreamJson,
            ),
            ("json", incord_code::task_run::OutputFormat::Json),
            ("text", incord_code::task_run::OutputFormat::Text),
        ] {
            match parse(&["-t", "do it", "-w", ".", "--output-format", name]).expect("valid") {
                Parsed::Task { format, .. } => assert_eq!(format, want),
                other => panic!("got {other:?}"),
            }
        }
        let err = parse(&["-t", "x", "--output-format", "yaml"]).expect_err("unknown");
        assert!(err.to_string().contains("stream-json"), "{err}");
        let err = parse(&["--output-format", "json"]).expect_err("no task");
        assert!(err.to_string().contains("--task"), "{err}");
        assert!(USAGE.contains("--output-format"));
    }

    /// A task with no text is a mistake worth catching at the boundary.
    #[test]
    fn a_task_flag_without_text_is_rejected() {
        assert!(parse(&["--task"]).is_err());
    }

    #[test]
    fn help_and_version_exit_without_running() {
        for flag in ["-h", "--help"] {
            match parse(&[flag]).unwrap() {
                Parsed::Exit(text) => assert!(text.contains("USAGE")),
                other => panic!("{flag} must not start a session, got {other:?}"),
            }
        }
        match parse(&["--version"]).unwrap() {
            Parsed::Exit(text) => assert!(text.contains("incode")),
            other => panic!("--version must not start a session, got {other:?}"),
        }
    }
}
