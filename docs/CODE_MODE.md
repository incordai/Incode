# incode — Code Mode (one script, many tool calls)

**Status: PARKED, 2026-10-27.** A prototype was built and measured (§5b) and then removed:
`beside` saved nothing, and `only` saved context on one task and cost more on the other,
on samples too small to set a default from. Nothing of it is in the tree. Every claim
about the existing tree cites a file and was read on 2026-10-27.

**Decisions taken by the owner, 2026-10-27:** write this spec before any code; build a
prototype and measure whether it saves anything; on the results, park it and keep this
document.

**The prototype, as measured** — `src/toolbox/script.rs`, its 11 tests, the wiring in
`toolbox/mod.rs` and `rquickjs =0.14.0` — is kept as a patch outside the repository, on
the machine it was built on. What it established, for whoever picks this up:

- The engine items once marked UNVERIFIED in §4 work on Windows with no extra tools:
  - the interrupt handler stops `while (true) {}`;
  - the heap limit ends a runaway allocation with "out of memory";
  - a blocking host function works from an `async` script;
  - time inside a host call can be left off the clock.
- A host function must be `'static`, so the script cannot borrow the toolbox. It ran
  on its own thread and sent each call back over a channel to the calling thread, which
  ran it through `run_call`.
- The control held: with a script's calls sent past `run_call`, the read-only and the
  prompt tests both failed on their assertions.

## 1. The problem, as the code has it

On the API route incode runs the tool loop itself. Each step is one model request
(`src/agent/api.rs:1009`, `for _ in 0..max_steps`), and each request sends the whole
conversation and every tool definition again (`request_body(...)`, `api.rs:1024-1029`).
Every tool result is appended to the conversation (`api.rs:1091`) and is paid for again by
every later step of the turn.

Calls the model asks for **in the same reply** already run in one step: the loop runs all
of `reply.calls` before it sends again (`api.rs:1067-1090`). So independent calls are
already batched. What still costs a request each is a **dependent chain**, where a result
decides the next input:

- memory: `recall_index`, then `recall_get` for the hits worth reading, then sometimes
  `recall_timeline` around one of them (`src/recall.rs:276-279`, two stages on purpose);
- code: `Grep`, then `Read` of the files it found, then `Outline` of one of them;
- checks: run a command, read the failing file it names, read the test beside it.

Each link is a full request, and every intermediate result (a 40-hit index, a whole file
the model only needed one line of) stays in the conversation for the rest of the turn.

The CLI routes (claude, codex, agy) have the same shape inside the agent's own loop. incode
sees each of their calls arrive separately at its tool server (`src/tool_bridge.rs`,
`tools/call`), and cannot change how that agent loops. It can only offer a better tool.

## 2. The idea, and the prior art

"Code mode": the model writes one short program that calls the tools as functions, and
only what the program returns goes back to the model. A three-link chain becomes one
request, and the intermediate results never enter the conversation.

The prior art is a TypeScript package in the `cloudflare/agents` repository
(`packages/codemode`, MIT, marked "Experimental — may have breaking changes"). It is
**not** adopted, for four reasons read from its README:

1. its sandboxed executor "Requires Cloudflare Workers environment" (a Worker Loader
   binding), which a local Rust binary does not have;
2. it is TypeScript, so incode would have to ship a JavaScript runtime beside itself;
3. its tool wrapper "excludes tools with `needsApproval: true`" rather than pausing for
   them, which contradicts incode's rule that every call meets its gate;
4. it is experimental upstream.

The **idea** is adopted, built natively in incode, so the gate stays where it is.

## 3. Design

### 3a. One tool

A new Toolbox tool, working name `Script`, input `{ "code": string }`. `code` is the body
of an async JavaScript function; its `return` value is the tool's result, as JSON.

```js
const hits = await memory.recall_index({ query: "agy agent file", top_k: 20 });
const refs = hits.filter(h => h.preview.includes("agent_name")).slice(0, 3);
const full = await memory.recall_get({ refs: refs.map(h => h.ref) });
return full.map(f => f.line);
```

It is added to the catalogue every route already reads (`Toolbox::definitions`,
`src/toolbox/mod.rs:1432`) and dispatched beside the others (`dispatch_text`,
`mod.rs:2552`). So the API route and the tool server, and with it claude, codex and agy,
all get it with no per-agent code.

### 3b. The invariant: no second path to a tool

Inside the script, `tools.<Name>(input)` and `memory.<name>(input)` are host functions.
Each one calls `Toolbox::run_call` (`mod.rs:1743`), the same entry the API loop uses
(`api.rs:1074`), which runs `decide_and_run` (`mod.rs:1754`). Every gate therefore applies
to every call **inside** a script exactly as it applies to a call made directly:

- unknown tool refused (`mod.rs:1771`);
- a sub-agent's tool allowlist (`mod.rs:1783`);
- read-only and plan sessions refuse anything that is not observe-only (`mod.rs:2204`);
- rules that ask (`mod.rs:2213`) and the Prompt mode (`mod.rs:2229`) ask the person;
- the operator's hook (`mod.rs:1908`) and the engine's pre-tool gate (`mod.rs:1918`);
- a checkpoint before each change (`dispatch`, `mod.rs:2443`), so undo stays per call;
- telemetry (`run_call`, `mod.rs:1749`) and the trace.

No tool is reached from a script by any other route. The tests in §6 fail if one is.

`memory.*` are names for the recall tools already in the Toolbox (`recall_index`,
`recall_get`, `recall_timeline`, dispatched at `mod.rs:2611-2613`). Nothing is stored by
the script and no cache is kept: `src/recall.rs` stays the only reader of memory.

### 3c. What the script itself is

`Script` is classified as observe-only. It changes nothing by itself; anything it changes
is changed by an inner call, and that call meets its own gate. So:

- in a read-only session a script may read and recall, and an inner `Write` is refused
  with the same message a direct `Write` gets;
- in Prompt mode the person is **not** asked about the script as a whole. They are asked
  about each inner call that changes something, with the call's own input in front of
  them. Approving a whole program up front would ask a person to review code, and the
  inner calls would still be asked.

### 3d. Approvals in the middle of a script

`confirm_with_person` (`mod.rs:2245`) blocks until it is answered. The script runs on its
own thread, so an inner call that needs a person simply waits, and the script resumes
with the answer. A declined call throws a JavaScript error carrying the same text
`decide_and_run` returned, which the script may catch.

### 3e. What the script cannot reach

The context holds only `tools`, `memory` and a captured `console.log`. No file system, no
network, no `require`/`import`, no timers, no host objects. Every effect on the world goes
through §3b.

`Script` cannot be called from inside a script, and in the first version neither can a
tool that starts a sub-agent. Both are refused by name, so a script cannot fan out into
work nobody sees.

### 3f. Limits

| Limit | Why |
|---|---|
| Time spent running JavaScript, with time inside host calls (and waiting for a person) not counted | stops `while (true) {}` without punishing a slow build or a person reading |
| Memory for the script's heap | a script cannot exhaust the harness |
| Inner calls per script | bounds the work one request can start |
| Size of the returned value and of the captured log | the point is a small result; an oversized one is cut with a note, the way tool output is cut today |

The numbers are set when it is built, from measurements in §5, and named as constants.

### 3g. Failure and partial effects

An uncaught error ends the script. The result is an error that still lists every inner
call that ran, in order, and marks the ones that changed something. A model told only
"the script failed" would not know a `Write` before the failure had landed, and would
build on the wrong picture of the workspace.

### 3h. What the person sees

The script's source is shown and recorded as the `Script` call's input. Each inner call
is traced by `decide_and_run` as it is today, and reported to the activity view as a
child of the `Script` call, so the session shows what ran and not only that a script did.

### 3i. Telling the model what it can call

The tool's description lists each tool the session offers as a one-line signature built
from its JSON schema: name, required arguments, and what it returns. That list is
generated, never written by hand, so it cannot drift from the catalogue. It is kept short
because it is sent on every request. The full schemas are already in the definitions.

## 4. The engine

The script needs a JavaScript engine embedded in incode. Candidates, read from crates.io
on 2026-10-27; incode's floor is `rust-version = "1.88"` (`Cargo.toml:42`):

| Crate | Version | Licence | rust-version | Note |
|---|---|---|---|---|
| `rquickjs` | 0.14.0 | MIT | 1.87 | Bindings to QuickJS; builds its C source. Fits the floor. |
| `boa_engine` | 0.22.0 | Unlicense OR MIT | 1.91.0 | Pure Rust, but above the floor: an older version or a raised floor. |

**Proposed: `rquickjs`.** It fits the floor, and it is small and quick to start, which
matters for a tool that may run several times a turn. The C build is a cost on every
target, but not a new one: `Cargo.lock` already carries `cc` and `ring`, which compile C
on all three CI systems today.

UNVERIFIED until compiled: the interrupt handler used for the time limit, the heap
limit, and calling a blocking Rust function from an `async` script. The research gate
(`src/research.rs`) refuses a dependency the PRD never argued for, so the PRD names it in
the same change that adds it, with the version, licence and the alternative above.

**Alternatives considered:**

- **No engine; a JSON pipeline tool** (steps that refer to earlier steps' results). No
  dependency, but no loops, filters or conditions, and models write JavaScript far more
  reliably than a format invented here.
- **Do nothing.** Calls in one reply are already batched (§1). This is the right answer
  if §5 shows dependent chains are rare in real sessions.

## 5. Measure before claiming

No saving is claimed until it is measured. Two scenarios, run on the API route against a
stand-in transport that counts requests and input tokens, each with and without `Script`:

1. a memory recall chain: index, then get for the useful hits;
2. a code chain: grep, then read the files it found, then outline one of them.

The result is the request count and the input tokens for each, recorded in this document.
Then one live turn on one CLI route, read from the trace. If the counts do not fall,
`Script` is not offered by default.

### 5b. Measured, 2026-10-27

No API key was set on this machine, so the live runs used the claude route instead of a
stand-in transport: `incode -t … -p workspace-write -m sonnet --output-format stream-json`.
Every run was interleaved, and every answer was checked against the source (the 4 call
sites; `agent_name` at `agent_file.rs:36`).

- **Tokens and cost** come from the turn's one `usage` event, which on this route is the
  sum over every request (`agent/claude_code/wire.rs` `result_events`).
- **Requests are DERIVED**: one per run of consecutive `tool_use` events, plus the final
  answer. The route does not report a request count.

Modes, set by the prototype's switch (removed with it): `off` = no Script; `beside` =
`INCODE_SCRIPT=1`; `only` = `INCODE_SCRIPT=only`.

| Task | Mode | Runs | Requests | Input processed (avg) | Output (avg) | Cost (avg) | Tool output chars (avg) |
|---|---|---|---|---|---|---|---|
| memory chain | off | 2 | 3, 4 | 101,899 | 945 | $0.082 | 6,553 |
| memory chain | beside | 2 | 3, 3 | 100,087 | 818 | $0.105 | 7,206 |
| memory chain | only | 2 | 4, 4 | 78,802 | 1,430 | $0.092 | 8,494 |
| code chain | off | 2 | 4, 4 | 152,843 | 1,816 | $0.157 | 35,068 |
| code chain | beside | 2 | 4, 4 | 156,050 | 1,712 | $0.157 | 32,023 |
| code chain | only | 1 valid | 5 | 84,881 | 1,830 | $0.098 | 9,101 |

What it shows:

1. **`beside` saves nothing.** On the memory task the model never called `Script`. On the
   code task it called it only to batch reads, and in the first trial it returned the raw
   text (27,689 characters).
2. **Requests do not fall in any mode** (3 to 5 everywhere). The model already sends
   independent calls together in one reply (`api.rs:1067-1090` runs them all in one
   step), and in `only` mode it writes several small exploratory scripts in a row, much
   as it would make calls.
3. **`only` cuts the context.**
   - Code chain: input processed fell 44% and cost 38%. Tool output fell from about 35k
     to 9k characters, because the scripts returned what they found and not the files.
   - Memory chain: input fell 23%, but cost rose 12%. Writing code costs OUTPUT tokens,
     which are priced several times higher than input, and `only` wrote half again as
     many (1,430 against 945).
4. **The samples are small.** Two runs per arm, one valid for code/`only`. The spread
   between repeats of the same arm (up to 48% in input processed) is as large as some
   of the differences.
5. **`only` can deadlock against the engine's session gate.** In run `code-only-1` the
   gate refused the `Script` call itself three times ("This session has not oriented").
   The call that opens the gate (`session_note`) can only travel inside a `Script`, so
   the session never started, and the run is excluded from the table. Each inner call
   already meets the engine gate through `run_call`, so the open question is whether
   the outer `Script` call should be put to the engine at all (§8).

Each run's events are kept outside the repository (the scratch measurement folder) and
are not part of this document.

## 6. Tests

Each is written to fail when its defect is put back, and each is run once with that
defect in place before it is trusted.

| Test | The defect it catches |
|---|---|
| A script's `Write` in a read-only session is refused with the direct call's message | an inner call that skips `decide_and_run` |
| A sub-agent's script cannot call a tool outside that agent's list | the allowlist checked only for direct calls |
| In Prompt mode each changing inner call is asked once; a declined one throws, and the script can catch it | approvals skipped, asked twice, or ending the turn |
| `while (true) {}` is stopped by the time limit; a call that waits on a slow stand-in tool is not | the clock counting host time, or not counting at all |
| `typeof require`, `fetch` and `process` are all `undefined` | an ambient capability leaking into the context |
| `Script` inside `Script` is refused | unbounded nesting |
| A failure after a `Write` reports the `Write` as landed | partial effects hidden from the model |
| `memory.recall_index` reaches the same handler as a direct `recall_index` | a second, unrecorded route to memory |
| An oversized return is cut with a note | the result cap not applied |

## 7. Order of work

1. Engine spike behind a test only: create a context, call one blocking host function,
   stop a loop with the interrupt handler, set the heap limit. Settles §4's UNVERIFIED
   items.
2. `Script` in the Toolbox with §3b, §3e and §3f, and the tests in §6.
3. The measurement in §5, recorded here.
4. Offered by default only if §5 shows a saving; until then, offered when the session
   asks for it.

## 8. Open for the owner

- The tool's name (`Script` is a working name).
- Whether a sub-agent tool should ever be callable from a script (§3e refuses it).
- The default limits, once §5 has numbers to set them from.
- Whether to continue at all, on §5b: `beside` saved nothing, and `only` cut input by
  23-44% while raising output, on samples too small to set a default from.
- If it continues: whether the outer `Script` call skips the engine's pre-tool gate, as
  it changes nothing itself and each inner call meets that gate (§5b item 5). Today it
  can deadlock a session in `only` mode.
