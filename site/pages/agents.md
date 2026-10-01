---
title: For AI agents
eyebrow: Build
lede: A compact operating guide for agents that help someone use steward, or that write software on top of it. Facts first, then recipes, then the mistakes to avoid.
description: Operating guide for AI agents using or integrating steward. Detection, safe calls, intent-to-command recipes and pitfalls.
---

::: agent
Every page of this site is also plain Markdown: replace `.html` with `.md`.
[llms.txt](llms.txt) indexes them, and [llms-full.txt](llms-full.txt) has
them all in one file. The [API reference](api.md) is the authority on
methods and shapes.
:::

## Facts

- steward is a per-user Linux service, `stewardd`, that indexes the paths
  under configured **roots**: `lstat` fields, directory subtree totals,
  classification tags, and content ids for some files.
- It **only reads** the files it indexes. It never modifies, moves or
  deletes them.
- Its answers come from the index and reflect the **last scan** of each path
  (there is no inotify), not necessarily this instant.
- A **content id** is `btv2:` plus 64 hex digits: the BitTorrent v2 (BEP 52)
  Merkle root of the file's bytes. Equal ids mean identical bytes. Renames
  keep the id; writes change it. Empty files have none.
- Clients connect to `$XDG_RUNTIME_DIR/steward/content.socket`
  (applications) or `api.socket` (administration) and speak JSON-RPC 2.0,
  one JSON object per line.
- The `steward` command is a client for `api.socket`. Most subcommands print
  JSON; `tree`, `ls` and `locate` print text.

## Is steward here?

```sh
test -S "$XDG_RUNTIME_DIR/steward/content.socket" && echo running
steward status | jq '{roots: [.configured[].path], scanning, hashing: .hashing.path}'
```

If the socket is missing, steward isn't running. `systemctl --user status
stewardd` says whether it is installed. Don't start or install it without
the user's agreement.

## Ground rules

1. **Read freely; change only when asked.** Lookups are harmless. Adding or
   removing roots, editing `settings.toml`, forcing hashing of large
   folders, and exports are the user's decisions. Ask first.
2. **steward reports; it doesn't decide.** A duplicate list says which files
   hold identical bytes, not which copy to delete. Never delete or move the
   user's files because of something steward said, unless the user tells
   you exactly what to do.
3. **Check freshness when it matters.** Before acting on a path steward
   returned, confirm it (`stat` it yourself, or use `resolve --recheck`).
   After changing files yourself, tell steward (`steward inspect PATHS` or
   `steward invalidate DIR`).
4. **Offline is not gone.** A root or observation marked offline is on an
   unmounted volume. Its files still exist.
5. **Hashing costs a full read.** `inspect` and `cid` on a few files are
   cheap. `hash` on a media folder can take hours. Don't start one
   casually.

## Recipes

| the user wants | do |
|---|---|
| to know what uses the space | `steward tree PATH -d 2` (text), or `steward raw children '{"path":"/abs/path"}'` (JSON) |
| the size of a folder | `steward stat PATH \| jq '{total_alloc, total_size, total_files}'` |
| to find files by name | `steward locate 'pattern'`: substring, or glob with `* ? [`; names only, not contents |
| the content id of a file | `steward inspect PATH \| jq -r '.[0].id'` |
| other copies of a file | `id=$(steward inspect PATH \| jq -r '.[0].id'); steward find "$id"` |
| where some content is now | `steward resolve ID --recheck`: check `state` and `observations[].online` |
| duplicates | `steward dups PATH` (only hashed files count; `content-summary` shows coverage) |
| what's in a git checkout that is ignored or built | `steward stat PATH \| jq .tags`; tags like `classify:build-output`, `classify:ignored` |
| why steward seems behind or busy | `steward status \| jq '{activity, hashing, problems: .problems[:5]}'` |
| steward to notice a change now | `steward inspect PATHS` (files: rescan + id), or `steward scan DIR` |
| to index another folder | with consent: `steward put-root PATH` (add `--contentid FOLDER` for ids) |

Paths can be relative or use `~`. The CLI makes them absolute. The raw API
needs absolute paths.

## Reading results

- Sizes are bytes. `alloc` and `total_alloc` are space on disk, and `size`
  and `total_size` are apparent size. Report the one the user means. For
  "how much space", use `total_alloc`.
- `total_dirs` counts the directory itself.
- `tags` on `stat` include inherited ones. On `children`, only each child's
  own.
- `resolve` states: `present`, `offline` (all copies on unmounted volumes),
  `absent` (known, no copy now), `unknown` (never seen), `mismatch` (your
  size disagrees).
- Errors carry `data.type`. The command line prints
  `steward: error: <message>` and exits 1. The common ones are
  `not_indexed` (not scanned yet or no such path), `not_under_root`
  (outside every root), `invalid_params`.

## Writing software that uses steward

Read [Building on steward](applications.md), then the
[API reference](api.md). In short:

- Make steward optional. The program must work without it.
- Connect to `content.socket`. In Python, vendor `python/steward_client.py`
  (stdlib only): `Client()` / `AsyncClient()`.
- To identify files, `inspect(paths)` (batch). To locate content,
  `resolve([(id, size)], recheck=True)`. After writing files,
  `inspect` or `invalidate`. When bytes disagree, `verify(id, path)` and
  resolve again.
- To follow changes, `AsyncClient.events(ids=…)`. Handle `Gap` by
  re-resolving everything.
- Every method is safe to repeat after `ConnectionLost`.

```python
from steward_client import Client, ConnectionLost

try:
    with Client(timeout=30) as c:
        [f] = c.inspect(["/home/me/Downloads/file.iso"])
        copies = c.resolve([(f.id, f.size)], recheck=True)[0].online if f.id else []
except ConnectionLost:
    copies = []          # steward isn't available: carry on without it
```
