KERNEL := "/boot/vmlinuz-linux-cachyos"

# Run yserver in virtme-ng with virtio-gpu DRM device and a QEMU window.
yserver:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --graphics --rw \
        -- target/debug/yserver

# Run yserver in virtme-ng headless; stdout/stderr reach the host terminal.
yserver-headless:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-device virtio-gpu-pci" \
        -- target/debug/yserver

# Run the nested ynest binary on the host's X server (no virtme).
ynest display="99":
    cargo run --bin ynest -- {{display}}

# Smoke-test virtme harness: bring up Xorg + xterm in a QEMU window.
harness-check:
    vng -r {{KERNEL}} --disable-microvm --graphics --rw \
        -- bash -c "Xorg :1 vt1 -logfile /tmp/xorg-test.log & sleep 5 && DISPLAY=:1 xterm"
