//! `impl Backend for HostX11Backend` — every method delegates to the
//! existing `HostX11Backend::method` directly. The body is intentionally
//! mechanical so a future trait surface change (Phase 6.3+) can land
//! by editing one file.

use std::io;

use yserver_protocol::x11::{ClipRectangles, FontMetrics, xfixes};

use crate::backend::{
    AnyHandle, Backend, ClipState, CursorHandle, DrawState, FillState, FontHandle, GlyphSetHandle,
    PictureHandle, PixmapHandle, WindowHandle,
};

use super::{HostSubwindowConfig, HostSubwindowVisual, HostX11Backend, PointerPosition};

impl Backend for HostX11Backend {
    fn window_id(&self) -> u32 {
        HostX11Backend::window_id(self)
    }

    fn root_visual_xid(&self) -> u32 {
        HostX11Backend::root_visual_xid(self)
    }

    fn argb_visual_xid(&self) -> Option<u32> {
        HostX11Backend::argb_visual_xid(self)
    }

    fn argb_colormap_xid(&self) -> Option<u32> {
        HostX11Backend::argb_colormap_xid(self)
    }

    fn render_opcode(&self) -> Option<u8> {
        HostX11Backend::render_opcode(self)
    }

    fn xkb_opcode(&self) -> Option<u8> {
        HostX11Backend::xkb_opcode(self)
    }

    fn xkb_info(&self) -> Option<(u8, u8, u8)> {
        HostX11Backend::xkb_info(self)
    }

    fn composite_opcode(&self) -> Option<u8> {
        HostX11Backend::composite_opcode(self)
    }

    fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32> {
        HostX11Backend::render_format_for_ynest_id(self, ynest_fmt)
    }

    fn ping(&mut self) -> io::Result<()> {
        HostX11Backend::ping(self)
    }

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
    ) -> io::Result<WindowHandle> {
        HostX11Backend::create_subwindow(
            self,
            host_parent,
            x,
            y,
            width,
            height,
            border_width,
            visual,
            background_pixel,
            background_pixmap,
        )
    }

    fn destroy_subwindow(&mut self, host_xid: u32) -> io::Result<()> {
        HostX11Backend::destroy_subwindow(self, host_xid)
    }

    fn map_subwindow(&mut self, host_xid: u32) -> io::Result<()> {
        HostX11Backend::map_subwindow(self, host_xid)
    }

    fn unmap_subwindow(&mut self, host_xid: u32) -> io::Result<()> {
        HostX11Backend::unmap_subwindow(self, host_xid)
    }

    fn configure_subwindow(
        &mut self,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()> {
        HostX11Backend::configure_subwindow(self, host_xid, config)
    }

    fn reparent_subwindow(
        &mut self,
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()> {
        HostX11Backend::reparent_subwindow(self, host_xid, host_parent, x, y)
    }

    fn change_subwindow_attributes(
        &mut self,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()> {
        HostX11Backend::change_subwindow_attributes(self, host_xid, value_mask, values)
    }

    fn name_window_pixmap(&mut self, host_window: WindowHandle) -> io::Result<PixmapHandle> {
        HostX11Backend::name_window_pixmap(self, host_window)
    }

    fn create_pixmap(&mut self, depth: u8, width: u16, height: u16) -> io::Result<PixmapHandle> {
        HostX11Backend::create_pixmap(self, depth, width, height)
    }

    fn free_pixmap(&mut self, host_xid: u32) -> io::Result<()> {
        HostX11Backend::free_pixmap(self, host_xid)
    }

    fn open_font(&mut self, name: &str) -> io::Result<(FontHandle, FontMetrics)> {
        HostX11Backend::open_font(self, name)
    }

    fn close_font(&mut self, host_xid: u32) -> io::Result<()> {
        HostX11Backend::close_font(self, host_xid)
    }

    fn create_cursor(
        &mut self,
        source_pixmap: PixmapHandle,
        mask_pixmap: Option<PixmapHandle>,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorHandle> {
        HostX11Backend::create_cursor(self, source_pixmap, mask_pixmap, fore, back, hot_x, hot_y)
    }

    fn define_cursor(&mut self, host_window_xid: u32, cursor_host_xid: u32) -> io::Result<()> {
        HostX11Backend::define_cursor(self, host_window_xid, cursor_host_xid)
    }

    fn set_container_background_pixel(&mut self, pixel: u32) -> io::Result<()> {
        HostX11Backend::set_container_background_pixel(self, pixel)
    }

    fn set_container_background_pixmap(&mut self, host_pixmap_xid: u32) -> io::Result<()> {
        HostX11Backend::set_container_background_pixmap(self, host_pixmap_xid)
    }

    fn clear_clip_rectangles(&mut self) -> io::Result<()> {
        HostX11Backend::clear_clip_rectangles(self)
    }

    fn set_clip_rectangles(&mut self, clip: Option<ClipRectangles>) -> io::Result<()> {
        HostX11Backend::set_clip_rectangles(self, clip)
    }

    fn set_clip_pixmap(
        &mut self,
        host_pixmap: u32,
        clip_x_origin: i16,
        clip_y_origin: i16,
    ) -> io::Result<()> {
        HostX11Backend::set_clip_pixmap(self, host_pixmap, clip_x_origin, clip_y_origin)
    }

    fn set_gc_fill_solid(&mut self) -> io::Result<()> {
        HostX11Backend::set_gc_fill_solid(self)
    }

    fn set_gc_fill_tiled(
        &mut self,
        host_pixmap: u32,
        tile_x_origin: i16,
        tile_y_origin: i16,
    ) -> io::Result<()> {
        HostX11Backend::set_gc_fill_tiled(self, host_pixmap, tile_x_origin, tile_y_origin)
    }

    fn apply_clip_state(&mut self, clip: &ClipState) -> io::Result<()> {
        HostX11Backend::apply_clip_state(self, clip)
    }

    fn apply_fill_state(&mut self, fill: &FillState) -> io::Result<()> {
        HostX11Backend::apply_fill_state(self, fill)
    }

    fn apply_draw_state(&mut self, state: &DrawState) -> io::Result<()> {
        HostX11Backend::apply_draw_state(self, state)
    }

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
    ) -> io::Result<()> {
        HostX11Backend::copy_area(
            self,
            src_host_xid,
            dst_host_xid,
            src_x,
            src_y,
            dst_x,
            dst_y,
            width,
            height,
        )
    }

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
    ) -> io::Result<()> {
        HostX11Backend::copy_plane(
            self,
            src_host_xid,
            dst_host_xid,
            src_x,
            src_y,
            dst_x,
            dst_y,
            width,
            height,
            plane,
        )
    }

    fn put_image(
        &mut self,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()> {
        HostX11Backend::put_image(self, host_xid, depth, width, height, dst_x, dst_y, data)
    }

    fn get_image(
        &mut self,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        HostX11Backend::get_image(self, host_xid, format, x, y, width, height, plane_mask)
    }

    fn poly_line(
        &mut self,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        HostX11Backend::poly_line(self, host_xid, foreground, coordinate_mode, points)
    }

    fn poly_segment(&mut self, host_xid: u32, foreground: u32, segments: &[u8]) -> io::Result<()> {
        HostX11Backend::poly_segment(self, host_xid, foreground, segments)
    }

    fn poly_rectangle(
        &mut self,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        HostX11Backend::poly_rectangle(self, host_xid, foreground, rectangles)
    }

    fn poly_arc(&mut self, host_xid: u32, foreground: u32, arcs: &[u8]) -> io::Result<()> {
        HostX11Backend::poly_arc(self, host_xid, foreground, arcs)
    }

    fn poly_point(
        &mut self,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        HostX11Backend::poly_point(self, host_xid, foreground, coordinate_mode, points)
    }

    fn poly_fill_rectangle(
        &mut self,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        HostX11Backend::poly_fill_rectangle(self, host_xid, foreground, rectangles)
    }

    fn poly_fill_arc(&mut self, host_xid: u32, foreground: u32, arcs: &[u8]) -> io::Result<()> {
        HostX11Backend::poly_fill_arc(self, host_xid, foreground, arcs)
    }

    fn fill_poly(
        &mut self,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        HostX11Backend::fill_poly(self, host_xid, foreground, coord_mode, points)
    }

    fn fill_rectangle(
        &mut self,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        HostX11Backend::fill_rectangle(self, host_xid, foreground, x, y, width, height)
    }

    fn poly_text8(&mut self, host_xid: u32, foreground: u32, body: &[u8]) -> io::Result<()> {
        HostX11Backend::poly_text8(self, host_xid, foreground, body)
    }

    fn poly_text16(&mut self, host_xid: u32, foreground: u32, body: &[u8]) -> io::Result<()> {
        HostX11Backend::poly_text16(self, host_xid, foreground, body)
    }

    fn image_text8(
        &mut self,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        HostX11Backend::image_text8(self, host_xid, foreground, background, text_len, body)
    }

    fn image_text16(
        &mut self,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        HostX11Backend::image_text16(self, host_xid, foreground, background, text_len, body)
    }

    fn render_create_picture(
        &mut self,
        host_drawable: AnyHandle,
        ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        HostX11Backend::render_create_picture(self, host_drawable, ynest_format, value_mask, values)
    }

    fn render_change_picture(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()> {
        HostX11Backend::render_change_picture(self, host_pic, body)
    }

    fn render_free_picture(&mut self, host_pic: u32) -> io::Result<()> {
        HostX11Backend::render_free_picture(self, host_pic)
    }

    fn render_create_glyphset(&mut self, ynest_format: u32) -> io::Result<Option<GlyphSetHandle>> {
        HostX11Backend::render_create_glyphset(self, ynest_format)
    }

    fn render_free_glyphset(&mut self, host_gs: u32) -> io::Result<()> {
        HostX11Backend::render_free_glyphset(self, host_gs)
    }

    fn render_add_glyphs(&mut self, host_gs: u32, body_tail: &[u8]) -> io::Result<()> {
        HostX11Backend::render_add_glyphs(self, host_gs, body_tail)
    }

    fn render_free_glyphs(&mut self, host_gs: u32, glyph_ids: &[u8]) -> io::Result<()> {
        HostX11Backend::render_free_glyphs(self, host_gs, glyph_ids)
    }

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
    ) -> io::Result<()> {
        HostX11Backend::render_composite(
            self, op, host_src, host_mask, host_dst, src_x, src_y, mask_x, mask_y, dst_x, dst_y,
            width, height,
        )
    }

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
    ) -> io::Result<()> {
        HostX11Backend::render_composite_glyphs(
            self, minor, op, host_src, host_dst, mask_fmt, host_gs, src_x, src_y, items, x_off,
            y_off,
        )
    }

    fn render_fill_rectangles(
        &mut self,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        HostX11Backend::render_fill_rectangles(self, host_dst, op, color, rects, x_off, y_off)
    }

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
    ) -> io::Result<()> {
        HostX11Backend::render_trapezoids(
            self,
            op,
            host_src,
            host_dst,
            host_mask_format,
            src_x,
            src_y,
            traps,
            x_off,
            y_off,
        )
    }

    fn render_create_solid_fill(&mut self, color: [u8; 8]) -> io::Result<Option<PictureHandle>> {
        HostX11Backend::render_create_solid_fill(self, color)
    }

    fn render_create_linear_gradient(&mut self, body: &[u8]) -> io::Result<Option<PictureHandle>> {
        HostX11Backend::render_create_linear_gradient(self, body)
    }

    fn render_create_radial_gradient(&mut self, body: &[u8]) -> io::Result<Option<PictureHandle>> {
        HostX11Backend::render_create_radial_gradient(self, body)
    }

    fn render_create_cursor(
        &mut self,
        host_src_pic: PictureHandle,
        x: u16,
        y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        HostX11Backend::render_create_cursor(self, host_src_pic, x, y)
    }

    fn render_set_picture_clip_rectangles(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()> {
        HostX11Backend::render_set_picture_clip_rectangles(self, host_pic, body)
    }

    fn render_set_picture_filter(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()> {
        HostX11Backend::render_set_picture_filter(self, host_pic, body)
    }

    fn render_set_picture_transform(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()> {
        HostX11Backend::render_set_picture_transform(self, host_pic, body)
    }

    fn render_query_version(&mut self) -> io::Result<(u32, u32)> {
        HostX11Backend::render_query_version(self)
    }

    fn xkb_proxy(&mut self, minor: u8, body: &[u8]) -> io::Result<Option<Vec<u8>>> {
        HostX11Backend::xkb_proxy(self, minor, body)
    }

    fn xfixes_change_cursor_by_name(
        &mut self,
        host_cursor_xid: u32,
        name_bytes: &[u8],
    ) -> io::Result<()> {
        HostX11Backend::xfixes_change_cursor_by_name(self, host_cursor_xid, name_bytes)
    }

    fn set_shape_rectangles(
        &mut self,
        host_xid: u32,
        kind: u8,
        rects: &[xfixes::RegionRect],
    ) -> io::Result<()> {
        HostX11Backend::set_shape_rectangles(self, host_xid, kind, rects)
    }

    fn warp_pointer(&mut self, dst_host_xid: u32, dst_x: i16, dst_y: i16) -> io::Result<()> {
        HostX11Backend::warp_pointer(self, dst_host_xid, dst_x, dst_y)
    }

    fn query_pointer(&mut self) -> io::Result<PointerPosition> {
        HostX11Backend::query_pointer(self)
    }

    fn list_fonts_proxy(&mut self, max_names: u16, pattern: &str) -> io::Result<Vec<u8>> {
        HostX11Backend::list_fonts_proxy(self, max_names, pattern)
    }

    fn list_fonts_with_info_proxy(
        &mut self,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<Vec<u8>>> {
        HostX11Backend::list_fonts_with_info_proxy(self, max_names, pattern)
    }

    fn get_atom_name(&mut self, atom: u32) -> io::Result<Option<String>> {
        HostX11Backend::get_atom_name(self, atom)
    }

    fn get_keyboard_mapping(&mut self, first_keycode: u8, count: u8) -> io::Result<(u8, Vec<u32>)> {
        HostX11Backend::get_keyboard_mapping(self, first_keycode, count)
    }

    fn get_modifier_mapping(&mut self) -> io::Result<(u8, Vec<u8>)> {
        HostX11Backend::get_modifier_mapping(self)
    }
}
