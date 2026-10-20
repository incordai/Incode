"""Interactive sessions: ``incode --acp`` driven over ACP (JSON-RPC 2.0, one message per line).

One :class:`Session` is one ``incode --acp`` process holding one ACP session.
A reader thread parses incode's stdout; requests incode sends the client
(permission, file reads and writes) are answered on threads of their own, so a
slow callback never stops ``session/update`` or a cancel from flowing.
"""

import collections
import itertools
import json
import os
import queue
import subprocess
import threading
import time
from typing import Any, Callable, Deque, Dict, Iterator, List, Mapping, Optional, Sequence, Union

from . import __version__
from .errors import AcpError, AgentCrashed, IncodeError, IncodeTimeout
from .events import (
    ConfigOptionsChanged,
    Event,
    MessageChunk,
    ModeChanged,
    PermissionOption,
    PermissionRequest,
    PromptResult,
    ReadFileRequest,
    WriteFileRequest,
    parse_update,
)
from .fs import INTERNAL_ERROR, INVALID_PARAMS, RESOURCE_NOT_FOUND, FsError, WorkspaceFS

__all__ = ["Session", "PromptStream", "reject_all", "allow_all", "PROTOCOL_VERSION"]

PROTOCOL_VERSION = 1
METHOD_NOT_FOUND = -32601
STDERR_TAIL_BYTES = 64 * 1024

PermissionDecision = Union[str, PermissionOption, bool, None]
PermissionHandler = Callable[[PermissionRequest], PermissionDecision]
ReadHandler = Callable[[ReadFileRequest], str]
WriteHandler = Callable[[WriteFileRequest], None]


def reject_all(request: PermissionRequest) -> PermissionDecision:
    """The default permission handler: pick the reject option (``cancelled`` if there is none)."""
    return request.reject()


def allow_all(request: PermissionRequest) -> PermissionDecision:
    """Allow every call once. Only for trusted, sandboxed work."""
    return request.allow()


class _Pending:
    __slots__ = ("method", "done", "message", "callback")

    def __init__(self, method: str, callback: Optional[Callable[[Optional[dict], Optional[BaseException]], None]] = None):
        self.method = method
        self.done = threading.Event()
        self.message: Optional[dict] = None
        self.callback = callback



class PromptStream:
    """The events of one prompt, in order, ending with a :class:`PromptResult`.

    Iterate it (``for event in stream``). ``text`` accumulates the agent's
    message chunks; ``result`` is set once the prompt ends. ``wait()`` consumes
    the rest and returns the result.
    """

    def __init__(self, session: "Session", timeout: Optional[float]):
        self._session = session
        self._queue: "queue.Queue[Any]" = queue.Queue()
        self._deadline = None if timeout is None else time.monotonic() + timeout
        self._timeout = timeout
        self._finished = False
        self.text = ""
        self.result: Optional[PromptResult] = None
        self.events: List[Event] = []

    def __iter__(self) -> "PromptStream":
        return self

    def __next__(self) -> Union[Event, PromptResult]:
        if self._finished:
            raise StopIteration
        try:
            if self._deadline is None:
                item = self._queue.get()
            else:
                item = self._queue.get(timeout=max(self._deadline - time.monotonic(), 0))
        except queue.Empty:
            self._finished = True
            self._session.cancel()
            raise IncodeTimeout("the prompt did not end within %ss (a cancel was sent)" % self._timeout, stderr=self._session.stderr)
        if isinstance(item, PromptResult):
            self._finished = True
            self.result = item
            return item
        if isinstance(item, BaseException):
            self._finished = True
            raise item
        if isinstance(item, MessageChunk):
            self.text += item.text
        self.events.append(item)
        return item

    def wait(self) -> PromptResult:
        """Consume the remaining events and return how the prompt ended."""
        for _ in self:
            pass
        assert self.result is not None
        return self.result

    def cancel(self) -> None:
        self._session.cancel()

    # Called from the reader thread.
    def _put(self, item: Any) -> None:
        self._queue.put(item)


class Session:
    """``incode --acp`` as a context manager.

    ::

        with Session(workspace="/path/to/repo") as s:
            for event in s.prompt("Explain src/main.rs"):
                ...

    ``on_permission(request) -> decision`` answers ``session/request_permission``:
    return an option id (``request.allow()``, ``request.reject()``), ``True`` /
    ``False``, or ``None`` for ``cancelled``. The default rejects.

    ``on_read_file(ReadFileRequest) -> str`` and ``on_write_file(WriteFileRequest)``
    answer ``fs/read_text_file`` and ``fs/write_text_file``. The defaults serve the
    workspace only (symlinks and ``..`` resolved first). Pass ``False`` to not
    offer the capability at all.

    ``on_update(event)`` receives ``session/update`` events that arrive while no
    prompt is running (the slash commands after ``session/new``, MCP server
    status, history replayed by ``load_session_id``); they are also kept in
    ``idle_events``. ``on_protocol_error(line, error)`` hears about lines on
    incode's stdout that are not JSON-RPC; they are skipped.
    """

    def __init__(
        self,
        workspace: Optional[Union[str, "os.PathLike[str]"]] = None,
        *,
        incode_path: Union[str, "os.PathLike[str]", Sequence[str]] = "incode",
        model: Optional[str] = None,
        permission_mode: Optional[str] = None,
        on_permission: Optional[PermissionHandler] = None,
        on_read_file: Union[ReadHandler, None, bool] = None,
        on_write_file: Union[WriteHandler, None, bool] = None,
        on_update: Optional[Callable[[Event], None]] = None,
        on_protocol_error: Optional[Callable[[str, Exception], None]] = None,
        load_session_id: Optional[str] = None,
        mcp_servers: Optional[List[Mapping[str, Any]]] = None,
        env: Optional[Mapping[str, str]] = None,
        extra_args: Sequence[str] = (),
        request_timeout: Optional[float] = 120.0,
    ):
        self.workspace = os.path.abspath(os.fspath(workspace) if workspace is not None else os.getcwd())
        self.incode_path = incode_path
        self.model = model
        self.permission_mode = permission_mode
        self.on_permission: PermissionHandler = on_permission or reject_all
        self.fs = WorkspaceFS(self.workspace)
        self._read: Optional[ReadHandler] = None if on_read_file is False else (on_read_file if callable(on_read_file) else self.fs.read)
        self._write: Optional[WriteHandler] = None if on_write_file is False else (on_write_file if callable(on_write_file) else self.fs.write)
        self.on_update = on_update
        self.on_protocol_error = on_protocol_error
        self.load_session_id = load_session_id
        self.mcp_servers = list(mcp_servers or [])
        self.env = env
        self.extra_args = list(extra_args)
        self.request_timeout = request_timeout

        # Filled by start().
        self.session_id: Optional[str] = None
        self.agent_info: Dict[str, Any] = {}
        self.agent_capabilities: Dict[str, Any] = {}
        self.protocol_version: Optional[int] = None
        self.current_mode: Optional[str] = None
        self.available_modes: List[Dict[str, Any]] = []
        self.config_options: List[Dict[str, Any]] = []
        self.idle_events: Deque[Event] = collections.deque(maxlen=1000)
        self.protocol_errors: Deque[str] = collections.deque(maxlen=100)
        self.callback_errors: Deque[BaseException] = collections.deque(maxlen=100)

        self._proc: Optional[subprocess.Popen] = None
        self._ids = itertools.count(1)
        self._write_lock = threading.Lock()
        self._lock = threading.Lock()
        self._pending: Dict[Any, _Pending] = {}
        self._active: Optional[PromptStream] = None
        self._open_permissions: Dict[Any, str] = {}  # request id -> session id, until answered
        self._answered: set = set()
        self._stderr: Deque[bytes] = collections.deque()
        self._stderr_size = 0
        self._dead: Optional[IncodeError] = None
        self._closing = False
        self._threads: List[threading.Thread] = []

    # ------------------------------------------------------------ lifecycle

    def __enter__(self) -> "Session":
        if self._proc is None:
            self.start()
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def argv(self) -> List[str]:
        """The command line ``start()`` runs."""
        if isinstance(self.incode_path, (str, bytes)) or hasattr(self.incode_path, "__fspath__"):
            argv = [os.fspath(self.incode_path)]  # type: ignore[arg-type]
        else:
            argv = [os.fspath(p) for p in self.incode_path]
        argv.append("--acp")
        if self.model:
            argv += ["-m", self.model]
        if self.permission_mode:
            argv += ["-p", self.permission_mode]
        return argv + self.extra_args

    def start(self) -> str:
        """Start ``incode --acp``, ``initialize``, and open (or load) the session. Returns its id."""
        if self._proc is not None:
            raise IncodeError("this Session was already started")
        full_env = None
        if self.env is not None:
            full_env = dict(os.environ)
            full_env.update(self.env)
        argv = self.argv()
        try:
            self._proc = subprocess.Popen(
                argv,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env=full_env,
                bufsize=-1,
            )
        except OSError as e:
            raise IncodeError("could not start incode: %s (%r)" % (e.strerror or e, argv[0])) from e
        for target, name in ((self._read_stdout, "incode-acp-reader"), (self._read_stderr, "incode-acp-stderr")):
            t = threading.Thread(target=target, name=name, daemon=True)
            t.start()
            self._threads.append(t)
        try:
            init = self.request(
                "initialize",
                {
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientCapabilities": {
                        "fs": {"readTextFile": self._read is not None, "writeTextFile": self._write is not None},
                        "terminal": False,
                    },
                    "clientInfo": {"name": "incode-sdk-python", "version": __version__},
                },
            )
            self.protocol_version = init.get("protocolVersion")
            self.agent_capabilities = dict(init.get("agentCapabilities") or {})
            self.agent_info = dict(init.get("agentInfo") or {})
            if self.load_session_id:
                opened = self.request(
                    "session/load",
                    {"sessionId": self.load_session_id, "cwd": self.workspace, "mcpServers": self.mcp_servers},
                )
                self.session_id = self.load_session_id
            else:
                opened = self.request("session/new", {"cwd": self.workspace, "mcpServers": self.mcp_servers})
                self.session_id = str(opened["sessionId"])
        except BaseException:
            self.close()
            raise
        modes = opened.get("modes") or {}
        self.current_mode = modes.get("currentModeId")
        self.available_modes = list(modes.get("availableModes") or [])
        self.config_options = list(opened.get("configOptions") or [])
        return self.session_id

    def close(self, timeout: float = 5.0) -> None:
        """Close the session and stop the process (stdin closed, then terminate, then kill)."""
        proc = self._proc
        if proc is None or self._closing:
            return
        if self._dead is None and self.session_id and "close" in (self.agent_capabilities.get("sessionCapabilities") or {}):
            try:
                self.request("session/close", {"sessionId": self.session_id}, timeout=min(timeout, 2.0))
            except IncodeError:
                pass
        self._closing = True
        try:
            if proc.stdin:
                proc.stdin.close()
        except OSError:
            pass
        try:
            proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            proc.terminate()
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        for t in self._threads:
            if t is not threading.current_thread():
                t.join(timeout=2)
        for pipe in (proc.stdout, proc.stderr):
            try:
                if pipe:
                    pipe.close()
            except OSError:
                pass
        self._fail_all(IncodeError("the session was closed", stderr=self.stderr, exit_code=proc.returncode))

    @property
    def stderr(self) -> str:
        """The last 64 KiB incode wrote to stderr."""
        with self._lock:
            data = b"".join(self._stderr)
        return data.decode("utf-8", errors="replace")

    @property
    def returncode(self) -> Optional[int]:
        return None if self._proc is None else self._proc.poll()

    # ------------------------------------------------------------ requests

    def request(self, method: str, params: Optional[Mapping[str, Any]] = None, timeout: Optional[float] = -1) -> Dict[str, Any]:
        """Send a JSON-RPC request and wait for its result (``AcpError`` on an error reply).

        ``timeout`` defaults to the session's ``request_timeout``; ``None`` waits forever.
        """
        if timeout == -1:
            timeout = self.request_timeout
        pending = self._send_request(method, params)
        if not pending.done.wait(timeout):
            with self._lock:
                self._pending = {k: v for k, v in self._pending.items() if v is not pending}
            raise IncodeTimeout("no answer to %s within %ss" % (method, timeout), stderr=self.stderr)
        return self._result_of(pending)

    def notify(self, method: str, params: Optional[Mapping[str, Any]] = None) -> None:
        """Send a JSON-RPC notification."""
        self._send({"jsonrpc": "2.0", "method": method, "params": dict(params or {})})

    def _send_request(self, method: str, params: Optional[Mapping[str, Any]], callback: Any = None) -> _Pending:
        if self._dead is not None:
            raise self._dead
        if self._proc is None:
            raise IncodeError("the session is not started")
        rid = next(self._ids)
        pending = _Pending(method, callback)
        with self._lock:
            self._pending[rid] = pending
        try:
            self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": dict(params or {})})
        except BaseException:
            with self._lock:
                self._pending.pop(rid, None)
            raise
        return pending

    def _result_of(self, pending: _Pending) -> Dict[str, Any]:
        message = pending.message
        if message is None:
            raise self._dead or IncodeError("the session was closed")
        if "error" in message and message["error"] is not None:
            error = message["error"] if isinstance(message["error"], dict) else {"message": str(message["error"])}
            raise AcpError(pending.method, int(error.get("code") or 0), str(error.get("message") or ""), error.get("data"), stderr=self.stderr)
        result = message.get("result")
        return result if isinstance(result, dict) else {}

    def _send(self, message: Mapping[str, Any]) -> None:
        data = (json.dumps(message, ensure_ascii=False) + "\n").encode("utf-8")
        proc = self._proc
        if proc is None or proc.stdin is None:
            raise IncodeError("the session is not started")
        with self._write_lock:
            try:
                proc.stdin.write(data)
                proc.stdin.flush()
            except (BrokenPipeError, OSError, ValueError) as e:
                raise self._dead or AgentCrashed("incode is no longer reading its stdin: %s" % e, stderr=self.stderr, exit_code=proc.poll()) from e

    # ------------------------------------------------------------ session methods

    def prompt(self, prompt: Union[str, Sequence[Mapping[str, Any]]], *, timeout: Optional[float] = None) -> PromptStream:
        """Send ``session/prompt``. Returns a :class:`PromptStream` of events ending with a :class:`PromptResult`.

        ``prompt`` is text, or a list of ACP content blocks. ``timeout`` bounds the
        whole prompt; when it passes, a cancel is sent and :class:`IncodeTimeout` raised.
        """
        if self.session_id is None:
            raise IncodeError("the session is not started")
        blocks = [{"type": "text", "text": prompt}] if isinstance(prompt, str) else [dict(b) for b in prompt]
        stream = PromptStream(self, timeout)
        with self._lock:
            if self._active is not None:
                raise IncodeError("a prompt is already running in this session")
            self._active = stream

        def done(message: Optional[dict], error: Optional[BaseException]) -> None:
            # On the reader thread, after every update that came before the reply.
            with self._lock:
                if self._active is stream:
                    self._active = None
            if error is not None:
                stream._put(error)
                return
            assert message is not None
            if message.get("error") is not None:
                e = message["error"] if isinstance(message["error"], dict) else {"message": str(message["error"])}
                stream._put(AcpError("session/prompt", int(e.get("code") or 0), str(e.get("message") or ""), e.get("data"), stderr=self.stderr))
                return
            result = message.get("result") if isinstance(message.get("result"), dict) else {}
            stream._put(PromptResult(stop_reason=str(result.get("stopReason") or ""), raw=result))

        try:
            self._send_request("session/prompt", {"sessionId": self.session_id, "prompt": blocks}, callback=done)
        except BaseException:
            with self._lock:
                self._active = None
            raise
        return stream

    def cancel(self) -> None:
        """Send ``session/cancel``; the running prompt ends with ``stop_reason == "cancelled"``.

        Any permission question still open is answered ``cancelled``, as ACP asks of a client.
        """
        if self.session_id is None or self._dead is not None:
            return
        self.notify("session/cancel", {"sessionId": self.session_id})
        with self._lock:
            open_ids = [rid for rid, sid in self._open_permissions.items() if sid == self.session_id]
        for rid in open_ids:
            self._answer(rid, result={"outcome": {"outcome": "cancelled"}})

    def set_mode(self, mode_id: str) -> None:
        """``session/set_mode``: ``read-only``, ``workspace-write`` or ``danger-full-access``."""
        self.request("session/set_mode", {"sessionId": self.session_id, "modeId": mode_id})
        self.current_mode = mode_id

    def set_config_option(self, config_id: str, value: str) -> List[Dict[str, Any]]:
        """``session/set_config_option`` (``agent`` or ``role``). Returns every option as it now stands."""
        result = self.request("session/set_config_option", {"sessionId": self.session_id, "configId": config_id, "value": value})
        self.config_options = list(result.get("configOptions") or [])
        return self.config_options

    def list_sessions(self) -> List[Dict[str, Any]]:
        """``session/list`` for this workspace."""
        return list(self.request("session/list", {"cwd": self.workspace}).get("sessions") or [])

    # ------------------------------------------------------------ reader side

    def _read_stdout(self) -> None:
        proc = self._proc
        assert proc is not None and proc.stdout is not None
        try:
            for raw in iter(proc.stdout.readline, b""):
                line = raw.decode("utf-8", errors="replace").strip()
                if line:
                    self._dispatch(line)
        except (OSError, ValueError):
            pass
        self._stdout_closed()

    def _read_stderr(self) -> None:
        proc = self._proc
        assert proc is not None and proc.stderr is not None
        try:
            for chunk in iter(lambda: proc.stderr.read1(4096) if hasattr(proc.stderr, "read1") else proc.stderr.read(4096), b""):
                with self._lock:
                    self._stderr.append(chunk)
                    self._stderr_size += len(chunk)
                    while self._stderr_size > STDERR_TAIL_BYTES and len(self._stderr) > 1:
                        self._stderr_size -= len(self._stderr.popleft())
        except (OSError, ValueError):
            pass

    def _protocol_error(self, line: str, error: Exception) -> None:
        self.protocol_errors.append(line[:1000])
        if self.on_protocol_error is not None:
            try:
                self.on_protocol_error(line, error)
            except Exception as e:  # a callback must not stop the reader
                self.callback_errors.append(e)

    def _dispatch(self, line: str) -> None:
        try:
            message = json.loads(line)
        except ValueError as e:
            self._protocol_error(line, e)
            return
        if not isinstance(message, dict):
            self._protocol_error(line, ValueError("not a JSON-RPC object"))
            return
        method = message.get("method")
        if isinstance(method, str):
            params = message.get("params") if isinstance(message.get("params"), dict) else {}
            if message.get("id") is not None:
                t = threading.Thread(target=self._serve, args=(message["id"], method, params), daemon=True)
                t.start()
            elif method == "session/update":
                self._update(params)
            return
        if "id" in message and ("result" in message or "error" in message):
            with self._lock:
                pending = self._pending.pop(message["id"], None)
            if pending is None:
                self._protocol_error(line, ValueError("a response to no request this client sent"))
                return
            pending.message = message
            pending.done.set()
            if pending.callback is not None:
                pending.callback(message, None)
            return
        self._protocol_error(line, ValueError("neither a request, a notification nor a response"))

    def _update(self, params: Mapping[str, Any]) -> None:
        update = params.get("update")
        if not isinstance(update, Mapping):
            self._protocol_error(json.dumps(params)[:1000], ValueError("session/update without an update"))
            return
        event = parse_update(str(params.get("sessionId") or ""), update)
        if isinstance(event, ModeChanged):
            self.current_mode = event.mode_id
        elif isinstance(event, ConfigOptionsChanged):
            self.config_options = list(event.config_options)
        with self._lock:
            active = self._active
        if active is not None:
            active._put(event)
            return
        self.idle_events.append(event)
        if self.on_update is not None:
            try:
                self.on_update(event)
            except Exception as e:
                self.callback_errors.append(e)

    def _stdout_closed(self) -> None:
        proc = self._proc
        code = None
        if proc is not None:
            try:
                code = proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                code = proc.poll()
        # Let the stderr reader catch the process's last words.
        for t in self._threads:
            if t.name == "incode-acp-stderr" and t is not threading.current_thread():
                t.join(timeout=1)
        if self._closing:
            error: IncodeError = IncodeError("the session was closed", stderr=self.stderr, exit_code=code)
        else:
            error = AgentCrashed("incode --acp exited unexpectedly", stderr=self.stderr, exit_code=code)
        self._fail_all(error)

    def _fail_all(self, error: IncodeError) -> None:
        with self._lock:
            if self._dead is None:
                self._dead = error
            pending = list(self._pending.values())
            self._pending.clear()
            active, self._active = self._active, None
        for p in pending:
            p.done.set()
            if p.callback is not None:
                p.callback(None, error)
        if active is not None:
            active._put(error)

    # ------------------------------------------------------------ agent -> client requests

    def _answer(self, rid: Any, result: Any = None, error: Optional[Mapping[str, Any]] = None) -> None:
        with self._lock:
            if rid in self._answered:
                return
            self._answered.add(rid)
            self._open_permissions.pop(rid, None)
        message: Dict[str, Any] = {"jsonrpc": "2.0", "id": rid}
        if error is not None:
            message["error"] = dict(error)
        else:
            message["result"] = result if result is not None else {}
        try:
            self._send(message)
        except IncodeError:
            pass

    def _serve(self, rid: Any, method: str, params: Mapping[str, Any]) -> None:
        try:
            if method == "session/request_permission":
                self._serve_permission(rid, params)
            elif method == "fs/read_text_file" and self._read is not None:
                self._serve_fs(rid, lambda: {"content": self._read(_read_request(params))})  # type: ignore[misc]
            elif method == "fs/write_text_file" and self._write is not None:
                self._serve_fs(rid, lambda: (self._write(_write_request(params)), {})[1])  # type: ignore[misc]
            else:
                self._answer(rid, error={"code": METHOD_NOT_FOUND, "message": "Method not found", "data": method})
        except Exception as e:  # never leave incode waiting
            self.callback_errors.append(e)
            self._answer(rid, error={"code": INTERNAL_ERROR, "message": "client error: %s" % e})

    def _serve_permission(self, rid: Any, params: Mapping[str, Any]) -> None:
        request = PermissionRequest.from_params(params)
        with self._lock:
            self._open_permissions[rid] = request.session_id
        try:
            decision = self.on_permission(request)
        except Exception as e:
            self.callback_errors.append(e)
            decision = request.reject()
        if decision is True:
            decision = request.allow()
        elif decision is False:
            decision = request.reject()
        if isinstance(decision, PermissionOption):
            decision = decision.option_id
        if decision is None:
            outcome: Dict[str, Any] = {"outcome": "cancelled"}
        elif decision in [o.option_id for o in request.options]:
            outcome = {"outcome": "selected", "optionId": decision}
        else:
            self.callback_errors.append(ValueError("on_permission returned %r, not one of the offered option ids" % (decision,)))
            fallback = request.reject()
            outcome = {"outcome": "selected", "optionId": fallback} if fallback else {"outcome": "cancelled"}
        self._answer(rid, result={"outcome": outcome})

    def _serve_fs(self, rid: Any, run: Callable[[], Dict[str, Any]]) -> None:
        try:
            result = run()
        except FsError as e:
            self._answer(rid, error={"code": e.code, "message": e.message})
        except FileNotFoundError as e:
            self._answer(rid, error={"code": RESOURCE_NOT_FOUND, "message": "file not found: %s" % (e.filename or e)})
        except (TypeError, KeyError) as e:
            self._answer(rid, error={"code": INVALID_PARAMS, "message": "invalid params: %s" % e})
        except OSError as e:
            self._answer(rid, error={"code": INTERNAL_ERROR, "message": "%s: %s" % (e.strerror or e, e.filename or "")})
        else:
            self._answer(rid, result=result)


def _read_request(params: Mapping[str, Any]) -> ReadFileRequest:
    line, limit = params.get("line"), params.get("limit")
    return ReadFileRequest(
        session_id=str(params.get("sessionId") or ""),
        path=str(params["path"]),
        line=int(line) if isinstance(line, (int, float)) else None,
        limit=int(limit) if isinstance(limit, (int, float)) else None,
    )


def _write_request(params: Mapping[str, Any]) -> WriteFileRequest:
    content = params["content"]
    if not isinstance(content, str):
        raise TypeError("content must be a string")
    return WriteFileRequest(session_id=str(params.get("sessionId") or ""), path=str(params["path"]), content=content)


def iter_text(stream: Iterator[Any]) -> Iterator[str]:
    """Just the agent's message text from a prompt's events."""
    for event in stream:
        if isinstance(event, MessageChunk):
            yield event.text
