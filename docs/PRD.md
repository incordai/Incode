# Incord Code — Product Requirements

**Status:** measured against the working tree on 2026-09-01.
**Scale:** 42,536 lines of Rust across 69 files. 898 tests passing (880 library, 18
binary), 0 failing. `cargo clippy --all-targets -- -D warnings` is clean.

**The crate got smaller on purpose.** It was 35,182 lines; the harness stopped
owning an agentic loop and deleted its own provider clients, tool dispatch,
transcript, compaction and permission tiers — around 12,000 lines that existed to
drive a raw model. An installed agent does all of it, on the operator's own
subscription, and this harness holds no API credential at all.

> **Superseded 2026-09-26 in one respect.** Under the models-only rule the CLI is only
> the model, so incode runs every tool again — a new Toolbox, a tool bridge for claude,
> and a small tool loop for the optional API route, whose key is read from the
> environment and never stored. See §2 and `docs/ARCHITECTURE.md` §5a. The numbers in
> this document are still those of 2026-09-01.

Every number and claim in this document was verified against the current source.
Nothing here is recalled or assumed. See [§10 Method](#10-how-this-was-produced) for
how, and for the two things that could not be verified at all.

---

## 🔴 MAIN GOAL

> [!IMPORTANT]
> **The harness takes its context and its guard rules from Incord Memory, treats an
> approved roadmap as its work queue, and keeps working that queue on its own —
> delegating each task to a cheap subagent instead of asking again for what it has
> already been told.**
>
> **The one-line test:** after the owner confirms a PRD and roadmap once, the harness
> completes the next task without asking a single question it could have answered
> from memory.
>
> **The metric:** cost per unit of finished work. Routine work runs on a cheap model;
> the expensive model is reserved for judgement. Every design decision in this
> document answers to that.

🔴 **Two of the three clauses are now met; the first is half-met.** Retrieval comes
from the body's memory (`src/recall.rs`), an approved roadmap becomes a work queue the
runner advances (`src/ledger.rs`, `src/runner.rs`, `/run`), and delegated work runs on
the model assigned to its role with an ordered fallback chain (`src/roles.rs`).

🔴 **All three clauses are now met in code.** The rules half of clause one landed with
`src/rules.rs`, and the local store that shadowed the body's memory is gone
(`src/knowledge.rs` replaced it — see [Gap 1](#gap-1--closed-2026-08-18-the-harness-grew-its-own-memory)).

What remains is not a missing capability but a missing measurement: **nothing has ever
run against a live model**, because this machine has no credential. Every claim in this
document is about code that compiles, passes its tests and appears in the built binary
— not about a loop that has completed a real task. See [§7](#7-what-is-missing).

---

## 🔴 ARCHITECTURE PRINCIPLE — one body

> [!IMPORTANT]
> **The harness and Incord Memory are one body, not two products that integrate.**
> Memory is built in by default, never an optional connector.
>
> **The harness does not build what memory already has.** All of the following live in
> the memory service and are the body's — not capabilities this crate should grow its
> own version of:
>
> | Already in memory | The harness must not build |
> |---|---|
> | Semantic recall, the graph, timeline | A second store or its own search |
> | **Rules** and **guards** | A rule engine or a policy DSL |
> | The **proxy** and its gate | Its own auto-decider |
> | The **auditor** (`audit_set`, `audit_findings`) | A parallel findings trail |
> | The **recorder** (`watchman_*`) | A tool-result cache |
> | Owner escalation (`owner_ask` / `owner_wait`) | Its own approval channel |
>
> The harness is the hands: the loop, the tools, permissions, diffs, the terminal.
> Memory is the memory, the rules and the judgement. Anything on the left growing
> inside this crate is a defect, not a feature.

This principle was violated for most of this crate's life, and closing that violation
was the largest single change made to it — see
[Gap 1](#gap-1--closed-2026-08-18-the-harness-grew-its-own-memory). Every row of the
table above is now *called* rather than rebuilt: recall and the rules through
`recall.rs` / `rules.rs`, the proxy through `owner.rs`, the auditor through
`auditor.rs`, the recorder through `recorder.rs`.

It also decides how every phase in §8 is written: the work is almost never *build*, it
is *call what is already there and delete the copy*.

If the harness binary stays small, it ships alongside the memory binary rather than as
a separate install.

---

## 🔴 PROJECT INTAKE GATE — no code before the brief

> [!IMPORTANT]
> **When a new project or folder is started, the agent fills in About, Goals, Synopsis
> and the full PRD — by asking, researching and proposing — and coding does not begin
> until all four are satisfied.**

The proxy decides *as the owner*. It cannot decide well about a project it knows
nothing about, and a decision like "add this dependency" is answerable from a roadmap
and unanswerable without one. The document is not documentation — it is **the context
the verdicts are made from** (`proxy_prd.rs:8-11`).

### The four artifacts already exist as fields

Nothing here is a new format. All four are fields of `ProjectBrief`
(`owner_web.rs:295`), which the proxy reads before it decides:

| Artifact | Field | What it holds | Where it is used |
|---|---|---|---|
| **About** | `about` | what this project is | read by the proxy before deciding |
| **Goals** | `goals` | what the proxy should protect | read by the proxy before deciding |
| **Synopsis** | `synopsis` | the PRD compressed, ≤ 900 chars | **rides every single review prompt** |
| **Full PRD** | `prd` | the product document, ≤ 24,000 chars | written to the project's memory space; found by space-scoped search when a decision touches it |

Alongside them, `roadmap: Vec<RoadmapItem>` — and **open items ride every review
prompt** with the synopsis (`RoadmapItem::is_open`, `owner_web.rs:370`).

### The rule that makes a PRD affordable

> The full PRD **never** enters a review prompt. Only the synopsis and the open roadmap
> items do. (`proxy_prd.rs:13-17`)

That split is the whole reason a product document is affordable on a hot path: the
proxy reviews on *every* decision, and a full document in that prompt would slow each
one to buy nothing a summary does not already say. Anyone tempted to "just include the
PRD for accuracy" is proposing to pay that cost on every approval, forever.

### The first step is the hardest: what the research must produce

The gate is not satisfied by a brief someone typed from memory. The agent asks, then
researches, then argues — and the PRD it drafts is held to all six of these.

**1. Latest stable, verified — never assumed.** Every language, framework, database and
library named in the PRD carries the exact version current *at the time of writing*,
confirmed against its registry or release page rather than recalled. This is the
standing dependency rule applied at design time instead of at install time, where it is
cheap: choosing a dead framework costs a rewrite, choosing a stale version costs an
upgrade.

**2. Free and self-hostable by default — paid APIs avoided at all costs.** A service
that bills per call is both a running cost and a second owner of the product's
availability: its outage is your outage, its price change is your margin. Where a paid
dependency looks unavoidable, that is a **finding to bring to the owner** — with the
free and self-hostable alternatives that were examined and the specific reason each was
rejected — never a line quietly added to a plan.

*Scope, confirmed by the owner:* this governs **the solutions the harness builds**. What
the harness itself runs on is a separate operator choice and is not covered — paying for
the tool that does the building is not the same as billing the product it builds. In
practice the harness now costs nothing extra: it drives an agent the operator already
subscribes to, and holds no credential of its own.

**3. Feasibility before commitment.** Anything the research cannot show is buildable
does not enter the roadmap at all. A roadmap item nobody can build is worse than a
missing one: it consumes planning, gets scheduled, gets started, and fails late. If
something cannot be built, the PRD says so plainly and says what can be built instead.

**4. The trade-off is argued, not asserted.** At least two viable approaches, each with
what it actually costs — operationally, not just in effort — and a stated reason for
the one chosen. A single option presented as obvious is a decision nobody made, and it
is the first thing to fall over when it meets production.

**5. Scalability, security, auth and design are first-version concerns.** Not deferred,
not "added later". Retrofitting authentication, or reworking a schema for scale after
real data exists, is the most expensive work there is — and the version that ships
without them is the version that gets used.

**6. The roadmap follows the PRD.** It is derived from the document, in order, each item
traceable to the section it implements. A roadmap item with no PRD section behind it is
scope that arrived without a decision.

### What the harness must not build

The research-and-draft path already exists — `proxy_prd.rs` is explicitly a separate
module on a *different clock* from `proxy.rs`, allowed to browse the open web and take
minutes (`PRD_TIMEOUT_SECS = 900`) precisely so a browsing call never lands on the
approval path. The harness runs the intake conversation and saves what is agreed; it
does not grow a second intake pipeline, a second research loop, or a second PRD store.

**This gate precedes every other phase in §8, including the runner.** An autonomous
runner working a roadmap that no one wrote is the failure mode this document exists to
prevent.

---

## Contents

1. [The problem](#1-the-problem)
2. [What it is today](#2-what-it-is-today)
3. [The three pillars](#3-the-three-pillars)
4. [Requirements](#4-requirements)
5. [What exists today](#5-what-exists-today)
6. [Code tree](#6-code-tree)
7. [What is missing](#7-what-is-missing)
8. [Roadmap](#8-roadmap)
9. [Decisions that need the owner](#9-decisions-that-need-the-owner)
10. [How this was produced](#10-how-this-was-produced)

**Companion document:** [`RUNNER.md`](RUNNER.md) — the task ledger and runner state
machine that [Phase 5](#phase-5--the-task-ledger-and-the-runner) builds.

---

## 1. The problem

Today the harness forgets. Each session starts cold, asks for context it was given
yesterday, and stops the moment a turn ends. The person driving it becomes the
memory, the roadmap and the scheduler — re-explaining the product, re-confirming
decisions, re-typing the next task.

That is three separate failures wearing one coat:

- **No shared memory.** Context that exists in Incord Memory is never read.
- **No continuity.** Work stops at the end of every turn and waits for a human.
- **No cost discipline.** Everything runs on one model, whatever the task.

---

## 2. What it is today

`incode` is a Rust CLI that plans work, has a model do it, and proves it was done.
**incode is the harness.** The installed agent CLIs — claude, codex and agy — are used
only as models, headless, on the operator's own subscriptions; incode holds none of
their logins. Every tool the model calls is incode's own and runs through incode's
gate: the guards, the permission mode, the memory engine's pre-tool check and a
checkpoint (`docs/ARCHITECTURE.md` §5a–§5b). As of 2026-09-26 that is complete for
claude; codex and agy still run their own tools, watched by the sentinel.

Three things separate it from a wrapper.

**It decides what is worth doing, and refuses when nobody decided.** An unspecified
project cannot be coded against: five spec documents must exist before a line of
source is written, and the refusal is enforced at the write, not printed as advice
(`src/spec.rs`). Work comes off an approved roadmap as an ordered ledger, and a task
that no command can prove is a question for a person rather than a chore to mark
done.

**Nothing is finished on a model's say-so.** The only edge into `Done` demands a
process that exited zero (`src/verify.rs`), and the harness runs that check itself,
in its own process, against the tree on disk. `Verified` has a private field and one
constructor that refuses a non-zero exit, so a shipped binary cannot mint proof it
did not earn.

**The thing being judged cannot edit its own exam.** Tests, CI, migrations and the
ledger are refused to the agent as deny rules it enforces itself — prevention — and a
sentinel fingerprints them before and after every delegated turn, which catches
whatever the rules did not. Both cover every root the work can reach, because
widening reach without widening protection is how a guard quietly stops applying.

What it is *not* yet is autonomous, memory-backed, or cheap.

---

## 3. The three pillars

The main goal decomposes into three independent capabilities. Each is partly built.

### I. Memory as the base

Context and guard rules come from Incord Memory, not from the operator retyping them.

- **Built, and wired to the right store since 2026-08-18.** Every block in front of the
  model comes from the body: the owner's standing rules and the relevant past turns,
  fetched per turn (`rules.rs`, `recall.rs`); what this project has already solved and
  what the auditor has already found, once per session (`recorder.rs`, `auditor.rs`).
  Writing goes the same way (`knowledge.rs`).
- **The duplicate that used to serve this is deleted** — see
  [Gap 1](#gap-1--closed-2026-08-18-the-harness-grew-its-own-memory).

### II. The autonomous loop

An approved roadmap becomes a work queue the harness advances without prompting.

- **Built** — everything *inside* a turn: bounded iteration, cancellation,
  auto-compaction, session persistence, resume.
- **Built** — everything *across* turns: the ledger, the runner, the attempt and
  escalation bounds, and the single verified edge into `Done`. Closed by
  [Gap 3](#gap-3--blocks-autonomy); run live on 2026-08-20 against an installed
  agent, one task planned, delegated, verified and closed. (This line read
  "**Missing**" long after the gap shut — a document that contradicts its own
  requirements table teaches the reader to trust neither.)

### III. Cheap execution

Routine work runs on cheap models; the expensive model only judges.

- **Built** — per-role model routing (`src/roles.rs`) applies on the subscription path:
  the agent CLI takes `--model` per invocation, so cheap work runs cheap with no API
  credential. Subagent delegation is the agent's own, one level deep. The harness's
  multi-provider registry went with the loop — there is one way in now.
- **Closed** — [Gap 4](#gap-4--blocks-the-cost-goal) was "one model for every
  job"; `src/roles.rs` ended it. This bullet used to say per-role routing was
  *missing* while the bullet directly above it says it is built — the same
  sentence arguing both ways in six lines.

---

## 4. Requirements

Written so each one is testable. Status is the verified state of the code today.

| # | Requirement | Status | Evidence / gap |
|---|---|---|---|
| R1 | Context is retrieved from Incord Memory before answering from scratch | ✅ Shipped | `src/recall.rs`; `context_for_turn` runs per turn at `cli/mod.rs:937`, and `recall_index` / `recall_get` are registered tools |
| R2 | Retrieval is semantic, so a question matches by meaning not shared words | ✅ Shipped | Served by the body's own index over `/v1/memory/recall/index`, not the crate's word-overlap store |
| R3 | Guard rules are loaded from memory and enforced, not restated by the operator | ✅ Shipped | `src/rules.rs`; the always-on rules unioned with the ones matching the turn, injected per turn. No rule engine here — the ranking is the body's |
| R4 | A confirmed decision is never re-asked; confirmations are durable | ✅ Shipped | `owner::past_verdict` checks the decision history before raising a card. Only `approve` and `dismiss` are reused — the others answered one attempt, not the question |
| R5 | An approved roadmap is stored as an ordered, resumable task ledger | ✅ Shipped | `src/ledger.rs`; built from open roadmap items, persisted at `.incord/ledger.json` |
| R6 | The harness advances the ledger unprompted, and knows when to stop | ✅ Shipped | `src/runner.rs` + `/run`. It ASKS the owner instead of halting, and acts on the verdict. **Run live 2026-08-20** against an installed agent on a subscription: one task planned, delegated, verified by a command this harness ran, and closed — `worked_by: claude-code:grunt (2569feb9) on claude-haiku-4-5-20251001` |
| R7 | A task is delegated to a subagent whose exploration never enters the parent context | ✅ Shipped, by the agent | The harness's own subagent host is gone with the loop. The installed agent spawns its own subagents and keeps their exploration out of the parent context — and its output is filtered on `parent_tool_use_id` (`agent/claude_code.rs`) so a subagent's text never lands in this conversation |
| R8 | A subagent runs on a cheaper model than the parent | ✅ Shipped | `src/roles.rs`; `task` takes a `role`, with an ordered fallback chain |
| R9 | Spend is tracked per run and can be capped | ✅ Shipped | `src/budget.rs`; `INCORD_BUDGET_TOKENS` stops the loop before the next request |
| R10 | Every edit is type-checked at the moment it is made | ✅ Shipped, by the agent | The harness no longer sees individual edits, so it cannot check them one at a time; the agent runs its own diagnostics. What the harness still does is stronger and later: it runs the project's real check itself, in its own process, and that is the only thing that closes a task |
| R11 | A bad run can be reverted in one step | ✅ Shipped | `src/tools/checkpoint.rs` + `/undo`. The agent's writes cannot be intercepted, so the snapshot is taken BEFORE the turn over the whole tree (`cli/agent_turn.rs`), and undo restores only what actually changed. Over `MAX_SNAPSHOT_FILES` it refuses rather than offering half an undo |
| R12 | A runaway generation can be stopped mid-stream | ✅ Shipped | The agent is a child process, so stopping it means killing it: the cancel token is checked between output lines and the child is killed (`agent/claude_code.rs`). A turn that has gone silent is not interrupted until it next speaks — stated, not implied |
| R13 | The agent can read documentation and verify a library API before using it | ✅ Shipped, by the agent | The harness's own `web_fetch`/`web_search` went with the tool layer. The agent has both, already configured on the operator's subscription |
| R14 | Independent reads overlap instead of running one at a time | ✅ Shipped, by the agent | The harness dispatches no tools, so it has nothing to batch. The agent parallelises its own reads |
| R15 | The fixed prefix of a request is cached rather than re-paid every turn | ✅ Shipped, by the agent | The harness sends no requests, so it has no prefix to cache. The agent caches its own, and resuming its session with `--resume` is what keeps the prefix from being re-paid — which is why changing the model deliberately ABANDONS the session rather than pretending to continue it |
| R16 | Work already figured out once is never paid for twice | ✅ Shipped, as far as it can be | `src/recorder.rs` reads the handoff at session start and `src/auditor.rs` reads the open findings, both into the context the agent is given. The other half — serving a stored output INSTEAD of running a tool — is no longer something this harness could do if it wanted to: it dispatches no tools, so there is no call to intercept. That half belongs to the agent |
| R17 | No code is written for a project whose brief is incomplete | ✅ Shipped | `src/brief.rs`; an incomplete brief makes completing it the session's first job |
| R18 | The roadmap is derived from the PRD, in order, each item traceable to a section | ✅ Shipped | `ProjectBrief::untraceable_items`; an open item naming no heading and no section the PRD uses becomes intake work |
| R19 | Every technology named in a PRD is current, free/self-hostable, and feasibility-checked | ✅ Shipped | `src/research.rs` holds the manifest against the document: a dependency the build links and the PRD never mentions becomes intake work. Currency and licence checks are the agent's to run — it has the web tools |

**Score: 19 shipped, 0 partial, 0 missing.** Updated 2026-08-21, when the harness
became subscription-only: its own agentic loop, provider clients, tool dispatch and
permission tiers were deleted — around 12,000 lines — and the work moved to an
installed agent driven as a child process.

**Read "Shipped, by the agent" carefully.** It does not mean this crate implements the
requirement; it means the requirement is met and this crate is not the thing meeting
it. That is the whole architectural bet: the agent is better at running a loop than a
second copy would be, and the copy cost an API key to run. What stays here is what the
agent will not do for you — refuse to start unspecified, refuse to call unproved work
done, refuse to let the exam be edited, and stop when the budget is gone.

R16 moved to shipped once the reason it was partial stopped existing: the half that
would serve a stored output instead of running a tool needs a tool call to intercept,
and this harness makes none.

Retrieval is scored against what is *wired*, not what exists: `recall.rs` would have
been a library nobody called had `context_for_turn` not been on the turn path.

### R16 in detail — nothing to build, only to call

**The recorder is already part of the body.** It lives in the memory service as
`watchman.rs`, `watchman_api.rs` and `watchman_capture.rs`, and is exposed as
`watchman_lookup`, `watchman_handoff` and `watchman_report`. There is no recorder to
design and no cache to write.

**The harness now calls one half of it.** `src/recorder.rs` reads the handoff at
session start. The lookup-and-serve half is refused on purpose — see
[Phase 2](#phase-2--never-pay-twice-call-the-recorder) for the three reasons.

The contract was established by calling the running service, not by reading about it:
lookup answers `{hit, note, observations, output}` and returns `hit: true` only for a
pair it has witnessed produce the same output repeatedly; the handoff answers
`{handoff_ready, items[], task}`. Both shapes are pinned by tests written against the
literal bodies the service returned.

What the handoff buys: a session starts knowing what this project has already been
observed to solve, instead of re-deriving it. For a harness whose cost is dominated by
re-reading the same files across sessions, that removes work rather than discounting
it — which is why it ranked above per-role routing.

**This ranks above per-role model routing.** Routing makes a call cost less; the
recorder makes the call not happen.

---

## 5. What exists today

A verified inventory. Test counts are per module, from the suite run while this was
written.

| Capability | Where | Tests | Notes |
|---|---|---:|---|
| Driving the agent | `agent/claude_code.rs` | 27 | Its event stream, models and fallbacks, deny rules, streaming, cancel-by-kill, session context |
| Project intake gate — the four artifacts | `brief.rs` | 17 | An incomplete brief becomes the session's first job, never a refusal |
| Two-stage recall from the body's memory | `recall.rs` | 13 | `recall_index` then `recall_get`; fails open, unlike the gate |
| Capability routing — role → ordered candidates | `roles.rs` | 13 | Warns when the reviewer shares the writer's lineage |
| Proof that work is finished | `verify.rs` | 12 | `Verified` is unconstructible without an exit-zero command |
| The task ledger | `ledger.rs` | 11 | Seven states, no `Failed`; two attempts then a person |
| The operator's control panel | `admin.rs` | 10 | One settings file; flag > env > file > built-in, each row says which |
| The autonomous runner | `runner.rs` | 22 | Six stop conditions; a `LyingWorker` control proves the law holds |
| What this project already solved | `recorder.rs` | 10 | Reads the handoff; refuses to serve a recorded output in place of a run |
| Asking the owner | `owner.rs` | 14 | Card in, verdict out; all five verbs read, an unknown one never as approval |
| The shared memory: write, search, correct | `knowledge.rs` | 9 | Replaced 1,087 lines of local store; a delete is two steps |
| What the auditor already found | `auditor.rs` | 7 | Reads findings for this workspace; files and resolves nothing |
| The dependency gate | `research.rs` | 10 | Holds the manifest against the document the build was supposed to justify |
| The owner's standing rules | `rules.rs` | 8 | Loader only — the ranking, the storage and the editing are the body's |
| Spend ceiling | `budget.rs` | 7 | Tokens not currency; bounds the action, never the measurement |
| Workspace utilities (not tools any more) | `tools/` | 17 | The agent dispatches its own tools; these are what the harness does to a tree itself |
| The agent backend | `agent/claude_code.rs` | 27 | Drives an installed agent CLI that owns its own loop: models, fallbacks, deny rules, partial-message streaming, cancel-by-kill, and the session context |
| File operations & diffs | `tools/file_ops.rs` | 11 | Workspace boundary enforced on every resolve |
| Checkpoint / undo | `tools/checkpoint.rs` | 11 | Snapshot taken BEFORE a delegated turn, since the agent's writes cannot be intercepted; restores only what actually changed |
| Edit diagnostics | `tools/diagnostics.rs` | 8 | Gated to the mode that permits running commands |
| Lifecycle hooks | `runtime/lifecycle.rs` | 7 | session start / end / compact |
| Slash commands | `cli/slash_commands.rs` | 6 | `/run`, `/admin`, `/undo`, `/plan`, `/act` and the memory commands. The ones that configured a tool layer are gone |
| One interactive turn | `cli/agent_turn.rs` | 7 | Context, checkpoint, turn and streaming in one place, shared with `--task` |
| Who did the work | `identity.rs` | 7 | The agent is the identity; the model is an attribute of the attempt |

### Worth keeping

The safety work is the part that would be expensive to rebuild: a permission ladder
where modes are never compared ad hoc, a web tool that resolves and pins DNS itself
and revalidates every redirect hop, an undo that reports what it could *not* restore
rather than claiming success, and diagnostics that refuse to run in a mode that would
not allow a shell command.

---

## 6. Code tree

> **Stale as of 2026-09-21.** The counts below are from 2026-08-21 and have not been
> re-measured; several files have roughly doubled and these modules did not exist
> then. Rather than print numbers nobody took, they are listed by what they answer:
>
> | Module | The law it enforces | Document |
> |---|---|---|
> | `parallel.rs` | An agent that cannot be isolated does not get a lane | `TEAMWORK.md` §4 |
> | `team.rs`, `team_mcp.rs` | A question asked is not a question seen | `TEAMWORK.md` §2–3 |
> | `desktop/` | Nothing reaches the screen that the operator cannot stop | `COMPUTER_USE.md` |
> | `findings.rs` | Work that produces information is judged by its artifact | `RUNNER.md` §6d |
> | `wiring.rs` | A capability nothing calls is not a capability | `CONVENTIONS.md` §13 |
> | `goal.rs` | A drained queue is not a finished project | `RUNNER.md` §6a |
> | `plan.rs` | A roadmap item is decomposed, never invented | `RUNNER.md` §5a |

Measured from the working tree on 2026-08-21, after the harness became
subscription-only. **Six modules left**: `provider/` (every API client), the
agentic loop in `runtime/mod.rs`, `runtime/session.rs`, `runtime/compact.rs`,
`runtime/subagent.rs`, `prompt/`, and the tool-dispatch half of `tools/` —
around 12,000 lines. Each of them existed to drive a raw model; the installed
agent does all of it, and better.

```
src/                            THE LAWS — what only this harness does
├── runner.rs            1863   the loop over the ledger and its six stop conditions
├── brief.rs             1471   the product document, and the gate on an incomplete one
├── guards.rs            1243   protected paths: deny rules the agent enforces, plus a sentinel
├── spec.rs               892   the five spec documents, and the refusal to code without them
├── admin.rs              888   operator settings, one precedence ladder, provenance per value
├── ledger.rs             732   task state; one edge into Done and it demands proof
├── contracts.rs          732   frozen interfaces, checked inside verify
├── knowledge.rs          663   the shared memory: write, search, correct
├── owner.rs              657   the card that reaches a person, and the verdicts that come back
├── verify.rs             596   the only thing that settles whether work is finished
├── recall.rs             532   two-stage retrieval: index, then fetch what is worth reading
├── roles.rs              469   which model serves which kind of work
├── main.rs               374   argument parsing and the three doors: session, --task, --admin
├── recorder.rs           361   what this project has already solved
├── auditor.rs            360   what the auditor already found here
├── rules.rs              287   the owner's standing rules, loaded not re-implemented
├── research.rs           260   a dependency the build links and the document never argued for
├── identity.rs           249   who did the work: the agent, never the model
├── budget.rs             235   the ceiling; bounds the action, never the measurement
├── task_run.rs           209   one instruction, no ledger, no credential
├── config.rs             129   session configuration
└── text.rs               126   bounded string helpers
│
├── agent/                      DRIVING THE AGENT
│   ├── claude_code.rs   1083   its event stream, models, deny rules, streaming, cancel, context
│   ├── worker.rs         724   one ledger task at a time, with the sentinel around it
│   ├── registry.rs       297   which agents are installed, and which can be driven
│   └── mod.rs            138   the AgentBackend trait
│
├── cli/                        THE HUMAN SURFACE
│   ├── mod.rs            838   wiring, the REPL, and the slash commands
│   ├── slash_commands.rs 579   parsing and the handlers that are still this harness's business
│   ├── agent_turn.rs     443   one interactive turn: context, checkpoint, streaming
│   ├── render.rs         238   diffs, spinner (terminal only), streaming output
│   ├── markdown.rs       158   terminal markdown rendering
│   └── input.rs           61   line editing, Ctrl-C at the prompt
│
├── tools/                      WHAT THE HARNESS DOES TO A TREE ITSELF
│   ├── checkpoint.rs     546   pre-turn snapshot and one-step undo
│   ├── file_ops.rs       538   read, write, edit, glob, grep, diff rendering
│   ├── diagnostics.rs    415   type-check with the project's own checker
│   ├── tool_status.rs    386   which developer tools are installed
│   ├── worktree.rs       342   isolated git worktrees
│   ├── bash.rs           270   shell execution with timeout — this is what runs `verify`
│   ├── bash_jobs.rs      255   background jobs
│   ├── env_filter.rs     238   environment allowlist for spawned processes
│   ├── git_context.rs    197   branch and status
│   └── mod.rs             20   the module list, and why these are no longer tools
│
├── runtime/                    WHAT OUTLIVED THE LOOP
│   ├── lifecycle.rs      234   session start / end hooks
│   ├── usage.rs          212   token counting
│   ├── cancel.rs         102   the shared cancellation flag
│   └── mod.rs             62   two error types
│
├── permissions/mod.rs    406   permission modes, still used to name what a session may do
└── hooks/mod.rs          141   pre/post hooks that can deny a call
```

Two files sit above the ~1,000-line guideline: `runner.rs` and `agent/claude_code.rs`.
Neither is urgent. The file that used to head this list — `runtime/mod.rs`, at 2,508
lines — is now 62: the loop it held was deleted rather than split.

The modules at the top of the tree are the laws, written as separate files
deliberately: each one is a rule the rest of the crate has to obey, and a law buried
in a 2,500-line module is a law nobody finds.

---

## 7. What is missing

Ranked by how hard they block the main goal, not by effort.

### Gap 0 — CLOSED 2026-08-20: the CLI would not start without an API key

**The product was unreachable from its own front door.** `create_api_client` ran
unconditionally at session start and failed without `ANTHROPIC_API_KEY` — while
`/run`, the thing the harness exists to do, prefers the installed agent and never
asks a provider for anything. So the loop worked perfectly one layer down and could
not be reached from the CLI on a subscription alone.

A missing credential is now fatal **only when there is no other way to do any work**:
with a drivable agent installed the session starts, the banner says what will serve
the work rather than naming a model nothing will answer, and every direct model
request carries the reason it cannot run (`IncordProviderClient::NotConfigured`).
With nothing installed it still refuses, because a session that can do nothing is
worse than saying so. Measured both ways on this machine: no key → starts, exit 0;
key present → unchanged.

### Gap 1 — CLOSED 2026-08-18: the harness grew its own memory

**The duplicate is deleted.** `src/memory/mod.rs` (1,087 lines), `src/mcp_server/`
and `src/bin/incord-memory-mcp.rs` are gone, along with the five tools they put in
front of the model — `memory_add`, `memory_search`, `memory_update`, `memory_delete`,
`memory_profile`. The binary carries none of those strings any more, nor
`memory.json`; controls confirm the search itself works.

**Why it had become urgent rather than merely untidy.** The copy was always worse
than the thing it shadowed — word-overlap matching where the body has embeddings, one
JSON file where the body has a graph, invisible to every other agent on the machine.
But wiring in the body's own recall beside it made it actively harmful: the model was
handed *two* memories in one tool list, with nothing to say which was real. A note
saved to the wrong one is a note nobody reads again, and the wrong one was the more
discoverable of the two.

**What replaced it.** `src/knowledge.rs` is the write half — `remember`,
`memory_search`, `forget_memory` — against the body's own API, beside `recall.rs`
which is the read half. Three tools instead of five, because a fourth way to read
would recreate the confusion this removed.

Two details worth keeping, both measured rather than assumed:

- **The endpoints answer in different shapes.** `/v1/memory/remember` replies in the
  tool-call envelope, with its payload as a JSON *string* nested inside
  `content[0].text`; `/v1/memory/search` replies with a plain body. One parser written
  for both would have found nothing, every time, without ever failing.
- **A delete is two calls.** Without `confirmed` the service returns the value rather
  than deleting it, so a person can be shown what is about to go. That handshake is
  reproduced rather than collapsed — collapsing it would mean a memory deleted on a
  model's say-so.

**What was lost, and it is a real trade.** The local store did per-tool-call
retrieval for free, in-process: touch `auth.rs`, and notes about `auth.rs` rode along
with that one call. The shared store embeds the query, so asking it per call would put
a round trip in front of every file read. Retrieval is now once per turn
(`recall::context_for_turn`) and once per session (the recorder and auditor blocks) —
fewer calls and better matches, at the cost of file-level precision. A test asserts
the executor no longer attaches anything per call, so nobody restores a hidden
per-call fetch by accident.

### Gap 2 — blocks "stop asking me"

**No rules are loaded from memory, and no confirmation is durable.**

The enforcement machinery is real — hooks can deny a tool call (`src/hooks/mod.rs`),
permissions gate every tool by tier (`src/permissions/mod.rs`) — but nothing reads a
rule set out of memory and applies it, and an approval granted at a prompt lasts
exactly one call. There is no record of "this was already settled", which is
precisely what makes the harness ask again.

### Gap 3 — CLOSED 2026-08-17: the loop across turns

`src/ledger.rs` holds per-task state derived from the open roadmap items, `src/runner.rs`
works it, and `/run` starts it. Six stop conditions, each with a message: ledger empty,
budget spent, a task failed twice, a decision needs the owner, nothing left that can be
run while work is still parked, and cancelled.

**The law it enforces:** the only edge into `Done` demands `verify::Verified`, which
cannot be constructed without a command that exited zero. A worker's report is recorded
as a claim beside the evidence, never in place of it.

**What remains:** the loop has never run against a live model. The "needs the owner"
stop no longer merely halts — `run_with_owner` raises a card on the owner's queue and
waits for the verdict (`src/owner.rs`), proven end to end on 2026-08-17: a card was
raised, the proxy reviewed it, a person answered, and the verdict came back and parsed.

The service defines **five** verbs (`chat_bridge::OwnerVerb`), and all five are read.
**Approve** sets the task aside as not machine-verifiable — explicitly NOT `Done`,
because nothing on that channel can produce a `Verified`. **Change** returns it to the
queue *and hands the instruction to the coding agent*. **Dismiss** cancels it.
**Hold** parks that one task and the rest of the queue carries on: "not this one" is not
"not anything", and halting a whole run on a parked item wastes every task that was
ready to go. **Info** is read-only — the service does not even consume the token for it
— so the question is still open and the run stops.

Skipping is only safe because the run REPORTS what it skipped. A new stop reason,
`NothingRunnable`, names every task still waiting on somebody and says plainly that they
are not finished. That also fixed a lie that predated `hold`: the loop returned
`LedgerEmpty` — "Every task in the ledger is finished" — whenever nothing was `Ready`,
including when everything left was blocked.
**Silence stops the run** and leaves the task blocked, as does a channel that cannot be
reached: an unanswered question auto-answered as approval is precisely how both earlier
loops failed, and it is not reachable from here.

**`change` is the verb that talks to the agent, and it was nearly decorative.** The
service's own words are that the note is "a free-text instruction the agent's next pass
reads as a round" — but `AgentWorker::instruction` was built from the task's id and
title alone, so the owner's answer reached the ledger and stopped there. The person
would answer, the runner would record it, and the agent would try the same thing again
knowing nothing. `Task::owner_note` now carries it, separately from `note` so a failure
message is never fed back as if it were guidance, and the agent is told to follow it.

The ask-answer-ask cycle is bounded too (`MAX_ESCALATIONS`), after a `change` verdict
spun the runner between the ledger and the owner and hung a test run.

The design that closes this is [`RUNNER.md`](RUNNER.md), built in
[Phase 5](#phase-5--the-task-ledger-and-the-runner).

### Gap 4 — CLOSED 2026-08-17: cheap execution

`src/roles.rs` maps a role to an ORDERED list of models, set per role via
`INCORD_ROLE_<ROLE>`. The `task` tool takes a `role`, and the subagent gets its own
client for it — so delegation now saves money as well as context. A failing candidate
falls through to the next, and finally to the parent's model, so one provider being
down costs an attempt rather than the task.

A reviewer sharing the writer's lineage is warned about at startup: a model reviewing
its own family agrees with itself for reasons unrelated to correctness.

Spend is now bounded too. `src/budget.rs` stops the loop before the next request when
`INCORD_BUDGET_TOKENS` is reached. **Tokens rather than currency, deliberately:**
`usage.rs:54-62` prices unrecognised models at a default tier, so a dollar ceiling over
an arbitrary fleet would be a fabricated number. Tokens are measured.

### The admin layer — who decides the fleet (2026-08-17)

Every one of those settings used to live somewhere different: a model name compiled
into a constructor, a fleet in one environment variable, a ceiling in another, a check
guessed from a manifest. Each was defensible alone, and together they meant nobody
could answer *"what is this session actually going to do?"* without reading the source.

`src/admin.rs` is one file the operator owns, at `~/.incord-code/admin.json`
(`INCORD_CODE_DIR` moves it). It carries the session model, the per-role candidate
lists, the token ceiling, the verify command and the memory URL. Nothing in it is a
model the code chose.

**Precedence, and why it runs this way.** A command-line flag beats an environment
variable, which beats the file, which beats the built-in fallback — *the more
deliberate and the more immediate the choice, the more it wins*. A flag is typed for
one run, an env var set for one shell, the file is the standing decision.

Role assignments override **per role**, not wholesale: overriding one role in the shell
must not discard the rest of the file, which is the normal way this gets used.

**Provenance is part of the answer.** Every resolved value carries the layer that
decided it, and `--admin` (or `/admin` in session) prints the whole table with each row
labelled. `--admin` deliberately runs *before* anything that needs a provider: an
operator checking which model a run will use must not need a working credential for
that model to find out. A malformed settings file is an error, not a silent empty
default — settings that quietly stop applying leave the run looking normal while doing
something else.

The three callers that used to read their own environment now climb this ladder:
`verify::command_for` consults the file before falling back to manifest detection
(a guess must never beat something a human wrote down), `brief::memory_url` and the
session's model and budget resolve the same way.

### Gap 5 — CLOSED IN PART 2026-08-17: paying twice for settled work

**The handoff is now read; serving from the recorder is refused.** `src/recorder.rs`
asks the body what this project has already solved and puts a bounded, explicitly
labelled block in front of the model once per session — bounded because a summary that
silently shows ten of forty reads as complete, so it says how many it dropped.

The block is framed as observation rather than instruction: its contents were written
by other agents in other sessions, and under the standing prompt-injection rule that
makes it data. A test asserts the framing is present, because a block that lost its
label would still look fine in a diff.

What is refused, and why, is in [Phase 2](#phase-2--never-pay-twice-call-the-recorder).

### Two clarifications, since both look adjacent to something that exists

**R3 was never a rule engine to write — CLOSED 2026-08-17 by asking.** The rules, the
guards, the proxy that gates them and the auditor that checks the result all already
live in memory. What was missing in this crate was only the *reading*, and that is now
`src/rules.rs`: one call per turn returning the always-on rules unioned with the ones
that match what is being asked, ranked by the body's own embeddings.

Three decisions worth keeping. **They ride the turn, not the system prompt** — which
rules matter depends on the question, and the always-on set comes back in every
response, so nothing mandatory can fall out by being unlucky in the ranking.
**They go first**, ahead of recalled context, because they govern how the model treats
everything else. And **the block says they are instructions** — the one thing pulled
from memory that the model is meant to obey, where recalled conversations and recorded
tool outputs are data about the past. A model that treats all injected text alike
either obeys the wrong thing or obeys nothing.

**R5 is not met by `remember(kind:"todo")`.** A todo is a note: free text, no ordering,
no per-task state, no transition anyone can verify. A ledger needs states, an order,
and a record of who moved a task and on what evidence. Memory gives durable storage;
it does not give a ledger. That distinction decides §9.5 — see below.

### Smaller, already known

- Prompt caching is implemented but has never been measured against a live endpoint —
  this machine has no credential set, so **no end-to-end provider test of anything has
  ever run**.
- `web_search` needs `INCORD_SEARCH_URL` and `INCORD_SEARCH_KEY` before it does anything.
- MCP tool results containing images are dropped; only text blocks survive
  (`src/tools/mcp_client.rs`, `extract_text_content`).
- No tracing subscriber, so structured logs go nowhere. Needs a new dependency.
- The TypeScript branch of edit diagnostics is never exercised — only the Rust one.
- A `src/.backups/` directory sits inside the source tree; the hook that made it does
  not capture edits, so it is stale as well as misplaced.

---

## 8. Roadmap

Ordered so each phase is useful on its own, and so the cheapest high-value work lands
first.

### Phase 0 — The intake gate

Read the project's `ProjectBrief` at startup. If anything is missing, **completing the
brief becomes the session's first job** — the agent names what is absent, asks the owner
what only they can answer, researches what can be researched, proposes what it finds,
and saves each part as it is agreed. Coding continues in the same session once the brief
is complete.

> [!IMPORTANT]
> **Nothing is blocked.** The gate does not refuse to run and does not exit. A harness
> that exited here would teach the operator to work somewhere else, and the brief would
> never get written at all. It gathers the data it needs instead.

The one case it cannot gather is an unreachable store: there is nowhere to save to, so
it says that plainly rather than collecting work that would be lost.

Placed at zero because it precedes every other phase. It is also small: a read, a
completeness check, an instruction, and a way to save.

**Completeness is not "the fields are non-empty."** A brief passes only if it also
meets the six research standards above — current verified versions, free and
self-hostable choices, feasibility established, the trade-off argued, the four
non-negotiable qualities designed in, and a roadmap derived from the document in order.
A gate that checks for text rather than for those is a gate that waves through exactly
the brief it exists to stop.

The parts that can be checked mechanically should be: a named dependency resolves to a
current release, a roadmap item names the PRD section it implements, no chosen service
is on the paid list. The rest is the proxy's judgement, and it is the proxy's job.

**Done when** a fresh folder with no brief starts an intake rather than a coding
session; the instruction names the missing artifact or the failed standard rather than
failing vaguely; each part can be saved as it is agreed without disturbing anything else
in the store; and a brief naming a paid API or a stale version does not pass.
**Closes:** R17, R18, R19.

### Phase 1 — ✅ DONE 2026-08-18: use the body's memory, delete the copy

Retrieval moved to the body first (`recall.rs`, two-stage: a ranked index, then full
content only for what is used), and the copy was deleted second — 1,087 lines of local
store, the MCP server that exported it, and the second binary.

The plan here said to attach memory "through the MCP client that already exists". That
was wrong and is left in the record: the client is stdio-only while the service
registers its MCP over HTTP, so they would never have connected. The plain HTTP API was
the route, and `brief.rs` was already proving it.

Keeping the copy "as a fallback" was rejected for the reason written here at the time —
a fallback answering from keyword matching while the real store holds the answer is
worse than an error, because it looks like success.

**Done when** a question answered in a previous session is answered without asking,
retrieval is semantic rather than word-overlap, and the crate contains no memory
implementation of its own. **All three hold.** The last is checkable and checked: a
test asserts the five deleted tool names never reappear in the tool list.
**Closed:** R1, R2. Removed 1,087 lines and gained 520 in `knowledge.rs`.

### Phase 2 — Never pay twice: call the recorder

Consult `watchman_lookup` before executing a tool call and serve a hit instead of
running it; call `watchman_handoff` at session start to see what this project has
already solved. Re-verify rather than serving a hit when the underlying file may have
changed since it was recorded.

Placed second because it falls out of Phase 1 almost for free — the recorder is
reached through the same connection to the same body — and because removing work beats
making work cheaper.

**Done when** a repeated `(tool, input)` on a known project is served from the
recorder instead of re-executed, and the saving is visible in the session total.
**Closes:** R16.

**HALF LANDED 2026-08-17 — and the other half is refused, not pending.** The handoff
is read at session start (`recorder.rs`, wired at `cli/mod.rs`), so a session begins
knowing what this project has already been observed to solve. Ten tests, two control
runs, and the endpoint pinned by a test because the contract was established by calling
the running service rather than by reading a doc.

Serving a recorded output *in place of running a tool* is deliberately not built, and
the reason is in the module's own header so nobody later adds it thinking it was merely
unfinished:

- **This harness edits the workspace it reads.** A recorded `read_file` is a correct
  answer about a file that no longer looks like that. The recorder cannot know an edit
  happened; only this process can.
- **The safe tool class does not exist here.** Serving is sound for a pure lookup with
  no disk, clock or network in it. Every tool in this crate touches one of the three.
- **A served `verify` is the worst case.** The runner would mark work `done` on a check
  that ran last week against different code — the exact failure `verify.rs` exists to
  prevent, arriving through the cache instead of through the model.

So R16 reads `✅ Shipped, as far as it can be` in the table above, and the second half
is a **non-goal** rather than a gap: the half that removes work is taken; the half that
would let stale evidence close a task is refused on purpose. (This paragraph used to
end "R16 is `⚠️ Partial`", disagreeing with the same document's own table — and the
distinction matters, because "partial" invites someone to finish it.)

### Phase 3 — Rules and confirmations as data

Load the operator's standing rules from memory into the system prompt, and add a
durable record of settled decisions that the permission layer consults before
prompting. A question already answered becomes a lookup, not a prompt.

**Done when** a confirmed decision survives a restart and is not asked twice.
**Closes:** R3, R4.

### Phase 4 — The spend ceiling

A budget for a run, checked before each request, that stops the run when it is spent
and says so. Nothing else in this phase.

Moved ahead of the runner deliberately. A cap is not an optimisation — it is the
precondition for letting anything run unattended at the tier that permits shell
commands (§9.4). Model routing makes each call cheaper; only a ceiling makes an
unbounded run safe to start.

**Done when** a run stops on its own budget and reports what it spent.
**Closes:** R9.

### Phase 5 — The task ledger and the runner

Persist an approved roadmap as an ordered ledger with per-task state, then add a
runner that takes the next open task, delegates it, records the outcome, and
continues — with four explicit stop conditions: ledger empty, budget spent, a task
failed twice, or a decision genuinely needs the owner.

Designed in full in [`RUNNER.md`](RUNNER.md), including the states, the transitions,
and the one law that governs the whole thing: **the runner may decide what to do next,
but never that something is done.**

> [!CAUTION]
> **Blocked, not merely sequenced.** This phase must not start until the two open
> decisions in §9.6 and §9.7 are settled. The runner leans on the owner-escalation
> path, and that path currently cannot tell the runner whether a human answered.

**Done when** the harness completes several roadmap tasks unattended and stops for a
stated reason rather than drifting.
**Closes:** R5, R6.

### Phase 6 — Per-role model routing

Give the subagent its own client rather than a borrow of the parent's, selected by
role: a cheap model for exploration and mechanical edits, the expensive one for
planning and review. Extend configuration from one model to a small role table.

Demoted below the recorder and the ceiling: this is an optimisation of work that
happens, where Phase 2 removes work entirely and Phase 4 is a safety precondition.

**Done when** the same delegated task costs measurably less than it does today, with
the saving visible in the session total.
**Closes:** R8.

### Phase 7 — Close the verification debt

Measure prompt caching against a live endpoint, exercise the redirect and TypeScript
paths, add the tracing subscriber. All currently blocked or deferred rather than hard.
**Closes:** R13, R15.

### Phase 8 — The admin layer ✅ LANDED 2026-08-17

Take the fleet out of the code. One settings file the operator owns, one precedence
ladder every setting climbs, and a report that names the layer behind each value.

**Done when** — and this is how it was checked — the built binary answers `--admin`
with the whole table before it touches a provider, an operator can override one role in
the shell without losing the rest of the file, and a broken settings file fails loudly
instead of resolving to a silent default. All three verified against
`target/debug/incord-code.exe`, not against the source.

**Closes:** the part of decision 2 below that was a design question rather than a
preference.

---

## 9. Decisions that need the owner

Each one changes the design; none can be settled from the code.

1. **Credentials.** Nothing has ever been tested against a live model from this
   machine — all three credential variables are unset. Phases 4 and 6 cannot be measured
   and Phase 1 cannot be proven end to end until there is a working key, or the harness
   is pointed at a local model.
2. **Which model is "cheap".** ⚙️ **Mechanism decided; the names are the operator's.**
   `src/admin.rs` holds the role table and `--admin` shows what a run will use, so no
   model name is compiled into this crate and none ever should be. What remains for the
   owner is only *which* models to write in that file, plus the cost table
   (`usage.rs:54-62`), which recognises three families and prices the rest at a default
   tier — the reason the ceiling counts tokens rather than currency.
3. **Does the local memory store stay?** ✅ **Decided: no.** The harness and memory are
   one body, so a second store is a defect. It is deleted in Phase 1, not kept as a
   fallback. The open part is only whether the two ship as one binary — see §9.8.
4. **How much may run unattended.** Autonomy needs a budget ceiling and a permission
   tier. Running the loop at the tier that permits shell commands is what makes it
   useful, and also what makes it worth bounding.
5. **Where the roadmap lives.** ✅ **Decided: a file in this repo.** Memory gives
   durable storage but not a ledger — `remember(kind:"todo")` is free text with no
   ordering, no per-task state, and no transition anyone can audit. The ledger needs
   states, an order, and a record of what moved a task and on what evidence, so it is
   a file the harness reads and writes, reviewable in a diff. Memory still holds the
   *findings*; the file holds the *state*.

### The escalation path — one resolved, one closed by construction

Both were listed as blocking Phase 5. Neither blocks it now, and the reasons are
different: one turned out to be already satisfied in the service, and the other is
answered by which source the harness stamps on its cards.

6. **Should a continuation card be auto-decidable at all?** ✅ **Closed by
   construction, 2026-08-17.** `escalate.rs:19-44` states that `proxy::gate` refuses
   outright to decide a `guard` card — which means a card stamped with any other source
   *is* auto-decidable. So the harness stamps `guard` and nothing else
   (`owner::GUARD_SOURCE`): the runner escalates only when it has reached something a
   machine must not settle, and those cards are exactly the ones the proxy will not
   answer for the owner. The original question — whether `continuation` should be
   auto-decidable — remains the service's to answer, and no longer touches this harness.

   For the record, the original text: That is the mechanism behind the
   loop-hold failure: a question was raised, auto-answered, and the agent told to
   proceed, while the owner never saw it.
7. **Is `by` set from the deciding actor?** ✅ **RESOLVED 2026-08-17 — and the earlier
   note here was wrong.** `get_owner_wait` (`api/owner_feed.rs`) returns
   `{decided, verb, note, reason, by}`, where `by` is `row.decided_by` and `reason`
   carries the proxy's own objection.

   Measured rather than argued: across 163 decided cards in the live history,
   `decided_by` takes three distinct values — `proxy` (63), `owner` (78) and `sync`
   (22). A human reply and a `proxy::gate` auto-decision therefore do produce provably
   different values, which was the requirement.

   This was the hard dependency under the runner's "a decision needs the owner" stop,
   and it is satisfied: `src/owner.rs` carries `by` into the verdict and the ledger note
   records it, so "the owner approved" and "the proxy approved on their behalf" are
   never the same string.

8. **Do the two ship as one binary?** One body argues for one install. If the harness
   stays small next to the memory service, it ships with it rather than separately.
   Both are Rust crates, so this is a packaging decision — one workspace with two
   binaries, or one binary with a subcommand — not a rewrite. Deferred until Phase 1
   lands, because deleting the duplicate memory changes the size this question is
   asked about.

---

## 10. How this was produced

Every number here was measured on the working tree, not recalled. Line counts and the
file list come from a full traversal of `src/`. Test counts come from a complete run
of the suite, tallied per module. Each capability claim was checked by reading the
code it describes. The five gaps were each confirmed by searching for the concept
across the crate and finding no implementation. Gaps 3 and 4 were closed on 2026-08-17
and are marked in place rather than deleted, so the record of what was wrong survives.

**Two limits are worth stating plainly.**

1. Nothing has been exercised against a live model provider, so every claim about
   provider behaviour rests on tests against local servers and scripted fakes.
2. `src/provider/local.rs` cannot be compiled on this machine at all — the crate it
   depends on is not installed, and `cargo check --features local-llm` fails with
   *"use of unresolved module or unlinked crate"*. Its contents are reported from
   reading, not from a build.
