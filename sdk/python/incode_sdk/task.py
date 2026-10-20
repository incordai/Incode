"""One-shot runs: ``incode -t "<task>" --output-format json``."""

import json
import os
import subprocess
from dataclasses import dataclass, field
from typing import Any, Dict, List, Mapping, Optional, Sequence, Union

from .errors import IncodeError, IncodeTimeout, ProtocolError, TaskFailed

__all__ = ["TaskResult", "TaskUsage", "run_task", "build_task_argv"]

PathLike = Union[str, "os.PathLike[str]"]


@dataclass
class TaskUsage:
    """Token counts the agent reported for the run."""

    input_tokens: int = 0
    output_tokens: int = 0
    cache_creation_tokens: int = 0
    cache_read_tokens: int = 0

    @classmethod
    def from_json(cls, data: Optional[Mapping[str, Any]]) -> "TaskUsage":
        data = data or {}
        return cls(
            input_tokens=int(data.get("input_tokens") or 0),
            output_tokens=int(data.get("output_tokens") or 0),
            cache_creation_tokens=int(data.get("cache_creation_tokens") or 0),
            cache_read_tokens=int(data.get("cache_read_tokens") or 0),
        )


@dataclass
class TaskResult:
    """The ``result`` object ``incode -t ... --output-format json`` prints (src/task_run.rs).

    ``result`` is the agent's final answer when the run succeeded; ``error`` says
    why it did not. ``cost_usd`` is ``None`` for a model incode has no price for.
    ``raw`` is the whole object as printed, for fields this class does not name.
    """

    is_error: bool
    result: Optional[str]
    error: Optional[str]
    session_id: str
    agent: Optional[str]
    model: Optional[str]
    agent_session_id: Optional[str]
    stop_reason: Optional[str]
    served_by: List[str]
    usage: TaskUsage
    cost_usd: Optional[float]
    duration_ms: Optional[int]
    spec_gate: Optional[str]
    exit_code: int = 0
    stderr: str = ""
    raw: Dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, data: Mapping[str, Any], *, exit_code: int = 0, stderr: str = "") -> "TaskResult":
        cost = data.get("cost_usd")
        duration = data.get("duration_ms")
        return cls(
            is_error=bool(data.get("is_error")),
            result=data.get("result"),
            error=data.get("error"),
            session_id=str(data.get("session_id") or ""),
            agent=data.get("agent"),
            model=data.get("model"),
            agent_session_id=data.get("agent_session_id"),
            stop_reason=data.get("stop_reason"),
            served_by=list(data.get("served_by") or []),
            usage=TaskUsage.from_json(data.get("usage")),
            cost_usd=float(cost) if isinstance(cost, (int, float)) else None,
            duration_ms=int(duration) if isinstance(duration, (int, float)) else None,
            spec_gate=data.get("spec_gate"),
            exit_code=exit_code,
            stderr=stderr,
            raw=dict(data),
        )


def build_task_argv(
    task: str,
    *,
    workspace: Optional[PathLike] = None,
    model: Optional[str] = None,
    permission_mode: Optional[str] = None,
    incode_path: Union[PathLike, Sequence[str]] = "incode",
    extra_args: Sequence[str] = (),
) -> List[str]:
    """The command line :func:`run_task` runs."""
    argv = _command(incode_path)
    argv += ["-t", task, "--output-format", "json"]
    if workspace is not None:
        argv += ["-w", os.fspath(workspace)]
    if model:
        argv += ["-m", model]
    if permission_mode:
        argv += ["-p", permission_mode]
    argv += list(extra_args)
    return argv


def _command(incode_path: Union[PathLike, Sequence[str]]) -> List[str]:
    if isinstance(incode_path, (str, bytes)) or hasattr(incode_path, "__fspath__"):
        return [os.fspath(incode_path)]  # type: ignore[arg-type]
    return [os.fspath(part) for part in incode_path]


def _last_json_object(stdout: str) -> Optional[Dict[str, Any]]:
    """The last line of stdout that is a JSON object (the ``result``)."""
    for line in reversed(stdout.splitlines()):
        line = line.strip()
        if not line:
            continue
        try:
            value = json.loads(line)
        except ValueError:
            return None
        return value if isinstance(value, dict) else None
    return None


def run_task(
    task: str,
    *,
    workspace: Optional[PathLike] = None,
    model: Optional[str] = None,
    permission_mode: Optional[str] = None,
    timeout: Optional[float] = None,
    incode_path: Union[PathLike, Sequence[str]] = "incode",
    env: Optional[Mapping[str, str]] = None,
    extra_args: Sequence[str] = (),
) -> TaskResult:
    """Run one instruction with ``incode -t`` and return its parsed result.

    Raises :class:`TaskFailed` when incode reports ``is_error: true`` (the
    parsed result is on ``.result``), :class:`IncodeTimeout` when ``timeout``
    seconds pass (the process is killed), and :class:`IncodeError` when incode
    cannot be started or exits without a result object. Every error carries
    incode's stderr.

    Note: incode 0.1.0 accepts ``-p`` together with ``-t`` but does not apply it;
    the one-shot run uses the agent CLI's own permission handling.
    """
    argv = build_task_argv(
        task,
        workspace=workspace,
        model=model,
        permission_mode=permission_mode,
        incode_path=incode_path,
        extra_args=extra_args,
    )
    full_env = None
    if env is not None:
        full_env = dict(os.environ)
        full_env.update(env)
    try:
        proc = subprocess.run(
            argv,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            env=full_env,
        )
    except FileNotFoundError as e:
        raise IncodeError("could not start incode: %s (looked for %r)" % (e.strerror, argv[0])) from e
    except PermissionError as e:
        raise IncodeError("could not start incode: %s (%r)" % (e.strerror, argv[0])) from e
    except subprocess.TimeoutExpired as e:
        stderr = _text(e.stderr)
        raise IncodeTimeout("incode -t did not finish within %ss" % timeout, stderr=stderr) from e

    stdout = _text(proc.stdout)
    stderr = _text(proc.stderr)
    data = _last_json_object(stdout)
    if data is None:
        if proc.returncode != 0:
            raise IncodeError("incode -t failed before producing a result", stderr=stderr, exit_code=proc.returncode)
        raise ProtocolError(
            "incode -t printed no JSON result object (stdout: %r)" % stdout[-500:],
            stderr=stderr,
            exit_code=proc.returncode,
        )
    result = TaskResult.from_json(data, exit_code=proc.returncode, stderr=stderr)
    if result.is_error or proc.returncode != 0:
        raise TaskFailed(
            "incode -t reported an error: %s" % (result.error or "unknown error"),
            result=result,
            stderr=stderr,
            exit_code=proc.returncode,
        )
    return result


def _text(data: Any) -> str:
    if data is None:
        return ""
    if isinstance(data, bytes):
        return data.decode("utf-8", errors="replace")
    return str(data)
