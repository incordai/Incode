/**
 * The client's file system as offered to incode (`fs/read_text_file`,
 * `fs/write_text_file`). The default handlers serve the workspace only: a path
 * is resolved first — symlinks followed, `..` taken as the OS takes it — and
 * refused when its real path is not under the workspace's real path.
 */

import * as fs from "node:fs";
import * as path from "node:path";

import type { ReadFileRequest, WriteFileRequest } from "./events.js";

export const INVALID_PARAMS = -32602;
export const RESOURCE_NOT_FOUND = -32002;
export const INTERNAL_ERROR = -32603;

/** Throw from an fs callback to answer with this JSON-RPC error. */
export class FsError extends Error {
  constructor(
    message: string,
    readonly code: number = INTERNAL_ERROR,
  ) {
    super(message);
    this.name = new.target.name;
  }
}

/** The path is outside the workspace (after resolving symlinks and `..`). */
export class FsAccessDenied extends FsError {
  constructor(message: string) {
    super(message, INVALID_PARAMS);
  }
}

/**
 * The real path of `p`: the longest prefix that exists is resolved by the OS
 * (`realpath(3)`, so a symlink followed by `..` means what it means on disk),
 * and the rest — which does not exist yet — is appended and normalised.
 */
export function realPath(p: string): string {
  const absolute = path.isAbsolute(p) ? p : path.join(process.cwd(), p);
  const { root } = path.parse(absolute);
  const parts = absolute.slice(root.length).split(/[\\/]+/).filter((x) => x !== "");
  for (let i = parts.length; i >= 0; i--) {
    const prefix = root + parts.slice(0, i).join(path.sep);
    let real: string;
    try {
      real = fs.realpathSync.native(prefix);
    } catch {
      continue;
    }
    return path.resolve(real, ...parts.slice(i));
  }
  return path.resolve(absolute);
}

/** Reads and writes confined to one directory tree. */
export class WorkspaceFs {
  readonly root: string;

  constructor(root: string) {
    this.root = realPath(path.resolve(root));
  }

  contains(real: string): boolean {
    const rel = path.relative(this.root, real);
    return rel === "" || (rel !== ".." && !rel.startsWith(".." + path.sep) && !path.isAbsolute(rel));
  }

  /** The real path of `p`, or `FsAccessDenied` when it is outside the root. */
  resolve(p: string): string {
    if (typeof p !== "string" || p === "") throw new FsAccessDenied("access denied: no path given");
    if (p.includes("\0")) throw new FsAccessDenied("access denied: the path contains a NUL byte");
    // ACP paths are absolute; a relative one is taken against the root.
    const absolute = path.isAbsolute(p) ? p : this.root + path.sep + p;
    const real = realPath(absolute);
    if (!this.contains(real)) throw new FsAccessDenied(`access denied: ${p} is outside the workspace ${this.root}`);
    return real;
  }

  async read(request: ReadFileRequest): Promise<string> {
    const real = this.resolve(request.path);
    let text: string;
    try {
      text = await fs.promises.readFile(real, "utf8");
    } catch (e) {
      const code = (e as NodeJS.ErrnoException).code;
      if (code === "ENOENT") throw new FsError(`file not found: ${request.path}`, RESOURCE_NOT_FOUND);
      if (code === "EISDIR") throw new FsError(`is a directory: ${request.path}`, INVALID_PARAMS);
      throw e;
    }
    return sliceLines(text, request.line, request.limit);
  }

  async write(request: WriteFileRequest): Promise<void> {
    let real = this.resolve(request.path);
    await fs.promises.mkdir(path.dirname(real), { recursive: true });
    // Again, now that the parents exist: nothing may have become a symlink
    // out of the tree in between.
    real = this.resolve(real);
    await fs.promises.writeFile(real, request.content, "utf8");
  }
}

/** Lines `line` (1-based) onward, at most `limit` of them. */
export function sliceLines(text: string, line: number | null, limit: number | null): string {
  if (line === null && limit === null) return text;
  const lines = text.match(/[^\n]*\n|[^\n]+$/g) ?? [];
  const start = Math.max((line ?? 1) - 1, 0);
  const end = limit === null ? lines.length : start + Math.max(limit, 0);
  return lines.slice(start, end).join("");
}
