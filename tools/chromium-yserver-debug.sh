#!/usr/bin/env bash
# Launch Chromium against yserver with full Mesa/EGL/ANGLE debug logging,
# to diagnose the client-side "failed to create drawable" /
# "Could not create the initialization pbuffer" GL-init failure.
#
# Usage (run from inside a wezterm/terminal on the yserver session, so
# DISPLAY is already :7):
#
#     tools/chromium-yserver-debug.sh [logfile] [-- extra chromium args]
#
#   logfile   where to capture combined stdout+stderr (default: chromium.log)
#
# Everything the GL stack prints goes to stderr, so the single redirect
# captures the Mesa loader_dri3 trace (where "failed to create drawable"
# originates), the EGL driver messages, and Chrome's own GL logging.
#
# After it runs, the failing DRI call + reason will be in the logfile;
# grep for "failed to create drawable", "dri3", "EGL", "ANGLE".

set -uo pipefail

LOGFILE="chromium.log"
if [[ $# -gt 0 && "$1" != "--" ]]; then
    LOGFILE="$1"
    shift
fi
[[ "${1:-}" == "--" ]] && shift
EXTRA_ARGS=("$@")

# Resolve a chromium/chrome binary.
BIN=""
for cand in chromium google-chrome-stable google-chrome chrome; do
    if command -v "$cand" >/dev/null 2>&1; then
        BIN="$cand"
        break
    fi
done
if [[ -z "$BIN" ]]; then
    echo "no chromium/chrome binary found on PATH" >&2
    exit 1
fi

if [[ -z "${DISPLAY:-}" ]]; then
    echo "DISPLAY is unset — run this from a terminal on the yserver session (e.g. wezterm on :7)" >&2
    exit 1
fi

echo "launching $BIN on DISPLAY=$DISPLAY → $LOGFILE" >&2

# --- GL-stack debug environment ---------------------------------------
# Mesa: verbose loader (DRI3 drawable creation), driver debug, GLX/EGL.
export MESA_DEBUG=1            # Mesa driver debug messages
export LIBGL_DEBUG=verbose     # classic libGL / loader_dri3 path tracing
export EGL_LOG_LEVEL=debug     # Mesa EGL (eglInitialize / surface creation)
export MESA_LOADER_DRIVER_OVERRIDE="${MESA_LOADER_DRIVER_OVERRIDE:-}"  # leave as-is unless set
# ANGLE (Chrome's GL layer) extra diagnostics.
export ANGLE_DEFAULT_PLATFORM="${ANGLE_DEFAULT_PLATFORM:-}"
# Force X11 (Ozone) and GTK X11 backend.
export GDK_BACKEND=x11
export -n WAYLAND_DISPLAY WAYLAND_SOCKET 2>/dev/null || true

# --- Chromium flags ----------------------------------------------------
# Force the X11 ozone backend, keep the GPU process on (don't let it fall
# straight to software), and turn on verbose GL/EGL/ANGLE vlogging to
# stderr so the failure point is captured alongside the Mesa output.
CHROME_FLAGS=(
    --ozone-platform=x11
    --ignore-gpu-blocklist
    --enable-logging=stderr
    --v=1
    --vmodule="*gl*=3,*egl*=3,*angle*=3,*dri*=3,gpu*=2"
    # Sandbox blocks /dev/dri access in some setups; disabling it isolates
    # whether the failure is a sandbox denial vs a real DRI/Mesa error.
    --disable-gpu-sandbox
    # Use a throwaway profile so a running Chrome instance doesn't capture
    # this launch (which would lose our logging).
    --user-data-dir="$(mktemp -d /tmp/chromium-yserver-debug.XXXXXX)"
)

env DISPLAY="$DISPLAY" \
    MESA_DEBUG="$MESA_DEBUG" \
    LIBGL_DEBUG="$LIBGL_DEBUG" \
    EGL_LOG_LEVEL="$EGL_LOG_LEVEL" \
    GDK_BACKEND=x11 \
    "$BIN" "${CHROME_FLAGS[@]}" "${EXTRA_ARGS[@]}" >"$LOGFILE" 2>&1

echo "exit=$? — see $LOGFILE" >&2
