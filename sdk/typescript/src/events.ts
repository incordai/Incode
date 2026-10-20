/** Typed `session/update` events, permission requests and fs requests. */

export type Json = Record<string, unknown>;

interface Base {
  sessionId: string;
  /** The update object as sent. */
  raw: Json;
}

export interface ContentChunkBase extends Base {
  text: string;
  content: Json;
}

/** Part of the agent's reply (`agent_message_chunk`). */
export interface MessageChunk extends ContentChunkBase {
  type: "message_chunk";
}
/** Part of the agent's reasoning (`agent_thought_chunk`). */
export interface ThoughtChunk extends ContentChunkBase {
  type: "thought_chunk";
}
/** The person's own words, replayed by `session/load` (`user_message_chunk`). */
export interface UserMessageChunk extends ContentChunkBase {
  type: "user_message_chunk";
}

/** A tool call starts (`tool_call`). */
export interface ToolCall extends Base {
  type: "tool_call";
  toolCallId: string;
  title: string;
  kind: string | null;
  status: string | null;
  rawInput: unknown;
  content: unknown[];
  locations: unknown[];
}

/** A tool call progresses or ends (`tool_call_update`). Unset fields are `null`. */
export interface ToolCallUpdate extends Base {
  type: "tool_call_update";
  toolCallId: string;
  status: string | null;
  title: string | null;
  kind: string | null;
  content: unknown[] | null;
  rawInput: unknown;
  rawOutput: unknown;
}

export interface PlanEntry {
  content: string;
  priority: string | null;
  status: string | null;
}

/** The task list as it now stands (`plan`). */
export interface Plan extends Base {
  type: "plan";
  entries: PlanEntry[];
}

/** The context window's fill (`usage_update`). */
export interface Usage extends Base {
  type: "usage";
  used: number | null;
  size: number | null;
  cost: unknown;
}

/** The permission mode changed (`current_mode_update`). */
export interface ModeChanged extends Base {
  type: "mode_changed";
  modeId: string;
}

/** The pickers changed (`config_option_update`). */
export interface ConfigOptionsChanged extends Base {
  type: "config_options_changed";
  configOptions: Json[];
}

/** The slash commands on offer (`available_commands_update`). */
export interface AvailableCommands extends Base {
  type: "available_commands";
  commands: Json[];
}

/** A `session/update` kind this SDK does not model. */
export interface UnknownUpdate extends Base {
  type: "unknown";
  kind: string;
}

export type SessionEvent =
  | MessageChunk
  | ThoughtChunk
  | UserMessageChunk
  | ToolCall
  | ToolCallUpdate
  | Plan
  | Usage
  | ModeChanged
  | ConfigOptionsChanged
  | AvailableCommands
  | UnknownUpdate;

/** How a prompt ended: end_turn, cancelled, max_tokens, max_turn_requests, refusal. */
export interface PromptResult {
  type: "prompt_result";
  stopReason: string;
  raw: Json;
}

function str(v: unknown): string | null {
  return typeof v === "string" ? v : null;
}
function num(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}
function obj(v: unknown): Json {
  return v && typeof v === "object" && !Array.isArray(v) ? (v as Json) : {};
}
function arr(v: unknown): unknown[] {
  return Array.isArray(v) ? v : [];
}

/** Turn one `params.update` object into a typed event. */
export function parseUpdate(sessionId: string, update: Json): SessionEvent {
  const raw = update;
  const kind = str(update["sessionUpdate"]) ?? "";
  switch (kind) {
    case "agent_message_chunk":
    case "agent_thought_chunk":
    case "user_message_chunk": {
      const content = obj(update["content"]);
      const text = content["type"] === "text" ? (str(content["text"]) ?? "") : "";
      const type =
        kind === "agent_message_chunk" ? "message_chunk" : kind === "agent_thought_chunk" ? "thought_chunk" : "user_message_chunk";
      return { type, sessionId, text, content, raw };
    }
    case "tool_call":
      return {
        type: "tool_call",
        sessionId,
        toolCallId: str(update["toolCallId"]) ?? "",
        title: str(update["title"]) ?? "",
        kind: str(update["kind"]),
        status: str(update["status"]),
        rawInput: update["rawInput"],
        content: arr(update["content"]),
        locations: arr(update["locations"]),
        raw,
      };
    case "tool_call_update":
      return {
        type: "tool_call_update",
        sessionId,
        toolCallId: str(update["toolCallId"]) ?? "",
        status: str(update["status"]),
        title: str(update["title"]),
        kind: str(update["kind"]),
        content: Array.isArray(update["content"]) ? update["content"] : null,
        rawInput: update["rawInput"],
        rawOutput: update["rawOutput"],
        raw,
      };
    case "plan":
      return {
        type: "plan",
        sessionId,
        entries: arr(update["entries"]).map((e) => {
          const entry = obj(e);
          return { content: str(entry["content"]) ?? "", priority: str(entry["priority"]), status: str(entry["status"]) };
        }),
        raw,
      };
    case "usage_update":
      return { type: "usage", sessionId, used: num(update["used"]), size: num(update["size"]), cost: update["cost"], raw };
    case "current_mode_update":
      return { type: "mode_changed", sessionId, modeId: str(update["currentModeId"]) ?? "", raw };
    case "config_option_update":
      return { type: "config_options_changed", sessionId, configOptions: arr(update["configOptions"]).map(obj), raw };
    case "available_commands_update":
      return { type: "available_commands", sessionId, commands: arr(update["availableCommands"]).map(obj), raw };
    default:
      return { type: "unknown", sessionId, kind, raw };
  }
}

// ------------------------------------------------------------ agent -> client requests

export interface PermissionOption {
  optionId: string;
  name: string;
  /** allow_once | allow_always | reject_once | reject_always */
  kind: string;
}

/**
 * `session/request_permission`: incode asks before a tool call runs.
 *
 * `onPermission` returns one of `options`' ids (or the option), `true` /
 * `false`, or `null` for `cancelled`. `allow()` and `reject()` pick the
 * matching option for you.
 */
export class PermissionRequest {
  readonly sessionId: string;
  readonly toolCall: Json;
  readonly options: PermissionOption[];
  readonly raw: Json;

  constructor(params: Json) {
    this.raw = params;
    this.sessionId = str(params["sessionId"]) ?? "";
    this.toolCall = obj(params["toolCall"]);
    this.options = arr(params["options"]).map((o) => {
      const option = obj(o);
      return { optionId: String(option["optionId"]), name: str(option["name"]) ?? "", kind: str(option["kind"]) ?? "" };
    });
  }

  get title(): string {
    return str(this.toolCall["title"]) ?? "";
  }

  get toolCallId(): string {
    return str(this.toolCall["toolCallId"]) ?? "";
  }

  optionOfKind(...kinds: string[]): string | null {
    for (const kind of kinds) {
      const found = this.options.find((o) => o.kind === kind);
      if (found) return found.optionId;
    }
    return null;
  }

  /** The allow-once option (or allow-always when that is all there is). */
  allow(): string | null {
    return this.optionOfKind("allow_once", "allow_always");
  }

  allowAlways(): string | null {
    return this.optionOfKind("allow_always", "allow_once");
  }

  /** The reject-once option (or reject-always); `null` (cancelled) when there is none. */
  reject(): string | null {
    return this.optionOfKind("reject_once", "reject_always");
  }
}

export type PermissionDecision = string | PermissionOption | boolean | null | undefined;

/** `fs/read_text_file`. `line` is 1-based; `limit` is a number of lines. */
export interface ReadFileRequest {
  sessionId: string;
  path: string;
  line: number | null;
  limit: number | null;
}

/** `fs/write_text_file`. */
export interface WriteFileRequest {
  sessionId: string;
  path: string;
  content: string;
}
