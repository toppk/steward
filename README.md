# steward

A per-user filesystem index service for Linux: locate-style path index with
qdirstat-style subtree totals, classification (git repos, ignored build
output, caches), and BitTorrent v2 content ids — served to applications over
`$XDG_RUNTIME_DIR/steward/service.socket`. No inotify.

See [docs/design.md](docs/design.md) for the architecture.

```sh
cargo build --release
./target/release/stewardd &          # indexes $HOME without a config
./target/release/steward tree ~ -d 2
./target/release/steward locate '*.torrent'
./target/release/steward cid some/file
./target/release/steward dups ~/Pictures
```

MIT licensed.
