//! Who does the work: `/agent` (the lead and the sub-agents) and `/roles`
//! (which models serve each kind of delegated work). Both save to
//! `admin.json` and apply from the next run.

use crate::cli::render;
use crate::outln;

/// `/agent`: list the catalogue, or choose the lead or the sub-agents.
///
/// A choice is saved to admin.json and applies from the next `/run` and the
/// next session. This conversation's agent is not swapped mid-flight: its
/// transcript lives inside the agent that holds it.
pub fn handle_agent(
    arg: Option<&str>,
    catalogue: &crate::agent::catalogue::Catalogue,
    lead: &crate::agent::DetectedAgent,
    data_dir: &std::path::Path,
) {
    match agent_command(arg, catalogue, lead, data_dir) {
        Ok(text) => outln!("{}", text.trim_end()),
        Err(why) => render::print_error(&why),
    }
}

/// What `/agent` does with its argument, and what it says.
///
/// - nothing: the catalogue, the lead, and the sub-agents with where they
///   came from;
/// - `<id>`: saves the lead;
/// - `sub <id>`: saves the sub-agents; `sub` or `sub same` clears that back
///   to the lead's default.
///
/// An id must be in the catalogue and usable, or nothing is written. The
/// settings file is READ before it is written, and a malformed one is refused
/// rather than replaced by a file holding only this one choice.
fn agent_command(
    arg: Option<&str>,
    catalogue: &crate::agent::catalogue::Catalogue,
    lead: &crate::agent::DetectedAgent,
    data_dir: &std::path::Path,
) -> Result<String, String> {
    let mut admin = crate::admin::AdminConfig::load(data_dir)?;
    let Some(arg) = arg.map(str::trim).filter(|a| !a.is_empty()) else {
        let sub = catalogue.sub_agent(lead, admin.sub_agent.as_deref());
        return Ok(catalogue.report(lead, &sub, admin.resolve_agent().value.as_deref()));
    };
    let (word, rest) = arg
        .split_once(char::is_whitespace)
        .map_or((arg, ""), |(word, rest)| (word, rest.trim()));
    const APPLIES: &str = "Applies from the next /run and the next session.";

    if word.eq_ignore_ascii_case("sub") {
        if rest.is_empty() || rest.eq_ignore_ascii_case("same") {
            admin.sub_agent = None;
            admin.save(data_dir)?;
            return Ok(format!(
                "Sub-agents follow the lead's default again. {APPLIES}"
            ));
        }
        let chosen = catalogue.route_for(rest)?;
        admin.sub_agent = Some(chosen.id.clone());
        admin.save(data_dir)?;
        return Ok(format!(
            "Sub-agents: {} (your choice), saved to admin.json. {APPLIES}",
            chosen.id
        ));
    }
    if !rest.is_empty() {
        return Err("/agent [id | sub [id]]  (one id at a time)".to_string());
    }

    let chosen = catalogue.route_for(word)?;
    admin.agent = Some(chosen.id.clone());
    admin.save(data_dir)?;
    let mut said = if chosen.id == lead.id {
        format!(
            "Lead: {} saved to admin.json — already this session's lead.",
            chosen.id
        )
    } else {
        format!(
            "Lead: {} saved to admin.json. {APPLIES} This conversation stays on {}.",
            chosen.id, lead.id
        )
    };
    if crate::admin::AdminConfig::default().resolve_agent().source
        == crate::admin::Source::Environment
    {
        said.push_str(&format!(
            "\n{} is set in this shell and overrides the saved choice until it is unset.",
            crate::admin::AGENT_ENV
        ));
    }
    Ok(said)
}

/// `/roles`: which models serve each kind of delegated work, or set one.
pub fn handle_roles(arg: Option<&str>, data_dir: &std::path::Path) {
    match roles_command(arg, data_dir) {
        Ok(text) => outln!("{}", text.trim_end()),
        Err(why) => render::print_error(&why),
    }
}

/// What `/roles` does with its argument, and what it says.
///
/// - nothing: every role, what it resolves to, and which layer decided it;
/// - `<role> <model|@tier>...`: saves that role's candidates, in order;
/// - `<role> default` (or `clear`): removes the role from the file, so it runs
///   on the session model unless the shell says otherwise.
///
/// The role must be a known one and every `@tier` must exist, or nothing is
/// written. The settings file is READ before it is written, and a malformed
/// one is refused rather than replaced by a file holding only this role.
fn roles_command(arg: Option<&str>, data_dir: &std::path::Path) -> Result<String, String> {
    use crate::roles::{KNOWN_ROLES, MAX_CANDIDATES, TIER_PREFIX};
    const USAGE: &str = "/roles [role <model|@tier>... | role default]";
    const APPLIES: &str = "Applies from the next /run and the next session.";

    let mut admin = crate::admin::AdminConfig::load(data_dir)?;
    let Some(arg) = arg.map(str::trim).filter(|a| !a.is_empty()) else {
        return Ok(roles_listing(&admin));
    };
    let (typed_role, rest) = arg
        .split_once(char::is_whitespace)
        .map_or((arg, ""), |(word, rest)| (word, rest.trim()));
    let role = typed_role.to_ascii_lowercase();
    if !KNOWN_ROLES.contains(&role.as_str()) {
        return Err(format!(
            "'{typed_role}' is not a role. The roles are: {}",
            KNOWN_ROLES.join(", ")
        ));
    }
    if rest.is_empty() {
        return Err(format!("{USAGE}  (name the models for {role})"));
    }

    // Spaces and commas both separate, so `a b` and `a, b` mean the same.
    let joined = rest.split_whitespace().collect::<Vec<_>>().join(",");
    let given = joined.split(',').filter(|m| !m.trim().is_empty()).count();
    if given > MAX_CANDIDATES {
        return Err(format!(
            "{role} lists {given} candidates; a role may hold at most {MAX_CANDIDATES}."
        ));
    }
    let candidates = crate::roles::parse_candidates(&joined);

    let clears =
        |word: &str| word.eq_ignore_ascii_case("default") || word.eq_ignore_ascii_case("clear");
    let overridden = crate::admin::AdminConfig::default().role_source(&role)
        == crate::admin::Source::Environment;
    let env_note = |said: &mut String| {
        if overridden {
            said.push_str(&format!(
                "\n{}{} is set in this shell and overrides the saved choice until it is unset.",
                crate::roles::ROLE_ENV_PREFIX,
                role.to_ascii_uppercase()
            ));
        }
    };

    if candidates.iter().any(|c| clears(c)) {
        if candidates.len() > 1 {
            return Err(format!(
                "'default' clears {role} and takes nothing after it. {USAGE}"
            ));
        }
        admin.roles.remove(&role);
        admin.save(data_dir)?;
        let mut said =
            format!("{role}: removed from admin.json — it runs on the session model. {APPLIES}");
        env_note(&mut said);
        return Ok(said);
    }

    let unknown: Vec<&str> = candidates
        .iter()
        .map(String::as_str)
        .filter(|c| {
            c.strip_prefix(TIER_PREFIX)
                .is_some_and(|name| !admin.tiers.contains_key(name.trim()))
        })
        .collect();
    if !unknown.is_empty() {
        let defined = if admin.tiers.is_empty() {
            "no tiers are defined in admin.json".to_string()
        } else {
            format!(
                "the tiers are: {}",
                admin
                    .tiers
                    .keys()
                    .map(|t| format!("{TIER_PREFIX}{t}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return Err(format!(
            "{} names no tier; {defined}. Nothing was saved.",
            unknown.join(", ")
        ));
    }

    admin.roles.insert(role.clone(), candidates.clone());
    admin.save(data_dir)?;
    let mut said = format!(
        "{role}: {} (your choice), saved to admin.json. {APPLIES}",
        candidates.join(" > ")
    );
    env_note(&mut said);
    Ok(said)
}

/// One line per role, in the order the harness knows them.
///
/// Built from the same resolution the run uses (`resolve_roles`,
/// `role_source`, `unknown_tiers`), so this cannot say one thing while the
/// run does another.
fn roles_listing(admin: &crate::admin::AdminConfig) -> String {
    use crate::admin::Source;
    let table = admin.resolve_roles().value;
    let mut out = String::from("Roles (candidates, tried in order <- where they came from)\n");
    for role in crate::roles::KNOWN_ROLES {
        let candidates = table.candidates_for(role);
        let models = if candidates.is_empty() {
            "session model".to_string()
        } else {
            candidates.join(" > ")
        };
        let source = match admin.role_source(role) {
            Source::Environment => {
                let var = format!(
                    "{}{}",
                    crate::roles::ROLE_ENV_PREFIX,
                    role.to_ascii_uppercase()
                );
                if admin.roles.get(role).is_some_and(|m| !m.is_empty()) {
                    format!("{var}, over your choice")
                } else {
                    var
                }
            }
            Source::Harness => format!("the harness in {}", crate::harness::HARNESS_ENV),
            Source::AdminFile => "your choice".to_string(),
            Source::Flag | Source::BuiltIn => "not set".to_string(),
        };
        let purpose = crate::roles::role_purpose(role)
            .map(short_purpose)
            .unwrap_or_default();
        out.push_str(&format!(
            "  {role:<10} {models}  <- {source}  ({purpose})\n"
        ));
    }
    for (role, reference) in admin.unknown_tiers() {
        out.push_str(&format!(
            "  !! {}\n",
            crate::admin::unknown_tier_warning(&role, &reference)
        ));
    }
    if admin.tiers.is_empty() {
        out.push_str("  tiers      none defined\n");
    } else {
        for (tier, models) in &admin.tiers {
            let name = format!("@{tier}");
            out.push_str(&format!("  {name:<10} {}  (tier)\n", models.join(" > ")));
        }
    }
    out.push_str("Set one with /roles <role> <model|@tier>...; /roles <role> default clears it.\n");
    out
}

/// The opening phrase of a role's purpose, short enough to share its line.
fn short_purpose(purpose: &str) -> String {
    const MAX: usize = 32;
    let phrase = purpose
        .split(['—', ',', '.', ':'])
        .next()
        .unwrap_or(purpose)
        .trim()
        .to_lowercase();
    if phrase.chars().count() <= MAX {
        return phrase;
    }
    let mut short = String::new();
    for word in phrase.split_whitespace() {
        if short.chars().count() + word.chars().count() + 1 > MAX {
            break;
        }
        if !short.is_empty() {
            short.push(' ');
        }
        short.push_str(word);
    }
    format!("{short}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn installed(id: &str, protocol: crate::agent::Protocol) -> crate::agent::DetectedAgent {
        crate::agent::DetectedAgent {
            id: id.to_string(),
            display_name: id.to_string(),
            command: id.to_string(),
            path: None,
            version: Some("1.0".to_string()),
            protocol,
        }
    }

    /// Two usable local agents and one that is found but cannot be driven.
    fn two_agents_and_one_unverified() -> crate::agent::catalogue::Catalogue {
        crate::agent::catalogue::Catalogue::of_local(vec![
            installed("claude-code", crate::agent::Protocol::ClaudeStreamJson),
            installed(
                crate::agent::registry::CODEX,
                crate::agent::Protocol::CodexJson,
            ),
            installed("cursor", crate::agent::Protocol::Unverified),
        ])
    }

    fn session_lead() -> crate::agent::DetectedAgent {
        installed("claude-code", crate::agent::Protocol::ClaudeStreamJson)
    }

    /// **`/agent <unknown>` is refused, and nothing is written.** A choice that
    /// saved a name nothing answers to would fall back in silence next session.
    #[test]
    fn an_unknown_agent_is_refused_and_nothing_is_written() {
        let _guard = agent_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        let said = agent_command(
            Some("no-such-agent"),
            &two_agents_and_one_unverified(),
            &session_lead(),
            dir.path(),
        );
        assert!(
            said.as_ref()
                .is_err_and(|why| why.contains("not in the catalogue") && why.contains("codex")),
            "it must refuse and name what exists: {said:?}"
        );
        assert!(
            !crate::admin::AdminConfig::path_in(dir.path()).exists(),
            "a refused choice wrote the settings file"
        );
    }

    /// A known agent that cannot be used is refused with the reason, and
    /// nothing is written either.
    #[test]
    fn an_agent_that_cannot_be_used_is_refused_with_the_reason() {
        let _guard = agent_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        let said = agent_command(
            Some("cursor"),
            &two_agents_and_one_unverified(),
            &session_lead(),
            dir.path(),
        );
        assert!(
            said.as_ref()
                .is_err_and(|why| why.contains("cannot be used") && why.contains("protocol")),
            "{said:?}"
        );
        assert!(!crate::admin::AdminConfig::path_in(dir.path()).exists());
    }

    /// **`/agent <known>` persists the lead to admin.json**, by its catalogue
    /// id, and keeps every other setting already in the file.
    #[test]
    fn a_known_agent_is_saved_as_the_lead_in_admin_json() {
        let _guard = agent_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        crate::admin::AdminConfig {
            model: Some("kept-model".to_string()),
            ..Default::default()
        }
        .save(dir.path())
        .expect("seed");

        let said = agent_command(
            Some("CODEX"),
            &two_agents_and_one_unverified(),
            &session_lead(),
            dir.path(),
        );
        assert!(
            said.as_ref()
                .is_ok_and(|text| text.contains("next session")),
            "{said:?}"
        );
        let saved = crate::admin::AdminConfig::load(dir.path()).expect("load");
        assert_eq!(saved.agent.as_deref(), Some(crate::agent::registry::CODEX));
        assert_eq!(saved.model.as_deref(), Some("kept-model"));
    }

    /// **`/agent sub <id>` saves the sub-agents; `/agent sub` puts them back
    /// to the lead's default** — and bare `/agent` says which it is.
    #[test]
    fn the_sub_agent_is_saved_shown_and_cleared() {
        let _guard = agent_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        let catalogue = two_agents_and_one_unverified();
        let lead = session_lead();

        agent_command(Some("sub codex"), &catalogue, &lead, dir.path()).expect("saves");
        let saved = crate::admin::AdminConfig::load(dir.path()).expect("load");
        assert_eq!(
            saved.sub_agent.as_deref(),
            Some(crate::agent::registry::CODEX)
        );
        assert_eq!(
            saved.agent, None,
            "choosing sub-agents must not move the lead"
        );
        let listing = agent_command(None, &catalogue, &lead, dir.path()).expect("lists");
        assert!(
            listing.contains("sub-agents   codex (your choice)"),
            "{listing}"
        );

        assert!(agent_command(Some("sub cursor"), &catalogue, &lead, dir.path()).is_err());
        assert_eq!(
            crate::admin::AdminConfig::load(dir.path())
                .expect("load")
                .sub_agent
                .as_deref(),
            Some(crate::agent::registry::CODEX),
            "a refused choice must leave the saved one alone"
        );

        agent_command(Some("sub same"), &catalogue, &lead, dir.path()).expect("clears");
        assert_eq!(
            crate::admin::AdminConfig::load(dir.path())
                .expect("load")
                .sub_agent,
            None
        );
        let listing = agent_command(None, &catalogue, &lead, dir.path()).expect("lists");
        assert!(
            listing.contains("sub-agents   claude-code (default for claude-code)"),
            "{listing}"
        );
    }

    /// A settings file that does not parse is refused, not replaced by one
    /// holding only this choice — that would erase every other setting.
    #[test]
    fn a_malformed_settings_file_is_not_overwritten() {
        let _guard = agent_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = crate::admin::AdminConfig::path_in(dir.path());
        std::fs::write(&path, "{ not json").expect("write");
        let said = agent_command(
            Some("codex"),
            &two_agents_and_one_unverified(),
            &session_lead(),
            dir.path(),
        );
        assert!(said.is_err(), "{said:?}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "{ not json");
    }

    fn clear_role_env() {
        for role in crate::roles::KNOWN_ROLES {
            std::env::remove_var(role_env(role));
        }
    }

    fn role_env(role: &str) -> String {
        format!(
            "{}{}",
            crate::roles::ROLE_ENV_PREFIX,
            role.to_ascii_uppercase()
        )
    }

    /// A settings file with one tier, `top`, and nothing else.
    fn seed_with_a_tier(dir: &std::path::Path) {
        let mut tiers = std::collections::BTreeMap::new();
        tiers.insert(
            "top".to_string(),
            vec!["model-top-1".to_string(), "model-top-2".to_string()],
        );
        crate::admin::AdminConfig {
            model: Some("kept-model".to_string()),
            tiers,
            ..Default::default()
        }
        .save(dir)
        .expect("seed");
    }

    /// **`/roles <unknown> ...` is refused, and nothing is written.** A role
    /// nothing asks for would sit in the file looking configured forever.
    #[test]
    fn an_unknown_role_is_refused_and_nothing_is_written() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        let said = roles_command(Some("architect model-a"), dir.path());
        assert!(
            said.as_ref()
                .is_err_and(|why| why.contains("architect") && why.contains("explore")),
            "it must refuse and name the roles that exist: {said:?}"
        );
        assert!(
            !crate::admin::AdminConfig::path_in(dir.path()).exists(),
            "a refused role wrote the settings file"
        );
    }

    /// **A tier nobody defines is refused, and the file is left exactly as it
    /// was.** Saved, it would resolve to nothing and the role would quietly
    /// run on the session model.
    #[test]
    fn an_unknown_tier_is_refused_and_nothing_is_written() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        seed_with_a_tier(dir.path());
        let path = crate::admin::AdminConfig::path_in(dir.path());
        let before = std::fs::read_to_string(&path).expect("read");

        let said = roles_command(Some("review model-a @nope"), dir.path());
        assert!(
            said.as_ref()
                .is_err_and(|why| why.contains("@nope") && why.contains("@top")),
            "it must refuse, name the bad tier and the ones that exist: {said:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            before,
            "a refused tier changed the settings file"
        );
    }

    /// More candidates than a role may hold is refused rather than silently
    /// cut short — the ones dropped would be the operator's last fallbacks.
    #[test]
    fn too_many_candidates_are_refused_and_nothing_is_written() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        let many: Vec<String> = (0..=crate::roles::MAX_CANDIDATES)
            .map(|i| format!("model-{i}"))
            .collect();
        let said = roles_command(Some(&format!("plan {}", many.join(" "))), dir.path());
        assert!(
            said.as_ref()
                .is_err_and(|why| why.contains(&crate::roles::MAX_CANDIDATES.to_string())),
            "{said:?}"
        );
        assert!(!crate::admin::AdminConfig::path_in(dir.path()).exists());
    }

    /// **A role is saved lower-case, in order, and read back** — and every
    /// other setting in the file survives the write.
    #[test]
    fn a_role_is_saved_and_read_back() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        seed_with_a_tier(dir.path());

        let said = roles_command(Some("REVIEW model-a, @top"), dir.path());
        assert!(
            said.as_ref().is_ok_and(|text| text.contains("next /run")),
            "{said:?}"
        );
        let saved = crate::admin::AdminConfig::load(dir.path()).expect("load");
        assert_eq!(
            saved.roles.get("review"),
            Some(&vec!["model-a".to_string(), "@top".to_string()])
        );
        assert!(!saved.roles.contains_key("REVIEW"));
        assert_eq!(saved.model.as_deref(), Some("kept-model"));
        assert!(saved.tiers.contains_key("top"), "the ladder was lost");

        let listing = roles_command(None, dir.path()).expect("lists");
        let line = listing
            .lines()
            .find(|l| l.trim_start().starts_with("review"))
            .unwrap_or_default();
        assert!(
            line.contains("model-a > model-top-1 > model-top-2") && line.contains("your choice"),
            "{listing}"
        );
    }

    /// **`default` (or `clear`) removes the role**, so it runs on the session
    /// model again, and leaves the other roles alone.
    #[test]
    fn default_clears_the_role() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        roles_command(Some("plan model-a"), dir.path()).expect("saves plan");
        roles_command(Some("grunt model-b"), dir.path()).expect("saves grunt");

        roles_command(Some("Plan default"), dir.path()).expect("clears plan");
        let saved = crate::admin::AdminConfig::load(dir.path()).expect("load");
        assert!(!saved.roles.contains_key("plan"), "{:?}", saved.roles);
        assert_eq!(saved.roles.get("grunt"), Some(&vec!["model-b".to_string()]));

        roles_command(Some("grunt clear"), dir.path()).expect("clears grunt");
        let saved = crate::admin::AdminConfig::load(dir.path()).expect("load");
        assert!(saved.roles.is_empty(), "{:?}", saved.roles);
    }

    /// A settings file that does not parse is refused, not replaced by one
    /// holding only this role — that would erase every other setting.
    #[test]
    fn roles_leaves_a_malformed_settings_file_alone() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = crate::admin::AdminConfig::path_in(dir.path());
        std::fs::write(&path, "{ not json").expect("write");
        let said = roles_command(Some("plan model-a"), dir.path());
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "{ not json",
            "the malformed settings file was overwritten"
        );
        assert!(said.is_err(), "{said:?}");
    }

    /// **The listing names where every role came from**: the file, the shell,
    /// or nowhere — and a tier that does not exist is called out, not shown as
    /// a role nobody configured.
    #[test]
    fn the_listing_shows_where_each_role_came_from() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        let mut roles = std::collections::BTreeMap::new();
        roles.insert("implement".to_string(), vec!["model-a".to_string()]);
        roles.insert("diagnose".to_string(), vec!["@missing".to_string()]);
        crate::admin::AdminConfig {
            roles,
            ..Default::default()
        }
        .save(dir.path())
        .expect("seed");
        std::env::set_var(role_env("plan"), "model-env");

        let listing = roles_command(None, dir.path());
        clear_role_env();
        let listing = listing.expect("lists");

        let line_for = |role: &str| {
            listing
                .lines()
                .find(|l| l.trim_start().starts_with(role))
                .unwrap_or_default()
                .to_string()
        };
        let implement = line_for("implement");
        assert!(
            implement.contains("model-a") && implement.contains("your choice"),
            "{listing}"
        );
        let plan = line_for("plan");
        assert!(
            plan.contains("model-env") && plan.contains("INCODE_ROLE_PLAN"),
            "{listing}"
        );
        let explore = line_for("explore");
        assert!(
            explore.contains("session model") && explore.contains("not set"),
            "{listing}"
        );
        for role in crate::roles::KNOWN_ROLES {
            assert!(
                listing.lines().any(|l| l.trim_start().starts_with(role)),
                "`{role}` is missing from the listing:\n{listing}"
            );
        }
        assert!(
            listing.contains("diagnose names @missing, which no tier defines"),
            "an unknown tier must be flagged:\n{listing}"
        );
    }

    /// Saving a role the shell overrides still saves it, and says the saved
    /// choice will not apply while the variable is set.
    #[test]
    fn a_role_the_shell_overrides_is_saved_with_a_warning() {
        let _guard = agent_lock();
        clear_role_env();
        let dir = tempfile::tempdir().expect("temp dir");
        std::env::set_var(role_env("explore"), "model-env");
        let said = roles_command(Some("explore model-a"), dir.path());
        clear_role_env();

        assert!(
            said.as_ref().is_ok_and(
                |text| text.contains("INCODE_ROLE_EXPLORE") && text.contains("overrides")
            ),
            "{said:?}"
        );
        let saved = crate::admin::AdminConfig::load(dir.path()).expect("load");
        assert_eq!(
            saved.roles.get("explore"),
            Some(&vec!["model-a".to_string()])
        );
    }
}
