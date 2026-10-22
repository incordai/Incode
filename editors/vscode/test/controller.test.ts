// The controller (src/controller.ts), the fs bridge and the picture checks
// (src/images.ts) end to end against the fake agent, with a fake `vscode`
// module. Checks what the chat view is sent.
import { strict as assert } from "node:assert";
import { promises as fs } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { after, before, describe, it } from "node:test";
import type * as ControllerModule from "../src/controller";
import { FakeDocument, FakeState, installFakeVscode, makeFakeVscode, runJsAgentsWithNode } from "./fakeVscode";

const FAKE = path.resolve(__dirname, "..", "..", "test", "fixtures", "fake-agent.js");
/** A 1x1 PNG. */
const PNG_BASE64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

const state: FakeState = { roots: [], config: { path: FAKE, args: [] }, documents: [], applied: [], opened: [] };
installFakeVscode(makeFakeVscode(state));
runJsAgentsWithNode();
// eslint-disable-next-line @typescript-eslint/no-require-imports
const { Controller } = require("../src/controller") as typeof ControllerModule;

type ToView = ControllerModule.ToView;

function harness(posted: ToView[]) {
  return {
    async until(pred: () => boolean, what: string): Promise<void> {
      const deadline = Date.now() + 5000;
      while (!pred()) {
        if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}; got ${JSON.stringify(posted.map((m) => m.type))}`);
        await new Promise((r) => setTimeout(r, 5));
      }
    },
    said: () =>
      posted
        .filter((m): m is Extract<ToView, { type: "agentText" }> => m.type === "agentText")
        .map((m) => m.text)
        .join(""),
    notices: () =>
      posted.filter((m): m is Extract<ToView, { type: "notice" }> => m.type === "notice").map((m) => `${m.level}: ${m.text}`),
  };
}

describe("Controller with the fake agent", () => {
  let ws: string;
  let controller: InstanceType<typeof Controller>;
  const posted: ToView[] = [];
  const logged: string[] = [];
  const { until, said, notices } = harness(posted);

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

  it("the agent's capabilities reach the view (also on replay) and the title-bar context key", async () => {
    posted.length = 0;
    await controller.onMessage({ type: "ready" });
    assert.deepEqual(posted[1], { type: "capabilities", images: true, reopen: true });
    assert.equal(state.contexts?.["incode.canReopenSession"], true);
  });

  it("Open Session lists every page of the workspace's sessions; the pick is loaded and its history shown", async () => {
    posted.length = 0;
    state.quickPicks = [];
    state.pick = (items) => (items as { sessionId: string }[]).find((i) => i.sessionId === "kept-1");
    await controller.pickSession();
    const offered = state.quickPicks[0] as { label: string; detail: string }[];
    assert.deepEqual(
      offered.map((i) => [i.label, i.detail]),
      [
        ["fix the parser", "kept-1"],
        ["write the docs", "kept-2"],
      ],
      "both pages of session/list",
    );
    const kinds = posted.map((m) => m.type);
    assert.equal(kinds[0], "reset", "the old session's messages are cleared first");
    const user = kinds.indexOf("user");
    const agentText = kinds.indexOf("agentText");
    assert.ok(user > 0 && agentText > user, `the replay is shown in order: ${JSON.stringify(kinds)}`);
    assert.ok(user < kinds.indexOf("modes"), "the replay arrives before the answer and is not dropped");
    assert.deepEqual(posted[user], { type: "user", text: "fix the parser" });
    assert.equal(said(), "Fixed.");
    const config = posted.find((m) => m.type === "config") as Extract<ToView, { type: "config" }>;
    assert.equal(config.configOptions[0].currentValue, "codex", "the loaded session's own options");
    await until(() => posted.some((m) => m.type === "commands"), "the loaded session's commands");
    assert.ok(posted.some((m) => m.type === "status" && m.state === "ready"));
    assert.ok(!logged.join("").includes("[close]"), "the old session was closed cleanly");

    // A prompt now goes to the loaded session (the fake speaks for kept-1).
    posted.length = 0;
    await controller.onMessage({ type: "send", text: "next" });
    assert.equal(said(), "echo: next");
  });

  it("the session already open is not loaded twice; a session the agent does not keep fails visibly", async () => {
    posted.length = 0;
    await controller.loadSession("kept-1");
    assert.deepEqual(notices(), ["info: That session is already open."]);
    assert.ok(!posted.some((m) => m.type === "reset"));

    // Away and back: the one left must have been closed on the agent, which
    // refuses to load a session that is still open.
    await controller.loadSession("kept-2");
    posted.length = 0;
    await controller.loadSession("kept-1");
    assert.deepEqual(notices(), []);
    assert.ok(posted.some((m) => m.type === "status" && m.state === "ready"), JSON.stringify(posted.filter((m) => m.type === "status")));
    assert.ok(posted.some((m) => m.type === "user" && m.text === "fix the parser"));

    posted.length = 0;
    await controller.loadSession("no-such");
    const status = posted.filter((m) => m.type === "status").at(-1) as Extract<ToView, { type: "status" }>;
    assert.equal(status.state, "error");
    assert.match(status.text, /^Could not load the session: Invalid params: no session `no-such` is kept/);
    // The next prompt opens a fresh session.
    posted.length = 0;
    await controller.onMessage({ type: "send", text: "after" });
    assert.equal(said(), "echo: after");
  });

  it("a prompt with a picture sends an ACP image block beside the text", async () => {
    posted.length = 0;
    await controller.onMessage({ type: "send", text: "images", images: [{ name: "dot.png", mimeType: "image/png", data: PNG_BASE64 }] });
    assert.equal(said(), `1 image(s): image/png ${Buffer.from(PNG_BASE64, "base64").length} bytes`);
    assert.ok(posted.some((m) => m.type === "user" && m.text === "images" && m.images?.[0] === "dot.png"));
    assert.ok(!JSON.stringify(posted).includes(PNG_BASE64), "the bytes are never kept for the view's replay");
  });

  it("a picture that is not PNG/JPEG, lies about its type, or is too large is refused before anything is sent", async () => {
    const big = Buffer.alloc(4_000_000);
    Buffer.from(PNG_BASE64, "base64").copy(big);
    const cases: [unknown, RegExp][] = [
      [[{ name: "notes.txt", mimeType: "text/plain", data: Buffer.from("hello").toString("base64") }], /notes\.txt is text\/plain; only PNG and JPEG/],
      [[{ name: "fake.png", mimeType: "image/png", data: Buffer.from("not a png at all").toString("base64") }], /fake\.png does not hold the image\/png picture/],
      [[{ name: "big.png", mimeType: "image/png", data: big.toString("base64") }], /big\.png is 3\.8 MiB; a picture may be at most 3\.6 MiB/],
      [Array.from({ length: 5 }, () => ({ name: "dot.png", mimeType: "image/png", data: PNG_BASE64 })), /at most 4 pictures/],
    ];
    for (const [images, why] of cases) {
      posted.length = 0;
      await controller.onMessage({ type: "send", text: "images", images });
      assert.equal(notices().length, 1, JSON.stringify(notices()));
      assert.match(notices()[0], /^error: Not sent: /);
      assert.match(notices()[0], why);
      assert.ok(!posted.some((m) => m.type === "user" || m.type === "turn"), "no prompt was sent");
    }
  });

  it("the Image button's file dialog attaches a PNG and refuses a non-image and an oversized file", async () => {
    const png = path.join(ws, "shot.png");
    const txt = path.join(ws, "notes.png");
    const big = path.join(ws, "huge.png");
    await fs.writeFile(png, Buffer.from(PNG_BASE64, "base64"));
    await fs.writeFile(txt, "plain text with a .png name");
    await fs.writeFile(big, Buffer.alloc(4_000_000));
    state.openDialog = [png, txt, big];
    posted.length = 0;
    await controller.onMessage({ type: "attachImage" });
    const attached = posted.filter((m): m is Extract<ToView, { type: "attachment" }> => m.type === "attachment");
    assert.deepEqual(
      attached.map((m) => [m.image.name, m.image.mimeType, m.image.data]),
      [["shot.png", "image/png", PNG_BASE64]],
    );
    assert.deepEqual(notices(), [
      "error: Not attached: notes.png is not a PNG or JPEG picture.",
      "error: Not attached: huge.png is 3.8 MiB; a picture may be at most 3.6 MiB.",
    ]);
  });

  it("Send Selection fills the input by default, and sends at once with incode.sendSelectionImmediately", async () => {
    posted.length = 0;
    delete state.config.sendSelectionImmediately;
    await controller.sendSelection("look at this");
    assert.deepEqual(posted, [{ type: "prefill", text: "look at this" }]);

    posted.length = 0;
    state.config.sendSelectionImmediately = true;
    try {
      await controller.sendSelection("look at this");
    } finally {
      delete state.config.sendSelectionImmediately;
    }
    assert.ok(posted.some((m) => m.type === "user" && m.text === "look at this"));
    assert.equal(said(), "echo: look at this");
    assert.ok(!posted.some((m) => m.type === "prefill"));
  });
});

describe("Controller with an agent that offers neither session/load nor images", () => {
  let ws: string;
  let controller: InstanceType<typeof Controller>;
  const posted: ToView[] = [];
  const { until, notices } = harness(posted);

  before(async () => {
    ws = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(), "incode-vscode-ctl-")));
    state.roots = [ws];
    state.config.args = ["--no-reopen", "--no-image"];
    const output = { append: () => undefined, appendLine: () => undefined, show: () => undefined };
    controller = new Controller(output as never, "0.0.0-test");
    controller.attach({ post: (m) => posted.push(m), reveal: async () => undefined });
    await controller.onMessage({ type: "ready" });
    await until(() => posted.some((m) => m.type === "commands"), "the slash commands");
  });
  after(async () => {
    state.config.args = [];
    controller.dispose();
    await new Promise((r) => setTimeout(r, 100));
    await fs.rm(ws, { recursive: true, force: true });
  });

  it("the view is told, and the title-bar button's context key is off", async () => {
    posted.length = 0;
    await controller.onMessage({ type: "ready" });
    assert.deepEqual(posted[1], { type: "capabilities", images: false, reopen: false });
    assert.equal(state.contexts?.["incode.canReopenSession"], false);
  });

  it("Open Session says so and asks the agent nothing", async () => {
    state.infos = [];
    state.quickPicks = [];
    await controller.pickSession();
    assert.deepEqual(state.infos, ["incode: this agent cannot reopen sessions (it does not offer session/list and session/load)."]);
    assert.equal(state.quickPicks.length, 0);
    posted.length = 0;
    await controller.loadSession("kept-1");
    assert.deepEqual(notices(), ["info: This agent cannot reopen sessions."]);
    assert.ok(!posted.some((m) => m.type === "reset"), "the current session is kept");
  });

  it("a picture is not sent, and the Image button does nothing but say why", async () => {
    posted.length = 0;
    await controller.onMessage({ type: "send", text: "images", images: [{ name: "dot.png", mimeType: "image/png", data: PNG_BASE64 }] });
    assert.deepEqual(notices(), ["error: Not sent: this agent does not take pictures (it does not advertise promptCapabilities.image)."]);
    assert.ok(!posted.some((m) => m.type === "user" || m.type === "turn"));
    posted.length = 0;
    state.openDialog = [path.join(ws, "never-asked.png")];
    await controller.onMessage({ type: "attachImage" });
    assert.deepEqual(notices(), ["info: This agent does not take pictures."]);
    // Text alone still works.
    posted.length = 0;
    await controller.onMessage({ type: "send", text: "hi" });
    assert.ok(posted.some((m) => m.type === "agentText" && m.text === "echo: hi"));
  });
});
