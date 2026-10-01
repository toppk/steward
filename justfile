bin := justfile_directory() / "target/release"
here := invocation_directory()
config_dir := env("XDG_CONFIG_HOME", env("HOME") / ".config") / "steward"

default:
    @just --list

build:
    cargo build --release

test:
    cargo test

lint:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

fmt:
    cargo fmt

# Write an example settings.toml unless one exists.
init-config:
    mkdir -p {{config_dir}}
    test -e {{config_dir}}/settings.toml || cp packaging/settings.example.toml {{config_dir}}/settings.toml
    @echo {{config_dir}}/settings.toml

# Run the daemon in the foreground; `just daemon -v` or `-vv` for more.
daemon *args: build
    {{bin}}/stewardd {{args}}

# qdirstat-style GUI; needs a running daemon.
ui path=here: build
    {{bin}}/steward-ui {{path}}

# Fedora packages needed to build steward-ui.
deps:
    sudo dnf install -y libxkbcommon-x11-devel libxcb-devel fontconfig-devel freetype-devel wayland-devel vulkan-loader-devel

# Run any steward subcommand, e.g. `just cli status`.
cli *args: build
    cd {{here}} && {{bin}}/steward {{args}}

locate pattern: build
    cd {{here}} && {{bin}}/steward locate '{{pattern}}'

tree path="." depth="2": build
    cd {{here}} && {{bin}}/steward tree {{path}} -d {{depth}}

scan path=".": build
    cd {{here}} && {{bin}}/steward scan {{path}}

dups path=".": build
    cd {{here}} && {{bin}}/steward dups {{path}}

# Index PATH straight into DB, no daemon (e.g. `just index ~ /tmp/home.db`).
index path db: build
    cd {{here}} && {{bin}}/steward --db {{db}} scan {{path}}

# Write a qdirstat cache for PATH from DB, no daemon.
export-qdirstat path db out: build
    cd {{here}} && {{bin}}/steward --db {{db}} export-qdirstat {{path}} -o {{out}}

# Install binaries to ~/.local/bin and the systemd user unit.
install: build
    install -Dm755 {{bin}}/stewardd {{bin}}/steward {{bin}}/steward-ui -t ~/.local/bin
    install -Dm644 packaging/stewardd.service -t ~/.config/systemd/user
    systemctl --user daemon-reload
    @echo "enable with: systemctl --user enable --now stewardd"

uninstall:
    -systemctl --user disable --now stewardd
    rm -f ~/.local/bin/stewardd ~/.local/bin/steward ~/.local/bin/steward-ui ~/.config/systemd/user/stewardd.service
    systemctl --user daemon-reload

logs:
    journalctl --user -u stewardd -f

# Build the documentation site into _site/ (needs pandoc).
docs:
    site/build.sh _site

# Build the site and serve it at http://localhost:8000.
docs-serve: docs
    python3 -m http.server -d _site 8000
