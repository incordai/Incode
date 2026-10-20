import { strict as assert } from "node:assert";
import { describe, it } from "node:test";

// media/markdown.js is a plain script shared with the webview.
// eslint-disable-next-line @typescript-eslint/no-require-imports
const md = require("../../media/markdown.js") as {
  parse(text: string): Block[];
  parseInline(text: string): Inline[];
  render(doc: FakeDocument, container: FakeElement, text: string, onLink?: (url: string) => void): void;
};

type Inline = { t: string; v?: string; href?: string; c?: Inline[] };
type Block = { t: string; inl?: Inline[]; text?: string; lang?: string; items?: Inline[][]; level?: number };

/** Just enough DOM to run render() and see what it builds. */
class FakeElement {
  children: FakeElement[] = [];
  attrs: Record<string, string> = {};
  textContent = "";
  constructor(readonly tag: string) {}
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
  addEventListener(): void {}
  set innerHTML(_v: string) {
    throw new Error("innerHTML must never be used");
  }
  /** Tags and text, flattened, for assertions. */
  dump(): string {
    if (this.tag === "#text") return this.textContent;
    const inner = this.children.length ? this.children.map((c) => c.dump()).join("") : this.textContent;
    return `<${this.tag}>${inner}</${this.tag}>`;
  }
}
class FakeDocument {
  createElement(tag: string): FakeElement {
    return new FakeElement(tag);
  }
  createTextNode(text: string): FakeElement {
    const e = new FakeElement("#text");
    e.textContent = text;
    return e;
  }
}

function rendered(text: string): string {
  const root = new FakeElement("div");
  md.render(new FakeDocument(), root, text);
  return root.children.map((c) => c.dump()).join("");
}

describe("markdown-lite", () => {
  it("HTML in model output stays text", () => {
    const blocks = md.parse('<script>alert(1)</script> <img src=x onerror="y">');
    assert.deepEqual(blocks, [
      { t: "p", inl: [{ t: "text", v: '<script>alert(1)</script> <img src=x onerror="y">' }] },
    ]);
    // And render() puts it in a text node, not markup.
    const root = new FakeElement("div");
    md.render(new FakeDocument(), root, "<b>hi</b>");
    assert.equal(root.children[0].children[0].tag, "#text");
    assert.equal(root.children[0].children[0].textContent, "<b>hi</b>");
  });

  it("only http(s) links become links", () => {
    assert.deepEqual(md.parseInline("[a](https://x.dev)"), [{ t: "a", href: "https://x.dev", c: [{ t: "text", v: "a" }] }]);
    assert.deepEqual(md.parseInline("[a](javascript:alert(1))"), [
      { t: "text", v: "a" },
      { t: "text", v: ")" },
    ]);
    assert.deepEqual(md.parseInline("[a](command:workbench.action.quit)"), [{ t: "text", v: "a" }]);
  });

  it("inline code, bold, italic", () => {
    assert.equal(rendered("use `x<y>` and **bold** and *it*"), "<p>use <code>x<y></code> and <strong>bold</strong> and <em>it</em></p>");
  });

  it("fenced code keeps its text verbatim, even unterminated while streaming", () => {
    assert.deepEqual(md.parse("```rust\nfn main() {}\n```\nafter"), [
      { t: "code", lang: "rust", text: "fn main() {}" },
      { t: "p", inl: [{ t: "text", v: "after" }] },
    ]);
    assert.deepEqual(md.parse("```\n<b>partial"), [{ t: "code", lang: "", text: "<b>partial" }]);
  });

  it("headings, lists, quotes, rules", () => {
    assert.equal(rendered("# Title\n- a\n- b\n\n1. one\n> q\n---"), "<h3>Title</h3><ul><li>a</li><li>b</li></ul><ol><li>one</li></ol><blockquote>q</blockquote><hr></hr>");
  });

  it("snake_case words are not italic", () => {
    assert.equal(rendered("call my_func_name now"), "<p>call my_func_name now</p>");
  });
});
