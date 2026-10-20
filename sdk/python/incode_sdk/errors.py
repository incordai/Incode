"""Exceptions raised by the SDK."""

from typing import Any, Optional

__all__ = [
    "IncodeError",
    "TaskFailed",
    "IncodeTimeout",
    "ProtocolError",
    "AcpError",
    "AgentCrashed",
]


class IncodeError(Exception):
    """Base class for every error the SDK raises.

    ``stderr`` holds what incode wrote to stderr (the tail, for a session),
    ``exit_code`` the process's exit status when it has exited.
    """

    def __init__(self, message: str, *, stderr: str = "", exit_code: Optional[int] = None):
        super().__init__(message)
        self.message = message
        self.stderr = stderr
        self.exit_code = exit_code

    def __str__(self) -> str:
        text = self.message
        if self.exit_code is not None:
            text += " (exit code %s)" % self.exit_code
        tail = self.stderr.strip()
        if tail:
            tail = tail[-2000:]
            text += "\n--- incode stderr ---\n" + tail
        return text


class TaskFailed(IncodeError):
    """``incode -t`` ran and reported ``is_error: true``. ``result`` is the parsed TaskResult."""

    def __init__(self, message: str, *, result: Any, stderr: str = "", exit_code: Optional[int] = None):
        super().__init__(message, stderr=stderr, exit_code=exit_code)
        self.result = result


class IncodeTimeout(IncodeError):
    """The process or a request did not finish in time."""


class ProtocolError(IncodeError):
    """incode wrote something the SDK could not understand where it needed to."""


class AcpError(IncodeError):
    """incode answered an ACP request with a JSON-RPC error."""

    def __init__(self, method: str, code: int, message: str, data: Any = None, *, stderr: str = ""):
        detail = "%s failed: %s (code %s)" % (method, message, code)
        if data is not None:
            detail += ": %s" % (data,)
        super().__init__(detail, stderr=stderr)
        self.method = method
        self.code = code
        self.data = data


class AgentCrashed(IncodeError):
    """The ``incode --acp`` process went away while the SDK was waiting on it."""
