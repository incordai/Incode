# incode SDKs

Libraries for programs that drive `incode`, in two ways:

| | One-shot | Interactive |
|---|---|---|
| incode side | `incode -t "<task>" --output-format json` | `incode --acp` (Agent Client Protocol over stdio) |
| Python (`sdk/python`, `incode_sdk`) | `run_task()` | `Session` (threads; `incode_sdk.aio.AsyncSession` for asyncio) |
| TypeScript (`sdk/typescript`, `@incord/incode-sdk`) | `runTask()` | `Session` (async iterator of events) |

Both SDKs have no runtime dependencies and the same shape: a prompt streams typed
events (message and thought chunks, tool calls and their updates, the plan, the
context window's usage, mode and option changes) and ends with a `PromptResult`
carrying the stop reason. incode's requests to the client are answered by
callbacks: `session/request_permission` (default: reject) and
`fs/read_text_file` / `fs/write_text_file` (default: inside the workspace only,
after resolving symlinks and `..`).

```
sdk/
  python/            incode_sdk package, unittest suite
  typescript/        @incord/incode-sdk package, node:test suite
  testing/
    fake_incode.py   a scripted fake incode (both -t and --acp) used by both suites
  examples/          python_example.py, typescript_example.mjs
```

## Running the tests

From the repository root:

```sh
# Python (3.9+, stdlib only)
python3 -m unittest discover sdk/python/tests

# TypeScript (Node 18+; needs network once for `npm install`, offline after)
cd sdk/typescript && npm install && npm test
```

Both suites run against `sdk/testing/fake_incode.py`, so they need `python3` but
no incode build and no agent CLI. Each has one live test, skipped unless
`INCODE_BIN` names a real binary; it runs `initialize` and `session/new` only:

```sh
cargo build --release
INCODE_BIN=$PWD/target/release/incode python3 -m unittest discover sdk/python/tests
(cd sdk/typescript && INCODE_BIN=$PWD/../../target/release/incode npm test)
```

## The fake

`sdk/testing/fake_incode.py` picks a scenario from the task or prompt text — see
its docstring. `hello` streams one of everything, `permission` asks permission,
`fs R W` reads R and writes W through the client, `wait` waits for a cancel,
`crash` exits mid-prompt, `malformed` writes lines that are not JSON-RPC. Point
an SDK at it with `incode_path=[sys.executable, ".../fake_incode.py"]` /
`incodePath: ["python3", ".../fake_incode.py"]`.

## Things to know about incode itself

- `-p` with `-t` sets what every candidate agent may do (`read-only`, `workspace-write`, ...).
- An error found before the run starts (bad `-w`, bad `-p`) still prints one
  result object with `--output-format json` (`{"type": "result", "is_error":
  true, "error": ...}`) and exits 1; the SDKs raise `TaskFailed` with it, or
  `IncodeError` with stderr from an older incode that printed nothing.
- `session/update` notifications are per session, not per prompt. incode sends
  `available_commands_update` just *after* answering `session/new`, so a prompt
  sent at once can see it in its stream.
- incode does not currently send `fs/read_text_file` or `fs/write_text_file`
  (its agent CLIs use their own file tools); the SDKs answer them anyway, as ACP
  clients, so they work if and when it does.
