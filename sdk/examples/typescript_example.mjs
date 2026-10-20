#!/usr/bin/env node
// Drive incode from Node: a one-shot task, then an ACP session that streams a prompt.
//
//   (cd sdk/typescript && npm install && npm run build)
//   node sdk/examples/typescript_example.mjs /path/to/repo "Summarise this project"
//
// INCODE_PATH picks the binary (default: `incode` on PATH). To try it without a
// real agent: INCODE_PATH=sdk/testing/fake_incode.py node sdk/examples/typescript_example.mjs . hello
import { IncodeError, runTask, Session } from "../typescript/dist/index.js";

const [workspace = ".", text = "Summarise this project in three sentences."] = process.argv.slice(2);
const incodePath = process.env.INCODE_PATH ?? "incode";

try {
  const task = await runTask(text, { workspace, incodePath, timeoutMs: 10 * 60_000 });
  console.log(`one-shot: ${task.result}\n  tokens in/out ${task.usage.inputTokens}/${task.usage.outputTokens}, cost ${task.costUsd ?? "unknown"}`);

  const session = await Session.start({
    workspace,
    incodePath,
    permissionMode: "read-only",
    onPermission: (request) => {
      console.log(`\n[permission] ${request.title} -> rejected`);
      return request.reject();
    },
  });
  try {
    process.once("SIGINT", () => session.cancel());
    for await (const event of session.prompt(text)) {
      if (event.type === "message_chunk") process.stdout.write(event.text);
      else if (event.type === "tool_call") console.log(`\n[tool] ${event.title}`);
      else if (event.type === "plan") console.log(`\n[plan] ${event.entries.map((e) => `${e.content} (${e.status})`).join("; ")}`);
      else if (event.type === "prompt_result") console.log(`\n-- stop reason: ${event.stopReason}`);
    }
  } finally {
    await session.close();
  }
} catch (e) {
  if (e instanceof IncodeError) {
    console.error(`incode failed: ${e.message}`);
    process.exitCode = 1;
  } else {
    throw e;
  }
}
