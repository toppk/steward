---
title: Desktop app
eyebrow: Use
lede: steward-ui is a window onto the daemon. It shows where your space goes, what is duplicated, how each root is configured, and what the daemon is doing right now. It never touches the disk itself. Everything it shows comes from stewardd.
description: steward-ui, the desktop app. Its tabs, its keys, and what each view shows.
---

## Starting it

```sh
steward-ui            # opens your home directory
steward-ui /home/media/TV
```

The path picks the root to show first and the folder to open in it. The app
needs a running daemon. Warnings it shows in colour are also written to
stderr (`steward-ui: warning: …`), so they can be copied.

Across the top are the **roots**, one button each. Hover for a root's totals
and how full its filesystem is.
A root whose volume isn't mounted says *(offline)* and shows the index as it
was last scanned. On the right, badges appear while the daemon is
**scanning** or **hashing**. Click either for the details in the Daemon tab.

## Tree

A qdirstat-style view of the current root: every directory with its space on
disk, its share of the parent, a bar, its file count, and tags such as
`classify:repo` or `classify:build-output` (build output, caches and
dependencies in amber, trash in red). Children are sorted largest first.

**Rank by Space | Items** switches what the tree sorts, bars and
percentages by. Items counts every entry beneath (files, directories,
symlinks), which is what fills a filesystem's inodes or btrfs metadata.
Expanded folders and the selection are kept.

Selecting an entry shows its details: path, mode, owner, space on disk and
apparent size, counts, and its content id if it has one. **All locations**
opens the Content ids tab on every path holding the same bytes.

| key | action |
|---|---|
| <kbd>↑</kbd> <kbd>↓</kbd> or <kbd>k</kbd> <kbd>j</kbd> | move the selection |
| <kbd>PgUp</kbd> <kbd>PgDn</kbd> <kbd>Home</kbd> <kbd>End</kbd> | move further |
| <kbd>→</kbd> or <kbd>l</kbd> | expand; again to step into the first child |
| <kbd>←</kbd> or <kbd>h</kbd> | collapse; again to go to the parent |
| <kbd>Enter</kbd> or <kbd>Space</kbd> | expand or collapse |
| <kbd>g</kbd> | make the selected directory the top of the view |
| <kbd>Backspace</kbd> or <kbd>u</kbd> | go up a level |
| <kbd>r</kbd> or <kbd>F5</kbd> | rescan the selected directory |
| <kbd>i</kbd> | rank by space or by items |
| <kbd>/</kbd> or <kbd>Ctrl</kbd>+<kbd>F</kbd> | locate: find by name across the index |

Locate takes a substring, or a glob such as `*.CR3`. Move through the results
with the arrow keys, <kbd>Enter</kbd> to reveal one in the tree,
<kbd>Esc</kbd> to return.

## Content ids

For the current root:

- **Coverage** of each content-id folder: how many files and bytes have
  content ids, distinct ids, duplicate groups and reclaimable space, with a
  **Hash now** button to fill the gaps.
- **Look up** any `btv2:` id and list every path holding it. Click one to
  show it in the tree.
- **Duplicates**: groups of identical files, most wasted space first. Click
  a group for its paths.

## Settings

Every root and its policy, edited in place: path, rescan interval, full-scan
cadence, one filesystem, classification, exclude patterns and content-id
folders. **Save** sends the change to the daemon, which validates it,
writes it into `settings.toml` (your comments are kept) and applies it at
once. **Add root** creates one, **Remove root** drops a root and its index
entries (after you confirm; your files are not touched).

## Daemon

The daemon's internal state, as a snapshot. **Refresh** takes a new one, and
**Refresh every 2 s** keeps it live.

Daemon
:   Version, process id, uptime, the index file and its size on disk, hashing
    threads, both socket paths, connected clients and event subscribers.

Now
:   The scan in progress and how long it has run; the hashing job with a
    progress bar, rate and estimated time left; every file being read at
    this moment, with how far along it is; and the folders waiting to be
    hashed.

Warnings and errors
:   The last 200 the daemon logged, newest first, each with where it
    happened, e.g. `hash{path=/home/media/TV}: …`.

Roots
:   Each root's state (indexed, offline, not scanned yet), its totals, its
    rescan interval, when the next scan is due, and its filesystem: how full,
    free inodes where the filesystem has a fixed number, and btrfs metadata
    use. Anything above 95% is shown in amber.

Recent scans
:   The last 50 scans: when, what, full or trusting, how long, how many
    entries, and what changed (added, updated, deleted, unreadable,
    offline).

Recent events
:   The latest content and storage events: files observed, moved and lost;
    volumes going offline and coming back.

## Everywhere

| key | action |
|---|---|
| <kbd>Ctrl</kbd>+<kbd>1</kbd> … <kbd>Ctrl</kbd>+<kbd>4</kbd> | Tree, Content ids, Settings, Daemon |
| <kbd>Ctrl</kbd>+<kbd>+</kbd> <kbd>Ctrl</kbd>+<kbd>−</kbd> <kbd>Ctrl</kbd>+<kbd>0</kbd> | larger, smaller, normal size (the keypad keys work too) |
| <kbd>Ctrl</kbd>+<kbd>Q</kbd> | quit |
