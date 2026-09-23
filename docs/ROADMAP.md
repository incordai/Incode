# Roadmap

Ordered by what it costs to be wrong about, not by what is quickest.

Process for every item: `PROCESS.md`. Local `main` is the test bench, the gate
runs before anything is pushed, and every load-bearing change gets a control.

**Status as of 2026-09-22:** 1288 lib + 38 bin tests, clippy 0, fmt clean,
43 unwired capabilities (67 at the start of the day).

---

## A. The 51 unwired capabilities

`wiring.rs` reports them; `MAX_UNWIRED` is the ratchet and may only fall.

The rule is **wire it, do not delete it** (`PROCESS.md` §5). Four of the six
originals had a correct home and deleting them would have left the gap they were
written for still open.

Each item gets the same three questions, in order:

1. **Where should this have been called?** If there is an answer, wire it and
   control-test the wiring.
2. **Is there a live replacement?** Then this is a superseded duplicate — say
   which one replaced it, and remove it in a change of its own.
3. **Neither?** `DELIBERATELY_UNWIRED` with a reason over 30 characters. A short
   reason is a rubber stamp.

None of these are pre-judged below. The grouping is by *where they live*, and
the disposition is decided by reading the code, not by this table.

### A1 — Things that report, and are never reported — DONE 2026-09-22

Each was an answer to a question an operator actually asks, computed and
thrown away. Eight of the nine are wired; one was allowlisted honestly.

| Item | Where it went | What it was hiding |
|---|---|---|
| `verify::detection_note` | `--admin`'s verify line | "none" read the same for *no ecosystem here* and *two, so running one's tests proves nothing about the other* — opposite problems, opposite fixes |
| `budget::exhausted_message` | `StopReason::BudgetSpent`, via `exhausted_text` | the stop said "24000 used" — not what it was measured against, not which variable raises it |
| `parallel::why_not` | `parallel::shortfall_note`, printed by the runner | `INCORD_LANES=4` on a non-repository gave a sequential run indistinguishable from having nothing to parallelise |
| `ledger::attempts_left` | `task_instruction` | the final attempt worked exactly like the first, reaching for the better change rather than the one it was sure of |
| `ask::unattended_note` | `task_run`, after the turn | **`--task` never parsed the agent's `<ask>` blocks.** An agent that hit a fork, asked, and guessed produced a run reporting plain success with the question sitting in the prose |
| `brief::may_code` | `/run`'s brief gate | read as `matches!(Gate::Ready(_))` — true today, not the rule; a variant added later would fall through silently |
| `agent::registry::display_line` | `doctor`'s "none drivable" arm | the arm someone reads to find out why nothing runs listed names without versions |
| `ledger::has_open_work` | the runner's drained-queue branch | **the largest find — see below** |
| `budget::exhausted_message` (the method) | allowlisted | it is the method spelling of `exhausted_text`, which *is* wired; inventing a call site would be worse than saying so |

#### The bug `has_open_work` was hiding

`next_ready` finds `Ready`. `waiting_ids` finds `Blocked` and `Pending`.
**`Running` was in neither, and nothing reclaims it** — there is no lease. A
run killed mid-task left that state on disk, and the next run walked past it
into the goals check.

Measured with a control, with goals that pass: the run reported
`LedgerEmpty` — *every task is finished* — with a task still marked in
progress. That is the same fabrication `NothingRunnable` was added to stop,
arriving through the one state neither query covered.

Now `StopReason::Stranded`, its own reason rather than folded into
`NothingRunnable` — that message says the tasks are "waiting on somebody",
and these are waiting on nobody. Sending a person to find who is holding it
would send them after someone who does not exist.

**Still open from this group:** `ledger::is_done` and the three `tasks_by_*`
queries (A2) have no natural caller yet and have NOT been settled either way.

### A2 — Per-worker ledger queries (3)

`tasks_by_worker`, `tasks_by_instance`, `tasks_by_fingerprint`. Three spellings
of one question. Decide which one the spend report and `--serve` want, then wire
that and settle the other two.

### A3 — Fleet and admin (5)

`fleet::is_available`, `models_for`, `is_mixed`; `admin::skill_enabled`,
`mcp_enabled`. The settings say a plugin is off; check whether anything asks.

### A4 — Desktop (5)

`has_mixed_scaling`, `virtual_bounds`, `capture_monitor`, `coords::to_model`,
`mcp::client_config_json`. Multi-monitor is the live question here: DPI work
exists and a second display may not be reachable.

### A5 — Tooling and runtime (7)

`tool_status::is_installed`, `diagnostics::for_mode`, `env_filter::allowed_count`,
`bash_jobs::get_output`, `usage::current_turn_usage`, `usage::cumulative_usage`,
`lifecycle::run_on_compact`.

### A6 — Superseded by a live replacement (2)

`slash_commands::handle_model` and `handle_tool_status`. Both have working
replacements — `/model` is handled inline in `cli/mod.rs:1159` and `/tools` calls
`handle_delegated_tools`. **Verified, not assumed.** These are removals rather
than wirings, and belong in their own commit.

### A7 — Vestiges of the removed tool loop (5)

`render::print_tool_call`, `print_tool_result`, `print_diff`,
`print_streaming_delta`, `permissions::with_defaults`. The loop that called them
is gone (`tools/mod.rs` records it). `with_defaults` names `read_file`,
`write_file`, `bash` — tools this crate no longer dispatches, which is why
`for_desktop` was written beside it instead.

### A8 — The rest (15)

`cli/console::line_count`, `cli/footer::input_row`, `top_rule`,
`cli/render::is_drawing`, `guards::target_command`, `links::context_for`,
`knowledge::work_query_subject`, `roles::with_role`,
`worktree::with_branch_prefix`, `task_run::run_task`, `serve::published`,
`wiring::unwired_in`, `wiring::line_report`, and two more the report names.

---

## B. Carried over, still open

### B1 — Desktop on macOS (Phase C) and Linux/Xvfb (Phase D)

`COMPUTER_USE.md` has the design. Neither can be *verified* from this machine,
and `PROCESS.md` §4 says a claim about an external system needs that system.
Writing it blind produces code that compiles and has never run.

**Needs:** a machine of that kind, or an explicit decision to ship it untested
and say so in the docs.

### B2 — The codex MCP tool call

Registration syntax is proven — the TOML escaping was verified by an accidental
before/after control. The tool *call* has never run: blocked by a provider usage
limit. One live call is all that is missing.

### B3 — The viewer's stop button — DONE 2026-09-23

The owner decided: reads, plus two writes behind a per-run token
(`.incord/serve.token`, `Authorization: Bearer`). `POST /v1/run/stop` writes
`.incord/run.stop`; a running `/run` watches for it and trips the same cancel
Ctrl-C does, which ends the agent and its whole process tree. `POST
/v1/team/answer` records the person's answer to an agent's question. Every
request also passes the Host and Origin checks.

**Not covered by a test:** the two lines in `cli/mod.rs` that arm the watcher
when `/run` starts — exercising them needs a live `/run`. The watcher, the
route and the file between them are each tested with a control.

### B4 — The stale code tree in `PRD.md` §6

Counts are from 2026-08-21 and several files have roughly doubled. Marked stale
with the missing modules listed by what they answer, rather than filled with
numbers nobody measured.

---

## C. Standing, never finished

- **`MAX_UNWIRED` may only fall.** The ratchet is the point: it does not clear
  the debt, it stops a seventh dead capability arriving unnoticed.
- **A false positive in a check is worse than a miss.** One was found on
  2026-09-22 — `production_half` cut a file at the first `#[cfg(test)]`
  anywhere in it, so four working capabilities were reported dead. Four wrong
  entries make the whole list something a reader has to verify by hand.
- **Every doc claim is checked against the code before it is written.** Two
  entries on this list were called gaps in an earlier report and were not
  (`rules_for`, `findings_for` — both are the unbounded spelling of a request
  the real callers make with a deadline).
