// The controller and the fs bridge, end to end against the fake agent, with
// a fake `vscode` module. Checks what the chat view is sent.
import { strict as assert } from "node:assert";
import { promises as fs } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { after, before, describe, it } from "node:test";
import type * as ControllerModule from "../src/controller";
import { FakeDocument, FakeState, installFakeVscode, makeFakeVscode } from "./fakeVscode";

const FAKE = path.resolve(__dirname, "..", "..", "test", "fixtures", "fake-agent.js");

const state: FakeState = { roots: [], config: { path: FAKE, args: [] }, documents: [], applied: [], opened: [] };
installFakeVscode(makeFakeVscode(state));
// eslint-disable-next-line @typescript-eslint/no-require-imports
const { Controller } = require("../src/controller") as typeof ControllerModule;

type ToView = ControllerModule.ToView;

describe("Controller with the fake agent", { skip: process.platform === "win32" ? "runs the fixture by its shebang" : false }, () => {
  let ws: string;
  let controller: InstanceType<typeof Controller>;
  const posted: ToView[] = [];
  const logged: string[] = [];

  async function until(pred: () => boolean, what: string): Promise<void> {
    const deadline = Date.now() + 5000;
    while (!pred()) {
      if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}; got ${JSON.stringify(posted.map((m) => m.type))}`);
      await new Promise((r) => setTimeout(r, 5));
    }
  }
  const said = () =>
    posted
      .filter((m): m is Extract<ToView, { type: "agentText" }> => m.type === "agentText")
      .map((m) => m.text)
      .join("");

  before(async () => {
    ws = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(), "incode-vscode-ctl-")));
    state.roots = [ws];
    const output = { append: (s: string) => logged.push(s), appendLine: (s: string) => logged.push(s + "\n"), show: () => undefined };
    controller = new Controller(output as never, "0.0.0-test");
    controller.attach({ post: (m) => posted.push(m), reveal: async () => undefined });
  });
  after(async () => {
    controller.dispose();
    await new Promise((r) => setTimeout(r, 100));
    await fs.rm(ws, { recursive: true, force: true });
  });

  it("opens a session when the view is ready, with modes, config and slash commands", async () => {
    await controller.onMessage({ type: "ready" });
    await until(() => posted.some((m) => m.type === "commands"), "the slash commands");
    const modes = posted.find((m) => m.type === "modes") as Extract<ToView, { type: "modes" }>;
    assert.equal(modes.modes?.currentModeId, "workspace-write");
    const config = posted.find((m) => m.type === "config") as Extract<ToView, { type: "config" }>;
    assert.equal(config.configOptions[0].id, "agent");
    const commands = posted.find((m) => m.type === "commands") as Extract<ToView, { type: "commands" }>;
    assert.deepEqual(commands.commands.map((c) => c.name), ["undo"]);
    assert.match(logged.join(""), /fake agent ready/, "stderr goes to the output channel");
  });

  it("streams a turn: text, the merged tool call, plan, usage, turn end", async () => {
    posted.length = 0;
    await controller.onMessage({ type: "send", text: "stream" });
    assert.equal(said(), "Hello, world");
    const tools = posted.filter((m) => m.type === "tool") as Extract<ToView, { type: "tool" }>[];
    assert.equal(tools.length, 2);
    assert.equal(tools[1].call.title, "Read file", "the update keeps the title");
    assert.equal(tools[1].call.status, "completed");
    assert.ok(posted.some((m) => m.type === "plan"));
    assert.ok(posted.some((m) => m.type === "usage" && m.used === 1000 && m.size === 200000));
    assert.deepEqual(posted.at(-1), { type: "turn", running: false, stopReason: "end_turn" });
  });

  it("a permission question is answered from the view", async () => {
    posted.length = 0;
    const turn = controller.onMessage({ type: "send", text: "permission" });
    await until(() => posted.some((m) => m.type === "permission"), "the permission card");
    const card = posted.find((m) => m.type === "permission") as Extract<ToView, { type: "permission" }>;
    assert.equal(card.title, "Run `rm -rf build`");
    assert.deepEqual(card.options.map((o) => o.optionId), ["allow", "always", "reject"]);
    await controller.onMessage({ type: "permission", key: card.key, optionId: "reject" });
    await turn;
    assert.equal(said(), "chose reject");
    assert.ok(posted.some((m) => m.type === "permissionDone" && m.key === card.key && m.chosen === "reject"));
  });

  it("Stop cancels the turn and takes down an open permission card", async () => {
    posted.length = 0;
    const turn = controller.onMessage({ type: "send", text: "permission-hang" });
    await until(() => posted.some((m) => m.type === "permission"), "the permission card");
    await controller.onMessage({ type: "stop" });
    await turn;
    assert.ok(posted.some((m) => m.type === "permissionDone" && m.chosen === undefined));
    assert.deepEqual(posted.at(-1), { type: "turn", running: false, stopReason: "cancelled" });
  });

  it("fs/read_text_file returns the unsaved buffer of an open file", async () => {
    const file = path.join(ws, "open.txt");
    await fs.writeFile(file, "on disk\n");
    state.documents.push(new FakeDocument(file, "in the editor\n"));
    posted.length = 0;
    await controller.onMessage({ type: "send", text: `read ${file}` });
    assert.equal(said(), "content: in the editor\n");
    const other = path.join(ws, "closed.txt");
    await fs.writeFile(other, "from disk\n");
    posted.length = 0;
    await controller.onMessage({ type: "send", text: `read ${other}` });
    assert.equal(said(), "content: from disk\n");
    posted.length = 0;
    await controller.onMessage({ type: "send", text: `read ${path.join(ws, "missing.txt")}` });
    assert.match(said(), /^error -32002/);
  });

  it("fs/write_text_file goes through a WorkspaceEdit and is saved", async () => {
    const file = path.join(ws, "sub", "new.txt");
    posted.length = 0;
    state.applied.length = 0;
    await controller.onMessage({ type: "send", text: `write ${file} hello there` });
    assert.equal(said(), "wrote {}");
    assert.equal(state.applied.length, 1);
    assert.deepEqual(state.applied[0].ops.map((o) => o.kind), ["create", "insert"]);
    assert.equal(await fs.readFile(file, "utf8"), "hello there");

    posted.length = 0;
    await controller.onMessage({ type: "send", text: `write ${file} replaced` });
    assert.deepEqual(state.applied[1].ops.map((o) => o.kind), ["replace"]);
    assert.equal(await fs.readFile(file, "utf8"), "replaced");
  });

  it("fs requests outside the workspace are refused, and nothing is written", async () => {
    const outside = path.join(path.dirname(ws), `outside-${path.basename(ws)}.txt`);
    posted.length = 0;
    state.applied.length = 0;
    await controller.onMessage({ type: "send", text: `write ${outside} nope` });
    assert.match(said(), /^error -32602: refused: .*outside the workspace folders/);
    assert.equal(state.applied.length, 0);
    await assert.rejects(fs.stat(outside));
  });

  it("set_config_option sends the agent's new options to the view; links open only for http(s)", async () => {
    posted.length = 0;
    await controller.onMessage({ type: "setConfig", configId: "agent", value: "codex" });
    const config = posted.find((m) => m.type === "config") as Extract<ToView, { type: "config" }>;
    assert.equal(config.configOptions[0].currentValue, "codex");
    await controller.onMessage({ type: "openLink", url: "https://example.com/x" });
    await controller.onMessage({ type: "openLink", url: "command:workbench.action.quit" });
    await controller.onMessage({ type: "openLink", url: "file:///etc/passwd" });
    assert.deepEqual(state.opened, ["https://example.com/x"]);
  });

  it("the agent exiting is shown, and restart brings a new session", async () => {
    posted.length = 0;
    await controller.onMessage({ type: "send", text: "exit" });
    await until(() => posted.some((m) => m.type === "status" && m.state === "stopped"), "the stopped status");
    posted.length = 0;
    await controller.restart();
    await until(() => posted.some((m) => m.type === "commands"), "the new session's commands");
    posted.length = 0;
    await controller.onMessage({ type: "send", text: "hi" });
    assert.equal(said(), "echo: hi");
  });

  it("a view opened again is replayed what it missed", async () => {
    posted.length = 0;
    await controller.onMessage({ type: "ready" });
    assert.equal(posted[0].type, "reset");
    assert.ok(posted.some((m) => m.type === "user" && m.text === "hi"));
    assert.ok(posted.some((m) => m.type === "agentText" && m.text === "echo: hi"));
  });
});
