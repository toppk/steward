"""Python client for stewardd, the steward file index service.

Sync and asyncio clients over the daemon's unix socket
(``$XDG_RUNTIME_DIR/steward/service.socket``). The protocol is one JSON
object per line, one response line per request; see
``crates/steward-proto`` for the authoritative definitions.

    from steward_client import Client

    with Client() as c:
        for e in c.children("~"):
            print(e.total_alloc, e.path)
        print(c.content_id("~/Movies/film.mkv"))

    import asyncio
    from steward_client import AsyncClient

    async def main():
        async with AsyncClient() as c:
            print(await c.locate("*.torrent"))

    asyncio.run(main())

Standard library only; Python 3.9+.
"""

from __future__ import annotations

import asyncio
import json
import os
import socket
import tempfile
from dataclasses import dataclass, field, fields
from typing import Any, Callable, Union

__all__ = ["AsyncClient", "Client", "Entry", "StewardError", "socket_path"]

PathLike = Union[str, "os.PathLike[str]"]

# Directory listings of large trees are multi-megabyte single lines.
_LINE_LIMIT = 1 << 30


class StewardError(Exception):
    """An error the daemon returned, or a malformed response."""


@dataclass(frozen=True)
class Entry:
    """One indexed path. ``total_*`` are subtree totals for directories and
    the entry's own figures otherwise; ``alloc`` is bytes on disk."""

    path: str
    kind: str
    mode: int
    uid: int
    gid: int
    size: int
    alloc: int
    mtime: int
    total_size: int
    total_alloc: int
    total_files: int
    total_dirs: int
    tags: list[str] = field(default_factory=list)
    category: str | None = None
    content_id: str | None = None

    @classmethod
    def from_dict(cls, d: dict) -> Entry:
        # Ignore fields a newer daemon adds.
        return cls(**{f.name: d[f.name] for f in fields(cls) if f.name in d})

    @property
    def is_dir(self) -> bool:
        return self.kind == "dir"

    @property
    def name(self) -> str:
        return os.path.basename(self.path.rstrip("/")) or self.path


def socket_path() -> str:
    """Where stewardd listens, resolved the same way the daemon does."""
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if not runtime:
        runtime = os.path.join(
            tempfile.gettempdir(), f"steward-{os.stat('/proc/self').st_uid}"
        )
    return os.path.join(runtime, "steward", "service.socket")


def _abs(path: PathLike) -> str:
    return os.path.abspath(os.path.expanduser(os.fspath(path)))


def _identity(x: Any) -> Any:
    return x


def _entries(x: Any) -> list[Entry]:
    return [Entry.from_dict(d) for d in x]


# Each operation is (request, how to convert its result), shared by both
# clients so the sync and async APIs cannot drift apart.
_Op = tuple[dict, Callable[[Any], Any]]


def _op(op: str, conv: Callable[[Any], Any] = _identity, **kw: Any) -> _Op:
    return {"op": op, **kw}, conv


def _decode(line: bytes) -> Any:
    if not line:
        raise StewardError("stewardd closed the connection")
    try:
        resp = json.loads(line)
    except ValueError as e:
        raise StewardError(f"malformed response: {e}") from None
    if resp.get("status") == "ok":
        return resp.get("result")
    raise StewardError(resp.get("message", f"unexpected response: {resp!r}"))


class _Ops:
    """Request builders; each client wraps them with its transport."""

    @staticmethod
    def status() -> _Op:
        return _op("status")

    @staticmethod
    def reload() -> _Op:
        return _op("reload")

    @staticmethod
    def scan(path: PathLike, trust_dir_mtime: bool) -> _Op:
        return _op("scan", path=_abs(path), trust_dir_mtime=trust_dir_mtime)

    @staticmethod
    def invalidate(path: PathLike) -> _Op:
        return _op("invalidate", path=_abs(path))

    @staticmethod
    def stat(path: PathLike) -> _Op:
        return _op("stat", Entry.from_dict, path=_abs(path))

    @staticmethod
    def children(path: PathLike) -> _Op:
        return _op("children", _entries, path=_abs(path))

    @staticmethod
    def locate(pattern: str, limit: int) -> _Op:
        return _op("locate", pattern=pattern, limit=limit)

    @staticmethod
    def classify(path: PathLike) -> _Op:
        return _op("classify", path=_abs(path))

    @staticmethod
    def content_id(path: PathLike) -> _Op:
        return _op("content_id", path=_abs(path))

    @staticmethod
    def hash_tree(path: PathLike) -> _Op:
        return _op("hash_tree", path=_abs(path))

    @staticmethod
    def find_content(content_id: str) -> _Op:
        return _op("find_content", id=content_id)

    @staticmethod
    def duplicates(path: PathLike, limit: int) -> _Op:
        return _op("duplicates", path=_abs(path), limit=limit)

    @staticmethod
    def settings() -> _Op:
        return _op("settings")

    @staticmethod
    def put_root(root: dict) -> _Op:
        root = {**root, "path": _abs(root["path"])}
        return _op("put_root", root=root)

    @staticmethod
    def remove_root(path: PathLike) -> _Op:
        return _op("remove_root", path=_abs(path))

    @staticmethod
    def content_summary(path: PathLike) -> _Op:
        return _op("content_summary", path=_abs(path))

    @staticmethod
    def export_qdirstat(path: PathLike, out: PathLike) -> _Op:
        return _op("export_qdirstat", path=_abs(path), out=_abs(out))


class Client:
    """Blocking client; one connection, requests answered in order.
    Not safe to share between threads without a lock."""

    def __init__(self, path: str | None = None, timeout: float | None = None):
        self.path = path or socket_path()
        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._sock.settimeout(timeout)
        try:
            self._sock.connect(self.path)
        except OSError as e:
            self._sock.close()
            raise StewardError(f"connecting to stewardd at {self.path}: {e}") from e
        self._reader = self._sock.makefile("rb")

    def close(self) -> None:
        self._reader.close()
        self._sock.close()

    def __enter__(self) -> Client:
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def call(self, request: dict) -> Any:
        """Send any request dict; returns the ``result`` or raises."""
        self._sock.sendall(json.dumps(request).encode() + b"\n")
        return _decode(self._reader.readline())

    def _do(self, op: _Op) -> Any:
        request, conv = op
        return conv(self.call(request))

    def status(self) -> dict:
        return self._do(_Ops.status())

    def reload(self) -> dict:
        """Re-read settings.toml: start new roots, prune removed ones."""
        return self._do(_Ops.reload())

    def scan(self, path: PathLike, trust_dir_mtime: bool = False) -> dict:
        """Rescan now and wait for it; returns the scan report."""
        return self._do(_Ops.scan(path, trust_dir_mtime))

    def invalidate(self, path: PathLike) -> Any:
        """Tell the daemon something under ``path`` changed; returns at once."""
        return self._do(_Ops.invalidate(path))

    def stat(self, path: PathLike) -> Entry:
        """The entry with its inherited tags."""
        return self._do(_Ops.stat(path))

    def children(self, path: PathLike) -> list[Entry]:
        """Direct children, largest on disk first."""
        return self._do(_Ops.children(path))

    def locate(self, pattern: str, limit: int = 1000) -> list[str]:
        """Paths whose name contains ``pattern``, or matches it as a glob."""
        return self._do(_Ops.locate(pattern, limit))

    def classify(self, path: PathLike) -> dict:
        return self._do(_Ops.classify(path))

    def content_id(self, path: PathLike) -> str | None:
        """``btv2:<hex>`` (the BEP 52 pieces root), hashing if needed;
        ``None`` for an empty file."""
        return self._do(_Ops.content_id(path))

    def hash_tree(self, path: PathLike) -> dict:
        """Compute missing or stale content ids under ``path``; can be slow."""
        return self._do(_Ops.hash_tree(path))

    def find_content(self, content_id: str) -> list[str]:
        """Indexed paths whose current content has this id."""
        return self._do(_Ops.find_content(content_id))

    def duplicates(self, path: PathLike, limit: int = 1000) -> list[dict]:
        """Groups of identical content under ``path``, most wasted first."""
        return self._do(_Ops.duplicates(path, limit))

    def export_qdirstat(self, path: PathLike, out: PathLike) -> dict:
        """Write a qdirstat cache file; ``out`` is written by the daemon."""
        return self._do(_Ops.export_qdirstat(path, out))

    def settings(self) -> dict:
        """Settings file, and each root's policy with its index state."""
        return self._do(_Ops.settings())

    def put_root(self, root: dict) -> dict:
        """Add or replace a root: ``{"path": ..., "exclude": [...],
        "contentid": [...], ...}``; omitted keys take their defaults. The
        daemon validates it, writes settings.toml and applies it."""
        return self._do(_Ops.put_root(root))

    def remove_root(self, path: PathLike) -> dict:
        return self._do(_Ops.remove_root(path))

    def content_summary(self, path: PathLike) -> dict:
        """Content-id coverage and duplication under ``path``."""
        return self._do(_Ops.content_summary(path))


class AsyncClient:
    """asyncio client; one connection, concurrent calls are serialized."""

    def __init__(self, path: str | None = None):
        self.path = path or socket_path()
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None
        self._lock = asyncio.Lock()

    async def connect(self) -> AsyncClient:
        try:
            self._reader, self._writer = await asyncio.open_unix_connection(
                self.path, limit=_LINE_LIMIT
            )
        except OSError as e:
            raise StewardError(f"connecting to stewardd at {self.path}: {e}") from e
        return self

    async def close(self) -> None:
        if self._writer is not None:
            self._writer.close()
            await self._writer.wait_closed()
            self._writer = None

    async def __aenter__(self) -> AsyncClient:
        return await self.connect()

    async def __aexit__(self, *exc: Any) -> None:
        await self.close()

    async def call(self, request: dict) -> Any:
        """Send any request dict; returns the ``result`` or raises."""
        if self._writer is None or self._reader is None:
            await self.connect()
        assert self._writer is not None and self._reader is not None
        async with self._lock:
            self._writer.write(json.dumps(request).encode() + b"\n")
            await self._writer.drain()
            return _decode(await self._reader.readline())

    async def _do(self, op: _Op) -> Any:
        request, conv = op
        return conv(await self.call(request))

    async def status(self) -> dict:
        return await self._do(_Ops.status())

    async def reload(self) -> dict:
        return await self._do(_Ops.reload())

    async def scan(self, path: PathLike, trust_dir_mtime: bool = False) -> dict:
        return await self._do(_Ops.scan(path, trust_dir_mtime))

    async def invalidate(self, path: PathLike) -> Any:
        return await self._do(_Ops.invalidate(path))

    async def stat(self, path: PathLike) -> Entry:
        return await self._do(_Ops.stat(path))

    async def children(self, path: PathLike) -> list[Entry]:
        return await self._do(_Ops.children(path))

    async def locate(self, pattern: str, limit: int = 1000) -> list[str]:
        return await self._do(_Ops.locate(pattern, limit))

    async def classify(self, path: PathLike) -> dict:
        return await self._do(_Ops.classify(path))

    async def content_id(self, path: PathLike) -> str | None:
        return await self._do(_Ops.content_id(path))

    async def hash_tree(self, path: PathLike) -> dict:
        return await self._do(_Ops.hash_tree(path))

    async def find_content(self, content_id: str) -> list[str]:
        return await self._do(_Ops.find_content(content_id))

    async def duplicates(self, path: PathLike, limit: int = 1000) -> list[dict]:
        return await self._do(_Ops.duplicates(path, limit))

    async def export_qdirstat(self, path: PathLike, out: PathLike) -> dict:
        return await self._do(_Ops.export_qdirstat(path, out))

    async def settings(self) -> dict:
        return await self._do(_Ops.settings())

    async def put_root(self, root: dict) -> dict:
        return await self._do(_Ops.put_root(root))

    async def remove_root(self, path: PathLike) -> dict:
        return await self._do(_Ops.remove_root(path))

    async def content_summary(self, path: PathLike) -> dict:
        return await self._do(_Ops.content_summary(path))


if __name__ == "__main__":
    import sys

    with Client() as c:
        target = sys.argv[1] if len(sys.argv) > 1 else "~"
        me = c.stat(target)
        print(
            f"{me.path}: {me.total_alloc:,} bytes on disk, "
            f"{me.total_dirs - 1:,} dirs, {me.total_files:,} files"
        )
        for e in c.children(target)[:10]:
            share = e.total_alloc / me.total_alloc if me.total_alloc else 0
            slash = "/" if e.is_dir else ""
            print(f"  {share:6.1%}  {e.total_alloc:>16,}  {e.name}{slash}")
