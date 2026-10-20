"""asyncio wrappers. The work still happens on threads; this only makes it awaitable.

Callbacks (``on_permission``, ``on_read_file``, ...) stay plain functions and run
on the session's worker threads.
"""

import asyncio
import functools
from typing import Any, AsyncIterator, Dict, List, Mapping, Optional, Sequence, Union

from .events import Event, PromptResult
from .session import PromptStream, Session
from .task import TaskResult, run_task

__all__ = ["AsyncSession", "run_task_async"]

_END = object()


async def _in_thread(fn: Any, *args: Any, **kwargs: Any) -> Any:
    loop = asyncio.get_running_loop()
    return await loop.run_in_executor(None, functools.partial(fn, *args, **kwargs))


async def run_task_async(task: str, **kwargs: Any) -> TaskResult:
    """:func:`incode_sdk.run_task`, awaitable."""
    return await _in_thread(run_task, task, **kwargs)


class AsyncSession:
    """:class:`incode_sdk.Session`, awaitable. Takes the same arguments.

    ::

        async with AsyncSession(workspace=".") as s:
            async for event in s.prompt("hello"):
                print(event)
    """

    def __init__(self, *args: Any, **kwargs: Any):
        self.sync = Session(*args, **kwargs)

    async def __aenter__(self) -> "AsyncSession":
        await self.start()
        return self

    async def __aexit__(self, *exc: Any) -> None:
        await self.close()

    async def start(self) -> str:
        return await _in_thread(self.sync.start)

    async def close(self) -> None:
        await _in_thread(self.sync.close)

    @property
    def session_id(self) -> Optional[str]:
        return self.sync.session_id

    async def prompt(
        self, prompt: Union[str, Sequence[Mapping[str, Any]]], *, timeout: Optional[float] = None
    ) -> AsyncIterator[Union[Event, PromptResult]]:
        stream: PromptStream = await _in_thread(self.sync.prompt, prompt, timeout=timeout)
        while True:
            item = await _in_thread(next, stream, _END)
            if item is _END:
                return
            yield item

    async def cancel(self) -> None:
        await _in_thread(self.sync.cancel)

    async def set_mode(self, mode_id: str) -> None:
        await _in_thread(self.sync.set_mode, mode_id)

    async def set_config_option(self, config_id: str, value: str) -> List[Dict[str, Any]]:
        return await _in_thread(self.sync.set_config_option, config_id, value)
