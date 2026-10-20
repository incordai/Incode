// Against the real binary: `INCODE_BIN=/path/to/incode npm test`.
// Skipped when INCODE_BIN is not set. Only initialize and session/new: a
// prompt would need an agent CLI and its login.
import { strict as assert } from "node:assert";
import { promises as fs } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { describe, it } from "node:test";
import { SessionNotification, spawnAgent } from "../src/acp";

const BIN = process.env.INCODE_BIN;

describe("the real incode --acp", { skip: BIN ? false : "set INCODE_BIN to run" }, () => {
  it("initialize and session/new", { timeout: 120_000 }, async (t) => {
    const ws = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(), "incode-vscode-real-")));
    t.after(() => fs.rm(ws, { recursive: true, force: true }));
    const stderr: string[] = [];
    const updates: SessionNotification[] = [];
    const agent = spawnAgent({
      command: BIN!,
      cwd: ws,
      handlers: { requestPermission: async () => ({ outcome: "cancelled" }) },
      onStderr: (s) => stderr.push(s),
    });
    agent.client.on("update", (n: SessionNotification) => updates.push(n));
    try {
      const init = await agent.client.initialize({ name: "incode-vscode-test", version: "0" });
      assert.equal(init.protocolVersion, 1);
      assert.equal(init.agentInfo?.name, "incode");
      const session = await agent.client.newSession(ws);
      assert.match(session.sessionId, /\S/);
      assert.ok(session.modes?.availableModes.some((m) => m.id === "read-only"), JSON.stringify(session.modes));
      assert.ok(Array.isArray(session.configOptions));
      // The slash commands follow the answer.
      const deadline = Date.now() + 10_000;
      while (!updates.some((u) => u.update.sessionUpdate === "available_commands_update") && Date.now() < deadline) {
        await new Promise((r) => setTimeout(r, 20));
      }
      assert.ok(
        updates.some((u) => u.update.sessionUpdate === "available_commands_update"),
        `no available_commands_update; stderr:\n${stderr.join("")}`,
      );
    } finally {
      await agent.stop(5000);
    }
  });
});
