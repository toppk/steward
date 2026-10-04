---
title: Getting started
eyebrow: Start
lede: Build steward, run it as a user service, point it at your files and ask it some questions. Ten minutes, most of them spent waiting for the first scan.
description: Build, install and configure steward, then run your first queries.
---

## Install

steward runs on Linux, on x86_64 and ARM64. Install the latest release into
`~/.local/bin`, without root:

```sh
curl -fsSL https://toppk.github.io/steward/install.sh | sh
```

The installer downloads the release for your machine, verifies its SHA-256
checksums, and installs:

| program | what it is |
|---|---|
| `steward` | the command line, and the daemon (`steward daemon`) that scans, classifies, hashes and answers requests |
| `steward-ui` | the desktop app, installed where the X11/Wayland keyboard libraries are present (`STEWARD_UI=1` or `0` decides) |

It only ever replaces earlier steward builds, and never an unrelated program
of the same name. Read [the script](install.sh) first if you like. Set
`STEWARD_INSTALL_DIR` to install somewhere else.

Then run the daemon as a systemd **user** service, now and at every login:

```sh
steward service install
steward service logs -f      # watch it work
```

`steward service install` writes `~/.config/systemd/user/steward.service`
for the installed `steward`, enables it and starts it. The service runs at
`Nice=10` with idle I/O priority, so scans and hashing yield to everything
else. To keep it running while you're logged out, enable lingering
(`loginctl enable-linger`).

**Upgrading** is `steward upgrade`: it installs the latest release the same
way and restarts the service. `steward version` shows the version of both
the command and the running daemon.

### From source

You need Rust 1.97 or newer, [just](https://github.com/casey/just) and, for
the desktop app, the GPUI build dependencies (`just deps` installs them on
Fedora). Then:

```sh
git clone https://github.com/toppk/steward
cd steward
just install        # builds, installs to ~/.local/bin, runs `steward service install`
```

To try it without installing, run `just daemon -v` in one terminal and
`just cli …` in another.

## Choose what to index

With no settings file, steward indexes your home directory. To choose,
write `~/.config/steward/settings.toml`. `just init-config` installs a fully
commented example. A typical file:

```toml
[[root]]
path = "~"
exclude = ["/.cache/", "node_modules/"]

[[root]]
path = "/home/media"
classify = false                 # no git repositories to find here
contentid = ["Movies", "TV"]     # compute content ids for these folders
```

Each `[[root]]` is a directory tree with its own policy: how often to
rescan, what to exclude, whether to classify, which folders get content ids.
Apply changes with `steward reload` (or `kill -HUP` the daemon). The
desktop app's Settings tab edits the same file and applies it at once.
[Configuration](configuration.html) lists every key.

## Watch the first scan

The first scan of a root reads every directory under it. Ask for progress:

```sh
steward status          # JSON: roots, scan in progress, hashing, activity
steward settings        # each root's policy and index totals
```

or open the desktop app (`steward-ui`) and its **Daemon** tab. When a root
has been scanned once, later rescans are quick: by default steward rescans
each root once a day, and skips directories whose times show nothing
changed inside them.

## Ask some questions

```sh
steward tree ~ -d 2                  # where the space goes, two levels deep
steward ls ~/Downloads               # children, largest first
steward stat ~/src/steward           # one entry, with its tags
steward locate '*.iso'               # by name: glob, or substring without * ? [
steward locate invoice -l 20         # substring match, first 20
```

Every answer comes from the index, without touching the disk. `stat` shows
tags such as `classify:repo` or `classify:build-output`, inherited from
the directory that earned them.

## Content ids

Folders listed under `contentid` are hashed in the background after each
scan. Any file can also be hashed on demand:

```sh
steward inspect ~/Downloads/debian-13.iso
# [{ "path": "...", "kind": "file", "id": "btv2:5b1f…", "size": 702545920, "error": null }]

steward resolve btv2:5b1f…           # where is this content now?
steward dups /home/media             # identical files, most wasted space first
steward content-summary /home/media  # how much of it has content ids
```

A content id names the bytes, so it stays the same when the file is renamed
or moved, even onto another disk. [Concepts](concepts.html#content-ids)
explains what it is and when it changes.

## Without the daemon

Every `steward` command can also work directly on an index file, with no
daemon involved, using `--db PATH` (or `STEWARD_DB`). This is handy for
experiments and for one-off indexes:

```sh
steward --db /tmp/usr.db scan /usr
steward --db /tmp/usr.db tree /usr -d 1
steward --db /tmp/usr.db export-qdirstat /usr -o /tmp/usr.cache.gz
```

The last command writes a cache file that [qdirstat](https://github.com/shundhammer/qdirstat)
opens directly.

## Uninstall

```sh
steward service uninstall                       # stop, disable and remove the unit
rm ~/.local/bin/steward ~/.local/bin/steward-ui
```

Your settings (`~/.config/steward/`) and the index (`~/.local/state/steward/`)
stay; delete them yourself if you want them gone.
