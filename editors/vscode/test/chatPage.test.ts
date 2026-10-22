// The chat page: media/chat.js run in a small fake DOM built from the shell
// src/chatView.ts serves, and that shell's Content-Security-Policy. Covers
// attaching pictures (paste, drop, the Image button, the extension's file
// dialog), their refusals, and what a send carries.
import { strict as assert } from "node:assert";
import { readFileSync } from "node:fs";
import * as path from "node:path";
import { beforeEach, describe, it } from "node:test";
import * as vm from "node:vm";
import type * as ChatViewModule from "../src/chatView";
import { FakeState, installFakeVscode, makeFakeVscode } from "./fakeVscode";

const state: FakeState = { roots: [], config: {}, documents: [], applied: [], opened: [] };
installFakeVscode(makeFakeVscode(state));
// eslint-disable-next-line @typescript-eslint/no-require-imports
const { contentSecurityPolicy, pageHtml } = require("../src/chatView") as typeof ChatViewModule;

const CHAT_JS = readFileSync(path.resolve(__dirname, "..", "..", "media", "chat.js"), "utf8");
const PNG = Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==", "base64");

type Listener = (e: FakeEvent) => void;

class FakeEvent {
  prevented = false;
  constructor(init: Record<string, unknown> = {}) {
    Object.assign(this, init);
  }
  preventDefault(): void {
    this.prevented = true;
  }
}

/** Just enough of an element for chat.js; innerHTML is refused outright. */
class FakeElement {
  children: FakeElement[] = [];
  listeners: Record<string, Listener[]> = {};
  attrs: Record<string, string> = {};
  className = "";
  textContent = "";
  hidden = false;
  disabled = false;
  value = "";
  type = "";
  title = "";
  alt = "";
  src = "";
  id = "";
  scrollTop = 0;
  scrollHeight = 0;
  clientHeight = 0;
  constructor(readonly tagName: string) {}
  get firstChild(): FakeElement | undefined {
    return this.children[0];
  }
  appendChild(c: FakeElement): FakeElement {
    this.children.push(c);
    return c;
  }
  removeChild(c: FakeElement): void {
    this.children = this.children.filter((x) => x !== c);
  }
  setAttribute(k: string, v: string): void {
    this.attrs[k] = v;
  }
  addEventListener(type: string, fn: Listener): void {
    (this.listeners[type] ??= []).push(fn);
  }
  dispatch(type: string, e = new FakeEvent()): FakeEvent {
    for (const fn of this.listeners[type] ?? []) fn(e);
    return e;
  }
  focus(): void {}
  setSelectionRange(): void {}
  get classList() {
    const list = () => this.className.split(/\s+/).filter(Boolean);
    return {
      add: (c: string) => (this.className = [...new Set([...list(), c])].join(" ")),
      toggle: (c: string, on: boolean) => (this.className = (on ? [...new Set([...list(), c])] : list().filter((x) => x !== c)).join(" ")),
      contains: (c: string) => list().includes(c),
    };
  }
  querySelector(sel: string): FakeElement | null {
    const cls = sel.replace(/^\./, "");
    for (const c of this.children) {
      if (c.classList.contains(cls)) return c;
      const deeper = c.querySelector(sel);
      if (deeper) return deeper;
    }
    return null;
  }
  set innerHTML(_v: string) {
    throw new Error("innerHTML must never be used");
  }
  /** Every descendant, depth first. */
  all(): FakeElement[] {
    return this.children.flatMap((c) => [c, ...c.all()]);
  }
}

interface FakeFile {
  name: string;
  type: string;
  size: number;
  bytes: Buffer;
}
const file = (name: string, type: string, bytes: Buffer): FakeFile => ({ name, type, size: bytes.length, bytes });

interface Page {
  byId: Map<string, FakeElement>;
  sent: Record<string, unknown>[];
  reads: string[];
  receive(m: unknown): void;
  tick(): Promise<void>;
  notices(): string[];
  thumbs(): FakeElement[];
}

/** Load the shell's elements (by id, with their `hidden`) and run chat.js against them. */
function openPage(): Page {
  const html = pageHtml("vscode-webview-src", "NONCE", (f) => `media/${f}`);
  const byId = new Map<string, FakeElement>();
  for (const m of html.matchAll(/<(\w+) id="([^"]+)"([^>]*)>/g)) {
    const e = new FakeElement(m[1]);
    e.id = m[2];
    e.hidden = /\bhidden\b/.test(m[3]);
    byId.set(m[2], e);
  }
  const sent: Record<string, unknown>[] = [];
  const reads: string[] = [];
  const onMessage: ((e: { data: unknown }) => void)[] = [];
  class FileReader {
    result: string | null = null;
    onload: (() => void) | null = null;
    onerror: (() => void) | null = null;
    readAsDataURL(f: FakeFile): void {
      reads.push(f.name);
      queueMicrotask(() => {
        this.result = `data:${f.type};base64,${f.bytes.toString("base64")}`;
        this.onload?.();
      });
    }
  }
  const document = {
    getElementById: (id: string) => byId.get(id) ?? null,
    createElement: (tag: string) => new FakeElement(tag),
    createTextNode: (text: string) => Object.assign(new FakeElement("#text"), { textContent: text }),
    body: new FakeElement("body"),
  };
  const sandbox = {
    // Copied, as postMessage copies; also brings the object out of the page's realm.
    acquireVsCodeApi: () => ({ postMessage: (m: Record<string, unknown>) => sent.push(JSON.parse(JSON.stringify(m))) }),
    document,
    window: { addEventListener: (type: string, fn: (e: { data: unknown }) => void) => type === "message" && onMessage.push(fn) },
    self: { IncodeMarkdown: { render: (_d: unknown, body: FakeElement, text: string) => (body.textContent = text) } },
    requestAnimationFrame: (cb: () => void) => (cb(), 0),
    FileReader,
  };
  vm.runInNewContext(CHAT_JS, sandbox, { filename: "media/chat.js" });
  const log = byId.get("log")!;
  return {
    byId,
    sent,
    reads,
    receive: (m) => onMessage.forEach((fn) => fn({ data: m })),
    tick: () => new Promise((r) => setImmediate(r)),
    notices: () => log.all().filter((e) => e.className.startsWith("notice")).map((e) => `${e.className.replace("notice ", "")}: ${e.textContent}`),
    thumbs: () => byId.get("attachments")!.children,
  };
}

function paste(page: Page, files: FakeFile[], text = false): FakeEvent {
  const items = [
    ...(text ? [{ kind: "string", getAsFile: () => null }] : []),
    ...files.map((f) => ({ kind: "file", getAsFile: () => f })),
  ];
  return page.byId.get("input")!.dispatch("paste", new FakeEvent({ clipboardData: { items } }));
}

describe("the chat page's Content-Security-Policy (src/chatView.ts)", () => {
  it("adds data: to img-src only, for the thumbnails; everything else stays as strict", () => {
    assert.equal(
      contentSecurityPolicy("vscode-webview-src", "N"),
      "default-src 'none'; style-src vscode-webview-src; font-src vscode-webview-src; img-src vscode-webview-src data:; script-src 'nonce-N'",
    );
    const html = pageHtml("vscode-webview-src", "N", (f) => `media/${f}`);
    assert.ok(html.includes(`content="${contentSecurityPolicy("vscode-webview-src", "N")}"`));
    assert.equal((html.match(/data:/g) ?? []).length, 1, "data: appears in the CSP and nowhere else");
  });

  it("media/chat.css keeps the attachment strip hidden while it is empty", () => {
    // `display: flex` beats the `hidden` attribute unless a rule restores it.
    const css = readFileSync(path.resolve(__dirname, "..", "..", "media", "chat.css"), "utf8");
    assert.match(css, /#attachments\s*\{[^}]*display:\s*flex/);
    assert.match(css, /#attachments\[hidden\]\s*\{[^}]*display:\s*none/);
  });
});

describe("pictures in the chat input (media/chat.js)", () => {
  let page: Page;
  beforeEach(() => {
    page = openPage();
    assert.deepEqual(page.sent, [{ type: "ready" }]);
    page.sent.length = 0;
  });

  it("an agent without the capability: no Image button, and a pasted picture is refused", async () => {
    assert.equal(page.byId.get("attach")!.hidden, true);
    page.receive({ type: "capabilities", images: false, reopen: false });
    paste(page, [file("dot.png", "image/png", PNG)]);
    await page.tick();
    assert.deepEqual(page.notices(), ["error: Not attached: this agent does not take pictures."]);
    assert.equal(page.thumbs().length, 0);
    assert.deepEqual(page.reads, [], "the file was never read");
  });

  it("a pasted PNG shows a thumbnail and is sent with the text as base64", async () => {
    page.receive({ type: "capabilities", images: true, reopen: true });
    assert.equal(page.byId.get("attach")!.hidden, false);
    const e = paste(page, [file("dot.png", "image/png", PNG)]);
    assert.equal(e.prevented, true, "a paste carrying a file is taken over");
    await page.tick();
    assert.equal(page.byId.get("attachments")!.hidden, false);
    assert.equal(page.thumbs().length, 1);
    const img = page.thumbs()[0].all().find((x) => x.tagName === "img")!;
    assert.equal(img.src, `data:image/png;base64,${PNG.toString("base64")}`);
    const input = page.byId.get("input")!;
    input.value = "what is this?";
    page.byId.get("send")!.dispatch("click");
    assert.deepEqual(page.sent, [
      { type: "send", text: "what is this?", draft: 1, images: [{ name: "dot.png", mimeType: "image/png", data: PNG.toString("base64") }] },
    ]);
    assert.equal(page.thumbs().length, 0, "the input is cleared");
    assert.equal(page.byId.get("attachments")!.hidden, true);
  });

  it("a non-image file and an oversized picture are refused before they are read", async () => {
    page.receive({ type: "capabilities", images: true, reopen: true });
    paste(page, [file("notes.txt", "text/plain", Buffer.from("hello"))]);
    paste(page, [file("anim.gif", "image/gif", Buffer.from("GIF89a"))]);
    paste(page, [file("huge.png", "image/png", Buffer.alloc(4_000_000))]);
    paste(page, [file("edge.png", "image/png", Buffer.alloc(3_750_001))]);
    await page.tick();
    assert.deepEqual(page.notices(), [
      "error: Not attached: notes.txt is text/plain; only PNG and JPEG pictures are accepted.",
      "error: Not attached: anim.gif is image/gif; only PNG and JPEG pictures are accepted.",
      "error: Not attached: huge.png is 3.8 MiB; a picture may be at most 3.6 MiB.",
      "error: Not attached: edge.png is 3.6 MiB; a picture may be at most 3.6 MiB.",
    ]);
    assert.deepEqual(page.reads, []);
    assert.equal(page.thumbs().length, 0);
  });

  it("at most four pictures, and five MiB together", async () => {
    page.receive({ type: "capabilities", images: true, reopen: true });
    for (let i = 0; i < 5; i++) paste(page, [file(`p${i}.png`, "image/png", PNG)]);
    await page.tick();
    assert.equal(page.thumbs().length, 4);
    assert.deepEqual(page.notices(), ["error: Not attached: at most 4 pictures go with one prompt."]);

    page = openPage();
    page.receive({ type: "capabilities", images: true, reopen: true });
    paste(page, [file("a.png", "image/png", Buffer.alloc(3_000_000))]);
    await page.tick();
    paste(page, [file("b.png", "image/png", Buffer.alloc(3_000_000))]);
    await page.tick();
    assert.equal(page.thumbs().length, 1);
    assert.deepEqual(page.notices(), ["error: Not attached: the pictures would come to more than 5.0 MiB together."]);
  });

  it("the remove control takes a thumbnail away; a text paste stays a text paste", async () => {
    page.receive({ type: "capabilities", images: true, reopen: true });
    paste(page, [file("a.png", "image/png", PNG), file("b.png", "image/png", PNG)]);
    await page.tick();
    assert.equal(page.thumbs().length, 2);
    const remove = page.thumbs()[0].all().find((x) => x.className === "remove")!;
    assert.equal(remove.attrs["aria-label"], "Remove a.png");
    remove.dispatch("click");
    assert.deepEqual(page.thumbs().map((t) => t.title.split(" ")[0]), ["b.png"]);
    assert.equal(paste(page, [], true).prevented, false);
  });

  it("a dropped file, and one picked through the extension's dialog, are attached too", async () => {
    page.receive({ type: "capabilities", images: true, reopen: true });
    const composer = page.byId.get("composer")!;
    assert.equal(composer.dispatch("dragover", new FakeEvent({ dataTransfer: { types: ["Files"], files: [] } })).prevented, true);
    composer.dispatch("drop", new FakeEvent({ dataTransfer: { files: [file("drop.png", "image/png", PNG)] } }));
    await page.tick();
    page.byId.get("attach")!.dispatch("click");
    assert.deepEqual(page.sent, [{ type: "attachImage" }]);
    page.receive({ type: "attachment", image: { name: "picked.jpg", mimeType: "image/jpeg", data: "/9j/AA==" } });
    assert.deepEqual(page.thumbs().map((t) => t.title.split(" ")[0]), ["drop.png", "picked.jpg"]);
  });

  it("pictures without text are not sent; a sent prompt's pictures are named in the log", async () => {
    page.receive({ type: "capabilities", images: true, reopen: true });
    paste(page, [file("dot.png", "image/png", PNG)]);
    await page.tick();
    page.byId.get("send")!.dispatch("click");
    assert.deepEqual(page.sent, []);
    assert.deepEqual(page.notices(), ["info: Add a question to go with the picture."]);
    assert.equal(page.thumbs().length, 1, "the picture stays for the question");

    page.receive({ type: "user", text: "what is this?", images: ["dot.png"] });
    const user = page.byId.get("log")!.all().find((e) => e.className === "msg user")!;
    assert.equal(user.textContent, "what is this?");
    assert.equal(user.children[0].textContent, "Attached: dot.png");
  });

  it("a draft the extension refuses comes back (text and pictures); one it took does not; newer typing is kept", async () => {
    page.receive({ type: "capabilities", images: true, reopen: true });
    const input = page.byId.get("input")!;
    const send = (text: string): number => {
      input.value = text;
      page.byId.get("send")!.dispatch("click");
      return page.sent.at(-1)!.draft as number;
    };
    paste(page, [file("dot.png", "image/png", PNG)]);
    await page.tick();
    const first = send("what is this?");
    assert.equal(input.value, "");
    assert.equal(page.thumbs().length, 0);
    page.receive({ type: "notSent", draft: first, reason: "a turn is still running" });
    assert.equal(input.value, "what is this?", "the text is back");
    assert.deepEqual(page.thumbs().map((t) => t.title.split(" ")[0]), ["dot.png"], "the picture is back");

    const second = send("what is this?");
    page.receive({ type: "sent", draft: second });
    page.receive({ type: "notSent", draft: second, reason: "late" });
    assert.equal(input.value, "", "a draft that was sent is not restored");
    assert.equal(page.thumbs().length, 0);

    const third = send("first try");
    input.value = "something new";
    page.receive({ type: "notSent", draft: third, reason: "no session" });
    assert.equal(input.value, "something new", "what was typed since is not overwritten");
  });

  it("an agent that loses the capability (a restart) drops what was attached", async () => {
    page.receive({ type: "capabilities", images: true, reopen: true });
    paste(page, [file("dot.png", "image/png", PNG)]);
    await page.tick();
    page.receive({ type: "capabilities", images: false, reopen: false });
    assert.equal(page.thumbs().length, 0);
    assert.equal(page.byId.get("attach")!.hidden, true);
  });
});
