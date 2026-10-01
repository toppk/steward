---
title: Operations
eyebrow: Use
lede: Running steward day to day. This covers the service, what it logs, how to see what it is doing, what it costs, and what to do when something looks wrong.
description: Running stewardd as a service, logging, status and diagnostics, resource use and troubleshooting.
---

## The service

`just install` installs a systemd user unit; enable it once:

```sh
systemctl --user enable --now stewardd
systemctl --user status stewardd
journalctl --user -u stewardd -f        # or: just logs
```

The unit runs `stewardd` with `Nice=10` and `IOSchedulingClass=idle`, so
scans and hashing only use the disk when nothing else wants it. It is a
per-user service: one daemon per user, indexing what that user can read,
answering only that user (the sockets are in a `0700` directory).

`stewardd` takes `-v` (debug) and `-vv` (trace) for more logging, see
below. Run in a terminal, it is also the quickest way to watch a first scan:

```sh
just daemon -v
```

::: note
After upgrading steward, restart the daemon
(`systemctl --user restart stewardd`). Clients and daemon must speak the
same protocol, and the daemon's process is what is running, not the files
on disk.
:::

## Logging

steward's programs log one line per event to stderr:

```
21:44:02.118 stewardd: scan{path=/home/me kind=full}: done in 4210 ms: read 182203 dirs, …
21:44:09.540 stewardd: warning: hash{path=/home/media/TV}: saving content ids: database snapshot is stale…; retrying (1 of 9)
21:45:11.003 stewardd: error: hash /home/media/Movies: …
```

The part in braces is the **span**: the scan, hashing job or client
connection the line belongs to. Warnings and errors are labelled, and
coloured on a terminal.

| level | stewardd shows it | what's there |
|---|---|---|
| error | always | a scan, hashing job, reload or invalidation that failed |
| warning | always | retries, unreadable files, volumes found offline, verifications that found other bytes |
| info | always | startup, each scheduled scan's summary, hashing results |
| debug | with `-v` | roots and their policies, each phase of scans and hashing, progress every minute |
| trace | with `-vv` | every file hashed, every request, every event |

`RUST_LOG` overrides the levels with `tracing`'s filter syntax, for example
`RUST_LOG=steward_index=trace` for the scanner alone.

Under systemd, when stderr is the journal, each line carries its syslog
priority, so `journalctl --user -u stewardd -p warning` shows just warnings
and errors. Lines have no timestamp there, because the journal adds its own.

The `steward` command logs only warnings and errors unless given `-v`;
`steward-ui` logs the messages it shows.

## Status

`steward status` returns the daemon's whole state as JSON. The parts most
worth knowing:

| field | what it holds |
|---|---|
| `activity.scan` | the scan in progress: path, kind, seconds |
| `hashing` | the hashing job: path, files and bytes done and total (including bytes read so far of files in flight), start time |
| `activity.reading` | every file being read for hashing right now: path, size, bytes read, seconds |
| `activity.hash_queue` | folders waiting to be hashed |
| `activity.connections`, `activity.subscribers` | clients connected, and those subscribed to events |
| `daemon` | version, pid, uptime, index path and size on disk, hashing threads, socket paths |
| `recent_scans` | the last 50 scans with timings, changes and errors |
| `schedule` | when each root is scanned next |
| `problems` | the last 200 warnings and errors, newest first |

```sh
watch -n2 'steward status | jq .activity'
steward status | jq '.problems[:5]'
```

`kill -USR2 $(pgrep -x stewardd)` writes the `activity` part to the log. That
helps when you are reading the journal and don't have a client handy.

The desktop app's [Daemon tab](gui.html#daemon) shows all of this.

## What it costs

Measured on the author's machine (NVMe, 15.7 million paths in the home
directory, a 44 TiB media library):

| | |
|---|---|
| first scan of the home directory | about 1½ minutes |
| rescan of an unchanged `/usr` (750,000 paths) | 69 ms trusting, 1 s full |
| memory while scanning | about 0.5 GB resident |
| index size | about 130 bytes per path (1.9 GB for 15.7 million) |
| content ids | one read of each file, at disk speed; 32 bytes per MiB kept |

Scans are I/O on metadata only. Hashing reads every byte of the files under
`contentid` folders once, then only files that change.

## The index file

The index lives at `~/.local/state/steward/index.db` (see
[`db`](configuration.html#top-level-keys)), with a write-ahead log next to
it. It holds nothing that can't be rebuilt from the filesystem, except
content ids, which cost a full read to recompute.

To start over: stop the daemon, delete `index.db` and the files beside it
named `index.db-*`, start the daemon. Every root is scanned again, and
content ids are recomputed.

Removing a root from the configuration removes its entries from the index
at the next reload.

## Troubleshooting

`… is not indexed: no configured root covers it`
:   The path isn't under any `[[root]]`. Add one (`steward put-root PATH`)
    or check for a typo.

`… is not indexed yet: root … has not finished its first scan`
:   Wait; `steward status | jq .activity.scan` shows the scan's progress.

`… is not in the index: nothing by that name under root …`
:   The root is indexed but has no such path: it was created after the last
    scan, or excluded. `steward inspect PATH` (or `invalidate`) updates it.

`… is not under a configured root` (type `not_under_root`)
:   `scan`, `inspect` and `verify` only work inside configured roots.

A root says *(offline)*
:   The volume it lives on isn't mounted. Mount it; the next scan
    (`steward scan ROOT`) brings it back online with everything still
    identified.

Hashing seems stuck
:   `steward status | jq '.activity.reading'` shows each file being read and
    how far along it is. A single large file on a slow disk can take
    minutes. Check `problems` for errors.

`steward: error: connecting to stewardd at …`
:   The daemon isn't running, or `XDG_RUNTIME_DIR` differs between the
    daemon and the client (common in `sudo` or `ssh` sessions).

Something is changed on disk, but steward hasn't noticed
:   steward doesn't watch for changes. The next scheduled scan will notice;
    `steward scan PATH` or `steward inspect PATH` notices now. Files
    rewritten in place, keeping their directory's times, are only noticed by
    full scans, which are the default.
