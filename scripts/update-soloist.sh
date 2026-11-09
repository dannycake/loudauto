#!/usr/bin/env bash
# Downloads Spotify Soloist, Spotify's official headless player, straight from Spotify into
# LOUDAUTO's data folder: ~/.local/share/loudauto/soloist/soloist, where loudautod looks.
#
# LOUDAUTO never ships Soloist: Spotify asks that its archives and binaries aren't
# redistributed. Each machine fetches its own copy from Spotify, the same file Spotify's
# "Downloads and updates" page links to:
#   https://developer.spotify.com/documentation/soloist/reference/downloads-and-updates
#
# Soloist builds expire 90 days after Spotify builds them. install.sh runs this once and
# enables loudauto-soloist-update.timer, which runs it weekly. A run downloads only when
# Spotify has published a newer build than the one installed.
#
# Usage: update-soloist.sh [--force]
set -euo pipefail

force=false
case "${1:-}" in
    --force) force=true ;;
    "") ;;
    *) echo "usage: $(basename "$0") [--force]" >&2; exit 2 ;;
esac

case "$(uname -m)" in
    x86_64) arch=x86_64 ;;
    aarch64 | arm64) arch=arm64 ;;
    armv7l) arch=arm32 ;;
    *) echo "Spotify publishes no Soloist build for $(uname -m)." >&2; exit 1 ;;
esac
url="https://soloist-builds.spotifycdn.com/soloist_release_${arch}.tar.gz"

dir="${XDG_DATA_HOME:-$HOME/.local/share}/loudauto/soloist"
bin="$dir/soloist"
etag_file="$dir/.etag"
mkdir -p "$dir"

etag_of() { tr -d '\r' < "$1" | awk -F': ' 'tolower($1) == "etag" { print $2 }' | tail -n 1; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Ask whether Spotify's build changed, without downloading it.
if ! $force && [[ -x "$bin" && -f "$etag_file" ]]; then
    if curl -fsSI --proto '=https' "$url" -o "$tmp/head"; then
        if [[ "$(etag_of "$tmp/head")" == "$(cat "$etag_file")" ]]; then
            echo "Soloist is up to date: $("$bin" --version 2>/dev/null || echo unknown)"
            exit 0
        fi
    fi
fi

echo "Downloading Soloist ($arch) from Spotify..."
curl -fL --proto '=https' --progress-bar -D "$tmp/headers" -o "$tmp/soloist.tar.gz" "$url"
mkdir "$tmp/x"
tar -xzf "$tmp/soloist.tar.gz" -C "$tmp/x"
[[ -f "$tmp/x/soloist" ]] || { echo "The archive has no soloist executable." >&2; exit 1; }
chmod 755 "$tmp/x/soloist"
version="$("$tmp/x/soloist" --version)" || { echo "The new build doesn't run here." >&2; exit 1; }

# Replace in one step: a Soloist already running keeps the old file until it restarts.
install -m 755 "$tmp/x/soloist" "$dir/.soloist.new"
mv -f "$dir/.soloist.new" "$bin"
for notice in THIRD_PARTY_LICENSES.txt CHANGELOG.md; do
    [[ -f "$tmp/x/$notice" ]] && install -m 644 "$tmp/x/$notice" "$dir/$notice"
done
etag_of "$tmp/headers" > "$etag_file"

echo "Installed $version"
# The build date is the (YYYYMMDD) in the version line; it expires 90 days later.
if [[ "$version" =~ \(([0-9]{8})\) ]]; then
    echo "This build expires around $(date -d "${BASH_REMATCH[1]} +90 days" +%Y-%m-%d)."
fi
if systemctl --user --quiet is-active loudauto.service 2>/dev/null; then
    echo "LOUDAUTO switches to it the next time it starts Soloist."
    echo "To switch now: systemctl --user restart loudauto.service"
fi
