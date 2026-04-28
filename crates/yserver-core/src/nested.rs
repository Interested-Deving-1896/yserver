use std::{
    collections::HashMap,
    fs,
    io::{self, ErrorKind, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU16, AtomicU32, Ordering},
    },
    thread,
};

use log::{debug, error, info, warn};
use yserver_protocol::x11::{
    self, AtomId, ClientByteOrder, ClientId, RequestHeader, ResourceId, SequenceNumber,
};

use crate::{
    host_x11::{HostEvent, HostKeyboard, HostX11},
    resources::{MapState, Pixmap, ROOT_COLORMAP, ROOT_VISUAL, ROOT_WINDOW, Window},
    server::{ClientHandle, ServerState},
};

static NEXT_CLIENT_ID: AtomicU32 = AtomicU32::new(1);

struct OwnedGetPropertyReply {
    format: u8,
    r#type: AtomId,
    bytes_after: u32,
    value_len: u32,
    value: Vec<u8>,
}

pub fn run(display: u16) -> io::Result<()> {
    let socket_dir = PathBuf::from("/tmp/.X11-unix");
    fs::create_dir_all(&socket_dir)?;

    let socket_path = socket_dir.join(format!("X{display}"));
    match fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    let listener = UnixListener::bind(&socket_path)?;
    info!("ynest listening on DISPLAY=:{display}");

    let host = match HostX11::open_from_env() {
        Ok(host) => {
            info!("host X11 container window: 0x{:x}", host.window_id());
            Some(Arc::new(Mutex::new(host)))
        }
        Err(err) => {
            error!("could not open host X11 window: {err}");
            None
        }
    };
    if let Some(host) = host.as_ref() {
        let _ = host.lock().map(|mut host| host.ping());
    }
    let host_window_id = host
        .as_ref()
        .and_then(|host| host.lock().ok().map(|host| host.window_id()));

    if let Some(window_id) = host_window_id {
        spawn_window_close_watcher(window_id);
    }

    let server = Arc::new(Mutex::new(ServerState::new()));

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let client_id = ClientId(NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed));
                let host = host.clone();
                let server = server.clone();
                thread::spawn(move || {
                    if let Err(err) = handle_client(client_id, stream, server, host, host_window_id)
                    {
                        info!("client {} disconnected: {err}", client_id.0);
                    }
                });
            }
            Err(err) => error!("accept failed: {err}"),
        }
    }

    Ok(())
}

fn lock_server(server: &Mutex<ServerState>) -> io::Result<std::sync::MutexGuard<'_, ServerState>> {
    server
        .lock()
        .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "server state poisoned"))
}

fn emit_x11_error(
    writer: &Arc<Mutex<UnixStream>>,
    sequence: SequenceNumber,
    code: u8,
    bad_value: u32,
    major_opcode: u8,
) -> io::Result<()> {
    let mut w = writer
        .lock()
        .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "client writer mutex poisoned"))?;
    x11::write_error(&mut *w, sequence, code, bad_value, 0, major_opcode)
}

fn collect_destroy_order(
    table: &crate::resources::ResourceTable,
    root: ResourceId,
    out: &mut Vec<ResourceId>,
) {
    let Some(w) = table.window(root) else {
        return;
    };
    for child in w.children.clone() {
        collect_destroy_order(table, child, out);
    }
    out.push(root);
}

fn handle_client(
    client_id: ClientId,
    mut stream: UnixStream,
    server: Arc<Mutex<ServerState>>,
    host: Option<Arc<Mutex<HostX11>>>,
    host_window_id: Option<u32>,
) -> io::Result<()> {
    let setup = x11::read_setup_request(&mut stream)?;
    if setup.byte_order != ClientByteOrder::LittleEndian {
        x11::write_setup_failed(
            &mut stream,
            setup.byte_order,
            "ynest currently supports only little-endian clients",
        )?;
        return Ok(());
    }

    let allocated = lock_server(&server)?.id_allocator.allocate();
    let Some((resource_id_base, resource_id_mask)) = allocated else {
        x11::write_setup_failed(
            &mut stream,
            setup.byte_order,
            "ynest exhausted resource ID space",
        )?;
        return Ok(());
    };

    info!(
        "client {} setup: protocol {}.{}, base=0x{:x}",
        client_id.0, setup.protocol_major, setup.protocol_minor, resource_id_base
    );

    let current_input_masks: u32 = {
        let s = lock_server(&server)?;
        s.clients
            .values()
            .filter_map(|c| c.event_masks.get(&ROOT_WINDOW).copied())
            .fold(0u32, |a, b| a | b)
    };

    x11::write_setup_success(
        &mut stream,
        x11::SetupSuccess {
            protocol_major: setup.protocol_major,
            protocol_minor: setup.protocol_minor,
            release_number: 1,
            resource_id_base,
            resource_id_mask,
            motion_buffer_size: 0,
            maximum_request_length: u16::MAX,
            image_byte_order: setup.byte_order,
            bitmap_format_bit_order: setup.byte_order,
            bitmap_format_scanline_unit: 32,
            bitmap_format_scanline_pad: 32,
            min_keycode: 8,
            max_keycode: 255,
            vendor: "yserver",
            root: x11::Screen {
                root: ROOT_WINDOW,
                default_colormap: ROOT_COLORMAP,
                white_pixel: 0x00ff_ffff,
                black_pixel: 0,
                current_input_masks,
                width_px: 800,
                height_px: 600,
                width_mm: 211,
                height_mm: 158,
                min_installed_maps: 1,
                max_installed_maps: 1,
                root_visual: ROOT_VISUAL,
                root_depth: 24,
            },
        },
    )?;

    let mut reader = stream.try_clone()?;
    let writer = Arc::new(Mutex::new(stream));
    let focused_window = Arc::new(Mutex::new(ROOT_WINDOW));
    let last_sequence = Arc::new(AtomicU16::new(0));

    {
        let mut s = lock_server(&server)?;
        s.clients.insert(
            client_id.0,
            ClientHandle {
                writer: writer.clone(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: last_sequence.clone(),
                resource_id_base,
                resource_id_mask,
                event_masks: HashMap::new(),
            },
        );
    }

    if let Some(host_window_id) = host_window_id {
        match HostKeyboard::open_from_env(host_window_id) {
            Ok(keyboard) => spawn_keyboard_forwarder(
                client_id,
                keyboard,
                writer.clone(),
                focused_window.clone(),
                last_sequence.clone(),
            ),
            Err(err) => warn!("client {} keyboard forwarding disabled: {err}", client_id.0),
        }
    }

    #[allow(clippy::redundant_closure_call)]
    let result: io::Result<()> = (|| {
        let mut sequence = SequenceNumber(0);
        loop {
            let Some((header, body)) = x11::read_request(&mut reader)? else {
                return Ok(());
            };
            sequence = sequence.next();
            last_sequence.store(sequence.0, Ordering::Relaxed);
            handle_request(
                client_id,
                &server,
                host.as_ref(),
                &writer,
                &focused_window,
                sequence,
                header,
                &body,
            )?;
        }
    })();

    let (closed_fonts, pending_destroys) = {
        let mut s = lock_server(&server)?;
        let mut owned_roots: Vec<ResourceId> = Vec::new();
        s.resources
            .collect_owned_window_roots(client_id, &mut owned_roots);

        let mut pending: Vec<(
            ResourceId,
            ResourceId,
            Vec<crate::server::EventTarget>,
            Vec<crate::server::EventTarget>,
        )> = Vec::new();
        let mut all_destroyed: Vec<ResourceId> = Vec::new();
        for root in owned_roots {
            let mut order = Vec::new();
            collect_destroy_order(&s.resources, root, &mut order);
            for w in &order {
                let parent = s.resources.window(*w).map_or(ROOT_WINDOW, |win| win.parent);
                let on_w = s.subscribers(*w, 0x0002_0000);
                let on_p = s.subscribers(parent, 0x0008_0000);
                pending.push((*w, parent, on_w, on_p));
            }
            let _ = s.resources.destroy_window(root);
            all_destroyed.extend(order);
        }
        s.drop_window_subscriptions(&all_destroyed);
        let fonts = s.resources.remove_non_window_resources_owned_by(client_id);
        s.clients.remove(&client_id.0);
        (fonts, pending)
    };
    for (w, parent, subs_w, subs_p) in pending_destroys {
        for target in subs_w {
            let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
            let mut buf = Vec::with_capacity(32);
            x11::encode_destroy_notify_event(&mut buf, seq, target.byte_order, w, w);
            if let Ok(mut wr) = target.writer.lock() {
                let _ = wr.write_all(&buf);
            }
        }
        for target in subs_p {
            let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
            let mut buf = Vec::with_capacity(32);
            x11::encode_destroy_notify_event(&mut buf, seq, target.byte_order, parent, w);
            if let Ok(mut wr) = target.writer.lock() {
                let _ = wr.write_all(&buf);
            }
        }
    }
    if let Some(host) = host.as_ref()
        && let Ok(mut h) = host.lock()
    {
        for xid in closed_fonts {
            let _ = h.close_font(xid);
        }
    }
    result
}

fn spawn_window_close_watcher(window_id: u32) {
    thread::spawn(move || {
        debug!("window-close watcher starting for 0x{window_id:x}");
        let mut watcher = match HostKeyboard::open_from_env(window_id) {
            Ok(w) => w,
            Err(err) => {
                error!("could not start window-close watcher: {err}");
                return;
            }
        };
        debug!("window-close watcher ready");
        loop {
            match watcher.read_event() {
                Ok(HostEvent::Key(_)) => {}
                Ok(HostEvent::Closed) => {
                    info!("host window closed, exiting");
                    std::process::exit(0);
                }
                Err(err) => {
                    info!("host connection lost ({err}), exiting");
                    std::process::exit(0);
                }
            }
        }
    });
}

fn spawn_keyboard_forwarder(
    client_id: ClientId,
    mut keyboard: HostKeyboard,
    writer: Arc<Mutex<UnixStream>>,
    focused_window: Arc<Mutex<ResourceId>>,
    last_sequence: Arc<AtomicU16>,
) {
    thread::spawn(move || {
        loop {
            let event = match keyboard.read_event() {
                Ok(HostEvent::Key(event)) => event,
                Ok(HostEvent::Closed) => {
                    info!("host window closed, exiting");
                    std::process::exit(0);
                }
                Err(err) => {
                    info!("host connection lost ({err}), exiting");
                    std::process::exit(0);
                }
            };
            let focus = focused_window
                .lock()
                .map(|focus| *focus)
                .unwrap_or(ROOT_WINDOW);
            if focus == ROOT_WINDOW {
                continue;
            }

            debug!(
                "client {} key {} {} -> 0x{:x}",
                client_id.0,
                if event.pressed { "press" } else { "release" },
                event.keycode,
                focus.0
            );
            let Some(mut writer) = writer.lock().ok() else {
                return;
            };
            if let Err(err) = x11::write_key_event(
                &mut *writer,
                x11::KeyEvent {
                    pressed: event.pressed,
                    keycode: event.keycode,
                    sequence: SequenceNumber(last_sequence.load(Ordering::Relaxed)),
                    time: event.time,
                    root: ROOT_WINDOW,
                    event: focus,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x: event.event_x,
                    event_y: event.event_y,
                    state: event.state & 0x004d,
                },
            ) {
                warn!("client {} keyboard forwarding stopped: {err}", client_id.0);
                return;
            }
        }
    });
}

fn set_focused_window(
    focused_window: &Arc<Mutex<ResourceId>>,
    server: &Arc<Mutex<ServerState>>,
    window: ResourceId,
) -> io::Result<()> {
    if window == ResourceId(0) {
        return Ok(());
    }
    let Ok(mut focused_window) = focused_window.lock() else {
        return Ok(());
    };
    if *focused_window == window {
        return Ok(());
    }

    let prev = *focused_window;
    *focused_window = window;
    drop(focused_window);

    if prev != ROOT_WINDOW {
        crate::server::emit_window_event(server, prev, 0x0020_0000, |buf, seq, order| {
            x11::encode_focus_event(buf, seq, order, false, prev);
        });
    }
    crate::server::emit_window_event(server, window, 0x0020_0000, |buf, seq, order| {
        x11::encode_focus_event(buf, seq, order, true, window);
    });
    Ok(())
}

fn clear_extent(requested: u16, offset: i16, window_extent: u16) -> u16 {
    if requested != 0 {
        return requested;
    }

    if offset <= 0 {
        window_extent
    } else {
        window_extent.saturating_sub(offset as u16)
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn handle_request(
    client_id: ClientId,
    server: &Arc<Mutex<ServerState>>,
    host: Option<&Arc<Mutex<HostX11>>>,
    writer: &Arc<Mutex<UnixStream>>,
    focused_window: &Arc<Mutex<ResourceId>>,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<()> {
    let lock_writer = || -> io::Result<std::sync::MutexGuard<'_, UnixStream>> {
        writer
            .lock()
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "client writer mutex poisoned"))
    };
    match header.opcode {
        1 => {
            if let Some(request) = x11::create_window_request(header.data, body) {
                debug!(
                    "client {} create window 0x{:x} parent=0x{:x} mask=0x{:x}",
                    client_id.0,
                    request.window.0,
                    request.parent.0,
                    request.event_mask.unwrap_or(0)
                );
                let new_id = request.window.0;
                let mask = request.event_mask.unwrap_or(0);
                let window_id = request.window;
                let validation_failed = {
                    let s = lock_server(server)?;
                    let handle = s.clients.get(&client_id.0).expect("client registered");
                    let owned = crate::server::IdAllocator::validate_owned(
                        new_id,
                        handle.resource_id_base,
                        handle.resource_id_mask,
                    );
                    let in_use = s.resources.any_resource_exists(request.window);
                    !owned || in_use
                };
                if validation_failed {
                    return emit_x11_error(writer, sequence, x11::error::BAD_ID_CHOICE, new_id, 1);
                }
                {
                    let mut s = lock_server(server)?;
                    s.resources.create_window(client_id, request);
                    if mask != 0 {
                        s.clients
                            .get_mut(&client_id.0)
                            .expect("client registered")
                            .event_masks
                            .insert(window_id, mask);
                    }
                }
                let wants_focus = {
                    let s = lock_server(server)?;
                    let mask = s
                        .clients
                        .get(&client_id.0)
                        .and_then(|c| c.event_masks.get(&window_id).copied())
                        .unwrap_or(0);
                    let viewable = s
                        .resources
                        .window(window_id)
                        .is_some_and(|w| w.map_state == MapState::Viewable);
                    viewable && (mask & 0x3) != 0
                };
                if wants_focus {
                    set_focused_window(focused_window, server, window_id)?;
                }
            }
            log_void(client_id, sequence, "CreateWindow")
        }
        2 => {
            if let Some(request) = x11::change_window_attributes_request(body) {
                if let Some(event_mask) = request.event_mask {
                    debug!(
                        "client {} attrs window 0x{:x} mask=0x{:x}",
                        client_id.0, request.window.0, event_mask
                    );
                }
                let target_window = request.window;
                let want_focus_check;
                let viewable;
                {
                    let mut s = lock_server(server)?;
                    if let Some(event_mask) = request.event_mask {
                        let entry = s.clients.get_mut(&client_id.0).expect("client registered");
                        if event_mask == 0 {
                            entry.event_masks.remove(&target_window);
                        } else {
                            entry.event_masks.insert(target_window, event_mask);
                        }
                    }
                    s.resources.change_window_attributes(request);
                    want_focus_check = s
                        .clients
                        .get(&client_id.0)
                        .and_then(|c| c.event_masks.get(&target_window).copied())
                        .unwrap_or(0);
                    viewable = s
                        .resources
                        .window(target_window)
                        .is_some_and(|w| w.map_state == MapState::Viewable);
                }
                if viewable && want_focus_check & 0x3 != 0 {
                    set_focused_window(focused_window, server, target_window)?;
                }
            }
            log_void(client_id, sequence, "ChangeWindowAttributes")
        }
        3 => {
            log_reply(client_id, sequence, "GetWindowAttributes");
            let attrs = {
                let s = lock_server(server)?;
                let id = x11::drawable_request_id(body).unwrap_or(ROOT_WINDOW);
                let target = if s.resources.window(id).is_some() {
                    id
                } else {
                    ROOT_WINDOW
                };
                let your_event_mask = s
                    .clients
                    .get(&client_id.0)
                    .and_then(|c| c.event_masks.get(&target).copied())
                    .unwrap_or(0);
                let all_event_masks: u32 = s
                    .clients
                    .values()
                    .filter_map(|c| c.event_masks.get(&target).copied())
                    .fold(0u32, |a, b| a | b);
                window_attributes(s.resources.window(target), all_event_masks, your_event_mask)
            };
            x11::write_get_window_attributes_reply(&mut *lock_writer()?, sequence, attrs)
        }
        4 => {
            if let Some(window) = x11::free_resource_id(body) {
                let pending = {
                    let mut s = lock_server(server)?;
                    let mut order = Vec::new();
                    collect_destroy_order(&s.resources, window, &mut order);
                    let mut pending: Vec<(
                        ResourceId,
                        ResourceId,
                        Vec<crate::server::EventTarget>,
                        Vec<crate::server::EventTarget>,
                    )> = Vec::new();
                    for w in &order {
                        let parent = s.resources.window(*w).map_or(ROOT_WINDOW, |win| win.parent);
                        let on_window = s.subscribers(*w, 0x0002_0000); // StructureNotify
                        let on_parent = s.subscribers(parent, 0x0008_0000); // SubstructureNotify
                        pending.push((*w, parent, on_window, on_parent));
                    }
                    let _ = s.resources.destroy_window(window);
                    s.drop_window_subscriptions(&order);
                    pending
                };
                for (w, parent, subs_w, subs_p) in pending {
                    for target in subs_w {
                        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                        let mut buf = Vec::with_capacity(32);
                        x11::encode_destroy_notify_event(&mut buf, seq, target.byte_order, w, w);
                        if let Ok(mut wr) = target.writer.lock() {
                            let _ = wr.write_all(&buf);
                        }
                    }
                    for target in subs_p {
                        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                        let mut buf = Vec::with_capacity(32);
                        x11::encode_destroy_notify_event(
                            &mut buf,
                            seq,
                            target.byte_order,
                            parent,
                            w,
                        );
                        if let Ok(mut wr) = target.writer.lock() {
                            let _ = wr.write_all(&buf);
                        }
                    }
                }
            }
            log_void(client_id, sequence, "DestroyWindow")
        }
        7 => log_void(client_id, sequence, "ReparentWindow"),
        8 => {
            if let Some(window) = x11::map_window_id(body) {
                let map_info = {
                    let mut s = lock_server(server)?;
                    s.resources.map_window(window);
                    s.resources
                        .window(window)
                        .map(|w| (w.parent, w.override_redirect, w.width, w.height))
                };
                let wants_focus = {
                    let s = lock_server(server)?;
                    let mask = s
                        .clients
                        .get(&client_id.0)
                        .and_then(|c| c.event_masks.get(&window).copied())
                        .unwrap_or(0);
                    let viewable = s
                        .resources
                        .window(window)
                        .is_some_and(|w| w.map_state == MapState::Viewable);
                    viewable && (mask & 0x3) != 0
                };
                if wants_focus {
                    debug!("focus key window 0x{:x}", window.0);
                    set_focused_window(focused_window, server, window)?;
                }
                if let Some((_parent, override_redirect, width, height)) = map_info {
                    crate::server::emit_window_event(
                        server,
                        window,
                        0x0002_0000,
                        |buf, seq, order| {
                            x11::encode_map_notify_event(
                                buf,
                                seq,
                                order,
                                window,
                                window,
                                override_redirect,
                            );
                        },
                    );
                    crate::server::emit_window_event(
                        server,
                        window,
                        0x0000_8000,
                        |buf, seq, order| {
                            x11::encode_expose_event(buf, seq, order, window, width, height);
                        },
                    );
                }
            }
            log_void(client_id, sequence, "MapWindow")
        }
        9 => {
            if let Some(parent) = x11::map_window_id(body) {
                let children = {
                    let s = lock_server(server)?;
                    s.resources.children(parent).to_vec()
                };
                for child in children {
                    let extents = {
                        let mut s = lock_server(server)?;
                        s.resources.map_window(child);
                        s.resources.window(child).map(|w| (w.width, w.height))
                    };
                    let wants_focus = {
                        let s = lock_server(server)?;
                        let mask = s
                            .clients
                            .get(&client_id.0)
                            .and_then(|c| c.event_masks.get(&child).copied())
                            .unwrap_or(0);
                        let viewable = s
                            .resources
                            .window(child)
                            .is_some_and(|w| w.map_state == MapState::Viewable);
                        viewable && (mask & 0x3) != 0
                    };
                    if wants_focus {
                        debug!("focus key window 0x{:x}", child.0);
                        set_focused_window(focused_window, server, child)?;
                    }
                    if let Some((width, height)) = extents {
                        crate::server::emit_window_event(
                            server,
                            child,
                            0x0000_8000,
                            |buf, seq, order| {
                                x11::encode_expose_event(buf, seq, order, child, width, height);
                            },
                        );
                    }
                }
            }
            log_void(client_id, sequence, "MapSubwindows")
        }
        10 => {
            if let Some(window) = x11::map_window_id(body) {
                let mut s = lock_server(server)?;
                s.resources.unmap_window(window);
            }
            log_void(client_id, sequence, "UnmapWindow")
        }
        12 => {
            if let Some(request) = x11::configure_window_request(body) {
                let configure = {
                    let mut s = lock_server(server)?;
                    s.resources
                        .configure_window(request)
                        .map(|w| (w.id, window_geometry(w), w.override_redirect))
                };
                if let Some((window_id, geometry, override_redirect)) = configure {
                    crate::server::emit_window_event(
                        server,
                        window_id,
                        0x0002_0000,
                        |buf, seq, order| {
                            x11::encode_configure_notify_event(
                                buf,
                                seq,
                                order,
                                window_id,
                                window_id,
                                geometry,
                                override_redirect,
                            );
                        },
                    );
                }
            }
            log_void(client_id, sequence, "ConfigureWindow")
        }
        14 => {
            log_reply(client_id, sequence, "GetGeometry");
            let geometry = {
                let s = lock_server(server)?;
                let drawable = x11::drawable_request_id(body).unwrap_or(ROOT_WINDOW);
                s.resources
                    .window(drawable)
                    .map(window_geometry)
                    .or_else(|| s.resources.pixmap(drawable).map(pixmap_geometry))
                    .unwrap_or_else(|| {
                        window_geometry(
                            s.resources.window(ROOT_WINDOW).expect("root window exists"),
                        )
                    })
            };
            x11::write_get_geometry_reply(&mut *lock_writer()?, sequence, geometry)
        }
        15 => {
            log_reply(client_id, sequence, "QueryTree");
            let (parent, children) = {
                let s = lock_server(server)?;
                let window = x11::drawable_request_id(body).unwrap_or(ROOT_WINDOW);
                let window_state = s
                    .resources
                    .window(window)
                    .or_else(|| s.resources.window(ROOT_WINDOW))
                    .expect("root window exists");
                (window_state.parent, window_state.children.clone())
            };
            x11::write_query_tree_reply(
                &mut *lock_writer()?,
                sequence,
                ROOT_WINDOW,
                parent,
                &children,
            )
        }
        16 => {
            let name = x11::intern_atom_name(body);
            let atom = {
                let mut s = lock_server(server)?;
                s.atoms.intern(&name, header.data != 0)
            };
            debug!(
                "client {} #{} InternAtom {:?} -> {}",
                client_id.0, sequence.0, name, atom.0
            );
            x11::write_intern_atom_reply(&mut *lock_writer()?, sequence, atom)
        }
        17 => {
            let atom = x11::request_atom(body);
            let name = {
                let s = lock_server(server)?;
                s.atoms.name(atom).unwrap_or("UNKNOWN").to_owned()
            };
            debug!(
                "client {} #{} GetAtomName {} -> {:?}",
                client_id.0, sequence.0, atom.0, name
            );
            x11::write_get_atom_name_reply(&mut *lock_writer()?, sequence, &name)
        }
        18 => {
            let Some(req) = x11::change_property_request(header.data, body) else {
                return emit_x11_error(writer, sequence, x11::error::BAD_LENGTH, 0, 18);
            };

            let Some(mode) = crate::properties::ChangeMode::from_protocol(req.mode) else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(req.mode),
                    18,
                );
            };
            let Some(format) = crate::properties::PropertyFormat::from_protocol(req.format) else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(req.format),
                    18,
                );
            };
            let expected_bytes = (req.length as usize).checked_mul(format.bytes());
            if expected_bytes != Some(req.data.len()) {
                return emit_x11_error(writer, sequence, x11::error::BAD_LENGTH, 0, 18);
            }

            let (timestamp, subscribers) = {
                let mut s = lock_server(server)?;
                if s.resources.window(req.window).is_none() {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_WINDOW,
                        req.window.0,
                        18,
                    );
                }
                if !s.atoms.exists(req.property) {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_ATOM,
                        req.property.0,
                        18,
                    );
                }
                if !s.atoms.exists(req.r#type) {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_ATOM,
                        req.r#type.0,
                        18,
                    );
                }
                let existing = s
                    .resources
                    .window_property(req.window, req.property)
                    .cloned();
                let new_value = match crate::properties::apply_change(
                    existing.as_ref(),
                    mode,
                    req.r#type,
                    format,
                    &req.data,
                ) {
                    Ok(v) => v,
                    Err(crate::properties::ChangePropertyError::BadMatch) => {
                        drop(s);
                        return emit_x11_error(
                            writer,
                            sequence,
                            x11::error::BAD_MATCH,
                            req.window.0,
                            18,
                        );
                    }
                    Err(crate::properties::ChangePropertyError::BadAlloc) => {
                        drop(s);
                        return emit_x11_error(writer, sequence, x11::error::BAD_ALLOC, 0, 18);
                    }
                    Err(crate::properties::ChangePropertyError::BadValue) => {
                        drop(s);
                        return emit_x11_error(writer, sequence, x11::error::BAD_VALUE, 0, 18);
                    }
                };
                s.resources
                    .set_window_property(req.window, req.property, new_value);
                let timestamp = s.timestamp_now();
                let subs = s.subscribers(req.window, 0x0040_0000);
                (timestamp, subs)
            };

            for target in subscribers {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(32);
                x11::encode_property_notify_event(
                    &mut buf,
                    seq,
                    target.byte_order,
                    req.window,
                    req.property,
                    timestamp,
                    false,
                );
                if let Ok(mut w) = target.writer.lock() {
                    let _ = w.write_all(&buf);
                }
            }
            Ok(())
        }
        19 => {
            let Some(req) = x11::delete_property_request(body) else {
                return emit_x11_error(writer, sequence, x11::error::BAD_LENGTH, 0, 19);
            };
            let (existed, timestamp, subscribers) = {
                let mut s = lock_server(server)?;
                if s.resources.window(req.window).is_none() {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_WINDOW,
                        req.window.0,
                        19,
                    );
                }
                if !s.atoms.exists(req.property) {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_ATOM,
                        req.property.0,
                        19,
                    );
                }
                let existed = s
                    .resources
                    .delete_window_property(req.window, req.property)
                    .is_some();
                let timestamp = s.timestamp_now();
                let subs = if existed {
                    s.subscribers(req.window, 0x0040_0000)
                } else {
                    Vec::new()
                };
                (existed, timestamp, subs)
            };
            if existed {
                for target in subscribers {
                    let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                    let mut buf = Vec::with_capacity(32);
                    x11::encode_property_notify_event(
                        &mut buf,
                        seq,
                        target.byte_order,
                        req.window,
                        req.property,
                        timestamp,
                        true,
                    );
                    if let Ok(mut w) = target.writer.lock() {
                        let _ = w.write_all(&buf);
                    }
                }
            }
            Ok(())
        }
        20 => {
            let Some(req) = x11::get_property_request(header.data, body) else {
                return emit_x11_error(writer, sequence, x11::error::BAD_LENGTH, 0, 20);
            };
            let (reply_owned, delete_subscribers, timestamp) = {
                let mut s = lock_server(server)?;
                if s.resources.window(req.window).is_none() {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_WINDOW,
                        req.window.0,
                        20,
                    );
                }
                if !s.atoms.exists(req.property) {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_ATOM,
                        req.property.0,
                        20,
                    );
                }
                if req.r#type.0 != 0 && !s.atoms.exists(req.r#type) {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_ATOM,
                        req.r#type.0,
                        20,
                    );
                }
                let existing = s
                    .resources
                    .window_property(req.window, req.property)
                    .cloned();
                let slice = match crate::properties::slice_for_get(
                    existing.as_ref(),
                    req.r#type,
                    req.long_offset,
                    req.long_length,
                ) {
                    Ok(s) => s,
                    Err(crate::properties::ChangePropertyError::BadValue) => {
                        drop(s);
                        return emit_x11_error(
                            writer,
                            sequence,
                            x11::error::BAD_VALUE,
                            req.long_offset,
                            20,
                        );
                    }
                    Err(_) => unreachable!("slice_for_get only returns BadValue on error"),
                };
                let value_len_units = if slice.format == 0 {
                    0
                } else {
                    slice.value.len() as u32 / u32::from(slice.format / 8)
                };
                let owned = OwnedGetPropertyReply {
                    format: slice.format,
                    r#type: slice.r#type,
                    bytes_after: slice.bytes_after,
                    value_len: value_len_units,
                    value: slice.value.to_vec(),
                };

                // Decide whether `delete=1` actually fires.
                let type_matched = existing
                    .as_ref()
                    .is_some_and(|p| req.r#type.0 == 0 || req.r#type == p.r#type);
                let mut subs = Vec::new();
                let mut timestamp = 0u32;
                if req.delete && type_matched && slice.bytes_after == 0 && existing.is_some() {
                    s.resources.delete_window_property(req.window, req.property);
                    timestamp = s.timestamp_now();
                    subs = s.subscribers(req.window, 0x0040_0000);
                }
                (owned, subs, timestamp)
            };

            {
                let mut w = lock_writer()?;
                x11::write_get_property_reply(
                    &mut *w,
                    sequence,
                    x11::GetPropertyReply {
                        format: reply_owned.format,
                        r#type: reply_owned.r#type,
                        bytes_after: reply_owned.bytes_after,
                        value_len: reply_owned.value_len,
                        value: &reply_owned.value,
                    },
                )?;
            }
            for target in delete_subscribers {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(32);
                x11::encode_property_notify_event(
                    &mut buf,
                    seq,
                    target.byte_order,
                    req.window,
                    req.property,
                    timestamp,
                    true,
                );
                if let Ok(mut w) = target.writer.lock() {
                    let _ = w.write_all(&buf);
                }
            }
            Ok(())
        }
        22 => log_void(client_id, sequence, "SetSelectionOwner"),
        23 => {
            log_reply(client_id, sequence, "GetSelectionOwner");
            x11::write_get_selection_owner_reply(&mut *lock_writer()?, sequence, ResourceId(0))
        }
        25 => log_void(client_id, sequence, "SendEvent"),
        26 => {
            log_reply(client_id, sequence, "GrabPointer");
            x11::write_grab_reply(&mut *lock_writer()?, sequence, 0)
        }
        27 => log_void(client_id, sequence, "UngrabPointer"),
        28 => log_void(client_id, sequence, "GrabButton"),
        29 => log_void(client_id, sequence, "UngrabButton"),
        31 => {
            log_reply(client_id, sequence, "GrabKeyboard");
            x11::write_grab_reply(&mut *lock_writer()?, sequence, 0)
        }
        32 => log_void(client_id, sequence, "UngrabKeyboard"),
        33 => log_void(client_id, sequence, "GrabKey"),
        34 => log_void(client_id, sequence, "UngrabKey"),
        36 => log_void(client_id, sequence, "GrabServer"),
        37 => log_void(client_id, sequence, "UngrabServer"),
        38 => {
            log_reply(client_id, sequence, "QueryPointer");
            let pointer = host
                .and_then(|host| host.lock().ok()?.query_pointer().ok())
                .filter(|pointer| pointer.same_screen);
            let reply_data = if let Some(pointer) = pointer {
                x11::QueryPointerReply {
                    root: ROOT_WINDOW,
                    child: ROOT_WINDOW,
                    root_x: pointer.root_x,
                    root_y: pointer.root_y,
                    win_x: pointer.win_x,
                    win_y: pointer.win_y,
                    mask: pointer.mask,
                }
            } else {
                x11::QueryPointerReply {
                    root: ROOT_WINDOW,
                    child: ROOT_WINDOW,
                    ..Default::default()
                }
            };
            x11::write_query_pointer_reply(&mut *lock_writer()?, sequence, reply_data)
        }
        40 => {
            log_reply(client_id, sequence, "TranslateCoordinates");
            x11::write_translate_coordinates_reply(
                &mut *lock_writer()?,
                sequence,
                ResourceId(0),
                0,
                0,
            )
        }
        42 => {
            if let Some(window) = x11::input_focus_window(body) {
                set_focused_window(focused_window, server, window)?;
            }
            log_void(client_id, sequence, "SetInputFocus")
        }
        43 => {
            log_reply(client_id, sequence, "GetInputFocus");
            let focus = focused_window
                .lock()
                .map(|focus| *focus)
                .unwrap_or(ROOT_WINDOW);
            x11::write_get_input_focus_reply(&mut *lock_writer()?, sequence, focus)
        }
        44 => {
            log_reply(client_id, sequence, "QueryKeymap");
            x11::write_query_keymap_reply(&mut *lock_writer()?, sequence)
        }
        45 => {
            if let Some(request) = x11::open_font_request(body) {
                debug!(
                    "client {} #{} OpenFont {:?}",
                    client_id.0, sequence.0, request.name
                );
                let new_id = request.font.0;
                let validation_failed = {
                    let s = lock_server(server)?;
                    let handle = s.clients.get(&client_id.0).expect("client registered");
                    !crate::server::IdAllocator::validate_owned(
                        new_id,
                        handle.resource_id_base,
                        handle.resource_id_mask,
                    ) || s.resources.any_resource_exists(request.font)
                };
                if validation_failed {
                    return emit_x11_error(writer, sequence, x11::error::BAD_ID_CHOICE, new_id, 45);
                }
                let host_result = if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    match host.open_font(&request.name) {
                        Ok(pair) => Some(pair),
                        Err(err) => {
                            warn!(
                                "client {} OpenFont {:?} failed on host: {err}",
                                client_id.0, request.name
                            );
                            None
                        }
                    }
                } else {
                    None
                };
                if let Some((host_xid, metrics)) = host_result {
                    let mut s = lock_server(server)?;
                    s.resources.install_font(
                        client_id,
                        request.font,
                        request.name,
                        host_xid,
                        metrics,
                    );
                }
                Ok(())
            } else {
                log_void(client_id, sequence, "OpenFont")
            }
        }
        46 => {
            if let Some(font) = x11::free_resource_id(body) {
                let removed = {
                    let mut s = lock_server(server)?;
                    s.resources.close_font(font)
                };
                if let Some(removed) = removed
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    let _ = host.close_font(removed.host_xid);
                }
            }
            log_void(client_id, sequence, "CloseFont")
        }
        47 => {
            log_reply(client_id, sequence, "QueryFont");
            let metrics = {
                let s = lock_server(server)?;
                x11::drawable_request_id(body)
                    .and_then(|id| s.resources.fontable(id))
                    .map(|font| font.metrics.clone())
                    .unwrap_or_default()
            };
            x11::write_query_font_reply(&mut *lock_writer()?, sequence, &metrics)
        }
        48 => {
            log_reply(client_id, sequence, "QueryTextExtents");
            let extents = {
                let s = lock_server(server)?;
                x11::query_text_extents_request(header.data, body)
                    .and_then(|req| {
                        s.resources
                            .fontable(req.fontable)
                            .map(|font| font.metrics.text_extents(&req.chars))
                    })
                    .unwrap_or_default()
            };
            x11::write_query_text_extents_reply(&mut *lock_writer()?, sequence, extents)
        }
        49 => {
            log_reply(client_id, sequence, "ListFonts");
            if let Some(request) = x11::list_fonts_request(body)
                && let Some(host) = host
                && let Ok(mut host) = host.lock()
                && let Ok(mut reply) = host.list_fonts_proxy(request.max_names, &request.pattern)
            {
                rewrite_reply_sequence(&mut reply, sequence);
                lock_writer()?.write_all(&reply)?;
            }
            Ok(())
        }
        50 => {
            log_reply(client_id, sequence, "ListFontsWithInfo");
            if let Some(request) = x11::list_fonts_request(body)
                && let Some(host) = host
                && let Ok(mut host) = host.lock()
                && let Ok(replies) =
                    host.list_fonts_with_info_proxy(request.max_names, &request.pattern)
            {
                for mut reply in replies {
                    rewrite_reply_sequence(&mut reply, sequence);
                    lock_writer()?.write_all(&reply)?;
                }
            }
            Ok(())
        }
        53 => {
            if let Some(request) = x11::create_pixmap_request(header.data, body) {
                let new_id = request.pixmap.0;
                let validation_failed = {
                    let s = lock_server(server)?;
                    let handle = s.clients.get(&client_id.0).expect("client registered");
                    let owned = crate::server::IdAllocator::validate_owned(
                        new_id,
                        handle.resource_id_base,
                        handle.resource_id_mask,
                    );
                    let in_use = s.resources.any_resource_exists(request.pixmap);
                    !owned || in_use
                };
                if validation_failed {
                    return emit_x11_error(writer, sequence, x11::error::BAD_ID_CHOICE, new_id, 53);
                }
                {
                    let mut s = lock_server(server)?;
                    s.resources.create_pixmap(client_id, request);
                }
            }
            log_void(client_id, sequence, "CreatePixmap")
        }
        54 => {
            if let Some(pixmap) = x11::free_resource_id(body) {
                let mut s = lock_server(server)?;
                s.resources.free_pixmap(pixmap);
            }
            log_void(client_id, sequence, "FreePixmap")
        }
        55 => {
            if let Some(request) = x11::create_gc_request(body) {
                let new_id = request.gc.0;
                let validation_failed = {
                    let s = lock_server(server)?;
                    let handle = s.clients.get(&client_id.0).expect("client registered");
                    let owned = crate::server::IdAllocator::validate_owned(
                        new_id,
                        handle.resource_id_base,
                        handle.resource_id_mask,
                    );
                    let in_use = s.resources.any_resource_exists(request.gc);
                    !owned || in_use
                };
                if validation_failed {
                    return emit_x11_error(writer, sequence, x11::error::BAD_ID_CHOICE, new_id, 55);
                }
                {
                    let mut s = lock_server(server)?;
                    s.resources.create_gc(client_id, request);
                }
            }
            log_void(client_id, sequence, "CreateGC")
        }
        56 => {
            if let Some(request) = x11::change_gc_request(body) {
                let mut s = lock_server(server)?;
                s.resources.change_gc(request);
            }
            log_void(client_id, sequence, "ChangeGC")
        }
        59 => log_void(client_id, sequence, "SetClipRectangles"),
        60 => {
            if let Some(gc) = x11::free_resource_id(body) {
                let mut s = lock_server(server)?;
                s.resources.free_gc(gc);
            }
            log_void(client_id, sequence, "FreeGC")
        }
        61 => {
            if let Some(request) = x11::clear_area_request(body) {
                let extents = {
                    let s = lock_server(server)?;
                    s.resources
                        .window(request.window)
                        .map(|w| (w.background_pixel, w.width, w.height))
                };
                if let Some((background_pixel, w_width, w_height)) = extents {
                    let width = clear_extent(request.width, request.x, w_width);
                    let height = clear_extent(request.height, request.y, w_height);
                    if width != 0
                        && height != 0
                        && let Some(host) = host
                        && let Ok(mut host) = host.lock()
                    {
                        host.fill_rectangle(background_pixel, request.x, request.y, width, height)?;
                    }
                }
            }
            log_void(client_id, sequence, "ClearArea")
        }
        62 => log_void(client_id, sequence, "CopyArea"),
        64 => log_void(client_id, sequence, "PolyPoint"),
        65 => {
            if let Some((gc_id, points)) = x11::poly_line_data(body) {
                let foreground = {
                    let s = lock_server(server)?;
                    s.resources.gc_foreground(ResourceId(gc_id))
                };
                if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.poly_line(foreground, header.data, points)?;
                }
            }
            log_void(client_id, sequence, "PolyLine")
        }
        66 => log_void(client_id, sequence, "PolySegment"),
        67 => log_void(client_id, sequence, "PolyRectangle"),
        68 => {
            if let Some((gc_id, arcs)) = x11::poly_arc_data(body) {
                let foreground = {
                    let s = lock_server(server)?;
                    s.resources.gc_foreground(ResourceId(gc_id))
                };
                if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.poly_arc(foreground, arcs)?;
                }
            }
            log_void(client_id, sequence, "PolyArc")
        }
        69 => log_void(client_id, sequence, "FillPoly"),
        70 => {
            if let Some((gc_id, rectangles)) = x11::poly_fill_rectangle_data(body) {
                let foreground = {
                    let s = lock_server(server)?;
                    s.resources.gc_foreground(ResourceId(gc_id))
                };
                if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.poly_fill_rectangle(foreground, rectangles)?;
                }
            }
            log_void(client_id, sequence, "PolyFillRectangle")
        }
        71 => {
            if let Some((gc_id, arcs)) = x11::poly_fill_arc_data(body) {
                let foreground = {
                    let s = lock_server(server)?;
                    s.resources.gc_foreground(ResourceId(gc_id))
                };
                if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.poly_fill_arc(foreground, arcs)?;
                }
            }
            log_void(client_id, sequence, "PolyFillArc")
        }
        72 => log_void(client_id, sequence, "PutImage"),
        74 => {
            if let Some((_drawable, gc_id, text_body)) = x11::poly_text_data(body) {
                let foreground = {
                    let s = lock_server(server)?;
                    s.resources.gc_foreground(ResourceId(gc_id))
                };
                if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.poly_text8(foreground, text_body)?;
                }
            }
            log_void(client_id, sequence, "PolyText8")
        }
        76 => {
            if let Some((drawable, gc_id, text_body)) = x11::image_text8_data(body) {
                debug!("focus text drawable 0x{drawable:x}");
                set_focused_window(focused_window, server, ResourceId(drawable))?;
                let gc = ResourceId(gc_id);
                let (foreground, background) = {
                    let s = lock_server(server)?;
                    (s.resources.gc_foreground(gc), s.resources.gc_background(gc))
                };
                if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.image_text8(foreground, background, header.data, text_body)?;
                }
            }
            log_void(client_id, sequence, "ImageText8")
        }
        78 => log_void(client_id, sequence, "CreateColormap"),
        84 => {
            log_reply(client_id, sequence, "AllocColor");
            let color = x11::alloc_color_request(body).unwrap_or_default();
            x11::write_alloc_color_reply(&mut *lock_writer()?, sequence, color)
        }
        85 => {
            let name = x11::alloc_named_color_name(body);
            let color = x11::lookup_color_name(&name).unwrap_or_else(|| {
                debug!(
                    "client {} #{} AllocNamedColor unknown name {:?} -> fallback gray",
                    client_id.0, sequence.0, name
                );
                x11::Rgb16 {
                    red: 0xc0c0,
                    green: 0xc0c0,
                    blue: 0xc0c0,
                }
            });
            debug!(
                "client {} #{} AllocNamedColor {:?}",
                client_id.0, sequence.0, name
            );
            x11::write_alloc_named_color_reply(&mut *lock_writer()?, sequence, color)
        }
        91 => {
            let pixels = x11::query_colors_pixels(body);
            debug!(
                "client {} #{} QueryColors {} pixels",
                client_id.0,
                sequence.0,
                pixels.len()
            );
            x11::write_query_colors_reply(&mut *lock_writer()?, sequence, &pixels)
        }
        92 => {
            let name = x11::alloc_named_color_name(body);
            let color = x11::lookup_color_name(&name).unwrap_or_else(|| {
                debug!(
                    "client {} #{} LookupColor unknown name {:?} -> fallback gray",
                    client_id.0, sequence.0, name
                );
                x11::Rgb16 {
                    red: 0xc0c0,
                    green: 0xc0c0,
                    blue: 0xc0c0,
                }
            });
            debug!(
                "client {} #{} LookupColor {:?}",
                client_id.0, sequence.0, name
            );
            x11::write_lookup_color_reply(&mut *lock_writer()?, sequence, color)
        }
        94 => {
            if let Some(cursor) = x11::create_glyph_cursor_id(body) {
                let new_id = cursor.0;
                let validation_failed = {
                    let s = lock_server(server)?;
                    let handle = s.clients.get(&client_id.0).expect("client registered");
                    let owned = crate::server::IdAllocator::validate_owned(
                        new_id,
                        handle.resource_id_base,
                        handle.resource_id_mask,
                    );
                    let in_use = s.resources.any_resource_exists(cursor);
                    !owned || in_use
                };
                if validation_failed {
                    return emit_x11_error(writer, sequence, x11::error::BAD_ID_CHOICE, new_id, 94);
                }
                {
                    let mut s = lock_server(server)?;
                    s.resources.create_glyph_cursor(client_id, cursor);
                }
            }
            log_void(client_id, sequence, "CreateGlyphCursor")
        }
        95 => {
            if let Some(cursor) = x11::free_resource_id(body) {
                let mut s = lock_server(server)?;
                s.resources.free_cursor(cursor);
            }
            log_void(client_id, sequence, "FreeCursor")
        }
        96 => log_void(client_id, sequence, "RecolorCursor"),
        98 => {
            let name = x11::query_extension_name(body);
            debug!(
                "client {} #{} QueryExtension {:?} -> absent",
                client_id.0, sequence.0, name
            );
            x11::write_query_extension_reply(&mut *lock_writer()?, sequence, false, 0, 0, 0)
        }
        99 => {
            log_reply(client_id, sequence, "ListExtensions");
            x11::write_list_extensions_reply(&mut *lock_writer()?, sequence)
        }
        101 => {
            log_reply(client_id, sequence, "GetKeyboardMapping");
            let first_keycode = body.first().copied().unwrap_or(0);
            let keycode_count = body.get(1).copied().unwrap_or(0);
            x11::write_get_keyboard_mapping_reply(
                &mut *lock_writer()?,
                sequence,
                first_keycode,
                keycode_count,
                4,
            )
        }
        103 => log_void(client_id, sequence, "Bell"),
        104 => log_void(client_id, sequence, "ChangeKeyboardControl"),
        108 => log_void(client_id, sequence, "SetScreenSaver"),
        110 => {
            log_reply(client_id, sequence, "ListHosts");
            x11::write_list_hosts_reply(&mut *lock_writer()?, sequence)
        }
        115 => {
            log_reply(client_id, sequence, "ForceScreenSaver");
            Ok(())
        }
        116 => log_void(client_id, sequence, "SetPointerMapping"),
        117 => {
            log_reply(client_id, sequence, "GetPointerMapping");
            x11::write_get_pointer_mapping_reply(&mut *lock_writer()?, sequence)
        }
        118 => log_void(client_id, sequence, "SetModifierMapping"),
        119 => {
            log_reply(client_id, sequence, "GetModifierMapping");
            x11::write_get_modifier_mapping_reply(&mut *lock_writer()?, sequence)
        }
        127 => log_void(client_id, sequence, "NoOperation"),
        opcode => {
            debug!(
                "client {} #{} unsupported opcode {} ({} bytes)",
                client_id.0,
                sequence.0,
                opcode,
                body.len() + 4
            );
            Ok(())
        }
    }
}

fn rewrite_reply_sequence(reply: &mut [u8], sequence: SequenceNumber) {
    if reply.len() >= 4 {
        let bytes = sequence.0.to_le_bytes();
        reply[2] = bytes[0];
        reply[3] = bytes[1];
    }
}

fn log_void(client_id: ClientId, sequence: SequenceNumber, name: &str) -> io::Result<()> {
    debug!("client {} #{} {name}", client_id.0, sequence.0);
    Ok(())
}

fn log_reply(client_id: ClientId, sequence: SequenceNumber, name: &str) {
    debug!("client {} #{} {name}", client_id.0, sequence.0);
}

fn window_attributes(
    window: Option<&Window>,
    all_event_masks: u32,
    your_event_mask: u32,
) -> x11::WindowAttributes {
    let window = window.expect("root window exists");
    x11::WindowAttributes {
        visual: window.visual,
        class: window.class.protocol_value(),
        bit_gravity: 1,
        win_gravity: 1,
        backing_planes: u32::MAX,
        backing_pixel: window.background_pixel,
        save_under: false,
        map_is_installed: true,
        map_state: window.map_state.protocol_value(),
        override_redirect: window.override_redirect,
        colormap: ROOT_COLORMAP,
        all_event_masks,
        your_event_mask,
        do_not_propagate_mask: 0,
    }
}

fn window_geometry(window: &Window) -> x11::Geometry {
    x11::Geometry {
        root: ROOT_WINDOW,
        x: window.x,
        y: window.y,
        width: window.width,
        height: window.height,
        border_width: window.border_width,
        depth: window.depth,
    }
}

fn pixmap_geometry(pixmap: &Pixmap) -> x11::Geometry {
    x11::Geometry {
        root: ROOT_WINDOW,
        x: 0,
        y: 0,
        width: pixmap.width,
        height: pixmap.height,
        border_width: 0,
        depth: pixmap.depth,
    }
}
