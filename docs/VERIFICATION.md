# Incord Code — Verification

How each acceptance criterion is proven. **The runner reads this**: every roadmap item
below turns into the check that decides whether its task is `Done`, and `Done` is
reachable only through a process that exited zero (`src/verify.rs`, `src/ledger.rs`).

A claim in this file is either **MEASURED** — it was run, on this machine, on the date
shown — or **UNVERIFIED**, and says so. Nothing here is predicted.

---

## 0. The one command

```
cargo test
```

That is the project's check. It is not configured anywhere; it is *detected* from
`Cargo.toml` by `verify::detected_command` (`src/verify.rs:241`). The resolution order
is fixed and detection is deliberately last, so anything a human writes down wins:

| Precedence | Source | Where |
|---|---|---|
| 1 | `INCORD_VERIFY_CMD` environment variable | `src/verify.rs:208` |
| 2 | `verify_command` in `~/.incord-code/admin.json` | `src/admin.rs` |
| 3 | detected from the manifest → `cargo test` | `src/verify.rs:241` |

**The model never chooses.** `verify`'s tool schema takes no arguments
(`src/verify.rs:252`), so a check that always passes cannot be supplied through the
argument list.

### The ecosystems detection knows — extended 2026-09-21

Thirteen: Rust, Node.js, Python, Go, **PHP**, Java (Maven), Gradle, Ruby,
.NET, Elixir, Swift, Dart, CMake. A table (`verify::ECOSYSTEMS`), not a chain
of ifs — because the chain could not answer the only question that has to be
settled before a command is chosen: **how many are present.**

> The chain returned the FIRST match. A PHP application with a JavaScript
> front end matched `package.json`, so the back end was "proved" by running
> the front end's tests — a check that passes while the changed code is never
> executed, and `Verified` is minted from its exit code.

So **two ecosystems means no detected command.** The operator names the check
once, and both layers above already beat detection. This is stricter than what
it replaced: a Rust workspace that also has a `package.json` for its docs site
used to get `cargo test` and now gets nothing until somebody says. That is the
correct direction to fail — the cost is one setting, and the alternative is a
proof nobody can trust. `verify::detection_note` says which ecosystems it saw,
because "pick one" and "nothing here" are different problems.

Several manifests from the **same** ecosystem are one ecosystem: a Python
project carrying `pyproject.toml`, `setup.py` and `tox.ini` is not ambiguous.

A few ecosystems name a **checked-in runner** that supersedes the canonical
command when the file exists — `vendor/bin/phpunit` over `composer test`,
`./gradlew` over `gradle`, `./mvnw` over `mvn`, `.rspec` over `rake test`. Not
a heuristic: a file is there or it is not, same evidence as the manifest. It
exists because for those ecosystems the obvious command is the one that
usually **fails**. The Windows `.bat` sibling counts, or the wrapper would be
missed on the platform this harness is developed on.

### The same list gates dependencies

`research::declared_dependencies` reads twelve manifest formats plus .NET
project files, and **reads every one that matches, not just the first** — a
project can genuinely be two things, and stopping at the first would hand the
other a clean bill nobody checked. Platform entries are dropped (`php`,
`ext-*`, `python`), as are dev-dependencies and Go's `// indirect` lines:
nobody chose those, and holding a PRD to them floods the report.

### The one command is not the only check — added 2026-09-21

A task that produces **information** has no build that speaks to it. Judging
`explore: locate the ball entity` by `node --check game.js` is how a live run
marked four such steps `Done` for reasons unrelated to any of them.

Those tasks are checked by their **deliverable** instead:

```
incode --check-findings .incord/findings/r-1.2.md
```

The same law, not an exception to it. `Verified` is built from a process that
exited zero, and this is a process that exits zero. What changed is which
command is asked, and a research task never inherits the project's.

The judge is the harness's own binary because the only thing that knows what
a findings file must contain is the harness. `verify::run` resolves the bare
name `incode` at run time — `INCORD_SELF_BIN`, else this process when its
file stem really is `incode`, else `PATH` — so the argv stored in the ledger
stays portable across machines and rebuilds.

**What it proves:** the file exists and holds at least 200 non-whitespace
characters. **What it does not prove:** that any of it is true. Nothing reads
it for sense — a model grading a model is what this whole document refuses —
so the real check on bad research is the task that depends on it failing its
own. See [`RUNNER.md`](RUNNER.md) §6d.

### Verification under lanes

When `INCORD_LANES > 1` the check runs on the **merged** tree, never inside a
lane. A lane's own tree contains one agent's work, and proving it proves only
that the agent did not break its own lane. Two lanes can each pass alone and
break the moment they are put together. See [`TEAMWORK.md`](TEAMWORK.md) §4.

### Environment it needs

| Requirement | Verified value |
|---|---|
| Rust toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)`, `cargo 1.97.1` |
| Clippy | `clippy 0.1.97` |
| Features | none — `default = []`. No optional Incord crate is declared, so the crate builds standalone. |
| Network | none required by the suite (see the caveat below) |
| Wall clock | ~66s for the test binary, plus a cold `cargo build` |

**Caveat, stated rather than glossed:** the suite was run with the memory service
listening on `127.0.0.1:9180`. Every body-facing test found in `src/` points at either
`http://127.0.0.1:1` (a deliberately dead port — `auditor.rs:355`, `guards.rs:1460`,
`knowledge.rs:656`, `owner.rs:549`, `recall.rs:434`, `recorder.rs:356`, `rules.rs:282`,
`spec.rs:792`, `task_run.rs:178`) or a test server bound on a random port
(`brief.rs:1186`), which is what a hermetic suite looks like. **It was NOT proven with
the service stopped.** The command that would settle it, and the only one that counts:
stop the service, run `cargo test`, and confirm exit 0 with the same 533 passes.

### What a pass looks like — MEASURED 2026-08-23

```
running 533 tests
test result: ok. 533 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 65.73s

running 18 tests
test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Exit code `0`. 533 unit tests, 18 doc-tests, 0 integration tests (there is no `tests/`
directory — every test is a `#[cfg(test)] mod tests` beside the code it covers).

**A failure is any of:** a non-zero exit; a killed or timed-out process, which
`CommandEvidence` records as `exit: None` and which is a failure and never a pass
(`src/verify.rs`); or a contract breach, below.

---

## 1. The contract check rides the same command

`verify` does not stop at a green suite. `verify::run_tool` and `verify::verify` both
call `contracts::check` **before** minting proof, so a task whose own tests pass while a
frozen interface has moved cannot reach `Done`.

- **Input:** `.incord/contracts.json` — 26 schemas, 14 endpoints.
- **What is checked:** every declared schema name appears in some source file; every
  field of a schema appears in a file that names that schema; every endpoint's route
  (method stripped) appears in some source file. Matching ignores case and punctuation,
  so `user_id` / `userId` / `UserId` are one name.
- **What is deliberately NOT checked:** types, nullability, semantics. Those belong to
  the compiler and the suite, which already run.
- **A malformed contracts file is an ERROR, never an empty pass** (`contracts.rs:221`).
  This is the single most important line in the checker: a file it cannot read would
  otherwise verify nothing while reporting success.

### What a pass looks like

```
Every declared contract is present in the code.
```

A breach reads `MISSING schema \`X\`` / `MISSING field \`X.y\`` / `MISSING endpoint \`P\``
and is returned as an *error*, so the model cannot read it as a pass.

### Status — MEASURED 2026-08-23, with a control

There is **no non-interactive entry point** that runs `contracts::check` alone: it runs
inside `verify`, which needs a session or a runner task. So the file was validated by a
line-for-line replica of `mentions()`, `files_naming()`, `read_sources()` and `check()`,
run with a control:

| Run | Result |
|---|---|
| Real contracts, 54 source files scanned | **0 breaches** |
| Control — one absent schema, one absent field on a real schema, one absent route | **3 breaches, all three fired** |

The control is what makes the clean run mean anything. Without it, a probe that could
not read the files at all would report the same "no breaches".

The control also earned its keep on the first draft: three schema names had been written
down (`OwnerCard`, `DecisionRow`, `HandoffItem`) for shapes that cross the boundary as
anonymous `serde_json::Value` payloads with no named type in the code. The check caught
all three. They are now recorded under their endpoints as `_request_fields` /
`_response_row_fields` / `_response_item_fields` — documentation, explicitly **not**
enforced.

**Open:** the authoritative `contracts::check` has not itself been run against this
workspace. It will run on the first `verify` of the first task, and that is the run that
counts.

---

## 2. Style gates — MEASURED 2026-08-23

Not part of `cargo test`, so a runner that only runs the check will not catch them. Run
them before a task is reported finished.

| Command | Exit | Output |
|---|---|---|
| `cargo fmt --check` | `0` | none |
| `cargo clippy --all-targets` | `0` | none — no warnings |

No `#[allow]` was added to force this green. The tree contains exactly two, both
predating this document: `src/agent/mod.rs:82` (`clippy::struct_field_names`, on
`TurnOutcome.served_by`) and `src/runner.rs:381` (`clippy::too_many_arguments`). A third
appearing alongside a "fix" is a check being silenced, not a bug being fixed.

---

## 3. What "tested" means here

A green suite proves the code **builds** and that nothing else broke. It proves nothing
about the thing that changed. For a task to count as verified, its test must do all
four:

1. **Execute the changed line** — name it: "the test at `file:line` calls it". Testing
   the layer underneath is not testing the layer.
2. **Assert on behaviour, not shape** — the row that survived, the task that did *not*
   reach `Done`, the guard that refused. Not that a function returned `Ok`.
3. **Exercise the real mechanism** — if the bug is "an unverified task is journalled as
   finished", the test must actually run a *failing* check, not assert the code back to
   itself.
4. **Fail when the defect is put back** — reintroduce it, confirm the test fails *on the
   intended assertion*, restore the fix.

And verify the control itself: a control that does not actually break the code disables
nothing, so the test "passing" proves only that the control was broken.

If a test misses one of the four, **say which one** rather than letting the word
"tested" carry weight it has not earned.

This is the house rule, and the codebase already follows it — see
`runner::tests::unverified_work_is_never_journalled` (`src/runner.rs:1943`), which
exercises the mechanism rather than restating it.

---

## 4. Per-roadmap-item acceptance

Ten items, from the brief. Each names the check that decides it. `cargo test` (exit 0)
plus a clean contract check is the floor for every one of them; the row adds what is
specific.

| # | Item | How it is proven | Status |
|---|---|---|---|
| **r1** | Harness core — loop, tools, permissions, checkpoints, delegation | `cargo test` exit 0. Per-area coverage: `guards` 40, `runner` 29+8, `agent::claude_code` 27, `agent::worker` 25, `ledger` 17, `contracts` 16, `verify` 14, `permissions` 9. | **done — MEASURED** |
| **r2** | Fill the project brief (about, goals, synopsis, PRD) | `GET http://127.0.0.1:9180/v1/owner/proxy`; the `C:\Projects\incord-code` brief has all five artifacts non-empty. `ProjectBrief::missing()` returns `[]`. | **done — MEASURED 2026-08-23**: about 672 chars, goals 1321, synopsis 756, prd 18259, roadmap 10 items |
| **r3** | Attach to Incord Memory over HTTP; delete the local store | No local store may exist: `grep -rn "struct .*Memory\|memory.json" src/` returns nothing that stores. Every body-facing module calls and none writes — the fourteen endpoints in `contracts.json` are the whole surface. | **UNVERIFIED — the grep above has not been run as an acceptance check** |
| **r4** | Load guard rules and durable decisions from memory, not from restatement | `rules.rs` and `guards.rs` hold no rule text of their own beyond `DEFAULT_PROTECTED`; `GET /v1/memory/rules/for` and `/v1/memory/enforcement` supply the rest. Pass = `rules` 8 tests + `guards` 40 tests green **and** no new literal rule added to `src/`. | **UNVERIFIED** |
| **r5** | Decision attribution — a `by` field on the owner verdict | `Verdict` carries `by` on all five verbs (`src/owner.rs:71`), and a proxy decision is never rendered as the owner's. Pass = `owner` 14 tests + `runner::attribution_tests` 8 tests green. | **MEASURED green; the attribution rendering itself is UNVERIFIED end-to-end** |
| **r6** | Task ledger and runner | `ledger` 17 + `runner` 29 tests green, and specifically `unverified_work_is_never_journalled`. Pass additionally requires: `Ledger::complete` remains the only edge into `Done` and still demands a `Verified`. | **MEASURED green** |
| **r7** | Per-role model routing so subagents run on a cheap model | `roles` 13 tests green is the floor and is **not sufficient**: routing pays only if the cheap model actually serves the turn. Real proof = one delegated turn whose `TurnOutcome.served_by` names the cheap model, captured from a live run. `SessionStarted.model` is the model *asked for* and must not be read as evidence. | **UNVERIFIED — no live `served_by` capture exists** |
| **r8** | Budget caps that actually stop a run | `budget` 7 tests green, and specifically that `spent()` keeps counting past the ceiling (bound the action, never the measurement). Real proof = a run that halts with `StopReason::BudgetSpent`. **Blocked on a number: no token figure exists in any file.** | **UNVERIFIED, and unimplementable until the owner names a figure** |
| **r9** | Close the measurement gaps — live cache-hit measurement, web_search credentials | Requires live numbers, not tests. Pass = a recorded turn showing `cache_read_input_tokens` > 0 on a resumed session, against ~36k cache-creation on a fresh one. | **UNVERIFIED** |
| **r10** | Fix the misleading `Cargo.toml` rationale and the four empty features | `cargo test` exit 0 with `default = []`, plus each of `agent-tools`, `codegraph`, `tokenizer` removed and dead `cfg` references cleaned up. Pass = `cargo test --all` exits 0 with `default = []`. | **VERIFIED — empty features removed, Cargo.toml rationale updated honestly, all dead cfgs cleaned up** |

---

## 5. Checks this project does not have

Named rather than left to be discovered:

- **No CI.** There is no `.github/`, no pipeline, no remote. Every check above is run by
  hand or by the runner on this machine. The project is not a git repository.
- **No integration test target.** `tests/` does not exist. The third `running 0 tests`
  line in the output above is a doc-test harness, not an integration suite.
- **No standalone contract-check command.** See §1. This is the one gap that would pay
  for itself soonest — the check is the project's own law and cannot currently be run
  without a session.
- **No coverage measurement.** Test counts are counts, not coverage.
