# Phase 6.2 host-X11 surface audit

Inputs to the Phase 6.2 implementation plan (Step 0). Generated
2026-05-03 against branch `phase6-2-backend-trait` at HEAD
`084312d` (plan v3 commit; identical to master tip `b45e876`
for non-doc files — no code changes between the two). All line
numbers are from the live source on that commit. Method-call enumeration is from grep over
`crates/yserver-core/src/nested.rs` and
`crates/yserver-core/src/server.rs`; field enumeration from grep over
`crates/yserver-core/src/resources.rs`.

`server.rs` does not call any host-X11 method directly — every
`crate::host_x11::*` reference there is a type or value import
(`HostPointerEvent`, `HostXidMap`, `PointerEventKind`). The trait
extraction therefore only needs to retarget `nested.rs`.

## host_xid-bearing fields in resources.rs

### Optional (`Option<u32>`) — 9 fields

- `Visual.host_visual_xid` (line 50) — set via `set_visual_host_xid`
- `Colormap.host_colormap_xid` (line 61) — set via `set_colormap_host_xid`
- `ReparentResult.host_xid` (line 111) — return-only struct
- `PictureState.host_owned_pixmap` (line 124) — pixmap created for the picture
- `Window.background_pixmap_host_xid` (line 1624)
- `Window.border_pixmap_host_xid` (line 1627)
- `Window.host_xid` (line 1632) — set when window is mirrored on host
- `Pixmap.host_xid` (line 1720) — set via `set_pixmap_host_xid`
- `Cursor.host_xid` (line 1766)

### Required (`u32`) — 8 fields

- `HostDrawableTarget::Window.host_xid` (line 68)
- `HostDrawableTarget::Pixmap.host_xid` (line 73)
- `PictureState.host_picture_xid` (line 123)
- `GlyphSetState.host_glyphset_xid` (line 130)
- `GcClipState::Pixmap.host_pixmap` (line 1539) — **not in plan
  template's expected 17-field inventory; see drift section below**
- `GcFillState::Tiled.host_pixmap` (line 1553)
- `NamedCompositePixmap.host_pixmap` (line 1567)
- `Font.host_xid` (line 1757)

### Map keys / non-struct — 1

- `ResourceTable.host_glyphset_refcounts: HashMap<u32, usize>`
  (line 142) — keyed by host xid

**Total: 17 struct fields + 1 map key = 18 distinct host-XID slots.**
The plan template predicted 16 + 1 = 17; the missing one is
`GcClipState::Pixmap.host_pixmap`.

## host method calls in nested.rs (deduplicated)

Captured via:

```sh
grep -oE "\b(host|h|hh)\.[a-z_][a-z0-9_]*\(" \
     crates/yserver-core/src/nested.rs \
     crates/yserver-core/src/server.rs \
  | sed -E 's/^[^:]+://; s/\($//' | sort -u
```

plus the inline-locked patterns
`host.lock().ok()?.query_pointer(...)` (`nested.rs:6890`) and
`host.lock().ok()?.xkb_info()?` (`nested.rs:393`).

False positives ignored (these are not host-X11 calls): `h.lock`,
`host.lock`, `host.clone`, `host.as_ref`, `host.to_le_bytes`,
`h.to_le_bytes`.

### Lifecycle / resource (14)

`create_subwindow`, `destroy_subwindow`, `map_subwindow`,
`unmap_subwindow`, `configure_subwindow`, `reparent_subwindow`,
`change_subwindow_attributes`, `create_pixmap`, `free_pixmap`,
`open_font`, `close_font`, `create_cursor`, `define_cursor`,
`name_window_pixmap`.

### Drawing primitives (17)

`copy_area`, `copy_plane`, `put_image`, `get_image`, `poly_line`,
`poly_segment`, `poly_rectangle`, `poly_arc`, `poly_point`,
`poly_fill_rectangle`, `poly_fill_arc`, `poly_text8`, `poly_text16`,
`image_text8`, `image_text16`, `fill_poly`, `fill_rectangle`.

Per the design, every drawing method takes a `&DrawState` carrying
the resolved foreground / clip / fill bundle.

### GC state (5)

`clear_clip_rectangles`, `set_clip_rectangles`, `set_clip_pixmap`,
`set_gc_fill_solid`, `set_gc_fill_tiled`.

In Step 3 these are folded into the `&DrawState` resolution and
should disappear from the public trait surface; for the audit they
are listed as separate trait methods because they are called
separately today (see `apply_gc_*` helpers below).

### State accessors (10)

`window_id`, `root_visual_xid`, `argb_visual_xid`,
`argb_colormap_xid`, `render_opcode`, `xkb_opcode`, `xkb_info`,
`composite_opcode`, `render_format_for_ynest_id`, `ping`.

### Other host calls (1)

`query_pointer` — accessed via inline
`host.lock().ok()?.query_pointer(...)` at `nested.rs:6890`.

### Extension: RENDER (19)

`render_create_picture`, `render_change_picture`,
`render_free_picture`, `render_create_glyphset`,
`render_free_glyphset`, `render_add_glyphs`, `render_free_glyphs`,
`render_composite`, `render_composite_glyphs`,
`render_fill_rectangles`, `render_trapezoids`,
`render_create_solid_fill`, `render_create_linear_gradient`,
`render_create_radial_gradient`, `render_create_cursor`,
`render_set_picture_clip_rectangles`, `render_set_picture_filter`,
`render_set_picture_transform`, `render_query_version`.

### Extension: other (3)

`xkb_proxy` (XKB), `xfixes_change_cursor_by_name` (XFIXES),
`set_shape_rectangles` (SHAPE).

### Misc (8)

`warp_pointer`, `list_fonts_proxy`, `list_fonts_with_info_proxy`,
`get_atom_name`, `get_keyboard_mapping`, `get_modifier_mapping`,
`set_container_background_pixel`,
`set_container_background_pixmap`.

### HostX11-specific (NOT on the trait)

- `allocate_xid` — called at `nested.rs:1450, 1683, 1848, 1885,
  1983, 2027, 2963, 4701, 5090, 7155` (10 sites). Internal and
  removed in Step 2 once handle newtypes own xid generation.
- Pump construction (`HostInputPump::open_from_env`) — called at
  `nested.rs:293, 605, 777`. Reached through a separately-held
  concrete `Arc<Mutex<HostX11>>` clone (post-Step-5:
  `Arc<Mutex<HostX11Backend>>`), not via the trait.

### Mutex plumbing (NOT on the trait)

`host.lock()`, `host.clone()`, `host.as_ref()` — `Arc<Mutex<_>>`
plumbing, untouched by Phase 6.2.

## apply_gc_* helpers in nested.rs

- `apply_gc_clip(host: &mut HostX11, state: &GcClipState)` —
  `nested.rs:4311`. Callers: `nested.rs:7384, 7440, 7475, 7498,
  7521, 7544, 7567, 7593, 7621, 7651, 7732, 7831, 7853, 7878, 7909`
  (15 sites).
- `apply_gc_fill_state(host: &mut HostX11, state: GcFillState)` —
  `nested.rs:4329`. Callers: `nested.rs:7594, 7622, 7652` (3 sites,
  paired with `apply_gc_clip` for `PolyFillRectangle`,
  `PolyFillArc`, `FillPoly`).

These move into Step 3's `DrawState` resolution; the resolved
`DrawState` is passed as `&DrawState` to each drawing trait method,
removing the explicit `apply_gc_clip` / `apply_gc_fill_state` calls
at every draw site.

## Pump and `window_id()` access patterns (Step 5 reference)

These are the non-trait paths that need direct concrete
`Arc<Mutex<HostX11>>` (post-Step-5: `Arc<Mutex<HostX11Backend>>`)
access in Step 5 (none of the post-lock variants below go through
the Backend trait without it):

- `HostInputPump::open_from_env`: `nested.rs:293, 605, 777`
- `host.lock().ok().map(|h| h.window_id())`: `nested.rs:5072, 5543`
  — used to resolve the host parent xid for `ROOT_WINDOW`
  in CreateWindow / ReparentWindow paths (post-lock)
- `host.lock().ok().map(|host| host.window_id())`: `nested.rs:257`
  (host-XID seeding during server bootstrap, post-lock)
- `host.window_id()` (pre-Arc, on the freshly-opened `HostX11` before
  it is wrapped in `Arc<Mutex<_>>`): `nested.rs:243` — bootstrap
  log only; not a lock-holding call

`window_id()` is itself a trait method (state accessor), so the
`host.lock().ok().map(|h| h.window_id())` shape continues to work
through the trait once the lock yields a `dyn Backend`. Pump
construction does not.

## Trait surface implication

| Category                | Trait methods |
| ----------------------- | ------------- |
| Lifecycle / resource    | 14            |
| Drawing primitives      | 17            |
| GC state                | 5             |
| State accessors         | 10            |
| Other (query_pointer)   | 1             |
| Extension: RENDER       | 19            |
| Extension: other        | 3             |
| Misc                    | 8             |
| **Total**               | **77**        |

Plus, *not* on the trait: `allocate_xid` (removed in Step 2), pump
construction, and `Arc<Mutex<_>>` plumbing.

The trait should:
- Take `&DrawState` on every drawing method (per the design doc).
- Return owned `WindowHandle` / `PixmapHandle` / etc. on `create_*`
  (Step 2).
- Provide all 10 state accessors as methods (no fields exposed
  through the trait).
- NOT include pump construction, `allocate_xid`, or `lock`.

## Drift from the plan template

The plan template (lines 205-339 of
`docs/superpowers/plans/2026-05-03-phase6-2-backend-trait.md`)
predicted certain numbers that the audit corrects:

1. **Field count: 17 → 18.** The plan template did not list
   `GcClipState::Pixmap.host_pixmap` (line 1539). It is a Required
   `u32`, parallel to `GcFillState::Tiled.host_pixmap`. Step 1 must
   wrap it in a handle just like the fill-state one. The total is
   17 struct fields + 1 map key (refcounts), not 16 + 1.

2. **Line numbers off by 1–2 for five fields (table below).** All
   five line-number predictions in the template's field inventory
   drifted as the file grew. Trivial, mentioned for completeness so
   a reader cross-referencing the template's exact line numbers
   does not assume the structures moved:

   | Field | Template | Actual |
   |-------|----------|--------|
   | `Window.host_xid` | 1631 | 1632 |
   | `Window.background_pixmap_host_xid` | 1623 | 1624 |
   | `Window.border_pixmap_host_xid` | 1626 | 1627 |
   | `Pixmap.host_xid` | 1719 | 1720 |
   | `GcFillState::Tiled.host_pixmap` | 1551 | 1553 |

3. **Trait method total: ~110 → 77.** The plan template's "~110
   trait methods" estimate over-counted. The audit lands at 77
   trait methods with the categorization the plan asked for. The
   gap is mostly that the plan template counted variants of
   `poly_text*` / `image_text*` separately and inflated some
   categories. RENDER still contributes 19; Misc 8; the headline
   number is just lower.

4. **Lifecycle/resource header `(12)` vs 15-name list.** The
   template's Lifecycle section
   (`docs/superpowers/plans/2026-05-03-phase6-2-backend-trait.md:246-252`)
   uses the header `### Lifecycle / resources (12)` but enumerates
   15 distinct names. The audit lists 14 — the same set minus
   `allocate_xid`, which is intentionally dropped because it is
   not on the trait (it disappears in Step 2). Read the audit's 14
   as authoritative for trait surface; the template's `(12)` is a
   stale header count.

5. **State accessor count: 8 → 10.** The template's prose at
   `2026-05-03-phase6-2-backend-trait.md:325` says "Provide all 8
   state accessors as methods", but the audit enumerates 10:
   `window_id`, `root_visual_xid`, `argb_visual_xid`,
   `argb_colormap_xid`, `render_opcode`, `xkb_opcode`, `xkb_info`,
   `composite_opcode`, `render_format_for_ynest_id`, `ping`. The
   two extras (`xkb_info`, `render_format_for_ynest_id`) are
   inline-locked in `nested.rs` and were missed by the template's
   prose count. The audit overrides the template here — Step 5
   should expose 10 accessors.

6. **No methods from the template's expected list are missing**
   from the codebase. Every named method exists in
   `crates/yserver-core/src/host_x11.rs` (verified by spot-grepping
   `pub fn <name>` for each enumerated method) and is reachable
   from `nested.rs`.

7. **No callers of `query_pointer` or `xkb_info` go through a
   `host.METHOD` shape** — they are invoked through inline
   `host.lock().ok()?.METHOD(...)` chains. The plan flagged this
   correctly; the broader grep pattern suggested in Task 0.1
   confirmed the hits.

## Findings the plan didn't anticipate

- **`render_query_version` call shape is inline-locked, not
  `host.METHOD`.** The method itself is already on the template's
  RENDER list (template line ~285), so Step 5 implementers should
  *not* spend time deciding whether to add it to the trait — it is
  already in the trait spec. The novel finding is the call shape:
  it is invoked via `.and_then(|mut h| h.render_query_version().ok())`
  at `nested.rs:1398`, which is the same inline-lock pattern used
  for `query_pointer` / `xkb_info` — i.e. it is one of the
  callsites Step 5 must rewrite to use the locked-trait-object
  shape, not a missing-method discovery.

- **`host_window_id` field on the per-client context** — the
  bootstrap hands the host window xid down to `handle_client` as a
  bare `u32` (`nested.rs:351, 509, 604`), not via the `host` Arc.
  Step 5 doesn't need to thread this through the Backend trait; it
  is already plumbed independently.

- **`apply_gc_fill_state` is called only 3 times** (paired with
  `apply_gc_clip` at fill-using draws), versus 15 sites for
  `apply_gc_clip` alone. The DrawState collapse in Step 3 should
  account for this asymmetry: solid-foreground draws (PolyLine,
  PolyArc, PolyPoint, PolyRectangle, PolySegment, PolyText*,
  ImageText*) only need clip; fill draws (PolyFillRectangle,
  PolyFillArc, FillPoly) need clip + fill.

- **`hh.copy_plane` (lowercase double-h) is unique** —
  `nested.rs:7441` is the only host method called via `hh.` rather
  than `host.` / `h.`. This is just a local variable rename
  (`if let Ok(mut hh) = host_arc.lock()`) and is mechanical to
  unify in Step 5.
