# steward

A per-user filesystem index service for Linux: locate-style path index with
qdirstat-style subtree totals, classification (git repos, ignored build
output, caches), and BitTorrent v2 content ids — served to applications over
`$XDG_RUNTIME_DIR/steward/service.socket`. No inotify.

See [docs/design.md](docs/design.md) for the architecture.

```sh
just deps               # once: Fedora dev packages for the GUI
just init-config        # writes ~/.config/steward/settings.toml (all commented out)
just daemon -v          # run stewardd in the foreground (-v: roots and state changes, -vv: connections)
just locate '*.torrent' # in another terminal
just tree ~ 3           # qdirstat-style tree in the terminal, depth 3
just ui ~               # the same tree in a GPUI window (needs `just deps` once)
just cli status         # any steward subcommand
just install            # ~/.local/bin + systemd user unit
```

Every `steward` subcommand also runs without the daemon against an index file
with `--db PATH` (or `STEWARD_DB`), e.g.
`steward --db /tmp/home.db scan ~` then
`steward --db /tmp/home.db export-qdirstat ~ -o /tmp/home.cache.gz`
(qdirstat 2.0 cache format, matching qdirstat's own writer line for line).

`python/steward_client.py` is a single-file, stdlib-only Python client
(`Client` and asyncio `AsyncClient`) for applications; run it directly for a
quick look at a directory.

MIT licensed.
