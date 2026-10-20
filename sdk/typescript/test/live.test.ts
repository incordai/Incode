// Against the real binary: set INCODE_BIN=/path/to/incode to run; skipped otherwise.
// Only `initialize` and `session/new` — no turn runs, so no agent CLI is needed.
import assert from "node:assert/strict";
import { it } from "node:test";

import { Session } from "../src/index.js";
import { tempDirs } from "./support.js";

const INCODE_BIN = process.env["INCODE_BIN"];

it("live: initialize + session/new against INCODE_BIN", { skip: INCODE_BIN ? false : "set INCODE_BIN to run" }, async () => {
  const dirs = tempDirs();
  const session = new Session({ workspace: dirs.ws, incodePath: INCODE_BIN, requestTimeoutMs: 60_000 });
  try {
    await session.start();
    assert.equal(session.protocolVersion, 1);
    assert.equal(session.agentInfo["name"], "incode");
    assert.ok(session.sessionId);
    assert.ok(session.availableModes.some((m) => m["id"] === "workspace-write"));
    assert.ok(session.currentMode);
    assert.ok(session.configOptions.some((o) => o["id"] === "agent"));
  } finally {
    await session.close();
    dirs.cleanup();
  }
});
