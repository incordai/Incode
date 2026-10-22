#!/usr/bin/env node
// A fake ACP agent for the unit tests: newline-delimited JSON-RPC on stdio,
// shaped like `incode --acp`. What a prompt does is chosen by its first word:
//
//   stream            three text chunks, a tool call and its update, a plan, usage
//   permission        asks session/request_permission, then says what it got
//   read <path> [line limit]   asks fs/read_text_file, says the content or the error
//   write <path> <content...>  asks fs/write_text_file, says ok or the error
//   hang              waits for session/cancel, then answers `cancelled`
//   permission-hang   asks for permission, and on session/cancel answers `cancelled`
//   garbage           writes malformed lines, then a valid chunk
//   unknown           sends the client a request for a method it does not have
//   exit              exits with code 3 in the middle of the turn
//   images            says what image blocks the prompt carried
//   whoami            says which session the prompt was sent to
//
// Kept sessions, as incode keeps them: `session/list` names two, one per
// page (to exercise the cursor), and `session/load` replays one's history as
// session/update BEFORE it answers, then sends the slash commands, as incode
// does (src/acp_agent/mod.rs, "The history first, then the answer").
// Loading a session that is still open is refused, as incode refuses it.
//
// `--acp` is accepted and ignored, so the tests spawn it the way the
// extension spawns incode. `--no-reopen` takes `loadSession` and
// `sessionCapabilities.list` away; `--no-image` takes `promptCapabilities.image`
// away (and a prompt with an image is then refused).
"use strict";

const SESSION = "sess-1";
const NO_REOPEN = process.argv.includes("--no-reopen");
const NO_IMAGE = process.argv.includes("--no-image");
const KEPT = [
  {
    sessionId: "kept-1",
    title: "fix the parser",
    updatedAt: "2026-10-01T10:00:00Z",
    history: [
      { sessionUpdate: "user_message_chunk", content: { type: "text", text: "fix the parser" } },
      { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Fixed." } },
    ],
  },
  {
    sessionId: "kept-2",
    title: "write the docs",
    updatedAt: "2026-09-30T08:00:00Z",
    history: [{ sessionUpdate: "user_message_chunk", content: { type: "text", text: "write the docs" } }],
  },
];
// Loadable but not listed: `kept-slow` answers its load 150 ms late (and an
// agent told to stop answers it before exiting, as a process would that was
// mid-write); `kept-broken` replays part of its history, then fails.
const UNLISTED = [
  { sessionId: "kept-slow", history: [{ sessionUpdate: "user_message_chunk", content: { type: "text", text: "slow one" } }] },
  { sessionId: "kept-broken", history: [{ sessionUpdate: "user_message_chunk", content: { type: "text", text: "half a history" } }] },
];
let slowLoad = null; // a promise while kept-slow's answer is pending
const open = new Set(); // session ids open on this connection
let buffer = "";
let nextId = 1;
const waiting = new Map(); // our request id -> resolve
let running = null; // { id, onCancel }
let current = SESSION; // the session the running prompt speaks for
let opened = 0; // session/new calls so far

function send(message) {
  process.stdout.write(JSON.stringify(message) + "\n");
}
function respond(id, result) {
  send({ jsonrpc: "2.0", id, result });
}
function fail(id, code, message, data) {
  send({ jsonrpc: "2.0", id, error: { code, message, data } });
}
function update(u, sessionId = current) {
  send({ jsonrpc: "2.0", method: "session/update", params: { sessionId, update: u } });
}
function say(text) {
  update({ sessionUpdate: "agent_message_chunk", content: { type: "text", text } });
}
const modes = () => ({
  currentModeId: "workspace-write",
  availableModes: [
    { id: "read-only", name: "Read only" },
    { id: "workspace-write", name: "Workspace write" },
  ],
});
function ask(method, params) {
  const id = `agent-${nextId++}`;
  return new Promise((resolve) => {
    waiting.set(id, resolve);
    send({ jsonrpc: "2.0", id, method, params });
  });
}

const configOptions = (agent) => [
  {
    id: "agent",
    name: "Agent",
    type: "select",
    currentValue: agent,
    options: [
      { value: "claude-code", name: "Claude Code" },
      { value: "codex", name: "Codex" },
    ],
  },
];

async function prompt(id, params) {
  const blocks = params.prompt || [];
  const images = blocks.filter((b) => b.type === "image");
  if (images.length > 0 && NO_IMAGE) return fail(id, -32602, "Invalid params", "images are not advertised");
  current = params.sessionId;
  const text = blocks.map((b) => (b.type === "text" ? b.text : "")).join("");
  const [word, ...rest] = text.trim().split(/\s+/);
  const cancelled = new Promise((resolve) => (running = { id, onCancel: resolve }));
  const done = (stopReason) => {
    running = null;
    respond(id, { stopReason });
  };
  switch (word) {
    case "stream":
      say("Hello");
      say(", ");
      say("world");
      update({ sessionUpdate: "tool_call", toolCallId: "t1", title: "Read file", kind: "read", status: "pending" });
      update({
        sessionUpdate: "tool_call_update",
        toolCallId: "t1",
        status: "completed",
        content: [{ type: "content", content: { type: "text", text: "contents" } }],
      });
      update({ sessionUpdate: "plan", entries: [{ content: "step one", priority: "high", status: "in_progress" }] });
      update({ sessionUpdate: "usage_update", used: 1000, size: 200000 });
      return done("end_turn");
    case "permission":
    case "permission-hang": {
      const asked = ask("session/request_permission", {
        sessionId: current,
        toolCall: { toolCallId: "t2", title: "Run `rm -rf build`", kind: "execute", status: "pending" },
        options: [
          { optionId: "allow", name: "Allow", kind: "allow_once" },
          { optionId: "always", name: "Always allow", kind: "allow_always" },
          { optionId: "reject", name: "Reject", kind: "reject_once" },
        ],
      });
      if (word === "permission-hang") {
        await cancelled;
        const reply = await asked;
        say(`after cancel: ${JSON.stringify(reply.result)}`);
        return done("cancelled");
      }
      const reply = await asked;
      const outcome = reply.result && reply.result.outcome;
      say(outcome && outcome.outcome === "selected" ? `chose ${outcome.optionId}` : "chose nothing");
      return done("end_turn");
    }
    case "read": {
      const [path, line, limit] = rest;
      const p = { sessionId: current, path };
      if (line) p.line = Number(line);
      if (limit) p.limit = Number(limit);
      const reply = await ask("fs/read_text_file", p);
      say(reply.error ? `error ${reply.error.code}: ${reply.error.data}` : `content: ${reply.result.content}`);
      return done("end_turn");
    }
    case "write": {
      const [path, ...words] = rest;
      const reply = await ask("fs/write_text_file", { sessionId: current, path, content: words.join(" ") });
      say(reply.error ? `error ${reply.error.code}: ${reply.error.data}` : `wrote ${JSON.stringify(reply.result)}`);
      return done("end_turn");
    }
    case "hang":
      say("working");
      await cancelled;
      return done("cancelled");
    case "garbage":
      process.stdout.write("this is not json\n");
      process.stdout.write("[1,2,3]\n");
      process.stdout.write('{"jsonrpc":"2.0","id":"nobody","result":{}}\n');
      process.stdout.write("\n");
      process.stdout.write('{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1"}}\n');
      say("still here");
      return done("end_turn");
    case "unknown": {
      const reply = await ask("x/does_not_exist", {});
      say(`code ${reply.error && reply.error.code}`);
      return done("end_turn");
    }
    case "exit":
      say("bye");
      setTimeout(() => process.exit(3), 20);
      return undefined;
    case "whoami":
      say(`session ${current}`);
      return done("end_turn");
    case "images":
      say(
        `${images.length} image(s): ` +
          images.map((b) => `${b.mimeType} ${Buffer.from(String(b.data), "base64").length} bytes`).join(", "),
      );
      return done("end_turn");
    default:
      say(`echo: ${text}`);
      return done("end_turn");
  }
}

function handle(line) {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return fail(null, -32700, "Parse error");
  }
  if (msg.method === undefined) {
    const resolve = waiting.get(msg.id);
    if (resolve) {
      waiting.delete(msg.id);
      resolve(msg);
    }
    return undefined;
  }
  const { id, method, params } = msg;
  switch (method) {
    case "initialize":
      return respond(id, {
        protocolVersion: 1,
        agentCapabilities: {
          loadSession: !NO_REOPEN,
          promptCapabilities: { image: !NO_IMAGE, embeddedContext: true },
          sessionCapabilities: NO_REOPEN ? { close: {} } : { close: {}, list: {} },
        },
        authMethods: [],
        agentInfo: { name: "fake", version: "0.0.1", clientCapabilitiesSeen: params.clientCapabilities },
      });
    case "session/new": {
      // sess-1, sess-2, …: each new session its own id, as incode's are.
      const sid = `sess-${++opened}`;
      open.add(sid);
      respond(id, { sessionId: sid, modes: modes(), configOptions: configOptions("claude-code") });
      return update({ sessionUpdate: "available_commands_update", availableCommands: [{ name: "undo", description: "Undo" }] }, sid);
    }
    case "session/list": {
      if (NO_REOPEN) return fail(id, -32601, "Method not found");
      if (!params || typeof params.cwd !== "string") return fail(id, -32602, "Invalid params", "cwd is required");
      const page = params.cursor === "2" ? 1 : 0;
      const s = KEPT[page];
      return respond(id, {
        sessions: [{ sessionId: s.sessionId, cwd: params.cwd, title: s.title, updatedAt: s.updatedAt }],
        ...(page === 0 ? { nextCursor: "2" } : {}),
      });
    }
    case "session/load": {
      if (NO_REOPEN) return fail(id, -32601, "Method not found");
      const kept = [...KEPT, ...UNLISTED].find((s) => s.sessionId === params.sessionId);
      if (!kept || typeof params.cwd !== "string" || !Array.isArray(params.mcpServers)) {
        return fail(id, -32602, "Invalid params", `no session \`${params.sessionId}\` is kept`);
      }
      if (open.has(kept.sessionId)) return fail(id, -32600, "Invalid request", `session \`${kept.sessionId}\` is already open`);
      for (const u of kept.history) update(u, kept.sessionId);
      if (kept.sessionId === "kept-broken") return fail(id, -32603, "Internal error", "the transcript could not be read");
      open.add(kept.sessionId);
      const answer = () => {
        respond(id, { modes: modes(), configOptions: configOptions("codex") });
        update({ sessionUpdate: "available_commands_update", availableCommands: [{ name: "undo", description: "Undo" }] }, kept.sessionId);
      };
      if (kept.sessionId !== "kept-slow") return answer();
      slowLoad = new Promise((resolve) => setTimeout(() => (answer(), (slowLoad = null), resolve()), 150));
      return undefined;
    }
    case "session/prompt":
      if (running) return fail(id, -32600, "Invalid request", "a prompt is already running");
      return void prompt(id, params);
    case "session/cancel":
      if (running) running.onCancel();
      return undefined;
    case "session/set_mode":
      return respond(id, {});
    case "session/set_config_option":
      return respond(id, { configOptions: configOptions(params.value) });
    case "session/close":
      if (!open.delete(params.sessionId)) return fail(id, -32602, "Invalid params", `no session \`${params.sessionId}\``);
      return respond(id, {});
    default:
      if (id !== undefined) return fail(id, -32601, "Method not found");
      return undefined;
  }
}

process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let nl;
  while ((nl = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, nl);
    buffer = buffer.slice(nl + 1);
    if (line.trim()) handle(line);
  }
});
process.stdin.on("end", () => (slowLoad ? slowLoad.then(() => setTimeout(() => process.exit(0), 20)) : process.exit(0)));
process.stderr.write("fake agent ready\n");
