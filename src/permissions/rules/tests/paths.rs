use super::*;

// --- paths ----------------------------------------------------------------

#[test]
fn a_name_pattern_matches_at_any_depth_and_through_directories() {
    let rules = set(Layer::User, &[], &[], &["Read(.env)", "Edit(secrets)"]);
    assert_eq!(path(&rules, "Read", ".env"), Some(Effect::Deny));
    assert_eq!(path(&rules, "Read", "app/config/.env"), Some(Effect::Deny));
    assert_eq!(path(&rules, "Read", "/elsewhere/.env"), Some(Effect::Deny));
    assert_eq!(path(&rules, "Read", ".env.example"), None);
    assert_eq!(path(&rules, "Write", "secrets/key.pem"), Some(Effect::Deny));
    assert_eq!(path(&rules, "Edit", "a/secrets/b/c"), Some(Effect::Deny));
}

#[test]
fn dot_dot_does_not_escape_a_path_rule() {
    let rules = set(Layer::User, &[], &[], &["Read(.env)", "Read(/private/**)"]);
    assert_eq!(path(&rules, "Read", "src/../.env"), Some(Effect::Deny));
    assert_eq!(
        path(&rules, "Read", "./src/./../private/x"),
        Some(Effect::Deny)
    );
    assert_eq!(path(&rules, "Read", "/ws/private/x"), Some(Effect::Deny));
}

#[test]
fn a_rooted_pattern_matches_from_the_workspace() {
    let rules = set(Layer::User, &["Read(src/**)"], &[], &["Read(/build)"]);
    assert_eq!(path(&rules, "Read", "src/main.rs"), Some(Effect::Allow));
    assert_eq!(path(&rules, "Read", "src/a/b.rs"), Some(Effect::Allow));
    assert_eq!(path(&rules, "Read", "lib/src/x.rs"), None, "rooted at /ws");
    assert_eq!(path(&rules, "Read", "/other/src/main.rs"), None);
    assert_eq!(path(&rules, "Read", "build/out.o"), Some(Effect::Deny));
    assert_eq!(
        path(&rules, "Read", "x/build/out.o"),
        None,
        "`/build` is rooted"
    );
}

#[test]
fn absolute_and_home_patterns() {
    let rules = set(
        Layer::User,
        &[],
        &[],
        &["Read(~/.ssh/**)", "Read(//etc/shadow)"],
    );
    assert_eq!(
        path(&rules, "Read", "/home/me/.ssh/id_rsa"),
        Some(Effect::Deny)
    );
    assert_eq!(path(&rules, "Read", "/etc/shadow"), Some(Effect::Deny));
    assert_eq!(path(&rules, "Read", "etc/shadow"), None);
}

#[test]
fn read_and_edit_rules_cover_their_families() {
    let rules = set(Layer::User, &[], &[], &["Read(.env)", "Edit(Cargo.lock)"]);
    for tool in ["Read", "Grep", "Glob", "LS", "Outline"] {
        assert_eq!(path(&rules, tool, ".env"), Some(Effect::Deny), "{tool}");
    }
    for tool in ["Write", "Edit", "MultiEdit", "NotebookEdit", "ApplyPatch"] {
        assert_eq!(
            path(&rules, tool, "Cargo.lock"),
            Some(Effect::Deny),
            "{tool}"
        );
    }
    assert_eq!(
        path(&rules, "Write", ".env"),
        None,
        "a Read rule is not an Edit rule"
    );
    // A rule naming one tool exactly covers only it.
    let only = set(Layer::User, &[], &[], &["Write(x.txt)"]);
    assert_eq!(path(&only, "Write", "x.txt"), Some(Effect::Deny));
    assert_eq!(path(&only, "Edit", "x.txt"), None);
}

#[test]
fn several_paths_are_allowed_only_when_all_are() {
    let rules = set(Layer::User, &["Edit(src/**)"], &[], &[]);
    let both = Subject::Paths(vec!["src/a.rs".to_string(), "src/b.rs".to_string()]);
    let mixed = Subject::Paths(vec!["src/a.rs".to_string(), "build.rs".to_string()]);
    assert!(rules.decide("ApplyPatch", &both, &ctx("/ws")).is_some());
    assert!(rules.decide("ApplyPatch", &mixed, &ctx("/ws")).is_none());
}

// --- other tools ----------------------------------------------------------

#[test]
fn domain_rules_match_the_host_and_its_subdomains() {
    let rules = set(Layer::User, &["WebFetch(domain:example.com)"], &[], &[]);
    let fetch = |url: &str| {
        rules
            .decide("WebFetch", &Subject::Url(url.to_string()), &ctx("/ws"))
            .map(|v| v.effect)
    };
    assert_eq!(fetch("https://example.com/a"), Some(Effect::Allow));
    assert_eq!(fetch("https://docs.example.com/a"), Some(Effect::Allow));
    assert_eq!(fetch("https://example.com.evil.net/a"), None);
    assert_eq!(fetch("https://notexample.com/"), None);
}

#[test]
fn an_mcp_server_rule_covers_each_of_its_tools() {
    let rules = set(Layer::User, &[], &["mcp__github"], &["mcp__fs__write_file"]);
    let call = |tool: &str| {
        rules
            .decide(tool, &Subject::None, &ctx("/ws"))
            .map(|v| v.effect)
    };
    assert_eq!(call("mcp__github__create_issue"), Some(Effect::Ask));
    assert_eq!(call("mcp__githubx__thing"), None);
    assert_eq!(call("mcp__fs__write_file"), Some(Effect::Deny));
    assert_eq!(call("mcp__fs__read_file"), None);
}
