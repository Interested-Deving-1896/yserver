#!/usr/bin/env bash
# Reproduce-or-acquit test: run marco against a regular Xephyr nested
# X server (not yserver). If marco crashes here too, the bug is in
# marco's environment (libX11/libXRes/host distro). If marco runs
# fine here but crashes against yserver, the bug is in yserver's wire
# protocol.
#
# Usage: tools/marco-xephyr.sh
# Outputs:
#   ./marco-xephyr.txt    — marco's gdb output
#   ./xephyr.log          — Xephyr's stderr
#
# Expects Xephyr installed (`pacman -S xorg-server-xephyr` on Arch).

set -u

NEST_DISPLAY=":18"
XEPHYR_LOG="./xephyr.log"
OUT="./marco-xephyr.txt"

: "${DEBUGINFOD_URLS:=https://debuginfod.archlinux.org https://debuginfod.cachyos.org}"
export DEBUGINFOD_URLS

# Sanity: Xephyr present?
if ! command -v Xephyr >/dev/null; then
    echo "Xephyr not installed — pacman -S xorg-server-xephyr" >&2
    exit 1
fi

# Make sure host DISPLAY is set (Xephyr needs an outer X server).
if [[ -z "${DISPLAY:-}" ]]; then
    echo "Need a host DISPLAY set (i.e. run from a graphical session)" >&2
    exit 1
fi

echo "outer DISPLAY=$DISPLAY  nested DISPLAY=$NEST_DISPLAY"

# Start Xephyr in the background.
Xephyr -screen 1600x900 -title "marco-test-xephyr" "$NEST_DISPLAY" \
    >"$XEPHYR_LOG" 2>&1 &
XEPHYR_PID=$!
trap 'kill -TERM "$XEPHYR_PID" 2>/dev/null || true; wait "$XEPHYR_PID" 2>/dev/null || true' EXIT

# Wait until Xephyr's socket is listening (poll up to ~5s).
for _ in $(seq 1 50); do
    if [[ -S "/tmp/.X11-unix/X${NEST_DISPLAY#:}" ]]; then break; fi
    sleep 0.1
done
if [[ ! -S "/tmp/.X11-unix/X${NEST_DISPLAY#:}" ]]; then
    echo "Xephyr socket /tmp/.X11-unix/X${NEST_DISPLAY#:} never appeared" >&2
    cat "$XEPHYR_LOG" >&2
    exit 2
fi

# Run marco under gdb against Xephyr.
echo "spawning marco against $NEST_DISPLAY (gdb output → $OUT)"
DISPLAY="$NEST_DISPLAY" gdb -q -batch \
    -ex 'set confirm off' \
    -ex 'set pagination off' \
    -ex 'set print thread-events off' \
    -ex 'set debuginfod enabled on' \
    -ex 'handle SIGPIPE nostop noprint pass' \
    -ex 'handle SIGUSR1 nostop noprint pass' \
    -ex run \
    -ex 'echo \n--- thread bt ---\n' \
    -ex 'thread apply all bt 25' \
    -ex 'echo \n--- registers ---\n' \
    -ex 'info registers' \
    -ex 'echo \n--- disasm at $pc ---\n' \
    -ex 'x/16i $pc' \
    -ex quit \
    --args marco 2>&1 | tee "$OUT"

echo
echo "Xephyr log tail:"
tail -20 "$XEPHYR_LOG"
