# Incord Code

`incode` — an autonomous coding harness. It plans work, delegates it to a coding
agent you already have installed and logged in, and proves the work was done
before it calls it done.

It is **not** another coding assistant. The agent you install brings the loop,
the tools and the model. What this adds is the part an agent cannot do for
itself: refuse to start before the project is specified, keep the files that
judge the work out of the agent's reach, and close a task only on a command that
exited zero.

---

## What it does

- **Delegates, on your own subscription.** The work goes to an installed agent
  CLI that authenticates itself. This harness reads, holds and transmits no
  credential of its own.
- **Refuses to code an unspecified project.** About, Goals, Synopsis, PRD and a
  roadmap must exist before a source file may be written. On an existing
  codebase it reads the code first and asks only what the code cannot answer.
- **Proves completion.** A task reaches `Done` only through a command that
  exited zero. No model's report — however confident, from whichever model —
  reaches that state.
- **Protects the exam.** Tests, CI config, migrations, `.env` files and the
  ledger are refused as write targets. An agent that can edit its own tests will
  eventually make a failing one pass by editing it.
- **Routes by role.** Reading and renaming go to a cheap model; judgement goes to
  an expensive one. You decide which, per role.
- **Bounds the spend.** A token ceiling stops the next request. What was spent is
  always counted in full, including the request that crossed the line.
- **Undoes a turn.** The tree is snapshotted before a delegated turn; `/undo`
  restores only what actually changed.
- **Diffs against past code.** Unified line diffs compare files or the entire
  workspace against older git revisions, commits, or other files via `/diff`.
- **Remembers across sessions.** Context, standing rules, skills, already-solved
  work and audit findings are read from the shared memory service — not restated
  by you every morning.
- **Compresses verification logs.** Noisy test suites and compiler runs (hundreds of
  passing test lines or repeated warnings) are compacted before entering failure
  reports, preserving critical error traces and panics without bloating model context.

## Core Architecture: Asymmetric Execution & Incord Memory

The primary design goal of `incode` is **asymmetric model economics for radical token efficiency**:

```
                       ┌──────────────────────────────┐
                       │          BIG MODEL           │
                       │    (Planning & Synthesis)    │
                       └──────────────┬───────────────┘
                                      │
              1. Plans & decomposes   │  4. Re-checks chunk diffs
                 into bounded tasks   │     via Incord Memory
                                      ▼
                       ┌──────────────────────────────┐
                       │         SMALL MODELS         │
                       │ (High-Volume Implementation) │
                       └──────────────┬───────────────┘
                                      │
                                      │  2. Executes chunks,
                                      │     writes code
                                      ▼
                       ┌──────────────────────────────┐
                       │        INCORD MEMORY         │
                       │ (Auto-Capturing Code Ledger) │
                       └──────────────┬───────────────┘
                                      │
                                      │  3. Auto-indexes diffs &
                                      │     records tool turns
                                      ▼
                       ┌──────────────────────────────┐
                       │       MECHANICAL PROOF       │
                       │   (verify::Verified: exit 0) │
                       └──────────────────────────────┘
```

1. **Big Model for Planning & Decomposition (`src/plan.rs`):**
   An expensive, high-reasoning frontier model is used sparingly: it reads the project brief/PRD, determines the architecture, and splits roadmap items into role-tagged chunks (`plan::split`). It never reads raw codebase files, greps strings, or writes boilerplate — avoiding high-tier token burn on low-altitude tasks.

2. **Small Models for High-Volume Implementation (`src/roles.rs`):**
   Fast, cheap, or local models (`haiku`, local models, etc.) take the decomposed tasks (`grunt`, `implement`, `frontend`). They perform 80–90% of the raw tool calls, edits, and test iterations at a fraction of the cost.

3. **Strict Agent Isolation & Agent ID Attribution (`src/identity.rs`, `src/ledger.rs`):**
   Each spawned worker agent carries an unforgeable identity:
   - **`AgentId`**: Stable FNV-1a fingerprint (`backend:role`, e.g. `claude-code:grunt (2569feb9)`).
   - **`instance`**: Unique per-spawn identifier (`agent-xxxxxxxxxxxx`).
   All turns, code edits, and completed tasks are tagged with this identity in `.incord/journal.jsonl` and Incord Memory (`PROJECT#FINGERPRINT`). Workers operate in strict isolation: no small agent can inspect, tamper with, or disturb another small agent's workspace or context. Only the Main Agent (Orchestrator) has the full roadmap context to query, inspect, and evaluate individual worker contributions (`tasks_by_instance`, `tasks_by_fingerprint`).

4. **Incord Memory Re-Checking Substrate (`src/knowledge.rs`, `src/recall.rs`):**
   Incord Memory automatically captures every code edit and turn in the background. When the Main Agent re-checks the small models' work, it avoids loading massive multi-thousand-line contexts by using Incord Memory's high-efficiency AST tools:
   - **`code_skeleton`**: Strips implementation bodies for ~90% token reduction, returning AST definitions, signatures, and doc comments so the evaluator verifies interface contracts without reading raw implementations.
   - **`git_blast_radius`**: Analyzes the git diff against the memory graph, tracing transitive callers (depth 1–5) and identifying impacted test suites.

5. **Unforgeable Proof & Targeted Small-Agent Repair Loop (`src/verify.rs`, `src/runner.rs`):**
   Work completed by small models is never accepted on prose claims. `verify::Verified` demands a host process exit code 0, while `src/guards.rs` prevents agents from weakening the tests that judge them.
   - **Self-Healing Loop**: If verification fails or a regression is detected, the Main Agent does not rewrite the code with expensive models. Instead, it identifies the specific worker responsible from the task attribution and re-dispatches *that specific small agent* with the compacted failure diagnostics (`compact_log`) and previous chunk context to edit and fix the issue it created.

6. **Zero-Hardcoding Multi-Agent Fallback Chain (`src/agent/registry.rs`, `src/task_run.rs`):**
   Execution is never pinned to a single hardcoded agent. `incode` probes and builds a candidate chain of drivable agents (`registry::drivable_chain`). If the primary agent encounters authentication expiry, process crashes, or quota exhaustion, `task_run` catches the failure and automatically cascades execution to the next available agent (e.g. from Claude Code to Codex CLI or Antigravity / Gemini) so work completes reliably.

## Ecosystem Positioning & Comparison

`incode` was evaluated against the 120+ terminal-native coding agents and orchestration harnesses curated in `awesome-cli-coding-agents` (open-source agents, session multiplexers, parallel worktree runners, autonomous loops, and agent infrastructure).

While most tools focus on driving a raw model in an open-ended loop or tiling terminal multiplexers, `incode` acts as the deterministic execution layer and **the hands of Incord Memory**. It enforces invariants that models and wrappers cannot enforce on themselves:

| Capability / Guarantee | Mainstream Coding CLIs & Harnesses | `incode` Guarantee |
|---|---|---|
| **Credential Model** | Requires raw provider API keys or vendor tokens | **Zero credentials held** — delegates via locally authenticated subscription CLIs (`claude`, `codex`, `agy`) |
| **Agent Fallback Chain** | Pinned to a single CLI/model; fails immediately on error | **Automatic Multi-Agent Fallback** — Ordered candidate chain cascades from primary to alternatives on auth or process failures (`src/agent/registry.rs`, `src/task_run.rs`) |
| **Model Token Economics** | Homogeneous model usage (expensive model does all typing) | **Asymmetric planning & execution** — Frontier model plans/reviews; cheap small models execute chunks (`src/plan.rs`, `src/roles.rs`) |
| **Agent Isolation & ID** | Shared scratchpads or untracked multi-agent churn | **Strict worker isolation & Agent IDs** — Tasks tagged with `AgentId` and instance ID; isolated memory queries (`src/identity.rs`, `src/ledger.rs`) |
| **Re-checking Efficiency** | Re-reads full files (100k+ token context blowup) | **AST Skeleton & Blast Radius** — `code_skeleton` (~90% cheaper token cost) and `git_blast_radius` callers walk (`src/knowledge.rs`) |
| **Repair & Failure Loop** | Global re-prompting or expensive whole-plan resets | **Targeted small-agent repair** — Failing tasks re-dispatch the exact small agent that created the issue with compacted logs (`src/runner.rs`, `src/verify.rs`) |
| **Completion Verification** | Model self-reporting or summary assertions | **Unforgeable mechanical proof** — `verify::Verified` demands a host process exit code 0 (`src/verify.rs`) |
| **Evaluation Integrity** | Agents can freely edit tests, CI scripts, and checks | **Protected exam sentinel** — Deny rules + pre/post turn SHA256 tree hashing reject self-modified checks (`src/guards.rs`) |
| **Intake Discipline** | Immediate coding on unvetted, ambiguous prompts | **Strict 5-spec gate** — PRD, Architecture, Conventions, Assumptions, and Verification must exist before source is written (`src/spec.rs`, `src/brief.rs`) |
| **Memory & Context** | Fragmented per-repo files (`.memcode`, `AGENTS.md`) | **Unified Incord Memory** — Shared semantic recall, Watchman tool-result handoffs, and always-on ranked rules (`src/recall.rs`, `src/rules.rs`) |
| **Log Compression** | Dumps raw build/test stdout into model context | **Pre-LLM compaction** — Collapses duplicate lines and passing test noise so failure diagnostics are not crowded out (`src/verify.rs`) |
| **Task State Tracking** | Ad-hoc prompt loops or loose markdown checklists | **Deterministic 7-state ledger** — Append-only `.incord/journal.jsonl` audit trail with single-runner mutex (`src/ledger.rs`, `src/runner.rs`) |

## Requirements

- **Rust 1.88+** to build. This is measured, not guessed — it is the highest
  `rust-version` declared by any locked dependency's own manifest (`home
  0.5.12`; 154 of the 209 declare one at all), and it is
  declared in `Cargo.toml` so cargo refuses an older toolchain by name rather
  than failing deep inside a dependency.
- **A coding agent CLI**, installed and logged in. `/doctor` names what it found
  and what is missing.
- **Optional:** the Incord memory service on `http://127.0.0.1:9180`. Every part
  that uses it is a plugin and defaults to on; without it the harness still
  works and simply knows less. Point elsewhere with `INCORD_MEMORY_URL`.

## Install

```sh
git clone <this repo> incord-code
cd incord-code
cargo build --release
```

The binary lands at `target/release/incode`. Put it on your `PATH`:

```sh
# Linux / macOS
install -m755 target/release/incode ~/.local/bin/incode

# Windows (PowerShell)
Copy-Item target\release\incode.exe $env:USERPROFILE\.local\bin\
```

Then check the machine is ready:

```sh
incode          # start a session, then type /doctor
```

## Usage

```
incode [OPTIONS]

  -m, --model <NAME>        Model to use (or set INCORD_MODEL)
  -w, --workspace <PATH>    Workspace root (default: current directory)
  -p, --permission-mode <M> read-only | workspace-write | danger-full-access | prompt | allow
      --no-env-filter       Pass the full environment to spawned commands
      --admin               Print the operator's settings and where each came from
  -t, --task <TEXT>         Run one instruction and exit (needs no API credential)
  -r, --resume <ID>         Continue an agent session by its id
      --add-dir <PATH>      Also work in this directory (repeatable)
  -h, --help                Print help
  -V, --version             Print version
```

One instruction, no session:

```sh
incode --task "add a health endpoint and a test for it"
```

### Commands inside a session

| Command | What it does |
|---|---|
| `/help` | Every command, with what it does |
| `/doctor` | Whether this session can work: agents, login, guards, specs, ledger, memory |
| `/status` | Model, workspace, permission tier, tokens spent, and the ledger's progress |
| `/admin` | Which model, ceiling and check this session resolved to, and which layer decided each |
| `/run` | Work the roadmap ledger until it is empty or something needs a person |
| `/roadmap` | What the roadmap says, and which goals a command has proven |
| `/guards` | The paths the agent is not allowed to write |
| `/links <file>` | What else uses what this file defines, and what it depends on |
| `/undo` | Put the files from the last change back as they were |
| `/diff [file] [rev]` | Unified diff of file or workspace against older code |
| `/plan` · `/act` | Propose changes without making them · apply them, within the guards |
| `/model [name]` | Show or switch the model |
| `/clear` · `/save` · `/sessions` · `/resume <id>` | Session handling |
| `/remember <text>` · `/memories <query>` · `/forget <ref>` | The shared long-term memory |
| `/skills` | The skills this session loaded from the shared store |
| `/bg <cmd>` · `/jobs` · `/kill <id>` | Background shell jobs |
| `/worktree [cmd]` | Git worktree create / list / remove |
| `/add-dir <path>` | Also work in this directory — guards and undo extend to it too |
| `/tools` · `/mcp` · `/env` · `/compact` | Where tools, MCP servers and context are handled |
| `/quit` | Exit |

## Configuration

Settings resolve in this order — **flag > environment > admin file > built-in** —
and `--admin` prints which layer decided each value.

`~/.incord-code/admin.json`:

```json
{
  "roles": {
    "explore":   ["haiku", "sonnet"],
    "grunt":     ["haiku", "sonnet"],
    "diagnose":  ["sonnet", "opus"],
    "implement": ["sonnet", "opus"],
    "frontend":  ["sonnet", "opus"],
    "plan":      ["opus", "sonnet"],
    "review":    ["opus", "sonnet"]
  },
  "budget_tokens": 2000000,
  "verify_command": ["cargo", "test"]
}
```

A role's candidates are an **ordered fallback chain**. A candidate may name the
agent that should serve it as `agent:model`; a bare model name means "whichever
agent is running". A candidate naming an agent that is not installed is skipped
rather than treated as an error, so one table works on every machine.

Useful environment variables:

| Variable | What it sets |
|---|---|
| `INCORD_MODEL` | The session's model |
| `INCORD_ROLE_<ROLE>` | That role's ordered candidates, comma-separated |
| `INCORD_BUDGET_TOKENS` | The token ceiling for a run |
| `INCORD_MEMORY_URL` | Where the memory service listens |
| `INCORD_CODE_DIR` | Where this harness keeps its settings |
| `INCORD_TRACE` | Say what the harness did, as it happens |
| `INCORD_SEARCH_URL` / `INCORD_SEARCH_KEY` | The search endpoint for the API path |

### What lives where

| Path | What it holds |
|---|---|
| `~/.incord-code/admin.json` | Your settings — models, ceiling, check, plugins |
| `~/.incord-code/sessions/sessions.json` | Session pointers, so a conversation can be reopened |
| `~/.incord-code/history.txt` | The lines you typed |
| `<project>/.incord/ledger.json` | What is being worked on in *this* checkout |
| `<project>/.incord/contracts.json` | Interfaces frozen before the work was split up |
| `<project>/.incord/guards.json` | Extra protected paths for this project |

Per-project state lives **in the project**; `~/.incord-code` is yours, not the
project's.

## Security notes

- **No credential is read or held.** The agent authenticates itself; this
  harness only detects that a binary exists.
- **Spawned commands get a bounded environment**, not a safe one. The allowlist
  deliberately passes cloud, git-forge and model credentials through, because
  `gh` and a deploy cannot work without them. Unset what you do not want
  reachable before starting a session. `--no-env-filter` widens this, not
  narrows it.
- **`history.txt` is plain text.** Anything you type is written there, a
  credential included. Delete the file if you paste one.
- **Web reads refuse the private network** by name and again by resolved
  address, with the connection pinned to the address that was checked. A
  redirect to a different host is checked by name only.
- **Retrieved text is data, never instructions.** What comes back from memory,
  the web or a tool is labelled as such before the model sees it.

## Development

```sh
cargo test                          # 1,070+ tests (1,049 lib + 22 bin)
cargo clippy --all-targets -- -D warnings
cargo fmt
```

The documents under `docs/` are the input the loop reads, not decoration:
`PRD.md` (what and why), `ARCHITECTURE.md` (the shape), `CONVENTIONS.md` (how
the code should read), `ASSUMPTIONS.md` (what was decided without asking),
`RUNNER.md` (the autonomous loop), `VERIFICATION.md` (what counts as done).

### Ecosystem-Informed Roadmap Priorities

From our comparative analysis against the 120+ coding harness ecosystem:
1. **Multi-Worktree Parallel Runner**: Extending `src/runner.rs` to spawn concurrent workers in isolated Git worktrees for independent roadmap tasks (`depends_on: []`) using `src/tools/worktree.rs`.
2. **Standard Agent Client Protocol (ACP)**: Implementing native ACP JSON-RPC 2.0 client support in `src/agent/` to drive any standards-compliant coding agent backend.
3. **AST-Level Anti-Regression Guards**: Compiling memory-recorded failure lessons into pre-commit syntax/AST guards so resolved bugs can never be reintroduced.

## Licence

MIT — see [LICENSE](LICENSE).
