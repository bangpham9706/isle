#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 zickrian
# Packs dist/isle-<version>-<arch>-linux.tar.gz (+ .sha256) with ready-built programs; arg: version (default Cargo.toml).
# Needs libgtk-4-dev for isle-shelf, and cmake g++ libclang-dev for isle-parakeet and isle-whisper
# (plus libvulkan-dev glslc for the GPU).
set -euo pipefail

cd "$(dirname "$0")/.."
VERSION="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)}"
NAME="isle-$VERSION-$(uname -m)-linux"
OUT="dist/$NAME"

cargo build --release --locked -p isle-daemon -p isle-shelf -p isle-parakeet -p isle-whisper

rm -rf "$OUT" "dist/$NAME.tar.gz" "dist/$NAME.tar.gz.sha256"
mkdir -p "$OUT/bin"
install -m755 target/release/isled target/release/isle-shelf target/release/isle-parakeet target/release/isle-whisper "$OUT/bin/"
strip "$OUT/bin/isled" "$OUT/bin/isle-shelf" "$OUT/bin/isle-parakeet" "$OUT/bin/isle-whisper"
cp -r gnome-extension data install.sh uninstall.sh README.md LICENSE CHANGELOG.md "$OUT/"
# What install.sh installs as commands: the voice set-up and the battery charge limit.
mkdir -p "$OUT/scripts"
install -m755 scripts/setup-voice.sh scripts/isle-charge-limit "$OUT/scripts/"
# Compiled on install, for the GLib there.
rm -f "$OUT/gnome-extension/schemas/gschemas.compiled"

tar -C dist -czf "dist/$NAME.tar.gz" "$NAME"
(cd dist && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
rm -rf "$OUT"
echo "dist/$NAME.tar.gz"
