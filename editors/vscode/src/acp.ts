/**
 * An Agent Client Protocol (ACP) client: the editor's side of `incode --acp`.
 *
 * Newline-delimited JSON-RPC 2.0 over the agent's stdin/stdout. Nothing here
 * imports `vscode`: the extension supplies the behaviour that needs the editor
 * (permission prompts, file reads and writes) as callbacks, so every protocol
 * rule can be tested against a fake agent in plain Node.
 *
 * Layers, bottom up:
 * - {@link LineFramer}: bytes to lines, with a cap on one line's length.
 * - {@link JsonRpcConnection}: request/response correlation, notifications,
 *   answering the agent's own requests, failing everything pending when the
 *   stream ends.
 * - {@link AcpClient}: the ACP methods by name, the agent→client requests
 *   (`session/request_permission`, `fs/read_text_file`, `fs/write_text_file`)
 *   routed to {@link ClientHandlers}, and cancel.
 * - {@link spawnAgent}: starts the process and wires the above to it.
 * - {@link mergeToolCall}: folds `tool_call` / `tool_call_update` into one
 *   record per call, for display.
 */

import { spawn, ChildProcess } from "node:child_process";
import { EventEmitter } from "node:events";
import type { Readable, Writable } from "node:stream";

// ---------------------------------------------------------------------------
// Protocol shapes (the subset this client reads and writes)
// ---------------------------------------------------------------------------

export const PROTOCOL_VERSION = 1;

export type RequestId = number | string | null;

export interface JsonRpcError {
  code: number;
  message: string;
  data?: unknown;
}

/** JSON-RPC 2.0 error codes used here. */
export const ErrorCodes = {
  ParseError: -32700,
  InvalidRequest: -32600,
  MethodNotFound: -32601,
  InvalidParams: -32602,
  InternalError: -32603,
  /** ACP: the resource (file) was not found. */
  ResourceNotFound: -32002,
} as const;

/** A JSON-RPC error answered by the other side, or raised by a handler. */
export class AcpError extends Error {
  constructor(
    readonly code: number,
    message: string,
    readonly data?: unknown,
  ) {
    super(message);
    this.name = "AcpError";
  }

  toJson(): JsonRpcError {
    const out: JsonRpcError = { code: this.code, message: this.message };
    if (this.data !== undefined) out.data = this.data;
    return out;
  }

  /** A readable one-liner: the message plus the agent's `data`, if it is text. */
  describe(): string {
    return typeof this.data === "string" && this.data.length > 0
      ? `${this.message}: ${this.data}`
      : this.message;
  }
}

/** Raised for every pending request when the connection ends. */
export class ConnectionClosedError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ConnectionClosedError";
  }
}

export interface ContentBlockText {
  type: "text";
  text: string;
}
export interface ContentBlockResource {
  type: "resource";
  resource: { uri: string; text: string; mimeType?: string };
}
export type ContentBlock = ContentBlockText | ContentBlockResource | { type: string; [k: string]: unknown };

export interface SessionMode {
  id: string;
  name: string;
  description?: string | null;
}
export interface SessionModeState {
  currentModeId: string;
  availableModes: SessionMode[];
}

export interface ConfigSelectOption {
  value: string;
  name: string;
  description?: string | null;
}
export interface ConfigSelectGroup {
  group: string;
  name: string;
  options: ConfigSelectOption[];
}
export interface SessionConfigOption {
  id: string;
  name: string;
  description?: string | null;
  type: "select" | string;
  currentValue: string;
  options: ConfigSelectOption[] | ConfigSelectGroup[];
}

export interface AvailableCommand {
  name: string;
  description: string;
  input?: { hint: string } | null;
}

export interface PlanEntry {
  content: string;
  priority: "high" | "medium" | "low" | string;
  status: "pending" | "in_progress" | "completed" | string;
}

export type ToolCallStatus = "pending" | "in_progress" | "completed" | "failed";

export interface ToolCallContent {
  type: "content" | "diff" | "terminal" | string;
  content?: ContentBlock;
  path?: string;
  oldText?: string | null;
  newText?: string;
  terminalId?: string;
}

export interface ToolCall {
  toolCallId: string;
  title: string;
  kind?: string;
  status?: ToolCallStatus;
  content?: ToolCallContent[];
  locations?: { path: string; line?: number | null }[];
  rawInput?: unknown;
  rawOutput?: unknown;
}

/** `tool_call_update`: every field but the id is optional. */
export type ToolCallUpdate = Partial<ToolCall> & { toolCallId: string };

export type SessionUpdate =
  | { sessionUpdate: "user_message_chunk"; content: ContentBlock }
  | { sessionUpdate: "agent_message_chunk"; content: ContentBlock }
  | { sessionUpdate: "agent_thought_chunk"; content: ContentBlock }
  | ({ sessionUpdate: "tool_call" } & ToolCall)
  | ({ sessionUpdate: "tool_call_update" } & ToolCallUpdate)
  | { sessionUpdate: "plan"; entries: PlanEntry[] }
  | { sessionUpdate: "available_commands_update"; availableCommands: AvailableCommand[] }
  | { sessionUpdate: "current_mode_update"; currentModeId: string }
  | { sessionUpdate: "config_option_update"; configOptions: SessionConfigOption[] }
  | { sessionUpdate: "usage_update"; used: number; size: number; cost?: { amount: number; currency: string } | null }
  | { sessionUpdate: string; [k: string]: unknown };

export interface SessionNotification {
  sessionId: string;
  update: SessionUpdate;
}

export interface PermissionOption {
  optionId: string;
  name: string;
  kind: "allow_once" | "allow_always" | "reject_once" | "reject_always" | string;
}

export interface RequestPermissionParams {
  sessionId: string;
  toolCall: ToolCallUpdate;
  options: PermissionOption[];
}

export type PermissionOutcome = { outcome: "selected"; optionId: string } | { outcome: "cancelled" };

export interface ReadTextFileParams {
  sessionId: string;
  path: string;
  /** 1-based first line. */
  line?: number | null;
  limit?: number | null;
}

export interface WriteTextFileParams {
  sessionId: string;
  path: string;
  content: string;
}

export interface InitializeResult {
  protocolVersion: number;
  agentCapabilities?: {
    loadSession?: boolean;
    promptCapabilities?: { image?: boolean; audio?: boolean; embeddedContext?: boolean };
    sessionCapabilities?: { close?: object | null; list?: object | null };
    [k: string]: unknown;
  };
  authMethods?: unknown[];
  agentInfo?: { name: string; title?: string; version?: string };
}

export interface NewSessionResult {
  sessionId: string;
  modes?: SessionModeState | null;
  configOptions?: SessionConfigOption[] | null;
}

export interface LoadSessionResult {
  modes?: SessionModeState | null;
  configOptions?: SessionConfigOption[] | null;
}

export type StopReason = "end_turn" | "max_tokens" | "max_turn_requests" | "refusal" | "cancelled" | string;

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/** The longest line accepted from the agent (the agent's own cap is 8 MiB). */
export const MAX_LINE_BYTES = 16 * 1024 * 1024;

/**
 * Splits a byte stream into lines. A line longer than `maxBytes` is dropped as
 * it arrives (never buffered whole) and reported once through `onOverflow`;
 * reading resumes at the next newline.
 */
export class LineFramer {
  private parts: Buffer[] = [];
  private size = 0;
  private overflowing = false;

  constructor(
    private readonly onLine: (line: string) => void,
    private readonly onOverflow: () => void,
    private readonly maxBytes = MAX_LINE_BYTES,
  ) {}

  push(chunk: Buffer | string): void {
    let buf = typeof chunk === "string" ? Buffer.from(chunk, "utf8") : chunk;
    while (buf.length > 0) {
      const nl = buf.indexOf(0x0a);
      const piece = nl === -1 ? buf : buf.subarray(0, nl);
      if (!this.overflowing) {
        if (this.size + piece.length > this.maxBytes) {
          this.overflowing = true;
          this.parts = [];
          this.size = 0;
        } else {
          this.parts.push(piece);
          this.size += piece.length;
        }
      }
      if (nl === -1) return;
      this.finishLine();
      buf = buf.subarray(nl + 1);
    }
  }

  /** The stream ended: a last line without a terminator still counts. */
  end(): void {
    if (this.size > 0 || this.overflowing) this.finishLine();
  }

  private finishLine(): void {
    if (this.overflowing) {
      this.overflowing = false;
      this.onOverflow();
      return;
    }
    let line = Buffer.concat(this.parts, this.size).toString("utf8");
    this.parts = [];
    this.size = 0;
    if (line.endsWith("\r")) line = line.slice(0, -1);
    this.onLine(line);
  }
}

// ---------------------------------------------------------------------------
// JSON-RPC
// ---------------------------------------------------------------------------

export type RequestHandler = (params: unknown, id: RequestId) => Promise<unknown> | unknown;
export type NotificationHandler = (method: string, params: unknown) => void;

/** What one incoming line is, or why it is not a message. */
export type Classified =
  | { kind: "request"; id: RequestId; method: string; params: unknown }
  | { kind: "notification"; method: string; params: unknown }
  | { kind: "response"; id: RequestId; result?: unknown; error?: JsonRpcError }
  | { kind: "invalid"; id: RequestId; error: JsonRpcError };

function isValidId(v: unknown): v is number | string {
  return typeof v === "string" || (typeof v === "number" && Number.isFinite(v));
}

/** Classify one line. Pure, so the rules are testable without a stream. */
export function classifyLine(line: string): Classified {
  let value: unknown;
  try {
    value = JSON.parse(line);
  } catch (e) {
    return { kind: "invalid", id: null, error: { code: ErrorCodes.ParseError, message: "Parse error", data: String(e) } };
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return {
      kind: "invalid",
      id: null,
      error: { code: ErrorCodes.InvalidRequest, message: "Invalid Request", data: "expected one JSON-RPC object per line" },
    };
  }
  const msg = value as Record<string, unknown>;
  const id: RequestId = isValidId(msg.id) ? msg.id : null;
  if (msg.jsonrpc !== "2.0") {
    return { kind: "invalid", id, error: { code: ErrorCodes.InvalidRequest, message: "Invalid Request", data: "jsonrpc must be \"2.0\"" } };
  }
  if (typeof msg.method === "string") {
    if ("id" in msg && msg.id !== null && msg.id !== undefined) {
      if (!isValidId(msg.id)) {
        return { kind: "invalid", id: null, error: { code: ErrorCodes.InvalidRequest, message: "Invalid Request", data: "bad id" } };
      }
      return { kind: "request", id: msg.id, method: msg.method, params: msg.params };
    }
    return { kind: "notification", method: msg.method, params: msg.params };
  }
  if ("result" in msg || "error" in msg) {
    if ("error" in msg) {
      const err = msg.error as Partial<JsonRpcError> | null;
      const error: JsonRpcError = {
        code: typeof err?.code === "number" ? err.code : ErrorCodes.InternalError,
        message: typeof err?.message === "string" ? err.message : "unknown error",
      };
      if (err && "data" in err) error.data = err.data;
      return { kind: "response", id, error };
    }
    return { kind: "response", id, result: msg.result };
  }
  return { kind: "invalid", id, error: { code: ErrorCodes.InvalidRequest, message: "Invalid Request" } };
}

interface Pending {
  method: string;
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
}

export interface ConnectionEvents {
  /** A line that is not a message, a response nobody asked for, a too-long line. */
  protocolError: (description: string) => void;
  /** The stream ended; every pending request has been rejected. */
  close: (reason: string) => void;
}

/** JSON-RPC 2.0, one message per line, over a pair of streams. */
export class JsonRpcConnection extends EventEmitter {
  private nextId = 1;
  private readonly pending = new Map<RequestId, Pending>();
  private readonly handlers = new Map<string, RequestHandler>();
  private notificationHandler: NotificationHandler = () => undefined;
  private closed = false;
  private closeReason = "";
  private readonly framer: LineFramer;

  constructor(
    input: Readable,
    private readonly output: Writable,
    maxLineBytes = MAX_LINE_BYTES,
  ) {
    super();
    this.framer = new LineFramer(
      (line) => this.onLine(line),
      () => this.emit("protocolError", `a line longer than ${maxLineBytes} bytes was dropped`),
      maxLineBytes,
    );
    input.on("data", (chunk: Buffer | string) => this.framer.push(chunk));
    input.on("end", () => {
      this.framer.end();
      this.close("the agent closed its output");
    });
    input.on("error", (e: Error) => this.close(`reading from the agent failed: ${e.message}`));
    output.on("error", (e: Error) => this.close(`writing to the agent failed: ${e.message}`));
  }

  get isClosed(): boolean {
    return this.closed;
  }

  onRequest(method: string, handler: RequestHandler): void {
    this.handlers.set(method, handler);
  }

  onNotification(handler: NotificationHandler): void {
    this.notificationHandler = handler;
  }

  request<T = unknown>(method: string, params?: unknown): Promise<T> {
    if (this.closed) return Promise.reject(new ConnectionClosedError(this.closeReason || "connection closed"));
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { method, resolve: resolve as (v: unknown) => void, reject });
      this.write({ jsonrpc: "2.0", id, method, params });
    });
  }

  notify(method: string, params?: unknown): void {
    if (this.closed) return;
    this.write({ jsonrpc: "2.0", method, params });
  }

  /** End the connection from this side: pending requests are rejected. */
  close(reason: string): void {
    if (this.closed) return;
    this.closed = true;
    this.closeReason = reason;
    const err = new ConnectionClosedError(reason);
    for (const p of this.pending.values()) p.reject(err);
    this.pending.clear();
    this.emit("close", reason);
  }

  private write(message: object): void {
    if (this.closed) return;
    // JSON.stringify escapes control characters inside strings, so the
    // encoded message never contains a raw newline.
    try {
      this.output.write(JSON.stringify(message) + "\n");
    } catch (e) {
      this.close(`writing to the agent failed: ${(e as Error).message}`);
    }
  }

  private respond(id: RequestId, result: unknown, error?: JsonRpcError): void {
    if (error) this.write({ jsonrpc: "2.0", id, error });
    else this.write({ jsonrpc: "2.0", id, result: result === undefined ? null : result });
  }

  private onLine(line: string): void {
    if (line.trim() === "") return;
    const msg = classifyLine(line);
    switch (msg.kind) {
      case "invalid":
        this.emit("protocolError", `${msg.error.message}${msg.error.data ? `: ${String(msg.error.data)}` : ""} — ${truncate(line, 200)}`);
        // A request we cannot read is still owed an answer when its id is known.
        if (msg.id !== null) this.respond(msg.id, undefined, msg.error);
        return;
      case "response": {
        const p = this.pending.get(msg.id);
        if (!p) {
          this.emit("protocolError", `a response to unknown request id ${JSON.stringify(msg.id)}`);
          return;
        }
        this.pending.delete(msg.id);
        if (msg.error) p.reject(new AcpError(msg.error.code, msg.error.message, msg.error.data));
        else p.resolve(msg.result);
        return;
      }
      case "notification":
        try {
          this.notificationHandler(msg.method, msg.params);
        } catch (e) {
          this.emit("protocolError", `handling ${msg.method} failed: ${(e as Error).message}`);
        }
        return;
      case "request":
        void this.dispatch(msg.id, msg.method, msg.params);
        return;
    }
  }

  private async dispatch(id: RequestId, method: string, params: unknown): Promise<void> {
    const handler = this.handlers.get(method);
    if (!handler) {
      this.respond(id, undefined, { code: ErrorCodes.MethodNotFound, message: "Method not found", data: method });
      return;
    }
    try {
      const result = await handler(params, id);
      this.respond(id, result);
    } catch (e) {
      const error =
        e instanceof AcpError
          ? e.toJson()
          : { code: ErrorCodes.InternalError, message: "Internal error", data: e instanceof Error ? e.message : String(e) };
      this.respond(id, undefined, error);
    }
  }
}

function truncate(s: string, n: number): string {
  return s.length > n ? `${s.slice(0, n)}…` : s;
}

// ---------------------------------------------------------------------------
// ACP client
// ---------------------------------------------------------------------------

/** What the editor does for the agent. Each may throw an {@link AcpError}. */
export interface ClientHandlers {
  /**
   * Ask the person. `signal` aborts when the session's turn is cancelled: the
   * client has then already answered `cancelled`, and the prompt can go away.
   */
  requestPermission(params: RequestPermissionParams, signal: AbortSignal): Promise<PermissionOutcome>;
  readTextFile?(params: ReadTextFileParams): Promise<{ content: string }>;
  writeTextFile?(params: WriteTextFileParams): Promise<void>;
}

export interface AcpClientEvents {
  update: (n: SessionNotification) => void;
  protocolError: (description: string) => void;
  close: (reason: string) => void;
}

export interface ClientInfo {
  name: string;
  title?: string;
  version: string;
}

/** The ACP methods, by name, over one connection. */
export class AcpClient extends EventEmitter {
  /** Open permission questions, per session, so a cancel can answer them. */
  private readonly asking = new Map<string, Set<AbortController>>();
  initialized?: InitializeResult;

  constructor(
    readonly connection: JsonRpcConnection,
    private readonly handlers: ClientHandlers,
  ) {
    super();
    connection.on("protocolError", (d: string) => this.emit("protocolError", d));
    connection.on("close", (r: string) => {
      for (const set of this.asking.values()) for (const c of set) c.abort();
      this.asking.clear();
      this.emit("close", r);
    });
    connection.onNotification((method, params) => {
      if (method === "session/update" && isSessionNotification(params)) {
        this.emit("update", params);
      }
      // Every other notification is dropped, as JSON-RPC allows.
    });
    connection.onRequest("session/request_permission", (params) => this.askPermission(params));
    connection.onRequest("fs/read_text_file", async (params) => {
      if (!handlers.readTextFile) throw new AcpError(ErrorCodes.MethodNotFound, "Method not found", "fs/read_text_file");
      return handlers.readTextFile(expectFsParams(params) as ReadTextFileParams);
    });
    connection.onRequest("fs/write_text_file", async (params) => {
      if (!handlers.writeTextFile) throw new AcpError(ErrorCodes.MethodNotFound, "Method not found", "fs/write_text_file");
      const p = expectFsParams(params) as WriteTextFileParams;
      if (typeof p.content !== "string") throw new AcpError(ErrorCodes.InvalidParams, "Invalid params", "content must be a string");
      await handlers.writeTextFile(p);
      return {};
    });
  }

  private async askPermission(params: unknown): Promise<{ outcome: PermissionOutcome }> {
    const p = params as RequestPermissionParams;
    if (!p || typeof p.sessionId !== "string" || !Array.isArray(p.options) || !p.toolCall) {
      throw new AcpError(ErrorCodes.InvalidParams, "Invalid params", "sessionId, toolCall and options are required");
    }
    const controller = new AbortController();
    let set = this.asking.get(p.sessionId);
    if (!set) this.asking.set(p.sessionId, (set = new Set()));
    set.add(controller);
    const cancelled = new Promise<PermissionOutcome>((resolve) => {
      controller.signal.addEventListener("abort", () => resolve({ outcome: "cancelled" }), { once: true });
    });
    try {
      const outcome = await Promise.race([this.handlers.requestPermission(p, controller.signal), cancelled]);
      if (outcome.outcome === "selected" && !p.options.some((o) => o.optionId === outcome.optionId)) {
        return { outcome: { outcome: "cancelled" } };
      }
      return { outcome };
    } finally {
      set.delete(controller);
      if (set.size === 0) this.asking.delete(p.sessionId);
      controller.abort();
    }
  }

  async initialize(info: ClientInfo, capabilities = defaultClientCapabilities()): Promise<InitializeResult> {
    const result = await this.connection.request<InitializeResult>("initialize", {
      protocolVersion: PROTOCOL_VERSION,
      clientCapabilities: capabilities,
      clientInfo: info,
    });
    if (!result || result.protocolVersion !== PROTOCOL_VERSION) {
      throw new AcpError(
        ErrorCodes.InternalError,
        `the agent speaks ACP protocol version ${String(result?.protocolVersion)}, this client speaks ${PROTOCOL_VERSION}`,
      );
    }
    this.initialized = result;
    return result;
  }

  newSession(cwd: string, mcpServers: unknown[] = []): Promise<NewSessionResult> {
    return this.connection.request<NewSessionResult>("session/new", { cwd, mcpServers });
  }

  loadSession(sessionId: string, cwd: string, mcpServers: unknown[] = []): Promise<LoadSessionResult> {
    return this.connection.request<LoadSessionResult>("session/load", { sessionId, cwd, mcpServers });
  }

  async prompt(sessionId: string, prompt: ContentBlock[]): Promise<StopReason> {
    const r = await this.connection.request<{ stopReason: StopReason }>("session/prompt", { sessionId, prompt });
    return r?.stopReason ?? "end_turn";
  }

  /**
   * Cancel the session's running turn. Open permission questions for it are
   * answered `cancelled` at once, as the protocol requires of the client.
   */
  cancel(sessionId: string): void {
    const set = this.asking.get(sessionId);
    if (set) for (const c of [...set]) c.abort();
    this.connection.notify("session/cancel", { sessionId });
  }

  async setMode(sessionId: string, modeId: string): Promise<void> {
    await this.connection.request("session/set_mode", { sessionId, modeId });
  }

  async setConfigOption(sessionId: string, configId: string, value: string): Promise<SessionConfigOption[] | undefined> {
    const r = await this.connection.request<{ configOptions?: SessionConfigOption[] }>("session/set_config_option", {
      sessionId,
      configId,
      value,
    });
    return r?.configOptions;
  }

  async closeSession(sessionId: string): Promise<void> {
    await this.connection.request("session/close", { sessionId });
  }

  get supportsClose(): boolean {
    const close = this.initialized?.agentCapabilities?.sessionCapabilities?.close;
    return close !== undefined && close !== null;
  }
}

export function defaultClientCapabilities(): object {
  return { fs: { readTextFile: true, writeTextFile: true }, terminal: false };
}

function isSessionNotification(p: unknown): p is SessionNotification {
  if (!p || typeof p !== "object") return false;
  const n = p as Record<string, unknown>;
  return (
    typeof n.sessionId === "string" &&
    !!n.update &&
    typeof n.update === "object" &&
    typeof (n.update as Record<string, unknown>).sessionUpdate === "string"
  );
}

function expectFsParams(params: unknown): { sessionId: string; path: string } {
  const p = params as Record<string, unknown> | null;
  if (!p || typeof p.sessionId !== "string" || typeof p.path !== "string") {
    throw new AcpError(ErrorCodes.InvalidParams, "Invalid params", "sessionId and path are required");
  }
  return p as { sessionId: string; path: string };
}

// ---------------------------------------------------------------------------
// Spawning
// ---------------------------------------------------------------------------

export interface SpawnOptions {
  command: string;
  /** After `--acp`. */
  args?: string[];
  /** Before `--acp`: lets the tests run `node fake-agent.js --acp`. */
  prefixArgs?: string[];
  cwd: string;
  env?: NodeJS.ProcessEnv;
  handlers: ClientHandlers;
  /** Each chunk of the agent's stderr. */
  onStderr?: (text: string) => void;
}

export interface AgentProcess {
  client: AcpClient;
  child: ChildProcess;
  /** Resolves once the process has exited, with how. */
  exited: Promise<{ code: number | null; signal: NodeJS.Signals | null; error?: Error }>;
  /** Close stdin (the agent's signal to shut down), then kill after `graceMs`. */
  stop(graceMs?: number): Promise<void>;
}

/** Start `command --acp ...args` and connect an {@link AcpClient} to it. */
export function spawnAgent(opts: SpawnOptions): AgentProcess {
  const child = spawn(opts.command, [...(opts.prefixArgs ?? []), "--acp", ...(opts.args ?? [])], {
    cwd: opts.cwd,
    env: opts.env ?? process.env,
    stdio: ["pipe", "pipe", "pipe"],
    shell: false,
    windowsHide: true,
  });
  const connection = new JsonRpcConnection(child.stdout!, child.stdin!);
  const client = new AcpClient(connection, opts.handlers);
  child.stderr!.setEncoding("utf8");
  child.stderr!.on("data", (d: string) => opts.onStderr?.(d));

  const exited = new Promise<{ code: number | null; signal: NodeJS.Signals | null; error?: Error }>((resolve) => {
    child.once("error", (error) => {
      connection.close(`could not start \`${opts.command}\`: ${error.message}`);
      resolve({ code: null, signal: null, error });
    });
    child.once("exit", (code, signal) => {
      connection.close(`the agent exited (${signal ? `signal ${signal}` : `code ${String(code)}`})`);
      resolve({ code, signal });
    });
  });

  const stop = async (graceMs = 2500): Promise<void> => {
    if (child.exitCode !== null || child.signalCode !== null) return;
    try {
      child.stdin?.end();
    } catch {
      /* already closed */
    }
    const timer = setTimeout(() => child.kill(), graceMs);
    await exited;
    clearTimeout(timer);
  };

  return { client, child, exited, stop };
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

/**
 * Fold a `tool_call` or `tool_call_update` into the record kept for that call.
 * An update for a call never announced starts a record of its own.
 */
export function mergeToolCall(prev: ToolCall | undefined, update: ToolCallUpdate): ToolCall {
  const base: ToolCall = prev ?? { toolCallId: update.toolCallId, title: update.title ?? "tool call", status: "pending" };
  const out: ToolCall = { ...base };
  for (const key of ["title", "kind", "status", "content", "locations", "rawInput", "rawOutput"] as const) {
    const v = update[key];
    if (v !== undefined && v !== null) (out as unknown as Record<string, unknown>)[key] = v;
  }
  return out;
}

/** Text of a content block, for display; non-text blocks are named. */
export function textOf(block: ContentBlock | undefined): string {
  if (!block) return "";
  if (block.type === "text" && typeof (block as ContentBlockText).text === "string") return (block as ContentBlockText).text;
  if (block.type === "resource_link") return `[${String((block as Record<string, unknown>).uri ?? "resource")}]`;
  if (block.type === "resource") {
    const r = (block as ContentBlockResource).resource;
    return r && typeof r.text === "string" ? r.text : "[resource]";
  }
  return `[${block.type}]`;
}

/** The options of a select, flattened (groups are a display nicety). */
export function flattenSelectOptions(options: SessionConfigOption["options"]): ConfigSelectOption[] {
  const out: ConfigSelectOption[] = [];
  for (const o of options as (ConfigSelectOption | ConfigSelectGroup)[]) {
    if ("group" in o && Array.isArray((o as ConfigSelectGroup).options)) out.push(...(o as ConfigSelectGroup).options);
    else out.push(o as ConfigSelectOption);
  }
  return out;
}
