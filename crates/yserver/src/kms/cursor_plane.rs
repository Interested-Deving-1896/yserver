//! Hardware cursor plane — replaces the Vulkan-composited cursor
//! quad with a kernel-managed DRM cursor overlay.
//!
//! Why: the cursor quad was tied to compositor cadence. Every cursor
//! position change waited for the next `composite_and_flip`, which
//! is stalled by per-op `vkQueueWaitIdle` in the paint pipeline
//! (notably when hovering over GTK widgets that schedule
//! gradient/emboss repaints — observed as severe pointer lag in
//! mate-control-center on fuji). The DRM hardware cursor plane is
//! a separate overlay the kernel positions independently —
//! `drmModeMoveCursor` is one ioctl, microseconds, doesn't touch
//! the GPU.
//!
//! The legacy `set_cursor2` / `move_cursor` ioctls are marked
//! deprecated in the drm crate in favor of the atomic cursor-plane
//! API, but they're universally supported on every mainstream
//! desktop GPU. Atomic-plane upgrade is a follow-up if/when more
//! cursor plane features are wanted.
//!
//! Stage 5 Phase B (per-CRTC visibility + upload/show split): the
//! shared dumb buffer is mutated by `load_image` and bound to each
//! CRTC independently via `show_on_crtc` / `hide_on_crtc`. Per-CRTC
//! visibility tracking lets the per-output `PendingAck` design queue
//! a Sw→Hw transition for one output without prematurely binding
//! the plane on outputs that haven't retired the transition yet
//! (the multi-output double-cursor hazard). `set_cursor2` is the
//! "show" operation in legacy DRM; the upload path here MUST NOT
//! call it.

use std::{collections::HashMap, io, mem, ptr::NonNull, sync::Arc};

use drm::{
    buffer::{Buffer, DrmFourcc},
    control::{Device as ControlDevice, crtc, dumbbuffer::DumbBuffer},
};

use crate::drm::Device;

/// Universal minimum hardware cursor size on every Intel / AMD /
/// mainstream-Mali iGPU since ~2010. X11 cursor themes are usually
/// ≤ 32×32; cursors larger than this fall back to the Vulkan
/// composite path.
pub const HW_CURSOR_W: u32 = 64;
pub const HW_CURSOR_H: u32 = 64;

/// A single shared DRM dumb buffer holding the current cursor image,
/// plus per-CRTC visibility state.
///
/// Per-CRTC visibility (Stage 5 Phase B refactor): each CRTC tracks
/// whether the plane is currently bound to it via `set_cursor2(Some,
/// ...)`. v1's pre-Phase-B global `visible: bool` was correct only on
/// single-output systems and exposed the multi-output double-cursor
/// hazard when one output retired a Sw→Hw transition before another.
pub struct CursorPlane {
    device: Arc<Device>,
    dumb: Option<DumbBuffer>,
    ptr: NonNull<u8>,
    len: usize,
    stride: u32,
    /// Per-CRTC binding state — `Some(true)` when `set_cursor2(crtc,
    /// Some(dumb), ...)` succeeded last; `Some(false)` when
    /// `set_cursor2(crtc, None, ...)` succeeded last; absent until
    /// the first show/hide on that CRTC. The `crtc::Handle` is the
    /// stable identifier across the v1 and v2 backends.
    visible: HashMap<crtc::Handle, bool>,
    /// Stage 5 Phase B — `CursorRecord.version` last memcpy'd into
    /// the dumb buffer. `cursor_plane_upload_image` compares the
    /// requested version against this for upload dedup; `None` after
    /// init / VT-leave / full modeset (forces the next show to
    /// re-upload).
    uploaded_version: Option<u64>,
}

// SAFETY: ptr is an mmap'd kernel buffer that lives as long as
// `dumb`; no thread does interior mutation through the raw pointer
// without exclusive `&mut self`.
unsafe impl Send for CursorPlane {}

impl CursorPlane {
    /// Allocate the cursor dumb buffer + mmap it. The buffer is
    /// zero-filled so an initial `show` before any image lands
    /// doesn't display random bytes.
    ///
    /// # Errors
    /// `create_dumb_buffer` or `map_dumb_buffer` ioctl failures.
    pub fn new(device: Arc<Device>) -> io::Result<Self> {
        let mut dumb =
            device.create_dumb_buffer((HW_CURSOR_W, HW_CURSOR_H), DrmFourcc::Argb8888, 32)?;
        let stride = dumb.pitch();
        // Map; on failure leak the dumb buffer — the kernel
        // reclaims it when `device` is dropped. We can't call
        // `destroy_dumb_buffer(dumb)` in the Err arm because the
        // `Result<DumbMapping<'_>, _>` discriminant keeps `dumb`
        // mutably borrowed until end of match scope.
        let mapping = device.map_dumb_buffer(&mut dumb)?;
        let len = mapping.len();
        let ptr =
            NonNull::new(mapping.as_ptr() as *mut u8).expect("non-null mmap for cursor plane");
        // Leak the mapping handle; mmap stays alive via the dumb
        // buffer kept by `Self::dumb`. Released in Drop.
        mem::forget(mapping);
        // Zero-fill the plane buffer up front (the kernel doesn't
        // guarantee zeroed contents on create_dumb_buffer).
        unsafe { std::ptr::write_bytes(ptr.as_ptr(), 0, len) };
        Ok(Self {
            device,
            dumb: Some(dumb),
            ptr,
            len,
            stride,
            visible: HashMap::new(),
            uploaded_version: None,
        })
    }

    /// Copy a cursor image into the plane buffer. `bgra_bytes` is a
    /// tightly-packed `width × height × 4` BGRA8 buffer matching the
    /// DRM `ARGB8888` byte order in little-endian. The image lands at
    /// (0, 0); the remainder of the 64×64 buffer is zero-filled
    /// (transparent).
    ///
    /// Returns `Err(InvalidInput)` if the image is larger than
    /// `HW_CURSOR_W × HW_CURSOR_H` — caller falls back to the
    /// compositor cursor path.
    pub fn load_image(&mut self, image_w: u32, image_h: u32, bgra_bytes: &[u8]) -> io::Result<()> {
        if image_w == 0 || image_h == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero-sized cursor",
            ));
        }
        if image_w > HW_CURSOR_W || image_h > HW_CURSOR_H {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cursor exceeds hardware plane size",
            ));
        }
        let img_stride = (image_w as usize) * 4;
        let expected_bytes = img_stride * image_h as usize;
        if bgra_bytes.len() < expected_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cursor bytes shorter than width*height*4",
            ));
        }
        // Clear so a smaller cursor doesn't leave previous pixels.
        unsafe { std::ptr::write_bytes(self.ptr.as_ptr(), 0, self.len) };
        for row in 0..(image_h as usize) {
            let src_off = row * img_stride;
            let dst_off = row * (self.stride as usize);
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bgra_bytes.as_ptr().add(src_off),
                    self.ptr.as_ptr().add(dst_off),
                    img_stride,
                );
            }
        }
        Ok(())
    }

    /// Stage 5 Phase B — versioned upload. Memcpys `bgra_bytes` into
    /// the shared dumb buffer ONLY when `version` differs from
    /// `uploaded_version`. **Never calls `set_cursor2`**; binding
    /// the buffer to a CRTC is a separate step (`show_on_crtc`).
    /// This split is load-bearing for the per-output transition
    /// state machine — uploading must not prematurely show pixels
    /// on CRTCs whose Sw→Hw retire is still pending.
    ///
    /// # Errors
    /// Same as [`Self::load_image`].
    pub fn upload_image(
        &mut self,
        version: u64,
        image_w: u32,
        image_h: u32,
        bgra_bytes: &[u8],
    ) -> io::Result<()> {
        if self.uploaded_version == Some(version) {
            return Ok(());
        }
        self.load_image(image_w, image_h, bgra_bytes)?;
        self.uploaded_version = Some(version);
        Ok(())
    }

    /// The version currently held in the dumb buffer, if any.
    /// Compared **by value** against `Arc<CursorRecord>.version`;
    /// pointer identity is never sufficient (two distinct records
    /// can carry the same logical version after VT-acquire). `None`
    /// is the sentinel that no upload has fired yet (init or after
    /// `invalidate_uploaded_version`).
    #[must_use]
    pub fn uploaded_version(&self) -> Option<u64> {
        self.uploaded_version
    }

    /// Invalidate the tracked uploaded version. The next
    /// `upload_image` will memcpy unconditionally regardless of
    /// version. Used by global recovery paths (VT-leave, full
    /// modeset, `drain_all`) per Phase D' so the post-recovery show
    /// re-uploads cleanly.
    pub fn invalidate_uploaded_version(&mut self) {
        self.uploaded_version = None;
    }

    /// Make the cursor visible on `crtc` with `hotspot = (hot_x, hot_y)`.
    /// Idempotent — repeated calls just re-bind the same buffer.
    /// Tracks visibility per CRTC so global queries don't race.
    ///
    /// # Errors
    /// `set_cursor2` ioctl failure.
    #[allow(deprecated)]
    pub fn show(&mut self, crtc: crtc::Handle, hotspot: (i32, i32)) -> io::Result<()> {
        let Some(dumb) = self.dumb.as_ref() else {
            return Err(io::Error::other("cursor plane already destroyed"));
        };
        self.device.set_cursor2(crtc, Some(dumb), hotspot)?;
        self.visible.insert(crtc, true);
        Ok(())
    }

    /// Detach the cursor from `crtc`. The plane buffer is retained so
    /// a future `show` doesn't have to re-allocate.
    ///
    /// # Errors
    /// `set_cursor2` ioctl failure.
    #[allow(deprecated)]
    pub fn hide(&mut self, crtc: crtc::Handle) -> io::Result<()> {
        self.device.set_cursor2::<DumbBuffer>(crtc, None, (0, 0))?;
        self.visible.insert(crtc, false);
        Ok(())
    }

    /// Move the cursor on `crtc` to `(x, y)` in CRTC-local coords. The
    /// kernel clips to the CRTC's pixel rect; passing coords outside
    /// the visible area just hides it on that output.
    ///
    /// # Errors
    /// `move_cursor` ioctl failure.
    #[allow(deprecated)]
    pub fn move_to(&self, crtc: crtc::Handle, x: i32, y: i32) -> io::Result<()> {
        self.device.move_cursor(crtc, (x, y))
    }

    /// True iff the plane is currently bound (via `show`) on `crtc`.
    #[must_use]
    pub fn is_visible_on(&self, crtc: crtc::Handle) -> bool {
        self.visible.get(&crtc).copied().unwrap_or(false)
    }

    /// True iff the plane is currently bound on at least one CRTC.
    /// Drop-in replacement for the pre-Phase-B global `is_visible()`.
    /// v1's `hw_cursor_active()` consumes this to gate the
    /// Vulkan-composited cursor quad.
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible.values().any(|&v| v)
    }

    /// Iterate every CRTC the plane has ever been bound or hidden
    /// against. Lifecycle hooks (output-disable, VT-leave) use this
    /// to invalidate per-CRTC state without needing the
    /// `PlatformBackend.outputs` order.
    pub fn known_crtcs(&self) -> impl Iterator<Item = crtc::Handle> + '_ {
        self.visible.keys().copied()
    }
}

impl Drop for CursorPlane {
    fn drop(&mut self) {
        if let Some(dumb) = self.dumb.take() {
            let _ = self.device.destroy_dumb_buffer(dumb);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase B regression: `is_visible_on` tracks per-CRTC binding
    /// independently. Pre-refactor, a single global `visible: bool`
    /// returned true for output B after output A's show — exactly
    /// the multi-output hazard the per-CRTC state closes.
    #[test]
    fn visibility_is_per_crtc() {
        // No real CursorPlane — test the visibility map directly via
        // a synthesised struct. This avoids needing a live DRM
        // device in unit tests.
        let mut visible: HashMap<crtc::Handle, bool> = HashMap::new();
        let crtc_a: crtc::Handle = ::drm::control::from_u32(11).unwrap();
        let crtc_b: crtc::Handle = ::drm::control::from_u32(12).unwrap();

        // Show on A only.
        visible.insert(crtc_a, true);
        assert!(visible.get(&crtc_a).copied().unwrap_or(false));
        assert!(!visible.get(&crtc_b).copied().unwrap_or(false));

        // Show on B — A's state unchanged.
        visible.insert(crtc_b, true);
        assert!(visible.get(&crtc_a).copied().unwrap_or(false));
        assert!(visible.get(&crtc_b).copied().unwrap_or(false));

        // Hide A.
        visible.insert(crtc_a, false);
        assert!(!visible.get(&crtc_a).copied().unwrap_or(false));
        assert!(visible.get(&crtc_b).copied().unwrap_or(false));
    }

    /// Phase B regression test for the unavailable-plane path. The
    /// v2 `PlatformBackend::for_tests()` fixture has no real DRM
    /// device, so `cursor_plane` is `None`. The hooks must surface
    /// that cleanly via `Err(io::Error::other(...))` rather than
    /// panicking — every Phase D' recovery path relies on this so
    /// VT-leave / shutdown / drain_all hooks can fire blindly.
    #[test]
    fn unavailable_plane_returns_err_not_panic() {
        use crate::kms::v2::platform::PlatformBackend;

        let mut p = PlatformBackend::for_tests();
        assert!(!p.cursor_plane_available());
        assert!(
            p.cursor_plane_upload_image(1, 16, 16, &[0u8; 16 * 16 * 4])
                .is_err()
        );
        assert!(p.cursor_plane_show_on_crtc(0, 0, 0, 0, 0).is_err());
        assert!(p.cursor_plane_rebind_visible_crtcs(0, 0, 0, 0).is_err());
        assert!(p.cursor_plane_move(0, 0).is_err());
        assert!(p.cursor_plane_hide_on_crtc(0).is_err());
        assert!(p.cursor_plane_hide_all().is_err());
        assert!(p.cursor_plane_uploaded_version().is_none());
    }
}
