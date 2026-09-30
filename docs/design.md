# steward design

steward is a per-user service that owns one index of the filesystem and
serves it to applications over `$XDG_RUNTIME_DIR/steward/service.socket`.
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
3. **Client invalidation.** Apps that change files send
   `{"op":"invalidate","path":…}`; the daemon debounces for 2 s, drops paths
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
hard link), relinked by each hashing pass so renamed files are found again. A file that changes
while being hashed is discarded and retried next pass. Only subtrees listed
under `contentid` in the config are hashed automatically; `steward cid` and
`steward hash` work anywhere.

Queries: `find_content` (id → current paths), `duplicates` (by wasted
bytes).

## Protocol

One JSON object per line, one response per request:
`{"op":"children","path":"/home/me"}` →
`{"status":"ok","result":[…]}`. Ops: `status`, `scan`, `invalidate`, `stat`,
`children`, `locate`, `classify`, `content_id`, `hash_tree`, `find_content`,
`duplicates`, `content_summary`, `export_qdirstat`, and settings management:
`settings`, `put_root`, `remove_root`, `reload`. Settings changes are
validated by the daemon, written into settings.toml with comments kept
(`toml_edit`), and applied at once, so applications manage roots the same way
a person editing the file does. Debug with
`socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/steward/service.socket`.
The socket directory is `0700`. varlink is an obvious later option since it's
the same framing idea with an interface description.

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

## Known gaps

- `locate` matches the final name component only; full-path and faster
  substring search want a trigram/FTS index (turso has an `fts` feature).
- Classification reloads the whole subtree after every scan (1.4 s for
  `/usr`); it should follow the scan's dirty set instead.
- Per-user only. A system-wide instance would need to filter results by the
  caller's permissions (`SO_PEERCRED`).
- Paths in JSON are converted lossily; non-UTF-8 names need a byte-safe
  encoding on the wire.
- Piece layers aren't stored; a torrent app must re-hash to build one.
