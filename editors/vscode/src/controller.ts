/**
 * One incode agent process and its current session, for this window, and the
 * bridge between it and the chat view.
 */

import * as path from "node:path";
import * as vscode from "vscode";
import {
  AcpError,
  AgentProcess,
  AvailableCommand,
  ConnectionClosedError,
  ContentBlock,
  PermissionOutcome,
  RequestPermissionParams,
  SessionConfigOption,
  SessionInfo,
  SessionModeState,
  SessionNotification,
  ToolCall,
  mergeToolCall,
  spawnAgent,
  textOf,
} from "./acp";
import { readTextFile, writeTextFile, workspaceRoots } from "./fsBridge";
import { Attachment, MAX_IMAGE_BYTES, checkAttachments, formatBytes, imageFromFile } from "./images";

/** A message to the webview. See media/chat.js for the receiving side. */
export type ToView =
  | { type: "reset" }
  | { type: "status"; state: "stopped" | "starting" | "ready" | "error"; text: string }
  /** `images`: the names of the pictures sent with it, never their bytes. */
  | { type: "user"; text: string; images?: string[] }
  /** What the agent advertised: pictures in a prompt, reopening a kept session. */
  | { type: "capabilities"; images: boolean; reopen: boolean }
  /** A picture picked through the editor's file dialog, for the input. */
  | { type: "attachment"; image: Attachment }
  /**
   * The answer to a `send` that carried a `draft` id: the prompt went to the
   * agent (the page drops its copy of the draft), or it did not (the page
   * puts the draft back). Never replayed.
   */
  | { type: "sent"; draft: number }
  | { type: "notSent"; draft: number; reason: string }
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
  /**
   * `images` is checked again here (src/images.ts) whatever the page did.
   * `draft` identifies the page's copy of what was sent; it is answered with
   * `sent` or `notSent`.
   */
  | { type: "send"; text: string; images?: unknown; draft?: number }
  | { type: "attachImage" }
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
/** The most `session/list` pages followed for one pick. */
const MAX_LIST_PAGES = 10;

/** What became of a prompt: on its way (`turn` settles when it ends), or refused before it went. */
type Submitted = { ok: true; turn: Promise<void> } | { ok: false; level: "info" | "error"; reason: string };

/** A session request that finished after its agent was stopped or replaced: nothing it learned applies. */
class Superseded extends Error {
  constructor() {
    super("the agent was restarted");
    this.name = "Superseded";
  }
}

export class Controller implements vscode.Disposable {
  private agent?: AgentProcess;
  private starting?: Promise<AgentProcess>;
  private sessionId?: string;
  /** The session a `session/load` is opening: its replayed history is shown as it arrives. */
  private loadingId?: string;
  private caps = { images: false, reopen: false };
  private cwd?: string;
  /**
   * A turn is claimed: from the moment a send is accepted for checking
   * (before any await) until its prompt ends or it is refused. `claim`
   * numbers the holder, so a stale release (after a restart) frees nothing.
   */
  private running = false;
  private claim = 0;
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
    this.view.post({ type: "capabilities", ...this.caps });
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
      case "send": {
        const submitted = await this.submit(typeof m.text === "string" ? m.text : "", m.images);
        if (typeof m.draft === "number") {
          // Straight to the view: an answer about one draft is not history.
          this.view?.post(submitted.ok ? { type: "sent", draft: m.draft } : { type: "notSent", draft: m.draft, reason: submitted.reason });
        }
        if (!submitted.ok) return this.refused(submitted);
        return submitted.turn;
      }
      case "attachImage":
        return this.attachImage();
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
    // A process being stopped by a restart may still speak; it is not heard.
    agent.client.on("update", (n: SessionNotification) => this.agent === agent && this.onUpdate(n));
    agent.client.on("protocolError", (d: string) => this.output.appendLine(`[protocol] ${d}`));
    agent.client.on("close", (reason: string) => {
      if (this.agent !== agent) return;
      this.agent = undefined;
      this.sessionId = undefined;
      this.running = false;
      this.claim++;
      this.abandonOpening();
      this.output.appendLine(`[stopped] ${reason}`);
      this.post({ type: "turn", running: false });
      this.post({ type: "status", state: "stopped", text: `incode stopped: ${reason}` });
    });
    this.agent = agent;
    this.cwd = cwd;
    try {
      const init = await agent.client.initialize({ name: "incode-vscode", title: "incode for VS Code", version: this.version });
      if (this.agent !== agent) throw new Superseded();
      this.output.appendLine(`[ready] ${init.agentInfo?.title ?? init.agentInfo?.name ?? "agent"} ${init.agentInfo?.version ?? ""}`);
      // Not through post(): a reset clears the replay, and these hold until
      // the next process. replayToView sends them again.
      this.caps = { images: agent.client.supportsImages, reopen: agent.client.supportsReopen };
      this.view?.post({ type: "capabilities", ...this.caps });
      // Shows or hides the "Open Session…" button in the view's title bar.
      void vscode.commands.executeCommand("setContext", "incode.canReopenSession", this.caps.reopen);
    } catch (e) {
      if (e instanceof Superseded) {
        await agent.stop(500);
        throw e;
      }
      const text = describe(e);
      this.post({ type: "status", state: "error", text: `Could not start incode: ${text}. Check the \`incode.path\` setting and the incode output.` });
      this.output.show(true);
      await agent.stop(500);
      if (this.agent === agent) this.agent = undefined;
      throw e;
    }
    return agent;
  }

  /** The current session and the process it lives in, opening one if there is none. */
  private async ensureSession(): Promise<{ agent: AgentProcess; sessionId: string }> {
    const agent = await this.ensureAgent();
    if (this.agent !== agent) throw new Superseded();
    if (this.sessionId) return { agent, sessionId: this.sessionId };
    return { agent, sessionId: await this.openSession(agent) };
  }

  private opening?: Promise<string>;
  /** Which `openSession` call `opening`, `loadingId` and `early` belong to. */
  private openToken?: object;
  private early: SessionNotification[] = [];

  /** Forget an in-flight session/new or session/load: its process is gone or replaced. */
  private abandonOpening(): void {
    this.opening = undefined;
    this.openToken = undefined;
    this.loadingId = undefined;
    this.early = [];
  }

  /**
   * `session/new`, or with `load` `session/load` of that kept session, after
   * closing the one it replaces. A load is preceded by a reset (see
   * {@link loadSession}): its history arrives before the answer and is shown
   * as it comes. Either way `opening` is set at once, so a prompt sent
   * meanwhile waits for this session instead of opening another.
   */
  private openSession(agent: AgentProcess, load?: { sessionId: string; closing?: string }): Promise<string> {
    if (this.opening) return this.opening;
    const token = {};
    this.openToken = token;
    // Every continuation below first asks whether this call is still the
    // current one (a restart or the process exiting abandons it): a late
    // answer from a dead process must not set the session of the new one.
    const current = (): boolean => this.openToken === token && this.agent === agent;
    this.opening = (async () => {
      this.loadingId = load?.sessionId;
      this.post({ type: "status", state: "starting", text: load ? "Loading the session…" : "Opening a session…" });
      try {
        // Waited for: incode refuses to load a session while one of the same
        // id is still open, and the close frees the old session's slot.
        if (load?.closing && agent.client.supportsClose) {
          await agent.client.closeSession(load.closing).catch((e) => this.output.appendLine(`[close] ${describe(e)}`));
          if (!current()) throw new Superseded();
        }
        const opened = load
          ? { ...(await agent.client.loadSession(load.sessionId, this.cwd!)), sessionId: load.sessionId }
          : await agent.client.newSession(this.cwd!);
        if (!current()) throw new Superseded();
        this.sessionId = opened.sessionId;
        if (!load) {
          this.tools.clear();
          this.post({ type: "reset" });
        }
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
        if (e instanceof Superseded || !current()) throw e instanceof Superseded ? e : new Superseded();
        if (load) {
          // The session it replaced is closed and what was replayed is only
          // part of one: clear it, and say what the next message does.
          this.tools.clear();
          this.post({ type: "reset" });
          this.post({ type: "status", state: "error", text: `Could not load the session: ${describe(e)}` });
          this.notice("info", "The session was not opened. The next message starts a new session.");
        } else {
          this.post({ type: "status", state: "error", text: `Could not open a session: ${describe(e)}` });
        }
        throw e;
      } finally {
        if (this.openToken === token) this.abandonOpening();
      }
    })();
    return this.opening;
  }

  /**
   * Send a prompt: the text, then each picture as an ACP `image` block. A
   * prompt needs text (incode refuses one without), and pictures go only to
   * an agent that advertised `promptCapabilities.image`.
   */
  async send(text: string, images?: unknown): Promise<void> {
    const submitted = await this.submit(text, images);
    if (!submitted.ok) return this.refused(submitted);
    return submitted.turn;
  }

  private refused(r: { level: "info" | "error"; reason: string }): void {
    this.notice(r.level, `Not sent: ${r.reason}.`);
  }

  /**
   * Check a prompt and send it, or say why not. The turn is claimed BEFORE
   * the first await, so a second send while a session is still being opened
   * is refused rather than sent beside the first; every refusal releases it.
   */
  private async submit(text: string, images?: unknown): Promise<Submitted> {
    const trimmed = text.trim();
    if (!trimmed) return { ok: false, level: "info", reason: "the message is empty" };
    if (this.running) return { ok: false, level: "info", reason: "a turn is still running. Stop it first, or wait for it to finish" };
    const checked = checkAttachments(images);
    if (!checked.ok) return { ok: false, level: "error", reason: checked.reason };
    const claim = ++this.claim;
    this.running = true;
    const release = (): void => {
      if (this.claim === claim) this.running = false;
    };
    let agent: AgentProcess;
    let sessionId: string;
    try {
      ({ agent, sessionId } = await this.ensureSession());
    } catch (e) {
      release();
      return { ok: false, level: "error", reason: e instanceof Superseded ? "the agent was restarted" : `no session could be opened (${describe(e)})` };
    }
    if (this.claim !== claim) return { ok: false, level: "info", reason: "the agent was restarted" };
    const pictures = checked.value;
    if (pictures.length > 0 && !agent.client.supportsImages) {
      release();
      return { ok: false, level: "error", reason: "this agent does not take pictures (it does not advertise promptCapabilities.image)" };
    }
    const prompt: ContentBlock[] = [{ type: "text", text: trimmed }];
    for (const p of pictures) prompt.push({ type: "image", mimeType: p.mimeType, data: p.data });
    this.post({ type: "user", text: trimmed, ...(pictures.length > 0 ? { images: pictures.map((p) => p.name) } : {}) });
    this.post({ type: "turn", running: true });
    const turn = (async () => {
      // How it ended is said only while its session is the one shown (a New
      // Session cancels it); that it ended is always said, to end the spinner.
      const shown = (): boolean => this.sessionId === sessionId;
      try {
        const stopReason = await agent.client.prompt(sessionId, prompt);
        if (this.claim === claim) this.post(shown() ? { type: "turn", running: false, stopReason } : { type: "turn", running: false });
      } catch (e) {
        if (this.claim === claim) {
          if (shown() && !(e instanceof ConnectionClosedError)) this.notice("error", describe(e));
          this.post({ type: "turn", running: false });
        }
      } finally {
        release();
      }
    })();
    return { ok: true, turn };
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
    // A session still being opened or loaded is let finish first, then
    // closed like any other: otherwise the in-flight one would become the
    // "new" session.
    if (this.opening) await this.opening.catch(() => undefined);
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
    this.claim++;
    // A session/new or session/load in flight on the old process answers
    // into nothing: see openSession's `current()`.
    this.abandonOpening();
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

  /**
   * `incode.sendSelection`: into the input to add a question to, or — with
   * `incode.sendSelectionImmediately` — sent as it is. A selection that
   * cannot be sent now (a turn is running, no session) goes to the input
   * instead of being lost.
   */
  async sendSelection(text: string): Promise<void> {
    const now = vscode.workspace.getConfiguration("incode").get<boolean>("sendSelectionImmediately") === true;
    if (!now) return this.prefill(text);
    const submitted = await this.submit(text);
    if (submitted.ok) return submitted.turn;
    this.prefill(text);
    this.notice("info", `Not sent: ${submitted.reason}. The selection was put in the input instead.`);
  }

  // ----- kept sessions -------------------------------------------------------

  /** `incode.openSession`: list the workspace's sessions, let the person pick one, load it. */
  async pickSession(): Promise<void> {
    let agent: AgentProcess;
    try {
      agent = await this.ensureAgent();
    } catch {
      return;
    }
    if (!agent.client.supportsReopen) {
      void vscode.window.showInformationMessage("incode: this agent cannot reopen sessions (it does not offer session/list and session/load).");
      return;
    }
    if (this.running) {
      this.notice("info", "A turn is still running. Stop it first, then open the session.");
      return;
    }
    const sessions: SessionInfo[] = [];
    try {
      let cursor: string | undefined;
      for (let page = 0; page < MAX_LIST_PAGES; page++) {
        const got = await agent.client.listSessions(this.cwd!, cursor);
        sessions.push(...got.sessions);
        if (!got.nextCursor) break;
        cursor = got.nextCursor;
      }
    } catch (e) {
      this.notice("error", `Could not list the sessions: ${describe(e)}`);
      return;
    }
    if (sessions.length === 0) {
      void vscode.window.showInformationMessage("incode: no earlier sessions are kept in this workspace.");
      return;
    }
    const items = sessions.map((s) => ({
      label: s.title?.trim() || "(untitled)",
      description: [whenOf(s.updatedAt), s.sessionId === this.sessionId ? "current" : ""].filter(Boolean).join(" · "),
      detail: s.sessionId,
      sessionId: s.sessionId,
    }));
    const pick = await vscode.window.showQuickPick(items, {
      placeHolder: "Open an incode session",
      matchOnDescription: true,
      matchOnDetail: true,
    });
    if (pick) await this.loadSession(pick.sessionId);
  }

  /**
   * Leave the current session and open `sessionId` again: the view is reset
   * and shows the agent's replay of its history.
   */
  async loadSession(sessionId: string): Promise<void> {
    let agent: AgentProcess;
    try {
      agent = await this.ensureAgent();
    } catch {
      return;
    }
    if (!agent.client.supportsReopen) {
      this.notice("info", "This agent cannot reopen sessions.");
      return;
    }
    if (this.opening) await this.opening.catch(() => undefined);
    if (this.agent !== agent) return;
    // Refused rather than cancelled: the running turn's end would otherwise
    // land in the reopened session.
    if (this.running) {
      this.notice("info", "A turn is still running. Stop it first, then open the session.");
      return;
    }
    if (sessionId === this.sessionId) {
      this.notice("info", "That session is already open.");
      return;
    }
    const old = this.sessionId;
    this.sessionId = undefined;
    this.tools.clear();
    this.post({ type: "reset" });
    try {
      await this.openSession(agent, { sessionId, closing: old });
    } catch {
      /* shown in the view */
    }
  }

  // ----- pictures ----------------------------------------------------------

  /** The view's "Image" button: pick PNG/JPEG files with the editor's dialog. */
  async attachImage(): Promise<void> {
    if (!this.caps.images) {
      this.notice("info", "This agent does not take pictures.");
      return;
    }
    const uris = await vscode.window.showOpenDialog({
      canSelectMany: true,
      openLabel: "Attach",
      filters: { Images: ["png", "jpg", "jpeg"] },
    });
    for (const uri of uris ?? []) {
      const name = path.basename(uri.fsPath);
      try {
        // The size first, so an oversized file is never read.
        const stat = await vscode.workspace.fs.stat(uri);
        if (stat.size > MAX_IMAGE_BYTES) {
          this.notice("error", `Not attached: ${name} is ${formatBytes(stat.size)}; a picture may be at most ${formatBytes(MAX_IMAGE_BYTES)}.`);
          continue;
        }
        const checked = imageFromFile(name, await vscode.workspace.fs.readFile(uri));
        if (!checked.ok) this.notice("error", `Not attached: ${checked.reason}.`);
        // Straight to the view, not through post(): megabytes of base64 do
        // not belong in the replay.
        else this.view?.post({ type: "attachment", image: checked.value });
      } catch (e) {
        this.notice("error", `Not attached: ${name}: ${describe(e)}`);
      }
    }
  }

  // ----- agent → client ----------------------------------------------------

  private onUpdate(n: SessionNotification): void {
    // A session being loaded is known by id before the answer, and its
    // history comes before the answer: it is shown as it arrives.
    const active = this.sessionId ?? this.loadingId;
    if (active === undefined) {
      if (this.opening && this.early.length < 1000) this.early.push(n);
      return;
    }
    if (n.sessionId !== active) return;
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

/** An RFC 3339 time as the person's locale shows it; nothing for a missing or unreadable one. */
function whenOf(updatedAt: string | null | undefined): string {
  if (!updatedAt) return "";
  const t = new Date(updatedAt);
  return Number.isNaN(t.getTime()) ? "" : t.toLocaleString();
}

function describe(e: unknown): string {
  if (e instanceof AcpError) return e.describe();
  if (e instanceof Error) return e.message;
  return String(e);
}
