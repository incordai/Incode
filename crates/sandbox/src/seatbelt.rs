//! macOS: `sandbox-exec` with a generated Seatbelt profile.
//!
//! The command becomes `sandbox-exec -D TMP=… -D WS0=… -p <profile> <helper>
//! …`; the profile is in force for the helper and everything it execs. Paths
//! go in as `-D` parameters, never interpolated, so a quote or backslash in
//! a path cannot break the profile; they are canonicalised first because
//! Seatbelt matches resolved paths.
//!
//! The profile is generated and unit-tested on every platform. It follows the
//! shape incode's sandbox ran on macOS 26 (writes confined, children
//! confined, `git`/`cc` working); the network clauses for `proxy` follow the
//! forms shipping profiles use and have NOT yet been run on a Mac.

use crate::policy::{FsMode, NetMode};

/// Where the tool is. A fixed path, so a workspace cannot shadow it on PATH.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The profile for `fs` with `roots` writable roots, and `net` (with the
/// proxy's port under `proxy`). The last matching rule wins, so each deny is
/// stated before its allows.
#[must_use]
pub fn profile(
    fs: FsMode,
    roots: usize,
    deny_read: usize,
    net: NetMode,
    proxy_port: Option<u16>,
) -> String {
    let mut p = String::from("(version 1) (allow default)");
    if fs != FsMode::Off {
        let mut allow = String::from("(subpath (param \"TMP\"))");
        for i in 0..roots {
            allow.push_str(&format!(" (subpath (param \"WS{i}\"))"));
        }
        p.push_str(&format!(
            " (deny file-write* (subpath \"/\")) \
             (allow file-write* {allow} \
             (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\") \
             (literal \"/dev/dtracehelper\") (regex #\"^/dev/fd/[0-9]+$\") \
             (regex #\"^/dev/ttys[0-9]+$\"))"
        ));
        for i in 0..deny_read {
            p.push_str(&format!(" (deny file-read* (subpath (param \"DR{i}\")))"));
        }
    }
    match net {
        NetMode::Allow => {}
        NetMode::Deny | NetMode::Proxy => {
            // Unix-domain sockets stay allowed: they are local IPC (an
            // ssh-agent, a build daemon), not the network.
            p.push_str(
                " (deny network*) \
                 (allow network-bind (local unix-socket (path-regex #\"^/\"))) \
                 (allow network-outbound (remote unix-socket (path-regex #\"^/\")))",
            );
            if let (NetMode::Proxy, Some(port)) = (net, proxy_port) {
                p.push_str(&format!(
                    " (allow network-outbound (remote ip \"localhost:{port}\"))"
                ));
            }
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_write_denies_then_allows_the_roots() {
        let p = profile(FsMode::WorkspaceWrite, 2, 0, NetMode::Allow, None);
        let deny = p.find("(deny file-write*").expect("deny");
        let allow = p.find("(allow file-write*").expect("allow");
        assert!(deny < allow, "the deny comes first: {p}");
        assert!(p.contains("(param \"WS0\")") && p.contains("(param \"WS1\")"));
        assert!(!p.contains("network"));
    }

    #[test]
    fn read_only_has_only_the_temp_dir_and_sinks() {
        let p = profile(FsMode::ReadOnly, 0, 0, NetMode::Allow, None);
        assert!(!p.contains("WS0"));
        assert!(p.contains("(param \"TMP\")"));
    }

    #[test]
    fn proxy_allows_exactly_the_proxy_port() {
        let p = profile(FsMode::Off, 0, 0, NetMode::Proxy, Some(41234));
        assert!(p.contains("(deny network*)"));
        assert!(p.contains("(remote ip \"localhost:41234\")"));
        assert!(!p.contains("file-write"));
        let deny = profile(FsMode::Off, 0, 0, NetMode::Deny, None);
        assert!(!deny.contains("remote ip"));
    }

    #[test]
    fn deny_read_comes_after_the_allow_default() {
        let p = profile(FsMode::ReadOnly, 0, 2, NetMode::Allow, None);
        let allow = p.find("(allow default)").expect("allow");
        let deny = p
            .find("(deny file-read* (subpath (param \"DR0\")))")
            .expect("DR0");
        assert!(allow < deny, "{p}");
        assert!(p.contains("(param \"DR1\")"));
    }

    #[test]
    fn parentheses_balance() {
        for net in [NetMode::Allow, NetMode::Deny, NetMode::Proxy] {
            let p = profile(FsMode::WorkspaceWrite, 3, 2, net, Some(1));
            let open = p.matches('(').count();
            let close = p.matches(')').count();
            assert_eq!(open, close, "{p}");
        }
    }
}
