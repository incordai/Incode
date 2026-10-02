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

**How a worker runs.** Insi does not start Claude Code, Codex or agy itself. It starts **incode** (`incode -t … --output-format stream-json`, or `--acp`) in a sandbox worktree. incode drives the chosen agent, so every worker gets incode's guards, permission gate, checkpoints and proof by exit code without Insi re-implementing them.

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

A repo already carries incode's `.incode/` folder. Insi adds one file there, and **reuses** what incode already has rather than repeating it:

- protected paths: incode's `.incode/guards.json` (plus the built-in protected set);
- the check command: incode's `verify_command`;
- `.incord/` is not used: it belongs to Incord Memory.

`.incode/si.toml`:

```toml
topic = "incord/memory-service"
eval_tasks = "evals/tasks"           # public task definitions
private_checks = "git@github.com:org/insi-private-checks.git"
trusted_issue_label = "agent-ok"
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
| `si-core` | Shared types: Topic, Lesson, Task, EvalResult, Candidate, Trust | `serde`, `thiserror` |
| `si-config` | Parse `.incode/si.toml` and `harness-config/` | `toml`, `serde` |
| `si-memory` | Client for Incord Memory: topic load, lesson write, audit events | HTTP (`reqwest`) |
| `si-connectors` | `Connector` trait + GitHub, code, CI logs, email | `gh` CLI first |
| `si-sandbox` | Worktree create/cleanup, sandboxed run, resource limits | `git` CLI; containers later |
| `si-eval` | Task runner, budgets, public/private checks, multi-seed stats | `tokio` |
| `si-loop` | Proposer, bandit, compare, accept/reject | `tokio`, seeded `rand` |
| `si-guard` | Path policy, secret/PII scan, dependency-change detector | `globset`, `regex` |
| `si-pr` | Branch, commit, PR body with evidence | `gh` CLI first; GitHub App before unattended runs |
| `si-server` | GitHub App webhooks, MCP server, scheduler | `axum`, an MCP server crate |
| `si-cli` | `insi eval`, `insi run`, `insi audit`, `insi propose` | `clap` |

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

As of Oct 1, 2026. "incode" paths are in `D:\Projects\incord-code`.

### Already built (Insi reuses these)

| PRD part | What exists | Where |
| --- | --- | --- |
| Proof by exit code (F5/F6 checks) | A task is `Done` only through a command that exited zero; a model's report cannot produce that state | incode `src/verify.rs` (`Verified`) |
| Failed-then-fixed signal (F2 trigger) | Every attempt, pass or fail, recorded on the ledger task with its failure note; at most `MAX_ATTEMPTS` | incode `src/ledger.rs`, `src/runner.rs` |
| Compact failure logs | Repeated lines and passing-test noise collapsed before a failure reaches a model | incode `src/verify.rs` (`compact_log`) |
| Guards (F4) | Protected paths (tests, CI, migrations, `.env`, the ledger, `.incode/guards.json`) refused as write targets | incode `src/guards.rs` |
| Permission decider (F4, hooks) | Incord Memory's `pre-tool` hook asked on every tool call: allow, block or ask a person; fails closed for writes | incode toolbox; `docs/incord-memory-integration.md` |
| Worktrees (F6 sandbox) | Create, list, remove; parallel `/run` lanes in worktrees, merged back, conflicts kept on a branch | incode `src/tools/worktree.rs`, `src/parallel.rs` |
| PR opening (F7, operator stage) | `/pr [branch] [--draft]`: refuses the default branch, uncommitted work, an empty branch and bad names before anything is pushed; pushes; reuses an open PR or runs `gh pr create` | incode `src/tools/pull_request.rs` (new, not yet committed) |
| Routing between agents (F6 surface) | Roles with ordered model candidates, tiers, CLI agents and API models, all in `admin.json` | incode `src/roles.rs`, `src/agent/catalogue.rs` |
| Agent definitions (F6 surface) | Sub-agents as Markdown files with role, model and tool allow-list; a built-in `researcher` (memory, then web, saves findings) | incode `src/subagents.rs`, `defaults/agents/` |
| Token budget | A token ceiling that stops the next request and counts every request in full | incode `src/budget.rs` |
| Memory client | Recorder lookup and handoff, `remember`, recall | incode `src/recorder.rs`, `src/knowledge.rs` |
| Running under the Watchman app | incode as the lead of a Watchman orchestration reaches its orchestrator (the control credentials pass incode's environment filter) | incode `src/tools/env_filter.rs` (new, not yet committed) |

### Left to build

| # | Piece | Phase |
| --- | --- | --- |
| 1 | Benchmark format and `insi eval`: tasks, public and private checks, seeds, results | 1 |
| 2 | Private-check repository, outside every checkout | 1 |
| 3 | First 20–30 tasks, from bug-fix commits in incord-code's history that came with a test | 1 |
| 4 | Dollar and wall-time budgets (incode has tokens only) | 1 |
| 5 | Error-signature normalizer | 1 |
| 6 | Lesson writer: failed-then-verified task → lesson with evidence, in Incord Memory | 1 |
| 7 | Topic resolution rules (repo, path, command) and topic fields on lessons — confirm what Incord Memory already stores | 1 |
| 8 | Briefing builder and just-in-time injection through the pre-tool hook | 1 |
| 9 | `insi` workspace and binary; CLI | 1 |
| 10 | MCP server (read side) | 2 |
| 11 | Check inside a worktree before any merge or PR (also needed by `/run` lanes-as-PRs) | 2 |
| 12 | PR body with evidence; secret/PII scan before opening | 2 |
| 13 | Trusted-label issue intake | 2 |
| 14 | GitHub App with short-lived, PR-only tokens | 2 |
| 15 | `harness-config/` folder, read by incode | 3 |
| 16 | Proposer, accept-if-better, UCB1 bandit, audit events | 3 |
| 17 | Connectors: CI logs, error tracking, docs, email | 4 |
| 18 | Containers for issue work | 4 |

## Decisions (were open questions)

Proposed answers; each stands until revised.

- **First benchmark repo:** incord-code itself. Its bug-fix commits that added a test are ready-made tasks: the parent commit is the starting state, and the new test is the private check.
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
