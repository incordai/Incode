/**
 * One incode agent process and its current session, for this window, and the
 * bridge between it and the chat view.
 */

import * as vscode from "vscode";
import {
  AcpError,
  AgentProcess,
  AvailableCommand,
  ConnectionClosedError,
  PermissionOutcome,
  RequestPermissionParams,
  SessionConfigOption,
  SessionModeState,
  SessionNotification,
  ToolCall,
  mergeToolCall,
  spawnAgent,
  textOf,
} from "./acp";
import { readTextFile, writeTextFile, workspaceRoots } from "./fsBridge";

/** A message to the webview. See media/chat.js for the receiving side. */
export type ToView =
  | { type: "reset" }
  | { type: "status"; state: "stopped" | "starting" | "ready" | "error"; text: string }
  | { type: "user"; text: string }
  | { type: "agentText"; text: string }
  | { type: "thought"; text: string }
  | { type: "tool"; call: ToolCall }
  | { type: "plan"; entries: unknown[] }
  | { type: "commands"; commands: AvailableCommand[] }
  | { type: "modes"; modes: SessionModeState | null }
  | { type: "mode"; currentModeId: string }
  | { type: "config"; configOptions: SessionConfigOption[] }
  | { type: "usage"; used: number; size: number; cost?: { amount: number; currency: string } | null }
  | { type: "permission"; key: number; title: string; options: { optionId: string; name: string; kind: string }[] }
  | { type: "permissionDone"; key: number; chosen?: string }
  | { type: "turn"; running: boolean; stopReason?: string }
  | { type: "notice"; level: "info" | "error"; text: string }
  | { type: "prefill"; text: string };

/** A message from the webview. */
export type FromView =
  | { type: "ready" }
  | { type: "send"; text: string }
  | { type: "stop" }
  | { type: "permission"; key: number; optionId: string }
  | { type: "setMode"; modeId: string }
  | { type: "setConfig"; configId: string; value: string }
  | { type: "openLink"; url: string }
  | { type: "newSession" }
  | { type: "restart" };

export interface View {
  post(message: ToView): void;
  reveal(): Promise<void>;
}

/** How many messages are kept to replay into a view that is opened again. */
const REPLAY_LIMIT = 4000;

export class Controller implements vscode.Disposable {
  private agent?: AgentProcess;
  private starting?: Promise<AgentProcess>;
  private sessionId?: string;
  private cwd?: string;
  private running = false;
  private tools = new Map<string, ToolCall>();
  private permissionKey = 0;
  private readonly permissions = new Map<number, (o: PermissionOutcome) => void>();
  private replay: ToView[] = [];
  private view?: View;

  constructor(
    private readonly output: vscode.OutputChannel,
    private readonly version: string,
  ) {}

  attach(view: View | undefined): void {
    this.view = view;
  }

  // ----- to the view -------------------------------------------------------

  private post(message: ToView): void {
    if (message.type === "reset") this.replay = [];
    const last = this.replay[this.replay.length - 1];
    // Streamed text is kept as one entry per run, not one per chunk.
    if (last && (message.type === "agentText" || message.type === "thought") && last.type === message.type) {
      this.replay[this.replay.length - 1] = { type: message.type, text: last.text + message.text };
    } else {
      this.replay.push(message);
      if (this.replay.length > REPLAY_LIMIT) this.replay.splice(0, this.replay.length - REPLAY_LIMIT);
    }
    this.view?.post(message);
  }

  /** The view (re)loaded: give it everything since the last reset. */
  private replayToView(): void {
    if (!this.view) return;
    this.view.post({ type: "reset" });
    for (const m of this.replay) if (m.type !== "reset") this.view.post(m);
    this.view.post({ type: "turn", running: this.running });
  }

  private notice(level: "info" | "error", text: string): void {
    this.post({ type: "notice", level, text });
    if (level === "error") this.output.appendLine(`[error] ${text}`);
  }

  // ----- from the view -----------------------------------------------------

  async onMessage(m: FromView): Promise<void> {
    switch (m.type) {
      case "ready":
        this.replayToView();
        if (!this.agent && !this.starting) void this.ensureSession().catch(() => undefined);
        return;
      case "send":
        return this.send(m.text);
      case "stop":
        return this.stop();
      case "permission": {
        const resolve = this.permissions.get(m.key);
        if (resolve) resolve({ outcome: "selected", optionId: m.optionId });
        return;
      }
      case "setMode":
        return this.setMode(m.modeId);
      case "setConfig":
        return this.setConfig(m.configId, m.value);
      case "openLink": {
        let uri: vscode.Uri;
        try {
          uri = vscode.Uri.parse(m.url, true);
        } catch {
          return;
        }
        if (uri.scheme === "http" || uri.scheme === "https") await vscode.env.openExternal(uri);
        return;
      }
      case "newSession":
        return this.newSession();
      case "restart":
        return this.restart();
    }
  }

  // ----- the agent ---------------------------------------------------------

  private settings(): { command: string; args: string[] } {
    const cfg = vscode.workspace.getConfiguration("incode");
    const command = (cfg.get<string>("path") ?? "incode").trim() || "incode";
    const args = (cfg.get<unknown[]>("args") ?? []).filter((a): a is string => typeof a === "string");
    return { command, args };
  }

  private async ensureAgent(): Promise<AgentProcess> {
    if (this.agent) return this.agent;
    if (this.starting) return this.starting;
    this.starting = this.startAgent().finally(() => (this.starting = undefined));
    return this.starting;
  }

  private async startAgent(): Promise<AgentProcess> {
    const roots = workspaceRoots();
    if (roots.length === 0) {
      const text = "Open a folder first: incode works in a workspace folder.";
      this.post({ type: "status", state: "error", text });
      throw new Error(text);
    }
    const cwd = roots[0];
    const { command, args } = this.settings();
    this.post({ type: "status", state: "starting", text: `Starting ${command} --acp…` });
    this.output.appendLine(`[start] ${command} --acp ${args.join(" ")} (cwd ${cwd})`);
    const agent = spawnAgent({
      command,
      args,
      cwd,
      handlers: {
        requestPermission: (p, signal) => this.askPermission(p, signal),
        readTextFile,
        writeTextFile,
      },
      onStderr: (text) => this.output.append(text),
    });
    agent.client.on("update", (n: SessionNotification) => this.onUpdate(n));
    agent.client.on("protocolError", (d: string) => this.output.appendLine(`[protocol] ${d}`));
    agent.client.on("close", (reason: string) => {
      if (this.agent !== agent) return;
      this.agent = undefined;
      this.sessionId = undefined;
      this.running = false;
      this.output.appendLine(`[stopped] ${reason}`);
      this.post({ type: "turn", running: false });
      this.post({ type: "status", state: "stopped", text: `incode stopped: ${reason}` });
    });
    this.agent = agent;
    this.cwd = cwd;
    try {
      const init = await agent.client.initialize({ name: "incode-vscode", title: "incode for VS Code", version: this.version });
      this.output.appendLine(`[ready] ${init.agentInfo?.title ?? init.agentInfo?.name ?? "agent"} ${init.agentInfo?.version ?? ""}`);
    } catch (e) {
      const text = describe(e);
      this.post({ type: "status", state: "error", text: `Could not start incode: ${text}. Check the \`incode.path\` setting and the incode output.` });
      this.output.show(true);
      await agent.stop(500);
      if (this.agent === agent) this.agent = undefined;
      throw e;
    }
    return agent;
  }

  private async ensureSession(): Promise<string> {
    const agent = await this.ensureAgent();
    if (this.sessionId) return this.sessionId;
    return this.openSession(agent);
  }

  private opening?: Promise<string>;
  private early: SessionNotification[] = [];

  private openSession(agent: AgentProcess): Promise<string> {
    if (this.opening) return this.opening;
    this.opening = (async () => {
      this.post({ type: "status", state: "starting", text: "Opening a session…" });
      try {
        const opened = await agent.client.newSession(this.cwd!);
        this.sessionId = opened.sessionId;
        this.tools.clear();
        this.post({ type: "reset" });
        this.post({ type: "modes", modes: opened.modes ?? null });
        this.post({ type: "config", configOptions: opened.configOptions ?? [] });
        this.post({ type: "status", state: "ready", text: "" });
        // Updates that arrived in the same read as the answer, before
        // `sessionId` was known (the slash commands follow the answer).
        const early = this.early.filter((n) => n.sessionId === opened.sessionId);
        this.early = [];
        for (const n of early) this.onUpdate(n);
        return opened.sessionId;
      } catch (e) {
        this.post({ type: "status", state: "error", text: `Could not open a session: ${describe(e)}` });
        throw e;
      } finally {
        this.opening = undefined;
        this.early = [];
      }
    })();
    return this.opening;
  }

  async send(text: string): Promise<void> {
    const trimmed = text.trim();
    if (!trimmed) return;
    if (this.running) {
      this.notice("info", "A turn is still running. Stop it first, or wait for it to finish.");
      return;
    }
    let sessionId: string;
    try {
      sessionId = await this.ensureSession();
    } catch {
      return;
    }
    const agent = this.agent!;
    this.running = true;
    this.post({ type: "user", text: trimmed });
    this.post({ type: "turn", running: true });
    try {
      const stopReason = await agent.client.prompt(sessionId, [{ type: "text", text: trimmed }]);
      this.post({ type: "turn", running: false, stopReason });
    } catch (e) {
      if (!(e instanceof ConnectionClosedError)) this.notice("error", describe(e));
      this.post({ type: "turn", running: false });
    } finally {
      this.running = false;
    }
  }

  stop(): void {
    if (this.agent && this.sessionId) this.agent.client.cancel(this.sessionId);
  }

  private async setMode(modeId: string): Promise<void> {
    if (!this.agent || !this.sessionId) return;
    try {
      await this.agent.client.setMode(this.sessionId, modeId);
      this.post({ type: "mode", currentModeId: modeId });
    } catch (e) {
      this.notice("error", `Could not change mode: ${describe(e)}`);
    }
  }

  private async setConfig(configId: string, value: string): Promise<void> {
    if (!this.agent || !this.sessionId) return;
    try {
      const options = await this.agent.client.setConfigOption(this.sessionId, configId, value);
      if (options) this.post({ type: "config", configOptions: options });
    } catch (e) {
      this.notice("error", `Could not change ${configId}: ${describe(e)}`);
    }
  }

  async newSession(): Promise<void> {
    const agent = this.agent;
    const old = this.sessionId;
    if (agent && old) {
      if (this.running) agent.client.cancel(old);
      this.sessionId = undefined;
      if (agent.client.supportsClose) {
        agent.client.closeSession(old).catch((e) => this.output.appendLine(`[close] ${describe(e)}`));
      }
    }
    this.post({ type: "reset" });
    try {
      await this.ensureSession();
    } catch {
      /* shown in the view */
    }
  }

  async restart(): Promise<void> {
    const agent = this.agent;
    this.agent = undefined;
    this.sessionId = undefined;
    this.running = false;
    if (agent) {
      this.post({ type: "status", state: "starting", text: "Restarting…" });
      await agent.stop();
    }
    this.post({ type: "reset" });
    try {
      await this.ensureSession();
    } catch {
      /* shown in the view */
    }
  }

  prefill(text: string): void {
    this.post({ type: "prefill", text });
  }

  // ----- agent → client ----------------------------------------------------

  private onUpdate(n: SessionNotification): void {
    if (this.sessionId === undefined && this.opening) {
      if (this.early.length < 1000) this.early.push(n);
      return;
    }
    if (n.sessionId !== this.sessionId) return;
    const u = n.update as { sessionUpdate: string; [k: string]: unknown };
    switch (u.sessionUpdate) {
      case "agent_message_chunk":
        this.post({ type: "agentText", text: textOf(u.content as never) });
        return;
      case "agent_thought_chunk":
        this.post({ type: "thought", text: textOf(u.content as never) });
        return;
      case "user_message_chunk":
        this.post({ type: "user", text: textOf(u.content as never) });
        return;
      case "tool_call":
      case "tool_call_update": {
        const id = String(u.toolCallId ?? "");
        if (!id) return;
        const merged = mergeToolCall(this.tools.get(id), u as never);
        this.tools.set(id, merged);
        this.post({ type: "tool", call: merged });
        return;
      }
      case "plan":
        this.post({ type: "plan", entries: Array.isArray(u.entries) ? u.entries : [] });
        return;
      case "available_commands_update":
        this.post({ type: "commands", commands: Array.isArray(u.availableCommands) ? (u.availableCommands as AvailableCommand[]) : [] });
        return;
      case "current_mode_update":
        if (typeof u.currentModeId === "string") this.post({ type: "mode", currentModeId: u.currentModeId });
        return;
      case "config_option_update":
        if (Array.isArray(u.configOptions)) this.post({ type: "config", configOptions: u.configOptions as SessionConfigOption[] });
        return;
      case "usage_update":
        if (typeof u.used === "number" && typeof u.size === "number") {
          this.post({ type: "usage", used: u.used, size: u.size, cost: (u.cost as never) ?? null });
        }
        return;
      default:
        // Updates this client does not show (session_info_update, …).
        return;
    }
  }

  private askPermission(p: RequestPermissionParams, signal: AbortSignal): Promise<PermissionOutcome> {
    const key = ++this.permissionKey;
    const title = p.toolCall.title ?? this.tools.get(p.toolCall.toolCallId)?.title ?? "The agent asks for permission";
    return new Promise<PermissionOutcome>((resolve) => {
      const finish = (outcome: PermissionOutcome): void => {
        if (!this.permissions.delete(key)) return;
        this.post({ type: "permissionDone", key, chosen: outcome.outcome === "selected" ? outcome.optionId : undefined });
        resolve(outcome);
      };
      this.permissions.set(key, finish);
      signal.addEventListener("abort", () => finish({ outcome: "cancelled" }), { once: true });
      this.post({
        type: "permission",
        key,
        title,
        options: p.options.map((o) => ({ optionId: o.optionId, name: o.name, kind: o.kind })),
      });
      void this.view?.reveal();
    });
  }

  dispose(): void {
    const agent = this.agent;
    this.agent = undefined;
    void agent?.stop(1000);
  }
}

function describe(e: unknown): string {
  if (e instanceof AcpError) return e.describe();
  if (e instanceof Error) return e.message;
  return String(e);
}
