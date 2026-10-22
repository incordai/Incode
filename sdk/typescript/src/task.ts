/**
 * One-shot runs: `incode -t "<task>" --output-format json` (`runTask`), or
 * `stream-json`, one event at a time (`streamTask`).
 */

import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

import { IncodeError, IncodeTimeout, ProtocolError, TaskFailed } from "./errors.js";

export type IncodePath = string | readonly string[];

export interface TaskUsage {
  inputTokens: number;
  outputTokens: number;
  cacheCreationTokens: number;
  cacheReadTokens: number;
}

/** The `result` object `incode -t ... --output-format json` prints (src/task_run.rs). */
export interface TaskResult {
  isError: boolean;
  /** The agent's final answer, when the run succeeded. */
  result: string | null;
  /** Why it did not. */
  error: string | null;
  sessionId: string;
  agent: string | null;
  model: string | null;
  agentSessionId: string | null;
  stopReason: string | null;
  servedBy: string[];
  usage: TaskUsage;
  /** `null` for a model incode has no price for. */
  costUsd: number | null;
  durationMs: number | null;
  specGate: string | null;
  exitCode: number | null;
  stderr: string;
  /** The whole object as printed. */
  raw: Record<string, unknown>;
}

export interface RunTaskOptions {
  /** Workspace root (`-w`). Default: incode's current directory. */
  workspace?: string;
  /** Model (`-m`). */
  model?: string;
  /**
   * Permission mode (`-p`). Note: incode 0.1.0 accepts `-p` with `-t` but does
   * not apply it; the one-shot run uses the agent CLI's own handling.
   */
  permissionMode?: string;
  /** Kill the run after this many milliseconds and reject with IncodeTimeout. */
  timeoutMs?: number;
  /** The incode executable, or a command prefix such as `["python3", "fake_incode.py"]`. Default `"incode"`. */
  incodePath?: IncodePath;
  /** Extra environment variables, on top of `process.env`. */
  env?: Record<string, string>;
  /** Extra arguments, appended last (for example `["--spec-gate=off"]`). */
  extraArgs?: readonly string[];
  /** Abort the run (the process is killed). */
  signal?: AbortSignal;
}

function asString(v: unknown): string | null {
  return typeof v === "string" ? v : null;
}

function asNumber(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/** Parse one result object. */
export function parseTaskResult(data: Record<string, unknown>, exitCode: number | null = 0, stderr = ""): TaskResult {
  const usage = (data["usage"] && typeof data["usage"] === "object" ? data["usage"] : {}) as Record<string, unknown>;
  return {
    isError: data["is_error"] === true,
    result: asString(data["result"]),
    error: asString(data["error"]),
    sessionId: asString(data["session_id"]) ?? "",
    agent: asString(data["agent"]),
    model: asString(data["model"]),
    agentSessionId: asString(data["agent_session_id"]),
    stopReason: asString(data["stop_reason"]),
    servedBy: Array.isArray(data["served_by"]) ? data["served_by"].filter((x): x is string => typeof x === "string") : [],
    usage: {
      inputTokens: asNumber(usage["input_tokens"]) ?? 0,
      outputTokens: asNumber(usage["output_tokens"]) ?? 0,
      cacheCreationTokens: asNumber(usage["cache_creation_tokens"]) ?? 0,
      cacheReadTokens: asNumber(usage["cache_read_tokens"]) ?? 0,
    },
    costUsd: asNumber(data["cost_usd"]),
    durationMs: asNumber(data["duration_ms"]),
    specGate: asString(data["spec_gate"]),
    exitCode,
    stderr,
    raw: data,
  };
}

/** The command line `runTask` (`json`) or `streamTask` (`stream-json`) runs, as `[command, ...args]`. */
export function buildTaskArgv(
  task: string,
  options: RunTaskOptions = {},
  outputFormat: "json" | "stream-json" = "json",
): string[] {
  const argv = commandOf(options.incodePath);
  argv.push("-t", task, "--output-format", outputFormat);
  if (options.workspace !== undefined) argv.push("-w", options.workspace);
  if (options.model) argv.push("-m", options.model);
  if (options.permissionMode) argv.push("-p", options.permissionMode);
  argv.push(...(options.extraArgs ?? []));
  return argv;
}

export function commandOf(incodePath: IncodePath | undefined): string[] {
  if (incodePath === undefined) return ["incode"];
  return typeof incodePath === "string" ? [incodePath] : [...incodePath];
}

function lastJsonObject(stdout: string): Record<string, unknown> | null {
  const lines = stdout.split(/\r?\n/).filter((l) => l.trim() !== "");
  const last = lines[lines.length - 1];
  if (last === undefined) return null;
  try {
    const value: unknown = JSON.parse(last);
    return value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

/**
 * Run one instruction with `incode -t` and resolve with its parsed result.
 *
 * Rejects with `TaskFailed` when incode reports `is_error: true` (the parsed
 * result is on `.result`), `IncodeTimeout` after `timeoutMs`, and `IncodeError`
 * when incode cannot start or exits without a result object.
 */
export function runTask(task: string, options: RunTaskOptions = {}): Promise<TaskResult> {
  const [command, ...args] = buildTaskArgv(task, options);
  return new Promise<TaskResult>((resolve, reject) => {
    let stdout = "";
    let stderr = "";
    let settled = false;
    let timedOut = false;
    let aborted = false;
    const child = spawn(command as string, args, {
      stdio: ["ignore", "pipe", "pipe"],
      env: options.env ? { ...process.env, ...options.env } : process.env,
    });
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (d: string) => (stdout += d));
    child.stderr.on("data", (d: string) => (stderr += d));

    const timer =
      options.timeoutMs !== undefined
        ? setTimeout(() => {
            timedOut = true;
            child.kill("SIGKILL");
          }, options.timeoutMs)
        : undefined;
    const onAbort = (): void => {
      aborted = true;
      child.kill("SIGKILL");
    };
    options.signal?.addEventListener("abort", onAbort, { once: true });

    const finish = (fn: () => void): void => {
      if (settled) return;
      settled = true;
      if (timer) clearTimeout(timer);
      options.signal?.removeEventListener("abort", onAbort);
      fn();
    };

    child.on("error", (err: NodeJS.ErrnoException) => {
      finish(() => reject(new IncodeError(`could not start incode: ${err.message} (${command})`, { stderr })));
    });
    child.on("close", (code) => {
      finish(() => {
        if (timedOut) return reject(new IncodeTimeout(`incode -t did not finish within ${options.timeoutMs}ms`, { stderr }));
        if (aborted) return reject(new IncodeError("incode -t was aborted", { stderr }));
        const data = lastJsonObject(stdout);
        if (data === null) {
          if (code !== 0) return reject(new IncodeError("incode -t failed before producing a result", { stderr, exitCode: code }));
          return reject(
            new ProtocolError(`incode -t printed no JSON result object (stdout: ${JSON.stringify(stdout.slice(-500))})`, {
              stderr,
              exitCode: code,
            }),
          );
        }
        const result = parseTaskResult(data, code, stderr);
        if (result.isError || code !== 0) {
          return reject(
            new TaskFailed(`incode -t reported an error: ${result.error ?? "unknown error"}`, result, { stderr, exitCode: code }),
          );
        }
        resolve(result);
      });
    });
    if (options.signal?.aborted) onAbort();
  });
}

/**
 * One line of `incode -t ... --output-format stream-json` before the result:
 * `type` is `init`, `text`, `thinking`, `notice`, `tool_use`, `tool_result` or
 * `usage` (src/task_run.rs `Report::event`); `data` is the whole object.
 */
export interface TaskEvent {
  type: string;
  data: Record<string, unknown>;
}

/**
 * A `stream-json` run. Iterate it (`for await`) for its events as they arrive;
 * `result` resolves with the run's `TaskResult`, or rejects as `runTask` would
 * (`TaskFailed`, `IncodeTimeout`, `IncodeError`, `ProtocolError`). Iterating to
 * the end throws that same error.
 */
export interface TaskStream extends AsyncIterable<TaskEvent> {
  readonly result: Promise<TaskResult>;
}

/** Run one instruction with `incode -t ... --output-format stream-json`. Takes `runTask`'s options. */
export function streamTask(task: string, options: RunTaskOptions = {}): TaskStream {
  const [command, ...args] = buildTaskArgv(task, options, "stream-json");
  const queue: TaskEvent[] = [];
  let wake: (() => void) | null = null;
  let finished = false;
  let stderr = "";
  let timedOut = false;
  let aborted = false;
  let notJson: string | null = null;
  let resultData: Record<string, unknown> | null = null;
  let spawnError: NodeJS.ErrnoException | null = null;

  const child = spawn(command as string, args, {
    stdio: ["ignore", "pipe", "pipe"],
    env: options.env ? { ...process.env, ...options.env } : process.env,
  });
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (d: string) => (stderr += d));
  const lines = createInterface({ input: child.stdout, crlfDelay: Infinity });
  lines.on("line", (raw: string) => {
    const line = raw.trim();
    if (line === "" || notJson !== null) return;
    let value: unknown;
    try {
      value = JSON.parse(line);
    } catch {
      notJson = line;
      child.kill("SIGKILL");
      return;
    }
    if (!value || typeof value !== "object" || Array.isArray(value)) return;
    const data = value as Record<string, unknown>;
    if (data["type"] === "result") {
      resultData = data;
      return;
    }
    queue.push({ type: typeof data["type"] === "string" ? data["type"] : "", data });
    wake?.();
  });

  const timer =
    options.timeoutMs !== undefined
      ? setTimeout(() => {
          timedOut = true;
          child.kill("SIGKILL");
        }, options.timeoutMs)
      : undefined;
  const onAbort = (): void => {
    aborted = true;
    child.kill("SIGKILL");
  };
  options.signal?.addEventListener("abort", onAbort, { once: true });
  if (options.signal?.aborted) onAbort();

  const exited = new Promise<number | null>((resolve) => {
    child.on("error", (err: NodeJS.ErrnoException) => {
      spawnError = err;
      resolve(null);
    });
    child.on("close", (code) => resolve(code));
  });

  const result = exited.then((code): TaskResult => {
    finished = true;
    if (timer) clearTimeout(timer);
    options.signal?.removeEventListener("abort", onAbort);
    wake?.();
    if (spawnError) throw new IncodeError(`could not start incode: ${spawnError.message} (${command})`, { stderr });
    if (timedOut) throw new IncodeTimeout(`incode -t did not finish within ${options.timeoutMs}ms`, { stderr });
    if (aborted) throw new IncodeError("incode -t was aborted", { stderr });
    if (notJson !== null) {
      throw new ProtocolError(`incode -t printed a line that is not JSON: ${JSON.stringify((notJson as string).slice(0, 500))}`, {
        stderr,
        exitCode: code,
      });
    }
    if (resultData === null) {
      if (code !== 0) throw new IncodeError("incode -t failed before producing a result", { stderr, exitCode: code });
      throw new ProtocolError("incode -t printed no JSON result object", { stderr, exitCode: code });
    }
    const parsed = parseTaskResult(resultData, code, stderr);
    if (parsed.isError || code !== 0) {
      throw new TaskFailed(`incode -t reported an error: ${parsed.error ?? "unknown error"}`, parsed, {
        stderr,
        exitCode: code,
      });
    }
    return parsed;
  });
  // Handled here so a caller that only iterates is not warned of an
  // unhandled rejection; `result` itself still rejects for whoever awaits it.
  result.catch(() => undefined);

  return {
    result,
    async *[Symbol.asyncIterator](): AsyncIterator<TaskEvent> {
      for (;;) {
        const next = queue.shift();
        if (next !== undefined) {
          yield next;
          continue;
        }
        if (finished) break;
        await new Promise<void>((resolve) => (wake = resolve));
        wake = null;
      }
      await result;
    },
  };
}
