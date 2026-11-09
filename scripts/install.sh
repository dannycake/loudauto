#!/usr/bin/env bash
# Builds LOUDAUTO and installs it for this user: loudautod (the engine), loudauto (the
# window) and loudauto-cli into ~/.local/bin, the desktop file, and the engine's systemd
# user unit. Then it downloads Spotify Soloist from Spotify (scripts/update-soloist.sh,
# installed as loudauto-update-soloist) and enables the weekly timer that keeps it fresh.
# Run it again to update. It starts nothing: the window starts the engine, or
# `systemctl --user start loudauto.service` does.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

bin_dir="$HOME/.local/bin"
apps_dir="$HOME/.local/share/applications"
unit_dir="$HOME/.config/systemd/user"

packages=(-p engine)
binaries=(loudautod)
# The window's crate comes with Milestone C.
if [[ -d crates/app ]]; then
    packages+=(-p app)
    binaries+=(loudauto)
else
    echo "Note: no crates/app yet, so the window (loudauto) is skipped."
fi
packages+=(-p spot-cli)
binaries+=(loudauto-cli)

cargo build --release "${packages[@]}"
# The target dir may be shared (~/.cargo/config.toml), so ask cargo where it is.
release="$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)/release"

mkdir -p "$bin_dir" "$apps_dir" "$unit_dir"
for binary in "${binaries[@]}"; do
    install -m 755 "$release/$binary" "$bin_dir/$binary"
done
install -m 644 packaging/to.skunk.loudauto.desktop "$apps_dir/to.skunk.loudauto.desktop"
install -m 644 packaging/loudauto.service "$unit_dir/loudauto.service"
install -m 755 scripts/update-soloist.sh "$bin_dir/loudauto-update-soloist"
install -m 644 packaging/loudauto-soloist-update.service "$unit_dir/loudauto-soloist-update.service"
install -m 644 packaging/loudauto-soloist-update.timer "$unit_dir/loudauto-soloist-update.timer"
systemctl --user daemon-reload
echo "Installed ${binaries[*]} and loudauto-update-soloist into $bin_dir, the desktop file and the user units."

# Soloist comes from Spotify, never from this repository.
"$bin_dir/loudauto-update-soloist"
systemctl --user enable --now loudauto-soloist-update.timer
echo "Soloist is checked weekly for a newer build (loudauto-soloist-update.timer)."

if systemctl --user --quiet is-active loudauto.service; then
    echo "The engine is running the old build: systemctl --user restart loudauto.service"
fi

echo "If you haven't yet, save your Soloist API key: loudauto-cli set-key"

# Japanese, Chinese and Korean titles need a CJK font.
if [[ -z "$(fc-list :lang=ja)" ]]; then
    echo "No CJK font is installed. To add one: sudo pacman -S noto-fonts-cjk"
fi
