# Incord Code — Architecture

**What this is for.** Every implementation task inherits this document, so an error
here is the most expensive kind in the system: it is copied into work that has already
been done by the time anyone notices. The PRD says *what* and *why*; this says *how it
is built* and, more importantly, **why it is cut this way** — the boundaries are the
part that a later agent will otherwise redraw by accident.

Measured against the working tree. Every claim cites a file.

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

| | The harness (`incode`) | The agent (an installed CLI) | The body (memory service) |
|---|---|---|---|
| **Owns** | the plan, the check, the guards, the ledger, the budget, the terminal | the loop, its own tools, its own subagents, its own context | recall, rules, the proxy, the auditor, the recorder, the owner queue |
| **State** | one workspace | one conversation, which it stores itself | every project, every agent on the machine |
| **Reached by** | the person, at a prompt | spawned as a child process | HTTP on `127.0.0.1`, default port in `brief.rs` |
| **Lifetime** | a session | a turn | a machine |
| **Costs** | nothing | the operator's subscription | nothing |

**The harness does not talk to a model.** It holds no API credential and has no
provider client: it decides what is worth doing and whether it was actually done, and
delegates the doing. That is why a subscription is enough to run all of it.

Everything the harness knows that outlives a session, it asks for. It caches none of
it. A cache here is a second source of truth, and a second source of truth is the
defect above wearing a performance argument.

### Why HTTP, and where MCP lives now

Every body-facing module reaches the service over its plain HTTP API on `127.0.0.1`.

The harness used to carry its own MCP client, and attaching it to the service looked
obvious — it did not work, because that client was **stdio-only** while the service
registers over **HTTP**. The client is gone along with the rest of the tool layer:
MCP servers are the agent's business now, configured in the agent's own settings and
connected by it. The harness proxies nothing. The one exception is an ACP client's
stdio servers under `incode --acp`, which incode connects to itself and runs as its
own tools (below).

### `incode --acp`: incode as the agent an editor drives

An Agent Client Protocol client (an editor, or the owner's desktop app) starts
`incode --acp` and drives it with JSON-RPC 2.0, one message per line on stdin/stdout.
incode is the **agent** here, never the client: underneath, the work is still done by
the subscription CLIs, through the same `cli::prepare_session` the REPL builds its
session with and the same `cli::plan_turn` / `AgentSession::turn_observed` a typed line
runs. The code is `src/acp_agent/`; payloads are `agent-client-protocol-schema` types.

| Method | What incode does |
|---|---|
| `initialize` | protocol version 1; prompts take text and embedded context; `loadSession: true`; `sessionCapabilities` `close` and `list`; `mcpCapabilities` `http: false`, `sse: false` (stdio servers only); no auth methods |
| `authenticate` | invalid params — no method was offered; the CLIs hold their own logins |
| `session/new` | `cwd` must be absolute and an existing directory; it becomes the workspace (made canonical by `prepare_session`). The session is built on its own thread, the client's stdio MCP servers are started, and the answer carries `modes` and `configOptions`; then `available_commands_update` lists its slash commands, and a message chunk says what each MCP server offered or why it did not start |
| `session/load` | a session kept in `cwd` (its transcript exists): built like `session/new` under the same id, its history replayed as `user_message_chunk` / `agent_message_chunk` BEFORE the answer (`modes`, `configOptions`); the model's own conversation is resumed where it can be (below) |
| `session/list` | the transcripts in `cwd` (id, title = the first person's line, `updatedAt` = the file's time), newest first, at most 200; with no `cwd`, every workspace an ACP session record names |
| `session/prompt` | one turn: text as `agent_message_chunk`, tool calls and results as `tool_call` / `tool_call_update`, an edit that landed as a `diff`, the task list as `plan`, the context window's fill as `usage_update`; replies with the stop reason, or a JSON-RPC error if the turn failed. A prompt that is a slash command runs that command instead; `/run` works the ledger and sends it as the plan |
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
agent CLI writes files itself, so nothing is read at the moment of the write: a splice
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

**Slash commands.** `/undo`, `/plan`, `/act`, `/diff`, `/links`, `/roadmap`, `/run`,
`/doctor`, `/memories`, `/remember`, `/worktree`, `/agent` and `/roles` are announced,
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
is verified first, and the prompt then answers `cancelled`. A run is refused in a
`read-only` session: every task changes files.

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
closed). Bounds: `initialize` and `tools/list` 20 s each, servers started in parallel;
a call 120 s, abandoned at once when the turn is stopped; at most 16 servers and 128
tools each; stdout lines 16 MiB. A server gets the filtered environment plus the
client's variables, and runs in the workspace. The servers are children of incode,
held in `SessionTools`: when the session ends they get EOF, 2 s, then their process
trees are killed. HTTP and SSE servers are not started (no HTTP MCP client exists in
the crate), and `initialize` says so — so the owner's app, which sends its Incord
Memory server only when `http` is true, sends none.

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
which calls neither it nor `session/load`), HTTP / SSE MCP servers, elicitation, and
fs / terminal.

**The workspace is made absolute once.** `prepare_session` canonicalises the workspace
(`file_ops::workspace_root`, the plain `C:\x` spelling, not `\\?\C:\x`), and
`file_ops::resolve_path` anchors a relative path to the process directory before it
normalises it — `incode -w .` refused `LS "."` and every write into a new top-level
directory as "path escapes the workspace", because `./.` normalised to an empty path
and the walk to the nearest existing ancestor ended at nothing.

### The engine's hooks, run by incode for its own tools

`hooks::engine` runs the memory engine's lifecycle hooks at the moment incode's own
Toolbox is about to act. Only calls that go through the Toolbox are covered: today
that is the API route (`agent::api`). A CLI child (Claude, Codex, Antigravity) runs
its own tools inside its own process, so no call it makes passes this gate.

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
- **Bounds:** 610 s for `pre-tool`, `permission-request` and (pinned for later)
  `capture` — the engine's own 600 s registration plus a margin, so the engine's
  deadline fires first. Not the operator hooks' 10 s. A stopped turn (the cancel token)
  ends a wait at once.
- **Order in `Toolbox::run_call`:** the operator's guards → `hook pre-tool` → snapshot →
  dispatch. A guard refusal never spawns the engine.
- **Replies:** exit 2 → refused, stderr is the reason (for `AskUserQuestion`, exit 2 is
  the owner's answer and returned as a NON-error result); exit 0 silent → run; exit 0
  `permissionDecision: allow` → run, and `additionalContext` is appended to the result;
  `ask` → the permission flow below; any other exit, a timeout, or a spawn failure →
  **fail closed for `Write`/`Edit`/`Bash`, fail open for everything else** (owner's
  decision).
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
- **`session-start`** (60 + 10 s): in `prepare_session` (`startup`, or `resume` with
  `--resume`) and on `/clear` (`clear`, new id). Its block replaces incode's own rules,
  recorder and auditor blocks (Option A) in every backend's system context
  (`AgentBackend::set_system_context`); with no answer those stay.
- **`user-prompt`** (60 + 10 s): in `plan_turn`, after the greeting short-circuit; its
  block replaces `rules::context_for_turn` for the turn.
- **Oversized blocks:** over 12 KB (start) or 6 KB (turn) the head is given inline and the
  whole written to `.incord/context/<session>-{start,turn}.md`, which the model can Read.
- **`capture`** (the stop, 610 s): at the end of every successful turn, with
  `last_assistant_message` and `stop_hook_active`. A `{"decision":"block","reason":R}`
  reply sends R to the same backend as the next prompt — a CLI resumes its session, the
  API appends to its conversation. At most `MAX_CONTINUATIONS` (3) per turn; not sent at
  all when the turn was stopped or while a `/bg` job runs.
- **`session-end`** (120 + 10 s): REPL exit (`prompt_input_exit`), `/clear` (old id,
  `clear`), the end of an ACP session's thread (`session/close` or the client going
  away — shutdown waits for it, up to the bound), and the end of `--task`.
- **Transcript:** the engine records a conversation only from `transcript_path`, so
  incode writes `.incord/transcripts/<session>.jsonl` in the engine's shape (`type`,
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

So a **complete** brief is cached to `.incord/brief.cache.json` on every successful
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
| `permissions.rs` | **What the session may do reaches every place that does it.** A mode governs the agent's file writing *and* the desktop: `-p read-only` means the agent may look at the screen and not touch it. The policy applies at `desktop::mcp::call_tool`, the one tool dispatcher left in this process. |
| `hooks.rs` | **The operator can put their own command in front of an action.** Pre-tool hooks run on the desktop server and refuse with exit code 2; post-tool hooks run only for calls that happened and cannot deny. Bounded at 10s, because a hook runs between the agent asking and the screen moving. `hooks/engine.rs` is the other half: the memory engine's own hooks, run before each Toolbox call with a piped stdin and the engine's 600 s bounds — see §2, "The engine's hooks". |
| `admin.rs` | One settings file, one precedence ladder, every value able to say which layer decided it. |
| `research.rs` | A dependency the build links and the document never argued for is a decision nobody recorded. |
| `identity.rs` | **The agent is the identity; the model is an attribute of the attempt.** A fallback changes what answered without changing who asked, so an identity built on the model would split one worker's history the first time a provider was busy. |

### The mechanical half

`agent/` discovers an installed agent and drives it — `claude_code.rs` speaks its
stream protocol, `worker.rs` hands it one ledger task at a time. `cli/` is the
terminal, and `cli/agent_turn.rs` is one interactive turn. `tools/` is no longer a
tool layer at all: it is what the harness does to a workspace **itself** — run the
check, snapshot a tree so a turn can be undone, read the git context, report what is
installed. `runtime/` is what outlived the loop: two error types, the token counter
and the cancel flag.

`desktop/` and `team_mcp.rs` are the harness's two **MCP servers** — the only way a
delegated agent gains a capability, since there is no model loop here to dispatch one.
Both run as separate processes (`incode --desktop-mcp`, `incode --team-mcp`) so a crash
in either cannot take a live session with it, and both are registered into a single
`mcpServers` object because each config is a whole map and two of them means the second
wins. See `COMPUTER_USE.md` and `TEAMWORK.md`.

**What used to be here and is not:** the harness's own agentic loop
(`ConversationRuntime`), its provider clients (`provider/`), its tool dispatch and
permission tiers, its transcript and compaction. Around 12,000 lines. Every one of
those things exists in the installed agent, done better, and keeping a second worse
copy meant holding an API key to run it.

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

### Every turn
```
the line the user typed ─┬─► rules::context_for_turn ──► the owner's standing rules
                         └─► recall::context_for_turn ─► what memory holds about it
                                       │
                                       └─► dynamic context (rules FIRST)
```

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
session start: local CLIs found on the PATH, the API configured by `INCORD_API_URL` /
`INCORD_API_KEY` / `INCORD_API_MODEL` (id `api`), and the agents each API provider in
`admin.json` lists about itself. Every entry is marked usable, or not usable with the
reason. `/agent` prints it; `/agent <id>` saves the lead (`agent`) and
`/agent sub <id>` the sub-agents (`sub_agent`), both applying from the next `/run` and
the next session.

- **Lead**: `INCORD_AGENT`, else `admin.json` `agent`, when usable; else the first
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
| `.incord/ledger.json` | **machine** | per-task execution state |
| `.incord/contracts.json` | human approves | frozen interfaces |
| `.incord/guards.json` | human sets once | extra protected paths |
| `.incord/team.jsonl` | **machine**, append-only | what agents asked each other, and who answered |
| `.incord/desktop.jsonl` | **machine**, append-only | what an agent did to the screen, streamed to `--serve` |
| `.incord/desktop.stop` | **human** | its existence halts the desktop; nothing parses it |
| `.incord/serve.token` | machine, per `--serve` start | the token a write to `--serve` must carry; readable by anything in the workspace, agents included |
| `.incord/run.stop` | **human**, via `--serve` | a stop request; a running `/run` consumes it and stops, one found at `/run` start is cleared unobeyed |
| `.incord/findings/<task>.md` | **agent** | one research task's answer, and the thing its check reads |
| `.incord/brief.cache.json` | machine | the last complete brief, so an unreachable memory service does not stop a run |
| `<repo-parent>/worktrees/` | machine | one checkout per lane, removed after; the **branch is kept** |
| `~/.incord-code/admin.json` | human sets once | model, roles, ceiling, check |
| `~/.incord-code/sessions/` | machine | session transcripts |

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
| `Bash` | run | execute | output 30 000 chars; timeout | `run_in_background` starts a session job (`MAX_RUNNING_JOBS` 8) |
| `BashOutput` | observe | read | `MAX_JOB_READ_BYTES` 30 000 per read; `MAX_JOB_BUFFER_BYTES` 1 MiB kept per stream | new output since the last read; loss past the buffer is counted |
| `KillShell` | run | execute | — | kills the job's process tree |
| `TodoWrite` | session | think | `MAX_TODOS` 100, `MAX_TODO_CHARS` 500 | kept per session; `--acp` sends it as the plan, with its priority, on the call's successful result |
| `Diagnostics` | run | execute | `PROJECT_CHECK_TIMEOUT` 300 s, `MAX_PROJECT_FINDINGS` 50, `MAX_DIAGNOSTIC_FILES` 50 | `INCORD_CHECK_CMD`, else `cargo check` / `tsc --noEmit` |
| `WebFetch` | observe | fetch | 20 s, 512 KiB read, 20 000 chars shown; rendered: 20 s + 10 s kill grace, 2 MiB read | private addresses refused by name and by resolution; carries no credential; an HTML answer becomes markdown through Read's HTML conversion (readability when the page reads as an article, the whole page less nav/scripts otherwise), anything else is returned as sent. Where the headless browser is found: `render_js` runs the page's script first, and a page that comes back as an app shell (script, under 200 characters of text) is rendered on its own; `render_js` is not advertised where there is no browser |
| `Screenshot` | observe | other | `PAGE_DEADLINE` 60 s, load 30 s, settle 3 s; viewport 200–3840 x 200–2160 CSS px; the image bounds below | OFFERED ONLY when the headless browser is found. A public `url` (WebFetch's address rules) or an HTML/SVG `path` in the workspace; `width`/`height`, `scale` 1 or 2, `full_page` (clipped at `MAX_IMAGE_EDGE` px tall, said so). The answer is an image block. See "The headless browser" below |
| `RenderPdf` | change | edit | `PDF_DEADLINE` 120 s, whole-tree kill | OFFERED ONLY when a printer is found (`pdf_renderer`, below). A public `url` or a workspace HTML/SVG `path`, to `output` (must end `.pdf`); the guards and the checkpoint see `output`; the engine's `code-edit` is told it. Printed in a private temp profile and copied into place only when it exists |
| `screen_info`, `screenshot`, `ui_tree`, `cursor_position` | observe | other | a call 120 s; the image bounds | OFFERED ONLY when the desktop is switched on (`INCORD_DESKTOP`). Executed by the desktop child — see "The desktop" below. `screenshot` answers with an image block |
| `click_element`, `type_text`, `paste_text`, `key`, `scroll`, `drag`, `move_mouse`, `click` | run | other | a call 120 s | the same; refused read-only, asked under prompt, refused when the engine cannot decide |
| `WebSearch` | observe | fetch | 15 s, 512 KiB, 8 results | provider from `admin.json` `web_search` — contract in `web::search_with` |
| `Skill` | observe | other | — | |
| `Agent` | run | other | depth 2 | |
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

The owner's `incord-browser` (`toolbox::browser`), found by `INCODE_BROWSER` (alone when
set), then `admin.json` `browser_bin` (alone when set), then `PATH` (`obscura`,
`incord-browser`). Not found: Screenshot is not offered, `WebFetch` runs no script and is
advertised without `render_js`, and RenderPdf is offered only with a system browser.

- **WebFetch with script:** `obscura fetch <url> --dump html --quiet --timeout 20 -o
  <temp>`, one child per call, whole tree killed 10 s past its own timeout, 2 MiB read
  back, then `html_to_markdown`. The url passes `web::check_public` (by name and by
  resolution) before the browser starts.
- **Screenshot:** `obscura serve --host 127.0.0.1 --port <picked> --quiet --font-dir
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

**RenderPdf** (`toolbox::pdf`) prints with a system headless browser of the Chromium
family when one is found — `INCODE_PDF_BROWSER` (alone when set), then `PATH`, then the
standard install paths, the x86 program directory first on Windows — as `--headless
--disable-gpu --no-pdf-header-footer --user-data-dir=<temp> --print-to-pdf=<temp>`. Its
PDF has real text (measured: Read on the output returns the page's words). Otherwise it
falls back to incord-browser's `Page.printToPDF`, whose PDF is image-only today
(incordai/incord-browser#11), and every such result says so. `admin.json` `pdf_renderer`:
`"auto"` (default), `"chromium"` or `"incord-browser"` — the owner's switch for when #11
is fixed. Measured: this browser refuses `preferCSSPageSize` (`-32601`), so the fallback
prints at the protocol's default page.

### The desktop (phase 6b)

The twelve desktop tools are toolbox tools when the desktop is switched on
(`INCORD_DESKTOP`; `cli::desktop_setup_for` → `Toolbox::enable_desktop`, on the bridged
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

- **It has never run against a live model.** No credential on the development machine.
  Everything here compiles, passes its tests, and appears in the built binary; none of
  it has completed a real task end to end.
- **Shell is not sandboxed.** `guards.rs` catches direct textual routes to a protected
  file. A variable, a glob, or a script that writes on the agent's behalf gets past it.
  The real boundary for shell is the permission tier.
- **Contracts are checked by name, not by type.** Types belong to the project's own
  compiler, which already runs.
- **One runner, no concurrency.** Multiple runners over one ledger needs a claim
  protocol and is not attempted.
