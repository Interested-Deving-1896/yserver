//! DRI3 fd-leak harness — Phase 4.2 design §5.4.
//!
//! 10k iterations of (allocate dma-buf → import as DrawableImage →
//! drop). Asserts `/proc/self/fd` count returns to baseline after
//! the loop. Catches any slip in the §3.2 fd-ownership rule (close
//! on every error path between `dup` and successful `vkAllocateMemory`).
//!
//! Marked `#[ignore]` because it needs a working Vulkan ICD (lavapipe
//! suffices). Run with `cargo test -p yserver --test dri3_fd_leak --
//! --ignored` under vng or on bare metal.

#![cfg(target_os = "linux")]

use std::fs;

fn fd_count() -> usize {
    fs::read_dir("/proc/self/fd")
        .map(|d| d.count())
        .unwrap_or(0)
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn dri3_import_loop_does_not_leak_fds() {
    use ash::vk;
    use yserver::kms::vk::{device::VkContext, dri3::DRM_FORMAT_MOD_LINEAR, target::DrawableImage};

    let vk = VkContext::new().expect("VkContext init failed — install lavapipe or run under vng");

    let baseline = fd_count();
    let iterations = 10_000usize;
    let mut peak = baseline;

    for _ in 0..iterations {
        // Self-allocate a dma-buf-exportable VkImage and re-import it
        // via DrawableImage::from_dmabuf. Both legs run on the same
        // VkContext (Venus would normally split them across two
        // contexts but the leak harness only cares about fd accounting
        // on the import side).
        let exporter = create_dmabuf_export(&vk, 64, 64).expect("export image");
        let drawable = DrawableImage::from_dmabuf(
            vk.clone(),
            exporter.fd,
            64,
            64,
            vk::Format::B8G8R8A8_UNORM,
            DRM_FORMAT_MOD_LINEAR,
            &[exporter.offset],
            &[exporter.pitch],
        )
        .expect("import");
        drop(drawable);

        let now = fd_count();
        if now > peak {
            peak = now;
        }
    }

    let after = fd_count();
    assert!(
        after <= baseline.saturating_add(8),
        "fd count grew: baseline={baseline}, after={after}, peak={peak}"
    );

    // Ignore peak in the assertion — the test is about leak,
    // not about transient growth during a loop iteration.
    let _ = peak;

    // Ignore the test scaffolding helpers when this test is built
    // outside the harness; suppresses dead-code lints in the no-Vulkan
    // configuration.
    let _: fn(&_, _, _) -> _ = create_dmabuf_export;
}

#[cfg_attr(not(test), allow(dead_code))]
struct ExportImage {
    fd: std::os::fd::OwnedFd,
    offset: u64,
    pitch: u32,
}

#[cfg_attr(not(test), allow(dead_code))]
fn create_dmabuf_export(
    _vk: &std::sync::Arc<yserver::kms::vk::device::VkContext>,
    _width: u32,
    _height: u32,
) -> Result<ExportImage, String> {
    // Real exporter implementation tracks scanout.rs's
    // allocate_vk_scanout_image but with TILING_LINEAR + a cleaner
    // standalone API. Phase 4.2 first cut leaves this as a stub the
    // ignored test refers to so the harness compiles; the live run is
    // gated on writing this helper for the vng leg of the matrix.
    Err("dri3_fd_leak: create_dmabuf_export helper not yet implemented".to_string())
}
