import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import { afterEach, beforeEach, describe, it } from "node:test";

import {
  AcpError,
  AgentCrashed,
  IncodeError,
  IncodeTimeout,
  Session,
  type PermissionRequest,
  type PromptResult,
  type PromptStream,
  type SessionEvent,
  type SessionOptions,
  type WriteFileRequest,
} from "../src/index.js";
import { FAKE, tempDirs, waitFor, type TempDirs } from "./support.js";

async function all(stream: PromptStream): Promise<{ events: SessionEvent[]; result: PromptResult }> {
  const seen: Array<SessionEvent | PromptResult> = [];
  for await (const e of stream) seen.push(e);
  const last = seen.pop();
  assert.ok(last && last.type === "prompt_result", JSON.stringify(last));
  return { events: seen as SessionEvent[], result: last };
}

let dirs: TempDirs;
const open: Session[] = [];

function session(options: SessionOptions = {}): Session {
  const s = new Session({ workspace: dirs.ws, incodePath: FAKE, requestTimeoutMs: 10_000, ...options });
  open.push(s);
  return s;
}

/** Started, and past the `available_commands_update` incode sends after answering `session/new`. */
async function started(options: SessionOptions = {}): Promise<Session> {
  const s = session(options);
  await s.start();
  if (!options.loadSessionId) await waitFor(() => s.idleEvents.some((e) => e.type === "available_commands"));
  return s;
}

beforeEach(() => {
  dirs = tempDirs();
});
afterEach(async () => {
  await Promise.all(open.splice(0).map((s) => s.close()));
  dirs.cleanup();
});

describe("start", () => {
  it("opens a session and passes the flags", async () => {
    const argvFile = path.join(dirs.root, "argv.json");
    const s = await started({ model: "opus", permissionMode: "read-only", env: { FAKE_INCODE_ARGV: argvFile } });
    assert.equal(s.sessionId, "fake-session-1");
    assert.equal(s.protocolVersion, 1);
    assert.equal(s.agentInfo["name"], "incode");
    assert.equal(s.agentCapabilities["loadSession"], true);
    assert.equal(s.currentMode, "read-only");
    assert.deepEqual(
      s.availableModes.map((m) => m["id"]),
      ["read-only", "workspace-write", "danger-full-access"],
    );
    assert.deepEqual(
      s.configOptions.map((o) => o["id"]),
      ["agent", "role"],
    );
    await waitFor(() => s.idleEvents.length > 0);
    const first = s.idleEvents[0];
    assert.equal(first?.type, "available_commands");
    assert.deepEqual(JSON.parse(fs.readFileSync(argvFile, "utf8")), ["--acp", "-m", "opus", "-p", "read-only"]);
    await s.close();
    assert.equal(s.exitCode, 0);
  });

  it("onUpdate hears idle updates", async () => {
    const heard: SessionEvent[] = [];
    await started({ onUpdate: (e) => heard.push(e) });
    await waitFor(() => heard.length > 0);
    assert.equal(heard[0]?.type, "available_commands");
  });

  it("a refused start rejects with stderr", async () => {
    await assert.rejects(session({ permissionMode: "prompt" }).start(), (e: unknown) => {
      assert.ok(e instanceof AgentCrashed);
      assert.equal(e.exitCode, 1);
      assert.match(e.stderr, /--acp/);
      return true;
    });
  });

  it("a missing workspace is refused by the agent", async () => {
    await assert.rejects(session({ workspace: path.join(dirs.root, "missing") }).start(), (e: unknown) => {
      assert.ok(e instanceof AcpError);
      assert.equal(e.code, -32602);
      assert.equal(e.method, "session/new");
      return true;
    });
  });

  it("a missing binary rejects", async () => {
    await assert.rejects(session({ incodePath: path.join(dirs.root, "nope") }).start(), /could not start incode/);
  });

  it("loads a kept session and replays its history", async () => {
    const s = await started({ loadSessionId: "kept-session" });
    assert.equal(s.sessionId, "kept-session");
    assert.deepEqual(
      s.idleEvents.slice(0, 2).map((e) => e.type),
      ["user_message_chunk", "message_chunk"],
    );
    await assert.rejects(session({ loadSessionId: "no-such" }).start(), AcpError);
  });

  it("lists this workspace's sessions newest first, or every workspace's", async () => {
    const s = await started();
    const mine = await s.sessions();
    assert.deepEqual(
      mine.map((i) => i.sessionId),
      ["fake-session-1", "kept-session"],
    );
    assert.ok(mine.every((i) => i.cwd === s.workspace));
    assert.equal(mine[1]?.title, "an earlier session");
    assert.equal(mine[0]?.updatedAt, "2026-10-22T12:00:00Z");
    const everywhere = await s.sessions({ allWorkspaces: true });
    assert.deepEqual(
      everywhere.map((i) => i.sessionId),
      ["fake-session-1"],
    );
    assert.equal((await s.listSessions()).length, 2, "the raw form stays as it was");
  });
});

describe("prompt", () => {
  it("streams typed events then the result", async () => {
    const s = await started();
    const stream = s.prompt("hello");
    const { events, result } = await all(stream);
    assert.equal(result.stopReason, "end_turn");
    assert.equal(stream.result, result);
    assert.deepEqual(
      events.map((e) => e.type),
      ["thought_chunk", "message_chunk", "tool_call", "tool_call_update", "plan", "usage", "unknown", "message_chunk"],
    );
    assert.equal(stream.text, "Hello (mode workspace-write, agent claude-code)\n");
    const [, , call, done, plan, usage, unknown] = events;
    assert.ok(call?.type === "tool_call");
    assert.deepEqual([call.toolCallId, call.kind, call.status, call.title], ["t1", "read", "in_progress", "Read a.txt"]);
    assert.deepEqual(call.rawInput, { path: "a.txt" });
    assert.ok(done?.type === "tool_call_update");
    assert.deepEqual([done.toolCallId, done.status, done.rawOutput], ["t1", "completed", "contents of a"]);
    assert.ok(plan?.type === "plan");
    assert.deepEqual(plan.entries[0], { content: "Read a.txt", priority: "high", status: "completed" });
    assert.ok(usage?.type === "usage");
    assert.deepEqual([usage.used, usage.size], [38833, 1000000]);
    assert.ok(unknown?.type === "unknown");
    assert.equal(unknown.kind, "some_future_update");
    assert.equal((await s.prompt("hello again").wait()).stopReason, "end_turn");
  });

  it("accepts content blocks", async () => {
    const s = await started();
    assert.equal((await s.prompt([{ type: "text", text: "hello" }]).wait()).stopReason, "end_turn");
  });

  it("allows one prompt at a time", async () => {
    const s = await started();
    const stream = s.prompt("wait");
    assert.throws(() => s.prompt("hello"), IncodeError);
    s.cancel();
    assert.equal((await stream.wait()).stopReason, "cancelled");
  });

  it("sets the mode and a config option", async () => {
    const s = await started();
    await s.setMode("read-only");
    assert.equal(s.currentMode, "read-only");
    const options = await s.setConfigOption("agent", "codex");
    assert.equal(options.find((o) => o["id"] === "agent")?.["currentValue"], "codex");
    const stream = s.prompt("hello");
    await stream.wait();
    assert.match(stream.text, /mode read-only, agent codex/);
    assert.ok(s.idleEvents.some((e) => e.type === "mode_changed" && e.modeId === "read-only"));
    await assert.rejects(s.setMode("yolo"), (e: unknown) => e instanceof AcpError && e.code === -32602);
    await assert.rejects(s.setConfigOption("agent", "not-installed"), AcpError);
  });
});

describe("permission", () => {
  it("rejects by default", async () => {
    const s = await started();
    const stream = s.prompt("permission");
    const { events, result } = await all(stream);
    assert.equal(result.stopReason, "end_turn");
    assert.match(stream.text, /permission: reject/);
    const update = events.find((e) => e.type === "tool_call_update");
    assert.ok(update?.type === "tool_call_update");
    assert.equal(update.status, "failed");
  });

  it("round-trips an allow", async () => {
    const asked: PermissionRequest[] = [];
    const s = await started({
      onPermission: async (request) => {
        asked.push(request);
        await new Promise((r) => setTimeout(r, 10));
        return request.allow();
      },
    });
    const stream = s.prompt("permission");
    await stream.wait();
    assert.match(stream.text, /permission: allow/);
    const request = asked[0];
    assert.ok(request);
    assert.equal(request.sessionId, "fake-session-1");
    assert.equal(request.toolCallId, "g1");
    assert.match(request.title, /writing here needs the owner/);
    assert.deepEqual(
      request.options.map((o) => o.kind),
      ["allow_once", "reject_once"],
    );
  });

  for (const [decision, said] of [
    [true, /permission: allow/],
    [false, /permission: reject/],
    [null, /permission: cancelled/],
    ["bogus", /permission: reject/],
  ] as const) {
    it(`answers ${JSON.stringify(decision)}`, async () => {
      const s = await started({ onPermission: () => decision });
      const stream = s.prompt("permission");
      await stream.wait();
      assert.match(stream.text, said);
    });
  }

  it("a throwing callback rejects", async () => {
    const s = await started({
      onPermission: () => {
        throw new Error("callback bug");
      },
    });
    const stream = s.prompt("permission");
    await stream.wait();
    assert.match(stream.text, /permission: reject/);
    assert.match(String(s.callbackErrors[0]), /callback bug/);
  });

  it("cancel answers an open question cancelled", async () => {
    let release: () => void = () => {};
    let asked = false;
    const s = await started({
      onPermission: (request) =>
        new Promise((resolve) => {
          asked = true;
          release = () => resolve(request.allow());
        }),
    });
    const stream = s.prompt("permission-wait");
    await waitFor(() => asked);
    s.cancel();
    const result = await stream.wait();
    release();
    assert.equal(result.stopReason, "cancelled");
    assert.match(stream.text, /permission answer after cancel: cancelled/);
  });
});

describe("fs", () => {
  async function fsPrompt(s: Session, readPath: string, writePath: string): Promise<string> {
    const stream = s.prompt(`fs ${readPath} ${writePath}`);
    assert.equal((await stream.wait()).stopReason, "end_turn");
    return stream.text;
  }

  it("reads and writes inside the workspace", async () => {
    fs.writeFileSync(path.join(dirs.ws, "in.txt"), "hello from the workspace");
    const s = await started();
    const text = await fsPrompt(s, path.join(dirs.ws, "in.txt"), path.join(dirs.ws, "sub", "out.txt"));
    assert.match(text, /read: hello from the workspace/);
    assert.match(text, /write: ok/);
    assert.equal(fs.readFileSync(path.join(dirs.ws, "sub", "out.txt"), "utf8"), "written by fake\n");
  });

  it("refuses outside the workspace, through .. and symlinks", async () => {
    const secret = path.join(dirs.root, "secret.txt");
    fs.writeFileSync(secret, "do not read");
    const outside = path.join(dirs.root, "out.txt");
    fs.symlinkSync(dirs.root, path.join(dirs.ws, "escape"));
    // Raw strings: `..` must reach incode's client unnormalised.
    const cases: Array<[string, string]> = [
      [secret, outside],
      [`${dirs.ws}/../secret.txt`, `${dirs.ws}/../out.txt`],
      [`${dirs.ws}/escape/secret.txt`, `${dirs.ws}/escape/out.txt`],
      [`${dirs.ws}/escape/ws/../secret.txt`, `${dirs.ws}/escape/ws/../out.txt`],
    ];
    const s = await started();
    for (const [readPath, writePath] of cases) {
      const text = await fsPrompt(s, readPath, writePath);
      assert.match(text, /read error: -32602 access denied/, readPath);
      assert.match(text, /write error: -32602 access denied/, writePath);
      assert.doesNotMatch(text, /do not read/);
    }
    assert.equal(fs.existsSync(outside), false);
  });

  it("a missing file is resource-not-found", async () => {
    const s = await started();
    const text = await fsPrompt(s, path.join(dirs.ws, "nope.txt"), path.join(dirs.ws, "ok.txt"));
    assert.match(text, /read error: -32002/);
  });

  it("disabled and custom handlers", async () => {
    const writes: WriteFileRequest[] = [];
    const s = await started({ onReadFile: false, onWriteFile: (r) => void writes.push(r) });
    const text = await fsPrompt(s, "/anything", "/virtual/out.txt");
    assert.match(text, /read error: -32601/);
    assert.match(text, /write: ok/);
    assert.equal(writes[0]?.path, "/virtual/out.txt");
    assert.equal(writes[0]?.content, "written by fake\n");
  });
});

describe("failures", () => {
  it("cancel ends the prompt cancelled", async () => {
    const s = await started();
    const stream = s.prompt("wait");
    const first = await stream.next();
    assert.ok(!first.done && first.value.type === "message_chunk");
    assert.equal(first.value.text, "working");
    const began = Date.now();
    stream.cancel();
    assert.equal((await stream.wait()).stopReason, "cancelled");
    assert.ok(Date.now() - began < 5000);
    assert.equal((await s.prompt("hello").wait()).stopReason, "end_turn");
  });

  it("an agent crash mid-prompt rejects the stream", async () => {
    const s = await started();
    const stream = s.prompt("crash");
    const first = await stream.next();
    assert.ok(!first.done && first.value.type === "message_chunk" && first.value.text === "about to crash");
    await assert.rejects(stream.next(), (e: unknown) => {
      assert.ok(e instanceof AgentCrashed);
      assert.equal(e.exitCode, 3);
      assert.match(e.stderr, /crashed mid-prompt/);
      return true;
    });
    assert.throws(() => s.prompt("hello"), AgentCrashed);
    await assert.rejects(s.setMode("read-only"), AgentCrashed);
  });

  it("skips malformed lines", async () => {
    const heard: string[] = [];
    const s = await started({ onProtocolError: (line) => heard.push(line) });
    const stream = s.prompt("malformed");
    assert.equal((await stream.wait()).stopReason, "end_turn");
    assert.equal(stream.text, "still alive");
    assert.equal((await s.prompt("hello").wait()).stopReason, "end_turn");
    assert.equal(heard.length, 4, JSON.stringify(heard));
    assert.ok(heard.includes("this is not json"));
  });

  it("answers an unknown agent request with method-not-found", async () => {
    const s = await started();
    const stream = s.prompt("unknown-request");
    await stream.wait();
    assert.match(stream.text, /terminal\/create -> -32601/);
  });

  it("times a prompt out", async () => {
    const s = await started();
    await assert.rejects(s.prompt("wait", { timeoutMs: 500 }).wait(), IncodeTimeout);
  });
});
