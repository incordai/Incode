# @incord/incode-sdk (TypeScript)

Drive [incode](../../README.md) from Node 18+. No runtime dependencies; ESM with
type declarations.

```sh
cd sdk/typescript && npm install && npm run build   # dist/index.js + dist/index.d.ts
```

## One-shot: `runTask`

Runs `incode -t "<task>" --output-format json` and parses the result object.

```ts
import { runTask, TaskFailed, IncodeError } from "@incord/incode-sdk";

try {
  const r = await runTask("Add a unit test for parse_args", {
    workspace: "/path/to/repo",
    model: "sonnet",
    timeoutMs: 600_000,
  });
  console.log(r.result, r.usage.inputTokens, r.usage.outputTokens, r.costUsd /* may be null */);
} catch (e) {
  if (e instanceof TaskFailed) console.error("failed:", e.result.error, e.exitCode);
  else if (e instanceof IncodeError) console.error(e.message, e.stderr);
  else throw e;
}
```

`TaskResult`: `isError, result, error, sessionId, agent, model, agentSessionId,
stopReason, servedBy, usage {inputTokens, outputTokens, cacheCreationTokens,
cacheReadTokens}, costUsd, durationMs, specGate, exitCode, stderr, raw`.
Options: `workspace`, `model`, `permissionMode` (note: incode does not currently
apply `-p` to `-t` runs), `timeoutMs`, `signal` (AbortSignal), `incodePath` (a
path, or a command prefix array), `env`, `extraArgs`.

## Interactive: `Session`

`incode --acp`, spoken over stdin/stdout as ACP (JSON-RPC 2.0, one message per
line). One `Session` is one process holding one ACP session.

```ts
import { Session } from "@incord/incode-sdk";

const session = await Session.start({
  workspace: "/path/to/repo",
  permissionMode: "workspace-write",
  onPermission: async (request) => {
    console.log("asked:", request.title);
    return request.allow(); // or request.reject(), true/false, null (= cancelled)
  },
});
try {
  for await (const event of session.prompt("Explain what src/main.rs does")) {
    switch (event.type) {
      case "message_chunk": process.stdout.write(event.text); break;
      case "tool_call": console.log(`\n[tool] ${event.title}`); break;
      case "plan": console.log(event.entries); break;
      case "prompt_result": console.log(`\nstop: ${event.stopReason}`); break;
    }
  }
  await session.setMode("read-only");
  await session.setConfigOption("agent", "codex");
  const stream = session.prompt("And now?");
  const result = await stream.wait(); // stream.text holds the reply
} finally {
  await session.close();
}
```

- `prompt(textOrBlocks, { timeoutMs })` returns a `PromptStream`, an async
  iterator whose last item is `{ type: "prompt_result", stopReason }`. It keeps
  `.text`, `.events` and `.result`. One prompt at a time per session.
- `cancel()` sends `session/cancel` (the prompt ends `cancelled`) and answers
  any open permission question `cancelled`.
- Event `type`s: `message_chunk`, `thought_chunk`, `user_message_chunk`,
  `tool_call`, `tool_call_update`, `plan`, `usage` (context window
  `used`/`size`), `mode_changed`, `config_options_changed`,
  `available_commands`, `unknown`. Each has `sessionId` and `raw`.
- Updates that arrive while no prompt is running go to `onUpdate(event)` and
  `idleEvents`. Updates are per session, not per prompt: one sent just as a
  prompt starts lands in that prompt's stream.
- `onReadFile` / `onWriteFile` answer `fs/read_text_file` / `fs/write_text_file`
  (sync or async). The defaults (`WorkspaceFs`) refuse any path whose real path
  — symlinks followed, `..` resolved as the OS does — is outside the workspace,
  with JSON-RPC error -32602. Pass `false` to not offer the capability. Throw
  `new FsError(message, code)` from a custom handler to choose the error.
- Lines on incode's stdout that are not JSON-RPC are skipped and reported to
  `onProtocolError(line, error)` and `protocolErrors`.
- If incode exits, every pending call and the running prompt reject with
  `AgentCrashed` (`exitCode`, `stderr`).
- Other options: `loadSessionId`, `mcpServers`, `env`, `extraArgs`,
  `requestTimeoutMs` (default 120000). Raw access: `request()`, `notify()`.

## Errors

`IncodeError` (base; `stderr`, `exitCode`) → `TaskFailed` (`result`),
`IncodeTimeout`, `ProtocolError`, `AcpError` (`method`, `code`, `data`),
`AgentCrashed`.

## Tests

```sh
cd sdk/typescript
npm install          # once; needs the registry (typescript, @types/node)
npm test             # compiles src+test to dist-test/, runs node --test; offline
# plus the live test against a real binary (initialize + session/new only):
INCODE_BIN=/path/to/incode npm test
```

The suite runs against `../testing/fake_incode.py` through `python3` (override
with `PYTHON=/path/to/python3`).
