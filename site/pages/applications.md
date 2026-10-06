---
title: Building on steward
eyebrow: Build
lede: How an application should use steward. That means asking about paths, learning what a file's bytes are, finding content wherever it now lives, telling steward what you changed, and following changes as they happen. It also means keeping steward optional.
description: Patterns for applications using steward's content.socket, with Python examples.
---

## Principles

**Treat steward as enrichment, not a dependency.** Your application should
work, perhaps more slowly or with less to show, when steward isn't
installed, isn't running, or doesn't cover the path in question. Check for
the socket and fall back gracefully. Nothing in steward is something your
application can't, in principle, do itself; steward just did it already,
for everyone.

**Use `content.socket`.** It has everything an application needs. Lookups,
the content primitives and events are all there, and none of them can change
what is indexed or how. Leave `api.socket` to the user's own tools.

**Expect the picture to be recent, not instant.** steward has no inotify.
What you read reflects the last scan of that path. When it matters, ask
steward to look now (`inspect`), or recheck what it tells you (`resolve`
with `recheck`).

**You report, steward decides.** Applications tell steward that paths may
have changed, or that a file may not hold what steward thinks. steward
looks for itself and records what it finds. No call lets an application
assert facts into the catalog, and none writes to a file.

## Connecting

The socket is `$XDG_RUNTIME_DIR/steward/content.socket`. The protocol is
JSON-RPC 2.0, one JSON object per line. Any language can speak it: see the
[API reference](api.html).

For Python, `python/steward_client.py` in the repository is a single file
with no dependencies beyond the standard library: copy it into your project.
It has a blocking `Client` and an asyncio `AsyncClient` with the same
methods, typed results and errors that carry a stable `type`.

```python
from steward_client import Client, StewardError, ConnectionLost

def steward():
    """A steward connection, or None when steward isn't available."""
    try:
        return Client(timeout=30)          # content.socket by default
    except ConnectionLost:
        return None
```

## Paths are bytes

File names on Linux are bytes and need not be UTF-8. steward never mangles
them: such bytes travel as `\udcXX` escapes
([details](api.html#file-names-that-arent-utf-8)), which Python decodes
natively. Treat every path you get as an opaque value to hand to the
filesystem (or back to steward), and only make it printable at the moment
you show it:

```python
p = c.locate("Deluxe")[0]               # str, possibly with lone surrogates
open(p, "rb")                           # works
raw = os.fsencode(p)                    # the exact bytes on disk
print(p.encode("utf-8", "backslashreplace").decode())   # safe to print
```

## Ask about paths

```python
with Client() as c:
    e = c.stat("~/Pictures")
    print(e.total_alloc, e.total_files, e.tags)

    for child in c.children("~/Pictures"):    # largest on disk first
        print(child.name, child.total_alloc, child.category, child.content_id)

    raws = c.locate("*.CR3", limit=5000)      # glob on the file name
```

`stat` includes tags inherited from parent directories, so a file inside a
git checkout's build output carries `classify:repo` and
`classify:build-output`. `children` reports each child's own tags.

## Learn what a file's bytes are

When you need a file's content id, ask steward to **inspect** it:

```python
[f] = c.inspect(["~/Downloads/talk.mp4"])
if f.ok:
    print(f.id)        # btv2:…, or None for an empty file
else:
    print(f.error["type"], f.error["message"])
```

`inspect` updates the index for that path first (it may be new, renamed or
rewritten), then returns the stored id if the file's size and modification
time are unchanged since it was hashed, or hashes it now. It takes many
paths at once and answers each separately, in order. Errors are per path
(`not_under_root`, `not_found`, `changing`, `unreadable`) and never fail the
whole call.

Hashing reads the whole file, at disk speed. Ask for the files you actually
need, and batch them.

## Find content wherever it is now

Your application remembers a content id, and later wants the bytes. Ask
steward to **resolve** it:

```python
[r] = c.resolve([(content_id, expected_size)], recheck=True)

if r.state == "present":
    path = r.online[0].path               # any online copy: they are identical
elif r.state == "offline":
    where = r.observations[0].offline_at  # e.g. /mnt/archive: ask the user to plug it in
elif r.state in ("absent", "unknown"):
    ...                                    # no copy steward knows of
elif r.state == "mismatch":
    ...                                    # your size and steward's disagree: not the content you meant
```

Passing the size you expect is a cheap guard. `recheck=True` makes steward
`lstat` each copy before answering, and drop any that changed or vanished.
Use it when you are about to open the file. Copies sharing an `inode` are
hard links. Several ids can be resolved in one call, and the results come
back in order.

## Tell steward what you changed

When your application writes, moves or deletes files under steward's roots,
say so. There are two ways, depending on whether you need an answer:

```python
# You need the content ids of what you just wrote:
results = c.inspect(written_paths)

# You don't need anything back; steward rescans soon (about 2 s later):
c.invalidate(directory)
```

Either way, other applications see the change sooner than the next scheduled
scan, and receive the events.

## When bytes don't match

Your application may find that a file steward listed as content X doesn't
hold X: a checksum failed, or a decoder choked. Ask steward to **verify** it:

```python
v = c.verify(content_id, path, reason="checksum mismatch at offset 4 MiB")
if v.state == "unchanged":
    ...   # the file does hold content_id; the problem is elsewhere
elif v.state == "changed":
    ...   # it holds v.current now; steward has stopped listing it for content_id
elif v.state in ("gone", "unreadable", "not_file"):
    ...
```

steward rereads the whole file regardless of its stat (verification exists
for changes that kept size and time), records what it holds, and from then
on reports it accordingly. `reason` is only logged. While a verification
runs, that path is left out of `resolve` results. Afterwards, `resolve`
again to find another copy.

## Follow changes

Rather than polling, subscribe to events for the content you care about.
`AsyncClient.events` gives a stream on its own connection that reconnects by
itself, resumes where it left off, and tells you when it couldn't:

```python
import asyncio
from steward_client import AsyncClient, Event, Gap

async def follow(tracked: dict[str, str]):          # content id -> path
    async with AsyncClient() as c:
        async with c.events(ids=tracked) as events:
            async for e in events:
                if isinstance(e, Gap):
                    # Missed events (daemon restart, or we fell behind): re-resolve everything.
                    for r in await c.resolve(list(tracked), recheck=True):
                        tracked[r.id] = r.online[0].path if r.online else None
                elif e.name == "content.moved":
                    tracked[e.data["id"]] = e.data["to"]
                elif e.name == "content.lost":
                    tracked[e.data["id"]] = None     # and resolve to find another copy
                elif e.name == "storage.offline":
                    ...                              # paths under e.data["path"] are unreachable for now
```

The `ids` filter applies to `content.*` events. `storage.*` events always
arrive, and carry only a directory: compare it with the paths you hold.
`await events.watch(new_ids)` changes the filter without losing anything.
Ids in results and events are always in the form `btv2:` plus 64 lowercase
hex digits, so use that form as your keys.

Events arrive when steward notices a change, at the next scan, `inspect`,
`invalidate` or `verify`. They aren't instant. They are numbered, and a
`Gap` is the only way to miss one, so your view never silently drifts.

## Piece layers

For each content id of a file over 1 MiB, steward keeps the hash of every
1 MiB section. From it, steward derives the content's BEP 52 Merkle layer
for any power-of-two piece size of at least 1 MiB, without reading the
file:

```python
layer = c.piece_layer(content_id, piece_size=4 << 20)   # bytes: 32 per piece
```

That is exactly a v2 torrent's `piece layers` entry for the file. It is
useful wherever partial verification of large files matters. The result is
empty for files no bigger than one piece.

## Errors

Every error has a stable `type` for programs and a message for people:

| type | means | do |
|---|---|---|
| `not_indexed` | the path isn't in the index (yet) | `inspect` it, or treat as unknown |
| `not_under_root` | the path is outside every configured root | steward can't help with this path |
| `invalid_params` | a malformed id, a relative path, a bad piece size | fix the call |
| `unknown_content` | no such content id | treat as unknown |
| `no_layer` | the content predates verification layers | `inspect` a copy |
| `changing` | the file kept changing while being read | retry later |
| `forbidden` | an administration method on `content.socket` | use the user's tools for that |
| `transport` | the connection failed (`ConnectionLost` in Python) | reconnect; every method is safe to repeat |

```python
try:
    e = c.stat(path)
except StewardError as err:
    if err.type == "not_indexed":
        [f] = c.inspect([path])
```

## Concurrency and connections

Requests on one connection are handled concurrently and matched by `id`. A
long `inspect` does not hold up a quick `stat` sent after it. `AsyncClient`
does this for you. A subscribed connection carries events as well as
responses: `AsyncClient.events` opens its own.

If the daemon restarts, calls in flight fail with `ConnectionLost` and the
next call reconnects. Every steward method is safe to repeat.

## Other languages

Any language with Unix sockets and JSON will do:

```sh
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"stat","params":{"path":"/etc"}}' |
  socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/steward/content.socket"
```

In Rust, the `steward-proto` crate has the request types and a small
blocking client (`Client::connect_to(&steward_proto::content_socket_path())`).

## Checklist

- Works without steward; detects the socket and degrades gracefully.
- Uses `content.socket`.
- Passes absolute paths (the clients resolve `~` and relative paths for you).
- Passes expected sizes to `resolve`, and `recheck=True` before opening.
- Calls `inspect` or `invalidate` after writing under steward's roots.
- Calls `verify` when bytes disagree with an id, then resolves again.
- Treats a `Gap`, or a new `epoch`, as "re-resolve everything".
