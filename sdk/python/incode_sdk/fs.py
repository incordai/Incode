"""The client's file system, as offered to incode (``fs/read_text_file``, ``fs/write_text_file``).

The default handlers serve files inside the session's workspace only. A path
is resolved first — symlinks followed, ``..`` collapsed — and refused when the
real path is not under the workspace's real path.
"""

import os
from typing import Optional, Union

from .events import ReadFileRequest, WriteFileRequest

__all__ = ["FsError", "FsAccessDenied", "WorkspaceFS", "INVALID_PARAMS", "RESOURCE_NOT_FOUND", "INTERNAL_ERROR"]

INVALID_PARAMS = -32602
RESOURCE_NOT_FOUND = -32002
INTERNAL_ERROR = -32603


class FsError(Exception):
    """Raise from an fs callback to answer with this JSON-RPC error."""

    def __init__(self, message: str, code: int = INTERNAL_ERROR):
        super().__init__(message)
        self.code = code
        self.message = message


class FsAccessDenied(FsError):
    """The path is outside the workspace (after resolving symlinks and ``..``)."""

    def __init__(self, message: str):
        super().__init__(message, INVALID_PARAMS)


class WorkspaceFS:
    """Reads and writes confined to one directory tree."""

    def __init__(self, root: Union[str, "os.PathLike[str]"]):
        self.root = os.path.realpath(os.fspath(root))

    def resolve(self, path: str) -> str:
        """The real path of ``path``, or :class:`FsAccessDenied` when it is outside the root."""
        if not isinstance(path, str) or not path:
            raise FsAccessDenied("access denied: no path given")
        if "\x00" in path:
            raise FsAccessDenied("access denied: the path contains a NUL byte")
        if not os.path.isabs(path):
            # ACP paths are absolute; a relative one is taken against the root.
            path = os.path.join(self.root, path)
        real = os.path.realpath(path)
        if not self.contains(real):
            raise FsAccessDenied("access denied: %s is outside the workspace %s" % (path, self.root))
        return real

    def contains(self, real: str) -> bool:
        try:
            return os.path.commonpath([self.root, real]) == self.root
        except ValueError:  # different drives on Windows
            return False

    def read(self, request: ReadFileRequest) -> str:
        real = self.resolve(request.path)
        try:
            with open(real, "r", encoding="utf-8", errors="replace", newline="") as f:
                text = f.read()
        except FileNotFoundError:
            raise FsError("file not found: %s" % request.path, RESOURCE_NOT_FOUND)
        except IsADirectoryError:
            raise FsError("is a directory: %s" % request.path, INVALID_PARAMS)
        return slice_lines(text, request.line, request.limit)

    def write(self, request: WriteFileRequest) -> None:
        real = self.resolve(request.path)
        parent = os.path.dirname(real)
        os.makedirs(parent, exist_ok=True)
        # Resolve again now that the parents exist: nothing may have been
        # swapped for a symlink out of the tree in between.
        real = self.resolve(real)
        if os.path.isdir(real):
            raise FsError("is a directory: %s" % request.path, INVALID_PARAMS)
        with open(real, "w", encoding="utf-8", newline="") as f:
            f.write(request.content)


def slice_lines(text: str, line: Optional[int], limit: Optional[int]) -> str:
    """Lines ``line`` (1-based) onward, at most ``limit`` of them."""
    if line is None and limit is None:
        return text
    lines = text.splitlines(keepends=True)
    start = max((line or 1) - 1, 0)
    end = len(lines) if limit is None else start + max(limit, 0)
    return "".join(lines[start:end])
