# Status archive — Stage 4 close (2026-05-20 / 2026-05-21)

Snapshot of the diagnosis and fix-chain narrative that landed Stage 4
closure on `cow-authoritative-mode`. Stage 4 itself is closed; this
file preserves the per-iteration trace material that drove the fix
chain so future debugging sessions can pick up the context.

The summary lives in `status.md`. Detailed sub-stage history for
4a–4d.7 (which stays in tree as a working record) also lives in
`status.md` under the now-`[x]` Stage 4 sections.

---

## "Where we are" — pre-close running diagnosis (2026-05-20 / 2026-05-21)

This was the live "Where we are" preamble in `status.md` up to the
moment Stage 4 closed. It captures the COW/redirect rendering
failures the cow-authoritative branch went after and the per-fix
narrative as each pin landed.

Hardware MATE still showed COW/redirect rendering failures during
the early cow-authoritative work: `nm-applet` may not appear,
windows can disappear while moving, and the MATE pager still tracks
the moving window even when the real window is invisible.
Telemetry from both XFCE and MATE ruled out the current extracted
perf fixes as the cause: no missed page flips, no KMS `EBUSY` loop,
no hot `vkQueueWaitIdle`, and input/page-flip cadence stayed
healthy during the repro. The failure mode was a compositor-update
bug, not CPU overhead or present starvation.

Focused trace work narrowed the failure much further than the
earlier COW hypotheses: `SetPictureClipRegion` for the fullscreen
compositor pixmaps carried only a tiny panel strip, and the
matching `render_composite` calls for Caja's frame/content
therefore saw either that strip or an empty final clip. The
companion `damage_fanout` trace showed why: drawing into the
fullscreen compositor pixmaps never matched any DAMAGE subscribers,
so Marco's DAMAGE `Subtract` / update-region path only saw panel
damage and never the actual window-content damage. That explained
the symptom where windows started hidden or disappeared during
drag, then reappeared when panel hover triggered some unrelated
repaint. The first fix in that direction was in the core Present
Copy path: after `PresentPixmap` copies into the destination
window and waits for the drawable to go idle, yserver now feeds
DAMAGE on that destination window using the request's `update`
region when present, or the copied pixmap bounds otherwise.
Regression test:
`present_pixmap_update_region_emits_damage_on_destination_window`.
Separately, the older silent `normalize_region_rects` 4096-rect
cap was removed because it could truncate heavily fragmented
regions down to upper bands only; that was a real bug surfaced by
the perf run, but it was not sufficient to fix the MATE
hidden-window repro by itself. Hardware cursor plane updates can
still visually jam if enabled; the default path uses the confirmed
good software cursor, while `YSERVER_V2_HW_CURSOR=1` remains
opt-in for focused cursor-plane debugging.

The next damage/XFixes trace moved the bug boundary again. The
region algebra itself was behaving consistently in the bad frame;
by the time Marco starts its `CopyRegion` / `SubtractRegion` /
`IntersectRegion` chain, the accumulated update region is already
wrong. The earlier `NameWindowPixmap` drawable-identity fix was
real, but it was not sufficient for the startup-hidden/
dialog-hidden repro. The next traces showed a tighter ordering
bug: on the failing frame yserver reaches `SetPictureClipRegion(
... n=0)` and `render_composite ... final_clip=<empty>` for the
dialog/window before the next `damage_notify_queue` for that same
redirected frame window lands. Even more importantly, some
redirected/viewable windows first become interesting to marco only
after yserver already emitted the initial full-window
configure/map damage, so that seed is lost (`configure_damage_emit`
with `match_ids=0`, followed later by the first `match_ids=1`).
That left the compositor with no initial window region, and later
damage could arrive one compositor cycle too late; the window then
stayed hidden until an unrelated repaint (panel hover, etc.)
drove another pass. Xorg is broader here than the previous fix:
`DamageExtRegister()` immediately reports the current `borderClip`
for every window-backed `XDamageCreate`, not just redirected
windows. yserver now mirrors that behavior at the same granularity
it already uses for immediate seeds: any already-viewable
`XDamageCreate(window)` now seeds initial damage right away.
Regression: `damage_create_on_viewable_window_seeds_full_damage`.

The next bad run: startup-hidden dialogs were still reproducible,
but the failure boundary was now concrete. yserver was synthesizing
full-window `DamageNotify` on every redirected `ConfigureWindow`,
including stack-only restacks (`CWSibling` / `CWStackMode`) of
marco's fullscreen desktop window `0x01300005`. That injected
bogus full-screen damage (`damage=0xe0020d`), marco unioned it
into the compositor update region, and the next subtract cleared
the whole region before the dialog composites ran, leaving
`SetPictureClipRegion(... n=0)` and `render_composite ...
final_clip=<empty>` for the dialog pass. The current tree now
limits synthetic ConfigureWindow damage to actual geometry changes
(`x/y/width/height/border`) and explicitly skips pure restacks.
Regression:
`configure_window_stack_only_on_redirected_window_does_not_emit_damage`.

The next dump/trace comparison found another concrete Xorg
mismatch in the DAMAGE extension itself. yserver's
`DAMAGE::Subtract` correctly computed `parts = old ∩ repair` and
`damage = old - repair`, but then only cleared
`pending_notify_fired=false` and waited for unrelated future
drawing before notifying again. Xorg's `ProcDamageSubtract` does
more: when `repair != None` and some damage remains, it
immediately re-reports the remaining region for the coalesced
report levels (Delta/BoundingBox/NonEmpty). Without that follow-up
notify, marco can drain one chunk of damage, see no new wake for
the leftover chunk, and temporarily build an empty compositor
update region until some later panel hover or repaint restarts
the cycle. The current tree now mirrors Xorg here: after
`DAMAGE::Subtract`, remaining coalesced damage is immediately
re-notified via `report_existing_damage_to_state(...)`.
Regression:
`damage_subtract_with_remaining_nonempty_damage_rereports_immediately`.

The next bad MATE run found a more basic Present/Xorg mismatch in
the Copy path. Xorg's `present_copy_region()` does **not**
interpret `x_off/y_off` as a source origin. It performs
`CopyArea(src=(0,0) -> dst=(x_off,y_off), size=pixmap_wh)` and,
when an `update` region is provided, installs that region as a
destination clip translated by `(x_off, y_off)`. yserver had the
opposite mapping in its immediate Copy fallback:
`copy_area(src=(x_off,y_off) -> dst=(0,0), size=pixmap_wh)`, and
it ignored the `update` region for the actual copy. That was a
concrete protocol-behavior bug and matched the compositor symptoms
much better than the earlier heuristic fixes. The current tree
mirrors Xorg's shape: with an `update` region, yserver issues one
backend copy per update rect (`src=rect`, `dst=rect + x_off/y_off`);
without an update region it copies the full pixmap to destination
offset `(x_off, y_off)`. Regressions:
`present_pixmap_copy_uses_update_region_rects_as_copy_clips` and
`present_pixmap_copy_without_update_uses_dst_offset_not_src_offset`.

The next startup-hidden dump pointed at the DAMAGE
`Subtract(parts=...)` materialization itself. Marco's fullscreen
compositor pass copies the returned `parts` region into its
accumulated update region (`0x00e00255 -> 0x00e002b0` in the
trace), and in the bad frame that region was already a malformed
set of panel-edge bands before any window-specific
subtract/intersect ran. The cause: yserver stores DAMAGE as an
append-only raw rect list in `damage_fanout`, but
`DAMAGE::Subtract` was handing that raw list straight back to the
client when `repair == None`. Xorg's internal damage state is a
real canonical region, so clients see a normalized parts region
there rather than duplicate overlapping rectangles. The current
tree canonicalizes the stored damage rects before computing
`parts` / `new_damage` in `DAMAGE::Subtract`. Regressions:
`damage_subtract_with_no_repair_returns_old_damage_in_parts_and_clears`
and `damage_subtract_with_no_repair_canonicalizes_parts_region`.

The newest mixed startup run (panels hidden, dialog visible)
pointed at coalesced DAMAGE notify timing across geometry changes.
Xorg keeps notifying on coalesced damage when the drawable
geometry changes mid-cycle; yserver was only looking at the
boolean `pending_notify_fired`, so if a panel first reported
damage at `(0,-28)` and then moved to `(0,0)` before
`DAMAGE::Subtract`, the second full-window damage append was
silently suppressed as "already notified". That left marco
draining damage against stale geometry and stale update clips
until some unrelated repaint woke a new cycle. The current tree
tracks `DamageObject.last_reported_geometry` and re-emits
coalesced damage whenever the drawable geometry changes, even if
the object is still mid-cycle. Regression:
`geometry_change_rereports_damage_mid_cycle`.

The new startup-hidden-dialog run found the remaining gap in the
map/viewability path. The dialog window's `XDamageCreate(window)`
happened while the child was still `Unviewable`, so the
create-time seed correctly did nothing; later, when the WM
frame/ancestor map promoted that child to `Viewable`, yserver
emitted Expose down the subtree but only seeded DAMAGE for the
mapped parent itself, not for descendants that transitioned
`Unviewable -> Viewable`. That left marco without the dialog's
first visible pixels until some later configure/motion repaint
landed. The current tree mirrors the map-time damage bump across
the newly-viewable subtree when handling `MapWindow`. Regression:
`map_window_seeds_damage_for_newly_viewable_descendant`.

Startup-hidden was now fixed but drag-hide remained. The bad drag
dump showed the keyring dialog backing was fully correct while
the COW/scanout omitted it, which meant marco was compositing
with the wrong stack model in that pass rather than missing the
dialog paint itself. yserver was sending `ConfigureNotify` with
`above-sibling = None` unconditionally:
`encode_configure_notify_event()` hardcoded zero, so every move/
restack update told clients "no sibling above me" regardless of
the actual root child order. That was a concrete WM-facing
protocol bug and a plausible explanation for marco treating the
fullscreen desktop window as if it sat above the dragged dialog
during motion. Follow-up: the first implementation got the
direction wrong. Xorg's `ConfigureNotify.aboveSibling` uses the
sibling the window is immediately **above** in stack order (the
lower neighbor, Xorg's `pSib` / `nextSib`), not the sibling above
the window. yserver now mirrors that exact direction from the
parent child list and threads it through every emitted
`ConfigureNotify`. Regressions:
`encode_configure_notify_event_writes_above_sibling` and
`configure_notify_above_sibling_tracks_restacked_order`.

The first hardware-NVIDIA MATE run with brisk-menu (Ubuntu MATE's
standalone applications popup, replacing the classic in-panel
menu used on the other test box) found a spec-conformance gap in
`MapWindow`. Brisk-menu pounds `XMapWindow`/`XMapRaised` on its
already-viewable override-redirect popup at ~40–60 Hz (GTK3's
`gtk_window_present` loop, amplified by yserver's stub
`XIGrabDevice`). `handle_map_window` was discarding the
`was_unmapped` return from `state.resources.map_window` and
unconditionally re-emitting MapNotify → Expose → full-extent
damage on every call. Marco then issued a fresh
`COMPOSITE::NameWindowPixmap` and full recomposite of the popup
~50×/s, visible as menu flicker on the COW-authoritative KMS
scene. Xorg `dix/window.c:2661` early-returns `Success` on
`pWin->mapped` before any redirect / MapNotify / exposure work;
yserver now does the same. Regression:
`map_window_on_already_mapped_window_is_no_op`.

Same hardware-NVIDIA MATE run, cursor-shape regression on
resize-frame interior. Pointer entering a marco resize-edge
correctly swapped the cursor to the right resize shape, and
leaving the top-level frame entirely restored the default; but
moving from the edge back into the frame interior left the
resize cursor sticky. Marco implements this by issuing dynamic
`XDefineCursor(frame, resize_xid)` / `XDefineCursor(frame, None)`
pairs on motion, where the second call (`cursor = None = xid 0`)
must clear the per-window cursor so the effective cursor walks
up to the parent / `core.active_cursor` fallback. yserver's CWA
handler in `handle_change_window_attributes` short-circuited that
reset path: `state.resources.cursor_host_xid(ResourceId(0))`
returns `None` (no cursor resource with xid 0), and the
`if let (Some(hw), Some(ch))` guard around the
`backend.define_cursor` call dropped the request silently. Both
v1 and v2 backends already treat `cursor_host_xid == 0` as the X11
None case (clear per-window cursor + refresh effective cursor) —
the bug was only in the protocol-layer routing. Match Xorg
`dix/window.c:1487-1491`, which sets `pCursor = (CursorPtr) None`
when `cursorID == None`. Regression:
`cwa_cursor_none_propagates_define_cursor_zero_to_backend`.

Same NVIDIA hardware, XFCE this time: the CWA-cursor-None fix
made marco's frame work in MATE but XFCE still showed a stuck
arrow cursor on xfwm4 frame edges. xfwm4 attaches resize-edge
cursors to thin frame sub-windows (one child per edge under each
frame top-level) — not to the frame top-level itself, in contrast
to marco, which dynamically swaps the cursor on the frame
top-level via `XDefineCursor`. Pre-fix, v2's `window_under_cursor`
only iterated `core.top_level_order` and returned at the topmost
top-level containing the cursor. `prev_pointer_window` therefore
stayed pinned to the frame top-level, the cursor chain walk
picked up only the frame's (`None`) cursor + the root fallback,
and the xfwm4 resize sprites never became effective. Sub-window
descent now matches Xorg `dix/events.c`'s `XYToWindow`: after
locking the topmost mapped top-level, walk children sorted by
`stack_rank` back-to-front and descend into the topmost mapped
child whose parent-relative box contains the cursor; SHAPE-input
(or bounding) trims hittable region at every level; the depth
bound matches the cursor walk's 64. Regression:
`window_under_cursor_descends_into_subwindow_tree`. Side effect:
button-press routing also descends now, so xfwm4's resize-edge
sub-windows receive their `ButtonPress` events when the user
starts a resize (was previously delivered to the frame top-level,
which has no resize behaviour bound to it).

Text widgets in GTK4 apps (`gnome-text-editor`, modern GTK apps)
kept showing the default arrow over text areas under both XFCE
and MATE. Capture: the client issued 567 `XIChangeCursor` (XI2
opcode 42) requests on hover, zero core `XDefineCursor` calls.
yserver's XI2 dispatch treated minor 42 as a logging no-op
(`debug!` + `Ok(Handled)`), so the I-beam never reached the
backend. Modern GTK4 sets per-widget cursors through XInput2
rather than core X11 (GTK3 still uses both, depending on widget
age). Handler now parses `window(4) + cursor(4) + deviceid(2) +
pad(2)` and routes to `backend.define_cursor` — same shape as
the CWA-cursor path, including the `cursor = None` (xid 0) clear
case. Per-device cursor routing isn't implemented yet; this
treats every `XIChangeCursor` as if `deviceid` were
`AllMasterDevices`, which is what GTK relies on in practice.
Regression: `xi_change_cursor_propagates_define_cursor_to_backend`.

Cleanup pass on the stub-handler audit from the same MATE/XFCE
smoke runs. (1) `XFIXES::SetCursorName` (minor 23, 47 hits total)
was falling through the XFIXES dispatch's "unknown minor" warning
branch — but the request is real: Xcursor uses it to tag themed
cursors with a name string so a later `XFixesGetCursorName` can
read it back. yserver didn't implement the GetCursorName side
yet, but the per-request warning was misleading — added a
recognised no-op handler that accepts and ignores. (2)
`RANDR::Set{Screen,Crtc}Config` was returning `status=Success(0)`
unconditionally with a "stub" log line; now validates the
requested CRTC mode against `state.randr.modes` and returns
`status=Failed(3)` when the mode is unknown (mode=0 = disable
CRTC still accepts). Log message clarified — no longer claims
"stub" since the no-op acceptance is intentional for yserver's
fixed-KMS single-mode setup (the alternative — `BadValue` —
makes `mate-settings-daemon`'s "restore last session" path noisy
at every login). Regression:
`randr_set_crtc_config_validates_mode_id`.

Promoted `XFIXES::SetCursorName` from no-op accept to a full
round-trip with `XFIXES::GetCursorName`. `Cursor` gained a
`name_atom: Option<AtomId>` field; `SetCursorName` parses the
name bytes and interns via `state.atoms.intern(name,
only_if_exists=false)` (mirroring Xorg's `MakeAtom(tchar, nbytes,
TRUE)`), then stores the atom on the cursor. `GetCursorName`
(minor 24, previously unhandled → "unknown minor" warning) reads
the atom back and replies with the spec-shaped 32-byte header +
name + 4-byte-padded payload. Unnamed cursors report atom=0 (X11
None) and an empty name, matching `ProcXFixesGetCursorName`'s
`pCursor->name == 0` branch. Cross-checked against
`xfixes/cursor.c` in the Xorg checkout at
`/home/jos/realhome/Projects/xserver`. Regression:
`xfixes_cursor_name_round_trip`.

Closed the last stub-handler bucket from the audit: the XI2 grab
opcodes (51-55) had been no-op `Success`-replying stubs. GTK /
Qt thought they owned the device but pointer events kept going
to the normal hit-tested window, so popups dismissed on the
first stray motion event and `gtk_window_present` re-mapped the
popup at ~50 Hz (the same root cause as brisk-menu's MapWindow
remap storm). yserver already had working core X11 grab state
(`state.pointer_grab` / `active_pointer_grab` / `button_grabs` /
`active_keyboard_grab` / `key_grabs`) and a matching
`active_grab_target` redirect in the pointer/key fanout. The XI2
handlers now wire into that same shared state, matching Xorg
`Xi/xigrabdev.c::ProcXIGrabDevice` which unpacks the XI2 fields
and calls the shared `GrabDevice()` helper. Mapping:
`deviceid == 3` (master keyboard) populates
`active_keyboard_grab`; any other deviceid populates the
pointer-grab tuple. `XIPassiveGrabDevice` with
`grab_type=Button(0)` / `Keycode(1)` pushes into `button_grabs`
/ `key_grabs` (one entry per modifier; `num_modifiers=0` becomes
a single entry with mask 0; XI2 `Any` (bit 31) maps to core X11
`AnyModifier` 0x8000). `XIPassiveUngrabDevice` removes matching
entries. Enter/FocusIn/TouchBegin passive grabs are logged +
skipped — yserver has no matching machinery yet. Regressions:
`xi_grab_device_sets_active_grab_state` and
`xi_passive_grab_device_pushes_button_and_key_grabs`.

Wiring grabs surfaced three more layers that GTK3 popup menus
rely on, each cross-checked against `xfce-xorg.xtrace` (lines
140188+ for marco title-bar core grabs, lines 218731+ for
xfce4-panel main-menu core grabs, lines 41986+ for pluma XI2
popups). All three are now in tree.

(1) Synthesised grab-activation crossings. Xorg emits
`EnterNotify`/`LeaveNotify` (`FocusIn`/`FocusOut` for keyboard
grabs) with `mode=NotifyGrab, detail=NotifyNonlinear` when a grab
activates or transitions between grab windows. Without these,
GTK3's popup state machine never engages — the menu is visible
and the grab is held, but hover/click tracking stays dormant and
items don't highlight or activate. yserver now emits the matching
pair on both `XIGrabDevice` (XI2 minor 51, see
`xi_grab_device_emits_grab_activation_crossings`) and core
`GrabPointer`/`GrabKeyboard`, including the cross-window
Leave-on-previous + Enter-on-new pair when GTK3 re-grabs from its
initial input-shadow window onto the visible popup. The matching
`NotifyUngrab` pair fires on `XIUngrabDevice` / `UngrabPointer` /
`UngrabKeyboard`.

(2) Natural Enter/Leave under an active grab. Pre-fix the pointer
fanout's active-grab redirect unconditionally set
`handled_core_via_grab = true` for ALL pointer events, including
`EnterNotify`/`LeaveNotify`, then short-circuited the normal
core-propagation path. Natural crossings as the pointer moved
between windows were dropped entirely while a grab was active —
so GTK3 never received the "pointer entered me" cue needed to
transition menu state from "menu open, no item active" to
"tracking hover". Crossings now skip the redirect and fall
through to normal propagation, matching Xorg
`dix/events.c::DeliverGrabbedEvent` which only re-routes events
explicitly listed in the grab's event mask.

(3) `owner_events=true` semantics. The active-grab redirect was
forcibly delivering motion + button events to `grab_window`
regardless of the grab's `owner_events` flag — pure
`owner_events=false` behaviour. Per spec (and Xorg
`xfce-xorg.xtrace:219000+`), when `owner_events=true` AND the
natural hit-test window is owned by the grab client, events
should be reported on the natural deepest window (so GTK3 menus
see motion events on the panel button until the pointer actually
crosses into the popup). Captured in
`ActivePointerGrab.owner_events` (read from `XIGrabDevice` body
byte 16 / core `GrabPointer` header data byte) and now consulted
by the redirect: only redirect to `grab_window` if
`owner_events=false` OR the natural target is owned by a
different client. Verified end-to-end: pluma right-click popup,
gnome-text-editor right-click popup, marco title-bar right-click
menu, and xfce4-panel whisker-menu all now highlight items on
hover and activate on click under MATE+marco / XFCE+xfwm4.

---

## Stage 4d post-smoke retrospective (2026-05-17 through 2026-05-19)

The pre-`cow-authoritative-mode` retrospective on the Stage 4d
hardware smoke. Most of these items were closed by the
cow-authoritative branch or by the fix chain narrated above; the
record stays here so the negative-results history isn't lost.

### Stage 4d hardware-smoke state (2026-05-17)

**mate-no-comp**: ~95% parity with v1. Desktop icons, panel-left,
panel-right (clock + system tray icons), bottom panel "Control
Center" task entry, full Control Center window, tooltips visible.
Missing vs v1: yellow group-header labels ("Filter", "Groups",
"Common Tasks") in Control Center sidebar — likely a
colored-source-with-glyph-mask Render::Composite that doesn't
fully work yet.

**mate-with-compositing**: BROKEN. Marco issues
`RedirectSubwindows(root, Manual)`. Top-levels removed from scene
per spec §I4. Marco's compositor partially populates COW —
mate-panel's own paint shows (panel-left glyphs, bottom-panel
task list, top-right 2 small tray icons), nm-applet popup appears
with artefacts, but most of the scene (control center, hover
menus, clock-applet, bottom panel center) shows only marco's
shadow over wallpaper. Indicates marco's COW has alpha=0 in most
areas + COW is alpha_passthrough=true (correct for the COW layer)
so root wallpaper bleeds through.

**xfce-with-compositing** (default xfwm4 compositor): WORSE.
xfwm4 also does `RedirectSubwindows(root, Manual)`. Almost
entire screen dark gray (root storage default, xfdesktop is a
redirected top-level + xfdesktop's wallpaper paint goes to B but
B is never composited visibly), only 2 tray icons + nm-applet
popup visible.

**mate-no-comp v1 baseline**: full visual parity with real X11.

**mate-with-comp v1 baseline (2026-05-17 19:12)**: looks
**identical to v1 no-comp**. v1's `Manual`-redirect path
effectively no-ops — windows stay in v1's per-window-mirror
scanout walk, marco's compositor reads return nothing useful from
NameWindowPixmap, marco's COW PresentPixmap lands but doesn't
visibly affect output. v1 silently ignores marco's "you take over
compositing" intent. **No tooltips have shadows, no transparency
effects, no real compositor visual on v1.** Apps render via the
bypass, not the compositor.

**2026-05-19 follow-up**: the scene-boundary fix is back in place
so MATE is usable again, but drop shadows are still missing. The
current yserver-side fix stores the original drawable origin on
`CreatePicture` and uses it to translate root/window-space clip
regions into picture-local coordinates before Render scissoring.
XFIXES region helpers also mirror Xorg for `CreateRegionFromWindow`
honoring its `kind` byte, `CreateRegionFromGC` /
`CreateRegionFromPicture` by copying the client clip, and
`InvertRegion` now computes `bounds - source` instead of
discarding the source operand. Direct RENDER
`SetPictureClipRectangles` now also canonicalizes overlapping
bands before storing them; the live trace showed repeated
identical / overlapping bands surviving in the picture-clip
lists, which is our bug, not a DE quirk.

**2026-05-19 follow-up 2**: the Render destination-clip idea
turned out to be the wrong layer and has been backed out. The
Xorg trace still matters, but the `subwindow-mode` metadata there
is pointing us toward source-validation / source-clip handling
for window-backed pictures, not toward clipping the Render
destination itself. Keep this as a live mismatch, not a solved
branch.

2026-05-19 follow-up after the latest mate smoke: the log now
shows `clear_window_area_with_background` hitting depth-32
visible windows with `bg_pixmap=None` and `bg_pixel=0x00000000`,
and the fallback clear was decoding that as transparent black.
That is the current active alpha bug; the fallback clear path
now bypasses the generic fill path and issues a direct opaque
fill for server-owned background clears.

**2026-05-19 PM** (yoga / Snapdragon X1 / Turnip): mate-hw smoke
after the cc10689 + 6464531 + 6ffd370 + 8c5c841 + 22223f5
audit-fix stack still showed Control Center sidebar + other bits
invisible; cursor moves "uncovered" the missing pixels.
Diagnosis via temporary `store.damage` instrumentation: out of
20,377 damage calls per session, **19,563 (96%) were silently
dropped** because their target had `scene_participating=false`.
Hot ids in the drop list were Manual-redirect *backings* — e.g.
id 152 was the backing for panel-top window 0x4000c1, which the
scene_walk trace confirmed was being emitted with `source_id=152`.
Root cause: `activate_redirect_backing_for` /
`flip_redirect_target_mode` / `rotate_redirected_backing_on_resize`
in `yserver-core::core_loop::process_request` computed a single
`participating = mode == Automatic` flag and applied it to both
the window AND the backing. Post-`6ffd370` the scene samples B
via `redirected_target` in **both** modes, so B's
`scene_participating` must be `true` regardless of mode for
`store.damage(B_id, …)` to accumulate. Only the W flag should
toggle with mode. Buffer-age clipped compose then had no damage
region for the redirected-window areas, retained whatever was in
each BO, and the cursor-projected damage (which goes directly
into `projected_damage` in `build_scene`, bypassing the store)
was the only thing causing those areas to repaint. This is the
same v2-side change attempted as part of the reverted 4d.8c,
applied in isolation now that the audit fixes have closed the
side issues that pushed the original revert. TDD: new
`manual_redirect_marks_backing_scene_participating_so_paints_emit_damage`
test pins B's flag;
`manual_redirect_keeps_backing_out_of_scene` renamed to
`_keeps_window_out_of_scene` and trimmed (the B assertion moved
out);
`rotate_redirected_backing_preserves_manual_scene_participation`
backing assertion inverted to `participating: true`.

### Stage 4d close decision (2026-05-17, superseded)

The pre-cow-authoritative reading was that v1's compositing
"support" is a no-op fallback that happens to render apps, and
v2 implemented the spec correctly (Manual-redirected windows
removed from scene; compositor is supposed to populate COW) but
no real compositor we'd tested populated COW correctly without
proper substrate (full PictFormat tracking, alpha-aware sampling,
possibly multi-layer alpha-mask). The compositing substrate was
judged bigger than 4d's scope, and the close path was framed as
either (a) "pragmatic close" — deviate from spec §I4 — or (b)
"spec-compliant alternative" — complete the compositor chain
(probably a separate Stage 4e).

The cow-authoritative-mode plan that closed Stage 4 went with
neither path exactly: instead, Phase 1 strips top-levels from the
scene **when COW is registered** (so non-compositor sessions
still get the full per-window scene walk), and Phase 2 reconciles
redirect status on `ReparentWindow`. Combined with the
correctness fix chain narrated above, that closed the symptoms
the 4d.8 pragmatic-floor attempt had failed to fix.

### Stage 4d.8 pragmatic-floor attempt — TRIED AND REVERTED 2026-05-17

Implemented as five sub-commits (b5d6287, 60db57c, 2283a11,
d2003d3) over a single evening. **Reverted (`8f0274c`, `8065a6f`,
`d46db4e`, `9ab8973`) after hardware-smoke showed the cumulative
effect made BOTH comp and non-comp WORSE than the pre-4d.8
baseline.**

- **4d.8a**: `default_window_init_color(32) = (0,0,0,1)`
  (opaque-black instead of transparent).
- **4d.8b**: `set_window_scene_participation(false)` (Manual mode)
  became a no-op — Manual-redirected windows stayed in the scene
  walk, sampling from B via 4c.3's `redirected_target` indirection.
- **4d.8c**: `activate_redirect_backing_for` set the backing
  `scene_participating=true` in BOTH modes (not just Automatic)
  so paint damage on B got tracked.
- **4d.8d**: skipped the COW draw in `build_scene` because
  marco/xfwm4 PresentPixmap full-screen onto COW with alpha=1
  everywhere → COW covered all scene-drawn windows.
- **4d.8e**: `emit_window_subtree` skipped descendant recursion
  under a redirected ancestor (rationale: ancestor's B has the
  subtree via cascade paint per X11 Composite spec).

Symptom progression after each landing:

- After 4d.8b: trace confirmed Manual windows stayed in scene
  with source_id != store_id (route indirection working), but
  visually the comp-mode result was identical to before —
  marco's COW still covered everything opaquely because
  xRGB-intent alpha bytes from marco's offscreen propagated to
  COW (no PictFormat tracking).
- After 4d.8c: damage on B was tracked. Cursor movement revealed
  window content (proving damage→repaint chain worked), but
  flicker + "windows disappearing on hover-over-menu" + "layer
  switching when calendar opens" indicated COW was
  unconditionally covering scene draws.
- After 4d.8d: COW draw skipped. Full panel + some of Control
  Center visible. But CC's main content area was transparent —
  wallpaper-through. Hypothesis (4d.8e): double-emit of
  redirected parent + child where child's empty own-storage
  covered parent's cascade-painted B.
- After 4d.8e: static scanout looked good (Caja file manager
  rendered fully with sidebar + toolbar + items). Dynamic
  correctness BROKEN: windows + bits appearing / disappearing
  during use, "slow as molasses", flicker. Both comp and non-comp
  affected.

**Honest retrospective.** The 4d.8 stack chased visual symptoms
with progressively desperate, non-evidence-based fixes. Each one
moved a static-frame visible state forward but introduced
second-order issues in damage/repaint timing. By 4d.8e the
dynamic experience was worse than v1's "ignore the compositor"
floor on both comp AND non-comp paths. Reverted to restore the
d8bcd92 checkpoint state (post-4d.7 alpha_passthrough flip,
pre-4d.8 pragmatic floor). Even at that revert point, non-comp
dynamic correctness was reported as "bits appear/disappear, slow"
— suggesting the underlying damage/repaint/perf issue predated
4d.8 entirely and was masked when only static smoke was being
inspected. The cow-authoritative-mode plan picked up from that
revert checkpoint.

**What 4d.8 taught us (negative results worth recording)**:

- Keeping Manual-redirected windows in the scene without proper
  PictFormat tracking interacts badly with COW.
- COW pixel-as-ARGB without xRGB intent makes
  compositor-paint-to-COW unconditionally cover the scene.
- Damage tracking on backings WORKS (4d.8c verified) but doesn't
  fix the visual end-to-end without correct alpha semantics.
- Skipping descendant emission of redirected windows is
  structurally correct per X11 Composite spec, but the
  cascade-via-parent's-B model requires that the scene's single
  draw of the parent's B reflects all paint to that subtree —
  which depends on every paint correctly resolving to B via
  `resolve_paint_target`. Any escape path (e.g., a non-redirected
  child with its own paint that should also show) needs separate
  handling.

### Open investigation items captured at the close of 4d (2026-05-17, now mostly closed)

Captured at the point the 4d.8 attempt was reverted; most of
these were closed by the cow-authoritative branch + the fix
chain. The residuals are tracked in `known-issues.md`.

- ~~Why non-comp non-static behavior is also bad~~ (flicker,
  slow, missing bits). **Closed**: traced through the fix chain
  to a combination of DAMAGE delivery gaps + Present Copy
  direction + ConfigureNotify above_sibling + viewable
  XDamageCreate seeding.
- ~~Damage tracking correctness in steady state.~~ **Closed**:
  DAMAGE Subtract canonicalization + remaining-region re-report
  + geometry-change re-report + map-time descendant seed
  collectively fixed the steady-state class.
- **Performance**: scene tick cost in v2 may have grown from
  cumulative storage allocations, retire queues, descriptor-pool
  ring pressure. Tracked under **Stage 5**.
- **PictFormat / xRGB-vs-ARGB picture intent tracking** to
  properly support compositor-WM sessions in a future stage.
  Moved to `known-issues.md` as the Stage 4e candidate.
- ~~CursorFlicker / trail under compositing~~ — partly addressed
  by the hardware/software cursor split. Residual under
  `known-issues.md` if any.
- **KmsCore.pictures disconnect cleanup** (Task 4, still open).
  Moved to `known-issues.md`.

### Tasks fully tackled in the 4d closing session (2026-05-17 PM, pre-cow-authoritative)

Stage 4d.1 (`3ed630c`), 4d.2 (`589aa87`), 4d.3 DRI3 backfill
(`9414096`), 4d.4 disconnect-recovery participation (`6b63173`),
4d.5 rotate-redirected-backing order (`cd22f47`), 4d.6 depth-24
force opaque source (`d20f279`), 4d.7 alpha_passthrough=true for
windows (`f3e9276`). These are all kept; only the 4d.8 stack was
reverted.

The Justfile defaults now include `yserver::kms::v2::scene=trace`
in the recipe log defaults for ongoing diagnosis (`f3cd9cd`).

Open notes at the time:

- **Task 4 — KmsCore.pictures disconnect cleanup** (now in
  `known-issues.md`).
- Yellow group-header labels missing in mate Control Center
  sidebar (now in `known-issues.md`; likely a colored-source +
  glyph-mask Render::Composite path issue).
- ~~Control Center "bits flicker on hover" under marco-comp —
  buffer-age / damage-tracking hint.~~ **Resolved 2026-05-19 PM**
  by the Manual-redirect backing `scene_participating=true` fix
  above; was a manifestation of the same silent-damage-drop bug.
  Yoga smoke confirmed no more cursor-uncovers-bits symptom
  after the fix.
- v2 should still backfill PictFormat tracking + alpha
  interpretation per picture format (now in `known-issues.md`).
- Client-created pixmaps now initialize to opaque black, while
  visible windows keep the alpha-sensitive depth-32 default.
  This was a targeted response to compositor-owned offscreen
  buffers starting transparent and leaking desktop through
  unpainted regions.
- RENDER drawable sources now carry their requested `PictFormat`
  through the v2 resolver, so xRGB/RGB24 pictures are forced
  opaque even when they sit on 32-bit storage. The old
  depth-only heuristic was too coarse for compositor-managed
  window surfaces.
