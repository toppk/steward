# steward design

steward is a per-user service that owns one index of the filesystem and
serves it to applications over `$XDG_RUNTIME_DIR/steward/content.socket`
(administration goes through `api.socket` next to it).
It is deliberately the opposite of baloo/tracker: the core only knows
*structure* (paths, stat fields, well-known classes, content identity).
Anything content-specific — EXIF, audio tags, full text — belongs in the
application that cares, keyed by the content ids steward provides.

```
 apps (file manager, photo app, torrent client, qdirstat-style viewer)
        │  JSON lines over unix socket
 ┌──────┴───────────────────────────────────────────────────────┐
 │ stewardd                                                     │
 │  scheduler ─ invalidation queue ─ request handlers           │
 │  ┌─────────────┐  ┌──────────────────┐  ┌──────────────────┐ │
 │  │ L1 index    │→ │ L2 classify      │  │ L3 contentid     │ │
 │  │ scan + diff │  │ tags, gitignore  │  │ BEP 52 merkle    │ │
 │  └──────┬──────┘  └────────┬─────────┘  └────────┬─────────┘ │
 │         └────────── turso (SQLite format) ───────┘           │
 └──────────────────────────────────────────────────────────────┘
```

## Crates

| crate | role |
|---|---|
| `steward-index` | L1: parallel walker, diffing writer, queries, storage for tags and content ids |
| `steward-classify` | L2: directory classes and gitignore state → tags; file categories by extension |
| `steward-contentid` | L3: BitTorrent v2 pieces root, no I/O policy, no DB |
| `steward-proto` | wire types and socket path |
| `stewardd` | the service: config, scheduling, invalidation, socket server |
| `steward-cli` | `steward` command: `tree`, `ls`, `locate`, `dups`, `cid`, … |
| `steward-ui` | GPUI qdirstat-style tree with locate; a plain socket client like any other app |

## Layer 1: the index

One `entries` row per path: `parent`, `name` (raw bytes, so non-UTF-8 names
survive), `kind`, `mode`, `uid`, `gid`, `size`, `alloc` (`st_blocks*512`),
`nlink`, `dev`, `ino`, `mtime_ns`, `ctime_ns`, plus subtree totals `t_size`,
`t_alloc`, `t_files`, `t_dirs` on directories. Roots have `parent = 0` and
their absolute path as `name`. `UNIQUE(parent, name)` serves both tree
navigation and path resolution; it is the only secondary index.

Kept lean on purpose (measured on a 15.7M-entry home, 1.9 GB): files store
ctime and totals as 0, which take no bytes, because only directories' ctime
(trusting rescans) and totals are used; a file's share of its directory's
totals is computed from its own size, alloc and nlink. Two further options
were measured and deferred until size matters: keying the name index on a
hash (about 270 MB, but the database would no longer enforce one entry per
name), and compressing timestamps.

**Scan pipeline.** A rayon walker (the dust/disktree shape) stats every
entry and sends one `Listing` per directory to a single async writer. A
listing is always sent before its subdirectories are walked, so the writer
already has the parent's row id. The writer loads the stored children of that
directory (one indexed query), diffs by name, and writes only inserts,
updates and subtree deletes, committing every 20k changes so readers see
progress. Totals are then recomputed only for directories that changed and
their ancestors, deepest first.

**Hardlinks** contribute `1/nlink` of their size to every directory that
holds a link, which keeps totals local (no global de-dup pass) and exact
whenever all links are inside the subtree being viewed.

**Scanning a subpath** updates that subtree and re-aggregates its ancestors.
Scanning a parent of an existing root adopts the old root instead of
duplicating it.

### Why turso / SQLite

Measured on this machine (turso 0.8.1, release build):

| operation | time |
|---|---|
| insert 1M rows, one transaction | 1.4 s |
| build `parent` index over 1M rows | 0.7 s |
| `name LIKE '%x%'` over 1M rows | 110 ms |
| 1000 child listings (20k rows) | 11 ms |
| `/usr` first scan, 753k entries | 14 s |
| `/usr` full rescan, nothing changed | 1.6 s |
| `/usr` trusting rescan | 0.14 s |
| `steward tree /usr -d 2`, `steward locate` | < 0.1 s |

Readers are unaffected by a running scan. That is fast enough that an
in-memory tree in the daemon isn't needed; the database is the single source
of truth, which is what lets other processes and layers share it. All SQL
lives in `steward-index`, so swapping stores later touches one crate.

## Invalidation without inotify

inotify needs a watch per directory, overflows, and costs kernel memory
proportional to the tree. steward instead layers cheap, bounded checks:

Periodic rescans default to once a day, each one full; the checks below
are what shorter intervals (`interval_minutes`, `full_every`) choose between.

1. **Trusting rescan.** A directory's mtime/ctime
   changes whenever an entry is added, removed or renamed in it, so a
   directory whose stamps match the index is not `readdir`'d; the walk
   descends through its known subdirectories. This is the mlocate trick and
   is why a rescan of `/usr` costs 140 ms. It misses in-place size changes
   of files in unchanged directories.
2. **Full rescan** (every `full_every`th round). Stats every file; still
   only writes what differs.
3. **Client invalidation.** Apps that change files call `invalidate`
   (or `inspect`, below, to have them handled now); the daemon debounces for 2 s, drops paths
   covered by another, and rescans the nearest surviving ancestor. The
   applications doing the writing know best what they touched.
4. **Correctness rule:** a directory's stored mtime is only ever updated by
   its *own* listing, never by its parent's diff. Otherwise an interrupted
   scan could store a new mtime without the new children, and every later
   trusting scan would believe the stale listing.

Future, all still inotify-free:

- **Lazy validation on read.** Before answering `children`, `lstat` the
  directory and rescan it inline if its stamps moved (one syscall per
  query), so interactive views are never stale.
- **btrfs generations** (`BTRFS_IOC_TREE_SEARCH` / `find-new`) to list
  changed inodes since the last scan's transid without walking.
- **fanotify with `FAN_MARK_FILESYSTEM`** as an opt-in for system
  installs; one mark per filesystem, but it needs `CAP_SYS_ADMIN`.

## Layer 2: classification

Runs in-process after every scan and writes `tags(entry, source, tag)` with
`source = "classify"`. A tag goes on the *topmost* entry it applies to and
is inherited; `stat` returns effective (inherited) tags, `children` returns
each child's own.

- `repo` on a directory containing `.git`; `vcs-metadata` on the `.git`.
- `ignored` for entries matched by the repo's `.gitignore` files and
  `.git/info/exclude` (the `ignore` crate's matcher, applied over the index,
  not a second filesystem walk; only the ignore files themselves are read).
- Well-known directories, trusted only with evidence where the name is too
  generic: `target` only beside `Cargo.toml`, `node_modules` beside
  `package.json`, `.venv` only with `pyvenv.cfg`; plus caches
  (`__pycache__`, `.mypy_cache`, `.cache`, …), build output (`.next`,
  `CMakeFiles`, `zig-out`, …), and trash.
- File categories (image, video, audio, archive, document, source, …) come
  from the extension and are computed on read, never stored.

Classification of a changed subpath restarts from its enclosing repository so
ignore rules from above still apply, and is skipped below an already
classified directory. Other classifiers are meant to be separate clients
that own their own `source` namespace (not yet exposed over the socket).

## Layer 3: content ids

The id is the BitTorrent v2 (BEP 52) **pieces root**: SHA-256 of each 16 KiB
block, a binary merkle tree padded to a power of two with zero leaves. It is
independent of piece length, so the same id identifies content across
torrents and lets a torrent be generated from the index. Verified identical
to libtorrent 2.0.11's `pieces root` for files from 730 B to 214 MB. Empty
files have no id, as in BEP 52.

Stored per `(dev, ino)` with the `size` and `mtime_ns` it was computed
under; an id is only returned while both still match. Renames and moves
within a filesystem keep the id: they change the inode's ctime, which is
deliberately not compared (a rename must not re-read a film). Any write
changes mtime and invalidates the id; restoring an old mtime after writing
(`touch -d`) would go unnoticed, the same trade rsync's quick check makes.
Paths are found through `content_entries` (hashed inodes only, one row per
hard link). A scan that meets a new or changed file whose inode was hashed
under its current size and mtime links it at once, so a renamed or moved
file keeps its id without being read, and the scan reports the change (see
events below). It looks inodes up one at a time, switching to loading every
hashed inode once a scan meets more than 2,048 new files. A file that changes
while being hashed is discarded and retried next pass. Only subtrees listed
under `contentid` in the config are hashed automatically; `steward cid` and
`steward hash` work anywhere.

Queries: `find_content` (id → current paths), `resolve` (below),
`duplicates` (by wasted bytes).

### Verification layer

With each content id the hashing pass keeps a **1 MiB verification layer**:
the root of every 64-block (1 MiB) subtree of the file's BEP-52 tree, 32
bytes per MiB (about 0.9 GB for 28 TiB), keyed by content id. Any piece
layer for a power-of-two piece size of 1 MiB or more derives from it by
hashing pairs upward, so a v2 torrent for already-hashed content never
re-reads the file; `piece_layer` returns exactly the bytes of a torrent's
`piece layers` entry (verified against libtorrent at 1, 4 and 16 MiB).
Files of 1 MiB or less have none, as in BEP 52.

## Filesystems and offline volumes

Stored filesystem identity is `f_fsid` from statvfs, which Linux derives from
the filesystem UUID (plus the subvolume on btrfs), never `st_dev`: btrfs
assigns `st_dev` at mount time, so it can change with mount order across
reboots. A known directory found on a different filesystem than it was
indexed on belongs to a volume that is not mounted: it is reported
**offline** and nothing under it is read or changed. An unmounted media
disk must never read as "630,000 files deleted".

Steward only ever reads indexed files. The only files it writes are its own
index and socket, `settings.toml` when a root change is requested, and
qdirstat exports, which refuse to overwrite an existing file.

## Content primitives

What applications build on, all on `content.socket`. The test for adding
one: it must make sense for any application, not one consumer. Steward
knows content and where it was observed; what a consumer does with that
(sharing it, editing it, deduplicating it) stays in the consumer.

- **`resolve {contents: [{id, size?}], recheck?}`**: where each content is.
  Per id, in request order: `state` is `present` (a copy is reachable),
  `offline` (copies are known, all on unmounted volumes), `absent` (seen
  before, no copy now), `unknown` (never seen) or `mismatch` (the given size
  differs from the content's); `observations` lists
  `{path, inode, online, offline_at, mtime_ns}`, reachable ones first (hard
  links share `inode`); `layer` says whether the verification layer is
  stored. `recheck` re-stats every copy first: changed or vanished copies are
  dropped and their directories queued for rescanning, a copy that fails to
  stat is offline if the nearest existing ancestor is an indexed directory
  now on another filesystem.
- **`inspect {paths}`**: these paths may have changed; bring the catalog
  up to date for them now. For a file, the parent directory is rescanned
  (its own listing read even when its times say nothing changed, since
  in-place edits leave them alone; subdirectories trusted as usual), then its
  content id established: the stored id if size and mtime still match, else
  a hash on the hashing pool. Directories are rescanned in full; files below
  them are hashed only by the configured policy. Answers per path
  `{path, kind, id, size, error}`, errors typed (`not_under_root`,
  `not_found`, `changing`, `unreadable`).
- **`verify {id, path, reason}`**: an application has reason to think
  `path` no longer holds `id` (it found bytes that disagree). The path is
  withheld from `resolve` while steward rescans its directory and rereads
  the file in full, whatever the stat says; it records what the file holds
  and answers `unchanged`, `changed` (with `current`), `gone`, `unreadable`
  (the inode's id is dropped) or `not_file`. `reason` is logged, never
  interpreted. This is how silent changes behind an unchanged size and mtime
  are caught.
- **`piece_layer {id, piece_size}`**: a stored part of the content's own
  hash tree (above).

None of them writes to a file. The worst a client can do is make steward
read and hash files under its roots.

### Events

`subscribe {since?, ids?}` turns on events for a connection; `unsubscribe`
stops them. Each arrives as an `event` notification
`{seq, time, name, data}`:

| name | data | from |
|---|---|---|
| `content.observed` | `{id, path}` | a scan linking a hashed inode at a new path, or a hash |
| `content.moved` | `{id, from, to}` | one scan losing and gaining the same hashed inode |
| `content.lost` | `{id, path, reason}` | `deleted`, `changed` (new stat, or a reread disagreed), `unreadable` |
| `storage.offline` | `{path}` | a scan finding a known directory's volume unmounted |
| `storage.online` | `{path}` | a scan finding it back |
| `storage.unindexed` | `{path}` | a root removed from the configuration |

`ids` filters `content.*` events; `storage.*` always arrive, and carry the
directory only. Events are as timely as what produces them: a periodic
scan, an `invalidate`, an `inspect` or a `verify` (there is no inotify).

The daemon numbers events and keeps the last 4,096 in memory. `since`
replays those after it; the subscribe result `{epoch, seq, complete}` says
whether the replay reached back that far. `epoch` changes on every daemon
start, when earlier `seq`s mean nothing. A subscriber that falls more than
1,024 events behind gets a `gap` notification `{after}`. After any gap a
client resolves what it tracks again.

## Protocol

JSON-RPC 2.0, one message per line, on two Unix sockets in
`$XDG_RUNTIME_DIR/steward/` (directory `0700`). The split is by capability,
not by client:

- `content.socket`, for applications: `status`, `stat`, `children`, `locate`,
  `content_id`, `find_content`, `duplicates`, `content_summary`,
  `piece_layer`, `resolve`, `inspect`, `verify`, `invalidate`, `subscribe`,
  `unsubscribe`.
- `api.socket`, administration: all of those, plus `scan`, `classify`,
  `hash_tree`, `export_qdirstat` and settings management (`settings`,
  `put_root`, `remove_root`, `reload`). Settings changes are validated by
  the daemon, written into settings.toml with comments kept (`toml_edit`),
  and applied at once.

`{"jsonrpc":"2.0","id":1,"method":"stat","params":{"path":"/home/me"}}` →
`{"jsonrpc":"2.0","id":1,"result":{…}}`. Requests on one connection run
concurrently; match responses by `id`. Errors are
`{code, message, data: {type}}`, where `type` is stable for programs:
`parse_error`, `invalid_request`, `method_not_found`, `invalid_params`,
`forbidden` (an administration method on `content.socket`), `not_indexed`,
`not_under_root`, `unknown_content`, `no_layer`, `changing`, `failed`. Debug
with `steward raw METHOD '{…}'` or
`socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/steward/content.socket`.

`status` includes `activity`: the scan in progress (path, mode, seconds),
every file being read for hashing (path, size, seconds), the hash queue, the
open connections and subscribers, and the last event's `seq`. `kill -USR2`
writes the same report to the daemon's log.

`python/steward_client.py` is a standard-library client: blocking and
asyncio, typed results for the content primitives, and an event stream that
reconnects, resumes from the last `seq` and yields a `Gap` when it can't.

## Configuration

`$XDG_CONFIG_HOME/steward/settings.toml` (or `$STEWARD_CONFIG`). Roots are
subtrees of the one `/` namespace, stored in one index; each carries its own
policy, so layers can run on much less than layer 1 covers:

```toml
[[root]]
path = "~"
exclude = ["/down/big", "*.iso"]   # gitignore syntax, relative to the root

[[root]]
path = "/home/media"
classify = false
contentid = ["Movies", "TV"]       # content ids only here
```

Also per root: `interval_minutes`, `full_every`, `one_filesystem`. With no
roots the daemon indexes `$HOME`.

`steward reload` (or SIGHUP) re-reads the file: new roots start scanning,
roots whose settings changed get a full rescan (so a new exclude drops its
entries at once), and indexed roots no configured root covers are deleted,
also at startup. The daemon refuses to scan outside its roots, so the index
only ever holds what the settings describe; `steward --db` works ad hoc.

A single database for all roots is deliberate for now: one namespace means
one answer to "where else is this content". Splitting per root (independent
write locks, removal by deleting a file) stays possible because all SQL is
in `steward-index`.

## Logging

stewardd, `steward` and `steward-ui` log through `tracing` (the
`steward-log` crate), one line per event on stderr:
`stewardd: warning: hash{path=/home/media/TV}: saving content ids: …; retrying`.
Errors and warnings are labelled (coloured on a terminal); spans name the
scan, hashing job or client connection a line belongs to. The daemon logs
at info by default, `-v` adds debug (each phase of scans and hashing),
`-vv` trace (every file, request and event); the CLI is quiet unless
something is wrong, and its `-v` steps up from warnings. `RUST_LOG`
overrides both (`RUST_LOG=steward_index=trace`). When stderr is the systemd
journal, lines carry their syslog priority and no timestamp; otherwise the
daemon prefixes local time.

## Known gaps

- `locate` matches the final name component only; full-path and faster
  substring search want a trigram/FTS index (turso has an `fts` feature).
- Classification reloads the whole subtree after every scan (1.4 s for
  `/usr`); it should follow the scan's dirty set instead.
- Per-user only. A system-wide instance would need to filter results by the
  caller's permissions (`SO_PEERCRED`).
- Paths in JSON are converted lossily; non-UTF-8 names need a byte-safe
  encoding on the wire.
- The event backlog lives in memory: a daemon restart is always a gap.
- Offline handling is tested by faking a directory's stored fsid, not with
  real mounts.
