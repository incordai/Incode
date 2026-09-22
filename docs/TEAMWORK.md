# Agents working as a team

**What this covers.** Two things that sound separate and are not: how agents
working the same project **talk to each other**, and how more than one of them
**runs at the same time**. The first is useless without the second having been
thought about, and the second is dangerous without the first.

**Where the code is.** `src/team.rs` (the channel), `src/team_mcp.rs` (reaching
it mid-turn), `src/parallel.rs` (lanes), and the wiring in `src/runner.rs` and
`src/agent/worker.rs`.

---

## 1. The failure this exists for

Work is split by role and handed to different models. One is given the login
flow, another the schema underneath it — and the login flow cannot be written
without knowing what the schema calls a user.

Today the second agent guesses. The guess is found at integration, by which
time the fix touches both pieces and whichever agent wrote the first has long
since finished and been paid for.

[`src/contracts.rs`](../src/contracts.rs) already freezes the shapes that
**can** be written down before the split. This is for everything else: the
question that only occurs to someone once they are inside the work.

---

## 2. The channel

`.incord/team.jsonl`, append-only, one JSON object per line.

### Why a file

Same constraint as the desktop journal (`COMPUTER_USE.md` §14c), and the same
answer. An agent is a **subprocess** with its own loop; the harness does not
share memory with it, and there may be no second agent alive at the moment a
question is asked. A file is the one medium every participant can reach, and it
keeps the conversation when nobody is listening yet.

### The sequence number is the cursor, not a byte offset

The file is trimmed at 256 KB. Trimming moves every later record, so a reader
holding a byte position resumes mid-line or replays what it already showed.
Each message carries a monotonic `seq`, and a restarted harness resumes after
the highest one it finds — otherwise an answer citing `#4` would come to mean
something else.

### Nobody blocks waiting for an answer

The obvious design — ask, and wait — deadlocks the moment the only other
participant is the agent doing the waiting, which in a sequential run is
always. So **an ask records and returns.** The agent carries on with its best
assumption.

That is correct, and it has exactly one condition: somebody has to SEE the
question afterwards. Three things make that true:

- An open question keeps being put in front of agents who could answer it,
  **ignoring the read cursor**, until an answer arrives. Without this a
  question asked while an agent was mid-turn is already behind that agent's
  cursor by the time it reads again — asked once, seen by nobody, open forever.
- `runner::unanswered_questions` is printed after every run: *"N question(s)
  between agents went unanswered — each one is a guess somebody made."*
- An answer stops the question being shown, immediately.

A channel whose unanswered questions sit in a file nobody opens is not a
channel. It is a place guesses go to look like decisions.

### Addressing is a closed vocabulary

Recipients are the roles in `roles::KNOWN_ROLES` (`explore`, `diagnose`,
`plan`, `implement`, `review`, `frontend`, `grunt`) or `all`.

**A name outside that list becomes a broadcast**, and the sender is told. This
is not tidiness. An agent asked to name a recipient invents a plausible one —
`schema`, `auth`, `db` — and a message addressed to an invented name is lost in
the worst possible way: the asker believes it asked, and nobody was ever going
to answer. This was found by the first test written against the feature, which
invented `schema` exactly as a model would.

---

## 3. Reaching the channel mid-turn

The channel is read from the prompt and written from the report, so on its own
an agent can only speak at the **start and end** of its turn. That is enough
for a question it already knew it had, and no use at all for the ones that
matter — the question that occurs to you at the moment you open the file and
find the column is not called what you assumed. By then the prompt is long gone
and the report is many tool calls away, so the agent does the only thing left:
it guesses, and writes the guess.

An MCP server is how a running agent reaches anything (see `COMPUTER_USE.md`
§1 — the harness owns no model loop). So the channel gets one.

```
incode --team-mcp --team-channel <path>
```

Four tools: `team_ask`, `team_tell`, `team_answer`, `team_read`.

### Identity comes from the environment, never the caller

A tool that took `from` as an argument would let an agent file its words under
another agent's name — and the harness **records** those words, so it would be
recording a claim as a fact.

`INCORD_TEAM_AS` is set by the harness on the agent's process when it spawns it
for a task. The MCP server is a **child** of that agent, so it inherits the
variable, and the role is settled before the model has said anything.

This indirection is load-bearing and worth keeping: an agent CLI's MCP servers
are configured **once**, and the role changes **every task**. The registration
cannot carry the identity. The environment can.

> **Verified live, 2026-09-21.** A real `claude-code` agent read the harness's
> `--mcp-config`, spawned `incode --team-mcp`, called `team_tell`, and the
> message landed on disk as `from: implement` — the identity it had inherited.

### Messages from other agents are UNTRUSTED

GLOBAL RULE 17. Every other participant is a language model, and a message is
the most direct route there is from one model's output into another's context.
The warning is in every tool **description**, not only in this document,
because the description is the part that reaches the model.

---

## 4. Lanes: more than one agent at once

`INCORD_LANES=<n>`, default **1**, capped at 8.

### Two measured facts decide the whole design

Read off this ledger, not assumed:

1. **Every task a plan produces writes code.** `Ledger::from_plan` folds the
   read-only roles — `explore`, `diagnose`, `review` — into a writing task's
   title as *preparation*, because a step that only produces information
   cannot be proved by a build.
2. **Nothing is ordered unless the planner says so.** `Task::with_dependencies`
   had no production caller at all until `plan::Step::after` existed; a plan
   that declares no ordering still produces tasks that are all ready at once.

So all the work is writes and none of it is sequenced. There is no subset that
is "safe to parallelise", and running two of these in one directory is not slow
or untidy — it is **wrong**: each agent edits files the other is reading, and
then `verify` runs one command over a tree neither of them intended and mints a
`Verified` from it. The check would pass. It would be proving something nobody
wrote.

### So isolation is the precondition, not an optimisation

A lane is a **git worktree**: its own directory, its own branch, off the same
base commit, taken once before any lane opens. Agents cannot see each other's
edits.

**Without git there is no isolation and therefore no parallelism.** This does
not error — it reports one lane, says why, and the run proceeds exactly as it
always has. A harness that refused to work in a directory that is not a
repository would be trading a feature nobody asked for against the thing people
actually do.

### The check runs AFTER the merge, not in the lane

A lane's own tree contains one agent's work. Proving that tree proves the agent
did not break its own lane — which is not the question. **Two lanes can each
pass alone and break the moment they are put together**, which is the ordinary
result of two people editing one codebase.

So a lane produces a **report**, which is a claim, and the runner checks the
merged tree.

### Conflicts are never resolved here

Two agents changed the same lines. Choosing between them is a judgement about
the work, and that is the one seat this harness never sits in — it is where
both previous autonomous loops failed (`VERIFICATION.md`).

So the merge is **aborted**, and:

- the main tree is left clean, with no conflict markers — otherwise the next
  lane's merge and the next task's check run against a tree containing them;
- the losing lane's **branch is kept, never deleted**. The work is on it;
- the task goes back to the queue to be done again, sequentially, where it can
  see the other change.

**And the retry is told where its work is** — added 2026-09-21. "Done again"
used to mean *from nothing*: the retry got a bare task, wrote the whole change
a second time, and the nearly-right version sat on a branch nobody named. The
branch is now recorded on the task (`Task::unmerged_branch`) and the retry's
instruction says to start from it — read `git diff HEAD...<branch>`, apply it
to the tree as it is now, keep the other change intact.

This is not the harness resolving the conflict. It is still an agent, in one
tree, making a judgement about the work, with both changes in front of it.
What changed is that it no longer has to rediscover its own answer first. Full
rules in [`RUNNER.md`](RUNNER.md) §6b.

### A lane that did not land is a failed attempt

The agent reported success and meant it: in its own tree, the work is finished.
But its change is on a branch, not in the tree `verify` is about to run over —
so believing the report would prove a tree that does not contain the change and
mark the task complete on the strength of it.

That is the fabrication the whole verify design exists to stop, arriving
through a new door. It was written into the code before any test covered it,
and the first control run against it caught nothing. The test now exists:
remove the rule and two tasks are marked `Done` when only one merged.

---

## 4a. When an agent runs out of subscription mid-task

A standby takes over — and until 2026-09-21 it was handed the **identical
instruction** the first agent got. `run_on_agent` clears `said` on the way in,
deliberately, because the two agents are separate conversations and the new one
cannot resume a session it never had.

That is right about the **session** and wrong about the **knowledge**. Two
things went missing:

1. Everything the first agent found. It had read the files and said where the
   bug was; the second re-derived all of it on a different subscription —
   which is the cost the fallback exists to avoid.
2. **That the first agent had already edited the tree.** This is the dangerous
   one. The standby started from a task description that no longer matched the
   workspace, and could redo or half-undo work already on disk.

So a `<handover>` block now carries everything the harness can observe:

| Carried | Source |
|---|---|
| Why the previous agent stopped | the failure that moved the work |
| What it said before it stopped | `said`, read **before** `run_on_agent` clears it |
| Which files it already changed | `git status --porcelain`, diffed against a snapshot taken **before** the attempt |

The snapshot matters: without it, a workspace that was already dirty would be
reported to the standby as the previous agent's doing.

Its own words are marked as **a lead to confirm, not fact** — it is the report
of an agent that then failed. The spoken text is bounded at 4,000 characters,
because a long transcript costs the new agent the context it needs for the work
itself.

**What cannot be carried is the hidden conversation.** That lives on the
vendor's side and no amount of harness code reaches it. Where git cannot answer
"what changed" — the workspace is not a repository — the block says **UNKNOWN**
rather than "it changed nothing", because telling a new agent something false
about a tree it is about to edit is worse than telling it nothing.

---

## 5. How lanes join the runner without a second copy of the law

The runner's law — attempts, escalation, the owner channel, and the single edge
into `Done` through `verify` — is written once, in the sequential loop.
Re-implementing it for the parallel path would mean two copies of the rule that
decides whether work is finished, and **the one that gets fixed would not
always be the one that ran.**

So the integration is deliberately shallow:

```
loop:
  refresh_ready()
  run_lanes_if_useful()    <- NEW: works a batch concurrently, caches reports
  pick the next task
  report = cached report, if a lane already did this one
           otherwise worker.work(task)     <- unchanged
  verify / record / escalate                <- unchanged
```

A cached report is **consumed, never reused**: a second attempt must actually
re-run the agent, or the attempt limit becomes a way of recording the same
failure twice.

`run_lanes_if_useful` does nothing — the ordinary case — when reports are still
unconsumed, the operator asked for one lane, the workspace is not a git
repository, fewer than two tasks are ready, or the pool declines.

### The worker factory

`LaneWorkers` supplies one worker per lane. It is a factory rather than the
runner's own worker because lanes run at the same time and the runner holds
exactly one `&mut dyn TaskWorker`, which cannot be in two places.

`AgentBackend` is **not `Send`** — it owns callbacks into the terminal — so the
backend is constructed **inside** the lane's thread, from settings that can
cross it. A process per lane is not waste: the worker already drops the agent's
conversation between tasks, so a reused one would start from nothing anyway.

`available()` is asked **before** any worktree is created, and is separate from
`spawn_for` because asking "could you?" must not build one.

---

## 6. Operator surface

| Setting | Default | What it does |
|---|---|---|
| `INCORD_LANES` | `1` | Agents at once. Needs git. Clamped to 8. A typo falls back to 1 rather than stopping the run |
| `INCORD_TEAM_AS` | `unrouted` | Set by the harness per task; read by the team MCP server |
| `incode --team-mcp --team-channel <p>` | — | The server itself. Launched BY an MCP client, not by hand |

Worktrees are created in `<repo-parent>/worktrees/` — `tools/worktree.rs`'s
existing choice, which means lane checkouts sit **beside** the repository
rather than inside it.

---

## 7. What this does not settle

- **Nothing dispatches an agent to answer a question.** Questions reach the
  next agent that could answer and persist until one does; no separate turn is
  spent answering. Doing that after the work is finished would be too late —
  the guess is already baked in.
- **Lane count is not adaptive.** Eight lanes on a two-task ledger is two
  lanes, but nothing measures whether the machine can carry the number asked
  for.
- **A conflicted task is retried sequentially, not merged intelligently.** The
  branch is kept, and a person can merge it by hand. The harness will not.
- **Codex's team registration is unverified end to end.** The override syntax
  is confirmed against the real CLI — single backslashes are rejected
  (`expected a sequence`), doubled ones load — but no tool call has been made
  through it, because that account hit its usage limit.
