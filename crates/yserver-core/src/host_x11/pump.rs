//! Host X11 input pump and connection setup.
//!
//! Owns the read-side of the host connection (used by `nested::run` to
//! pull input/expose/configure events) and the per-client write handles
//! used to register host-window-id → nested-id mappings. The pump opens
//! its own socket to the host (separate from `HostX11Backend`'s request stream)
//! so reads can block in one thread while the request side fans out
//! across worker threads.
//!
//! Setup-time helpers live here too: `connect_to_host`, `XAuthority`,
//! `read_setup_reply`. `HostX11Backend::open_from_env` calls them through
//! `pub(super)` re-exports so it doesn't have to duplicate the wire
//! decoding.

use std::{
    collections::HashMap,
    env, fs,
    io::{self, ErrorKind, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use yserver_protocol::x11::ResourceId;

use super::{HostXidMap, pad4, padded_len, read_i16, read_u16, read_u32, write_u16, write_u32};

const MIT_MAGIC_COOKIE: &str = "MIT-MAGIC-COOKIE-1";

pub struct HostInputPump {
    read_stream: UnixStream,
    handle: HostInputPumpHandle,
}

#[derive(Clone)]
pub struct HostInputPumpHandle {
    write_stream: Arc<Mutex<UnixStream>>,
    xid_map: HostXidMap,
}

pub(super) fn connect_to_host() -> io::Result<UnixStream> {
    let display = env::var("DISPLAY").map_err(|_| {
        io::Error::new(
            ErrorKind::NotFound,
            "DISPLAY is not set for host X11 backend",
        )
    })?;
    let display_number = parse_display_number(&display)?;
    let socket_path = format!("/tmp/.X11-unix/X{display_number}");

    let auth = XAuthority::load(display_number).unwrap_or_default();
    let mut stream = UnixStream::connect(socket_path)?;
    write_setup_request(&mut stream, auth.as_ref())?;
    Ok(stream)
}

impl HostInputPump {
    pub fn open_from_env(window_id: u32) -> io::Result<Self> {
        let mut stream = connect_to_host()?;
        let _setup = read_setup_reply(&mut stream)?;
        select_keyboard_events(&mut stream, window_id)?;
        // The main pump owns POINTER_EVENT_MASK on the container. ButtonPress
        // is exclusive in X11 — only the main pump (not per-client kb pumps)
        // can hold it. Without this, clicks on the container area (where no
        // top-level child intervenes) drop on the host side and never reach
        // any nested client.
        select_pointer_events_on_container(&mut stream, window_id)?;
        stream.flush()?;
        let read_stream = stream.try_clone()?;
        // Map the host container window to ynest's ROOT_WINDOW so Expose
        // events on the container (raised by the host when subwindows uncover
        // the desktop area) are delivered as Expose on ROOT_WINDOW. Without
        // this entry expose_event_fanout drops the event and the desktop
        // background is never repainted after a window drag.
        let mut xid_map = HashMap::new();
        xid_map.insert(window_id, crate::resources::ROOT_WINDOW);
        let handle = HostInputPumpHandle {
            write_stream: Arc::new(Mutex::new(stream)),
            xid_map: Arc::new(Mutex::new(xid_map)),
        };
        Ok(Self {
            read_stream,
            handle,
        })
    }

    #[must_use]
    pub fn handle(&self) -> HostInputPumpHandle {
        self.handle.clone()
    }

    pub fn read_event(&mut self) -> io::Result<HostEvent> {
        loop {
            let mut event = [0; 32];
            self.read_stream.read_exact(&mut event)?;
            let event_type = event[0] & 0x7f;
            match event_type {
                2 | 3 => {
                    return Ok(HostEvent::Key(HostKeyEvent {
                        pressed: event_type == 2,
                        keycode: event[1],
                        time: read_u32(&event[4..8]),
                        root_x: read_i16(&event[20..22]),
                        root_y: read_i16(&event[22..24]),
                        event_x: read_i16(&event[24..26]),
                        event_y: read_i16(&event[26..28]),
                        state: read_u16(&event[28..30]),
                    }));
                }
                4..=6 => {
                    let kind = match event_type {
                        4 => PointerEventKind::ButtonPress,
                        5 => PointerEventKind::ButtonRelease,
                        _ => PointerEventKind::MotionNotify,
                    };
                    return Ok(HostEvent::Pointer(HostPointerEvent {
                        kind,
                        host_xid: read_u32(&event[12..16]), // event window
                        detail: event[1],
                        time: read_u32(&event[4..8]),
                        root_x: read_i16(&event[20..22]),
                        root_y: read_i16(&event[22..24]),
                        event_x: read_i16(&event[24..26]),
                        event_y: read_i16(&event[26..28]),
                        state: read_u16(&event[28..30]),
                    }));
                }
                7 | 8 => {
                    let kind = if event_type == 7 {
                        PointerEventKind::EnterNotify
                    } else {
                        PointerEventKind::LeaveNotify
                    };
                    return Ok(HostEvent::Pointer(HostPointerEvent {
                        kind,
                        host_xid: read_u32(&event[12..16]),
                        detail: 0,
                        time: read_u32(&event[4..8]),
                        root_x: read_i16(&event[20..22]),
                        root_y: read_i16(&event[22..24]),
                        event_x: read_i16(&event[24..26]),
                        event_y: read_i16(&event[26..28]),
                        state: read_u16(&event[28..30]),
                    }));
                }
                12 => {
                    let host_xid = read_u32(&event[4..8]);
                    let x = read_u16(&event[8..10]);
                    let y = read_u16(&event[10..12]);
                    let width = read_u16(&event[12..14]);
                    let height = read_u16(&event[14..16]);
                    let count = read_u16(&event[16..18]);
                    log::trace!(
                        "host pump: Expose host_xid=0x{host_xid:x} x={x} y={y} w={width} h={height} count={count}",
                    );
                    return Ok(HostEvent::Expose(HostExposeEvent {
                        host_xid,
                        x,
                        y,
                        width,
                        height,
                        count,
                    }));
                }
                22 => {
                    return Ok(HostEvent::Configure(HostConfigureEvent {
                        host_xid: read_u32(&event[8..12]),
                        x: read_i16(&event[16..18]),
                        y: read_i16(&event[18..20]),
                        width: read_u16(&event[20..22]),
                        height: read_u16(&event[22..24]),
                    }));
                }
                17 => return Ok(HostEvent::Closed),
                _ => {}
            }
        }
    }
}

const POINTER_EVENT_MASK: u32 = 0x0000_0004 // ButtonPress
    | 0x0000_0008 // ButtonRelease
    | 0x0000_0010 // EnterWindow
    | 0x0000_0020 // LeaveWindow
    | 0x0000_0040 // PointerMotion
    | 0x0000_8000; // Exposure

/// Mask used for sub-window host children: Exposure only. Per X11
/// event-propagation rules a host event with no listener at the leaf
/// bubbles up to the closest interested ancestor — for our sub-windows
/// that's the container, where input is dispatched. Selecting any
/// pointer events here would short-circuit our internal dispatch and
/// deliver to the wrong client. See Xnest `Window.c:91` and the design
/// doc's "Input event path" section.
const SUBWINDOW_EVENT_MASK: u32 = 0x0000_8000; // Exposure

impl HostInputPumpHandle {
    pub fn register_top_level(&self, nested_id: ResourceId, host_xid: u32) -> io::Result<()> {
        self.register_host_window(nested_id, host_xid, POINTER_EVENT_MASK)
    }

    /// Mirror a sub-window's host child for Expose-event delivery.
    /// Adds the host_xid → nested_id mapping so the pump can route
    /// host Exposes through `expose_event_fanout`, and selects
    /// `ExposureMask` only on the host child. Phase 3.6 Step 3+4
    /// invariant: every InputOutput sub-window goes through this
    /// register call after CreateWindow + before its first map.
    pub fn register_subwindow(&self, nested_id: ResourceId, host_xid: u32) -> io::Result<()> {
        self.register_host_window(nested_id, host_xid, SUBWINDOW_EVENT_MASK)
    }

    fn register_host_window(
        &self,
        nested_id: ResourceId,
        host_xid: u32,
        event_mask: u32,
    ) -> io::Result<()> {
        // Insert into the map *before* writing to X11 so that any pointer
        // events arriving on this subwindow after ChangeWindowAttributes are
        // sent can be resolved to a nested window id immediately.
        if let Ok(mut map) = self.xid_map.lock() {
            map.insert(host_xid, nested_id);
        }
        // ChangeWindowAttributes — value-mask = (1<<11) (event-mask), value = pointer mask.
        let mut out = Vec::new();
        out.push(2); // ChangeWindowAttributes
        out.push(0);
        write_u16(&mut out, 4);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, 1 << 11);
        write_u32(&mut out, event_mask);
        let mut stream = self
            .write_stream
            .lock()
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "host pump stream poisoned"))?;
        stream.write_all(&out)?;
        stream.flush()?;
        log::debug!(
            "host pump: register host_window nested=0x{:x} host=0x{:x} event_mask=0x{:x}",
            nested_id.0,
            host_xid,
            event_mask
        );
        Ok(())
    }

    /// Drop the host_xid → nested ResourceId mapping. Used at
    /// DestroyWindow / Reparent-away-from-root for any host-mirrored
    /// window — top-level or sub-window — so stale host events on
    /// the now-defunct xid drop silently instead of misrouting.
    pub fn unregister_host_window(&self, host_xid: u32) {
        if let Ok(mut map) = self.xid_map.lock() {
            map.remove(&host_xid);
        }
    }

    /// Compatibility shim — `unregister_host_window` is the canonical
    /// name; older call sites use this one. Will be folded into the
    /// canonical name in a follow-up.
    pub fn unregister_top_level(&self, host_xid: u32) {
        self.unregister_host_window(host_xid);
    }

    #[must_use]
    pub fn xid_map(&self) -> HostXidMap {
        self.xid_map.clone()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum HostEvent {
    Key(HostKeyEvent),
    Pointer(HostPointerEvent),
    Expose(HostExposeEvent),
    Configure(HostConfigureEvent),
    Closed,
}

#[derive(Clone, Copy, Debug)]
pub struct HostExposeEvent {
    pub host_xid: u32,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub count: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct HostConfigureEvent {
    pub host_xid: u32,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerEventKind {
    ButtonPress,
    ButtonRelease,
    MotionNotify,
    EnterNotify,
    LeaveNotify,
}

#[derive(Clone, Copy, Debug)]
pub struct HostPointerEvent {
    pub kind: PointerEventKind,
    pub host_xid: u32,
    pub detail: u8,
    pub time: u32,
    pub root_x: i16,
    pub root_y: i16,
    pub event_x: i16,
    pub event_y: i16,
    pub state: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct HostKeyEvent {
    pub pressed: bool,
    pub keycode: u8,
    pub time: u32,
    pub root_x: i16,
    pub root_y: i16,
    pub event_x: i16,
    pub event_y: i16,
    pub state: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct PointerPosition {
    pub same_screen: bool,
    pub win_x: i16,
    pub win_y: i16,
    pub mask: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostSubwindowConfig {
    pub x: Option<i16>,
    pub y: Option<i16>,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub border_width: Option<u16>,
    pub sibling: Option<u32>,
    pub stack_mode: Option<u8>,
}

#[derive(Clone, Debug, Default)]
struct XAuthority {
    name: Vec<u8>,
    data: Vec<u8>,
}

impl XAuthority {
    fn load(display_number: u16) -> io::Result<Option<Self>> {
        let path = env::var_os("XAUTHORITY")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".Xauthority")))
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "no Xauthority path"))?;

        let bytes = fs::read(path)?;
        let display_number = display_number.to_string();
        let mut cursor = 0;
        let mut fallback = None;

        while cursor < bytes.len() {
            let Some(_family) = read_be_u16_record(&bytes, &mut cursor) else {
                break;
            };
            let Some(address) = read_record_field(&bytes, &mut cursor) else {
                break;
            };
            let Some(number) = read_record_field(&bytes, &mut cursor) else {
                break;
            };
            let Some(name) = read_record_field(&bytes, &mut cursor) else {
                break;
            };
            let Some(data) = read_record_field(&bytes, &mut cursor) else {
                break;
            };

            if name == MIT_MAGIC_COOKIE.as_bytes() && number == display_number.as_bytes() {
                let auth = Self { name, data };
                if address.is_empty() {
                    return Ok(Some(auth));
                }
                fallback = Some(auth);
            }
        }

        Ok(fallback)
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct HostSetup {
    pub(super) resource_id_base: u32,
    pub(super) root: u32,
    pub(super) root_visual: u32,
    pub(super) root_depth: u8,
    pub(super) white_pixel: u32,
    pub(super) black_pixel: u32,
    /// First TrueColor visual at depth 32 with a non-zero alpha mask, if
    /// the host advertises one. Used to forward CreateWindow with our
    /// `ARGB_VISUAL` so the host produces an ARGB drawable instead of a
    /// 24-bit one. `None` = host has no ARGB visual; we fall back to
    /// CopyFromParent in that case.
    pub(super) argb_visual: Option<u32>,
}

fn parse_display_number(display: &str) -> io::Result<u16> {
    let display = display
        .rsplit_once(':')
        .map_or(display, |(_, suffix)| suffix);
    let number = display.split('.').next().unwrap_or(display);
    number.parse::<u16>().map_err(|err| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("unsupported DISPLAY value {display:?}: {err}"),
        )
    })
}

fn write_setup_request(stream: &mut UnixStream, auth: Option<&XAuthority>) -> io::Result<()> {
    let (name, data) = auth
        .map(|auth| (auth.name.as_slice(), auth.data.as_slice()))
        .unwrap_or((&[][..], &[][..]));

    let mut out = Vec::new();
    out.push(b'l');
    out.push(0);
    write_u16(&mut out, 11);
    write_u16(&mut out, 0);
    write_u16(&mut out, name.len() as u16);
    write_u16(&mut out, data.len() as u16);
    write_u16(&mut out, 0);
    out.extend_from_slice(name);
    pad4(&mut out);
    out.extend_from_slice(data);
    pad4(&mut out);
    stream.write_all(&out)
}

pub(super) fn read_setup_reply(stream: &mut UnixStream) -> io::Result<HostSetup> {
    let mut header = [0; 8];
    stream.read_exact(&mut header)?;
    if header[0] != 1 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("host X11 setup failed with status {}", header[0]),
        ));
    }

    let length = u16::from_le_bytes([header[6], header[7]]) as usize * 4;
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    if body.len() < 40 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "host X11 setup body is too short",
        ));
    }

    let resource_id_base = read_u32(&body[4..8]);
    let vendor_len = read_u16(&body[16..18]) as usize;
    let roots_len = body[20] as usize;
    let pixmap_formats_len = body[21] as usize;
    if roots_len == 0 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "host X11 server has no roots",
        ));
    }

    let screen_offset = 32 + padded_len(vendor_len) + pixmap_formats_len * 8;
    if body.len() < screen_offset + 40 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "host X11 screen body is too short",
        ));
    }

    let screen = &body[screen_offset..];
    let allowed_depths_len = screen[39] as usize;
    let argb_visual = scan_for_argb_visual(&body, screen_offset + 40, allowed_depths_len);

    Ok(HostSetup {
        resource_id_base,
        root: read_u32(&screen[0..4]),
        root_visual: read_u32(&screen[32..36]),
        root_depth: screen[38],
        white_pixel: read_u32(&screen[8..12]),
        black_pixel: read_u32(&screen[12..16]),
        argb_visual,
    })
}

/// Walk the screen's depth-list looking for a depth-32 TrueColor visual
/// with a non-zero alpha mask. Each depth record is `depth(1) pad(1)
/// visuals_len(2) pad(4)` followed by `visuals_len * 24` bytes of
/// VisualType records (visual_id(4) class(1) bits_per_rgb(1)
/// colormap_entries(2) red(4) green(4) blue(4) pad(4)).
///
/// Returns `None` if the host has no such visual or the body is too
/// short to parse cleanly. `pad(4)` after the per-depth header is part
/// of the X11 protocol layout — see Protocol Reference, Setup section.
fn scan_for_argb_visual(body: &[u8], mut off: usize, depth_count: usize) -> Option<u32> {
    for _ in 0..depth_count {
        if body.len() < off + 8 {
            return None;
        }
        let depth = body[off];
        let visuals_len = read_u16(&body[off + 2..off + 4]) as usize;
        off += 8;
        for _ in 0..visuals_len {
            if body.len() < off + 24 {
                return None;
            }
            let visual_id = read_u32(&body[off..off + 4]);
            let class = body[off + 4];
            let red_mask = read_u32(&body[off + 8..off + 12]);
            let green_mask = read_u32(&body[off + 12..off + 16]);
            let blue_mask = read_u32(&body[off + 16..off + 20]);
            // Approximate "alpha mask present": for a 32-bit TrueColor
            // visual the host does not actually expose the alpha mask in
            // the setup reply (X11 only exposes R/G/B). We infer ARGB by
            // depth=32 + class=TrueColor + the standard 8-bit RGB layout.
            let standard_argb_layout =
                red_mask == 0x00ff_0000 && green_mask == 0x0000_ff00 && blue_mask == 0x0000_00ff;
            if depth == 32 && class == 4 && standard_argb_layout {
                return Some(visual_id);
            }
            off += 24;
        }
    }
    None
}

fn select_pointer_events_on_container(stream: &mut UnixStream, window_id: u32) -> io::Result<()> {
    let mut out = Vec::new();
    out.push(2); // ChangeWindowAttributes
    out.push(0);
    write_u16(&mut out, 4);
    write_u32(&mut out, window_id);
    write_u32(&mut out, 1 << 11); // value-mask = event-mask
    write_u32(&mut out, POINTER_EVENT_MASK);
    stream.write_all(&out)
}

fn select_keyboard_events(stream: &mut UnixStream, window_id: u32) -> io::Result<()> {
    let mut out = Vec::new();
    out.push(2);
    out.push(0);
    write_u16(&mut out, 4);
    write_u32(&mut out, window_id);
    write_u32(&mut out, 1 << 11);
    // KeyPress | KeyRelease | StructureNotify only.
    //
    // Per X11 spec ButtonPress is *exclusive* — only one client at a
    // time per window. The main `HostInputPump` (created in
    // `nested::run`) already selects POINTER_EVENT_MASK (which
    // includes ButtonPress) on the container; if the per-client kb
    // pump tries to also select ButtonPress here the host returns
    // BadAccess for the entire ChangeWindowAttributes request,
    // leaving the kb pump connection with no mask at all on
    // container — the pump then silently receives nothing including
    // KeyPress. Selecting only KeyPress / KeyRelease /
    // StructureNotify avoids the conflict; pointer events arrive on
    // the main pump's connection where they belong.
    write_u32(&mut out, (1 << 0) | (1 << 1) | (1 << 17));
    stream.write_all(&out)
}

fn read_be_u16_record(bytes: &[u8], cursor: &mut usize) -> Option<u16> {
    let end = *cursor + 2;
    let value = u16::from_be_bytes(bytes.get(*cursor..end)?.try_into().ok()?);
    *cursor = end;
    Some(value)
}

fn read_record_field(bytes: &[u8], cursor: &mut usize) -> Option<Vec<u8>> {
    let len = read_be_u16_record(bytes, cursor)? as usize;
    let end = *cursor + len;
    let value = bytes.get(*cursor..end)?.to_vec();
    *cursor = end;
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::scan_for_argb_visual;

    /// `(visual_id, class, red_mask, green_mask, blue_mask)`.
    type VisualRec = (u32, u8, u32, u32, u32);
    type DepthRec<'a> = (u8, &'a [VisualRec]);

    /// Build a single-screen depth list with the given (depth, visuals)
    /// records. Returns the byte buffer + the offset at which the depth
    /// list starts (matches the layout `read_setup_reply` hands to
    /// `scan_for_argb_visual`).
    fn build_depth_list(records: &[DepthRec<'_>]) -> (Vec<u8>, usize) {
        let mut body = Vec::new();
        let depth_offset = 0;
        for (depth, visuals) in records {
            body.push(*depth);
            body.push(0); // pad
            body.extend_from_slice(&(visuals.len() as u16).to_le_bytes());
            body.extend_from_slice(&[0; 4]); // pad
            for (vid, class, red, green, blue) in *visuals {
                body.extend_from_slice(&vid.to_le_bytes());
                body.push(*class);
                body.push(8); // bits_per_rgb
                body.extend_from_slice(&256u16.to_le_bytes()); // colormap_entries
                body.extend_from_slice(&red.to_le_bytes());
                body.extend_from_slice(&green.to_le_bytes());
                body.extend_from_slice(&blue.to_le_bytes());
                body.extend_from_slice(&[0; 4]); // pad
            }
        }
        (body, depth_offset)
    }

    #[test]
    fn scan_for_argb_visual_picks_depth_32_truecolor_with_8bit_rgb() {
        let (body, off) = build_depth_list(&[
            (24, &[(0x21, 4, 0x00ff_0000, 0x0000_ff00, 0x0000_00ff)]),
            (32, &[(0x42, 4, 0x00ff_0000, 0x0000_ff00, 0x0000_00ff)]),
        ]);
        assert_eq!(scan_for_argb_visual(&body, off, 2), Some(0x42));
    }

    #[test]
    fn scan_for_argb_visual_skips_non_true_color_at_depth_32() {
        let (body, off) =
            build_depth_list(&[(32, &[(0x42, 5, 0x00ff_0000, 0x0000_ff00, 0x0000_00ff)])]);
        assert_eq!(scan_for_argb_visual(&body, off, 1), None);
    }

    #[test]
    fn scan_for_argb_visual_returns_none_when_no_depth_32() {
        let (body, off) =
            build_depth_list(&[(24, &[(0x21, 4, 0x00ff_0000, 0x0000_ff00, 0x0000_00ff)])]);
        assert_eq!(scan_for_argb_visual(&body, off, 1), None);
    }

    #[test]
    fn scan_for_argb_visual_returns_none_on_truncated_body() {
        // Header claims a depth-32 visual but the body cuts off early.
        let mut body = Vec::new();
        body.push(32);
        body.push(0);
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&[0; 4]);
        body.extend_from_slice(&[0; 12]); // less than the 24-byte VisualType
        assert_eq!(scan_for_argb_visual(&body, 0, 1), None);
    }
}
