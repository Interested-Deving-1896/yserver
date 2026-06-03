#!/usr/bin/env bash
# x11trace the Chromium (ANGLE-Vulkan) run against yserver, to capture the
# last X request/reply before Chromium's startup CHECK traps and drops the
# connection. Chromium is stripped (gdb gives no symbols), so the proven
# way to find these X11 CHECK bugs is to diff the final exchange against an
# Xorg capture (cf. the ListInputDevices / QueryTree fixes).
#
# x11trace tunnels a fake display :8 → real yserver on :7; Chromium connects
# to :8 so every request/event is logged. When Chromium traps, it closes the
# connection and x11trace stops — the tail of the trace is the trigger.
#
# Usage (run from a terminal on the yserver session, DISPLAY=:7):
#
#     tools/chromium-yserver-trace.sh [backend] [tracefile] [logfile]
#
#   backend     ANGLE backend (default: vulkan)
#   tracefile   x11trace output (default: chromium-vk.xtrace)
#   logfile     chromium stdout+stderr (default: chromium.log)

set -uo pipefail

BACKEND="${1:-vulkan}"
TRACEFILE="${2:-chromium-vk.xtrace}"
LOGFILE="${3:-chromium.log}"
REAL_DISPLAY="${DISPLAY:-:7}"
FAKE_DISPLAY=":8"

if ! command -v x11trace >/dev/null 2>&1; then
    echo "x11trace not installed" >&2
    exit 1
fi

BIN=""
for cand in /usr/lib/chromium/chromium /usr/lib64/chromium/chromium \
            "$(command -v chromium 2>/dev/null)" \
            "$(command -v google-chrome-stable 2>/dev/null)"; do
    if [[ -n "$cand" && -x "$cand" ]]; then BIN="$cand"; break; fi
done
[[ -z "$BIN" ]] && { echo "no chromium/chrome binary found" >&2; exit 1; }

PROFILE="$(mktemp -d /tmp/cr-trace.XXXXXX)"

echo "x11trace $REAL_DISPLAY → $FAKE_DISPLAY, chromium(--use-angle=$BACKEND) → $LOGFILE, trace → $TRACEFILE" >&2

# Start the proxy on :8 forwarding to the real yserver display.
x11trace -d "$REAL_DISPLAY" -D "$FAKE_DISPLAY" -n -o "$TRACEFILE" &
TRACE_PID=$!
cleanup() { kill "$TRACE_PID" 2>/dev/null; }
trap cleanup EXIT
sleep 1

CHROME_FLAGS=(
    --ozone-platform=x11
    --use-angle="$BACKEND"
    --ignore-gpu-blocklist
    --no-sandbox
    --disable-breakpad
    --in-process-gpu
    --enable-logging=stderr
    --v=1
    --vmodule="*x11*=2,*ozone*=2,*gl*=2,*angle*=2,gpu*=2"
    --user-data-dir="$PROFILE"
)

env DISPLAY="$FAKE_DISPLAY" GDK_BACKEND=x11 MESA_DEBUG=1 \
    "$BIN" "${CHROME_FLAGS[@]}" >"$LOGFILE" 2>&1

echo "exit=$? — trace=$TRACEFILE log=$LOGFILE profile=$PROFILE" >&2
echo "tail of trace:" >&2
tail -5 "$TRACEFILE" 2>/dev/null >&2
