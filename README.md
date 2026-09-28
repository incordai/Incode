# incode

`incode` — an autonomous coding harness. It plans work, has a model do it, runs
every tool the model calls through its own guards, and proves the work was done
before it calls it done.

**incode is the harness.** The coding-agent CLIs you already have and are logged
in to — `claude`, `codex` and `agy` — are used only as models, run headless on
your own subscriptions. The loop's tools, the permission checks, the undo
history, the memory hooks and the record of what happened are incode's own.
(Today that is fully true for `claude`; `codex` and `agy` still run their own
tools — see [Limits](#limits).)

---

## Contents

- [What it does](#what-it-does)
- [How it is built](#how-it-is-built)
- [Requirements](#requirements) · [Install](#install) · [Quick start](#quick-start)
- [A new project](#a-new-project)
- [Modes](#modes)
- [Choosing agents and models](#choosing-agents-and-models)
- [Commands inside a session](#commands-inside-a-session)
- [Tools](#tools)
- [Safety](#safety)
- [Incord Memory integration](#incord-memory-integration)
- [User-defined sub-agents](#user-defined-sub-agents)
- [Configuration](#configuration)
- [Security notes](#security-notes)
- [Limits](#limits)
- [Development](#development)

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
- **At least one coding agent CLI**, installed and logged in: `claude`, `codex`
  or `agy`. `/doctor` names what it found and what is missing. `gemini`,
  `opencode` and `cursor` are detected but not driven.
- **Optional:** the Incord Memory service on `http://127.0.0.1:9180` and its
  engine binary. Without them incode still works and knows less. Point
  elsewhere with `INCORD_MEMORY_URL` and `INCORD_ENGINE_BIN`.
- **Optional programs** that add tools when found: a headless browser
  installed as an [external tool](#external-tools), a Chromium-family browser
  for text PDFs, and LibreOffice for `ConvertDocument` (see [Tools](#tools)).

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

Each Outline language is a default cargo feature (`lang-rust`, `lang-ts`,
`lang-python`, `lang-go`, `lang-js`); build with `--no-default-features` and a
subset to drop grammars.

## Quick start

```sh
incode                      # a session in the current directory; type /doctor
incode -w path/to/project   # a session in another directory
incode -t "add a health endpoint and a test for it"
```

## A new project

Open incode in an empty folder and its first job is the specification, not the
code. It asks at most seven questions in one batch, researches the rest, and
writes:

| File | What it holds |
|---|---|
| `docs/PRD.md` | What is being built, with a `## Non-goals` section |
| `docs/ARCHITECTURE.md` | The stack and the modules, with a `## Layout`: the stack's official generator command (`cargo new`, `npm create vite@<version>`, `uv init`) with its verified version, and the folder tree |
| `.incode/contracts.json` | The shapes that cross module boundaries |
| `docs/CONVENTIONS.md` | Naming, errors, comments, tests |
| `docs/VERIFICATION.md` | The command that proves each roadmap item |
| `docs/ASSUMPTIONS.md` | Every choice made without asking, with its reason |

Writing source is refused until they exist. The roadmap then starts with
**`s1 Create the layout`**: the generator is run as ordinary work, and the item
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

## Modes

| Invocation | What it is |
|---|---|
| `incode` | The interactive session (REPL). |
| `incode -t "<text>"` | One instruction, then exit. Needs no API credential. |
| `incode -t "<text>" --output-format stream-json` | The same, printing one JSON event per line: `init`, `text`, `thinking`, `tool_use`, `tool_result`, `usage`, `result`. stdout carries JSON only; everything else goes to stderr. |
| `incode -t "<text>" --output-format json` | The same, printing only the final `result` object. |
| `incode --acp` | incode as an Agent Client Protocol agent on stdin/stdout, for an editor or app. Takes `-p` (`read-only`, `workspace-write`, `danger-full-access`), `-m` and `--no-env-filter`; each `session/new` names its own workspace. |
| `incode --serve [--port N]` | A local viewer on `127.0.0.1` for what a run in this workspace is doing. Reads, plus two writes behind a per-run token in `.incode/serve.token`: stop the running `/run`, and answer an agent's question. |
| `incode --admin` | Print the resolved settings and which layer decided each. |
| `incode --desktop-stop` | Halt the desktop driving this workspace. |
| `incode --install-tools` | Download, check and unpack every [external tool](#external-tools) in `admin.json`, then exit (non-zero if any was not installed). |
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

- **Local agents** found on `PATH` (`claude`, `codex`, `agy`).
- **The API route** (id `api`): a model reached over HTTP, configured with
  `INCODE_API_URL` (the messages endpoint), `INCODE_API_KEY` and
  `INCODE_API_MODEL`. incode runs its own tool loop for it. Setting these adds
  an entry; it does not replace the installed agents.
- **Provider agents** from `admin.json` `api_providers` (below). They are listed
  and marked not usable: driving them is not wired yet.

| Command | What it does |
|---|---|
| `/agent` | List every agent and whether it can be used, with the reason when not. |
| `/agent <id>` | Save the lead agent (`agent` in `admin.json`). |
| `/agent sub <id>` | Save the agent that works `/run`'s tasks, sequential and in lanes (`sub_agent`). `/agent sub` alone clears it; unset, it follows the lead. |
| `/roles` | Show every role, the models it resolves to, and where that came from (your choice, an `INCODE_ROLE_*` variable, or unset = the session model). |
| `/roles <role> <model\|@tier>...` | Save a role's ordered candidates (at most 8). An unknown role or tier is refused and nothing is written. |
| `/roles <role> default` | Clear a role. |
| `/model [name]` | Show or switch the session's model. |

The lead is `INCODE_AGENT`, else `admin.json` `agent`, when usable; else the
first usable local agent; else a usable API agent. Roles are `explore`,
`diagnose`, `plan`, `implement`, `review`, `frontend` and `grunt`. A candidate
may name its agent as `agent:model`; a candidate naming an agent that is not
installed is skipped. Changes apply from the next `/run` and the next session.

### The API providers contract

```json
"api_providers": [{"name": "team", "url": "https://agents.example", "key_env": "TEAM_API_KEY"}]
```

- `key_env` is the **name** of an environment variable. The key is read from it
  when needed and never written to disk or printed.
- incode sends `GET {url}/v1/agents` with `Authorization: Bearer <key>`, and
  allows 3 seconds. The url must be `https`, or plain `http` to this machine
  only; any other url is refused before the key is sent.
- The reply is `{"agents": [{"id": "coder", "name": "Coder", "model": "model-1"}]}`.
  `id` is required and unique; `name` and `model` are optional.
- Each agent's catalogue id is `<provider name>/<id>`. A provider that cannot
  be asked shows as one entry saying why.

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
| `/links <file>` | What else uses what this file defines, and what it depends on |
| `/undo` | Put the files from the last changed turn back |
| `/rewind <n>` | Put the files back as they were n changed turns ago |
| `/diff [file] [rev]` | Unified diff of a file or the workspace against older code |
| `/plan` · `/act` | Read-only mode · workspace-write mode |
| `/image <path>` | Send a PNG or JPEG with the next message |
| `/clear` · `/save` · `/sessions` · `/resume <id>` | Session handling |
| `/remember <text>` · `/memories <query>` · `/forget <ref> [confirm]` | The shared long-term memory |
| `/skills` | The skills this session loaded from the shared store |
| `/bg <cmd>` · `/jobs` · `/kill <id>` | Background shell jobs (shared with the model's background `Bash`) |
| `/worktree [cmd]` | Git worktree create / list / remove |
| `/add-dir <path>` | Also work in this directory — guards and undo extend to it too |
| `/tools add <name>` · `/tools update [name]` · `/tools remove <name>` | Install, update or remove an [external tool](#external-tools) (also over `--acp`) |
| `/tools` · `/mcp` · `/env` · `/compact` | Information screens (`/tools` also lists the external tools and what is installed) |
| `/quit` | Exit |

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
| `screen_info` · `screenshot` · `ui_tree` · `cursor_position` | observe | Look at the desktop. Offered only when `INCODE_DESKTOP=1`. |
| `click_element` · `type_text` · `paste_text` · `key` · `scroll` · `drag` · `move_mouse` · `click` | run | Act on the desktop. Same condition; refused in read-only. |
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

## Safety

In the order a call meets them:

1. **Known tool, valid input.** An unknown name or an input that breaks the
   tool's schema runs nothing.
2. **Guards.** Tests, CI config, migrations, `.env` files, the ledger and the
   paths in `.incode/guards.json` are refused as write targets, for every file a
   change touches. For an agent that still runs its own tools (`codex`, `agy`),
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
| Windows | a write-restricted token: nothing can be written but `NUL` | **not confined.** The only sound route rewrites the workspace's ACL on every command; incode does not do that, and says so in the trace |

Honest limits: **the network is not restricted on any OS**; Windows
workspace-write runs unconfined; and the operator's own `/bg` jobs at the REPL
are never sandboxed. Where a mode cannot be applied, the command runs
unconfined and the trace says which.

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
  "web_search": { "url": "https://search.example/api", "key_env": "SEARCH_KEY" },
  "tools": [{ "name": "browser", "release": "<owner>/<repo>", "version": "latest",
              "asset": "<prefix>-{arch}-{os}.{ext}", "run": ["<program>", "mcp"],
              "provides": "browser" }],
  "pdf_renderer": "auto",
  "sandbox": "workspace-write",
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
| `api_providers` | Services that list API agents ([contract](#the-api-providers-contract)) |
| `web_search` | `WebSearch`'s endpoint (`url`, the query sent as `q`) and the name of its key's variable. The key is sent only over https or to loopback |
| `tools` | [External tools](#external-tools): each one's release, version, asset, `run` and `provides` |
| `browser_bin` · `pdf_renderer` | For development, a headless browser executable that wins over the installed one · which program `RenderPdf` uses |
| `sandbox` | `off` · `workspace-write` · `read-only`; unset follows the permission mode |
| `hooks` (also `desktop_hooks`) | Your `pre_tool_use` / `post_tool_use` commands, run for every tool. The event is JSON on stdin; exit 2 on a pre-tool command denies; bounded at 10 s |
| `plugins` | Which memory-service parts are used; all on by default |

Environment variables:

| Variable | What it sets |
|---|---|
| `INCODE_MODEL` | The session's model |
| `INCODE_AGENT` | The lead agent, by id |
| `INCODE_ROLE_<ROLE>` | That role's ordered candidates, comma-separated |
| `INCODE_BUDGET_TOKENS` | The token ceiling for a run |
| `INCODE_VERIFY_CMD` | The check command |
| `INCODE_CHECK_CMD` | What `Diagnostics` runs |
| `INCODE_LANES` | Parallel `/run` lanes (needs git; at most 8) |
| `INCODE_SANDBOX` | The sandbox mode |
| `INCODE_DESKTOP` | `1` offers the desktop tools |
| `INCODE_API_URL` · `INCODE_API_KEY` · `INCODE_API_MODEL` | The API route |
| `INCORD_MEMORY_URL` · `INCORD_ENGINE_BIN` | The memory service · its engine binary |
| `INCODE_BROWSER` · `INCODE_PDF_BROWSER` · `INCODE_SOFFICE` | The headless browser · the PDF printer · LibreOffice |
| `INCODE_SERVE_ORIGINS` | Extra origins `--serve` accepts |
| `INCODE_DIR` | Where incode keeps its settings (default `~/.incode`) |
| `INCODE_TRACE` | Say what incode did, as it happens |
| `INCODE_ASCII` · `INCODE_NO_SPLASH` · `INCODE_MOUSE` | Terminal display |

### What lives where

| Path | What it holds |
|---|---|
| `~/.incode/admin.json` | Your settings |
| `~/.incode/agents/*.md` | Your sub-agents, for every project |
| `~/.incode/sessions/<workspace>.json` | Session pointers, so a conversation can be reopened |
| `~/.incode/history.txt` | The lines you typed |
| `~/.incode/tools/<name>/` | An installed external tool: `<version>/` and `installed.json` |
| `<project>/.incode/ledger.json` | What is being worked on in *this* checkout |
| `<project>/.incode/contracts.json` | Interfaces frozen before the work was split up |
| `<project>/.incode/guards.json` | Extra protected paths for this project |
| `<project>/.incode/agents/*.md` | The project's sub-agents |
| `<project>/.incode/checkpoints/<session>/` | Each turn's "before", for `/undo` and `/rewind` |
| `<project>/.incode/transcripts/` · `.incode/context/` | Session transcripts · oversized memory blocks |
| `<project>/.incode/transcripts/tasks/<task>.jsonl` | What each `/run` agent was told and did, one file per task |
| `<project>/.incode/.gitignore` | Which of the above is this machine's own and is not committed |

Per-project state lives **in the project**; `~/.incode` is yours. Of the
project's `.incode/`, commit `contracts.json`, `guards.json`, `agents/*.md` and
`.gitignore`; the `.gitignore` incode writes keeps the rest out.

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

- `codex` and `agy` still use their own tools; only `claude` runs as a model
  only. On those routes the guards are a sentinel (detection), not a gate.
- A client's MCP servers are stdio or Streamable HTTP; the older SSE transport is
  not spoken.
- The Windows sandbox does not confine workspace-write, and no OS restricts
  the network.
- Provider agents from `api_providers` are listed but not driven.

The full list is in `docs/ROADMAP.md`.

## Development

```sh
cargo test                          # 1,700+ #[test] functions
cargo clippy --all-targets -- -D warnings
cargo fmt
```

The documents under `docs/` are the input the loop reads, not decoration:
`PRD.md` (what and why), `ARCHITECTURE.md` (the shape), `CONVENTIONS.md` (how
the code should read), `ASSUMPTIONS.md` (what was decided without asking),
`RUNNER.md` (the autonomous loop), `VERIFICATION.md` (what counts as done),
`ROADMAP.md` (what is done and what is open).

## Licence

MIT — see [LICENSE](LICENSE).
