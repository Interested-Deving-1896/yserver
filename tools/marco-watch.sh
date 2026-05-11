#!/usr/bin/env bash
# Run marco under gdb with a watchpoint on dpy->lock_fns->lock_display
# to catch the moment something corrupts it.
# Usage: DISPLAY=:7 tools/marco-watch.sh
# Output: ./marco-watch.txt

set -u

: "${DISPLAY:=:7}"
export DISPLAY

: "${DEBUGINFOD_URLS:=https://debuginfod.archlinux.org https://debuginfod.cachyos.org}"
export DEBUGINFOD_URLS

OUT="${OUT:-./marco-watch.txt}"
SCRIPT="$(dirname "$(readlink -f "$0")")/marco-gdb-watch.gdb"

exec gdb -q -batch -x "$SCRIPT" --args marco 2>&1 | tee "$OUT"
