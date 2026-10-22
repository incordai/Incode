// The package entry (src/index.ts): what a consumer of `@incord/incode-sdk`
// can import, and that it works through that import.

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import * as entry from "../src/index.js";
import { FAKE, tempDirs } from "./support.js";

describe("the package entry (src/index.ts)", () => {
  it("exports the one-shot runs, runTask and streamTask", () => {
    assert.equal(typeof entry.runTask, "function");
    assert.equal(typeof entry.streamTask, "function");
    assert.equal(typeof entry.buildTaskArgv, "function");
    assert.equal(typeof entry.parseTaskResult, "function");
  });

  it("streams a run through the entry, events then result", async () => {
    const dirs = tempDirs();
    try {
      const stream = entry.streamTask("hello", { workspace: dirs.ws, incodePath: FAKE });
      const types: string[] = [];
      for await (const event of stream) types.push(event.type);
      assert.deepEqual(types, ["init", "text", "tool_use", "tool_result", "usage"]);
      assert.equal((await stream.result).result, "done: hello");
    } finally {
      dirs.cleanup();
    }
  });
});
