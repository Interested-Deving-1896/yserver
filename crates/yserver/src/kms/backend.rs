use std::{
    cell::RefCell,
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
};

use crossbeam_channel::Sender;
use pixman::{Color, FormatCode, Image, Operation, Rectangle16, Repeat};
use yserver_core::{
    backend::{
        AnyHandle, Backend, BackendEventSink, ClipState, CursorHandle, DrawState, FillState,
        FontHandle, GcFunction, GlyphSetHandle, OriginContext, PictureHandle, PixmapHandle,
        WindowHandle,
    },
    host_x11::{
        HostEvent, HostExposeEvent, HostKeyEvent, HostPointerEvent, HostSubwindowConfig,
        HostSubwindowVisual, HostXidMap, PointerEventKind, PointerPosition,
    },
};
use yserver_protocol::x11::{
    CharInfo as ProtocolCharInfo, ClipRectangles, FontMetrics, ResourceId, xfixes,
};

use crate::drm;

/// Newtype wrapper around `freetype::Face`.
/// `repr(transparent)` is required so `RefCell::as_ptr` can be safely cast
/// from `*mut FreetypeFace` to `*mut freetype::Face` in `render_text_string`.
/// SAFETY: All access is serialized through `Arc<Mutex<dyn Backend>>`.
/// Single-threaded context makes this sound. `Face` contains raw pointers
/// and `Rc<Vec<u8>>` by default, both `!Send`.
#[repr(transparent)]
pub struct FreetypeFace(#[allow(dead_code)] pub freetype::Face);
unsafe impl Send for FreetypeFace {}

/// Newtype wrapper around `xkb::Context`.
/// SAFETY: All access is serialized through `Arc<Mutex<dyn Backend>>`.
/// The raw pointer in xkbcommon is not `Send`, but the C library is thread-safe.
pub struct XkbContext(pub xkbcommon::xkb::Context);
unsafe impl Send for XkbContext {}

/// Newtype wrapper around `xkb::Keymap`.
/// SAFETY: All access is serialized through `Arc<Mutex<dyn Backend>>`.
pub struct XkbKeymap(pub xkbcommon::xkb::Keymap);
unsafe impl Send for XkbKeymap {}

/// Newtype wrapper around `xkb::State`.
/// SAFETY: All access is serialized through `Arc<Mutex<dyn Backend>>`.
pub struct XkbState(pub xkbcommon::xkb::State);
unsafe impl Send for XkbState {}

/// Newtype wrapper around pixman::Image.
/// SAFETY: All access is serialized through `Arc<Mutex<dyn Backend>>`.
/// The main thread owns scanout; window/pixmap images are only touched
/// on the main thread.
pub struct PixmanImage(pub Image<'static, 'static>);

unsafe impl Send for PixmanImage {}

impl PixmanImage {
    /// Create a blank Pixman image with the given format and dimensions.
    pub fn new(format: FormatCode, width: u16, height: u16, clear: bool) -> io::Result<Self> {
        Image::new(format, width as usize, height as usize, clear)
            .map(Self)
            .map_err(|_| io::Error::other("pixman image creation failed"))
    }

    /// Create a Pixman image wrapping an external buffer (for scanout).
    ///
    /// # Safety
    /// Caller guarantees the buffer outlives the image and is valid for the
    /// given dimensions and rowstride. The buffer must remain valid for the
    /// lifetime of the returned `PixmanImage`.
    pub unsafe fn from_buffer(
        format: FormatCode,
        width: u16,
        height: u16,
        bits: *mut u32,
        rowstride_bytes: usize,
        clear: bool,
    ) -> io::Result<Self> {
        unsafe {
            Image::from_raw_mut(
                format,
                width as usize,
                height as usize,
                bits,
                rowstride_bytes,
                clear,
            )
        }
        .map(Self)
        .map_err(|_| io::Error::other("pixman image creation from buffer failed"))
    }

    pub fn width(&self) -> usize {
        self.0.width()
    }

    pub fn height(&self) -> usize {
        self.0.height()
    }

    pub fn stride(&self) -> usize {
        self.0.stride()
    }

    /// SAFETY: The returned pointer is valid for the lifetime of the image.
    /// Caller must ensure no other mutable references exist.
    pub fn data(&self) -> *mut u32 {
        // SAFETY: Caller guarantees serialized access.
        unsafe { self.0.data() }
    }
}

/// Convert an X11 24-bit pixel (0xRRGGBB) to a Pixman Color.
/// Append 1×1 rects covering a Bresenham line from (x0,y0) to (x1,y1).
fn bresenham_segment(x0: i32, y0: i32, x1: i32, y1: i32, out: &mut Vec<Rectangle16>) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let (mut x, mut y) = (x0, y0);
    loop {
        out.push(Rectangle16 {
            x: x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            y: y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            width: 1,
            height: 1,
        });
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// Scanline fill a polygon (even-odd rule).  Edges are pairs of i32
/// vertices.  Output is a Vec of 1-pixel-tall horizontal Rectangle16 spans.
fn scanline_fill_polygon(verts: &[(i32, i32)], out: &mut Vec<Rectangle16>) {
    if verts.len() < 3 {
        return;
    }
    let y_min = verts.iter().map(|&(_, y)| y).min().unwrap();
    let y_max = verts.iter().map(|&(_, y)| y).max().unwrap();
    let mut crossings: Vec<i32> = Vec::with_capacity(verts.len());
    for y in y_min..=y_max {
        crossings.clear();
        for i in 0..verts.len() {
            let (x0, y0) = verts[i];
            let (x1, y1) = verts[(i + 1) % verts.len()];
            // Skip horizontal edges; use half-open [min_y, max_y) so
            // shared vertices contribute exactly once across two edges.
            let (ya, yb) = if y0 < y1 { (y0, y1) } else { (y1, y0) };
            if ya == yb || y < ya || y >= yb {
                continue;
            }
            // Linear interpolation: x at scanline y.
            let x = x0 as i64 + ((y - y0) as i64 * (x1 - x0) as i64) / (y1 - y0) as i64;
            crossings.push(x as i32);
        }
        crossings.sort_unstable();
        let mut i = 0;
        while i + 1 < crossings.len() {
            let x_start = crossings[i];
            let x_end = crossings[i + 1];
            if x_end > x_start {
                let w = (x_end - x_start) as i64;
                out.push(Rectangle16 {
                    x: x_start.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    y: y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    width: w.min(u16::MAX as i64) as u16,
                    height: 1,
                });
            }
            i += 2;
        }
    }
}

/// Clip a list of `Rectangle16` to the bounds `[0, iw) × [0, ih)` and drop
/// rects that fall entirely outside.  Pixman's `fill_rectangles` is supposed
/// to clip on its own but in our build a partially-out-of-bounds rect
/// (especially with negative x/y) can segfault; pre-clipping is the cheap
/// defensive workaround.
fn clip_rects_to_image(rects: &[Rectangle16], iw: i32, ih: i32) -> Vec<Rectangle16> {
    let mut out = Vec::with_capacity(rects.len());
    for r in rects {
        let x1 = (r.x as i32).max(0);
        let y1 = (r.y as i32).max(0);
        let x2 = ((r.x as i32) + r.width as i32).min(iw);
        let y2 = ((r.y as i32) + r.height as i32).min(ih);
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        out.push(Rectangle16 {
            x: x1 as i16,
            y: y1 as i16,
            width: (x2 - x1) as u16,
            height: (y2 - y1) as u16,
        });
    }
    out
}

fn color_from_u32(pixel: u32) -> Color {
    let r = ((pixel >> 16) & 0xFF) as u16;
    let g = ((pixel >> 8) & 0xFF) as u16;
    let b = (pixel & 0xFF) as u16;
    Color::new(r << 8, g << 8, b << 8, 0xFFFF)
}

/// Apply X11 GC `function` to a set of rectangles on `img`.
///
/// `GcFunction::Copy` maps to `PIXMAN_OP_SRC` (fast path).
/// `GcFunction::Xor` requires manual pixel manipulation: pixman's Porter-Duff
/// `PIXMAN_OP_XOR` is `src*(1-dst.a) + dst*(1-src.a)` which gives zero for
/// fully opaque images — NOT the bitwise XOR that X11 GXxor specifies.
/// All other GcFunction variants fall back to `Src` with a debug log.
fn fill_rects_with_gc_function(
    img: &mut PixmanImage,
    function: GcFunction,
    foreground_rgb: u32,
    rects: &[Rectangle16],
) {
    if matches!(function, GcFunction::Xor) {
        // Bitwise XOR over the RGB channels (X byte is preserved).
        let xor_mask = foreground_rgb & 0x00FF_FFFF;
        let stride_words = img.0.stride() / 4;
        let iw = img.0.width() as i32;
        let ih = img.0.height() as i32;
        // SAFETY: PixmanImage::data() is unsafe; we hold an exclusive &mut
        // reference to img so no other live references to the pixel buffer exist.
        let ptr = unsafe { img.0.data() };
        for r in rects {
            let x0 = (r.x as i32).max(0) as usize;
            let y0 = (r.y as i32).max(0) as usize;
            let x1 = (r.x as i32 + r.width as i32).min(iw).max(0) as usize;
            let y1 = (r.y as i32 + r.height as i32).min(ih).max(0) as usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    // SAFETY: x < iw ≤ img width, y < ih ≤ img height, and
                    // stride_words * ih ≤ allocation size.
                    unsafe {
                        let p = ptr.add(y * stride_words + x);
                        let old = *p;
                        *p = (old & 0xFF00_0000) | ((old ^ xor_mask) & 0x00FF_FFFF);
                    }
                }
            }
        }
        return;
    }
    let op = match function {
        GcFunction::Copy => Operation::Src,
        other => {
            log::debug!("GC function {:?} not implemented, falling back to Copy", other);
            Operation::Src
        }
    };
    let color = color_from_u32(foreground_rgb);
    let _ = img.0.fill_rectangles(op, color, rects);
}

/// Parse a packed pair of i16 values (2 bytes each) from a byte slice.
fn read_i16_pair(data: &[u8], offset: usize) -> Option<(i16, i16)> {
    if offset + 4 > data.len() {
        return None;
    }
    let x = i16::from_le_bytes([data[offset], data[offset + 1]]);
    let y = i16::from_le_bytes([data[offset + 2], data[offset + 3]]);
    Some((x, y))
}

/// Parse a packed rectangle (x:i16, y:i16, w:u16, h:u16) from a byte slice.
fn read_rect(data: &[u8], offset: usize) -> Option<Rectangle16> {
    if offset + 8 > data.len() {
        return None;
    }
    let x = i16::from_le_bytes([data[offset], data[offset + 1]]);
    let y = i16::from_le_bytes([data[offset + 2], data[offset + 3]]);
    let w = u16::from_le_bytes([data[offset + 4], data[offset + 5]]);
    let h = u16::from_le_bytes([data[offset + 6], data[offset + 7]]);
    Some(Rectangle16 {
        x,
        y,
        width: w,
        height: h,
    })
}

pub struct KmsBackend {
    // DRM (Phase 6.1 reuse)
    device: Arc<drm::Device>,
    output: drm::modeset::Output,
    fb_w: u16,
    fb_h: u16,
    swapchain: drm::Swapchain,

    // Window tracking: nested window resource ID -> local window state
    windows: HashMap<u32, WindowState>,
    next_host_xid: u32, // Monotonic counter, starts at 0x00400000

    // Backend trait state
    window_id: u32,
    root_visual_xid: u32,
    event_sink: Option<Box<dyn BackendEventSink>>,
    xid_map: HostXidMap,
    key_subscribers: Arc<Mutex<Vec<Sender<HostKeyEvent>>>>,

    // xkbcommon
    #[allow(dead_code)]
    xkb_context: XkbContext,
    xkb_keymap: XkbKeymap,
    xkb_state: XkbState,

    // libinput
    input_ctx: Option<crate::input::SendContext>,

    // Fonts (freetype)
    font_loader: FontLoader,
    fonts: HashMap<u32, FontState>,

    // Pixman pixmaps (non-window drawables)
    pixmaps: HashMap<u32, PixmapState>,

    // Background state (root)
    bg_pixel: Option<u32>,
    bg_pixmap: Option<PixmapHandle>,

    // Software cursor
    cursor_x: f32,
    cursor_y: f32,

    // Current font for text rendering
    current_font: Option<u32>,

    // Current GC drawing function (default: Copy)
    current_function: GcFunction,

    // RENDER picture tracking
    pictures: HashMap<u32, PictureState>,
}

/// State for a RENDER picture on the KMS backend.
enum PictureState {
    /// Picture wraps a window or pixmap drawable. Composites are forwarded
    /// to that drawable's Pixman image.
    Drawable {
        /// XID of the backing window or pixmap in self.windows / self.pixmaps.
        host_xid: u32,
        /// Optional clip rectangles set via SetPictureClipRectangles.
        clip: Option<Vec<Rectangle16>>,
    },
    /// 1×1 solid colour image (CreateSolidFill). Used as composite source.
    SolidFill {
        image: RefCell<PixmanImage>,
    },
}

struct WindowState {
    _nested_id: ResourceId,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border_width: u16,
    mapped: bool,
    _override_redirect: bool,
    _parent: Option<u32>,
    children: Vec<u32>,
    bg_pixel: Option<u32>,
    bg_pixmap: Option<PixmapHandle>,
    image: RefCell<PixmanImage>,
    #[allow(dead_code)]
    depth: u8,
    #[allow(dead_code)]
    visual: u32,
}

struct FontState {
    #[allow(dead_code)]
    handle: u32,
    face: RefCell<FreetypeFace>,
    metrics: FontMetrics,
    char_info_cache: HashMap<char, ProtocolCharInfo>,
}

struct PixmapState {
    #[allow(dead_code)]
    handle: u32,
    image: PixmanImage,
    #[allow(dead_code)]
    depth: u8,
}

/// Manages freetype font loading with XLFD fallback.
struct FontLoader {
    library: freetype::Library,
}

impl FontLoader {
    fn new() -> io::Result<Self> {
        Ok(Self {
            library: freetype::Library::init()
                .map_err(|e| io::Error::other(format!("freetype init failed: {e:?}")))?,
        })
    }

    fn is_xlfd_pattern(name: &str) -> bool {
        name.starts_with('-')
    }

    fn open_font(
        &self,
        name: &str,
    ) -> io::Result<(freetype::Face, FontMetrics, HashMap<char, ProtocolCharInfo>)> {
        let path = if Self::is_xlfd_pattern(name) {
            None
        } else {
            self.library
                .new_face(name, 0)
                .ok()
                .map(|face| (face, name.to_string()))
        };

        let candidates = [
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/gnu-free/FreeMono.ttf",
            "/usr/share/fonts/freefonts/FreeMono.ttf",
            "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ];

        let face = if let Some((f, _)) = path {
            f
        } else {
            let mut loaded = None;
            for candidate in &candidates {
                if let Ok(f) = self.library.new_face(candidate, 0) {
                    loaded = Some(f);
                    break;
                }
            }
            loaded.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("font not found: {name}"))
            })?
        };

        let _ = face.set_char_size(12 << 6, 12 << 6, 96, 96);
        let (metrics, char_cache) = compute_font_metrics(&face);
        Ok((face, metrics, char_cache))
    }
}

fn compute_char_info(face: &freetype::Face, ch: char) -> ProtocolCharInfo {
    let glyph_idx = ch as usize;
    let _ = face.load_char(glyph_idx, freetype::face::LoadFlag::RENDER);
    let glyph = face.glyph();
    let bitmap = glyph.bitmap();
    let metrics = glyph.metrics();

    let width = (metrics.horiAdvance >> 6) as i16;
    let left_side_bearing = (metrics.horiBearingX >> 6) as i16;
    let right_side_bearing = left_side_bearing + bitmap.width() as i16;
    let ascent = (metrics.horiBearingY >> 6) as i16;
    let descent = (bitmap.rows() as i16) - ascent;

    ProtocolCharInfo {
        left_side_bearing,
        right_side_bearing,
        character_width: width,
        ascent,
        descent,
        attributes: 0,
    }
}

fn compute_font_metrics(face: &freetype::Face) -> (FontMetrics, HashMap<char, ProtocolCharInfo>) {
    let mut char_info_cache = HashMap::new();
    // min_bounds tracks the per-glyph minimum across each metric, so each
    // field starts at its type's MAX so the first observation overwrites it.
    let mut min_bounds = ProtocolCharInfo {
        left_side_bearing: i16::MAX,
        right_side_bearing: i16::MAX,
        character_width: i16::MAX,
        ascent: i16::MAX,
        descent: i16::MAX,
        attributes: 0,
    };
    // max_bounds tracks the per-glyph maximum, so each field starts at MIN.
    let mut max_bounds = ProtocolCharInfo {
        left_side_bearing: i16::MIN,
        right_side_bearing: i16::MIN,
        character_width: i16::MIN,
        ascent: i16::MIN,
        descent: i16::MIN,
        attributes: 0,
    };

    for code in 0x20u32..=0x7E {
        let ch = char::from_u32(code).unwrap();
        let ci = compute_char_info(face, ch);
        if ci.left_side_bearing < min_bounds.left_side_bearing {
            min_bounds.left_side_bearing = ci.left_side_bearing;
        }
        if ci.right_side_bearing > max_bounds.right_side_bearing {
            max_bounds.right_side_bearing = ci.right_side_bearing;
        }
        if ci.character_width < min_bounds.character_width {
            min_bounds.character_width = ci.character_width;
        }
        if ci.character_width > max_bounds.character_width {
            max_bounds.character_width = ci.character_width;
        }
        if ci.ascent > max_bounds.ascent {
            max_bounds.ascent = ci.ascent;
        }
        if ci.descent > max_bounds.descent {
            max_bounds.descent = ci.descent;
        }
        if ci.ascent < min_bounds.ascent {
            min_bounds.ascent = ci.ascent;
        }
        if ci.descent < min_bounds.descent {
            min_bounds.descent = ci.descent;
        }
        char_info_cache.insert(ch, ci);
    }

    let font_ascent = max_bounds.ascent;
    let font_descent = max_bounds.descent;

    let metrics = FontMetrics {
        min_bounds,
        max_bounds,
        min_char_or_byte2: 0x20,
        max_char_or_byte2: 0x7E,
        default_char: 0x20,
        draw_direction: 0, // LeftToRight
        min_byte1: 0,
        max_byte1: 0,
        all_chars_exist: true,
        font_ascent,
        font_descent,
        properties: Vec::new(),
        char_infos: char_info_cache.values().cloned().collect(),
    };
    (metrics, char_info_cache)
}

impl KmsBackend {
    pub fn open(device_path: &str) -> io::Result<Self> {
        let device = Arc::new(drm::Device::open(device_path)?);
        let output = drm::modeset::discover_output(&device)?;
        let fb_w = output.picked.width;
        let fb_h = output.picked.height;

        let mut buffers = Vec::with_capacity(2);
        for _ in 0..2 {
            let b = drm::Buffer::new(Arc::clone(&device), fb_w, fb_h)?;
            buffers.push(b);
        }

        let initial_fb = buffers[0].fb_id();
        drm::modeset::commit_modeset(&device, &output, initial_fb)?;

        let swapchain = drm::Swapchain::with_initial_scanout(buffers, 0);

        let input_ctx = match crate::input::SendContext::new() {
            Ok(ctx) => Some(ctx),
            Err(err) => {
                log::warn!("libinput unavailable, continuing without input: {err}");
                None
            }
        };

        let ctx = XkbContext(xkbcommon::xkb::Context::new(
            xkbcommon::xkb::CONTEXT_NO_FLAGS,
        ));
        let keymap = xkbcommon::xkb::Keymap::new_from_names(
            &ctx.0,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .or_else(|| {
            xkbcommon::xkb::Keymap::new_from_names(
                &ctx.0,
                "",
                "",
                "",
                "",
                None,
                xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        })
        .ok_or_else(|| io::Error::other("failed to create xkb keymap"))?;
        let xkb_state = XkbState(xkbcommon::xkb::State::new(&keymap));
        let xkb_keymap = XkbKeymap(keymap);

        let mut xid_map = HashMap::new();
        xid_map.insert(0x00000001, ResourceId(0x0000_0100));
        let xid_map = Arc::new(Mutex::new(xid_map));

        Ok(Self {
            device,
            output,
            fb_w,
            fb_h,
            swapchain,
            windows: HashMap::new(),
            next_host_xid: 0x0040_0000,
            window_id: 1,
            root_visual_xid: 0x21,
            event_sink: None,
            xid_map,
            key_subscribers: Arc::new(Mutex::new(Vec::new())),
            xkb_context: ctx,
            xkb_keymap,
            xkb_state,
            input_ctx,
            font_loader: FontLoader::new()?,
            fonts: HashMap::new(),
            pixmaps: HashMap::new(),
            bg_pixel: None,
            bg_pixmap: None,
            cursor_x: 0.0,
            cursor_y: 0.0,
            current_font: None,
            current_function: GcFunction::Copy,
            pictures: HashMap::new(),
        })
    }

    fn next_host_xid(&mut self) -> u32 {
        self.next_host_xid = self
            .next_host_xid
            .checked_add(1)
            .expect("xid space exhausted");
        self.next_host_xid
    }

    /// Borrow a drawable's Pixman image and pass it to a closure.
    #[allow(dead_code)]
    fn with_image<F, R>(&self, host_xid: u32, f: F) -> Option<R>
    where
        F: FnOnce(&PixmanImage) -> R,
    {
        if let Some(w) = self.windows.get(&host_xid) {
            let img = w.image.borrow();
            Some(f(&img))
        } else {
            self.pixmaps.get(&host_xid).map(|p| f(&p.image))
        }
    }

    /// Mutably borrow a drawable's Pixman image and pass it to a closure.
    fn with_image_mut<F, R>(&mut self, host_xid: u32, f: F) -> Option<R>
    where
        F: FnOnce(&mut PixmanImage) -> R,
    {
        if let Some(w) = self.windows.get(&host_xid) {
            let mut img = w.image.borrow_mut();
            Some(f(&mut img))
        } else if let Some(p) = self.pixmaps.get_mut(&host_xid) {
            Some(f(&mut p.image))
        } else {
            None
        }
    }

    fn window_under_cursor(&self) -> Option<u32> {
        // Top-levels are direct children of the root container (window_id).
        // The root container is not itself an entry in self.windows.
        let root_id = self.window_id;
        let top_levels: Vec<u32> = self
            .windows
            .iter()
            .filter(|(_, w)| w._parent.is_none_or(|p| p == root_id))
            .map(|(&id, _)| id)
            .collect();
        for window_id in top_levels.into_iter().rev() {
            let w = &self.windows[&window_id];
            if !w.mapped {
                continue;
            }
            let x = self.cursor_x as f64;
            let y = self.cursor_y as f64;
            if x >= w.x as f64
                && x < (w.x as f64 + w.width as f64)
                && y >= w.y as f64
                && y < (w.y as f64 + w.height as f64)
            {
                return Some(window_id);
            }
        }
        None
    }

    fn synthesize_expose(&mut self, host_xid: u32, x: u16, y: u16, w: u16, h: u16) {
        let expose_event = HostEvent::Expose(HostExposeEvent {
            host_xid,
            x,
            y,
            width: w,
            height: h,
            count: 0,
        });
        if let Some(ref mut sink) = self.event_sink {
            sink.handle_backend_event(yserver_core::backend::BackendEvent::HostEvent(expose_event));
        }
    }

    fn serialize_modifiers(&self) -> u16 {
        let state = &self.xkb_state.0;
        let flags = xkbcommon::xkb::STATE_MODS_EFFECTIVE;
        let mut mask: u16 = 0;
        if state.mod_name_is_active("Shift", flags) {
            mask |= 0x01;
        }
        if state.mod_name_is_active("Lock", flags) {
            mask |= 0x02;
        }
        if state.mod_name_is_active("Control", flags) {
            mask |= 0x04;
        }
        if state.mod_name_is_active("Mod1", flags) {
            mask |= 0x08;
        }
        if state.mod_name_is_active("Mod2", flags) {
            mask |= 0x10;
        }
        if state.mod_name_is_active("Mod3", flags) {
            mask |= 0x20;
        }
        if state.mod_name_is_active("Mod4", flags) {
            mask |= 0x40;
        }
        if state.mod_name_is_active("Mod5", flags) {
            mask |= 0x80;
        }
        mask
    }

    /// Process all pending libinput events and route them through xkbcommon
    /// and the event sink. Called by the epoll loop when libinput fd is readable.
    pub fn process_input_events(&mut self) -> io::Result<()> {
        let Some(input_ctx) = &mut self.input_ctx else {
            return Ok(());
        };
        let events = input_ctx.dispatch()?;
        for event in events {
            match event {
                crate::input::InputEvent::KeyPress { keycode } => {
                    self.process_key_event(keycode, true);
                }
                crate::input::InputEvent::KeyRelease { keycode } => {
                    self.process_key_event(keycode, false);
                }
                crate::input::InputEvent::PointerMotion { dx, dy } => {
                    self.process_pointer_motion(dx, dy);
                }
                crate::input::InputEvent::PointerMotionAbsolute { x_norm, y_norm } => {
                    let x = (x_norm.clamp(0.0, 1.0) * (self.fb_w as f64 - 1.0)) as f32;
                    let y = (y_norm.clamp(0.0, 1.0) * (self.fb_h as f64 - 1.0)) as f32;
                    self.process_pointer_absolute(x, y);
                }
                crate::input::InputEvent::Button { code, pressed } => {
                    self.process_pointer_button(code, pressed);
                }
            }
        }
        Ok(())
    }

    fn process_key_event(&mut self, evdev_keycode: u32, is_press: bool) {
        let xkb_keycode = xkbcommon::xkb::Keycode::new(evdev_keycode + 8);
        let direction = if is_press {
            xkbcommon::xkb::KeyDirection::Down
        } else {
            xkbcommon::xkb::KeyDirection::Up
        };
        self.xkb_state.0.update_key(xkb_keycode, direction);

        let mask = self.serialize_modifiers();
        let key_event = HostKeyEvent {
            pressed: is_press,
            keycode: (evdev_keycode + 8) as u8,
            time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u32,
            root_x: self.cursor_x as i16,
            root_y: self.cursor_y as i16,
            event_x: self.cursor_x as i16,
            event_y: self.cursor_y as i16,
            state: mask,
        };
        // Fan out to key subscribers (keyboard forwarders)
        let subs = self.key_subscribers.lock().unwrap();
        for tx in subs.iter() {
            let _ = tx.send(key_event);
        }
    }

    fn process_pointer_absolute(&mut self, x: f32, y: f32) {
        self.cursor_x = x.clamp(0.0, self.fb_w as f32 - 1.0);
        self.cursor_y = y.clamp(0.0, self.fb_h as f32 - 1.0);
        self.dispatch_motion_event();
    }

    fn dispatch_motion_event(&mut self) {
        let host_xid = self.window_under_cursor().unwrap_or(0);
        let mask = self.serialize_modifiers();
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;
        let ptr_event = HostPointerEvent {
            kind: PointerEventKind::MotionNotify,
            host_xid,
            detail: 0,
            time,
            root_x: self.cursor_x as i16,
            root_y: self.cursor_y as i16,
            event_x: self.cursor_x as i16,
            event_y: self.cursor_y as i16,
            state: mask,
        };
        if let Some(ref mut sink) = self.event_sink {
            sink.handle_backend_event(yserver_core::backend::BackendEvent::HostEvent(
                HostEvent::Pointer(ptr_event),
            ));
        }
    }

    fn process_pointer_motion(&mut self, dx: f64, dy: f64) {
        self.cursor_x = (self.cursor_x + dx as f32).clamp(0.0, self.fb_w as f32 - 1.0);
        self.cursor_y = (self.cursor_y + dy as f32).clamp(0.0, self.fb_h as f32 - 1.0);
        let host_xid = self.window_under_cursor().unwrap_or(0);
        let mask = self.serialize_modifiers();
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;
        let ptr_event = HostPointerEvent {
            kind: PointerEventKind::MotionNotify,
            host_xid,
            detail: 0,
            time,
            root_x: self.cursor_x as i16,
            root_y: self.cursor_y as i16,
            event_x: self.cursor_x as i16,
            event_y: self.cursor_y as i16,
            state: mask,
        };
        if let Some(ref mut sink) = self.event_sink {
            sink.handle_backend_event(yserver_core::backend::BackendEvent::HostEvent(
                HostEvent::Pointer(ptr_event),
            ));
        }
    }

    fn process_pointer_button(&mut self, code: u32, pressed: bool) {
        let detail = match code {
            0x110 => 1, // BTN_LEFT
            0x111 => 3, // BTN_RIGHT
            0x112 => 2, // BTN_MIDDLE
            0x113 => 8, // BTN_SIDE
            0x114 => 9, // BTN_EXTRA
            _ => 0,
        };
        let host_xid = self.window_under_cursor().unwrap_or(0);
        let mask = self.serialize_modifiers();
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;
        let kind = if pressed {
            PointerEventKind::ButtonPress
        } else {
            PointerEventKind::ButtonRelease
        };
        let ptr_event = HostPointerEvent {
            kind,
            host_xid,
            detail,
            time,
            root_x: self.cursor_x as i16,
            root_y: self.cursor_y as i16,
            event_x: self.cursor_x as i16,
            event_y: self.cursor_y as i16,
            state: mask,
        };
        if let Some(ref mut sink) = self.event_sink {
            sink.handle_backend_event(yserver_core::backend::BackendEvent::HostEvent(
                HostEvent::Pointer(ptr_event),
            ));
        }
    }

    /// Acquire a swapchain buffer, composite all windows onto it, draw the
    /// software cursor, and submit the flip. Called by the epoll loop on
    /// page-flip completion or on a timer.
    pub fn composite_and_flip(&mut self) -> io::Result<()> {
        let buf_idx = self
            .swapchain
            .acquire_idx()
            .ok_or_else(|| io::Error::other("no swapchain buffer available"))?;

        let buf = self.swapchain.buffer_mut(buf_idx);
        let w = buf.width();
        let h = buf.height();
        let stride_bytes = buf.stride() as usize;
        let pixels = buf.pixels_mut().as_mut_ptr();

        // Create a temporary scanout image wrapping the swapchain buffer.
        // SAFETY: the buffer is owned by the swapchain and outlives this image.
        let mut scanout = unsafe {
            PixmanImage::from_buffer(FormatCode::X8R8G8B8, w, h, pixels, stride_bytes, false)?
        };

        // Fill root background; fall back to mid-grey so client windows stand out.
        {
            let bg_color = self.bg_pixel.map(color_from_u32)
                .unwrap_or_else(|| Color::new(0x5050, 0x5050, 0x5050, 0xffff));
            let root_rect = Rectangle16 { x: 0, y: 0, width: self.fb_w, height: self.fb_h };
            let _ = scanout.0.fill_rectangles(Operation::Src, bg_color, &[root_rect]);
        }

        // Composite top-level windows: direct children of the root container.
        let root_id = self.window_id;
        let top_levels: Vec<u32> = self
            .windows
            .iter()
            .filter(|(_, w)| w._parent.is_none_or(|p| p == root_id))
            .map(|(&id, _)| id)
            .collect();
        for &window_id in &top_levels {
            self.composite_window_into(&mut scanout, window_id);
        }

        // Draw software cursor (16x16 white rectangle)
        self.draw_cursor_onto(&mut scanout);

        // Image is dropped here (before submit), releasing the mutable borrow
        // on the swapchain buffer's pixels.
        drop(scanout);

        let fb_id = self.swapchain.buffer(buf_idx).fb_id();
        drm::page_flip::submit_flip(&self.device, &self.output, fb_id)?;
        self.swapchain
            .submit(buf_idx)
            .map_err(|e| io::Error::other(format!("swapchain.submit: {e}")))?;

        Ok(())
    }

    /// Recursively composite a window and its children into the target image.
    /// Children are composited into the window's own image first (natural clipping),
    /// then the window is composited onto the target.
    fn composite_window_into(&self, parent_img: &mut PixmanImage, window_id: u32) {
        let Some(window) = self.windows.get(&window_id) else {
            return;
        };
        if !window.mapped {
            return;
        }

        // Composite children into this window's image first
        let children: Vec<u32> = window.children.clone();
        for &child_id in &children {
            let child_target = &mut window.image.borrow_mut();
            self.composite_window_into(child_target, child_id);
        }

        // Now composite the window (with its children painted) onto the parent
        let window = &self.windows[&window_id];
        let x = window.x as i32;
        let y = window.y as i32;
        let w = window.width as i32;
        let h = window.height as i32;
        let src_img = window.image.borrow();
        parent_img.0.composite32(
            Operation::Over,
            &src_img.0,
            None,
            (0, 0),
            (0, 0),
            (x, y),
            (w, h),
        );
    }

    /// Draw a 16x16 white software cursor onto the scanout image.
    fn draw_cursor_onto(&self, scanout: &mut PixmanImage) {
        let cx = self.cursor_x as i32;
        let cy = self.cursor_y as i32;
        let cursor_w = 16i32;
        let cursor_h = 16i32;
        let color = Color::new(0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF);
        let rect = Rectangle16 {
            x: cx as i16,
            y: cy as i16,
            width: cursor_w as u16,
            height: cursor_h as u16,
        };
        let _ = scanout.0.fill_rectangles(Operation::Src, color, &[rect]);
    }

    /// Render a string of character bytes onto a drawable using the current font.
    /// Each byte is treated as a character index into the loaded font.
    fn render_text_string(
        &mut self,
        host_xid: u32,
        foreground: u32,
        x: i32,
        y: i32,
        text: &[u8],
    ) -> io::Result<()> {
        let Some(font_xid) = self.current_font else {
            return Ok(());
        };

        // Phase 1: render all glyphs into owned pixel buffers while holding
        // the RefCell borrow.  We must drop the borrow before phase 2 so that
        // with_image_mut (which requires &mut self) can be called.
        struct RenderedGlyph {
            dst_x: i32,
            dst_y: i32,
            w: usize,
            h: usize,
            pixels: Vec<u8>,    // row-major, w*h bytes
            #[allow(dead_code)] advance: i32,
        }

        let mut rendered: Vec<RenderedGlyph> = Vec::new();
        let mut cursor_x = x;

        {
            let Some(fs) = self.fonts.get(&font_xid) else {
                return Ok(());
            };
            let face = fs.face.borrow();
            let char_cache = &fs.char_info_cache;

            for &ch_byte in text {
                let ch = ch_byte as char;
                let Some(ci) = char_cache.get(&ch) else {
                    cursor_x += 6;
                    continue;
                };

                let _ = face.0.load_char(ch as usize, freetype::face::LoadFlag::RENDER);
                let glyph = face.0.glyph();
                let bitmap = glyph.bitmap();

                if bitmap.width() > 0 && bitmap.rows() > 0 {
                    let w = bitmap.width() as usize;
                    let h = bitmap.rows() as usize;
                    let stride = bitmap.pitch();
                    let buf = bitmap.buffer();

                    let mut pixels = vec![0u8; w * h];
                    for row in 0..h {
                        let src = if stride >= 0 {
                            row * stride as usize
                        } else {
                            (h - 1 - row) * (stride as isize).unsigned_abs()
                        };
                        pixels[row * w..row * w + w].copy_from_slice(&buf[src..src + w]);
                    }

                    rendered.push(RenderedGlyph {
                        dst_x: cursor_x + glyph.bitmap_left(),
                        dst_y: y - glyph.bitmap_top(),
                        w,
                        h,
                        pixels,
                        advance: ci.character_width as i32,
                    });
                }
                cursor_x += ci.character_width as i32;
            }
        } // RefCell borrow released here

        // Phase 2: composite each glyph onto the destination drawable.
        let fg_color = color_from_u32(foreground);
        for g in &rendered {
            let mut color_img = Image::new(FormatCode::A8R8G8B8, 1, 1, true)
                .map_err(|_| io::Error::other("pixman color image"))?;
            let _ = color_img.fill_rectangles(
                Operation::Src,
                fg_color,
                &[Rectangle16 { x: 0, y: 0, width: 1, height: 1 }],
            );
            // The 1×1 solid-colour source must tile across the full glyph
            // width/height.  Without REPEAT_NORMAL, pixman returns transparent
            // black for any source read outside (0, 0), making Operation::Over
            // a no-op for every column except the leftmost — producing scattered
            // dots (only pixels where glyph-col=0 has non-zero alpha are drawn).
            color_img.set_repeat(Repeat::Normal);

            // A8 image: pixman allocates with 4-byte row stride so we must
            // write byte-by-byte using the actual stride, not width.
            let glyph_img = Image::new(FormatCode::A8, g.w, g.h, true)
                .map_err(|_| io::Error::other("pixman glyph image"))?;
            let stride_bytes = glyph_img.stride();
            // SAFETY: gdata points into the pixman-allocated buffer for
            // glyph_img which lives for this block.  We write only within
            // [0, (h-1)*stride_bytes + (w-1)] which is inside the allocation.
            let gdata = unsafe { glyph_img.data() } as *mut u8;
            for row in 0..g.h {
                for col in 0..g.w {
                    unsafe {
                        *gdata.add(row * stride_bytes + col) = g.pixels[row * g.w + col];
                    }
                }
            }

            self.with_image_mut(host_xid, |dst| {
                let dst_w = dst.0.width() as i32;
                let dst_h = dst.0.height() as i32;
                // Skip glyphs that fall entirely outside the destination.
                // Some clients send extreme negative coords during probes;
                // pixman's composite32 has historically struggled with very
                // large negative offsets in our build, so guard explicitly.
                if g.dst_x + (g.w as i32) <= 0 || g.dst_y + (g.h as i32) <= 0
                    || g.dst_x >= dst_w || g.dst_y >= dst_h
                {
                    return;
                }
                dst.0.composite32(
                    Operation::Over,
                    &color_img,
                    Some(&glyph_img),
                    (0, 0),
                    (0, 0),
                    (g.dst_x, g.dst_y),
                    (g.w as i32, g.h as i32),
                );
            });
        }
        Ok(())
    }

    /// Return the scanout framebuffer dimensions.
    pub fn fb_dimensions(&self) -> (u16, u16) {
        (self.fb_w, self.fb_h)
    }

    /// Return the raw libinput fd for epoll registration, if available.
    pub fn input_fd(&self) -> Option<std::os::unix::io::RawFd> {
        self.input_ctx.as_ref().map(|ctx| ctx.fd())
    }

    /// Return the DRM device fd for epoll registration.
    pub fn drm_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::fd::{AsFd, AsRawFd};
        self.device.as_fd().as_raw_fd()
    }

    /// Drain pending page-flip events, acquire the next swapchain buffer,
    /// composite all windows onto it, draw the cursor, and submit a new flip.
    pub fn drain_page_flips_and_composite(&mut self) -> io::Result<()> {
        let mut handled = 0u32;
        drm::page_flip::drain_events(&self.device, || handled += 1)?;
        for _ in 0..handled {
            if let Some(idx) = self.swapchain.submitted_idx() {
                self.swapchain
                    .complete(idx)
                    .map_err(|e| io::Error::other(format!("swapchain.complete: {e}")))?;
            }
        }
        // Always composite on flip completion (self-driving at vsync)
        self.composite_and_flip()
    }

    /// Disable the DRM output (CRTC + plane) for clean shutdown.
    pub fn disable_output(&self) -> io::Result<()> {
        drm::modeset::disable_output(&self.device, &self.output)
    }
}

impl Backend for KmsBackend {
    fn window_id(&self) -> u32 {
        self.window_id
    }

    fn root_visual_xid(&self) -> u32 {
        self.root_visual_xid
    }

    fn argb_visual_xid(&self) -> Option<u32> {
        None
    }

    fn argb_colormap_xid(&self) -> Option<u32> {
        None
    }

    fn render_opcode(&self) -> Option<u8> {
        // X11 conventional major opcode for RENDER. Advertising RENDER as
        // present (with all 21 render_* trait methods stubbed below as
        // no-ops) is enough to flip fvwm3 from a two-level frame hierarchy
        // into a single-level one — without RENDER fvwm3 builds a deeper
        // frame, which makes GetGeometry on client windows return (0,0)
        // and traps FvwmPager's init loop. See:
        // docs/superpowers/notes/2026-05-04-phase6-5-fvwm3-trace.md.
        Some(133)
    }

    fn xkb_opcode(&self) -> Option<u8> {
        None
    }

    fn xkb_info(&self) -> Option<(u8, u8, u8)> {
        None
    }

    fn composite_opcode(&self) -> Option<u8> {
        None
    }

    fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32> {
        // Pass the format ID through as an opaque handle. For zero (no mask)
        // the caller (nested.rs) maps it to Some(0) directly; we only reach
        // here for nonzero values. Returning Some(ynest_fmt) is sufficient
        // for render_trapezoids to receive a non-None host_mask_format and
        // proceed; the actual format code is mapped to PIXMAN_a8 inside
        // render_trapezoids.
        if ynest_fmt == 0 { None } else { Some(ynest_fmt) }
    }

    fn ping(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Ok(())
    }

    fn set_event_sink(&mut self, sink: Option<Box<dyn BackendEventSink>>) {
        self.event_sink = sink;
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn create_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        _background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle> {
        let host_xid = self.next_host_xid();
        // Pre-fill the window image with its background pixel so clients
        // that expect the X server to auto-clear (e.g. xclock drawing black
        // tick marks on a "white" background) see the right backdrop.
        let mut img = PixmanImage::new(FormatCode::X8R8G8B8, width, height, true)?;
        if let Some(pixel) = background_pixel {
            let color = color_from_u32(pixel);
            let _ = img.0.fill_rectangles(
                Operation::Src,
                color,
                &[Rectangle16 { x: 0, y: 0, width, height }],
            );
        }
        let image = RefCell::new(img);
        let depth = match visual {
            HostSubwindowVisual::CopyFromParent => 24,
            HostSubwindowVisual::Explicit { depth, .. } => depth,
        };
        let visual_xid = match visual {
            HostSubwindowVisual::CopyFromParent => 0,
            HostSubwindowVisual::Explicit { visual_xid, .. } => visual_xid,
        };
        self.windows.insert(
            host_xid,
            WindowState {
                _nested_id: ResourceId(0x0000_0100),
                x,
                y,
                width,
                height,
                border_width,
                mapped: false,
                _override_redirect: false,
                _parent: Some(host_parent.as_raw()),
                children: Vec::new(),
                bg_pixel: background_pixel,
                bg_pixmap: None,
                image,
                depth,
                visual: visual_xid,
            },
        );
        if let Some(parent) = self.windows.get_mut(&host_parent.as_raw()) {
            parent.children.push(host_xid);
        }
        WindowHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create window handle"))
    }

    fn destroy_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        // Gather sibling info before removing
        let parent_xid = self.windows.get(&host_xid).and_then(|w| w._parent);
        let siblings = if let Some(parent) = parent_xid {
            self.windows
                .get(&parent)
                .map(|p| p.children.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        if self.windows.remove(&host_xid).is_some() {
            // Update parent's children list
            if let Some(parent_xid) = parent_xid
                && let Some(parent) = self.windows.get_mut(&parent_xid)
            {
                parent.children.retain(|&c| c != host_xid);
            }
        }
        let mut map = self.xid_map.lock().unwrap();
        map.remove(&host_xid);
        drop(map);

        // Expose siblings that may have been uncovered
        for &sibling_id in &siblings {
            if let Some(s) = self.windows.get(&sibling_id)
                && s.mapped
            {
                self.synthesize_expose(sibling_id, 0, 0, s.width, s.height);
            }
        }
        Ok(())
    }

    fn map_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        if let Some(window) = self.windows.get_mut(&host_xid) {
            window.mapped = true;
            let w = window.width;
            let h = window.height;
            self.synthesize_expose(host_xid, 0, 0, w, h);
        }
        Ok(())
    }

    fn unmap_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        // Gather info before unmapping
        let info = self
            .windows
            .get(&host_xid)
            .map(|w| (w._parent, w.children.clone(), w.x, w.y, w.width, w.height));
        let Some((parent_xid, _children, _wx, _wy, _ww, _wh)) = info else {
            return Ok(());
        };

        // Unmap the window
        if let Some(window) = self.windows.get_mut(&host_xid) {
            window.mapped = false;
        }

        // Expose siblings at parent level that may have been uncovered
        // For simplicity, expose all mapped siblings
        if let Some(parent) = parent_xid
            && let Some(p) = self.windows.get(&parent)
        {
            let children: Vec<u32> = p.children.clone();
            for &sibling_id in &children {
                if sibling_id != host_xid
                    && let Some(s) = self.windows.get(&sibling_id)
                    && s.mapped
                {
                    self.synthesize_expose(sibling_id, 0, 0, s.width, s.height);
                }
            }
        }
        Ok(())
    }

    fn configure_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()> {
        let resized = self
            .windows
            .get(&host_xid)
            .is_some_and(|_w| config.width.is_some() || config.height.is_some());
        if let Some(window) = self.windows.get_mut(&host_xid) {
            if let Some(w) = config.width {
                window.width = w;
            }
            if let Some(h) = config.height {
                window.height = h;
            }
            if let Some(x) = config.x {
                window.x = x;
            }
            if let Some(y) = config.y {
                window.y = y;
            }
            if let Some(bw) = config.border_width {
                window.border_width = bw;
            }
            if resized {
                let (w, h) = (window.width, window.height);
                let mut img = PixmanImage::new(FormatCode::X8R8G8B8, w, h, true)?;
                if let Some(pixel) = window.bg_pixel {
                    let color = color_from_u32(pixel);
                    let _ = img.0.fill_rectangles(
                        Operation::Src,
                        color,
                        &[Rectangle16 { x: 0, y: 0, width: w, height: h }],
                    );
                }
                window.image = RefCell::new(img);
            }
        }
        // Mutable borrow on windows ends here, safe to call synthesize_expose
        if resized && let Some(w) = self.windows.get(&host_xid) {
            self.synthesize_expose(host_xid, 0, 0, w.width, w.height);
        }
        Ok(())
    }

    fn reparent_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        new_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()> {
        let Some(window) = self.windows.get_mut(&host_xid) else {
            return Ok(());
        };
        let old_parent = window._parent;
        window._parent = Some(new_parent);
        window.x = x;
        window.y = y;
        if let Some(old_parent_xid) = old_parent
            && let Some(parent) = self.windows.get_mut(&old_parent_xid)
        {
            parent.children.retain(|&c| c != host_xid);
        }
        if let Some(parent) = self.windows.get_mut(&new_parent) {
            parent.children.push(host_xid);
        }
        Ok(())
    }

    fn change_subwindow_attributes(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()> {
        let Some(window) = self.windows.get_mut(&host_xid) else {
            return Ok(());
        };
        let mut idx = 0;
        if value_mask & 0x01 != 0 && !values.is_empty() {
            // CWBackPixmap
            window.bg_pixmap = PixmapHandle::from_raw(values[idx]);
            idx += 1;
        }
        if value_mask & 0x02 != 0 && values.len() > idx {
            // CWBackPixel
            window.bg_pixel = Some(values[idx]);
        }
        Ok(())
    }

    fn update_host_event_mask(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _mask: u32,
        _enabled: bool,
    ) -> io::Result<()> {
        Ok(())
    }

    fn register_top_level(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        let mut map = self.xid_map.lock().unwrap();
        map.insert(host_xid, nested_id);
        Ok(())
    }

    fn register_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        let mut map = self.xid_map.lock().unwrap();
        map.insert(host_xid, nested_id);
        Ok(())
    }

    fn unregister_host_window(&mut self, host_xid: u32) {
        let mut map = self.xid_map.lock().unwrap();
        map.remove(&host_xid);
    }

    fn xid_map(&self) -> HostXidMap {
        Arc::clone(&self.xid_map)
    }

    fn add_key_subscriber(&mut self, tx: Sender<HostKeyEvent>) {
        self.key_subscribers.lock().unwrap().push(tx);
    }

    fn name_window_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        _host_window: WindowHandle,
    ) -> io::Result<PixmapHandle> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "name_window_pixmap not supported",
        ))
    }

    fn create_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle> {
        let host_xid = self.next_host_xid();
        let format = match depth {
            1 => FormatCode::A1,
            8 => FormatCode::A8,
            24 => FormatCode::X8R8G8B8,
            32 => FormatCode::A8R8G8B8,
            _ => FormatCode::X8R8G8B8,
        };
        let image = PixmanImage::new(format, width, height, true)?;
        self.pixmaps.insert(
            host_xid,
            PixmapState {
                handle: host_xid,
                image,
                depth,
            },
        );
        PixmapHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create pixmap handle"))
    }

    fn free_pixmap(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.pixmaps.remove(&host_xid);
        Ok(())
    }

    fn open_font(
        &mut self,
        _origin: Option<OriginContext>,
        name: &str,
    ) -> io::Result<(FontHandle, FontMetrics)> {
        let (face, metrics, char_cache) = self.font_loader.open_font(name)?;
        let host_xid = self.next_host_xid();
        let handle = FontHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create font handle"))?;
        self.fonts.insert(
            host_xid,
            FontState {
                handle: host_xid,
                face: RefCell::new(FreetypeFace(face)),
                metrics: metrics.clone(),
                char_info_cache: char_cache,
            },
        );
        Ok((handle, metrics))
    }

    fn close_font(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.fonts.remove(&host_xid);
        Ok(())
    }

    fn create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _source_pixmap: PixmapHandle,
        _mask_pixmap: Option<PixmapHandle>,
        _fore: (u16, u16, u16),
        _back: (u16, u16, u16),
        _hot_x: u16,
        _hot_y: u16,
    ) -> io::Result<CursorHandle> {
        let host_xid = self.next_host_xid();
        CursorHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create cursor handle"))
    }

    fn define_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _host_window_xid: u32,
        _cursor_host_xid: u32,
    ) -> io::Result<()> {
        Ok(())
    }

    fn set_container_background_pixel(
        &mut self,
        _origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()> {
        self.bg_pixel = Some(pixel);
        Ok(())
    }

    fn set_container_background_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()> {
        self.bg_pixmap = PixmapHandle::from_raw(host_pixmap_xid);
        Ok(())
    }

    fn clear_clip_rectangles(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Ok(())
    }

    fn set_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        _clip: Option<ClipRectangles>,
    ) -> io::Result<()> {
        Ok(())
    }

    fn set_clip_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pixmap: u32,
        _clip_x_origin: i16,
        _clip_y_origin: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn set_gc_fill_solid(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Ok(())
    }

    fn set_gc_fill_tiled(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pixmap: u32,
        _tile_x_origin: i16,
        _tile_y_origin: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn apply_clip_state(
        &mut self,
        _origin: Option<OriginContext>,
        _clip: &ClipState,
    ) -> io::Result<()> {
        Ok(())
    }

    fn apply_fill_state(
        &mut self,
        _origin: Option<OriginContext>,
        _fill: &FillState,
    ) -> io::Result<()> {
        Ok(())
    }

    fn apply_draw_state(
        &mut self,
        _origin: Option<OriginContext>,
        state: &DrawState,
    ) -> io::Result<()> {
        if let Some(font) = state.font {
            self.current_font = Some(font.as_raw());
        }
        self.current_function = state.function;
        Ok(())
    }

    fn copy_area(
        &mut self,
        _origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        // Get source dimensions and copy pixel data to avoid borrow conflicts
        let (Some(src_w), Some(src_h), Some(src_stride)) = (
            self.windows
                .get(&src_host_xid)
                .map(|w| w.image.borrow().width())
                .or_else(|| self.pixmaps.get(&src_host_xid).map(|p| p.image.width())),
            self.windows
                .get(&src_host_xid)
                .map(|w| w.image.borrow().height())
                .or_else(|| self.pixmaps.get(&src_host_xid).map(|p| p.image.height())),
            self.windows
                .get(&src_host_xid)
                .map(|w| w.image.borrow().stride())
                .or_else(|| self.pixmaps.get(&src_host_xid).map(|p| p.image.stride())),
        ) else {
            return Ok(());
        };
        // Copy source pixels into a temporary buffer
        let src_data = if let Some(w) = self.windows.get(&src_host_xid) {
            let img = w.image.borrow();
            let data = img.data();
            (0..(src_h * src_stride / 4))
                .map(|i| unsafe { *data.add(i) })
                .collect::<Vec<u32>>()
        } else if let Some(p) = self.pixmaps.get(&src_host_xid) {
            let data = p.image.data();
            (0..(src_h * src_stride / 4))
                .map(|i| unsafe { *data.add(i) })
                .collect::<Vec<u32>>()
        } else {
            return Ok(());
        };

        // Hold the destination's RefMut for the entire write so RefCell's
        // aliasing invariants are upheld. (For pixmaps the data() pointer is
        // available through &p.image since pixman::Image::data() is unsafe
        // and not bounded by Rust borrow rules; the RefMut for windows is
        // the analogue.)
        let dst_w;
        let dst_h;
        let dst_stride;
        let dst_data;
        let _dst_window_borrow;
        if let Some(w) = self.windows.get(&dst_host_xid) {
            let img = w.image.borrow_mut();
            dst_w = img.width();
            dst_h = img.height();
            dst_stride = img.stride();
            dst_data = img.data();
            _dst_window_borrow = Some(img);
        } else if let Some(p) = self.pixmaps.get(&dst_host_xid) {
            dst_w = p.image.width();
            dst_h = p.image.height();
            dst_stride = p.image.stride();
            dst_data = p.image.data();
            _dst_window_borrow = None;
        } else {
            return Ok(());
        };
        for row in 0..height as isize {
            for col in 0..width as isize {
                let sx = (src_x as isize + col) as usize;
                let sy = (src_y as isize + row) as usize;
                let dx = (dst_x as isize + col) as usize;
                let dy = (dst_y as isize + row) as usize;
                if sx < src_w && sy < src_h && dx < dst_w && dy < dst_h {
                    let src_pixel = src_data[sy * src_stride / 4 + sx];
                    unsafe {
                        *dst_data.add(dy * dst_stride / 4 + dx) = src_pixel;
                    }
                }
            }
        }
        Ok(())
    }

    fn copy_plane(
        &mut self,
        _origin: Option<OriginContext>,
        _src_host_xid: u32,
        _dst_host_xid: u32,
        _src_x: i16,
        _src_y: i16,
        _dst_x: i16,
        _dst_y: i16,
        _width: u16,
        _height: u16,
        _plane: u32,
    ) -> io::Result<()> {
        // TODO: implement with plane mask
        Ok(())
    }

    fn put_image(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()> {
        let Some(img_w) = self
            .windows
            .get(&host_xid)
            .map(|w| w.image.borrow().width())
            .or_else(|| self.pixmaps.get(&host_xid).map(|p| p.image.width()))
        else {
            return Ok(());
        };
        let img_h = self
            .windows
            .get(&host_xid)
            .map(|w| w.image.borrow().height())
            .or_else(|| self.pixmaps.get(&host_xid).map(|p| p.image.height()))
            .unwrap();
        let stride = self
            .windows
            .get(&host_xid)
            .map(|w| w.image.borrow().stride())
            .or_else(|| self.pixmaps.get(&host_xid).map(|p| p.image.stride()))
            .unwrap()
            / 4;
        let img_data = if let Some(w) = self.windows.get(&host_xid) {
            w.image.borrow().data()
        } else {
            self.pixmaps.get(&host_xid).unwrap().image.data()
        };

        match depth {
            24 | 32 => {
                // X8R8G8B8 / A8R8G8B8 — 4 bytes per pixel
                for row in 0..height as isize {
                    let dy = dst_y as isize + row;
                    if dy < 0 || dy >= img_h as isize {
                        continue;
                    }
                    for col in 0..width as isize {
                        let dx = dst_x as isize + col;
                        if dx < 0 || dx >= img_w as isize {
                            continue;
                        }
                        let src_offset = ((row * width as isize + col) * 4) as usize;
                        if src_offset + 3 >= data.len() {
                            continue;
                        }
                        let r = data[src_offset] as u32;
                        let g = data[src_offset + 1] as u32;
                        let b = data[src_offset + 2] as u32;
                        let a = if depth == 32 {
                            data[src_offset + 3] as u32
                        } else {
                            0xFF
                        };
                        let pixel = (a << 24) | (r << 16) | (g << 8) | b;
                        unsafe {
                            *img_data.add(dy as usize * stride + dx as usize) = pixel;
                        }
                    }
                }
            }
            1 => {
                // Depth-1 PutImage targets an A1 (1 bpp) pixmap. The
                // u32-stride math used above would write 4 bytes per pixel
                // and overrun the buffer. Until we have a byte-aware A1
                // path, skip — the only client we've seen using this is
                // xterm's cursor mask, and `define_cursor` is a no-op.
            }
            _ => {
                // Unsupported depth — skip.
            }
        }
        Ok(())
    }

    fn get_image(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        _format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        _plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        let img_w;
        let img_h;
        let stride;
        let img_data;
        if let Some(w) = self.windows.get(&host_xid) {
            let img = w.image.borrow();
            img_w = img.width();
            img_h = img.height();
            stride = img.stride() / 4;
            img_data = img.data();
        } else if let Some(p) = self.pixmaps.get(&host_xid) {
            img_w = p.image.width();
            img_h = p.image.height();
            stride = p.image.stride() / 4;
            img_data = p.image.data();
        } else {
            return Ok(None);
        };
        let mut result = Vec::with_capacity(width as usize * height as usize * 4);
        for row in 0..height as isize {
            let dy = y as isize + row;
            if dy < 0 || dy >= img_h as isize {
                // out of bounds — write zeros
                result.resize(result.len() + width as usize * 4, 0);
                continue;
            }
            for col in 0..width as isize {
                let dx = x as isize + col;
                if dx < 0 || dx >= img_w as isize {
                    result.extend_from_slice(&[0; 4]);
                } else {
                    let pixel = unsafe { *img_data.add(dy as usize * stride + dx as usize) };
                    result.extend_from_slice(&pixel.to_le_bytes());
                }
            }
        }
        Ok(Some(result))
    }

    fn poly_line(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        // X11 PolyLine: connect consecutive points with line segments.
        // coordinate_mode 0 = Origin (absolute), 1 = Previous (each point is
        // a delta from the previous).  Rasterise each segment with Bresenham.
        let mut rects: Vec<Rectangle16> = Vec::new();
        let mut prev: Option<(i32, i32)> = None;
        let mut offset = 0;
        while offset + 4 <= points.len() {
            let Some((x, y)) = read_i16_pair(points, offset) else {
                break;
            };
            offset += 4;
            let (xi, yi) = if coordinate_mode == 1 {
                if let Some((px, py)) = prev {
                    (px + x as i32, py + y as i32)
                } else {
                    (x as i32, y as i32)
                }
            } else {
                (x as i32, y as i32)
            };
            if let Some((px, py)) = prev {
                bresenham_segment(px, py, xi, yi, &mut rects);
            }
            prev = Some((xi, yi));
        }
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            let clipped = clip_rects_to_image(&rects, img.0.width() as i32, img.0.height() as i32);
            fill_rects_with_gc_function(img, function, foreground, &clipped);
        });
        Ok(())
    }

    fn poly_segment(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()> {
        // Each segment is (x1:i16, y1:i16, x2:i16, y2:i16). Bresenham
        // rasterises diagonals correctly (axis-aligned bbox would only work
        // for horizontal / vertical segments).
        let mut rects: Vec<Rectangle16> = Vec::new();
        let mut offset = 0;
        while offset + 8 <= segments.len() {
            let Some((x1, y1)) = read_i16_pair(segments, offset) else { break; };
            let Some((x2, y2)) = read_i16_pair(segments, offset + 4) else { break; };
            offset += 8;
            bresenham_segment(x1 as i32, y1 as i32, x2 as i32, y2 as i32, &mut rects);
        }
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            let clipped = clip_rects_to_image(&rects, img.0.width() as i32, img.0.height() as i32);
            fill_rects_with_gc_function(img, function, foreground, &clipped);
        });
        Ok(())
    }

    fn poly_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        // Draw rectangle outlines (4 thin rectangles per rect)
        let mut rects = Vec::new();
        let mut offset = 0;
        while offset + 8 <= rectangles.len() {
            let Some(r) = read_rect(rectangles, offset) else {
                break;
            };
            offset += 8;
            if r.width == 0 || r.height == 0 {
                continue;
            }
            // top edge
            rects.push(Rectangle16 {
                x: r.x,
                y: r.y,
                width: r.width,
                height: 1,
            });
            // bottom edge
            rects.push(Rectangle16 {
                x: r.x,
                y: r.y.wrapping_add(r.height as i16).wrapping_sub(1),
                width: r.width,
                height: 1,
            });
            // left edge
            rects.push(Rectangle16 {
                x: r.x,
                y: r.y,
                width: 1,
                height: r.height,
            });
            // right edge
            rects.push(Rectangle16 {
                x: r.x.wrapping_add(r.width as i16).wrapping_sub(1),
                y: r.y,
                width: 1,
                height: r.height,
            });
        }
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            fill_rects_with_gc_function(img, function, foreground, &rects);
        });
        Ok(())
    }

    fn poly_arc(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        // Draw arc outlines.  Each arc is 12 bytes:
        //   x(i16) y(i16) w(u16) h(u16) angle1(i16) angle2(i16)
        // Like poly_fill_arc we treat partial-angle arcs as full ellipses
        // for now (the angle-mask refinement is a follow-up).
        //
        // Algorithm: for each scanline `py` of the bounding box, compute the
        // ellipse's inside x-range [x0, x1] and emit:
        //   - the full horizontal span at the first/last interior scanline
        //     (the top/bottom caps),
        //   - segments connecting the prev row's left/right edges to this
        //     row's left/right edges otherwise (the side outlines).
        // This produces a closed 1-pixel outline.
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            let iw = img.0.width() as i32;
            let ih = img.0.height() as i32;
            let mut rects: Vec<Rectangle16> = Vec::new();
            for chunk in arcs.chunks_exact(12) {
                let ax = i16::from_le_bytes([chunk[0], chunk[1]]) as i32;
                let ay = i16::from_le_bytes([chunk[2], chunk[3]]) as i32;
                let aw = u16::from_le_bytes([chunk[4], chunk[5]]) as i32;
                let ah = u16::from_le_bytes([chunk[6], chunk[7]]) as i32;
                if aw <= 0 || ah <= 0 {
                    continue;
                }
                let cx = ax as f64 + (aw as f64) * 0.5;
                let cy = ay as f64 + (ah as f64) * 0.5;
                let rx = (aw as f64) * 0.5;
                let ry = (ah as f64) * 0.5;

                let row_at = |py: i32| -> Option<(i32, i32)> {
                    let dy = (py as f64 + 0.5 - cy) / ry;
                    if dy.abs() > 1.0 {
                        return None;
                    }
                    let dx = (1.0 - dy * dy).sqrt() * rx;
                    let x0 = (cx - dx).floor() as i32;
                    let x1 = (cx + dx).ceil() as i32;
                    Some((x0, x1))
                };

                let mut prev: Option<(i32, i32)> = None;
                for py in ay..ay + ah {
                    let Some((x0, x1)) = row_at(py) else {
                        prev = None;
                        continue;
                    };
                    let next = row_at(py + 1);
                    let cap = prev.is_none() || next.is_none();
                    if cap {
                        // Full horizontal span (top or bottom of curve).
                        rects.push(Rectangle16 {
                            x: x0 as i16,
                            y: py as i16,
                            width: (x1 - x0 + 1) as u16,
                            height: 1,
                        });
                    } else {
                        // Side connectors: left edge and right edge runs
                        // bridging this row's edge to the previous row's.
                        let (px0, px1) = prev.unwrap();
                        let l_lo = px0.min(x0);
                        let l_hi = px0.max(x0);
                        rects.push(Rectangle16 {
                            x: l_lo as i16,
                            y: py as i16,
                            width: (l_hi - l_lo + 1) as u16,
                            height: 1,
                        });
                        let r_lo = px1.min(x1);
                        let r_hi = px1.max(x1);
                        rects.push(Rectangle16 {
                            x: r_lo as i16,
                            y: py as i16,
                            width: (r_hi - r_lo + 1) as u16,
                            height: 1,
                        });
                    }
                    prev = Some((x0, x1));
                }
            }
            let clipped = clip_rects_to_image(&rects, iw, ih);
            fill_rects_with_gc_function(img, function, foreground, &clipped);
        });
        Ok(())
    }

    fn poly_point(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        _coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        let mut rects = Vec::new();
        let mut offset = 0;
        while offset + 4 <= points.len() {
            let Some((x, y)) = read_i16_pair(points, offset) else {
                break;
            };
            offset += 4;
            rects.push(Rectangle16 {
                x,
                y,
                width: 1,
                height: 1,
            });
        }
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            fill_rects_with_gc_function(img, function, foreground, &rects);
        });
        Ok(())
    }

    fn poly_fill_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        let mut rects = Vec::new();
        let mut offset = 0;
        while offset + 8 <= rectangles.len() {
            let Some(r) = read_rect(rectangles, offset) else {
                break;
            };
            offset += 8;
            rects.push(r);
        }
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            fill_rects_with_gc_function(img, function, foreground, &rects);
        });
        Ok(())
    }

    fn poly_fill_arc(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        // Each arc is 12 bytes: x(i16) y(i16) w(u16) h(u16) angle1(i16) angle2(i16).
        // angles are in 64ths of a degree (X11 convention).
        // We treat any arc with |angle2| >= 360*64 as a full ellipse and fill it
        // with a scanline approach. Partial arcs fall back to filling the full
        // ellipse for now; xeyes uses full circles so this is sufficient.
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            let img_w = img.0.width() as i32;
            let img_h = img.0.height() as i32;
            let mut rects: Vec<Rectangle16> = Vec::new();
            for chunk in arcs.chunks_exact(12) {
                let ax = i16::from_le_bytes([chunk[0], chunk[1]]) as i32;
                let ay = i16::from_le_bytes([chunk[2], chunk[3]]) as i32;
                let aw = u16::from_le_bytes([chunk[4], chunk[5]]) as i32;
                let ah = u16::from_le_bytes([chunk[6], chunk[7]]) as i32;
                if aw <= 0 || ah <= 0 {
                    continue;
                }
                let cx = ax as f64 + (aw as f64) * 0.5;
                let cy = ay as f64 + (ah as f64) * 0.5;
                let rx = (aw as f64) * 0.5;
                let ry = (ah as f64) * 0.5;
                let y_start = ay.max(0);
                let y_end = (ay + ah).min(img_h);
                for py in y_start..y_end {
                    let dy = (py as f64 + 0.5 - cy) / ry;
                    if dy.abs() > 1.0 {
                        continue;
                    }
                    let dx = (1.0 - dy * dy).sqrt() * rx;
                    let x0 = (cx - dx).floor().max(0.0) as i32;
                    let x1 = (cx + dx).ceil().min(img_w as f64) as i32;
                    if x1 <= x0 {
                        continue;
                    }
                    rects.push(Rectangle16 {
                        x: x0 as i16,
                        y: py as i16,
                        width: (x1 - x0) as u16,
                        height: 1,
                    });
                }
            }
            if !rects.is_empty() {
                fill_rects_with_gc_function(img, function, foreground, &rects);
            }
        });
        Ok(())
    }

    fn fill_poly(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        // Parse i16 vertex pairs.  coord_mode 0 = Origin (absolute), 1 =
        // Previous (deltas from prior vertex).
        let mut verts: Vec<(i32, i32)> = Vec::with_capacity(points.len() / 4);
        let mut offset = 0;
        let mut last = (0i32, 0i32);
        while offset + 4 <= points.len() {
            let Some((x, y)) = read_i16_pair(points, offset) else { break; };
            offset += 4;
            let (xi, yi) = if coord_mode == 1 && !verts.is_empty() {
                (last.0 + x as i32, last.1 + y as i32)
            } else {
                (x as i32, y as i32)
            };
            verts.push((xi, yi));
            last = (xi, yi);
        }
        let mut rects: Vec<Rectangle16> = Vec::new();
        scanline_fill_polygon(&verts, &mut rects);
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            let clipped = clip_rects_to_image(&rects, img.0.width() as i32, img.0.height() as i32);
            fill_rects_with_gc_function(img, function, foreground, &clipped);
        });
        Ok(())
    }

    fn fill_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let rect = Rectangle16 { x, y, width, height };
        let function = self.current_function;
        self.with_image_mut(host_xid, |img| {
            fill_rects_with_gc_function(img, function, foreground, &[rect]);
        });
        Ok(())
    }

    fn poly_text8(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + text_items
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut items = &body[12..];
        let mut cursor_x = x;

        while !items.is_empty() {
            let delta = items[0] as usize;
            items = &items[1..];
            if delta == 0 {
                break; // end of items
            } else if delta == 255 {
                // Font change: skip 3 pad bytes + 4 byte fontable
                if items.len() >= 7 {
                    let font_xid = u32::from_le_bytes([items[3], items[4], items[5], items[6]]);
                    self.current_font = Some(font_xid);
                    items = &items[7..];
                } else {
                    break;
                }
            } else if delta <= 254 {
                // String item: delta bytes follow
                if items.len() >= delta {
                    let text = &items[..delta];
                    self.render_text_string(host_xid, foreground, cursor_x, y, text)?;
                    cursor_x += delta as i32;
                    items = &items[delta..];
                } else {
                    break;
                }
            }
        }
        Ok(())
    }

    fn poly_text16(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        // TODO: implement 16-bit text rendering
        Ok(())
    }

    fn image_text8(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + string(text_len)
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;

        // Draw background rectangle first
        if let Some(font_state) = self.current_font.and_then(|f| self.fonts.get(&f)) {
            let total_width: i32 = body[12..]
                .iter()
                .take(text_len as usize)
                .map(|&b| {
                    font_state
                        .char_info_cache
                        .get(&(b as char))
                        .map(|ci| ci.character_width as i32)
                        .unwrap_or(6)
                })
                .sum();
            let ascent = font_state.metrics.font_ascent as i32;
            let descent = font_state.metrics.font_descent as i32;
            // Clamp to i16/u16 ranges so a buggy font (huge ascent) can't
            // produce a rect that overflows pixman's internal arithmetic.
            let rect = Rectangle16 {
                x: x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                y: (y - ascent).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                width: total_width.clamp(0, u16::MAX as i32) as u16,
                height: (ascent + descent).clamp(0, u16::MAX as i32) as u16,
            };
            let function = self.current_function;
            self.with_image_mut(host_xid, |img| {
                fill_rects_with_gc_function(img, function, background, &[rect]);
            });
        }

        // Render the string (clamp to available body bytes)
        let end = (12usize + text_len as usize).min(body.len());
        let text = &body[12..end];
        self.render_text_string(host_xid, foreground, x, y, text)
    }

    fn image_text16(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _background: u32,
        _text_len: u8,
        _body: &[u8],
    ) -> io::Result<()> {
        // TODO: implement 16-bit image text
        Ok(())
    }

    fn render_create_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_drawable: AnyHandle,
        _ynest_format: u32,
        _value_mask: u32,
        _values: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        let drawable_xid = host_drawable.as_raw();
        let picture_xid = self.next_host_xid();
        self.pictures.insert(
            picture_xid,
            PictureState::Drawable { host_xid: drawable_xid, clip: None },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_change_picture(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_free_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
    ) -> io::Result<()> {
        self.pictures.remove(&host_pic);
        Ok(())
    }

    fn render_create_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        _ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>> {
        Ok(None)
    }

    fn render_free_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        _host_gs: u32,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_add_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        _host_gs: u32,
        _body_tail: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_free_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        _host_gs: u32,
        _glyph_ids: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_composite(
        &mut self,
        _origin: Option<OriginContext>,
        _op: u8,
        _host_src: u32,
        _host_mask: u32,
        _host_dst: u32,
        _src_x: i16,
        _src_y: i16,
        _mask_x: i16,
        _mask_y: i16,
        _dst_x: i16,
        _dst_y: i16,
        _width: u16,
        _height: u16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_composite_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        _minor: u8,
        _op: u8,
        _host_src: u32,
        _host_dst: u32,
        _mask_fmt: u32,
        _host_gs: u32,
        _src_x: i16,
        _src_y: i16,
        _items: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_fill_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        _host_dst: u32,
        _op: u8,
        _color: [u8; 8],
        _rects: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_trapezoids(
        &mut self,
        _origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        if !traps.len().is_multiple_of(40) || traps.is_empty() {
            return Ok(());
        }

        // Translate X RENDER op code to pixman op. X RENDER and pixman share
        // the same numeric values (0=Clear, 1=Src, 2=Dst, 3=Over, …).
        let pixman_op = op as u32;

        // Decode the trap wire bytes element-by-element to avoid alignment UB.
        // Each trap is 40 bytes: top(4), bottom(4), left.p1.x(4), left.p1.y(4),
        // left.p2.x(4), left.p2.y(4), right.p1.x(4), right.p1.y(4),
        // right.p2.x(4), right.p2.y(4).
        let n_traps = traps.len() / 40;
        let mut trap_vec: Vec<pixman::ffi::pixman_trapezoid_t> =
            Vec::with_capacity(n_traps);
        for chunk in traps.chunks_exact(40) {
            let t = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let b = i32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            let lp1x = i32::from_le_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]);
            let lp1y = i32::from_le_bytes([chunk[12], chunk[13], chunk[14], chunk[15]]);
            let lp2x = i32::from_le_bytes([chunk[16], chunk[17], chunk[18], chunk[19]]);
            let lp2y = i32::from_le_bytes([chunk[20], chunk[21], chunk[22], chunk[23]]);
            let rp1x = i32::from_le_bytes([chunk[24], chunk[25], chunk[26], chunk[27]]);
            let rp1y = i32::from_le_bytes([chunk[28], chunk[29], chunk[30], chunk[31]]);
            let rp2x = i32::from_le_bytes([chunk[32], chunk[33], chunk[34], chunk[35]]);
            let rp2y = i32::from_le_bytes([chunk[36], chunk[37], chunk[38], chunk[39]]);
            trap_vec.push(pixman::ffi::pixman_trapezoid_t {
                top: t,
                bottom: b,
                left: pixman::ffi::pixman_line_fixed_t {
                    p1: pixman::ffi::pixman_point_fixed_t { x: lp1x, y: lp1y },
                    p2: pixman::ffi::pixman_point_fixed_t { x: lp2x, y: lp2y },
                },
                right: pixman::ffi::pixman_line_fixed_t {
                    p1: pixman::ffi::pixman_point_fixed_t { x: rp1x, y: rp1y },
                    p2: pixman::ffi::pixman_point_fixed_t { x: rp2x, y: rp2y },
                },
            });
        }

        // Look up source picture — must be SolidFill. Borrow and get raw ptr.
        let src_ptr = match self.pictures.get(&host_src) {
            Some(PictureState::SolidFill { image }) => image.borrow().0.as_ptr(),
            _ => {
                log::debug!(
                    "render_trapezoids: host_src 0x{:x} is not a SolidFill picture; skipping",
                    host_src
                );
                return Ok(());
            }
        };

        // Look up destination picture — must be Drawable. Extract host_xid
        // and any clip info, then release the pictures borrow before we
        // mutably borrow the drawable image below.
        let (drawable_xid, clip) = match self.pictures.get(&host_dst) {
            Some(PictureState::Drawable { host_xid, clip }) => {
                (*host_xid, clip.clone())
            }
            _ => {
                log::debug!(
                    "render_trapezoids: host_dst 0x{:x} is not a Drawable picture; skipping",
                    host_dst
                );
                return Ok(());
            }
        };

        // Apply clip if set, composite traps, then clear clip.
        // We need to borrow the dst image mutably; use with_image_mut.
        // src_ptr is valid for the duration of this call because self.pictures
        // is not modified between obtaining src_ptr and the composite call.
        self.with_image_mut(drawable_xid, |dst| {
            // SAFETY: dst.0.as_ptr() returns a valid *mut pixman_image_t that
            // pixman allocated and that we own (inside RefCell<PixmanImage>).
            // src_ptr was obtained from another PixmanImage we also own and
            // that outlives this call (src and dst are different pictures by
            // checked contract). trap_vec is a Vec we own. n_traps matches
            // trap_vec.len().
            let dst_ptr = dst.0.as_ptr();

            // Apply clip region if present.
            if let Some(ref rects) = clip {
                use pixman::{Box32, Region32};
                let boxes: Vec<Box32> = rects
                    .iter()
                    .map(|r| Box32 {
                        x1: r.x as i32,
                        y1: r.y as i32,
                        x2: r.x as i32 + r.width as i32,
                        y2: r.y as i32 + r.height as i32,
                    })
                    .collect();
                let region = Region32::init_rects(boxes.as_slice());
                let _ = dst.0.set_clip_region32(Some(&region));
            }

            unsafe {
                pixman::ffi::pixman_composite_trapezoids(
                    pixman_op,
                    src_ptr,
                    dst_ptr,
                    // Use PIXMAN_a8 as the mask-format for anti-aliased
                    // trap coverage. X RENDER mask format IDs are opaque
                    // to us; a8 gives 256-level AA which is correct for
                    // all common cases.
                    pixman::ffi::pixman_format_code_t_PIXMAN_a8,
                    src_x as std::os::raw::c_int,
                    src_y as std::os::raw::c_int,
                    x_off as std::os::raw::c_int,
                    y_off as std::os::raw::c_int,
                    trap_vec.len() as std::os::raw::c_int,
                    trap_vec.as_ptr(),
                );
            }

            // Clear clip after composite to avoid stale clip affecting
            // subsequent operations on this image.
            if clip.is_some() {
                let _ = dst.0.set_clip_region32(None);
            }
        });

        Ok(())
    }

    fn render_create_solid_fill(
        &mut self,
        _origin: Option<OriginContext>,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>> {
        // X RENDER CreateSolidFill color: 16-bit per channel, little-endian.
        // Byte layout: red[0..2], green[2..4], blue[4..6], alpha[6..8].
        let r = u16::from_le_bytes([color[0], color[1]]);
        let g = u16::from_le_bytes([color[2], color[3]]);
        let b = u16::from_le_bytes([color[4], color[5]]);
        let a = u16::from_le_bytes([color[6], color[7]]);
        let pixman_color = Color::new(r, g, b, a);

        // Create a 1×1 A8R8G8B8 image, fill it, and set repeat so it tiles.
        let mut img = PixmanImage::new(FormatCode::A8R8G8B8, 1, 1, true)?;
        let _ = img.0.fill_rectangles(
            Operation::Src,
            pixman_color,
            &[Rectangle16 { x: 0, y: 0, width: 1, height: 1 }],
        );
        img.0.set_repeat(Repeat::Normal);

        let picture_xid = self.next_host_xid();
        self.pictures.insert(
            picture_xid,
            PictureState::SolidFill { image: RefCell::new(img) },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_linear_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        _body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        Ok(None)
    }

    fn render_create_radial_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        _body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        Ok(None)
    }

    fn render_create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _host_src_pic: PictureHandle,
        _x: u16,
        _y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        Ok(None)
    }

    fn render_set_picture_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Wire body (passed through from nested.rs): picture(4) +
        // clip_x_origin(INT16) + clip_y_origin(INT16) + N × [x y w h].
        // The picture XID has already been resolved to host_pic; we just
        // skip past it. Origin offset is stored but not applied — xclock
        // sets it to (0,0) and that's all we currently exercise.
        if body.len() < 8 {
            return Ok(());
        }
        let _x_origin = i16::from_le_bytes([body[4], body[5]]);
        let _y_origin = i16::from_le_bytes([body[6], body[7]]);
        let rects_data = &body[8..];
        let mut rects = Vec::with_capacity(rects_data.len() / 8);
        for chunk in rects_data.chunks_exact(8) {
            let x = i16::from_le_bytes([chunk[0], chunk[1]]);
            let y = i16::from_le_bytes([chunk[2], chunk[3]]);
            let w = u16::from_le_bytes([chunk[4], chunk[5]]);
            let h = u16::from_le_bytes([chunk[6], chunk[7]]);
            rects.push(Rectangle16 { x, y, width: w, height: h });
        }
        if let Some(PictureState::Drawable { clip, .. }) = self.pictures.get_mut(&host_pic) {
            *clip = if rects.is_empty() { None } else { Some(rects) };
        }
        // SolidFill pictures: clip is a no-op.
        Ok(())
    }

    fn render_set_picture_filter(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_set_picture_transform(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_query_version(&mut self, _origin: Option<OriginContext>) -> io::Result<(u32, u32)> {
        Ok((1, 1))
    }

    fn xkb_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        _minor: u8,
        _body: &[u8],
    ) -> io::Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn xfixes_change_cursor_by_name(
        &mut self,
        _origin: Option<OriginContext>,
        _host_cursor_xid: u32,
        _name_bytes: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn set_shape_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _kind: u8,
        _rects: &[xfixes::RegionRect],
    ) -> io::Result<()> {
        Ok(())
    }

    fn warp_pointer(
        &mut self,
        _origin: Option<OriginContext>,
        _dst_host_xid: u32,
        _dst_x: i16,
        _dst_y: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn query_pointer(&mut self, _origin: Option<OriginContext>) -> io::Result<PointerPosition> {
        Ok(PointerPosition {
            same_screen: true,
            win_x: self.cursor_x as i16,
            win_y: self.cursor_y as i16,
            mask: self.serialize_modifiers(),
        })
    }

    fn list_fonts_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        _max_names: u16,
        _pattern: &str,
    ) -> io::Result<Vec<u8>> {
        // Return a valid 32-byte ListFonts reply with zero names so the
        // client doesn't block waiting on us.  Layout:
        //   [0]      reply type = 1
        //   [1]      unused
        //   [2..4]   sequence (rewritten by caller)
        //   [4..8]   reply length (extra 4-byte units) = 0
        //   [8..10]  number-of-names = 0
        //   [10..32] unused/pad
        let mut reply = vec![0u8; 32];
        reply[0] = 1;
        Ok(reply)
    }

    fn list_fonts_with_info_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        _max_names: u16,
        _pattern: &str,
    ) -> io::Result<Vec<Vec<u8>>> {
        // ListFontsWithInfo sends one reply per font and a final
        // terminator reply with `name-length == 0` to signal end of list.
        // Send only the terminator so clients unblock.  Reply size is 60
        // bytes (32-byte header + 28 bytes of font-info fields, all zero).
        let mut term = vec![0u8; 60];
        term[0] = 1; // reply type
        Ok(vec![term])
    }

    fn get_atom_name(
        &mut self,
        _origin: Option<OriginContext>,
        _atom: u32,
    ) -> io::Result<Option<String>> {
        Ok(None)
    }

    fn get_keyboard_mapping(
        &mut self,
        _origin: Option<OriginContext>,
        first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)> {
        let mut rows: Vec<Vec<u32>> = Vec::new();
        let max_kc = (first_keycode as u16) + (count as u16);
        for kc in first_keycode as u16..max_kc {
            let xkb_kc = xkbcommon::xkb::Keycode::new(kc as u32);
            let syms = self.xkb_keymap.0.key_get_syms_by_level(xkb_kc, 0, 0);
            rows.push(syms.iter().map(|s| s.raw()).collect());
        }
        let max_levels = rows.iter().map(|r| r.len()).max().unwrap_or(1) as u8;
        let mut flat = Vec::with_capacity((count as usize) * (max_levels as usize));
        for row in &rows {
            flat.extend_from_slice(row);
            let pad = max_levels as usize - row.len();
            flat.resize(flat.len() + pad, 0); // NoSymbol padding
        }
        Ok((max_levels, flat))
    }

    fn get_modifier_mapping(
        &mut self,
        _origin: Option<OriginContext>,
    ) -> io::Result<(u8, Vec<u8>)> {
        // Conventional defaults: 8 rows, up to 4 keycodes each
        // Shift(0x32,0x3E), Lock(0x42), Control(0x25,0x69),
        // Mod1(0x40,0x6C), Mod2(0x4D), Mod3(0x73), Mod4(0x85,0x86), Mod5(empty)
        // Encoded as count + flat vec of 8*4 = 32 bytes
        let data: Vec<u8> = vec![
            0x32, 0x3E, 0, 0, // Shift
            0x42, 0, 0, 0, // Lock
            0x25, 0x69, 0, 0, // Control
            0x40, 0x6C, 0, 0, // Mod1
            0x4D, 0, 0, 0, // Mod2
            0x73, 0, 0, 0, // Mod3
            0x85, 0x86, 0, 0, // Mod4
            0, 0, 0, 0, // Mod5
        ];
        Ok((4, data))
    }
}

#[cfg(test)]
mod tests {
    use pixman::{Color, FormatCode, Image, Operation, Rectangle16, Repeat};
    use yserver_core::backend::GcFunction;

    use super::{PixmanImage, fill_rects_with_gc_function};

    // ---------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------

    /// Fill a PixmanImage with a solid 24-bit colour (X8R8G8B8 format).
    fn fill_image(img: &mut PixmanImage, pixel: u32) {
        let color = super::color_from_u32(pixel);
        let w = img.0.width() as u16;
        let h = img.0.height() as u16;
        let _ = img.0.fill_rectangles(
            Operation::Src,
            color,
            &[Rectangle16 { x: 0, y: 0, width: w, height: h }],
        );
    }

    /// Read the packed X8R8G8B8 pixel at (x, y) from a PixmanImage.
    fn read_pixel(img: &PixmanImage, x: usize, y: usize) -> u32 {
        let stride_words = img.0.stride() / 4;
        // SAFETY: x, y are within the image bounds (caller's responsibility).
        unsafe { *img.0.data().add(y * stride_words + x) }
    }

    // ---------------------------------------------------------------------------
    // GcFunction::Copy: fill_rects_with_gc_function must overwrite the destination
    // ---------------------------------------------------------------------------

    #[test]
    fn fill_rects_copy_overwrites_destination() {
        let mut img = PixmanImage::new(FormatCode::X8R8G8B8, 4, 4, true).unwrap();
        fill_image(&mut img, 0x00ff_ffff); // white
        let rect = Rectangle16 { x: 0, y: 0, width: 4, height: 4 };
        fill_rects_with_gc_function(&mut img, GcFunction::Copy, 0x00ff_00ff, &[rect]);
        let pixel = read_pixel(&img, 1, 1);
        assert_eq!(pixel & 0x00ff_ffff, 0x00ff_00ff, "Copy should overwrite with magenta");
    }

    // ---------------------------------------------------------------------------
    // GcFunction::Xor: must produce bitwise XOR of destination and foreground.
    //
    // NOTE: KmsBackend requires DRM hardware and cannot be constructed in a unit
    // test.  This test verifies XOR semantics at the PixmanImage level by calling
    // fill_rects_with_gc_function() directly — the same helper invoked by every
    // client-draw primitive (poly_segment, poly_line, fill_rectangle, …).
    //
    // NOTE: pixman's Porter-Duff PIXMAN_OP_XOR produces zero for fully-opaque
    // images (src*(1-dst.a) + dst*(1-src.a) = 0 when both alphas are 1).
    // fill_rects_with_gc_function implements GcFunction::Xor as a manual bitwise
    // XOR over the RGB channels to match X11 GXxor semantics.
    // ---------------------------------------------------------------------------

    #[test]
    fn poly_segment_xor_inverts_destination_pixels() {
        // Create a 16×16 image pre-filled with white (0x00FFFFFF).
        let mut img = PixmanImage::new(FormatCode::X8R8G8B8, 16, 16, true).unwrap();
        fill_image(&mut img, 0x00ff_ffff); // white

        // Draw a horizontal line at y=8 with magenta (0x00FF00FF) using XOR.
        let row: Vec<Rectangle16> = (0..16_i16)
            .map(|x| Rectangle16 { x, y: 8, width: 1, height: 1 })
            .collect();
        fill_rects_with_gc_function(&mut img, GcFunction::Xor, 0x00ff_00ff, &row);

        // White (0xFFFFFF) XOR magenta (0xFF00FF) = green (0x00FF00).
        let pixel = read_pixel(&img, 8, 8);
        assert_eq!(
            pixel & 0x00ff_ffff,
            0x0000_ff00,
            "expected green (0x00FF00), got 0x{:08x}",
            pixel
        );

        // Pixels outside the drawn row must remain white.
        let untouched = read_pixel(&img, 8, 0);
        assert_eq!(
            untouched & 0x00ff_ffff,
            0x00ff_ffff,
            "pixel at (8,0) should be untouched white"
        );
    }

    // ---------------------------------------------------------------------------
    // Glyph rendering: verify freetype GRAY mode + pixman A8 stride handling.
    //
    // This test does NOT require DRM hardware.  It loads a font via freetype,
    // renders a single glyph, and composites it onto a white pixman image using
    // exactly the same path as render_text_string.
    // ---------------------------------------------------------------------------

    #[test]
    fn glyph_render_gray_pixels_land_on_correct_rows() {
        // ------------------------------------------------------------------
        // 1. Load font and render glyph 'A'.
        // ------------------------------------------------------------------
        let lib = freetype::Library::init().expect("freetype init");
        let candidates = [
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
        ];
        let face = candidates
            .iter()
            .find_map(|p| lib.new_face(p, 0).ok())
            .expect("DejaVuSansMono.ttf not found — install dejavu fonts");
        let _ = face.set_char_size(12 << 6, 12 << 6, 96, 96);
        let _ = face.load_char('A' as usize, freetype::face::LoadFlag::RENDER);
        let glyph = face.glyph();
        let bitmap = glyph.bitmap();

        // Must be GRAY (8bpp) — not MONO.  RENDER flag on an outline font
        // always produces GRAY; MONO would indicate an embedded bitmap strike.
        let pm = bitmap.pixel_mode().expect("pixel_mode");
        assert_eq!(
            pm,
            freetype::bitmap::PixelMode::Gray,
            "expected GRAY pixel mode, got {:?}",
            pm
        );

        let w = bitmap.width() as usize;
        let h = bitmap.rows() as usize;
        let pitch = bitmap.pitch();
        let buf = bitmap.buffer();

        assert!(w > 0 && h > 0, "glyph 'A' should have non-empty bitmap");
        // For GRAY the pitch in bytes >= width in pixels.
        assert!(pitch >= 0, "expected positive (downward) pitch");
        assert!(pitch as usize >= w, "pitch should be >= width for GRAY");

        // ------------------------------------------------------------------
        // 2. Copy glyph pixels into a flat Vec (same logic as render_text_string).
        // ------------------------------------------------------------------
        let mut pixels = vec![0u8; w * h];
        for row in 0..h {
            let src = if pitch >= 0 {
                row * pitch as usize
            } else {
                (h - 1 - row) * (pitch as isize).unsigned_abs()
            };
            pixels[row * w..row * w + w].copy_from_slice(&buf[src..src + w]);
        }

        // At least some pixels must be non-zero (the glyph is not blank).
        let has_nonzero = pixels.iter().any(|&b| b > 0);
        assert!(has_nonzero, "glyph pixels should contain non-zero alpha values");

        // ------------------------------------------------------------------
        // 3. Write into a pixman A8 image using stride (same as phase 2).
        // ------------------------------------------------------------------
        let glyph_img = Image::new(FormatCode::A8, w, h, true)
            .expect("pixman A8 image");
        let stride_bytes = glyph_img.stride();
        // stride_bytes must be >= w (pixman pads A8 rows to 4-byte alignment).
        assert!(stride_bytes >= w, "pixman A8 stride must be >= width");

        let gdata = unsafe { glyph_img.data() } as *mut u8;
        for row in 0..h {
            for col in 0..w {
                unsafe {
                    *gdata.add(row * stride_bytes + col) = pixels[row * w + col];
                }
            }
        }

        // Verify that the A8 image contains non-zero bytes in its first row.
        let first_row_nonzero = (0..w).any(|col| {
            unsafe { *gdata.add(col) > 0 }
        });
        assert!(first_row_nonzero, "A8 image first row should have non-zero alpha");

        // ------------------------------------------------------------------
        // 4. Composite onto a white X8R8G8B8 image and verify pixels changed.
        //
        // We use bitmap_top to position the glyph correctly: the baseline is
        // at y = bitmap_top (so the glyph top is at row 0, baseline at
        // bitmap_top). With a foreground of black (0x000000) on white
        // (0xFFFFFF), composited pixels should be darker than 0xFFFFFF.
        // ------------------------------------------------------------------
        let baseline_y = glyph.bitmap_top() as i32;  // rows from top to baseline
        let img_h = (baseline_y + 4).max(h as i32 + 4) as u16;
        let img_w = (w + 4) as u16;
        let mut dst = PixmanImage::new(FormatCode::X8R8G8B8, img_w, img_h, true)
            .expect("dst image");
        fill_image(&mut dst, 0x00ff_ffff); // white

        let mut color_img = Image::new(FormatCode::A8R8G8B8, 1, 1, true)
            .expect("color image");
        let black = Color::new(0, 0, 0, 0xffff);
        let _ = color_img.fill_rectangles(
            Operation::Src,
            black,
            &[Rectangle16 { x: 0, y: 0, width: 1, height: 1 }],
        );
        // Must tile across the glyph — same fix as render_text_string.
        color_img.set_repeat(Repeat::Normal);

        // dst_y = baseline_y - bitmap_top = 0 (glyph top lands on row 0).
        let dst_y = baseline_y - glyph.bitmap_top();
        dst.0.composite32(
            Operation::Over,
            &color_img,
            Some(&glyph_img),
            (0, 0),
            (0, 0),
            (0, dst_y),
            (w as i32, h as i32),
        );

        // The destination should no longer be all-white: the composited 'A'
        // glyph (black foreground) should have darkened some pixels.
        let any_changed = (0..img_w as usize).any(|x| {
            (0..img_h as usize).any(|y| read_pixel(&dst, x, y) & 0x00ff_ffff != 0x00ff_ffff)
        });
        assert!(any_changed, "composite should darken some white pixels with black 'A'");
    }

    // ---------------------------------------------------------------------------
    // RENDER picture + trapezoid tests.
    //
    // KmsBackend requires DRM hardware so we cannot instantiate it here.
    // Instead we exercise the same Pixman logic that render_trapezoids uses,
    // calling pixman_composite_trapezoids directly with a solid-fill 1×1 source
    // image and an A8R8G8B8 destination.
    // ---------------------------------------------------------------------------

    /// Encode one X RENDER Trapezoid (40 bytes, little-endian 16.16 fixed).
    #[allow(clippy::too_many_arguments)]
    fn encode_trap(
        top: i32, bottom: i32,
        lp1x: i32, lp1y: i32, lp2x: i32, lp2y: i32,
        rp1x: i32, rp1y: i32, rp2x: i32, rp2y: i32,
    ) -> Vec<u8> {
        let mut buf = Vec::with_capacity(40);
        for v in [top, bottom, lp1x, lp1y, lp2x, lp2y, rp1x, rp1y, rp2x, rp2y] {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf
    }

    /// Decode the wire bytes for one trap into a pixman_trapezoid_t.
    fn decode_trap(bytes: &[u8]) -> pixman::ffi::pixman_trapezoid_t {
        assert_eq!(bytes.len(), 40);
        let i32_at = |off: usize| i32::from_le_bytes([bytes[off], bytes[off+1], bytes[off+2], bytes[off+3]]);
        pixman::ffi::pixman_trapezoid_t {
            top:    i32_at(0),
            bottom: i32_at(4),
            left: pixman::ffi::pixman_line_fixed_t {
                p1: pixman::ffi::pixman_point_fixed_t { x: i32_at(8),  y: i32_at(12) },
                p2: pixman::ffi::pixman_point_fixed_t { x: i32_at(16), y: i32_at(20) },
            },
            right: pixman::ffi::pixman_line_fixed_t {
                p1: pixman::ffi::pixman_point_fixed_t { x: i32_at(24), y: i32_at(28) },
                p2: pixman::ffi::pixman_point_fixed_t { x: i32_at(32), y: i32_at(36) },
            },
        }
    }

    #[test]
    fn render_trapezoids_over_produces_nonzero_alpha_in_dst() {
        // Destination: 8×8 A8R8G8B8, cleared to transparent black.
        let dst_img = PixmanImage::new(FormatCode::A8R8G8B8, 8, 8, true).unwrap();

        // Source: 1×1 solid red (fully opaque), with REPEAT_NORMAL so it tiles.
        let mut src_img = Image::new(FormatCode::A8R8G8B8, 1, 1, true).unwrap();
        let red = Color::new(0xFFFF, 0x0000, 0x0000, 0xFFFF);
        let _ = src_img.fill_rectangles(
            Operation::Src,
            red,
            &[Rectangle16 { x: 0, y: 0, width: 1, height: 1 }],
        );
        src_img.set_repeat(Repeat::Normal);

        // A rectangle trap covering pixels (1,1)–(6,6).
        // In 16.16 fixed: pixel N → N << 16.
        let left_x  = 1i32 << 16;
        let right_x = 6i32 << 16;
        let top_y   = 1i32 << 16;
        let bot_y   = 6i32 << 16;
        let wire = encode_trap(
            top_y, bot_y,
            left_x, top_y, left_x, bot_y,   // left edge: vertical at x=1
            right_x, top_y, right_x, bot_y, // right edge: vertical at x=6
        );
        let trap_struct = decode_trap(&wire);

        // SAFETY: both images are valid, non-overlapping pixman images owned by
        // this stack frame.  trap_struct is POD constructed above.
        unsafe {
            pixman::ffi::pixman_composite_trapezoids(
                pixman::ffi::pixman_op_t_PIXMAN_OP_OVER,
                src_img.as_ptr(),
                dst_img.0.as_ptr(),
                pixman::ffi::pixman_format_code_t_PIXMAN_a8,
                0, 0, // src_x, src_y
                0, 0, // dst_x, dst_y
                1,
                &trap_struct,
            );
        }

        // Center pixel (3,3) must have nonzero alpha after the composite.
        let stride_words = dst_img.0.stride() / 4;
        let pixel = unsafe { *dst_img.0.data().add(3 * stride_words + 3) };
        let alpha = (pixel >> 24) & 0xFF;
        assert!(
            alpha > 0,
            "center pixel at (3,3) should have nonzero alpha after trap composite; got 0x{:08x}",
            pixel
        );

        // And pixels outside the trap (e.g. (0,0)) must remain transparent.
        let corner = unsafe { *dst_img.0.data().add(0) };
        assert_eq!(
            (corner >> 24) & 0xFF,
            0,
            "pixel at (0,0) outside trap should remain transparent; got 0x{:08x}",
            corner
        );
    }

    #[test]
    fn render_trapezoids_center_pixel_carries_source_color() {
        // Destination: 8×8 A8R8G8B8, cleared to transparent black.
        let dst_img = PixmanImage::new(FormatCode::A8R8G8B8, 8, 8, true).unwrap();

        // Source: solid green (0x00FF00), fully opaque.
        let mut src_img = Image::new(FormatCode::A8R8G8B8, 1, 1, true).unwrap();
        let green = Color::new(0x0000, 0xFFFF, 0x0000, 0xFFFF);
        let _ = src_img.fill_rectangles(
            Operation::Src,
            green,
            &[Rectangle16 { x: 0, y: 0, width: 1, height: 1 }],
        );
        src_img.set_repeat(Repeat::Normal);

        // Rectangular trap covering the full image interior (1,1)–(6,6).
        let l  = 1i32 << 16;
        let r  = 6i32 << 16;
        let t  = 1i32 << 16;
        let b  = 6i32 << 16;
        let trap_struct = decode_trap(&encode_trap(t, b, l, t, l, b, r, t, r, b));

        unsafe {
            pixman::ffi::pixman_composite_trapezoids(
                pixman::ffi::pixman_op_t_PIXMAN_OP_OVER,
                src_img.as_ptr(),
                dst_img.0.as_ptr(),
                pixman::ffi::pixman_format_code_t_PIXMAN_a8,
                0, 0, 0, 0, 1, &trap_struct,
            );
        }

        // Center pixel (3,3): alpha must be 0xFF and RGB must be pure green.
        let stride_words = dst_img.0.stride() / 4;
        let pixel = unsafe { *dst_img.0.data().add(3 * stride_words + 3) };
        let a = (pixel >> 24) & 0xFF;
        let r_ch = (pixel >> 16) & 0xFF;
        let g_ch = (pixel >> 8)  & 0xFF;
        let b_ch =  pixel        & 0xFF;
        assert_eq!(a, 0xFF, "center alpha should be fully opaque");
        assert_eq!(r_ch, 0x00, "center red channel should be 0");
        assert_eq!(g_ch, 0xFF, "center green channel should be 0xFF");
        assert_eq!(b_ch, 0x00, "center blue channel should be 0");
    }
}
