/**
 * Drive the incode coding agent from Node.
 *
 * - `runTask()` — one instruction, `incode -t ... --output-format json`.
 * - `Session` — an interactive session over ACP, `incode --acp`.
 */

export { AcpError, AgentCrashed, IncodeError, IncodeTimeout, ProtocolError, TaskFailed } from "./errors.js";
export type { ErrorDetails } from "./errors.js";
export { buildTaskArgv, parseTaskResult, runTask, streamTask } from "./task.js";
export type { IncodePath, RunTaskOptions, TaskEvent, TaskResult, TaskStream, TaskUsage } from "./task.js";
export { parseUpdate, PermissionRequest } from "./events.js";
export type {
  AvailableCommands,
  ConfigOptionsChanged,
  Json,
  MessageChunk,
  ModeChanged,
  PermissionDecision,
  PermissionOption,
  Plan,
  PlanEntry,
  PromptResult,
  ReadFileRequest,
  SessionEvent,
  ThoughtChunk,
  ToolCall,
  ToolCallUpdate,
  UnknownUpdate,
  Usage,
  UserMessageChunk,
  WriteFileRequest,
} from "./events.js";
export { FsAccessDenied, FsError, INTERNAL_ERROR, INVALID_PARAMS, RESOURCE_NOT_FOUND, WorkspaceFs, realPath } from "./fs.js";
export { allowAll, PROTOCOL_VERSION, PromptStream, rejectAll, Session } from "./session.js";
export type { PermissionHandler, PromptOptions, ReadFileHandler, SessionOptions, WriteFileHandler } from "./session.js";
