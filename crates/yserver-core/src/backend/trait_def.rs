//! `Backend` trait — the surface that `nested.rs` calls on the host
//! during request dispatch. Exists primarily as a seam for testing
//! (`RecordingBackend` lives next door, gated `#[cfg(test)]`) and so
//! that Phase 6.3+ can land a KMS backend without touching every call
//! site in `nested.rs`.
//!
//! Method signatures mirror the existing `HostX11Backend::*` methods
//! 1:1 — no `Param` structs, no parameter renaming. The pragmatic
//! guidance for Step 5 is that the trait surface follows existing
//! signatures rather than the plan's draft shape; bundling parameters
//! into structs would cascade into churn at every call site for no
//! gain. Several methods still take raw `u32` host xids rather than
//! handle newtypes for the same reason — call sites pass `u32` from
//! the `ResourceTable`'s `host_xid` field, and rewrapping/unwrapping
//! at every call boundary is noise.
//!
//! `register_event_sink` and the corresponding `BackendEventSink`
//! trait are intentionally omitted from this iteration. The current
//! pump-routing path (`HostInputPump` thread + `pointer_event_fanout`
//! / `expose_event_fanout`) is its own concern and folding it into a
//! sink trait is Step 5.5/6.x work — see the Step 5 task description.

use std::io;

use yserver_protocol::x11::{ClipRectangles, FontMetrics, xfixes};

use crate::{
    backend::{
        AnyHandle, ClipState, CursorHandle, DrawState, FillState, FontHandle, GlyphSetHandle,
        PictureHandle, PixmapHandle, WindowHandle,
    },
    host_x11::{HostSubwindowConfig, HostSubwindowVisual, PointerPosition},
};

/// The dynamic backend surface. `Send` is required so that
/// `Arc<Mutex<dyn Backend>>` is `Send + Sync` (`Mutex<T>` is Sync iff
/// `T: Send`). `Sync` on the trait itself is not required because all
/// `Backend` access is mediated through a `Mutex`.
pub trait Backend: Send {
    // ──────────────────────────────────────────────────────────────
    // Lifecycle / state accessors
    // ──────────────────────────────────────────────────────────────

    fn window_id(&self) -> u32;
    fn root_visual_xid(&self) -> u32;
    fn argb_visual_xid(&self) -> Option<u32>;
    fn argb_colormap_xid(&self) -> Option<u32>;
    fn render_opcode(&self) -> Option<u8>;
    fn xkb_opcode(&self) -> Option<u8>;
    fn xkb_info(&self) -> Option<(u8, u8, u8)>;
    fn composite_opcode(&self) -> Option<u8>;
    fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32>;
    fn ping(&mut self) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Subwindow lifecycle
    // ──────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn create_subwindow(
        &mut self,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle>;

    fn destroy_subwindow(&mut self, host_xid: u32) -> io::Result<()>;

    fn map_subwindow(&mut self, host_xid: u32) -> io::Result<()>;

    fn unmap_subwindow(&mut self, host_xid: u32) -> io::Result<()>;

    fn configure_subwindow(&mut self, host_xid: u32, config: HostSubwindowConfig)
    -> io::Result<()>;

    fn reparent_subwindow(
        &mut self,
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()>;

    fn change_subwindow_attributes(
        &mut self,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()>;

    fn name_window_pixmap(&mut self, host_window: WindowHandle) -> io::Result<PixmapHandle>;

    // ──────────────────────────────────────────────────────────────
    // Resources (pixmap, font, cursor)
    // ──────────────────────────────────────────────────────────────

    fn create_pixmap(&mut self, depth: u8, width: u16, height: u16) -> io::Result<PixmapHandle>;

    fn free_pixmap(&mut self, host_xid: u32) -> io::Result<()>;

    fn open_font(&mut self, name: &str) -> io::Result<(FontHandle, FontMetrics)>;

    fn close_font(&mut self, host_xid: u32) -> io::Result<()>;

    fn create_cursor(
        &mut self,
        source_pixmap: PixmapHandle,
        mask_pixmap: Option<PixmapHandle>,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorHandle>;

    fn define_cursor(&mut self, host_window_xid: u32, cursor_host_xid: u32) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Container background (root-mapped helpers)
    // ──────────────────────────────────────────────────────────────

    fn set_container_background_pixel(&mut self, pixel: u32) -> io::Result<()>;

    fn set_container_background_pixmap(&mut self, host_pixmap_xid: u32) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // GC state (sync points feeding the host's shared GC)
    // ──────────────────────────────────────────────────────────────

    fn clear_clip_rectangles(&mut self) -> io::Result<()>;

    fn set_clip_rectangles(&mut self, clip: Option<ClipRectangles>) -> io::Result<()>;

    fn set_clip_pixmap(
        &mut self,
        host_pixmap: u32,
        clip_x_origin: i16,
        clip_y_origin: i16,
    ) -> io::Result<()>;

    fn set_gc_fill_solid(&mut self) -> io::Result<()>;

    fn set_gc_fill_tiled(
        &mut self,
        host_pixmap: u32,
        tile_x_origin: i16,
        tile_y_origin: i16,
    ) -> io::Result<()>;

    fn apply_clip_state(&mut self, clip: &ClipState) -> io::Result<()>;

    fn apply_fill_state(&mut self, fill: &FillState) -> io::Result<()>;

    fn apply_draw_state(&mut self, state: &DrawState) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Drawing primitives
    //
    // These match the existing `HostX11Backend::*` signatures: they
    // take raw `u32` host xids and a foreground colour (the Phase 6.2
    // additive scope adds `&DrawState` propagation through the
    // `apply_draw_state` sync point above; the methods themselves are
    // unchanged). Phase 6.3+ may collapse the surface as the trait
    // grows additional impls, but for now matching the existing shape
    // keeps Step 5 churn-free at every call site.
    // ──────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn copy_area(
        &mut self,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn copy_plane(
        &mut self,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
        plane: u32,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn put_image(
        &mut self,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn get_image(
        &mut self,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>>;

    fn poly_line(
        &mut self,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()>;

    fn poly_segment(&mut self, host_xid: u32, foreground: u32, segments: &[u8]) -> io::Result<()>;

    fn poly_rectangle(
        &mut self,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()>;

    fn poly_arc(&mut self, host_xid: u32, foreground: u32, arcs: &[u8]) -> io::Result<()>;

    fn poly_point(
        &mut self,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()>;

    fn poly_fill_rectangle(
        &mut self,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()>;

    fn poly_fill_arc(&mut self, host_xid: u32, foreground: u32, arcs: &[u8]) -> io::Result<()>;

    fn fill_poly(
        &mut self,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()>;

    fn fill_rectangle(
        &mut self,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()>;

    fn poly_text8(&mut self, host_xid: u32, foreground: u32, body: &[u8]) -> io::Result<()>;

    fn poly_text16(&mut self, host_xid: u32, foreground: u32, body: &[u8]) -> io::Result<()>;

    fn image_text8(
        &mut self,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()>;

    fn image_text16(
        &mut self,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // RENDER
    // ──────────────────────────────────────────────────────────────

    fn render_create_picture(
        &mut self,
        host_drawable: AnyHandle,
        ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_change_picture(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()>;

    fn render_free_picture(&mut self, host_pic: u32) -> io::Result<()>;

    fn render_create_glyphset(&mut self, ynest_format: u32) -> io::Result<Option<GlyphSetHandle>>;

    fn render_free_glyphset(&mut self, host_gs: u32) -> io::Result<()>;

    fn render_add_glyphs(&mut self, host_gs: u32, body_tail: &[u8]) -> io::Result<()>;

    fn render_free_glyphs(&mut self, host_gs: u32, glyph_ids: &[u8]) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn render_composite(
        &mut self,
        op: u8,
        host_src: u32,
        host_mask: u32,
        host_dst: u32,
        src_x: i16,
        src_y: i16,
        mask_x: i16,
        mask_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn render_composite_glyphs(
        &mut self,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        mask_fmt: u32,
        host_gs: u32,
        src_x: i16,
        src_y: i16,
        items: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()>;

    fn render_fill_rectangles(
        &mut self,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn render_trapezoids(
        &mut self,
        op: u8,
        host_src: u32,
        host_dst: u32,
        host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()>;

    fn render_create_solid_fill(&mut self, color: [u8; 8]) -> io::Result<Option<PictureHandle>>;

    fn render_create_linear_gradient(&mut self, body: &[u8]) -> io::Result<Option<PictureHandle>>;

    fn render_create_radial_gradient(&mut self, body: &[u8]) -> io::Result<Option<PictureHandle>>;

    fn render_create_cursor(
        &mut self,
        host_src_pic: PictureHandle,
        x: u16,
        y: u16,
    ) -> io::Result<Option<CursorHandle>>;

    fn render_set_picture_clip_rectangles(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()>;

    fn render_set_picture_filter(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()>;

    fn render_set_picture_transform(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()>;

    fn render_query_version(&mut self) -> io::Result<(u32, u32)>;

    // ──────────────────────────────────────────────────────────────
    // Other extensions
    // ──────────────────────────────────────────────────────────────

    fn xkb_proxy(&mut self, minor: u8, body: &[u8]) -> io::Result<Option<Vec<u8>>>;

    fn xfixes_change_cursor_by_name(
        &mut self,
        host_cursor_xid: u32,
        name_bytes: &[u8],
    ) -> io::Result<()>;

    fn set_shape_rectangles(
        &mut self,
        host_xid: u32,
        kind: u8,
        rects: &[xfixes::RegionRect],
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Misc
    // ──────────────────────────────────────────────────────────────

    fn warp_pointer(&mut self, dst_host_xid: u32, dst_x: i16, dst_y: i16) -> io::Result<()>;

    fn query_pointer(&mut self) -> io::Result<PointerPosition>;

    fn list_fonts_proxy(&mut self, max_names: u16, pattern: &str) -> io::Result<Vec<u8>>;

    fn list_fonts_with_info_proxy(
        &mut self,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<Vec<u8>>>;

    fn get_atom_name(&mut self, atom: u32) -> io::Result<Option<String>>;

    fn get_keyboard_mapping(&mut self, first_keycode: u8, count: u8) -> io::Result<(u8, Vec<u32>)>;

    fn get_modifier_mapping(&mut self) -> io::Result<(u8, Vec<u8>)>;
}

// Compile-time assertion that `Backend` is object-safe and that the
// `Arc<Mutex<dyn Backend>>` shape used by the hot-path call sites is
// `Send + Sync` (so worker threads can hold it).
const _: fn() = || {
    fn assert_obj_safe(_: &dyn Backend) {}
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<std::sync::Arc<std::sync::Mutex<dyn Backend>>>();
    let _ = assert_obj_safe;
};
