import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

// Compiled to dist-test/test/support.js; the fake lives in sdk/testing/.
export const FAKE_INCODE = fileURLToPath(new URL("../../../testing/fake_incode.py", import.meta.url));
/** Run through python3 so the tests do not depend on the script's executable bit. */
export const FAKE: string[] = [process.env["PYTHON"] ?? "python3", FAKE_INCODE];

export interface TempDirs {
  root: string;
  ws: string;
  cleanup(): void;
}

/** A real temp root with a `ws` workspace inside it. */
export function tempDirs(): TempDirs {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "incode-sdk-")));
  const ws = path.join(root, "ws");
  fs.mkdirSync(ws);
  return { root, ws, cleanup: () => fs.rmSync(root, { recursive: true, force: true }) };
}

export async function waitFor(check: () => boolean, ms = 5000): Promise<void> {
  const deadline = Date.now() + ms;
  while (!check()) {
    if (Date.now() > deadline) throw new Error("timed out waiting");
    await new Promise((r) => setTimeout(r, 20));
  }
}
