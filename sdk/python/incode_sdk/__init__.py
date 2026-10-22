"""Drive the incode coding agent from Python.

* :func:`run_task` — one instruction, ``incode -t ... --output-format json``.
* :func:`stream_task` — the same, ``stream-json``: each event as it arrives.
* :class:`Session` — an interactive session over ACP, ``incode --acp``.
* :mod:`incode_sdk.aio` — awaitable wrappers of both.
"""

__version__ = "0.1.0"

from .errors import AcpError, AgentCrashed, IncodeError, IncodeTimeout, ProtocolError, TaskFailed  # noqa: E402
from .events import (  # noqa: E402
    AvailableCommands,
    ConfigOptionsChanged,
    Event,
    MessageChunk,
    ModeChanged,
    PermissionOption,
    PermissionRequest,
    Plan,
    PlanEntry,
    PromptResult,
    ReadFileRequest,
    ThoughtChunk,
    ToolCall,
    ToolCallUpdate,
    UnknownUpdate,
    Usage,
    UserMessageChunk,
    WriteFileRequest,
)
from .fs import FsAccessDenied, FsError, WorkspaceFS  # noqa: E402
from .session import PromptStream, Session, allow_all, reject_all  # noqa: E402
from .task import TaskEvent, TaskResult, TaskStream, TaskUsage, run_task, stream_task  # noqa: E402

__all__ = [
    "__version__",
    "run_task",
    "stream_task",
    "TaskEvent",
    "TaskStream",
    "TaskResult",
    "TaskUsage",
    "Session",
    "PromptStream",
    "allow_all",
    "reject_all",
    "Event",
    "MessageChunk",
    "ThoughtChunk",
    "UserMessageChunk",
    "ToolCall",
    "ToolCallUpdate",
    "Plan",
    "PlanEntry",
    "Usage",
    "ModeChanged",
    "ConfigOptionsChanged",
    "AvailableCommands",
    "UnknownUpdate",
    "PromptResult",
    "PermissionOption",
    "PermissionRequest",
    "ReadFileRequest",
    "WriteFileRequest",
    "WorkspaceFS",
    "FsError",
    "FsAccessDenied",
    "IncodeError",
    "TaskFailed",
    "IncodeTimeout",
    "ProtocolError",
    "AcpError",
    "AgentCrashed",
]
