"""Typed ``session/update`` events, permission requests and fs requests."""

from dataclasses import dataclass, field
from typing import Any, Dict, List, Mapping, Optional, Union

__all__ = [
    "Event",
    "MessageChunk",
    "ThoughtChunk",
    "UserMessageChunk",
    "ToolCall",
    "ToolCallUpdate",
    "PlanEntry",
    "Plan",
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
    "parse_update",
]


def _text_of(content: Any) -> str:
    if isinstance(content, Mapping) and content.get("type") == "text":
        return str(content.get("text") or "")
    return ""


@dataclass
class MessageChunk:
    """Part of the agent's reply (``agent_message_chunk``)."""

    session_id: str
    text: str
    content: Dict[str, Any] = field(default_factory=dict)
    raw: Dict[str, Any] = field(default_factory=dict, repr=False)


@dataclass
class ThoughtChunk:
    """Part of the agent's reasoning (``agent_thought_chunk``)."""

    session_id: str
    text: str
    content: Dict[str, Any] = field(default_factory=dict)
    raw: Dict[str, Any] = field(default_factory=dict, repr=False)


@dataclass
class UserMessageChunk:
    """The person's own words, replayed by ``session/load`` (``user_message_chunk``)."""

    session_id: str
    text: str
    content: Dict[str, Any] = field(default_factory=dict)
    raw: Dict[str, Any] = field(default_factory=dict, repr=False)


@dataclass
class ToolCall:
    """A tool call starts (``tool_call``)."""

    session_id: str
    tool_call_id: str
    title: str
    kind: Optional[str]
    status: Optional[str]
    raw_input: Any = None
    content: List[Any] = field(default_factory=list)
    locations: List[Any] = field(default_factory=list)
    raw: Dict[str, Any] = field(default_factory=dict, repr=False)


@dataclass
class ToolCallUpdate:
    """A tool call progresses or ends (``tool_call_update``). Unset fields are ``None``."""

    session_id: str
    tool_call_id: str
    status: Optional[str] = None
    title: Optional[str] = None
    kind: Optional[str] = None
    content: Optional[List[Any]] = None
    raw_input: Any = None
    raw_output: Any = None
    raw: Dict[str, Any] = field(default_factory=dict, repr=False)


@dataclass
class PlanEntry:
    content: str
    priority: Optional[str]
    status: Optional[str]


@dataclass
class Plan:
    """The task list as it now stands (``plan``); every entry, every time."""

    session_id: str
    entries: List[PlanEntry]
    raw: Dict[str, Any] = field(default_factory=dict, repr=False)


@dataclass
class Usage:
    """The context window's fill (``usage_update``): ``used`` of ``size`` tokens."""

    session_id: str
    used: Optional[int]
    size: Optional[int]
    cost: Any = None
    raw: Dict[str, Any] = field(default_factory=dict, repr=False)


@dataclass
class ModeChanged:
    """The session's permission mode changed (``current_mode_update``)."""

    session_id: str
    mode_id: str
    raw: Dict[str, Any] = field(default_factory=dict, repr=False)


@dataclass
class ConfigOptionsChanged:
    """The pickers changed (``config_option_update``)."""

    session_id: str
    config_options: List[Dict[str, Any]]
    raw: Dict[str, Any] = field(default_factory=dict, repr=False)


@dataclass
class AvailableCommands:
    """The slash commands the session offers (``available_commands_update``)."""

    session_id: str
    commands: List[Dict[str, Any]]
    raw: Dict[str, Any] = field(default_factory=dict, repr=False)


@dataclass
class UnknownUpdate:
    """A ``session/update`` kind this SDK does not model; ``kind`` names it."""

    session_id: str
    kind: str
    raw: Dict[str, Any] = field(default_factory=dict, repr=False)


@dataclass
class PromptResult:
    """How a prompt ended. ``stop_reason``: end_turn, cancelled, max_tokens, max_turn_requests, refusal."""

    stop_reason: str
    raw: Dict[str, Any] = field(default_factory=dict, repr=False)


Event = Union[
    MessageChunk,
    ThoughtChunk,
    UserMessageChunk,
    ToolCall,
    ToolCallUpdate,
    Plan,
    Usage,
    ModeChanged,
    ConfigOptionsChanged,
    AvailableCommands,
    UnknownUpdate,
]


def parse_update(session_id: str, update: Mapping[str, Any]) -> Event:
    """Turn one ``params.update`` object into a typed event."""
    raw = dict(update)
    kind = str(update.get("sessionUpdate") or "")
    if kind in ("agent_message_chunk", "agent_thought_chunk", "user_message_chunk"):
        content = update.get("content")
        content = dict(content) if isinstance(content, Mapping) else {}
        cls = {"agent_message_chunk": MessageChunk, "agent_thought_chunk": ThoughtChunk}.get(kind, UserMessageChunk)
        return cls(session_id=session_id, text=_text_of(content), content=content, raw=raw)
    if kind == "tool_call":
        return ToolCall(
            session_id=session_id,
            tool_call_id=str(update.get("toolCallId") or ""),
            title=str(update.get("title") or ""),
            kind=update.get("kind"),
            status=update.get("status"),
            raw_input=update.get("rawInput"),
            content=list(update.get("content") or []),
            locations=list(update.get("locations") or []),
            raw=raw,
        )
    if kind == "tool_call_update":
        content = update.get("content")
        return ToolCallUpdate(
            session_id=session_id,
            tool_call_id=str(update.get("toolCallId") or ""),
            status=update.get("status"),
            title=update.get("title"),
            kind=update.get("kind"),
            content=list(content) if isinstance(content, list) else None,
            raw_input=update.get("rawInput"),
            raw_output=update.get("rawOutput"),
            raw=raw,
        )
    if kind == "plan":
        entries = [
            PlanEntry(content=str(e.get("content") or ""), priority=e.get("priority"), status=e.get("status"))
            for e in update.get("entries") or []
            if isinstance(e, Mapping)
        ]
        return Plan(session_id=session_id, entries=entries, raw=raw)
    if kind == "usage_update":
        used, size = update.get("used"), update.get("size")
        return Usage(
            session_id=session_id,
            used=int(used) if isinstance(used, (int, float)) else None,
            size=int(size) if isinstance(size, (int, float)) else None,
            cost=update.get("cost"),
            raw=raw,
        )
    if kind == "current_mode_update":
        return ModeChanged(session_id=session_id, mode_id=str(update.get("currentModeId") or ""), raw=raw)
    if kind == "config_option_update":
        return ConfigOptionsChanged(session_id=session_id, config_options=list(update.get("configOptions") or []), raw=raw)
    if kind == "available_commands_update":
        return AvailableCommands(session_id=session_id, commands=list(update.get("availableCommands") or []), raw=raw)
    return UnknownUpdate(session_id=session_id, kind=kind, raw=raw)


# ---------------------------------------------------------------- agent -> client requests


@dataclass
class PermissionOption:
    option_id: str
    name: str
    kind: str  # allow_once | allow_always | reject_once | reject_always


@dataclass
class PermissionRequest:
    """``session/request_permission``: incode asks before a tool call runs.

    Your ``on_permission`` callback returns one of ``options``' ids (or the
    option itself), or ``None`` to answer ``cancelled``. ``allow()``,
    ``allow_always()`` and ``reject()`` pick the matching option for you.
    """

    session_id: str
    tool_call: Dict[str, Any]
    options: List[PermissionOption]
    raw: Dict[str, Any] = field(default_factory=dict, repr=False)

    @property
    def title(self) -> str:
        return str(self.tool_call.get("title") or "")

    @property
    def tool_call_id(self) -> str:
        return str(self.tool_call.get("toolCallId") or "")

    def option_of_kind(self, *kinds: str) -> Optional[str]:
        for kind in kinds:
            for option in self.options:
                if option.kind == kind:
                    return option.option_id
        return None

    def allow(self) -> Optional[str]:
        """The allow-once option (or allow-always when that is all there is)."""
        return self.option_of_kind("allow_once", "allow_always")

    def allow_always(self) -> Optional[str]:
        return self.option_of_kind("allow_always", "allow_once")

    def reject(self) -> Optional[str]:
        """The reject-once option (or reject-always). ``None`` (cancelled) when there is none."""
        return self.option_of_kind("reject_once", "reject_always")

    @classmethod
    def from_params(cls, params: Mapping[str, Any]) -> "PermissionRequest":
        options = [
            PermissionOption(option_id=str(o.get("optionId")), name=str(o.get("name") or ""), kind=str(o.get("kind") or ""))
            for o in params.get("options") or []
            if isinstance(o, Mapping)
        ]
        tool_call = params.get("toolCall")
        return cls(
            session_id=str(params.get("sessionId") or ""),
            tool_call=dict(tool_call) if isinstance(tool_call, Mapping) else {},
            options=options,
            raw=dict(params),
        )


@dataclass
class ReadFileRequest:
    """``fs/read_text_file``. ``line`` is 1-based; ``limit`` is a number of lines."""

    session_id: str
    path: str
    line: Optional[int] = None
    limit: Optional[int] = None


@dataclass
class WriteFileRequest:
    """``fs/write_text_file``."""

    session_id: str
    path: str
    content: str
