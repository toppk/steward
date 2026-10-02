---
title: Concepts
eyebrow: Start
lede: The handful of ideas that explain everything else. These are roots and the index, how steward stays current without inotify, what offline means, classification, content ids, and the observations and events built on them.
description: steward's concepts — roots, the index, scans, offline volumes, classification, content ids, observations and events.
---

## Roots and the index

A **root** is a directory tree you ask steward to index, together with its
policy: how often to rescan it, what to exclude, whether to classify it,
which of its folders get content ids. All roots share **one index** and one
namespace of absolute paths, so a query about `/home/media/TV` and one about
`~/src` go to the same place. If roots are nested, a path belongs to the
deepest root that contains it.

For every path under a root, the index keeps what `lstat` reports:

| kept | not kept |
|---|---|
| name and parent, type, permissions, owner and group | file contents |
| size, and space on disk (blocks × 512) | extended attributes, ACLs |
| link count, filesystem id, inode number | access times |
| modification time (and change time, for directories) | anything inside archives |

Each directory also carries **subtree totals**: total size, total space on
disk, and the number of files, directories and **items** beneath it. Items
counts every entry: files, directories, symlinks, sockets and the rest. It
is what uses up a filesystem's inodes (or, on btrfs, its metadata space),
so ranking by items finds where millions of small files pile up. They are kept
current as the tree changes, so "how big is this folder" is a lookup, not a
walk. A file with several hard links contributes its share of the bytes to
each directory that holds a link, so totals don't count the same bytes
twice. Items count names, so each link counts once: like `du --inodes -l`,
not plain `du --inodes`, which counts a shared inode only once.

The index is a single database file in SQLite format, by default
`~/.local/state/steward/index.db`. Its size grows with the number of paths,
not the size of the files: about 130 bytes per path.

## Staying current without inotify

steward does not watch the filesystem with inotify. Watching tens of
millions of paths needs one kernel watch per directory, raised system
limits, and a full rescan after every overflow or restart anyway. Instead,
steward keeps up in four ways:

Periodic rescans
:   Each root is rescanned on its own schedule, once a day by default
    (`interval_minutes`). Writes are limited to what changed.

Trusting rescans
:   A directory whose modification and change times match the index has the
    same names in it as last time, so a *trusting* rescan does not read it
    again. It only descends into its subdirectories. This is the trick
    `updatedb` uses, and it makes a rescan of an unchanged `/usr` take
    milliseconds. It cannot see a file rewritten in place (the directory's
    times don't change), so every `full_every`th rescan stats every file.
    With the default `full_every = 1`, every scheduled rescan is full.

Invalidation
:   An application that changed files can say so (`invalidate`). steward
    waits two seconds to gather related changes, then rescans the nearest
    directory that still exists.

Inspection
:   An application that needs the answer now asks steward to `inspect`
    specific paths. They are rescanned and, for files, hashed before the
    call returns.

A directory modified within a second of a scan is stored as "untrusted", so
the next trusting scan reads it again. A change landing in the same clock
tick as the scan cannot be missed for good.

## Filesystems and offline volumes

steward records each path's **filesystem id**, which Linux derives from the
filesystem's UUID (and the subvolume, on btrfs). It does not use the device
number, which can change between boots.

When a scan finds a known directory on a *different* filesystem than it was
indexed on, that directory's volume isn't mounted: the scan sees the empty
mount point instead. steward then reports the directory as **offline**,
leaves everything under it exactly as indexed, and does not read or change
it. Unplugging a disk is not mistaken for deleting everything on it, and
plugging it back in brings everything back, still identified.

Offline status shows up in `settings`, in `resolve` results, in
`storage.offline` and `storage.online` events, and in the desktop app.

By default (`one_filesystem = true`) a root does not descend into other
filesystems mounted beneath it. Make another root for each one you want
indexed.

## Classification

Classification records what things *are*, using names and a little context
and never contents. It runs after each scan of a root that has
`classify = true`.

**Directory classes** are tags on the topmost directory they apply to, and
every path beneath inherits them. They are prefixed `classify:`.

| tag | meaning |
|---|---|
| `repo` | the top of a git repository (it contains `.git`) |
| `vcs-metadata` | the `.git` directory itself |
| `ignored` | a path the repository's `.gitignore` rules ignore |
| `build-output` | `target/` beside `Cargo.toml`, `build/` beside a CMake, Gradle or Meson file, `dist/`, `zig-out/`, `.next/`, `CMakeFiles/` … |
| `dependencies` | `node_modules/` beside `package.json`, `.terraform/` |
| `venv` | a Python virtual environment (`.venv/` or `venv/` containing `pyvenv.cfg`) |
| `cache` | `__pycache__/`, `.mypy_cache/`, `.pytest_cache/`, `.gradle/`, `.cache/` … |
| `trash` | the desktop trash (`~/.local/share/Trash`, `.Trash-<uid>` on volumes) |

Names that would be ambiguous alone need a neighbour to confirm them: a
folder called `build` is only build output if a build file sits next to it.

**File categories** come from the file name alone and are computed when
asked, not stored: `image`, `video`, `audio`, `document`, `source`,
`archive`, `object`, `disk-image`, `torrent`.

## Content ids

A content id names a file's **bytes**, not its path. steward uses the
BitTorrent v2 format (BEP 52) and writes it as `btv2:` followed by 64 hex
digits:

```
btv2:1d8e17666fc6c23760eb2d8ec0883756d1a0dd34b6cf71292c6f37b38567434f
```

It is the root of a SHA-256 Merkle tree over the file's 16 KiB blocks: the
same value BEP 52 calls the file's *pieces root*. Any software that reads
v2 torrents, or computes the same tree, gets the same id for the same bytes,
and the id doesn't depend on a torrent's piece size. Empty files have no
content id, as in BEP 52.

### When an id is computed

- In the background, for every file under a root's `contentid` folders,
  after each scan of that root.
- On demand, for any file, through `inspect`, `content_id` or `verify`, or
  `hash_tree` for a whole folder.

Hashing runs on its own thread pool (`hash_threads`, at most four by
default) so it never slows scans, and saves its progress as it goes.

### When an id stays and when it goes

An id is stored per **inode**, with the size and modification time it was
computed under. It is reported only while both still match.

- **Renames and moves keep it.** They change neither size nor modification
  time, so the id moves with the file, and a scan that finds the file at its
  new path links it at once without reading it again. A move onto another
  filesystem creates a new inode, which has to be read again before it has
  an id (the same one), either by its folder's policy or because an
  application asks.
- **Hard links share it.** All paths to one inode show the same id.
- **Writes drop it.** Any write changes the modification time, so the old id
  stops being reported, and the file is hashed again.
- **One gap:** bytes changed while size and modification time were restored
  (`touch -d`, some sync tools) go unnoticed until something reads the file.
  `verify` exists for exactly this. It rereads the file and records what it
  really holds.

### The verification layer

Alongside each id of a file over 1 MiB, steward keeps the hash of every
1 MiB section of the file: 32 bytes per MiB, under 1 GB for a 28 TiB
library. From it steward derives the tree's layer for any power-of-two
piece size of 1 MiB or more (`piece_layer`) without reading the file again.
That is the same data a v2 torrent stores in its `piece layers` field.

## Observations

steward's catalog is a set of **observations**: "content X was seen at path
P on filesystem F, inode I". `resolve` turns a content id into its current
observations and a **state**:

| state | meaning |
|---|---|
| `present` | at least one copy is reachable now |
| `offline` | copies are known, but all of them are on volumes that aren't mounted |
| `absent` | steward has seen this content before, but holds no current copy |
| `unknown` | steward has never seen this content |
| `mismatch` | the caller said to expect a size, and this content has a different one |

Copies with the same id are byte-identical, so "which copy" is a choice of
convenience (reachable, nearby, on a fast disk), not of correctness.

## Events

Applications can subscribe to changes in the catalog instead of polling:

| event | when |
|---|---|
| `content.observed` | content was seen at a new path (a scan linked a hashed inode, or a file was hashed) |
| `content.moved` | the same inode was found at a new path in one scan: a rename or move |
| `content.lost` | a path stopped holding content: `deleted`, `changed`, or `unreadable` |
| `storage.offline` | a known directory's volume is no longer mounted |
| `storage.online` | it is back |
| `storage.unindexed` | a root was removed from the configuration |

Events are numbered, and the daemon keeps the most recent 4,096, so a client
that reconnects can resume where it stopped. When that isn't possible (the
daemon restarted, or the client fell too far behind) the client is told so
explicitly and should re-resolve what it tracks. Without inotify, an event
arrives when steward notices the change: at the next scan, invalidation,
inspection or verification.

## Two sockets

steward listens on two Unix sockets in `$XDG_RUNTIME_DIR/steward/`, and the
split is by **capability**:

- `content.socket` is for applications: catalog lookups, the content
  primitives (`resolve`, `inspect`, `verify`, `piece_layer`), and events.
  Nothing on it changes what is indexed or how.
- `api.socket` is administration: everything above, plus adding and
  removing roots, forcing scans, classification and hashing jobs, settings,
  and qdirstat exports.

The command line tool and the desktop app use `api.socket`. Most
applications only need `content.socket`, and that is the one to grant to a
sandboxed or less trusted program.
