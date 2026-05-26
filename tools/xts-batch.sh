#!/usr/bin/env bash
# One-off driver: run a list of xts scenarios against yserver (KMS) in
# vng with the egl-headless display config, capture each summary tally,
# and flag any crash/hang. Results aggregated to /tmp/xts-batch-results.txt.
set -u

KERNEL=/boot/vmlinuz-linux-cachyos
OPTS="-display egl-headless,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true -device virtio-tablet-pci -device virtio-keyboard-pci"
REP=$(find /home/jos/Projects/xts -path '*/bin/reports/xts-report' -executable 2>/dev/null | head -1)
OUT=/tmp/xts-batch-results.txt
INNER=900      # xts per-scenario timeout (s)
OUTER=1100     # hard cap on the vng invocation (s)
: > "$OUT"

cd /home/jos/Projects/yserver
cargo build --release --bin yserver 2>&1 | tail -1

for scen in "$@"; do
    echo "=== $(date +%H:%M:%S) running $scen ===" | tee -a "$OUT"
    log=/tmp/xts-batch-$scen.log
    timeout "$OUTER" vng -r "$KERNEL" --disable-microvm --rw \
        --qemu-opts="$OPTS" \
        -- tools/yserver-vng-run.sh xts "$scen" "$INNER" >"$log" 2>&1
    rc=$?
    d=$(ls -1dt /home/jos/Projects/xts/results/*/ 2>/dev/null | head -1)
    tally=$("$REP" -d2 -f "${d}journal" 2>/dev/null | grep -E "^${scen}[[:space:]]" | head -1)
    # crash/hang signals: panic in the captured log, or outer-timeout (124).
    crash=$(grep -ciE "thread '.*' panicked|RUST_BACKTRACE" "$log" 2>/dev/null)
    hangnote=""
    [ "$rc" = "124" ] && hangnote=" [OUTER-TIMEOUT/possible-hang]"
    [ "$crash" != "0" ] && hangnote="$hangnote [PANIC x$crash]"
    echo "  ${tally:-<no summary>}${hangnote}" | tee -a "$OUT"
done
echo "=== BATCH DONE $(date +%H:%M:%S) ===" | tee -a "$OUT"
