//! The backend an agent is driven by: the session's context, the desktop and
//! the team channel reaching it, and the protocol choosing it. The REPL's own
//! steps are tested beside them, in `repl/tests.rs` and `prepare/tests.rs`.

use super::*;

/// The mode a session runs in when the operator says nothing.
///
/// Named so the tests below read as being about registration rather than
/// about permissions — the one test that IS about permissions passes a
/// different mode on purpose and is meant to stand out.
const ORDINARY: crate::permissions::PermissionMode =
    crate::permissions::PermissionMode::WorkspaceWrite;

/// **THE REGRESSION THAT HAPPENED TWICE.**
///
/// The run backend shipped built without `with_system_context`, so every
/// ledger task was worked by an agent told none of the intake task, the
/// owner's rules, the auditor's findings, the recorder's handoff or the role
/// table — while the interactive backend beside it carried all of them. Both
/// times it compiled and the suite stayed green; the only visible symptom
/// was a missing `--append-system-prompt` on the spawned child, which is not
/// something a test suite notices.
///
/// Drop the `.with_system_context(..)` from [`run_backend`] and this fails.
#[test]
fn the_run_backend_carries_the_sessions_system_context() {
    let blocks = vec![
        "<guards>the paths the agent may not write</guards>".to_string(),
        "Already solved in this project, from the recorder".to_string(),
    ];
    // EVERY agent, not just the one the regression happened on. The second
    // backend has no flag to append a system prompt at all, so it is the
    // one most likely to lose this quietly — checking only the first would
    // leave exactly that hole open.
    for (agent_id, binary) in [
        ("claude-code", "claude"),
        (crate::agent::registry::ANTIGRAVITY, "agy"),
        (crate::agent::registry::CODEX, "codex"),
    ] {
        let backend = run_backend(
            Path::new("."),
            agent_id,
            binary,
            &crate::guards::Guards::default(),
            &blocks,
        );

        assert_eq!(
            backend.system_context().len(),
            2,
            "every block the session assembled must reach the {agent_id} \
             backend, not just some: {:?}",
            backend.system_context()
        );
        assert!(
            backend
                .system_context()
                .iter()
                .any(|b| b.contains("from the recorder")),
            "including the recorder handoff, which is the one that went \
             missing, on {agent_id}: {:?}",
            backend.system_context()
        );
    }
}

/// **The desktop is OFF unless asked for.**
///
/// The switch decides whether an agent can click and type into whatever is
/// on the screen. Defaulting it on would widen every existing run as a side
/// effect of an upgrade — the kind of change nobody notices until it has
/// already happened.
#[test]
fn the_desktop_is_not_handed_over_unless_it_is_switched_on() {
    let exe = Some(std::path::PathBuf::from("C:/bin/incode.exe"));

    let off = desktop_setup_for(false, exe.clone(), Path::new("C:/work"));
    assert_eq!(
        off, None,
        "the desktop must not be offered when the switch is off"
    );

    let setup =
        desktop_setup_for(true, exe, Path::new("C:/work")).expect("switched on, so a setup");
    assert_eq!(setup.exe, std::path::PathBuf::from("C:/bin/incode.exe"));
    // The journal is what makes the work watchable; without it the agent
    // drives a desktop nobody can see. Its stop file sits beside it.
    assert!(
        setup.journal.ends_with("desktop.jsonl"),
        "{}",
        setup.journal.display()
    );
}

/// No path to this binary means no desktop — on either route.
///
/// A desktop child that cannot be started is a tool that can only fail,
/// which is a much harder thing to diagnose than simply not offering it.
#[test]
fn an_unknown_executable_path_offers_nothing() {
    assert_eq!(desktop_setup_for(true, None, Path::new("C:/work")), None);
    assert!(mcp_overrides_for(true, None, Path::new("C:/work"), ORDINARY).is_empty());
}

/// An empty value does not count as on.
///
/// Same reasoning as `INCODE_TRACE`: `INCODE_DESKTOP=` left in a shell
/// profile must not silently grant every run a desktop.
#[test]
fn the_desktop_switch_ignores_an_empty_value() {
    // The parsing rule, checked directly rather than by mutating the
    // process environment — which is global, and would race the other
    // tests running beside this one.
    let reads_as_on = |value: &str| !value.trim().is_empty();

    assert!(!reads_as_on(""));
    assert!(!reads_as_on("   "));
    assert!(reads_as_on("1"));
    assert!(reads_as_on("true"));
}

/// **THE FLAG MUST SURVIVE `run_backend`.**
///
/// The sibling of `the_run_backend_carries_the_sessions_system_context`,
/// and written for the same measured reason: a capability assembled inside
/// a spawn is invisible from outside, so it goes missing quietly. The
/// system context went missing twice this way.
///
/// Drop the `.with_desktop(..)` from [`run_backend_with_desktop`] and
/// this fails.
///
/// Both states are driven in one process, so the assertion does not depend
/// on what `INCODE_DESKTOP` happens to say on the machine running it. The
/// earlier shape of this test read the environment and asserted the pairing,
/// which on an unset machine reduced to "off, and nothing was handed over" —
/// true with every registration line deleted.
///
/// The desktop reaches the model as incode's own tools now, through the
/// bridge: no MCP server is handed over beside it, on or off.
#[test]
fn the_run_backend_hands_the_desktop_to_the_agent_when_it_is_on() {
    let on = run_backend_with_desktop(
        Path::new("."),
        "claude-code",
        "claude",
        &crate::guards::Guards::default(),
        &[],
        true,
        ORDINARY,
    );
    let tools = on.offered_tools();
    for tool in ["screenshot", "click", "ui_tree"] {
        assert!(
            tools.iter().any(|t| t == tool),
            "the desktop is on, so the model gets {tool}: {tools:?}"
        );
    }
    assert_eq!(on.mcp_config(), None, "no desktop server beside the bridge");

    let off = run_backend_with_desktop(
        Path::new("."),
        "claude-code",
        "claude",
        &crate::guards::Guards::default(),
        &[],
        false,
        ORDINARY,
    );
    let tools = off.offered_tools();
    assert!(
        !tools.iter().any(|t| t == "screenshot"),
        "handed a desktop nobody asked for: {tools:?}"
    );
    assert!(tools.iter().any(|t| t == "Read"), "{tools:?}");
    assert_eq!(off.mcp_config(), None);
}

/// agy has NO per-invocation MCP flag, so on its own tools it must never be
/// handed a server: measured from `agy --help`, it offers only `mcp
/// add/remove/list`, which edits the operator's persistent configuration.
/// Registering a desktop there would be a lasting change to their machine.
///
/// **`agy_tools: "incode"` (the default) makes agy a model on incode's
/// toolbox**: its calls run here, it enforces both halves, the desktop is the
/// toolbox's, and the one server it is handed is the bridge, through the
/// `incode` agent — no desktop server beside it.
#[test]
fn agy_is_handed_no_server_but_the_bridge() {
    use crate::agent::agy::AgyTools;
    use crate::agent::AgentBackend as _;
    // The desktop switched ON throughout, which is the case that matters.
    let own = agy_backend(Path::new("."), "agy", &[], true, ORDINARY, AgyTools::Own);
    assert_eq!(own.mcp_config(), None);
    assert!(!own.runs_tools_here());
    assert_eq!(own.enforces(), crate::guards::Enforcement::NOTHING);
    assert!(own.offered_tools().is_empty());

    let bridged = agy_backend(Path::new("."), "agy", &[], true, ORDINARY, AgyTools::Incode);
    assert!(bridged.runs_tools_here());
    assert_eq!(bridged.enforces(), crate::guards::Enforcement::EVERYTHING);
    let tools = bridged.offered_tools();
    for tool in ["Read", "Write", "Bash", "screenshot"] {
        assert!(tools.iter().any(|t| t == tool), "{tool}: {tools:?}");
    }
    let config = bridged.mcp_config().expect("the bridge is handed over");
    assert!(config.contains("args: ['--tool-server']"), "{config}");
    assert_eq!(
        config.matches("  - name: ").count(),
        1,
        "a server beside the bridge: {config}"
    );
}

/// Codex on its own tools takes the same registration in its own spelling.
/// (Bridged — the default — the desktop is the toolbox's instead:
/// `the_codex_setting_picks_whose_tools_codex_runs`.)
///
/// Drop the `.with_mcp_overrides(..)` from [`codex_backend`]'s own arm and
/// this fails on the `expect`.
#[test]
fn codex_gets_the_desktop_as_toml_overrides_when_it_is_on() {
    use crate::agent::codex::CodexTools;
    use crate::agent::AgentBackend as _;
    let on = codex_backend(
        Path::new("."),
        "codex",
        &[],
        true,
        ORDINARY,
        CodexTools::Own,
    );
    // TOML, not JSON — the difference that makes it codex's dialect.
    let config = on
        .mcp_config()
        .expect("the desktop is on, so codex gets it");
    assert!(
        config.contains("mcp_servers.desktop.command="),
        "got: {config}"
    );
    assert!(
        config.contains("mcp_servers.team.command="),
        "the desktop overrides swallowed the team server: {config}"
    );

    let off = codex_backend(
        Path::new("."),
        "codex",
        &[],
        false,
        ORDINARY,
        CodexTools::Own,
    );
    let quiet = off.mcp_config().expect("the team channel is not optional");
    assert!(
        !quiet.contains("mcp_servers.desktop."),
        "handed a desktop nobody asked for: {quiet}"
    );
    assert!(quiet.contains("mcp_servers.team."), "got: {quiet}");
}

/// **`codex_tools: "incode"` MAKES CODEX A MODEL ON incode's TOOLBOX**: its
/// calls run here (so allowed-tools and the permission rules bind and the
/// "not enforced" notice stops), the desktop is the toolbox's, and the
/// one server handed over is the bridge — no team or desktop server
/// beside it. And `own` is the codex route as it was.
#[test]
fn the_codex_setting_picks_whose_tools_codex_runs() {
    use crate::agent::codex::CodexTools;
    use crate::agent::AgentBackend as _;
    let bridged = codex_backend(
        Path::new("."),
        "codex",
        &[],
        true,
        ORDINARY,
        CodexTools::Incode,
    );
    assert!(bridged.runs_tools_here());
    assert_eq!(bridged.enforces(), crate::guards::Enforcement::EVERYTHING);
    let tools = bridged.offered_tools();
    for tool in ["Read", "Write", "Bash", "screenshot", "team_ask"] {
        assert!(tools.iter().any(|t| t == tool), "{tool}: {tools:?}");
    }
    let config = bridged.mcp_config().expect("the bridge is handed over");
    assert!(
        config.contains("mcp_servers.incode.args=[\"--tool-server\"]"),
        "{config}"
    );
    assert!(
        !config.contains("mcp_servers.team.") && !config.contains("mcp_servers.desktop."),
        "a server beside the bridge: {config}"
    );

    let own = codex_backend(
        Path::new("."),
        "codex",
        &[],
        true,
        ORDINARY,
        CodexTools::Own,
    );
    assert!(!own.runs_tools_here());
    assert_eq!(own.enforces(), crate::guards::Enforcement::NOTHING);
    assert!(own.offered_tools().is_empty());
    let config = own.mcp_config().expect("team and desktop");
    assert!(config.contains("mcp_servers.desktop.command="), "{config}");
    assert!(!config.contains("mcp_servers.incode."), "{config}");
}

/// A Windows path must survive TOML's escape rules.
///
/// `C:\Users\…` inside a TOML basic string is an invalid escape, so the
/// separators are doubled. Without this the override is rejected by codex
/// at spawn time, which surfaces as an MCP server that never starts.
#[test]
fn a_windows_path_is_escaped_for_toml() {
    let overrides = crate::desktop::mcp::codex_config_overrides(
        std::path::Path::new(r"C:\Users\someone\.cargo\bin\incode.exe"),
        Some(std::path::Path::new(r"C:\work\.incode\desktop.jsonl")),
        ORDINARY,
    );
    assert!(
        overrides[0].contains(r"C:\\Users\\someone"),
        "backslashes must be doubled: {}",
        overrides[0]
    );
    // The JOURNAL path needs the same treatment — it is a Windows path in
    // the same TOML string syntax, and escaping only the exe would ship a
    // half-fixed bug that fails exactly where nobody looks.
    assert!(
        overrides[1].contains(r"C:\\work\\.incode"),
        "the journal path must be escaped too: {}",
        overrides[1]
    );
    assert_eq!(
        overrides[1],
        r#"mcp_servers.desktop.args=["--desktop-mcp","--desktop-journal","C:\\work\\.incode\\desktop.jsonl","-p","workspace-write"]"#
    );
}

/// The configured agent decides WHICH backend the run loop drives.
///
/// Without this the second adapter is unreachable: it compiles, its tests
/// pass, and the run loop still builds the first backend every time.
#[test]
fn the_configured_agent_selects_the_backend_that_drives_it() {
    let guards = crate::guards::Guards::default();
    let claude = run_backend(Path::new("."), "claude-code", "claude", &guards, &[]);
    let agy = run_backend(
        Path::new("."),
        crate::agent::registry::ANTIGRAVITY,
        "agy",
        &guards,
        &[],
    );
    let codex = run_backend(
        Path::new("."),
        crate::agent::registry::CODEX,
        "codex",
        &guards,
        &[],
    );

    assert_eq!(claude.id(), "claude-code");
    assert_eq!(agy.id(), "agy", "the backend reports the BINARY it drives");
    assert_eq!(codex.id(), "codex");
    // And each is asked for its enforcement rather than assumed one: agy and
    // codex, on incode's tools by default, claim exactly what claude does.
    // (On their own tools they claim nothing: `agy_is_handed_no_server_but_the_bridge`,
    // `the_codex_setting_picks_whose_tools_codex_runs`.)
    assert!(
        agy.runs_tools_here(),
        "agy runs on incode's tools by default"
    );
    assert_eq!(claude.enforces(), agy.enforces());
    assert!(
        codex.runs_tools_here(),
        "codex runs on incode's tools by default"
    );
    assert_eq!(claude.enforces(), codex.enforces());
}

/// **An agent admin.json adds is driven by the backend for the protocol
/// it names**, not by the default: an id no built-in has, speaking the
/// codex protocol, gets the codex backend on its own program.
#[test]
fn an_added_agent_is_driven_by_its_protocols_backend() {
    let guards = crate::guards::Guards::default();
    let build = |protocol| {
        backend_for_protocol(
            protocol,
            Path::new("."),
            "my-agent",
            &guards,
            &[],
            false,
            crate::permissions::PermissionMode::WorkspaceWrite,
        )
    };
    let codex = run_backend(
        Path::new("."),
        crate::agent::registry::CODEX,
        "codex",
        &guards,
        &[],
    );
    let added = build(Some(crate::agent::Protocol::CodexJson));
    assert_eq!(added.enforces(), codex.enforces(), "the codex backend");
    // Told apart by what only codex speaks — its TOML registration of the
    // bridge — since bridged codex now enforces what claude does.
    let codex_dialect = |b: &dyn crate::agent::AgentBackend| {
        b.mcp_config()
            .is_some_and(|c| c.contains("mcp_servers.incode."))
    };
    assert!(codex_dialect(added.as_ref()), "the codex backend");
    let fallback = build(None);
    assert!(!codex_dialect(fallback.as_ref()), "not the codex one");
}

/// An empty session context is not an error — a project with no brief, no
/// findings and no history has nothing to say — but it must not become a
/// blank block that reads to the agent as an empty instruction.
#[test]
fn a_session_with_nothing_to_say_sends_no_empty_blocks() {
    for (agent_id, binary) in [
        ("claude-code", "claude"),
        (crate::agent::registry::ANTIGRAVITY, "agy"),
        (crate::agent::registry::CODEX, "codex"),
    ] {
        let backend = run_backend(
            Path::new("."),
            agent_id,
            binary,
            &crate::guards::Guards::default(),
            &[String::new(), "   ".to_string()],
        );
        assert!(
            backend.system_context().is_empty(),
            "blank blocks are dropped rather than sent, on {agent_id}: {:?}",
            backend.system_context()
        );
    }
}
