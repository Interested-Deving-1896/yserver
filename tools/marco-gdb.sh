#!/usr/bin/env bash
# Run marco under gdb against yserver and capture a backtrace on SEGV.
# Usage: DISPLAY=:7 tools/marco-gdb.sh
# Output: ./marco-gdb.txt (in cwd)

set -u

: "${DISPLAY:=:7}"
export DISPLAY

OUT="${OUT:-./marco-gdb.txt}"

: "${DEBUGINFOD_URLS:=https://debuginfod.archlinux.org https://debuginfod.cachyos.org}"
export DEBUGINFOD_URLS

exec gdb -q -batch \
    -ex 'set debuginfod enabled on' \
    -ex 'set debuginfod verbose 1' \
    -ex 'set pagination off' \
    -ex 'set print thread-events off' \
    -ex 'handle SIGPIPE nostop noprint pass' \
    -ex 'handle SIGUSR1 nostop noprint pass' \
    -ex run \
    -ex 'echo \n--- backtrace (all threads) ---\n' \
    -ex 'thread apply all bt full' \
    -ex 'echo \n--- registers ---\n' \
    -ex 'info registers' \
    -ex 'echo \n--- disasm at $pc ---\n' \
    -ex 'x/16i $pc' \
    -ex 'echo \n--- frame 1 (libX11 caller of NULL) ---\n' \
    -ex 'frame 1' \
    -ex 'info frame' \
    -ex 'info args' \
    -ex 'info locals' \
    -ex 'echo \n--- 16 instructions around the call site that targeted NULL ---\n' \
    -ex 'x/16i $pc-48' \
    -ex 'echo \n--- 8 instructions starting at $pc ---\n' \
    -ex 'x/8i $pc' \
    -ex 'echo \n--- addr2line for return address in frame 1 ---\n' \
    -ex 'printf "frame1_pc=%#x\n", $pc' \
    -ex 'shell true' \
    -ex 'echo \n--- info sharedlibrary ---\n' \
    -ex 'info sharedlibrary' \
    -ex quit \
    --args marco 2>&1 | tee "$OUT"
