//! The settings: precedence, the file, the report, the sandbox and engine
//! switches, and the plugins.

use super::*;

fn lock() -> std::sync::MutexGuard<'static, ()> {
    crate::brief::ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn clear_env() {
    std::env::remove_var(DATA_DIR_ENV);
    std::env::remove_var(crate::sandbox::SANDBOX_ENV);
    std::env::remove_var(crate::sandbox::SANDBOX_NETWORK_ENV);
    std::env::remove_var(crate::sandbox::SANDBOX_ENGINE_ENV);
    std::env::remove_var(AGENT_ENV);
    std::env::remove_var(crate::config::MODEL_ENV);
    std::env::remove_var(crate::budget::BUDGET_ENV);
    std::env::remove_var(crate::verify::VERIFY_COMMAND_ENV);
    std::env::remove_var(crate::agent::codex::CODEX_TOOLS_ENV);
    std::env::remove_var(crate::agent::agy::AGY_TOOLS_ENV);
    for role in crate::roles::KNOWN_ROLES {
        std::env::remove_var(format!(
            "{}{}",
            crate::roles::ROLE_ENV_PREFIX,
            role.to_ascii_uppercase()
        ));
    }
}

/// **The ladder must reach the table that actually routes work.** Expanding
/// correctly in isolation proves nothing if `resolve_roles` hands the raw
/// `@top` string to the backend as though it were a model name.
#[test]
fn a_role_naming_a_tier_resolves_to_that_tiers_models() {
    let _guard = lock();
    clear_env();
    let config = AdminConfig {
        tiers: BTreeMap::from([(
            "top".to_string(),
            vec!["opus".to_string(), "other-agent:big".to_string()],
        )]),
        roles: BTreeMap::from([("plan".to_string(), vec!["@top".to_string()])]),
        ..AdminConfig::default()
    };
    assert_eq!(
        config.resolve_roles().value.candidates_for("plan"),
        ["opus".to_string(), "other-agent:big".to_string()],
        "the role must end up holding real models, not the reference"
    );
}

/// One ladder, many roles — the whole point. Adding a model to `top` must
/// reach every role that names it, without editing those roles.
#[test]
fn one_ladder_serves_every_role_that_names_it() {
    let _guard = lock();
    clear_env();
    let config = AdminConfig {
        tiers: BTreeMap::from([("top".to_string(), vec!["opus".to_string()])]),
        roles: BTreeMap::from([
            ("plan".to_string(), vec!["@top".to_string()]),
            ("review".to_string(), vec!["@top".to_string()]),
            ("diagnose".to_string(), vec!["@top".to_string()]),
        ]),
        ..AdminConfig::default()
    };
    let table = config.resolve_roles().value;
    for role in ["plan", "review", "diagnose"] {
        assert_eq!(
            table.candidates_for(role),
            ["opus".to_string()],
            "`{role}` did not pick the ladder up"
        );
    }
}

/// **A dangling reference must be named.** It costs the role its candidates
/// and falls back to the session model, which is indistinguishable from a
/// role nobody configured — so the typo would never be found.
#[test]
fn a_role_naming_a_tier_that_does_not_exist_is_reported() {
    let config = AdminConfig {
        tiers: BTreeMap::from([("top".to_string(), vec!["opus".to_string()])]),
        roles: BTreeMap::from([("plan".to_string(), vec!["@tpo".to_string()])]),
        ..AdminConfig::default()
    };
    assert_eq!(
        config.unknown_tiers(),
        vec![("plan".to_string(), "@tpo".to_string())]
    );
    assert!(
        config
            .report(Path::new("."), None)
            .contains("no tier defines"),
        "the report must show it, or nothing does"
    );
}

/// The agent obeys the same ladder as everything else.
#[test]
fn the_agent_follows_the_precedence_ladder() {
    let _guard = lock();
    clear_env();
    let mut config = AdminConfig::default();

    assert_eq!(
        config.resolve_agent().value,
        None,
        "no preference by default"
    );
    assert_eq!(config.resolve_agent().source, Source::BuiltIn);

    config.agent = Some("from-file".to_string());
    let chosen = config.resolve_agent();
    assert_eq!(chosen.value.as_deref(), Some("from-file"));
    assert_eq!(chosen.source, Source::AdminFile);

    std::env::set_var(AGENT_ENV, "from-env");
    let chosen = config.resolve_agent();
    assert_eq!(
        chosen.value.as_deref(),
        Some("from-env"),
        "env beats the file"
    );
    assert_eq!(chosen.source, Source::Environment);
    clear_env();
}

/// **A pin that did not apply must be said out loud.** Falling back to
/// another agent in silence is the whole failure this setting exists to
/// end: the operator reads the file, believes the choice took, and is
/// billed against a different subscription.
#[test]
fn an_agent_that_is_not_installed_is_reported_not_ignored() {
    let chosen = Resolved::new(Some("absent-agent".to_string()), Source::AdminFile);
    let line = AdminConfig::describe_agent(&chosen, &["present-agent".to_string()], &[]);
    assert!(line.contains("absent-agent"), "{line}");
    assert!(line.contains("NOT INSTALLED"), "{line}");
    assert!(
        line.contains("present-agent"),
        "it must name what IS there, or the operator cannot fix it: {line}"
    );
}

/// **Installed and drivable are different facts.** An agent found on the
/// machine whose protocol has never been verified cannot take a turn, and
/// saying "installed" about it sends the operator to fix the wrong thing.
#[test]
fn an_agent_that_is_installed_but_not_drivable_says_which() {
    let chosen = Resolved::new(Some("codex".to_string()), Source::AdminFile);
    let line = AdminConfig::describe_agent(
        &chosen,
        &["claude-code".to_string()],
        &["codex".to_string()],
    );
    assert!(
        line.contains("NOT DRIVABLE"),
        "it must not read as missing when it is present: {line}"
    );
    assert!(
        !line.contains("NOT INSTALLED"),
        "it IS installed — saying otherwise sends them to reinstall it: {line}"
    );
}

/// What was found but cannot be driven is still named, so a short drivable
/// list is explained rather than just short.
#[test]
fn agents_that_were_found_but_cannot_be_driven_are_still_named() {
    let chosen = Resolved::new(None, Source::BuiltIn);
    let line = AdminConfig::describe_agent(
        &chosen,
        &["claude-code".to_string()],
        &["codex".to_string(), "gemini".to_string()],
    );
    assert!(line.contains("claude-code"), "{line}");
    assert!(
        line.contains("not drivable: codex, gemini"),
        "the others were found and that must be visible: {line}"
    );
}

/// A pin that DID apply reads plainly, with who decided it.
#[test]
fn an_installed_agent_is_reported_with_its_source() {
    let chosen = Resolved::new(Some("present-agent".to_string()), Source::Environment);
    let line = AdminConfig::describe_agent(&chosen, &["present-agent".to_string()], &[]);
    assert!(line.contains("present-agent"), "{line}");
    assert!(!line.contains("NOT INSTALLED"), "{line}");
    assert!(line.contains(Source::Environment.as_str()), "{line}");
}

/// No preference names what is installed, so the report answers "which
/// agent will actually run this" rather than leaving it blank.
#[test]
fn no_preference_still_names_what_is_installed() {
    let chosen = Resolved::new(None, Source::BuiltIn);
    let line = AdminConfig::describe_agent(&chosen, &["one".to_string(), "two".to_string()], &[]);
    assert!(line.contains("one, two"), "{line}");

    let none = AdminConfig::describe_agent(&chosen, &[], &[]);
    assert!(
        none.contains("none drivable"),
        "an empty machine must say so: {none}"
    );
}

/// The whole precedence ladder, in one place: flag beats env beats file
/// beats built-in.
#[test]
fn the_more_immediate_choice_wins() {
    let _guard = lock();
    clear_env();

    let admin = AdminConfig {
        model: Some("from-the-file".to_string()),
        ..Default::default()
    };

    let from_file = admin.resolve_model(None);
    assert_eq!(from_file.value, "from-the-file");
    assert_eq!(from_file.source, Source::AdminFile);

    std::env::set_var(crate::config::MODEL_ENV, "from-the-env");
    let from_env = admin.resolve_model(None);
    assert_eq!(from_env.value, "from-the-env");
    assert_eq!(from_env.source, Source::Environment);

    let from_flag = admin.resolve_model(Some("from-the-flag"));
    assert_eq!(from_flag.value, "from-the-flag");
    assert_eq!(from_flag.source, Source::Flag);

    clear_env();
    let nothing_set = AdminConfig::default().resolve_model(None);
    assert_eq!(nothing_set.source, Source::BuiltIn);
    assert!(!nothing_set.value.is_empty());
}

/// An operator overriding ONE role for one shell must not lose the rest of
/// their standing fleet.
#[test]
fn an_environment_override_replaces_one_role_not_the_whole_fleet() {
    let _guard = lock();
    clear_env();

    let mut roles = BTreeMap::new();
    roles.insert("explore".to_string(), vec!["file-explorer".to_string()]);
    roles.insert("review".to_string(), vec!["file-reviewer".to_string()]);
    let admin = AdminConfig {
        roles,
        ..Default::default()
    };

    std::env::set_var(
        format!("{}EXPLORE", crate::roles::ROLE_ENV_PREFIX),
        "env-explorer",
    );
    let resolved = admin.resolve_roles();
    clear_env();

    assert_eq!(resolved.value.model_for("explore"), Some("env-explorer"));
    assert_eq!(
        resolved.value.model_for("review"),
        Some("file-reviewer"),
        "the role nobody overrode must survive"
    );
}

/// Settings that quietly stop applying are worse than a loud failure: the
/// run looks normal while doing something else entirely.
#[test]
fn a_malformed_file_is_reported_rather_than_treated_as_no_settings() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(AdminConfig::path_in(dir.path()), "{ not json").expect("write");

    let error = AdminConfig::load(dir.path()).expect_err("must not be silently empty");
    assert!(error.contains("not valid admin settings"), "got: {error}");
}

/// **Settings saved with a byte-order mark load**, rather than being
/// refused as "not valid admin settings": Windows PowerShell 5.1 writes
/// the mark with `Set-Content -Encoding utf8`.
#[test]
fn a_file_with_a_byte_order_mark_loads() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        AdminConfig::path_in(dir.path()),
        b"\xEF\xBB\xBF{\"notify\": \"bell\"}",
    )
    .expect("write");
    let admin = AdminConfig::load(dir.path()).expect("loads");
    assert_eq!(admin.notify.as_deref(), Some("bell"));
}

#[test]
fn a_missing_or_empty_file_behaves_as_no_opinion() {
    let dir = tempfile::tempdir().expect("temp dir");
    assert_eq!(
        AdminConfig::load(dir.path()).expect("missing is fine"),
        AdminConfig::default()
    );

    std::fs::write(AdminConfig::path_in(dir.path()), "   \n").expect("write");
    assert_eq!(
        AdminConfig::load(dir.path()).expect("empty is fine"),
        AdminConfig::default()
    );
}

/// A provider comes back holding the key variable's NAME, which is all it
/// ever holds.
fn a_provider() -> ApiProvider {
    ApiProvider {
        name: "team".to_string(),
        url: "http://127.0.0.1:9".to_string(),
        key_env: "TEAM_KEY_VARIABLE".to_string(),
        agents_path: Some("/agents".to_string()),
        turns_path: Some("/chat".to_string()),
        list_timeout_secs: Some(7),
    }
}

/// An agent CLI changed, and one added, by the protocol's name.
fn a_changed_and_an_added_cli_agent() -> Vec<CliAgent> {
    vec![
        CliAgent {
            id: "codex".to_string(),
            commands: vec!["codex-nightly".to_string()],
            ..CliAgent::default()
        },
        CliAgent {
            id: "mine".to_string(),
            display_name: Some("My agent".to_string()),
            commands: vec!["my-agent".to_string()],
            version_args: Some(vec!["-V".to_string()]),
            protocol: Some(crate::agent::Protocol::ClaudeStreamJson),
            default_sub_agent: Some("codex".to_string()),
        },
    ]
}

/// A configured model with every field set, holding its key variable's NAME
/// only.
fn a_model() -> ApiModel {
    ApiModel {
        name: "router".to_string(),
        format: crate::agent::api::WireFormat::Openai,
        url: "https://router.invalid/v1/chat/completions".to_string(),
        key_env: "ROUTER_KEY_VARIABLE".to_string(),
        model: "a-model".to_string(),
        sub_model: "a-fast-model".to_string(),
        aliases: BTreeMap::from([("haiku".to_string(), "a-fast-model".to_string())]),
        max_tokens: Some(64_000),
        max_steps: Some(60),
        timeout_secs: Some(600),
        max_retries: Some(0),
        prompt_cache: Some(false),
        auth: Some(crate::agent::api::KeyHeader::Bearer),
        api_version: Some("2099-01-01".to_string()),
        context_window: Some(1_000_000),
    }
}

/// An external tool, as `/tools add` reads it back.
fn a_tool() -> crate::ext_tools::ToolEntry {
    crate::ext_tools::ToolEntry {
        name: "browser".to_string(),
        release: Some("owner/repo".to_string()),
        asset: Some("b-{arch}-{os}.{ext}".to_string()),
        run: vec!["b".to_string(), "mcp".to_string()],
        provides: Some("browser".to_string()),
        ..crate::ext_tools::ToolEntry::default()
    }
}

/// A hook on every desktop event. An operator who writes a guard and gets it
/// back empty has no guard and no way to tell. No `..Default`: a new event
/// fails to compile here until it is given a hook.
fn a_hook_on_every_desktop_event() -> crate::hooks::HookConfig {
    crate::hooks::HookConfig {
        pre_tool_use: vec!["a-guard-command".into()],
        post_tool_use: vec!["a-logging-command".into()],
        user_prompt_submit: vec!["a-prompt-check".into()],
        pre_compact: vec!["a-compact-note".into()],
        stop: vec!["a-stop-note".into()],
        notification: vec!["a-notify-hook".into()],
        session_start: vec!["a-start-note".into()],
        session_end: vec!["a-end-note".into()],
        subagent_stop: vec!["a-subagent-note".into()],
    }
}

/// One permission rule of each kind, or a `/roles` save would drop them.
fn one_rule_of_each_kind() -> crate::permissions::rules::RuleLists {
    crate::permissions::rules::RuleLists {
        allow: vec!["Bash(npm test:*)".to_string()],
        ask: vec!["Bash(git push:*)".to_string()],
        deny: vec!["Read(.env)".to_string()],
    }
}

/// Every setting written, saved and loaded back unchanged. The literal below
/// names every `AdminConfig` field with no `..Default`, so a new setting
/// fails to compile here until it is given a value that has to survive the
/// file. The larger values are built by the helpers above.
#[test]
fn settings_round_trip_through_the_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let admin = AdminConfig {
        model: Some("a-model".to_string()),
        // A pinned agent has to survive the file too, or the setting is one
        // an operator can write and never get back.
        agent: Some("an-agent".to_string()),
        sub_agent: Some("another-agent".to_string()),
        api_providers: vec![a_provider()],
        cli_agents: a_changed_and_an_added_cli_agent(),
        api_models: vec![a_model()],
        // The search provider too, holding its key variable's NAME only.
        web_search: Some(SearchProvider {
            url: "https://search.invalid/q".to_string(),
            key_env: Some("SEARCH_KEY_VARIABLE".to_string()),
        }),
        // The browser and the PDF switch, which the owner flips by hand.
        browser_bin: Some("C:/tools/incord-browser.exe".to_string()),
        tools: vec![a_tool()],
        // Turning the built-in tools off is a choice that must come back.
        default_tools: Some(false),
        pdf_renderer: Some(PdfRenderer::Browser),
        sandbox: Some("read-only".to_string()),
        sandbox_network: Some("deny".to_string()),
        sandbox_engine: Some("sbx".to_string()),
        sandbox_allow_hosts: vec!["crates.io".to_string()],
        sandbox_loopback_ports: vec![5173],
        sandbox_backend: Some("wsl".to_string()),
        // The Bash tool's shell, which the owner may pin on Windows.
        shell: Some("C:/Program Files/Git/bin/bash.exe".to_string()),
        codex_tools: Some("incode".to_string()),
        agy_tools: Some("own".to_string()),
        // The ladder has to survive the file too — a tier an operator
        // writes and cannot read back is a setting that does not exist.
        tiers: BTreeMap::from([(
            "top".to_string(),
            vec!["a-model".to_string(), "other:big".to_string()],
        )]),
        roles: BTreeMap::from([(
            "implement".to_string(),
            vec!["a@lab-a".to_string(), "b@lab-b".to_string()],
        )]),
        budget_tokens: Some(50_000),
        verify_command: Some(vec!["cargo".to_string(), "test".to_string()]),
        // A pinned generator is the owner's standing preference for a new
        // project's layout; one that does not come back was never pinned.
        scaffold: BTreeMap::from([("rust".to_string(), "cargo new --vcs none app".to_string())]),
        memory_url: Some("http://127.0.0.1:9180".to_string()),
        // The engine the harness runs its own hooks through: a path the
        // operator chose must come back, or the default quietly replaces it.
        engine_bin: Some("C:\\engine\\incord-memory-service.exe".to_string()),
        plugins: Plugins::default(),
        desktop_hooks: a_hook_on_every_desktop_event(),
        desktop_display: Some("xvfb:1280x800".to_string()),
        desktop_limits: crate::desktop::limits::DesktopLimits {
            max_actions_per_minute: Some(60),
            tree_depth: Some(20),
            ..Default::default()
        },
        notify: Some("desktop".to_string()),
        notify_command: Some("say \"$INCODE_MESSAGE\"".to_string()),
        notify_after_secs: Some(30),
        statusline_command: Some("~/.incode/statusline.sh".to_string()),
        editing_mode: Some("vi".to_string()),
        output_style: Some("concise".to_string()),
        permissions: one_rule_of_each_kind(),
        // Telemetry the owner turned on must stay on.
        telemetry: Some(true),
    };
    admin.save(dir.path()).expect("saves");

    assert_eq!(
        AdminConfig::load(dir.path()).expect("loads"),
        admin,
        "what was written must be what comes back"
    );
}

/// The sandbox policy says whether the operator NAMED the mode (Windows
/// changes an ACL for workspace-write only then) and resolves the network:
/// unset is `allow`, a word nobody uses is `allow` from the built-in layer,
/// and the environment wins over the file.
#[test]
fn the_sandbox_policy_says_whether_the_mode_was_named_and_resolves_the_network() {
    use crate::permissions::PermissionMode as P;
    use crate::sandbox::{SandboxMode, SandboxNetwork, SANDBOX_ENV, SANDBOX_NETWORK_ENV};
    let _guard = lock();
    clear_env();

    let unset = AdminConfig::default().sandbox_policy(P::WorkspaceWrite);
    assert_eq!(unset.mode, SandboxMode::WorkspaceWrite);
    assert!(!unset.named, "the permission-mode default is not named");
    assert_eq!(unset.network, SandboxNetwork::Allow);

    let admin = AdminConfig {
        sandbox: Some("workspace-write".to_string()),
        sandbox_network: Some("deny".to_string()),
        ..Default::default()
    };
    let named = admin.sandbox_policy(P::ReadOnly);
    assert_eq!(named.mode, SandboxMode::WorkspaceWrite);
    assert!(named.named);
    assert_eq!(named.network, SandboxNetwork::Deny);
    assert_eq!(admin.resolve_sandbox_network().source, Source::AdminFile);

    let typo = AdminConfig {
        sandbox_network: Some("block".to_string()),
        ..Default::default()
    };
    let resolved = typo.resolve_sandbox_network();
    assert_eq!(resolved.value, SandboxNetwork::Allow);
    assert_eq!(resolved.source, Source::BuiltIn);

    std::env::set_var(SANDBOX_NETWORK_ENV, "allow");
    std::env::set_var(SANDBOX_ENV, "read-only");
    let env = admin.sandbox_policy(P::WorkspaceWrite);
    assert_eq!(env.network, SandboxNetwork::Allow);
    assert_eq!(env.mode, SandboxMode::ReadOnly);
    assert!(env.named, "the environment names it too");
    assert_eq!(admin.resolve_sandbox_network().source, Source::Environment);
    clear_env();
}

/// **CODEX RUNS ON incode's TOOLS UNLESS SOMEONE SAID OTHERWISE.** Unset
/// is `incode`; the settings switch it to `own`; the environment wins
/// over the settings; a word nobody uses is `incode` from the built-in
/// layer, never a guess at what was meant.
#[test]
fn codex_tools_resolve_like_the_other_settings() {
    use crate::agent::codex::CodexTools;
    let unset = AdminConfig::default();
    assert_eq!(
        unset.resolve_codex_tools_with(None),
        Resolved::new(CodexTools::Incode, Source::BuiltIn)
    );

    let admin = AdminConfig {
        codex_tools: Some("own".to_string()),
        ..Default::default()
    };
    assert_eq!(
        admin.resolve_codex_tools_with(None),
        Resolved::new(CodexTools::Own, Source::AdminFile)
    );
    let parsed: AdminConfig = serde_json::from_str(r#"{"codex_tools":"own"}"#).expect("admin.json");
    assert_eq!(parsed, admin, "the admin.json spelling");

    let typo = AdminConfig {
        codex_tools: Some("bridged".to_string()),
        ..Default::default()
    };
    assert_eq!(
        typo.resolve_codex_tools_with(None),
        Resolved::new(CodexTools::Incode, Source::BuiltIn)
    );

    assert_eq!(
        admin.resolve_codex_tools_with(Some("incode")),
        Resolved::new(CodexTools::Incode, Source::Environment)
    );
    assert_eq!(
        unset.resolve_codex_tools_with(Some(" Own ")),
        Resolved::new(CodexTools::Own, Source::Environment)
    );
    // An unusable value in the environment is no answer: the settings speak.
    assert_eq!(
        admin.resolve_codex_tools_with(Some("")).source,
        Source::AdminFile
    );
}

/// The report names the choice and who made it.
#[test]
fn the_report_says_whose_tools_codex_runs() {
    let _guard = lock();
    clear_env();
    let admin = AdminConfig {
        codex_tools: Some("incode".to_string()),
        ..Default::default()
    };
    let report = admin.report(Path::new("."), None);
    assert!(
        report.contains("codex tools    incode  <- admin file"),
        "{report}"
    );
    clear_env();
}

/// **AGY RUNS ON incode's TOOLS UNLESS SOMEONE SAID OTHERWISE**, resolved
/// layer for layer as `codex_tools` is: unset is `incode`; the settings
/// switch it to `own`; the environment wins over the settings; a word nobody
/// uses is `incode` from the built-in layer.
#[test]
fn agy_tools_resolve_like_codex_tools() {
    use crate::agent::agy::AgyTools;
    let unset = AdminConfig::default();
    assert_eq!(
        unset.resolve_agy_tools_with(None),
        Resolved::new(AgyTools::Incode, Source::BuiltIn)
    );

    let admin = AdminConfig {
        agy_tools: Some("own".to_string()),
        ..Default::default()
    };
    assert_eq!(
        admin.resolve_agy_tools_with(None),
        Resolved::new(AgyTools::Own, Source::AdminFile)
    );
    let parsed: AdminConfig = serde_json::from_str(r#"{"agy_tools":"own"}"#).expect("admin.json");
    assert_eq!(parsed, admin, "the admin.json spelling");
    // The two settings are separate: codex's says nothing about agy's.
    let codex_only = AdminConfig {
        codex_tools: Some("own".to_string()),
        ..Default::default()
    };
    assert_eq!(
        codex_only.resolve_agy_tools_with(None),
        Resolved::new(AgyTools::Incode, Source::BuiltIn)
    );

    let typo = AdminConfig {
        agy_tools: Some("bridged".to_string()),
        ..Default::default()
    };
    assert_eq!(
        typo.resolve_agy_tools_with(None),
        Resolved::new(AgyTools::Incode, Source::BuiltIn)
    );

    assert_eq!(
        admin.resolve_agy_tools_with(Some("incode")),
        Resolved::new(AgyTools::Incode, Source::Environment)
    );
    assert_eq!(
        unset.resolve_agy_tools_with(Some(" Own ")),
        Resolved::new(AgyTools::Own, Source::Environment)
    );
    assert_eq!(
        admin.resolve_agy_tools_with(Some("")).source,
        Source::AdminFile
    );
}

/// The report names agy's choice and who made it, and the variable is the
/// one [`crate::agent::agy::AGY_TOOLS_ENV`] names.
#[test]
fn the_report_says_whose_tools_agy_runs() {
    let _guard = lock();
    clear_env();
    let admin = AdminConfig {
        agy_tools: Some("own".to_string()),
        ..Default::default()
    };
    let report = admin.report(Path::new("."), None);
    assert!(
        report.contains("agy tools      own  <- admin file"),
        "{report}"
    );
    std::env::set_var(crate::agent::agy::AGY_TOOLS_ENV, "incode");
    assert_eq!(
        admin.resolve_agy_tools(),
        Resolved::new(crate::agent::agy::AgyTools::Incode, Source::Environment)
    );
    clear_env();
}

/// The engine: `builtin` unless named, the settings name it, the
/// environment wins, and a word nobody uses is the built-in default.
#[test]
fn the_sandbox_engine_resolves_like_the_other_settings() {
    use crate::permissions::PermissionMode as P;
    use crate::sandbox::{SandboxEngine, SANDBOX_ENGINE_ENV};
    let _guard = lock();
    clear_env();
    let unset = AdminConfig::default();
    assert_eq!(
        unset.sandbox_policy(P::WorkspaceWrite).engine,
        SandboxEngine::Builtin
    );

    let admin = AdminConfig {
        sandbox_engine: Some("sbx".to_string()),
        ..Default::default()
    };
    assert_eq!(
        admin.sandbox_policy(P::WorkspaceWrite).engine,
        SandboxEngine::Sbx
    );
    assert_eq!(admin.resolve_sandbox_engine().source, Source::AdminFile);

    let typo = AdminConfig {
        sandbox_engine: Some("bubblewrap".to_string()),
        ..Default::default()
    };
    assert_eq!(typo.resolve_sandbox_engine().value, SandboxEngine::Builtin);
    assert_eq!(typo.resolve_sandbox_engine().source, Source::BuiltIn);

    std::env::set_var(SANDBOX_ENGINE_ENV, "builtin");
    assert_eq!(admin.resolve_sandbox_engine().value, SandboxEngine::Builtin);
    assert_eq!(admin.resolve_sandbox_engine().source, Source::Environment);
    clear_env();
}

/// **"NONE" IS NOT AN ANSWER AN OPERATOR CAN ACT ON.**
///
/// Two workspaces get no check for opposite reasons: one names no
/// ecosystem this harness knows, the other names two and running one's
/// tests would not prove the other's code. The first wants a command
/// written down; the second wants a choice made. The report said "none"
/// for both, and `detection_note` had already written the difference and
/// was called by nothing.
#[test]
fn a_report_with_no_check_says_which_kind_of_none_it_is() {
    let _guard = lock();
    clear_env();
    let admin = AdminConfig::default();

    // Nothing recognisable here.
    let empty = tempfile::tempdir().expect("temp dir");
    let bare = admin.report(empty.path(), None);
    assert!(bare.contains("nothing can prove"), "{bare}");
    assert!(
        bare.contains("names an ecosystem"),
        "the report does not say WHY there is no check: {bare}"
    );

    // Two ecosystems, which is a different problem with a different fix.
    let mixed = tempfile::tempdir().expect("temp dir");
    std::fs::write(mixed.path().join("Cargo.toml"), "[package]\n").expect("write");
    std::fs::write(mixed.path().join("package.json"), "{}").expect("write");
    let ambiguous = admin.report(mixed.path(), None);
    assert!(
        ambiguous.contains("2 ecosystems are present"),
        "the ambiguous case reads the same as the empty one: {ambiguous}"
    );
    assert!(
        ambiguous.contains("Rust") && ambiguous.contains("Node.js"),
        "the report must name them, or the operator cannot choose: {ambiguous}"
    );
}

/// The report is the answer to "what is this session going to do?", so every
/// value must carry where it was decided.
#[test]
fn the_report_names_the_source_of_every_setting() {
    let _guard = lock();
    clear_env();
    let dir = tempfile::tempdir().expect("temp dir");

    let mut roles = BTreeMap::new();
    roles.insert("explore".to_string(), vec!["small-model".to_string()]);
    let admin = AdminConfig {
        model: Some("main-model".to_string()),
        roles,
        budget_tokens: Some(1234),
        verify_command: Some(vec!["just".to_string(), "check".to_string()]),
        ..Default::default()
    };

    let report = admin.report(dir.path(), None);
    assert!(report.contains("main-model"), "got: {report}");
    assert!(report.contains("small-model"), "got: {report}");
    assert!(report.contains("1234"), "got: {report}");
    assert!(report.contains("just check"), "got: {report}");
    assert!(
        report.matches("admin file").count() >= 3,
        "each value must say where it came from; got: {report}"
    );
}

/// A same-lineage reviewer must be visible in the control panel, not only at
/// startup — this is the screen an operator looks at when setting the fleet.
#[test]
fn the_report_warns_when_the_reviewer_shares_the_writers_lineage() {
    let _guard = lock();
    clear_env();
    let dir = tempfile::tempdir().expect("temp dir");

    let mut roles = BTreeMap::new();
    roles.insert("implement".to_string(), vec!["w@lab-a".to_string()]);
    roles.insert("review".to_string(), vec!["r@lab-a".to_string()]);
    let admin = AdminConfig {
        roles,
        ..Default::default()
    };

    let report = admin.report(dir.path(), None);
    assert!(report.contains("same lineage"), "got: {report}");
}

/// No ceiling and no check are both real states an operator needs to see
/// stated, not inferred from a blank line.
#[test]
fn absent_settings_are_stated_rather_than_left_blank() {
    let _guard = lock();
    clear_env();
    let dir = tempfile::tempdir().expect("temp dir");

    let report = AdminConfig::default().report(dir.path(), None);
    assert!(report.contains("no ceiling"), "got: {report}");
    assert!(report.contains("none assigned"), "got: {report}");
}

/// Provenance has to be per role, not per table: overriding one role in the
/// shell is the normal way this gets used, and a headline saying "env" over
/// a row that came from the file is a wrong answer to the only question the
/// report exists to answer.
#[test]
fn each_role_reports_the_layer_that_actually_decided_it() {
    let _guard = lock();
    clear_env();
    let dir = tempfile::tempdir().expect("temp dir");

    let mut roles = BTreeMap::new();
    roles.insert("implement".to_string(), vec!["file-choice".to_string()]);
    roles.insert("review".to_string(), vec!["file-review".to_string()]);
    let admin = AdminConfig {
        roles,
        ..Default::default()
    };

    std::env::set_var(
        format!("{}REVIEW", crate::roles::ROLE_ENV_PREFIX),
        "shell-review",
    );
    let table = admin.resolve_roles().value;
    let implement_source = admin.role_source("implement");
    let review_source = admin.role_source("review");
    let report = admin.report(dir.path(), None);
    clear_env();

    assert_eq!(table.candidates_for("review"), ["shell-review".to_string()]);
    assert_eq!(
        table.candidates_for("implement"),
        ["file-choice".to_string()],
        "an override for one role must not discard the rest of the file"
    );
    assert_eq!(review_source, Source::Environment);
    assert_eq!(implement_source, Source::AdminFile);
    assert!(
        report.contains("file-choice") && report.contains("shell-review"),
        "got: {report}"
    );
    // Both layers named, each against its own row.
    assert!(report.contains("<- admin file"), "got: {report}");
    assert!(report.contains("<- env"), "got: {report}");
}

/// The file is only reachable if everything agrees where it lives.
#[test]
fn the_settings_are_read_back_from_where_they_were_written() {
    let _guard = lock();
    clear_env();
    let dir = tempfile::tempdir().expect("temp dir");

    let written = AdminConfig {
        model: Some("standing-choice".to_string()),
        budget_tokens: Some(4242),
        ..Default::default()
    };
    written.save(dir.path()).expect("save");

    std::env::set_var(DATA_DIR_ENV, dir.path());
    let loaded = AdminConfig::load_default();
    clear_env();

    assert_eq!(loaded, written);
    assert_eq!(loaded.resolve_model(None).value, "standing-choice");
    assert_eq!(loaded.resolve_budget().value.ceiling(), Some(4242));
}

/// **Every part of the body is a plugin.** The harness must run without any
/// of them; what it loses is context, never the ability to work.
#[test]
fn all_of_the_body_can_be_switched_off() {
    let _guard = lock();
    clear_env();
    let dir = tempfile::tempdir().expect("temp dir");
    let admin = AdminConfig {
        plugins: Plugins {
            owner: false,
            auditor: false,
            recorder: false,
            guards: false,
            skills: false,
            mcp: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let report = admin.report(dir.path(), None);
    for expected in ["owner", "auditor", "recorder", "guards", "skills", "mcp"] {
        assert!(
            report.contains(expected),
            "`{expected}` must be listed: {report}"
        );
    }
    assert!(report.contains("OFF:"), "got: {report}");
}

/// **The owner channel is not like the others**, and the report says so.
/// Turning it off does not move those decisions elsewhere — the runner stops
/// at them, which is the honest behaviour and needs stating.
#[test]
fn switching_off_the_owner_channel_says_what_happens_instead() {
    let _guard = lock();
    clear_env();
    let dir = tempfile::tempdir().expect("temp dir");
    let admin = AdminConfig {
        plugins: Plugins {
            owner: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let report = admin.report(dir.path(), None);
    assert!(report.contains("STOPS"), "got: {report}");
    assert!(report.contains("rather than asking"), "got: {report}");
}

/// **Individual removals are reported even when the category is on.** An
/// operator who dropped three skills and saw "all on" would be reading a
/// report that lied by omission.
#[test]
fn removing_one_skill_is_visible_even_though_skills_are_on() {
    let _guard = lock();
    clear_env();
    let dir = tempfile::tempdir().expect("temp dir");
    let admin = AdminConfig {
        plugins: Plugins {
            disabled_skills: vec!["react-component-best-practices".to_string()],
            disabled_mcp: vec!["some-server".to_string()],
            ..Default::default()
        },
        ..Default::default()
    };
    let report = admin.report(dir.path(), None);
    assert!(
        report.contains("skills removed: react-component-best-practices"),
        "got: {report}"
    );
    assert!(
        report.contains("mcp servers removed: some-server"),
        "got: {report}"
    );
}

/// Removal is by name, case-insensitively, and one removal never affects
/// another item.
#[test]
fn a_removal_takes_out_exactly_one_thing() {
    let plugins = Plugins {
        disabled_skills: vec!["Ponytail".to_string()],
        ..Default::default()
    };
    assert!(!plugins.skill_enabled("ponytail"), "case must not matter");
    assert!(plugins.skill_enabled("test-driven-developer"));
    assert!(
        plugins.mcp_enabled("anything"),
        "skills and mcp are separate"
    );
}

/// Switching a category off takes out everything in it, without needing the
/// operator to list each one.
#[test]
fn switching_a_category_off_takes_out_everything_in_it() {
    let plugins = Plugins {
        skills: false,
        ..Default::default()
    };
    assert!(!plugins.skill_enabled("anything"));
    assert!(plugins.mcp_enabled("anything"));
}

/// Defaults are ON, because the body is normally there and a harness that
/// ignored it would re-ask what it has already been told.
#[test]
fn everything_is_on_until_the_operator_says_otherwise() {
    let plugins = Plugins::default();
    assert!(plugins.disabled().is_empty());
    assert!(plugins.owner && plugins.auditor && plugins.recorder);
    assert!(plugins.guards && plugins.skills && plugins.mcp);
}

/// A settings file written before plugins existed must not read as "all off"
/// — that would silently disable the whole body on upgrade.
#[test]
fn an_older_settings_file_keeps_everything_on() {
    let older = r#"{"model":"a-model"}"#;
    let admin: AdminConfig = serde_json::from_str(older).expect("parses");
    assert!(
        admin.plugins.disabled().is_empty(),
        "an upgrade must not switch the body off"
    );
}
