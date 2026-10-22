// Against the real binary: `INCODE_BIN=/path/to/incode npm test`.
// Skipped when INCODE_BIN is not set. No prompt: one would need an agent CLI
// and its login.
import { strict as assert } from "node:assert";
import { promises as fs } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { describe, it } from "node:test";
import { AcpError, SessionNotification, spawnAgent } from "../src/acp";

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

  // What src/controller.ts relies on: the capabilities it gates on, the
  // session/list shape, and session/load replaying before it answers. The
  // session is incode's own record — a transcript in `.incode/transcripts` —
  // written here, so no agent CLI is needed.
  it("advertises reopen and images; session/list names a kept session, session/load replays it, an image block decodes", { timeout: 120_000 }, async (t) => {
    const ws = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(), "incode-vscode-real-")));
    t.after(() => fs.rm(ws, { recursive: true, force: true }));
    const id = "0b9f7c1e-5d2a-4c8e-9a51-3f6d2e8b7a10";
    const dir = path.join(ws, ".incode", "transcripts");
    await fs.mkdir(dir, { recursive: true });
    const line = (type: string, content: unknown) => JSON.stringify({ type, message: { role: type, content } });
    await fs.writeFile(
      path.join(dir, `${id}.jsonl`),
      [line("user", "fix the parser"), line("assistant", [{ type: "text", text: "Fixed it." }])].join("\n") + "\n",
    );
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
      await agent.client.initialize({ name: "incode-vscode-test", version: "0" });
      assert.equal(agent.client.supportsReopen, true);
      assert.equal(agent.client.supportsImages, true);
      const listed = await agent.client.listSessions(ws);
      const kept = listed.sessions.find((s) => s.sessionId === id);
      assert.ok(kept, JSON.stringify(listed));
      assert.equal(kept.title, "fix the parser");
      assert.ok(kept.updatedAt && !Number.isNaN(Date.parse(kept.updatedAt)), `updatedAt: ${String(kept.updatedAt)}`);

      let atAnswer = -1;
      const loaded = await agent.client.loadSession(id, ws).then((r) => ((atAnswer = updates.length), r));
      assert.ok(loaded.modes?.availableModes.length, JSON.stringify(loaded));
      const replayed = updates.slice(0, 2).map((u) => [u.sessionId, u.update.sessionUpdate, (u.update as { content?: { text?: string } }).content?.text]);
      assert.ok(atAnswer >= 2, `the history comes before the answer; stderr:\n${stderr.join("")}`);
      assert.deepEqual(replayed, [
        [id, "user_message_chunk", "fix the parser"],
        [id, "agent_message_chunk", "Fixed it."],
      ]);
      await assert.rejects(agent.client.loadSession("no-such-session", ws), (e: unknown) => e instanceof AcpError && e.code === -32602);

      // The image block's shape is the one incode decodes: a GIF is refused
      // by its type, which incode can only read from the block's own fields
      // (a malformed block would fail to decode instead). Refused before any
      // agent CLI is involved.
      await assert.rejects(
        agent.client.prompt(id, [
          { type: "text", text: "what is this?" },
          { type: "image", mimeType: "image/gif", data: Buffer.from("GIF89a").toString("base64") },
        ]),
        (e: unknown) => e instanceof AcpError && e.code === -32602 && /only PNG and JPEG/.test(e.describe()),
      );
    } finally {
      await agent.stop(5000);
    }
  });
});
