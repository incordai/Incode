# incode for VS Code

A chat view for [incode](../../README.md) inside VS Code. The extension starts
`incode --acp` and talks to it over the Agent Client Protocol (ACP):
newline-delimited JSON-RPC 2.0 on the agent's stdin and stdout. incode does the
work; the extension is the client.

## What it does

- **Chat view** in its own activity-bar container (the `</>` icon). Streams the
  agent's text (a small markdown subset, rendered as text nodes; model output
  is never inserted as HTML, and the page runs under a strict
  Content-Security-Policy: everything `'none'` but `media/`, the two
  nonce'd scripts, and `data:` images for the attachment thumbnails), its thinking, each tool call with its status,
  output and diffs, and the plan.
- **Slash commands**: type `/` for completion from the commands incode offers
  (`available_commands_update`). Tab or Enter picks one.
- **Permission prompts** appear as buttons with the options the agent offers
  (allow once / allow always / reject).
- **Pickers** for the session's mode (`read-only`, `workspace-write`,
  `danger-full-access`) and its config options (incode offers the agent and
  the role).
- **Stop** cancels the running turn (`session/cancel`); any open permission
  question is answered `cancelled`.
- **Usage**: the context window's fill (`usage_update`), top right.
- **Reopening a session**: `incode: Open Session…` (also a button in the chat
  view's title bar) asks the agent for the sessions kept in the workspace
  (`session/list`, following `nextCursor`), shows them in a QuickPick with
  their title and when they were last updated, and opens the one you pick
  (`session/load`). The current session is closed first, the view is
  cleared, and the agent's replay of the history (`user_message_chunk` /
  `agent_message_chunk`, sent before it answers) is shown as it arrives. Only
  offered when the agent advertises both `loadSession: true` and
  `sessionCapabilities.list`; otherwise the button is hidden and the command
  says so. `incode: New Session` still starts a fresh one.
- **Pictures in a prompt**: paste or drop a picture into the input, or use the
  **Image** button (the editor's file dialog). Each shows as a thumbnail with
  a × to remove it, and is sent with the text as an ACP `image` block
  (base64 `data` and `mimeType`). Only when the agent advertises
  `promptCapabilities.image`; a prompt needs text beside its pictures. Only
  PNG and JPEG, recognised from the file's own bytes, since those are what
  incode accepts. Bounds, from incode's own: at most 3 750 000 bytes per
  picture (`MAX_IMAGE_BYTES`; its base64 is 5 000 000 characters, where model
  endpoints commonly stop taking an image), at most 4 per prompt, and 5 MiB
  together, because a prompt is one JSON-RPC line and incode drops a line
  over 8 MiB (base64 makes 5 MiB about 6.7 MiB, leaving room for the text).
  The page refuses a file before reading it; the extension checks everything
  again (`src/images.ts`). Sent pictures are named in the log, not kept: the
  bytes never enter the view's replay.
- **Files through the editor**: the extension advertises `fs.readTextFile` and
  `fs.writeTextFile`. A read returns the open editor buffer, unsaved changes
  included, when the file is open. A write is applied as a `WorkspaceEdit`
  (visible, undoable with Ctrl+Z) and then saved. Both are refused for any
  path outside the workspace folders, with symlinks resolved. (incode 0.1.0
  edits files with its own tools and does not send these requests yet; the
  support is there for when it does.)
- The agent's stderr goes to the **incode** output channel.

### Commands

| Command | |
| --- | --- |
| `incode: Open Chat` (`incode.openChat`) | Show the chat view. The agent starts when the view first opens. |
| `incode: New Session` (`incode.newSession`) | Close the current session and start a fresh one on the same process. |
| `incode: Open Session…` (`incode.openSession`) | Pick one of the workspace's kept sessions and open it again, with its history. Needs an agent that offers `session/list` and `session/load`. |
| `incode: Send Selection to Chat` (`incode.sendSelection`) | Put the selection (or the current line) into the chat input as `path:start-end` and a fenced block, so you can add your question. With `incode.sendSelectionImmediately` it is sent at once instead. Also in the editor context menu. |
| `incode: Restart Agent` (`incode.restart`) | Stop the process and start it again. Use after changing settings or when it exits. |

### Settings

| Setting | Default | |
| --- | --- | --- |
| `incode.path` | `incode` | The binary. A bare name is looked up on `PATH`. |
| `incode.args` | `[]` | Extra arguments after `--acp`, e.g. `["-p", "read-only"]` or `["-m", "<model>"]`. |
| `incode.sendSelectionImmediately` | `false` | `Send Selection to Chat` sends the selection as a prompt at once instead of filling the input. While a turn is running it still goes to the input. Read each time, no restart needed. |

The process runs with the first workspace folder as its working directory, and
the session is opened on that folder (`session/new` `cwd`). A window with no
folder open cannot start a session.

## Build

Requires Node.js 22 or newer (the test script uses `node --test` with a glob).

```sh
cd editors/vscode
npm install        # dev dependencies only: typescript, @types/node, @types/vscode
npm run compile    # tsc, strict, into out/
```

The extension has no runtime dependencies beyond the VS Code API.

## Test

```sh
npm test
```

compiles, then runs the unit tests with `node:test`, offline:

- `test/acp.test.ts`: the ACP client (`src/acp.ts`) against a fake agent,
  `test/fixtures/fake-agent.js`, a small Node script speaking the protocol on
  stdio: initialize, session/new, a streamed prompt, set_mode and
  set_config_option, the permission round trip, `fs/*` requests including
  out-of-workspace refusal, cancel (also of an open permission question),
  malformed lines, the agent exiting mid-turn, a missing binary,
  `session/list` paging, `session/load` replaying before it answers, image
  blocks. It also covers the JSON-RPC layer over in-memory streams (framing,
  over-long lines, errors). The fake agent drops `loadSession`/`list` with
  `--no-reopen` and `promptCapabilities.image` with `--no-image`.
- `test/controller.test.ts`: the controller and the fs bridge with a minimal
  fake `vscode` module (`test/fakeVscode.ts`) against the same fake agent:
  what the chat view is sent for a turn, permission cards answered and
  cancelled, reads returning unsaved buffers, writes going through a
  `WorkspaceEdit`, refusals outside the workspace, link opening limited to
  http(s), restart after the agent exits, replay into a reopened view,
  Open Session (every page listed, the history shown, the old session closed,
  an unknown session's error), pictures sent and refused (type, spoofed
  type, size, count; the file dialog), Send Selection with and without
  `incode.sendSelectionImmediately`, and an agent without `loadSession` or
  `image`. The fixture is started with node (`runJsAgentsWithNode`), so this
  runs on Windows too.
- `test/extension.test.ts`: `activate` against the fake module: the commands
  registered are exactly the ones `package.json` contributes; Send Selection
  and Open Session end to end.
- `test/chatPage.test.ts`: `media/chat.js` in a small fake DOM built from the
  shell `src/chatView.ts` serves: pictures pasted, dropped and picked, the
  thumbnail and its remove control, refusals before a file is read, what a
  send carries; and the CSP.
- `test/images.test.ts`: the picture rules (`src/images.ts`) at each bound.
- `test/paths.test.ts`: the workspace path check as a pure function (POSIX
  and Windows rules, `..`, prefix tricks, symlinks), line slicing for
  `fs/read_text_file`, and the selection format.
- `test/markdown.test.ts`: the markdown-lite renderer never produces HTML from
  model text and only links `http(s)` URLs.
- `test/real-incode.test.ts`: against the real binary: initialize and
  session/new; the reopen and image capabilities; `session/list` and
  `session/load` of a transcript the test writes into
  `.incode/transcripts`; incode decoding an `image` block (a GIF is refused
  by its type). No prompt reaches a model. Skipped unless `INCODE_BIN` is set:

  ```sh
  cargo build --release                               # at the repo root
  INCODE_BIN="$PWD/../../target/release/incode" npm test
  ```

To try the extension in a development host: open `editors/vscode` in VS Code,
run `npm run compile`, and press F5 ("Run Extension"), or run
`code --extensionDevelopmentPath="$PWD"`.

## Package and install

```sh
npm run package    # runs `npx @vscode/vsce package`; needs network access the first time
code --install-extension incode-0.1.0.vsix
```

or in VS Code: Extensions view, `...` menu, **Install from VSIX...**.

`vsce` is fetched by `npx` rather than listed as a dev dependency, to keep
`npm install` small; `npm test` does not need it.

## Layout

| Path | |
| --- | --- |
| `src/acp.ts` | The ACP client, independent of VS Code: framing, JSON-RPC correlation, notifications, agent→client requests (permission, fs) through callbacks, cancel, spawning. |
| `src/paths.ts` | The workspace path rule and line slicing. Pure. |
| `src/selection.ts` | The text `sendSelection` produces. Pure. |
| `src/images.ts` | Which pictures a prompt may carry, and the bounds. Pure. |
| `src/fsBridge.ts` | `fs/read_text_file` and `fs/write_text_file` on top of the editor. |
| `src/controller.ts` | One agent process and session per window; between the agent and the view. |
| `src/chatView.ts` | The webview, its CSP and HTML shell. |
| `media/chat.js`, `media/markdown.js`, `media/chat.css` | The webview's script and styles. |
