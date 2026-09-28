# incode — Conventions

**Why this file exists.** Work here is routed across models. Each has a different
default style, and without an explicit convention a codebase built across four of them
reads like four contractors worked unsupervised — which is exactly the tell that
destroys trust in the cost saving. With one, the routing is invisible in the output.

**Every rule has a good and a bad example**, because models follow examples far better
than descriptions. If you are adding a rule and cannot write the bad example, the rule
is not yet specific enough to follow.

---

## 1. Comments say WHY, never WHAT

The code already says what it does. A comment that restates it is a second thing to
keep in sync, and it will not be kept in sync.

```rust
// BAD — restates the line below, and will be wrong after one edit
// Increment the attempt counter
task.attempts += 1;

// GOOD — says the thing the code cannot
// Not reset on an owner redirect: a task the owner has sent back twice is still
// a task that has failed twice, and clearing the count would let a pair of
// redirections run forever.
task.attempts = task.attempts.saturating_add(1);
```

A comment that says what a *constant* is worth is the same mistake. Say why that value.

```rust
// BAD
/// Maximum attempts: 2.
pub const MAX_ATTEMPTS: u32 = 2;

// GOOD
/// Two, then stop. A third attempt on an unchanged hypothesis is how a loop
/// burns a night.
pub const MAX_ATTEMPTS: u32 = 2;
```

---

## 2. A comment is a claim, and claims get checked

Never state what other code does on the strength of its comment. Quote the executable
line, or say the claim is unverified.

```rust
// BAD — inherited from another file's comment, and it was wrong
// The permission layer already blocks writes to tests.
write_file(path)?;

// GOOD
// Permissions gate by TOOL NAME (`permissions/mod.rs:213`), never by path — so
// this check is the only thing standing between an agent and its own exam.
if let Some(refusal) = guards.refuse_write(path) { ... }
```

---

## 3. Errors say what to do next

An error a person reads at 3am should end their investigation, not start it.

```rust
// BAD
return Err("verification failed".to_string());

// GOOD
return Err(format!(
    "no check is configured for this project, so nothing can be proven. \
     Set {VERIFY_COMMAND_ENV} to the command that decides whether the work \
     is correct."
));
```

Refusals additionally offer the legitimate route, or they simply train people to work
around them:

```rust
// GOOD
"`{path}` is protected and cannot be written. Fix the code so the existing check
 passes. If the check itself is genuinely wrong, say so and let a person change it."
```

---

## 4. Ambiguity is never resolved in the permissive direction

When an answer cannot be read, the safe reading is the one that stops.

```rust
// BAD — an unknown verb falls through to "carry on", so a refusal reads as a go-ahead
match verb {
    "dismiss" => Verdict::Dismissed,
    _ => Verdict::Approved,
}

// GOOD
match verb {
    "dismiss" => Ok(Verdict::Dismissed),
    other => Err(format!(
        "the verdict `{other}` is not one this harness knows how to act on"
    )),
}
```

The same rule covers absent data: a missing field is missing, not zero, not empty, not
allowed.

---

## 5. Truncation is always stated

A list that shows ten of forty and says nothing reads as the whole picture, and gets
planned from as if it were.

```rust
// BAD
for item in items.iter().take(MAX_SHOWN) { out.push_str(&render(item)); }

// GOOD
for item in items.iter().take(MAX_SHOWN) { out.push_str(&render(item)); }
if items.len() > MAX_SHOWN {
    out.push_str(&format!("- ... and {} more not shown\n", items.len() - MAX_SHOWN));
}
```

The same rule bounds *actions*, never *measurements*: a dry run reports the true total
even when the live run is capped.

---

## 6. Parse what the service actually sent

Contracts with anything outside this crate are pinned against a body the running
service really returned, kept as a literal constant beside the test.

```rust
// BAD — an imagined shape; passes forever while finding nothing
const BODY: &str = r#"{"severity": 3}"#;

// GOOD — copied from the live response, including the surprise
/// `severity` arrives as a STRING here even though it is a number.
const REAL_BODY: &str = r#"{"findings":[{"severity":"4","status":"open"}]}"#;
```

Unknown fields are ignored, never rejected: the other side ships on its own schedule.

---

## 7. Tests are named as the law they defend

The name is what a person reads when it fails. `test_complete` tells them nothing.

```rust
// BAD
#[test] fn test_verify() { ... }

// GOOD
#[test] fn a_confident_worker_cannot_complete_work_the_check_rejects() { ... }
#[test] fn an_unanswered_question_stops_the_run_and_approves_nothing() { ... }
```

Assertions carry the reason, so a failure explains itself:

```rust
// BAD
assert_eq!(task.state, TaskState::Abandoned);

// GOOD
assert_eq!(
    task.state, TaskState::Abandoned,
    "approved-without-a-check is SET ASIDE, never Done"
);
```

---

## 8. Every load-bearing test gets a control run

Reintroduce the defect, confirm the test fails **on the intended assertion**, restore
the fix. A test that passes with and without the fix is decoration.

This is a process rule, not a code rule, and it belongs here because it is the one most
often skipped under time pressure — which is exactly when it matters.

---

## 9. Naming

- Functions are verbs about the domain: `refuse_write`, `past_verdict`, `set_aside`.
  Not `check_path`, `get_verdict`, `handle_task`.
- Types are the thing, not the pattern: `Verdict`, `Breach`, `Guards`. Not
  `VerdictManager`, `ContractHelper`.
- Booleans read as the question: `can_be_verified`, `states_non_goals`, `is_open`.
- A field's name says its role, not its type: `owner_note`, not `note_string`.

```rust
// BAD
pub fn process(&self, data: &str) -> Option<String>

// GOOD
pub fn refuse_write(&self, path: &str) -> Option<String>
```

---

## 10. Module layout

One module answers one question and is named for it. Order inside a file:

1. `//!` header — what this is for, and the reasoning that would otherwise be lost
2. public constants, each with the reason for its value
3. types
4. the public functions, most important first
5. private helpers
6. `#[cfg(test)] mod tests`

Keep files under ~1,000 lines; a new area gets its own module. Files above that are
marked in the code tree so nobody has to rediscover it.

---

## 11. Rust specifics

- `#[must_use]` on anything returning a value a caller could drop by mistake.
- `?` over `unwrap()`; `expect("why")` in tests only, with the reason.
- Errors are `Result<T, String>` at service boundaries — the message is the product.
- No `#[allow(...)]`. A silenced lint is a hidden bug; fix the code.
- Format with `cargo fmt`. Avoid `\`-continued string literals in code that rustfmt
  will touch: it joins them and leaves literal whitespace in the output.

```rust
// BAD — rustfmt turns this into a message with a run of spaces in it
return Err(format!("the task {id} failed \
                    twice and was set aside"));

// GOOD
return Err(format!("the task {id} failed twice and was set aside"));
```

---

## 12. What NOT to add

- No `TODO` as a fix. State the gap in the report instead.
- No dead code, no commented-out blocks, no "kept as a fallback".
- No new dependency without a version, a licence, and a stated alternative — and
  `research.rs` will catch one the PRD never argued for.
- No vendor or model names in code, comments, identifiers or logs. Providers are
  operator configuration; the code says `role` and `provider`.

---

## 13. A capability nothing calls is not a capability — added 2026-09-21

`Task::with_dependencies` was written, documented and tested, and **no
production code ever called it** — so `depends_on` was empty on every real
task and `parallel::batch` was free to run two tasks that had to be
sequential. `Task::with_tier` had the same shape: `role()` answered `None`
for every real task, so nothing was ever routed to the model it was planned
for. Both bugs look identical from the test suite, which is green, and from
the code, which is present and correct.

`wiring.rs` is the check. It counts how many times each `pub fn` is mentioned
in the **production** half of the crate — comments stripped, test modules
excluded — and flags a declaration mentioned only once, which is the
declaration itself.

It is a **ratchet**, not a gate: `MAX_UNWIRED` is the current count, the test
fails if the number goes up, and the constant is lowered as things are wired.
Starting from zero would have meant either a sixty-item cleanup before any
feature could land, or an `#[allow]`, which §11 forbids.

`DELIBERATELY_UNWIRED` is the escape hatch, and it demands a reason longer
than thirty characters. A short reason is a rubber stamp.

The rule on writing new code: if you add a `pub fn` and nothing calls it in
the same change, either wire it or do not add it. `findings::read` was
written in this phase and deleted before it shipped for exactly that reason —
the comment where it used to be says why the design does not want it.

### The rule on an entry that is already there: wire it, do not delete it

An item on this list is a capability somebody built and nobody reached. The
first question is *where should this have been called*, not *can this go*.
Four of the six originals turned out to have a correct home:

| Capability | Where it belonged |
|---|---|
| `PermissionPolicy` | `desktop::mcp::call_tool` — `-p read-only` now means the agent may look at the screen and not touch it |
| `hooks` | the same choke point, so an operator's own command can refuse a click |
| `Task::with_contracts` | `from_plan`, so the frozen-interface block reaches the agent that could break it |
| `Task::with_dependencies` | `from_plan`, resolving the planner's `[after N]` |

Deleting any of them would have removed a working answer to a real question
and left the gap it was written for still open.

### A false positive is worse than a miss

`production_half` cut each file at the first `#[cfg(test)]` *anywhere* in it.
One test-only method 177 lines into `cli/agent_turn.rs` hid the remaining
1,028 lines, so four capabilities reached by every `/undo` were reported dead.

Four wrong entries make the whole list something a reader has to check by
hand before believing any of it, which is how a check stops being used. The
marker is now anchored to column zero — a test *module* is unindented, an
attribute on an item inside an `impl` is not.
