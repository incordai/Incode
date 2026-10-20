import { strict as assert } from "node:assert";
import { promises as fs } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { describe, it } from "node:test";
import { checkWorkspacePath, checkWorkspacePathReal, isInside, sliceLines } from "../src/paths";
import { formatSelection } from "../src/selection";

describe("checkWorkspacePath (posix)", () => {
  const roots = ["/home/me/proj", "/srv/other"];
  const ok = (p: string) => checkWorkspacePath(p, roots, "posix").ok;

  it("allows files in any workspace folder, and the folder itself", () => {
    assert.equal(ok("/home/me/proj/src/main.rs"), true);
    assert.equal(ok("/home/me/proj"), true);
    assert.equal(ok("/srv/other/a"), true);
    assert.equal(ok("/home/me/proj/..foo/x"), true, "`..foo` is a name, not a parent");
  });

  it("refuses paths outside, including by `..` and prefix tricks", () => {
    assert.equal(ok("/etc/passwd"), false);
    assert.equal(ok("/home/me/proj/../secret"), false);
    assert.equal(ok("/home/me/proj-evil/x"), false, "a sibling sharing the prefix is outside");
    assert.equal(ok("/home/me"), false);
    assert.equal(ok("/home/me/proj/a/../../proj2/x"), false);
  });

  it("refuses relative paths, empty paths, NUL bytes, and everything when no folder is open", () => {
    assert.equal(ok("src/main.rs"), false);
    assert.equal(ok(""), false);
    assert.equal(ok("/home/me/proj/a\0b"), false);
    assert.equal(checkWorkspacePath("/home/me/proj/x", [], "posix").ok, false);
  });

  it("normalises the path it allows", () => {
    const r = checkWorkspacePath("/home/me/proj/./a/../b.txt", roots, "posix");
    assert.deepEqual(r, { ok: true, path: "/home/me/proj/b.txt", root: "/home/me/proj" });
  });
});

describe("checkWorkspacePath (win32)", () => {
  it("compares case-insensitively and across drives", () => {
    const roots = ["C:\\Users\\me\\proj"];
    assert.equal(checkWorkspacePath("c:\\users\\ME\\proj\\src\\a.ts", roots, "win32").ok, true);
    assert.equal(checkWorkspacePath("D:\\Users\\me\\proj\\a.ts", roots, "win32").ok, false);
    assert.equal(checkWorkspacePath("C:\\Users\\me\\proj\\..\\x", roots, "win32").ok, false);
    assert.equal(isInside("C:\\a", "C:\\ab", "win32"), false);
  });
});

describe("checkWorkspacePathReal", () => {
  it("refuses a symlink inside the workspace that points outside", async (t) => {
    const base = await fs.mkdtemp(path.join(os.tmpdir(), "incode-vscode-"));
    t.after(() => fs.rm(base, { recursive: true, force: true }));
    const ws = path.join(base, "ws");
    const outside = path.join(base, "outside");
    await fs.mkdir(ws);
    await fs.mkdir(outside);
    await fs.writeFile(path.join(ws, "in.txt"), "x");
    try {
      await fs.symlink(outside, path.join(ws, "link"), "dir");
    } catch {
      t.skip("symlinks are not available here");
      return;
    }
    assert.equal((await checkWorkspacePathReal(path.join(ws, "in.txt"), [ws])).ok, true);
    assert.equal((await checkWorkspacePathReal(path.join(ws, "new", "deep.txt"), [ws])).ok, true, "a new file");
    const viaLink = await checkWorkspacePathReal(path.join(ws, "link", "secret.txt"), [ws]);
    assert.equal(viaLink.ok, false);
    assert.match(!viaLink.ok ? viaLink.reason : "", /resolves outside/);
  });
});

describe("sliceLines", () => {
  const text = "a\nb\nc\nd";
  it("returns everything without line/limit", () => assert.equal(sliceLines(text), text));
  it("is 1-based", () => assert.equal(sliceLines(text, 2), "b\nc\nd"));
  it("honours limit", () => assert.equal(sliceLines(text, 2, 2), "b\nc\n"));
  it("limit alone starts at the top", () => assert.equal(sliceLines(text, null, 1), "a\n"));
  it("past the end is empty", () => assert.equal(sliceLines(text, 10), ""));
});

describe("formatSelection", () => {
  it("names the file and lines and fences the code", () => {
    assert.equal(
      formatSelection({ path: "src/a.ts", languageId: "typescript", startLine: 3, endLine: 4, text: "x\ny" }),
      "src/a.ts:3-4\n```typescript\nx\ny\n```\n",
    );
    assert.equal(
      formatSelection({ path: "a.md", languageId: "markdown", startLine: 1, endLine: 1, text: "```js\n```\n" }),
      "a.md:1\n````markdown\n```js\n```\n````\n",
    );
  });
});
