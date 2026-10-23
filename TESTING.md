# Testing incode locally

Everything CI runs, and the parts it cannot, from your own machine.

## One command

```sh
scripts/test-all.sh                 # every part this machine has the toolchain for
scripts/test-all.sh rust sbx        # only some parts: rust sbx python ts vscode
QUICK=1 scripts/test-all.sh         # the same without clippy (the slowest step)
```

A part whose toolchain (cargo, python3, npm) or folder is missing is skipped
and named in the summary; the exit code is non-zero when any part that ran
failed.

On Linux, building incode needs pkg-config and the OpenSSL headers; the
desktop backends (x11rb, zbus) are pure Rust and need no system library:

```sh
sudo apt-get install -y --no-install-recommends pkg-config libssl-dev
```

The X11 screen capture has a live test that needs an X server; with Xvfb:

```sh
Xvfb :99 -screen 0 1280x800x24 &
DISPLAY=:99 cargo test --lib desktop::capture::x11 -- --ignored
```

The tests create git repositories and commit in them, so git needs a name and
an email (`git config --global user.name ...`, `user.email ...`).

## The parts

| Part | Commands | What it covers |
|---|---|---|
| rust | `cargo fmt --all -- --check` · `cargo clippy --all-targets -- -D warnings` · `cargo test --lib` · `cargo test --bins` | incode itself: about 2,100 library and 50 binary tests |
| rust | `cargo test --test sbx_engine` | Real shell commands run under `sandbox_engine = "sbx"`: a write outside the workspace refused (with an unconfined control), a planted git hook undone, `GIT_DIR` stripped. Linux; says it skipped elsewhere |
| sbx | `cargo clippy -p incord-sandbox --all-targets -- -D warnings` · `cargo test -p incord-sandbox` | The standalone sandbox: escape tests per OS, the MCP server, `.git` protection, WSL path translation. On Windows CI, `sbx setup-windows` runs first so the native network-deny tests run |
| python | `python3 -m unittest discover sdk/python/tests` | The Python SDK, against a fake incode |
| ts | `cd sdk/typescript && npm install && npm test` | The TypeScript SDK, against the same fake |
| vscode | `cd editors/vscode && npm install && npm run compile && npm test` | The VS Code extension's ACP client, against a fake agent |

The SDK and extension suites also talk to the real binary when you point them
at one: `INCODE_BIN=$PWD/target/debug/incode` (build it first with
`cargo build`).

## Windows

From the incode folder, in PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\test-windows.ps1
```

It builds `sbx`, runs its Windows tests (writes outside the workspace, child
processes, the time, CPU and memory limits, killing `sbx`, the environment,
exit codes, the MCP server) and writes `sbx-windows-report.txt`. These need
real Windows: Wine cannot check Windows' own security. Nothing is installed
and nothing outside a temp folder is changed.

The network tests (deny and proxy natively: TCP, UDP, the offline account's
grants, concurrent runs, the proxy's allowlist) run only where
`sbx setup-windows` has been run, from an elevated prompt (it makes a local
account and firewall filters; `sbx setup-windows --remove` undoes it);
elsewhere they print SKIPPED. CI runs the setup on `windows-latest` before
`cargo test -p incord-sandbox`, so they always run there.

The whole Rust suite runs on Windows too (`cargo test --lib`, `cargo test --bins`).

## Live tests, off by default

`#[ignore]`d because they touch the real machine or need a service. Run one
with `cargo test --lib <name> -- --ignored --nocapture`.

| Test | Needs | What it does |
|---|---|---|
| `sandbox::windows::acl` `live_*` | Windows; `INCODE_SANDBOX_LIVE_DIR` (a scratch folder) and `INCODE_SANDBOX_LIVE_BIN` (a built `incode.exe`) | Grants, confines and restores real ACLs; sweeps a crashed session's grant |
| `desktop::thread` (moves the cursor) | A desktop session you are not using | Moves the real mouse and puts it back |
| `desktop::capture::tests::live::report_real_capture` | A display | Captures the real screen and prints the coordinate mapping |
| `desktop::win32`, `desktop::uia` reports | Windows | Print the display table and the focused window's accessibility tree |
| `mcp_oauth::tests::open_browser_runs_nothing_from_the_address` | Windows with a default browser | Opens a browser tab on a loopback listener and checks the address reached it, while a `\|mkdir,…` in the address ran nothing |
| `toolbox::code_edits_tests::live_an_unseen_change_is_refused_by_the_engine` | `INCODE_LIVE_MEMORY_URL` (a throwaway Incord Memory engine) and `INCODE_LIVE_REPO` | Checks the engine refuses an edit to code it has not seen |

## By hand

Things the suites cannot judge, worth a minute in a real terminal and editor:

- `incode` in a terminal: the status line (`statusline_command`), `/vim`,
  `/cost`, a notification after a long turn (`notify`), a custom command from
  `.incode/commands/`, `/review`.
- `incode review` on a branch with a change; `incode mcp login <name>` against
  a remote MCP server that needs a login; `incode plugin add` / `update`.
- `sandbox_engine = "sbx"` with `sandbox_allow_hosts`, then a command that
  reaches an allowed host and one that does not.
- The VS Code extension (install the `.vsix` its README builds) and any ACP
  editor (`incode --acp`).
