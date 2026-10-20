import { strict as assert } from "node:assert";
import * as path from "node:path";
import { PassThrough } from "node:stream";
import { after, describe, it } from "node:test";
import {
  AcpClient,
  AcpError,
  AgentProcess,
  ClientHandlers,
  ConnectionClosedError,
  JsonRpcConnection,
  LineFramer,
  PermissionOutcome,
  RequestPermissionParams,
  SessionNotification,
  classifyLine,
  flattenSelectOptions,
  mergeToolCall,
  spawnAgent,
} from "../src/acp";
import { checkWorkspacePath, sliceLines } from "../src/paths";

const FAKE = path.resolve(__dirname, "..", "..", "test", "fixtures", "fake-agent.js");
const WORKSPACE = path.resolve(__dirname, "..", "..", "test", "fixtures");

interface Harness {
  agent: AgentProcess;
  updates: SessionNotification[];
  stderr: string[];
  protocolErrors: string[];
  permissions: RequestPermissionParams[];
  /** The text of every agent_message_chunk so far. */
  said(): string;
}

const started: AgentProcess[] = [];
after(async () => {
  await Promise.all(started.map((a) => a.stop(200)));
});

function start(overrides: Partial<ClientHandlers> = {}): Harness {
  const updates: SessionNotification[] = [];
  const stderr: string[] = [];
  const protocolErrors: string[] = [];
  const permissions: RequestPermissionParams[] = [];
  const roots = [WORKSPACE];
  const handlers: ClientHandlers = {
    async requestPermission(p) {
      permissions.push(p);
      return { outcome: "selected", optionId: "always" };
    },
    async readTextFile(p) {
      const check = checkWorkspacePath(p.path, roots);
      if (!check.ok) throw new AcpError(-32602, "Invalid params", `refused: ${check.reason}`);
      return { content: sliceLines("one\ntwo\nthree\n", p.line, p.limit) };
    },
    async writeTextFile(p) {
      const check = checkWorkspacePath(p.path, roots);
      if (!check.ok) throw new AcpError(-32602, "Invalid params", `refused: ${check.reason}`);
    },
    ...overrides,
  };
  const agent = spawnAgent({
    command: process.execPath,
    prefixArgs: [FAKE],
    cwd: WORKSPACE,
    handlers,
    onStderr: (t) => stderr.push(t),
  });
  started.push(agent);
  agent.client.on("update", (n: SessionNotification) => updates.push(n));
  agent.client.on("protocolError", (d: string) => protocolErrors.push(d));
  return {
    agent,
    updates,
    stderr,
    protocolErrors,
    permissions,
    said: () =>
      updates
        .filter((u) => u.update.sessionUpdate === "agent_message_chunk")
        .map((u) => (u.update as { content: { text: string } }).content.text)
        .join(""),
  };
}

async function ready(h: Harness): Promise<string> {
  await h.agent.client.initialize({ name: "test", version: "0" });
  const s = await h.agent.client.newSession(WORKSPACE);
  return s.sessionId;
}

describe("AcpClient against the fake agent", () => {
  it("initialize → session/new → a streamed prompt", async () => {
    const h = start();
    const init = await h.agent.client.initialize({ name: "test", version: "0" });
    assert.equal(init.protocolVersion, 1);
    assert.deepEqual(
      (init.agentInfo as unknown as { clientCapabilitiesSeen: unknown }).clientCapabilitiesSeen,
      { fs: { readTextFile: true, writeTextFile: true }, terminal: false },
      "the client advertises fs read/write",
    );
    assert.equal(h.agent.client.supportsClose, true);
    const session = await h.agent.client.newSession(WORKSPACE);
    assert.equal(session.sessionId, "sess-1");
    assert.equal(session.modes?.currentModeId, "workspace-write");
    assert.equal(session.configOptions?.[0].id, "agent");

    const stop = await h.agent.client.prompt(session.sessionId, [{ type: "text", text: "stream" }]);
    assert.equal(stop, "end_turn");
    assert.equal(h.said(), "Hello, world");
    const kinds = h.updates.map((u) => u.update.sessionUpdate);
    assert.deepEqual(kinds, [
      "available_commands_update",
      "agent_message_chunk",
      "agent_message_chunk",
      "agent_message_chunk",
      "tool_call",
      "tool_call_update",
      "plan",
      "usage_update",
    ]);
    // Updates arrive before the prompt's answer, in order; the tool call folds.
    let call = undefined;
    for (const u of h.updates) {
      if (u.update.sessionUpdate === "tool_call" || u.update.sessionUpdate === "tool_call_update") {
        call = mergeToolCall(call, u.update as never);
      }
    }
    assert.equal(call?.title, "Read file");
    assert.equal(call?.status, "completed");
    assert.equal(call?.content?.length, 1);
    assert.match(h.stderr.join(""), /fake agent ready/);
  });

  it("set_mode and set_config_option", async () => {
    const h = start();
    const id = await ready(h);
    await h.agent.client.setMode(id, "read-only");
    const options = await h.agent.client.setConfigOption(id, "agent", "codex");
    assert.equal(options?.[0].currentValue, "codex");
  });

  it("a permission round trip", async () => {
    const h = start();
    const id = await ready(h);
    await h.agent.client.prompt(id, [{ type: "text", text: "permission" }]);
    assert.equal(h.permissions.length, 1);
    assert.equal(h.permissions[0].toolCall.toolCallId, "t2");
    assert.deepEqual(
      h.permissions[0].options.map((o) => o.kind),
      ["allow_once", "allow_always", "reject_once"],
    );
    assert.equal(h.said(), "chose always");
  });

  it("an option the agent did not offer is answered as cancelled", async () => {
    const h = start({ requestPermission: async () => ({ outcome: "selected", optionId: "made-up" }) });
    const id = await ready(h);
    await h.agent.client.prompt(id, [{ type: "text", text: "permission" }]);
    assert.equal(h.said(), "chose nothing");
  });

  it("fs/read_text_file inside the workspace, with line and limit", async () => {
    const h = start();
    const id = await ready(h);
    await h.agent.client.prompt(id, [{ type: "text", text: `read ${path.join(WORKSPACE, "a.txt")} 2 1` }]);
    assert.equal(h.said(), "content: two\n");
  });

  it("fs requests outside the workspace are refused", async () => {
    const h = start();
    const id = await ready(h);
    const outside = path.resolve(WORKSPACE, "..", "..", "package.json");
    await h.agent.client.prompt(id, [{ type: "text", text: `read ${outside}` }]);
    assert.match(h.said(), /^error -32602: refused: .* is outside the workspace folders$/);
    const before = h.said().length;
    await h.agent.client.prompt(id, [{ type: "text", text: `write ${path.join(WORKSPACE, "..", "escape.txt")} hi` }]);
    assert.match(h.said().slice(before), /^error -32602: refused/);
    const before2 = h.said().length;
    await h.agent.client.prompt(id, [{ type: "text", text: `write relative/path.txt hi` }]);
    assert.match(h.said().slice(before2), /^error -32602: refused: .*not an absolute path/);
  });

  it("fs/write_text_file inside the workspace answers {}", async () => {
    const h = start();
    const id = await ready(h);
    await h.agent.client.prompt(id, [{ type: "text", text: `write ${path.join(WORKSPACE, "new.txt")} some words` }]);
    assert.equal(h.said(), "wrote {}");
  });

  it("fs requests without handlers are 'method not found'", async () => {
    const h = start({ readTextFile: undefined });
    const id = await ready(h);
    await h.agent.client.prompt(id, [{ type: "text", text: `read ${path.join(WORKSPACE, "a.txt")}` }]);
    assert.match(h.said(), /^error -32601/);
  });

  it("an unknown agent→client request is answered 'method not found'", async () => {
    const h = start();
    const id = await ready(h);
    await h.agent.client.prompt(id, [{ type: "text", text: "unknown" }]);
    assert.equal(h.said(), "code -32601");
  });

  it("cancel ends a running prompt with `cancelled`", async () => {
    const h = start();
    const id = await ready(h);
    const running = h.agent.client.prompt(id, [{ type: "text", text: "hang" }]);
    while (!h.said().includes("working")) await new Promise((r) => setTimeout(r, 5));
    h.agent.client.cancel(id);
    assert.equal(await running, "cancelled");
  });

  it("cancel answers an open permission question `cancelled`", async () => {
    let aborted = false;
    let asked = false;
    const h = start({
      requestPermission: (_p, signal) =>
        new Promise<PermissionOutcome>(() => {
          // Never answered by the person; the cancel must answer it.
          asked = true;
          signal.addEventListener("abort", () => (aborted = true));
        }),
    });
    const id = await ready(h);
    const running = h.agent.client.prompt(id, [{ type: "text", text: "permission-hang" }]);
    while (!asked) await new Promise((r) => setTimeout(r, 5));
    h.agent.client.cancel(id);
    assert.equal(await running, "cancelled");
    assert.equal(h.said(), 'after cancel: {"outcome":{"outcome":"cancelled"}}');
    assert.equal(aborted, true, "the handler is told to take its prompt down");
  });

  it("malformed lines are reported and skipped; the session carries on", async () => {
    const h = start();
    const id = await ready(h);
    const stop = await h.agent.client.prompt(id, [{ type: "text", text: "garbage" }]);
    assert.equal(stop, "end_turn");
    assert.equal(h.said(), "still here");
    assert.equal(h.protocolErrors.length, 3, h.protocolErrors.join("\n"));
    assert.match(h.protocolErrors[0], /Parse error/);
    assert.match(h.protocolErrors[1], /one JSON-RPC object per line/);
    assert.match(h.protocolErrors[2], /unknown request id "nobody"/);
    // The update without `update` was dropped, not delivered half-formed.
    assert.ok(h.updates.every((u) => u.update && typeof u.update.sessionUpdate === "string"));
  });

  it("the agent exiting mid-turn rejects the pending prompt and emits close", async () => {
    const h = start();
    const id = await ready(h);
    const closed = new Promise<string>((r) => h.agent.client.once("close", r));
    await assert.rejects(h.agent.client.prompt(id, [{ type: "text", text: "exit" }]), (e: unknown) => {
      assert.ok(e instanceof ConnectionClosedError);
      return true;
    });
    assert.match(await closed, /agent (exited|closed)/);
    const how = await h.agent.exited;
    assert.equal(how.code, 3);
    await assert.rejects(h.agent.client.newSession(WORKSPACE), ConnectionClosedError, "later requests fail at once");
  });

  it("a binary that does not exist fails cleanly", async () => {
    const agent = spawnAgent({
      command: path.join(WORKSPACE, "no-such-incode"),
      cwd: WORKSPACE,
      handlers: { requestPermission: async () => ({ outcome: "cancelled" }) },
    });
    await assert.rejects(agent.client.initialize({ name: "t", version: "0" }), ConnectionClosedError);
    const how = await agent.exited;
    assert.ok(how.error, "the spawn error is reported");
  });

  it("stop() closes stdin and the agent exits", async () => {
    const h = start();
    await ready(h);
    await h.agent.stop(2000);
    const how = await h.agent.exited;
    assert.equal(how.code, 0);
  });
});

describe("JsonRpcConnection over in-memory streams", () => {
  function pair(): { conn: JsonRpcConnection; toClient: PassThrough; fromClient: string[] } {
    const toClient = new PassThrough();
    const out = new PassThrough();
    const fromClient: string[] = [];
    out.on("data", (d: Buffer) => fromClient.push(...d.toString("utf8").split("\n").filter(Boolean)));
    return { conn: new JsonRpcConnection(toClient, out, 256), toClient, fromClient };
  }

  it("correlates responses by id, in any order, and maps errors", async () => {
    const { conn, toClient, fromClient } = pair();
    const a = conn.request("a");
    const b = conn.request("b");
    await new Promise((r) => setImmediate(r));
    const [ra, rb] = fromClient.map((l) => JSON.parse(l));
    toClient.write(JSON.stringify({ jsonrpc: "2.0", id: rb.id, error: { code: -32602, message: "bad", data: "why" } }) + "\n");
    toClient.write(JSON.stringify({ jsonrpc: "2.0", id: ra.id, result: { ok: 1 } }) + "\n");
    assert.deepEqual(await a, { ok: 1 });
    await assert.rejects(b, (e: unknown) => e instanceof AcpError && e.code === -32602 && e.describe() === "bad: why");
  });

  it("a request split across chunks with CRLF still arrives", async () => {
    const { conn, toClient, fromClient } = pair();
    conn.onRequest("ping", (p) => ({ pong: p }));
    const line = JSON.stringify({ jsonrpc: "2.0", id: 9, method: "ping", params: 1 });
    toClient.write(line.slice(0, 10));
    toClient.write(line.slice(10) + "\r\n");
    await new Promise((r) => setTimeout(r, 10));
    assert.deepEqual(JSON.parse(fromClient[0]), { jsonrpc: "2.0", id: 9, result: { pong: 1 } });
  });

  it("an over-long line is dropped and the next one still arrives", async () => {
    const { conn, toClient } = pair();
    const errors: string[] = [];
    const seen: string[] = [];
    conn.on("protocolError", (d: string) => errors.push(d));
    conn.onNotification((m) => seen.push(m));
    toClient.write("x".repeat(1000) + "\n");
    toClient.write(JSON.stringify({ jsonrpc: "2.0", method: "n" }) + "\n");
    await new Promise((r) => setTimeout(r, 10));
    assert.equal(errors.length, 1);
    assert.match(errors[0], /longer than 256 bytes/);
    assert.deepEqual(seen, ["n"]);
  });

  it("a request it cannot read is answered with its own id", async () => {
    const { toClient, fromClient } = pair();
    toClient.write(JSON.stringify({ id: 4, method: "x" }) + "\n");
    await new Promise((r) => setTimeout(r, 10));
    const reply = JSON.parse(fromClient[0]);
    assert.equal(reply.id, 4);
    assert.equal(reply.error.code, -32600);
  });

  it("a handler that throws answers an internal error", async () => {
    const { conn, toClient, fromClient } = pair();
    conn.onRequest("boom", () => {
      throw new Error("kaput");
    });
    toClient.write(JSON.stringify({ jsonrpc: "2.0", id: "q", method: "boom" }) + "\n");
    await new Promise((r) => setTimeout(r, 10));
    const reply = JSON.parse(fromClient[0]);
    assert.equal(reply.id, "q");
    assert.equal(reply.error.code, -32603);
    assert.equal(reply.error.data, "kaput");
  });

  it("end of input rejects everything pending", async () => {
    const { conn, toClient } = pair();
    const p = conn.request("never");
    toClient.end();
    await assert.rejects(p, ConnectionClosedError);
    assert.equal(conn.isClosed, true);
  });

  it("the AcpClient drops session/update without a valid shape", async () => {
    const { conn, toClient } = pair();
    const client = new AcpClient(conn, { requestPermission: async () => ({ outcome: "cancelled" }) });
    const got: unknown[] = [];
    client.on("update", (u) => got.push(u));
    toClient.write(JSON.stringify({ jsonrpc: "2.0", method: "session/update", params: { sessionId: 1 } }) + "\n");
    toClient.write(
      JSON.stringify({
        jsonrpc: "2.0",
        method: "session/update",
        params: { sessionId: "s", update: { sessionUpdate: "plan", entries: [] } },
      }) + "\n",
    );
    await new Promise((r) => setTimeout(r, 10));
    assert.equal(got.length, 1);
  });
});

describe("pure helpers", () => {
  it("classifyLine", () => {
    assert.equal(classifyLine("nope").kind, "invalid");
    assert.equal(classifyLine("[]").kind, "invalid");
    assert.deepEqual(classifyLine('{"jsonrpc":"2.0","method":"m"}'), { kind: "notification", method: "m", params: undefined });
    assert.equal(classifyLine('{"jsonrpc":"2.0","id":1,"method":"m"}').kind, "request");
    assert.equal(classifyLine('{"jsonrpc":"2.0","id":1,"result":null}').kind, "response");
    const bad = classifyLine('{"jsonrpc":"1.0","id":"z","method":"m"}');
    assert.equal(bad.kind, "invalid");
    assert.equal(bad.kind === "invalid" && bad.id, "z");
  });

  it("LineFramer keeps the last unterminated line and handles UTF-8 split across chunks", () => {
    const lines: string[] = [];
    const f = new LineFramer((l) => lines.push(l), () => lines.push("<overflow>"), 100);
    const bytes = Buffer.from("héllo\nwörld", "utf8");
    f.push(bytes.subarray(0, 2));
    f.push(bytes.subarray(2, 9));
    f.push(bytes.subarray(9));
    f.end();
    assert.deepEqual(lines, ["héllo", "wörld"]);
  });

  it("mergeToolCall keeps fields an update leaves out", () => {
    const a = mergeToolCall(undefined, { toolCallId: "1", title: "Edit x", kind: "edit", status: "pending" });
    const b = mergeToolCall(a, { toolCallId: "1", status: "in_progress" });
    const c = mergeToolCall(b, { toolCallId: "1", status: "failed", title: undefined });
    assert.deepEqual(c, { toolCallId: "1", title: "Edit x", kind: "edit", status: "failed" });
    assert.equal(mergeToolCall(undefined, { toolCallId: "z" }).title, "tool call");
  });

  it("flattenSelectOptions flattens groups", () => {
    assert.deepEqual(
      flattenSelectOptions([
        { group: "g", name: "G", options: [{ value: "a", name: "A" }] },
        { value: "b", name: "B" },
      ] as never).map((o) => o.value),
      ["a", "b"],
    );
  });
});
