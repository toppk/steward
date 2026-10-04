---
title: Operations
eyebrow: Use
lede: Running steward day to day. This covers the service, what it logs, how to see what it is doing, what it costs, and what to do when something looks wrong.
description: Running steward's daemon as a service, logging, status and diagnostics, resource use and troubleshooting.
---

## The service

`steward service install` writes a systemd user unit,
`~/.config/systemd/user/steward.service`, that runs `steward daemon` from
wherever `steward` is installed, then enables and starts it:

```sh
steward service install      # write, enable, (re)start
steward service status       # systemd's view: running since, recent lines
steward service logs -f      # the journal, following
steward service restart      # also: start, stop
steward service uninstall    # stop, disable, remove the unit
```

The unit runs the daemon with `Nice=10` and `IOSchedulingClass=idle`, so
scans and hashing only use the disk when nothing else wants it. It is a
per-user service: one daemon per user, indexing what that user can read,
answering only that user (the sockets are in a `0700` directory). It starts
with your first login session; `loginctl enable-linger` keeps it running
when you're logged out. `steward service` only ever rewrites or removes a
unit it wrote itself.

`steward daemon` runs the daemon in the foreground, which is the quickest
way to watch a first scan (`-v` for debug, `-vv` for trace):

```sh
steward daemon -v
```

### Upgrading

```sh
steward upgrade
```

That installs the latest release over the current one (verifying its
checksums, as the installer does) and restarts the service, so the daemon
and its clients always speak the same protocol. `steward version` shows both
versions. If you installed from source, `just install` does the same.

## Logging

steward's programs log one line per event to stderr:

```
21:44:02.118 steward: scan{path=/home/me kind=full}: done in 4210 ms: read 182203 dirs, …
21:44:09.540 steward: warning: hash{path=/home/media/TV}: saving content ids: database snapshot is stale…; retrying (1 of 9)
21:45:11.003 steward: error: hash /home/media/Movies: …
```

The part in braces is the **span**: the scan, hashing job or client
connection the line belongs to. Warnings and errors are labelled, and
coloured on a terminal.

| level | the daemon shows it | what's there |
|---|---|---|
| error | always | a scan, hashing job, reload or invalidation that failed |
| warning | always | retries, unreadable files, volumes found offline, verifications that found other bytes |
| info | always | startup, each scheduled scan's summary, hashing results |
| debug | with `-v` | roots and their policies, each phase of scans and hashing, progress every minute |
| trace | with `-vv` | every file hashed, every request, every event |

`RUST_LOG` overrides the levels with `tracing`'s filter syntax, for example
`RUST_LOG=steward_index=trace` for the scanner alone.

Under systemd, when stderr is the journal, each line carries its syslog
priority, so `journalctl --user -u steward -p warning` shows just warnings
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

`systemctl --user kill -s USR2 steward` writes the `activity` part to the log. That
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

`steward: error: connecting to the steward daemon at …`
:   The daemon isn't running (`steward service status`), or `XDG_RUNTIME_DIR` differs between the
    daemon and the client (common in `sudo` or `ssh` sessions).

Out of inodes, or "No space left on device" with space free
:   Something has created a great many small files. Find where:
    `steward tree ~ -d 3 --by items`, or **Rank by Items** in the desktop
    app. On btrfs there is no inode limit; small files exhaust *metadata*
    space instead, and `steward settings | jq '.roots[].fs'` (or the Daemon
    tab) shows its use. To survey a disk your user can't read, such as a
    backup volume, index it once as root into a scratch file and explore that:
    `steward --db /root/backup.db scan /backup`, then
    `steward --db /root/backup.db tree /backup -d 3 --by items`.

Something is changed on disk, but steward hasn't noticed
:   steward doesn't watch for changes. The next scheduled scan will notice;
    `steward scan PATH` or `steward inspect PATH` notices now. Files
    rewritten in place, keeping their directory's times, are only noticed by
    full scans, which are the default.
