# sbx: one sandbox for any agent

`sbx` runs a command under one policy: what it may write, what it may read,
which hosts it may reach, which environment it starts with, and how much it
may use. It works with any agent CLI (`claude`, `codex`, `incode`, `aider`,
…), with a test suite a harness runs, or with a single tool call. The policy
means the same thing on every platform. Where a platform cannot enforce part
of it, `sbx` says so before anything runs. By default it then refuses
instead of running with less.

```sh
# Write only in this project; network only to the package registry; 30 minutes.
sbx run -w . --allow-host crates.io --allow-host '*.crates.io' --wall 1800 -- cargo build

# An agent CLI confined to its workspace, with a folder it must not see.
sbx run -w . -w ~/.claude -w ~/.claude.json --deny-read ../private-checks -- claude -p "fix the failing test"

# What would this machine enforce?
sbx doctor
sbx check --policy agent.toml --json
```

It's a crate in the incode repository for now and doesn't depend on incode.
It builds to one binary, `sbx`, and a library, `incord_sandbox`.

## The policy file

```toml
[fs]
mode = "workspace-write"        # off | read-only | workspace-write
write = ["."]                   # writable roots
deny_read = ["../private"]      # hidden: not even readable

[network]
mode = "proxy"                  # allow | deny | proxy
allow = ["crates.io", "*.crates.io:443", "registry.npmjs.org"]
allow_private = false           # may an allowed name resolve to a private address?

[env]
inherit = "all"                 # all | none (essentials kept)
pass = ["ANTHROPIC_API_KEY"]    # kept when inherit = "none"
remove = ["AWS_*"]              # a trailing * matches a prefix
strip_agent_session = true      # CLAUDECODE, CLAUDE_CODE_SESSION*, ... (default)
strip_git_redirects = true      # GIT_DIR, GIT_WORK_TREE, ... (default)
set = { CI = "1" }

[limits]
wall_secs = 1800                # the whole process group is killed
cpu_secs = 1200                 # per process
memory_mb = 4096                # address space per process
file_size_mb = 1024
open_files = 4096
```

Every flag of `sbx run` has a policy key. Flags apply on top of `--policy`.
An unknown key is an error, so a misspelt `wirte` can't silently do nothing.

## What is enforced, and how

| | Linux | macOS | Windows |
|---|---|---|---|
| filesystem: write | Landlock (5.13+) | Seatbelt | **not yet** |
| filesystem: `deny_read` | Landlock | Seatbelt | **not yet** |
| network: `deny` | Landlock TCP (6.7+) | Seatbelt | **not yet** |
| network: `proxy` | Landlock: TCP only to the proxy's port | Seatbelt: outbound only to the proxy | **not yet** |
| limits | `setrlimit` + wall clock | `setrlimit` + wall clock | wall clock |
| environment | yes | yes | yes |

- **Every process it starts.** The confinement is applied in a helper
  process just before it execs the command. Children inherit it, and it
  can't be widened again (`no_new_privs` is set).
- **A private temp directory.** Each run gets one (`TMPDIR`, `TMP`,
  `TEMP`), removed afterwards. The shared `/tmp` is not writable.
- **The proxy.** `proxy` mode runs an allowlisting HTTP proxy in the
  unconfined parent and points the command at it (`HTTP(S)_PROXY`).
  - It sees host names, not content: TLS is tunnelled with `CONNECT`.
  - It resolves each name once and connects to the checked address, so DNS
    can't redirect an allowed name to a private address or the cloud
    metadata service. Turn this check off with `allow_private`.
  - When `sbx` itself runs behind an HTTP proxy, allowed requests are chained
    through it, and that proxy does the resolving.
- **`deny_read`.** Landlock only grants, so a hidden path is carved out by
  granting its siblings, level by level from `/`.
  - Directories *above* a hidden path can't be listed (`ls /` is refused);
    files beneath them can still be read.
  - A symlink that leads into or above a hidden path isn't granted.
  - Not covered: a hard link to a hidden file made elsewhere beforehand, or
    the same directory mounted a second time.
- **Process lifetime.**
  - With a wall-clock limit, the command gets its own process group. The
    limit kills all of it, and `SIGINT`/`SIGTERM`/`SIGHUP` sent to `sbx` are
    passed on.
  - If `sbx` is killed with `SIGKILL`, a supervisor kills the group too.
  - Without a limit, the command stays in the caller's process group, so the
    caller's own group kill reaches it.

**Known limits:**
- Landlock does not cover UDP. Under `deny`, DNS still works. Under
  `proxy`, the command doesn't need DNS, but it could still send UDP.
- Landlock matches network ports, not addresses. Under `proxy` the command
  could connect to *any* host on the proxy's (random, high) port.
- On macOS the profile is generated and unit-tested, but this crate's
  network and `deny_read` clauses have not yet been run on a Mac.

## Exit codes

| code | meaning |
|---|---|
| the command's | it ran |
| 124 | the wall-clock limit killed it |
| 125 | `sbx` refused (strict, or a bad policy) or failed |
| 126 / 127 | the command could not be run / found |
| 128 + n | it was killed by signal n |

## Using it from Rust

```rust
use incord_sandbox::{Policy, Sandbox};

fn main() {
    incord_sandbox::enter_if_requested(); // first: the helper is this binary
    let mut policy = Policy::workspace("/path/to/worktree");
    policy.fs.deny_read.push("/path/to/hidden-tests".into());
    policy.limits.wall_secs = Some(1800);
    let outcome = Sandbox::new(policy)
        .run("cargo", &["test".into()], std::path::Path::new("/path/to/worktree"))
        .expect("runs");
    std::process::exit(outcome.code());
}
```

`Sandbox::command` returns the `Command` and a guard instead, for a caller
that sets its own pipes. Keep the guard alive until the command exits: it
holds the proxy and the temp directory.

## Tests

`cargo test -p incord-sandbox` runs:
- the unit tests (policy, environment, allowlist, Seatbelt profile);
- on Linux, escape tests through the real binary: writes outside the root,
  symlinks, hard links, renames, `/proc/self/root`, child processes, reading
  a hidden path, connecting and binding under `deny`, bypassing the proxy,
  the wall limit and `SIGKILL` cleanup, limits and exit codes.

Each escape test skips, and says so, on a kernel that cannot enforce what
it checks.
