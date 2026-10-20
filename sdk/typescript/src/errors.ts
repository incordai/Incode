/** Errors raised by the SDK. Every one carries incode's stderr (the tail, for a session). */

export interface ErrorDetails {
  stderr?: string;
  exitCode?: number | null;
}

export class IncodeError extends Error {
  readonly stderr: string;
  readonly exitCode: number | null;

  constructor(message: string, details: ErrorDetails = {}) {
    const stderr = details.stderr ?? "";
    const exitCode = details.exitCode ?? null;
    let full = message;
    if (exitCode !== null) full += ` (exit code ${exitCode})`;
    const tail = stderr.trim();
    if (tail) full += `\n--- incode stderr ---\n${tail.slice(-2000)}`;
    super(full);
    this.name = new.target.name;
    this.stderr = stderr;
    this.exitCode = exitCode;
  }
}

/** `incode -t` ran and reported `is_error: true`; `result` is the parsed result. */
export class TaskFailed extends IncodeError {
  constructor(
    message: string,
    readonly result: import("./task.js").TaskResult,
    details: ErrorDetails = {},
  ) {
    super(message, details);
  }
}

/** The process or a request did not finish in time. */
export class IncodeTimeout extends IncodeError {}

/** incode wrote something the SDK could not understand where it needed to. */
export class ProtocolError extends IncodeError {}

/** incode answered an ACP request with a JSON-RPC error. */
export class AcpError extends IncodeError {
  constructor(
    readonly method: string,
    readonly code: number,
    readonly rpcMessage: string,
    readonly data?: unknown,
    details: ErrorDetails = {},
  ) {
    let text = `${method} failed: ${rpcMessage} (code ${code})`;
    if (data !== undefined && data !== null) text += `: ${typeof data === "string" ? data : JSON.stringify(data)}`;
    super(text, details);
  }
}

/** The `incode --acp` process went away while the SDK was waiting on it. */
export class AgentCrashed extends IncodeError {}
