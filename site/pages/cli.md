---
title: Command line
eyebrow: Use
lede: "`steward` asks the daemon questions and gives it instructions. Most answers are JSON, ready for jq. A few commands print a human-readable tree or list instead."
description: Every steward command line subcommand and option.
---

## Usage

```
steward [--db PATH] [-v…] <command> [arguments]
```

`steward` talks to the running daemon over its administration socket
(`$XDG_RUNTIME_DIR/steward/api.socket`). Errors go to stderr as
`steward: error: …` with exit status 1.

| option | meaning |
|---|---|
| `--db PATH` | Work directly on this index file, without the daemon (also `STEWARD_DB`). Settings still come from `settings.toml`. |
| `-v`, `-vv`, `-vvv` | Log more to stderr: info, debug, trace. Mostly useful with `--db`, where the engine runs inside the command. |

Paths may be relative; `steward` makes them absolute before sending them.

## Looking around

`steward status`
:   The daemon's state as JSON: configured and indexed roots, whether a scan
    is running, the hashing job, live activity, recent scans, the schedule
    and recent warnings. See [Operations](operations.html#status).

`steward settings`
:   The settings file, each root's policy, its index totals and whether it
    is offline.

`steward ls PATH [--by space|items]`
:   The children of a directory, largest on disk first, with size, share,
    file count and tags.

`steward tree PATH [-d DEPTH] [-t TOP] [--by space|items]`
:   A qdirstat-style tree, largest first: `DEPTH` levels (default 2),
    the `TOP` largest children at each level (default 10).

    ```
    $ steward tree /usr/share -d 1 -t 4
       13.6G 100.0% ##########   318801 share/
        3.9G  28.6% ###           23851   kicad/
        1.4G   9.9% #              2779   virtio-win/
      987.0M   7.1% #             17019   cursor/
      970.1M   7.0% #              2594   code/
        6.5G                      ... 469 more
    ```

    The columns are space on disk, share of the parent, a bar, and the
    number of files beneath.

    `--by items` ranks by **items** instead: every entry beneath (files,
    directories, symlinks and the rest), which is what uses up inodes (or
    btrfs metadata). The first column is then the item count and the last
    the space on disk:

    ```
    $ steward tree /usr/share -d 1 -t 3 --by items
      390.3k 100.0% ##########    13.6G share/
       58.4k  15.0% #            172.9M   icons/
       43.3k  11.1% #            655.4M   doc/
       37.9k   9.7% #            183.5M   help/
      250.7k                      ... 470 more
    ```

`steward stat PATH`
:   One entry as JSON: type, mode, owner, size, space on disk, modification
    time, subtree totals, tags (including inherited ones), file category and
    current content id.

`steward locate PATTERN [-l LIMIT]`
:   Paths whose final name component matches: a glob if the pattern has
    `*`, `?` or `[` (case-sensitive), otherwise a case-insensitive
    substring. At most `LIMIT` results (default 1000).

## Content ids

`steward inspect PATH…`
:   Bring the index up to date for these paths now, and give each regular
    file's content id, hashing it if needed. Directories are rescanned.

`steward cid PATH`
:   One file's content id, hashing it if needed.

`steward resolve ID… [--recheck]`
:   Where copies of each content id are, whether they are reachable, and the
    content's state. `--recheck` re-stats each copy first.

`steward find ID`
:   The paths currently holding this content, as a JSON array.

`steward dups PATH [-l LIMIT]`
:   Groups of identical files under `PATH` (among hashed files), most wasted
    space first. Default limit 50.

`steward content-summary PATH`
:   How many files and bytes under `PATH` have content ids, distinct ids,
    duplicate groups and reclaimable bytes.

`steward hash PATH`
:   Hash every file under `PATH` that has no current content id. Waits until
    done: for a large folder, hours.

`steward piece-layer ID [-p PIECE_SIZE]`
:   The BEP 52 piece layer of this content for a power-of-two piece size of
    at least 1 MiB (default 1 MiB), as hex, without reading the file.

## Keeping current

`steward scan PATH [--trust]`
:   Rescan `PATH` now and wait for it. `--trust` skips directories whose
    times show nothing changed. The path must be under a configured root.

`steward invalidate PATH`
:   Tell the daemon something under `PATH` changed. It rescans a couple of
    seconds later; the command returns at once.

`steward classify PATH`
:   Re-run classification under `PATH`.

`steward events [--since SEQ] [--id ID]…`
:   Print content and storage events as they happen, one JSON object per
    line. `--since` first replays the daemon's backlog after that sequence
    number; `--id` limits content events to these ids.

## Roots and settings

`steward put-root PATH [options]`
:   Add a root, or replace the one at `PATH`, in `settings.toml` and apply
    it. Options: `--interval MINUTES` (default 1440), `--full-every N`
    (default 1), `--cross-filesystems`, `--no-classify`, `--exclude PATTERN`
    and `--contentid FOLDER` (both repeatable).

`steward remove-root PATH`
:   Remove a root from `settings.toml` and its entries from the index.

`steward reload`
:   Re-read `settings.toml`: start new roots, rescan changed ones, prune
    removed ones.

## Exporting

`steward export-qdirstat PATH -o FILE`
:   Write `PATH`'s subtree as a gzipped qdirstat 2.0 cache file, which
    qdirstat opens directly. Refuses to overwrite an existing file.

## Anything else

`steward raw METHOD [PARAMS]`
:   Call any API method with JSON parameters and print the result:

    ```sh
    steward raw stat '{"path": "/etc"}'
    steward raw resolve '{"contents": [{"id": "btv2:1d8e…", "size": 3145728}], "recheck": true}'
    ```

## With jq

```sh
steward status | jq .activity                               # what is running now
steward settings | jq '.roots[] | {path: .settings.path, offline}'
steward dups ~ | jq -r '.[] | "\(.wasted)\t\(.paths[0])"'
steward resolve "$id" | jq -r '.[0].observations[] | select(.online) | .path'
```

## Without the daemon

With `--db PATH`, every command runs the engine inside `steward` itself,
against that index file. Nothing else needs to be running, and it is a
convenient way to build a one-off index of something:

```sh
steward --db /tmp/usr.db scan /usr
steward --db /tmp/usr.db dups /usr
```

`events` needs the daemon. Don't point `--db` at the index a running daemon
is using: the file is locked.
