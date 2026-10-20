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
| network: `deny` | Landlock TCP (6.7+) + seccomp for every other socket | Seatbelt | **not yet** |
| network: `proxy` | Landlock: TCP only to the proxy's port; seccomp as for `deny` | Seatbelt: outbound only to the proxy | **not yet** |
| `.git/hooks`, `.git/config` | snapshotted, put back after the run | Seatbelt refuses the write | **not yet** |
| signals, abstract Unix sockets | Landlock scopes (6.12+): only within the sandbox | Seatbelt default | **not yet** |
| Windows programs (inside WSL2) | interop interpreter and sockets hidden, `WSL_INTEROP` removed, vsock refused | n/a | n/a |
| limits | `setrlimit` + wall clock; no core dumps | `setrlimit` + wall clock; no core dumps | wall clock |
| environment | yes | yes | yes |

- **Every process it starts.** The confinement is applied in a helper
  process just before it execs the command. Children inherit it, and it
  can't be widened again (`no_new_privs` is set).
- **A private temp directory.** Each run gets one (`TMPDIR`, `TMP`,
  `TEMP`), removed afterwards. The shared `/tmp` is not writable.
- **Sockets Landlock does not see.** Landlock covers TCP only. Under `deny`
  and `proxy` a seccomp filter makes `socket()` fail (`EACCES`) for anything
  but Unix sockets, netlink and plain TCP: UDP, ICMP, raw and packet
  sockets, SCTP and MPTCP (Landlock's TCP rules do not apply to them: an
  MPTCP connect got through `deny` on Linux 6.18), and other families such
  as `AF_VSOCK`.
  - So under `deny`, DNS is blocked too: a lookup fails. Under `proxy` the
    command never needed DNS; the proxy resolves names.
  - Netlink stays: `getaddrinfo` and `ip` read the machine's addresses and
    routes through it. A command can see the host's interfaces; changing
    them needs a capability it does not have, and nothing sent on netlink
    leaves the machine.
  - `io_uring`, which can open sockets past a seccomp filter, is refused
    (`EPERM`; runtimes fall back). On x86-64 the 32-bit syscall entry points
    are filtered as well.
  - The filter knows x86-64 and aarch64. On any other architecture none is
    installed, the network is reported **not enforced**, and strict mode
    refuses.
- **Terminal and VM-socket hardening.** The seccomp filter is also installed
  whenever the filesystem is confined, not only under `deny`/`proxy`. It
  always refuses:
  - `ioctl(TIOCSTI)` and `ioctl(TIOCLINUX)` (EPERM), which would let a
    command type keystrokes into the terminal it shares with your shell, to
    run unconfined after it exits. Window size, termios and the other
    terminal ioctls still work.
  - `AF_VSOCK` sockets (EACCES): vsock reaches a VM's host directly, and on
    WSL2 that means Windows interop.
  - io_uring.

  A filesystem-only policy on an architecture without a filter runs without
  this extra hardening.
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
- **The repository's hooks and config** (`fs.protect_git`, on by default).
  - **Why:** a hook, or a `core.fsmonitor`/`core.hooksPath` line, is code
    git runs the next time anyone uses the repository, outside any sandbox.
  - **How (Linux):** Landlock can't make part of a writable root read-only
    without also blocking file creation in the root, so `sbx` snapshots each
    root's `.git/hooks` and `.git/config` before the run. Anything changed is
    put back afterwards and reported (`protected path changed by the
    command, restored …`, and `protected_restored` from `sbx mcp`).
  - **How (macOS):** Seatbelt also refuses the writes.
  - **Worktrees:** a `.git` file (`gitdir:`) and its `commondir` are
    followed.
  - **Opting out:** `--no-protect-git` for a run that should change hooks.
- **Signals and abstract sockets.** On Landlock ABI v6 (Linux 6.12+) a
  sandboxed command can't signal processes outside its sandbox, or connect to
  an abstract Unix socket made outside it (a D-Bus session bus, X11). `sbx
  doctor` says whether the kernel does this.
- **Code the loader would inject.** `LD_PRELOAD`, `LD_AUDIT`,
  `DYLD_INSERT_LIBRARIES` and `DYLD_FORCE_FLAT_NAMESPACE` are removed by
  default (`env.strip_loader_injection`): they would load a library into
  every program, the sandbox's own helper included. Core dumps are off
  (`RLIMIT_CORE=0`), so a crash can't write the process's memory to disk.
- **Process lifetime.**
  - With a wall-clock limit, the command gets its own process group. The
    limit kills all of it, and `SIGINT`/`SIGTERM`/`SIGHUP` sent to `sbx` are
    passed on.
  - If `sbx` is killed with `SIGKILL`, a supervisor kills the group too.
  - Without a limit, the command stays in the caller's process group, so the
    caller's own group kill reaches it.

**Known limits:**
- A name lookup a local daemon answers over a Unix socket still works
  under `deny`: with `nss-resolve` (systemd-resolved) or `nscd` in
  `/etc/nsswitch.conf`, the name reaches the resolver and the answer comes
  back. Plain `files dns` setups fail the lookup.
- Landlock matches network ports, not addresses. Under `proxy` the command
  could connect to *any* host on the proxy's (random, high) port.
- On macOS the profile is generated and unit-tested, but this crate's
  network, `deny_read` and protected-path clauses have not yet been run on a
  Mac. The profile still starts from `(allow default)`; a deny-by-default
  profile (only the system services a build needs) is the next step there.
- The hook protection on Linux restores *after* the run. A hook planted
  during the run would execute if someone ran git in that workspace before
  the run ended.

## Windows: run it inside WSL2

On Windows the sandbox is the Linux one, running inside WSL2: Landlock,
seccomp, the git protection and `sbx mcp` all apply unchanged.

**Setup, once** (PowerShell, from the incode folder):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1
```

It:
1. installs WSL2 and Ubuntu if they're missing (asks first, one
   administrator prompt, usually a restart, then run it again);
2. builds the Linux `sbx` inside WSL2 at `~/.local/bin/sbx`;
3. builds `sbx.exe` and adds it to your PATH;
4. runs `sbx doctor`.

**`sbx.exe` forwards into WSL2.**
- Every command it gets (`run`, `check`, `doctor`, `mcp`) is passed to the
  Linux `sbx` with `wsl.exe`, from the same folder.
- Paths are translated: `C:\work\app` becomes `/mnt/c/work/app`, and
  `\\wsl$\Ubuntu\home\me` becomes `/home/me`.
- Output, input and the exit code pass straight through, so `sbx mcp` works
  for agents on Windows too.
- **Choosing a distribution:** `SBX_WSL_DISTRO` picks one other than the
  default.
- **Without WSL:** `sbx.exe` refuses with the setup steps instead of running
  unconfined. `SBX_NATIVE=1` runs natively anyway, with only the
  environment and time limit, and files and network not confined.
- **Windows environment variables don't reach WSL** unless listed in
  `WSLENV` (for example `WSLENV=ANTHROPIC_API_KEY`).

WSL2 adds one way out that plain Linux doesn't have: starting a Windows
program (`cmd.exe`, `powershell.exe`, any `.exe` under `/mnt/c`), which
runs on Windows, outside the sandbox. When the filesystem is confined and
WSL is detected, `sbx` closes it (`fs.block_wsl_interop`, on by default):
- WSL's interop interpreter (`/init`, read from
  `/proc/sys/fs/binfmt_misc/WSLInterop`) is hidden like a `deny_read` path,
  so no `.exe` can start;
- `/run/WSL`, where the interop sockets live, is hidden and `WSL_INTEROP`
  is removed;
- vsock is refused by seccomp.

Hiding `/init` means `ls /` is refused inside the sandbox; files beneath
`/` still read normally.

**Network inside WSL2:** `network: deny` needs Landlock's TCP rules (Linux
6.7+). On an older WSL kernel only UDP/DNS are blocked, `sbx doctor` says
the network is not enforced, and strict mode refuses `deny`/`proxy` rather
than pretend. Update WSL (`wsl --update`) for a newer kernel.

**Tested:** the mechanism (a hidden interpreter can't run, directly or as
a script's) runs on every Linux CI machine. `tests/wsl.rs` tries the real
ways out (`cmd.exe`, `/mnt/c/.../cmd.exe`, `powershell.exe`,
`WSL_INTEROP`, `/run/WSL`), but only on a WSL2 machine. It has not run on
one yet, so run `cargo test -p incord-sandbox --test wsl` there before
relying on this.

## Exit codes

| code | meaning |
|---|---|
| the command's | it ran |
| 124 | the wall-clock limit killed it |
| 125 | `sbx` refused (strict, or a bad policy) or failed |
| 126 / 127 | the command could not be run / found |
| 128 + n | it was killed by signal n |

## Sandboxes as a service: `sbx mcp`

`sbx mcp` is an MCP server (stdio) that lets an agent work in sandboxes:
create one, write scripts and files into it, run them, read what they
produced, and delete it. Any agent that speaks MCP can use it: Claude Code,
Codex, incode, or your own.

```json
{ "mcpServers": { "sbx": { "command": "sbx", "args": ["mcp", "--net", "proxy", "--allow-host", "pypi.org", "--allow-host", "*.pythonhosted.org"] } } }
```

| Tool | What it does |
|---|---|
| `sandbox_create` | A private folder. Network `deny` unless the agent asks for `proxy` with hosts (or `allow`) within the server's limits. |
| `sandbox_write_file` | Write a script or file, creating folders; `executable` sets 755. |
| `sandbox_run` | Run `command` (argv) or `script` (text, run with `interpreter`, default `sh`) with optional `stdin`, `cwd` and `timeout_secs`. Returns `exit_code`, `stdout`, `stderr` (64 KB each), `timed_out` and `network_refused`. |
| `sandbox_read_file` / `sandbox_list` | Read results back (256 KB at most). |
| `sandbox_info` | The folder, network and limits, and what this machine enforces. |
| `sandbox_destroy` | Delete it. |

The operator sets the ceilings when starting the server (`--net`,
`--allow-host`, `--max-wall`, `--policy` for environment and limits). An
agent can ask for less, never more, and a request above a ceiling is refused
rather than quietly lowered.

By default every sandbox gets:
- no network;
- an empty environment apart from `PATH`, `HOME` and the other
  essentials, so no host secrets;
- 4 GB of memory and 1 GB files per process;
- 120 seconds a run (at most 600).

Every run is confined to its sandbox's folder. When a run ends, everything
it started is killed, so nothing keeps running in the background.

The server itself is not confined, so the file tools only accept relative
paths with no `..`, and refuse any path through a symlink (each part is
checked). This stops a script from planting a link that would lead the
server out of the sandbox. Sandboxes are removed when the server stops,
unless `--keep` is given.

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
- `tests/mcp.rs`: `sbx mcp` driven over stdio like an agent drives it. It writes a script, runs it and reads its result; tries escapes (writing outside, the network, host secrets, a planted symlink, `..`, absolute paths); checks the time limit and background cleanup, the limits on what an agent may ask for, and that sandboxes are removed on exit;
- the unit tests (policy, environment, allowlist, Seatbelt profile);
- on Linux, escape tests through the real binary: writes outside the root,
  symlinks, hard links, renames, `/proc/self/root`, child processes, reading
  a hidden path, connecting and binding under `deny`, UDP and raw sockets
  and DNS under `deny` and `proxy`, bypassing the proxy, the wall limit and
  `SIGKILL` cleanup, limits and exit codes;
- on macOS, the same escapes against Seatbelt where they apply (writes,
  symlinks, children, read-only, `deny_read`, TCP under `deny`, the proxy,
  the environment, the wall limit).

Each escape test skips, and says so, on a kernel that cannot enforce what
it checks.
