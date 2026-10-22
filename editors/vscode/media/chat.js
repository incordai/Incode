// The chat view. Talks to the extension with postMessage only; every piece of
// text from the agent goes into the DOM through textContent (directly or via
// IncodeMarkdown.render), never innerHTML.
(function () {
  "use strict";
  // eslint-disable-next-line no-undef
  var vscode = acquireVsCodeApi();
  var md = self.IncodeMarkdown;

  var $ = function (id) {
    return document.getElementById(id);
  };
  var log = $("log");
  var input = $("input");
  var sendBtn = $("send");
  var stopBtn = $("stop");
  var statusEl = $("status");
  var planEl = $("plan");
  var usageEl = $("usage");
  var modeWrap = $("mode-wrap");
  var modeSel = $("mode");
  var configEl = $("config");
  var completions = $("completions");
  var composer = $("composer");
  var attachBtn = $("attach");
  var attachmentsEl = $("attachments");

  // Pictures for the next prompt. The bounds are src/images.ts's, repeated
  // here so a file is refused before it is read; the extension checks again.
  var IMAGE_TYPES = ["image/png", "image/jpeg"];
  var MAX_IMAGE_BYTES = 3750000;
  var MAX_IMAGES = 4;
  var MAX_TOTAL_IMAGE_BYTES = 5 * 1024 * 1024;
  var imagesAllowed = false;
  var attachments = []; // { name, mimeType, data (base64), size }
  var draftSeq = 0;
  var pending = null; // { draft, text, attachments }: sent, not yet answered

  var commands = [];
  var running = false;
  var current = null; // { kind: "agentText"|"thought", el, text }
  var tools = {}; // toolCallId -> element
  var permissions = {}; // key -> element
  var renderPending = null;

  function el(tag, cls, text) {
    var e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined && text !== null) e.textContent = String(text);
    return e;
  }

  function atBottom() {
    return log.scrollHeight - log.scrollTop - log.clientHeight < 40;
  }
  function append(node) {
    var stick = atBottom();
    log.appendChild(node);
    if (stick) log.scrollTop = log.scrollHeight;
  }
  function openLink(url) {
    vscode.postMessage({ type: "openLink", url: url });
  }

  function endStream() {
    if (current && renderPending) flushRender();
    current = null;
  }

  function flushRender() {
    renderPending = null;
    if (!current) return;
    var stick = atBottom();
    if (current.kind === "thought") current.body.textContent = current.text;
    else md.render(document, current.body, current.text, openLink);
    if (stick) log.scrollTop = log.scrollHeight;
  }

  function streamText(kind, text) {
    if (!current || current.kind !== kind) {
      endStream();
      var box = el("div", kind === "thought" ? "msg thought" : "msg agent");
      if (kind === "thought") box.appendChild(el("div", "label", "Thinking"));
      var body = el("div", "body");
      box.appendChild(body);
      append(box);
      current = { kind: kind, el: box, body: body, text: "" };
    }
    current.text += text;
    if (!renderPending) renderPending = requestAnimationFrame(flushRender);
  }

  var STATUS_TEXT = { pending: "pending", in_progress: "running", completed: "done", failed: "failed" };

  function renderTool(call) {
    var box = tools[call.toolCallId];
    if (!box) {
      endStream();
      box = el("details", "tool");
      tools[call.toolCallId] = box;
      append(box);
    }
    while (box.firstChild) box.removeChild(box.firstChild);
    var status = call.status || "pending";
    box.setAttribute("data-status", status);
    var summary = el("summary");
    summary.appendChild(el("span", "tool-status " + status, STATUS_TEXT[status] || status));
    summary.appendChild(el("span", "tool-title", call.title || "tool call"));
    if (call.kind) summary.appendChild(el("span", "tool-kind", call.kind));
    box.appendChild(summary);
    (call.locations || []).forEach(function (loc) {
      box.appendChild(el("div", "tool-loc", loc.path + (loc.line ? ":" + loc.line : "")));
    });
    (call.content || []).forEach(function (c) {
      if (c.type === "diff") {
        var d = el("div", "diff");
        d.appendChild(el("div", "diff-path", c.path || ""));
        if (c.oldText) d.appendChild(el("pre", "diff-old", c.oldText));
        d.appendChild(el("pre", "diff-new", c.newText || ""));
        box.appendChild(d);
      } else if (c.type === "content" && c.content) {
        var text = c.content.type === "text" ? c.content.text : "[" + c.content.type + "]";
        box.appendChild(el("pre", "tool-output", text));
      } else if (c.type === "terminal") {
        box.appendChild(el("div", "tool-loc", "terminal " + (c.terminalId || "")));
      }
    });
  }

  function renderPlan(entries) {
    while (planEl.firstChild) planEl.removeChild(planEl.firstChild);
    if (!entries || entries.length === 0) {
      planEl.hidden = true;
      return;
    }
    planEl.hidden = false;
    planEl.appendChild(el("div", "label", "Plan"));
    var ol = el("ol");
    entries.forEach(function (e) {
      var li = el("li", "plan-" + (e.status || "pending"), e.content);
      li.title = (e.status || "pending") + (e.priority ? " · " + e.priority + " priority" : "");
      ol.appendChild(li);
    });
    planEl.appendChild(ol);
  }

  function renderPermission(m) {
    endStream();
    var box = el("div", "permission");
    box.appendChild(el("div", "permission-title", m.title));
    var row = el("div", "permission-actions");
    m.options.forEach(function (o) {
      var kind = String(o.kind || "");
      var b = el("button", /^reject/.test(kind) ? "secondary" : "", o.name);
      b.type = "button";
      b.title = kind.replace(/_/g, " ");
      b.addEventListener("click", function () {
        vscode.postMessage({ type: "permission", key: m.key, optionId: o.optionId });
      });
      row.appendChild(b);
    });
    box.appendChild(row);
    box._options = m.options;
    permissions[m.key] = box;
    append(box);
  }

  function permissionDone(m) {
    var box = permissions[m.key];
    if (!box) return;
    delete permissions[m.key];
    var row = box.querySelector(".permission-actions");
    if (row) box.removeChild(row);
    var name = null;
    (box._options || []).forEach(function (o) {
      if (o.optionId === m.chosen) name = o.name;
    });
    box.appendChild(el("div", "permission-result", m.chosen ? "Answered: " + (name || m.chosen) : "Cancelled"));
    box.classList.add("done");
  }

  function flatOptions(options) {
    var out = [];
    (options || []).forEach(function (o) {
      if (o && Array.isArray(o.options)) {
        o.options.forEach(function (x) {
          out.push(x);
        });
      } else if (o) out.push(o);
    });
    return out;
  }

  function renderModes(modes) {
    while (modeSel.firstChild) modeSel.removeChild(modeSel.firstChild);
    if (!modes || !modes.availableModes || modes.availableModes.length === 0) {
      modeWrap.hidden = true;
      return;
    }
    modes.availableModes.forEach(function (m) {
      var o = el("option", "", m.name);
      o.value = m.id;
      if (m.description) o.title = m.description;
      modeSel.appendChild(o);
    });
    modeSel.value = modes.currentModeId;
    modeWrap.hidden = false;
  }

  function renderConfig(options) {
    while (configEl.firstChild) configEl.removeChild(configEl.firstChild);
    (options || []).forEach(function (opt) {
      if (opt.type !== "select") return;
      var label = el("label", "picker");
      label.appendChild(document.createTextNode(opt.name + " "));
      if (opt.description) label.title = opt.description;
      var sel = el("select");
      flatOptions(opt.options).forEach(function (o) {
        var op = el("option", "", o.name);
        op.value = o.value;
        if (o.description) op.title = o.description;
        sel.appendChild(op);
      });
      sel.value = opt.currentValue;
      sel.addEventListener("change", function () {
        vscode.postMessage({ type: "setConfig", configId: opt.id, value: sel.value });
      });
      label.appendChild(sel);
      configEl.appendChild(label);
    });
  }

  function formatTokens(n) {
    if (n >= 1000000) return (n / 1000000).toFixed(1) + "M";
    if (n >= 1000) return Math.round(n / 1000) + "k";
    return String(n);
  }

  function renderUsage(m) {
    var pct = m.size > 0 ? Math.round((m.used / m.size) * 100) : 0;
    var text = formatTokens(m.used) + " / " + formatTokens(m.size) + " (" + pct + "%)";
    if (m.cost && typeof m.cost.amount === "number") text += " · " + m.cost.amount.toFixed(2) + " " + m.cost.currency;
    usageEl.textContent = text;
    usageEl.className = pct >= 90 ? "high" : pct >= 70 ? "mid" : "";
  }

  function setRunning(r) {
    running = r;
    stopBtn.hidden = !r;
    sendBtn.disabled = r;
    document.body.classList.toggle("running", r);
  }

  function reset() {
    while (log.firstChild) log.removeChild(log.firstChild);
    current = null;
    tools = {};
    permissions = {};
    renderPlan([]);
    usageEl.textContent = "";
  }

  // ----- slash-command completion -----------------------------------------

  var selected = 0;
  var shown = [];

  function matches() {
    var v = input.value;
    var m = /^\/(\S*)$/.exec(v);
    if (!m) return [];
    var q = m[1].toLowerCase();
    return commands.filter(function (c) {
      return c.name.toLowerCase().indexOf(q) === 0;
    });
  }

  function updateCompletions() {
    shown = matches();
    while (completions.firstChild) completions.removeChild(completions.firstChild);
    if (shown.length === 0) {
      completions.hidden = true;
      return;
    }
    if (selected >= shown.length) selected = 0;
    shown.forEach(function (c, i) {
      var li = el("li", i === selected ? "active" : "");
      li.setAttribute("role", "option");
      li.appendChild(el("span", "cmd", "/" + c.name));
      if (c.input && c.input.hint) li.appendChild(el("span", "hint", c.input.hint));
      li.appendChild(el("span", "desc", c.description));
      li.addEventListener("mousedown", function (e) {
        e.preventDefault();
        choose(i);
      });
      completions.appendChild(li);
    });
    completions.hidden = false;
  }

  function choose(i) {
    var c = shown[i];
    if (!c) return;
    input.value = "/" + c.name + " ";
    selected = 0;
    updateCompletions();
    input.focus();
  }

  input.addEventListener("input", function () {
    selected = 0;
    updateCompletions();
  });
  input.addEventListener("keydown", function (e) {
    if (!completions.hidden && shown.length > 0) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        selected = (selected + (e.key === "ArrowDown" ? 1 : shown.length - 1)) % shown.length;
        updateCompletions();
        return;
      }
      if (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey)) {
        e.preventDefault();
        choose(selected);
        return;
      }
      if (e.key === "Escape") {
        completions.hidden = true;
        return;
      }
    }
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      send();
    }
  });

  function send() {
    var text = input.value;
    if (running) return;
    if (!text.trim()) {
      if (attachments.length > 0) localNotice("info", "Add a question to go with the picture.");
      return;
    }
    // The draft is kept until the extension says it went: a refusal (a turn
    // running, no session, a picture the agent cannot take) puts it back.
    pending = { draft: ++draftSeq, text: text, attachments: attachments };
    var message = { type: "send", text: text, draft: pending.draft };
    if (attachments.length > 0) {
      message.images = attachments.map(function (a) {
        return { name: a.name, mimeType: a.mimeType, data: a.data };
      });
    }
    vscode.postMessage(message);
    input.value = "";
    attachments = [];
    renderAttachments();
    updateCompletions();
  }

  /** The extension refused draft `m.draft`: restore what has not been replaced since. */
  function restoreDraft(m) {
    if (!pending || pending.draft !== m.draft) return;
    var draft = pending;
    pending = null;
    if (!input.value.trim()) input.value = draft.text;
    if (attachments.length === 0 && draft.attachments.length > 0) {
      attachments = draft.attachments;
      renderAttachments();
    }
  }

  // ----- pictures -----------------------------------------------------------

  function localNotice(level, text) {
    endStream();
    append(el("div", "notice " + level, text));
  }

  function formatBytes(n) {
    if (n >= 1024 * 1024) return (n / (1024 * 1024)).toFixed(1) + " MiB";
    if (n >= 1024) return Math.round(n / 1024) + " KiB";
    return n + " bytes";
  }

  /** Why a picture of this type and size cannot be added now, or null. */
  function refusal(name, type, size) {
    if (!imagesAllowed) return "this agent does not take pictures";
    if (IMAGE_TYPES.indexOf(type) < 0) return name + " is " + (type || "not a picture") + "; only PNG and JPEG pictures are accepted";
    if (size > MAX_IMAGE_BYTES) return name + " is " + formatBytes(size) + "; a picture may be at most " + formatBytes(MAX_IMAGE_BYTES);
    if (attachments.length >= MAX_IMAGES) return "at most " + MAX_IMAGES + " pictures go with one prompt";
    var total = size;
    attachments.forEach(function (a) {
      total += a.size;
    });
    if (total > MAX_TOTAL_IMAGE_BYTES) return "the pictures would come to more than " + formatBytes(MAX_TOTAL_IMAGE_BYTES) + " together";
    return null;
  }

  function addAttachment(a) {
    var why = refusal(a.name, a.mimeType, a.size);
    if (why) {
      localNotice("error", "Not attached: " + why + ".");
      return;
    }
    attachments.push(a);
    renderAttachments();
  }

  /** A File from paste, drop or the clipboard: checked, then read as base64. */
  function addFile(file) {
    var name = file.name || "pasted picture";
    var why = refusal(name, file.type, file.size);
    if (why) {
      localNotice("error", "Not attached: " + why + ".");
      return;
    }
    var reader = new FileReader();
    reader.onload = function () {
      var url = String(reader.result || "");
      var comma = url.indexOf(",");
      if (comma < 0) return;
      addAttachment({ name: name, mimeType: file.type, data: url.slice(comma + 1), size: file.size });
    };
    reader.onerror = function () {
      localNotice("error", "Not attached: " + name + " could not be read.");
    };
    reader.readAsDataURL(file);
  }

  function renderAttachments() {
    while (attachmentsEl.firstChild) attachmentsEl.removeChild(attachmentsEl.firstChild);
    attachmentsEl.hidden = attachments.length === 0;
    attachments.forEach(function (a, i) {
      var box = el("div", "thumb");
      box.title = a.name + " (" + formatBytes(a.size) + ")";
      var img = el("img");
      img.alt = a.name;
      // A data: URL of the picture's own bytes; the CSP allows data: for img-src only.
      img.src = "data:" + a.mimeType + ";base64," + a.data;
      box.appendChild(img);
      var remove = el("button", "remove", "×");
      remove.type = "button";
      remove.title = "Remove " + a.name;
      remove.setAttribute("aria-label", "Remove " + a.name);
      remove.addEventListener("click", function () {
        attachments.splice(i, 1);
        renderAttachments();
      });
      box.appendChild(remove);
      attachmentsEl.appendChild(box);
    });
  }

  function listOf(files) {
    var out = [];
    for (var i = 0; files && i < files.length; i++) out.push(files[i]);
    return out;
  }

  input.addEventListener("paste", function (e) {
    var items = (e.clipboardData && e.clipboardData.items) || [];
    var files = [];
    for (var i = 0; i < items.length; i++) {
      if (items[i].kind === "file") {
        var f = items[i].getAsFile();
        if (f) files.push(f);
      }
    }
    // Text pastes as text; only a paste that carries files is taken over.
    if (files.length === 0) return;
    e.preventDefault();
    files.forEach(addFile);
  });
  // Files dropped from the system's file manager. (VS Code's own explorer
  // drags URIs, not files; those are left to the editor.)
  composer.addEventListener("dragover", function (e) {
    var types = (e.dataTransfer && e.dataTransfer.types) || [];
    if (Array.prototype.indexOf.call(types, "Files") >= 0) e.preventDefault();
  });
  composer.addEventListener("drop", function (e) {
    var files = listOf(e.dataTransfer && e.dataTransfer.files);
    if (files.length === 0) return;
    e.preventDefault();
    files.forEach(addFile);
  });
  attachBtn.addEventListener("click", function () {
    vscode.postMessage({ type: "attachImage" });
  });

  sendBtn.addEventListener("click", send);
  stopBtn.addEventListener("click", function () {
    vscode.postMessage({ type: "stop" });
  });
  modeSel.addEventListener("change", function () {
    vscode.postMessage({ type: "setMode", modeId: modeSel.value });
  });

  // ----- from the extension -----------------------------------------------

  window.addEventListener("message", function (event) {
    var m = event.data;
    if (!m || typeof m.type !== "string") return;
    switch (m.type) {
      case "reset":
        reset();
        break;
      case "status":
        statusEl.textContent = m.text || "";
        statusEl.className = m.state;
        break;
      case "user": {
        endStream();
        var box = el("div", "msg user", m.text);
        if (Array.isArray(m.images) && m.images.length > 0) {
          box.appendChild(el("div", "attached", "Attached: " + m.images.join(", ")));
        }
        append(box);
        break;
      }
      case "capabilities":
        imagesAllowed = !!m.images;
        attachBtn.hidden = !imagesAllowed;
        if (!imagesAllowed && attachments.length > 0) {
          attachments = [];
          renderAttachments();
        }
        break;
      case "sent":
        if (pending && pending.draft === m.draft) pending = null;
        break;
      case "notSent":
        restoreDraft(m);
        break;
      case "attachment":
        if (m.image) {
          var bytes = Math.floor((String(m.image.data || "").length * 3) / 4);
          addAttachment({ name: m.image.name, mimeType: m.image.mimeType, data: m.image.data, size: bytes });
        }
        break;
      case "agentText":
        streamText("agentText", m.text);
        break;
      case "thought":
        streamText("thought", m.text);
        break;
      case "tool":
        renderTool(m.call);
        break;
      case "plan":
        renderPlan(m.entries);
        break;
      case "commands":
        commands = Array.isArray(m.commands) ? m.commands : [];
        updateCompletions();
        break;
      case "modes":
        renderModes(m.modes);
        break;
      case "mode":
        modeSel.value = m.currentModeId;
        break;
      case "config":
        renderConfig(m.configOptions);
        break;
      case "usage":
        renderUsage(m);
        break;
      case "permission":
        renderPermission(m);
        break;
      case "permissionDone":
        permissionDone(m);
        break;
      case "turn":
        if (!m.running) endStream();
        setRunning(!!m.running);
        if (!m.running && m.stopReason && m.stopReason !== "end_turn") {
          append(el("div", "notice", "Turn ended: " + m.stopReason.replace(/_/g, " ")));
        }
        break;
      case "notice":
        endStream();
        append(el("div", "notice " + (m.level || "info"), m.text));
        break;
      case "prefill":
        input.value = (input.value ? input.value.replace(/\s*$/, "\n") : "") + m.text;
        input.focus();
        input.setSelectionRange(input.value.length, input.value.length);
        break;
    }
  });

  setRunning(false);
  vscode.postMessage({ type: "ready" });
})();
