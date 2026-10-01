# Roadmap

Ordered by what it costs to be wrong about, not by what is quickest.

Process for every item: `PROCESS.md`. Local `main` is the test bench, the gate
runs before anything is pushed, and every load-bearing change gets a control.

**Status as of 2026-09-22:** 1288 lib + 38 bin tests, clippy 0, fmt clean,
43 unwired capabilities (67 at the start of the day).

**Status as of 2026-09-26 (HEAD `b39a99e`):** the models-only harness, `--acp`, the
full toolbox and the memory lifecycle landed (section 0). The unwired ratchet
`MAX_UNWIRED` is 42. Test counts were not re-measured for this update.

---

## 0. The models-only harness — 2026-09-24 to 2026-09-26

The owner's principle: incode is the harness. claude, codex and agy are used only
as models, headless, on the owner's subscriptions, and every tool is incode's own.
This is fully true for claude; section 0b says what is not.

### 0a. Done

| What | Commit |
|---|---|
| One agent catalogue (local CLIs, the API route, `api_providers`); `/agent`, `/agent <id>`, `/agent sub <id>` | `5640c0e` |
| `/roles`: list, set and clear each role's models; `/run` reads roles from `admin.json` at start | `3a3ca3f` |
| `incode --acp`: incode as an Agent Client Protocol agent (initialize, session/new, prompt, cancel, set_mode, close); stdout kept pure | `f9a8491` |
| ACP: `usage_update`, edits as diff blocks, slash commands | `63fc212` |
| The memory engine's `pre-tool` run in incode's Toolbox; fail closed for changes; Allow/Reject raced against `permission-request`; `AskUserQuestion` as a tool | `d7f5d41` |
| The bridge: claude runs with its own tools, settings, hooks, CLAUDE.md and skills off; its only tools are incode's, over `incode --tool-server` | `27cf90f` |
| The memory lifecycle fired by incode on every route (`session-start`, `user-prompt`, `code-edit`, `capture`, `session-end`); transcripts in `.incode/transcripts/`; CLAUDE.md and AGENTS.md read by incode | `4802719` |
| Toolbox: MultiEdit, ApplyPatch, LS, NotebookEdit, background Bash with BashOutput/KillShell, WebFetch, WebSearch, TodoWrite, Diagnostics, watchman_lookup, the team verbs; one reach table; WebFetch no longer leaks the search key | `d80b02a` |
| ripgrep-based Glob/Grep/LS, Outline (tree-sitter), documents in Read, schema check on every call; ConvertDocument when LibreOffice is found | `079988b` |
| Pictures in tool results; the headless browser (WebFetch `render_js`, Screenshot, RenderPdf); the desktop tools behind the Toolbox gate | `70c5556` |
| ACP: agent and role pickers, the client's stdio MCP servers run by incode, `/run` as a live plan, `session/load` and `session/list` | `fd2ffec` |
| ACP: sub-agent tabs, `_meta.incode`, live command output, thinking, pictures in a prompt; the `Agent` tool on the session's own CLI | `eac0322` |
| User-defined sub-agents and `/agents`; `-t --output-format stream-json\|json`; checkpoints on disk with `/undo` after restart and `/rewind <n>`; the OS sandbox; operator hooks on every tool; `/run` workers as memory sessions; codex `--disable hooks`; `run_on_compact` allowlisted (A5) | `b39a99e` |

### 0b. Still open

- **codex and agy still use their own tools.** agy has no switch to turn them off
  or add MCP per run; codex has no switch for `apply_patch` / `update_plan`, and
  its candidate flags could not be proved (usage limit). On both routes the guards
  are the Sentinel (detection), not a gate (`ARCHITECTURE.md` §5a).
- **Driving ACP agents (opencode, gemini).** Built in `fcfa2d9`, reverted in
  `c98757a`; not in HEAD. gemini, opencode and cursor are detected, not driven.
- ~~**Provider agents** from `api_providers` are listed and marked not usable.~~
  DONE 2026-09-29 (the owner chose the chat-completions format): driven at
  `{url}/v1/chat/completions` with the agent's id as the model. Same change:
  admin.json `api_models` (Anthropic, Z.ai incl. the GLM Coding Plan,
  OpenRouter — config, not code), both wire formats (`agent::openai` translates
  at the edge), `anthropic-version`, one key header per route (`auth`), the
  https-or-loopback rule on every API key, a route's `sub_model` for its
  sub-agents, and `/models`. Not run against a live service: no key was used.
- **The API route's gaps are closed (2026-10-01).** Prompt caching: the
  `anthropic` format marks the tools, the system prompt and the newest message
  as cache breakpoints (`prompt_cache`, default on); the chat format relies on
  the service's own automatic caching and reads its cached-token count. Not yet
  measured against a live service. The session's permission mode
  and `--add-dir` reach its toolbox since 2026-10-01 — before that a `plan`
  session on an API model could write (shown by a control: the test failed
  with "a plan session wrote a file" when the forwarding was removed). The reply ceiling, tool rounds and request
  timeout are per-entry config (`max_tokens`, `max_steps`, `timeout_secs`).
  Retry on a busy answer is DONE 2026-10-01: 429, 503 and 529 are sent again
  up to `max_retries` (default 3), waiting `Retry-After` or 1, 2, 4… s, capped
  at 30 s, before any of the reply is read (so a streamed turn never repeats
  text); a wrong request is not retried.
- **No SSE MCP transport.** An ACP client's HTTP MCP servers are spoken to over
  Streamable HTTP (`toolbox::mcp_http`, `mcpCapabilities.http: true`); the older SSE
  transport is not, and `initialize` says so. A server that forgets its session
  (`404`) is not re-initialized: its calls fail until the next session. Also not
  offered over ACP: `session/resume`, elicitation, fs and terminal.
- **Windows workspace-write sandbox — opt-in, not the default.** Built as a
  per-SESSION grant (one ACE for a private session SID on each root, revoked at
  session end, a crashed session's ACE swept by the next): 2.3–3.2 s each way on
  an 18,673-object tree, measured and live-tested on a scratch copy
  (`ARCHITECTURE.md` §5b). Applied only when `sandbox: "workspace-write"` or
  `INCODE_SANDBOX` names it; the permission-mode default still leaves it
  unconfined. Open: whether to make it the default, and a size cap — the grant is
  linear in the tree (a `node_modules`-heavy workspace pays per object, once per
  session). Not yet driven end to end through a real model session.
- **`session/new` — 2.7 s, from 11–14 s.** The client's MCP servers connect in
  the background (42 s → 14 s, earlier). Now the memory engine's `session-start`
  (8.2 s) runs on a thread and the first turn collects it before `user-prompt`;
  the spec note and the guards request (each up to the 1.2 s deadline) overlap the
  agent catalogue; and the fleet reuses the catalogue's probe instead of probing
  again (0.7 s). Measured live under `--acp`: 14.1 / 11.1 / 12.8 s before, 5.9 /
  2.7 / 2.7 s after (`ARCHITECTURE.md` §4). The guards request that "failed" was
  a timeout against the memory service's first-request latency (0.2–49 s after an
  idle gap, seen on other routes too), not a wrong route; its trace now names the
  cause. Open: the first turn still pays the 8.2 s (plus `user-prompt`'s own
  8.2 s) — the engine's latency, measured here, not incode's; `rules` and the
  auditor still wait in line when the service stalls (2.8 s in one run); `--task`
  still waits for `session-start` in line; the REPL was not timed live.
- **The `Bash` tool's shell on Windows.** Fixed: it was cmd.exe; it is now Git Bash
  (or `INCODE_SHELL` / `admin.json` `shell`), else PowerShell, and its description
  names the shell (`ARCHITECTURE.md`, "Which shell"). A foreground command's stdin
  is now the null device, so one that waits for input no longer holds the REPL's
  terminal until its timeout. Open: under the Windows sandbox
  token Git Bash cannot start (msys maps a per-user shared section the token may not
  write), so a read-only session — and a named workspace-write one — runs PowerShell.
  Running msys under the token would mean granting that section to the session SID.
  The operator's own hooks and `verify_command` still run through cmd.exe: they are
  the operator's commands, written for it.
- **Network isolation — partial.** `sandbox_network: "deny"` (or
  `INCODE_SANDBOX_NETWORK`) is enforced on macOS (Seatbelt; loopback and Unix
  sockets kept) and on Linux 6.7+ (Landlock ABI v4, TCP only, loopback included);
  reported NOT enforced on older Linux kernels and on Windows (only an
  AppContainer could, and it cannot read the workspace or toolchains ungranted).
  Not verified live: the macOS profile (the Mac was offline) and the Linux
  enforced path (WSL's kernel is 5.15, ABI V1).
- ~~**A full macOS build and test of HEAD is not verified.**~~ DONE 2026-10-01 on
  the owner's Mac: it built, and 7 tests failed. Two were real bugs, fixed —
  RenderPdf hung for its 120 s deadline on macOS because Chrome writes the PDF
  and does not exit (the print now ends when the PDF is whole), and
  `convert::output_path` turned `a\b.pptx` into `a\a\b.pdf` off Windows. Five
  were tests that assumed Windows (a `node` dependency, a backslash literal,
  the Windows command-line limit, an off-by-one in a Unix loop, CRLF bytes).
  Now 1814 lib + 46 bin pass there, and the macOS-only build warnings are gone.
- **External tools — done; the owner's own release is not published yet.** A
  program incode runs is an `admin.json` `tools` entry, downloaded from its
  release only when asked (`/tools add|update|remove`, `incode --install-tools`),
  checked against GitHub's sha256 digest or a pinned one (neither: refused),
  unpacked with path, link, entry and size guards, and started as an MCP server in
  every mode; `provides: "browser"` makes it the browser Screenshot, RenderPdf and
  `render_js` use, and `PATH` is no longer searched for a browser
  (`ARCHITECTURE.md` §5b, "External tools"). Live-tested against upstream
  `h4ckf0r0day/obscura` v0.2.3 in a scratch data dir: installed and checked; its
  37 MCP tools listed; `browser_navigate` + `browser_evaluate` on a loopback page
  returned its title through the gate (pre-tool hook, reach run); a built-in
  Screenshot ran on the installed binary; a byte-flipped copy was refused. Open:
  `incordai/incord-browser` has no published release, so the owner's entry cannot be
  installed until one exists; a tool installed mid-session is offered from the next
  session (a session's servers connect once); the macOS/Linux tar.gz path is
  unit-tested only, not run live.
- **incord-browser: what is still open upstream.** Re-measured 2026-09-27 on
  078786a against headless Edge; incode now uses the fixes (RenderPdf prefers
  incord-browser when its probe finds text, and passes `preferCSSPageSize` when
  it is honoured). Fixed: #6, #7, #9, #11, #12, #14. Why the code probes the
  print rather than trusting a version (history the code comments no longer
  carry): a build from before incordai/incord-browser#11 was fixed (measured
  on 971d3e8) prints pictures of the page with no text layer, and refuses
  `preferCSSPageSize` with `-32601`; a build with the fix prints a text layer,
  laid out again for the paper as a system browser does (the same two Letter
  pages as Edge for one article, 1864 characters extracted where Edge gives
  1884). The image-only note a RenderPdf result carries is for the first kind.
  Still open:
  - #8, part: an inline SVG is centred now, but SVG `<text font-family="Arial"
    font-size="24">` still draws in the inherited serif at a smaller size, with
    or without `--font-dir`.
  - #13, part: `--dump markdown` tables are valid GFM, but a `caption-side:
    bottom` set in a stylesheet (not inline) still puts the caption above.
  - #12, second half: `--font-dir` still loads every face up front.
  - #11, remainder: the PDF is a raster with a text layer, not vector (about
    2.5x Edge's size for the same article).
  - Not filed yet: with `--font-dir C:\Windows\Fonts`, `font-family: Arial;
    font-weight: 700` draws in a condensed face (Arial Narrow–like). Seen on
    971d3e8 too, so not a regression.
- **App-side work (the owner's desktop app, outside this repo):** a resume list
  (it calls neither `session/list` nor `session/load`); drawing image content;
  drawing the `_meta.incode` badges; a question card for `AskUserQuestion` (the app
  answers every agent request but `session/request_permission` with `-32601`);
  withdrawing a permission card when the engine answers first (no agent-side
  cancel).
- **`/undo` after a restart has not been run live.** Unit-tested against the
  on-disk checkpoints only.
- **codex pictures have not been run live.** `--image` is taken from `codex exec
  --help`; every codex turn here answered with the usage limit.
- **ConvertDocument** has run only against a fake `soffice`: LibreOffice is not
  installed on the development machine.
- **Thinking** arrives only on the claude route; the API route does not ask for
  it and codex's reasoning items were not measured.

---

## 1. Project folders and planning — 2026-09-27

Adapted, in incode's own terms, from a cross-check against mattpocock/skills
(MIT). Done/not-done stays one chain — PRD section → roadmap id → ledger tasks →
`VERIFICATION.md` goal check — and no second tracker was added.

### 1a. Project folders — DONE

- **`.incode/.gitignore`** (`folders.rs`): written when any writer creates
  `.incode/`, healed at session start, only ever appended to. Ignores the
  machine-local state, keeps `contracts.json`, `guards.json`, `agents/*.md`
  tracked. This repository's own `.gitignore` stopped ignoring all of `.incode/`
  for the same reason, and commits the `.incode/.gitignore`.
- **Task transcripts** moved from `.incode/agents/` (the owner's sub-agent
  definitions) to `.incode/transcripts/tasks/`; old ones are migrated at start.
- **`## Layout` required in `docs/ARCHITECTURE.md`**, checked by the same
  `spec::has_section` the PRD's non-goals go through: the stack's official
  generator with a verified version, and the folder tree.
- **The scaffold is roadmap item 1** on a project with no code (`scaffold.rs`):
  `s1 Create the layout`, proven by the stack's build or test command in
  `VERIFICATION.md`, which the spec gate requires. No templates; the owner may pin
  a generator per stack (`scaffold` in `admin.json`).
- **`RUNNER.md`** no longer describes a `journal.jsonl` or a `runner.lock`; neither
  was built.

### 1b. Planning and intake — DONE

- **Planner** (`plan.rs`): thin vertical slices, prefactoring first, expand →
  migrate in batches → contract; `[after N]` and the twelve-step cap unchanged.
- **Tests first, within the guards**: for a behaviour change the test is its own
  first step, written pending so the project's check stays green (a step is
  verified by that check, and a red test would fail it); the change is `[after]`
  it and turns the test on. Both steps touch a protected path and need the owner.
  No TDD role; the guard is unchanged.
- **Intake questions carry a recommended answer** (`(recommended)` or
  `recommended: true`), still one batch of at most seven; Enter takes it, and
  unanswered it stands as an assumption.
- **`## Language` required in `ARCHITECTURE.md`**, checked like `## Layout`. No ADR
  files: decisions stay in `ASSUMPTIONS.md` and memory.
- **Two reviewers** (`standards-reviewer`, `spec-reviewer`) ship as files and are
  seeded into `~/.incode/agents/`, findings only.

### 1c. Still open

- ~~**On incode's own toolbox a test write is refused, not put to the owner.**~~
  DONE 2026-09-27, the owner's decision: a protected write is put to a person
  (`Toolbox::ask_protected`) — the one at the session, else during `/run` the
  owner's channel (`owner::OwnerPrompter`, set from `run_ledger` when
  `plugins.owner` is on), under the sentinel's card title
  (`guards::breach_title`), so an allow given before the write is the answer the
  sentinel finds after it. Nobody to ask, or a read-only session: refused as
  before. An allow passes the protected path only; the intake gate still stands.
  The file allowed is recorded (`toolbox::AllowedWrites`, shared with sub-agents)
  so the interactive turn's sentinel does not then report it as a breach — found
  in the live test of the installed build, fixed and re-tested live.
- **A red test step is not provable on its own.** The runner verifies every code
  step with the project's check, so tests-first relies on the test being written
  pending. A step whose check is "this new test fails for the right reason" would
  need a verification the `Verified` law does not allow today.

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
| `parallel::why_not` | `parallel::shortfall_note`, printed by the runner | `INCODE_LANES=4` on a non-repository gave a sequential run indistinguishable from having nothing to parallelise |
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

`run_on_compact` is settled (`b39a99e`): allowlisted in `DELIBERATELY_UNWIRED`,
because incode never compacts — the model CLI compacts inside its own process.

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

Both machines now exist: the owner's Mac (macOS 26.6.2, arm64, over ssh) and
WSL2 Ubuntu 24.04 on the development PC.

**macOS — built 2026-10-01** (`src/desktop/macos.rs`), compiled and its whole
suite run on the Mac (1814 lib + 46 bin passed, 0 failed):
- monitors and capture through xcap, in POINTS (`dpi::Awareness::Points`);
- **a Retina bug fixed on the way** — `capture` took the region's size from
  the image (pixels) and its origin from the display (points), so every click
  landed at twice its distance from the corner; `capture::desktop_view` now
  takes the region in desktop units and only the model size from the image,
  and refuses a capture whose shape is not the display's;
- the accessibility tree through AXUIElement (direct framework calls, no new
  package), sharing the walk with Windows (`uia::walk`), 1 s per request;
- the clipboard through `pbcopy` (its test puts the owner's clipboard back);
- the Accessibility and Screen Recording grants probed before each action and
  reported by `screen_info` (`missing_grants`), refusing by name rather than
  letting macOS drop the action silently.

**Still open on macOS:** the live drive of a real window — a person grants
Accessibility and Screen Recording once (System Settings › Privacy & Security),
then `screen_info`, `screenshot`, `ui_tree` and a click are run against it.
Isolation stays a VM (`COMPUTER_USE.md` 7e).

**Linux on X11 — built 2026-10-01** (`src/desktop/linux.rs`), tested in Docker
(Debian 13, Rust 1.97) instead of the WSL machine, whose apt is broken:
- the display is NAMED in admin.json `desktop_display`: `":N"` attaches,
  `"xvfb"`/`"xvfb:WxH"` starts a private Xvfb (`-displayfd`, so no race for a
  number; `PR_SET_PDEATHSIG`, so it dies with the server however that ends);
  unset is a refusal — never `$DISPLAY`, the operator's own desktop (7e);
- input through enigo's x11rb backend (the 3 packages above, now added);
  clipboard through `xclip` or `xsel`, refusing by name when neither is there;
- the AT-SPI2 tree over zbus (already in the lock), filtered to applications
  whose process `DISPLAY` is this server's, the active window or the one
  showing; GTK4's SENSITIVE-without-ENABLED read as enabled;
- **GTK4 trees are refused, measured not assumed:** GTK 4.18.6 reports every
  control at (0,0) in screen AND parent coordinates (GTK 3.24.49 reports true
  positions), so `ui_tree` names the toolkit and points at screenshot + click.

Live, against the real `incode --desktop-mcp` (8/8): a GTK3 window walked at
its true position, `click_element` moved focus off a button into the entry,
`type_text` + `paste_text` + `key enter` made it return `hello world`, a
screenshot came back, GTK4 was refused by name yet typed into, and the private
Xvfb was gone after the server ended. Whole lib suite on Linux: 1821 passed;
the 2 `file_ops` failures were the read-only source mount (they write into the
crate directory), and pass on a writable copy.

**Still open on Linux:** the pixel viewer (x11vnc + noVNC, 14b); Qt and
Chromium trees not yet measured.

### B2 — The codex MCP tool call

Registration syntax is proven — the TOML escaping was verified by an accidental
before/after control. The tool *call* has never run: blocked by a provider usage
limit. One live call is all that is missing.

### B3 — The viewer's stop button — DONE 2026-09-23

The owner decided: reads, plus two writes behind a per-run token
(`.incode/serve.token`, `Authorization: Bearer`). `POST /v1/run/stop` writes
`.incode/run.stop`; a running `/run` watches for it and trips the same cancel
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
