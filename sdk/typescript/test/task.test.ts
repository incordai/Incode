import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import { afterEach, beforeEach, describe, it } from "node:test";

import {
  buildTaskArgv,
  IncodeError,
  IncodeTimeout,
  parseTaskResult,
  ProtocolError,
  runTask,
  streamTask,
  TaskFailed,
} from "../src/index.js";
import { FAKE, tempDirs, type TempDirs } from "./support.js";

describe("runTask", () => {
  let dirs: TempDirs;
  beforeEach(() => {
    dirs = tempDirs();
  });
  afterEach(() => dirs.cleanup());

  it("parses a successful result and passes the flags", async () => {
    const argvFile = path.join(dirs.root, "argv.json");
    const result = await runTask("add a test", {
      workspace: dirs.ws,
      model: "sonnet",
      permissionMode: "read-only",
      incodePath: FAKE,
      env: { FAKE_INCODE_ARGV: argvFile },
    });
    assert.equal(result.isError, false);
    assert.equal(result.result, "done: add a test");
    assert.equal(result.error, null);
    assert.equal(result.model, "sonnet");
    assert.equal(result.agent, "claude-code");
    assert.equal(result.stopReason, "end_turn");
    assert.deepEqual(result.servedBy, ["claude-code"]);
    assert.deepEqual(result.usage, { inputTokens: 120, outputTokens: 45, cacheCreationTokens: 7, cacheReadTokens: 300 });
    assert.equal(result.costUsd, 0.0123);
    assert.equal(result.durationMs, 42);
    assert.equal(result.specGate, "on");
    assert.equal(result.exitCode, 0);
    assert.match(result.stderr, /fake: running/);
    assert.deepEqual(JSON.parse(fs.readFileSync(argvFile, "utf8")), [
      "-t",
      "add a test",
      "--output-format",
      "json",
      "-w",
      dirs.ws,
      "-m",
      "sonnet",
      "-p",
      "read-only",
    ]);
  });

  it("rejects with TaskFailed carrying the result and stderr", async () => {
    await assert.rejects(runTask("fail please", { workspace: dirs.ws, incodePath: FAKE }), (e: unknown) => {
      assert.ok(e instanceof TaskFailed);
      assert.equal(e.exitCode, 1);
      assert.equal(e.result.isError, true);
      assert.equal(e.result.error, "the agent failed: fake failure");
      assert.equal(e.result.costUsd, null);
      assert.match(e.stderr, /fake: running/);
      assert.match(e.message, /fake failure/);
      return true;
    });
  });

  it("rejects with IncodeError when there is no result", async () => {
    await assert.rejects(runTask("crash", { workspace: dirs.ws, incodePath: FAKE }), (e: unknown) => {
      assert.ok(e instanceof IncodeError);
      assert.ok(!(e instanceof TaskFailed));
      assert.equal(e.exitCode, 2);
      assert.match(e.stderr, /could not start/);
      return true;
    });
  });

  it("reports a bad workspace", async () => {
    await assert.rejects(runTask("x", { workspace: path.join(dirs.ws, "missing"), incodePath: FAKE }), (e: unknown) => {
      assert.ok(e instanceof IncodeError);
      assert.match(e.stderr, /not a directory/);
      return true;
    });
  });

  it("rejects non-JSON stdout as a ProtocolError", async () => {
    await assert.rejects(runTask("garbage", { workspace: dirs.ws, incodePath: FAKE }), ProtocolError);
  });

  it("times out", async () => {
    await assert.rejects(runTask("sleep", { workspace: dirs.ws, incodePath: FAKE, timeoutMs: 1000 }), IncodeTimeout);
  });

  it("aborts", async () => {
    const controller = new AbortController();
    const running = runTask("sleep", { workspace: dirs.ws, incodePath: FAKE, signal: controller.signal });
    setTimeout(() => controller.abort(), 200);
    await assert.rejects(running, /aborted/);
  });

  it("reports a missing binary", async () => {
    await assert.rejects(runTask("x", { incodePath: path.join(dirs.root, "no-such-incode") }), /could not start incode/);
  });

  it("parses the real binary's failure shape", () => {
    const line =
      '{"type":"result","is_error":true,"session_id":"","agent":null,"model":null,"agent_session_id":null,' +
      '"stop_reason":null,"served_by":[],"usage":{"input_tokens":0,"output_tokens":0,"cache_creation_tokens":0,' +
      '"cache_read_tokens":0},"duration_ms":0,"spec_gate":"on","error":"nothing to do: the instruction is empty"}';
    const r = parseTaskResult(JSON.parse(line) as Record<string, unknown>, 1);
    assert.equal(r.isError, true);
    assert.equal(r.costUsd, null);
    assert.equal(r.result, null);
    assert.equal(r.error, "nothing to do: the instruction is empty");
  });

  it("builds the argv", () => {
    assert.deepEqual(buildTaskArgv("hi"), ["incode", "-t", "hi", "--output-format", "json"]);
    assert.deepEqual(buildTaskArgv("hi", {}, "stream-json"), ["incode", "-t", "hi", "--output-format", "stream-json"]);
  });
});

describe("streamTask", () => {
  let dirs: TempDirs;
  beforeEach(() => {
    dirs = tempDirs();
  });
  afterEach(() => dirs.cleanup());

  it("yields each event in order, then the result", async () => {
    const argvFile = path.join(dirs.root, "argv.json");
    const stream = streamTask("add a test", { workspace: dirs.ws, incodePath: FAKE, env: { FAKE_INCODE_ARGV: argvFile } });
    const types: string[] = [];
    let text = "";
    for await (const event of stream) {
      types.push(event.type);
      if (event.type === "text") text = String(event.data["text"]);
    }
    assert.deepEqual(types, ["init", "text", "tool_use", "tool_result", "usage"]);
    assert.equal(text, "working on: add a test");
    const result = await stream.result;
    assert.equal(result.result, "done: add a test");
    assert.equal(result.usage.inputTokens, 120);
    assert.ok((JSON.parse(fs.readFileSync(argvFile, "utf8")) as string[]).includes("stream-json"));
  });

  it("rejects a failed run at the end of iteration, after its events", async () => {
    const seen: string[] = [];
    await assert.rejects(
      (async () => {
        for await (const event of streamTask("fail now", { workspace: dirs.ws, incodePath: FAKE })) seen.push(event.type);
      })(),
      TaskFailed,
    );
    assert.equal(seen[0], "init");
  });

  it("rejects a line that is not JSON", async () => {
    await assert.rejects(streamTask("events-garbage", { workspace: dirs.ws, incodePath: FAKE }).result, ProtocolError);
  });

  it("rejects a crash with incode's stderr", async () => {
    await assert.rejects(streamTask("crash", { workspace: dirs.ws, incodePath: FAKE }).result, (e: unknown) => {
      assert.ok(e instanceof IncodeError);
      assert.match(e.stderr ?? "", /could not start/);
      return true;
    });
  });

  it("kills a run that takes too long", async () => {
    await assert.rejects(streamTask("sleep", { workspace: dirs.ws, incodePath: FAKE, timeoutMs: 1000 }).result, IncodeTimeout);
  });
});
