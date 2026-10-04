---
title: Overview
hero: true
hero-eyebrow: Per-user file index service · Linux
hero-title: Know your files. Every path, every copy.
hero-lede: steward keeps one current picture of your filesystem, covering every path, how much space it takes, what kind of thing it is, and a content id that follows each file wherever it moves. It shares that picture with your applications over a local socket and never changes a byte of what it indexes.
hero-links:
  - label: Install
    href: "#install"
    kind: primary
  - label: Get started
    href: getting-started.html
    kind: secondary
  - label: Build on steward
    href: applications.html
    kind: secondary
description: steward is a per-user file index service for Linux. It indexes paths, sizes, classes and BitTorrent v2 content ids, and serves them to applications over a Unix socket.
---

## What steward does

Many programs on a desktop walk the same directories over and over: the
disk-usage viewer, the file search tool, the backup tool, the photo
library, the deduplicator. Each builds its own partial, soon-stale picture.
steward does the walking once, keeps the result current, and lets every
application ask.

::: cards
::: card
[Layer 1]{.label}

### The index

Every path under your roots with its `lstat` fields, plus subtree totals
for each directory: size, space on disk, file and directory counts. Name
search like `locate`, and disk usage like `qdirstat`, from one index.
:::

::: card
[Layer 2]{.label}

### Classification

What things *are*: git repositories, ignored build output, dependency
folders, virtualenvs, caches and trash, plus a category for each file
(image, video, audio, document, source, archive…).
:::

::: card
[Layer 3]{.label}

### Content ids

A content id for each file: the BitTorrent v2 (BEP 52) Merkle root. It
names bytes, not paths, so it survives renames and moves, finds duplicates
anywhere, and matches the id other software computes for the same bytes.
:::
:::

## Install

One command, no root, Linux on x86_64 or ARM64:

```sh
curl -fsSL https://toppk.github.io/steward/install.sh | sh
steward service install      # run the daemon now and at every login
```

The script downloads the latest release for your machine, verifies its
SHA-256 checksums and installs `steward` (and, on desktops, `steward-ui`) into
`~/.local/bin`. It says whether it installed, upgraded or found the same
version already there. It never replaces a program that isn't steward, and
doesn't touch your shell configuration. Later, `steward upgrade` does the
same and restarts the daemon.

Prefer to look first? [Read install.sh](install.sh), or download a binary
from [GitHub Releases](https://github.com/toppk/steward/releases/latest)
(`steward_linux_amd64` and its `.sha256`, for example), check it with
`sha256sum -c`, make it executable and put it on your `PATH`.
[Getting started](getting-started.html) covers configuration and building
from source.

## What it promises

::: safety
**steward only reads what it indexes.** It never creates, modifies, moves or
deletes your files. The only things it writes are its own index, its
sockets, its settings file when you change settings through it, and export
files you ask for (which it will not overwrite).
:::

- **Unplugged is not deleted.** When a disk is unmounted, everything that
  was on it is reported *offline*, not gone. Nothing is dropped, and
  nothing is rescanned as if 600,000 files had vanished.
- **Identity follows the bytes.** A renamed or moved file keeps its content
  id without being read again. A file whose bytes change loses it.
- **Applications get capabilities, not control.** Applications talk to
  `content.socket`, which offers lookups and content primitives. Changing
  what is indexed, or how, is administration, on a separate socket.
- **No inotify.** steward keeps up with periodic scans that skip unchanged
  directories, and with applications telling it what they changed. That
  scales to tens of millions of files with no kernel watch limits.

## How it fits together

::: stack
::: tier
[File manager]{.box}
[Disk usage viewer]{.box}
[Photo library]{.box}
[Your application]{.box}
:::

[JSON-RPC 2.0 over Unix sockets]{.wire}

::: tier
[content.socket [lookups · resolve · inspect · verify · events]{.small}]{.box .socket}
[api.socket [roots · scans · settings · maintenance]{.small}]{.box .socket}
:::

::: tier
[steward daemon [scheduler · scanner · classifier · hasher]{.small}]{.box .daemon}
:::

::: tier
[index [SQLite format, via turso]{.small}]{.box .store}
[settings.toml [your roots and their policies]{.small}]{.box .store}
:::
:::

The daemon (`steward daemon`) runs as your user, as a systemd user service,
at low CPU and I/O priority. The rest of the `steward` command and the
`steward-ui` desktop app are clients like any other.

## Who this documentation is for

::: cards
::: card
### People running steward

[Getting started](getting-started.html), [Configuration](configuration.html),
the [command line](cli.html) and the [desktop app](gui.html).
:::

::: card
### Application developers

[Building on steward](applications.html) explains the patterns. The
[API reference](api.html) lists every method, result and event.
:::

::: card
### AI agents

[For AI agents](agents.html) is a compact operating guide. Every page is
also available as Markdown, and [llms.txt](llms.txt) indexes them.
:::
:::

## A first look

Once it is installed and running:

```sh
steward status                     # roots, scans, hashing, activity
steward tree ~ -d 2                # where the space goes
steward locate '*.iso'             # find by name, from the index
steward inspect ~/Downloads/film.mkv   # its content id, now
steward dups ~                     # identical files, most wasted space first
```

On the author's machine, a first scan of a 15.7-million-entry home
directory takes about a minute and a half. A daily rescan of an unchanged
`/usr` takes 69 ms, and the daemon runs in about half a gigabyte of memory.
