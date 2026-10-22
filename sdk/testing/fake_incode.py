#!/usr/bin/env python3
"""A fake `incode` for the SDK test suites.

It speaks just enough of the two interfaces the SDKs drive:

* ``fake_incode.py -t <task> --output-format json [-w DIR] [-m MODEL] [-p MODE]``
  prints one result object in the shape of ``src/task_run.rs`` (``Report::finish``).
  The task text picks the scenario:

  ``fail``     -> ``is_error: true`` result, exit 1
  ``crash``    -> nothing on stdout, a message on stderr, exit 2
  ``garbage``  -> a line that is not JSON on stdout, exit 0
  ``sleep``    -> sleeps 30 s (for timeouts)
  otherwise    -> success, ``result`` is ``"done: <task>"``

  With ``--output-format stream-json``, the events ``init``, ``text``,
  ``tool_use``, ``tool_result`` and ``usage`` come first, one per line
  (``events-garbage`` adds a line that is not JSON after them).

* ``fake_incode.py --acp`` speaks ACP (newline-delimited JSON-RPC 2.0) on
  stdin/stdout, mirroring ``src/acp_agent``: ``initialize``, ``session/new``,
  ``session/load``, ``session/prompt``, ``session/cancel``, ``session/set_mode``,
  ``session/set_config_option``, ``session/close``. The prompt text picks the
  scenario (the first word):

  ``hello``          thought, two message chunks, a tool call and its update,
                     a plan, a usage_update; ``end_turn`` (also the default)
  ``permission``     sends ``session/request_permission`` and reports the answer
  ``permission-wait`` sends ``session/request_permission`` and waits for a cancel
  ``fs R W``         ``fs/read_text_file`` R, then ``fs/write_text_file`` W
  ``wait``           says ``working`` and waits for ``session/cancel``
  ``crash``          one chunk, then the process exits 3 mid-prompt
  ``malformed``      writes lines that are not JSON-RPC, then carries on
  ``unknown-request`` sends ``terminal/create`` and reports the error it gets

If ``FAKE_INCODE_ARGV`` is set, the argv is written there as JSON (both modes).
Python 3.9+, stdlib only.
"""

import json
import os
import queue
import sys
import threading
import time

PROTOCOL_VERSION = 1
MODES = [
    {"id": "read-only", "name": "Read only", "description": "Proposes changes without making them."},
    {"id": "workspace-write", "name": "Workspace write", "description": "Edits files in the workspace."},
    {"id": "danger-full-access", "name": "Full access", "description": "Full access."},
]


def record_argv():
    path = os.environ.get("FAKE_INCODE_ARGV")
    if path:
        with open(path, "w", encoding="utf-8") as f:
            json.dump(sys.argv[1:], f)


def flag(args, *names):
    for i, a in enumerate(args):
        if a in names and i + 1 < len(args):
            return args[i + 1]
    return None


# ---------------------------------------------------------------- -t mode


def run_task(args):
    task = flag(args, "-t", "--task")
    fmt = flag(args, "--output-format") or "text"
    model = flag(args, "-m", "--model")
    workspace = flag(args, "-w", "--workspace") or os.getcwd()
    if not os.path.isdir(workspace):
        sys.stderr.write("Error: workspace `%s` is not a directory\n" % workspace)
        return 1
    word = task.strip().split()[0] if task.strip() else ""
    if word == "crash":
        sys.stderr.write("fake: the agent could not start\n")
        return 2
    if word == "sleep":
        time.sleep(30)
        return 0
    if word == "garbage":
        sys.stdout.write("this is not json\n")
        return 0
    sys.stderr.write("fake: running %r\n" % task)
    failed = word == "fail" or not task.strip()
    result = {
        "type": "result",
        "is_error": failed,
        "session_id": "11111111-2222-3333-4444-555555555555",
        "agent": "claude-code",
        "model": model or "fake-model",
        "agent_session_id": "agent-sess-1",
        "stop_reason": None if failed else "end_turn",
        "served_by": ["claude-code"],
        "usage": {
            "input_tokens": 120,
            "output_tokens": 45,
            "cache_creation_tokens": 7,
            "cache_read_tokens": 300,
        },
        "cost_usd": None if failed else 0.0123,
        "duration_ms": 42,
        "spec_gate": "on",
    }
    if failed:
        result["error"] = "nothing to do: the instruction is empty" if not task.strip() else "the agent failed: fake failure"
    else:
        result["result"] = "done: " + task
    if fmt == "stream-json":
        # The events src/task_run.rs `Report::init` / `Report::event` write
        # before the result, one JSON object per line.
        for event in (
            {"type": "init", "session_id": result["session_id"], "agent": "claude-code", "model": result["model"]},
            {"type": "text", "text": "working on: " + task},
            {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "README.md"}},
            {"type": "tool_result", "id": "t1", "output": "read it", "is_error": False},
            {"type": "usage", "input_tokens": 120, "output_tokens": 45, "cache_creation_tokens": 7, "cache_read_tokens": 300},
        ):
            sys.stdout.write(json.dumps(event) + "\n")
            sys.stdout.flush()
        if word == "events-garbage":
            sys.stdout.write("this is not json\n")
            sys.stdout.flush()
    if fmt in ("json", "stream-json"):
        sys.stdout.write(json.dumps(result) + "\n")
    else:
        sys.stdout.write(result.get("result", "") + "\n")
    sys.stdout.flush()
    return 1 if failed else 0


# ---------------------------------------------------------------- --acp mode


class Agent:
    def __init__(self):
        self.out_lock = threading.Lock()
        self.next_id = 1000
        self.waiting = {}  # request id -> queue for the client's answer
        self.waiting_lock = threading.Lock()
        self.sessions = {}  # id -> dict(mode, agent, role, cancel Event, busy)

    # -- output
    def write_raw(self, text):
        with self.out_lock:
            sys.stdout.write(text + "\n")
            sys.stdout.flush()

    def send(self, message):
        message = dict(message)
        message["jsonrpc"] = "2.0"
        self.write_raw(json.dumps(message))

    def respond(self, rid, result=None, error=None):
        if error is not None:
            self.send({"id": rid, "error": error})
        else:
            self.send({"id": rid, "result": result if result is not None else {}})

    def update(self, session_id, update):
        self.send({"method": "session/update", "params": {"sessionId": session_id, "update": update}})

    def chunk(self, session_id, text, kind="agent_message_chunk"):
        self.update(session_id, {"sessionUpdate": kind, "content": {"type": "text", "text": text}})

    def ask(self, method, params, timeout=20.0):
        """Send a request to the client; return its whole response message."""
        with self.waiting_lock:
            self.next_id += 1
            rid = self.next_id
            box = queue.Queue()
            self.waiting[rid] = box
        self.send({"id": rid, "method": method, "params": params})
        try:
            return box.get(timeout=timeout)
        except queue.Empty:
            return {"error": {"code": -1, "message": "fake: no answer"}}
        finally:
            with self.waiting_lock:
                self.waiting.pop(rid, None)

    # -- session state
    def config_options(self, s):
        return [
            {
                "id": "agent",
                "name": "Agent",
                "type": "select",
                "currentValue": s["agent"],
                "options": [
                    {"value": "claude-code", "name": "Claude Code"},
                    {"value": "codex", "name": "Codex"},
                ],
            },
            {
                "id": "role",
                "name": "Role",
                "category": "model",
                "type": "select",
                "currentValue": s["role"],
                "options": [{"value": v, "name": v.title()} for v in ("explore", "diagnose", "plan")],
            },
        ]

    def open_session(self, sid, mode):
        s = {"mode": mode, "agent": "claude-code", "role": "plan", "cancel": threading.Event(), "busy": False}
        self.sessions[sid] = s
        return s

    # -- dispatch
    def handle_line(self, line):
        if not line.strip():
            return
        try:
            msg = json.loads(line)
        except ValueError as e:
            self.respond(None, error={"code": -32700, "message": "Parse error", "data": str(e)})
            return
        if not isinstance(msg, dict):
            self.respond(None, error={"code": -32600, "message": "Invalid Request"})
            return
        if "method" not in msg:
            with self.waiting_lock:
                box = self.waiting.get(msg.get("id"))
            if box is not None:
                box.put(msg)
            return
        method, params, rid = msg["method"], msg.get("params") or {}, msg.get("id")
        if rid is None:
            if method == "session/cancel":
                s = self.sessions.get(params.get("sessionId"))
                if s and s["busy"]:
                    s["cancel"].set()
            return
        handler = getattr(self, "m_" + method.replace("/", "_"), None)
        if handler is None:
            self.respond(rid, error={"code": -32601, "message": "Method not found"})
            return
        handler(rid, params)

    def m_initialize(self, rid, params):
        self.client_caps = params.get("clientCapabilities") or {}
        self.respond(
            rid,
            {
                "protocolVersion": PROTOCOL_VERSION,
                "agentCapabilities": {
                    "loadSession": True,
                    "promptCapabilities": {"image": True, "audio": False, "embeddedContext": True},
                    "mcpCapabilities": {"http": True, "sse": False},
                    "sessionCapabilities": {"list": {}, "close": {}},
                },
                "authMethods": [],
                "agentInfo": {"name": "incode", "title": "incode (fake)", "version": "0.0.0-fake"},
            },
        )

    def check_cwd(self, rid, params):
        cwd = params.get("cwd")
        if not isinstance(cwd, str) or not os.path.isabs(cwd) or not os.path.isdir(cwd):
            self.respond(rid, error={"code": -32602, "message": "Invalid params", "data": "cwd must be an absolute directory"})
            return False
        return True

    def session_answer(self, sid, s):
        return {
            "sessionId": sid,
            "modes": {"currentModeId": s["mode"], "availableModes": MODES},
            "configOptions": self.config_options(s),
        }

    def m_session_new(self, rid, params):
        if not self.check_cwd(rid, params):
            return
        sid = "fake-session-%d" % (len(self.sessions) + 1)
        s = self.open_session(sid, START_MODE)
        self.respond(rid, self.session_answer(sid, s))
        self.update(
            sid,
            {
                "sessionUpdate": "available_commands_update",
                "availableCommands": [{"name": "run", "description": "Work the ledger"}],
            },
        )

    def m_session_load(self, rid, params):
        if not self.check_cwd(rid, params):
            return
        sid = params.get("sessionId")
        if sid != "kept-session":
            self.respond(rid, error={"code": -32602, "message": "Invalid params", "data": "no session `%s`" % sid})
            return
        s = self.open_session(sid, START_MODE)
        self.chunk(sid, "remember the parser", "user_message_chunk")
        self.chunk(sid, "I will.")
        self.respond(rid, self.session_answer(sid, s))

    def m_session_close(self, rid, params):
        s = self.sessions.pop(params.get("sessionId"), None)
        if s:
            s["cancel"].set()
        self.respond(rid, {})

    def m_session_set_mode(self, rid, params):
        s = self.sessions.get(params.get("sessionId"))
        mode = params.get("modeId")
        if s is None or mode not in [m["id"] for m in MODES]:
            self.respond(rid, error={"code": -32602, "message": "Invalid params", "data": "unknown mode or session"})
            return
        s["mode"] = mode
        self.respond(rid, {})
        self.update(params["sessionId"], {"sessionUpdate": "current_mode_update", "currentModeId": mode})

    def m_session_set_config_option(self, rid, params):
        s = self.sessions.get(params.get("sessionId"))
        cid, value = params.get("configId"), params.get("value")
        allowed = {"agent": ["claude-code", "codex"], "role": ["explore", "diagnose", "plan"]}
        if s is None or cid not in allowed or value not in allowed[cid]:
            self.respond(rid, error={"code": -32602, "message": "Invalid params", "data": "unknown option or value"})
            return
        s[cid] = value
        self.respond(rid, {"configOptions": self.config_options(s)})

    def m_session_prompt(self, rid, params):
        sid = params.get("sessionId")
        s = self.sessions.get(sid)
        if s is None:
            self.respond(rid, error={"code": -32602, "message": "Invalid params", "data": "no session"})
            return
        if s["busy"]:
            self.respond(rid, error={"code": -32600, "message": "Invalid Request", "data": "a prompt is already running in this session"})
            return
        s["busy"] = True
        s["cancel"].clear()
        text = "".join(b.get("text", "") for b in params.get("prompt") or [] if isinstance(b, dict))
        threading.Thread(target=self.turn, args=(rid, sid, s, text), daemon=True).start()

    # -- the scripted turns
    def turn(self, rid, sid, s, text):
        words = text.split()
        word = words[0] if words else "hello"
        try:
            stop = getattr(self, "turn_" + word.replace("-", "_"), self.turn_hello)(sid, s, words[1:])
        finally:
            s["busy"] = False
        if stop is not None:
            self.respond(rid, {"stopReason": stop})

    def turn_hello(self, sid, s, args):
        self.chunk(sid, "Thinking it over", "agent_thought_chunk")
        self.chunk(sid, "Hel")
        self.update(
            sid,
            {
                "sessionUpdate": "tool_call",
                "toolCallId": "t1",
                "title": "Read a.txt",
                "kind": "read",
                "status": "in_progress",
                "rawInput": {"path": "a.txt"},
                "locations": [{"path": "/ws/a.txt"}],
            },
        )
        self.update(
            sid,
            {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "t1",
                "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "contents of a"}}],
                "rawOutput": "contents of a",
            },
        )
        self.update(
            sid,
            {
                "sessionUpdate": "plan",
                "entries": [
                    {"content": "Read a.txt", "priority": "high", "status": "completed"},
                    {"content": "Report", "priority": "medium", "status": "in_progress"},
                ],
            },
        )
        self.update(sid, {"sessionUpdate": "usage_update", "used": 38833, "size": 1000000})
        self.update(sid, {"sessionUpdate": "some_future_update", "whatever": 1})
        self.chunk(sid, "lo (mode %s, agent %s)\n" % (s["mode"], s["agent"]))
        return "end_turn"

    def permission_params(self, sid):
        return {
            "sessionId": sid,
            "toolCall": {
                "toolCallId": "g1",
                "title": "Write guarded.txt — writing here needs the owner",
                "kind": "edit",
                "status": "pending",
                "rawInput": {"path": "guarded.txt"},
            },
            "options": [
                {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
            ],
        }

    def turn_permission(self, sid, s, args):
        self.update(sid, {"sessionUpdate": "tool_call", "toolCallId": "g1", "title": "Write guarded.txt", "kind": "edit", "status": "pending"})
        answer = self.ask("session/request_permission", self.permission_params(sid))
        outcome = (answer.get("result") or {}).get("outcome") or {}
        if outcome.get("outcome") == "selected":
            said = "permission: " + str(outcome.get("optionId"))
        elif outcome.get("outcome") == "cancelled":
            said = "permission: cancelled"
        else:
            said = "permission: error %s" % json.dumps(answer.get("error"))
        allowed = outcome.get("optionId") == "allow"
        self.update(sid, {"sessionUpdate": "tool_call_update", "toolCallId": "g1", "status": "completed" if allowed else "failed"})
        self.chunk(sid, said)
        return "end_turn"

    def turn_permission_wait(self, sid, s, args):
        box = {}

        def ask():
            box["answer"] = self.ask("session/request_permission", self.permission_params(sid))

        asker = threading.Thread(target=ask, daemon=True)
        asker.start()
        s["cancel"].wait(10)
        asker.join(5)
        outcome = ((box.get("answer") or {}).get("result") or {}).get("outcome") or {}
        self.chunk(sid, "permission answer after cancel: %s" % outcome.get("outcome"))
        return "cancelled" if s["cancel"].is_set() else "end_turn"

    def turn_fs(self, sid, s, args):
        read_path = args[0] if args else "/nonexistent"
        write_path = args[1] if len(args) > 1 else "/nonexistent-out"
        got = self.ask("fs/read_text_file", {"sessionId": sid, "path": read_path})
        if "result" in got:
            self.chunk(sid, "read: %s\n" % got["result"].get("content"))
        else:
            self.chunk(sid, "read error: %s %s\n" % (got["error"].get("code"), got["error"].get("message")))
        got = self.ask("fs/write_text_file", {"sessionId": sid, "path": write_path, "content": "written by fake\n"})
        if "result" in got:
            self.chunk(sid, "write: ok\n")
        else:
            self.chunk(sid, "write error: %s %s\n" % (got["error"].get("code"), got["error"].get("message")))
        return "end_turn"

    def turn_wait(self, sid, s, args):
        self.chunk(sid, "working")
        if s["cancel"].wait(10):
            return "cancelled"
        self.chunk(sid, "nobody cancelled")
        return "end_turn"

    def turn_crash(self, sid, s, args):
        self.chunk(sid, "about to crash")
        sys.stderr.write("fake: crashed mid-prompt\n")
        sys.stderr.flush()
        os._exit(3)

    def turn_malformed(self, sid, s, args):
        self.write_raw("this is not json")
        self.write_raw('{"jsonrpc": "2.0", "method": "session/update"')
        self.write_raw("[1, 2, 3]")
        self.write_raw('{"jsonrpc": "2.0", "id": 987654, "result": {}}')
        self.chunk(sid, "still alive")
        return "end_turn"

    def turn_unknown_request(self, sid, s, args):
        got = self.ask("terminal/create", {"sessionId": sid, "command": "ls"})
        self.chunk(sid, "terminal/create -> %s" % json.dumps(got.get("error", {}).get("code")))
        return "end_turn"


START_MODE = "workspace-write"


def run_acp(args):
    global START_MODE
    mode = flag(args, "-p", "--permission-mode")
    if mode:
        if mode not in [m["id"] for m in MODES]:
            sys.stderr.write("`--acp` runs as read-only, workspace-write or danger-full-access, not `%s`\n" % mode)
            return 1
        START_MODE = mode
    sys.stderr.write("incode ACP agent ready on stdio (fake)\n")
    sys.stderr.flush()
    agent = Agent()
    for line in sys.stdin:
        agent.handle_line(line.rstrip("\r\n"))
    return 0


def main():
    args = sys.argv[1:]
    record_argv()
    if "--acp" in args:
        return run_acp(args)
    if "-t" in args or "--task" in args:
        return run_task(args)
    if "-V" in args or "--version" in args:
        print("incode 0.0.0-fake")
        return 0
    sys.stderr.write("fake incode: unsupported invocation %r\n" % (args,))
    return 64


if __name__ == "__main__":
    sys.exit(main())
