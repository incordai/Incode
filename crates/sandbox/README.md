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

| | Linux | macOS | Windows (native) |
|---|---|---|---|
| filesystem: write | Landlock (5.13+) | Seatbelt | write-restricted token + a per-run ACE on each root |
| filesystem: `deny_read` | Landlock | Seatbelt | **not enforced** (WSL2 does it) |
| network: `deny` | Landlock TCP (6.7+) + seccomp for every other socket | Seatbelt | after `sbx setup-windows`: runs as an offline account WFP filters cut off; else **not enforced** (WSL2 does it) |
| network: `proxy` | Landlock: TCP only to the proxy's port; seccomp as for `deny` | Seatbelt: outbound only to the proxy | after `sbx setup-windows`: the offline account may reach only the proxy's loopback ports; else **not enforced** (WSL2 does it) |
| `.git/hooks`, `.git/config` | snapshotted, put back after the run | Seatbelt refuses the write | snapshotted, put back after the run |
| signals, abstract Unix sockets | Landlock scopes (6.12+): only within the sandbox | Seatbelt default | the token cannot open other processes for writing (measured by incode) |
| Windows programs (inside WSL2) | interop interpreter and sockets hidden, `WSL_INTEROP` removed, vsock refused | n/a | n/a |
| limits | `setrlimit` + wall clock; no core dumps | `setrlimit` + wall clock; no core dumps | job object: wall clock, CPU and committed memory per process; **no** `file_size_mb`, `open_files` |
| environment | yes | yes | yes |

On Windows, `sbx.exe` runs natively when native enforces the whole policy,
and forwards it to the Linux `sbx` inside WSL2 when it doesn't (see
[Windows](#windows-native-or-wsl2)).

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

## Windows: native, or WSL2

`sbx.exe` has two backends, picked per run by `SBX_BACKEND`:

| `SBX_BACKEND` | what runs |
|---|---|
| `auto` (default) | natively when native enforces everything the policy asks for; otherwise in WSL2 when it is set up; otherwise refused, naming both ways forward |
| `native` | always natively; what it can't enforce is refused (strict) or reported (`--best-effort`) |
| `wsl` | always the Linux `sbx` inside WSL2 |

`SBX_NATIVE=1`, the older switch, still means `native`. `sbx doctor` shows
what native enforces and whether WSL2 forwarding is available.

So a policy that confines files, environment and time runs natively. One
that asks for `network: deny`/`proxy` runs natively too once
[`sbx setup-windows`](#network-natively-sbx-setup-windows) has been run on
the machine, and goes to WSL2 otherwise. `deny_read` always goes to WSL2.
`sbx mcp` follows the same rule from its ceilings: with `--net allow` it
runs natively, with `--net deny` (its default) or `proxy` natively after
the setup and in WSL2 without it.

### Native

Ported from incode's own Windows sandbox, whose authors measured it on
Windows. This crate's port is checked by its tests
(`tests/escape_windows.rs`) on the `windows-latest` CI runner.

- **Writes.** The command runs under a *write-restricted token*: it reads
  anything its user can, but writes only where the user may **and** one of
  the token's restricting SIDs is granted. Those are a private SID made for
  this run, `Everyone` (so `> NUL` works) and the logon SID (so the command
  can start). The run's SID is granted `Modify`, as one inheritable ACE, on
  each writable root and on the run's private temp directory, and nowhere
  else. `read-only` grants only the temp directory.
- **The grant is per run.** Before the command starts, the ACE is added to
  each root; after it exits, it is removed. NTFS writes an inherited ACE into
  every object beneath a root, so this takes time in proportion to the tree
  (incode measured about 2.5 s each way on 18,673 files). No file's contents
  or modification time change, and incode found the tree's ACLs
  byte-identical afterwards. A grant is written down first
  (`%LOCALAPPDATA%\sbx\grants`). If `sbx` crashes, the next run removes
  the ACE it left. A drive root and any folder holding your user profile
  are refused as roots.
- **Everything it starts.** The command starts suspended, goes into a job
  object, and only then runs. Everything it starts is in the job too and
  cannot leave it. The job is killed when the command exits, at the
  wall-clock limit, or when `sbx` itself dies. So on Windows nothing the
  command started outlives the run, even without a time limit.
- **Limits.** `memory_mb` caps the memory each process *commits* (an
  allocation beyond it fails), which is not quite `RLIMIT_AS`. `cpu_secs`
  caps each process's user-mode CPU time. `file_size_mb` and `open_files`
  have no Windows equivalent: asking for them is reported not enforced.
  (`sbx mcp` sets no file size limit on Windows for this reason.)
- **Programs.** The program is found on `PATH` with `PATHEXT`, never in the
  current folder, so a `cmd.exe` planted in the workspace is not picked up.
  A `.bat`/`.cmd` runs through `cmd.exe /d`, so `cmd` parses its arguments.
- **Exit codes** are passed through whole (32 bits).

**Not enforced natively, and why:**
- **Network, until `sbx setup-windows` has run.** No token restricts the
  network (incode measured TCP, HTTP and HTTPS all working under it). Only
  an AppContainer does, and an AppContainer reads only what is granted to
  it: the workspace, your toolchains, your profile. So without the setup
  below, `deny` and `proxy` go to WSL2 (`auto`) or are refused (`native`).
- **`deny_read`.** A write-restricted token checks its restricting SIDs only
  for writes, so no ACE can hide a path from the sandbox alone. Denying your
  own SID would hide it from you everywhere. A fully restricted token would
  need everything a toolchain reads granted to it. So `deny_read` goes to
  WSL2 (`auto`) or is refused (`native`).
- **Places `Everyone` may write** stay writable to the command, because
  `Everyone` is one of the token's restricting SIDs.
- **Git Bash and every msys program** fail to start under the token: the
  msys runtime maps a shared section the token may not map writable, and
  dies with `CreateFileMapping ..., Win32 error 5` (measured by incode).
  Use `cmd` or PowerShell natively, or `SBX_BACKEND=wsl` for a POSIX shell.
  `sbx mcp` scripts default to PowerShell on Windows.
- **Grants are visible.** While a run lasts, every object under a root
  carries the run's ACE, and the grant and revoke cost time on large trees.

### Network natively: `sbx setup-windows`

The Windows Filtering Platform can block a connection by the *user* who
makes it. So, the way OpenAI Codex's Windows sandbox does it
(`codex-rs/windows-sandbox-rs`), a one-time setup makes a local account
that the firewall cuts off, and `deny`/`proxy` runs start the command as
that account. From an elevated prompt (Run as administrator), once:

```powershell
sbx setup-windows                         # or: ... --read C:\Users\me\.cargo --read C:\Users\me\.rustup
sbx doctor                                # [ok  ] offline user ...
sbx run -w . --net deny -- cargo test     # natively now, not in WSL2
sbx setup-windows --remove                # undoes all of it
```

What it does (each step is idempotent; running it again resets the
password and re-adds each filter):

1. **An account, `sbx-offline`**, member of `Users` only (it needs `Users`
   to log on locally and to read `C:\Windows` and `Program Files`; it is
   taken out of `Administrators` if anyone put it there), with a random
   password (256 bits) that is never shown, hidden from the sign-in screen.
2. **Its password**, DPAPI-protected with machine scope, in
   `%ProgramData%\sbx\offline.conf`. The folder's ACL allows SYSTEM,
   Administrators and the user who ran the setup, and nobody else: not
   `Users`, so not the offline account, and not a sandboxed command. Machine
   scope rather than user scope because the setup may run elevated as a
   different administrator than the user who runs sandboxes; not LSA
   private data because only administrators can read that back, and runs
   are not elevated. The ACL is the protection; DPAPI makes the file
   useless off this machine.
3. **Persistent WFP filters** for that account's SID
   (`FWPM_CONDITION_ALE_USER_ID`): every outbound connection and every first
   UDP/raw packet (`ALE_AUTH_CONNECT`), every inbound connection
   (`ALE_AUTH_RECV_ACCEPT`), IPv4 and IPv6, loopback included, and raw ICMP
   sockets (`ALE_RESOURCE_ASSIGNMENT`), all blocked. One permit outranks
   them: TCP to `127.0.0.1` on ports 47600-47615 (`--proxy-ports` changes
   the range), where a `proxy` run's proxy listens. WFP directly
   (`FwpmFilterAdd0`, in one transaction, under fixed keys), not `netsh`.
4. `--read <DIR>` (repeatable, kept across re-runs) lets the account read a
   folder: a toolchain under your profile (see *Reads* below).

At run time (unelevated), `sbx` starts its helper a second time as the
account with `CreateProcessWithLogonW`, the one call an unprivileged process
can start another user's process with (`CreateProcessAsUserW` with that
user's token needs `SeAssignPrimaryTokenPrivilege`, and
`CreateProcessWithTokenW` `SeImpersonatePrivilege`; Codex starts its command
runner the same way). That helper builds the write-restricted token from its
own token, as above, and starts the command; the job object holds both, so
limits, the wall clock, killing the tree and exit codes work as before. No
profile is loaded for the account (Codex's runner doesn't either): it would
create `C:\Users\sbx-offline`, a home every run would share. The command
gets the run's computed environment.

What changes for the command:
- **Reads.** It reads what the account may read, not what you may: system
  folders, and what each run grants it (the writable roots, the folder it
  starts in, the `sbx` program, its private temp directory), plus folders
  granted with `--read`. A toolchain installed under your profile
  (`.cargo`, `.rustup`, a per-user Python or Node) needs `--read` once.
  Narrower than without network deny, never wider. One more right: the
  account may *list* the folder the current folder is in (names, no
  contents), by one ACE on that folder alone that nothing beneath it
  inherits, for the run. PowerShell needs it: it looks its starting folder
  up in the parent, and without it starts at `C:\` (so `Set-Content x.txt`
  wrote `C:\x.txt`).
- **Writes** need both the account (the token's normal check) and the run's
  private SID (its restricted check), so each root gets an ACE for each.
  Another run's command, also the account but with another run SID, cannot
  write your run's roots. The account's ACE on a folder is shared by runs
  that grant the same folder, and removed only when the last of them ends.
  With `--fs off` the command is not write-restricted but is still the
  account: it can write the folder it starts in and its temp directory, and
  what the account itself may write.
- **`sbx` itself** must be readable by the account; it is granted per run
  when you own the file (a build under your profile), and is already when
  installed under `Program Files`.

**Known limits:**
- **DNS answers still come back.** Name lookups go to the DNS Client
  service, which resolves on the command's behalf as `NetworkService`, as
  `systemd-resolved` does on Linux; a connection to the answer is blocked.
- **SMB and WebDAV paths** (`\\host\share`) are opened by the kernel
  redirector, not by the command, and are not stopped by the filters. The
  account's random password authenticates nowhere else, but a server that
  allows guest access would be reachable.
- **Loopback is blocked under `deny`**, including a test suite talking to a
  server it started itself, and Python's `asyncio` on Windows (its event
  loop wakes itself through a loopback socket pair). Use `proxy` with no
  hosts, or `allow`, for those.
- **Under `proxy`** the command can reach any process listening on the
  proxy port range, including another `proxy` run's proxy (with that run's
  allowlist). The ports are below Windows' dynamic range, so nothing gets
  one by chance.
- **Proxies the account cannot use.** Windows PowerShell 5.1's
  `Invoke-WebRequest` ignores `HTTP_PROXY`, and .NET Framework sends
  requests for `localhost` past any proxy; tools that honour `HTTPS_PROXY`
  (`curl`, `git`, `cargo`, `npm`, `pip`) work.
- Only the user who ran the setup can use it (the record is readable by
  that user alone). Another user's `sbx doctor` says so.
- The thread objects of the account's helper get the account logon's default
  DACL, which is not measured yet; its process object is locked down
  explicitly (`D:P(A;;GA;;;SY)(A;;0x101000;;;OW)`).

**Tested:** `tests/escape_windows.rs` runs on the `windows-latest` CI
runner after `sbx setup-windows` (CI runs it; the runner is an
administrator): TCP to a local server and to the internet refused under
`deny` with the filesystem confined, read-only and off (and allowed without
`deny`); UDP refused; writes confined, temp writable, exit codes and
"not found" passed through both helpers; the account's ACE present during a
run and gone after; two concurrent runs sharing a root (the second ending
first leaves the first its grant, and cannot write the first's other root);
the proxy letting through only its allowlist, a direct connection refused;
`sbx mcp` running a deny sandbox's PowerShell script as the account.
Without the setup these tests print SKIPPED.

### WSL2

Inside WSL2 the sandbox is the Linux one: Landlock, seccomp, the git
protection and `sbx mcp` all apply unchanged.

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

**How `sbx.exe` forwards.**
- The command (`run`, `check`, `mcp`; `doctor` too under `SBX_BACKEND=wsl`)
  is passed to the Linux `sbx` with `wsl.exe`, from the same folder.
- Paths are translated: `C:\work\app` becomes `/mnt/c/work/app`, and
  `\\wsl$\Ubuntu\home\me` becomes `/home/me`.
- Output, input and the exit code pass straight through, so `sbx mcp` works
  for agents on Windows too.
- **Choosing a distribution:** `SBX_WSL_DISTRO` picks one other than the
  default.
- **Windows environment variables don't reach WSL** unless listed in
  `WSLENV` (for example `WSLENV=ANTHROPIC_API_KEY`).
- The command must exist inside WSL (`python3`, not `python.exe`).

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
| `sandbox_create` | A private folder. Network `deny` unless the agent asks for `proxy` with hosts (or `allow`) within the server's limits. Natively on Windows `deny` and `proxy` need `sbx setup-windows` first; without it asking for them is refused at once, saying so. |
| `sandbox_write_file` | Write a script or file, creating folders; `executable` sets 755. |
| `sandbox_run` | Run `command` (argv) or `script` (text, run with `interpreter`, default `sh`; PowerShell natively on Windows) with optional `stdin`, `cwd` and `timeout_secs`. Returns `exit_code`, `stdout`, `stderr` (64 KB each), `timed_out` and `network_refused`. |
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
- 4 GB of memory and 1 GB files per process (no file size limit natively
  on Windows, which cannot enforce one);
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
- the unit tests (policy, environment, allowlist, Seatbelt profile, and the
  Windows backend's pure parts on every platform: backend choice, SID and
  record handling, root checks, program lookup, command-line quoting, job
  limits, what each status reports);
- on Linux, escape tests through the real binary: writes outside the root,
  symlinks, hard links, renames, `/proc/self/root`, child processes, reading
  a hidden path, connecting and binding under `deny`, UDP and raw sockets
  and DNS under `deny` and `proxy`, bypassing the proxy, the wall limit and
  `SIGKILL` cleanup, limits and exit codes;
- on macOS, the same escapes against Seatbelt where they apply (writes,
  symlinks, children, read-only, `deny_read`, TCP under `deny`, the proxy,
  the environment, the wall limit);
- on Windows (`tests/escape_windows.rs`, with only `cmd` and PowerShell):
  writes outside the root, the grant held during the run and gone after,
  read-only and its private temp, child `cmd` and PowerShell, a planted
  program in the current folder, the wall limit and killing `sbx` taking
  the whole tree, the job's memory and CPU limits, the environment, exit
  codes, refusing what native can't enforce, `auto` without WSL, `doctor`,
  and `sbx mcp` run natively; and, where `sbx setup-windows` has run (CI
  runs it first), network deny and proxy natively (see
  [above](#network-natively-sbx-setup-windows)).

Each escape test skips, and says so, on a kernel that cannot enforce what
it checks.
