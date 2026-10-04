---
title: Configuration
eyebrow: Use
lede: One TOML file says what steward indexes and how. It is yours to edit, and steward edits it too (keeping your comments) when you change roots from the desktop app or the API.
description: Every key in steward's settings.toml, and the environment variables steward reads.
---

## Where it lives

`~/.config/steward/settings.toml`, or `$XDG_CONFIG_HOME/steward/settings.toml`.
Set `STEWARD_CONFIG` to use another file. A missing file is fine: steward
then indexes your home directory with default settings.

Unknown keys are errors, so a typo is reported instead of silently ignored.

## Applying changes

After editing the file, any of these applies it without restarting:

```sh
steward reload                       # or:
systemctl --user reload steward      # the service maps reload to SIGHUP
```

A reload starts scanning new roots, rescans roots whose settings changed,
and **removes roots you deleted from the index** (the files themselves are
untouched, of course). The reload result lists what was added, changed and
removed.

The desktop app's Settings tab, `steward put-root` / `steward remove-root`,
and the `put_root` / `remove_root` API methods validate the change, write
it into this file with your comments and formatting kept, and apply it at
once.

## Top-level keys

| key | default | meaning |
|---|---|---|
| `db` | `~/.local/state/steward/index.db` | The index file. `~` is expanded; `$XDG_STATE_HOME` moves the default. |
| `hash_threads` | half the CPUs, at most 4 | Files hashed at once for content ids. Hashing is mostly disk-bound, and a few sequential readers beat many seeking ones on spinning disks. A change applies to the next hashing job. |
| `[[root]]` | your home directory | One table per root, below. With no `[[root]]` at all, steward indexes `$HOME`; `root = []` means no roots. |

## Root keys

| key | default | meaning |
|---|---|---|
| `path` | *required* | The directory to index: absolute, or starting with `~`. |
| `interval_minutes` | `1440` (a day) | Time between scheduled rescans. At least 1. |
| `full_every` | `1` | Every Nth scheduled rescan is full; the rest are trusting. `0` and `1` both mean always full. See [Staying current](concepts.html#staying-current-without-inotify). |
| `one_filesystem` | `true` | Don't descend into other filesystems mounted below `path`. |
| `exclude` | `[]` | Paths not to index at all, as gitignore patterns relative to `path`. |
| `classify` | `true` | Tag repositories, ignored files, build output, caches and the like. |
| `contentid` | `[]` | Folders whose files get content ids automatically, relative to `path` or absolute (inside the root). |

### Exclude patterns

`exclude` uses `.gitignore` syntax, anchored at the root's `path`:

```toml
exclude = [
  "/Downloads/incomplete",   # leading slash: this exact path under the root
  "*.iso",                   # any file or directory with this name, anywhere
  "node_modules/",           # trailing slash: directories only
  "!/keep/node_modules/",    # negation re-includes
]
```

Excluded paths are not scanned, not counted in totals and never hashed.

### Content id folders

Hashing reads every byte, so choose the folders where content identity is
useful (a media library, downloads, photo archives) rather than all of your
home directory:

```toml
[[root]]
path = "/home/media"
classify = false
contentid = ["Movies", "TV", "/home/media/Music"]
```

After each scan of the root, files in these folders without a current
content id are queued for hashing. Files elsewhere get one only when an
application or the command line asks (`inspect`, `cid`, `hash`).

## A complete example

```toml
# ~/.config/steward/settings.toml

hash_threads = 2

[[root]]
path = "~"
exclude = ["/.cache/", "/.local/share/Steam/", "node_modules/"]

[[root]]
path = "/home/media"
interval_minutes = 720          # twice a day
classify = false
contentid = ["Movies", "TV"]

[[root]]
path = "/mnt/archive"           # an external disk; offline when unplugged
contentid = ["/mnt/archive"]    # everything on it
```

## Environment

| variable | used by | meaning |
|---|---|---|
| `STEWARD_CONFIG` | daemon, CLI | Path of the settings file. |
| `XDG_CONFIG_HOME` | daemon, CLI | Base for the default settings path. |
| `XDG_STATE_HOME` | daemon | Base for the default index path. |
| `XDG_RUNTIME_DIR` | everything | Where the sockets live (`$XDG_RUNTIME_DIR/steward/`). Without it, a private directory under `/tmp`. |
| `STEWARD_DB` | CLI | Work directly on this index file instead of through the daemon (same as `--db`). |
| `RUST_LOG` | everything | Override log filtering, e.g. `RUST_LOG=steward_index=trace`. See [Operations](operations.html#logging). |
