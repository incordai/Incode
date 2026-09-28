# incode — Assumptions

Every choice made during this intake **rather than asked**, with its reasoning. The
owner skims one list instead of answering twenty questions, and correcting an assumption
here is free, because nothing has been built on it yet.

Written 2026-08-23 and re-measured 2026-09-01, against the code as it stands: 69
source files, 42,536 lines, `cargo test` green at 880 + 18.

Each entry says what was decided, why, and **what it would cost to reverse**. A cheap
reversal is a decision the owner can ignore; an expensive one is a decision worth
reading.

---

## A. Where the brief's `about` and `goals` came from

**Decided:** both were written from documents the owner had already approved — `## 0.
ABOUT` and `## 0b. GOALS` of the brief's own PRD, plus the "It must protect:" clause of
the existing synopsis — and not from a question.

**Why:** the intake's own rule is to ask only what reading cannot answer. Reading
answered this. Two of a seven-question budget spent re-asking something already written
down would be two questions not spent on the four things the PRD explicitly leaves
**unresolved** (§10: audience, budget figure, sequencing, README).

**Cost to reverse:** one `POST /v1/owner/proxy`. Free.

**Where it went:** the memory service, `C:\Projects\incord-code` brief — `about` 672
chars, `goals` 1321 chars. Not a file in this repo. `src/brief.rs:15` is explicit that
nothing here stores a brief, and a local copy would be the second-memory defect
returning.

---

## B. The brief was written over HTTP, not through `save_brief`

**Decided:** the config document was fetched from `GET /v1/owner/proxy`, patched at one
path, and posted back — the exact sequence `src/brief.rs:744` (`save`) performs.

**Why:** `save_brief` is a tool **this crate offers to the agent it drives**
(`src/brief.rs:569`). It is not available to an agent working *on* the crate. The
endpoint is the same one either way.

**The risk this carried, and how it was handled:** that endpoint **replaces the whole
document** — the owner's rules, proxy mode, model and severity ceiling included
(`src/brief.rs:683`). Serialising a partial structure would have deleted all of them.
So: the live document was backed up first to
`.backups/proxy-config.1787518599.bak.json` (116,165 bytes), treated as opaque JSON,
patched at two keys, and **read back after writing** rather than trusting the `200`.
Confirmed after the write: 11 briefs preserved (of 11), all seven top-level keys
preserved.

**Cost to reverse:** restore the backup. Free.

---

## C. Contracts record the code as it IS, not as it should be

**Decided:** `.incode/contracts.json` freezes 26 schemas and 14 endpoints exactly as the
source declares them today — including names and field spellings that a redesign might
change.

**Why:** the file's job is to stop the *next* change breaking a caller nobody looked at.
A contract describing an improved design would fail against the code on day one, and a
checker that fails for reasons unrelated to the change is a checker people learn to
ignore.

**Cost to reverse:** free while nothing is built against it; it becomes expensive the
moment a task is dispatched with "honour these shapes" in its description.

---

## D. Three boundary shapes are documented but NOT enforced

**Decided:** the `POST /v1/owner/enqueue` request body, a `GET /v1/owner/history` row,
and a `GET /v1/memory/watchman/handoff` item are recorded under their endpoints as
`_request_fields` / `_response_row_fields` / `_response_item_fields`, which the parser
keeps and ignores.

**Why — and this was measured, not reasoned:** the first draft gave them names
(`OwnerCard`, `DecisionRow`, `HandoffItem`). The check reported all three MISSING,
correctly: they cross the boundary as anonymous `serde_json::Value` payloads built and
read field-by-field (`src/owner.rs:148`, `src/owner.rs:210`, `src/recorder.rs:112`), and
no such type exists in the code. Inventing a name to satisfy a document is exactly the
drift the file exists to prevent.

**What this costs:** these three are the least protected shapes at the boundary. If the
service renames `proxy_note` or `observations`, nothing fails until a person notices.

**Cost to reverse:** give them real structs — a small, contained code change, after
which they become enforceable. Worth doing; deliberately not done during intake, because
intake does not write source.

---

## E. The project's check is `cargo test`, detected rather than configured

**Decided:** `cargo test` is recorded in `docs/VERIFICATION.md` as the command that
settles whether work is done.

**Why:** verified, not assumed — `INCODE_VERIFY_CMD` is unset in this environment, and
`~/.incode/admin.json` does not exist, so `verify::command_for` falls through both
operator layers to `detected_command`, which returns `["cargo", "test"]` for a crate
with a `Cargo.toml` (`src/verify.rs:241`). Detection is deliberately last in that ladder,
so anything the owner writes down will beat it without any change to this document.

**Cost to reverse:** set `INCODE_VERIFY_CMD`. Free.

---

## F. `cargo fmt` and `cargo clippy` are gates, but NOT part of the check

**Decided:** both are recorded in `VERIFICATION.md` §2 as measured-green
(`cargo fmt --check` exit 0; `cargo clippy --all-targets` exit 0, no warnings) and as
things to run before reporting a task finished — but they were **not** folded into the
verify command.

**Why:** folding them in means setting `INCODE_VERIFY_CMD` to a compound command, which
changes what "done" means for every task in the ledger at once. That is the owner's call,
not a side effect of writing a document. Note the consequence honestly: a runner that
only runs the check will not catch a clippy regression.

**Cost to reverse:** one environment variable, or one line in `admin.json`. Free — and
this is the assumption most likely to be worth reversing.

---

## G. The contract check was validated by a replica, with a control

**Decided:** `.incode/contracts.json` was proven clean using a line-for-line
reimplementation of `mentions()`, `files_naming()`, `read_sources()` and `check()`, run
alongside a control (one absent schema, one absent field on a real schema, one absent
route) that reported 3 breaches out of 3.

**Why:** `contracts::check` has no standalone entry point — it runs inside `verify`,
which needs a session or a runner task. A clean result from a probe that could not read
the files would look identical to a clean result from a probe that worked, which is why
the control is not optional.

**What is still open, stated plainly:** the *authoritative* checker has not been run
against this workspace. It will run on the first `verify` of the first task. The replica
lives outside the repo (session scratch), so it cannot rot into a second implementation.

**Cost to reverse:** nothing to reverse. The gap closes on its own at the first task.

---

## H. The suite is treated as hermetic — and this is NOT proven

**Decided:** `VERIFICATION.md` records that the suite needs no network.

**Why:** every body-facing test in `src/` points at `http://127.0.0.1:1`, a deliberately
dead port (`auditor.rs:355`, `guards.rs:1460`, `knowledge.rs:656`, `owner.rs:549`,
`recall.rs:434`, `recorder.rs:356`, `rules.rs:282`, `spec.rs:792`, `task_run.rs:178`), or
at a test server bound on a random port (`brief.rs:1186`).

**The honest caveat:** the 533 passes were measured **with the memory service running**
on `127.0.0.1:9180`. The reading above is inference from the source, not a measurement.
The one command that settles it: stop the service, run `cargo test`, confirm exit 0.
Not run, because stopping a service the owner's other agents depend on is not an intake
decision.

---

## I. Nothing in `docs/` was rewritten

**Decided:** `ARCHITECTURE.md`, `CONVENTIONS.md`, `RUNNER.md` and `PRD.md` were read and
left untouched. Only the three missing documents were added.

**Why:** all four are current and detailed — `ARCHITECTURE.md` names the module map, the
laws per module, the data flow and the on-disk state, and matches what the code does.
Rewriting a correct document to match a house style is scope nobody asked for.

---

## J. Two discrepancies found and REPORTED, not fixed

Both are outside intake's remit. Neither was changed.

1. **Two different PRDs exist.** `docs/PRD.md` on disk is 58,052 bytes; the `prd` field
   in the brief the loop actually reads is 18,259 bytes. They are different documents
   with different section structures — the on-disk one has `## 7. What is missing` and
   `## 8. Roadmap`; the brief's has `## 7. The gaps` and `## 8. Scope / Non-goals`. **The
   loop reads the brief's copy, not the file.** Which is authoritative is question 1
   below.

2. **A stale doc reference.** `src/agent/mod.rs:3` and `src/task_run.rs:3` both refer to
   `crate::provider::ApiClient`. There is no `provider` module in `src/lib.rs` and no
   `mod provider` anywhere in the tree. The doc comments describe an altitude
   distinction that is still conceptually right, but they cite a path that no longer
   resolves. A one-line fix in each, for whoever next touches those files.

---

## K. No dependency was added, and no source was written

**Decided:** `Cargo.toml` is untouched.

**Why:** intake does not write code, and nothing in completing a brief needs a new crate.
Recorded because "did you add anything?" should have an answer that is not "probably
not". The only files created are the three the loop was missing:
`.incode/contracts.json`, `docs/VERIFICATION.md`, `docs/ASSUMPTIONS.md`. The only
mutation outside this repo is the two brief fields in §A, backed up per §B.

---

## L. The roadmap was left exactly as it is

**Decided:** all 10 items (`r1`–`r10`) kept, with their current statuses.

**Why:** the roadmap follows the PRD, and every item traces to a section of it — the
condition `ProjectBrief::untraceable_items` enforces. Re-deriving it would be rewriting a
plan the owner already approved. `docs/VERIFICATION.md` §4 adds an acceptance check per
item without changing any of them, and marks honestly which are **UNVERIFIED** (r3, r4,
r7, r8, r9, r10) rather than inheriting `todo` as if it meant "known how to prove".

**One item is blocked, not merely undone:** `r8` (budget caps) cannot be implemented at
all — no token figure exists in any file. That is question 3 below.
