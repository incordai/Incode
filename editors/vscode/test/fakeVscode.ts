/**
 * Just enough of the `vscode` module to run the controller and the fs bridge
 * in plain Node. Installed with {@link installFakeVscode} before they are
 * required.
 */
import { promises as fs } from "node:fs";
import Module = require("node:module");
import * as path from "node:path";

export class Position {
  constructor(
    readonly line: number,
    readonly character: number,
  ) {}
}
export class Range {
  constructor(
    readonly start: Position,
    readonly end: Position,
  ) {}
}
export class Uri {
  private constructor(
    readonly scheme: string,
    readonly fsPath: string,
    private readonly raw: string,
  ) {}
  static file(p: string): Uri {
    return new Uri("file", path.resolve(p), `file://${p}`);
  }
  static parse(s: string): Uri {
    const m = /^([a-z][a-z0-9+.-]*):/i.exec(s);
    if (!m) throw new Error("no scheme");
    return new Uri(m[1].toLowerCase(), "", s);
  }
  toString(): string {
    return this.raw;
  }
}

/** An open document: text in memory, saved to disk on save(). */
export class FakeDocument {
  readonly uri: Uri;
  isDirty = false;
  constructor(
    fsPath: string,
    public text: string,
  ) {
    this.uri = Uri.file(fsPath);
  }
  getText(): string {
    return this.text;
  }
  positionAt(offset: number): Position {
    const before = this.text.slice(0, offset).split("\n");
    return new Position(before.length - 1, before[before.length - 1].length);
  }
  offsetAt(p: Position): number {
    const lines = this.text.split("\n");
    let o = 0;
    for (let i = 0; i < p.line; i++) o += lines[i].length + 1;
    return o + p.character;
  }
  async save(): Promise<boolean> {
    await fs.mkdir(path.dirname(this.uri.fsPath), { recursive: true });
    await fs.writeFile(this.uri.fsPath, this.text);
    this.isDirty = false;
    return true;
  }
}

type Op = { kind: "create"; uri: Uri } | { kind: "replace"; uri: Uri; range: Range; text: string } | { kind: "insert"; uri: Uri; at: Position; text: string };

export class WorkspaceEdit {
  readonly ops: Op[] = [];
  createFile(uri: Uri): void {
    this.ops.push({ kind: "create", uri });
  }
  replace(uri: Uri, range: Range, text: string): void {
    this.ops.push({ kind: "replace", uri, range, text });
  }
  insert(uri: Uri, at: Position, text: string): void {
    this.ops.push({ kind: "insert", uri, at, text });
  }
}

export interface FakeState {
  roots: string[];
  config: Record<string, unknown>;
  documents: FakeDocument[];
  applied: WorkspaceEdit[];
  opened: string[];
}

export function makeFakeVscode(state: FakeState): Record<string, unknown> {
  const findDoc = (p: string) => state.documents.find((d) => d.uri.fsPath === p);
  const workspace = {
    get workspaceFolders() {
      return state.roots.map((r, index) => ({ uri: Uri.file(r), name: path.basename(r), index }));
    },
    get textDocuments() {
      return state.documents;
    },
    getConfiguration: () => ({ get: (k: string) => state.config[k] }),
    fs: {
      stat: async (uri: Uri) => fs.stat(uri.fsPath),
      readFile: async (uri: Uri) => new Uint8Array(await fs.readFile(uri.fsPath)),
    },
    openTextDocument: async (uri: Uri) => {
      let doc = findDoc(uri.fsPath);
      if (!doc) {
        doc = new FakeDocument(uri.fsPath, await fs.readFile(uri.fsPath, "utf8").catch(() => ""));
        state.documents.push(doc);
      }
      return doc;
    },
    applyEdit: async (edit: WorkspaceEdit) => {
      state.applied.push(edit);
      for (const op of edit.ops) {
        let doc = findDoc(op.uri.fsPath);
        if (op.kind === "create") {
          if (!doc) state.documents.push(new FakeDocument(op.uri.fsPath, ""));
          continue;
        }
        if (!doc) doc = await workspace.openTextDocument(op.uri);
        if (op.kind === "replace") {
          doc.text = doc.text.slice(0, doc.offsetAt(op.range.start)) + op.text + doc.text.slice(doc.offsetAt(op.range.end));
        } else {
          const at = doc.offsetAt(op.at);
          doc.text = doc.text.slice(0, at) + op.text + doc.text.slice(at);
        }
        doc.isDirty = true;
      }
      return true;
    },
  };
  return {
    Position,
    Range,
    Uri,
    WorkspaceEdit,
    workspace,
    env: { openExternal: async (u: Uri) => (state.opened.push(u.toString()), true) },
    commands: { executeCommand: async () => undefined },
    window: {},
  };
}

/** Make `require("vscode")` return `fake` from now on. */
export function installFakeVscode(fake: Record<string, unknown>): void {
  const M = Module as unknown as { _resolveFilename: (req: string, ...rest: unknown[]) => string };
  const original = M._resolveFilename;
  const id = "fake-vscode-module";
  require.cache[id] = { id, filename: id, loaded: true, exports: fake } as unknown as NodeJS.Module;
  M._resolveFilename = function (req: string, ...rest: unknown[]) {
    return req === "vscode" ? id : original.call(this, req, ...rest);
  };
}
