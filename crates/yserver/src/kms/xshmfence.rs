//! Minimal FFI to `libxshmfence` — the shared-memory + futex
//! fence protocol Mesa's `loader_dri3` uses for `FenceFromFD`.
//!
//! Mesa's `xshmfence_alloc_shm` creates a memfd-backed shared
//! 4-byte counter; `xshmfence_share_fd` (now removed in favour of
//! the existing memfd) hands the fd to the X server via DRI3
//! `FenceFromFD`. Server side: `xshmfence_map_shm` mmaps the fd,
//! `xshmfence_trigger` writes 1 + futex_wake. Mesa's wait side
//! futexes on the same physical page — wakes immediately.
//!
//! Phase 4.2.3 design §3.4 expected `vkImportSemaphoreFdKHR` to
//! work, but Venus passthrough rejects the fd because it isn't a
//! sync_file. Falling back to xshmfence is what Xorg's `misync`
//! layer does internally.

use std::os::fd::{AsRawFd, BorrowedFd};

#[allow(non_camel_case_types)]
#[repr(C)]
pub struct xshmfence {
    _private: [u8; 0],
}

// SAFETY: The pointer is opaque — accesses go through xshmfence_*
// functions which are themselves thread-safe per the xshmfence
// contract (futex-backed, atomic counter).
unsafe impl Send for FenceMapping {}
unsafe impl Sync for FenceMapping {}

#[link(name = "xshmfence")]
unsafe extern "C" {
    fn xshmfence_map_shm(fd: i32) -> *mut xshmfence;
    fn xshmfence_unmap_shm(f: *mut xshmfence);
    fn xshmfence_trigger(f: *mut xshmfence) -> i32;
    fn xshmfence_query(f: *mut xshmfence) -> i32;
    fn xshmfence_reset(f: *mut xshmfence);
}

/// Mapped xshmfence — owns the mmap and the underlying fd is
/// duplicated by `xshmfence_map_shm` so we can drop the original.
pub struct FenceMapping {
    ptr: *mut xshmfence,
}

impl FenceMapping {
    /// Map a memfd-backed xshmfence. The fd is duplicated by
    /// `xshmfence_map_shm` internally; the caller's fd is unaffected.
    /// Returns `None` if the fd isn't an xshmfence (e.g. a real
    /// sync_file).
    pub fn map(fd: BorrowedFd<'_>) -> Option<Self> {
        // SAFETY: fd is a borrowed valid file descriptor; the
        // library duplicates it internally so the caller retains
        // ownership.
        let ptr = unsafe { xshmfence_map_shm(fd.as_raw_fd()) };
        if ptr.is_null() {
            None
        } else {
            Some(Self { ptr })
        }
    }

    /// Atomically trigger the fence, waking any process waiting on
    /// it via `xshmfence_await`.
    pub fn trigger(&self) -> i32 {
        // SAFETY: ptr is valid for the lifetime of self.
        unsafe { xshmfence_trigger(self.ptr) }
    }

    /// Reset the fence back to untriggered.
    #[allow(dead_code)]
    pub fn reset(&self) {
        unsafe { xshmfence_reset(self.ptr) };
    }

    /// Whether the fence has been triggered. Returns 0 if reset, 1
    /// if triggered.
    #[allow(dead_code)]
    pub fn query(&self) -> i32 {
        unsafe { xshmfence_query(self.ptr) }
    }
}

impl Drop for FenceMapping {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: we own the mapping until Drop fires once.
            unsafe { xshmfence_unmap_shm(self.ptr) };
        }
    }
}
