# Insi — Self-Improving Agent Harness (PRD)

Oct 1, 2026 · @Ravikash Gupat · revised Oct 1, 2026 (gaps closed, status added)

## Overview

Insi is a standalone Rust engine that makes coding agents measurably better at working on any codebase over time, under human control. The name blends Incord and **Self-Improving**; it does not mean superintelligence.

The problem: models are frozen at training time, so agents repeat the same mistakes, and nothing they learn on one task survives to the next. Fine-tuning frontier models is out of reach for a small team.

The approach: improve everything *around* the model instead of its weights. Insi turns verified fixes into topic-scoped lessons loaded before each task, enforces guards in code, and runs an improvement loop that proposes harness changes as pull requests backed by eval evidence. Agents can investigate across code, GitHub, memory and email. Workers write only inside their own sandbox worktree; **the only thing that ever leaves the machine is a pull request**, and a human always merges.

**Pitch:** Self-improving coding agents, governed by humans.

**Constraint:** the whole system is written in Rust. It builds on three things that already exist, named precisely because two of them have been called "Watchman":

| Name in this PRD | What it is |
| --- | --- |
| **Incord Memory** | The memory engine, `incord-memory-service`, on `127.0.0.1:9180`: recall, rules, beliefs, the owner channel, and its **recorder** (the HTTP routes under `/v1/memory/watchman/…`) and **auditor**. Lessons, topics and audit events live here. |
| **incode** | The coding harness (`D:\Projects\incord-code`): drives Claude Code, Codex and agy as models, runs every tool through its own gate, proves work by exit code, manages worktrees and opens PRs (`/pr`). |
| **Watchman app** | The desktop UI (`D:\Projects\watchman`). It already runs incode as a provider over ACP; Insi does not depend on it. |

## Goals and non-goals

**Goals**

1. Cut repeat mistakes: the same failure signature should rarely happen twice in the same topic.
2. Make every lesson verifiable: stored with evidence (test pass, human approval), scope, and history.
3. Improve the harness itself through a measured loop, with changes landing only as reviewed PRs.
4. Work on any repo through per-repo config, with no code changes inside the repo.
5. Prove results with an eval benchmark before making any public claims.

**Non-goals**

- Training or fine-tuning frontier models. LoRA on small local models (via Cortex) is a later, optional phase.
- Autonomous merging or deploying. Agents never write to main.
- Claiming AGI or ASI. Marketing says "Self-Improving" in full wherever "SI" appears.
- Replacing Claude Code, Codex or agy. They remain the worker agents; Insi sits around them.
- Re-implementing what incode already enforces (guards, the permission gate, proof by exit code). Insi drives workers *through* incode.

## Background: the AIDE² paper

> **Not yet verified.** The figures below are from the paper as summarised when this PRD was written; check them against the paper itself before any public claim cites them.

Weco AI's AIDE² (arXiv 2609.26457, Sept 2026) showed that an agent can improve its own harness without touching model weights, and that the gains generalize. This is the closest published evidence for the Insi approach.

**What they did**

- Two loops: an inner research agent solves tasks; an outer agent rewrites the inner agent's code (search policy, context handling, prompts).
- The model stayed fixed: Claude Opus 4.7 drove the outer loop, Gemini 3 Flash ran the inner agents.
- A rewrite was kept only if it beat the current best on private held-out data the inner agent never saw, under a fixed dollar budget.

**Results**

- An 8-day run tried 99 rewrites and accepted 7; the selection grade rose from 0.703 to 0.778.
- The final agent matched or beat a production agent built over two years of human R&D on four external benchmarks, including an out-of-distribution weather-forecasting task.
- Reward hacking fell from 55% to 32% without being optimized for. One rewrite repaired a broken scoring script instead of exploiting it.

**Limits they report**

- The "ignition test" (is the improved agent a better self-improver?) was inconclusive with three seeds.
- Noise compounds across both loops; one false acceptance can derail the search.
- Evolved agents are complex and hard to interpret, and runs are expensive.
- No governance: accepted rewrites go live with no audit or human gate.

**What Insi takes from it**

- Separate the score the agent sees (public) from the score that accepts changes (private).
- Fixed per-task budget, so gains come from method, not spend.
- Multi-seed evaluation and accept-only-if-better selection.
- Failure memory (their version: inject up to three recent error signatures once the bug rate passes 15%).
- A strategy bandit with periodic forking to escape plateaus.
- Bounded, compact context instead of full history.

**What Insi adds**

- Human-gated PRs and an audit trail in Incord Memory for every self-modification.
- Protected paths the loop can never edit (evals, guards, CI).
- Structured, topic-scoped lessons with evidence, beyond raw error lines.
- Connectors across code, GitHub, memory and email with trust levels.

## Users and use cases

The first user is a solo founder or small team running several repos with coding agents; later, teams buying the Incord stack.

| Use case | Trigger | Output |
| --- | --- | --- |
| Fix a labelled GitHub issue | Issue gets the trusted label | PR with fix, tests, and lessons used |
| Avoid a known mistake | Task starts in a topic with prior lessons | Briefing loaded before the agent acts |
| Learn from a fix | A failure is fixed and verified | New scoped lesson with evidence |
| Improve the harness | Scheduled improvement run | PR to `harness-config/` with before/after eval numbers |
| Trace a reported bug | Support email in the read-only inbox | Linked issue and investigation notes; no outbound reply without approval |
| Prove it works | Benchmark run, with and without memory | Report on repeat-failure rate, retries, cost |

## System architecture

Insi is a standalone Rust service with its own binary, `insi`, not code inside any one repo, so it can serve every repo through config.

**The four interfaces.** Everything enters Insi through exactly these:

| Interface | Who uses it | What it may do |
| --- | --- | --- |
| **CLI** (`insi eval`, `insi run`, `insi propose`, `insi audit`) | The operator, and the scheduler | Start eval runs, improvement runs and issue work; read results |
| **Hooks** (incode's pre-tool gate and Incord Memory's decider) | Every worker, on every tool call | **Enforcement:** allow, ask or block; just-in-time lesson injection |
| **MCP server** (read side) | Any agent, inside incode or not | Briefings, lesson lookup by error signature, topic load. Advisory only: MCP is called at the agent's discretion, so nothing that must hold is enforced here |
| **GitHub webhooks** (a GitHub App) | GitHub | Report labelled issues and CI results; never a write path into Insi's state |

**How a worker runs.** Insi drives a worker through a **worker adapter**: a command line, configured per repo, that takes the task's prompt and works in the sandbox worktree it is started in. The first adapter is **incode** (`incode -t … --output-format stream-json`): it drives Claude Code, Codex or agy and gives every worker incode's guards, permission gate, checkpoints and proof by exit code without Insi re-implementing them. Any other agent that runs headless in a folder is a second adapter, not a code change — so incode is the recommended worker, never a requirement.

**Usable from any project.** Insi is one binary that works in a repository of any language, added in one of three ways:

| Layer | What it is | Who uses it |
| --- | --- | --- |
| **The `insi` binary** | CLI and MCP server in one program, installed once per machine | Every project, any language |
| **`insi init`** | Run inside a repo: writes `.insi/config.toml`, an `evals/` folder, a CI workflow that runs the checks on agent PRs, and the MCP/hook registration for the agents in use | How a project adopts Insi |
| **Packages per ecosystem** | `insi` as a Rust library crate, and thin npm and pip packages that fetch the binary — the pattern Ruff, Biome and esbuild use | Teams that want it pinned in their own lockfile |

Nothing is published until the Phase 1 gate is met: private first.

**Outputs.** The engine produces two things only: audit records in Incord Memory, and pull requests a human reviews.

## Functional requirements

### F1. Topic memory

- Every memory item (fact, edit, belief, lesson, policy) has a topic ID. Topics are hierarchical (`rust` → `incord` → `incord/memory-service`); children inherit parent items.
- A task's topic is resolved by rules first: repo, working directory, touched paths, command. A small model is a fallback only when rules cannot decide; it is an `api_models` entry in config, never named in code.
- Retrieval is deterministic. No model is needed to load a topic.

### F2. Lessons from failures

- A lesson is written only after its fix is verified, never at the moment of failure. The trigger is concrete: a task that failed its check and then passed it, which incode's ledger already records.
- Fields: topic, `use`, `avoid`, normalized error signature, evidence (task ID), `verified_by`, count, last seen.
- Trust order for `verified_by`: human > objective check (tests, build) > agent claim. Agent-claim lessons cannot become hard rules until confirmed or repeated.
- Error signatures are normalized: no line numbers, timestamps, temp paths or hashes.
- A lesson seen 2–3 times is promoted to a belief. Contradictions supersede the old item and keep its history.
- Lessons are saved on the narrowest topic and promoted to a parent only when they recur in several children.

### F3. Briefing and injection

- Pre-task: load the topic's items in fixed order: policies, beliefs, recent lessons, facts; capped by a token budget.
- Lessons are injected as positive actions ("use X", or "use X, not Y"). Raw logs and failed commands stay in the audit store.
- Just-in-time: the pre-tool hook re-injects a matching lesson right before a related tool call.
- In-task: on error, normalize the signature, look it up, inject the known fix.
- Re-fetch when the agent's scope moves to a different topic.

### F4. Guards

- Hard rules are enforced by incode's gate and Incord Memory's decider, not by prompt text.
- Every tool call passes the decider: allow, ask, or block.

### F5. Eval benchmark

- 20–30 real tasks per starting repo, each with starting state, prompt, public check, private check, and a fixed budget (tokens, dollars, wall time).
- **Private checks live outside the agent's checkout**, in a separate repository the worker cannot reach, and are run by Insi after the worker stops. A protected path is not enough: incode's guards stop writes to a path, not reads, so a check inside the checkout can be read and gamed.
- Runs use several seeds per task; results store success, retries, tokens, cost, time, and repeat-failure count.

### F6. Improvement loop

- Editable surface is `harness-config/` only: prompts, routing between Claude Code, Codex and agy, retry limits, injection rules, briefing format. **This folder does not exist yet**: today those settings are spread across incode's code, `admin.json` (`roles`, `tiers`, `api_models`) and `agents/*.md`. Gathering them into one versioned folder that incode reads is its own piece of work, done before the loop.
- The proposer reads eval results and lessons, then proposes ONE change per candidate.
- Each candidate runs in its own git worktree against the benchmark, under the same budget as the incumbent. **The check runs inside that worktree**: a candidate's work is not merged anywhere before it is judged.
- Accept only if it beats the incumbent on private checks by a set margin across seeds.
- Strategy selection uses a UCB1 bandit over named strategies, with a fork from the best every N steps.
- Winners become PRs; losers are logged with their scores.

### F7. Issue and PR workflow

- Agents act only on issues with a trusted label applied by a maintainer, or from allowlisted authors.
- Work happens on a branch in a sandbox. Nothing but a PR leaves the machine.
- A PR includes: diff, rationale, lessons used, test results, and for harness changes, before/after eval numbers.
- Before a PR is opened, its diff, commit messages and body pass a secret and PII scan.
- CI re-runs tests and evals independently. A human merges.
- **Identity, in two stages.** Operator-run PRs use the operator's own `gh` login (incode `/pr`, built). Unattended runs (issue work, the loop) require the GitHub App with short-lived, repo-scoped, PR-only tokens; the loop does not run unattended before that exists.

### F8. Connectors

- One trait for every source; each item carries its source and trust level (Trusted, Internal, Untrusted).
- Untrusted content (email, outside issues, web) is data, never instructions.
- Connectors return only items linked to the current topic.
- Build order: memory, code, GitHub; then CI logs and error tracking; then docs; then email (read-only first).

### F9. Per-repo config

Insi keeps its own folder, `.insi/`, so it works in a repo that has never seen incode; `insi init` writes it. Where incode is present, its settings are read rather than repeated: protected paths from `.incode/guards.json`, the check command from incode's `verify_command`. `.incord/` belongs to Incord Memory and is not used.

`.insi/config.toml`:

```toml
topic = "incord/memory-service"
test_cmd = "cargo test --workspace"          # the public check
protected = [".github/workflows/**", "evals/**"]
eval_tasks = "evals/tasks"                   # public task definitions
private_checks = "<path or git URL of the private-checks repo>"
trusted_issue_label = "agent-ok"

[worker]                                     # the worker adapter
command = ["incode", "-t", "{prompt}", "--output-format", "stream-json"]
```

## Security and governance

The rule: agents may read broadly, but nothing leaves the machine except a PR, and every guard that matters is enforced in Rust code, not in a prompt.

| Risk | Requirement |
| --- | --- |
| Agent pushes or merges | GitHub App with short-lived tokens, scoped repos, PR-create only; branch protection on main. Until the App exists, PRs are operator-run only |
| Loop edits its own judge | Protected paths: `evals/**`, guards, policies, permission decider, `.github/workflows/**`, enforced by incode's gate and CODEOWNERS; private checks are not in the checkout at all |
| CI runs PR code with secrets | Agent PRs run CI without secrets, or only after maintainer approval |
| Prompt injection via issues or email | Untrusted trust level; act only on trusted-label issues; risky actions (network, secrets, dependency changes) blocked during issue work |
| Sensitive data leaks into a PR | Redaction on connector input; secret and PII scan on diff, commit message and PR body before the PR is opened |
| Supply chain | Any dependency change is flagged for extra review |
| Sensitive data in memory | Incord Memory's store-time guards, already in place |
| Untraceable self-modification | Incord Memory logs proposal, evidence, CI result, approval and merge for every change |
| Code runs on the host | Candidates and issue work run in sandboxes with no production credentials: incode's OS sandbox first, containers for issue work |

Outbound actions other than PRs (sending email, external comments) are proposals that wait for approval.

## Rust implementation plan

One Cargo workspace, `insi`, with its own binary, next to `incord-memory-service` (reached over HTTP) and incode (run as a process per worker). Lessons and topics live in Incord Memory; this workspace owns the loop, evals, sandbox and GitHub side.

**Dependencies start with the tools already in use.** incode drives git and GitHub through the `git` and `gh` command-line programs, which keeps the dependency tree small; Insi does the same first, and adopts a library (`octocrab`, `gix`, `bollard`) only where a spike shows the CLI cannot do the job.

### Workspace layout

| Crate | Responsibility | Starting point |
| --- | --- | --- |
Built first (Phase 1), at `D:\Projects\insi`:

| Crate | Responsibility | Starting point |
| --- | --- | --- |
| `insi-core` | Shared types (Task, Budget, EvalResult, Lesson, Trust), `.insi/config.toml`, the error-signature normalizer | `serde`, `toml` |
| `insi-eval` | Mining tasks from history, the task runner in a throwaway worktree, budgets, public/private checks, multi-seed results | `git` CLI, std processes |
| `insi-memory` | Client for Incord Memory: lesson write, briefing load, audit events | HTTP |
| `insi` (binary) | `insi mine`, `insi eval`, `insi report`, later `insi init`, `insi run`, `insi propose` | std argument parsing first |

Later phases add `insi-loop` (proposer, bandit, accept/reject), `insi-guard` (path policy, secret/PII scan, dependency-change detector — a dependency of the PR step, so no PR is opened without it), `insi-pr` (`gh` first; GitHub App before unattended runs), `insi-connectors` and `insi-server` (webhooks, MCP server, scheduler).

Dependency names are starting candidates; pick per crate after a spike, vetted like any other dependency.

### Key types and traits

```rust
pub enum Trust { Trusted, Internal, Untrusted }

pub struct Lesson {
    pub topic: TopicId,
    pub use_: String,
    pub avoid: Option<String>,
    pub error_sig: String,          // normalized
    pub evidence: TaskId,
    pub verified_by: Verification,  // Human | Check | AgentClaim
    pub count: u32,
    pub last_seen: DateTime<Utc>,
}

pub trait Connector: Send + Sync {
    fn id(&self) -> &str;
    fn trust(&self) -> Trust;
    async fn read(&self, topic: &TopicId, q: Query) -> Result<Vec<Item>>;
    async fn propose(&self, a: Action) -> Result<Proposal>; // never acts directly
}

pub struct EvalTask {
    pub id: TaskId,
    pub repo_state: GitRef,
    pub prompt: String,
    pub public_check: Check,
    pub private_check: Check,       // outside the checkout, never shown to agents
    pub budget: Budget,             // tokens, usd, wall time
}

pub trait Proposer {
    async fn propose(&self, incumbent: &HarnessConfig, history: &[Scored]) -> Result<ConfigPatch>;
}

pub fn accept(cand: &[EvalResult], inc: &[EvalResult], margin: f64) -> bool;
```

### Engineering rules

- Async on `tokio`; no blocking calls inside the loop.
- Seeded randomness only, so runs are reproducible.
- Every state change emits an audit event to Incord Memory.
- The guard crate is a dependency of the PR crate: a PR cannot be opened without passing `si-guard`.

## Success metrics

Every metric is measured on the benchmark with and without Insi, same budget, several seeds. Targets are set after the first baseline run.

| Metric | Definition | Direction |
| --- | --- | --- |
| Repeat-failure rate | Share of failures whose signature already has a lesson in the topic | Down |
| Task success | Private check passes within budget | Up |
| Retries per task | Failed attempts before success | Down |
| Cost per solved task | Tokens and dollars per private-check pass | Down |
| PR acceptance rate | Agent PRs merged without changes | Up |
| Held-out generalization | Gain on tasks and repos never used for selection | Up |
| Reward-hacking rate | Public check passes but private check fails | Down |
| Backfire rate | Tasks where an injected lesson caused a wrong action | Down |

## Milestones and build order

The benchmark comes first because every later claim depends on it; each phase ends at a measurable gate. No dates are set yet.

| Phase | Builds | Gate (measured, not claimed) |
| --- | --- | --- |
| **1. Benchmark and lessons** | `insi eval` over 20–30 tasks with private checks; the lesson writer (F2), signature normalizer and briefing (F3) | Repeat-failure rate and task success, with lessons on vs off, 3 seeds, same budget: lessons on is measurably better |
| **2. Issue → PR** | Trusted-label intake, sandboxed issue work, PR with evidence, secret/PII scan, the GitHub App | 10 agent PRs opened with no secret, scope or guard violation, and their acceptance rate recorded |
| **3. Improvement loop** | `harness-config/` migration, proposer, candidate worktrees with in-worktree checks, accept-if-better, bandit | At least one accepted change that beats the incumbent on private checks across seeds, and still beats it on held-out tasks |
| **4. Wider sources** | CI logs, error tracking, docs, read-only email; containers for issue work | Each connector returns only topic-linked items with their trust level; untrusted items are never acted on as instructions |

Each phase is useful on its own: phase 1 alone shows whether topic memory reduces mistakes.

## Status: what exists today, and what is left

As of Oct 2, 2026. "incode" paths are in `D:\Projects\incord-code`.

### Already built (Insi reuses these)

| PRD part | What exists | Where |
| --- | --- | --- |
| Proof by exit code (F5/F6 checks) | A task is `Done` only through a command that exited zero; a model's report cannot produce that state | incode `src/verify.rs` (`Verified`) |
| Failed-then-fixed signal (F2 trigger) | Every attempt, pass or fail, recorded on the ledger task with its failure note; at most `MAX_ATTEMPTS` | incode `src/ledger.rs`, `src/runner.rs` |
| Compact failure logs | Repeated lines and passing-test noise collapsed before a failure reaches a model | incode `src/verify.rs` (`compact_log`) |
| Guards (F4) | Protected paths (tests, CI, migrations, `.env`, the ledger, `.incode/guards.json`) refused as write targets | incode `src/guards.rs` |
| Permission decider (F4, hooks) | Incord Memory's `pre-tool` hook asked on every tool call: allow, block or ask a person; fails closed for writes | incode toolbox; `docs/incord-memory-integration.md` |
| Worktrees (F6 sandbox) | Create, list, remove; parallel `/run` lanes in worktrees, merged back, conflicts kept on a branch | incode `src/tools/worktree.rs`, `src/parallel.rs` |
| PR opening and merging (F7, operator stage) | `/pr [branch] [--draft]`: refuses the default branch, uncommitted work, an empty branch and bad names before anything is pushed; pushes; reuses an open PR or runs `gh pr create`. `/pr merge`: merges only a ready PR at the local tip with no failing or running check, pinned to that head | incode `src/tools/pull_request.rs` |
| Routing between agents (F6 surface) | Roles with ordered model candidates, tiers, CLI agents and API models, all in `admin.json` | incode `src/roles.rs`, `src/agent/catalogue.rs` |
| Agent definitions (F6 surface) | Sub-agents as Markdown files with role, model and tool allow-list; a built-in `researcher` (memory, then web, saves findings) | incode `src/subagents.rs`, `defaults/agents/` |
| Token budget | A token ceiling that stops the next request and counts every request in full | incode `src/budget.rs` |
| Memory client | Recorder lookup and handoff, `remember`, recall | incode `src/recorder.rs`, `src/knowledge.rs` |
| Running under the Watchman app | incode as the lead of a Watchman orchestration reaches its orchestrator (the control credentials pass incode's environment filter) | incode `src/tools/env_filter.rs` |

### Built in the Insi repo (Phase 1, Oct 2, 2026)

`D:\Projects\insi`, a Cargo workspace, local commits only. Crates `insi-core`, `insi-eval`, `insi-memory` and the `insi` binary; 36 tests, clippy and fmt clean.

| Piece | What exists |
| --- | --- |
| `insi mine` | Every fix commit that changed source and a test file of its own becomes a task: parent = start, message = prompt, test files = hidden check, commit = reference, commit date kept. Reads the source repository only |
| Private checks | Hidden test files stored apart (`--private`), outside every checkout; copied into the worktree only after the worker stops, over anything it wrote at those paths |
| `insi validate` | Controls per task: the check must fail at the base and pass at the reference; only such tasks are "fair" (`--fair-only`) |
| `insi eval` | Each attempt in its own worktree of Insi's own clone; setup, worker (wall-clock budget), hidden check; records pass, time, tokens (from incode's `stream-json`), failure signature, tags, artifacts (worker diff, outputs). `--seeds`, `--jobs N`, `--half older\|newer` |
| Signature normalizer | Terminal colour stripped; paths, hashes, numbers normalised; only lines that say what failed |
| Lesson writer (`insi learn`) | A failed attempt on a fair task + its verified fix → one lesson (issue, root cause, use, avoid), refused when a field is empty, oversized or holds code; `verified_by: check` |
| Briefing | Lessons of the task's repository learned strictly before the task (compared as instants), never from the task itself, newest first, within 4,000 characters; attempts tagged `lessons=on\|off` and record which lessons they got |
| `insi report` | Pass rate, repeat failures (same signature again), timeouts, errors, tokens and time per condition |
| Watchman benchmark | 38 tasks mined, all validated: 35 fair |
| Interface brief (`insi brief`) | The prompt names the module paths and exported names the hidden tests import that the base lacks — never a test or expected value — and says nobody will answer questions. Insi `dd99838` |

**Pilot findings (Oct 2).** The first pilot (incode worker, routed to `gemini-3.1-pro-high`, 1800 s) passed 0 of 4. Every failure was a name the prompt never gave: a module the fix created (`editorDoc`, `ompInterjections`) or a function it exported (`isHiddenEditorHost`, `decodePtyChunk`); one worker also asked a question and waited out its budget. 20 of the 38 tasks have such a gap. A benchmark whose baseline is zero cannot show lessons helping, so the interface brief above was added before measuring. Three attempts at once also ran the machine out of memory: attempts run one at a time.

Decision: benchmark lessons are kept in a JSONL file under the work folder, not in Incord Memory. `lesson_write` files an item under the project of the folder it names, which would put benchmark lessons in Watchman's memory, and its retrieval is semantic; an experiment needs a set it can reset and reproduce. Writing verified lessons to Incord Memory is the production path, behind the same store trait.

### Left to build

| # | Piece | Phase | Status |
| --- | --- | --- | --- |
| 1 | Benchmark format and `insi eval`: tasks, public and private checks, seeds, results | 1 | Built, except a separate public check |
| 2 | Private-check repository, outside every checkout | 1 | Built as a folder; a repository of its own is not needed yet |
| 3 | First 20–30 tasks, from bug-fix commits in the Watchman app's history that changed a separate test file | 1 | Built: 38 mined, all validated — 35 fair (fail at base, pass at reference); 3 pass at base and are left out |
| 4 | Dollar and wall-time budgets (incode has tokens only) | 1 | Built: wall time; dollars — `[prices]` per model in config, `cost_usd` per attempt, `insi eval --max-dollars` (Insi `7587c56`) |
| 5 | Error-signature normalizer | 1 | Built |
| 6 | Lesson writer: failed-then-verified task → lesson with evidence, in Incord Memory | 1 | Built, file store; Incord Memory store left |
| 7 | Topic resolution rules (repo, path, command) and topic fields on lessons — confirm what Incord Memory already stores | 1 | Built (`42ecbb7`): hierarchical topics (`shop` covers `shop/api`); `[[topic_rules]]` by path globs and command words, most specific wins, no model. Lessons filed on the topic of the files their fix changed; briefing = topic + parents; `insi hook` brings narrower lessons on the first edit in their part, and matches failures on the command's topic. Lessons reach Incord Memory tagged `topic:<t>` (insi-memory `incord.rs`). Left: promotion to a parent when a lesson recurs in several children (F2), and the hook inside `insi eval` workers, so narrower lessons count in a measurement |
| 8 | Briefing builder and just-in-time injection through the pre-tool hook | 1 | Built: pre-task briefing, and `insi hook` (Insi `bfb64c3`) — lessons naming a file before it is edited, lessons matching a failure after a command fails; verified live in Claude Code 2.1.288 |
| 9 | `insi` workspace and binary; CLI | 1 | Built |
| — | **The Phase 1 measurement**: lessons off vs on, newer half, 3 seeds | 1 | Next, once validation ends |
| 10 | MCP server (read side) | 2 | Built: `insi mcp` (`insi_briefing`, `insi_lessons_for_error`), Insi `5b68733` |
| 11 | Check inside a worktree before any merge or PR (also needed by `/run` lanes-as-PRs) | 2 | Done in incode: lane check before merge (`INCODE_LANE_CHECK`, 6f1ecdf) and lanes opened as draft PRs (`INCODE_LANES_AS=pr`), settled on the next run (`docs/LANES_AS_PRS.md`). Not yet run against GitHub live |
| 12 | PR body with evidence; secret/PII scan before opening | 2 | incode `/pr` credential scan built (`289090a`); `insi issue` PR body with evidence and the same scan built (Insi `5b68733`). PII scan built in both: card numbers (Luhn), US SSNs, real-domain emails (Insi `61ac225`, incode `/pr`) |
| 13 | Trusted-label issue intake | 2 | Built: `insi issue --number N` — open + labelled only, draft PR after `test_cmd` passes (Insi `5b68733`) |
| 14 | GitHub App with short-lived, PR-only tokens | 2 | Not started; `gh`'s login is used meanwhile |
| 15 | `harness-config/` folder, read by incode | 3 | Built: Insi runs a worker with `INSI_HARNESS=<folder>` (Insi `ec27d8d`; `insi improve` passes each candidate's own snapshot). incode `src/harness.rs` reads `instructions.md` (system context, after CLAUDE.md/AGENTS.md), `roles.json` (roles/tiers over admin.json per key, at read time only so `/roles` never saves it; `INCODE_ROLE_*` still wins; `--admin` shows `<- harness`), `agents/*.md` (between project and operator). A harness file that does not read refuses the run. Verified live on the debug binary. Left: retry caps (`MAX_ATTEMPTS`, `MAX_REPLANS`) are consts used by `/run`'s ledger, not by `-t`; Insi's own `prompt.md` is the task-prompt surface |
| 16 | Proposer, accept-if-better, UCB1 bandit, audit events | 3 | Built (Insi `c645085`): crate `insi-loop` (harness snapshots by content id, one-file proposals with path/size checks, paired accept rule with margin + held-out not-worse, UCB1, JSONL audit) and `insi improve --harness <dir>` (select on the older fair half, hold out the newer; scores cached per harness; proposer never shown check output). Tested on the synthetic repo. Surface today: the harness's `prompt.md`, put before every task. Left: incode reading the same folder (row 15), audit events to Incord Memory, winners as PRs once the GitHub App exists (row 14). "Fork from the best every N steps" is inherent: under strict accept-if-better the incumbent is always the best |
| 17 | Connectors: CI logs, error tracking, docs, email | 4 | Built (Insi `32e2a82`): crate `insi-connectors`, one `Source` trait, every item with source + `Trust` (trusted/internal/untrusted, in insi-core) + topic; only the asked topic's line of descent returned, bounded; untrusted text fenced as data and unable to close its fence; a failing source named, not fatal. Sources: `docs` (README + docs/, trusted), `ci` (failed Actions runs via `gh`, internal, topic from files the log names), `[[connectors]]` commands printing JSON lines (error tracking, mail exporters; untrusted by default; Insi holds no credentials). Served by `insi context` and MCP `insi_context`; kept out of eval briefings. Verified live on docs; CI live returned none (incord-code has no Actions runs). Left: a first real error-tracking/mail exporter configured by the owner |
| 18 | Containers for issue work | 4 | Built (Insi `75b1f87`): `[sandbox] command = [...]` with `{tree}`; `insi issue` runs setup, the worker AND the check inside it (the check runs worker-written code); commit, scan and push stay on the host; the PR says where it ran. Tested with a stand-in wrapper. Left: a live Docker run — the Docker CLI is installed but Docker Desktop's daemon is not running on this machine |
| 19 | `insi init` for any repo: `.insi/config.toml`, `evals/`, CI workflow, MCP/hook registration | 2 | `insi init` built (`ded3d68`): detects Node/Rust/Python/Go, writes the config, ignores the work folder. CI workflows for Insi and incord-code committed. `insi mcp` registered in `.mcp.json` (`5ae02c3`) and `insi hook` in `.claude/settings.json` (`bfb64c3`); an existing file is never rewritten. CI workflow for agent PRs built (`a73f599`): `.github/workflows/insi.yml` runs setup and the repo's own `test_cmd` on PRs from `insi/` branches, read-only token not persisted. No `evals/` folder, by decision: a task's definition holds its reference commit (the answer), so tasks stay in `private_checks` outside the checkout. Done |
| 20 | Packages: `insi` on crates.io, npm and pip wrappers — after the Phase 1 gate | 2 | Ready, not published (Insi `c0308a3`): crates publish-ready (`cargo publish --workspace --dry-run` passes all six); `release.yml` drafts a GitHub release with 5 platform archives + `checksums.json` on a `v*` tag matching every manifest; npm and pip wrappers with no dependencies download the archive and check it against the digest pinned in the package (tested offline, including a wrong digest and a path-escaping archive). `RELEASING.md` has the steps. The name `insi` was free on crates.io, npm and PyPI on Oct 2. Left: the owner's accounts and the decision to publish, after the Phase 1 gate; a LICENSE file naming the copyright holder |

## Decisions (were open questions)

Proposed answers; each stands until revised.

- **First benchmark repo:** the Watchman app (`D:\Projects\watchman`), read only — Insi works in its own clone and never touches that checkout. Measured on Oct 2: 722 commits, 427 changing source together with a separate test file, 36 of them fixes. A fix commit's parent is the starting state and its test file the private check. incord-code was the first idea and was measured too small for it: 79 commits, 4 fix-like, and tests inline in the source, so a fix's test cannot be held back without the fix.
- **Published or private:** private until the Phase 1 gate is met.
- **Acceptance margin and seeds:** start at 3 seeds; set the margin from the variance the first baseline measures.
- **Sandbox runtime:** worktrees plus incode's OS sandbox first; Docker for issue work in phase 4.
- **Inside incode or separate:** a separate `insi` binary that drives workers through incode, so it serves any agent and any repo.
- **Topic-fallback model:** a Haiku-class `api_models` entry, set in config.
- **Private checks in a separate repo:** yes. A protected path stops writes, not reads.

## References

1. Srikanth, Zhao, Xu, Wu, Jiang (Weco AI). [Recursive self-improvement of AI research agents](https://arxiv.org/html/2609.26457v1). arXiv 2609.26457, 22 Sept 2026. Primary reference: two-loop design, public/private grading, fixed budgets, failure memory, strategy bandit, reward-hacking results, ignition test.
2. [ICLR 2026 Workshop on Recursive Self-Improvement, accepted papers](https://recursive-workshop.github.io/papers.html), including work on reward hacking in self-improving code agents and meta-learned agentic memory.
3. [Continual Learning in Transition](https://arxiv.org/pdf/2608.06216), survey with references on memory-driven agent self-evolution and experience graphs.
4. [Continual learning roundup, Aug 2026](https://scipapermill.com/2026/08/15/continual-learning-navigating-the-shifting-sands-of-ai-knowledge/): Self-Distillation Fine-Tuning and Mixture-of-LoRA continual learning, relevant to the later Cortex/LoRA phase.
