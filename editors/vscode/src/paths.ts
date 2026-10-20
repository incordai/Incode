/**
 * Which files the agent may read and write through the editor: only those
 * inside one of the workspace folders. Pure functions, no `vscode`, so the
 * rule is tested on its own.
 */

import { promises as fs } from "node:fs";
import * as nodePath from "node:path";

export type Platform = "posix" | "win32";

function api(platform: Platform): nodePath.PlatformPath {
  return platform === "win32" ? nodePath.win32 : nodePath.posix;
}

export function currentPlatform(): Platform {
  return process.platform === "win32" ? "win32" : "posix";
}

/**
 * Whether `candidate` lies inside `root` (or is `root` itself). Both must be
 * absolute; they are normalised lexically (`..` and `.` resolved) first.
 * Windows compares case-insensitively.
 */
export function isInside(root: string, candidate: string, platform: Platform = currentPlatform()): boolean {
  const p = api(platform);
  if (!p.isAbsolute(root) || !p.isAbsolute(candidate)) return false;
  let r = p.resolve(root);
  let c = p.resolve(candidate);
  if (platform === "win32") {
    r = r.toLowerCase();
    c = c.toLowerCase();
  }
  const rel = p.relative(r, c);
  if (rel === "") return true;
  // `..foo` is a name inside root; `..` and `../x` (or `..\x`) are not.
  const escapes = rel === ".." || rel.startsWith(`..${p.sep}`);
  return !escapes && !p.isAbsolute(rel);
}

export type PathCheck = { ok: true; path: string; root: string } | { ok: false; reason: string };

/**
 * Decide whether the agent may touch `requested`. It must be an absolute path
 * (ACP requires absolute paths) inside one of `roots`. Returns the normalised
 * path, or why it was refused.
 */
export function checkWorkspacePath(requested: string, roots: readonly string[], platform: Platform = currentPlatform()): PathCheck {
  const p = api(platform);
  if (typeof requested !== "string" || requested.length === 0) return { ok: false, reason: "no path given" };
  if (requested.includes("\0")) return { ok: false, reason: "the path contains a NUL byte" };
  if (!p.isAbsolute(requested)) return { ok: false, reason: `\`${requested}\` is not an absolute path` };
  if (roots.length === 0) return { ok: false, reason: "no workspace folder is open" };
  const normalised = p.resolve(requested);
  for (const root of roots) {
    if (isInside(root, normalised, platform)) return { ok: true, path: normalised, root };
  }
  return { ok: false, reason: `\`${requested}\` is outside the workspace folders` };
}

/**
 * {@link checkWorkspacePath}, then the same check on the path with symlinks
 * resolved: a link inside the workspace that points outside it is refused.
 * For a path that does not exist yet, its nearest existing ancestor is
 * resolved and the rest appended.
 */
export async function checkWorkspacePathReal(
  requested: string,
  roots: readonly string[],
  realpath: (p: string) => Promise<string> = (p) => fs.realpath(p),
  platform: Platform = currentPlatform(),
): Promise<PathCheck> {
  const lexical = checkWorkspacePath(requested, roots, platform);
  if (!lexical.ok) return lexical;
  const p = api(platform);
  const realRoots: string[] = [];
  for (const root of roots) {
    try {
      realRoots.push(await realpath(root));
    } catch {
      realRoots.push(root);
    }
  }
  let existing = lexical.path;
  const rest: string[] = [];
  for (;;) {
    try {
      const real = await realpath(existing);
      const full = rest.length ? p.join(real, ...rest.reverse()) : real;
      const again = checkWorkspacePath(full, realRoots, platform);
      return again.ok ? lexical : { ok: false, reason: `\`${requested}\` resolves outside the workspace folders` };
    } catch {
      const parent = p.dirname(existing);
      if (parent === existing) return lexical;
      rest.push(p.basename(existing));
      existing = parent;
    }
  }
}

/**
 * The part of `text` that `fs/read_text_file` asked for: from 1-based `line`,
 * at most `limit` lines. Line endings are kept as they are.
 */
export function sliceLines(text: string, line?: number | null, limit?: number | null): string {
  if ((line === undefined || line === null) && (limit === undefined || limit === null)) return text;
  const start = Math.max(1, Math.floor(line ?? 1)) - 1;
  const lines = text.split(/(?<=\n)/);
  const end = limit === undefined || limit === null ? lines.length : start + Math.max(0, Math.floor(limit));
  return lines.slice(start, end).join("");
}
