//! On Windows, where a command runs: natively, or forwarded into WSL2.
//!
//! Native Windows confines writes (a write-restricted token), the
//! environment and the limits a job object can carry, but not the network
//! and not `deny_read` ([`crate::windows`]). WSL2 runs the Linux sandbox,
//! which enforces all of it, at the price of a Linux userland: the command
//! must exist inside WSL, and paths are translated. So `sbx.exe` picks per
//! invocation, from `SBX_BACKEND`:
//!
//! - `native`: always natively. What native cannot enforce is refused
//!   (strict) or reported (`--best-effort`), as on every platform.
//! - `wsl`: always forwarded to the Linux `sbx` inside WSL2.
//! - `auto` (the default): natively when native enforces everything the
//!   policy asks for; otherwise forwarded to WSL2 when it is set up;
//!   otherwise refused, with both ways forward named, unless `--best-effort`
//!   accepts running natively with less.
//!
//! `SBX_NATIVE=1`, the older switch, still means `native`.
//!
//! The choice is a pure function of the policy's gaps and whether WSL is
//! ready, so it is tested on every platform.

/// Which backend was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Native,
    Wsl,
    Auto,
}

/// What to do with this invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    Native,
    Wsl,
    /// Neither can run it as asked; the message says why and what to do.
    Refuse(String),
}

/// How to set up WSL forwarding, for messages.
pub const WSL_SETUP: &str = "set up WSL2 forwarding once (PowerShell, from the incode folder): \
     powershell -ExecutionPolicy Bypass -File scripts\\install-windows.ps1";

impl Backend {
    /// The backend from the environment: `SBX_BACKEND`, else `SBX_NATIVE`
    /// (any non-empty value but `0` means native), else `auto`.
    ///
    /// # Errors
    /// When `SBX_BACKEND` names no backend: a typo must not silently pick one.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        if let Some(v) = var("SBX_BACKEND").filter(|v| !v.trim().is_empty()) {
            return match v.trim().to_ascii_lowercase().as_str() {
                "native" => Ok(Self::Native),
                "wsl" => Ok(Self::Wsl),
                "auto" => Ok(Self::Auto),
                other => Err(format!(
                    "SBX_BACKEND=`{other}` is not a backend (native | wsl | auto)"
                )),
            };
        }
        match var("SBX_NATIVE") {
            Some(v) if !v.is_empty() && v != "0" => Ok(Self::Native),
            _ => Ok(Self::Auto),
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Wsl => "wsl",
            Self::Auto => "auto",
        }
    }
}

/// Where to run, given the backend asked for, what native Windows would NOT
/// enforce of the policy (`native_gaps`, as [`crate::Enforcement::gaps`]
/// words them), whether gaps were accepted (`--best-effort`), and whether
/// WSL forwarding is ready (asked only when it matters, since asking starts
/// WSL: `Ok` or the reason it is not).
pub fn choose(
    backend: Backend,
    native_gaps: &[String],
    best_effort: bool,
    wsl_ready: impl FnOnce() -> Result<(), String>,
) -> Choice {
    match backend {
        Backend::Native => Choice::Native,
        Backend::Wsl => Choice::Wsl,
        Backend::Auto if native_gaps.is_empty() => Choice::Native,
        Backend::Auto => match wsl_ready() {
            Ok(()) => Choice::Wsl,
            Err(_) if best_effort => Choice::Native,
            Err(why) => Choice::Refuse(format!(
                "this policy asks for what native Windows cannot enforce:\n  {}\n\
                 and WSL2 forwarding, which would, is not available ({why}).\n\
                 Either {WSL_SETUP} (sbx then forwards such policies there by itself), \
                 or drop those parts of the policy (or accept them unenforced with \
                 --best-effort).",
                native_gaps.join("\n  ")
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn the_backend_comes_from_sbx_backend_then_sbx_native() {
        assert_eq!(Backend::from_env(env(&[])), Ok(Backend::Auto));
        assert_eq!(
            Backend::from_env(env(&[("SBX_BACKEND", "WSL")])),
            Ok(Backend::Wsl)
        );
        assert_eq!(
            Backend::from_env(env(&[("SBX_BACKEND", "native")])),
            Ok(Backend::Native)
        );
        assert_eq!(
            Backend::from_env(env(&[("SBX_NATIVE", "1")])),
            Ok(Backend::Native)
        );
        assert_eq!(
            Backend::from_env(env(&[("SBX_NATIVE", "0")])),
            Ok(Backend::Auto)
        );
        // SBX_BACKEND wins over the older switch.
        assert_eq!(
            Backend::from_env(env(&[("SBX_BACKEND", "auto"), ("SBX_NATIVE", "1")])),
            Ok(Backend::Auto)
        );
        assert!(Backend::from_env(env(&[("SBX_BACKEND", "wls")])).is_err());
    }

    #[test]
    fn auto_runs_natively_when_native_enforces_everything_and_never_asks_wsl() {
        let choice = choose(Backend::Auto, &[], false, || {
            panic!("WSL must not be started when native suffices")
        });
        assert_eq!(choice, Choice::Native);
    }

    #[test]
    fn auto_forwards_what_native_cannot_enforce_to_wsl_when_ready() {
        let gaps = vec!["network: NOT ENFORCED - no token restricts the network".to_string()];
        assert_eq!(choose(Backend::Auto, &gaps, false, || Ok(())), Choice::Wsl);
        assert_eq!(choose(Backend::Auto, &gaps, true, || Ok(())), Choice::Wsl);
    }

    #[test]
    fn auto_refuses_naming_both_fixes_when_wsl_is_not_there() {
        let gaps = vec!["filesystem: NOT ENFORCED - deny_read".to_string()];
        let Choice::Refuse(why) = choose(Backend::Auto, &gaps, false, || {
            Err("wsl.exe was not found".into())
        }) else {
            panic!("must refuse");
        };
        assert!(why.contains("deny_read"), "{why}");
        assert!(why.contains("wsl.exe was not found"), "{why}");
        assert!(why.contains("install-windows.ps1"), "{why}");
        assert!(why.contains("--best-effort"), "{why}");
        // Best effort accepts the gaps and runs natively.
        assert_eq!(
            choose(Backend::Auto, &gaps, true, || Err("no".into())),
            Choice::Native
        );
    }

    #[test]
    fn an_explicit_backend_is_obeyed_without_asking_wsl() {
        let gaps = vec!["network: NOT ENFORCED".to_string()];
        let never = || -> Result<(), String> { panic!("not asked") };
        assert_eq!(choose(Backend::Native, &gaps, false, never), Choice::Native);
        assert_eq!(choose(Backend::Wsl, &[], false, never), Choice::Wsl);
    }
}
