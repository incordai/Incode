/**
 * The chat webview: a view in the incode activity-bar container.
 *
 * Security: a strict Content-Security-Policy (no inline script except the one
 * nonce'd loader, no remote anything), local resources limited to `media/`,
 * and the page never turns model output into HTML (see media/markdown.js).
 */

import * as crypto from "node:crypto";
import * as vscode from "vscode";
import type { Controller, FromView, ToView, View } from "./controller";

export const VIEW_ID = "incode.chat";

export class ChatViewProvider implements vscode.WebviewViewProvider, View {
  private view?: vscode.WebviewView;

  constructor(
    private readonly extensionUri: vscode.Uri,
    private readonly controller: Controller,
  ) {}

  resolveWebviewView(view: vscode.WebviewView): void {
    this.view = view;
    const media = vscode.Uri.joinPath(this.extensionUri, "media");
    view.webview.options = { enableScripts: true, localResourceRoots: [media] };
    view.webview.html = this.html(view.webview, media);
    view.webview.onDidReceiveMessage((m: FromView) => {
      if (!m || typeof m !== "object" || typeof (m as { type?: unknown }).type !== "string") return;
      void this.controller.onMessage(m);
    });
    view.onDidDispose(() => {
      if (this.view === view) this.view = undefined;
    });
  }

  post(message: ToView): void {
    void this.view?.webview.postMessage(message);
  }

  async reveal(): Promise<void> {
    if (this.view) this.view.show(true);
    else await vscode.commands.executeCommand(`${VIEW_ID}.focus`);
  }

  private html(webview: vscode.Webview, media: vscode.Uri): string {
    const nonce = crypto.randomBytes(16).toString("base64");
    return pageHtml(webview.cspSource, nonce, (f) => webview.asWebviewUri(vscode.Uri.joinPath(media, f)).toString());
  }
}

/**
 * The page's Content-Security-Policy. Everything is `'none'` but what the
 * page loads from `media/` and its two nonce'd scripts. `img-src` also allows
 * `data:`, and only that: the thumbnail of a picture attached to the input
 * is its own bytes as a `data:` URL. An image cannot run script, and
 * `connect-src`, `frame-src` and the rest stay `'none'`, so nothing the page
 * shows can reach out.
 */
export function contentSecurityPolicy(cspSource: string, nonce: string): string {
  return [
    "default-src 'none'",
    `style-src ${cspSource}`,
    `font-src ${cspSource}`,
    `img-src ${cspSource} data:`,
    `script-src 'nonce-${nonce}'`,
  ].join("; ");
}

/** The HTML shell; media/chat.js fills it. `uri` maps a file in `media/` to the webview's URI for it. */
export function pageHtml(cspSource: string, nonce: string, uri: (file: string) => string): string {
  const csp = contentSecurityPolicy(cspSource, nonce);
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<link rel="stylesheet" href="${uri("chat.css")}">
<title>incode</title>
</head>
<body>
<header id="bar">
  <label class="picker" id="mode-wrap" hidden>Mode <select id="mode"></select></label>
  <span id="config"></span>
  <span id="usage" title="Context window"></span>
</header>
<div id="status" role="status"></div>
<main id="log" aria-live="polite"></main>
<section id="plan" hidden></section>
<footer id="composer">
  <ul id="completions" role="listbox" hidden></ul>
  <div id="attachments" hidden></div>
  <textarea id="input" rows="3" placeholder="Ask incode… (/ for commands, Enter to send, Shift+Enter for a new line)"></textarea>
  <div class="actions">
    <button id="attach" type="button" class="secondary" title="Attach a PNG or JPEG picture (you can also paste or drop one)" hidden>Image</button>
    <button id="send" type="button">Send</button>
    <button id="stop" type="button" class="secondary" hidden>Stop</button>
  </div>
</footer>
<script nonce="${nonce}" src="${uri("markdown.js")}"></script>
<script nonce="${nonce}" src="${uri("chat.js")}"></script>
</body>
</html>`;
}
