use super::*;

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |name| map.get(name).cloned()
}

const SAMPLE: &str = r#"{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": {"GITHUB_TOKEN": "${GITHUB_TOKEN}"}
    },
    "docs": {
      "type": "http",
      "url": "https://${HOST:-mcp.example.com}/mcp",
      "headers": {"Authorization": "Bearer ${DOCS_TOKEN}"}
    },
    "legacy": {"type": "sse", "url": "https://old.example.com/sse"},
    "bare-url": {"url": "https://bare.example.com/mcp"}
  },
  "someOtherKey": true
}"#;

#[test]
fn a_dot_mcp_json_parses_stdio_and_http_servers() {
    let file = parse(SAMPLE).expect("parses");
    assert_eq!(file.servers.len(), 4);
    let github = &file.servers["github"];
    assert_eq!(github.command.as_deref(), Some("npx"));
    assert_eq!(github.args.len(), 2);
    assert_eq!(github.env["GITHUB_TOKEN"], "${GITHUB_TOKEN}");
    assert_eq!(file.servers["docs"].kind.as_deref(), Some("http"));
    assert!(
        file.extra.contains_key("someOtherKey"),
        "unknown keys are kept"
    );
    assert!(parse("").expect("empty").servers.is_empty());
    assert!(parse("{\"mcpServers\": []}").is_err());
}

#[test]
fn entries_become_specs_with_variables_expanded() {
    let file = parse(SAMPLE).expect("parses");
    let lookup = vars(&[("GITHUB_TOKEN", "ghp_x"), ("DOCS_TOKEN", "d0c")]);
    let github = file.servers["github"]
        .to_spec("github", &lookup)
        .expect("spec");
    match github.transport {
        Transport::Stdio { command, args, env } => {
            assert_eq!(command, PathBuf::from("npx"));
            assert_eq!(args[0], "-y");
            assert_eq!(env, [("GITHUB_TOKEN".to_string(), "ghp_x".to_string())]);
        }
        Transport::Http { .. } => panic!("stdio expected"),
    }
    let docs = file.servers["docs"].to_spec("docs", &lookup).expect("spec");
    match docs.transport {
        Transport::Http { url, headers, .. } => {
            assert_eq!(url, "https://mcp.example.com/mcp", "the default applies");
            assert_eq!(headers[0].1, "Bearer d0c");
        }
        Transport::Stdio { .. } => panic!("http expected"),
    }
    // `sse`, and a bare `url`, are spoken to over Streamable HTTP.
    for name in ["legacy", "bare-url"] {
        let spec = file.servers[name].to_spec(name, &lookup).expect("spec");
        assert!(matches!(spec.transport, Transport::Http { .. }), "{name}");
    }
}

#[test]
fn an_unset_variable_stops_the_server_and_names_the_variable() {
    let file = parse(SAMPLE).expect("parses");
    let why = file.servers["github"]
        .to_spec("github", &vars(&[]))
        .expect_err("unset");
    assert!(why.contains("GITHUB_TOKEN"), "{why}");
}

#[test]
fn expansion_handles_defaults_and_leaves_bare_dollars_alone() {
    let lookup = vars(&[("A", "1"), ("EMPTY", "")]);
    assert_eq!(expand("x${A}y", &lookup).expect("ok"), "x1y");
    assert_eq!(expand("${B:-two}", &lookup).expect("ok"), "two");
    assert_eq!(expand("${A:-two}", &lookup).expect("ok"), "1");
    assert_eq!(expand("${EMPTY:-d}", &lookup).expect("ok"), "d");
    assert_eq!(expand("${B:-}", &lookup).expect("ok"), "");
    assert_eq!(expand("$A and $$", &lookup).expect("ok"), "$A and $$");
    assert_eq!(expand("${A}${A}", &lookup).expect("ok"), "11");
    assert!(expand("${B}", &lookup).is_err());
    assert!(expand("${A", &lookup).is_err());
    assert!(expand("${A B}", &lookup).is_err());
}

#[test]
fn a_server_with_nothing_to_run_is_refused() {
    let none = ServerEntry::default();
    assert!(none.to_spec("x", &vars(&[])).is_err());
    let odd = ServerEntry {
        kind: Some("carrier-pigeon".to_string()),
        ..ServerEntry::default()
    };
    assert!(odd.to_spec("x", &vars(&[])).is_err());
    let http = ServerEntry {
        kind: Some("http".to_string()),
        ..ServerEntry::default()
    };
    assert!(http.to_spec("x", &vars(&[])).is_err());
}

// --- approval -------------------------------------------------------------

fn stdio(command: &str, args: &[&str]) -> ServerEntry {
    ServerEntry {
        command: Some(command.to_string()),
        args: args.iter().map(|s| (*s).to_string()).collect(),
        ..ServerEntry::default()
    }
}

/// **A REPO THAT CHANGES WHAT ITS SERVER RUNS IS ASKED ABOUT AGAIN.**
#[test]
fn the_approval_key_changes_with_anything_that_decides_what_runs() {
    let base = stdio("npx", &["server"]);
    let key = approval_key("s", &base);
    assert!(key.starts_with("s:"));
    assert_eq!(key.len(), 2 + 64);
    assert_eq!(key, approval_key("s", &base.clone()), "stable");
    let mut env = base.clone();
    env.env.insert("X".to_string(), "1".to_string());
    let mut url = base.clone();
    url.url = Some("https://x".to_string());
    let mut extra = base.clone();
    extra
        .extra
        .insert("description".to_string(), Value::from("ignored"));
    for (what, other) in [
        ("name", approval_key("t", &base)),
        ("command", approval_key("s", &stdio("sh", &["server"]))),
        (
            "args",
            approval_key("s", &stdio("npx", &["server", "--evil"])),
        ),
        ("env", approval_key("s", &env)),
        ("url", approval_key("s", &url)),
    ] {
        assert_ne!(key, other, "a changed {what} must change the key");
    }
    assert_eq!(key, approval_key("s", &extra), "unrelated keys do not");
    // Moving text between fields is a different server.
    assert_ne!(
        approval_key("s", &stdio("a", &["b c"])),
        approval_key("s", &stdio("a b", &["c"]))
    );
}

#[test]
fn approvals_are_remembered_and_keyed_by_what_was_approved() {
    let data = tempfile::tempdir().expect("data");
    let entry = stdio("npx", &["server"]);
    assert!(!is_approved(data.path(), "s", &entry));
    approve(data.path(), "s", &entry).expect("approve");
    assert!(is_approved(data.path(), "s", &entry));
    assert!(!is_approved(data.path(), "s", &stdio("npx", &["other"])));
    assert!(!is_approved(data.path(), "t", &entry));
}

fn workspace_with(mcp_json: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("ws");
    std::fs::write(dir.path().join(PROJECT_FILE), mcp_json).expect("write");
    dir
}

#[test]
fn an_unapproved_project_server_is_not_started_without_a_person() {
    let ws = workspace_with(r#"{"mcpServers": {"evil": {"command": "curl", "args": ["x"]}}}"#);
    let data = tempfile::tempdir().expect("data");
    let (specs, said) = session_servers(ws.path(), data.path(), None);
    assert!(specs.is_empty());
    assert!(
        said.iter().any(|s| s.contains("incode mcp approve evil")),
        "{said:?}"
    );
}

#[test]
fn a_person_approves_once_and_it_is_remembered() {
    let ws = workspace_with(r#"{"mcpServers": {"tool": {"command": "my-server"}}}"#);
    let data = tempfile::tempdir().expect("data");
    let asked = std::cell::Cell::new(0);
    let yes = |_: &str, _: &ServerEntry| {
        asked.set(asked.get() + 1);
        true
    };
    let (specs, _) = session_servers(ws.path(), data.path(), Some(&yes));
    assert_eq!(specs.len(), 1);
    let (specs, _) = session_servers(ws.path(), data.path(), Some(&yes));
    assert_eq!(specs.len(), 1);
    assert_eq!(asked.get(), 1, "asked once, then remembered");
    let no = |_: &str, _: &ServerEntry| false;
    std::fs::write(
        ws.path().join(PROJECT_FILE),
        r#"{"mcpServers": {"tool": {"command": "my-server", "args": ["--changed"]}}}"#,
    )
    .expect("rewrite");
    let (specs, said) = session_servers(ws.path(), data.path(), Some(&no));
    assert!(
        specs.is_empty(),
        "a changed server is asked again: {said:?}"
    );
}

#[test]
fn user_servers_need_no_approval_and_a_project_overrides_by_name() {
    let ws = workspace_with(r#"{"mcpServers": {"shared": {"command": "project-cmd"}}}"#);
    let data = tempfile::tempdir().expect("data");
    std::fs::write(
        data.path().join(USER_FILE),
        r#"{"mcpServers": {"mine": {"command": "user-cmd"}, "shared": {"command": "user-shared"}}}"#,
    )
    .expect("write");
    let (servers, _) = configured(ws.path(), data.path());
    let shared = servers.iter().find(|s| s.name == "shared").expect("shared");
    assert_eq!(shared.scope, Scope::Project);
    assert_eq!(shared.entry.command.as_deref(), Some("project-cmd"));
    let (specs, _) = session_servers(ws.path(), data.path(), None);
    let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["mine"], "the project's `shared` is not approved");
}

#[test]
fn the_managed_policy_filters_whatever_named_the_server() {
    use crate::permissions::managed::McpPolicy;
    let lookup = vars(&[]);
    let specs = vec![
        stdio("/opt/ok/server", &[])
            .to_spec("ok", &lookup)
            .expect("spec"),
        stdio("curl", &[]).to_spec("evil", &lookup).expect("spec"),
    ];
    let policy = McpPolicy {
        allow: Some(vec!["command:/opt/ok/*".to_string()]),
        deny: Vec::new(),
    };
    let (kept, said) = apply_managed_policy(&policy, specs);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].name, "ok");
    assert!(said[0].contains("`evil`"), "{said:?}");
}

// --- the command line -----------------------------------------------------

fn run(args: &[&str], ws: &Path, data: &Path) -> Result<String, String> {
    let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    command(&args, ws, data)
}

#[test]
fn mcp_add_get_list_and_remove() {
    let ws = tempfile::tempdir().expect("ws");
    let data = tempfile::tempdir().expect("data");
    let (ws, data) = (ws.path(), data.path());
    assert!(run(&["list"], ws, data)
        .expect("list")
        .contains("no MCP servers"));
    run(
        &[
            "add",
            "-e",
            "TOKEN=${T}",
            "fs",
            "--",
            "npx",
            "-y",
            "server",
            "--flag",
        ],
        ws,
        data,
    )
    .expect("add stdio");
    let saved = read(&data.join(USER_FILE)).expect("read");
    assert_eq!(saved.servers["fs"].args, ["-y", "server", "--flag"]);
    assert_eq!(saved.servers["fs"].env["TOKEN"], "${T}");
    run(
        &[
            "add",
            "-s",
            "project",
            "-H",
            "Authorization: Bearer x",
            "web",
            "https://e.com/mcp",
        ],
        ws,
        data,
    )
    .expect("add http");
    let project = read(&ws.join(PROJECT_FILE)).expect("read");
    assert_eq!(project.servers["web"].kind.as_deref(), Some("http"));
    assert_eq!(project.servers["web"].headers["Authorization"], "Bearer x");
    let listed = run(&["list"], ws, data).expect("list");
    assert!(listed.contains("fs [user]"), "{listed}");
    assert!(listed.contains("web [project]"), "{listed}");
    assert!(
        listed.contains("approved"),
        "added here is approved: {listed}"
    );
    assert!(
        !listed.contains("Bearer x"),
        "header values are never shown"
    );
    assert!(run(&["get", "web"], ws, data)
        .expect("get")
        .contains("https://e.com/mcp"));
    assert!(run(&["get", "nope"], ws, data).is_err());
    run(&["remove", "fs"], ws, data).expect("remove");
    assert!(!run(&["list"], ws, data)
        .expect("list")
        .contains("fs [user]"));
    assert!(run(&["remove", "fs"], ws, data).is_err());
    assert!(run(&["frobnicate"], ws, data).is_err());
    assert!(run(&["add", "only-a-name"], ws, data).is_err());
}

#[test]
fn mcp_approve_approves_the_project_server_as_written() {
    let ws = workspace_with(r#"{"mcpServers": {"tool": {"command": "my-server"}}}"#);
    let data = tempfile::tempdir().expect("data");
    assert!(run(&["approve", "nope"], ws.path(), data.path()).is_err());
    run(&["approve", "tool"], ws.path(), data.path()).expect("approve");
    let (specs, said) = session_servers(ws.path(), data.path(), None);
    assert_eq!(specs.len(), 1, "{said:?}");
}
