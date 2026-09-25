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
connected by it. The harness proxies nothing.

### `incode --acp`: incode as the agent an editor drives

An Agent Client Protocol client (an editor, or the owner's desktop app) starts
`incode --acp` and drives it with JSON-RPC 2.0, one message per line on stdin/stdout.
incode is the **agent** here, never the client: underneath, the work is still done by
the subscription CLIs, through the same `cli::prepare_session` the REPL builds its
session with and the same `cli::plan_turn` / `AgentSession::turn_observed` a typed line
runs. The code is `src/acp_agent/`; payloads are `agent-client-protocol-schema` types.

| Method | What incode does |
|---|---|
| `initialize` | protocol version 1; prompts take text and embedded context; no session loading, no client MCP servers, no auth methods |
| `authenticate` | invalid params — no method was offered; the CLIs hold their own logins |
| `session/new` | `cwd` must be absolute and an existing directory; it becomes the workspace. The session is built on its own thread; after the answer, `available_commands_update` lists its slash commands |
| `session/prompt` | one turn: text as `agent_message_chunk`, tool calls and results as `tool_call` / `tool_call_update`, an edit that landed as a `diff`, the task list as `plan`, the context window's fill as `usage_update`; replies with the stop reason, or a JSON-RPC error if the turn failed. A prompt that is a slash command runs that command instead |
| `session/cancel` | trips the session's cancel token; the prompt answers `cancelled` |
| `session/set_mode` | `read-only`, `workspace-write`, `danger-full-access`; applied from the next turn |
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

**Slash commands.** `/undo`, `/plan`, `/act`, `/diff`, `/links`, `/roadmap`, `/doctor`,
`/memories`, `/remember`, `/worktree`, `/agent` and `/roles` are announced, with the
description and argument hint from `slash_commands::COMMANDS`. A prompt that parses as
a slash command runs the REPL's own handler inside `console::capture`, which returns
what the handler printed on that thread; that text is the reply, and the agent is not
run. `/plan` and `/act` also move the session to `read-only` / `workspace-write` and
send `current_mode_update`. Not offered: `/run` (long, must stream and honour cancel —
next step), `/bg` / `/jobs` / `/kill` (shell jobs outside the permission mode, with
completion reported by the REPL loop), and the terminal-session commands (`/quit`,
`/clear`, `/resume`, `/model`, `/add-dir`, `/forget`, `/help`, `/status` and the other
information screens). Anything else starting with `/` is answered with the list.

Not offered: `session/load` and `session/resume` (the agent CLI owns the transcript and
an ACP session id has no stored mapping to it), the client's MCP servers
(no seam in the backends, and a stdio server is a program the client would have this
machine run), and `session/request_permission` / fs / terminal.

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
| `hooks.rs` | **The operator can put their own command in front of an action.** Pre-tool hooks run on the desktop server and refuse with exit code 2; post-tool hooks run only for calls that happened and cannot deny. Bounded at 10s, because a hook runs between the agent asking and the screen moving. |
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
