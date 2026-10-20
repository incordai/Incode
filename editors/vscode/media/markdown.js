// Markdown-lite for the chat view.
//
// The model's text is NEVER turned into HTML. `parse` produces a small tree of
// plain objects; `render` builds DOM nodes from it with createElement and
// textContent only. A `<script>` in the model's output is shown as the text
// `<script>`. Links are kept only for http(s) URLs, and are opened by the
// extension (which checks the scheme again), not by the webview.
//
// Loaded as a plain script in the webview (sets `IncodeMarkdown`) and with
// require() in the unit tests.
(function (root) {
  "use strict";

  /** Inline tokens: text, code, b, i, a (http/https only). */
  function parseInline(src) {
    var out = [];
    var buf = "";
    function flush() {
      if (buf) {
        out.push({ t: "text", v: buf });
        buf = "";
      }
    }
    var i = 0;
    while (i < src.length) {
      var ch = src[i];
      if (ch === "\\" && i + 1 < src.length && /[\\`*_\[\]()#>-]/.test(src[i + 1])) {
        buf += src[i + 1];
        i += 2;
        continue;
      }
      if (ch === "`") {
        var end = src.indexOf("`", i + 1);
        if (end > i) {
          flush();
          out.push({ t: "code", v: src.slice(i + 1, end) });
          i = end + 1;
          continue;
        }
      }
      if ((ch === "*" || ch === "_") && src[i + 1] === ch) {
        var close = src.indexOf(ch + ch, i + 2);
        if (close > i + 2) {
          flush();
          out.push({ t: "b", c: parseInline(src.slice(i + 2, close)) });
          i = close + 2;
          continue;
        }
      }
      if (ch === "*" || (ch === "_" && (i === 0 || /\W/.test(src[i - 1])))) {
        var closeI = src.indexOf(ch, i + 1);
        if (closeI > i + 1 && src[i + 1] !== " " && src[closeI - 1] !== " ") {
          flush();
          out.push({ t: "i", c: parseInline(src.slice(i + 1, closeI)) });
          i = closeI + 1;
          continue;
        }
      }
      if (ch === "[") {
        var m = /^\[([^\]]*)\]\(([^)\s]*)\)/.exec(src.slice(i));
        if (m) {
          flush();
          if (/^https?:\/\//i.test(m[2])) {
            out.push({ t: "a", href: m[2], c: parseInline(m[1]) });
          } else {
            // Not a web link: shown as its text, the target dropped.
            out.push.apply(out, parseInline(m[1]));
          }
          i += m[0].length;
          continue;
        }
      }
      buf += ch;
      i++;
    }
    flush();
    return out;
  }

  /** Block tokens: p, h, code, ul, ol, quote, hr. */
  function parse(text) {
    var lines = String(text).replace(/\r\n?/g, "\n").split("\n");
    var blocks = [];
    var para = [];
    function endPara() {
      if (para.length) {
        var inl = [];
        para.forEach(function (l, n) {
          if (n > 0) inl.push({ t: "br" });
          inl.push.apply(inl, parseInline(l));
        });
        blocks.push({ t: "p", inl: inl });
        para = [];
      }
    }
    var i = 0;
    while (i < lines.length) {
      var line = lines[i];
      var fence = /^\s*(```+|~~~+)\s*([\w+#.-]*)\s*$/.exec(line);
      if (fence) {
        endPara();
        var marker = fence[1];
        var body = [];
        i++;
        while (i < lines.length && lines[i].trim().indexOf(marker) !== 0) {
          body.push(lines[i]);
          i++;
        }
        i++; // the closing fence (or past the end while still streaming)
        blocks.push({ t: "code", lang: fence[2] || "", text: body.join("\n") });
        continue;
      }
      if (/^\s*$/.test(line)) {
        endPara();
        i++;
        continue;
      }
      var h = /^(#{1,6})\s+(.*)$/.exec(line);
      if (h) {
        endPara();
        blocks.push({ t: "h", level: h[1].length, inl: parseInline(h[2]) });
        i++;
        continue;
      }
      if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) {
        endPara();
        blocks.push({ t: "hr" });
        i++;
        continue;
      }
      var li = /^\s*([-*+]|\d+[.)])\s+(.*)$/.exec(line);
      if (li) {
        endPara();
        var ordered = /\d/.test(li[1]);
        var items = [];
        while (i < lines.length) {
          var m = /^\s*([-*+]|\d+[.)])\s+(.*)$/.exec(lines[i]);
          if (!m || /\d/.test(m[1]) !== ordered) break;
          items.push(parseInline(m[2]));
          i++;
        }
        blocks.push({ t: ordered ? "ol" : "ul", items: items });
        continue;
      }
      var q = /^\s*>\s?(.*)$/.exec(line);
      if (q) {
        endPara();
        var quoted = [];
        while (i < lines.length) {
          var mq = /^\s*>\s?(.*)$/.exec(lines[i]);
          if (!mq) break;
          quoted.push(mq[1]);
          i++;
        }
        blocks.push({ t: "quote", inl: parseInline(quoted.join(" ")) });
        continue;
      }
      para.push(line);
      i++;
    }
    endPara();
    return blocks;
  }

  /** DOM for inline tokens. textContent only. */
  function renderInline(doc, parent, tokens, onLink) {
    tokens.forEach(function (tok) {
      var el;
      switch (tok.t) {
        case "text":
          parent.appendChild(doc.createTextNode(tok.v));
          return;
        case "br":
          parent.appendChild(doc.createElement("br"));
          return;
        case "code":
          el = doc.createElement("code");
          el.textContent = tok.v;
          break;
        case "b":
          el = doc.createElement("strong");
          renderInline(doc, el, tok.c, onLink);
          break;
        case "i":
          el = doc.createElement("em");
          renderInline(doc, el, tok.c, onLink);
          break;
        case "a":
          el = doc.createElement("a");
          el.setAttribute("href", "#");
          el.setAttribute("title", tok.href);
          el.addEventListener("click", function (e) {
            e.preventDefault();
            if (onLink) onLink(tok.href);
          });
          renderInline(doc, el, tok.c, onLink);
          break;
        default:
          return;
      }
      parent.appendChild(el);
    });
  }

  /** Replace `container`'s children with the rendering of `text`. */
  function render(doc, container, text, onLink) {
    while (container.firstChild) container.removeChild(container.firstChild);
    parse(text).forEach(function (b) {
      var el;
      switch (b.t) {
        case "p":
          el = doc.createElement("p");
          renderInline(doc, el, b.inl, onLink);
          break;
        case "h":
          el = doc.createElement("h" + Math.min(6, b.level + 2));
          renderInline(doc, el, b.inl, onLink);
          break;
        case "code":
          el = doc.createElement("pre");
          var code = doc.createElement("code");
          code.textContent = b.text;
          if (b.lang) el.setAttribute("data-lang", b.lang);
          el.appendChild(code);
          break;
        case "ul":
        case "ol":
          el = doc.createElement(b.t);
          b.items.forEach(function (item) {
            var liEl = doc.createElement("li");
            renderInline(doc, liEl, item, onLink);
            el.appendChild(liEl);
          });
          break;
        case "quote":
          el = doc.createElement("blockquote");
          renderInline(doc, el, b.inl, onLink);
          break;
        case "hr":
          el = doc.createElement("hr");
          break;
        default:
          return;
      }
      container.appendChild(el);
    });
  }

  var api = { parse: parse, parseInline: parseInline, render: render };
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.IncodeMarkdown = api;
})(typeof self !== "undefined" ? self : this);
