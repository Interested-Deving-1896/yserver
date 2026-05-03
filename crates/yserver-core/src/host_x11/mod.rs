mod pump;
mod request;
mod trait_impl;

pub use pump::{
    HostConfigureEvent, HostEvent, HostExposeEvent, HostInputPump, HostInputPumpHandle,
    HostPointerEvent, HostSubwindowConfig, PointerEventKind, PointerPosition,
};

use std::{
    collections::HashMap,
    io::{self, ErrorKind, Read, Write},
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
};

use log::debug;
use yserver_protocol::x11::ResourceId;

use crate::backend::{PixmapHandle, WindowHandle};

use pump::{HostSetup, connect_to_host, read_setup_reply};

struct HostRenderInfo {
    opcode: u8,
    fmt_a1: u32,
    fmt_a8: u32,
    fmt_rgb24: u32,
    fmt_argb32: u32,
}

struct HostXkbInfo {
    opcode: u8,
    first_event: u8,
    first_error: u8,
}

pub struct HostX11Backend {
    stream: UnixStream,
    window_id: u32,
    gc_id: u32,
    current_foreground: u32,
    current_background: u32,
    current_clip: HostClipState,
    current_fill: HostFillState,
    /// Phase 6.2 additive scope: cached values on the host's shared GC
    /// for the GC attributes that yserver-core now forwards. Setting
    /// these on first use means we don't re-issue identical ChangeGC's.
    /// Using `None` for the initial value means "host default, don't
    /// know exact byte" — the first non-default request will issue a
    /// ChangeGC unconditionally.
    current_function: Option<u8>,
    current_plane_mask: Option<u32>,
    current_line_width: Option<u16>,
    current_line_style: Option<u8>,
    current_cap_style: Option<u8>,
    current_join_style: Option<u8>,
    current_fill_rule: Option<u8>,
    current_subwindow_mode: Option<u8>,
    current_graphics_exposures: Option<bool>,
    current_dash_offset: Option<i16>,
    current_dashes: Option<Vec<u8>>,
    current_arc_mode: Option<u8>,
    sequence: u16,
    next_xid_counter: u32,
    render: Option<HostRenderInfo>,
    xkb: Option<HostXkbInfo>,
    /// Major opcode of the host's SHAPE extension, cached on init. `None`
    /// means the host doesn't advertise SHAPE — forwarders become no-ops.
    shape_opcode: Option<u8>,
    /// Major opcode of the host's XFIXES extension. Used so far only by
    /// `ChangeCursorByName`; other XFIXES requests are still served locally.
    xfixes_opcode: Option<u8>,
    /// Major opcode of the host's COMPOSITE extension. Used to forward
    /// `Composite::NameWindowPixmap` so that compositors (picom, mutter)
    /// see actual host backing-store contents through our nested layer.
    /// `None` means the host doesn't advertise COMPOSITE — clients then
    /// receive `BadAlloc` for `NameWindowPixmap`.
    composite_opcode: Option<u8>,
    /// Host XID of the host root visual. Pushed into `ResourceTable` so
    /// that core CreateWindow forwarding for our `ROOT_VISUAL` resolves
    /// to a real host visual.
    root_visual_xid: u32,
    /// Host XID of an ARGB (32-bit TrueColor) visual on the host, if
    /// one was advertised at setup. `None` means we can't honour
    /// `ARGB_VISUAL` for top-level CreateWindow on this host.
    argb_visual_xid: Option<u32>,
    /// Host XID of a colormap allocated for `argb_visual_xid` during
    /// init. Required by `CreateWindow` whenever the child visual is
    /// not `CopyFromParent`.
    argb_colormap_xid: Option<u32>,
    // Responses read during create_subwindow drain loops that belong to future
    // requests (sequence > geom_seq at time of read). Without this buffer,
    // the drain loop for window N discards the GetGeometry reply for window N+k,
    // causing the subsequent drain loop to hang forever.
    reply_buffer: Vec<HostResponse>,
    // GCs cached per pixmap depth. The default `gc_id` is bound to a depth-24
    // drawable so PutImage onto pixmaps with a different depth (e.g. depth-8
    // alpha masks for RENDER) would BadMatch. We lazily create one GC per
    // depth using the target drawable as the screen-and-depth reference.
    depth_gcs: HashMap<u8, u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostClipRectangles {
    pub ordering: u8,
    pub x_origin: i16,
    pub y_origin: i16,
    pub rectangles: Vec<u8>,
}

/// Tracks what clip-state the host shared GC currently has, so we don't
/// re-issue identical `SetClipRectangles` / `ChangeGC(clip-mask)` calls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum HostClipState {
    /// `clip-mask = None` — no clipping, draw everywhere.
    None,
    /// Clip to a list of rectangles set via `SetClipRectangles`.
    Rectangles(HostClipRectangles),
    /// Clip to the 1-bits of a depth-1 host pixmap, shifted by
    /// `(x_origin, y_origin)`. Used by wmaker for window-decoration
    /// symbols (close-button "X" etc.).
    Pixmap {
        host_pixmap: u32,
        x_origin: i16,
        y_origin: i16,
    },
}

/// Tracks the fill-style on the host shared GC so we don't re-issue
/// identical `ChangeGC(fill-style+tile)` calls. e16 paints popup
/// backgrounds via Tiled fill; the fill handlers must flip to Tiled
/// before the draw and back to Solid after, otherwise other clients'
/// later draws would inherit the tile pixmap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HostFillState {
    Solid,
    Tiled {
        host_pixmap: u32,
        x_origin: i16,
        y_origin: i16,
    },
}

/// Visual / depth / colormap selector for [`HostX11Backend::create_subwindow`].
/// `CopyFromParent` is the historical path — depth=0, visual=0, no
/// colormap value — used when the requested child visual matches the
/// host container's visual. `Explicit` carries the host xids needed to
/// honour ARGB top-levels: the host requires both a real visual id and
/// a colormap whose visual matches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostSubwindowVisual {
    CopyFromParent,
    Explicit {
        depth: u8,
        visual_xid: u32,
        colormap_xid: u32,
    },
}

impl HostSubwindowVisual {
    pub(super) fn depth(self) -> u8 {
        match self {
            Self::CopyFromParent => 0,
            Self::Explicit { depth, .. } => depth,
        }
    }

    pub(super) fn visual_xid(self) -> u32 {
        match self {
            Self::CopyFromParent => 0,
            Self::Explicit { visual_xid, .. } => visual_xid,
        }
    }
}

pub type HostXidMap = Arc<Mutex<HashMap<u32, ResourceId>>>;

impl HostX11Backend {
    pub fn open_from_env(width: u16, height: u16) -> io::Result<Self> {
        let mut stream = connect_to_host()?;
        let setup = read_setup_reply(&mut stream)?;
        let window_id = setup.resource_id_base;
        let gc_id = setup.resource_id_base + 1;
        let font_id = setup.resource_id_base + 2;
        create_window(&mut stream, &setup, window_id, width, height)?;
        open_font(&mut stream, font_id, b"fixed")?;
        create_gc(
            &mut stream,
            window_id,
            gc_id,
            setup.black_pixel,
            setup.white_pixel,
            font_id,
        )?;
        map_window(&mut stream, window_id)?;
        stream.flush()?;

        let mut this = Self {
            stream,
            window_id,
            gc_id,
            current_foreground: setup.black_pixel,
            current_background: setup.white_pixel,
            current_clip: HostClipState::None,
            current_fill: HostFillState::Solid,
            current_function: None,
            current_plane_mask: None,
            current_line_width: None,
            current_line_style: None,
            current_cap_style: None,
            current_join_style: None,
            current_fill_rule: None,
            current_subwindow_mode: None,
            current_graphics_exposures: None,
            current_dash_offset: None,
            current_dashes: None,
            current_arc_mode: None,
            sequence: 5,
            next_xid_counter: setup.resource_id_base + 3,
            render: None,
            xkb: None,
            shape_opcode: None,
            xfixes_opcode: None,
            composite_opcode: None,
            reply_buffer: Vec::new(),
            depth_gcs: HashMap::new(),
            root_visual_xid: setup.root_visual,
            argb_visual_xid: setup.argb_visual,
            argb_colormap_xid: None,
        };
        this.render = this.init_render().ok();
        this.xkb = this.init_xkb().ok();
        this.shape_opcode = this.query_extension_opcode(b"SHAPE").ok().flatten();
        if this.shape_opcode.is_none() {
            log::info!("host SHAPE extension absent — top-level shape forwarding disabled");
        }
        this.xfixes_opcode = this.query_extension_opcode(b"XFIXES").ok().flatten();
        if this.xfixes_opcode.is_none() {
            log::info!("host XFIXES extension absent — cursor-by-name forwarding disabled");
        }
        this.composite_opcode = this.query_extension_opcode(b"Composite").ok().flatten();
        if this.composite_opcode.is_none() {
            log::info!("host COMPOSITE extension absent — NameWindowPixmap will return BadAlloc");
        }
        if let Some(argb_visual) = this.argb_visual_xid {
            match this.create_argb_colormap(setup.root, argb_visual) {
                Ok(xid) => this.argb_colormap_xid = Some(xid),
                Err(err) => {
                    log::warn!(
                        "could not allocate host ARGB colormap (visual=0x{argb_visual:x}): {err}; \
                         ARGB CreateWindow will fall back to CopyFromParent"
                    );
                }
            }
        } else {
            log::info!("host advertises no depth-32 TrueColor visual — ARGB CreateWindow disabled");
        }
        Ok(this)
    }

    /// Host XIDs the upper layer pushes into the visual / colormap
    /// tables in `ResourceTable`. `argb_*` are `None` when the host
    /// has no depth-32 TrueColor visual.
    pub fn root_visual_xid(&self) -> u32 {
        self.root_visual_xid
    }

    pub fn argb_visual_xid(&self) -> Option<u32> {
        self.argb_visual_xid
    }

    pub fn argb_colormap_xid(&self) -> Option<u32> {
        self.argb_colormap_xid
    }

    /// Allocate a host colormap for our ARGB visual via `XCreateColormap(
    /// alloc=None, mid, root, visual)`. Sent fire-and-forget — host errors
    /// (visual not depth-32, etc.) become async and are absorbed silently;
    /// the resulting xid is still returned but if the host failed, later
    /// CreateWindow attempts using it will surface a host BadColor / BadValue
    /// (also absorbed). This is acceptable here — the alternative is a
    /// blocking sync round-trip during HostX11Backend init.
    fn create_argb_colormap(&mut self, host_root: u32, argb_visual: u32) -> io::Result<u32> {
        let cmap_id = self.next_xid();
        let mut out = Vec::with_capacity(16);
        out.push(78); // CreateColormap opcode
        out.push(0); // alloc = None
        write_u16(&mut out, 4); // length = 4 words
        write_u32(&mut out, cmap_id);
        write_u32(&mut out, host_root);
        write_u32(&mut out, argb_visual);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.sequence = self.sequence.wrapping_add(1);
        Ok(cmap_id)
    }

    /// Major opcode of the host's COMPOSITE extension, or `None` if the
    /// host didn't advertise it at startup. The nested COMPOSITE handler
    /// uses this to gate `NameWindowPixmap` forwarding.
    #[must_use]
    pub fn composite_opcode(&self) -> Option<u8> {
        self.composite_opcode
    }

    /// Forward `Composite::NameWindowPixmap(window, pixmap)` to the host.
    /// Caller is responsible for validating `host_window` is a redirected
    /// host top-level. Allocates a fresh host pixmap XID and returns it.
    /// No reply is generated by the host.
    pub fn name_window_pixmap(&mut self, host_window: WindowHandle) -> io::Result<PixmapHandle> {
        let Some(major) = self.composite_opcode else {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "host COMPOSITE extension not available",
            ));
        };
        let host_pixmap = self.next_xid();
        // Wire layout: opcode(1) minor(1) length(2 = 3) window(4) pixmap(4)
        let mut out = [0u8; 12];
        out[0] = major;
        out[1] = yserver_protocol::x11::composite::NAME_WINDOW_PIXMAP;
        out[2..4].copy_from_slice(&3u16.to_le_bytes());
        out[4..8].copy_from_slice(&host_window.as_raw().to_le_bytes());
        out[8..12].copy_from_slice(&host_pixmap.to_le_bytes());
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.sequence = self.sequence.wrapping_add(1);
        Ok(PixmapHandle::from_raw_panicking(host_pixmap))
    }

    pub(super) fn next_xid(&mut self) -> u32 {
        let xid = self.next_xid_counter;
        self.next_xid_counter = self.next_xid_counter.wrapping_add(1);
        xid
    }

    pub fn window_id(&self) -> u32 {
        self.window_id
    }

    pub fn render_opcode(&self) -> Option<u8> {
        self.render.as_ref().map(|r| r.opcode)
    }

    pub fn xkb_opcode(&self) -> Option<u8> {
        self.xkb.as_ref().map(|r| r.opcode)
    }

    pub fn xkb_info(&self) -> Option<(u8, u8, u8)> {
        self.xkb
            .as_ref()
            .map(|r| (r.opcode, r.first_event, r.first_error))
    }

    pub fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32> {
        let r = self.render.as_ref()?;
        match ynest_fmt {
            1 => Some(r.fmt_a1),
            2 => Some(r.fmt_a8),
            3 => Some(r.fmt_rgb24),
            4 => Some(r.fmt_argb32),
            _ => None,
        }
    }

    fn init_render(&mut self) -> io::Result<HostRenderInfo> {
        let ext_name = b"RENDER";
        let padded = padded_len(ext_name.len());
        let length_units = 2 + (padded / 4) as u16;
        let ext_seq = self.sequence; // use current BEFORE increment (matches open_font pattern)
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(98u8);
        out.push(0);
        write_u16(&mut out, length_units);
        write_u16(&mut out, ext_name.len() as u16);
        write_u16(&mut out, 0);
        out.extend_from_slice(ext_name);
        out.resize(8 + padded, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        debug!(
            "init_render: sent QueryExtension RENDER, expecting seq={}",
            ext_seq
        );

        let opcode;
        loop {
            let resp = read_response(&mut self.stream)?;
            debug!(
                "init_render: got response byte0={} seq={}",
                resp.bytes[0], resp.sequence
            );
            if resp.sequence == ext_seq {
                if resp.bytes[8] == 0 {
                    return Err(io::Error::other("host RENDER extension not present"));
                }
                opcode = resp.bytes[9];
                debug!("init_render: RENDER present, opcode={}", opcode);
                break;
            }
        }

        let fmt_seq = self.sequence; // use current BEFORE increment
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(1); // QueryPictFormats
        write_u16(&mut out, 1);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        debug!(
            "init_render: sent QueryPictFormats, expecting seq={}",
            fmt_seq
        );

        loop {
            let resp = read_response(&mut self.stream)?;
            debug!(
                "init_render: got response byte0={} seq={}",
                resp.bytes[0], resp.sequence
            );
            if resp.sequence == fmt_seq {
                let info = parse_host_pict_formats(&resp.bytes, opcode)?;
                debug!(
                    "init_render: host formats a1=0x{:x} a8=0x{:x} rgb24=0x{:x} argb32=0x{:x}",
                    info.fmt_a1, info.fmt_a8, info.fmt_rgb24, info.fmt_argb32
                );
                return Ok(info);
            }
        }
    }

    fn init_xkb(&mut self) -> io::Result<HostXkbInfo> {
        let ext_name = b"XKEYBOARD";
        let padded = padded_len(ext_name.len());
        let length_units = 2 + (padded / 4) as u16;
        let ext_seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(98u8);
        out.push(0);
        write_u16(&mut out, length_units);
        write_u16(&mut out, ext_name.len() as u16);
        write_u16(&mut out, 0);
        out.extend_from_slice(ext_name);
        out.resize(8 + padded, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()?;

        let (opcode, first_event, first_error);
        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence == ext_seq {
                if resp.bytes[8] == 0 {
                    return Err(io::Error::other("host XKEYBOARD extension not present"));
                }
                opcode = resp.bytes[9];
                first_event = resp.bytes[10];
                first_error = resp.bytes[11];
                break;
            }
        }

        // We also need to send UseExtension to the host for XKB to be fully functional.
        let use_seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(0); // UseExtension
        write_u16(&mut out, 2);
        write_u16(&mut out, 1); // want major 1
        write_u16(&mut out, 0); // want minor 0
        self.stream.write_all(&out)?;
        self.stream.flush()?;

        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence == use_seq {
                // byte 8 is supported (bool)
                if resp.bytes[8] == 0 {
                    return Err(io::Error::other("host XKB UseExtension failed"));
                }
                break;
            }
        }

        Ok(HostXkbInfo {
            opcode,
            first_event,
            first_error,
        })
    }

    /// Issue `QueryExtension(name)` on the host stream and return the major
    /// opcode if the extension is present. Used for capability probes that
    /// don't need the first-event/first-error fields (`init_render` and
    /// `init_xkb` cache those for their own bookkeeping).
    fn query_extension_opcode(&mut self, name: &[u8]) -> io::Result<Option<u8>> {
        let padded = padded_len(name.len());
        let length_units = 2 + (padded / 4) as u16;
        let ext_seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(98u8); // QueryExtension
        out.push(0);
        write_u16(&mut out, length_units);
        write_u16(&mut out, name.len() as u16);
        write_u16(&mut out, 0);
        out.extend_from_slice(name);
        out.resize(8 + padded, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence == ext_seq {
                if resp.bytes[8] == 0 {
                    return Ok(None);
                }
                return Ok(Some(resp.bytes[9]));
            }
            self.reply_buffer.push(resp);
        }
    }
}

fn create_window(
    stream: &mut UnixStream,
    setup: &HostSetup,
    window_id: u32,
    width: u16,
    height: u16,
) -> io::Result<()> {
    // Value-mask: bg-pixel (bit 1) | bit-gravity (bit 4) | event-mask (bit 11).
    // bit-gravity = NorthWest (1) so a host-side resize preserves the NW pixels.
    // Without this the gravity defaults to Forget and the host server is free
    // to clear the entire container on resize, which paints over every visible
    // subwindow and leaves the desktop blank until the apps redraw.
    let value_mask: u32 = (1 << 1) | (1 << 4) | (1 << 11);
    // length = 3 fixed words + 1 word per value bit (3 values). 3 + 3 = 6
    // fixed; add 4-word CreateWindow header → 10 total length units.
    let mut out = Vec::new();
    out.push(1);
    out.push(setup.root_depth);
    write_u16(&mut out, 11);
    write_u32(&mut out, window_id);
    write_u32(&mut out, setup.root);
    write_i16(&mut out, 80);
    write_i16(&mut out, 80);
    write_u16(&mut out, width);
    write_u16(&mut out, height);
    write_u16(&mut out, 0);
    write_u16(&mut out, 1);
    write_u32(&mut out, setup.root_visual);
    write_u32(&mut out, value_mask);
    write_u32(&mut out, setup.white_pixel); // bg-pixel
    write_u32(&mut out, 1); // bit-gravity = NorthWest
    write_u32(&mut out, 0x0000_8000 | 0x0002_0000); // event-mask
    stream.write_all(&out)
}

fn create_gc(
    stream: &mut UnixStream,
    drawable: u32,
    gc_id: u32,
    foreground: u32,
    background: u32,
    font_id: u32,
) -> io::Result<()> {
    let mut out = Vec::new();
    out.push(55);
    out.push(0);
    write_u16(&mut out, 7);
    write_u32(&mut out, gc_id);
    write_u32(&mut out, drawable);
    write_u32(&mut out, (1 << 2) | (1 << 3) | (1 << 14));
    write_u32(&mut out, foreground);
    write_u32(&mut out, background);
    write_u32(&mut out, font_id);
    stream.write_all(&out)
}

fn open_font(stream: &mut UnixStream, font_id: u32, name: &[u8]) -> io::Result<()> {
    let padded_name_len = padded_len(name.len());
    let length_units = 3 + u16::try_from(padded_name_len / 4)
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "font name is too long"))?;

    let mut out = Vec::new();
    out.push(45);
    out.push(0);
    write_u16(&mut out, length_units);
    write_u32(&mut out, font_id);
    write_u16(
        &mut out,
        u16::try_from(name.len())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "font name is too long"))?,
    );
    write_u16(&mut out, 0);
    out.extend_from_slice(name);
    out.resize(12 + padded_name_len, 0);
    stream.write_all(&out)
}

fn map_window(stream: &mut UnixStream, window_id: u32) -> io::Result<()> {
    let mut out = Vec::new();
    out.push(8);
    out.push(0);
    write_u16(&mut out, 2);
    write_u32(&mut out, window_id);
    stream.write_all(&out)
}

pub(super) fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

pub(super) fn read_i16(bytes: &[u8]) -> i16 {
    i16::from_le_bytes([bytes[0], bytes[1]])
}

pub(super) fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

pub(super) fn write_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn write_i16(out: &mut Vec<u8>, value: i16) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn padded_len(len: usize) -> usize {
    (len + 3) & !3
}

pub(super) fn pad4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

fn parse_host_pict_formats(bytes: &[u8], opcode: u8) -> io::Result<HostRenderInfo> {
    if bytes.len() < 32 {
        return Err(io::Error::other("QueryPictFormats reply too short"));
    }
    let num_formats = read_u32(&bytes[8..12]) as usize;
    let mut fmt_a1 = 0u32;
    let mut fmt_a8 = 0u32;
    let mut fmt_rgb24 = 0u32;
    let mut fmt_argb32 = 0u32;
    for i in 0..num_formats {
        let base = 32 + i * 28;
        if base + 28 > bytes.len() {
            break;
        }
        let id = read_u32(&bytes[base..base + 4]);
        let type_ = bytes[base + 4];
        let depth = bytes[base + 5];
        let alpha_shift = read_u16(&bytes[base + 20..base + 22]);
        let alpha_mask = read_u16(&bytes[base + 22..base + 24]);
        let red_shift = read_u16(&bytes[base + 8..base + 10]);
        let red_mask = read_u16(&bytes[base + 10..base + 12]);
        if type_ == 1 {
            match depth {
                1 if alpha_mask == 1 => fmt_a1 = id,
                8 if alpha_mask == 0xFF && alpha_shift == 0 => fmt_a8 = id,
                24 if red_mask == 0xFF && red_shift == 16 && alpha_mask == 0 => fmt_rgb24 = id,
                32 if alpha_mask == 0xFF && alpha_shift == 24 => fmt_argb32 = id,
                _ => {}
            }
        }
    }
    Ok(HostRenderInfo {
        opcode,
        fmt_a1,
        fmt_a8,
        fmt_rgb24,
        fmt_argb32,
    })
}

pub(super) struct HostResponse {
    sequence: u16,
    bytes: Vec<u8>,
}

pub(super) fn read_response(stream: &mut UnixStream) -> io::Result<HostResponse> {
    let mut header = [0u8; 32];
    loop {
        stream.read_exact(&mut header)?;
        match header[0] {
            0 | 1 => break,
            35 => {
                // GenericEvent: may have extra data beyond the 32-byte header.
                // Read and discard any extra bytes to keep the stream aligned.
                let extra =
                    u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize * 4;
                log::debug!(
                    "read_response: GenericEvent extra={} seq={}",
                    extra,
                    u16::from_le_bytes([header[2], header[3]])
                );
                if extra > 0 {
                    let mut tail = vec![0u8; extra];
                    stream.read_exact(&mut tail)?;
                }
                continue;
            }
            t => {
                log::debug!(
                    "read_response: skipping event type={} seq={}",
                    t,
                    u16::from_le_bytes([header[2], header[3]])
                );
                continue;
            }
        }
    }
    let sequence = u16::from_le_bytes([header[2], header[3]]);
    let extra = if header[0] == 1 {
        u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize * 4
    } else {
        0
    };
    let mut bytes = Vec::with_capacity(32 + extra);
    bytes.extend_from_slice(&header);
    if extra > 0 {
        let mut tail = vec![0u8; extra];
        stream.read_exact(&mut tail)?;
        bytes.extend_from_slice(&tail);
    }
    Ok(HostResponse { sequence, bytes })
}
