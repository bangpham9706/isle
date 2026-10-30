#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 zickrian
# Sets up speech recognition for Isle's voice assistant: downloads the speech model, checking its SHA-256.
# Usage: isle-setup-voice [tiny|base|small|medium|parakeet] [--remove-old]
#   The model given (then also chosen in Settings → Assistant), else the one chosen there, else parakeet.
#   --remove-old  also deletes what the older Python setup left: its venv and faster-whisper models.
set -euo pipefail

DATA="${XDG_DATA_HOME:-$HOME/.local/share}/isle"
MODELS="$DATA/models"
SCHEMAS="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/isle@zickrian.github.io/schemas"
WHISPER_URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main"
PARAKEET_URL="https://huggingface.co/handy-computer/parakeet-unified-en-0.6b-gguf/resolve/main"
VAD_URL="https://huggingface.co/ggml-org/whisper-vad/resolve/main"
# What an older Isle's faster-whisper setup left.
OLD_VENV="$DATA/voice-venv"
OLD_HELPER="$DATA/isle-whisper.py"
HUB="${HF_HUB_CACHE:-${HF_HOME:-${XDG_CACHE_HOME:-$HOME/.cache}/huggingface}/hub}"

# A step's heading, in bold.
step() { printf '\033[1m==> %s\033[0m\n' "$*"; }
# Says what went wrong and stops.
fail() { printf 'isle-setup-voice: %s\n' "$*" >&2; exit 1; }

MODEL=""
GIVEN=0
REMOVE_OLD=0
for arg in "$@"; do
    case "$arg" in
        tiny | base | small | medium | parakeet) MODEL="$arg" GIVEN=1 ;;
        --remove-old) REMOVE_OLD=1 ;;
        -h | --help) sed -n '4,7s/^# \{0,1\}//p' "$0"; exit 0 ;;
        *) fail "unknown argument '$arg' (a model is tiny, base, small, medium or parakeet)" ;;
    esac
done
if [ -z "$MODEL" ]; then
    # Isle in the home folder, else a system package's (its schema is the system's).
    MODEL="$({ gsettings --schemadir "$SCHEMAS" get org.gnome.shell.extensions.isle voice-speech-model ||
        gsettings get org.gnome.shell.extensions.isle voice-speech-model; } 2>/dev/null | tr -d "'" || true)"
fi
MODEL="${MODEL:-parakeet}"

# The official whisper.cpp files (8-bit), as isle-whisper names them: file, SHA-256, size.
case "$MODEL" in
    tiny) set -- ggml-tiny-q8_0.bin c2085835d3f50733e2ff6e4b41ae8a2b8d8110461e18821b09a15c40c42d1cca 43537433 ;;
    base) set -- ggml-base-q8_0.bin c577b9a86e7e048a0b7eada054f4dd79a56bbfa911fbdacf900ac5b567cbb7d9 81768585 ;;
    small) set -- ggml-small-q8_0.bin 49c8fb02b65e6049d5fa6c04f81f53b867b5ec9540406812c643f177317f779f 264464607 ;;
    medium) set -- ggml-medium-q8_0.bin 42a1ffcbe4167d224232443396968db4d02d4e8e87e213d3ee2e03095dea6502 823369779 ;;
    parakeet) set -- parakeet-unified-en-0.6b-Q8_0.gguf "" 731357568 ;;
    *) fail "Settings name an unknown model '$MODEL'; give one: isle-setup-voice small" ;;
esac
FILE="$1" SHA="$2" SIZE="$3"
# Silero, which finds where someone speaks so Whisper hears only that.
VAD_FILE=ggml-silero-v5.1.2.bin
VAD_SHA=29940d98d42b91fbd05ce489f3ecf7c72f0a42f027e4875919a28fb4c04ea2cf

command -v curl >/dev/null || fail "please install curl first."
command -v sha256sum >/dev/null || fail "please install sha256sum (coreutils) first."

# Whether file $1 is there with SHA-256 $2.
verified() { [ -f "$1" ] && [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$2" ]; }

# Downloads $1 to $MODELS/$2 unless it is there already, and checks it is the file published.
fetch() {
    local url="$1" file="$MODELS/$2" sha="$3"
    if verified "$file" "$sha"; then
        echo "Already downloaded: $file"
        return
    fi
    # A .part file is carried on from where an interrupted download stopped.
    curl -fL --retry 3 --progress-bar -C - -o "$file.part" "$url" ||
        curl -fL --retry 3 --progress-bar -o "$file.part" "$url" ||
        fail "could not download $url; check the internet connection and try again."
    if ! verified "$file.part" "$sha"; then
        rm -f "$file.part"
        fail "$2 did not download correctly (its SHA-256 differs); run isle-setup-voice again."
    fi
    mv -f "$file.part" "$file"
}

mkdir -p "$MODELS"
if [ "$MODEL" = parakeet ]; then
    # Handy already downloaded this to the Hugging Face cache: link it rather than download again.
    step "Setting up the 'parakeet' speech model in $MODELS"
    CACHE="$HUB/models--handy-computer--parakeet-unified-en-0.6b-gguf/snapshots"
    SOURCE="$(find "$CACHE" -maxdepth 2 -name "$FILE" 2>/dev/null | head -n 1 || true)"
    if [ -f "$MODELS/$FILE" ]; then
        echo "Already there: $MODELS/$FILE"
    elif [ -n "$SOURCE" ]; then
        ln -sf "$SOURCE" "$MODELS/$FILE"
        echo "Linked to Handy's copy: $SOURCE"
    else
        step "Downloading the 'parakeet' speech model ($((SIZE / 1000000)) MB) to $MODELS"
        curl -fL --retry 3 --progress-bar -C - -o "$MODELS/$FILE.part" "$PARAKEET_URL/$FILE" ||
            curl -fL --retry 3 --progress-bar -o "$MODELS/$FILE.part" "$PARAKEET_URL/$FILE" ||
            fail "could not download $PARAKEET_URL/$FILE; check the internet connection and try again."
        mv -f "$MODELS/$FILE.part" "$MODELS/$FILE"
    fi
else
    step "Downloading the '$MODEL' speech model ($((SIZE / 1000000)) MB) to $MODELS"
    fetch "$WHISPER_URL/$FILE" "$FILE" "$SHA"
    fetch "$VAD_URL/$VAD_FILE" "$VAD_FILE" "$VAD_SHA"
fi
# isled runs the model chosen in Settings, so a model named here becomes that choice.
if [ "$GIVEN" = 1 ]; then
    if { gsettings --schemadir "$SCHEMAS" set org.gnome.shell.extensions.isle voice-speech-model "$MODEL" ||
        gsettings set org.gnome.shell.extensions.isle voice-speech-model "$MODEL"; } 2>/dev/null; then
        echo "Settings → Assistant → Speech recognition is now '$MODEL'."
    else
        echo "Pick '$MODEL' in Settings → Assistant → Speech recognition so Isle uses it."
    fi
fi

# isle-parakeet (or isle-whisper) beside this script (both in ~/.local/bin), on the PATH, or built in the source folder.
HERE="$(dirname "$(readlink -f "$0")")"
WHISPER=""
for candidate in \
    "$HERE/isle-parakeet" "$(command -v isle-parakeet || true)" "$HERE/../target/release/isle-parakeet" \
    "$HERE/isle-whisper" "$(command -v isle-whisper || true)" "$HERE/../target/release/isle-whisper"; do
    if [ -n "$candidate" ] && [ -x "$candidate" ]; then
        WHISPER="$candidate"
        break
    fi
done
if [ -z "$WHISPER" ]; then
    echo "isle-whisper or isle-parakeet, which runs the model, is not installed: install Isle again" \
        "(from source they need cmake and a C++ compiler; see the README)."
fi

# The older setup, which isle-parakeet and isle-whisper replace: only offered once a helper is there to take over.
OLD=()
[ -d "$OLD_VENV" ] && OLD+=("$OLD_VENV")
[ -f "$OLD_HELPER" ] && OLD+=("$OLD_HELPER")
for size in tiny base small medium; do
    [ -d "$HUB/models--Systran--faster-whisper-$size" ] && OLD+=("$HUB/models--Systran--faster-whisper-$size")
done
if [ -n "$WHISPER" ] && [ "${#OLD[@]}" -gt 0 ]; then
    step "The older Python speech setup is no longer needed"
    # -L: the model files can be links into a shared cache.
    du -shL "${OLD[@]}" 2>/dev/null || true
    if [ "$REMOVE_OLD" = 0 ] && [ -t 0 ] && [ -t 1 ]; then
        read -r -p "Delete them? [y/N] " answer
        case "$answer" in [yY]*) REMOVE_OLD=1 ;; esac
    fi
    if [ "$REMOVE_OLD" = 1 ]; then
        for old in "${OLD[@]}"; do
            # A model file linked elsewhere in the cache goes too, not only the link.
            if [ -d "$old" ] && [ "${old#"$HUB"/}" != "$old" ]; then
                find "$old" -type l -print0 | while IFS= read -r -d '' link; do
                    target="$(readlink -f "$link" || true)"
                    case "$target" in "$HUB"/*) rm -f "$target" ;; esac
                done
            fi
            rm -rf "$old"
        done
        echo "Deleted."
    else
        echo "To delete them later: isle-setup-voice --remove-old"
    fi
fi

# What isled itself finds: the speech model, where it runs, and the AI it answers with.
step "Checking"
ISLED="$(command -v isled || echo "$HOME/.local/bin/isled")"
if [ -x "$ISLED" ]; then
    "$ISLED" --doctor
else
    echo "isled is not installed yet; install Isle, then run isled --doctor."
fi
echo "Done. Pick the AI in Settings → Assistant if you have more than one, then press Super+I and speak."
