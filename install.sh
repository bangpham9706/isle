#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 zickrian
# Builds Isle (or takes a release package's ready-built programs from bin/) and installs it for the user,
# no root: programs in ~/.local/bin, the extension, D-Bus and systemd user files, and the Inter font.
set -euo pipefail

cd "$(dirname "$0")"
UUID="isle@zickrian.github.io"
# The extension's id before 0.1.0; an install under it is replaced.
OLD_UUID="isle@isle.github.io"
BINDIR="$HOME/.local/bin"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# A bold heading for each stage.
step() { printf '\033[1m==> %s\033[0m\n' "$*"; }

[ "$(id -u)" -ne 0 ] || { echo "Run this as yourself, without sudo: Isle installs to your own home folder." >&2; exit 1; }
# One install at a time (an update clicked in Settings and one run in a terminal): the second stops.
mkdir -p "$DATA/isle"
if command -v flock >/dev/null; then
    exec 9>"$DATA/isle/install.lock"
    flock -n 9 || { echo "Isle: another install of Isle is running; try again once it is done." >&2; exit 1; }
fi
# Put in place whole or not at all: copied beside it, then renamed over it (a full disk leaves the old one).
put() {
    install -Dm755 "$1" "$2.new"
    mv -f "$2.new" "$2"
}
VERSION="$(grep -oE '"version-name": *"[^"]*"' gnome-extension/metadata.json | cut -d'"' -f4)"
# An update keeps the user's choices; only a first install sets Isle's look.
# Settings still saved (Isle uninstalled, now back) count as an update too.
if [ -d "$DATA/gnome-shell/extensions/$UUID" ] || [ -x "$BINDIR/isled" ] ||
    [ -n "$(dconf list /org/gnome/shell/extensions/isle/ 2>/dev/null)" ]; then
    FIRST=0
else
    FIRST=1
fi

# Installs a service file from data/, pointing it at the installed binaries.
install_service() {
    sed "s|@BINDIR@|$BINDIR|" "data/$1" > "$WORK/$1"
    install -Dm644 "$WORK/$1" "$2/$1"
}

SHELF=0
PARAKEET=0
WHISPER=0
# -f, not -x: unpacked where files cannot run (a /tmp mounted noexec), they still install.
if [ -f bin/isled ]; then
    # A release package: the programs come built.
    BUILT=bin
    [ -f bin/isle-shelf ] && SHELF=1
    [ -f bin/isle-parakeet ] && PARAKEET=1
    [ -f bin/isle-whisper ] && WHISPER=1
else
    BUILT=target/release
    step "Building isled"
    cargo build --release -p isle-daemon
    if pkg-config --exists gtk4; then
        step "Building isle-shelf"
        # It needs a newer Rust than isled (1.92); without it the rest still installs.
        if cargo build --release -p isle-shelf; then
            SHELF=1
        else
            echo "isle-shelf did not build (it needs Rust 1.92 or newer: 'rustup update'); skipping drag and drop on the shelf."
        fi
    else
        echo "GTK 4 development files not found: skipping isle-shelf (drag and drop on the shelf)."
        echo "Install them with 'sudo apt install libgtk-4-dev' and run this script again."
    fi
    # The voice assistant's speech recognition: Parakeet (transcribe.cpp) and whisper.cpp, both
    # built with cmake; Vulkan for the GPU.
    if command -v cmake >/dev/null && command -v c++ >/dev/null; then
        if command -v glslc >/dev/null && pkg-config --exists vulkan; then
            GPU_FEATURES=()
            PARAKEET_FEATURES=(--features vulkan)
        else
            echo "Vulkan headers or glslc not found: speech recognition will use the processor only."
            echo "For the GPU: 'sudo apt install libvulkan-dev glslc', then run this script again."
            GPU_FEATURES=(--no-default-features)
            PARAKEET_FEATURES=()
        fi
        step "Building isle-parakeet"
        if cargo build --release -p isle-parakeet "${PARAKEET_FEATURES[@]}"; then
            PARAKEET=1
        else
            echo "isle-parakeet did not build (it needs libclang too: 'sudo apt install libclang-dev'); the voice assistant needs it."
        fi
        step "Building isle-whisper"
        if cargo build --release -p isle-whisper "${GPU_FEATURES[@]}"; then
            WHISPER=1
        else
            echo "isle-whisper did not build (it needs libclang too: 'sudo apt install libclang-dev'); it covers other languages."
        fi
    else
        echo "cmake or a C++ compiler not found: skipping isle-parakeet and isle-whisper (the voice assistant's speech recognition)."
        echo "Install them with 'sudo apt install cmake g++ libclang-dev' and run this script again."
    fi
fi

step "Installing binaries to $BINDIR"
put "$BUILT/isled" "$BINDIR/isled"
if [ "$SHELF" = 1 ]; then
    put "$BUILT/isle-shelf" "$BINDIR/isle-shelf"
    # A running helper is the old version; it starts again, updated, when needed.
    pkill -x isle-shelf 2>/dev/null || true
fi
if [ "$PARAKEET" = 1 ]; then
    put "$BUILT/isle-parakeet" "$BINDIR/isle-parakeet"
fi
if [ "$WHISPER" = 1 ]; then
    put "$BUILT/isle-whisper" "$BINDIR/isle-whisper"
fi

step "Installing services"
# The D-Bus names before 0.1.0.
rm -f "$DATA/dbus-1/services/io.github.isle.Daemon.service" \
      "$DATA/dbus-1/services/io.github.isle.ShelfWindow.service"
install_service io.github.zickrian.isle.Daemon.service "$DATA/dbus-1/services"
# Also for a helper from an earlier install, when it was not rebuilt now.
if [ "$SHELF" = 1 ] || [ -x "$BINDIR/isle-shelf" ]; then
    install_service io.github.zickrian.isle.ShelfWindow.service "$DATA/dbus-1/services"
fi
install_service isled.service "$CONFIG/systemd/user"
install -Dm644 README.md "$DATA/doc/isle/README.md"
# The voice assistant: `isle-setup-voice` downloads the speech model once.
install -Dm755 scripts/setup-voice.sh "$BINDIR/isle-setup-voice"
# Sets a lasting battery charge limit; the assistant types it into a terminal when asked.
install -Dm755 scripts/isle-charge-limit "$BINDIR/isle-charge-limit"

step "Installing the Inter font"
# The island is set in Inter.
install -Dm644 data/fonts/InterVariable.ttf "$DATA/fonts/isle/InterVariable.ttf"
install -Dm644 data/fonts/Inter-LICENSE.txt "$DATA/fonts/isle/Inter-LICENSE.txt"
fc-cache "$DATA/fonts/isle" >/dev/null 2>&1 || true

step "Installing the GNOME Shell extension"
# Replace an install under the old id, so two islands never run at once.
# Settings carry over: the settings schema did not change.
if [ -d "$DATA/gnome-shell/extensions/$OLD_UUID" ]; then
    gnome-extensions disable "$OLD_UUID" 2>/dev/null || true
    rm -rf "$DATA/gnome-shell/extensions/$OLD_UUID"
    current=$(gsettings get org.gnome.shell enabled-extensions)
    updated=$(printf '%s' "$current" | sed -e "s/, '$OLD_UUID'//" -e "s/'$OLD_UUID', //" -e "s/\['$OLD_UUID'\]/@as []/")
    [ "$updated" != "$current" ] && gsettings set org.gnome.shell enabled-extensions "$updated"
    echo "Replaced the install under the old id $OLD_UUID."
fi
DEST="$DATA/gnome-shell/extensions/$UUID"
# Made aside (not among the extensions, where GNOME would read it), then swapped in: a copy that fails
# (a full disk) leaves the old one whole.
rm -rf "$DATA/isle"/extension-new*
STAGE="$(mktemp -d "$DATA/isle/extension-new.XXXXXX")"
chmod 755 "$STAGE"
mkdir -p "$(dirname "$DEST")"
cp -r gnome-extension/. "$STAGE/"
glib-compile-schemas "$STAGE/schemas"
rm -rf "$DEST"
mv "$STAGE" "$DEST"

step "Starting isled"
# Let the session bus see the new activation files.
busctl --user call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus ReloadConfig >/dev/null 2>&1 || true
# Without a user service manager (over SSH, say) isled still starts on its
# own when the extension first calls it.
if systemctl --user daemon-reload 2>/dev/null; then
    systemctl --user enable isled.service >/dev/null 2>&1 ||
        echo "isled could not be set to start with your session ('systemctl --user enable isled' says why); it still starts once the island calls it."
    # A failure here is reported, not fatal: the extension still goes in, and isled starts once called.
    systemctl --user restart isled.service ||
        echo "isled did not start; see 'journalctl --user -u isled' (the island still works without it, minus its services)."
else
    # An isled started by D-Bus would go on running the old version from its replaced file: stopped, it
    # starts anew as the island calls it again (within seconds; see the extension's service check).
    pkill -u "$(id -u)" -x isled 2>/dev/null || true
fi

# The voice assistant set up with an older Isle used Python (up to 3 GB): moved to isle-parakeet
# or isle-whisper, with the model chosen then, and the old setup deleted. Offline, it stays for
# isle-setup-voice.
if { [ "$PARAKEET" = 1 ] || [ "$WHISPER" = 1 ]; } && { [ -d "$DATA/isle/voice-venv" ] || [ -f "$DATA/isle/isle-whisper.py" ]; }; then
    step "Moving the voice assistant to the new speech recognition"
    "$BINDIR/isle-setup-voice" --remove-old </dev/null ||
        echo "That did not finish; run isle-setup-voice later (the voice assistant needs it)."
fi

# A running GNOME Shell on Wayland does not know a new extension until the
# next login, so add it to the enabled list directly when enabling fails.
if ! gnome-extensions enable "$UUID" 2>/dev/null; then
    # On the disabled list (turned off before an uninstall), it would stay off whatever the enabled one says.
    current=$(gsettings get org.gnome.shell disabled-extensions)
    updated=$(printf '%s' "$current" | sed -e "s/, '$UUID'//" -e "s/'$UUID', //" -e "s/\['$UUID'\]/@as []/")
    [ "$updated" = "$current" ] || gsettings set org.gnome.shell disabled-extensions "$updated"
    current=$(gsettings get org.gnome.shell enabled-extensions)
    case "$current" in
        *"'$UUID'"*) ;;
        "@as []") gsettings set org.gnome.shell enabled-extensions "['$UUID']" ;;
        *) gsettings set org.gnome.shell enabled-extensions "${current%]}, '$UUID']" ;;
    esac
fi

# GNOME runs no user extensions at all while this is on (Extensions → "Use Extensions" off).
if [ "$(gsettings get org.gnome.shell disable-user-extensions 2>/dev/null)" = "true" ]; then
    gsettings set org.gnome.shell disable-user-extensions false
    echo "Turned extensions on (they were all switched off in the Extensions app)."
fi

if [ "$FIRST" = 1 ]; then
step "Setting up the top bar"
# Isle's look: a slim glass top bar with the island hanging from it like a notch.
# Only on a first install; an update keeps what the user chose since.
gsettings --schemadir "$DEST/schemas" set org.gnome.shell.extensions.isle top-bar-style glass
gsettings --schemadir "$DEST/schemas" set org.gnome.shell.extensions.isle compact-top-bar true
gsettings --schemadir "$DEST/schemas" set org.gnome.shell.extensions.isle island-in-top-bar true
fi

# Other extensions are never changed; ones that fight with the island get a recommendation.
ENABLED="$(gsettings get org.gnome.shell enabled-extensions 2>/dev/null || true)"
# Whether another extension is installed (for the user or system-wide) and turned on.
extension_on() {
    case "$ENABLED" in *"'$1'"*) ;; *) return 1 ;; esac
    [ -d "$DATA/gnome-shell/extensions/$1" ] || [ -d "/usr/share/gnome-shell/extensions/$1" ]
}
RECOMMEND=()
BLUR="blur-my-shell@aunetx"
if extension_on "$BLUR"; then
    # Its settings schema: in its own folder, else among the system's.
    schema=()
    blur_command="gsettings"
    for dir in "$DATA/gnome-shell/extensions/$BLUR/schemas" "/usr/share/gnome-shell/extensions/$BLUR/schemas"; do
        if [ -f "$dir/gschemas.compiled" ]; then
            schema=(--schemadir "$dir")
            blur_command="gsettings --schemadir $(printf '%q' "$dir")"
            break
        fi
    done
    # Blur my Shell's top bar blur also darkens it, and the glass bar has no colour of its own.
    if [ "$(gsettings "${schema[@]}" get org.gnome.shell.extensions.blur-my-shell.panel blur 2>/dev/null)" = true ]; then
        RECOMMEND+=("Blur my Shell darkens the top bar behind the island. To blur only the rest, turn off
    Blur my Shell → Panel → Blur, or run: $blur_command set org.gnome.shell.extensions.blur-my-shell.panel blur false")
    fi
fi
for uuid in dynamic-island@bjagetic hidetopbar@mathieu.bidon.ca; do
    if extension_on "$uuid"; then
        RECOMMEND+=("$uuid takes the same place in the top bar and covers the island. Turn it off in
    the Extensions app, or run: gnome-extensions disable $uuid")
    fi
done
if [ "${#RECOMMEND[@]}" -gt 0 ]; then
    step "Recommended (Isle changed nothing here)"
    printf -- '- %s\n' "${RECOMMEND[@]}"
fi
if [ -d "/usr/share/gnome-shell/extensions/$UUID" ]; then
    echo "Isle is also installed system-wide (a .deb, .rpm or AUR package): this install in your home folder takes precedence for you."
fi

if [ "$FIRST" = 1 ]; then
    step "Isle $VERSION is installed"
else
    step "Isle is updated to $VERSION"
fi
SHELL_VERSION="$(gnome-shell --version 2>/dev/null | grep -oE '[0-9]+' | head -n1 || true)"
# Only an X11 session of GNOME 48 or older restarts the shell in place; Wayland needs a new login.
if [ "${XDG_SESSION_TYPE:-}" = "x11" ] && [ -n "$SHELL_VERSION" ] && [ "$SHELL_VERSION" -le 48 ]; then
    echo "Press Alt+F2, type 'r' and Enter to restart GNOME Shell."
else
    echo "Log out and back in once so GNOME Shell loads the new extension."
fi
# Some systems put ~/.local/bin on the PATH only once it exists, at the next login; others never.
case ":$PATH:" in
    *":$BINDIR:"*) ;;
    *) echo "Note: $BINDIR is not on your PATH, so run Isle's commands with it: $BINDIR/isled --doctor, $BINDIR/isle-setup-voice." ;;
esac
