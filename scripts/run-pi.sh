#!/bin/bash
# Launcher for the GivEnergy Plant Simulator on Raspberry Pi / Linux desktops
# where the WebKitGTK webview (used by Tauri) sometimes renders incorrectly.
#
# Symptoms this works around:
#   - Garbled / corrupted display inside the app window (title bar fine)
#   - Window paints once then freezes
#   - Blank webview area on launch
#
# These are caused by WebKitGTK 4.1's compositor misbehaving with the Pi's
# GPU drivers on Raspberry Pi OS (Debian trixie). Forcing CPU compositing
# and disabling the DMABuf renderer reliably fixes it at the cost of some
# animation smoothness.
#
# Usage:
#   ./scripts/run-pi.sh                       # launches giv-sim (release build)
#   ./scripts/run-pi.sh cargo tauri dev       # launch dev mode with the same fixes
#   GIVSIM_FORCE_GPU=1 ./scripts/run-pi.sh    # opt back in to GPU compositing
#
# Pass any extra arguments to giv-sim (or, if the first arg is "cargo", to cargo).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# These are the WebKitGTK env vars that fix the garbled-display issue on Pi 4.
# Disabling compositing mode forces WebKit to render via the CPU instead of
# the GPU; the Pi's VideoCore driver has well-known edge cases that the
# WebKit GTK 4.1 compositor hits. Disabling the DMABuf renderer avoids a
# second set of Pi-specific rendering bugs.
export WEBKIT_DISABLE_COMPOSITING_MODE=1
export WEBKIT_DISABLE_DMABUF_RENDERER=1

# Allow opting back into GPU compositing if the user wants to test whether
# their Pi is one of the lucky ones (export GIVSIM_FORCE_GPU=1).
if [[ "${GIVSIM_FORCE_GPU:-0}" == "1" ]]; then
    unset WEBKIT_DISABLE_COMPOSITING_MODE
    unset WEBKIT_DISABLE_DMABUF_RENDERER
    echo "[run-pi] GIVSIM_FORCE_GPU=1 — using GPU compositing (may garble)"
fi

# Optional: force software GL as a last-resort fallback. Off by default
# because it makes the UI noticeably sluggish. Enable with
# GIVSIM_SOFTWARE_GL=1 if CPU compositing alone still glitches.
if [[ "${GIVSIM_SOFTWARE_GL:-0}" == "1" ]]; then
    export LIBGL_ALWAYS_SOFTWARE=1
    echo "[run-pi] GIVSIM_SOFTWARE_GL=1 — forcing software GL"
fi

echo "[run-pi] WebKit env: compositing=${WEBKIT_DISABLE_COMPOSITING_MODE:-off} dmabuf=${WEBKIT_DISABLE_DMABUF_RENDERER:-off}"

# Locate the binary. We launch the Tauri GUI (sim-tauri) by default, since
# that's the one whose WebKitGTK webview is affected by the Pi rendering
# bugs. Pass `cli` as the first arg to launch the giv-sim CLI instead.
# Finally let the user point us at one with GIVSIM_BIN.
if [[ "${1:-}" == "cargo" ]]; then
    cd "$PROJECT_DIR"
    exec cargo tauri dev "${@:2}"
elif [[ "${1:-}" == "cli" ]]; then
    shift
    BIN="$PROJECT_DIR/target/release/giv-sim"
    if [[ ! -x "$BIN" ]]; then
        echo "[run-pi] No release CLI binary found. Building..." >&2
        cd "$PROJECT_DIR"
        cargo build --release --bin giv-sim
    fi
    exec "$BIN" "$@"
elif [[ -n "${GIVSIM_BIN:-}" ]]; then
    exec "$GIVSIM_BIN" "$@"
elif [[ -x "$PROJECT_DIR/target/release/sim-tauri" ]]; then
    exec "$PROJECT_DIR/target/release/sim-tauri" "$@"
else
    echo "[run-pi] No release GUI binary found. Building..." >&2
    cd "$PROJECT_DIR"
    cargo build --release --bin sim-tauri
    exec "$PROJECT_DIR/target/release/sim-tauri" "$@"
fi