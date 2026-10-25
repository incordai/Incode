//! What this machine enforces of a policy, part by part, before anything
//! runs: [`enforcement`]. `Sandbox::command` refuses a policy with gaps in
//! strict mode; `sbx check` and `sbx doctor` print the same report.
//!
//! The platform answers come from `linux`, `windows` and, for macOS, the
//! Seatbelt checks here.

use super::*;

/// Whether one part of the policy is in force.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "kebab-case")]
pub enum Status {
    /// The policy asks for nothing here.
    NotRequested,
    /// In force, and how.
    Enforced(String),
    /// Asked for but not in force on this machine, and why.
    NotEnforced(String),
}

impl Status {
    #[must_use]
    pub fn is_gap(&self) -> bool {
        matches!(self, Self::NotEnforced(_))
    }

    fn line(&self, what: &str) -> String {
        match self {
            Self::NotRequested => format!("{what}: not restricted"),
            Self::Enforced(how) => format!("{what}: enforced - {how}"),
            Self::NotEnforced(why) => format!("{what}: NOT ENFORCED - {why}"),
        }
    }
}

/// What this machine will enforce of a policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Enforcement {
    pub filesystem: Status,
    pub network: Status,
    pub limits: Status,
    /// Always enforced: the environment is computed by this process.
    pub environment: Status,
}

impl Enforcement {
    /// The parts asked for but not in force.
    #[must_use]
    pub fn gaps(&self) -> Vec<String> {
        [
            ("filesystem", &self.filesystem),
            ("network", &self.network),
            ("limits", &self.limits),
        ]
        .into_iter()
        .filter(|(_, s)| s.is_gap())
        .map(|(what, s)| s.line(what))
        .collect()
    }

    /// One line per part, for a person.
    #[must_use]
    pub fn report(&self) -> String {
        [
            self.filesystem.line("filesystem"),
            self.network.line("network"),
            self.limits.line("limits"),
            self.environment.line("environment"),
        ]
        .join("\n")
    }
}

/// The enforcement of `policy` on this machine.
#[must_use]
pub fn enforcement(policy: &Policy) -> Enforcement {
    Enforcement {
        filesystem: fs_status(&policy.fs),
        network: net_status(policy.network.mode),
        limits: limits::status(&policy.limits),
        environment: Status::Enforced(environment_summary(&policy.env)),
    }
}

fn environment_summary(e: &policy::Env) -> String {
    let mut parts = vec![match e.inherit {
        Inherit::All => "inherited".to_string(),
        Inherit::None => "only essentials and `pass`".to_string(),
    }];
    if e.strip_agent_session {
        parts.push("agent session stripped".into());
    }
    if e.strip_git_redirects {
        parts.push("git redirects stripped".into());
    }
    if !e.remove.is_empty() {
        parts.push(format!("{} pattern(s) removed", e.remove.len()));
    }
    parts.push("private TMPDIR".into());
    parts.join(", ")
}

#[cfg(target_os = "linux")]
fn fs_status(fs: &policy::Fs) -> Status {
    match linux::fs_status(fs.mode) {
        Status::Enforced(how) if wsl::detected() => Status::Enforced(format!(
            "{how}; WSL: starting Windows programs is blocked (interop interpreter and \
             sockets hidden, WSL_INTEROP removed, vsock refused)"
        )),
        other => other,
    }
}

#[cfg(target_os = "macos")]
fn fs_status(fs: &policy::Fs) -> Status {
    match fs.mode {
        FsMode::Off => Status::NotRequested,
        _ if Path::new(seatbelt::SANDBOX_EXEC).exists() => {
            Status::Enforced("Seatbelt (sandbox-exec)".into())
        }
        _ => Status::NotEnforced(format!("{} was not found", seatbelt::SANDBOX_EXEC)),
    }
}

#[cfg(windows)]
fn fs_status(fs: &policy::Fs) -> Status {
    windows::fs_status(fs)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn fs_status(fs: &policy::Fs) -> Status {
    match fs.mode {
        FsMode::Off => Status::NotRequested,
        _ => {
            Status::NotEnforced("no filesystem sandbox is implemented for this platform yet".into())
        }
    }
}

#[cfg(target_os = "linux")]
fn net_status(mode: NetMode) -> Status {
    linux::net_status(mode)
}

#[cfg(target_os = "macos")]
fn net_status(mode: NetMode) -> Status {
    match mode {
        NetMode::Allow => Status::NotRequested,
        _ if Path::new(seatbelt::SANDBOX_EXEC).exists() => Status::Enforced(match mode {
            NetMode::Proxy => "Seatbelt: outbound only to the proxy (and Unix sockets)".into(),
            _ => "Seatbelt: network denied (Unix sockets allowed)".into(),
        }),
        _ => Status::NotEnforced(format!("{} was not found", seatbelt::SANDBOX_EXEC)),
    }
}

#[cfg(windows)]
fn net_status(mode: NetMode) -> Status {
    if mode == NetMode::Allow {
        // Without asking whether the offline account is set up.
        return Status::NotRequested;
    }
    windows::net_status(mode, windows::setup::ready())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn net_status(mode: NetMode) -> Status {
    match mode {
        NetMode::Allow => Status::NotRequested,
        _ => Status::NotEnforced("no network sandbox is implemented for this platform yet".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A policy that asks for nothing has no gaps on any machine: nothing
    /// is requested, and the environment is always this process's to make.
    #[test]
    fn an_empty_policy_asks_for_nothing_and_has_no_gaps() {
        let e = enforcement(&Policy::default());
        assert_eq!(e.filesystem, Status::NotRequested);
        assert_eq!(e.network, Status::NotRequested);
        assert_eq!(e.limits, Status::NotRequested);
        assert!(
            matches!(e.environment, Status::Enforced(ref how) if how.contains("private TMPDIR"))
        );
        assert!(e.gaps().is_empty());
    }

    /// Only a part asked for and not in force is a gap; the report has one
    /// line per part, in order, saying which.
    #[test]
    fn only_what_is_not_enforced_is_a_gap() {
        let e = Enforcement {
            filesystem: Status::Enforced("Landlock".into()),
            network: Status::NotEnforced("kernel too old".into()),
            limits: Status::NotRequested,
            environment: Status::Enforced("inherited".into()),
        };
        assert_eq!(e.gaps(), ["network: NOT ENFORCED - kernel too old"]);
        assert_eq!(
            e.report().lines().collect::<Vec<_>>(),
            [
                "filesystem: enforced - Landlock",
                "network: NOT ENFORCED - kernel too old",
                "limits: not restricted",
                "environment: enforced - inherited",
            ]
        );
    }

    /// The environment summary names what the policy strips and removes.
    #[test]
    fn the_environment_summary_says_what_was_stripped() {
        let env = policy::Env {
            inherit: Inherit::None,
            remove: vec!["AWS_*".into(), "GH_*".into()],
            ..policy::Env::default()
        };
        let summary = environment_summary(&env);
        assert!(
            summary.starts_with("only essentials and `pass`"),
            "{summary}"
        );
        assert!(summary.contains("2 pattern(s) removed"), "{summary}");
        assert!(summary.ends_with("private TMPDIR"), "{summary}");
    }
}
