//! GLX texture-from-pixmap: exportable-image allocation + dma-buf export.
//!
//! Vulkan-gated — run with:
//!   cargo test --test glx_tfp_export -- --ignored

#![cfg(target_os = "linux")]

use yserver::kms::vk::device::VkContext;

#[test]
#[ignore = "requires a Vulkan device"]
fn allocate_exportable_yields_valid_dmabuf_fd() {
    let vk = VkContext::new().expect("VkContext init failed — install lavapipe or run on HW");

    let img = yserver::kms::vk::target::allocate_exportable(
        &vk,
        /* width */ 64,
        /* height */ 32,
        yserver::kms::vk::target::EXPORT_FORMAT_BGRA8,
    )
    .expect("allocate exportable image");

    // Stride/size from vkGetImageSubresourceLayout must be sane.
    assert!(
        img.stride >= 64 * 4,
        "stride {} too small for 64px BGRA8 row",
        img.stride
    );
    assert!(
        img.size as usize >= img.stride as usize * 32,
        "size {} too small for {}*32",
        img.size,
        img.stride
    );

    // Export must succeed and carry sane stride/size.
    let export = yserver::kms::vk::dri3::export_backing(&vk, &img).expect("export_backing failed");
    assert!(
        export.stride >= 64 * 4,
        "export stride {} too small",
        export.stride
    );
    assert!(
        export.size >= export.stride * 32,
        "export size {} too small for stride {} * 32 rows",
        export.size,
        export.stride
    );
}
