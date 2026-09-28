# incode — Architecture

**What this is for.** Every implementation task inherits this document, so an error
here is the most expensive kind in the system: it is copied into work that has already
been done by the time anyone notices. The PRD says *what* and *why*; this says *how it
is built* and, more importantly, **why it is cut this way** — the boundaries are the
part that a later agent will otherwise redraw by accident.

Measured against the working tree. Every claim cites a file.

---

## Names

Two products share this machine and must never share a name.

| | **incode** — this harness | **Incord Memory** — the separate memory product |
|---|---|---|
| Program | the command `incode` | the service/engine `incord-memory-service` |
| Per-project folder | `.incode/` (ledger, contracts, guards, agents, transcripts, checkpoints, context, findings, serve, team, desktop files) | `.incord/` (its own files: `RULE_ACK`, `MEMORY_FIRST_OFF`, `ALLOW_ONCE`, `PROJECT.md`, `backups/` …) |
| Machine-wide folder | `~/.incode/`, or wherever `INCODE_DIR` points | `%LOCALAPPDATA%\Incord Memory\`, and the spaceless copy at `%USERPROFILE%\.incord\` |
| Environment | `INCODE_*` (`INCODE_DIR`, `INCODE_AGENT`, `INCODE_TRACE`, …) | `INCORD_MEMORY_URL`, `INCORD_ENGINE_BIN` — incode reads these two to find the service, and they keep its name |
| MCP servers | `incode-desktop`, `incode-team` | its own, e.g. `incord-memory` |

The repository and the crate keep the name `incord-code` (`incord_code` in Rust
paths); only what incode writes, reads or shows is named `incode`. One incode-owned
name is still `incord`: the `_meta.incord` key on ACP updates (§2), which an external
client reads — renaming it is pending the owner's decision.

**Migration from the old names.** incode's state used to live in the project's
`.incord/` and in `~/.incord-code/`. At the start of a mode that reads the data
directory (the REPL, `--acp`, `-t`, `--desktop-mcp`, `--install-tools`,
`--tool-server`; never `--check-findings`, which checks and tests run as a
subprocess), when `INCODE_DIR` is not set, `~/.incode` does not exist
and `~/.incord-code` does, the old folder is **copied** to `~/.incode` through a
staging folder renamed into place, so a half copy is never taken for the real one,
and `~/.incord-code/MOVED_TO_INCODE.txt` records where it went; the old folder is
never deleted. Per project, on start and at each session start, every incode-owned
item still in `.incord/` whose target in `.incode/` does not exist is renamed into
`.incode/`, one atomic move per item; an item whose target exists is skipped (an
empty target folder gives way), and the memory engine's files stay in `.incord/`. `INCODE_TRACE` (area `migrate`) names
what moved. There is no fallback to the old `INCORD_*` variable names.

---

## 1. The one sentence

> **The harness is the hands. The memory service is the memory, the rules, and the
> judgement. Anything that looks like memory growing inside this crate is a defect.**

That is not a slogan — it is the rule that decides where every new module goes, and it
has already been enforced destructively once: 1,087 lines of local memory store, an MCP
server that exported it, and a second binary were deleted rather than improved.

---

## 2. What runs where

There are **three** processes and they are not peers.

| | The harness (`incode`) | The model (an installed CLI, headless) | The body (memory service) |
|---|---|---|---|
| **Owns** | the plan, the check, the guards, the ledger, the budget, the terminal, and every tool call (the `Toolbox`, §5a–§5b) | the model's reasoning and its conversation; codex and agy also still run their own tools (§5a) | recall, rules, the proxy, the auditor, the recorder, the owner queue |
| **State** | one workspace | one conversation, which it stores itself | every project, every agent on the machine |
| **Reached by** | the person, at a prompt, or an ACP client | spawned as a child process | HTTP on `127.0.0.1`, default port in `brief.rs`; the engine binary's hooks |
| **Lifetime** | a session | a turn | a machine |
| **Costs** | nothing | the operator's subscription | nothing |

**The harness uses the CLI as a model only.** It holds no subscription login: the CLI
authenticates itself, and incode decides what is worth doing, runs the tools the model
asks for, and checks whether the work was actually done. The one route where incode
talks to a model directly is the optional API route (`agent::api`, `INCODE_API_URL` /
`INCODE_API_KEY` / `INCODE_API_MODEL`): the key is read from the environment when a
request is made and never stored, and incode runs the tool loop itself.

Everything the harness knows that outlives a session, it asks for. It caches none of
it. A cache here is a second source of truth, and a second source of truth is the
defect above wearing a performance argument.

### Why HTTP, and where MCP lives now

Every body-facing module reaches the service over its plain HTTP API on `127.0.0.1`.

The harness used to carry its own MCP client, and attaching it to the service looked
obvious — it did not work, because that client was **stdio-only** while the service
registers over **HTTP**. incode now has an HTTP MCP client (`toolbox::mcp_http`), but
it serves one route only: the HTTP servers an ACP client names under `incode --acp`.
The body-facing modules still use the plain HTTP API.

MCP in incode today is three things, none of them the agent's own settings: the tool
bridge (`incode --tool-server`, §5a), the only MCP server a bridged claude is given
(`--strict-mcp-config`); `incode --team-mcp` and `incode --desktop-mcp`, handed to
codex, which is not bridged; and an ACP client's stdio and HTTP servers under
`incode --acp`, which incode connects to itself (`toolbox::mcp_client`,
`toolbox::mcp_http`) and runs as its own tools (below).

### `incode --acp`: incode as the agent an editor drives

An Agent Client Protocol client (an editor, or the owner's desktop app) starts
`incode --acp` and drives it with JSON-RPC 2.0, one message per line on stdin/stdout.
incode is the **agent** here, never the client: underneath, the work is still done by
the subscription CLIs, through the same `cli::prepare_session` the REPL builds its
session with and the same `cli::plan_turn` / `AgentSession::turn_observed` a typed line
runs. The code is `src/acp_agent/`; payloads are `agent-client-protocol-schema` types.

| Method | What incode does |
|---|---|
| `initialize` | protocol version 1; prompts take text, embedded context and images (`image: true`); `loadSession: true`; `sessionCapabilities` `close` and `list`; `mcpCapabilities` `http: true`, `sse: false` (stdio and Streamable HTTP servers); no auth methods. The client's `clientCapabilities.subagents` (the draft sub-agent extension, not in the schema crate) is read from the raw params: with it, workers are announced as sub-agents |
| `authenticate` | invalid params — no method was offered; the CLIs hold their own logins |
| `session/new` | `cwd` must be absolute and an existing directory; it becomes the workspace (made canonical by `prepare_session`). The session is built on its own thread, the client's stdio MCP servers are started, and the answer carries `modes` and `configOptions`; then `available_commands_update` lists its slash commands, and a message chunk says what each MCP server offered or why it did not start |
| `session/load` | a session kept in `cwd` (its transcript exists): built like `session/new` under the same id, its history replayed as `user_message_chunk` / `agent_message_chunk` BEFORE the answer (`modes`, `configOptions`); the model's own conversation is resumed where it can be (below) |
| `session/list` | the transcripts in `cwd` (id, title = the first person's line, `updatedAt` = the file's time), newest first, at most 200; with no `cwd`, every workspace an ACP session record names |
| `session/prompt` | one turn: text as `agent_message_chunk`, the model's reasoning as `agent_thought_chunk`, tool calls and results as `tool_call` / `tool_call_update` (a running command's output as `in_progress` updates first), an edit that landed as a `diff`, the task list as `plan`, the context window's fill as `usage_update`, each worker as a sub-agent; every step and the reply carry `_meta.incord`; replies with the stop reason, or a JSON-RPC error if the turn failed. Pictures in the prompt go to the agent leading the session, or the prompt is refused. A prompt that is a slash command runs that command instead; `/run` works the ledger and sends it as the plan, its lanes as sub-agents |
| `session/cancel` | trips the session's cancel token; the prompt answers `cancelled` |
| `session/set_mode` | `read-only`, `workspace-write`, `danger-full-access`; applied from the next turn |
| `session/set_config_option` | `agent` or `role` (both selects); answered at once with every option, applied before the next turn (or at once when the session is idle) |
| `session/close` | cancels the session's turn and frees its slot (at most 16 sessions open at once) |
| anything else | `-32601`; malformed JSON `-32700`; a line over 8 MiB is dropped unread and answered `-32600` under the id found in its first 4 KiB |

**stdout is the protocol and nothing else.** Before anything runs, `acp_agent::stdio`
takes the real stdin and stdout as private handles and repoints the process-wide
standard handles: stdout at stderr, stdin at the null device. Every render call, stray
`println!` and inherited child process therefore lands on stderr, and no child can read
the client's messages. A test drives a whole handshake and prompt through a real
process whose backend prints to stdout every way the crate can, and requires every
stdout line to be JSON-RPC.

**Edits as diffs.** A successful edit call's `tool_call_update` carries one `diff`
content block instead of the tool's output text (`acp_agent::present::Edits`). The
call and its result are incode's own on the bridged and API routes (a CLI that still
edits with its own tools reports its own); either way nothing is read at the moment
of the write: a splice
(`old_string` / `new_string`) is sent as the call carries it; a whole-file write
(`content`) is sent against what the file held before — the pre-turn snapshot the turn
already takes for `/undo` (`AgentSession::turn_observed` hands it to the observer as
`TurnStart`), or what the previous edit of that file in the same prompt left. A file
the snapshot shows did not exist has `oldText` null; a file whose before is not known
(snapshot capped or skipped, not text) gets its output, never a false "new file". The
path is absolute and must resolve inside `cwd`, or no diff is sent. Each side is cut at
`MAX_DIFF_TEXT_BYTES` (32 KiB, half of `MAX_TOOL_OUTPUT_BYTES`) and the cut is said.
Edits a backend does not report as a call with those keys (codex's file changes, a
shell command that writes) are not diffs.

**Usage.** `usage_update` carries `used` and `size` only when the agent reported both:
today the Claude CLI's result line (`usage.iterations`' last request for `used`,
`modelUsage[*].contextWindow` for `size`, as `AgentEvent::Context`). No other backend
reports a window, so none sends one. No `cost` is sent: on a subscription the CLI's
`costUSD` is a list-price figure, not money spent. Per-turn token counts would go on
the prompt result's `usage`, which the schema only has behind
`unstable_end_turn_token_usage`; it is not enabled.

**What a tool and a worker do while they run (`crate::watch`, `acp_agent::workers`).**
A turn's events say that a call started and what it returned. What happens in
between reaches a front end through a `watch::Watch` the session installs on its
backend (`AgentSession::set_watch` → `AgentBackend::set_watch` → `Toolbox::set_watch`;
carried into a backend switched in and into standbys): a running command's output,
what the guards or the engine decided about a call, the recorder's answer to a
`watchman_lookup`, and the workers a call or a run starts. The REPL installs none;
`--acp` installs `workers::SessionWatch`, and one `workers::Presenter` turns the
session's events and every worker's into updates.

- **Workers as sub-agents.** Only when the client advertised
  `clientCapabilities.subagents`. A worker is announced in its PARENT's session —
  the session, or the worker that started it — and its own conversation is sent
  under its own session id (`<session>/<kind>-<n>`), then its end in the parent's.
  The two updates are not in the stable schema, so they are written as the JSON the
  owner's app reads (`agentStore.ts`, inside an ordinary `session/update`):
  `{"sessionUpdate":"subagent_spawned","subagentSessionId","name","task","capabilities":{}}`
  and `{"sessionUpdate":"subagent_state_update","subagentSessionId","state"}` with
  `completed`, `failed` or `cancelled` (a tap dropped without an end says `failed`).
  Under its own id a worker's text streams as `agent_message_chunk` (its whole-block
  copy not repeated), its reasoning as `agent_thought_chunk`, its calls as
  `tool_call` / `tool_call_update` with their live output and diffs (a whole-file
  write with no "before" — a worker has no snapshot — is shown as its output). The
  workers are: the `Agent` tool's sub-agent (`toolbox::subagent`, named
  `<role> · sub-agent`; its events reach the tap from the child backend's own
  `run_turn` callback, and its own toolbox reports to the tap's watch), and each
  parallel lane of `/run` (`cli::run_ledger::LaneAgent`, named
  `<role> · lane <n>`, `n` counted per run; the tap is handed to its
  `AgentBackendWorker`, whose event callback feeds it, and the run's
  `runner::Progress::watch` is where the lanes get the watch). The team channel is
  not a worker: it has no process and no conversation, and what is said on it is
  said by a worker through its `team_*` calls, which appear under that worker.
- **Cancel reaches the workers.** The `Agent` tool's child gets the session's
  cancel token, so `session/cancel` kills its process tree within the bridge's poll
  and its tab says `cancelled`. A lane's backend gets the run's token too: a cancel
  now stops a running LANE at once (its work is sealed onto its branch when the
  lane is closed, and not merged), where the sequential task still finishes first.
- **Live command output.** A foreground `Bash` hands each piece of output on as the
  command prints it (`tools::bash::execute_bash_watched`, over the one bounded
  waiter); the toolbox shows the newest `watch::MAX_LIVE_OUTPUT_BYTES` (8 KiB) at
  most once per `LIVE_OUTPUT_EVERY` (500 ms), the cut said, as a `tool_call_update`
  with status `in_progress` and the text as its content (the content replaces the
  card's, as the protocol says, so the card shows the newest tail). The result then
  replaces it with the whole bounded output. The command is also stopped, with its
  tree, when the turn is. `BashOutput` is not streamed: it reads what a background
  job printed since the last read and returns at once, so its result is its output.
- **Thinking.** `AgentEvent::Thinking` → `agent_thought_chunk`. Only the claude
  route sends it: its partial messages carry `thinking_delta`s, and — measured on
  2.1.283 — every one of them is EMPTY unless the run asks for
  `--thinking-display summarized` (not in `--help`; in the CLI's option table,
  choices `summarized`/`omitted`/`highlights`), which incode now passes whenever
  partial messages are on. The API route does not ask its endpoint for thinking,
  and codex's reasoning items were not measured (no live turn is possible there).
- **Plan priority.** `TodoWrite`'s own priority, as before. The ledger has no
  priority field, so `/run`'s plan stays `medium` for every entry — none is invented.

**`_meta.incord` (B2).** Every `tool_call` and `tool_call_update` — the session's
and every worker's — and the answer to a prompt the model served carry
`"_meta": {"incord": {…}}` with only the fields incode knows; a field it does not
know is absent, never `null`, and with nothing known there is no `_meta`:

```jsonc
"_meta": {"incord": {
  "role": "plan",                 // the role the session's model was routed by (the picker), or the worker's
  "agent": "claude-code",         // the catalogue id serving it
  "model": "opus",                // the model ASKED for, when one was chosen
  "lane": "lane 2",               // a lane of /run
  "guard": {                      // on a result, when the guards or the engine decided
    "by": "guards",               // or "engine"
    "decision": "blocked",        // or "allowed"
    "rule": "tests/",             // the protected pattern that matched (guards)
    "path": "tests/probe.txt",    // the path it was about (guards)
    "reason": "…"                 // the engine's reason, cut at 500 characters (engine)
  },
  "watchman": {"hit": true, "observations": 5}   // on a watchman_lookup result
}}
```

`guard` is the last word before the call ran: a guard refusal; else the engine's
answer (allow, block, or a confirmation settled either way; an engine that could
not decide is reported as its refusal or its fail-open, with the reason); else,
with no engine, `allowed` by the guards for a call they look at (a change tool or
`Bash`). The permission mode's refusals and the person's answers are not in it.
`watchman` is what the recorder answered — its measured answer has `hit` and
`observations` and no token figure, so no `savedTokens` is sent. The prompt's
answer carries `role`, `agent` and `model` only; a slash command's and `/run`'s
carry none (the run is served by the lanes and the worker agent, not the session's
model). The owner's app does not draw these yet.

**Pictures in a prompt (A9).** `promptCapabilities.image` is true. An `image` block
is taken when its `mimeType` is `image/png` or `image/jpeg` and its bytes pass
`toolbox::Image::from_base64` — sniffed from the bytes, at most
`MAX_IMAGE_EDGE` px a side, `MAX_IMAGE_PIXELS`, `MAX_IMAGE_BYTES` — at most
`MAX_IMAGES_PER_RESULT` (4) per prompt; anything else refuses the prompt with the
bound it broke (`-32602`), and no turn runs. A picture with a slash command is
refused. The pictures are handed to the agent leading the session
(`AgentSession::attach_images` → `AgentBackend::attach_images`) BEFORE the engine
or the transcript hear the prompt, so an agent that cannot take them refuses the
prompt whole; a standby that takes the turn over does not get them. Per agent:

| Agent | How | Evidence |
|---|---|---|
| claude | the prompt goes on stdin as one stream-json user message — its text, then `{"type":"image","source":{"type":"base64","media_type","data"}}` per picture — with `-p --input-format stream-json` and no prompt on the argv | live, 2.1.283: a PNG of a red rectangle and a blue circle was described as exactly that; the CLI ended when stdin closed |
| API route | the user message's content becomes `[text, image…]`, the same image block a tool result's pictures use (`api::image_block`) | unit test on the request body |
| codex | each picture written to a file in the temp directory, passed as `--image <file>` right after `exec` / `exec resume` (before any other flag, so the multi-value list cannot swallow the prompt), removed after the turn | `codex exec --help` and `codex exec resume --help` (0.153.4) list `-i, --image <FILE>...`; NOT run live — every codex turn here answers with the usage limit |
| agy | refused, naming the agent | `agy --help` (1.2.11) has no image flag; its `--input-format stream-json` message shape is not measured |

The REPL takes one too: `/image <path>` (a file in the workspace, at most 32 MiB,
made to fit like a screenshot — `Image::fitted`) goes with the next message, or is
refused by an agent that cannot take it.

**Slash commands.** `/undo`, `/plan`, `/act`, `/diff`, `/links`, `/roadmap`, `/run`,
`/doctor`, `/memories`, `/remember`, `/worktree`, `/agent`, `/agents`, `/roles` and
`/rewind` are announced (`acp_agent::commands::OFFERED`),
with the description and argument hint from `slash_commands::COMMANDS`. A prompt that
parses as a slash command runs the REPL's own handler inside `console::capture`, which
returns what the handler printed on that thread; that text is the reply, and the agent
is not run. `/plan` and `/act` also move the session to `read-only` / `workspace-write`
and send `current_mode_update`. Not offered: `/bg` / `/jobs` / `/kill` (shell jobs
outside the permission mode, with completion reported by the REPL loop), and the
terminal-session commands (`/quit`, `/clear`, `/resume`, `/model`, `/add-dir`,
`/forget`, `/help`, `/status` and the other information screens). Anything else
starting with `/` is answered with the list.

**`/run`: the ledger as a live plan (`acp_agent::run`).** The REPL's handler is now one
function, `cli::run_ledger::run`, which both front ends call: the REPL with a status
bar (`Watch::Terminal`), `--acp` with its own `runner::Progress` (`Watch::By`) inside
`console::stream`, which hands every write the run makes on the session's thread to the
client as it is made (whole lines as `agent_message_chunk`). The runner now also tells
its `Progress` the whole ledger (`Progress::ledger`) at the top of every round and when
a task is claimed; `--acp` sends it as a `plan` — one entry per task, in ledger order,
the task's title as the text, `completed` for `Done`, `in_progress` for `Running`,
`pending` for every other state (`Blocked`, `Abandoned` and `Cancelled` say so in the
text); every entry is `medium`, because a ledger task has no priority. A plan is sent
only when it changed. The app shows it as "Plan · n of m". `session/cancel` trips the
session's token, which the run looks at between tasks — where Ctrl-C and the
interface's stop file (still watched) land — so the task already running finishes and
is verified first, and the prompt then answers `cancelled` — except a parallel
lane, whose agent is stopped at once (see "Cancel reaches the workers" below). A
run is refused in a `read-only` session: every task changes files.

**The pickers (`acp_agent::config`).** `configOptions` holds two selects, in the flat
shape the owner's app reads: `agent` — the catalogue's usable agents
(`Catalogue::usable_chain`), current = the session's lead; and `role` — the seven
`roles::KNOWN_ROLES`, each described with the operator's models for it, current `plan`
(the role `prepare_session` routes the session's model by), with the model the next turn
asks for in the option's description. A choice is kept on the reader and answered at
once; the session's thread applies it before the next turn: a new agent is built the
way the session's own was (`route_backend`) and put in with
`AgentSession::switch_backend`, which carries over the session's tools, roots, cancel
token, mode, prompter, memory id and context (the replaced agent becomes a standby
unless it is the API route; the new one starts a fresh conversation); the role then
routes the model with `AgentSession::use_role` (the operator's candidates first, then
the agent's own ladder, as the ledger's worker does). `config_option_update` is sent
whenever the options change for another reason — a choice applied (with the model it
settled on), or a standby that took over the session.

**The client's MCP servers (`toolbox::client_servers`).** The `mcpServers` of
`session/new` / `session/load` are NOT handed to the agent CLI: incode starts each
stdio server itself (`toolbox::mcp_client`, the client half extracted from the desktop's,
which now uses it too), lists its tools, and offers each through the Toolbox as
`mcp__<server>__<tool>` (names made `[A-Za-z0-9_-]`, at most 64 characters; the
server's `inputSchema` compiled once and checked on every call). Every such call goes
through `Toolbox::run_call`'s gate like any other: schema, guards, the permission mode,
the engine's `pre-tool`, the person to ask — with reach `run`, so it is refused
read-only, asked under `prompt`, and refused when the engine cannot decide (fail
closed). Bounds: `initialize` and `tools/list` 20 s each for a stdio server and 60 s
each for an HTTP one (`HTTP_START_BOUND`: the owner's memory service was measured at 1–15
s to list, more than 20 s cold), servers started in parallel; a call 120 s, abandoned at
once when the turn is stopped; at most 16 servers and 128 tools each; stdout lines 16
MiB. A server gets the filtered environment plus the client's variables, and runs in
the workspace. The servers are children of incode, held in `SessionTools`: when the
session ends they get EOF, 2 s, then their process trees are killed; one still
connecting then is dropped the moment it finishes.

*Connected in the background.* `session/new` does not wait for them: with the memory
service handed in, it took 42 s. `ClientServers::connect_in_background` starts each
server on its own thread and returns at once; the answer carries "MCP server `x`:
connecting in the background". Each server's tools join the session's `RwLock`ed list
the moment it has listed them, so the next `tools/list` offers them — the bridge's is
asked once per turn (claude is started per turn), the API route's every request. A
call routes to its server and runs without holding the list. What became of each
server (its tool count, what was left out, or why it was dropped at its bound) goes to
stderr (`incode acp: …`) at once, and to the client as an agent message: at once when
the session is answered and idle, else queued and said between turns
(`connected_later`), so it never lands inside the model's text or ahead of the session
id. **A turn does not wait** for a server still connecting: it runs without that
server's tools, and the model is told which servers are still connecting, by name
(`connecting_note`). Chosen over a short wait because the measured start is 1–15 s
and sometimes over 20 s, so no short bound would cover it, and the lifecycle hooks
already bring memory context into the turn; the tools arrive for the next turn.
Measured after the change: `session/new` with the memory service is 14.0 s, and 12.4 s
with no server at all — the rest is the session's own start, mostly the engine's
`session-start` hook (8.2 s), not the MCP servers.

*HTTP servers (`toolbox::mcp_http`).* An `http` entry is spoken to over MCP's
Streamable HTTP by the same `McpClient`, through a second `Link`: each JSON-RPC message
is one POST to the entry's `url` (`Accept: application/json, text/event-stream`), sent
from the shared runtime so the client's waits, bounds and stop work unchanged. The
answer is a JSON body (one message or a batch), an event stream read as it arrives
and let go at the reply (a server may hold it open), or `202` for a notification. An
`Mcp-Session-Id` the server names goes back on every later request, and a DELETE ends
the session when the tools are dropped. Refused: a url other than https, or plain
http to anywhere but this machine (`catalogue::key_may_travel_to`), before anything is
sent; any redirect (`runtime::http::no_redirect_client`, connect bounded at 10 s); an
answer past 16 MiB, declared or streamed. The entry's headers are sent as given and
written nowhere — not the trace, not a message, not `Debug` — and neither is the url.
A failure costs one request, not the server: an unreachable server, an HTTP error, or
an answer refused or unreadable becomes a JSON-RPC error for that request's id, since
a server behind a url can come back; an answer that never comes still marks the
server broken, as for a child. Each POST is capped at the call bound (120 s). SSE —
the older HTTP transport, a GET stream beside the POSTs — is not spoken, and
`initialize` says `sse: false`. The owner's app sends its Incord Memory server (`http`,
`<base>/v1/memory/mcp`, no headers) only when `http` is true, which it now is.

**Loading a session (`acp_agent::history`).** An ACP session always keeps its
transcript (`MemoryHooks::keep_transcript`, with or without an engine), and after each
turn a `session::Record` names the model's conversation, the backend holding it
(`agent`) and the ACP id (`incode_session`). `session/load` replays the transcript
(bounded at 2 MiB, newest kept, the cut said) and then: when the record's agent is the
agent now leading (and not the API route, whose conversation lived in the process that
ended), the model's conversation is resumed by id (`AgentSession::resume`, which the
claude bridge passes as `--resume`); otherwise the newest 24 KiB of the transcript
rides the next turn as context, marked as a replay. The memory engine's session-start
fires with `resume`.

**Permission requests (agent → client).** When the memory engine's `pre-tool` says a
call on incode's own Toolbox needs a person (see "The engine's hooks" below), the
client is sent `session/request_permission`: the session, a `toolCall` update carrying
the call's own id, a title (`<tool> <target> — <engine's reason>`), kind, status
`pending` and the raw input, and two options — `allow` (`allow_once`) and `reject`
(`reject_once`). The answer `{"outcome":{"outcome":"selected","optionId":…}}` allows
only on `allow`; `reject` or `cancelled` refuses. Outgoing requests are numbered
`incode-<n>` by the `Outbox`, registered before they are written, and the reader hands
each response to its waiter (`Outbox::resolve`); at end of input every waiter is
released and the call refused. The engine's `hook permission-request` races the
client; if it decides first, the client's request is withdrawn on this side only —
the protocol has no agent-side cancel the owner's app acts on, so its card stays until
answered and the late answer is dropped. `AskUserQuestion` is NOT sent to the client:
the owner's app answers every agent request other than `session/request_permission`
with `-32601`, so the model is told the question went unanswered, with its options.

Not offered: `session/resume` (stable in the schema; not needed by the owner's app,
which calls neither it nor `session/load`), SSE MCP servers, elicitation, and fs /
terminal.

**The workspace is made absolute once.** `prepare_session` canonicalises the workspace
(`file_ops::workspace_root`, the plain `C:\x` spelling, not `\\?\C:\x`), and
`file_ops::resolve_path` anchors a relative path to the process directory before it
normalises it — `incode -w .` refused `LS "."` and every write into a new top-level
directory as "path escapes the workspace", because `./.` normalised to an empty path
and the walk to the nearest existing ancestor ended at nothing.

### The engine's hooks, run by incode for its own tools

`hooks::engine` runs the memory engine's lifecycle hooks at the moment incode's own
Toolbox is about to act. Only calls that go through the Toolbox are covered: the API
route (`agent::api`) and the bridged claude route (§5a), with their sub-agents. codex
and Antigravity still run their own tools inside their own processes, so no call they
make passes this gate.

- **Binary:** `INCORD_ENGINE_BIN`, then `engine_bin` in `admin.json`, then the engine's
  install location — `%LOCALAPPDATA%\Incord Memory\incord-memory-service.exe`, which is
  what the engine's installer writes into Claude's and Codex's hooks, then the spaceless
  copy at `%USERPROFILE%\.incord\` it makes for Antigravity. No default elsewhere. A
  NAMED path that is missing is a spawn failure (the failure policy below); nothing
  named and nothing installed skips the hooks.
- **Call:** `<bin> hook <event>` (no agent flag: the engine's default contract), cwd the
  workspace, the event JSON written to a PIPED stdin that is then closed — never an
  inherited one, which under `--acp` is the protocol stream — stdout and stderr piped,
  the tree contained by `process_tree` and bounded by `tools::bash::wait_with_timeout_or`.
- **Bounds:** 610 s for `pre-tool`, `permission-request` and `capture` — the engine's
  own 600 s registration plus a margin, so the engine's
  deadline fires first. Not the operator hooks' 10 s. A stopped turn (the cancel token)
  ends a wait at once.
- **Order in `Toolbox::run_call`:** schema → the protected-path guards → the permission
  mode → the operator's pre-tool commands → `hook pre-tool` → snapshot → dispatch. A
  refusal at any earlier step never spawns the engine.
- **Replies:** exit 2 → refused, stderr is the reason (for `AskUserQuestion`, exit 2 is
  the owner's answer and returned as a NON-error result); exit 0 silent → run; exit 0
  `permissionDecision: allow` → run, and `additionalContext` is appended to the result;
  `ask` → the permission flow below; any other exit, a timeout, or a spawn failure →
  **fail closed for record, change and run tools, fail open for observe and session
  tools** (owner's decision; the reach of each tool is `toolbox::reach_of`, §5b — this
  began as Write/Edit/Bash only).
- **The permission flow (`Engine::settle`):** `hook permission-request` runs with the
  byte-identical `tool_input`, on a thread, while the session's `Prompter` asks here.
  The first answer wins and the loser is stopped (the hook's tree is killed; the prompt
  is told `settled`). Prompters: the REPL's terminal (`cli::prompt::Terminal`, one key,
  y/n, installed only when stdin and stdout are terminals; it borrows the keys and the
  screen from the status line through `render::PromptLease`), and the ACP client
  (`acp_agent::permission::ClientPrompter`). With no prompter (`--task`), only the hook
  can answer, and silence is a refusal carrying the engine's reason.
- **`AskUserQuestion`** (Toolbox tool, that exact name): goes through `pre-tool` like any
  call; not answered in the app (exit 0), it is answered by the prompter (the terminal's
  one-key menu from `ask::Question`), or reported unanswered with its options.
- **`code-edit`:** after a `Write`/`Edit` that LANDED, from `Toolbox::run_call`, on a
  thread of its own (not waited on, still bounded). A refused or failed write fires
  nothing.

### The engine's session lifecycle, fired by incode

`hooks::memory::MemoryHooks`, one per incode session, owned by `AgentSession` (REPL and
`--acp`) and by `task_run` (`--task`). It fires on every route — claude, the API, codex,
agy — because it is fired from incode's session and turn code, never from a CLI.

- **Session id:** incode's own — the ACP `sessionId`, or a v4 UUID the REPL / `--task`
  makes the same way. Every backend is told it (`AgentBackend::set_memory_session`), and
  a toolbox pins it (`Toolbox::pin_session`), so `pre-tool`, `permission-request`,
  `code-edit` and the lifecycle events all name one session. The model is told it too,
  at the top of the session-start block.
- **`session-start`** (60 + 10 s): asked in `prepare_session` (`startup`, or `resume`
  with `--resume`) on a thread of its own (`MemoryHooks::start_in_background`), so the
  session opens without it — it was 8.2 s of `session/new`'s 14. The first person's line
  collects it (`AgentSession::memory_prompt` → `settle_memory_start`, waiting at most
  the hook's own bound), before `user-prompt` is fired, so the engine hears the start
  first and the block rides that turn; a turn with no line (a continuation, `/run`), the
  session's end and `/clear` collect it too. `/clear` itself (`clear`, new id) still
  waits for the new one. Its block replaces incode's own rules, recorder and auditor
  blocks (Option A) in every backend's system context
  (`AgentBackend::set_system_context`); with no answer those stay. `--task` still waits
  for it in line: its one prompt is built from the block.
- **`user-prompt`** (60 + 10 s): in `plan_turn`, after the greeting short-circuit; its
  block replaces `rules::context_for_turn` for the turn.
- **Oversized blocks:** over 12 KB (start) or 6 KB (turn) the head is given inline and the
  whole written to `.incode/context/<session>-{start,turn}.md`, which the model can Read.
- **`capture`** (the stop, 610 s): at the end of every successful turn, with
  `last_assistant_message` and `stop_hook_active`. A `{"decision":"block","reason":R}`
  reply sends R to the same backend as the next prompt — a CLI resumes its session, the
  API appends to its conversation. At most `MAX_CONTINUATIONS` (3) per turn; not sent at
  all when the turn was stopped or while a `/bg` job runs.
- **`session-end`** (120 + 10 s): REPL exit (`prompt_input_exit`), `/clear` (old id,
  `clear`), the end of an ACP session's thread (`session/close` or the client going
  away — shutdown waits for it, up to the bound), and the end of `--task`.
- **Transcript:** the engine records a conversation only from `transcript_path`, so
  incode writes `.incode/transcripts/<session>.jsonl` in the engine's shape (`type`,
  `uuid`, `timestamp`, `cwd`, `message.content`; the model's text as text blocks), only
  when an engine is present. Both directories keep their newest 200 files.

---

## 3. Module map

Modules are cut by **which question they answer**, not by which layer they sit in. A
module here is a law plus the smallest amount of code that enforces it.

### The body-facing half — each one calls, none of them stores

| Module | Answers | Endpoint |
|---|---|---|
| `brief.rs` | "may work start on this project?" | `/v1/owner/proxy` |
| `rules.rs` | "what must this turn obey?" | `/v1/memory/rules/for` |
| `recall.rs` | "what do we already know?" | `/v1/memory/recall/{index,get}` |
| `knowledge.rs` | "save / search / correct" | `/v1/memory/{remember,search,messages}` |
| `recorder.rs` | "what has this project already solved?" | `/v1/memory/watchman/handoff` |
| `auditor.rs` | "what is already known to be wrong here?" | `/v1/memory/audit/findings` |
| `owner.rs` | "what does the person say?" | `/v1/owner/{enqueue,wait,history}` |

Every one of these **fails open** — an unreachable service costs context, never the
session — **except `owner.rs`, which fails closed.** That asymmetry is deliberate and
is the single most important line in this document: losing recall makes a turn worse,
while losing the owner channel and reading silence as approval is how both earlier
autonomous loops failed.

**`brief.rs` is the one that needed a third answer** — added 2026-09-21. The brief
gate fails closed for a good reason: no approved brief means no work, and treating an
unreachable store as "no brief" would be safe but wrong — it stops a run that a person
already approved, because of a network. Treating it as "brief is fine" would be the
silence-as-approval bug.

What actually happened: with the store down, every session on the machine was told to
complete its brief before writing code — including projects whose brief had been
finished for months. An outage in a service holding a *document* was undoing the
intake of every project on the machine.

So a **complete** brief is cached to `.incode/brief.cache.json` on every successful
read, and used only when the store cannot be reached. Two things make that honest:

- **A cached brief cannot go stale in a way that matters.** The only route to editing
  one is `save_brief`, which needs the same service that is not answering.
- **Only a complete brief is cached, and only a complete one is replayed.** Falling
  back to a half-finished brief would skip the intake it is in the middle of, which is
  the gate's whole purpose. An incomplete brief still stops the run, reachable or not.

The residual risk is an edit made from *another* machine since this one last succeeded.
That is why the fallback is traced rather than silent.

### The autonomous half — the laws

| Module | The law it enforces |
|---|---|
| `verify.rs` | **Nothing is done without a process that exited zero.** `Verified` has a private field and one constructor that refuses a non-zero exit. `CommandEvidence`'s fields are private too, and its fabricator is `#[cfg(test)]` — so a shipped binary cannot mint proof it did not earn. |
| `guards.rs` | **You do not get to edit your own exam.** Tests, CI, migrations, `.env` and the ledger are read-only to the agent — checked before dispatch for the harness's own tools, and by `Sentinel` (fingerprint before, compare after) for a delegated agent that brings its own. The second is detection rather than prevention, and does not revert. |
| `contracts.rs` | **A frozen interface is checked, not merely written down.** Runs inside `verify`, so a breach fails the same exit code as a broken test. |
| `ledger.rs` | **One edge into `Done`, and it demands proof.** Seven states, no `Failed` — a failure is a counter and a note. |
| `runner.rs` | **The runner decides what to do next; it never decides that something is done.** Nine stop conditions, each with a message — and a drained queue is not one of them until every roadmap goal's check has also passed (`RUNNER.md` §6a) and no task is left marked in progress. |
| `parallel.rs` | **An agent that cannot be isolated does not get a lane.** Every task a plan produces writes code, so two in one directory means `verify` proving a tree neither agent intended. A lane is a git worktree; no git, one lane. A merge conflict is never resolved — the branch is kept and the task goes back. |
| `team.rs` | **A question asked is not a question seen.** An ask never blocks, so the channel keeps an unanswered one in front of whoever could answer it, and the run reports the ones nobody did. Each is a guess somebody made. |
| `budget.rs` | **Bound the action, never the measurement.** `spent()` keeps counting past the ceiling. |
| `roles.rs` | Which model serves which kind of work, with an ordered fallback and a warning when the reviewer shares the writer's lineage. |
| `findings.rs` | **Work that produces information is judged by its artifact, never by the build.** A research task writes a findings file at a path fixed before it ran, and the check reads that file. It proves the deliverable exists and is not a shrug; it does not, and cannot, prove the findings are right. See `RUNNER.md` §6d. |
| `wiring.rs` | **A capability nothing calls is not a capability.** Counts production mentions of every `pub fn`; a declaration mentioned only once is dead. Ratcheted, so the number can fall and never rise. See `CONVENTIONS.md` §13. |
| `permissions.rs` | **What the session may do reaches every place that does it.** A mode governs every tool call *and* the desktop: `-p read-only` means the agent may look at the screen and not touch it. The policy applies in `Toolbox::run_call` for every tool (§5b), and again at `desktop::mcp::call_tool` in the desktop's executor. |
| `hooks.rs` | **The operator can put their own command in front of an action.** Pre-tool hooks run before every Toolbox tool (the desktop's in its executor) and refuse with exit code 2; post-tool hooks run only for calls that happened and cannot deny. Bounded at 10s, because a hook runs between the agent asking and the screen moving. `hooks/engine.rs` is the other half: the memory engine's own hooks, run before each Toolbox call with a piped stdin and the engine's 600 s bounds — see §2, "The engine's hooks". |
| `admin.rs` | One settings file, one precedence ladder, every value able to say which layer decided it. |
| `research.rs` | A dependency the build links and the document never argued for is a decision nobody recorded. |
| `identity.rs` | **The agent is the identity; the model is an attribute of the attempt.** A fallback changes what answered without changing who asked, so an identity built on the model would split one worker's history the first time a provider was busy. |

### The mechanical half

`agent/` discovers an installed agent and drives it as a model — `claude_code.rs`
speaks its stream protocol and opens the tool bridge per turn, `api.rs` is the API
route's own tool loop, `catalogue.rs` is the one list of agents, `worker.rs` hands one
ledger task at a time. `cli/` is the terminal, and `cli/agent_turn.rs` is one
interactive turn. `toolbox/` is the tool layer: every tool a model can call, behind
one gate (§5b). `tools/` is what those tools and the harness do to a workspace — run
the check, search, read documents, snapshot a tree so a turn can be undone, read the
git context, report what is installed. `runtime/` holds two error types, the token
counter and the cancel flag.

`tool_bridge.rs` (`incode --tool-server`) is how a bridged model reaches the toolbox
(§5a). `desktop/` and `team_mcp.rs` are MCP servers that run as separate processes
(`incode --desktop-mcp`, `incode --team-mcp`) so a crash in either cannot take a live
session with it. Their tools are now Toolbox tools on the bridged and API routes; the
servers are handed directly only to codex, which is not bridged. See
`COMPUTER_USE.md` and `TEAMWORK.md`.

**What used to be here and was removed:** an earlier agentic loop
(`ConversationRuntime`), its provider clients (`provider/`), its tool dispatch and
permission tiers, and its compaction — around 12,000 lines, deleted when the harness
first delegated everything to the installed agent. The tool layer and a smaller API
loop have since come back in a different shape (§5a, §5b), because the models-only
rule needs incode, not the CLI, to run every tool; compaction has not.

---

## 4. Data flow

### Session start — once
```
workspace ─┬─► brief::check ────────► intake task, if the brief is incomplete
           ├─► research::unvetted ──► dependencies the PRD never argued for
           ├─► auditor::context ────► open findings for these files
           ├─► recorder::context ───► what this project already solved
           └─► admin::resolve_* ────► model, roles, ceiling, verify command
                                       │
                                       └─► system prompt
```

What waits in line, and what does not (`cli::prepare_session`): the spec note (a memory
search on the 1.2 s waiting deadline) and the owner's guard categories
(`/v1/memory/enforcement`, same deadline) are each started on a thread as soon as their
inputs exist and collected after the agent catalogue, each put back at its place in the
prompt — as the recorder and auditor lookups already were. The catalogue's own probe of the machine is reused
for the fleet (it was probed twice, 0.7 s each). The engine's `session-start` is not
waited for at all (below). Measured live under `--acp` (debug build, no MCP server, the
owner's memory service): `session/new` 14.1 / 11.1 / 12.8 s before, 5.9 / 2.7 / 2.7 s
after; the 5.9 s run had the service's `rules` answer take 2.8 s.

**The enforcement request that "failed".** Traced before as `guards: could not be
loaded: … error sending request for url (…/v1/memory/enforcement)` after 1.2 s. The route
exists and incode's call is right (curl: `200` with the categories). With the cause chain
now in the error (`runtime::http::error_chain`) the trace says `operation timed out`:
the service answered after the 1.2 s deadline. Probed with an idle gap before each
request, the service took 0.2–14 s on first requests to this and to other routes
(`/v1/memory/rules`, `/v1/memory/settings/projects` — once all three together 49 s),
and 10–30 ms right after. It is the service's latency, not a missing route; incode no
longer waits for it in line.

### Every turn
```
the line the user typed ─┬─► rules::context_for_turn ──► the owner's standing rules
                         └─► recall::context_for_turn ─► what memory holds about it
                                       │
                                       └─► dynamic context (rules FIRST)
```

With the memory engine present, its `user-prompt` block replaces
`rules::context_for_turn` for the turn, and its `session-start` block replaces the
rules, recorder and auditor blocks at session start (§2, "The engine's session
lifecycle").

Rules go before recalled context because they govern how everything else is treated,
and the block says in words that rules **are** instructions while everything else
retrieved is data. A model that treats all injected text alike either obeys the wrong
thing or obeys nothing.

### A task, inside `/run`
```
ledger.next_ready
   │
   ├─ no check? ──► owner::past_verdict ─► already settled? use it
   │                     └─ no ──► owner::ask ──► wait ──► verdict
   │                                                │
   │                        approve → set aside (NOT done) · change → requeue with
   │                        the instruction · dismiss → cancel · hold → skip · info
   │                        or silence → stop
   │
   └─ has a check ──► worker.work ──► verify::verify ──┬─ exit 0 AND contracts hold
                                                       │     └─► Verified ─► Done
                                                       │            └─► journal.record
                                                       └─ otherwise ─► record_failure

   worker.attribution ──► task.worked_by          (every attempt, pass or fail)
```

**Who did it is recorded on every attempt, not only on the ones that worked.** A
failure with no author hides the pattern the record exists to expose: which worker
keeps producing work the check rejects. The identity is `backend:role` plus a stable
fingerprint; the model that actually billed is written beside it, taken from the
backend's own usage record rather than from the model that was *requested* — those
two disagree exactly when a fallback fires.

**The journal sits on the far side of the one edge into `Done`.** Unproved work is
never written to memory as finished, and a store that is unreachable costs the note
and never the work.

### Which agent — the catalogue

`agent::catalogue` builds one list of every agent the session could use, once at
session start: local CLIs found on the PATH, the API configured by `INCODE_API_URL` /
`INCODE_API_KEY` / `INCODE_API_MODEL` (id `api`), and the agents each API provider in
`admin.json` lists about itself. Every entry is marked usable, or not usable with the
reason. `/agent` prints it; `/agent <id>` saves the lead (`agent`) and
`/agent sub <id>` the sub-agents (`sub_agent`), both applying from the next `/run` and
the next session.

- **Lead**: `INCODE_AGENT`, else `admin.json` `agent`, when usable; else the first
  usable local agent; else a usable API agent.
- **Sub-agents** (the agent that works `/run`'s tasks, sequential and in lanes): the
  saved `sub_agent` when usable; else the lead's entry in the table beside
  `registry::KNOWN_AGENTS` (every agent maps to itself today); else the lead.

#### The provider contract

```json
"api_providers": [{"name": "team", "url": "https://agents.example", "key_env": "TEAM_API_KEY"}]
```

`key_env` is the NAME of an environment variable; the key is read from it when needed
and never written to disk or printed.

- **Request**: `GET {url}/v1/agents` (trailing `/` on `url` removed) with
  `Authorization: Bearer <value of key_env>`, finished within 3 seconds
  (`catalogue::PROVIDER_TIMEOUT`). Providers are asked one after another.
  The url must be `https`, or plain `http` to this machine only (`localhost`
  or a loopback address); any other url is refused before the key is sent.
- **Reply**: a 2xx status and `{"agents": [{"id": "coder", "name": "Coder", "model": "model-1"}]}`.
  `agents` is required and may be empty. `id` is required, non-empty, free of
  whitespace and control characters, and unique in the reply; `name` and `model` are
  optional. Unknown fields are ignored. Control characters are stripped from `name` and
  `model`, and both are cut to 80 characters.
- **Catalogue id**: `<provider name>/<id>`, so a provider agent can never shadow a
  local one.
- **Failure**: a missing key variable, an unreachable provider, a non-2xx status, or a
  reply that breaks the rules above lists the provider as ONE entry, not usable, with
  the reason.

Provider agents are listed but not driven yet: the contract does not say where an
agent's turns go, so each is marked not usable ("driving provider agents is not wired
yet") rather than routed to an endpoint nothing is known to answer.

---

## 5. On-disk state

| Path | Owner | Contents |
|---|---|---|
| `.incode/ledger.json` | **machine** | per-task execution state |
| `.incode/contracts.json` | human approves | frozen interfaces |
| `.incode/guards.json` | human sets once | extra protected paths |
| `.incode/team.jsonl` | **machine**, append-only | what agents asked each other, and who answered |
| `.incode/desktop.jsonl` | **machine**, append-only | what an agent did to the screen, streamed to `--serve` |
| `.incode/desktop.stop` | **human** | its existence halts the desktop; nothing parses it |
| `.incode/serve.token` | machine, per `--serve` start | the token a write to `--serve` must carry; readable by anything in the workspace, agents included |
| `.incode/run.stop` | **human**, via `--serve` | a stop request; a running `/run` consumes it and stops, one found at `/run` start is cleared unobeyed |
| `.incode/findings/<task>.md` | **agent** | one research task's answer, and the thing its check reads |
| `.incode/brief.cache.json` | machine | the last complete brief, so an unreachable memory service does not stop a run |
| `.incode/checkpoints/<session>/` | machine | each turn's "before", so `/undo` and `/rewind <n>` survive a restart (bounded: newest 20 turns, ≤ 256 MiB; a manifest naming a path outside the session's roots is not trusted) |
| `.incode/agents/*.md` | **human** | the project's user-defined sub-agents (`~/.incode/agents/*.md` for every project); see §5b, "User-defined sub-agents" |
| `.incode/transcripts/<session>.jsonl` | machine | the session's transcript in the memory engine's shape, when an engine is present or the session is an ACP one; newest 200 kept |
| `.incode/context/` | machine | an oversized `session-start` / `user-prompt` block, written whole; newest 200 kept |
| `<repo-parent>/worktrees/` | machine | one checkout per lane, removed after; the **branch is kept** |
| `~/.incode/admin.json` | human sets once | model, roles, ceiling, check |
| `~/.incode/sessions/<workspace>.json` | machine | session pointers (the agent's conversation id, the model), one file per workspace — not transcripts |
| `~/.incode/tools/<name>/` | machine, when the owner asks | an installed external tool: `<version>/` (the unpacked release) and `installed.json` (version, asset, sha256, source); see §5b, "External tools" |

These lived in `.incord/` and `~/.incord-code/` before the rename; see "Names" at the
top for the one-time migration, and for the memory engine's own files in `.incord/`.

Finished work is also written to the **shared memory** every agent on this machine
reads, under the subject `<project>#<agent-fingerprint>` — which turns "what has this
worker done?" into a lookup instead of a scan of prose. It is a record, never a source
of truth: nothing reads it back to decide whether a task is done.

**The ledger is machine-owned and protected from the agent** (`guards.rs`). Hand-edited
and machine-owned state diverge quietly, and debugging that divergence is miserable —
so scope changes go through the brief, and the ledger is derived from it.

**What is deliberately NOT on disk:** decisions and audit findings. Both already live
in the body, where they are searchable, shared between agents, and carry a lifecycle a
flat file cannot. Writing them here would be the duplicate-memory defect returning in a
different costume. Export them at the end if a portable copy is wanted; do not fork
them at the start.

---

## 5a. Models only: incode runs every tool

The owner's rule: incode IS the harness. An installed agent CLI is used only as the
model, headless. Its built-in tools, its hooks, its permission prompts and modes, its
edit events and its settings are not relied on. Every tool call the model makes runs in
incode's own `Toolbox` (`src/toolbox/`), so the guards, the memory engine's pre-tool
hook, the permission mode, the Allow/Reject flow, the cancel token and the observer that
draws the call apply the same way to every model.

### The bridge

```text
incode (parent) ──spawns──▶ agent CLI (the model; its own tools off)
     ▲                           │ starts, from --mcp-config
     │ 127.0.0.1 + turn token    ▼
     └──────────────────── incode --tool-server
```

- `incode --tool-server` (`src/tool_bridge.rs`) speaks MCP on stdio to the CLI, through
  the framing shared with the team server (`src/mcp_stdio.rs`). It runs nothing: each
  `tools/list` and `tools/call` is forwarded to the parent over a loopback socket.
- The parent (`Bridge`, opened per turn by `src/agent/claude_code.rs` `run_turn`) binds
  `127.0.0.1:0`, makes a 256-bit token, and passes both to the CLI through its
  ENVIRONMENT (`INCODE_TOOL_BRIDGE`, `INCODE_TOOL_TOKEN`), never its argv. The CLI's
  stdio servers inherit that environment (measured live).
- Every request carries the token, compared in constant time. A request is bounded in
  size (16 MiB) and arrival time (10 s); at most 8 are answered at once. A request after
  the turn was stopped or ended is refused without reaching the toolbox. The server half
  refuses to send its token to a non-loopback address.
- In the parent, each call is run on the thread that owns the turn, through
  `Toolbox::run_call`, and is reported as incode's own `ToolUse` / `ToolResult` events —
  so the REPL panels, `incode --acp` tool-call updates and diffs come from incode, not
  from the CLI's stream. The CLI's own `tool_use` / `tool_result` blocks are no longer
  read.
- The permission mode is incode's decision: the session's `plan` / `default` /
  `acceptEdits` is read back into read-only / prompt / workspace-write and enforced in
  `Toolbox::run_call` before the engine is asked. Nothing is handed to the CLI.

### Per CLI, as measured on this machine (2026-09-25)

**claude 2.1.282 — bridged.** Each flag was proved with a scratch MCP probe server and
a live `-p` run before it was relied on:

| Need | Flag | Evidence |
|---|---|---|
| No built-in tools | `--tools ""` | session `init` listed only `mcp__probe__Read`, `mcp__probe__Write`; asked for its tools it named only those two; the Write went to the probe. |
| Only incode's MCP servers | `--strict-mcp-config` + `--mcp-config` | `mcp_servers` held only the servers passed. |
| No hooks, settings or modes | `--setting-sources ""` | control run: 13 hook events (SessionStart, UserPromptSubmit, PreToolUse ×2, Stop) and `permissionMode: auto` from user settings; with the flag: 0 hook events, `permissionMode: default`. |
| No CLAUDE.md, no auto-memory | env `CLAUDE_CODE_DISABLE_CLAUDE_MDS=1`, `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1` | a canary in the workspace `CLAUDE.md` was repeated by the control run, unknown with these set. |
| No skills | `--disable-slash-commands` | `init` listed 18 skills without it, `skills: []` and `slash_commands: []` with it. Its agent list is still reported, but with no `Task`/`Agent` tool of its own nothing can start one. |
| Calls reach incode without a CLI prompt | `--allowedTools mcp__incode mcp__team …` | 0 `permission_denials`. |
| NOT usable | `--bare` | reads only an API key; with a subscription login it returned `Not logged in · Please run /login`. |
| Readable thinking (2.1.283) | `--thinking-display summarized`, with `--include-partial-messages` | without it a hard prompt streamed a `thinking` block whose deltas were all empty (`"thinking":""`, `estimated_tokens`); with it the summary arrived word by word as `thinking_delta`s, the reply unchanged. Not in `--help`. |
| A prompt with pictures (2.1.283) | `-p --input-format stream-json`, the prompt on stdin | one `{"type":"user","message":{"role":"user","content":[text, image]}}` line, stdin closed: the model described the picture, exit 0. |

Live, through incode: "create hello.txt containing hi" ran `Write` in incode's toolbox
(trace `tool Write <- {"path":"hello.txt","content":"hi"}`, engine pre-tool 3.2 s, exit
0); a `Write` to `tests/probe.txt` was refused by incode's guard and nothing was created;
under `incode --acp` the `tool_call` (`incode-N`, kind `edit`) and the diff came from
incode, with stdout still pure JSON-RPC.

What cannot be switched off: two built-in plugins still load (`agents-md`,
`telemetry`); the CLI still reports its own token usage and quota, which incode shows
because only the model's biller knows them.

**codex 0.153.4 — NOT bridged; its own tools still run.** Found in `codex exec --help`,
`codex features list` and the binary:
`--ignore-user-config` (hooks.json still loads — the control run warned about it),
`--ignore-rules`, `--disable shell_tool`, `--disable unified_exec`, `--disable view_image`,
`--disable multi_agent`, `--disable hooks`, `-c web_search="disabled"`, and
`-c mcp_servers.<name>.command/args/env` (an MCP probe registered this way received
`initialize` and `tools/list`). NOT proved: whether these leave the model with only MCP
tools — every live turn returned "You've hit your usage limit … try again at Oct 8th,
2026", so no tool list could be observed. `apply_patch` and `update_plan` have no
feature flag or config key (only the model catalogue's `apply_patch_tool_type`); no way
to switch them off was found. Until both are proved, codex keeps its own tools, the
Sentinel remains detection-only there, and its edit events still feed the diff.

**agy 1.2.11 (antigravity) — NOT bridged; cannot be.** `agy --help` has no flag to
remove or restrict its built-in tools, no per-run MCP configuration (only a persistent
`agy mcp add`, which edits the operator's machine), and no way to skip its settings. A
live run listed its tools as `view_file, run_command, manage_task, send_message,
schedule, invoke_subagent, define_subagent, manage_subagents, write_to_file,
replace_file_content, generate_image, read_url_content, search_web, ask_question,
call_mcp_tool, list_resources, read_resource`. It stays on its own harness, with the
Sentinel as detection.

### Known limitations on the bridged route

- `--add-dir` roots: **fixed (phase 4).** The toolbox acts inside an added root when a
  file tool names an absolute path in it, under the workspace's guards (matched on the
  path within the root); the intake gate stays the workspace's own.
- The team channel: **fixed (phase 5)** — `team_ask`, `team_tell`, `team_answer` and
  `team_read` are toolbox tools (`toolbox::team`, over `team_mcp::Server`), and the
  separate server is no longer handed to a bridged model. It is kept for codex, which
  still reaches the channel through `mcp_overrides_for`.
- The desktop: **fixed (phase 6b)** — its twelve tools are toolbox tools, reached
  through the bridge; `incode --desktop-mcp` is now only their executor, started by the
  toolbox. No desktop server is handed to the CLI. See §5b, "The desktop".
- The engine's session-capture hooks: **fixed (phase 4)** — incode fires them itself on
  every route; see "The engine's session lifecycle, fired by incode" in §2.
- Project instruction files: **fixed (phase 4)** — `instructions::project_block` reads
  `CLAUDE.md` and `AGENTS.md` from the workspace root (8 KB each, the rest pointed at)
  into the session's system context for every model, and for `--task`.
- A prompt over the command-line budget now loses its MIDDLE, not its end: the
  instruction is last in every prompt incode assembles (`budget_for_spawn`).

---

## 5b. Tools

Every tool a model can call, with the two rules `Toolbox::run_call` applies from ONE
table, `toolbox::reach_of`:

- **Mode.** `observe` and `session` tools run under every mode, never asked about. Every
  other reach is refused in a read-only session (`plan`) and asked about, call by call, in
  a prompting one (`default`); the writing modes leave it to the guards and the engine.
  `Agent` is also refused under read-only and prompt, because the agent it starts would
  not carry the mode.
- **Engine failure.** When the memory engine's pre-tool hook cannot decide (missing where
  named, crashed, past its bound), `record`, `change` and `run` tools are REFUSED
  (fail closed); `observe` and `session` tools run, traced (fail open).

A name missing from the table is refused as no tool at all, before anything is asked.
Then, still before the guards, the mode or the engine, the call's input is checked
against the schema its own tool advertises (`toolbox::validate`, jsonschema; the
schemas are compiled once per process from the full catalogue): a malformed call runs
nothing and comes back as an error listing every violation with its JSON path.
`ConvertDocument` is offered only where LibreOffice is found, and a call to it anywhere
else is refused before validation.
`change` tools are also checked against the operator's guards for EVERY file they touch
(`Toolbox::changed_paths`), snapshotted file by file for `/undo` when a checkpoint store
is attached, and reported to the engine's `code-edit` hook once they land (once per file
for a patch). The ACP kind is `acp_agent::present::tool_kind`.

| Tool | Reach | ACP kind | Bound | Notes |
|---|---|---|---|---|
| `Read` | observe | read | documents: `MAX_DOCUMENT_BYTES` 32 MiB in, `MAX_DOCUMENT_READ_CHARS` 60 000 out; archives `MAX_UNZIPPED_BYTES` 96 MiB inflated, `MAX_ZIP_ENTRIES` 10 000; tables `MAX_TABLE_ROWS` 200, `MAX_TABLE_COLUMNS` 40, `MAX_SHEETS` 30; `MAX_SLIDES` 500; PDF 60 s | a document comes back as markdown, paged by `offset`/`limit` (`tools::documents`): PDF (pdf-extract, on its own thread under `catch_unwind`), xlsx/xlsm/xlsb/xls/ods (calamine, a table per sheet), docx, pptx (slides in presentation order, with speaker notes) and odt (one zip + quick-xml walker). HTML, CSV and TSV stay source unless `render` is set. The zip budget is counted on bytes actually inflated, not the sizes the archive states. |
| `Write`, `Edit` | change | edit | — | |
| `MultiEdit` | change | edit | `MAX_EDITS` 100, report `MAX_CHANGE_REPORT_CHARS` 30 000 | all or none, in memory first |
| `ApplyPatch` | change | edit | `MAX_PATCH_FILES` 50 | unified diff; add/modify/delete; every hunk checked before any write; a failed write puts back the files already written; renames and binary patches refused |
| `NotebookEdit` | change | edit | `MAX_NOTEBOOK_BYTES` 32 MiB | replace / insert / delete a cell by id or index; written back as 1-space-indented JSON |
| `LS` | observe | search | `MAX_LS_ENTRIES` 500 | the shared walker at depth 1; what it skipped is NAMED (up to 20, the rest counted); `ignore` patterns from the model |
| `Glob`, `Grep` | observe | search | Glob `MAX_GLOB_RESULTS` 500 (rest counted); Grep `MAX_GREP_MATCHES` 100 matching lines (says it stopped), 300 chars a line, 30 000 chars, files over 50 MiB counted not searched | ripgrep's engine (`tools::search`). One walker for Glob, Grep and LS: `.gitignore`, `.ignore`, `.git/info/exclude` with `require_git(false)`, hidden files, `node_modules` and any directory holding `CACHEDIR.TAG` skipped. The model's glob is applied with globset AFTER the walk (an override or type filter re-admits hidden files — measured); no `/` matches a name at any depth, a `/` anchors to `path`. Grep: context lines, case-insensitive, a matching binary file reported without its bytes, non-matching binaries counted, BOM-marked UTF-16 transcoded. |
| `Outline` | observe | read | `MAX_OUTLINE_SYMBOLS` 400, `MAX_OUTLINE_BYTES` 4 MiB, 10 s parse | tree-sitter queries for rs, ts/tsx, js/jsx/mjs/cjs, py, go (`toolbox::outline`); kind, name, signature and line range, nested; a syntax error's line is reported; another language is refused by name. Each grammar is a cargo feature (`lang-rust`, `lang-ts`, `lang-python`, `lang-go`, `lang-js`), all default. |
| `ConvertDocument` | change | edit | `CONVERT_DEADLINE` 120 s, whole-tree kill | OFFERED ONLY when `soffice` is found: `INCODE_SOFFICE` (alone, when set), then `PATH`, then the platform's install paths. Headless, `--norestore --nologo --nodefault --nolockcheck`, a private `-env:UserInstallation` profile in a temp dir, PDF made there and copied into place; the guards and the checkpoint see the OUTPUT path. The conversion itself has run only against a fake `soffice`: LibreOffice is not installed on the development machine. |
| `Bash` | run | execute | output 30 000 chars; timeout; live view 8 KiB per 500 ms; shell version probe 10 s, once per process | `run_in_background` starts a session job (`MAX_RUNNING_JOBS` 8). In the foreground its output is handed to the watch as it prints (`--acp`: `in_progress` updates), and the command's tree is killed when the turn is stopped. The shell is `tools::shell_choice`'s (below), and the tool's description names it |
| `BashOutput` | observe | read | `MAX_JOB_READ_BYTES` 30 000 per read; `MAX_JOB_BUFFER_BYTES` 1 MiB kept per stream | new output since the last read; loss past the buffer is counted. Returns at once, so nothing is streamed |
| `KillShell` | run | execute | — | kills the job's process tree |
| `TodoWrite` | session | think | `MAX_TODOS` 100, `MAX_TODO_CHARS` 500 | kept per session; `--acp` sends it as the plan, with its priority, on the call's successful result |
| `Diagnostics` | run | execute | `PROJECT_CHECK_TIMEOUT` 300 s, `MAX_PROJECT_FINDINGS` 50, `MAX_DIAGNOSTIC_FILES` 50 | `INCODE_CHECK_CMD`, else `cargo check` / `tsc --noEmit` |
| `WebFetch` | observe | fetch | 20 s, 512 KiB read, 20 000 chars shown; rendered: 20 s + 10 s kill grace, 2 MiB read | private addresses refused by name and by resolution; carries no credential; an HTML answer becomes markdown through Read's HTML conversion (readability when the page reads as an article, the whole page less nav/scripts otherwise), anything else is returned as sent. Where the headless browser is found: `render_js` runs the page's script first, and a page that comes back as an app shell (script, under 200 characters of text) is rendered on its own; `render_js` is not advertised where there is no browser |
| `Screenshot` | observe | other | `PAGE_DEADLINE` 60 s, load 30 s, settle 3 s; viewport 200–3840 x 200–2160 CSS px; the image bounds below | OFFERED ONLY when the headless browser is found. A public `url` (WebFetch's address rules) or an HTML/SVG `path` in the workspace; `width`/`height`, `scale` 1 or 2, `full_page` (clipped at `MAX_IMAGE_EDGE` px tall, said so). The answer is an image block. See "The headless browser" below |
| `RenderPdf` | change | edit | `PDF_DEADLINE` 120 s, whole-tree kill | OFFERED ONLY when a printer is found (`pdf_renderer`, below). A public `url` or a workspace HTML/SVG `path`, to `output` (must end `.pdf`); the guards and the checkpoint see `output`; the engine's `code-edit` is told it. Printed in a private temp profile and copied into place only when it exists |
| `screen_info`, `screenshot`, `ui_tree`, `cursor_position` | observe | other | a call 120 s; the image bounds | OFFERED ONLY when the desktop is switched on (`INCODE_DESKTOP`). Executed by the desktop child — see "The desktop" below. `screenshot` answers with an image block |
| `click_element`, `type_text`, `paste_text`, `key`, `scroll`, `drag`, `move_mouse`, `click` | run | other | a call 120 s | the same; refused read-only, asked under prompt, refused when the engine cannot decide |
| `WebSearch` | observe | fetch | 15 s, 512 KiB, 8 results | provider from `admin.json` `web_search` — contract in `web::search_with` |
| `Skill` | observe | other | — | |
| `Agent` | run | other | depth 2 | on the model endpoint when one is configured; otherwise on the agent CLI this toolbox's model runs in (`Toolbox::run_subagents_on`, set by the claude backend): a second headless run of it as a model only, its own toolbox one level down, the session's mode, guards, engine, person to ask, cancel and memory session id, its role routing its model (the operator's candidates, then the CLI's own ladder). Neither: refused. A worker — see "What a tool and a worker do while they run" |
| `AskUserQuestion` | session | other | — | |
| `verify` | run | other | 600 s | |
| `recall_index`, `recall_get`, `memory_search`, `find_symbol`, `code_skeleton`, `git_blast_radius` | observe | other | 8 s | the memory service |
| `watchman_lookup` | observe | search | 8 s, 20 000 chars | `POST /v1/memory/watchman/lookup`; project named by `recorder::task_for` |
| `remember`, `forget_memory`, `save_brief` | record | other | 8 s | |
| `team_ask`, `team_tell`, `team_answer` | record | other | — | speaks as the harness's identity for the task |
| `team_read` | observe | other | `team::SHOWN` | |

Changed by this table (phase 5): `remember`, `forget_memory` and the team verbs used to
run under every mode and fail open; as records they are now refused read-only, asked
under prompt, and refused when the engine cannot decide.

`WebSearch` configuration: `"web_search": {"url": "https://…", "key_env": "NAME"}` in
`admin.json`. Only the variable's NAME is stored. The key is sent as a bearer only over
https or to loopback (`catalogue::key_may_travel_to`), no redirect is followed, and an
unconfigured search says so. The former `INCORD_SEARCH_URL` / `INCORD_SEARCH_KEY`
variables are gone — the second was also sent, as a bearer, on every `WebFetch`.

### Pictures in a result (phase 6b)

`Toolbox::run_call` answers with a `ToolOutput` (`toolbox::image`): the text, plus
`Image`s — PNG or JPEG, each checked from its own header when it is made. Every text
tool's answer is unchanged: its text, no images.

| Layer | What it carries |
|---|---|
| The bridge (MCP) | the parent sends the result as MCP content; the server half re-checks each picture and hands the CLI `{"type":"text"}` then `{"type":"image","data","mimeType"}` (`Handler::call_content` in `mcp_stdio`) |
| The API route | a text answer is the plain `content` string it always was; an answer with pictures is `[{"type":"text"}, {"type":"image","source":{"type":"base64","media_type","data"}}]` (`agent::api::tool_result_message`) |
| incode's events, transcript, engine | `ToolOutput::shown()`: the text, then `[image 1280x720 png]` per picture — never the pixels |
| ACP | the same text and placeholder. The owner's app (`incord-dashboard` `ChatView.tsx` `ToolBody`) draws only `text` and `diff` tool content, so no image block is sent to it |

Bounds (`toolbox::image`): `MAX_IMAGE_BYTES` 3 750 000 encoded (5 000 000 as base64),
`MAX_IMAGE_EDGE` 8 000 px, `MAX_IMAGE_PIXELS` 16 777 216, `MAX_IMAGES_PER_RESULT` 4. A
picture incode drew itself (Screenshot) is made to fit — scaled, then JPEG at 85/70/50 —
and the answer says what was done; a picture from another process (the desktop child,
the bridge's other half) that does not fit is left out, with a line saying why.

### The headless browser (phase 6b)

The headless browser (`toolbox::browser`) is a headless browser installed as a tool (e.g.
`release: "incordai/incord-browser"`). It is found by `INCODE_BROWSER` (alone when set),
then `admin.json` `browser_bin` (alone when set) — both for development — then the
installed external tool whose entry says `"provides": "browser"` (`ext_tools::provider`;
see "External tools" below). `PATH` is never searched and no browser name is written in
the code. Not found: Screenshot is not offered (a call is told how to install one),
`WebFetch` runs no script and is advertised without `render_js`, and RenderPdf is offered
only with a system browser. The subcommands below are the upstream browser's CLI
(`fetch`, `serve`).

- **WebFetch with script:** `<browser> fetch <url> --dump html --quiet --timeout 20 -o
  <temp>`, one child per call, whole tree killed 10 s past its own timeout, 2 MiB read
  back, then `html_to_markdown`. The url passes `web::check_public` (by name and by
  resolution) before the browser starts. Kept on incode's own conversion rather than the
  browser's `--dump markdown`, although its tables are valid GFM since
  incordai/incord-browser#13: incode's is the same conversion a plain fetch and Read give
  (readability, navigation left out, links resolved against the page), it already writes
  a rendered table as a GFM table (test `a_table_built_by_script_reads_as_a_markdown_table`),
  and `--dump markdown` still puts a caption above the table when `caption-side: bottom`
  comes from a stylesheet (measured on 078786a). `--font-dir` is not passed to `fetch`:
  measured on 078786a, `--dump html` / `--dump text` output is byte-identical with and
  without it (script sees `getBoundingClientRect().width` 0 either way), while a page that
  queries layout pays the font load (449 ms vs 51 ms). It matters for `fetch -s`, which
  incode does not use; Screenshot's `serve` passes it.
- **Screenshot:** `<browser> serve --host 127.0.0.1 --port <picked> --quiet --font-dir
  <system fonts>`, started lazily, once per session, held in `SessionTools` and killed with
  its tree when the session's tools are dropped. Driven over DevTools (`toolbox::cdp`, a
  synchronous `tungstenite` client over loopback; every read has a deadline):
  `/json/version` → `Target.createTarget` → `attachToTarget` (`flatten`) → `Page.enable` →
  `Emulation.setDeviceMetricsOverride` → `Page.navigate` + its load event (then up to 3 s
  for `networkIdle`) → `Page.captureScreenshot` (full page: `getLayoutMetrics` +
  `captureBeyondViewport` + clip) → `Target.closeTarget`. A workspace file is opened in a
  SECOND instance started with `--allow-file-access`; the one that reads the web never is.
- **Never passed:** `--allow-private-network` (incode's address checks refuse loopback and
  private addresses first, and the browser's own block — measured: `Access to
  private/internal IP address 127.0.0.1 is not allowed` — stays on for everything a page
  loads), `--stealth`, `--user-agent`. The browser's own User-Agent is a generic one
  (measured: `Mozilla/5.0 (X11; Linux x86_64) … Chrome/145.0.0.0 Safari/537.36`).
- **Known limits.** The browser resolves the name itself after incode checked it, so the
  DNS-pinning WebFetch does is not available there; its own private-network block is the
  second line. A workspace HTML file opened with file access can reference other local
  files (an `iframe`, an `img`); incode confines the path it opens, not what that page
  loads.

**RenderPdf** (`toolbox::pdf`) has two printers. The headless browser prints over DevTools
`Page.printToPDF` (`printBackground`, Letter unless the page's `@page { size }` is
honoured). A system headless browser of the Chromium family — `INCODE_PDF_BROWSER` (alone
when set), then `PATH`, then the standard install paths, the x86 program directory first
on Windows — prints as `--headless --disable-gpu --no-pdf-header-footer
--user-data-dir=<temp> --print-to-pdf=<temp>`.

What the headless browser's print gives is MEASURED, once per session, never read off a version
(`browser::print_ability`, cached in the session's `Browsers`): a `data:` page carrying a
probe word is printed on the web instance (no file access, no network) with
`preferCSSPageSize: true` — again without it if that is refused with `-32601` — and its
text is read back through the same pdf-extract path Read uses
(`documents::pdf_text`). The result is `{text, css_page_size}`; a probe that fails is
kept too, with its reason. Measured on 078786a: text found, `preferCSSPageSize` honoured
(a `13.333in 7.5in` `@page` prints at 960x540 pt, as Edge's CLI print does); on
971d3e8 (before incordai/incord-browser#11 was fixed): no text, `-32601`.

`admin.json` `pdf_renderer`: `"auto" | "system" | "browser"`. `"auto"` (default) prints
with the headless browser when its probe found text, else with the system browser; if the
headless browser then fails on the page, the system browser prints it and the result says
why. With only one of them installed, that one prints. `"system"` (the OS's headless
Chromium-family browser) / `"browser"` (the configured browser tool): only that one. The
result names the printer as the owner configured it, never by a name written in the code:
the browser tool entry's `name` (`with browser`), a browser named by `INCODE_BROWSER` /
`browser_bin` by its file stem, the system browser by its file stem (`with msedge`). A
print by a headless browser whose probe found no text says the PDF is IMAGE-ONLY. Measured against Edge on the same article: 2 Letter
pages each, 1864 characters extracted vs 1884. The pages are still a raster with an
invisible text layer over it, not vector drawing (about 2.5x Edge's file size).

### External tools (`ext_tools`)

A program incode runs for the owner is an entry of `admin.json` `tools` (`name`,
`release` = `<owner>/<repo>` or `url`, `version`, `asset` or `assets`, `sha256`, `run`,
`provides`), downloaded from its release when the owner asks and never looked up on
`PATH`. The owner renames or replaces the browser by editing that entry; nothing in the
code names it.

- **Asked for, never automatic.** `/tools add <name>…`, `/tools update [name]`,
  `/tools remove <name>` (REPL and ACP, one handler: `ext_tools::command`) and
  `incode --install-tools` (`ext_tools::install_all`). A session that finds an entry not
  installed says so — a REPL info line, an ACP agent message beside the MCP lines, a
  `-t` stderr line — and downloads nothing.
- **Resolved before fetched** (`ext_tools::fetch`). GitHub's API
  (`releases/latest` or `releases/tags/<version>`) gives the tag, the asset's
  `browser_download_url`, its `size` and its `digest` (`sha256:<hex>`). The expected
  sha256 is that digest or the entry's pinned `sha256`; both present and different, or
  neither present, and the install is refused before a byte is downloaded. A `url` source
  has no digest, so it needs the pin.
- **Downloaded bounded.** Over the crate's shared no-redirect client
  (`runtime::http::no_redirect_client`), redirects followed here one hop at a time, each
  address checked by `agent::catalogue::key_may_travel_to` (https anywhere, plain http
  to loopback only); at most 5 hops, 512 MiB (or the size the release declares) counted
  as it arrives, 15 minutes for the transfer, 30 s and 4 MiB for the release lookup.
  Headers: `Accept` and `User-Agent: incode` (GitHub's API requires one) — no credential,
  nothing about the operator. Hashed while streaming; a mismatch deletes the download
  and unpacks nothing. A transport error carries its whole cause chain.
- **Unpacked guarded** (`ext_tools::unpack`). zip via `zip`, tar.gz via `flate2` +
  `tar`, written entry by entry by incode (never the crates' own extract): each name must
  be relative and made of plain components (`ext_tools::inside_path`; for zip the crate's
  `enclosed_name` must agree), a `:` stream name is refused on Windows, links (symbolic
  and hard) are refused, and the archive stops at 10,000 entries or 2 GiB unpacked,
  counted as the bytes come out. Into a staging folder, then renamed to
  `~/.incode/tools/<name>/<version>/`; `run[0]` must exist there or nothing is
  installed. `tools/<name>/installed.json` records name, version, folder, asset, sha256
  and source. Older version folders are removed (one still running on Windows stays until
  the next update).
- **Run from its own folder.** `ext_tools::executable` resolves `run[0]` inside the
  installed folder (`.exe` added on Windows when the name has no extension), never on
  `PATH`. `ext_tools::session_servers` turns every installed entry into a stdio
  `ServerSpec` (`run[1..]` as arguments) and hands it to the same
  `SessionTools::connect_client_servers` an ACP client's servers use: in the background,
  at session start, in the REPL (`cli::connect_ext_tools`), with `-t`
  (`task_run::connect_ext_tools`, one `SessionTools` shared by every candidate backend)
  and under `--acp` (`SessionFactory::tool_servers`, merged with the client's servers in
  the one connect call). Its tools are `mcp__<name>__<tool>`, reach **run**, fail closed.
- **Measured live** (2026-09-27, upstream `h4ckf0r0day/obscura` v0.2.3, scratch
  `INCODE_DIR`): `--install-tools` resolved the tag, checked
  `obscura-x86_64-windows.zip` (68,156,208 bytes) against GitHub's digest `781a1b8b…`,
  and unpacked `obscura.exe` + `obscura-worker.exe` in 8.8 s. The same asset with one
  byte flipped, served on loopback against the pinned digest, was refused with both
  digests named and nothing left in `tools/browser/`.

### The desktop (phase 6b)

The twelve desktop tools are toolbox tools when the desktop is switched on
(`INCODE_DESKTOP`; `cli::desktop_setup_for` → `Toolbox::enable_desktop`, on the bridged
claude route and the API route). They are offered, schema-checked, held to the mode and
the engine, and drawn as incode's own calls like every other tool, and no desktop server
is handed to the CLI any more.

They are still EXECUTED by `incode --desktop-mcp`, now started by the toolbox itself
(`toolbox::desktop`) on the first desktop call, over its stdin/stdout, and held in
`SessionTools`. Moving the execution into the session's own process was rejected, for
three reasons: DPI awareness is process-wide and can be set once only
(`desktop::dpi`), so the REPL's process would be changed for every run; the input thread
owns the synthetic mouse and keyboard; and the kill switch — EOF on the child's stdin, and
the stop file beside its journal checked before every call — needs a process boundary to
be absolute. Dropping the session's tools closes the pipe, waits 2 s, then kills the
child's tree. The child runs `-p read-only` for a read-only session and `-p
workspace-write` otherwise (the toolbox has already asked the person under `prompt`), and
is restarted when the mode changes between the two; the operator's `desktop_hooks` still
run in it. Codex, which is not bridged, is still handed the desktop server as TOML
overrides.

### User-defined sub-agents (phase 8)

`src/subagents.rs` loads a Markdown definition per file from `.incode/agents/` in the
workspace and `~/.incode/agents/` for the operator; a project's definition wins over
the operator's of the same name. The front matter is `name`, `description`, optional
`role` (one of `roles::KNOWN_ROLES`) and `model`, and an optional `tools` allow-list of
Toolbox tool names; the body is the sub-agent's instructions. The `Agent` tool
(`toolbox::subagent`) takes `agent: <name>`: the definition's body rides the sub-agent's
system context, its `model`/`role` route the model (a call's `role` wins; a definition's
`model` wins over the role's), and its `tools` restrict the sub-agent's toolbox —
narrowed to what the PARENT may use, never wider, and checked at `Toolbox::run_call`'s
gate (`restrict_tools`/`allows`). The defined agents are named in the `Agent` tool's
schema and listed by `/agents` (REPL and ACP). Each file is bounded, and a bad one is
reported (by `/agents` and the trace), never a crash.

### Headless JSON output (phase 8)

`incode -t "<task>" --output-format stream-json` prints one JSON object per line as the
turn happens — `init`, `text`, `thinking`, `tool_use`, `tool_result`, `usage`, and last
`result` — for CI and scripts; `--output-format json` prints the `result` object alone.
The default stays text. In both JSON forms stdout carries the JSON and nothing else:
`task_run` takes the real stdout as a private handle and repoints the process's standard
output at stderr through the same `acp_agent::stdio::claim` that keeps `--acp` pure, so
every stray print, the console, and any inherited child land on stderr. The events are
the existing `AgentEvent` stream mapped to JSON (`task_run::Report`); the whole text
blocks are emitted, not the deltas beside them.

### The OS sandbox for shell commands (phase 8)

The `Bash` tool runs a command the MODEL chose. The guards catch a direct textual
route to a protected file and the permission mode decides whether the tool may run
at all, but neither stops a command that writes outside the workspace through a
variable, a glob or a script it wrote first (§8, "Shell is not sandboxed"). `src/sandbox`
hands the command to the operating system's own confinement so the write is
PREVENTED. It is a setting — `admin.json` `sandbox: off | workspace-write | read-only`,
or `INCODE_SANDBOX` — tied to the session's permission mode by default (plus a separate
network switch, below):

| permission mode | default sandbox |
|---|---|
| read-only, plan | `read-only` |
| prompt, workspace-write, acceptEdits | `workspace-write` |
| danger-full-access, allow | `off` |

- **`read-only`** — read anywhere, write nowhere (bar the null sink and the terminal).
- **`workspace-write`** — read anywhere, write only inside the session's roots (the
  workspace and each `--add-dir`) and the temp directory.
- **`off`** — unconfined.

**The network is a separate, opt-in switch:** `admin.json` `sandbox_network: "allow" |
"deny"`, or `INCODE_SANDBOX_NETWORK` (which wins). The default is `allow` — the behaviour
before the switch existed. `deny` is independent of the filesystem mode (it applies under
`off` too), and each OS does what it soundly can; `Sandbox::note` (traced for every
sandboxed command) says `network denied by …` or `network deny requested but NOT
enforced: …`, never silence:

| OS | `sandbox_network: "deny"` | verified |
|---|---|---|
| Linux | Landlock ABI v4 (Linux 6.7+): every TCP `bind` and `connect` refused, **loopback included** (Landlock matches ports, not addresses). UDP (so DNS) is not covered by any ABI. On an older kernel the network rights are dropped by the crate's best-effort level, so the helper reads the effective ABI back after `restrict_self` and traces NOT enforced; the parent asks first with a hard-requirement ruleset that is created but never applied (`linux::network_enforceable`) | WSL2 kernel 5.15 (ABI V1): the NOT-enforced path, live. The enforced path is compile- and unit-checked only — no ABI v4 kernel was at hand |
| macOS | the Seatbelt profile adds `(deny network*)` and re-allows **loopback** (`localhost:*` bind, inbound and outbound — a local dev server or a test suite still works) and Unix-domain sockets (ssh-agent, local daemons). A decision: this blocks reaching the outside, not local IPC. A dual-stack socket bound to `::ffff:127.0.0.1` is not matched by `localhost` and so is refused | the profile string is unit-tested on every platform; NOT run on a Mac (it was offline). The filter forms are the ones shipping Seatbelt profiles use |
| Windows | **not enforced**, and said so. A token cannot block the network; only an AppContainer without the internet capabilities can, and an AppContainer reads only what is granted to it or to `ALL APPLICATION PACKAGES` — measured: `System32` and Git's `cmd` are, but the workspace, `~\.cargo\bin` and the whole user profile are not. Granting all of them for a session is a far larger ACL change than the workspace grant below | the note's wording is unit-tested |

Both the foreground `Bash` and background `/bg`-style jobs run through `Sandbox::wrap`
(`tools::bash`, `tools::bash_jobs`); the operator's own `/bg` at the REPL runs unconfined
(it is their command, not the model's).

**Which shell (`tools::shell_choice`).** The tool is called `Bash` and models write
POSIX shell, so it never runs cmd.exe: on Windows `cmd /C` turned `mkdir -p .backups &&
cp a ".backups/a.$(date +%s)"` into a directory named `-p` and a file named with a
literal `$(date …)`. The shell is chosen once per process:

| OS | order |
|---|---|
| macOS, Linux | `sh -c`, as before |
| Windows | 1. `INCODE_SHELL`, then `admin.json` `shell` (a path or a name on `PATH`; `pwsh`/`powershell` run as PowerShell, anything else as a POSIX shell with `-c`; cmd.exe and the WSL launcher are refused and the refusal traced). 2. Git for Windows' `bin\bash.exe`, found above a `git.exe` on `PATH` (`<Git>\cmd`, `<Git>\bin`, `<Git>\mingw64\bin`) or under `ProgramFiles`, `ProgramW6432`, `ProgramFiles(x86)`, `%LOCALAPPDATA%\Programs` — never a `bash.exe` under the Windows directory, which is WSL and runs in another filesystem namespace. 3. PowerShell: `pwsh.exe` on `PATH`, then Windows PowerShell. None: every call fails saying where it looked |

A candidate is used only once it has answered its version probe (10 s bound), which
also gives the name the model is told: the `Bash` description ends with e.g. "Commands
run in Git Bash (bash 5.2) on Windows: write POSIX shell …" or "Commands run in Windows
PowerShell 5.1, not bash, …". Every argument is quoted whole by the C runtime's rules
(`quote_windows`), because `Command::arg` leaves an argument without spaces unquoted and
the msys runtime GLOBS an unquoted argument from a Windows parent; PowerShell gets
`-NoProfile -NonInteractive -Command <script>` with UTF-8 output and progress off (not
`-EncodedCommand`, which makes Windows PowerShell write its errors as CLIXML).

**Nothing is typed into a command.** A foreground command's stdin is the null device,
as a background job's already was: in the REPL this process's stdin is the person's
terminal, and a command that read from it (`read x`) held it until its timeout. Now
`read` sees end of input at once (`a_command_that_reads_input_is_not_handed_ours`).

**Git Bash cannot run under the Windows token.** Measured: every msys program started
under the write-restricted token died at once with `CreateFileMapping <user SID>.1,
Win32 error 5` — the msys runtime maps a per-user shared section that the user's other
msys processes created, and a write-restricted token may not map it writable. So
wherever the token applies (read-only; workspace-write when named) the command runs in
PowerShell, which runs under the token and is confined by it (live: reads work; writes
inside the workspace, outside it and to Documents are refused under read-only; the
workspace-write session test passes with the ACL snapshot byte-identical after it), and
the description says so (`Sandbox::posix_shell_runs`). Unconfined sessions — every
writing session unless workspace-write is named — run Git Bash.

Per OS, all measured live except where noted:

- **Linux — Landlock, sound.** The command runs through `incode --sandbox-exec`, which
  applies the kernel's own unprivileged filesystem confinement to itself (read
  everywhere; write beneath the roots and temp for workspace-write; write nothing for
  read-only, bar the device sinks) and then execs the shell. Applied before any thread
  starts, because an early-ABI ruleset binds one thread. Measured in WSL2 (kernel 5.15,
  ABI V1 so `PartiallyEnforced`): a write in a granted root works, a write to `$HOME` is
  denied and the file is not created, a child's write is denied, `git`/`cc`/`ls` and
  `/dev/null` work, `curl` is untouched. Crate: `landlock` 0.4.7 (below).
- **macOS — `sandbox-exec` (Seatbelt), sound.** A generated profile: allow all, then
  deny every write, then re-allow the temp directory, the roots (`WS0..WSn`, passed as
  `-D` parameters so a path with a quote or backslash needs no escaping) and the device
  sinks. Validated on macOS 26.6.2: a write in the workspace works, a write to `$HOME`
  is refused and not created, a child is refused, `git`/`cc`/`mktemp` and `curl` work.
  `sandbox-exec` is marked deprecated by Apple but present and silent. The profile
  string (`sandbox::seatbelt_profile`) is compiled and unit-tested on every platform.
- **Windows — a write-restricted token; workspace-write through a per-SESSION grant,
  opt-in.** The command runs through `incode --sandbox-exec`, whose helper
  (`sandbox::windows`) builds a token with `WRITE_RESTRICTED` and three restricting SIDs —
  a private SID, `Everyone` (so `> NUL` works) and this logon's SID (so the shell and its
  tools run) — plus a default DACL, and launches `cmd /S /C` under it with
  `CreateProcessAsUserW`, inheriting incode's pipes and joining incode's job. The token
  writes only where one of those SIDs is granted.
  - **read-only** restricts to incode's fixed SID (`…-4242`), granted nowhere. Measured
    without elevation: a write inside the workspace and a write to `Documents` are both
    refused and no file lands, `reg add HKCU` / `schtasks` / injection into `explorer` are
    refused, reads and `> NUL` work.
  - **workspace-write** (`sandbox::windows::acl`) restricts to a per-process session SID
    (`S-1-5-21-1111111111-2222222222-3333333333-<random>`). The first sandboxed command of
    the session adds ONE inheritable `Modify` ACE for that SID to each root (workspace and
    `--add-dir`, a root nested in another folded into it) and to a session temp directory
    (`<data dir>/sandbox-grants/<rid>.tmp`, which the helper points `TEMP`/`TMP` at — the
    user's `%TEMP%` is not granted); the ACE is removed when the last toolbox holding the
    grant drops (sub-agents in the same process share it, refcounted per root). A drive
    root or a directory holding the user profile is refused, and the command is then NOT
    run. Applied **only when the operator named the mode** (`sandbox: "workspace-write"`
    or `INCODE_SANDBOX`); left to the permission-mode default it stays unconfined and the
    trace says so, because it changes the operator's ACL for the session.
  - **The crash sweep.** Each grant is written down first (`<rid>.grant`: SID, temp,
    roots) and the process holds a named mutex `Global\incode-sandbox-<rid>` while it
    holds any grant. The next session's first grant revokes every record whose mutex is
    gone, removes its temp directory and deletes the record; a record whose revoke failed
    is kept for the next sweep. ACL changes are serialised across incode processes by a
    second named mutex, so two read-modify-writes of one DACL cannot lose one.
  - **Measured** on a scratch copy of incode's tree (17,679 files + 990 directories,
    never a real project): grant 2.41–2.49 s / revoke 2.40–2.58 s by .NET over three runs
    (6.0 s the first, cold), and 2.32–2.94 s / 2.37–3.23 s through incode's own Win32
    path over four live sessions. NTFS inheritance is WRITTEN into every descendant, so all 18,673 objects got the
    inherited ACE — but no file's contents or mtime changed and `git diff-files` stayed
    clean; propagation did not follow a junction out of the tree; and `icacls /save` of
    the whole tree after each session was byte-identical to before. Live, through the real
    helper: a write inside the tree and a delete are allowed; a write to `Documents`, a
    write to a directory outside the tree, and `icacls /grant` on a file outside are all
    refused ("Access is denied"); `%TEMP%` is the session's and writable. A child process
    killed mid-session (TerminateProcess) left its ACE on the root; the next grant swept
    it (sweep + grant 4.66–6.01 s) and the tree matched the pristine snapshot again.
    Before this, the only route found was a grant and revoke per COMMAND, which is why it
    was left off. Low integrity and AppContainer were measured too; neither is used.

The `--sandbox-exec` helper is its own CLI mode, handled in `main` before any thread
starts. On Unix it applies the confinement and execs; on Windows it launches the
restricted child and waits. On a platform or mode it cannot confine soundly, the command
runs unconfined and `Sandbox::note` says which — a session that ran unconfined is never
mistaken for one that was confined.

### Operator hooks on every tool (phase 8)

`hooks::HookRunner` runs the operator's own pre- and post-tool commands
(`admin.json` `desktop_hooks`, alias `hooks`) around EVERY Toolbox tool, in
`Toolbox::run_call` — after the guards and the permission mode, before the memory
engine. The event is one JSON object on a piped, then closed, stdin
(`hook_event_name`, `tool_name`, `tool_input`, `session_id`, `cwd`, plus
`tool_response`/`is_error` after), the shape a CLI's own hooks are given; exit 2 on a
pre-tool command denies, stderr the reason. The desktop tools are the one exception:
their executor (`desktop::mcp`) runs the same commands itself, as it always has, so they
are not doubled. The name predates their running for every tool. Shared with the memory
engine's runner through `hooks::run_piped` (piped-and-closed stdin, contained tree,
bounded wait).

### Dependencies (phase 6a)

Each vetted in a scratch crate and approved by the owner; each pinned exactly (`=`) in
`Cargo.toml`, with a one-line comment there. The lock gained 115 packages and changed
none; every one of the 115 manifests was read, 72 declare a `rust-version`, the highest
is 1.88 (calamine, globset, ignore, zip), so the crate's floor is unchanged.

| Crate | Pin | Why |
|---|---|---|
| `ignore` | 0.4.33 | ripgrep's walker: `.gitignore` / `.ignore` / `.git/info/exclude`, hidden files, inside or outside a repo — the one walker Glob, Grep and LS share |
| `grep-searcher` | 0.1.17 | line search with context lines, binary detection and BOM transcoding |
| `grep-regex` | 0.1.14 | the regex matcher grep-searcher drives |
| `globset` | 0.4.20 | the model's glob, applied after the walk so it only narrows |
| `tree-sitter` | 0.26.13 | parser runtime and query engine for Outline |
| `tree-sitter-language` | 0.1.7 | held back: 0.1.8 declares rust-version 1.90 |
| `tree-sitter-rust` / `-typescript` / `-python` / `-go` / `-javascript` | 0.24.2 / 0.23.2 / 0.25.0 / 0.25.0 / 0.25.0 | one grammar per language, each behind its `lang-*` feature |
| `pdf-extract` | 0.12.1 | PDF text; can panic, so every call is on its own thread under `catch_unwind`, with a deadline and an input size bound |
| `calamine` | 0.36.1, no default features | xlsx / xlsm / xlsb / xls / ods |
| `zip` | 8.6.0, `deflate` only | the archive under docx / pptx / odt, and the bomb check before calamine |
| `quick-xml` | 0.41.0, no default features | the XML in those archives; 0.41 was already in the lock |
| `dom_smoothie` | 0.18.2, no default features | readability: an HTML article as markdown |
| `dom_query` | 0.28.0, `markdown` only | a page that is not an article, as markdown |
| `jsonschema` | 0.58.0, no default features | each call's input against its tool's schema; the defaults would add a network resolver and a TLS stack, and a `$ref` must never fetch |

Removed: `walkdir` (no `.rs` file used it) and `glob` (its two uses were the Glob/Grep
walk and LS's `ignore`, both now globset). Both remain in the lock as other packages'
dependencies.

Phase 6b added one, vetted the same way in a scratch copy of the manifest:

| Crate | Pin | Why |
|---|---|---|
| `tungstenite` | 0.30.0, `handshake` only (no TLS) | the DevTools websocket to the headless browser, client half, over a loopback `TcpStream`. The latest release; MIT OR Apache-2.0; rust-version 1.85. It adds 7 packages to the lock — itself, `sha1` 0.11, `digest` 0.11, `block-buffer` 0.12, `crypto-common` 0.2, `hybrid-array` 0.4.15, `const-oid` 0.10.2, each MIT OR Apache-2.0 at rust-version 1.85 — and reuses `bytes`, `http`, `httparse`, `rand`, `data-encoding`, `log` and `thiserror`, already there |

Phase 8 added one, Linux-only, vetted the same way in a scratch copy of the manifest
(and its API compile-checked for `x86_64-unknown-linux-musl`, which needs no cross C
compiler):

| Crate | Pin | Why |
|---|---|---|
| `landlock` | 0.4.7, `[target.'cfg(target_os = "linux")']` | the Linux OS sandbox for shell commands: the kernel's own unprivileged filesystem confinement. The latest release (2026-07-27, not yanked), covering ABI V1–V9; MIT OR Apache-2.0; rust-version 1.71, below the crate's 1.88 floor. Adds EXACTLY ONE package to the lock — itself; its dependencies (`enumflags2` 0.7, `libc` 0.2, `thiserror` 2.0) are all already resolved |

The Windows sandbox needs no new crate: it turns on the `Win32_Security_Authorization`
feature of `windows-sys` 0.61, already in the lock, for the write-restricted token's
SID and DACL calls. The workspace-write grant and the network switch added NO crate and
NO feature: `GetNamedSecurityInfoW` / `SetEntriesInAclW` / `SetNamedSecurityInfoW` are in
`Win32_Security_Authorization`, `GetSecurityDescriptorControl` in `Win32_Security`, and
`CreateMutexW` / `OpenMutexW` / `ReleaseMutex` in `Win32_System_Threading` — all already
on (checked in the cached 0.61.2 source); the two file-rights constants that live in the
unenabled `Win32_Storage_FileSystem` are spelled numerically instead. The session RID
comes from `uuid` v4, already a dependency. Landlock's network rights (`AccessNet`,
ABI v4) are in the pinned `landlock` 0.4.7.

External tools (`ext_tools`) added one, vetted against the live index on 2026-09-27:

| Crate | Pin | Why |
|---|---|---|
| `tar` | 0.4.46, no default features | reading a release's tar.gz entry by entry, so every name is checked and every byte counted by incode. The latest release; MIT OR Apache-2.0; rust-version 1.63. `default-features = false` drops `xattr`. It adds TWO packages to the lock — itself and `filetime` 0.2.29 (MIT/Apache-2.0, rust-version 1.75) — whose `cfg-if` and `libc` were already there |

`sha2` 0.10.9 and `flate2` 1.1.10 are declared too, adding no package: both were already
resolved in the lock (sha2 under several crates, flate2 under `zip`'s `deflate`).

---

## 6. The invariants

A change that breaks one of these is a change to the architecture, not a bug fix.

1. **No memory in this crate.** No store, no cache, no "default brief", no second
   findings trail. Call the body.
2. **`Done` requires `Verified`.** No verdict, no report, no confidence from any model
   reaches that state.
3. **The judged cannot edit the judge.** Anything that decides correctness is
   read-only to the agent.
4. **Retrieval fails open; the owner channel fails closed.** Silence is never consent.
5. **Every stop says why.** A run that ends without a stated reason is
   indistinguishable from a crash.
6. **Bound the action, never the measurement.** A counter a human will decide on must
   be complete, even when the work it counts is capped.
7. **Provenance travels with the value.** Which layer set a setting, who decided a
   card, which command proved a task.

---

## 7. Testing conventions

Tests are named as **sentences that state the law**, not after the function they call:
`a_confident_worker_cannot_complete_work_the_check_rejects`, not `test_complete`. The
name is what a person reads when it fails at 3am, and `test_complete` tells them
nothing.

Every load-bearing test has a **control run**: the defect is reintroduced, the test is
confirmed to fail *on the intended assertion*, and the fix is restored. A test that
passes with and without the fix is decoration, however green.

Contracts with external services are pinned against **bodies the running service
actually returned**, kept as literal constants. A parser tested against an imagined
shape passes happily while the request goes nowhere.

---

## 8. What this architecture does not do

- **Live runs are partial.** The claude route has run live through the bridge, `-t`,
  `--acp` and the memory lifecycle (§2, §5a, §5b record each measurement). codex could
  not be run past its usage limit, the API route and the provider agents have no live
  endpoint here, and `/run` has not completed a full roadmap end to end on record.
- **Shell is not sandboxed** (mostly closed, phase 8). `guards.rs` catches direct textual
  routes to a protected file; a variable, a glob, or a script that writes on the agent's
  behalf gets past it. The OS sandbox (`src/sandbox`, §5b) now confines the shell where
  the platform can — Landlock on Linux, Seatbelt on macOS, a write-restricted token on
  Windows (workspace-write there only when named, through a per-session ACE) — and says
  plainly where it cannot (Windows workspace-write left to the default, the network on
  Windows and on Linux kernels before 6.7, UDP on Linux). The permission tier remains the
  boundary in the `off` mode and wherever the sandbox is not applied.
- **Contracts are checked by name, not by type.** Types belong to the project's own
  compiler, which already runs.
- **One runner, no concurrency.** Multiple runners over one ledger needs a claim
  protocol and is not attempted.
