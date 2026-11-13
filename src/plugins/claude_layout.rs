//! Plugins in the layout the Claude Code CLI installs them in, read as incode
//! plugins: once installed (`incode plugin add`), their commands, skills,
//! sub-agents, hooks and MCP servers serve every agent incode drives, because
//! incode runs them — not the agent CLI.
//!
//! | In the plugin | Read as |
//! |---|---|
//! | `.claude-plugin/plugin.json`: `name`, `version`, `description` | its manifest ([`manifest`]) |
//! | `commands/`, `skills/`, `agents/` | the same folders as a `plugin.toml` plugin's |
//! | `hooks/hooks.json`, or `hooks` in `plugin.json` (a path or the object) | [`hooks`], translated |
//! | `.mcp.json`, or `mcpServers` in `plugin.json` (a path or the object) | [`servers`] |
//! | `${CLAUDE_PLUGIN_ROOT}` | the plugin's folder |
//!
//! A folder with both manifests is a `plugin.toml` plugin; this layout is
//! read only when there is no `plugin.toml` ([`is_one`]).
//!
//! # Hooks
//!
//! `{"hooks": {"PreToolUse": [{"matcher": "Edit|Write", "hooks": [{"type":
//! "command", "command": "…"}]}]}}` — each event becomes the
//! [`crate::hooks::HookConfig`] field of the same name, each command a
//! [`crate::hooks::HookCommand`] marked `posix` (it was written for a POSIX
//! shell) with the plugin's folder in its environment. The stdin event they
//! are handed is already the shape they expect (`hooks` module doc).
//!
//! - `PreToolUse` / `PostToolUse` keep their `matcher`; incode's tools carry
//!   the same names (`Bash`, `Read`, `Edit`, `mcp__<server>__<tool>`), and a
//!   plugin's own servers' tools, `mcp__plugin_<plugin>_<server>__…` there,
//!   are `mcp__<plugin>_<server>__…` here, so the matcher is rewritten.
//! - `SessionStart` and `PreCompact` matchers are weighed as the hooks are
//!   read, against what incode starts and compacts with (`startup`,
//!   `manual`): a hook for `resume` alone is never run.
//! - A `Notification` hook with a matcher names notification kinds incode
//!   does not have, so only the ones without are kept.
//! - Left out, and said: an event incode has no point for, a hook whose
//!   `type` is not `command`, and a matcher that is not a regular expression.
//! - `timeout` is not read: every hook is bounded at
//!   [`crate::hooks::HOOK_TIMEOUT`].
//!
//! # Wired to
//!
//! Every reader is in [`super`]: `load_one` takes [`manifest`],
//! `Plugin::hooks` takes [`hooks`], `Plugin::servers` takes [`servers`], and
//! `plugins::session_agents` reads `agents/` for `subagents::load`. Servers
//! are built by the same `mcp_config::ServerEntry::to_spec` a `.mcp.json`
//! server is, so `${VAR}` and `${VAR:-default}` are expanded the same way.
//! The sibling layout, `plugin.toml`, is read in `super` itself.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::hooks::{HookCommand, HookConfig};

/// The manifest, under the plugin's folder.
pub const MANIFEST: &str = ".claude-plugin/plugin.json";

/// What the plugin's folder is called in its files.
pub const ROOT_VAR: &str = "CLAUDE_PLUGIN_ROOT";

/// Where its hooks are when `plugin.json` does not say.
const HOOKS_FILE: &str = "hooks/hooks.json";

/// Where its servers are when `plugin.json` does not say.
const SERVERS_FILE: &str = ".mcp.json";

/// The keys of `plugin.json` that name more than incode reads: extra
/// command or agent paths, and parts incode has no use for.
const UNREAD_KEYS: [&str; 4] = ["commands", "agents", "outputStyles", "lspServers"];

/// Whether `root` is a plugin in this layout and not a `plugin.toml` one.
#[must_use]
pub fn is_one(root: &Path) -> bool {
    !root.join(super::MANIFEST).is_file() && root.join(MANIFEST).is_file()
}

/// `plugin.json`'s name, version and description. Everything else it says is
/// read where it is used.
///
/// # Errors
///
/// It is not JSON, or names no usable plugin name.
pub fn manifest(text: &str) -> Result<super::Manifest, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let field = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let manifest = super::Manifest {
        name: field("name"),
        version: field("version"),
        description: field("description"),
    };
    if manifest.name.is_empty() {
        return Err("`name` is missing".to_string());
    }
    super::check_name(&manifest.name)?;
    Ok(manifest)
}

/// The plugin's `plugin.json`, parsed.
fn plugin_json(root: &Path) -> Result<Value, String> {
    let path = root.join(MANIFEST);
    let text = crate::text::read_authored(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// What `plugin.json` says that incode does not read, one line each, for
/// `incode plugin list` — so a plugin that relies on them is not mistaken
/// for one that works whole.
#[must_use]
pub fn unread(root: &Path) -> Vec<String> {
    let Ok(json) = plugin_json(root) else {
        return Vec::new();
    };
    UNREAD_KEYS
        .iter()
        .filter(|key| json.get(**key).is_some())
        .map(|key| match *key {
            "commands" | "agents" => {
                format!("`{key}` in plugin.json is not read; only its {key}/ folder is")
            }
            other => format!("`{other}` in plugin.json is not used by incode"),
        })
        .collect()
}

/// A part of the plugin `plugin.json` names under `key` — a path inside the
/// folder, or the object itself — else the file at `default`: the JSON, and
/// where it came from. `None` when there is neither.
fn part(root: &Path, key: &str, default: &str) -> Result<Option<(Value, PathBuf)>, String> {
    let named = plugin_json(root)?.get(key).cloned();
    let path = match named {
        Some(Value::String(rel)) => inside(root, &rel)?,
        Some(Value::Null) | None => root.join(default),
        Some(object @ Value::Object(_)) => return Ok(Some((object, root.join(MANIFEST)))),
        Some(other) => {
            return Err(format!(
                "{}: `{key}` is {other}, not a path or an object",
                root.join(MANIFEST).display()
            ))
        }
    };
    if !path.is_file() {
        return Ok(None);
    }
    let text = crate::text::read_authored(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some((value, path)))
}

/// `rel` under `root`, refused when it would leave it: a plugin's files are
/// read from its own folder only.
fn inside(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = Path::new(rel);
    if rel
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!(
            "{}: `{}` is not a path inside the plugin",
            root.join(MANIFEST).display(),
            rel.display()
        ));
    }
    Ok(root.join(rel))
}

/// The plugin's hooks, translated, and a line for each one left out. `None`
/// when it has none.
///
/// # Errors
///
/// Its hooks file or `plugin.json` cannot be read.
pub fn hooks(root: &Path) -> Result<Option<(HookConfig, Vec<String>)>, String> {
    let Some((value, from)) = part(root, "hooks", HOOKS_FILE)? else {
        return Ok(None);
    };
    let (mut config, left_out) = translate_hooks(&value);
    for hook in config.commands_mut() {
        hook.plugin_root = Some(root.to_path_buf());
        hook.posix = true;
    }
    let left_out = left_out
        .into_iter()
        .map(|why| format!("{}: {why}", from.display()))
        .collect();
    Ok(Some((config, left_out)))
}

/// `{"hooks": {Event: [{matcher, hooks: [{type, command}]}]}}` as incode's
/// events, and why each hook left out was. The object may also be the bare
/// event map.
#[must_use]
pub fn translate_hooks(value: &Value) -> (HookConfig, Vec<String>) {
    let mut config = HookConfig::default();
    let mut left_out = Vec::new();
    let events = value
        .get("hooks")
        .and_then(Value::as_object)
        .or_else(|| value.as_object());
    let Some(events) = events else {
        return (config, vec!["no `hooks` object".to_string()]);
    };
    for (event, groups) in events {
        if event == "description" {
            continue;
        }
        let Some(field) = field_for(&mut config, event) else {
            left_out.push(format!("`{event}` hooks: incode has no such event"));
            continue;
        };
        for group in groups.as_array().into_iter().flatten() {
            let matcher = group
                .get("matcher")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|m| !m.is_empty() && *m != "*")
                .map(|m| m.replace("mcp__plugin_", "mcp__"));
            if let Err(why) = kept(event, matcher.as_deref()) {
                left_out.push(why);
                continue;
            }
            for hook in group
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let kind = hook
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("command");
                let command = hook
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|c| !c.is_empty());
                match (kind, command) {
                    ("command", Some(command)) => field.push(HookCommand {
                        command: command.to_string(),
                        matcher: tool_event(event).then(|| matcher.clone()).flatten(),
                        ..HookCommand::default()
                    }),
                    ("command", None) => {
                        left_out.push(format!("a `{event}` hook with no `command`"));
                    }
                    (other, _) => left_out.push(format!(
                        "a `{event}` hook of type `{other}`: incode runs only `command` hooks"
                    )),
                }
            }
        }
    }
    (config, left_out)
}

/// Whether a group with `matcher` under `event` is kept, and why not.
fn kept(event: &str, matcher: Option<&str>) -> Result<(), String> {
    let Some(matcher) = matcher else {
        return Ok(());
    };
    let probe = HookCommand {
        matcher: Some(matcher.to_string()),
        ..HookCommand::default()
    };
    // What incode starts and compacts with: `cli::agent_turn` and `task_run`
    // pass `startup`, and `/compact` is the only compaction.
    let subject = match event {
        "PreToolUse" | "PostToolUse" => {
            return probe
                .matches("")
                .map(|_| ())
                .map_err(|why| format!("a `{event}` hook was left out: {why}"))
        }
        "SessionStart" => "startup",
        "PreCompact" => "manual",
        "Notification" => {
            return Err(format!(
                "a `Notification` hook for `{matcher}`: incode's notifications are not those \
                 kinds"
            ))
        }
        // The other events have no matcher to weigh.
        _ => return Ok(()),
    };
    match probe.matches(subject) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "a `{event}` hook for `{matcher}`: incode's {event} is always `{subject}`"
        )),
        Err(why) => Err(format!("a `{event}` hook was left out: {why}")),
    }
}

/// Whether `event` is about one tool call, so its matcher is kept.
fn tool_event(event: &str) -> bool {
    matches!(event, "PreToolUse" | "PostToolUse")
}

/// The field of `config` that `event` fills, when incode has the event.
fn field_for<'a>(config: &'a mut HookConfig, event: &str) -> Option<&'a mut Vec<HookCommand>> {
    Some(match event {
        "PreToolUse" => &mut config.pre_tool_use,
        "PostToolUse" => &mut config.post_tool_use,
        "UserPromptSubmit" => &mut config.user_prompt_submit,
        "PreCompact" => &mut config.pre_compact,
        "Stop" => &mut config.stop,
        "Notification" => &mut config.notification,
        "SessionStart" => &mut config.session_start,
        "SessionEnd" => &mut config.session_end,
        "SubagentStop" => &mut config.subagent_stop,
        _ => return None,
    })
}

/// The plugin's MCP servers, named `<plugin>_<server>`, with
/// `${CLAUDE_PLUGIN_ROOT}` as its folder and every other `${VAR}` from the
/// environment. The file may hold `{"mcpServers": {…}}` or the bare map.
///
/// # Errors
///
/// A file cannot be read, a server is not a server, or names a variable
/// that is unset.
pub(crate) fn servers(
    root: &Path,
    plugin: &str,
) -> Result<Vec<crate::toolbox::ServerSpec>, String> {
    let Some((value, from)) = part(root, "mcpServers", SERVERS_FILE)? else {
        return Ok(Vec::new());
    };
    let map = match value.get("mcpServers") {
        Some(Value::Object(map)) => map.clone(),
        _ => match value {
            Value::Object(map) => map,
            _ => return Err(format!("{}: not an object of servers", from.display())),
        },
    };
    let folder = root.to_string_lossy().into_owned();
    let lookup = |name: &str| {
        if name == ROOT_VAR || name == "INCODE_PLUGIN_ROOT" {
            Some(folder.clone())
        } else {
            std::env::var(name).ok()
        }
    };
    let mut specs = Vec::new();
    for (name, def) in map {
        let entry: crate::mcp_config::ServerEntry = serde_json::from_value(def)
            .map_err(|e| format!("{}: server `{name}`: {e}", from.display()))?;
        let mut spec = entry
            .to_spec(&format!("{plugin}_{name}"), &lookup)
            .map_err(|e| format!("{}: server `{name}`: {e}", from.display()))?;
        if let crate::toolbox::Transport::Stdio { env, .. } = &mut spec.transport {
            if !env.iter().any(|(k, _)| k == ROOT_VAR) {
                env.push((ROOT_VAR.to_string(), folder.clone()));
            }
        }
        specs.push(spec);
    }
    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{add, Scope};

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    /// A plugin in this layout with one of each part.
    fn sample(dir: &Path) -> PathBuf {
        let root = dir.join("source");
        write(
            &root.join(MANIFEST),
            r#"{"name": "review-kit", "version": "1.2.0", "description": "Reviews",
                "author": {"name": "someone"}, "outputStyles": "./styles"}"#,
        );
        write(&root.join("commands/check.md"), "Check $ARGUMENTS\n");
        write(
            &root.join("skills/triage/SKILL.md"),
            "---\nname: triage\ndescription: Sort the issues\n---\nbody\n",
        );
        write(
            &root.join("agents/critic.md"),
            "---\nname: critic\ndescription: Finds faults\nmodel: inherit\n---\nFind them.\n",
        );
        write(
            &root.join(HOOKS_FILE),
            r#"{"description": "guards", "hooks": {
                "PreToolUse": [{"matcher": "Edit|Write", "hooks": [
                    {"type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/guard.sh", "timeout": 30}]}],
                "SessionStart": [{"matcher": "resume", "hooks": [
                    {"type": "command", "command": "on-resume"}]}]}}"#,
        );
        write(
            &root.join(SERVERS_FILE),
            r#"{"db": {"command": "${CLAUDE_PLUGIN_ROOT}/bin/db", "args": ["--root", "${CLAUDE_PLUGIN_ROOT}"]},
                "docs": {"type": "http", "url": "https://docs.example/mcp",
                         "headers": {"X-Key": "${INCODE_TEST_UNSET_VAR:-none}"}}}"#,
        );
        root
    }

    /// **A PLUGIN IN THE OTHER CLI'S LAYOUT INSTALLS AS AN INCODE PLUGIN**:
    /// `incode plugin add` takes its folder, names it from `plugin.json`, and
    /// every part reaches the loader incode already has — commands, skills,
    /// agents, hooks (matcher kept, run in a POSIX shell, told the folder)
    /// and servers (`${CLAUDE_PLUGIN_ROOT}` filled, `${VAR:-default}`
    /// expanded).
    #[test]
    fn a_plugin_in_the_other_layout_installs_with_every_part() {
        let dir = tempfile::tempdir().expect("dir");
        let source = sample(dir.path());
        let target = dir.path().join("plugins");
        let plugin = add(&source.to_string_lossy(), &target).expect("installs");
        assert_eq!(plugin.manifest.name, "review-kit");
        assert_eq!(plugin.manifest.version, "1.2.0");
        assert_eq!(plugin.scope, Scope::User);
        let root = target.join("review-kit");
        assert!(is_one(&root));

        let found = crate::plugins::discover_in(None, &target);
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        assert_eq!(found.plugins.len(), 1);
        let listing = crate::plugins::listing(&found);
        assert!(
            listing.contains("1 command(s), 1 skill(s), 1 agent(s), 1 hook(s), 2 MCP server(s)"),
            "{listing}"
        );
        assert!(
            listing.contains("`outputStyles` in plugin.json"),
            "{listing}"
        );
        assert!(
            listing.contains("`SessionStart` hook for `resume`"),
            "{listing}"
        );

        let skills = plugin.skills();
        assert_eq!(skills[0].name, "review-kit:triage");

        let hooks = plugin.hooks().expect("hooks");
        assert_eq!(hooks.pre_tool_use, ["${CLAUDE_PLUGIN_ROOT}/guard.sh"]);
        let guard = &hooks.pre_tool_use[0];
        assert_eq!(guard.matcher.as_deref(), Some("Edit|Write"));
        assert!(guard.posix);
        assert_eq!(guard.plugin_root.as_deref(), Some(root.as_path()));
        assert!(hooks.session_start.is_empty(), "a `resume` hook was kept");

        let servers = plugin.servers().expect("servers");
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["review-kit_db", "review-kit_docs"]);
        let folder = root.to_string_lossy().into_owned();
        match &servers[0].transport {
            crate::toolbox::Transport::Stdio { command, args, env } => {
                assert_eq!(command, &PathBuf::from(format!("{folder}/bin/db")));
                assert_eq!(args, &["--root".to_string(), folder.clone()]);
                assert!(
                    env.contains(&(ROOT_VAR.to_string(), folder.clone())),
                    "{env:?}"
                );
            }
            other => panic!("expected stdio, got {other:?}"),
        }
        match &servers[1].transport {
            crate::toolbox::Transport::Http { headers, .. } => {
                assert_eq!(headers, &[("X-Key".to_string(), "none".to_string())]);
            }
            other => panic!("expected http, got {other:?}"),
        }

        assert_eq!(
            crate::plugins::remove("review-kit", &target).expect("removes"),
            root
        );
    }

    /// **EACH EVENT BECOMES INCODE'S, AND WHAT INCODE CANNOT RUN IS SAID**:
    /// a plugin's own server tools are renamed in the matcher; a start hook
    /// for `startup` is kept and one for `clear` is not; a compaction hook
    /// for `auto` is not; a notification hook with a matcher, a `prompt`
    /// hook, an unknown event and a broken matcher are each left out.
    #[test]
    fn hooks_are_translated_and_what_is_left_out_is_said() {
        let value: Value = serde_json::from_str(
            r#"{"hooks": {
              "PostToolUse": [{"matcher": "mcp__plugin_kit_db__.*", "hooks": [{"type": "command", "command": "log"}]},
                              {"matcher": "*", "hooks": [{"type": "command", "command": "every"}]},
                              {"matcher": "Edit(", "hooks": [{"type": "command", "command": "broken"}]}],
              "SessionStart": [{"matcher": "startup|resume", "hooks": [{"type": "command", "command": "hello"}]},
                               {"matcher": "clear", "hooks": [{"type": "command", "command": "cleared"}]}],
              "PreCompact": [{"matcher": "auto", "hooks": [{"type": "command", "command": "auto-only"}]}],
              "Notification": [{"matcher": "idle_prompt", "hooks": [{"type": "command", "command": "ping"}]},
                               {"hooks": [{"command": "always"}]}],
              "Stop": [{"hooks": [{"type": "prompt", "prompt": "Is it done?"}]}],
              "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "note"}]}],
              "PermissionRequest": [{"hooks": [{"type": "command", "command": "perm"}]}]
            }}"#,
        )
        .expect("json");
        let (config, left_out) = translate_hooks(&value);

        assert_eq!(config.post_tool_use, ["log", "every"]);
        assert_eq!(
            config.post_tool_use[0].matcher.as_deref(),
            Some("mcp__kit_db__.*")
        );
        assert_eq!(
            config.post_tool_use[0].matches("mcp__kit_db__query"),
            Ok(true)
        );
        assert_eq!(config.post_tool_use[1].matcher, None);
        assert_eq!(config.session_start, ["hello"]);
        assert_eq!(config.session_start[0].matcher, None);
        assert!(config.pre_compact.is_empty());
        assert_eq!(config.notification, ["always"]);
        assert!(config.stop.is_empty());
        assert_eq!(config.user_prompt_submit, ["note"]);
        assert_eq!(config.count(), 5);

        let said = left_out.join("\n");
        for expected in [
            "not a regular expression",
            "`SessionStart` hook for `clear`",
            "`PreCompact` hook for `auto`",
            "`Notification` hook for `idle_prompt`",
            "of type `prompt`",
            "`PermissionRequest` hooks",
        ] {
            assert!(said.contains(expected), "{expected} not said in:\n{said}");
        }
        assert_eq!(left_out.len(), 6, "{said}");
    }

    /// **`plugin.json` MAY NAME ITS HOOKS AND SERVERS** — a path inside the
    /// folder, or the object itself — and a path that leaves the folder is
    /// refused rather than read.
    #[test]
    fn plugin_json_names_parts_inside_its_folder_only() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("kit");
        write(
            &root.join(MANIFEST),
            r#"{"name": "kit", "hooks": "./config/guards.json",
                "mcpServers": {"db": {"command": "db"}}}"#,
        );
        write(
            &root.join("config/guards.json"),
            r#"{"hooks": {"PreToolUse": [{"hooks": [{"type": "command", "command": "guard"}]}]}}"#,
        );
        let (config, _) = hooks(&root).expect("reads").expect("has hooks");
        assert_eq!(config.pre_tool_use, ["guard"]);
        let servers = servers(&root, "kit").expect("servers");
        assert_eq!(servers[0].name, "kit_db");

        write(
            dir.path().join("outside.json").as_path(),
            r#"{"hooks": {}}"#,
        );
        write(
            &root.join(MANIFEST),
            r#"{"name": "kit", "hooks": "../outside.json"}"#,
        );
        let refused = hooks(&root).expect_err("a path outside the plugin");
        assert!(
            refused.contains("not a path inside the plugin"),
            "{refused}"
        );
    }

    /// A folder with both manifests is a `plugin.toml` plugin, and a
    /// `plugin.json` without a usable name is refused like a bad manifest.
    #[test]
    fn plugin_toml_wins_and_a_nameless_plugin_json_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let both = dir.path().join("both");
        write(&both.join(MANIFEST), r#"{"name": "json-name"}"#);
        write(
            &both.join(crate::plugins::MANIFEST),
            "name = \"toml-name\"\n",
        );
        assert!(!is_one(&both));

        let nameless = dir.path().join("nameless");
        write(&nameless.join(MANIFEST), r#"{"version": "1.0.0"}"#);
        let found = crate::plugins::discover_in(None, dir.path());
        let names: Vec<&str> = found
            .plugins
            .iter()
            .map(|p| p.manifest.name.as_str())
            .collect();
        assert_eq!(names, ["toml-name"]);
        assert!(
            found
                .problems
                .iter()
                .any(|p| p.contains("`name` is missing")),
            "{:?}",
            found.problems
        );
    }
}
