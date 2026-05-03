# Phase 6.2 — `Backend` trait extraction implementation plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Carve a `Backend` trait out of `yserver-core` so request
handlers in `nested.rs` and `server.rs` call into it (via
`Arc<Mutex<dyn Backend>>` for hot-path drawing/resource methods) and
a future KMS backend slots in. Lands three of the five C-prework
items from the 6.1 design (per-kind handle newtypes; bundle
`allocate_xid` into `create_*`; `&DrawState` by-borrow per drawing
call). The pump/main connection merge is explicitly **deferred** to
its own slice. Per-client kb pump construction stays direct against
the concrete `HostX11Backend` (the kb pump's `open_from_env(host_window_id)`
call survives unchanged in 6.2).

**Architecture:** Same `Arc<Mutex<dyn Backend>>` shape for the hot
request path as today's `Arc<Mutex<HostX11>>`. The pump construction
sites at `nested.rs:292-328` (main `HostInputPump`) and
`nested.rs:604` (per-client kb pump factory) keep a separate
concrete `Arc<Mutex<HostX11Backend>>` clone for direct access — no
downcast helper, no specialization tricks. The startup wiring
creates one `HostX11Backend`, holds an `Arc<Mutex<HostX11Backend>>`,
hands it to pump construction, and `clone`s it as
`Arc<Mutex<dyn Backend>>` for the request-handler dispatch.
Drawing
methods take `&DrawState` (no `GcHandle` on the trait). Per-kind
handle newtypes (`WindowHandle`, `PixmapHandle`, …) replace the 17
`u32`/`Option<u32>` host-XID slots in `resources.rs`. `Gc` (in
`resources.rs`) is **expanded** to store the full GC attribute set
that `DrawState` carries — additive change driven by the
"we'll need all GC fields anyway for KMS" decision; not silently
required by the trait extraction.

**Tech Stack:** Rust 2024 edition, existing workspace deps. No new
crate dependencies. The trait surface is ~70 methods; module layout
adds `crates/yserver-core/src/backend/` and converts
`crates/yserver-core/src/host_x11.rs` (3,643 lines) into a
`host_x11/` module.

**Branch:** Create `phase6-2-backend-trait` for development.
Squash-merge to master matching Phase 6.1's pattern. Per-step commits
during development for bisect.

**Companion design:** `docs/superpowers/specs/2026-05-03-phase6-2-backend-trait-design.md`.

**Codex review:** Plan went through three codex passes (2026-05-03).
Notable corrections folded in across passes: actual host-XID field
inventory (17 fields, several non-standard names); `Gc` expansion
explicitly called out as additive behavioral scope (not silent);
`apply_gc_clip` correctly placed in `nested.rs` not `host_x11.rs`;
full audited trait surface (~72 methods, including missed
`copy_plane` / `query_pointer` / `xkb_info`); pump construction
kept against a separate concrete `Arc<Mutex<HostX11Backend>>` clone
(no downcast helper); Step 1 smoke gate added; `RecordingBackend`
size estimate corrected; correct GC mask values; explicit `CopyGC`
+ `SetDashes` task. See "Codex review log" at the end.

---

## Status

Not started. Seven steps pending (was six; Step 0 added to ground the
trait surface in actual call-site data before Step 5).

## Strategy

Each numbered Step is one logical commit on `phase6-2-backend-trait`.
The order is chosen so `cargo build` + `cargo test --workspace` are
green at every commit. The branch squash-merges at the end; per-step
history is preserved during development for bisection.

After every commit:

```sh
cargo +nightly fmt
cargo clippy --workspace --all-targets
cargo test --workspace
```

All three must pass. Manual smoke gates fire at Steps 1, 2, 3, 5, 6.

## Pre-flight

Before Step 0 starts, on master:

```sh
git checkout master && git pull
git checkout -b phase6-2-backend-trait
```

Sanity-check the baseline:

```sh
cargo +nightly fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

All three must pass before starting. If any fails on master, stop and
fix that first.

---

## Step 0 — Audit the host-X11 surface

**Goal:** Produce a concrete enumeration of every `host.X` /
`h.X` method call and state-accessor in `nested.rs` and `server.rs`,
plus every `host_*xid` field in `resources.rs`. The output is a
checked-in audit document that becomes the source of truth for
Step 1 (field inventory) and Step 5 (trait surface). Without this
step, Step 5 ends up with a placeholder trait and a wave of
"missing method" errors when the `Arc<Mutex<dyn Backend>>` switch
lands.

**Files:**
- Create: `docs/superpowers/notes/2026-05-03-phase6-2-host-surface-audit.md`

**Estimate:** 30–45 minutes. Pure read + grep + write.

### Task 0.1 — Enumerate host method calls

The grep below catches more access shapes than the v1 audit caught.
Code uses several patterns: `host.X(...)`, `h.X(...)` (after `let h
= host.lock()`), `hh.X(...)` (renamed binding), and
`host.lock().ok()?.X(...)` (one-shot inline access).

```sh
{
  echo "## Calls that look like host_ref.METHOD(... — covers host./h./hh./*"
  grep -oE "\b(host|h|hh|host_x11|x11_host)[a-z_]*\.([a-z_][a-z0-9_]*)\(" \
       crates/yserver-core/src/nested.rs \
       crates/yserver-core/src/server.rs \
    | sed -E 's/^[^.]+\.//' \
    | sort -u
  echo
  echo "## Inline-locked access shapes: host.lock().*().METHOD(..."
  grep -oE "host\.lock\([^)]*\)[^.]*\.[a-z_]+\(" \
       crates/yserver-core/src/nested.rs \
       crates/yserver-core/src/server.rs \
    | sort -u
} > /tmp/phase6-2-host-calls.txt
```

Read `/tmp/phase6-2-host-calls.txt`. Cross-check that you see (at minimum):
`copy_plane`, `query_pointer`, `xkb_info` — these were missed by the
v1/v2 audit's narrower grep. If anything else is novel against the
v2 inventory in this plan's "Resource fields & host method
inventory" notes, fold it in.

The actual call list when you run the grep may differ as code
evolves; treat the audit document as the authoritative inventory,
not this plan's enumeration.

### Task 0.2 — Categorize the calls

For each entry, classify:

- **Lifecycle/resource** (create_*, destroy_*, free_*, open_font,
  close_font, define_cursor, name_window_pixmap, etc.) → trait method.
- **Drawing** (poly_*, fill_*, copy_area, put_image, get_image,
  set_clip_*, set_gc_fill_*, clear_clip_rectangles, image_text*,
  poly_text*) → trait method, takes `&DrawState`.
- **Window ops** (map_subwindow, unmap_subwindow, configure_subwindow,
  reparent_subwindow, change_subwindow_attributes) → trait method.
- **State accessors** (window_id, root_visual_xid, argb_visual_xid,
  argb_colormap_xid, render_opcode, xkb_opcode, composite_opcode,
  render_format_for_ynest_id, ping) → trait method, returns the value.
- **Extension proxies** (every render_*, xkb_proxy,
  xfixes_change_cursor_by_name, set_shape_rectangles) → trait method.
- **Misc** (warp_pointer, list_fonts_*, get_atom_name,
  get_keyboard_mapping, get_modifier_mapping,
  set_container_background_*) → trait method.
- **HostX11Backend-specific** (allocate_xid — internal, removed in
  Step 2; pump construction APIs accessed via a separate concrete
  `Arc<Mutex<HostX11Backend>>` clone) → not a trait method.
- **Mutex plumbing** (lock, clone, as_ref) — not a trait method;
  these are operations on the `Arc<Mutex<>>` wrapper, not the
  backend itself.

### Task 0.3 — Enumerate host_xid fields in resources.rs

```sh
grep -nE "host_[a-z_]*xid|host_(picture|glyphset|colormap|visual|pixmap|cursor|font)" \
     crates/yserver-core/src/resources.rs \
  | grep -E "^\s*[0-9]+:\s*pub\s|^\s*[0-9]+:\s*[a-z_]+:\s" \
  > /tmp/phase6-2-fields.txt
```

Read `/tmp/phase6-2-fields.txt`. Cross-check against the 17-field
inventory in the audit document below — note any drift.

### Task 0.4 — Enumerate kb-pump and pump-related access patterns

```sh
grep -nE "HostInputPump::open_from_env|window_id\(\)|host_window_id" \
     crates/yserver-core/src/nested.rs \
     crates/yserver-core/src/server.rs
```

Note every site. These are the non-trait paths that need
direct concrete-`Arc<Mutex<HostX11Backend>>` access in Step 5.

### Task 0.5 — Write the audit document

Create `docs/superpowers/notes/2026-05-03-phase6-2-host-surface-audit.md`:

```markdown
# Phase 6.2 host-X11 surface audit

Inputs to the Phase 6.2 implementation plan. Generated 2026-05-03 against
the master branch at the start of the phase.

## host_xid-bearing fields in resources.rs

### Optional (`Option<u32>`)

- `Visual.host_visual_xid` (line 50) — set via `set_visual_host_xid`
- `Colormap.host_colormap_xid` (line 61) — set via `set_colormap_host_xid`
- `Window.host_xid` (line 1631) — set when the window is mirrored on host
- `Window.background_pixmap_host_xid` (line 1623)
- `Window.border_pixmap_host_xid` (line 1626)
- `Pixmap.host_xid` (line 1719) — set via `set_pixmap_host_xid`
- `Cursor.host_xid` (line 1766)
- `PictureState.host_owned_pixmap` (line 124) — pixmap created for the picture
- `ReparentResult.host_xid` (line 111) — return-only struct

### Required (`u32`)

- `Font.host_xid` (line 1757)
- `PictureState.host_picture_xid` (line 123)
- `GlyphSetState.host_glyphset_xid` (line 130)
- `NamedCompositePixmap.host_pixmap` (line 1567)
- `GcFillState::Tiled.host_pixmap` (line 1551)
- `HostDrawableTarget::Window.host_xid` (line 68)
- `HostDrawableTarget::Pixmap.host_xid` (line 73)

### Map keys / non-struct

- `ResourceTable.host_glyphset_refcounts: HashMap<u32, usize>` (line 142) —
  keyed by host xid

Total: 16 struct fields + 1 map key = 17 distinct host-XID slots.

## host. and h. method calls in nested.rs (deduplicated)

### Lifecycle / resources (12)

`allocate_xid`, `create_subwindow`, `destroy_subwindow`,
`map_subwindow`, `unmap_subwindow`, `configure_subwindow`,
`reparent_subwindow`, `change_subwindow_attributes`,
`create_pixmap`, `free_pixmap`, `open_font`, `close_font`,
`create_cursor`, `define_cursor`, `name_window_pixmap`.

### Drawing (17)

`copy_area`, `copy_plane`, `put_image`, `get_image`, `poly_line`,
`poly_segment`, `poly_rectangle`, `poly_arc`, `poly_point`,
`poly_fill_rectangle`, `poly_fill_arc`, `poly_text*`,
`image_text*`, `fill_poly`, `fill_rectangle`.

GC state: `clear_clip_rectangles`, `set_clip_rectangles`,
`set_clip_pixmap`, `set_gc_fill_solid`, `set_gc_fill_tiled`.

### State accessors (10)

`window_id`, `root_visual_xid`, `argb_visual_xid`,
`argb_colormap_xid`, `render_opcode`, `xkb_opcode`, `xkb_info`,
`composite_opcode`, `render_format_for_ynest_id`, `ping`.

### Other host calls (1)

`query_pointer` — accessed via inline `host.lock().ok()?.query_pointer(...)`
at `nested.rs:6890`. Becomes a trait method like the rest.

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

### HostX11-specific (not on the trait)

`allocate_xid` — disappears in Step 2.
Pump construction (`HostInputPump::open_from_env`) — accessed via
a separately-held concrete `Arc<Mutex<HostX11Backend>>` clone passed
to the per-client kb pump factory at `nested.rs:604`.

### apply_gc_* helpers in nested.rs

- `apply_gc_clip(host: &mut HostX11, state: &GcClipState)` —
  `nested.rs:4311`
- `apply_gc_fill_state(host: &mut HostX11, state: GcFillState)` —
  `nested.rs:4329`

These move into Step 3's `DrawState` resolution.

## Trait surface implication

~110 trait methods (vs the design doc's "~95" estimate; the
difference is the 19 RENDER methods, several state accessors, and
the misc category — all of which the design doc's "..."
placeholder elided).

The trait should:
- Take `&DrawState` on every drawing method (per the design).
- Return owned `WindowHandle` etc. on `create_*` (Step 2).
- Provide all 8 state accessors as methods (no fields exposed
  through the trait).
- NOT include pump construction, `allocate_xid`, or `lock`.
```

Read it back to yourself; it should make sense end-to-end.

### Task 0.6 — Commit

```sh
git add docs/superpowers/notes/2026-05-03-phase6-2-host-surface-audit.md
git commit -m "docs: Phase 6.2 host-X11 surface audit"
```

The commit is squashed into the final feat: commit on merge, but
the audit document file stays in the tree as `docs/superpowers/notes/2026-05-03-phase6-2-host-surface-audit.md`.
`status.md` references it; it serves as execution evidence and
helps reviewers (and future readers re-running similar audits)
understand the actual call-site data the trait was carved against.

---

## Step 1 — Per-kind handle newtypes (prework #3 + #4)

**Goal:** Replace the 17 host-XID slots in `yserver-core` with
per-kind handle newtypes. Required slots use the handle directly;
optional slots use `Option<KindHandle>`. Compiler-driven type churn.

**Files:**
- Create: `crates/yserver-core/src/backend/mod.rs`
- Create: `crates/yserver-core/src/backend/handles.rs`
- Modify: `crates/yserver-core/src/lib.rs` (add `pub mod backend;`)
- Modify: `crates/yserver-core/src/resources.rs` (~17 field types)
- Modify: `crates/yserver-core/src/host_x11.rs` (~100 call sites)
- Modify: `crates/yserver-core/src/nested.rs` (~170 call sites)
- Modify: `crates/yserver-core/src/server.rs` (~10 call sites)

**Estimate:** ~10 files touched, ~400 LoC churn, ~280 call sites
adjusted mechanically.

### Task 1.1 — Create the handles module

Write `crates/yserver-core/src/backend/handles.rs`:

```rust
//! Per-kind newtypes wrapping host XIDs (or, in future backends,
//! native resource handles). All are `NonZeroU32` so that `0`
//! (X11's reserved value used as the None sentinel) is statically
//! unrepresentable in the success type and `Option<KindHandle>`
//! costs one word.

use std::num::NonZeroU32;

macro_rules! handle {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
        pub struct $name(NonZeroU32);

        impl $name {
            pub fn from_raw(raw: u32) -> Option<Self> {
                NonZeroU32::new(raw).map($name)
            }

            pub fn from_raw_panicking(raw: u32) -> Self {
                Self::from_raw(raw)
                    .unwrap_or_else(|| panic!("{} from zero raw", stringify!($name)))
            }

            pub fn as_raw(self) -> u32 {
                self.0.get()
            }

            #[cfg(test)]
            pub fn from_raw_for_test(raw: u32) -> Self {
                Self::from_raw_panicking(raw)
            }
        }
    };
}

handle!(WindowHandle, "Backend handle for an X11 InputOutput / InputOnly window.");
handle!(PixmapHandle, "Backend handle for a pixmap.");
handle!(PictureHandle, "Backend handle for a RENDER picture.");
handle!(GlyphSetHandle, "Backend handle for a RENDER glyphset.");
handle!(FontHandle, "Backend handle for an opened font.");
handle!(CursorHandle, "Backend handle for a cursor.");
handle!(ColormapHandle, "Backend handle for a colormap.");
handle!(VisualHandle, "Backend handle for a visual.");

#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
pub enum AnyHandle {
    Window(WindowHandle),
    Pixmap(PixmapHandle),
}

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum HandleKind {
    Window, Pixmap, Picture, GlyphSet, Font, Cursor, Colormap, Visual,
}

impl AnyHandle {
    pub fn kind(self) -> HandleKind {
        match self {
            AnyHandle::Window(_) => HandleKind::Window,
            AnyHandle::Pixmap(_) => HandleKind::Pixmap,
        }
    }

    pub fn as_raw(self) -> u32 {
        match self {
            AnyHandle::Window(h) => h.as_raw(),
            AnyHandle::Pixmap(h) => h.as_raw(),
        }
    }
}

impl From<WindowHandle> for AnyHandle {
    fn from(h: WindowHandle) -> Self { AnyHandle::Window(h) }
}

impl From<PixmapHandle> for AnyHandle {
    fn from(h: PixmapHandle) -> Self { AnyHandle::Pixmap(h) }
}
```

### Task 1.2 — Create backend module entry point

Write `crates/yserver-core/src/backend/mod.rs`:

```rust
//! Backend abstraction. Currently `HostX11Backend` is the sole impl;
//! Phase 6.3+ will add a KMS backend.

pub mod handles;

pub use handles::{
    AnyHandle, ColormapHandle, CursorHandle, FontHandle, GlyphSetHandle,
    HandleKind, PictureHandle, PixmapHandle, VisualHandle, WindowHandle,
};
```

Add `pub mod backend;` to `crates/yserver-core/src/lib.rs`.

Run `cargo build -p yserver-core`. Expect: success (no consumers yet).

### Task 1.3 — Re-type required (`u32`) fields one at a time

These are the seven required-only fields. They become non-`Option`
handles directly. **One commit per field**, with `cargo build` green
between each, so a regression bisects cleanly.

Order chosen so each re-type is independent (no field depends on
another's new type):

1. `Font.host_xid: u32` → `host_xid: FontHandle`
2. `PictureState.host_picture_xid: u32` → `host_picture_xid: PictureHandle`
3. `GlyphSetState.host_glyphset_xid: u32` → `host_glyphset_xid: GlyphSetHandle`
4. `NamedCompositePixmap.host_pixmap: u32` → `host_pixmap: PixmapHandle`
5. `GcFillState::Tiled.host_pixmap: u32` → `host_pixmap: PixmapHandle`
6. `HostDrawableTarget::Window.host_xid: u32` → `host_xid: WindowHandle`
7. `HostDrawableTarget::Pixmap.host_xid: u32` → `host_xid: PixmapHandle`

For each: re-type the field, run `cargo build`, walk every error.
Most errors are at constructor sites where today the code does:

```rust
PictureState { client, host_picture_xid: xid, host_owned_pixmap }
```

The fix is:

```rust
PictureState {
    client,
    host_picture_xid: PictureHandle::from_raw_panicking(xid),
    host_owned_pixmap: host_owned_pixmap.and_then(PixmapHandle::from_raw),
}
```

**Note on `host_glyphset_refcounts: HashMap<u32, usize>`:** the
key stays `u32` for now (it's a deduplication index, not a typed
slot). When the trait switch happens in Step 5, `nested.rs` callers
pass `glyphset_handle.as_raw()` for refcount queries.

### Task 1.4 — Re-type optional (`Option<u32>`) fields one at a time

Nine optional fields, each one its own task:

1. `Visual.host_visual_xid` → `Option<VisualHandle>`
2. `Colormap.host_colormap_xid` → `Option<ColormapHandle>`
3. `Window.host_xid` → `Option<WindowHandle>`
4. `Window.background_pixmap_host_xid` → `Option<PixmapHandle>`
5. `Window.border_pixmap_host_xid` → `Option<PixmapHandle>`
6. `Pixmap.host_xid` → `Option<PixmapHandle>`
7. `Cursor.host_xid` → `Option<CursorHandle>`
8. `PictureState.host_owned_pixmap` → `Option<PixmapHandle>`
9. `ReparentResult.host_xid` → `Option<WindowHandle>`

Same pattern: re-type, build, walk errors.

Common error patterns:

- `let xid: u32 = window.host_xid.unwrap();` → wrap value in handle:
  `let h = window.host_xid.unwrap();` (now `WindowHandle`); call
  `.as_raw()` only when feeding a `u32`-keyed map or wire byte
  buffer.
- `xid_map.insert(window.host_xid.unwrap(), id);` → key is
  `u32`, so `xid_map.insert(window.host_xid.unwrap().as_raw(), id);`.
- `if window.host_xid == Some(other_xid)` where `other_xid: u32` →
  `if window.host_xid.map(|h| h.as_raw()) == Some(other_xid)`.
  Most of these go away naturally as `other_xid` becomes a
  `WindowHandle` too.

### Task 1.5 — Audit `ClientRemovedResources` and other return structs

```sh
grep -nE "host_[a-z_]*xid:\s+(u32|Option<u32>)" \
     crates/yserver-core/src/*.rs
```

Anything still raw `u32` after Tasks 1.3 + 1.4 needs a decision:
- If it's an internal map key (`host_glyphset_refcounts`), leave as
  `u32` — these are wire-side and not type-safety-relevant.
- If it's a "list of XIDs to free on client disconnect", re-type
  to `Vec<HandleKindEnum>` or similar — concrete shape depends on
  the consumer.

### Task 1.6 — Run the full check suite

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo +nightly fmt --check
```

All green.

### Task 1.7 — Step 1 manual smoke gate

```sh
just ynest &
sleep 1
DISPLAY=:99 xterm     # type "ls", quit cleanly
```

Acceptance: xterm renders, accepts input, exits cleanly. No new
errors in the ynest log on stderr.

This catches `.as_raw()` mistakes in `xid_map` setup, root/container
host-id setup, pump registration writes, and background-pixmap
retention — none of which are type-system-detectable.

### Task 1.8 — Commit

```sh
git add crates/yserver-core/src/backend/ \
        crates/yserver-core/src/lib.rs \
        crates/yserver-core/src/resources.rs \
        crates/yserver-core/src/host_x11.rs \
        crates/yserver-core/src/nested.rs \
        crates/yserver-core/src/server.rs
git commit -m "feat: Phase 6.2 Step 1 — per-kind handle newtypes for host XIDs"
```

---

## Step 2 — Bundle `allocate_xid` into `create_*` (prework #1)

**Goal:** Eliminate the two-phase `let xid = host.allocate_xid();
host.create_subwindow(host_parent, host_xid, ...)?;` pattern in
favor of `let h = host.create_subwindow(host_parent, ...)?;`. Cleans
up call sites and pre-shapes the trait method signatures.

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs` (creator method signatures)
- Modify: `crates/yserver-core/src/nested.rs` (allocate-then-create call sites)
- Modify: `crates/yserver-core/src/resources.rs` (a few allocate-then-store sites)
- Modify: `crates/yserver-core/src/server.rs` (a few sites)

**Estimate:** ~5 files, ~300 LoC, ~50 call sites.

### Task 2.1 — Identify the actual creator methods

Run:

```sh
grep -nE "^\s*pub fn (create_|open_)" crates/yserver-core/src/host_x11.rs
```

Expected list (from the audit): `create_subwindow`, `create_pixmap`,
`create_cursor`, `open_font`, plus the RENDER family
(`render_create_picture`, `render_create_glyphset`,
`render_create_solid_fill`, `render_create_linear_gradient`,
`render_create_radial_gradient`, `render_create_cursor`).

Note: `create_argb_colormap` is private init-time code (allocated
once at startup, not per client request) — leave it alone, it's not
on the trait surface.

### Task 2.2 — Refactor `create_subwindow`

Current signature in `host_x11.rs`:

```rust
pub fn create_subwindow(&mut self, host_parent: u32, host_xid: u32, ...) -> io::Result<()>;
```

After:

```rust
pub fn create_subwindow(&mut self, host_parent: WindowHandle, ...) -> io::Result<WindowHandle>;
```

Inside, call `next_xid()` (rename of the now-private `allocate_xid`),
construct the handle from the raw, return it.

Build will fail; the next task fixes call sites.

### Task 2.3 — Walk every `create_subwindow` call site

In `nested.rs` and `resources.rs`:

Before:
```rust
let xid = host.allocate_xid();
host.create_subwindow(parent_host_xid, xid, ...)?;
window.host_xid = Some(WindowHandle::from_raw_panicking(xid));
```

After:
```rust
let h = host.create_subwindow(parent_handle, ...)?;
window.host_xid = Some(h);
```

If the call site also inserts into `xid_map`:

```rust
xid_map.insert(h.as_raw(), window.id);
```

### Task 2.4 — Apply the pattern to the other creator methods

In order (small first):

1. `create_pixmap` (~5 sites) → returns `PixmapHandle`
2. `create_cursor` (~3 sites) → returns `CursorHandle`
3. `open_font` (already mostly returns `(host_xid, FontMetrics)`) →
   refactor to return `(FontHandle, FontMetrics)`
4. `render_create_picture` (~3 sites) → returns `PictureHandle`
5. `render_create_glyphset` (~2 sites) → returns `GlyphSetHandle`
6. `render_create_solid_fill` (~1 site) → returns `PictureHandle`
7. `render_create_linear_gradient` (~1 site) → returns `PictureHandle`
8. `render_create_radial_gradient` (~1 site) → returns `PictureHandle`
9. `render_create_cursor` (~1 site) → returns `CursorHandle`

Build between batches.

### Task 2.5 — Rename `allocate_xid` to `next_xid` (private)

After Tasks 2.2–2.4, `host.allocate_xid()` calls in `nested.rs`
should be zero. Confirm:

```sh
grep -nE "host\.allocate_xid|\bh\.allocate_xid" crates/yserver-core/src/
```

Must return zero non-test hits (or only hits inside `host_x11.rs`'s
own internal helpers that became private). If non-zero in
`nested.rs` or `resources.rs`, those are call sites missed in 2.3
or 2.4 — fix them.

Then in `host_x11.rs`, rename the public method to `next_xid` and
make it `pub(super)` (or `pub(crate)` if needed for tests):

```rust
pub(super) fn next_xid(&mut self) -> u32 { /* unchanged body */ }
```

### Task 2.6 — Run the full check suite + manual smoke

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo +nightly fmt --check
```

All green. Then **manual smoke gate**:

```sh
just ynest &
sleep 1
DISPLAY=:99 xterm     # type "ls", quit
```

Acceptance: xterm renders, accepts input, exits cleanly.

### Task 2.7 — Commit

```sh
git add crates/yserver-core/src/host_x11.rs \
        crates/yserver-core/src/nested.rs \
        crates/yserver-core/src/resources.rs \
        crates/yserver-core/src/server.rs
git commit -m "feat: Phase 6.2 Step 2 — bundle allocate_xid into create_* methods"
```

---

## Step 3 — Expand `Gc`, define `DrawState`, refactor drawing call sites (prework #2 partial)

**Goal:** Define `DrawState` as a snapshot of GC state. **Expand `Gc`
in `resources.rs` to store the full set of GC attributes** (this is
additive behavioral scope per the "we'll need all GC fields anyway"
decision — clients send these via CreateGC/ChangeGC today and we
forward them transparently to the host without storing them
locally). Refactor `apply_gc_clip` and `apply_gc_fill_state` (in
`nested.rs:4311/4329`) to read from `&DrawState`. Refactor every
drawing call site in `nested.rs` to resolve once and pass `&DrawState`.

**Note on scope expansion:** Adding the missing GC fields to `Gc` is
**not pure refactor**. Today these GC attributes are parsed from
CreateGC/ChangeGC bytes, forwarded to the host, and discarded. After
Step 3 they're stored in our local `Gc` struct as well. Clients see
no difference in behavior (the host still does the rasterization),
but the local state grows. This expansion is intentional: the KMS
backend in Phase 6.3+ will need these fields to rasterize directly,
so storing them now de-risks that work.

**Files:**
- Create: `crates/yserver-core/src/backend/params.rs`
- Modify: `crates/yserver-core/src/backend/mod.rs` (re-exports)
- Modify: `crates/yserver-core/src/resources.rs` (`Gc` struct +
  CreateGC/ChangeGC parsing in `change_gc` / `create_gc`)
- Modify: `crates/yserver-core/src/host_x11.rs` (drawing methods take `&DrawState`)
- Modify: `crates/yserver-core/src/nested.rs` (resolve once per call site;
  `apply_gc_clip`/`apply_gc_fill_state` take `&DrawState`)
- New tests in `crates/yserver-core/src/resources.rs` test module

**Estimate:** ~5 files, ~500 LoC.

### Task 3.1 — Define `DrawState` and supporting types

Write `crates/yserver-core/src/backend/params.rs`:

```rust
//! Parameter types for the Backend trait. Snapshots of state that are
//! resolved by yserver-core once per request and passed to the backend.

use crate::backend::{FontHandle, PixmapHandle};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineStyle { Solid, OnOffDash, DoubleDash }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapStyle { NotLast, Butt, Round, Projecting }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JoinStyle { Miter, Round, Bevel }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FillStyle { Solid, Tiled, Stippled, OpaqueStippled }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FillRule { EvenOdd, Winding }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GcFunction { Clear, And, AndReverse, Copy, AndInverted, NoOp,
    Xor, Or, Nor, Equiv, Invert, OrReverse, CopyInverted, OrInverted, Nand, Set }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubwindowMode { ClipByChildren, IncludeInferiors }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArcMode { Chord, PieSlice }

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipState {
    None,
    Rectangles { origin: (i16, i16), rects: Vec<crate::Rect> },
    Pixmap { origin: (i16, i16), pixmap: PixmapHandle },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FillState {
    Solid,
    Tiled { pixmap: PixmapHandle, origin: (i16, i16) },
    Stippled { pixmap: PixmapHandle, origin: (i16, i16) },
    OpaqueStippled { pixmap: PixmapHandle, origin: (i16, i16) },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BgState {
    Pixel(u32),
    Pixmap(PixmapHandle),
    None,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrawState {
    pub foreground: u32,
    pub background: u32,
    pub line_width: u16,
    pub line_style: LineStyle,
    pub cap_style: CapStyle,
    pub join_style: JoinStyle,
    pub fill_style: FillStyle,
    pub fill_rule: FillRule,
    pub function: GcFunction,
    pub plane_mask: u32,
    pub font: Option<FontHandle>,
    pub clip: ClipState,
    pub fill: FillState,
    pub subwindow_mode: SubwindowMode,
    pub graphics_exposures: bool,
    pub dashes: Vec<u8>,
    pub dash_offset: i16,
    pub arc_mode: ArcMode,
}

impl Default for DrawState {
    fn default() -> Self {
        Self {
            foreground: 0,
            background: 0xff_ff_ff,
            line_width: 0,
            line_style: LineStyle::Solid,
            cap_style: CapStyle::Butt,
            join_style: JoinStyle::Miter,
            fill_style: FillStyle::Solid,
            fill_rule: FillRule::EvenOdd,
            function: GcFunction::Copy,
            plane_mask: u32::MAX,
            font: None,
            clip: ClipState::None,
            fill: FillState::Solid,
            subwindow_mode: SubwindowMode::ClipByChildren,
            graphics_exposures: true,
            dashes: vec![4, 4],
            dash_offset: 0,
            arc_mode: ArcMode::PieSlice,
        }
    }
}
```

Add `pub mod params;` and re-exports in `backend/mod.rs`. Build to
verify compilation.

### Task 3.2 — Expand `Gc` in resources.rs

Add the missing fields to `Gc`:

```rust
pub struct Gc {
    // (existing fields unchanged: id, drawable, foreground, background,
    //  line_width, font, clip_rectangles, clip_pixmap, clip_x_origin,
    //  clip_y_origin, fill_style, tile, stipple, tile_x_origin,
    //  tile_y_origin, owner)

    // ADDED IN PHASE 6.2 (additive scope):
    pub line_style: LineStyle,
    pub cap_style: CapStyle,
    pub join_style: JoinStyle,
    pub fill_rule: FillRule,
    pub function: GcFunction,
    pub plane_mask: u32,
    pub subwindow_mode: SubwindowMode,
    pub graphics_exposures: bool,
    pub dashes: Vec<u8>,
    pub dash_offset: i16,
    pub arc_mode: ArcMode,
}
```

Note: `fill_style` is already `u8`. Either keep it `u8` and convert
when resolving, or migrate it to the enum here too. Prefer migrating
for consistency.

In `Gc::default` (or wherever Gc gets its CreateGC defaults), give
each new field its X11-spec default value (matches `DrawState::default`
above).

### Task 3.3 — Update CreateGC / ChangeGC parsing

Find the request parsers (likely in `nested.rs` or
`resources.rs::change_gc`). Today they parse a subset of attributes
from the value-mask + value-list bytes. Extend to parse the full
attribute list. **Mask bits per X11 protocol spec, in
value-list order** (each value is 4 bytes on the wire, regardless
of natural type):

| Mask bit | Attribute | Wire type |
|---|---|---|
| 0x00000001 | Function | CARD8 (in 4-byte slot) → enum |
| 0x00000002 | PlaneMask | CARD32 |
| 0x00000004 | Foreground | CARD32 |
| 0x00000008 | Background | CARD32 |
| 0x00000010 | LineWidth | CARD16 (in 4-byte slot) |
| 0x00000020 | LineStyle | CARD8 → enum |
| 0x00000040 | CapStyle | CARD8 → enum |
| 0x00000080 | JoinStyle | CARD8 → enum |
| 0x00000100 | FillStyle | CARD8 → enum |
| 0x00000200 | FillRule | CARD8 → enum |
| 0x00000400 | Tile | PIXMAP |
| 0x00000800 | Stipple | PIXMAP |
| 0x00001000 | TileStippleX | INT16 |
| 0x00002000 | TileStippleY | INT16 |
| 0x00004000 | Font | FONT |
| 0x00008000 | SubwindowMode | CARD8 → enum |
| 0x00010000 | GraphicsExposures | BOOL |
| 0x00020000 | ClipX | INT16 |
| 0x00040000 | ClipY | INT16 |
| 0x00080000 | ClipMask | PIXMAP or None |
| 0x00100000 | DashOffset | CARD16 |
| 0x00200000 | Dashes | CARD8 (single padded byte; full dash list comes from `SetDashes`, opcode 58) |
| 0x00400000 | ArcMode | CARD8 → enum |

(Cross-reference with [X11 protocol spec section 7](https://www.x.org/releases/X11R7.7/doc/xproto/x11protocol.html)
for the authoritative table. The values above match the spec; if a
test fails, re-check against the spec.)

**Important — current state of GC forwarding to host:**

Today's `host_x11.rs` does not transparently forward all GC fields.
It maintains a single shared host GC per depth and applies a subset
of fields (foreground, font, clip rectangles/pixmap, fill style,
tile/stipple, tile origins) before each draw. Other fields
(line_style, cap_style, join_style, fill_rule, function, plane_mask,
subwindow_mode, graphics_exposures, dashes, dash_offset, arc_mode)
are NOT applied — drawing happens with the host GC's defaults.

Phase 6.2 changes this: the new fields stored on `Gc` are also
forwarded to the host's shared GC at draw time (extend
`apply_gc_clip` / `apply_gc_fill_state` and add an `apply_draw_state`
peer that pushes the additional fields). This is **a behavioral
improvement**, not a no-op refactor: previously, clients sending
e.g. `function=Xor` for an XOR drag rectangle were silently
overridden to `Copy`. Step 3 honors the requested function correctly.

If any rendering surprises emerge in Step 6 manual smoke (e.g. a WM
or app suddenly rendering differently because it relied on the
silent override behavior), revisit per-field whether the new
forwarding is too aggressive — but the expected outcome is "more
things work correctly," not regressions.

### Task 3.4 — Update `CopyGC` and `SetDashes` handlers

Two more GC-mutation paths beyond CreateGC/ChangeGC need updating
once `Gc` grows new fields:

**`CopyGC` (opcode 57).** Handler at `nested.rs:7230`.
`ResourceTable::copy_gc` today copies a subset of fields based on
the value-mask. Extend it to copy each new field when the
corresponding mask bit is set:

```rust
pub fn copy_gc(&mut self, src_id: ResourceId, dst_id: ResourceId, mask: u32) {
    // Existing logic for foreground / background / line_width / etc.
    // Add for each new field — mask bit and field copy.
    if mask & 0x00000001 != 0 { dst.function = src.function; }
    if mask & 0x00000002 != 0 { dst.plane_mask = src.plane_mask; }
    if mask & 0x00000020 != 0 { dst.line_style = src.line_style; }
    if mask & 0x00000040 != 0 { dst.cap_style = src.cap_style; }
    if mask & 0x00000080 != 0 { dst.join_style = src.join_style; }
    if mask & 0x00000200 != 0 { dst.fill_rule = src.fill_rule; }
    if mask & 0x00008000 != 0 { dst.subwindow_mode = src.subwindow_mode; }
    if mask & 0x00010000 != 0 { dst.graphics_exposures = src.graphics_exposures; }
    if mask & 0x00100000 != 0 { dst.dash_offset = src.dash_offset; }
    if mask & 0x00200000 != 0 {
        // Spec: dashes copied as a unit alongside dash_offset.
        dst.dashes = src.dashes.clone();
    }
    if mask & 0x00400000 != 0 { dst.arc_mode = src.arc_mode; }
}
```

Verify the existing handler's mask shape matches; adapt if the field
layout is different. Without this, `CopyGC` produces a destination
GC with stale-default values for the new fields, breaking any client
that copies an explicit-attribute GC.

**`SetDashes` (opcode 58).** Currently marked unimplemented in
`docs/status.md`. Phase 6.2 doesn't have to implement it, but the
single-byte `Dashes` field in CreateGC/ChangeGC (mask bit 0x00200000)
*is* parsed as part of Task 3.3 — the spec says CreateGC/ChangeGC's
Dashes value is "the dash length specifier." Treat this as setting
both `dashes = vec![n, n]` (the `Dashes` value) and `dash_offset = 0`
when the mask bit is set, matching X11 protocol semantics.
`SetDashes` (opcode 58) for the full per-segment dash list remains
unimplemented as today; this plan's expansion does not change that.

Add a unit test for `copy_gc` covering each new mask bit
(~5 tests, one per new field group).

### Task 3.5 — Add `ResourceTable::resolve_draw_state`

In `resources.rs`. **Important on fallback semantics:** today's code
silently degrades to unclipped/solid fill when a tile/stipple/clip
pixmap can't be resolved (`resources.rs:1188-1193`,
`resources.rs:1239-1242`). `resolve_draw_state` MUST preserve this
behavior. Returning `None` for a "tile pixmap missing host backing"
case would make every drawing call fail with `BadGC`-equivalent —
a real regression. `None` is reserved for "unknown gc_id" only.

```rust
impl ResourceTable {
    pub fn resolve_draw_state(&self, gc_id: ResourceId) -> Option<DrawState> {
        let gc = self.gcs.get(&gc_id.0)?;  // None ONLY for unknown GC

        // Clip resolution: degrade gracefully if host backing is missing.
        let clip = if let Some(rects) = &gc.clip_rectangles {
            ClipState::Rectangles {
                origin: (gc.clip_x_origin, gc.clip_y_origin),
                rects: rects.clone(),
            }
        } else if let Some(clip_pixmap_id) = gc.clip_pixmap {
            // Lookup may fail if the clip pixmap was freed; fall through
            // to unclipped, matching today's behavior.
            match self.pixmaps.get(&clip_pixmap_id.0).and_then(|p| p.host_xid) {
                Some(pixmap) => ClipState::Pixmap {
                    origin: (gc.clip_x_origin, gc.clip_y_origin),
                    pixmap,
                },
                None => ClipState::None,
            }
        } else {
            ClipState::None
        };

        // Fill resolution: degrade to Solid if the requested fill's
        // backing pixmap can't resolve.
        let fill = match gc.fill_style {
            FillStyle::Solid => FillState::Solid,
            FillStyle::Tiled => {
                gc.tile
                    .and_then(|t| self.pixmaps.get(&t.0))
                    .and_then(|p| p.host_xid)
                    .map(|pixmap| FillState::Tiled {
                        pixmap,
                        origin: (gc.tile_x_origin, gc.tile_y_origin),
                    })
                    .unwrap_or(FillState::Solid)
            }
            FillStyle::Stippled => {
                gc.stipple
                    .and_then(|s| self.pixmaps.get(&s.0))
                    .and_then(|p| p.host_xid)
                    .map(|pixmap| FillState::Stippled {
                        pixmap,
                        origin: (gc.tile_x_origin, gc.tile_y_origin),
                    })
                    .unwrap_or(FillState::Solid)
            }
            FillStyle::OpaqueStippled => {
                gc.stipple
                    .and_then(|s| self.pixmaps.get(&s.0))
                    .and_then(|p| p.host_xid)
                    .map(|pixmap| FillState::OpaqueStippled {
                        pixmap,
                        origin: (gc.tile_x_origin, gc.tile_y_origin),
                    })
                    .unwrap_or(FillState::Solid)
            }
        };

        // Font: same — degrade to None if not resolved.
        let font = gc.font
            .and_then(|f| self.fonts.get(&f.0))
            .map(|f| f.host_xid);

        Some(DrawState {
            foreground: gc.foreground,
            background: gc.background,
            line_width: gc.line_width,
            line_style: gc.line_style,
            cap_style: gc.cap_style,
            join_style: gc.join_style,
            fill_style: gc.fill_style,
            fill_rule: gc.fill_rule,
            function: gc.function,
            plane_mask: gc.plane_mask,
            font,
            clip,
            fill,
            subwindow_mode: gc.subwindow_mode,
            graphics_exposures: gc.graphics_exposures,
            dashes: gc.dashes.clone(),
            dash_offset: gc.dash_offset,
            arc_mode: gc.arc_mode,
        })
    }
}
```

Adjust to whatever the actual field names and getters are once Task
3.2 lands.

### Task 3.6 — Add 8 unit tests for `resolve_draw_state`

```rust
#[test]
fn resolve_draw_state_default_gc() { … }
#[test]
fn resolve_draw_state_tiled_fill_resolves_pixmap_handle() { … }
#[test]
fn resolve_draw_state_stippled_fill_resolves_pixmap_handle() { … }
#[test]
fn resolve_draw_state_clip_rectangles_with_origin() { … }
#[test]
fn resolve_draw_state_pixmap_clip_with_origin() { … }
#[test]
fn resolve_draw_state_unknown_gc_returns_none() { … }
#[test]
fn resolve_draw_state_tiled_with_freed_tile_pixmap_degrades_to_solid() { … }
#[test]
fn resolve_draw_state_clip_pixmap_freed_degrades_to_unclipped() { … }
```

`cargo test -p yserver-core resolve_draw_state -- --nocapture`. All
six pass.

### Task 3.7 — Refactor `apply_gc_clip` in nested.rs

`nested.rs:4311`:

Before:
```rust
fn apply_gc_clip(host: &mut HostX11, state: &GcClipState) -> io::Result<()> {
    match state {
        GcClipState::Rectangles(c) => host.set_clip_rectangles(Some(c.clone())),
        GcClipState::Pixmap { host_pixmap, clip_x_origin, clip_y_origin } =>
            host.set_clip_pixmap(*host_pixmap, *clip_x_origin, *clip_y_origin),
        GcClipState::None => host.clear_clip_rectangles(),
    }
}
```

After:
```rust
fn apply_clip_state(host: &mut HostX11, state: &ClipState) -> io::Result<()> {
    match state {
        ClipState::Rectangles { origin, rects } => {
            host.set_clip_rectangles(Some(ClipRectangles {
                rects: rects.clone(),
                x_origin: origin.0,
                y_origin: origin.1,
            }))
        }
        ClipState::Pixmap { origin, pixmap } => {
            host.set_clip_pixmap(pixmap.as_raw(), origin.0, origin.1)
        }
        ClipState::None => host.clear_clip_rectangles(),
    }
}
```

(rename to `apply_clip_state` to reflect the new parameter type;
update callers.)

Same shape for `apply_gc_fill_state` → `apply_fill_state` taking
`&FillState` (or expand parameter to `&DrawState` and read `.fill`
internally).

### Task 3.8 — Resolve once at every drawing call site in nested.rs

For each draw handler (`handle_poly_line`, `handle_poly_fill_rectangle`,
`handle_copy_area`, `handle_put_image`, etc.), the shape becomes:

Before:
```rust
let gc = resources.gcs.get(&gc_id).ok_or(...)?;
apply_gc_clip(host, &gc_clip_state)?;
host.poly_line(window.host_xid.unwrap().as_raw(), points)?;
```

After:
```rust
let state = resources.resolve_draw_state(gc_id).ok_or(...)?;
apply_clip_state(host, &state.clip)?;
apply_fill_state(host, &state.fill)?;
host.poly_line(window.host_xid.unwrap().as_raw(), &state, points)?;
```

This will be ~30+ call sites across `nested.rs`. Build between
batches.

Note: the actual host method signatures don't take `&DrawState` yet
(that's Task 3.9). For now, the resolve-once happens in nested.rs
and the resolved fields are passed individually if needed.

### Task 3.9 — Refactor host_x11.rs drawing methods to take `&DrawState`

For each drawing method in `host_x11.rs` (`poly_line`, `poly_segment`,
`copy_area`, `put_image`, etc.):

Before:
```rust
pub fn poly_line(&mut self, dst: u32, points: &[Point]) -> io::Result<()>;
```

After:
```rust
pub fn poly_line(&mut self, dst: AnyHandle, state: &DrawState, points: &[Point]) -> io::Result<()>;
```

The body uses `state` for clip/fill state internally (eliminating the
external `apply_clip_state` / `apply_fill_state` calls — they fold
into the host method).

This means `apply_clip_state` / `apply_fill_state` from Task 3.7
move into `host_x11.rs` as private helpers driven by the new
`&DrawState` parameter, and the explicit calls in nested.rs go away.

### Task 3.10 — Composite call sequences collapse

The 6.1 design's prework #2 named four composite operations. Three
collapse with `&DrawState`:

- **`put_image_with_clear`**: use a `DrawState` with
  `clip: ClipState::None` and call `put_image` normally. Inline
  the existing helper into call sites.
- **`fill_with_state`**: use a `DrawState` with
  `fill: FillState::Tiled{...}` and call `poly_fill_rectangle`
  normally.
- **`clear_area_with_bg`**: trait signature is
  `clear_area(win: WindowHandle, area: Rect, bg: BgState)`. Inline
  any existing helper.

Only `list_fonts_proxy` survives as a composite trait method.

### Task 3.11 — Run the full check suite + manual smoke

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo +nightly fmt --check
```

All green. Then **manual smoke gate** (one WM, drawing-heavy):

```sh
just ynest &
sleep 1
DISPLAY=:99 wmaker &
sleep 2
DISPLAY=:99 xterm -e fish &
DISPLAY=:99 xclock &
# Type into xterm, drag a window, watch xclock tick.
```

Acceptance: chrome renders, drag works, xterm input echoes, xclock
ticks. If anything is visibly broken, the most likely culprit is a
mis-resolved `DrawState` field (forgot to copy `dashes`,
`fill_style`, etc., in `resolve_draw_state`).

### Task 3.12 — Commit

```sh
git add crates/yserver-core/src/backend/ \
        crates/yserver-core/src/resources.rs \
        crates/yserver-core/src/host_x11.rs \
        crates/yserver-core/src/nested.rs
git commit -m "feat: Phase 6.2 Step 3 — DrawState by-borrow per drawing call (Gc expanded)"
```

---

## Step 4 — Module split of `host_x11.rs`

**Goal:** Convert the single 3,643-line `host_x11.rs` into a
`host_x11/` module split across `mod.rs`, `request.rs`, `pump.rs`,
`sync.rs`. Mostly file moves; some visibility work for
cross-module access.

**Files:**
- Create: `crates/yserver-core/src/host_x11/mod.rs`
- Create: `crates/yserver-core/src/host_x11/request.rs`
- Create: `crates/yserver-core/src/host_x11/pump.rs`
- Create: `crates/yserver-core/src/host_x11/sync.rs`
- Delete: `crates/yserver-core/src/host_x11.rs`

**Estimate:** Mostly file moves; some `pub(super)` / `pub(crate)`
visibility adjustments for cross-module access.

### Task 4.1 — Move `host_x11.rs` to `host_x11/mod.rs`

```sh
mkdir -p crates/yserver-core/src/host_x11
git mv crates/yserver-core/src/host_x11.rs crates/yserver-core/src/host_x11/mod.rs
cargo build -p yserver-core
```

Should succeed unchanged — module path is identical.

### Task 4.2 — Carve out `pump.rs`

Extract the `HostInputPump` struct, `HostInputPumpHandle`, the pump
thread function, the event-translation logic, and the helpers they
call. Cut from `mod.rs`, paste into `pump.rs`.

In `mod.rs`: add `pub mod pump;` and re-export
`pub use pump::{HostInputPump, HostInputPumpHandle};` (or whatever
external API today's `host_x11.rs` exposes for pump-related types).

For symbols the pump needs from `mod.rs` (e.g. `xid_map`), expose
them as `pub(super)`. The compiler will guide.

### Task 4.3 — Carve out `sync.rs`

Extract `sync_main_connection`, `reply_buffer` handling, and the
helpers they call. Cut, paste, re-export.

### Task 4.4 — Carve out `request.rs`

The bulk of `mod.rs` after Tasks 4.2 + 4.3 is request-side methods.
Cut these into `request.rs` as `impl HostX11 { ... }` blocks. `mod
request;` is enough — Rust automatically picks up `impl` blocks
across files within the same module.

### Task 4.5 — Inspect `mod.rs`

After Task 4.4, `mod.rs` should hold:
- The `HostX11` struct definition
- The constructor (`HostX11::open_from_env` or similar)
- Module declarations and re-exports
- Common helpers if needed

If still over ~500 lines, more carving is possible — but Phase 6.2
doesn't require optimal split, just *some* split.

### Task 4.6 — Run the full check suite

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo +nightly fmt --check
```

All green. No manual smoke needed for file moves — the unit tests +
build are sufficient.

### Task 4.7 — Commit

```sh
git add crates/yserver-core/src/host_x11/
git commit -m "refactor: Phase 6.2 Step 4 — split host_x11.rs into host_x11/{mod,request,pump,sync}.rs"
```

---

## Step 5 — Carve the `Backend` trait

**Goal:** Define the `Backend` trait (~70 methods, surface from Step
0's audit), rename `HostX11` → `HostX11Backend`, `impl Backend for
HostX11Backend`. Switch `nested.rs` and `server.rs` request handlers
to `Arc<Mutex<dyn Backend>>` for the hot path (drawing, resource ops,
extension proxies). Hold a *separate* concrete
`Arc<Mutex<HostX11Backend>>` clone for two specific call paths:
pump construction in `nested.rs:292-328` and per-client kb pump
construction in `nested.rs:604`. No downcast helper — both clones
are constructed at startup from the same `Arc::new(Mutex::new(...))`
and stored in two fields of the top-level wiring struct. Add
`RecordingBackend` test double + 2–4 `nested.rs` integration tests.

**Files:**
- Modify: `crates/yserver-core/src/backend/mod.rs` (add trait + sink + event types)
- Create: `crates/yserver-core/src/backend/sink.rs` (`BackendEventSink` impl wrapping the existing fanout)
- Create: `crates/yserver-core/src/backend/recording.rs` (test double, `#[cfg(test)]`)
- Modify: `crates/yserver-core/src/host_x11/mod.rs` (rename struct, impl trait)
- Modify: `crates/yserver-core/src/nested.rs` and `server.rs`
  (most call sites switch to `dyn Backend`; pump-construction sites stay concrete)
- New tests in `crates/yserver-core/src/backend/recording.rs` or a
  sibling test module.

**Estimate:** ~6 files, ~600 LoC + ~500 LoC for `RecordingBackend`
(realistic against ~110 trait methods).

### Task 5.1 — Define the `Backend` trait

In `crates/yserver-core/src/backend/mod.rs`, add the trait
definition. **Use Step 0's audit document as the source of truth**:
every `host.X` and `h.X` call enumerated there becomes either a
trait method or stays HostX11-specific.

Trait method count breakdown from the audit:
- 12 lifecycle/resource
- 17 drawing (taking `&DrawState`)
- 10 state accessors
- 19 RENDER (`render_*`)
- 3 other extensions (xkb_proxy, xfixes_change_cursor_by_name,
  set_shape_rectangles)
- 8 misc
- 1 inline-locked (`query_pointer`)

Total: ~70. Plus `register_event_sink`, `sync`. Final trait surface
is ~72 methods.

Trait shape sketch (the full version is in the design doc):

```rust
pub trait Backend: Send {
    // Lifecycle
    fn register_event_sink(&mut self, sink: Arc<dyn BackendEventSink + Send + Sync>);
    fn sync(&mut self) -> io::Result<()>;
    fn ping(&mut self) -> io::Result<()>;

    // State accessors
    fn window_id(&self) -> WindowHandle;
    fn root_visual_xid(&self) -> VisualHandle;
    fn argb_visual_xid(&self) -> Option<VisualHandle>;
    fn argb_colormap_xid(&self) -> Option<ColormapHandle>;
    fn render_opcode(&self) -> Option<u8>;
    fn xkb_opcode(&self) -> Option<u8>;
    fn composite_opcode(&self) -> Option<u8>;
    fn render_format_for_ynest_id(&self, ynest_id: u32) -> Option<u32>;
    fn xkb_info(&self) -> Option<&XkbInfo>;

    // Resource creation (atomic-bundled per Step 2)
    fn create_subwindow(&mut self, parent: WindowHandle, params: CreateSubwindowParams) -> io::Result<WindowHandle>;
    fn create_pixmap(&mut self, depth: u8, w: u16, h: u16, drawable: AnyHandle) -> io::Result<PixmapHandle>;
    fn create_cursor(&mut self, params: CreateCursorParams) -> io::Result<CursorHandle>;
    fn open_font(&mut self, name: &str) -> io::Result<(FontHandle, FontMetrics)>;

    // Resource destruction
    fn destroy_subwindow(&mut self, h: WindowHandle) -> io::Result<()>;
    fn free_pixmap(&mut self, h: PixmapHandle) -> io::Result<()>;
    fn close_font(&mut self, h: FontHandle) -> io::Result<()>;
    // (cursors are freed via render_free_cursor / similar)

    // Window ops
    fn map_subwindow(&mut self, h: WindowHandle) -> io::Result<()>;
    fn unmap_subwindow(&mut self, h: WindowHandle) -> io::Result<()>;
    fn configure_subwindow(&mut self, h: WindowHandle, params: ConfigureParams) -> io::Result<()>;
    fn reparent_subwindow(&mut self, child: WindowHandle, parent: WindowHandle, x: i16, y: i16) -> io::Result<()>;
    fn change_subwindow_attributes(&mut self, h: WindowHandle, params: ChangeAttrsParams) -> io::Result<()>;
    fn name_window_pixmap(&mut self, win: WindowHandle, pixmap: PixmapHandle) -> io::Result<()>;
    fn define_cursor(&mut self, win: WindowHandle, cursor: CursorHandle) -> io::Result<()>;

    // Drawing — all take &DrawState
    fn put_image(&mut self, dst: AnyHandle, state: &DrawState, img: ImageData) -> io::Result<()>;
    fn get_image(&mut self, src: AnyHandle, params: GetImageParams) -> io::Result<GetImageReply>;
    fn copy_area(&mut self, src: AnyHandle, dst: AnyHandle, state: &DrawState, params: CopyAreaParams) -> io::Result<()>;
    fn copy_plane(&mut self, src: AnyHandle, dst: AnyHandle, state: &DrawState, params: CopyPlaneParams) -> io::Result<()>;
    fn poly_line(&mut self, dst: AnyHandle, state: &DrawState, points: &[Point]) -> io::Result<()>;
    fn poly_segment(&mut self, dst: AnyHandle, state: &DrawState, segs: &[Segment]) -> io::Result<()>;
    fn poly_rectangle(&mut self, dst: AnyHandle, state: &DrawState, rects: &[Rect]) -> io::Result<()>;
    fn poly_arc(&mut self, dst: AnyHandle, state: &DrawState, arcs: &[Arc_]) -> io::Result<()>;
    fn poly_fill_rectangle(&mut self, dst: AnyHandle, state: &DrawState, rects: &[Rect]) -> io::Result<()>;
    fn poly_fill_arc(&mut self, dst: AnyHandle, state: &DrawState, arcs: &[Arc_]) -> io::Result<()>;
    fn fill_poly(&mut self, dst: AnyHandle, state: &DrawState, params: FillPolyParams) -> io::Result<()>;
    fn fill_rectangle(&mut self, dst: AnyHandle, state: &DrawState, rect: Rect) -> io::Result<()>;
    fn poly_text8(&mut self, dst: AnyHandle, state: &DrawState, params: PolyTextParams<u8>) -> io::Result<()>;
    fn poly_text16(&mut self, dst: AnyHandle, state: &DrawState, params: PolyTextParams<u16>) -> io::Result<()>;
    fn image_text8(&mut self, dst: AnyHandle, state: &DrawState, params: ImageTextParams<u8>) -> io::Result<()>;
    fn image_text16(&mut self, dst: AnyHandle, state: &DrawState, params: ImageTextParams<u16>) -> io::Result<()>;
    fn poly_point(&mut self, dst: AnyHandle, state: &DrawState, points: &[Point]) -> io::Result<()>;
    fn clear_area(&mut self, win: WindowHandle, area: Rect, bg: BgState) -> io::Result<()>;

    // Misc
    fn warp_pointer(&mut self, params: WarpPointerParams) -> io::Result<()>;
    fn query_pointer(&mut self, win: WindowHandle) -> io::Result<QueryPointerReply>;
    fn list_fonts_proxy(&mut self, pattern: &str, max: u16) -> io::Result<Vec<Vec<u8>>>;
    fn list_fonts_with_info_proxy(&mut self, pattern: &str, max: u16, sink: &mut dyn FontInfoSink) -> io::Result<()>;
    fn get_atom_name(&mut self, atom: u32) -> io::Result<Option<String>>;
    fn get_keyboard_mapping(&mut self, params: KbMappingParams) -> io::Result<KbMappingReply>;
    fn get_modifier_mapping(&mut self) -> io::Result<ModifierMappingReply>;
    fn set_container_background_pixel(&mut self, pixel: u32) -> io::Result<()>;
    fn set_container_background_pixmap(&mut self, pixmap: PixmapHandle) -> io::Result<()>;

    // RENDER — 19 methods as enumerated in the audit
    fn render_query_version(&mut self) -> io::Result<RenderVersionReply>;
    fn render_create_picture(&mut self, params: RenderCreatePictureParams) -> io::Result<PictureHandle>;
    fn render_change_picture(&mut self, h: PictureHandle, params: RenderChangePictureParams) -> io::Result<()>;
    fn render_free_picture(&mut self, h: PictureHandle) -> io::Result<()>;
    fn render_create_glyphset(&mut self, format: u32) -> io::Result<GlyphSetHandle>;
    fn render_free_glyphset(&mut self, h: GlyphSetHandle) -> io::Result<()>;
    fn render_add_glyphs(&mut self, h: GlyphSetHandle, params: AddGlyphsParams) -> io::Result<()>;
    fn render_free_glyphs(&mut self, h: GlyphSetHandle, params: FreeGlyphsParams) -> io::Result<()>;
    fn render_composite(&mut self, params: RenderCompositeParams) -> io::Result<()>;
    fn render_composite_glyphs(&mut self, params: RenderCompositeGlyphsParams) -> io::Result<()>;
    fn render_fill_rectangles(&mut self, params: RenderFillRectsParams) -> io::Result<()>;
    fn render_trapezoids(&mut self, params: RenderTrapezoidsParams) -> io::Result<()>;
    fn render_create_solid_fill(&mut self, params: RenderSolidFillParams) -> io::Result<PictureHandle>;
    fn render_create_linear_gradient(&mut self, params: RenderLinearGradientParams) -> io::Result<PictureHandle>;
    fn render_create_radial_gradient(&mut self, params: RenderRadialGradientParams) -> io::Result<PictureHandle>;
    fn render_create_cursor(&mut self, params: RenderCreateCursorParams) -> io::Result<CursorHandle>;
    fn render_set_picture_clip_rectangles(&mut self, h: PictureHandle, params: SetClipRectsParams) -> io::Result<()>;
    fn render_set_picture_filter(&mut self, h: PictureHandle, params: SetPictureFilterParams) -> io::Result<()>;
    fn render_set_picture_transform(&mut self, h: PictureHandle, transform: PictureTransform) -> io::Result<()>;

    // XKB / XFIXES / SHAPE
    fn xkb_proxy(&mut self, opcode: u8, body: &[u8], expects_reply: bool) -> io::Result<Option<Vec<u8>>>;
    fn xfixes_change_cursor_by_name(&mut self, cursor: CursorHandle, name: &str) -> io::Result<()>;
    fn set_shape_rectangles(&mut self, win: WindowHandle, params: ShapeRectsParams) -> io::Result<()>;
}
```

Plus the supporting types (`BackendEventSink`, `BackendEvent`,
`BackendError`, `BackendFatalError`) per the design doc.

### Task 5.2 — Plan the rename of `HostX11` → `HostX11Backend`

```sh
grep -rn "HostX11\b" crates/
```

Hits should be inside `yserver-core` and `yserver`'s binaries. If
anything else references it, plan the update.

### Task 5.3 — Rename the struct

```sh
grep -rln "HostX11\b" crates/ | xargs sed -i 's/HostX11\b/HostX11Backend/g'
```

Verify with `git diff` that the regex didn't catch anything
unintended (e.g. comments saying "HostX11"). Build to confirm.

### Task 5.4 — Wire startup to hold both concrete and dyn references

Find the top-level startup site (likely in `bin/ynest.rs` or
`lib.rs`'s `run()` for the ynest binary). Today it constructs a
`HostX11` and stores it as `Arc<Mutex<HostX11>>`. After this step:

```rust
// Construct once.
let backend_concrete: Arc<Mutex<HostX11Backend>> = Arc::new(Mutex::new(
    HostX11Backend::open_from_env()?
));

// Pump construction takes the concrete clone.
let pump = HostInputPump::open_from_env(
    backend_concrete.lock().unwrap().window_id().as_raw()
);
let _pump_thread = std::thread::spawn(move || run_main_pump(backend_concrete.clone(), pump));

// Per-client handler dispatch takes the dyn clone.
let backend_dyn: Arc<Mutex<dyn Backend>> = backend_concrete.clone();
serve_clients(backend_dyn);
```

The two `Arc`s share the same underlying `Mutex<HostX11Backend>`.
The `dyn` clone is a coercion of the concrete one — Rust supports
this directly via unsized coercion on `Arc<T>` → `Arc<dyn Trait>`
when `T: Trait`. No specialization, no downcast, no extra trait.

The per-client kb pump factory at `nested.rs:604` is similar: the
`handle_client` function takes `concrete: Arc<Mutex<HostX11Backend>>`
as a parameter (passed through from `serve_clients`); inside it
acquires the lock once to get `host_window_id`, then constructs
`HostInputPump::open_from_env(host_window_id)`. Same shape as today.

### Task 5.5 — Write `impl Backend for HostX11Backend`

In `host_x11/mod.rs` (or a dedicated `host_x11/trait_impl.rs` to
keep `mod.rs` lean), write the impl. Each method delegates to the
existing `HostX11Backend` method (which after Step 2 already returns
the right handle types).

Build. Most failures are inside the impl bodies where parameter
types don't quite match — adapt.

### Task 5.6 — Wire `register_event_sink` into the main HostInputPump

In `host_x11/pump.rs`, add a path for the main pump thread to
invoke `sink.deliver_event(ev)` when it has translated a host event
into a `BackendEvent`.

The pump receives an `Arc<dyn BackendEventSink + Send + Sync>` via
a new field on `HostX11Backend`. The sink is `Send + Sync`, so
calling into it doesn't require any other lock.

**Per-client kb pumps stay UNTOUCHED in this step.** The kb pump
factory at `nested.rs:604` keeps calling
`HostInputPump::open_from_env(host_window_id)` directly. That site
gets the `host_window_id` via `concrete.lock().unwrap().window_id().as_raw()`
where `concrete: &Arc<Mutex<HostX11Backend>>` is the concrete clone
held alongside the dyn clone. (Since `window_id()` is on the trait
too, code that already has `dyn Backend` access can also call it
through the trait — both work; the concrete-clone path is just for
sites that need to spawn pump threads.)

### Task 5.7 — Write the sink impl in yserver-core

`crates/yserver-core/src/backend/sink.rs`:

```rust
//! BackendEventSink impl that routes BackendEvent into the existing
//! per-client fanout machinery.

use std::sync::{Arc, Mutex};
use crate::backend::{BackendEvent, BackendEventSink, BackendFatalError};

pub struct CoreEventSink {
    state: Arc<Mutex<crate::server::ServerState>>,
}

impl CoreEventSink {
    pub fn new(state: Arc<Mutex<crate::server::ServerState>>) -> Self {
        Self { state }
    }
}

impl BackendEventSink for CoreEventSink {
    fn deliver_event(&self, ev: BackendEvent) {
        let mut state = match self.state.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        match ev {
            BackendEvent::Expose { window, x, y, w, h, count } =>
                state.expose_fanout(window, x, y, w, h, count),
            BackendEvent::ButtonPress { .. } => state.pointer_fanout(ev),
            BackendEvent::ButtonRelease { .. } => state.pointer_fanout(ev),
            BackendEvent::MotionNotify { .. } => state.pointer_fanout(ev),
            // … one match arm per BackendEvent variant
        }
    }

    fn deliver_fatal(&self, err: BackendFatalError) {
        log::error!("backend transport closed: {err:?}");
        // Signal main loop to terminate ynest. The exact mechanism
        // depends on what termination path nested.rs has today.
    }
}
```

Adapt to whatever today's event-fanout entry points look like. The
existing `pointer_event_fanout`, `expose_event_fanout`, etc.
functions in `nested.rs` either stay as standalone functions
called from the sink, or become `impl ServerState` methods.

### Task 5.8 — Switch hot-path call sites to `Arc<Mutex<dyn Backend>>`

In `nested.rs` and `server.rs`, find sites that hold
`Arc<Mutex<HostX11Backend>>` (post-Step-1 type). Switch most to
`Arc<Mutex<dyn Backend>>`. Sites that stay concrete:

- `nested.rs:292-328` (main `HostInputPump` construction).
  Continues to take `Arc<Mutex<HostX11Backend>>` because the pump
  thread needs concrete-type access.
- `nested.rs:604` (per-client kb pump factory). Same reason.

For these two sites, the construction code at the entry point of
ynest creates a single `HostX11Backend` first, holds a concrete
`Arc<Mutex<HostX11Backend>>`, hands it out for pump construction,
then `clone`s it as `Arc<Mutex<dyn Backend>>` for the
request-handler dispatch.

This is a bit awkward but explicit. The migration of pump
construction to also use `dyn Backend` is in the merge slice.

### Task 5.9 — Write `RecordingBackend` test double

`crates/yserver-core/src/backend/recording.rs`:

```rust
//! Test double for the Backend trait. Records every method call;
//! returns synthetic handles. Used to drive nested.rs request handlers
//! against without needing a real X server.

#![cfg(test)]

use super::*;
use std::sync::Mutex;

pub struct RecordingBackend {
    pub calls: Mutex<Vec<RecordedCall>>,
    next_handle: Mutex<u32>,
    fake_window_id: WindowHandle,
    fake_root_visual: VisualHandle,
    fake_argb_visual: Option<VisualHandle>,
    fake_argb_colormap: Option<ColormapHandle>,
}

#[derive(Debug, Clone)]
pub enum RecordedCall {
    CreateSubwindow { parent: WindowHandle },
    DestroySubwindow(WindowHandle),
    MapSubwindow(WindowHandle),
    UnmapSubwindow(WindowHandle),
    ConfigureSubwindow(WindowHandle),
    ChangeSubwindowAttributes(WindowHandle),
    CreatePixmap { depth: u8, w: u16, h: u16, drawable: AnyHandle },
    FreePixmap(PixmapHandle),
    PutImage { dst: AnyHandle },
    CopyArea { src: AnyHandle, dst: AnyHandle },
    PolyLine { dst: AnyHandle, n_points: usize },
    PolyFillRectangle { dst: AnyHandle, n_rects: usize },
    OpenFont(String),
    CloseFont(FontHandle),
    InternAtom { name: String, only_if_exists: bool },
    GetAtomName(u32),
    Sync,
    Ping,
    // … one variant per trait method that nested.rs tests exercise.
    //   Doesn't need to be exhaustive across all 70 trait methods —
    //   only the ones the tests actually drive.
}

impl RecordingBackend {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            next_handle: Mutex::new(0x10_000),
            fake_window_id: WindowHandle::from_raw_for_test(0x100),
            fake_root_visual: VisualHandle::from_raw_for_test(0x21),
            fake_argb_visual: Some(VisualHandle::from_raw_for_test(0x22)),
            fake_argb_colormap: Some(ColormapHandle::from_raw_for_test(0x23)),
        }
    }

    fn allocate_handle(&self) -> u32 {
        let mut n = self.next_handle.lock().unwrap();
        let h = *n;
        *n = n.wrapping_add(1);
        h
    }
}

impl Backend for RecordingBackend {
    fn register_event_sink(&mut self, _: Arc<dyn BackendEventSink + Send + Sync>) {}
    fn sync(&mut self) -> io::Result<()> {
        self.calls.lock().unwrap().push(RecordedCall::Sync);
        Ok(())
    }
    fn ping(&mut self) -> io::Result<()> {
        self.calls.lock().unwrap().push(RecordedCall::Ping);
        Ok(())
    }
    fn window_id(&self) -> WindowHandle { self.fake_window_id }
    fn root_visual_xid(&self) -> VisualHandle { self.fake_root_visual }
    fn argb_visual_xid(&self) -> Option<VisualHandle> { self.fake_argb_visual }
    fn argb_colormap_xid(&self) -> Option<ColormapHandle> { self.fake_argb_colormap }
    fn render_opcode(&self) -> Option<u8> { Some(139) }
    fn xkb_opcode(&self) -> Option<u8> { Some(136) }
    fn composite_opcode(&self) -> Option<u8> { Some(142) }
    fn render_format_for_ynest_id(&self, _: u32) -> Option<u32> { None }

    fn create_subwindow(&mut self, parent: WindowHandle, _: CreateSubwindowParams) -> io::Result<WindowHandle> {
        self.calls.lock().unwrap().push(RecordedCall::CreateSubwindow { parent });
        Ok(WindowHandle::from_raw_for_test(self.allocate_handle()))
    }

    fn map_subwindow(&mut self, h: WindowHandle) -> io::Result<()> {
        self.calls.lock().unwrap().push(RecordedCall::MapSubwindow(h));
        Ok(())
    }

    // … one impl per trait method; ~70 methods total. Most are 1–4
    // lines. Total: ~400–600 LoC.

    // Methods the tests don't exercise can return Ok(()) or a
    // synthetic handle without recording, but for completeness
    // every method should at minimum log.
}
```

This is realistically 400–600 LoC for the full impl. The earlier plan
estimate of "~150 LoC" was off; codex caught this in v1 review.

### Task 5.10 — Write 2–4 `RecordingBackend` integration tests

In a new test module (e.g. `crates/yserver-core/src/nested_tests.rs`
or inside `nested.rs`'s `#[cfg(test)] mod tests`):

```rust
#[test]
fn create_map_change_destroy_window_calls_backend_correctly() {
    let backend: Arc<Mutex<dyn Backend>> = Arc::new(Mutex::new(RecordingBackend::new()));
    let mut server = ServerState::new_for_test(backend.clone());

    // Drive a CreateWindow request through the handler:
    let client_id = server.register_test_client();
    server.handle_request(client_id, encode_create_window(...)).unwrap();
    server.handle_request(client_id, encode_map_window(...)).unwrap();
    server.handle_request(client_id, encode_change_property(...)).unwrap();
    server.handle_request(client_id, encode_destroy_window(...)).unwrap();

    let recording = backend.lock().unwrap();
    let recording = recording.as_any().downcast_ref::<RecordingBackend>().unwrap();
    let calls = recording.calls.lock().unwrap();
    assert!(matches!(calls[0], RecordedCall::CreateSubwindow { .. }));
    assert!(matches!(calls[1], RecordedCall::MapSubwindow(_)));
    // ChangeProperty doesn't go through the backend; assert it doesn't appear.
    assert!(matches!(calls[2], RecordedCall::DestroySubwindow(_)));
}
```

(The downcast pattern needs `Backend: Any` — add a `as_any` method
to the trait if needed for tests, or wrap the `Arc<Mutex<>>`
differently.)

Aim for 2–4 tests covering: (a) basic create+map+destroy flow;
(b) drawing operation routes through `&DrawState`; (c) sync is
called when expected; (d) handle types match across map/destroy.

### Task 5.11 — Run the full check suite + manual smoke

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo +nightly fmt --check
```

All green. Then **manual smoke gate**:

```sh
just ynest &
sleep 1
DISPLAY=:99 xterm    # rendering + input
DISPLAY=:99 wmaker & # WM startup
sleep 2
DISPLAY=:99 xterm    # second xterm under wmaker; type
```

Both xterms render and accept input; wmaker chrome appears on the
second one.

### Task 5.12 — Commit

```sh
git add crates/yserver-core/src/backend/ \
        crates/yserver-core/src/host_x11/ \
        crates/yserver-core/src/nested.rs \
        crates/yserver-core/src/server.rs
git commit -m "feat: Phase 6.2 Step 5 — carve Backend trait, HostX11Backend impl, RecordingBackend tests"
```

---

## Step 6 — Manual validation pass

**Goal:** Run the full Phase 3.x WM matrix + gtk3-demo end-to-end.
Fix any regressions. Update `docs/status.md`. Squash-merge to master.

**Files:**
- Modify: `docs/status.md` (add Phase 6.2 section under Phase 6)
- Optional: any bugfix commits surfaced by the validation matrix

### Task 6.1 — Validate wmaker

```sh
just ynest &
sleep 1
DISPLAY=:99 wmaker &
sleep 2
DISPLAY=:99 xterm &
DISPLAY=:99 xclock &
DISPLAY=:99 xeyes &
```

Acceptance:
- Chrome + clip + dock + appicons render.
- xterm/xclock open with correct icon graphics in appicons.
- Close button visible on title bar.
- Drag a window and restack — both work.

### Task 6.2 — Validate fvwm3

```sh
DISPLAY=:99 fvwm3 &
sleep 2
DISPLAY=:99 xclock &
DISPLAY=:99 gtk3-demo &
```

Acceptance:
- Chrome renders.
- Widget clicks activate (Phase 3.7 fix).
- xclock title bar text via RENDER.
- gtk3-demo sidebar nav works.

### Task 6.3 — Validate e16

```sh
DISPLAY=:99 enlightenment-16 &
sleep 3
# Right-click on desktop area
```

Acceptance:
- Top bar + pagers render.
- Right-click popup opens.
- Popup body has theme tile (not solid black).
- Menu-item click on "Settings" opens the Enlightenment Settings dialog.

### Task 6.4 — Validate openbox

```sh
DISPLAY=:99 openbox &
sleep 2
DISPLAY=:99 xeyes &
DISPLAY=:99 xclock &
```

Acceptance:
- Clients render inside openbox frames.
- (Openbox frame chrome itself is a known pre-existing gap; not a
  regression target.)

### Task 6.5 — Validate gtk3-demo

If not already covered in fvwm3 pass:

```sh
DISPLAY=:99 fvwm3 &
sleep 2
DISPLAY=:99 gtk3-demo &
```

Acceptance:
- Main window + sidebar nav + child dialogs work.
- Sidebar labels rendered.
- Click "Run" on a demo and it opens.

### Task 6.6 — Triage and fix regressions

If any WM or gtk3-demo regressed against the Phase 3.x baseline:

1. Capture the symptom (screenshot if rendering, log lines if logical).
2. Bisect against the Step 0–5 commits:
   ```sh
   git bisect start
   git bisect bad HEAD
   git bisect good <commit-before-step-1>
   ```
3. Fix in the offending step's commit if pre-merge, or as a follow-up
   commit on the branch if post-merge debugging is more tractable.

The three most likely culprits per the design's risk table:
- Step 1 (handle newtypes) — a missed `.as_raw()` call where a host
  XID is keyed into a `HashMap<u32, ...>`.
- Step 3 (`DrawState`) — a missed field in `resolve_draw_state` that
  was previously read directly from `Gc`. Or a CreateGC/ChangeGC
  parser that didn't store the new attribute.
- Step 5 (trait carve) — an `impl Backend` method that delegates to
  the wrong existing `HostX11Backend` method.

### Task 6.7 — Update `docs/status.md`

Add a new section under "Phase 6 — Standalone DRM/KMS":

```markdown
### Phase 6.2 — Backend trait extraction (complete)

Goal: carve a `Backend` trait out of `yserver-core` so request
handlers call into it (via `Arc<Mutex<dyn Backend>>` for the hot
path) and a future KMS backend slots in. Lands three of the five
C-prework items from the 6.1 design; per-client kb pump and main
HostInputPump construction stay against concrete `HostX11Backend`
via a separate concrete-typed `Arc<Mutex<HostX11Backend>>` clone.
Pump/main connection merge
is deferred to its own slice.

Design:
[`2026-05-03-phase6-2-backend-trait-design.md`](superpowers/specs/2026-05-03-phase6-2-backend-trait-design.md).
Plan:
[`2026-05-03-phase6-2-backend-trait.md`](superpowers/plans/2026-05-03-phase6-2-backend-trait.md).
Pre-step audit:
[`2026-05-03-phase6-2-host-surface-audit.md`](superpowers/notes/2026-05-03-phase6-2-host-surface-audit.md).

#### Landed (branch `phase6-2-backend-trait`, squash-merged to master)

- [x] **Step 0 — Host-X11 surface audit.** Concrete enumeration of
      every `host.X` / `h.X` call in nested.rs and server.rs, plus
      every host_xid-bearing field in resources.rs. Source of truth
      for the trait surface.
- [x] **Step 1 — Per-kind handle newtypes.** 8 newtypes
      (`WindowHandle`, `PixmapHandle`, `PictureHandle`,
      `GlyphSetHandle`, `FontHandle`, `CursorHandle`,
      `ColormapHandle`, `VisualHandle`) plus `AnyHandle` for
      drawables. Replaces 17 host_xid slots (7 required, 9 optional,
      1 map key) across `Window`, `Pixmap`, `Visual`, `Colormap`,
      `Font`, `Cursor`, `PictureState`, `GlyphSetState`,
      `NamedCompositePixmap`, `HostDrawableTarget`, plus
      `GcFillState::Tiled` and `ReparentResult`.
- [x] **Step 2 — Bundle `allocate_xid` into `create_*`.** `host.create_subwindow(...) -> io::Result<WindowHandle>` and peers replace the two-phase pattern.
- [x] **Step 3 — `Gc` expansion + `DrawState` resolution.** `Gc`
      gains `line_style`, `cap_style`, `join_style`, `fill_rule`,
      `function`, `plane_mask`, `subwindow_mode`,
      `graphics_exposures`, `dashes`, `dash_offset`, `arc_mode`
      (additive scope; clients send these and were previously
      dropped client-side). `ResourceTable::resolve_draw_state`
      computes a `DrawState` snapshot. Drawing methods take
      `&DrawState`. Three of four composite operations from the 6.1
      reading list collapse.
- [x] **Step 4 — Module split.** `host_x11.rs` → `host_x11/{mod, request, pump, sync}.rs`.
- [x] **Step 5 — `Backend` trait carve.** ~70 trait methods (see
      audit document for full surface). `HostX11Backend` is the sole
      impl. `register_event_sink` routes the main HostInputPump's
      events. Pump-construction sites hold a separate concrete-typed
      `Arc<Mutex<HostX11Backend>>` clone alongside the dyn one (no
      downcast helper). `RecordingBackend` test double + 2–4 integration tests.
- [x] **Step 6 — Validation.** Manual smoke under wmaker, fvwm3,
      e16, openbox + gtk3-demo. All Phase 3.x acceptance criteria
      met.

#### Phase 6.2 follow-ups (deferred, all moved to the merge slice)

The pump/main connection merge — Phase 3.7's structural fix and
prework item #5 — is its own future slice (Phase 6.2.5 or fold into
Phase 6.3 design). It carries:

- Single-X11-connection merge.
- `fd()` / `dispatch()` / `drain_events()` on the trait.
- Per-client kb pump dissolution.
- Migration of pump construction sites to also use `dyn Backend`
  (eliminating the separate concrete-typed clone for pump construction).
- 64-bit `seq_full` tracking for X11 16-bit sequence wrap.
- Retention window for late void-request errors.
- `OriginContext` plumbing for async host-error attribution.
- Reply demux / `ReplyMap` rework.
```

### Task 6.8 — Final commit on the branch + squash-merge

```sh
git add docs/status.md
git commit -m "docs: status.md — Phase 6.2 Backend trait extraction landed"
```

Then squash-merge to master:

```sh
git checkout master
git merge --squash phase6-2-backend-trait
git commit -m "feat: Phase 6.2 — Backend trait extraction"
```

Push when satisfied. Per the project memory, pushing in the bwrap
sandbox needs:

```sh
GIT_SSH_COMMAND="ssh -F /home/jos/realhome/Projects/dotfiles/ssh/config -o UserKnownHostsFile=/home/jos/realhome/.ssh/known_hosts" git push
```

---

## Open follow-ups (out of scope; tracked for later)

- **Pump/main connection merge.** Whole own slice. Brings async
  host-error attribution, sequence-wrap handling, per-client kb pump
  dissolution, and the `fd()/dispatch()/drain_events()` reshape of
  the trait. Eliminates the separate concrete-typed pump-construction clone.
- **Per-client GC mirroring** (Phase 3.7 follow-up `#940`). Trait
  shape is forward-compatible — a future per-client-GC backend
  caches resolved-state-per-`GcResourceId` internally without
  changing the trait surface.
- **KMS backend.** Phase 6.3+. The `RecordingBackend` test double
  established in Step 5 is the first existence proof that the trait
  is implementable by something other than `HostX11Backend`; KMS is
  the second.
- **Cross-trait extension drawing methods.** Several RENDER methods
  take GC state today and were folded into the `&DrawState` rule
  during Step 3. If any drawing-style RENDER methods were missed,
  they'll surface as drawing regressions in Step 6 manual smoke.

## Codex review log

Plan v1 was reviewed by codex (gpt-5.5, 2026-05-03). Findings:

1. Step 1 host-XID inventory was inaccurate — wrong field names
   (e.g. `host_picture_xid` not `host_xid`), wrong optionality (some
   fields are required, not optional), and several missed entirely
   (`Visual.host_visual_xid`, `Window.background_pixmap_host_xid`,
   `Window.border_pixmap_host_xid`, `HostDrawableTarget`, etc.). v2
   adds Step 0 audit and rewrites Step 1's inventory based on the
   audit.
2. Step 3's `DrawState` was over-specified — current `Gc` only stores
   a subset of the fields. v2 explicitly calls out the `Gc`
   expansion as additive behavioral scope and adds a sub-task to
   extend CreateGC/ChangeGC parsing.
3. `GcFillState::Stippled` and `OpaqueStippled` don't exist in
   today's enum. v2 keeps them in `FillState` (the trait-side type)
   but maps Stippled/OpaqueStippled `fill_style` values from `Gc`
   to those `FillState` variants in `resolve_draw_state`, with
   pixmap resolution from `gc.stipple`.
4. `apply_gc_clip` lives in `nested.rs:4311`, not `host_x11.rs`.
   v2's Step 3 correctly places the refactor in `nested.rs`.
5. Step 2 named `create_window`; the actual method is
   `create_subwindow`. v2 corrects throughout.
6. Step 5's "copy method signatures from the design verbatim" was
   insufficient. v2 adds Step 0 audit + explicit ~70-method trait
   surface.
7. Pump architecture under `dyn Backend` was hand-waved. v2 keeps
   `Arc<Mutex<HostX11Backend>>` for two specific pump-construction
   sites; uses a separate concrete-typed `Arc<Mutex<HostX11Backend>>`
   clone for pump construction (no downcast helper); documents as
   intentional escape hatch.
8. Per-client kb pump construction needs the host_window_id. v2
   resolves: `window_id()` is a trait method; the kb pump's
   `open_from_env(host_window_id)` uses
   `backend.lock().window_id()`.
9. `RecordingBackend` size estimate was too low. v2 corrects to
   400–600 LoC.
10. Step 4 "pure file moves" understated the visibility work. v2
    softens the claim.
11. Step 1 needed a smoke gate. v2 adds Task 1.7.

v2 was then reviewed by codex (third pass). Findings folded into v3
inline (no further codex review):

A. **Audit grep too narrow.** v2's grep missed `hh.copy_plane(...)`
   at `nested.rs:7441`, inline `host.lock().ok()?.query_pointer(...)`
   at `nested.rs:6890`, and `xkb_info()` at `nested.rs:393`. v3
   broadens the grep patterns to include `hh.X` and inline-locked
   shapes; trait surface adds `copy_plane`, `query_pointer`,
   `xkb_info`. Audit document inventory updated to drawing(17),
   state-accessors(10), plus a 1-method "other host calls" category.
B. **`as_host_x11()` downcast helper didn't compile and was
   unnecessary.** v2's specialization-based shape required unstable
   Rust. v3 deletes the helper entirely and uses unsized coercion
   on `Arc<HostX11Backend>` → `Arc<dyn Backend>` directly: startup
   constructs one `Arc<Mutex<HostX11Backend>>`, holds it for pump
   construction, `clone`s it as `Arc<Mutex<dyn Backend>>` for
   request dispatch.
C. **GC mask values were wrong.** v2's table had several shifted
   masks. v3 has the correct values per X11 spec; clarifies that
   `Dashes` in CreateGC/ChangeGC is one padded `CARD8` (the dash
   length specifier), not `LISTofCARD8`; the full per-segment dash
   list comes from `SetDashes` (opcode 58, currently
   unimplemented).
D. **`CopyGC` and `SetDashes` weren't covered.** Real bug: if `Gc`
   grows fields, `copy_gc` (called from CopyGC handler at
   `nested.rs:7230`) must copy them. v3 adds Task 3.4 explicitly
   covering all new fields' copy logic plus the
   CreateGC/ChangeGC `Dashes` semantics.
E. **`resolve_draw_state` regressed fallback semantics.** v2's
   sketch returned `None` when tile/stipple pixmaps couldn't
   resolve; today's code degrades to unclipped/solid. v3's
   `resolve_draw_state` returns `None` only for unknown GC; missing
   host backings degrade to `ClipState::None` /
   `FillState::Solid`, preserving today's behavior.
F. **GC forwarding misstatement.** v2 said missing GC fields are
   "transparently forwarded to the host." Actually, current code
   only applies a subset of GC fields to the host's shared GC; the
   rest take host defaults. v3 corrects: Phase 6.2 stores AND
   forwards the new fields, which is a behavioral improvement
   (line_style / function / plane_mask / arc_mode now honored).
G. **Internal count inconsistencies.** "~110 methods" appeared
   alongside "~70"; "17 RENDER" alongside 19 enumerated. v3 picks
   ~72 trait methods total (10 lifecycle, 17 drawing, 10 state, 19
   RENDER, 3 other-extension, 8 misc, 1 inline-locked,
   `register_event_sink`, `sync`) and uses it consistently.
H. **Audit document should stay in tree.** v2 said the audit commit
   was squashed away; v3 clarifies the squash applies to the
   commit history but the file stays under `docs/superpowers/notes/`.

After v3 revisions, the plan is implementation-ready. The Step 0
audit is the real safety net — any drift between this plan and the
actual codebase at execution time will surface there before any
code is written.
