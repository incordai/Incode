# Incord Memory engine integration — gap audit and spec

**Scope.** What incord-code (`incode`) needs in order to carry the engine's
lifecycle-hook features the way Claude Code, Codex, Antigravity and Cursor already
do. Audited read-only on 2026-09-24 against the working trees of
`D:\Projects\incord-code` and `D:\Projects\Chat\incord-memory-service`.

Every claim cites `file:line`. Paths under `src/` are incord-code unless prefixed
`engine:` (meaning `incord-memory-service/src/`). Anything not proven by code read
for this audit, or by the engine contract stated in the task, is marked
**UNVERIFIED**.

## Final status (2026-09-26, HEAD `b39a99e`)

This table is the current state. The phase tables under it are the build record,
and sections 0–9 are the audit as it was written before any of it was built; where
they disagree with this table, this table is right.

| # | Feature | Status | Where |
|---|---|---|---|
| 1 | `pre-tool` guard | **DONE** for every call that goes through incode's Toolbox: the claude route (over the tool bridge) and the API route, including the `Agent` tool's sub-agents, the desktop tools, `Screenshot`, `RenderPdf`, `ConvertDocument` and an ACP client's MCP tools. **Not** for codex or agy, which still run their own tools. Order: schema → guards → permission mode → operator hooks → `pre-tool` → checkpoint → dispatch. | `src/hooks/engine.rs`, `Toolbox::run_call` |
| 1 | Failure policy | An engine that fails, times out or is missing where one was named: **record, change and run** tools are refused (fail closed); **observe and session** tools run, traced (fail open). One table decides it (`toolbox::reach_of`, `d80b02a`), replacing the earlier Write/Edit/Bash rule. | `src/toolbox/mod.rs` |
| 2 | `AskUserQuestion` | **DONE** as a Toolbox tool of that exact name. Exit 2 is the owner's answer from the app (a non-error result); otherwise the terminal's one-key menu; under ACP or unattended the model is told it went unanswered, with its options. **Open (app side):** a question card; the app answers every agent request but `session/request_permission` with `-32601`. | `src/toolbox/mod.rs`, `src/ask.rs` |
| 3 | Stop channel (`capture`) | **DONE** on every route, at the end of every turn. A `block` reply is sent to the same backend as the next prompt; at most `MAX_CONTINUATIONS` (3) per turn; never after a cancel. | `src/hooks/memory.rs` |
| 4 | `permission-request` | **DONE** on the `ask` branch, with byte-identical `tool_input`, raced against the terminal or the ACP client's Allow/Reject card; first answer wins; with nobody there, a refusal. **Open (app side):** the card cannot be withdrawn when the hook answers first; its late answer is dropped. | `Engine::settle`, `acp_agent::permission` |
| 5a | `session-start` / `user-prompt` | **DONE** (Option A) on every route: the blocks replace incode's native rules, recorder and auditor blocks. Oversized blocks go to `.incode/context/` with the head inline. Cost measured: about 8.2 s per turn for `user-prompt`. | `src/hooks/memory.rs`, `cli::prepare_session`, `cli::plan_turn` |
| 5b | `code-edit` | **DONE** after every change tool that landed (once per file for a patch; the output path for `RenderPdf` and `ConvertDocument`), on its own thread. | `Toolbox::run_call` |
| 5c | `pre-compact` | **NOT APPLICABLE.** incode never compacts; `run_on_compact` is allowlisted in `wiring.rs` with that reason. | — |
| 5d | `session-end` | **DONE**: REPL exit, `/clear` (old id), ACP `session/close` and shutdown, the end of `-t`, and each `/run` worker. | `src/hooks/memory.rs`, `cli::run_memory` |
| 6 | Background-work awareness | **DONE**: `capture` is not sent while a background job (the model's or `/bg`) runs. | `SessionTools`, `src/hooks/memory.rs` |
| — | Session id | incode's own (the ACP `sessionId`, or a v4 UUID), given to every backend, the toolbox and the model. | `AgentBackend::set_memory_session` |
| — | Transcript | `.incode/transcripts/<session>.jsonl` in the engine's shape, when an engine is present or the session is an ACP one; newest 200 kept. | `src/hooks/memory.rs` |
| — | `/run` workers | Each worker (sequential, and each lane) is a memory session of its own. | `cli::run_memory` |
| — | Recording a session twice | claude: its settings and hooks are off. codex: `--disable hooks` (`b39a99e`). **agy** has no switch to skip its settings, so if its hooks file is installed the engine may still hear an agy session twice. | `src/agent/codex/argv.rs` |
| — | Operator hooks | The operator's `hooks` (alias `desktop_hooks`) run around every Toolbox tool, before the engine. | `hooks::HookRunner` |

### Phase 8 (2026-09-26) — CLI parity and the OS sandbox

| Item | Status |
|---|---|
| User-defined sub-agents | **DONE** — `src/subagents.rs` loads `.incode/agents/*.md` (project) and `~/.incode/agents/*.md` (user); front matter `name`/`description`/`role`/`model`/`tools`, then the prompt body. The `Agent` tool takes `agent: <name>` (`toolbox::subagent`), runs it with that definition's instructions/model/role and only its tools (narrowed to the parent's, never wider). Listed in the `Agent` schema and by `/agents` (REPL and ACP). A bad file is reported, never a crash. Live: an operator-defined `reviewer` run through the `Agent` tool. |
| Headless JSON output | **DONE** — `incode -t --output-format stream-json` prints one JSON event per line (init, text, thinking, tool_use, tool_result, usage, result); `--output-format json` prints the final object only. stdout is JSON only (`acp_agent::stdio::claim`, as under `--acp`); everything else goes to stderr. Live with the real claude (bridged): thinking, `tool_use` Write, `tool_result`, text, usage, result, all JSON, `agent=claude-code model=opus`. |
| Persistent checkpoints + `/rewind` | **DONE** — `tools::checkpoint` writes each sealed turn under `.incode/checkpoints/<session>/` (bounded: newest 20 turns, ≤ 256 MiB), so `/undo` works after a restart; `/rewind <n>` restores n changed turns back. A turn is sealed at its end: unchanged files dropped, files and directories it CREATED added (so an undo deletes them), and a file changed since is left alone and said. A manifest naming a path outside the session's roots is not trusted. `/rewind` announced over ACP. |
| OS sandbox for shell commands | **DONE (Windows + Linux live; macOS validated on a Mac)** — `src/sandbox`. `admin.json` `sandbox: off\|workspace-write\|read-only` (env `INCODE_SANDBOX`), tied to the permission mode by default. Linux: Landlock (`--sandbox-exec` helper). macOS: `sandbox-exec` Seatbelt profile. Windows: a write-restricted token for **read-only** (measured: writes denied inside and outside the workspace, reads and `NUL` work); **workspace-write left OFF** on Windows with the reason stated — see `docs/ARCHITECTURE.md`. Network not restricted on any OS; reported, not pretended. |
| Operator hooks on every tool | **DONE** — `hooks::HookRunner` now runs the operator's `admin.json` `desktop_hooks` (alias `hooks`) pre- and post-tool for EVERY Toolbox tool, in `Toolbox::run_call` (exit 2 denies; stdin piped with the event JSON, bounded 10 s). The desktop's own executor still runs them for the desktop tools, so they are not doubled. |
| `/run` workers as memory sessions | **DONE** — `cli::run_memory::Remembered` makes each ledger worker (sequential and each lane) a memory session of its own: `session-start`, a `capture` per task, `session-end`, under its own id, with its agents' calls filed under it. A carry-on answer is recorded, not followed (the task's check decides). |
| codex hooks off | **DONE** — `codex exec`/`exec resume` now carry `--disable hooks` (`src/agent/codex/argv.rs`), so codex does not fire its own `~/.codex/hooks.json` and record a session incode already records. Proven live on codex 0.145.0 and 0.153.4 (probe hooks file, trust bypassed): hooks on recorded `session-start`/`user-prompt`, `--disable hooks` recorded nothing. |
| `--thinking-display` fallback | **DONE** — the claude backend drops `--thinking-display summarized` and retries the turn once if the CLI rejects it with "unknown option" naming the flag, and remembers that for the session (`agent::claude_code`). Tested with a fake CLI that rejects the flag. |
| `run_on_compact` | Allowlisted honestly in `wiring.rs` `DELIBERATELY_UNWIRED` with the reason: incode never compacts (the model CLI compacts inside its own process); the claude stream's `compact_boundary` frame is where it would wire once measured. |

### Phases 2–4

| § | Feature | Status |
|---|---|---|
| 2 | Shared engine-hook runner | **DONE** — `src/hooks/engine.rs`. Piped-and-closed stdin, piped stdout/stderr, `process_tree` + `tools::bash::wait_with_timeout_or`, bounds 610 s for pre-tool / permission-request / capture. Binary: `INCORD_ENGINE_BIN` → `AdminConfig.engine_bin` → install location (see the correction under §2). Phase 4 added `session-start`, `user-prompt`, `code-edit` (bound 70 s), `capture` (610 s) and `session-end` (130 s). |
| 3 | Pre-tool guard | **DONE** in `Toolbox::run_call`: operator guards → engine → snapshot → dispatch. Failure policy taken by the owner: fail CLOSED for `Write`/`Edit`/`Bash`, fail OPEN for the rest. (Widened in `d80b02a`: every record, change and run tool fails closed — see the final table.) |
| 4 | `AskUserQuestion` tool | **DONE** — Toolbox tool of that exact name. Exit 2 = the owner's answer, returned as a non-error result. Exit 0: the REPL answers with a one-key menu; under ACP and unattended the model is told it went unanswered (the owner's app has no question/elicitation request — see §4). |
| 5 | Stop channel | **DONE (phase 4)**, on every route, from `AgentSession::turn_observed` and `task_run` (not from `ApiBackend` as §5 first proposed, so that the CLI routes get it too). `hook capture` with `last_assistant_message`, `stop_hook_active` and `transcript_path`; a `block` reply is sent to the same backend as the next prompt; cap `MAX_CONTINUATIONS` = 3 per turn; not spawned once the turn was stopped. |
| 6 | PermissionRequest | **DONE**, on the pre-tool `ask` branch only, with a byte-identical `tool_input`, racing the REPL prompt or the ACP client's `session/request_permission`; unattended, silence is a refusal. |
| 7 | 5a SessionStart / UserPromptSubmit | **DONE (phase 4), Option A.** SessionStart in `prepare_session` (`startup`/`resume`) and on `/clear` (`clear`); its block replaces the native rules/recorder/auditor blocks in every backend's system context. UserPromptSubmit in `plan_turn` after the greeting check, replacing `rules::context_for_turn`. Measured on this machine: session-start 8.2–9.9 s, user-prompt 8.2 s on every turn (see §7 5a). |
| 7 | 5b code-edit | **DONE (phase 4)** in `Toolbox::run_call`, only after a Write/Edit that landed; on its own thread, bounded 70 s. Covers the claude bridge. |
| 7 | 5c pre-compact | NOT APPLICABLE — nothing compacts. |
| 7 | 5d SessionEnd | **DONE (phase 4)**: REPL exit, `/clear` (old id), the end of an ACP session thread (`session/close` and shutdown), the end of `--task`. |
| 8 | Background-work awareness | **DONE (phase 4)** as the harness gate: while a `/bg` job runs the stop is not sent. incode also writes the Claude-shaped transcript (`.incode/transcripts/<session>.jsonl`), which is what `hook capture` records the conversation from. |

What still limits all of it: only calls that go through incode's own Toolbox reach the
engine. **Phase 3 (2026-09-25) put the claude route on the Toolbox**: claude is started
as a model only — `--tools ""`, `--strict-mcp-config`, `--setting-sources ""` — and its
only tools are incode's, over the tool bridge (`src/tool_bridge.rs`; see
`docs/ARCHITECTURE.md` §5a). On that route every call passes the operator's guards, the
permission mode and the engine's pre-tool hook, and a PermissionRequest is raised to the
REPL or the ACP client exactly as on the API route (measured live: `engine hook pre-tool
… exit 0` in the parent's trace for the model's `Write`). The caveats in §0 about the
CLI route no longer hold for claude. They still hold for **codex** and **antigravity**,
which run their own tools (codex: not provable while its account is over its usage
limit, and `apply_patch` has no off switch; agy: no flag exists).

~~New caveat on the claude route: with the CLI's settings off, the engine's session hooks
it used to fire from its own config (SessionStart, UserPromptSubmit, Stop) no longer
fire, and incode does not run them yet (§5, §7).~~ **Closed in phase 4:** incode fires
the whole lifecycle itself (`src/hooks/memory.rs`), keyed by incode's own session id
(the ACP `sessionId`, or a UUID the REPL / `--task` makes), and writes the transcript the
engine records from. Measured live, 2026-09-25: `incode -t "create a.txt with hi"` on the
claude route created `a.txt`, and the service then answered
`GET /v1/memory/conversations/<id>/messages` with both turns,
`GET /v1/memory/session?session_id=<id>` with a `Touched` entry for `a.txt` (from
`code-edit`), and `GET /v1/memory/session/catchup?cwd=<workspace>` with the request.
Codex and antigravity still load their own hooks files, so on those routes the engine
may hear a session twice — once from incode, once from the CLI under the CLI's id.
(Closed for codex in phase 8: it runs with `--disable hooks`. Still true for agy.)

**Phase 6b (2026-09-25).** Three more kinds of call now pass the engine's pre-tool hook,
because they are Toolbox tools: the desktop's twelve tools (previously run in
`incode --desktop-mcp` with only the desktop's own hooks, which still run there as a
second line), `Screenshot` (observe, fail open) and `RenderPdf` (change, fail closed;
its OUTPUT path is in `tool_input.paths` and is reported to `code-edit` once written). A
tool result may now carry pictures; the engine, the event stream and the transcript
incode writes see only the text and a `[image WxH png]` placeholder per picture, never
the base64 (`ToolOutput::shown`, `docs/ARCHITECTURE.md` §5b).

---

## 0. The shape of the problem: two routes, and only one of them is ours

incord-code runs a turn in one of two ways (`src/agent/mod.rs:1-15`):

| Route | Who owns the tool loop | Do the engine's hooks fire today? |
|---|---|---|
| **CLI delegation**: Claude Code (`src/agent/claude_code.rs`), Codex (`src/agent/codex.rs`), Antigravity (`src/agent/agy.rs`) | the child CLI | **Yes, inside the child.** They come from the child's OWN config, which the engine's installer writes (`engine:cli/agent_files.rs:289` Claude, `:383` Codex, `:536` Antigravity). incord-code neither adds nor suppresses them: it does not pass `--bare` (`src/agent/claude_code.rs:980-982`), and the stream parser already sees the hook events and drops them (`src/agent/claude_code.rs:1493-1497`). `src/session.rs:5-6` states this reliance outright: "Memory captures the work live … through the hooks the agent already fires." |
| **API route**: `ApiBackend` + `Toolbox` (`src/agent/api.rs:504-525`, `src/toolbox.rs:504`), id `"api"` (`src/agent/registry.rs:161`) | **incord-code** | **No.** Nothing in `src/` invokes `incord-memory-service` or any `hook <event>` subcommand (grep for `incord-memory-service`, `pre-tool`, `permission-request`, `code-edit`, `pre-compact`, `session-end`: no hits in production code). |

`incode --acp` (driven by the owner's desktop app) runs the same session and turn as
the REPL (`src/acp_agent/mod.rs:649` `plan_turn`, `:667` `turn_observed`), so it
inherits whichever route that session uses.

> **Superseded for claude (phase 3, 2026-09-25).** The table above describes the audit
> as it stood. claude now runs as a model only, with no settings and so no hooks of its
> own, and its tool calls run in incode's Toolbox over the tool bridge — the "API route"
> column applies to it. The rest of this section still describes codex and
> antigravity.

**So the gaps below are the API route (and ACP when it is on the API route).** On
the CLI route, whether each feature works depends on the child CLI. Three facts
about how incord-code starts the child limit that:

- **Nobody can type into the child.** Claude is started with `-p` (`src/agent/claude_code.rs:931`) and `stdin(Stdio::null())` (`:1060`). Codex also gets `stdin(Stdio::null())` (`src/agent/codex/argv.rs`, `command`).
- **Codex never asks for approval** when resuming, because it is started with `--dangerously-bypass-approvals-and-sandbox` (`src/agent/codex/argv.rs`, `command`).
- **Nothing in incord-code cuts a 600 s hook short on this route.**
  - The Claude/Codex stream readers have no turn timeout. The only `recv_timeout` in `claude_code.rs` is test code (`:1872`).
  - Antigravity gets `--print-timeout 30m` (`src/agent/agy.rs:36`, `:241`).

  Whether each CLI fires `PermissionRequest` / `AskUserQuestion` in print mode with no stdin is **UNVERIFIED**.

### Does the engine's installer write hooks for incord-code?

**No.** `engine:` has hook writers for these agents only:

- Claude — `cli/agent_files.rs:289`
- Codex — `:383`
- OpenCode — `:417`
- Windsurf — `:472`
- Antigravity — `:536`
- Cursor — `:681`

The Claude settings merge is `engine:hooks.rs:427-500`. Every `incord-code` hit in `engine:src` is a test fixture or a comment (e.g. `api/skills_api.rs:593`, `proxy.rs:4103`). None of them is an installer.

The file `D:\Projects\incord-code\.agents\hooks.json` is **Antigravity's workspace hooks file**:

- It carries `pre-tool --agy` and `session-start --agy`.
- It matches the Antigravity writer's workspace target, `.agents/hooks.json` (`engine:cli/agent_files.rs:530`).
- incord-code never reads it (no `hooks.json` reference anywhere in `src/`).

The engine's per-agent reply shapes are keyed by a flag (`engine:cli/hook_agent.rs:23-30`: `Default`, `Codex`, `Antigravity`). **incord-code should use the `Default` (Claude Code) contract, so it needs no flag.**

---

## 1. Summary

> **Historical.** This table is the audit's starting point on 2026-09-24. Every
> MISSING and PARTIAL row below has since been built or ruled not applicable; the
> current state is the "Final status" table at the top of this document.

| # | Feature | Status (incord-code's own loop) | Evidence |
|---|---|---|---|
| 1 | Guard **before** a tool runs (`hook pre-tool`: exit 2 block / exit 0 allow / stdout `ask`) | **PARTIAL** | Native guards run before every Toolbox call (`src/toolbox.rs:513` → `refused` `:323-347`). They cover Write/Edit/Bash only and are not the engine: there is no backup check, no secret scan and no forward-all. The only external pre-tool hook is **desktop-only** (`src/desktop/mcp.rs:297-312`) with an incompatible contract. The tool name and input go in env vars, not JSON on stdin (`src/hooks/mod.rs:17-19`, `:191-193`). The deny reason is read from stdout only (`:207`), but the engine writes it to stderr (`engine:cli/pre_tool_decide.rs:25`). stdin is not set, so the child inherits it (`:191-195`). The timeout is 10 s (`:45`). There is no `ask` handling. `IncordCodeConfig.hooks` (`src/config.rs:73`) has no consumer. |
| 2 | Multiple-choice question → owner/proxy card | **PARTIAL** | Questions exist as a text protocol: the model writes an `<ask>` block (`src/ask.rs:15-29`, `:40`). They are shown in the **local terminal only** (`src/cli/agent_turn.rs:745-751`, answered through `plan_turn` → `take_answer`, `src/cli/mod.rs:2149`). Unattended runs only print a note (`src/ask.rs:552`, `src/task_run.rs:280`). There is no question tool in the Toolbox (`src/toolbox.rs:42-56`), so the engine's `is_question_tool` (`engine:cli/stop_channel.rs:120-122`) never sees one. Nothing reaches the owner app. |
| 3 | Stop channel (`hook capture` → `{"decision":"block","reason"}` = continue) | **MISSING** | An API turn ends when the model stops calling tools (`src/agent/api.rs:633-641`). The session turn then returns (`src/cli/agent_turn.rs:775`). No card is raised and nothing continues the turn. The ledger runner's `owner::ask_and_wait` escalation (`src/runner.rs:736`, `:1309-1335`) is a different mechanism: a guard card for a **blocked task**, not a turn-end channel. |
| 4 | PermissionRequest (`hook permission-request` answers a pending prompt) | **MISSING** (nothing to attach to yet) | The API route has no permission prompt at all: `set_permission_mode` keeps its default (`src/agent/api.rs:893-899`). `PermissionPrompter` has only test implementations (`src/permissions/mod.rs:141`, `:382`, `:419`). ACP does not offer `session/request_permission` (`docs/ARCHITECTURE.md` §2, "Not offered"). |
| 5a | SessionStart / UserPromptSubmit context injection | **PARTIAL** (native HTTP equivalent, not the hooks) | Session start: rules, recorder, auditor and skills (`src/cli/mod.rs:267`, `:277`, `:346`, `:360`). Each turn: rules (`src/cli/mod.rs:2191`, REPL `:1561`) and recall (`src/recall.rs:220`), all over HTTP at `127.0.0.1:9180` (`src/brief.rs:31`). Whether this matches everything `hook session-start` / `hook user-prompt` inject is **UNVERIFIED**; see §5a. |
| 5b | PostToolUse `hook code-edit` | **MISSING** | Nothing runs after a successful Toolbox Write/Edit except tracing (`src/toolbox.rs:535-560`). |
| 5c | PreCompact `hook pre-compact` | **MISSING** (not applicable today) | The API route never compacts. The conversation is the `messages` vec, cleared whole by `clear_session` (`src/agent/api.rs:869-875`). `LifecycleHooks::run_on_compact` (`src/runtime/lifecycle.rs:60`) has no caller. |
| 5d | SessionEnd `hook session-end` | **MISSING** | `LifecycleHooks::run_session_end` runs at REPL exit (`src/cli/mod.rs:1616`), but it is a plain operator command. It sends no event on stdin and has no timeout (`src/runtime/lifecycle.rs:101-114` uses `.output()`). It is not called for ACP sessions. |
| 6 | Background-work awareness at stop | **MISSING** | The engine detects background work from a **Claude-format transcript** (`engine:cli/stop_channel.rs:279-285`, `engine:cli/hook_capture.rs:386-390`). incord-code writes no such transcript: `src/transcript.rs:100-131` writes `{kind,…}` lines, and `engine:hooks.rs:33-50` needs `type` + `uuid` + `message`. Background jobs exist only as the operator's `/bg` in the REPL (`src/cli/mod.rs:642`, `:1477`, `:1510`). The model cannot start one: the Bash tool has no background flag (`src/toolbox.rs:426-437`). |

Nothing is **SUPPORTED** end to end on the API route.

---

## 2. Shared piece: one engine-hook runner

Every feature below needs the same call. Build it once, as a sibling of the operator
hooks rather than a second copy of them.

**Where.** New file `src/hooks/engine.rs` (`pub mod engine;` in `src/hooks/mod.rs`).
It reuses the crate's existing process machinery:
- `crate::agent::process_tree::prepare` (`src/agent/process_tree.rs:35`);
- `crate::tools::bash::wait_with_timeout` (`src/tools/bash.rs:110`), which drains stdout and stderr on threads.

These are the same calls `src/hooks/mod.rs:202-205` already makes.

**Locating the binary.** Nothing in incord-code knows where the engine binary lives
today. Resolve it in this order:
1. The environment variable `INCORD_ENGINE_BIN` (new).
2. A new `AdminConfig` field `engine_bin: Option<String>`, next to `memory_url` (`src/admin.rs:160`).
3. The default install location, `%USERPROFILE%\.incord\incord-memory-service.exe`. That is the path the engine's own installer wrote on this machine (`D:\Projects\incord-code\.agents\hooks.json:4`). The non-Windows default path is **UNVERIFIED**.

   **Corrected in phase 2.** `.incord\` is only the spaceless COPY the installer makes for Antigravity, whose shell cannot quote a path (`engine:cli/agent_files.rs:566-584`). The Claude and Codex hooks the installer writes use its own `current_exe()` (`engine:cli/hooks_install.rs:241-245`), which on this machine is `%LOCALAPPDATA%\Incord Memory\incord-memory-service.exe` — 8 hooks each in `~/.claude/settings.json` and `~/.codex/hooks.json`. The two files were byte-identical when checked. The runner looks at the `%LOCALAPPDATA%` path first, then the `.incord` copy. incode spawns without a shell, so the space needs no quoting.

If no binary is found, trace it once and skip the hook. That follows the fail-open rule for body-facing modules (`docs/ARCHITECTURE.md` §3). The one exception is feature 1: see "Failure policy" there.

**Invocation contract (all events):**

```text
spawn:  <engine_bin> hook <event>          (no agent flag -> engine's Default/Claude contract)
cwd:    the workspace root
stdin:  PIPED; write the event JSON, then CLOSE it (drop the handle) before waiting
stdout: piped, parsed per event
stderr: piped; for `pre-tool` exit 2 it IS the reason
wait:   wait_with_timeout(child, <per-event bound below>)
```

Two things the existing operator runner does wrong for this purpose must not be copied:
- **Pipe stdin; do not inherit it** as `src/hooks/mod.rs:191-195` does. The engine reads the event from stdin. On the desktop MCP server and under ACP, the inherited stdin is the protocol stream (`docs/ARCHITECTURE.md` §2, "stdout is the protocol").
- **Read stderr when the exit code is 2.**

**Per-event bounds.** The harness must not time out a hook sooner than the engine registers it for Claude Code (`engine:hooks.rs`):

| Event | Subcommand | Engine registration | Harness bound |
|---|---|---|---|
| PreToolUse | `hook pre-tool` | 600 s (`engine:hooks.rs:490`) | **≥ 600 s** |
| Stop | `hook capture` | 600 s (`:460`) | **≥ 600 s** |
| PermissionRequest | `hook permission-request` | 600 s (`:500`); the engine itself waits ≤ 540 s (`engine:cli/permission_request.rs:26`) | **≥ 600 s** |
| SessionEnd | `hook session-end` | 120 s (`:478`) | ≥ 120 s |
| PreCompact | `hook pre-compact` | 30 s (`:456`) | ≥ 30 s |
| SessionStart / UserPromptSubmit / PostToolUse | `hook session-start` / `user-prompt` / `code-edit` | none set; the agent's default applies (value **UNVERIFIED**). The engine's Antigravity file uses 60 (`.agents/hooks.json:5-6`) | 60 s |

Use one constant per event, and add a small margin (for example +10 s) so the engine's own deadline fires first. Do **not** reuse `HOOK_TIMEOUT` (10 s, `src/hooks/mod.rs:45`).

**Anything above incord-code that has its own deadline must also allow for these
waits.** One case: the desktop app driving `incode --acp`. Its timeout on `session/prompt` is **UNVERIFIED**; it lives outside this repo.

**Checked in phase 2: the app sets no timeout on `session/prompt`.** `AcpClient.request` (`incord-dashboard/src/components/agent/acp.ts:184-194`) parks the promise in `pending` until a response or `close()` arrives, with no timer. `sendPrompt` awaits it with none either (`agentStore.ts:429-433`). The Tauri bridge's `agent_send` only writes the line (`src-tauri/src/agent.rs:677-689`). Its only timeouts are the login-shell probe (5 s, `agent.rs:322`) and model listing (30 s, `agent.rs:394`). A 600 s hook is not cut by the app.

**Common event fields.** The engine reads these field names, each under several spellings:
- `session_id` — `engine:cli/hooks_install.rs:29-38`
- `cwd` — `engine:cli/pre_tool_decide.rs:726-745`
- `transcript_path` — `engine:cli/hooks_install.rs:47`
- `last_assistant_message` — `engine:cli/hooks_install.rs:71`

Send these in every event:

```json
{ "session_id": "<ApiBackend.session>", "cwd": "<absolute workspace>", "hook_event_name": "<Event>" }
```

`session_id` comes from `ApiBackend.session` (`src/agent/api.rs:519`, minted by `new_session_id`, `:915`). `hook_event_name` mirrors what Claude sends. Whether the engine reads it is **UNVERIFIED**, and it is harmless if it does not.

Omit `transcript_path` until incord-code writes a Claude-format JSONL (§6). The engine's interactive Stop legs no longer require it (`engine:cli/hook_capture.rs:38-46`).

**Acceptance tests (shared):**
- A fake engine script that echoes its stdin back to a file proves that the event JSON arrived **on stdin** and that stdin was closed. The child must exit instead of hanging.
- A fake engine that sleeps past the bound proves that the child tree is killed at the bound, and that the outcome is the documented failure policy.
- A test that the bound constants are ≥ 600 for pre-tool, capture and permission-request. This pins the "never shorter than the engine" rule.

---

## 3. Feature 1 — Guard before a tool runs (PARTIAL)

> **Status: DONE (phase 2).** `Toolbox::run_call` (`src/toolbox.rs`), failure policy fail-closed for Write/Edit/Bash and fail-open for reads. All five acceptance tests below are in `toolbox::tests` / `agent::api::tests`. The file path in `tool_input` is made absolute for Read/Write/Edit. The session id is set by `Toolbox::set_session`, from `ApiBackend::new` and `clear_session`. What remains: calls made by a CLI child never reach this gate.

**Where.** In `Toolbox::run` (`src/toolbox.rs:504`), **after** the native guard refusal (`:513-516`) and **before** the checkpoint snapshot and dispatch (`:524-535`).

Native guards go first: they are free, and spawning the engine for a call already refused would waste a process. That is the same ordering argument as `src/desktop/mcp.rs:287-291`.

The Toolbox needs the session id. Add a setter in the style of `with_checkpoints` (`src/toolbox.rs:143`), called from `ApiBackend::new`/`clear_session` (`src/agent/api.rs:529`, `:869`).

Exempt from the call:
- `READ`, `GLOB`, `GREP`, `SKILL`, and the recall/knowledge reads.

  **UNVERIFIED whether the engine wants reads.** It registers pre-tool with matcher `*` (`engine:hooks.rs:489-491`), and it has its own `is_trivial_read` short-circuit (`engine:cli/pre_tool.rs:767`). So the safe default is to **send every tool** and let the engine decide.

**Event:**

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "api-…",
  "cwd": "D:\\Projects\\example",
  "tool_name": "Bash",
  "tool_input": { "command": "cargo test" }
}
```

`tool_name` and `tool_input` are passed through **unchanged**. The Toolbox names are already Claude's (`Read`/`Write`/`Edit`/`Glob`/`Grep`/`Bash`, `src/toolbox.rs:42-52`). Write/Edit carry `path` (`src/toolbox.rs:342`). The engine accepts `path` as well as `file_path` (`engine:cli/pre_tool.rs:237`, `:251`).

Send an **absolute** path: the engine keys backups and the project by path. That need is an inference, **UNVERIFIED**.

**Reply parsing:**

| Engine result | Meaning | Harness action |
|---|---|---|
| exit 2 | block (`engine:cli/pre_tool_decide.rs:25-26`) | `Err(stderr.trim())` as the tool result, marked as an error, the same way a guard refusal returns (`src/toolbox.rs:514-515`) |
| exit 0, stdout empty | allow | dispatch |
| exit 0, stdout `{"hookSpecificOutput":{"permissionDecision":"allow", "permissionDecisionReason", "additionalContext"?}}` | allow, with rules for this action (`engine:cli/pre_tool_decide.rs:598-607`) | dispatch. If `additionalContext` is present, add it to the tool result text so the model reads it. How Claude Code surfaces it is **UNVERIFIED**. |
| exit 0, stdout `{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":R,"additionalContext"?}}` | a human must confirm (`engine:cli/pre_tool_decide.rs:657-669`) | see "ask" below |
| any other exit / timeout / spawn failure | engine error | see "Failure policy" |

**"ask".** The Owner Console card stays live at the same time (`engine:cli/pre_tool_decide.rs:620-622`):
- **REPL:** show `R` and a yes/no prompt. At the same time, run `hook permission-request` for the same call (§6, feature 4). **The first answer wins.**
- **ACP or unattended** (no one at the prompt): run `hook permission-request` and wait. With no decision, **deny**, with `R` as the reason. That is fail-closed, consistent with `owner.rs` (`src/owner.rs:1-22`, `docs/ARCHITECTURE.md` §3).

**Failure policy.** This is an owner decision. Two options:
- **Mirror Claude Code:** a non-2 exit is a non-blocking error, so allow and trace it. The desktop hook runner already does this (`src/hooks/mod.rs:162-164`).
- **Fail closed:** refuse the call, because the guard is a safety function. This matches `Toolbox::refused` when the guards file cannot be read (`src/toolbox.rs:330-337`).

**Recommendation: fail closed for `Write`/`Edit`/`Bash` and fail open for reads.** Flag it for the owner before building.

**Acceptance tests (with a fake engine):**
1. exit 2 + stderr `"no backup"` → the tool is **not** executed (the file is unchanged on disk), and the tool result is an error containing `no backup`.
2. exit 0 + empty stdout → the tool runs.
3. exit 0 + `ask` JSON with no prompter (ACP/unattended) → the tool is not run, and the reason reaches the model.
4. A native guard refusal → the engine is **never spawned** (a counter file is untouched).
5. The event on stdin carries `tool_name`, `tool_input.command`, an absolute `cwd`, and `session_id` equal to the backend's session.

---

## 4. Feature 2 — Questions to the owner app (PARTIAL)

> **Status: DONE for the Toolbox (phase 2).** `AskUserQuestion` is offered and routed; the four acceptance tests are in `toolbox::tests`. Under ACP a question cannot go to the client: the owner's app answers every agent request other than `session/request_permission` with `-32601` (`acp.ts:229-239`), and it advertises no elicitation capability (`acp.ts:265-269`). So the model is told the question went unanswered, with its options. What remains: an app-side question request (the protocol's elicitation, or a choice card), after which `ClientPrompter::choose` can send it. The `<ask>` block stays for CLI backends.

**Gap.** The model's questions are `<ask>` text blocks. They are parsed after the turn (`src/cli/agent_turn.rs:745`) and answered at the local prompt (`src/cli/mod.rs:2149-2163`). The engine routes questions only when a **question tool** passes through `hook pre-tool`:
- `is_question_tool` accepts `AskUserQuestion` / `ask_question` (`engine:cli/stop_channel.rs:120-122`).
- The routing only happens when forward-all is on for that folder (`engine:cli/pre_tool.rs:747`).

**Where.** Add a Toolbox tool named exactly `AskUserQuestion`:
- a constant beside `AGENT` (`src/toolbox.rs:56`);
- a definition in `definitions()` (`:355`);
- an arm in the `run` match (`:535`).

Its input schema is Claude's shape, because that is the one the engine parses (`engine:cli/stop_channel.rs:116-118`):

```json
{
  "questions": [
    {
      "question": "Which database should this use?",
      "header": "Database",
      "options": [
        { "label": "Postgres", "description": "relational; the document implies joins" },
        { "label": "SQLite",   "description": "one file, nothing to operate" }
      ],
      "multiSelect": false
    }
  ]
}
```

Cap the options at `ask::MAX_OPTIONS` (6, `src/ask.rs:47`). Keep the `<ask>` block for CLI backends: they have their own question tools, and the block is their channel into this harness.

**Flow.** The call goes through the §3 pre-tool step like any other tool (event `tool_name: "AskUserQuestion"`, `tool_input` = the object above).

| Engine result | Meaning | Harness action |
|---|---|---|
| exit 2, stderr `"<who> answered your question: <label or words>\nContinue with that answer — do not ask it again."` (`engine:cli/stop_channel.rs:223-234`, emitted by `pre_tool_block`, `engine:cli/pre_tool.rs:754-756`) | answered in the app or by the proxy | return that text as the tool result. It is an **answer**, not an error: mark it `is_error: false`, or the model reads its own question as failed. |
| exit 0 | no answer in time, the owner chose the terminal, or forward-all is off (`engine:cli/pre_tool.rs:758-760`) | answer locally. **REPL:** convert the input to `ask::Question`/`Choice` (`src/ask.rs:50-64`), render it (`:81`), read the pick, and return it as the tool result. **ACP/unattended:** return `ask::unattended_note` (`src/ask.rs:552`) as the result. |

The engine waits up to `ANSWER_WAIT_SECS = 500` for an answer (`engine:cli/stop_channel.rs:31`). That is inside the 600 s registration, which is why the harness bound for pre-tool must be ≥ 600 s.

**Acceptance tests:**
1. A fake engine that exits 2 with the "answered your question: Postgres" text → the tool result is exactly that text, not an error, and **no local prompt is shown**.
2. A fake engine that exits 0 under ACP → the result is the unattended note listing both labels.
3. The event's `tool_input.questions[0].options[*].label` round-trips unchanged. The engine turns them into buttons (`engine:cli/stop_channel.rs:237-240`).
4. The tool name is exactly `AskUserQuestion`. Pin it with a test: a rename silently disables routing.

---

## 5. Feature 3 — Stop channel (MISSING)

**Where.** In `ApiBackend::run_turn_with`, at the point the model stops asking for tools (`src/agent/api.rs:633`). Do this before returning `TurnOutcome`.

It goes here and not in `AgentSession::turn_observed` for two reasons:
- the continuation needs the conversation (`self.messages`);
- on the CLI route the child's own Stop hook already fires, so a second call from the session layer would raise two cards for one stop.

**Event:**

```json
{
  "hook_event_name": "Stop",
  "session_id": "api-…",
  "cwd": "D:\\Projects\\example",
  "last_assistant_message": "<reply.text of this final response>",
  "stop_hook_active": false
}
```

On the first stop of a turn, send `stop_hook_active: false`. On any stop reached **after** a continuation this hook ordered, send `true`. The engine uses that flag as its re-entrancy brake (`engine:cli/hook_capture.rs:482`, `:523`, `:567`), and runs capture on the re-entrant stop (`:62-66`).

`transcript_path` is omitted until §6 exists. The engine reads the last message from the event first (`engine:cli/hooks_install.rs:57-71`).

**Reply parsing:**

| Engine result | Harness action |
|---|---|
| stdout `{"decision":"block","reason":R}` (`engine:cli/hook_agent.rs:46-51`, `Default` arm) | **Continue.** Push `{"role":"user","content":R}` onto `self.messages`, and go round the loop again with `stop_hook_active = true` on the next stop. |
| empty stdout / exit 0 | the turn ends as today |
| timeout / error | the turn ends as today, traced. A stop hook never blocks the harness from ending a turn. |

**Bounds.**
- Each continuation round counts against `MAX_STEPS` (40, `src/agent/api.rs:48`), so a looping channel ends as `step_limit` rather than running unbounded.
- Also cap continuations per turn with a small constant, for example 3. The engine's own brake is authoritative; this cap is a second fuse, the same "bound the action" principle as `src/budget.rs`.
- Check the cancel token (`src/agent/api.rs:588-600`) **before** spawning the hook. A Ctrl-C at a stop must not wait up to 600 s.

**Surface.**
- Emit the continuation text as an `AgentEvent::Text` so the REPL shows why the agent kept going.
- ACP shows it as an `agent_message_chunk`, through the observer (`src/cli/agent_turn.rs:492-494`).

**Acceptance tests (with a canned `Transport` like the existing API tests, `src/agent/api.rs:933-980`):**
1. The fake engine prints `{"decision":"block","reason":"now run the tests"}` once, then nothing → the model receives a user message `now run the tests` and one more request, and the second stop is sent with `stop_hook_active: true`.
2. The fake engine prints nothing → exactly one request, and the outcome matches today's.
3. A fake engine that always blocks → the turn ends at the continuation cap or `MAX_STEPS`, never unbounded.
4. The cancel token is tripped before the stop → the engine is not spawned.

---

## 6. Feature 4 — PermissionRequest (MISSING; attach to the "ask" path)

> **Status: DONE (phase 2).** `Engine::settle` (`src/hooks/engine.rs`) races the hook against the session's prompter: the REPL terminal, or under `--acp` the client's `session/request_permission` (roadmap A6 on the API route). The first answer wins and the loser is stopped. The acceptance tests are in `toolbox::tests`, and two ACP tests have the fake client allow and reject. Changed from the text below: under ACP the person at the client is asked, not only the hook. One limit remains: when the hook answers first, the app's card cannot be withdrawn, because the app handles no agent-side cancel. Its late answer is dropped.

**Gap.** incord-code has no permission prompt on its own loop (`src/agent/api.rs:893-899`), so there is nothing for the hook to answer yet. The only prompt this spec creates is the pre-tool **"ask"** (§3).

The engine never raises a card from this hook. It answers only when a call card for this exact call is already open (`engine:cli/permission_request.rs:13-17`, `:31`, `:40`). The card is raised by `hook pre-tool` in the same call's flow. So call it **only** on the "ask" branch of §3.

**Event:**

```json
{
  "hook_event_name": "PermissionRequest",
  "session_id": "api-…",
  "cwd": "D:\\Projects\\example",
  "tool_name": "Bash",
  "tool_input": { "command": "git push" }
}
```

The engine matches the card by the call's command or file (`engine:cli/permission_request.rs:67`, fields `file_path`/`filePath`/`path`/`notebook_path`). So `tool_input` must be the **same** object the pre-tool event carried.

**Reply parsing:**

| stdout | Harness action |
|---|---|
| `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}` | run the tool; cancel any local prompt |
| `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":M}}}` | refuse, with `M` as the tool error |
| empty | no answer from the app. **REPL:** keep the local prompt. **ACP/unattended:** deny (fail closed). |

Exit code 2 is **not** part of this contract (`engine:cli/permission_request.rs:13`). Parse stdout only.

**Race in the REPL.** Run the hook on a thread while the terminal prompt is shown. The first decision wins. On a terminal answer, the hook thread can be abandoned: the child tree is killed by `wait_with_timeout` at its bound, or explicitly through `process_tree`.

**Acceptance tests:**
1. The fake pre-tool hook prints `ask`, and the fake permission-request prints `allow` → the tool runs with no local input.
2. The same, but permission-request prints `deny` with message `no` → the tool result is an error containing `no`.
3. Under ACP, with empty permission-request stdout → the tool does not run.
4. The permission-request event's `tool_input` is byte-identical to the pre-tool event's.

---

## 7. Feature 5 — Context injection and the other lifecycle events

### 5a. SessionStart / UserPromptSubmit (PARTIAL)

incord-code already pulls the same **kinds** of context over HTTP:

- **Session start:**
  - rules — `src/rules.rs:191`, called at `src/cli/mod.rs:346`;
  - the recorder handoff — `src/cli/mod.rs:267`;
  - audit findings — `:277`;
  - skills — `:360`.
- **Each turn:**
  - rules — `src/cli/mod.rs:2191`;
  - recall — `src/recall.rs:220`, called on the REPL path at `src/cli/mod.rs:1561` area.

All of these are bounded to keep turns fast (`src/rules.rs:127-131`).

**UNVERIFIED:** whether this equals what `hook session-start` / `hook user-prompt` inject. The engine's session hooks also carry:
- project facts — `engine:cli/hooks_session.rs:1550-1589`;
- session catch-up — `:1818-1863`;
- the session record — `:1891-1941`;
- handoff — `:1969-1983`;
- the owner's answer delivered as an instruction — `:1042`.

No incord-code module was found calling the equivalent endpoints for project facts or catch-up.

**Option A (smallest):** call the hooks and inject their output, replacing the per-turn native rules block, so the two do not duplicate each other.
- **Where:**
  - `hook session-start`: once per session, where `prepare_session` assembles the system prompt (`src/cli/mod.rs:111`, around `:346`). Also after `/clear`, with `"source":"clear"`.
  - `hook user-prompt`: in `plan_turn` (`src/cli/mod.rs:2190-2193`), replacing the `rules::context_for_turn` push.
  - Both **only when the backend id is `"api"`**. For a CLI backend the child already fires these hooks, and injecting again would put the rules into the prompt twice.
- **Events:**
  ```json
  { "hook_event_name": "SessionStart", "session_id": "…", "cwd": "…", "source": "startup" }
  { "hook_event_name": "UserPromptSubmit", "session_id": "…", "cwd": "…", "prompt": "<the line typed>" }
  ```
  The `source` and `prompt` field names follow Claude Code's documented events. Which fields the engine reads for these two is **UNVERIFIED** beyond `cwd`/`session_id`.
- **Reply:** stdout `{"hookSpecificOutput":{"hookEventName":<event>,"additionalContext":<block>}}` (`engine:cli/hooks_session.rs:762-780`, default arm). Push `additionalContext` into the turn's `context` vec (`src/cli/mod.rs:2190`), rules first, which is the existing ordering (`docs/ARCHITECTURE.md` §4).
- **Bound:** 60 s (see the table in §2). This conflicts with the fast per-turn budget that native rules have (`src/rules.rs:128-129`): **measure it before choosing.**

**Option B:** keep the native path and add the missing endpoints. That needs endpoint paths this audit did not verify.

**Measured (phase 4, Option A built).** Real engine, scratch workspace, three runs each:
`hook session-start` 8.5 / 9.7 / 9.8 s (32 KB), `hook user-prompt` 8.25 / 8.24 / 8.25 s
(30 KB on a session's first prompt — full rule bodies — then 13 KB). In live incode runs:
session-start 8.2 s, 9.1 s, 9.9 s; user-prompt 8.2 s every turn; capture 18.9–24.4 s;
session-end 0.1–0.2 s; code-edit 0.1 s. The per-turn 8.2 s is what an agent CLI already
paid for this hook from its own settings; it is kept, not dropped, and reported here as
the cost. The near-constant 8.2 s suggests an internal timeout in the engine's per-turn
path — **UNVERIFIED**, not investigated (the engine was not to be edited).

**Acceptance tests (Option A):**
1. A fake engine printing `additionalContext: "RULES-X"` → the assembled prompt (`assemble_prompt`, `src/cli/agent_turn.rs:860`) contains `RULES-X` before the instruction.
2. On a CLI backend → the engine is not spawned.
3. A greeting line → the engine is not spawned (the `is_greeting` short-circuit, `src/cli/mod.rs:2186`, is preserved).

### 5b. PostToolUse `hook code-edit` (MISSING)

- **Where:** in `Toolbox::run` after dispatch (`src/toolbox.rs:535-560`), only when `name` is `WRITE` or `EDIT` **and** the outcome is `Ok`. That is the same "only for a call that happened" rule as `src/desktop/mcp.rs:321-332`. Run it off the turn's critical path: spawn it and do not wait, or wait on a detached thread bounded at 60 s. It is bookkeeping, and the engine's own write is bounded at 3 s (`engine:cli/capture_session.rs:244`).
- **Event:**
  ```json
  {
    "hook_event_name": "PostToolUse",
    "session_id": "…",
    "cwd": "…",
    "tool_name": "Write",
    "tool_input": { "path": "D:\\Projects\\example\\src\\lib.rs" },
    "tool_response": "<output>"
  }
  ```
  - The engine reads `tool_input.file_path` or `tool_input.path` (`engine:cli/capture_session.rs:256-262`).
  - It ingests only source extensions (`:265-273`).
  - Send an absolute path. Whether it resolves relative paths is **UNVERIFIED**.
  - `tool_response` mirrors Claude's event. Whether code-edit reads it is **UNVERIFIED**.
- **Reply:** none parsed.
- **Acceptance tests:**
  1. A successful Write → exactly one spawn, with an absolute `tool_input.path`.
  2. A refused Write (guard or engine) → no spawn.
  3. A Read → no spawn.

### 5c. PreCompact `hook pre-compact` (MISSING — not applicable yet)

The API route has no compaction (`src/agent/api.rs:869-875`). When compaction or trimming of `messages` is added, call this **before** the trim:

```json
{ "hook_event_name": "PreCompact", "session_id": "…", "cwd": "…", "trigger": "auto" }
```

The engine reads `trigger` (`engine:cli/hook_precompact.rs:55`). The bound is ≥ 30 s. No reply is parsed. **Acceptance test:** trimming calls the hook first; with no trim, there is no call.

### 5d. SessionEnd `hook session-end` (MISSING)

- **Where:**
  - REPL exit, beside the lifecycle call (`src/cli/mod.rs:1616`);
  - `ApiBackend::clear_session` (`src/agent/api.rs:869`), before the id is replaced, so the event names the session that ended;
  - ACP `session/close` (`src/acp_agent/mod.rs`, the close handler).

  Only on the `"api"` route.
- **Event:**
  ```json
  { "hook_event_name": "SessionEnd", "session_id": "…", "cwd": "…", "reason": "clear" }
  ```
  The `reason` values follow Claude's (`clear`/`logout`/`prompt_input_exit`/`other`). Which the engine reads is **UNVERIFIED**. The engine does nothing unless the owner enabled it for the folder (`engine:cli.rs:599-604`).
- **Bound:** ≥ 120 s. It must not hold up the process exit beyond that.
- **Acceptance test:** `/clear` sends one SessionEnd carrying the **old** session id, then a SessionStart with `source: "clear"`.

---

## 8. Feature 6 — Background-work awareness at stop (MISSING)

**What the engine needs.** It suppresses the stop card while background work is pending (`engine:cli/hook_capture.rs:383-390`). It learns about pending work only from a Claude-format transcript:
- `Command running in background with ID: <id>` / `agentId: <id>` starts a job;
- `<task-id><id></task-id>` ends one (`engine:cli/stop_channel.rs:273-285`).

incord-code writes no such transcript.

**Today the model cannot start background work on the API route:**
- the Bash tool has no background flag (`src/toolbox.rs:426-437`);
- subagents run synchronously (`src/toolbox.rs:178`).

So the only background jobs are the operator's `/bg` jobs (`src/cli/mod.rs:1477`).

**Spec.** The harness is the one process that knows its jobs, so it gates the stop itself:
1. Hand the `BashJobManager` (`src/tools/bash_jobs.rs:37`), or a read-only "running count" view of it, to the API backend. Today it is a local in the REPL loop (`src/cli/mod.rs:642`); ACP has none.
2. In §5's stop step, if any job is still running, **skip `hook capture` for this stop**, and trace "waiting on N background job(s)". When a job completes (`poll_completed`, `src/cli/mod.rs:1510`), the next stop runs normally.
3. If a model-facing background tool is ever added, emit its start and finish lines in the engine's exact strings above. Otherwise, write a Claude-shaped JSONL and pass `transcript_path`, so the engine's own detector works too. The second option needs `type`, `uuid`, `timestamp`, and `message.content` per line (`engine:hooks.rs:33-60`).

**Acceptance tests:**
1. With one running job, the stop step does not spawn the engine.
2. After the job completes, the next stop spawns it.
3. With no job manager (ACP), stops behave as §5.

---

## 9. Order of work

1. §2 runner, with its tests. Everything else depends on it.
2. §3 pre-tool. It is the security-relevant one, and it carries §4 questions and §6 permission on top of it.
3. §5 stop channel and §8 background gate, together: the gate has no effect without the stop.
4. §7 5b code-edit and 5d session-end. Both are small and fire-and-forget.
5. §7 5a. Measure first: it trades the fast per-turn native budget for engine completeness.
6. §7 5c, when compaction exists.

**Owner decisions needed before building:**
- the pre-tool failure policy (§3);
- Option A vs B for context injection (§7 5a).

**Must also be checked:** the ACP client's own prompt timeout (the desktop app, outside this repo), so that a 600 s hook is not cut by the app.
