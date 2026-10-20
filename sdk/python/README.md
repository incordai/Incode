# incode_sdk (Python)

Drive [incode](../../README.md) from Python 3.9+. Standard library only.

```sh
pip install ./sdk/python        # or put sdk/python on PYTHONPATH
```

## One-shot: `run_task`

Runs `incode -t "<task>" --output-format json` and parses the result object.

```python
from incode_sdk import run_task, TaskFailed, IncodeError

try:
    r = run_task("Add a unit test for parse_args", workspace="/path/to/repo",
                 model="sonnet", timeout=600)
    print(r.result)                      # the agent's final answer
    print(r.usage.input_tokens, r.usage.output_tokens, r.cost_usd)  # cost_usd may be None
    print(r.stop_reason, r.agent, r.duration_ms, r.agent_session_id)
except TaskFailed as e:                  # incode ran and said is_error: true
    print("failed:", e.result.error, e.exit_code)
except IncodeError as e:                 # could not start, timeout, no result object
    print(e, e.stderr)
```

`TaskResult` fields: `is_error, result, error, session_id, agent, model,
agent_session_id, stop_reason, served_by, usage (TaskUsage), cost_usd,
duration_ms, spec_gate, exit_code, stderr, raw`. Options: `workspace`, `model`,
`permission_mode` (note: incode does not currently apply `-p` to `-t` runs),
`timeout` (seconds; the process is killed), `incode_path` (a path, or a command
prefix list), `env` (added to the environment), `extra_args` (for example
`["--spec-gate=off"]`).

## Interactive: `Session`

`incode --acp`, spoken over stdin/stdout as ACP (JSON-RPC 2.0, one message per
line). One `Session` is one process holding one ACP session.

```python
from incode_sdk import Session, MessageChunk, ToolCall, ToolCallUpdate, Plan, Usage, PromptResult

def on_permission(request):          # session/request_permission
    print("asked:", request.title)
    return request.allow()           # or request.reject(), True/False, None (= cancelled)

with Session("/path/to/repo", model="sonnet", permission_mode="workspace-write",
             on_permission=on_permission) as s:
    print(s.session_id, s.current_mode, [o["id"] for o in s.config_options])
    for event in s.prompt("Explain what src/main.rs does"):
        if isinstance(event, MessageChunk):
            print(event.text, end="")
        elif isinstance(event, ToolCall):
            print("\n[tool]", event.title, event.kind)
        elif isinstance(event, PromptResult):
            print("\nstop:", event.stop_reason)   # end_turn, cancelled, ...

    s.set_mode("read-only")                  # session/set_mode
    s.set_config_option("agent", "codex")    # session/set_config_option
    stream = s.prompt("And now?")
    result = stream.wait()                   # consume; stream.text holds the reply
```

- `prompt(text_or_blocks, timeout=None)` returns a `PromptStream`: iterate it for
  events; the last item is `PromptResult`. It also keeps `.text`, `.events` and
  `.result`. One prompt at a time per session.
- `cancel()` sends `session/cancel` (the prompt ends `cancelled`) and answers any
  open permission question `cancelled`. Safe to call from another thread.
- Events: `MessageChunk`, `ThoughtChunk`, `UserMessageChunk`, `ToolCall`,
  `ToolCallUpdate`, `Plan`, `Usage` (context window `used`/`size`),
  `ModeChanged`, `ConfigOptionsChanged`, `AvailableCommands`, `UnknownUpdate`.
  Each has `session_id` and `raw` (the update as sent).
- Updates that arrive while no prompt is running go to `on_update(event)` and
  `idle_events` (for example the slash commands sent after `session/new`, or a
  loaded session's replayed history). Updates are per session, not per prompt:
  one sent just as a prompt starts lands in that prompt's stream.
- `on_read_file(ReadFileRequest) -> str` / `on_write_file(WriteFileRequest)`
  answer `fs/read_text_file` / `fs/write_text_file`. The defaults
  (`WorkspaceFS`) refuse any path whose real path — symlinks followed, `..`
  resolved — is outside the workspace, with JSON-RPC error -32602. Pass
  `False` to not offer the capability. Raise `FsError(message, code)` from a
  custom handler to choose the error.
- Lines on incode's stdout that are not JSON-RPC are skipped and reported to
  `on_protocol_error(line, error)` and `protocol_errors`.
- If incode exits, every waiting call and the running prompt raise
  `AgentCrashed` with `exit_code` and the stderr tail (`session.stderr`).
- Other options: `load_session_id` (`session/load`), `mcp_servers`, `env`,
  `extra_args`, `request_timeout` (seconds, default 120). Raw access:
  `request(method, params)` and `notify(method, params)`.

Callbacks run on worker threads, never on the reader, so a slow permission
prompt does not hold up streaming or a cancel.

### asyncio

```python
from incode_sdk.aio import AsyncSession, run_task_async

async with AsyncSession("/path/to/repo") as s:
    async for event in s.prompt("hello"):
        ...
```

A thin wrapper: the work still runs on threads.

## Errors

`IncodeError` (base; `.stderr`, `.exit_code`) → `TaskFailed` (`.result`),
`IncodeTimeout`, `ProtocolError`, `AcpError` (`.method`, `.code`, `.data`),
`AgentCrashed`.

## Tests

From the repository root:

```sh
python3 -m unittest discover sdk/python/tests
# plus the live test against a real binary (initialize + session/new only):
INCODE_BIN=/path/to/incode python3 -m unittest discover sdk/python/tests
```

The suite runs against `sdk/testing/fake_incode.py`.
