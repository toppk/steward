"""Python client for stewardd, the steward file index service.

stewardd speaks JSON-RPC 2.0, one message per line, on two Unix sockets in
``$XDG_RUNTIME_DIR/steward/``:

- ``content.socket``: what applications use. Catalog reads and the content
  primitives (resolve, inspect, verify, events). The default here.
- ``api.socket``: administration (roots, reload, scans, exports). Connect
  with ``admin=True``.

``crates/steward-proto`` holds the authoritative definitions.

    from steward_client import Client

    with Client() as c:
        for r in c.resolve(["btv2:1d8e…"], recheck=True):
            print(r.state, [o.path for o in r.observations if o.online])
        print(c.inspect(["~/Downloads/film.mkv"])[0].id)

    import asyncio
    from steward_client import AsyncClient, Gap

    async def main():
        async with AsyncClient() as c:
            print(await c.locate("*.torrent"))
            async with c.events(ids=["btv2:1d8e…"]) as events:
                async for e in events:
                    if isinstance(e, Gap):
                        ...  # missed events: resolve again
                    else:
                        print(e.name, e.data)

    asyncio.run(main())

Standard library only; Python 3.9+.
"""

from __future__ import annotations

import asyncio
import contextlib
import itertools
import json
import os
import socket
import tempfile
from collections.abc import AsyncIterator, Iterable, Iterator
from dataclasses import dataclass, field, fields
from typing import Any, Callable, Optional, Union

__all__ = [
    "AsyncClient",
    "Client",
    "ConnectionLost",
    "Entry",
    "Event",
    "EventStream",
    "Gap",
    "Inspected",
    "Observation",
    "Resolution",
    "StewardError",
    "Verdict",
    "api_socket_path",
    "content_socket_path",
]

PathLike = Union[str, "os.PathLike[str]"]
ContentRef = Union[str, "tuple[str, Optional[int]]"]

# Directory listings of large trees are multi-megabyte single lines.
_LINE_LIMIT = 1 << 30


class StewardError(Exception):
    """An error the daemon returned. ``type`` is stable for programs
    (``not_indexed``, ``not_under_root``, ``forbidden``, ``invalid_params``,
    ``unknown_content``, ``changing``, ``failed``…); the message is for
    people."""

    def __init__(self, message: str, type: str = "failed", code: int | None = None):
        super().__init__(message)
        self.type = type
        self.code = code


class ConnectionLost(StewardError):
    """The connection closed or could not be made. A call in flight may or
    may not have run; every steward method is safe to repeat."""

    def __init__(self, message: str):
        super().__init__(message, "transport")


class _NotSent(ConnectionLost):
    """The request never reached the daemon, so retrying it is harmless."""


def _from_dict(cls: Any, d: dict, **conv: Callable[[Any], Any]) -> Any:
    # Ignore fields a newer daemon adds.
    return cls(
        **{
            f.name: conv.get(f.name, lambda x: x)(d[f.name])
            for f in fields(cls)
            if f.name in d
        }
    )


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
    # Every entry beneath, itself included: roughly the inodes it uses.
    total_items: int = 0
    tags: list[str] = field(default_factory=list)
    category: str | None = None
    content_id: str | None = None

    @classmethod
    def from_dict(cls, d: dict) -> Entry:
        return _from_dict(cls, d)

    @property
    def is_dir(self) -> bool:
        return self.kind == "dir"

    @property
    def name(self) -> str:
        return os.path.basename(self.path.rstrip("/")) or self.path


@dataclass(frozen=True)
class Observation:
    """A path where steward last saw the content. Hard links share
    ``inode``. An offline copy sits on a volume that is not mounted;
    ``offline_at`` is the directory steward found unmounted."""

    path: str
    inode: str
    online: bool
    offline_at: str | None = None
    mtime_ns: int = 0


@dataclass(frozen=True)
class Resolution:
    """What steward knows of one content id.

    ``state``: ``present`` (a copy is reachable), ``offline`` (copies exist,
    none reachable), ``absent`` (seen before, no copy now), ``unknown``
    (never seen) or ``mismatch`` (the size given differs from the content's).
    ``observations`` lists reachable copies first. ``layer`` says whether
    ``piece_layer`` can answer for it without reading a file."""

    id: str
    size: int | None
    state: str
    layer: bool = False
    observations: tuple[Observation, ...] = ()

    @classmethod
    def from_dict(cls, d: dict) -> Resolution:
        return _from_dict(
            cls,
            d,
            observations=lambda os_: tuple(
                _from_dict(Observation, o) for o in os_
            ),
        )

    @property
    def online(self) -> tuple[Observation, ...]:
        return tuple(o for o in self.observations if o.online)


@dataclass(frozen=True)
class Inspected:
    """One path after ``inspect``: its kind and, for a non-empty regular
    file, its content id. ``error`` is ``{"type", "message"}`` or None."""

    path: str
    kind: str | None
    id: str | None
    size: int = 0
    error: dict | None = None

    @property
    def ok(self) -> bool:
        return self.error is None


@dataclass(frozen=True)
class Verdict:
    """What ``verify`` found at ``path``: ``unchanged`` (it holds ``id``),
    ``changed`` (it holds ``current``, or nothing hashable), ``gone``,
    ``unreadable`` or ``not_file``."""

    id: str
    path: str
    state: str
    current: str | None = None


@dataclass(frozen=True)
class Event:
    """A content or storage event: ``content.observed`` {id, path},
    ``content.moved`` {id, from, to}, ``content.lost`` {id, path, reason},
    ``storage.offline`` / ``storage.online`` / ``storage.unindexed`` {path}.
    ``seq`` orders events within one daemon run (``epoch``)."""

    seq: int
    time: float
    name: str
    data: dict
    epoch: str = ""


@dataclass(frozen=True)
class Gap:
    """Events were missed: a slow reader fell behind (``lagged``), the
    daemon restarted (``restarted``), or the backlog no longer reached back
    (``backlog``). Re-resolve whatever you track."""

    reason: str
    after: int | None = None


def _runtime_dir() -> str:
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if not runtime:
        runtime = os.path.join(
            tempfile.gettempdir(), f"steward-{os.stat('/proc/self').st_uid}"
        )
    return os.path.join(runtime, "steward")


def content_socket_path() -> str:
    """The application-facing socket, resolved as the daemon does."""
    return os.path.join(_runtime_dir(), "content.socket")


def api_socket_path() -> str:
    """The administration socket."""
    return os.path.join(_runtime_dir(), "api.socket")


def _abs(path: PathLike) -> str:
    return os.path.abspath(os.path.expanduser(os.fspath(path)))


def _identity(x: Any) -> Any:
    return x


def _entries(x: Any) -> list[Entry]:
    return [Entry.from_dict(d) for d in x]


def _content_ref(c: ContentRef) -> dict:
    if isinstance(c, str):
        return {"id": c}
    cid, size = c
    return {"id": cid} if size is None else {"id": cid, "size": size}


# Each operation is (method, params, how to convert its result), shared by
# both clients so the sync and async APIs cannot drift apart.
_Op = tuple[str, dict, Callable[[Any], Any]]


def _op(method: str, conv: Callable[[Any], Any] = _identity, **params: Any) -> _Op:
    return method, params, conv


def _error(msg: dict) -> StewardError:
    e = msg.get("error") or {}
    data = e.get("data") or {}
    return StewardError(
        e.get("message", f"unexpected response: {msg!r}"),
        data.get("type", "failed"),
        e.get("code"),
    )


def _parse(line: bytes) -> dict:
    try:
        msg = json.loads(line)
    except ValueError as e:
        raise ConnectionLost(f"malformed message from stewardd: {e}") from None
    if not isinstance(msg, dict):
        raise ConnectionLost(f"malformed message from stewardd: {msg!r}")
    return msg


def _notification(msg: dict, epoch: str) -> Event | Gap | None:
    if msg.get("method") == "event":
        p = msg.get("params") or {}
        return Event(p["seq"], p["time"], p["name"], p.get("data") or {}, epoch)
    if msg.get("method") == "gap":
        return Gap("lagged", (msg.get("params") or {}).get("after"))
    return None


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
    def resolve(contents: Iterable[ContentRef], recheck: bool) -> _Op:
        return _op(
            "resolve",
            lambda r: [Resolution.from_dict(d) for d in r],
            contents=[_content_ref(c) for c in contents],
            recheck=recheck,
        )

    @staticmethod
    def inspect(paths: Iterable[PathLike]) -> _Op:
        return _op(
            "inspect",
            lambda r: [_from_dict(Inspected, d) for d in r],
            paths=[_abs(p) for p in paths],
        )

    @staticmethod
    def verify(content_id: str, path: PathLike, reason: str) -> _Op:
        return _op(
            "verify",
            lambda r: _from_dict(Verdict, r),
            id=content_id,
            path=_abs(path),
            reason=reason,
        )

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
    def piece_layer(content_id: str, piece_size: int) -> _Op:
        return _op(
            "piece_layer",
            lambda r: bytes.fromhex(r["layer"]),
            id=content_id,
            piece_size=piece_size,
        )

    @staticmethod
    def export_qdirstat(path: PathLike, out: PathLike) -> _Op:
        return _op("export_qdirstat", path=_abs(path), out=_abs(out))


def _subscribe_params(ids: Iterable[str] | None, since: int | None) -> dict:
    params: dict = {}
    if ids is not None:
        params["ids"] = list(ids)
    if since is not None:
        params["since"] = since
    return params


class Client:
    """Blocking client; one connection, one call at a time. Not safe to
    share between threads without a lock."""

    def __init__(
        self,
        path: str | None = None,
        timeout: float | None = None,
        admin: bool = False,
    ):
        self.path = path or (api_socket_path() if admin else content_socket_path())
        self._ids = itertools.count(1)
        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._sock.settimeout(timeout)
        try:
            self._sock.connect(self.path)
        except OSError as e:
            self._sock.close()
            raise ConnectionLost(f"connecting to the steward daemon at {self.path}: {e}") from e
        self._reader = self._sock.makefile("rb")

    def close(self) -> None:
        self._reader.close()
        self._sock.close()

    def __enter__(self) -> Client:
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def _read(self) -> dict:
        line = self._reader.readline()
        if not line:
            raise ConnectionLost("stewardd closed the connection")
        return _parse(line)

    def call(self, method: str, params: dict | None = None) -> Any:
        """Call any method; returns its ``result`` or raises StewardError."""
        rid = next(self._ids)
        msg = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}}
        self._sock.sendall(json.dumps(msg).encode() + b"\n")
        while True:
            resp = self._read()
            if resp.get("id") != rid:
                continue  # a notification, or an answer to nothing of ours
            if "error" in resp:
                raise _error(resp)
            return resp.get("result")

    def _do(self, op: _Op) -> Any:
        method, params, conv = op
        return conv(self.call(method, params))

    def events(
        self, ids: Iterable[str] | None = None, since: int | None = None
    ) -> Iterator[Event | Gap]:
        """Turn this connection into an event stream: yields each Event, and
        a Gap where events were missed. ``ids`` limits content events to
        those contents; storage events always arrive. No reconnection; see
        ``AsyncClient.events`` for that."""
        start = self.call("subscribe", _subscribe_params(ids, since))
        epoch = start["epoch"]
        if not start["complete"]:
            yield Gap("backlog", since)
        while True:
            n = _notification(self._read(), epoch)
            if n is not None:
                yield n

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

    def resolve(
        self, contents: Iterable[ContentRef], recheck: bool = False
    ) -> list[Resolution]:
        """Where each content is now, in request order. Items are ids, or
        ``(id, expected_size)``. ``recheck`` re-stats every copy first and
        drops those that changed or vanished."""
        return self._do(_Ops.resolve(contents, recheck))

    def inspect(self, paths: Iterable[PathLike]) -> list[Inspected]:
        """These paths may have changed: update steward's catalog for them
        now and establish the content ids of regular files (hashing them if
        needed). Directories are rescanned whole. In request order; per-path
        failures are in ``error``."""
        return self._do(_Ops.inspect(paths))

    def verify(self, content_id: str, path: PathLike, reason: str = "") -> Verdict:
        """There is reason to think ``path`` no longer holds ``content_id``:
        steward rereads it in full and records what it holds. ``reason`` is
        only logged."""
        return self._do(_Ops.verify(content_id, path, reason))

    def duplicates(self, path: PathLike, limit: int = 1000) -> list[dict]:
        """Groups of identical content under ``path``, most wasted first."""
        return self._do(_Ops.duplicates(path, limit))

    def export_qdirstat(self, path: PathLike, out: PathLike) -> dict:
        """Write a qdirstat cache file; ``out`` is written by the daemon."""
        return self._do(_Ops.export_qdirstat(path, out))

    def piece_layer(self, content_id: str, piece_size: int = 1 << 20) -> bytes:
        """The BEP-52 ``piece layers`` value for ``content_id`` at
        ``piece_size`` (a power of two >= 1 MiB): concatenated 32-byte
        SHA-256 hashes, empty for a file no bigger than one piece. Derived
        from the stored 1 MiB layer; the file is not read."""
        return self._do(_Ops.piece_layer(content_id, piece_size))

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


class _Connection:
    """One asyncio connection: calls run concurrently and are matched to
    responses by id; notifications go to ``on_notification``."""

    def __init__(self, path: str, on_notification: Callable[[dict], None] | None):
        self.path = path
        self._on_notification = on_notification
        self._ids = itertools.count(1)
        self._pending: dict[int, asyncio.Future] = {}
        self._writer: asyncio.StreamWriter | None = None
        self._task: asyncio.Task | None = None
        self.closed = asyncio.Event()

    async def open(self) -> None:
        try:
            reader, self._writer = await asyncio.open_unix_connection(
                self.path, limit=_LINE_LIMIT
            )
        except OSError as e:
            raise ConnectionLost(f"connecting to the steward daemon at {self.path}: {e}") from e
        self._task = asyncio.get_running_loop().create_task(self._read_loop(reader))

    async def _read_loop(self, reader: asyncio.StreamReader) -> None:
        why = "stewardd closed the connection"
        try:
            while True:
                line = await reader.readline()
                if not line:
                    break
                msg = _parse(line)
                rid = msg.get("id")
                if "method" in msg and rid is None:
                    if self._on_notification is not None:
                        self._on_notification(msg)
                elif rid is not None:
                    fut = self._pending.pop(rid, None)
                    if fut is not None and not fut.done():
                        fut.set_result(msg)
        except (OSError, ValueError, StewardError) as e:
            why = f"connection to stewardd failed: {e}"
        finally:
            self._fail(why)

    def _fail(self, why: str) -> None:
        for fut in self._pending.values():
            if not fut.done():
                fut.set_exception(ConnectionLost(why))
        self._pending.clear()
        self.closed.set()
        if self._writer is not None:
            self._writer.close()

    async def call(self, method: str, params: dict) -> Any:
        if self.closed.is_set() or self._writer is None:
            raise _NotSent("not connected to stewardd")
        rid = next(self._ids)
        fut = asyncio.get_running_loop().create_future()
        self._pending[rid] = fut
        msg = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params}
        try:
            self._writer.write(json.dumps(msg).encode() + b"\n")
            await self._writer.drain()
        except OSError as e:
            self._pending.pop(rid, None)
            self._fail(f"writing to stewardd: {e}")
            raise _NotSent(f"writing to stewardd: {e}") from e
        resp = await fut
        if "error" in resp:
            raise _error(resp)
        return resp.get("result")

    async def close(self) -> None:
        if self._writer is not None:
            self._writer.close()
            with contextlib.suppress(OSError):
                await self._writer.wait_closed()
        if self._task is not None:
            self._task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self._task
        self._fail("closed")


class EventStream:
    """Events on a dedicated connection that survives daemon restarts.

    Iterate for Event and Gap items. After a reconnection it resumes from
    the last event it received; when that is impossible (the daemon
    restarted, or the backlog moved on) it yields a Gap first. ``watch``
    replaces the content-id filter without losing events."""

    def __init__(
        self,
        path: str,
        ids: Iterable[str] | None,
        since: int | None,
        reconnect: bool,
        max_backoff: float,
    ):
        self.path = path
        self._ids = None if ids is None else list(ids)
        self._since = since
        self._reconnect = reconnect
        self._max_backoff = max_backoff
        self._queue: asyncio.Queue = asyncio.Queue()
        self._conn: _Connection | None = None
        self._epoch: str | None = None
        # Highest seq received on the wire: where to resume from.
        self.seq: int | None = since
        self._closed = False

    def _received(self, msg: dict) -> None:
        if msg.get("method") == "event":
            self.seq = (msg.get("params") or {}).get("seq", self.seq)
        self._queue.put_nowait(msg)

    async def _open(self) -> None:
        conn = _Connection(self.path, self._received)
        await conn.open()
        try:
            start = await conn.call("subscribe", _subscribe_params(self._ids, self.seq))
        except BaseException:
            await conn.close()
            raise
        restarted = self._epoch is not None and start["epoch"] != self._epoch
        if restarted:
            self._queue.put_nowait(Gap("restarted", self.seq))
            self.seq = start["seq"]
        elif not start["complete"]:
            self._queue.put_nowait(Gap("backlog", self.seq))
        if self.seq is None:
            self.seq = start["seq"]
        self._epoch = start["epoch"]
        self._conn = conn
        closed = conn.closed
        asyncio.get_running_loop().create_task(self._watch_close(conn, closed))

    async def _watch_close(self, conn: _Connection, closed: asyncio.Event) -> None:
        await closed.wait()
        if self._conn is conn:
            self._conn = None
            self._queue.put_nowait(None)

    async def start(self) -> EventStream:
        try:
            await self._open()
        except ConnectionLost:
            if not self._reconnect:
                raise
            # Iteration keeps trying until stewardd is up.
        return self

    async def watch(self, ids: Iterable[str] | None) -> None:
        """Replace the content-id filter (None: every content event)."""
        self._ids = None if ids is None else list(ids)
        if self._conn is not None:
            start = await self._conn.call(
                "subscribe", _subscribe_params(self._ids, self.seq)
            )
            if not start["complete"]:
                self._queue.put_nowait(Gap("backlog", self.seq))

    def __aiter__(self) -> AsyncIterator[Event | Gap]:
        return self

    async def __anext__(self) -> Event | Gap:
        backoff = 0.2
        while True:
            if self._closed:
                raise StopAsyncIteration
            if self._conn is None and self._queue.empty():
                try:
                    await self._open()
                    backoff = 0.2
                except ConnectionLost:
                    if not self._reconnect:
                        raise
                    await asyncio.sleep(backoff)
                    backoff = min(backoff * 2, self._max_backoff)
                    continue
            item = await self._queue.get()
            if item is None:
                if not self._reconnect:
                    raise ConnectionLost("stewardd closed the event connection")
                continue
            if isinstance(item, Gap):
                return item
            n = _notification(item, self._epoch or "")
            if n is not None:
                return n

    async def close(self) -> None:
        self._closed = True
        conn, self._conn = self._conn, None
        if conn is not None:
            await conn.close()

    async def __aenter__(self) -> EventStream:
        return self

    async def __aexit__(self, *exc: Any) -> None:
        await self.close()


class AsyncClient:
    """asyncio client. Calls on one client run concurrently. If the daemon
    goes away, calls in flight raise ConnectionLost and the next call
    reconnects."""

    def __init__(self, path: str | None = None, admin: bool = False):
        self.path = path or (api_socket_path() if admin else content_socket_path())
        self._conn: _Connection | None = None
        self._connecting = asyncio.Lock()

    async def connect(self) -> AsyncClient:
        async with self._connecting:
            if self._conn is None or self._conn.closed.is_set():
                conn = _Connection(self.path, None)
                await conn.open()
                self._conn = conn
        return self

    async def close(self) -> None:
        conn, self._conn = self._conn, None
        if conn is not None:
            await conn.close()

    async def __aenter__(self) -> AsyncClient:
        return await self.connect()

    async def __aexit__(self, *exc: Any) -> None:
        await self.close()

    async def call(self, method: str, params: dict | None = None) -> Any:
        """Call any method; returns its ``result`` or raises StewardError."""
        for attempt in (1, 2):
            if self._conn is None or self._conn.closed.is_set():
                await self.connect()
            assert self._conn is not None
            try:
                return await self._conn.call(method, params or {})
            except _NotSent:
                if attempt == 2:
                    raise
        raise AssertionError("unreachable")

    async def _do(self, op: _Op) -> Any:
        method, params, conv = op
        return conv(await self.call(method, params))

    def events(
        self,
        ids: Iterable[str] | None = None,
        since: int | None = None,
        reconnect: bool = True,
        max_backoff: float = 30.0,
    ) -> _EventsContext:
        """Content and storage events on a connection of their own:
        ``async with client.events(ids=[...]) as events: async for e in
        events``. ``ids`` limits content events to those contents; storage
        events always arrive."""
        return _EventsContext(
            EventStream(self.path, ids, since, reconnect, max_backoff)
        )

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

    async def resolve(
        self, contents: Iterable[ContentRef], recheck: bool = False
    ) -> list[Resolution]:
        return await self._do(_Ops.resolve(contents, recheck))

    async def inspect(self, paths: Iterable[PathLike]) -> list[Inspected]:
        return await self._do(_Ops.inspect(paths))

    async def verify(
        self, content_id: str, path: PathLike, reason: str = ""
    ) -> Verdict:
        return await self._do(_Ops.verify(content_id, path, reason))

    async def duplicates(self, path: PathLike, limit: int = 1000) -> list[dict]:
        return await self._do(_Ops.duplicates(path, limit))

    async def export_qdirstat(self, path: PathLike, out: PathLike) -> dict:
        return await self._do(_Ops.export_qdirstat(path, out))

    async def piece_layer(self, content_id: str, piece_size: int = 1 << 20) -> bytes:
        return await self._do(_Ops.piece_layer(content_id, piece_size))

    async def settings(self) -> dict:
        return await self._do(_Ops.settings())

    async def put_root(self, root: dict) -> dict:
        return await self._do(_Ops.put_root(root))

    async def remove_root(self, path: PathLike) -> dict:
        return await self._do(_Ops.remove_root(path))

    async def content_summary(self, path: PathLike) -> dict:
        return await self._do(_Ops.content_summary(path))


class _EventsContext:
    def __init__(self, stream: EventStream):
        self._stream = stream

    async def __aenter__(self) -> EventStream:
        return await self._stream.start()

    async def __aexit__(self, *exc: Any) -> None:
        await self._stream.close()


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
