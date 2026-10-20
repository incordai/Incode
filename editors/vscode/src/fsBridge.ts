/**
 * `fs/read_text_file` and `fs/write_text_file`, answered through the editor.
 *
 * - A read returns the open editor buffer when the file is open (unsaved
 *   changes included), and the file on disk otherwise.
 * - A write is a WorkspaceEdit: it shows in the editor and can be undone. The
 *   document is then saved so the agent, which reads the disk too, sees it.
 * - Both are refused for any path outside the workspace folders, symlinks
 *   resolved ({@link checkWorkspacePathReal}).
 */

import * as vscode from "vscode";
import { AcpError, ErrorCodes, ReadTextFileParams, WriteTextFileParams } from "./acp";
import { checkWorkspacePathReal, currentPlatform, sliceLines } from "./paths";

export function workspaceRoots(): string[] {
  return (vscode.workspace.workspaceFolders ?? []).filter((f) => f.uri.scheme === "file").map((f) => f.uri.fsPath);
}

async function allowed(requested: string): Promise<string> {
  const check = await checkWorkspacePathReal(requested, workspaceRoots());
  if (!check.ok) throw new AcpError(ErrorCodes.InvalidParams, "Invalid params", `refused: ${check.reason}`);
  return check.path;
}

function samePath(a: string, b: string): boolean {
  return currentPlatform() === "win32" ? a.toLowerCase() === b.toLowerCase() : a === b;
}

function openDocument(fsPath: string): vscode.TextDocument | undefined {
  return vscode.workspace.textDocuments.find((d) => d.uri.scheme === "file" && samePath(d.uri.fsPath, fsPath));
}

async function exists(uri: vscode.Uri): Promise<boolean> {
  try {
    await vscode.workspace.fs.stat(uri);
    return true;
  } catch {
    return false;
  }
}

export async function readTextFile(params: ReadTextFileParams): Promise<{ content: string }> {
  const fsPath = await allowed(params.path);
  const open = openDocument(fsPath);
  let text: string;
  if (open) {
    text = open.getText();
  } else {
    try {
      const bytes = await vscode.workspace.fs.readFile(vscode.Uri.file(fsPath));
      text = new TextDecoder("utf-8").decode(bytes);
    } catch (e) {
      throw new AcpError(ErrorCodes.ResourceNotFound, "Resource not found", `${params.path}: ${(e as Error).message}`);
    }
  }
  return { content: sliceLines(text, params.line, params.limit) };
}

export async function writeTextFile(params: WriteTextFileParams): Promise<void> {
  const fsPath = await allowed(params.path);
  const uri = vscode.Uri.file(fsPath);
  const edit = new vscode.WorkspaceEdit();
  const metadata = { needsConfirmation: false, label: "incode: write file" };
  if (await exists(uri)) {
    const doc = await vscode.workspace.openTextDocument(uri);
    const all = new vscode.Range(doc.positionAt(0), doc.positionAt(doc.getText().length));
    edit.replace(uri, all, params.content, metadata);
  } else {
    edit.createFile(uri, { ignoreIfExists: true }, metadata);
    edit.insert(uri, new vscode.Position(0, 0), params.content, metadata);
  }
  if (!(await vscode.workspace.applyEdit(edit))) {
    throw new AcpError(ErrorCodes.InternalError, "Internal error", `the editor did not apply the write to ${params.path}`);
  }
  const doc = await vscode.workspace.openTextDocument(uri);
  if (doc.isDirty && !(await doc.save())) {
    throw new AcpError(ErrorCodes.InternalError, "Internal error", `the edit to ${params.path} was applied but could not be saved`);
  }
}
