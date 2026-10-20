/**
 * Interactive sessions: `incode --acp` driven over ACP (JSON-RPC 2.0, one
 * message per line on stdin/stdout). One `Session` is one process holding one
 * ACP session.
 */

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import * as path from "node:path";
import { createInterface } from "node:readline";

import { AcpError, AgentCrashed, IncodeError, IncodeTimeout } from "./errors.js";
import {
  parseUpdate,
  PermissionRequest,
  type Json,
  type PermissionDecision,
  type PromptResult,
  type ReadFileRequest,
  type SessionEvent,
  type WriteFileRequest,
} from "./events.js";
import { FsError, INTERNAL_ERROR, INVALID_PARAMS, RESOURCE_NOT_FOUND, WorkspaceFs } from "./fs.js";
import { commandOf, type IncodePath } from "./task.js";

export const PROTOCOL_VERSION = 1;
const METHOD_NOT_FOUND = -32601;
const STDERR_TAIL = 64 * 1024;
const SDK_VERSION = "0.1.0";

type MaybePromise<T> = T | Promise<T>;
export type PermissionHandler = (request: PermissionRequest) => MaybePromise<PermissionDecision>;
export type ReadFileHandler = (request: ReadFileRequest) => MaybePromise<string>;
export type WriteFileHandler = (request: WriteFileRequest) => MaybePromise<void>;

/** The default permission handler: pick the reject option (`cancelled` if there is none). */
export const rejectAll: PermissionHandler = (request) => request.reject();
/** Allow every call once. Only for trusted, sandboxed work. */
export const allowAll: PermissionHandler = (request) => request.allow();

export interface SessionOptions {
  /** The session's workspace (`cwd`). Default: `process.cwd()`. Made absolute. */
  workspace?: string;
  /** The incode executable, or a command prefix. Default `"incode"`. */
  incodePath?: IncodePath;
  /** `-m`. */
  model?: string;
  /** `-p`: read-only | workspace-write | danger-full-access. */
  permissionMode?: string;
  /** Answers `session/request_permission`. Default: reject. */
  onPermission?: PermissionHandler;
  /** Answers `fs/read_text_file`. Default: the workspace only. `false`: not offered. */
  onReadFile?: ReadFileHandler | false;
  /** Answers `fs/write_text_file`. Default: the workspace only. `false`: not offered. */
  onWriteFile?: WriteFileHandler | false;
  /** `session/update` events that arrive while no prompt is running. Also kept in `idleEvents`. */
  onUpdate?: (event: SessionEvent) => void;
  /** Lines on incode's stdout that are not JSON-RPC (they are skipped). */
  onProtocolError?: (line: string, error: Error) => void;
  /** Load this kept session (`session/load`) instead of opening a new one. */
  loadSessionId?: string;
  /** MCP servers for the session, in ACP's shape. */
  mcpServers?: Json[];
  /** Extra environment variables, on top of `process.env`. */
  env?: Record<string, string>;
  /** Extra arguments after `--acp`. */
  extraArgs?: readonly string[];
  /** How long to wait for a request's answer. Default 120000. `0` waits forever. */
  requestTimeoutMs?: number;
}

export interface PromptOptions {
  /** Bound the whole prompt; when it passes, a cancel is sent and the stream throws IncodeTimeout. */
  timeoutMs?: number;
}

interface Pending {
  method: string;
  resolve: (message: Json) => void;
  reject: (error: Error) => void;
}

type Item = SessionEvent | PromptResult | Error;

/**
 * The events of one prompt, in order, ending with a `PromptResult`
 * (`for await (const event of stream)`). `text` accumulates the agent's message
 * chunks; `result` is set when the prompt ends; `wait()` consumes the rest.
 */
export class PromptStream implements AsyncIterableIterator<SessionEvent | PromptResult> {
  text = "";
  result: PromptResult | undefined;
  readonly events: SessionEvent[] = [];
  private readonly items: Item[] = [];
  private waiter: ((item: Item) => void) | undefined;
  private finished = false;
  private readonly deadline: number | undefined;

  constructor(
    private readonly session: Session,
    private readonly timeoutMs: number | undefined,
  ) {
    this.deadline = timeoutMs === undefined ? undefined : Date.now() + timeoutMs;
  }

  /** @internal */
  push(item: Item): void {
    const waiter = this.waiter;
    if (waiter) {
      this.waiter = undefined;
      waiter(item);
    } else {
      this.items.push(item);
    }
  }

  private take(): Promise<Item | "timeout"> {
    const queued = this.items.shift();
    if (queued !== undefined) return Promise.resolve(queued);
    return new Promise((resolve) => {
      let timer: NodeJS.Timeout | undefined;
      this.waiter = (item) => {
        if (timer) clearTimeout(timer);
        resolve(item);
      };
      if (this.deadline !== undefined) {
        timer = setTimeout(
          () => {
            this.waiter = undefined;
            resolve("timeout");
          },
          Math.max(this.deadline - Date.now(), 0),
        );
      }
    });
  }

  async next(): Promise<IteratorResult<SessionEvent | PromptResult>> {
    if (this.finished) return { done: true, value: undefined };
    const item = await this.take();
    if (item === "timeout") {
      this.finished = true;
      this.session.cancel();
      throw new IncodeTimeout(`the prompt did not end within ${this.timeoutMs}ms (a cancel was sent)`, {
        stderr: this.session.stderr,
      });
    }
    if (item instanceof Error) {
      this.finished = true;
      throw item;
    }
    if (item.type === "prompt_result") {
      this.finished = true;
      this.result = item;
      return { done: false, value: item };
    }
    if (item.type === "message_chunk") this.text += item.text;
    this.events.push(item);
    return { done: false, value: item };
  }

  async return(): Promise<IteratorResult<SessionEvent | PromptResult>> {
    // Breaking out of `for await` leaves the prompt running; cancel() stops it.
    this.finished = true;
    return { done: true, value: undefined };
  }

  [Symbol.asyncIterator](): this {
    return this;
  }

  /** Consume the remaining events and return how the prompt ended. */
  async wait(): Promise<PromptResult> {
    for (;;) {
      const r = await this.next();
      if (r.done) break;
    }
    if (!this.result) throw new IncodeError("the prompt stream was abandoned before it ended");
    return this.result;
  }

  cancel(): void {
    this.session.cancel();
  }
}

/**
 * `incode --acp`.
 *
 * ```ts
 * const session = await Session.start({ workspace: "/path/to/repo" });
 * try {
 *   for await (const event of session.prompt("Explain src/main.rs")) { ... }
 * } finally {
 *   await session.close();
 * }
 * ```
 */
export class Session {
  readonly workspace: string;
  readonly fs: WorkspaceFs;
  sessionId: string | undefined;
  protocolVersion: number | undefined;
  agentInfo: Json = {};
  agentCapabilities: Json = {};
  currentMode: string | undefined;
  availableModes: Json[] = [];
  configOptions: Json[] = [];
  readonly idleEvents: SessionEvent[] = [];
  readonly protocolErrors: string[] = [];
  readonly callbackErrors: unknown[] = [];

  private readonly options: SessionOptions;
  private readonly onPermission: PermissionHandler;
  private readonly readFile: ReadFileHandler | undefined;
  private readonly writeFile: WriteFileHandler | undefined;
  private child: ChildProcessWithoutNullStreams | undefined;
  private nextId = 1;
  private readonly pending = new Map<number | string, Pending>();
  private active: PromptStream | undefined;
  private readonly openPermissions = new Map<unknown, string>();
  private readonly answered = new Set<unknown>();
  private stderrTail = "";
  private dead: IncodeError | undefined;
  private closing = false;
  private exited: Promise<void> | undefined;
  private exitCodeValue: number | null = null;

  constructor(options: SessionOptions = {}) {
    this.options = options;
    this.workspace = path.resolve(options.workspace ?? process.cwd());
    this.fs = new WorkspaceFs(this.workspace);
    this.onPermission = options.onPermission ?? rejectAll;
    this.readFile = options.onReadFile === false ? undefined : (options.onReadFile ?? ((r) => this.fs.read(r)));
    this.writeFile = options.onWriteFile === false ? undefined : (options.onWriteFile ?? ((r) => this.fs.write(r)));
  }

  /** Construct and `start()` in one step. */
  static async start(options: SessionOptions = {}): Promise<Session> {
    const session = new Session(options);
    await session.start();
    return session;
  }

  /** The command line `start()` runs. */
  argv(): string[] {
    const argv = commandOf(this.options.incodePath);
    argv.push("--acp");
    if (this.options.model) argv.push("-m", this.options.model);
    if (this.options.permissionMode) argv.push("-p", this.options.permissionMode);
    argv.push(...(this.options.extraArgs ?? []));
    return argv;
  }

  /** The last 64 KiB incode wrote to stderr. */
  get stderr(): string {
    return this.stderrTail;
  }

  /** The process's exit code once it has exited. */
  get exitCode(): number | null {
    return this.exitCodeValue;
  }

  /** Start `incode --acp`, `initialize`, and open (or load) the session. Resolves with its id. */
  async start(): Promise<string> {
    if (this.child) throw new IncodeError("this Session was already started");
    const [command, ...args] = this.argv();
    const child = spawn(command as string, args, {
      stdio: ["pipe", "pipe", "pipe"],
      env: this.options.env ? { ...process.env, ...this.options.env } : process.env,
    });
    this.child = child;
    child.stdin.on("error", () => {
      /* EPIPE after the agent exits: reported through `close` instead. */
    });
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (d: string) => {
      this.stderrTail = (this.stderrTail + d).slice(-STDERR_TAIL);
    });
    const lines = createInterface({ input: child.stdout, crlfDelay: Infinity });
    lines.on("line", (line) => {
      if (line.trim()) this.dispatch(line.trim());
    });
    this.exited = new Promise<void>((resolve) => {
      let spawnError: Error | undefined;
      child.on("error", (err) => {
        spawnError = err;
        // A spawn failure may not be followed by `close`.
        setTimeout(() => {
          if (!this.dead) this.failAll(new IncodeError(`could not start incode: ${err.message} (${command})`, { stderr: this.stderr }));
          resolve();
        }, 50);
      });
      child.on("close", (code) => {
        this.exitCodeValue = code;
        let error: IncodeError;
        if (spawnError) error = new IncodeError(`could not start incode: ${spawnError.message} (${command})`, { stderr: this.stderr });
        else if (this.closing) error = new IncodeError("the session was closed", { stderr: this.stderr, exitCode: code });
        else error = new AgentCrashed("incode --acp exited unexpectedly", { stderr: this.stderr, exitCode: code });
        this.failAll(error);
        resolve();
      });
    });

    try {
      const init = await this.request("initialize", {
        protocolVersion: PROTOCOL_VERSION,
        clientCapabilities: {
          fs: { readTextFile: this.readFile !== undefined, writeTextFile: this.writeFile !== undefined },
          terminal: false,
        },
        clientInfo: { name: "incode-sdk-typescript", version: SDK_VERSION },
      });
      this.protocolVersion = typeof init["protocolVersion"] === "number" ? init["protocolVersion"] : undefined;
      this.agentCapabilities = asObject(init["agentCapabilities"]);
      this.agentInfo = asObject(init["agentInfo"]);
      const mcpServers = this.options.mcpServers ?? [];
      let opened: Json;
      if (this.options.loadSessionId) {
        opened = await this.request("session/load", { sessionId: this.options.loadSessionId, cwd: this.workspace, mcpServers });
        this.sessionId = this.options.loadSessionId;
      } else {
        opened = await this.request("session/new", { cwd: this.workspace, mcpServers });
        this.sessionId = String(opened["sessionId"]);
      }
      const modes = asObject(opened["modes"]);
      this.currentMode = typeof modes["currentModeId"] === "string" ? modes["currentModeId"] : undefined;
      this.availableModes = asArray(modes["availableModes"]).map(asObject);
      this.configOptions = asArray(opened["configOptions"]).map(asObject);
      return this.sessionId;
    } catch (e) {
      await this.close();
      throw e;
    }
  }

  /** Close the session and stop the process (stdin closed, then SIGTERM, then SIGKILL). */
  async close(timeoutMs = 5000): Promise<void> {
    const child = this.child;
    if (!child || this.closing) {
      await this.exited;
      return;
    }
    const caps = asObject(this.agentCapabilities["sessionCapabilities"]);
    if (!this.dead && this.sessionId && "close" in caps) {
      try {
        await this.request("session/close", { sessionId: this.sessionId }, Math.min(timeoutMs, 2000));
      } catch {
        /* closing anyway */
      }
    }
    this.closing = true;
    child.stdin.end();
    const exited = this.exited ?? Promise.resolve();
    const within = (ms: number): Promise<boolean> =>
      Promise.race([exited.then(() => true), new Promise<boolean>((r) => setTimeout(() => r(false), ms).unref())]);
    if (!(await within(timeoutMs))) {
      child.kill("SIGTERM");
      if (!(await within(2000))) child.kill("SIGKILL");
    }
    await exited;
  }

  // ------------------------------------------------------------ requests

  /** Send a JSON-RPC request and resolve with its result (AcpError on an error reply). `0` waits forever. */
  request(method: string, params: Json = {}, timeoutMs: number = this.options.requestTimeoutMs ?? 120_000): Promise<Json> {
    return new Promise<Json>((resolve, reject) => {
      let timer: NodeJS.Timeout | undefined;
      let id: number;
      try {
        id = this.sendRequest(method, params, {
          method,
          resolve: (m) => {
            if (timer) clearTimeout(timer);
            try {
              resolve(this.resultOf(method, m));
            } catch (e) {
              reject(e);
            }
          },
          reject: (e) => {
            if (timer) clearTimeout(timer);
            reject(e);
          },
        });
      } catch (e) {
        reject(e);
        return;
      }
      if (timeoutMs > 0) {
        timer = setTimeout(() => {
          this.pending.delete(id);
          reject(new IncodeTimeout(`no answer to ${method} within ${timeoutMs}ms`, { stderr: this.stderr }));
        }, timeoutMs);
      }
    });
  }

  /** Register `pending` and send the request; its callbacks run synchronously on the reader. */
  private sendRequest(method: string, params: Json, pending: Pending): number {
    if (this.dead) throw this.dead;
    if (!this.child) throw new IncodeError("the session is not started");
    const id = this.nextId++;
    this.pending.set(id, pending);
    this.send({ jsonrpc: "2.0", id, method, params });
    return id;
  }

  private resultOf(method: string, message: Json): Json {
    const error = message["error"];
    if (error !== undefined && error !== null) {
      const e = asObject(error);
      throw new AcpError(method, typeof e["code"] === "number" ? e["code"] : 0, String(e["message"] ?? ""), e["data"], {
        stderr: this.stderr,
      });
    }
    return asObject(message["result"]);
  }

  /** Send a JSON-RPC notification. */
  notify(method: string, params: Json = {}): void {
    if (this.dead || !this.child) return;
    this.send({ jsonrpc: "2.0", method, params });
  }

  private send(message: Json): void {
    const child = this.child;
    if (!child || child.stdin.destroyed || !child.stdin.writable) return;
    child.stdin.write(JSON.stringify(message) + "\n");
  }

  // ------------------------------------------------------------ session methods

  /**
   * Send `session/prompt`. Returns a `PromptStream` of events ending with a
   * `PromptResult`. `prompt` is text or a list of ACP content blocks.
   */
  prompt(prompt: string | Json[], options: PromptOptions = {}): PromptStream {
    if (this.dead) throw this.dead;
    if (!this.sessionId) throw new IncodeError("the session is not started");
    if (this.active) throw new IncodeError("a prompt is already running in this session");
    const blocks = typeof prompt === "string" ? [{ type: "text", text: prompt }] : prompt;
    const stream = new PromptStream(this, options.timeoutMs);
    this.active = stream;
    const done = (item: PromptResult | Error): void => {
      // On the reader, after every update that came before the reply.
      if (this.active === stream) this.active = undefined;
      stream.push(item);
    };
    try {
      this.sendRequest(
        "session/prompt",
        { sessionId: this.sessionId, prompt: blocks },
        {
          method: "session/prompt",
          resolve: (message) => {
            try {
              const result = this.resultOf("session/prompt", message);
              done({ type: "prompt_result", stopReason: String(result["stopReason"] ?? ""), raw: result });
            } catch (e) {
              done(e as Error);
            }
          },
          reject: done,
        },
      );
    } catch (e) {
      this.active = undefined;
      throw e;
    }
    return stream;
  }

  /**
   * Send `session/cancel`; the running prompt ends with `stopReason: "cancelled"`.
   * Any permission question still open is answered `cancelled`, as ACP asks.
   */
  cancel(): void {
    if (!this.sessionId || this.dead) return;
    this.notify("session/cancel", { sessionId: this.sessionId });
    for (const [id, sid] of [...this.openPermissions]) {
      if (sid === this.sessionId) this.answer(id, { outcome: { outcome: "cancelled" } });
    }
  }

  /** `session/set_mode`: read-only, workspace-write or danger-full-access. */
  async setMode(modeId: string): Promise<void> {
    await this.request("session/set_mode", { sessionId: this.sessionId, modeId });
    this.currentMode = modeId;
  }

  /** `session/set_config_option` (`agent` or `role`). Resolves with every option as it now stands. */
  async setConfigOption(configId: string, value: string): Promise<Json[]> {
    const result = await this.request("session/set_config_option", { sessionId: this.sessionId, configId, value });
    this.configOptions = asArray(result["configOptions"]).map(asObject);
    return this.configOptions;
  }

  /** `session/list` for this workspace. */
  async listSessions(): Promise<Json[]> {
    return asArray((await this.request("session/list", { cwd: this.workspace }))["sessions"]).map(asObject);
  }

  // ------------------------------------------------------------ reader side

  private protocolError(line: string, error: Error): void {
    this.protocolErrors.push(line.slice(0, 1000));
    if (this.protocolErrors.length > 100) this.protocolErrors.shift();
    try {
      this.options.onProtocolError?.(line, error);
    } catch (e) {
      this.callbackErrors.push(e);
    }
  }

  private dispatch(line: string): void {
    let message: unknown;
    try {
      message = JSON.parse(line);
    } catch (e) {
      this.protocolError(line, e as Error);
      return;
    }
    if (!message || typeof message !== "object" || Array.isArray(message)) {
      this.protocolError(line, new Error("not a JSON-RPC object"));
      return;
    }
    const m = message as Json;
    const method = m["method"];
    if (typeof method === "string") {
      const params = asObject(m["params"]);
      if (m["id"] !== undefined && m["id"] !== null) void this.serve(m["id"], method, params);
      else if (method === "session/update") this.update(params);
      return;
    }
    if ("id" in m && ("result" in m || "error" in m)) {
      const id = m["id"] as number | string;
      const pending = this.pending.get(id);
      if (!pending) {
        this.protocolError(line, new Error("a response to no request this client sent"));
        return;
      }
      this.pending.delete(id);
      pending.resolve(m);
      return;
    }
    this.protocolError(line, new Error("neither a request, a notification nor a response"));
  }

  private update(params: Json): void {
    const update = params["update"];
    if (!update || typeof update !== "object") {
      this.protocolError(JSON.stringify(params).slice(0, 1000), new Error("session/update without an update"));
      return;
    }
    const event = parseUpdate(String(params["sessionId"] ?? ""), update as Json);
    if (event.type === "mode_changed") this.currentMode = event.modeId;
    else if (event.type === "config_options_changed") this.configOptions = event.configOptions;
    if (this.active) {
      this.active.push(event);
      return;
    }
    this.idleEvents.push(event);
    if (this.idleEvents.length > 1000) this.idleEvents.shift();
    try {
      this.options.onUpdate?.(event);
    } catch (e) {
      this.callbackErrors.push(e);
    }
  }

  private failAll(error: IncodeError): void {
    if (!this.dead) this.dead = error;
    const pending = [...this.pending.values()];
    this.pending.clear();
    for (const p of pending) p.reject(error);
    // A running prompt hears about it through its own pending request.
  }

  // ------------------------------------------------------------ agent -> client requests

  private answer(id: unknown, result?: unknown, error?: Json): void {
    if (this.answered.has(id)) return;
    this.answered.add(id);
    this.openPermissions.delete(id);
    if (error) this.send({ jsonrpc: "2.0", id, error });
    else this.send({ jsonrpc: "2.0", id, result: result ?? {} });
  }

  private async serve(id: unknown, method: string, params: Json): Promise<void> {
    try {
      if (method === "session/request_permission") {
        await this.servePermission(id, params);
      } else if (method === "fs/read_text_file" && this.readFile) {
        const read = this.readFile;
        await this.serveFs(id, async () => ({ content: await read(readRequest(params)) }));
      } else if (method === "fs/write_text_file" && this.writeFile) {
        const write = this.writeFile;
        await this.serveFs(id, async () => {
          await write(writeRequest(params));
          return {};
        });
      } else {
        this.answer(id, undefined, { code: METHOD_NOT_FOUND, message: "Method not found", data: method });
      }
    } catch (e) {
      this.callbackErrors.push(e);
      this.answer(id, undefined, { code: INTERNAL_ERROR, message: `client error: ${(e as Error).message}` });
    }
  }

  private async servePermission(id: unknown, params: Json): Promise<void> {
    const request = new PermissionRequest(params);
    this.openPermissions.set(id, request.sessionId);
    let decision: PermissionDecision;
    try {
      decision = await this.onPermission(request);
    } catch (e) {
      this.callbackErrors.push(e);
      decision = request.reject();
    }
    if (decision === true) decision = request.allow();
    else if (decision === false) decision = request.reject();
    if (decision && typeof decision === "object") decision = decision.optionId;
    let outcome: Json;
    if (decision === null || decision === undefined) {
      outcome = { outcome: "cancelled" };
    } else if (request.options.some((o) => o.optionId === decision)) {
      outcome = { outcome: "selected", optionId: decision };
    } else {
      this.callbackErrors.push(new Error(`onPermission returned ${JSON.stringify(decision)}, not one of the offered option ids`));
      const fallback = request.reject();
      outcome = fallback ? { outcome: "selected", optionId: fallback } : { outcome: "cancelled" };
    }
    this.answer(id, { outcome });
  }

  private async serveFs(id: unknown, run: () => Promise<Json>): Promise<void> {
    try {
      this.answer(id, await run());
    } catch (e) {
      if (e instanceof FsError) return this.answer(id, undefined, { code: e.code, message: e.message });
      const err = e as NodeJS.ErrnoException;
      if (err.code === "ENOENT") return this.answer(id, undefined, { code: RESOURCE_NOT_FOUND, message: `file not found: ${err.path ?? ""}` });
      if (e instanceof TypeError) return this.answer(id, undefined, { code: INVALID_PARAMS, message: `invalid params: ${e.message}` });
      this.answer(id, undefined, { code: INTERNAL_ERROR, message: err.message });
    }
  }
}

function asObject(v: unknown): Json {
  return v && typeof v === "object" && !Array.isArray(v) ? (v as Json) : {};
}

function asArray(v: unknown): unknown[] {
  return Array.isArray(v) ? v : [];
}

function readRequest(params: Json): ReadFileRequest {
  if (typeof params["path"] !== "string") throw new TypeError("path must be a string");
  return {
    sessionId: String(params["sessionId"] ?? ""),
    path: params["path"],
    line: typeof params["line"] === "number" ? params["line"] : null,
    limit: typeof params["limit"] === "number" ? params["limit"] : null,
  };
}

function writeRequest(params: Json): WriteFileRequest {
  if (typeof params["path"] !== "string") throw new TypeError("path must be a string");
  if (typeof params["content"] !== "string") throw new TypeError("content must be a string");
  return { sessionId: String(params["sessionId"] ?? ""), path: params["path"], content: params["content"] };
}
