# insbx: one sandbox for any agent

`insbx` runs a command under one policy: what it may write, what it may read,
which hosts it may reach, which environment it starts with, and how much it
may use. It works with any agent CLI (`claude`, `codex`, `incode`, `aider`,
…), with a test suite a harness runs, or with a single tool call. The policy
means the same thing on every platform. Where a platform cannot enforce part
of it, `insbx` says so before anything runs. By default it then refuses
instead of running with less.

```sh
# Write only in this project; network only to the package registry; 30 minutes.
insbx run -w . --allow-host crates.io --allow-host '*.crates.io' --wall 1800 -- cargo build

# Claude Code confined to its workspace and ~/.claude, network to the API only,
# with a folder it must not see (the ready-made policy: policies/claude-code.toml).
insbx run --policy policies/claude-code.toml --deny-read ../private-checks -- claude -p "fix the failing test"

# What would this machine enforce?
insbx doctor
insbx check --policy agent.toml --json
```

It's a crate in the incode repository for now and doesn't depend on incode.
It builds to one binary, `insbx`, and a library, `incord_sandbox`.

## The policy file

```toml
[fs]
mode = "workspace-write"        # off | read-only | workspace-write
write = ["."]                   # writable roots
deny_read = ["../private"]      # hidden: not even readable
msys = false                    # Windows, network deny/proxy: Git Bash works (--msys, see below)

[network]
mode = "proxy"                  # allow | deny | proxy
allow = ["crates.io", "*.crates.io:443", "registry.npmjs.org"]
allow_private = false           # may an allowed name resolve to a private address?
loopback_ports = [5173]         # this machine's own ports the proxy may reach

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

Every flag of `insbx run` has a policy key. Flags apply on top of `--policy`.
An unknown key is an error, so a misspelt `wirte` can't silently do nothing.

## What is enforced, and how

| | Linux | macOS | Windows (native) |
|---|---|---|---|
| filesystem: write | Landlock (5.13+) | Seatbelt | write-restricted token + a per-run ACE on each root |
| filesystem: `deny_read` | Landlock | Seatbelt | **not enforced** (WSL2 does it) |
| network: `deny` | Landlock TCP (6.7+) + seccomp for every other socket | Seatbelt | after `insbx setup-windows`: runs as an offline account WFP filters cut off; else **not enforced** (WSL2 does it) |
| network: `proxy` | Landlock: TCP only to the proxy's port; seccomp as for `deny` | Seatbelt: outbound only to the proxy | after `insbx setup-windows`: the offline account may reach only the proxy's loopback ports; else **not enforced** (WSL2 does it) |
| `.git/hooks`, `.git/config` | snapshotted, put back after the run | Seatbelt refuses the write | snapshotted, put back after the run |
| signals, abstract Unix sockets | Landlock scopes (6.12+): only within the sandbox | Seatbelt default | the token cannot open other processes for writing (measured by incode) |
| Windows programs (inside WSL2) | interop interpreter and sockets hidden, `WSL_INTEROP` removed, vsock refused | n/a | n/a |
| limits | `setrlimit` + wall clock; no core dumps | `setrlimit` + wall clock; no core dumps | job object: wall clock, CPU and committed memory per process; **no** `file_size_mb`, `open_files` |
| environment | yes | yes | yes |

On Windows, `insbx.exe` runs natively when native enforces the whole policy,
and forwards it to the Linux `insbx` inside WSL2 when it doesn't (see
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
  - `loopback_ports` (`--allow-loopback-port 5173`) lets the command reach
    those ports on this machine, say a dev server a browser test loads,
    without `allow_private`. The request must name `localhost`, `127.0.0.1`
    or `::1`, and only a loopback address is connected to. Nothing else
    private opens, and other ports stay refused. `insbx mcp` never gives a
    sandbox these ports, even when its base policy names some.
  - When `insbx` itself runs behind an HTTP proxy, allowed requests are chained
    through it, and that proxy does the resolving. Requests for this
    machine's own ports go straight to loopback, never through it.
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
    without also blocking file creation in the root, so `insbx` snapshots each
    root's `.git/hooks` and `.git/config` before the run. Anything changed is
    put back afterwards and reported (`protected path changed by the
    command, restored …`, and `protected_restored` from `insbx mcp`).
  - **How (macOS):** Seatbelt also refuses the writes.
  - **Worktrees:** a `.git` file (`gitdir:`) and its `commondir` are
    followed.
  - **Opting out:** `--no-protect-git` for a run that should change hooks.
- **Signals and abstract sockets.** On Landlock ABI v6 (Linux 6.12+) a
  sandboxed command can't signal processes outside its sandbox, or connect to
  an abstract Unix socket made outside it (a D-Bus session bus, X11). `insbx
  doctor` says whether the kernel does this.
- **Code the loader would inject.** `LD_PRELOAD`, `LD_AUDIT`,
  `DYLD_INSERT_LIBRARIES` and `DYLD_FORCE_FLAT_NAMESPACE` are removed by
  default (`env.strip_loader_injection`): they would load a library into
  every program, the sandbox's own helper included. Core dumps are off
  (`RLIMIT_CORE=0`), so a crash can't write the process's memory to disk.
- **Process lifetime.**
  - With a wall-clock limit, the command gets its own process group. The
    limit kills all of it, and `SIGINT`/`SIGTERM`/`SIGHUP` sent to `insbx` are
    passed on.
  - If `insbx` is killed with `SIGKILL`, a supervisor kills the group too.
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

`insbx.exe` has two backends, picked per run by `INSBX_BACKEND`:

| `INSBX_BACKEND` | what runs |
|---|---|
| `auto` (default) | natively when native enforces everything the policy asks for; otherwise in WSL2 when it is set up; otherwise refused, naming both ways forward |
| `native` | always natively; what it can't enforce is refused (strict) or reported (`--best-effort`) |
| `wsl` | always the Linux `insbx` inside WSL2 |

`INSBX_NATIVE=1`, the older switch, still means `native`. `insbx doctor` shows
what native enforces and whether WSL2 forwarding is available.

So a policy that confines files, environment and time runs natively. One
that asks for `network: deny`/`proxy` runs natively too once
[`insbx setup-windows`](#network-natively-insbx-setup-windows) has been run on
the machine, and goes to WSL2 otherwise. `deny_read` always goes to WSL2.
`insbx mcp` follows the same rule from its ceilings: with `--net allow` it
runs natively, with `--net deny` (its default) or `proxy` natively after
the setup and in WSL2 without it.

### Native

Ported from incode's own Windows sandbox, whose authors measured it on
Windows. This crate's port is checked by its tests
(`tests/escape_windows.rs`) on the `windows-latest` CI runner.

- **Writes.** The command runs under a *write-restricted token*: it reads
  anything its user can, but writes only where the user may **and** one of
  the token's restricting SIDs is granted. Those are a private SID made for
  this run (granted `Modify` on the run's private temp directory only), the
  SID of each of this run's writable roots (below), `Everyone` (so `> NUL`
  works) and the logon SID (so the command can start). `read-only` grants
  only the temp directory.
- **A root's grant is kept.** NTFS writes an inherited ACE into every object
  beneath a folder, so granting one takes time in proportion to its tree
  (2.3 s per walk measured on 17,752 objects). A per-run grant walked each
  root twice per run (grant and revoke), and five to seven times under
  network deny: `echo` took 4.7 s, and 14-20 s with the network denied, on
  a repository that size. So each writable root has its own SID, derived
  from its path, and its `Modify` ACE is made by the first run on the folder
  and then **kept**: later runs read the folder's own ACL to see it is
  there, and walk nothing. A run's token carries the SIDs of its own roots
  only, so a kept grant lets no other run write there. `insbx grants` lists
  kept folders; `insbx revoke <DIR>|--all` takes their grants back. No
  file's contents or modification time change. A drive root, any folder
  holding your user profile, and insbx's own state folder are refused as
  roots.
- **The temp directory's grant is per run.** It is added before the command
  starts and removed after it exits, and written down first
  (`%LOCALAPPDATA%\insbx\grants`): if `insbx` crashes, the next run removes
  the ACE it left.
- **Everything it starts.** The command starts suspended, goes into a job
  object, and only then runs. Everything it starts is in the job too and
  cannot leave it. The job is killed when the command exits, at the
  wall-clock limit, or when `insbx` itself dies. So on Windows nothing the
  command started outlives the run, even without a time limit.
- **Limits.** `memory_mb` caps the memory each process *commits* (an
  allocation beyond it fails), which is not quite `RLIMIT_AS`. `cpu_secs`
  caps each process's user-mode CPU time. `file_size_mb` and `open_files`
  have no Windows equivalent: asking for them is reported not enforced.
  (`insbx mcp` sets no file size limit on Windows for this reason.)
- **Programs.** The program is found on `PATH` with `PATHEXT`, never in the
  current folder, so a `cmd.exe` planted in the workspace is not picked up.
  A `.bat`/`.cmd` runs through `cmd.exe /d`, so `cmd` parses its arguments.
- **Exit codes** are passed through whole (32 bits).

**Not enforced natively, and why:**
- **Network, until `insbx setup-windows` has run.** No token restricts the
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
  msys runtime builds its pipes and sections with a DACL that grants only the
  token's user, which a write-restricted token may not write
  (`couldn't create signal pipe, Win32 error 5`, measured). They work with
  [`--msys`](#git-bash-natively---msys) (network `deny` or `proxy`, after
  `insbx setup-windows`); otherwise use `cmd` or PowerShell natively, or
  `INSBX_BACKEND=wsl` for a POSIX shell. `insbx mcp` scripts default to
  PowerShell on Windows.
- **Grants are visible, and kept.** Every object under a root that a run
  has used carries the root's ACE until `insbx revoke`. A file moved into a
  root from elsewhere keeps the ACL it had (no grant re-ACLs it, kept or
  per run), so the command cannot write it until `icacls <FILE> /reset`
  gives it its folder's ACL.

### Network natively: `insbx setup-windows`

The Windows Filtering Platform can block a connection by the *user* who
makes it. So, the way OpenAI Codex's Windows sandbox does it
(`codex-rs/windows-sandbox-rs`), a one-time setup makes a local account
that the firewall cuts off, and `deny`/`proxy` runs start the command as
that account. From an elevated prompt (Run as administrator), once:

```powershell
insbx setup-windows                         # or: ... --read C:\Users\me\.cargo --read C:\Users\me\.rustup
insbx doctor                                # [ok  ] offline user ...
insbx run -w . --net deny -- cargo test     # natively now, not in WSL2
insbx setup-windows --remove                # undoes all of it
```

What it does (each step is idempotent; running it again resets the
password and re-adds each filter):

1. **An account, `insbx-offline`**, member of `Users` only (it needs `Users`
   to log on locally and to read `C:\Windows` and `Program Files`; it is
   taken out of `Administrators` if anyone put it there), with a random
   password (256 bits) that is never shown, hidden from the sign-in screen.
   **And a second one, `insbx-msys`, the same way**, for `--msys` runs
   ([below](#git-bash-natively---msys)).
2. **Their passwords**, DPAPI-protected with machine scope, in
   `%ProgramData%\insbx\offline.conf`. The folder's ACL allows SYSTEM,
   Administrators and the user who ran the setup, and nobody else: not
   `Users`, so not the offline account, and not a sandboxed command. Machine
   scope rather than user scope because the setup may run elevated as a
   different administrator than the user who runs sandboxes; not LSA
   private data because only administrators can read that back, and runs
   are not elevated. The ACL is the protection; DPAPI makes the file
   useless off this machine.
3. **Persistent WFP filters** for both accounts' SIDs, in one condition so
   they can never differ
   (`FWPM_CONDITION_ALE_USER_ID`): every outbound connection and every first
   UDP/raw packet (`ALE_AUTH_CONNECT`), every inbound connection
   (`ALE_AUTH_RECV_ACCEPT`), IPv4 and IPv6, loopback included, and raw ICMP
   sockets (`ALE_RESOURCE_ASSIGNMENT`), all blocked. One permit outranks
   them: TCP to `127.0.0.1` on ports 47600-47615 (`--proxy-ports` changes
   the range), where a `proxy` run's proxy listens. WFP directly
   (`FwpmFilterAdd0`, in one transaction, under fixed keys), not `netsh`.
4. `--read <DIR>` (repeatable, kept across re-runs) lets both accounts read a
   folder: a toolchain under your profile (see *Reads* below).

A machine set up before `insbx-msys` existed needs `insbx setup-windows`
again: the record changed (version 2), and `insbx doctor` says so.

At run time (unelevated), `insbx` starts its helper a second time as the
account with `CreateProcessWithLogonW`, the one call an unprivileged process
can start another user's process with (`CreateProcessAsUserW` with that
user's token needs `SeAssignPrimaryTokenPrivilege`, and
`CreateProcessWithTokenW` `SeImpersonatePrivilege`; Codex starts its command
runner the same way). That helper builds the write-restricted token from its
own token, as above, and starts the command; the job object holds both, so
limits, the wall clock, killing the tree and exit codes work as before. No
profile is loaded for the account (Codex's runner doesn't either): it would
create `C:\Users\insbx-offline`, a home every run would share. The command
gets the run's computed environment.

What changes for the command:
- **Reads.** It reads what the account may read, not what you may: system
  folders, and what each run grants it (the writable roots, the folder it
  starts in, the `insbx` program, its private temp directory), plus folders
  granted with `--read`. A toolchain installed under your profile
  (`.cargo`, `.rustup`, a per-user Python or Node) needs `--read` once.
  Narrower than without network deny, never wider. One more right: the
  account may *list* each folder above the current folder, up to the drive
  root or `C:\Users` (names, no contents), by one ACE per folder that
  nothing beneath it inherits, for the run. Windows PowerShell needs it: it
  looks every part of its starting folder up in the folder above, and
  without it starts at `C:\` (so `Set-Content x.txt` wrote `C:\x.txt`). So
  during a run the command can see the names in, say, your profile folder
  and `AppData\Local\Temp` on the way to a project under them.
- **Writes** need both the account (the token's normal check) and the
  root's own SID (its restricted check), so each root gets an ACE for each.
  Another run's command, also the account but without that root's SID,
  cannot write your run's roots. The account's ACEs on roots and on the
  folder a run starts in are kept with the roots' own, so a later
  network-deny run may READ a folder an earlier one was given, never write
  it; `insbx revoke` takes them back too. Its ACEs on the temp directory and
  the `insbx` program last the run.
  With `--fs off` the command is not write-restricted but is still the
  account: it can write the folder it starts in and its temp directory, and
  what the account itself may write.
- **`insbx` itself** must be readable by the account; it is granted per run
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
  that user alone). Another user's `insbx doctor` says so.
- The thread objects of the account's helper get the account logon's default
  DACL, which is not measured yet; its process object is locked down
  explicitly (`D:P(A;;GA;;;SY)(A;;0x101000;;;OW)`).

**Tested:** `tests/escape_windows.rs` runs on the `windows-latest` CI
runner after `insbx setup-windows` (CI runs it; the runner is an
administrator): TCP to a local server and to the internet refused under
`deny` with the filesystem confined, read-only and off (and allowed without
`deny`); UDP refused; writes confined, temp writable, exit codes and
"not found" passed through both helpers; the account's ACE on a root kept
after a run and gone after `insbx revoke`; two concurrent runs sharing a root (the second ending
first leaves the first its grant, and cannot write the first's other root);
the proxy letting through only its allowlist, a direct connection refused;
`insbx mcp` running a deny sandbox's PowerShell script as the account.
Without the setup these tests print SKIPPED.

### Git Bash natively: `--msys`

Git Bash and every msys program fail under the write-restricted token. The
msys runtime builds its pipes and shared sections with a DACL that grants
only the token's user (msys2-runtime `sec_acl`: `GENERIC_ALL` for the user's
SID and Administrators, nothing else), and a write-restricted token passes a
write only where one of its restricting SIDs is granted. So `bash -c "echo ok
> x"` dies with `couldn't create signal pipe, Win32 error 5`. Measured: adding
the token's own user SID to its restricting SIDs makes it work.

That cannot be done as the offline account: its ACEs on roots and current
folders are kept across runs, and with its SID trusted a run wrote into
another folder an earlier deny run had used (measured). So `--msys` (or
`[fs] msys = true` in a policy) runs the command as the second account,
`insbx-msys`, which has no kept ACE anywhere:

```powershell
insbx run --msys --net deny -w . -- "C:\Program Files\Git\bin\bash.exe" -c "make test"
```

- **Needs** `insbx setup-windows` (it makes `insbx-msys` and gives it the
  same firewall filters and `--read` folders as `insbx-offline`), and network
  `deny` or `proxy`, the modes that run as an account. With `allow` the run
  is refused in strict mode (it would run as you, where Git Bash does not
  start); `--best-effort` runs it without `--msys`. `--msys` is ignored on
  Linux and macOS, whose sandboxes have no such limit.
- **Costs:** the account's access is granted per run and taken back when the
  run ends, by walking each writable root and read folder (NTFS stores
  inheritance in every object: about 2.4 s each way on 18,673 objects,
  measured by incode), so a big workspace pays that on every `--msys` run,
  not once. A run that crashes leaves its ACEs for the next run's sweep. If
  the sweep cannot take them back (the root was moved or renamed, a drive is
  detached, a revoke failed), the next `--msys` run is REFUSED, naming the
  paths: every msys run trusts that account, so a write ACE it keeps anywhere
  would be writable by all of them. Run `insbx revoke <FOLDER>` on each (where
  it is now), then `insbx revoke --msys-leftovers`. A root is told by what it
  is, not by its name: moved aside and replaced by a new folder of the same
  name counts as missing. And after every run's revoke the tree is read once
  more: an explicit entry for the account on a folder beneath a root (one the
  revoke walk cannot remove) makes exactly those objects leftovers too; a
  read-only entry (a `--read` folder's) does not.
- **Refused roots:** a root whose ACL passes down a CREATOR OWNER entry (each
  file the account creates would carry an entry naming it that no revoke
  removes), or an OWNER RIGHTS entry broader than `READ_CONTROL`. The message
  gives the `icacls /remove:g` that clears it. Only the root's own ACL is
  read, not every folder beneath it.
- **One `--msys` run at a time**, since the account is shared by every run
  (the named mutex `Global\insbx-msys`). A second one waits, and says so.
- **What stays kept:** each root's own SID (useless without a token that
  carries it), the user's `READ_CONTROL | WRITE_DAC` entry (as for the offline
  account), and one inheritable OWNER RIGHTS entry granting only
  `READ_CONTROL`. A file the msys account creates is owned by it, and an owner
  holds `WRITE_DAC` implicitly; the entry takes that implicit right away from
  a file created with the default ACL, and you, whose own entry survives, can
  still re-ACL it. It does not cover a file the account creates with its own
  explicit, protected ACL (the creator decides what is on its own file), and
  `Everyone` is a restricting SID, so wherever `Everyone` may write the msys
  token passes already. `insbx revoke` leaves these two entries.
- The roots' and read folders' access is the account's, so what it creates
  there is owned by `insbx-msys`, as under `deny` it is owned by
  `insbx-offline`.

`tests/escape_windows.rs` checks it where the account exists (else SKIPPED):
`bash -c "echo ok > x && cat x"` as `insbx-msys`; a write into a folder
another run kept a grant on refused; the account's ACEs gone from the root
afterwards, the root's SID and OWNER RIGHTS entry kept; two runs serialized.

### WSL2

Inside WSL2 the sandbox is the Linux one: Landlock, seccomp, the git
protection and `insbx mcp` all apply unchanged.

**Setup, once** (PowerShell, from the incode folder):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1
```

It:
1. installs WSL2 and Ubuntu if they're missing (asks first, one
   administrator prompt, usually a restart, then run it again);
2. builds the Linux `insbx` inside WSL2 at `~/.local/bin/insbx`;
3. builds `insbx.exe` and adds it to your PATH;
4. runs `insbx doctor`.

**How `insbx.exe` forwards.**
- The command (`run`, `check`, `mcp`; `doctor` too under `INSBX_BACKEND=wsl`)
  is passed to the Linux `insbx` with `wsl.exe`, from the same folder.
- Paths are translated: `C:\work\app` becomes `/mnt/c/work/app`, and
  `\\wsl$\Ubuntu\home\me` becomes `/home/me`.
- Output, input and the exit code pass straight through, so `insbx mcp` works
  for agents on Windows too.
- **Choosing a distribution:** `INSBX_WSL_DISTRO` picks one other than the
  default.
- **Windows environment variables don't reach WSL** unless listed in
  `WSLENV` (for example `WSLENV=ANTHROPIC_API_KEY`).
- The command must exist inside WSL (`python3`, not `python.exe`).

WSL2 adds one way out that plain Linux doesn't have: starting a Windows
program (`cmd.exe`, `powershell.exe`, any `.exe` under `/mnt/c`), which
runs on Windows, outside the sandbox. When the filesystem is confined and
WSL is detected, `insbx` closes it (`fs.block_wsl_interop`, on by default):
- WSL's interop interpreter (`/init`, read from
  `/proc/sys/fs/binfmt_misc/WSLInterop`) is hidden like a `deny_read` path,
  so no `.exe` can start;
- `/run/WSL`, where the interop sockets live, is hidden and `WSL_INTEROP`
  is removed;
- vsock is refused by seccomp.

Hiding `/init` means `ls /` is refused inside the sandbox; files beneath
`/` still read normally.

**Network inside WSL2:** `network: deny` needs Landlock's TCP rules (Linux
6.7+). On an older WSL kernel only UDP/DNS are blocked, `insbx doctor` says
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
| 125 | `insbx` refused (strict, or a bad policy) or failed |
| 126 / 127 | the command could not be run / found |
| 128 + n | it was killed by signal n |

## Sandboxes as a service: `insbx mcp`

`insbx mcp` is an MCP server (stdio) that lets an agent work in sandboxes:
create one, write scripts and files into it, run them, read what they
produced, and delete it. Any agent that speaks MCP can use it: Claude Code,
Codex, incode, or your own.

```json
{ "mcpServers": { "insbx": { "command": "insbx", "args": ["mcp", "--net", "proxy", "--allow-host", "pypi.org", "--allow-host", "*.pythonhosted.org"] } } }
```

| Tool | What it does |
|---|---|
| `sandbox_create` | A private folder. Network `deny` unless the agent asks for `proxy` with hosts (or `allow`) within the server's limits. Natively on Windows `deny` and `proxy` need `insbx setup-windows` first; without it asking for them is refused at once, saying so. |
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
- `tests/mcp.rs`: `insbx mcp` driven over stdio like an agent drives it. It writes a script, runs it and reads its result; tries escapes (writing outside, the network, host secrets, a planted symlink, `..`, absolute paths); checks the time limit and background cleanup, the limits on what an agent may ask for, and that sandboxes are removed on exit;
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
  writes outside the root, a root's grant kept and writable only to runs
  that name it, the temp grant gone after the run, `insbx revoke`,
  read-only and its private temp, child `cmd` and PowerShell, a planted
  program in the current folder, the wall limit and killing `insbx` taking
  the whole tree, the job's memory and CPU limits, the environment, exit
  codes, refusing what native can't enforce, `auto` without WSL, `doctor`,
  and `insbx mcp` run natively; and, where `insbx setup-windows` has run (CI
  runs it first), network deny and proxy natively (see
  [above](#network-natively-insbx-setup-windows)).

Each escape test skips, and says so, on a kernel that cannot enforce what
it checks.
