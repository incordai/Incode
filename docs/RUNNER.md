# The Task Ledger and the Runner — Phase 5 design

**Status:** BUILT as of 2026-08-17 — `src/ledger.rs`, `src/runner.rs`, `/run`.
This document is the design; the code is the authority where they differ.
**Closes:** R5. R6 is partial — the loop runs, but never yet against a live model.

> [!CAUTION]
> **One stop condition remains blocked.** The loop is built and three of its four stop
> conditions work. "A decision needs the owner" currently HALTS AND REPORTS rather than
> asking and waiting, because §9.7 of the PRD is a hard dependency: `get_owner_wait` returns `{decided, verb, note}` with no `by`, so an
> approval the owner gave and an approval the proxy gave for them are the same bytes.
> A runner whose stop condition is *"a decision needs the owner"* cannot implement
> that condition against an answer it cannot attribute. §7 below proposes a bounded
> workaround, but the workaround needs verifying before it is trusted.

---

## 1. The law

> [!IMPORTANT]
> **The runner may decide what to do next. It may never decide that something is done.**

Choosing the next task is scheduling, and scheduling is cheap to get wrong — a bad
order wastes a turn. Declaring a task complete is a claim about the world, and a wrong
one is absorbed silently into every task that follows.

Both autonomous loops in this system have already failed on exactly this line, in the
same way:

- **The loop-hold failure.** A question was raised, auto-answered by the proxy, and the
  agent was told *"the owner approved — proceed (they said ok)."* Nobody had approved.
- **The audit loop.** Four cross-layer findings reached status `approved` because the
  coder agent confirmed each with a file:line — having read only the handler. Four
  false positives in a row from a loop reporting itself verified.

One disease, twice: **an automated approver sitting in a seat that required judgement,
and its verdict labelled as though a person had given it.** Every rule below exists to
keep the runner out of that seat.

The mechanical consequence: there is **exactly one edge into `done`**, and its
precondition is a process exit status. Not a model's summary, not a subagent's final
answer, not the absence of an error. An exit code, recorded with the command that
produced it.

---

## 2. Two files, one of them append-only

Per §9.5 the ledger is a file in the repo, not memory. Memory holds the *findings*;
the file holds the *state*.

> [!IMPORTANT]
> **The ledger is not the roadmap. It derives from it.**
>
> The proxy already owns the plan: `ProjectBrief.roadmap: Vec<RoadmapItem>`
> (`owner_web.rs:355`), whose open items ride every review prompt. A `RoadmapItem` is
> `{id, title, status: todo|doing|done, note, at}` — a **plan**, with no `verify`, no
> `attempts`, no evidence. It answers *what should happen*.
>
> The ledger answers *what happened to it*: verification command, attempt counts, exit
> codes, lease. Those belong to execution, not to the plan, and putting them in the
> brief would push execution detail into a prompt that rides every approval.
>
> So every ledger task **references a `RoadmapItem` id** — it is never a task the
> roadmap does not contain — and status flows back: `running` writes `doing`, a
> verified `done` writes `done`, with the note carrying the evidence. Two lists that
> can disagree is the duplicate-memory disease in a second costume; one list with an
> execution record attached is not.

```
.incode/
├── ledger.json     current state — one entry per task, rewritten in place
├── journal.jsonl   every transition, append-only, never rewritten
└── runner.lock     single-runner mutex; holds pid + lease
```

**`ledger.json`** is the materialised view. It is what a human reads and what a diff
shows.

An earlier draft specified TOML, on the grounds that "JSON reorders and produces noise
diffs". That is true of *maps* and false of this shape: the ledger is an ordered array
of tasks, and `serde_json` preserves array order, so the same state serialises to the
same bytes. TOML would have meant a new dependency for a formatting preference, while
`serde_json` is already a direct dependency — so the implementation uses JSON.

```json
{
  "tasks": [{
  "id": "t-014",
  "roadmap_id": "r-014",
  "title": "Attach memory through the MCP client",
  "state": "ready",
  "depends_on": [],
  "verify": ["cargo", "test", "memory"],
  "attempts": 0,
  "evidence": null,
  "note": ""
  }]
}
```

**`journal.jsonl`** is the record of what moved and on what evidence. Append-only, so
its diff is always pure addition and its history cannot be quietly rewritten by the
process that failed.

```json
{"ts":"2026-08-14T09:31:02Z","task":"t-014","from":"running","to":"done",
 "by":"runner","evidence":{"kind":"command","argv":["cargo","test","..."],
 "exit":0,"ms":41207,"tree":"8f3c1ad"}}
```

**The ledger is derivable from the journal.** Replay reconstructs it. That is the
crash-recovery story and the audit story at once: if the process dies mid-write, the
journal is the truth and `ledger.json` is rebuilt from it.

---

## 3. States

Seven, and no more. Anything that looks like an eighth is an attempt counter or an
evidence record wearing a state's clothes.

| State | Meaning |
|---|---|
| `pending` | in the ledger; `depends_on` not yet satisfied |
| `ready` | eligible for pickup |
| `running` | claimed by a runner, lease held |
| `blocked` | an owner decision is outstanding; carries the ask token |
| `done` | verified by exit code — **terminal** |
| `abandoned` | failed its attempt budget; will not be retried |
| `cancelled` | the owner dismissed it |

There is deliberately **no `failed` state**. A failure is an increment to `attempts`
plus a journal entry, and the task returns to `ready`. A state nothing ever observes
is not a state.

`done` is terminal. If a completed task turns out to be wrong later, that is a *new*
task with its own evidence — not a quiet reopening that erases the record of what was
verified and when.

### Transitions

| From | To | Trigger | Evidence written |
|---|---|---|---|
| — | `pending` | task admitted from an approved roadmap | roadmap commit sha |
| `pending` | `ready` | every `depends_on` is `done` | the satisfied dep ids |
| `ready` | `running` | runner claims it | runner id, lease expiry |
| `running` | `done` | **verify exited 0** | argv, exit, duration, tree hash |
| `running` | `ready` | verify exited non-zero, `attempts < max` | argv, exit, stderr tail |
| `running` | `abandoned` | verify failed at `attempts == max` | all attempt records |
| `running` | `blocked` | the task needs an owner decision | ask token, severity |
| `running` | `ready` | lease expired (crash) | `crashes += 1` |
| `blocked` | `ready` | owner **approved** | verb, note, **`by`** |
| `blocked` | `ready` | owner **changed** it; title/verify updated from note | verb, note, `by` |
| `blocked` | `cancelled` | owner **dismissed** | verb, note, `by` |
| `blocked` | `blocked` | owner **held** | verb, note, `by` |
| `abandoned` | `ready` | owner explicitly resets, counters cleared | owner action |

Note the shape: **every arrow out of `blocked` requires a `by`.** That is why §9.7
blocks this phase rather than merely inconveniencing it.

---

## 4. Evidence

A closed set of three kinds. The point of closing it is that the weak kind cannot be
substituted for the strong one by accident.

| Kind | Produced by | May cause |
|---|---|---|
| `Command` | a process the **parent** ran | `done`, `abandoned`, retry |
| `OwnerDecision` | `owner_wait` | leaving `blocked` — never `done` |
| `Claim` | the subagent's final answer | **nothing** — recorded only |

**`Claim` is recorded and is never load-bearing.** The subagent's report goes into the
journal labelled as a claim, with the model that produced it, because it is useful when
reading back a failure. It moves nothing. This is the audit-loop lesson written down:
the thing that went wrong was not that the coder agent was mistaken, it was that its
say-so was wired to a status field.

### Three rules that keep `Command` honest

**The parent runs the verification, never the child.** A subagent that runs its own
test command and reports the result is reporting on itself. The parent claims the task,
delegates the work, and then runs `verify` in its own process against the tree on disk.

**No `verify`, no `done`.** A task with an empty `verify` field cannot reach `done` by
any path. It goes to `blocked` with an owner ask, because a task nobody can check is a
decision, not a chore. In Rust this should be unrepresentable rather than merely
unwritten:

```rust
/// Constructible only from a process that exited 0.
pub struct Verified(CommandEvidence);

impl Verified {
    pub fn from(ev: CommandEvidence) -> Option<Self> {
        (ev.exit == 0).then(|| Verified(ev))
    }
}

// The only fn that writes `done`, and it cannot be called without one.
fn complete(task: &mut Task, proof: Verified, journal: &mut Journal) { … }
```

If `complete` cannot be called without a `Verified`, and `Verified` cannot be built
without an exit code, then the law is enforced by the compiler and not by anyone
remembering it.

> [!WARNING]
> **The verify command is exempt from the recorder.** Phase 2 wires `watchman_lookup`
> so that a proven-deterministic `(tool, input)` is served instead of run. A verify
> command is exactly the shape that memoises well — same argv, same project — and if it
> is ever served from the recorder, the runner will mark tasks `done` on the strength
> of a test run that happened last week against different code. **Phase 2 and Phase 5
> are dangerous together and this is where.** The exemption belongs in the code that
> calls the recorder, with a test asserting a verify argv is never looked up.

**Where the command comes from (2026-08-17).** `verify::command_for` climbs the admin
ladder: `INCODE_VERIFY_CMD`, then `verify_command` in the operator's settings file,
then detection from the project manifest. Detection is last on purpose — it guesses
`cargo test` from a `Cargo.toml`, which is right for a plain crate and wrong for
anything with a real test story, and a guess must never outrank something a human wrote
down. `--admin` prints which of the three a run will use. The ceiling the runner stops
on resolves the same way, so both halves of "when does this stop?" are answerable
without reading the source.

---

## 5. The loop

```
start:
  acquire runner.lock                          → else stop(AlreadyRunning)
  replay journal → ledger                      (crash recovery)
  reclaim expired leases: running → ready, crashes += 1
  watchman_handoff(project)                    what this project already solved
  recall(rules for this repo)                  standing rules from memory (Phase 3)
  resolve any outstanding blocked ask tokens first

loop:
  if cancelled()            → stop(Cancelled)
  if budget.spent()         → stop(BudgetSpent)          (Phase 4)
  promote pending → ready where deps are done

  task = ledger.next_ready()
      none, and something is blocked  → stop(NeedsOwner)
      none                            → stop(LedgerEmpty)

  if task.verify is empty   → block(task, "no verification command"); continue

  claim(task)                                  lease = now + 15m, heartbeat renews
  claim_report = delegate(task)                subagent; cheap model once Phase 6 lands
  journal.record(Claim { claim_report })       recorded, load-bearing on nothing

  ev = run_verify(task)                        parent's own process
  match ev.exit:
    0  → complete(task, Verified::from(ev))
    _  → task.attempts += 1
         if task.attempts >= max_attempts:
             abandon(task, ev)
             → stop(TaskFailedTwice)
         else:
             release(task, ev)                 back to ready, next iteration retries

stop(reason):
  journal.record(Stop { reason, spent, done_count, remaining })
  owner_post(reason)                           the body's channel, not a new one
  release runner.lock
```

The runner picks the next ready task in ledger order among those whose dependencies
are met. Ordering is a scheduling decision, so it is allowed to be a heuristic — that
is the half of the law where being wrong is cheap.

### 5a. Dependencies the planner declares — added 2026-09-21

`Task::depends_on` existed and `Task::with_dependencies` had **no production
caller**: every dependency in the codebase was written by a test. With lanes,
that is not a dormant feature — it is a correctness hole. `parallel::batch`
takes ready tasks and runs them at once, and if "ready" never encodes
ordering, two tasks that must happen in sequence go into the same batch and
the merge is left to sort out what the plan should have.

A planner now writes it in the step:

```
<plan>
explore: find every writer of the status row
implement: fix the one that clears it
implement: add the regression test [after 2]
</plan>
```

`[after N]` is parsed off the end of the instruction and removed from it, so
the agent never sees the bookkeeping. Resolution rules, all of them chosen to
fail in the safe direction:

- Resolved against steps **already seen**. A forward reference names nothing
  the ledger can wait for, and a task waiting on a dependency that never
  arrives is a run that stops for a reason nobody can act on.
- A number naming a **folded** step resolves to the task that absorbed it —
  the work really is in there.
- Anything unresolvable is **dropped**, not kept. The worst case of a dropped
  dependency is two lanes that collide, which the merge already refuses and
  retries. The worst case of a kept-but-unsatisfiable one is a dead queue.

---

## 6. Stop conditions

Nine are implemented. `GuardDenied` below is still a design note, not code.

| Reason | Fires when |
|---|---|
| `LedgerEmpty` | no ready task, nothing blocked, **and every roadmap goal proved reached** — the only one that means "finished" |
| `NothingRunnable` | no ready task, but work is still parked or waiting. Names every waiting task and says plainly they are NOT finished — shipped 2026-08-17, because reporting this case as `LedgerEmpty` is how held work disappears |
| `Stranded` | work is open that is neither ready nor waiting on anyone — added 2026-09-22. `next_ready` finds `Ready` and `waiting_ids` finds `Blocked`/`Pending`; **`Running` was in neither, and nothing reclaims it.** A run killed mid-task left that state on disk and the next run walked past it into the goals check. Measured with a control: the run reported `LedgerEmpty` — *every task is finished* — with a task still marked in progress. Its own reason rather than folded into `NothingRunnable`, whose message says the tasks are "waiting on somebody"; these are waiting on nobody |
| `GoalsUnmet` | every task ran and a roadmap item is still not proved — see §6a |
| `GoalsUnchecked` | every task ran and the goals could not be READ, so nothing was proved — see §6a |
| `BudgetSpent` | ceiling reached |
| `TaskFailedTwice` | a task hit its attempt budget and was abandoned |
| `NeedsOwner` | an ask is outstanding, unanswered, or could not be put at all |
| `Cancelled` | the existing cancel token fired (R12, already shipped) |
| `GuardDenied` | a standing rule loaded from memory forbade the next action (Phase 3) |
| `AlreadyRunning` | another runner holds the lock |

**Every stop writes a reason and posts it.** The PRD's success test is that the harness
*"stops for a stated reason rather than drifting"* — so an unlabelled exit is a bug of
the same class as a wrong `done`, and the journal should have no `Stop` record without
one of the seven above.

`TaskFailedTwice` halting the whole run is the PRD's literal wording and is what is
specified here. The alternative — abandon that one task, continue with the rest, halt
only if something depended on it — is more useful and less safe, and it is listed in
§9 as needing a decision rather than being chosen quietly.

---

## 6a. The final testing — added 2026-09-21

**A drained queue used to be reported as finished work.** Every task could pass
its own check and leave the roadmap item undone, so the loop was able to
complete a whole plan, announce success, and be wrong. `goal.rs` had been
written to answer exactly this and its own header claimed the runner called it.
Nothing did — only a slash command.

So when the queue drains, the runner now runs **every roadmap item's check**
before it may say `LedgerEmpty`. A task's check proves a change compiles; a
goal's check proves the thing the owner asked for exists. Costly on purpose —
one command per item — which is why it is here, once, at the end.

### "Could not check" is its own answer

The roadmap lives in the brief, which is fetched over HTTP. Three outcomes, not
two, because they send an operator to opposite places:

| | Means | What to do |
|---|---|---|
| `LedgerEmpty` | checked, all reached | nothing |
| `GoalsUnmet` | checked, something is not reached | the code does not do what was asked |
| `GoalsUnchecked` | the roadmap could not be read | start the memory service |

Collapsing the third into the first is silent certification, and it would fire
**precisely when the service is down and nobody is watching**. Collapsing it
into the second sends someone to debug code in order to fix a stopped process.

A project with no brief has no roadmap, so there is no goal to be unmet:
`NoBrief` is `LedgerEmpty`, not a complaint. A gate that fires when it has no
information is a gate people switch off.

### Injected, like the owner and the journal

`RoadmapGoals` is a trait with `BriefGoals` (the real one) and `NoGoals` (the
default, which reports *unchecked* and deliberately never *reached*). A gate
whose default certifies is not a gate. It also keeps the network out of the
scheduling tests, which would otherwise pass or fail on whether the memory
service happened to be running.

---

## 6b. Lanes — more than one agent at once

Full design in [`TEAMWORK.md`](TEAMWORK.md) §4–5. What matters here is how
little of this loop it touches:

```
refresh_ready()
run_lanes_if_useful()   <- works a batch concurrently, caches their reports
pick the next task
report = cached, if a lane already did this one; else worker.work(task)
verify / record / escalate                                      <- unchanged
```

Lanes produce **reports only**. Every rule in §1 still lives in one place —
re-implementing attempts, escalation and the edge into `Done` for a parallel
path would mean two copies, and the one that gets fixed is never the one that
ran.

Three properties worth stating here because they are the loop's, not the
lanes':

- A cached report is **consumed, never reused.** A retry must actually re-run
  the agent, or the attempt limit becomes a way of recording one failure twice.
- **A lane that did not merge is a failed attempt**, whatever the agent said.
  Its work is on a branch, not in the tree the check is about to run over.
- The check runs on the **merged** tree. Two lanes can each pass alone and
  break together.

Default is one lane, which is byte-for-byte the behaviour above.

### A conflict does not throw the work away — added 2026-09-21

A conflicted lane wrote a real change and committed it. The merge failed
because another lane moved the same lines; the change is not wrong, the tree
is different. The task goes back to the queue, and for a while the retry was
handed a bare task — so it started from an empty tree and wrote the whole
thing again. One piece of work, paid for twice, with the nearly-right version
left on a branch nobody named.

`Task::unmerged_branch` now carries that branch, and the retry's instruction
reads:

> An earlier attempt at this task already wrote the change, and it is
> committed on the branch `<name>`. It could not be merged because another
> piece of work touched the same lines while it was being written — the
> change is not wrong, the tree moved under it. Start from that work, not
> from nothing: read it with `git diff HEAD...<name>`, then apply it to the
> tree as it is now, keeping the other change intact.

Three things this is careful about:

- **It is a fact, not a failure message.** `Task::owner_note` is the one line
  a retry must follow and it belongs to the owner; `note` is a record of
  everything that moved the task. Feeding a failure string into the prompt as
  if it were an instruction is worse than sending nothing, which is why this
  is a separate field holding a branch name and nothing else.
- **Only a conflict sets it.** A lane whose agent failed left nothing worth
  reconciling; pointing the next attempt at that branch sends it after an
  empty change.
- **It is cleared on `complete`, and on any later merge that lands.** A name
  that outlives the work it described points a reader at nothing.

The conflict summary now also names the branch. It used to be built with an
empty one, so every conflict note said the work was *"kept on branch ``"* — in
the one message whose entire job is to say where the work went.

---

## 6c. The agent's conversation is dropped between tasks

An agent CLI's conversation is cached on the vendor's side and every later turn
is billed against the whole of it, so one conversation spanning a run made the
last task in a plan the most expensive — carrying context the runner is
**already** re-supplying through `TaskWorker::brief`. Two channels for one
handover, one of them metered.

It is also isolation: tasks are deliberately routed to different models, and a
conversation spanning them mixes what a `review` agent saw into what an
`explore` agent is asked.

Dropped **per task, not per attempt** — a retry keeps its conversation, because
an agent that cannot remember what it just tried does not make a second
attempt, it makes the first one again. `TaskWorker::release` then runs on every
exit path, including the cancel and owner paths, which are the ones that sit
idle longest.

---

## 6d. Research tasks — judged by an artifact, not by the build — added 2026-09-21

### The bug that caused the fold

A live run planned an item into six steps and marked **four** of them `Done`,
including `explore: locate the ball entity` and `plan: specify the falling
behaviour`. Each was judged by the project's own check, `node --check
game.js`, which passed for reasons that had nothing to do with the step. Only
the `implement` step had earned it.

`roles::changes_code` was the answer: `explore`, `diagnose`, `plan` and
`review` produce **information**, and information is not provable by a build.
They stopped being tasks and became prose folded into the change they served.
That was honest and it fixed the fabrication.

### What the fold cost

A folded step is not a task, so it is never **routed**. The whole point of
`roles.rs` is that finding where something lives should run on a cheap model
and deciding whether a design is sound should not — and none of it was
happening: every read-only step ran on whichever model got the `implement`
task. The routing table was decoration. The test that was supposed to prove
routing worked asserted `asked_for == [["sonnet"]]` for a three-step plan and
passed, because there was only ever one task.

### What happens now

`explore`, `diagnose` and `review` are tasks again, with a different judge.
Each gets a **deliverable** — `.incode/findings/<task-id>.md`, fixed at
planning time — and its check is the command that reads it:

```
incode --check-findings .incode/findings/r-1.2.md
```

That command exits zero when the file exists and holds at least 200
non-whitespace characters, and non-zero with a message written for the agent
that will read it on its next attempt. The completion law is untouched: a
process exited zero, and `Verified` was built from that exit code and nothing
else.

`plan` still folds. These steps come **out** of a plan, so a `plan` step
inside one is the planner narrating itself, and giving it its own agent, turn
and deliverable pays twice for the decision that produced the list it appears
in.

### What this does NOT prove, stated plainly

That the findings are **correct**. Nothing reads them for sense — a model
judging a model is exactly what `verify.rs` exists to refuse — so a confident,
wrong answer of sufficient length passes. This check proves one thing: the
step produced the artifact it was asked for. That is strictly more than the
fold proved, which was nothing, and strictly less than a build proves about
code. What actually catches bad research is the task that depends on it: an
`implement` task built on a wrong diagnosis fails its own check, which is a
real command over real code.

### Ordering, which the fold used to guarantee for free

Putting the research inside the change's prompt made it sequential by
construction. As separate tasks it has to be said:

- A change **depends on** every research task planned before it, and its
  instruction lists their paths — *"Work already done for this task, read it
  before you start"*. Without the paths the agent goes and does the reading
  again, and the run pays twice for one answer.
- Research placed **after** a change depends on that change. Otherwise a
  closing `review` is free to run beside the `implement` task it was written
  to judge, and delivers a verdict on code that does not exist yet.

### Finding the binary that runs the check

The check's argv starts with the literal `incode`, not an absolute path: the
ledger outlives the binary that wrote it and may be read on another machine.
`verify::run` resolves that one name at the moment the check runs —
`INCODE_SELF_BIN` if the operator set it, else this process when this process
really is `incode` (by file stem, because `current_exe` answers for every
process including ones that would not recognise the flag), else the bare name
for an ordinary `PATH` lookup.

---

## 6e. Which agent spent it — added 2026-09-21

The ceiling answers *"is there budget left"*. An operator running four models
across three subscriptions is asking a different question — **which one is
burning it** — and a single total cannot answer that.

`RunOutcome::spend_by_agent` attributes every attempt's tokens to the agent
identity that made it, passing or failing, and `spend_report()` ranks them.
Two details that are easy to get wrong:

- Attributed to the **agent**, not the model. The agent is the identity and
  it chooses the model; a fallback changes what answered without changing who
  asked, and keying on the model would split one worker's history the first
  time a provider was busy.
- Summed as plain totals rather than folded as `TokenUsage`. `merge` exists to
  reconcile the two reports of one streaming response and takes the larger of
  each field — across separate turns that keeps the biggest turn instead of
  adding them up.

**Lanes are charged too.** A lane is a different worker in a different thread
that the runner never touches, so `LaneOutcome::spent` carries its usage back
and the runner records it — including for lanes that failed or could not
merge. They spent the tokens all the same, and a ceiling that only counted
successes would never stop a run that is going badly, which is the only run
whose ceiling matters. The control: two lanes spent 10k each and the run was
charged 0.

---

## 7. Escalation, and the §9.7 dependency

The runner does not build an approval channel. It calls `owner_ask` → `owner_wait` →
`{decided, verb, note}`, maps `approve|dismiss|hold|change` onto the four arrows out of
`blocked`, and posts progress with `owner_post`.

The problem is the response shape. Verified in current source: the response built in
`get_owner_wait` (`api/owner_feed.rs`) is `{ "decided": true, "verb": …, "note": … }`
— no `by`. A `by` **is** recorded at `api/owner.rs:256`, but onto the `owner_web`
event stream, not into the reply the agent reads. So:

> Without `by`, *"the owner approved"* and *"nobody approved"* are the same string.

### Threading `by` through is necessary but NOT sufficient

Read the value before relying on it. At `api/owner.rs:244`, `:256` and `:322`, `by` is
computed identically:

```rust
std::env::var("MEMORY_LOCAL_USER").ok().filter(|u| !u.is_empty())
    .unwrap_or_else(|| "owner".to_string())
```

That names **the local machine's user, not the actor who decided** — and it falls back
to the literal string `"owner"`. Threading *this* field into the reply would hand the
runner a value that says `owner` regardless of whether a person or the proxy answered,
which is the original defect wearing a field name.

So §9.7 of the PRD needs restating, and this design depends on the restated version:

> `by` must be set from the deciding actor — a human reply and a `proxy::gate`
> auto-decision must produce provably different values — and then carried into the
> `owner_wait` response.

A test that asserts a proxy-decided card never reports `by: "owner"` is the thing that
makes this real. Without it, the field is decoration.

### The proposed workaround, and what to check before trusting it

`proxy::gate` auto-decides a card when its severity is at or below `auto_max` and it is
not stamped with `GUARD_SOURCE` (`cli/escalate.rs:24-27`, `:406`). A card **above**
`auto_max` cannot be auto-decided, so any answer it receives is necessarily human.

That gives a bounded fallback: **the runner reads the effective `auto_max` at startup
and asks at `auto_max + 1`.** If `auto_max` is already at the ceiling, no severity is
provably human, and the runner refuses to start rather than escalate into a channel
that may answer itself.

Verified since this was drafted:

- `DEFAULT_AUTO_MAX_SEVERITY = 4` (`config.rs:110`), so the loop-hold card at severity
  3 was indeed below the line — it was auto-decidable by configuration, not by accident.
- `effective_auto_max` clamps to 5 (`owner_web.rs:433-439`). So `auto_max + 1` is
  available at the default, and the refuse-to-start branch is genuinely reachable when
  an operator raises `auto_max` to 5 — it is not dead code.

Still to verify before relying on it:

1. That `gate` refuses every card above `auto_max`, with no second path deciding on
   confidence alone.
2. That `owner_ask`'s PIN requirement at severity 4+ is enforced server-side, not merely
   offered by the client. (`ReplyOutcome::PinRequired` exists at `api/owner.rs:262`,
   which is evidence for it but not proof of where it is checked.)
3. That an unanswered high-severity ask times out rather than pinning the runner.

**This is a workaround, not the fix.** The fix is the restated §9.7 above: `by` set
from the deciding actor and carried into the reply. The workaround buys the ability to
start Phase 5 before that lands; it does not make §9.7 optional.

---

## 8. What the runner must not build

Per the PRD's one-body principle, the runner is hands. Every temptation below is
something the body already has, and each would look reasonable in isolation:

| Tempting to add | Already the body's |
|---|---|
| a notification when a run stops | `owner_post` |
| an approval prompt for unattended runs | `owner_ask` / `owner_wait` |
| a table of findings per task | `remember`, `audit_findings` |
| a cache of task results | `watchman_*` (with the §4 verify exemption) |
| a rules file next to the ledger | `recall` against memory |
| a retry policy DSL | `attempts`, and nothing more |

The ledger file is the **one** thing that is legitimately the harness's own, and only
because §9.5 established that a ledger needs ordering, per-task state and auditable
transitions — which durable storage does not provide.

---

## 9. What this design does not settle

1. **Attempt budget.** `max_attempts = 2` is taken from the PRD's *"failed twice"*.
   Whether a crash counts against it is answered here (it does not — `crashes` is
   separate, capped at 2, then abandoned) but not confirmed by anyone.
2. **Does `TaskFailedTwice` halt the run or skip the task?** §6. The PRD says halt.
3. **Permission tier for unattended running.** §9.4 of the PRD is still open, and the
   runner is not safe to start without it — the tier that makes it useful is the tier
   that permits shell commands.
4. **Concurrency.** This design is single-runner behind a lockfile. Multiple runners
   over one ledger needs a real claim protocol and is not attempted.
5. **Who admits tasks to the ledger.** Written here as an owner action against an
   approved roadmap. Whether the harness may propose additions to its own queue is a
   trust question, not a scheduling one, and it is the natural next thing to get wrong.

---

## 10. Done when

Each of these is a test, not an impression.

- [ ] A task with an empty `verify` never reaches `done` by any path.
- [ ] `complete` cannot be called without a `Verified`; `Verified` cannot be built from
      a non-zero exit.
- [ ] A subagent claiming success on a task whose verify fails leaves the task `ready`
      with `attempts = 1`, and the claim is in the journal marked as a claim.
- [ ] A verify argv is never submitted to `watchman_lookup`.
- [ ] Killing the runner mid-task and restarting reclaims the lease, increments
      `crashes`, and rebuilds `ledger.json` from the journal alone.
- [ ] The runner completes several roadmap tasks unattended and exits with one of the
      seven named reasons — with the journal containing no `Stop` lacking a reason.
- [ ] A proxy-auto-decided card never moves a task out of `blocked`.
      **This test cannot pass until §9.7 lands.**
