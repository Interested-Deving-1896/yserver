#![allow(dead_code)]

use std::collections::HashMap;

use yserver_protocol::x11::{
    AtomId, ChangeWindowAttributesRequest, ClientId, ConfigureWindowRequest, CreateGcRequest,
    CreatePixmapRequest, CreateWindowRequest, FontMetrics, GcChange, ResourceId,
};

use crate::properties::PropertyValue;

pub const SERVER_OWNER: ClientId = ClientId(0);

pub const ROOT_WINDOW: ResourceId = ResourceId(0x100);
pub const ROOT_COLORMAP: ResourceId = ResourceId(0x101);
pub const ROOT_VISUAL: ResourceId = ResourceId(0x102);

#[derive(Debug)]
pub struct ResourceTable {
    windows: HashMap<u32, Window>,
    pixmaps: HashMap<u32, Pixmap>,
    gcs: HashMap<u32, Gc>,
    fonts: HashMap<u32, Font>,
    cursors: HashMap<u32, Cursor>,
}

impl ResourceTable {
    pub fn new() -> Self {
        let mut windows = HashMap::new();
        windows.insert(
            ROOT_WINDOW.0,
            Window {
                id: ROOT_WINDOW,
                parent: ROOT_WINDOW,
                children: Vec::new(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                border_width: 0,
                depth: 24,
                visual: ROOT_VISUAL,
                class: WindowClass::InputOutput,
                map_state: MapState::Viewable,
                background_pixel: 0x00ff_ffff,
                override_redirect: false,
                cursor: None,
                owner: SERVER_OWNER,
                properties: HashMap::new(),
            },
        );

        Self {
            windows,
            pixmaps: HashMap::new(),
            gcs: HashMap::new(),
            fonts: HashMap::new(),
            cursors: HashMap::new(),
        }
    }

    pub fn create_window(&mut self, owner: ClientId, request: CreateWindowRequest) {
        let window = Window {
            id: request.window,
            parent: request.parent,
            children: Vec::new(),
            x: request.x,
            y: request.y,
            width: request.width,
            height: request.height,
            border_width: request.border_width,
            depth: request.depth,
            visual: if request.visual.0 == 0 {
                ROOT_VISUAL
            } else {
                request.visual
            },
            class: WindowClass::from_protocol(request.class),
            map_state: MapState::Unmapped,
            background_pixel: request.background_pixel.unwrap_or(0x00ff_ffff),
            override_redirect: request.override_redirect.unwrap_or(false),
            cursor: None,
            owner,
            properties: HashMap::new(),
        };

        self.windows
            .entry(request.parent.0)
            .or_insert_with(|| Window::placeholder(request.parent))
            .children
            .push(request.window);
        self.windows.insert(request.window.0, window);
    }

    pub fn destroy_window(&mut self, id: ResourceId) -> Vec<ResourceId> {
        let mut destroyed = Vec::new();
        self.destroy_window_inner(id, &mut destroyed);
        destroyed
    }

    fn destroy_window_inner(&mut self, id: ResourceId, destroyed: &mut Vec<ResourceId>) {
        let Some(window) = self.windows.remove(&id.0) else {
            return;
        };
        if let Some(parent) = self.windows.get_mut(&window.parent.0) {
            parent.children.retain(|child| *child != id);
        }
        destroyed.push(id);
        for child in window.children {
            self.destroy_window_inner(child, destroyed);
        }
    }

    pub fn change_window_attributes(&mut self, request: ChangeWindowAttributesRequest) {
        if let Some(window) = self.windows.get_mut(&request.window.0) {
            if let Some(background_pixel) = request.background_pixel {
                window.background_pixel = background_pixel;
            }
            if let Some(cursor) = request.cursor {
                window.cursor = Some(cursor);
            }
        }
    }

    pub fn configure_window(&mut self, request: ConfigureWindowRequest) -> Option<&Window> {
        let window = self.windows.get_mut(&request.window.0)?;
        if let Some(x) = request.x {
            window.x = x;
        }
        if let Some(y) = request.y {
            window.y = y;
        }
        if let Some(width) = request.width {
            window.width = width;
        }
        if let Some(height) = request.height {
            window.height = height;
        }
        if let Some(border_width) = request.border_width {
            window.border_width = border_width;
        }
        Some(window)
    }

    pub fn map_window(&mut self, id: ResourceId) {
        if let Some(window) = self.windows.get_mut(&id.0) {
            window.map_state = MapState::Viewable;
        }
    }

    #[must_use]
    pub fn unmap_window(&mut self, id: ResourceId) -> bool {
        if id == ROOT_WINDOW {
            return false;
        }
        let Some(window) = self.windows.get_mut(&id.0) else {
            return false;
        };
        let was_mapped = window.map_state != MapState::Unmapped;
        window.map_state = MapState::Unmapped;
        was_mapped
    }

    pub fn window(&self, id: ResourceId) -> Option<&Window> {
        self.windows.get(&id.0)
    }

    pub fn children(&self, parent: ResourceId) -> &[ResourceId] {
        self.windows
            .get(&parent.0)
            .map_or(&[], |window| window.children.as_slice())
    }

    #[must_use]
    pub fn window_property(&self, w: ResourceId, atom: AtomId) -> Option<&PropertyValue> {
        self.windows.get(&w.0)?.properties.get(&atom)
    }

    pub fn set_window_property(&mut self, w: ResourceId, atom: AtomId, value: PropertyValue) {
        if let Some(window) = self.windows.get_mut(&w.0) {
            window.properties.insert(atom, value);
        }
    }

    pub fn delete_window_property(&mut self, w: ResourceId, atom: AtomId) -> Option<PropertyValue> {
        self.windows.get_mut(&w.0)?.properties.remove(&atom)
    }

    pub fn create_pixmap(&mut self, owner: ClientId, request: CreatePixmapRequest) {
        self.pixmaps.insert(
            request.pixmap.0,
            Pixmap {
                id: request.pixmap,
                drawable: request.drawable,
                width: request.width,
                height: request.height,
                depth: request.depth,
                owner,
            },
        );
    }

    pub fn free_pixmap(&mut self, id: ResourceId) {
        self.pixmaps.remove(&id.0);
    }

    pub fn pixmap(&self, id: ResourceId) -> Option<&Pixmap> {
        self.pixmaps.get(&id.0)
    }

    pub fn create_gc(&mut self, owner: ClientId, request: CreateGcRequest) {
        self.gcs.insert(
            request.gc.0,
            Gc {
                id: request.gc,
                drawable: request.drawable,
                foreground: request.foreground.unwrap_or(0),
                background: request.background.unwrap_or(0x00ff_ffff),
                line_width: request.line_width.unwrap_or(0),
                font: request.font,
                owner,
            },
        );
    }

    pub fn change_gc(&mut self, request: GcChange) {
        let gc = self.gcs.entry(request.gc.0).or_insert(Gc {
            id: request.gc,
            drawable: ResourceId(0),
            foreground: 0,
            background: 0x00ff_ffff,
            line_width: 0,
            font: None,
            owner: SERVER_OWNER,
        });
        if let Some(foreground) = request.foreground {
            gc.foreground = foreground;
        }
        if let Some(background) = request.background {
            gc.background = background;
        }
        if let Some(line_width) = request.line_width {
            gc.line_width = line_width;
        }
        if let Some(font) = request.font {
            gc.font = Some(font);
        }
    }

    pub fn free_gc(&mut self, id: ResourceId) {
        self.gcs.remove(&id.0);
    }

    pub fn gc(&self, id: ResourceId) -> Option<&Gc> {
        self.gcs.get(&id.0)
    }

    pub fn gc_foreground(&self, id: ResourceId) -> u32 {
        self.gc(id).map_or(0, |gc| gc.foreground)
    }

    pub fn gc_background(&self, id: ResourceId) -> u32 {
        self.gc(id).map_or(0x00ff_ffff, |gc| gc.background)
    }

    pub fn install_font(
        &mut self,
        owner: ClientId,
        id: ResourceId,
        name: String,
        host_xid: u32,
        metrics: FontMetrics,
    ) {
        self.fonts.insert(
            id.0,
            Font {
                id,
                name,
                host_xid,
                metrics,
                owner,
            },
        );
    }

    pub fn close_font(&mut self, id: ResourceId) -> Option<Font> {
        self.fonts.remove(&id.0)
    }

    pub fn font(&self, id: ResourceId) -> Option<&Font> {
        self.fonts.get(&id.0)
    }

    /// Resolve a FONTABLE id (either a Font or a GC carrying a font) to a `&Font`.
    pub fn fontable(&self, id: ResourceId) -> Option<&Font> {
        if let Some(font) = self.fonts.get(&id.0) {
            return Some(font);
        }
        let gc_font = self.gcs.get(&id.0).and_then(|gc| gc.font)?;
        self.fonts.get(&gc_font.0)
    }

    pub fn create_glyph_cursor(&mut self, owner: ClientId, id: ResourceId) {
        self.cursors.insert(id.0, Cursor { id, owner });
    }

    pub fn free_cursor(&mut self, id: ResourceId) {
        self.cursors.remove(&id.0);
    }

    /// Top-level windows owned by `client`: windows whose parent is *not*
    /// owned by the same client. Reachable descendants (regardless of
    /// owner) get destroyed transitively when each root is destroyed.
    pub fn collect_owned_window_roots(&self, client: ClientId, out: &mut Vec<ResourceId>) {
        for (raw_id, w) in &self.windows {
            if w.owner != client {
                continue;
            }
            let parent_owner = self.windows.get(&w.parent.0).map(|p| p.owner);
            if parent_owner != Some(client) {
                out.push(ResourceId(*raw_id));
            }
        }
    }

    /// Remove every non-window resource owned by `client`. Returns the
    /// `host_xid` of every removed font so the caller can issue host-side
    /// `CloseFont` after dropping the `ServerState` lock.
    pub fn remove_non_window_resources_owned_by(&mut self, client: ClientId) -> Vec<u32> {
        self.pixmaps.retain(|_, p| p.owner != client);
        self.gcs.retain(|_, g| g.owner != client);
        self.cursors.retain(|_, c| c.owner != client);
        let mut closed_fonts = Vec::new();
        self.fonts.retain(|_, f| {
            if f.owner == client {
                closed_fonts.push(f.host_xid);
                false
            } else {
                true
            }
        });
        closed_fonts
    }

    #[must_use]
    pub fn any_resource_exists(&self, id: ResourceId) -> bool {
        self.windows.contains_key(&id.0)
            || self.pixmaps.contains_key(&id.0)
            || self.gcs.contains_key(&id.0)
            || self.fonts.contains_key(&id.0)
            || self.cursors.contains_key(&id.0)
    }
}

#[derive(Clone, Debug)]
pub struct Window {
    pub id: ResourceId,
    pub parent: ResourceId,
    pub children: Vec<ResourceId>,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub border_width: u16,
    pub depth: u8,
    pub visual: ResourceId,
    pub class: WindowClass,
    pub map_state: MapState,
    pub background_pixel: u32,
    pub override_redirect: bool,
    pub cursor: Option<ResourceId>,
    pub owner: ClientId,
    pub properties: HashMap<AtomId, PropertyValue>,
}

impl Window {
    fn placeholder(id: ResourceId) -> Self {
        Self {
            id,
            parent: ROOT_WINDOW,
            children: Vec::new(),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            border_width: 0,
            depth: 24,
            visual: ROOT_VISUAL,
            class: WindowClass::InputOutput,
            map_state: MapState::Unmapped,
            background_pixel: 0x00ff_ffff,
            override_redirect: false,
            cursor: None,
            owner: SERVER_OWNER,
            properties: HashMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowClass {
    CopyFromParent,
    InputOutput,
    InputOnly,
    Other(u16),
}

impl WindowClass {
    fn from_protocol(value: u16) -> Self {
        match value {
            0 => Self::CopyFromParent,
            1 => Self::InputOutput,
            2 => Self::InputOnly,
            value => Self::Other(value),
        }
    }

    pub fn protocol_value(self) -> u16 {
        match self {
            Self::CopyFromParent => 0,
            Self::InputOutput => 1,
            Self::InputOnly => 2,
            Self::Other(value) => value,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapState {
    Unmapped,
    Unviewable,
    Viewable,
}

impl MapState {
    pub fn protocol_value(self) -> u8 {
        match self {
            Self::Unmapped => 0,
            Self::Unviewable => 1,
            Self::Viewable => 2,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Pixmap {
    pub id: ResourceId,
    pub drawable: ResourceId,
    pub width: u16,
    pub height: u16,
    pub depth: u8,
    pub owner: ClientId,
}

#[derive(Clone, Debug)]
pub struct Gc {
    pub id: ResourceId,
    pub drawable: ResourceId,
    pub foreground: u32,
    pub background: u32,
    pub line_width: u16,
    pub font: Option<ResourceId>,
    pub owner: ClientId,
}

#[derive(Clone, Debug)]
pub struct Font {
    pub id: ResourceId,
    pub name: String,
    pub host_xid: u32,
    pub metrics: FontMetrics,
    pub owner: ClientId,
}

#[derive(Clone, Debug)]
pub struct Cursor {
    pub id: ResourceId,
    pub owner: ClientId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use yserver_protocol::x11::{ClientId, CreateWindowRequest};

    fn make_window(table: &mut ResourceTable, id: u32) {
        table.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(id),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                background_pixel: None,
                event_mask: None,
                override_redirect: None,
            },
        );
    }

    #[derive(Debug, Clone, Copy)]
    enum InitialState {
        Viewable,
        Unviewable,
        Unmapped,
    }

    fn arb_initial() -> impl Strategy<Value = InitialState> {
        prop_oneof![
            Just(InitialState::Viewable),
            Just(InitialState::Unviewable),
            Just(InitialState::Unmapped),
        ]
    }

    #[test]
    fn unmap_window_returns_true_on_transition_from_viewable() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x100002);
        table.map_window(ResourceId(0x100002));
        assert_eq!(
            table.window(ResourceId(0x100002)).unwrap().map_state,
            MapState::Viewable
        );
        let was_mapped = table.unmap_window(ResourceId(0x100002));
        assert!(was_mapped);
        assert_eq!(
            table.window(ResourceId(0x100002)).unwrap().map_state,
            MapState::Unmapped
        );
    }

    #[test]
    fn unmap_window_returns_true_on_transition_from_unviewable() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x100002);
        // Force Unviewable directly — no public setter, but the field is pub.
        table.windows.get_mut(&0x100002).unwrap().map_state = MapState::Unviewable;
        let was_mapped = table.unmap_window(ResourceId(0x100002));
        assert!(was_mapped);
        assert_eq!(
            table.window(ResourceId(0x100002)).unwrap().map_state,
            MapState::Unmapped
        );
    }

    #[test]
    fn unmap_window_returns_false_when_already_unmapped() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x100002);
        // create_window leaves new windows Unmapped.
        assert_eq!(
            table.window(ResourceId(0x100002)).unwrap().map_state,
            MapState::Unmapped
        );
        let first = table.unmap_window(ResourceId(0x100002));
        assert!(!first);
        let second = table.unmap_window(ResourceId(0x100002));
        assert!(!second);
    }

    #[test]
    fn unmap_window_returns_false_for_unknown_window() {
        let mut table = ResourceTable::new();
        let was_mapped = table.unmap_window(ResourceId(0x9999_9999));
        assert!(!was_mapped);
    }

    #[test]
    fn unmap_window_no_ops_on_root() {
        let mut table = ResourceTable::new();
        assert_eq!(
            table.window(ROOT_WINDOW).unwrap().map_state,
            MapState::Viewable
        );
        let was_mapped = table.unmap_window(ROOT_WINDOW);
        assert!(!was_mapped);
        assert_eq!(
            table.window(ROOT_WINDOW).unwrap().map_state,
            MapState::Viewable
        );
    }

    proptest! {
        #[test]
        fn unmap_window_state_machine(
            initial in arb_initial(),
            n in 1usize..=5,
        ) {
            let mut table = ResourceTable::new();
            make_window(&mut table, 0x100002);
            let target = ResourceId(0x100002);
            let initial_map_state = match initial {
                InitialState::Viewable => MapState::Viewable,
                InitialState::Unviewable => MapState::Unviewable,
                InitialState::Unmapped => MapState::Unmapped,
            };
            table.windows.get_mut(&target.0).unwrap().map_state = initial_map_state;

            let mut results = Vec::with_capacity(n);
            for _ in 0..n {
                results.push(table.unmap_window(target));
            }

            let expected_first = !matches!(initial, InitialState::Unmapped);
            prop_assert_eq!(results[0], expected_first);
            for r in results.iter().skip(1) {
                prop_assert!(!*r, "subsequent calls must return false");
            }
            prop_assert_eq!(
                table.window(target).unwrap().map_state,
                MapState::Unmapped
            );
        }
    }
}
