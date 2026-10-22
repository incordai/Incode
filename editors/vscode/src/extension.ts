import * as vscode from "vscode";
import { ChatViewProvider, VIEW_ID } from "./chatView";
import { Controller } from "./controller";
import { formatSelection } from "./selection";

export function activate(context: vscode.ExtensionContext): void {
  const output = vscode.window.createOutputChannel("incode");
  const version = String((context.extension.packageJSON as { version?: string }).version ?? "0.0.0");
  const controller = new Controller(output, version);
  const provider = new ChatViewProvider(context.extensionUri, controller);
  controller.attach(provider);

  context.subscriptions.push(
    output,
    controller,
    vscode.window.registerWebviewViewProvider(VIEW_ID, provider, { webviewOptions: { retainContextWhenHidden: true } }),
    vscode.commands.registerCommand("incode.openChat", () => provider.reveal()),
    vscode.commands.registerCommand("incode.newSession", async () => {
      await provider.reveal();
      await controller.newSession();
    }),
    vscode.commands.registerCommand("incode.openSession", async () => {
      await provider.reveal();
      await controller.pickSession();
    }),
    vscode.commands.registerCommand("incode.restart", async () => {
      await provider.reveal();
      await controller.restart();
    }),
    vscode.commands.registerCommand("incode.sendSelection", async () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor) {
        void vscode.window.showInformationMessage("incode: no active editor.");
        return;
      }
      const doc = editor.document;
      const sel = editor.selection;
      const range = sel.isEmpty ? doc.lineAt(sel.active.line).range : sel;
      const text = formatSelection({
        path: vscode.workspace.asRelativePath(doc.uri, false),
        languageId: doc.languageId,
        startLine: range.start.line + 1,
        endLine: range.end.character === 0 && range.end.line > range.start.line ? range.end.line : range.end.line + 1,
        text: doc.getText(range),
      });
      await provider.reveal();
      // Into the input, or sent at once with `incode.sendSelectionImmediately`.
      await controller.sendSelection(text);
    }),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("incode.path") || e.affectsConfiguration("incode.args")) {
        void vscode.window
          .showInformationMessage("incode: restart the agent to use the new settings.", "Restart")
          .then((choice) => (choice === "Restart" ? controller.restart() : undefined));
      }
    }),
  );
}

export function deactivate(): void {
  // The controller is disposed with the subscriptions, which stops the agent.
}
