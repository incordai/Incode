//! Which profile a turn is pointed at, and the logins outside the profiles
//! that keep it where it is.

use super::profiles::{choose_profile, find_expiry};
use super::*;

/// Build a home directory holding the named profiles.
///
/// `hours` is how long each session has left; negative means it expired.
fn home_with(profiles: &[(&str, Option<f64>)]) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("tmp");
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("fits");
    for (name, hours) in profiles {
        let dir = home.path().join(name);
        std::fs::create_dir_all(&dir).expect("dir");
        if let Some(h) = hours {
            #[allow(clippy::cast_possible_truncation)]
            let expires = now_ms + (h * 3_600_000.0) as i64;
            // Same nesting the real file uses, and the same field names.
            // The token fields are deliberately absent: the code must never
            // need them, and a test that supplied them would hide it if it did.
            std::fs::write(
                dir.join(".credentials.json"),
                format!(r#"{{"claudeAiOauth":{{"expiresAt":{expires},"scopes":[]}}}}"#),
            )
            .expect("write");
        }
    }
    home
}

#[test]
fn the_profile_with_a_live_session_is_chosen_over_the_default() {
    // The exact situation that broke every turn for a day: the default
    // profile's session had expired while another profile was still good,
    // and the agent fell back to the default because nothing said otherwise.
    let home = home_with(&[(".claude", Some(-4.0)), (".claude-work", Some(2.0))]);
    let chosen = pick_usable(profiles_in(home.path())).expect("one is live");
    assert!(
        chosen.dir.ends_with(".claude-work"),
        "chose {:?}, but only .claude-work can work",
        chosen.dir
    );
}

#[test]
fn the_longest_lived_session_wins() {
    let home = home_with(&[
        (".claude", Some(1.0)),
        (".claude-work", Some(9.0)),
        (".claude-other", Some(4.0)),
    ]);
    let chosen = pick_usable(profiles_in(home.path())).expect("live");
    assert!(chosen.dir.ends_with(".claude-work"), "{:?}", chosen.dir);
}

#[test]
fn every_session_expired_means_no_choice_rather_than_a_bad_one() {
    // Picking an expired profile would produce the same unexplained failure
    // this replaces. Choosing nothing lets the turn fail with the message
    // that actually names the problem.
    let home = home_with(&[(".claude", Some(-1.0)), (".claude-work", Some(-9.0))]);
    assert_eq!(pick_usable(profiles_in(home.path())), None);
}

#[test]
fn a_session_about_to_expire_is_not_treated_as_live() {
    // Starting a turn on a session with seconds left fails halfway through,
    // which is worse than not starting it.
    let home = home_with(&[(".claude", Some(0.01))]); // 36 seconds
    assert_eq!(pick_usable(profiles_in(home.path())), None);
}

#[test]
fn only_directories_count_as_profiles() {
    // The home also holds `.claude.json` and a long tail of its backups.
    let home = home_with(&[(".claude-work", Some(3.0))]);
    std::fs::write(home.path().join(".claude.json"), "{}").expect("write");
    std::fs::write(home.path().join(".claude.json.backup"), "{}").expect("write");
    let found = profiles_in(home.path());
    assert_eq!(found.len(), 1, "found {found:?}");
    assert!(found[0].dir.ends_with(".claude-work"));
}

#[test]
fn a_directory_with_no_session_recorded_is_skipped() {
    let home = home_with(&[(".claude", None), (".claude-work", Some(3.0))]);
    let found = profiles_in(home.path());
    assert_eq!(found.len(), 1, "found {found:?}");
}

#[test]
fn the_expiry_is_found_wherever_it_is_nested() {
    let doc: serde_json::Value =
        serde_json::from_str(r#"{"a":{"b":{"expiresAt":1234567890123}}}"#).expect("parse");
    assert_eq!(find_expiry(&doc), Some(1_234_567_890_123));
    let none: serde_json::Value = serde_json::from_str(r#"{"a":{"b":1}}"#).expect("parse");
    assert_eq!(find_expiry(&none), None);
}

/// An environment holding exactly these variables.
fn env_of(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |name| {
        vars.iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| (*v).to_string())
    }
}

/// **A LOGIN THE CLI USES ANYWAY IS NEVER SWAPPED FOR A FILE PROFILE.**
/// Redirecting to a live `.credentials.json` moved a Keychain login out
/// of reach on macOS, and with a credential in the environment it changed
/// nothing but which settings the CLI read.
#[test]
fn a_login_outside_the_profiles_keeps_the_agent_where_it_is() {
    let home = home_with(&[(".claude-work", Some(5.0))]);
    let outside = || Some(OutsideLogin::Environment("CLAUDE_CODE_OAUTH_TOKEN"));
    assert_eq!(
        choose_profile(profiles_in(home.path()), &outside),
        None,
        "a live file profile replaced a login the CLI already had"
    );

    let nothing_else = || None;
    let chosen = choose_profile(profiles_in(home.path()), &nothing_else).expect("live");
    assert!(
        chosen.dir.ends_with(".claude-work"),
        "with no other login, the live profile is still the choice"
    );
}

/// The Keychain costs a process, so it is not asked when no live file
/// profile could be chosen anyway.
#[test]
fn the_keychain_is_not_asked_when_no_profile_is_live() {
    let home = home_with(&[(".claude", Some(-2.0))]);
    let asked = std::cell::Cell::new(false);
    let outside = || {
        asked.set(true);
        Some(OutsideLogin::Keychain)
    };
    assert_eq!(choose_profile(profiles_in(home.path()), &outside), None);
    assert!(
        !asked.get(),
        "a process was spent on a question nobody needed"
    );
}

/// A credential variable is a login, and only its NAME is reported.
#[test]
fn a_credential_in_the_environment_is_a_login_named_by_its_variable() {
    let env = env_of(&[("CLAUDE_CODE_OAUTH_TOKEN", "x")]);
    assert_eq!(
        login_outside_profiles(&env, &|| false),
        Some(OutsideLogin::Environment("CLAUDE_CODE_OAUTH_TOKEN"))
    );
    let empty = env_of(&[("ANTHROPIC_API_KEY", "")]);
    assert_eq!(
        login_outside_profiles(&empty, &|| false),
        None,
        "an empty variable carries no credential"
    );
}

/// Provider switches are read the way the CLI reads them: `0` is off.
#[test]
fn a_provider_switch_counts_only_when_it_is_on() {
    for on in ["1", "true", "YES", " on "] {
        let env = move |name: &str| (name == "CLAUDE_CODE_USE_BEDROCK").then(|| on.to_string());
        assert_eq!(
            login_outside_profiles(&env, &|| false),
            Some(OutsideLogin::Environment("CLAUDE_CODE_USE_BEDROCK")),
            "`{on}` switches the provider on"
        );
    }
    for off in ["0", "false", "no", ""] {
        let env = move |name: &str| (name == "CLAUDE_CODE_USE_VERTEX").then(|| off.to_string());
        assert_eq!(
            login_outside_profiles(&env, &|| false),
            None,
            "`{off}` leaves the provider off"
        );
    }
}

/// A base URL says where requests go, not what authenticates them; on its
/// own it is the state in which every turn fails, and must not read as a
/// login.
#[test]
fn a_base_url_alone_is_not_a_login() {
    let env = env_of(&[("ANTHROPIC_BASE_URL", "https://gateway.example")]);
    assert_eq!(login_outside_profiles(&env, &|| false), None);
}

/// The default Keychain entry belongs to the default profile; once the
/// operator names another directory the CLI looks under another service,
/// so the default entry says nothing about this session.
#[test]
fn the_keychain_counts_only_for_the_default_profile() {
    let nothing = env_of(&[]);
    assert_eq!(
        login_outside_profiles(&nothing, &|| true),
        Some(OutsideLogin::Keychain)
    );
    let elsewhere = env_of(&[("CLAUDE_CONFIG_DIR", "/home/op/.claude-work")]);
    assert_eq!(login_outside_profiles(&elsewhere, &|| true), None);
}
