// src/extension.ts activated against the fake `vscode` module and the fake
// agent: the commands package.json contributes are the ones registered,
// `incode.sendSelection` honours `incode.sendSelectionImmediately`, and
// `incode.openSession` offers the agent's kept sessions. Messages are read
// where the chat view (src/chatView.ts) would receive them.
import { strict as assert } from "node:assert";
import { promises as fs, readFileSync } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { after, before, describe, it } from "node:test";
import type * as ExtensionModule from "../src/extension";
import type { ToView } from "../src/controller";
import { FakeState, Uri, installFakeVscode, makeFakeVscode, runJsAgentsWithNode } from "./fakeVscode";

const ROOT = path.resolve(__dirname, "..", "..");
const FAKE = path.join(ROOT, "test", "fixtures", "fake-agent.js");
const PACKAGE = JSON.parse(readFileSync(path.join(ROOT, "package.json"), "utf8")) as {
  contributes: { commands: { command: string }[]; configuration: { properties: Record<string, { default: unknown }> } };
};

const state: FakeState = { roots: [], config: { path: FAKE, args: [] }, documents: [], applied: [], opened: [] };
installFakeVscode(makeFakeVscode(state));
runJsAgentsWithNode();
// eslint-disable-next-line @typescript-eslint/no-require-imports
const extension = require("../src/extension") as typeof ExtensionModule;

describe("the extension (src/extension.ts)", () => {
  let ws: string;
  const subscriptions: { dispose(): unknown }[] = [];
  const posted: ToView[] = [];
  const said = () =>
    posted
      .filter((m): m is Extract<ToView, { type: "agentText" }> => m.type === "agentText")
      .map((m) => m.text)
      .join("");
  const run = (id: string) => state.commands!.get(id)!();
  async function until(pred: () => boolean, what: string): Promise<void> {
    const deadline = Date.now() + 5000;
    while (!pred()) {
      if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}; got ${JSON.stringify(posted.map((m) => m.type))}`);
      await new Promise((r) => setTimeout(r, 5));
    }
  }

  before(async () => {
    ws = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(), "incode-vscode-ext-")));
    state.roots = [ws];
    extension.activate({ extension: { packageJSON: { version: "0.0.0-test" } }, extensionUri: Uri.file(ROOT), subscriptions } as never);
    // Open the chat view the way VS Code does, and let its page say it is ready.
    let fromPage: (m: unknown) => void = () => undefined;
    const provider = state.views!.get("incode.chat") as { resolveWebviewView(v: unknown): void };
    provider.resolveWebviewView({
      webview: {
        options: {},
        html: "",
        cspSource: "vscode-webview-src",
        asWebviewUri: (u: Uri) => u,
        postMessage: async (m: ToView) => posted.push(m),
        onDidReceiveMessage: (fn: (m: unknown) => void) => ((fromPage = fn), { dispose: () => undefined }),
      },
      onDidDispose: () => ({ dispose: () => undefined }),
      show: () => undefined,
    });
    fromPage({ type: "ready" });
    await until(() => posted.some((m) => m.type === "commands"), "the session's slash commands");
  });
  after(async () => {
    for (const s of subscriptions) s.dispose();
    await new Promise((r) => setTimeout(r, 100));
    await fs.rm(ws, { recursive: true, force: true });
  });

  it("registers exactly the commands package.json contributes, and the new setting defaults to off", () => {
    assert.deepEqual([...state.commands!.keys()].sort(), PACKAGE.contributes.commands.map((c) => c.command).sort());
    assert.equal(PACKAGE.contributes.configuration.properties["incode.sendSelectionImmediately"].default, false);
  });

  it("Send Selection fills the input, or with incode.sendSelectionImmediately sends at once", async () => {
    const doc = {
      uri: Uri.file(path.join(ws, "src", "a.ts")),
      languageId: "typescript",
      lineAt: (n: number) => ({ range: { start: { line: n, character: 0 }, end: { line: n, character: 10 } } }),
      getText: () => "let x = 1;",
    };
    state.activeTextEditor = { document: doc, selection: { isEmpty: true, active: { line: 2 } } };
    const formatted = "src/a.ts:3\n```typescript\nlet x = 1;\n```\n";

    posted.length = 0;
    await run("incode.sendSelection");
    assert.deepEqual(posted, [{ type: "prefill", text: formatted }]);

    posted.length = 0;
    state.config.sendSelectionImmediately = true;
    try {
      await run("incode.sendSelection");
    } finally {
      delete state.config.sendSelectionImmediately;
    }
    assert.ok(posted.some((m) => m.type === "user" && m.text === formatted.trim()));
    assert.equal(said(), `echo: ${formatted.trim()}`);
  });

  it("Open Session offers the kept sessions and loads the one picked", async () => {
    state.quickPicks = [];
    state.pick = (items) => (items as { sessionId: string }[]).find((i) => i.sessionId === "kept-2");
    posted.length = 0;
    await run("incode.openSession");
    assert.deepEqual((state.quickPicks[0] as { label: string }[]).map((i) => i.label), ["fix the parser", "write the docs"]);
    assert.ok(posted.some((m) => m.type === "user" && m.text === "write the docs"), "the replayed history reaches the view");
    assert.equal(state.contexts?.["incode.canReopenSession"], true);
  });
});
