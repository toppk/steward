---
title: API reference
eyebrow: Reference
lede: The protocol stewardd speaks, every method it answers, the shapes of its results, its events and its errors.
description: steward's JSON-RPC 2.0 API — transport, sockets, every method, result types, events and errors.
---

## Transport

stewardd listens on two Unix stream sockets in `$XDG_RUNTIME_DIR/steward/`
(a `0700` directory; without `XDG_RUNTIME_DIR`, a private directory under
`/tmp`):

| socket | for | offers |
|---|---|---|
| `content.socket` | applications | [catalog](#catalog), [content](#content), [events](#events), `invalidate`, `status` |
| `api.socket` | the user's own tools | everything on `content.socket`, plus [maintenance](#maintenance) and [settings](#settings) |

The protocol is **JSON-RPC 2.0**, one JSON object per line (UTF-8, `\n`
terminated), in both directions.

```
→ {"jsonrpc":"2.0","id":1,"method":"stat","params":{"path":"/etc"}}
← {"jsonrpc":"2.0","id":1,"result":{"path":"/etc","kind":"dir", …}}
```

- Every request with an `id` gets exactly one response with that `id`. A
  request without one is a notification and gets none.
- `params` is an object (named parameters). Methods without parameters
  accept it absent, `null` or `{}`.
- Requests on one connection are handled **concurrently**; responses may
  come back in a different order. Match them by `id`.
- A connection that has [subscribed](#subscribe) also receives `event` and
  `gap` notifications between responses.

## Conventions

Paths
:   Absolute, without `~`. Answers carry absolute paths. Non-UTF-8 names are
    converted lossily.

Content ids
:   `btv2:` followed by 64 lowercase hex digits, the file's BEP 52
    pieces root. Requests also accept bare hex and either case.

Sizes
:   Bytes. `size` is the apparent size; `alloc` is space on disk
    (blocks × 512).

Times
:   `Entry.mtime` is in seconds since the Unix epoch. Fields ending in `_ns`
    are nanoseconds. Event `time`, `started`, `finished` and the like are
    seconds as floating point.

## Errors

```json
{"jsonrpc":"2.0","id":7,"error":{"code":-32000,"message":"/srv is not indexed: no configured root covers it","data":{"type":"not_indexed"}}}
```

`data.type` is stable and meant for programs. `message` is for people and
may change.

| code | `data.type` | meaning |
|---|---|---|
| -32700 | `parse_error` | the line wasn't JSON (the response has `id: null`) |
| -32600 | `invalid_request` | not a JSON-RPC 2.0 request |
| -32601 | `method_not_found` | no such method |
| -32602 | `invalid_params` | missing or ill-typed parameters, a malformed content id, a relative path, an unusable piece size |
| -32000 | `forbidden` | an administration method sent to `content.socket` |
| -32000 | `not_indexed` | the path is not in the index, or no root covers it |
| -32000 | `not_under_root` | the path is outside every configured root (for methods that scan or read it) |
| -32000 | `unknown_content` | no such content id |
| -32000 | `no_layer` | the content has no stored verification layer |
| -32000 | `changing` | the file kept changing while being read |
| -32000 | `failed` | anything else; see `message` |

## Types

### Entry

One indexed path.

| field | type | |
|---|---|---|
| `path` | string | absolute |
| `kind` | string | `file`, `dir`, `symlink` or `other` |
| `mode` | integer | permission bits (`0o7777` mask) |
| `uid`, `gid` | integer | owner |
| `size` | integer | apparent size |
| `alloc` | integer | space on disk |
| `mtime` | integer | modification time, seconds |
| `total_size`, `total_alloc` | integer | subtree totals for directories; the entry's own figures otherwise |
| `total_files`, `total_dirs` | integer | files and directories beneath (a directory counts itself) |
| `tags` | string[] | e.g. `classify:repo`; omitted when empty |
| `category` | string | files only: `image`, `video`, `audio`, `document`, `source`, `archive`, `object`, `disk-image`, `torrent`; omitted when none |
| `content_id` | string | when a current content id is stored; omitted otherwise |

### Resolution

What steward knows of one content id (result of [`resolve`](#resolve)).

| field | type | |
|---|---|---|
| `id` | string | normalised content id |
| `size` | integer \| null | the content's size, when known |
| `state` | string | `present`, `offline`, `absent`, `unknown` or `mismatch` |
| `layer` | boolean | a verification layer is stored (always true up to 1 MiB) |
| `observations` | Observation[] | current copies, reachable ones first |

**Observation**: `path` (string), `inode` (string, `"<fsid hex>:<inode>"`;
equal for hard links), `online` (boolean), `offline_at` (string \| null: the
unmounted directory holding it), `mtime_ns` (integer).

### Inspected

One path after [`inspect`](#inspect): `path`, `kind` (`file`, `dir`,
`symlink`, `other`, or null on error), `id` (string \| null: null for
directories, empty files and non-files), `size`, `error` (null, or
`{type, message}` with type `not_under_root`, `not_found`, `changing`,
`unreadable` or `invalid_params`).

### Verdict

What [`verify`](#verify) found: `id` (the id you claimed), `path`, `state`
(`unchanged`, `changed`, `gone`, `unreadable`, `not_file`), `current` (the
id the file holds now, or null).

### ScanReport

`root`, `dirs_read`, `dirs_trusted`, `entries_seen`, `inserted`, `updated`,
`deleted`, `errors` (unreadable entries), `millis`, `offline` (directories
found on unmounted volumes), `load_ms`, `write_ms`, `totals_ms`.

## Catalog

Read-only lookups in the index. Both sockets.

### status

The daemon's state. No parameters.

| field | |
|---|---|
| `db` | index file |
| `configured` | the configured roots and their policies |
| `indexed` | an Entry for each indexed root |
| `scanning` | whether a scan is running |
| `hashing` | the hashing job, or null: `path`, `files_total`, `files_done`, `bytes_total`, `bytes_done` (includes bytes read of files in flight), `started` |
| `activity` | `scan` (`path`, `kind`, `secs`, or null), `reading` (files being hashed: `path`, `size`, `read`, `secs`), `hash_queue`, `connections`, `subscribers`, `event_seq` |
| `daemon` | `version`, `pid`, `started`, `uptime_secs`, `epoch`, `hash_threads`, `db`, `db_bytes`, `api_socket`, `content_socket`, `managed` |
| `recent_scans` | the last 50 scans, newest first: ScanReport plus `kind` and `finished`, or `root`, `error`, `kind`, `finished` |
| `schedule` | `path` and `next` (seconds since epoch) for each root |
| `problems` | the last 200 warnings and errors, newest first: `time`, `level`, `context`, `message` |

### stat

`{path}` → Entry, with tags inherited from its ancestors.

### children

`{path}` → Entry[], the directory's children, largest `total_alloc`
first. Each child carries only its own tags.

### locate

`{pattern, limit = 1000}` → string[] of paths whose final name component
matches: a case-sensitive glob if `pattern` contains `*`, `?` or `[`,
otherwise a case-insensitive substring.

### find_content

`{id}` → string[]: the indexed paths currently holding this content.
[`resolve`](#resolve) says more (reachability, state, sizes).

### duplicates

`{path, limit = 1000}` → groups of identical hashed files with at least two
paths under `path`, most wasted space first:

```json
[{"id":"btv2:…","size":1048576000,"wasted":2097152000,"paths":["/a/x.mkv","/b/x.mkv","/c/x (1).mkv"]}]
```

### content_summary

`{path}` → content-id coverage under `path`: `files`, `bytes`,
`hashed_files`, `hashed_bytes`, `unhashed_files`, `unhashed_bytes`,
`distinct_ids`, `duplicate_groups`, `duplicate_files`, `wasted_bytes`,
`truncated` (true if it stopped at 2,000,000 files).

## Content

Both sockets.

### content_id

`{path}` → content id or null (empty file). Hashes the file if it has no
current id. The path must be a regular file. Prefer [`inspect`](#inspect),
which also updates the index for the path and takes many at once.

### resolve

`{contents: [{id, size?}], recheck = false}` → Resolution[], in request
order.

- `size`: the size you expect. A different known size gives
  `state: "mismatch"` and no observations.
- `recheck`: re-stat each copy before answering. Copies that changed or
  vanished are dropped, and their directories are queued for a rescan.
  A copy that can't be reached because its volume isn't mounted is
  reported offline, with `offline_at`.
- Paths being [verified](#verify) are left out.

```
→ {"jsonrpc":"2.0","id":2,"method":"resolve","params":{"contents":[{"id":"btv2:1d8e…","size":3145728}],"recheck":true}}
← {"jsonrpc":"2.0","id":2,"result":[{"id":"btv2:1d8e…","size":3145728,"state":"present","layer":true,
    "observations":[{"path":"/home/me/a.bin","inode":"9f3c…:1442","online":true,"offline_at":null,"mtime_ns":1790800000000000000}]}]}
```

### inspect

`{paths: [string]}` → Inspected[], in request order. These paths may have
changed: bring the index up to date for them now, and give each regular
file's content id.

- A file's parent directory is rescanned (its own listing read even if its
  times are unchanged); then the stored id is used if the file's size and
  modification time are unchanged, or the file is hashed now.
- A directory is rescanned in full. Files under it are hashed only if a
  root's `contentid` policy covers them.
- Paths must be absolute and under a configured root. Problems are reported
  per path; the call itself succeeds.

### verify

`{id, path, reason = ""}` → Verdict. There is reason to believe `path` no
longer holds `id`. steward withholds the path from `resolve`, rescans its
directory, rereads the file in full and records what it holds:

- `unchanged`: it holds `id`.
- `changed`: it holds `current` (null for an empty file). Paths that were
  listed for `id` get `content.lost` with reason `changed`.
- `gone`, `not_file`: nothing hashable is there.
- `unreadable`: reading failed. The inode's stored id is dropped, and its
  paths get `content.lost` with reason `unreadable`.

`reason` is logged, never interpreted. Errors: `invalid_params`,
`not_under_root`, `changing` (still changing after three reads).

### piece_layer

`{id, piece_size}` → `{id, size, piece_size, layer}`. The content's BEP 52
Merkle layer at `piece_size` (a power of two, at least 1 MiB), derived from
the stored 1 MiB layer without reading the file. `layer` is the
concatenated 32-byte hashes in hex, one per piece, and empty for content no
bigger than one piece. Errors: `unknown_content`, `no_layer`,
`invalid_params`.

### invalidate

`{path}` → `"queued"`. Something under `path` changed. stewardd gathers
invalidations for two seconds, then rescans the nearest existing directory
of each (in full).

## Events

### subscribe

`{since?, ids?}` → `{epoch, seq, complete}`. From now on this connection
also receives events.

- `since`: first replay the backlog's events after this `seq`.
- `ids`: deliver `content.*` events only for these content ids.
  `storage.*` events always arrive.
- `epoch` changes each time the daemon starts; `seq` is the last event's
  number in it.
- `complete` is false when events after `since` were missed: the backlog
  (the last 4,096 events) no longer reaches back that far, or `since` is
  ahead of `seq`. The daemon can't tell a `since` from an earlier run, so
  compare `epoch` with the one you saw before: a different epoch means you
  missed everything in between.

Subscribing again replaces the filter. The response comes before any
replayed event.

### unsubscribe

`{}` → `true`. Stop delivering events on this connection.

### Notifications

```json
{"jsonrpc":"2.0","method":"event","params":{"seq":42,"time":1790812345.12,"name":"content.moved","data":{"id":"btv2:…","from":"/a/x.mkv","to":"/b/x.mkv"}}}
{"jsonrpc":"2.0","method":"gap","params":{"after":41}}
```

| `name` | `data` | meaning |
|---|---|---|
| `content.observed` | `{id, path}` | the content was seen at this path (new, renamed onto, or hashed) |
| `content.moved` | `{id, from, to}` | the same inode moved from one path to another |
| `content.lost` | `{id, path, reason}` | the path no longer holds this content: `deleted`, `changed` or `unreadable` |
| `storage.offline` | `{path}` | this known directory's volume is not mounted |
| `storage.online` | `{path}` | it is mounted again |
| `storage.unindexed` | `{path}` | this root was removed from the configuration |

A `gap` notification means this subscriber fell behind and events after
`after` were dropped for it. After a gap, an incomplete subscribe, or a new
epoch, re-resolve whatever you track.

## Maintenance

`api.socket` only.

### scan

`{path, trust_dir_mtime = false}` → ScanReport. Rescan `path` (a root or
anything under one) now and wait. `trust_dir_mtime` skips directories whose
times show nothing changed. Error: `not_under_root`.

### classify

`{path}` → `{scanned, tagged}`. Re-run classification, from the enclosing
repository if there is one.

### hash_tree

`{path}` → `{stale, hashed}`. Hash every file under `path` without a current
content id, and wait. For a media folder, this can take hours.

### export_qdirstat

`{path, out}` → `{entries, out}`. Write `path`'s subtree as a gzipped
qdirstat 2.0 cache file at `out`. Refuses to overwrite an existing file.

## Settings

`api.socket` only. Changes are validated, written into `settings.toml`
(comments and formatting kept), and applied at once.

### settings

No parameters → `{file, db, roots, scanning, hashing, hash_threads,
hash_threads_default}`. Each of `roots` is `{settings, indexed, offline}`:
the root's policy, an Entry for it (or null before its first scan), and
whether its volume is offline.

### put_root

`{root}` → reload result. Add a root, or replace the one with the same
path. `root` has the [configuration keys](configuration.html#root-keys):
`path` is required, the rest default. Validation: the path must be an
existing absolute directory, `interval_minutes` at least 1, exclude patterns
valid, content-id folders inside the root.

### remove_root

`{path}` → reload result. Stop indexing a root and drop its entries.

### reload

No parameters → `{added, changed, removed}`. Re-read `settings.toml`,
start new roots, rescan changed ones, drop removed ones.
