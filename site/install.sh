#!/bin/sh
# Install or upgrade steward, without elevated privileges:
#   curl -fsSL https://toppk.github.io/steward/install.sh | sh
#
# Installs `steward` (command line and daemon) and, where the X11/Wayland
# keyboard libraries are present, `steward-ui` into ~/.local/bin, after
# verifying each download's SHA-256. Restarts the steward service if it is
# running. Environment:
#   STEWARD_INSTALL_DIR  where to install (default ~/.local/bin)
#   STEWARD_UI           1 to install steward-ui, 0 not to (default: detect)
#   STEWARD_RELEASE_URL  where release assets are (default: the latest GitHub release)
set -eu

repository="toppk/steward"
install_dir="${STEWARD_INSTALL_DIR:-$HOME/.local/bin}"
release_url="${STEWARD_RELEASE_URL:-https://github.com/$repository/releases/latest/download}"

say() { printf '%s\n' "$*"; }
fail() { printf 'steward install: %s\n' "$*" >&2; exit 1; }

case "$(uname -s):$(uname -m)" in
  Linux:x86_64|Linux:amd64) arch="amd64" ;;
  Linux:aarch64|Linux:arm64) arch="arm64" ;;
  *) fail "steward supports Linux on x86_64 and ARM64." ;;
esac

command -v curl >/dev/null 2>&1 || fail "curl is required."
command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is required."

case ":${PATH:-}:" in
  *":$install_dir:"*) ;;
  *) say "warning: $install_dir is not on PATH; add it to run steward by name." >&2 ;;
esac

temporary_dir=$(mktemp -d)
trap 'rm -rf "$temporary_dir"' EXIT HUP INT TERM

# fetch NAME: download NAME_linux_ARCH and its checksum, verify, print the path.
fetch() {
  asset="${1}_linux_$arch"
  file="$temporary_dir/$asset"
  curl --fail --silent --show-error --location --output "$file" "$release_url/$asset" ||
    fail "could not download $asset from $release_url."
  expected=$(curl --fail --silent --show-error --location "$release_url/$asset.sha256" | awk '{print $1}') ||
    fail "could not download $asset.sha256."
  case "$expected" in
    *[!0123456789abcdef]*|'') fail "the checksum for $asset is malformed." ;;
  esac
  actual=$(sha256sum "$file" | awk '{print $1}')
  [ "$expected" = "$actual" ] || fail "checksum verification failed for $asset."
  chmod 755 "$file"
  printf '%s\n' "$file"
}

# version_of PROGRAM: what it reports as `NAME vX.Y.Z` or `NAME dev`, or nothing.
version_of() {
  "$1" --version 2>/dev/null | head -n 1 || true
}

# put NAME FILE: install FILE as NAME, replacing only a steward build.
put() {
  name=$1
  file=$2
  target="$install_dir/$name"
  next=$(version_of "$file")
  case "$next" in
    "$name v"[0-9]*) ;;
    *) fail "the downloaded $name does not identify as a steward release ($next)." ;;
  esac
  previous="not installed"
  if [ -e "$target" ]; then
    previous=$(version_of "$target")
    case "$previous" in
      "$name v"[0-9]*|"$name dev") ;;
      *) fail "refusing to replace $target: it does not identify as a steward build." ;;
    esac
  else
    existing=$(command -v "$name" 2>/dev/null || true)
    if [ -n "$existing" ] && [ "$existing" != "$target" ]; then
      fail "refusing to install: $existing already provides a '$name' command."
    fi
  fi
  if [ "$previous" = "$next" ]; then
    say "Already installed: $next at $target"
    return 1
  fi
  mkdir -p "$install_dir"
  staging="$install_dir/.$name-install-$$"
  cp "$file" "$staging"
  chmod 755 "$staging"
  mv -f "$staging" "$target"
  if [ "$previous" = "not installed" ]; then
    say "Installed $next to $target"
  else
    say "Upgraded $target: $previous -> $next"
  fi
  return 0
}

has_display_libraries() {
  ldconfig -p 2>/dev/null | grep -q 'libxkbcommon-x11\.so\.0' &&
    ldconfig -p 2>/dev/null | grep -q 'libxcb\.so\.1'
}

changed=0
file=$(fetch steward)
put steward "$file" && changed=1

ui="${STEWARD_UI:-}"
if [ -z "$ui" ]; then
  if [ -e "$install_dir/steward-ui" ] || has_display_libraries; then ui=1; else ui=0; fi
fi
if [ "$ui" = 1 ]; then
  file=$(fetch steward-ui)
  put steward-ui "$file" && changed=1
else
  say "Skipping steward-ui (no X11 keyboard libraries found; STEWARD_UI=1 installs it anyway)."
fi

if command -v systemctl >/dev/null 2>&1 &&
  systemctl --user is-active --quiet steward.service 2>/dev/null; then
  if [ "$changed" = 1 ]; then
    systemctl --user restart steward.service
    say "Restarted steward.service on the new version."
  fi
else
  say ""
  say "Next: run the daemon as a user service (starts now and at each login):"
  say "  $install_dir/steward service install"
fi
