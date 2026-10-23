use super::*;

mod commands;
mod layers;
mod paths;

fn ctx(workspace: &str) -> PathContext {
    PathContext {
        workspace: PathBuf::from(workspace),
        home: Some(PathBuf::from("/home/me")),
    }
}

fn set(layer: Layer, allow: &[&str], ask: &[&str], deny: &[&str]) -> PermissionRules {
    let lists = RuleLists {
        allow: allow.iter().map(|s| (*s).to_string()).collect(),
        ask: ask.iter().map(|s| (*s).to_string()).collect(),
        deny: deny.iter().map(|s| (*s).to_string()).collect(),
    };
    let mut rules = PermissionRules::none();
    rules.add(&lists, layer, Some(Path::new("/home/me")));
    rules
}

fn bash(rules: &PermissionRules, command: &str) -> Option<Effect> {
    rules
        .decide("Bash", &Subject::Command(command.to_string()), &ctx("/ws"))
        .map(|v| v.effect)
}

fn path(rules: &PermissionRules, tool: &str, p: &str) -> Option<Effect> {
    rules
        .decide(tool, &Subject::Paths(vec![p.to_string()]), &ctx("/ws"))
        .map(|v| v.effect)
}
