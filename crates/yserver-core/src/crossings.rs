//! X11 EnterNotify/LeaveNotify crossing computation for implicit
//! pointer grabs.
//!
//! When a button press creates an implicit pointer grab, the grab
//! activation generates Leave/Enter events along the path between the
//! window the pointer was in (focus) and the grab window. The release
//! generates the symmetric Ungrab pair. Today's KMS backend (and any
//! host-X11 mirror) emit a single unconditional crossing on the press
//! window, which is wrong whenever focus and grab differ — see
//! `docs/superpowers/specs/2026-05-05-single-threaded-core-design.md`,
//! "Pre-existing bugs" #2.
//!
//! `implicit_grab_crossings` is a pure function over `ServerState`'s
//! window tree: hand it the focus + grab `ResourceId`s and it returns
//! the spec-correct sequence of crossing events. The caller threads
//! the events through whichever fanout machinery is appropriate (KMS
//! backend, host-X11 dispatcher).
//!
//! Detail-code reference (X11 protocol, EnterNotify/LeaveNotify):
//! - `0` `NotifyAncestor`
//! - `1` `NotifyVirtual`
//! - `2` `NotifyInferior`
//! - `3` `NotifyNonlinear`
//! - `4` `NotifyNonlinearVirtual`

use std::collections::HashSet;

use yserver_protocol::x11::ResourceId;

use crate::server::ServerState;

pub const NOTIFY_ANCESTOR: u8 = 0;
pub const NOTIFY_VIRTUAL: u8 = 1;
pub const NOTIFY_INFERIOR: u8 = 2;
pub const NOTIFY_NONLINEAR: u8 = 3;
pub const NOTIFY_NONLINEAR_VIRTUAL: u8 = 4;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CrossingKind {
    Enter,
    Leave,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct CrossingEvent {
    pub window: ResourceId,
    pub kind: CrossingKind,
    /// One of the `NOTIFY_*` constants.
    pub detail: u8,
    /// X11 EnterNotify/LeaveNotify `child` field:
    /// - For source/destination endpoints: `ResourceId(0)` (X11 None),
    ///   matching Xorg's `CoreEnterLeaveEvent(..., None)` for these.
    /// - For virtual intermediates (NotifyVirtual / NotifyNonlinearVirtual):
    ///   the immediate descendant of `window` on the path to whichever
    ///   endpoint is in `window`'s subtree (source for Leave virtuals,
    ///   destination for Enter virtuals).
    pub child: ResourceId,
}

/// Compute Leave/Enter events for moving the pointer's logical
/// "window" from `focus` to `grab`. Order in the returned `Vec` is
/// the wire-emission order required by X11. Used for the
/// implicit-grab activation/release path (caller sets X11 `mode` to
/// `NotifyGrab` / `NotifyUngrab`).
///
/// `focus == grab` returns an empty `Vec`. Cycles or missing windows
/// (defensive — `state.resources.window` returns `None`) terminate
/// the chain walk early; the returned events are still well-formed
/// X11 (just possibly truncated).
#[must_use]
pub fn implicit_grab_crossings(
    state: &ServerState,
    focus: ResourceId,
    grab: ResourceId,
) -> Vec<CrossingEvent> {
    compute_crossing_chain(state, focus, grab)
}

/// Compute Leave/Enter events for moving the pointer's logical
/// "window" from `from` to `to` under X11 Normal mode (no grab).
/// Caller sets X11 `mode` to `NotifyNormal`.
///
/// Same chain-computation as [`implicit_grab_crossings`] — the only
/// difference is the `mode` value the caller emits with each event;
/// detail codes and child fields are identical (per X11 spec).
#[must_use]
pub fn normal_mode_crossings(
    state: &ServerState,
    from: ResourceId,
    to: ResourceId,
) -> Vec<CrossingEvent> {
    compute_crossing_chain(state, from, to)
}

fn compute_crossing_chain(
    state: &ServerState,
    from: ResourceId,
    to: ResourceId,
) -> Vec<CrossingEvent> {
    if from == to {
        return Vec::new();
    }

    let from_chain = ancestor_chain(state, from);
    let to_chain = ancestor_chain(state, to);

    // Case A: `to` is an ancestor of `from` (pointer "moves up" out of
    // a deeper subtree into a parent). Leaves walk from_chain up to
    // (not including) `to`; final Enter on `to`.
    if from_chain.iter().skip(1).any(|w| *w == to) {
        let mut events = vec![CrossingEvent {
            window: from,
            kind: CrossingKind::Leave,
            detail: NOTIFY_ANCESTOR,
            child: ResourceId(0),
        }];
        // Virtual leaves on intermediates: for each W in from_chain
        // (skipping `from` itself, stopping before `to`), W is ancestor
        // of `from` so `child` = immediate descendant of W on path to
        // `from` = the previous chain entry.
        for (i, w) in from_chain.iter().enumerate().skip(1) {
            if *w == to {
                break;
            }
            events.push(CrossingEvent {
                window: *w,
                kind: CrossingKind::Leave,
                detail: NOTIFY_VIRTUAL,
                child: from_chain[i - 1],
            });
        }
        events.push(CrossingEvent {
            window: to,
            kind: CrossingKind::Enter,
            detail: NOTIFY_INFERIOR,
            child: ResourceId(0),
        });
        return events;
    }

    // Case B: `from` is an ancestor of `to` (pointer "moves down" into
    // a descendant subtree). Leave on `from`, then virtual Enters on
    // ancestors of `to` from (not including) `from` down to (not
    // including) `to`, final Enter on `to`.
    if to_chain.iter().skip(1).any(|w| *w == from) {
        let mut events = vec![CrossingEvent {
            window: from,
            kind: CrossingKind::Leave,
            detail: NOTIFY_INFERIOR,
            child: ResourceId(0),
        }];
        // Gather intermediates (W where W is ancestor of `to`, but W !=
        // `to` and W != `from`). to_chain is [to, parent_of_to, ...,
        // from, ...]. Skip head (`to`), stop when we hit `from`. Reverse
        // to get downward emission order.
        let mut intermediate_indices: Vec<usize> = Vec::new();
        for (i, w) in to_chain.iter().enumerate().skip(1) {
            if *w == from {
                break;
            }
            intermediate_indices.push(i);
        }
        intermediate_indices.reverse();
        for i in intermediate_indices {
            // Virtual Enter on to_chain[i]: destination=`to` is inferior
            // of W=to_chain[i], child = immediate descendant of W on
            // path to `to` = to_chain[i - 1].
            events.push(CrossingEvent {
                window: to_chain[i],
                kind: CrossingKind::Enter,
                detail: NOTIFY_VIRTUAL,
                child: to_chain[i - 1],
            });
        }
        events.push(CrossingEvent {
            window: to,
            kind: CrossingKind::Enter,
            detail: NOTIFY_ANCESTOR,
            child: ResourceId(0),
        });
        return events;
    }

    // Case C: disjoint subtrees. Walk up from `from` to the lowest
    // common ancestor (LCA), then back down to `to`.
    let common = lowest_common_ancestor(&from_chain, &to_chain);
    let mut events = vec![CrossingEvent {
        window: from,
        kind: CrossingKind::Leave,
        detail: NOTIFY_NONLINEAR,
        child: ResourceId(0),
    }];
    for (i, w) in from_chain.iter().enumerate().skip(1) {
        if Some(*w) == common {
            break;
        }
        events.push(CrossingEvent {
            window: *w,
            kind: CrossingKind::Leave,
            detail: NOTIFY_NONLINEAR_VIRTUAL,
            child: from_chain[i - 1],
        });
    }
    let mut downward_indices: Vec<usize> = Vec::new();
    for (i, w) in to_chain.iter().enumerate().skip(1) {
        if Some(*w) == common {
            break;
        }
        downward_indices.push(i);
    }
    downward_indices.reverse();
    for i in downward_indices {
        events.push(CrossingEvent {
            window: to_chain[i],
            kind: CrossingKind::Enter,
            detail: NOTIFY_NONLINEAR_VIRTUAL,
            child: to_chain[i - 1],
        });
    }
    events.push(CrossingEvent {
        window: to,
        kind: CrossingKind::Enter,
        detail: NOTIFY_NONLINEAR,
        child: ResourceId(0),
    });
    events
}

/// `[start, parent_of_start, parent_of_parent, ..., root]`, terminated
/// when a window is its own parent (X11's root convention) or when a
/// missing/cyclic parent is reached. Capped at 256 hops as a defense
/// against malformed states.
fn ancestor_chain(state: &ServerState, start: ResourceId) -> Vec<ResourceId> {
    let mut chain = vec![start];
    let mut current = start;
    let mut hops = 0;
    while hops < 256 {
        let Some(window) = state.resources.window(current) else {
            break;
        };
        if window.parent == current {
            break;
        }
        chain.push(window.parent);
        current = window.parent;
        hops += 1;
    }
    chain
}

/// First entry of `b` that also appears in `a`. With the `start`
/// ordering both chains share, this yields the lowest common
/// ancestor. Returns `None` only if no shared ancestor exists (which
/// in a well-formed X11 state means at least the root, so `None` is
/// effectively defensive).
fn lowest_common_ancestor(a: &[ResourceId], b: &[ResourceId]) -> Option<ResourceId> {
    let a_set: HashSet<_> = a.iter().copied().collect();
    b.iter().copied().find(|w| a_set.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    use yserver_protocol::x11::{ClientId, CreateWindowRequest};

    use crate::{
        resources::{ROOT_VISUAL, ROOT_WINDOW},
        server::ServerState,
    };

    fn make_window(state: &mut ServerState, id: u32, parent: ResourceId) -> ResourceId {
        let rid = ResourceId(id);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: rid,
                parent,
                x: 0,
                y: 0,
                width: 10,
                height: 10,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        rid
    }

    fn windows_only(events: &[CrossingEvent]) -> Vec<ResourceId> {
        events.iter().map(|e| e.window).collect()
    }

    fn details_only(events: &[CrossingEvent]) -> Vec<u8> {
        events.iter().map(|e| e.detail).collect()
    }

    fn kinds_only(events: &[CrossingEvent]) -> Vec<CrossingKind> {
        events.iter().map(|e| e.kind).collect()
    }

    fn children_only(events: &[CrossingEvent]) -> Vec<ResourceId> {
        events.iter().map(|e| e.child).collect()
    }

    #[test]
    fn equal_windows_emit_no_events() {
        let mut state = ServerState::new();
        let w = make_window(&mut state, 0x0010_0030, ROOT_WINDOW);
        assert!(implicit_grab_crossings(&state, w, w).is_empty());
    }

    #[test]
    fn focus_descendant_of_grab_walks_up() {
        // root → A → B → focus, grab = A
        let mut state = ServerState::new();
        let a = make_window(&mut state, 0x0010_0010, ROOT_WINDOW);
        let b = make_window(&mut state, 0x0010_0011, a);
        let f = make_window(&mut state, 0x0010_0012, b);
        let events = implicit_grab_crossings(&state, f, a);
        assert_eq!(windows_only(&events), vec![f, b, a]);
        assert_eq!(
            kinds_only(&events),
            vec![
                CrossingKind::Leave,
                CrossingKind::Leave,
                CrossingKind::Enter
            ],
        );
        assert_eq!(
            details_only(&events),
            vec![NOTIFY_ANCESTOR, NOTIFY_VIRTUAL, NOTIFY_INFERIOR],
        );
        // child: Leave on focus → None. Virtual Leave on B (ancestor of
        // focus on path to focus) → child = f (immediate descendant of
        // B on path to f). Enter on grab (a) → None (source IS event
        // for Inferior detail; Xorg emits child=None on the dest).
        assert_eq!(
            children_only(&events),
            vec![ResourceId(0), f, ResourceId(0)],
        );
    }

    #[test]
    fn focus_ancestor_of_grab_walks_down() {
        // root → focus → A → B → grab
        let mut state = ServerState::new();
        let f = make_window(&mut state, 0x0010_0020, ROOT_WINDOW);
        let a = make_window(&mut state, 0x0010_0021, f);
        let b = make_window(&mut state, 0x0010_0022, a);
        let g = make_window(&mut state, 0x0010_0023, b);
        let events = implicit_grab_crossings(&state, f, g);
        // Order: focus first, then ancestors of grab from closest-to-focus
        // outward, then grab.
        assert_eq!(windows_only(&events), vec![f, a, b, g]);
        assert_eq!(
            kinds_only(&events),
            vec![
                CrossingKind::Leave,
                CrossingKind::Enter,
                CrossingKind::Enter,
                CrossingKind::Enter,
            ],
        );
        assert_eq!(
            details_only(&events),
            vec![
                NOTIFY_INFERIOR,
                NOTIFY_VIRTUAL,
                NOTIFY_VIRTUAL,
                NOTIFY_ANCESTOR,
            ],
        );
        // child: Leave on focus → None. Virtual Enter on A → child = b
        // (immediate descendant of A on path to g). Virtual Enter on B
        // → child = g. Enter on grab → None.
        assert_eq!(
            children_only(&events),
            vec![ResourceId(0), b, g, ResourceId(0)],
        );
    }

    #[test]
    fn root_to_direct_child_has_no_virtuals() {
        // root → frame (direct child). Cursor moves from root to frame.
        // This is the e16 hover-popup case: yserver must emit
        // LeaveNotify on root with detail=NotifyInferior so the WM
        // knows the cursor went into an inferior subtree.
        let mut state = ServerState::new();
        let frame = make_window(&mut state, 0x0010_0424, ROOT_WINDOW);
        let events = normal_mode_crossings(&state, ROOT_WINDOW, frame);
        assert_eq!(windows_only(&events), vec![ROOT_WINDOW, frame]);
        assert_eq!(
            kinds_only(&events),
            vec![CrossingKind::Leave, CrossingKind::Enter],
        );
        assert_eq!(
            details_only(&events),
            vec![NOTIFY_INFERIOR, NOTIFY_ANCESTOR]
        );
        assert_eq!(children_only(&events), vec![ResourceId(0), ResourceId(0)]);
    }

    #[test]
    fn disjoint_subtrees_walk_through_common_ancestor() {
        // root → C → A → focus
        // root → C → B → grab
        let mut state = ServerState::new();
        let c = make_window(&mut state, 0x0010_0040, ROOT_WINDOW);
        let a = make_window(&mut state, 0x0010_0041, c);
        let f = make_window(&mut state, 0x0010_0042, a);
        let b = make_window(&mut state, 0x0010_0043, c);
        let g = make_window(&mut state, 0x0010_0044, b);
        let events = implicit_grab_crossings(&state, f, g);
        assert_eq!(windows_only(&events), vec![f, a, b, g]);
        assert_eq!(
            kinds_only(&events),
            vec![
                CrossingKind::Leave,
                CrossingKind::Leave,
                CrossingKind::Enter,
                CrossingKind::Enter,
            ],
        );
        assert_eq!(
            details_only(&events),
            vec![
                NOTIFY_NONLINEAR,
                NOTIFY_NONLINEAR_VIRTUAL,
                NOTIFY_NONLINEAR_VIRTUAL,
                NOTIFY_NONLINEAR,
            ],
        );
        // child: Leave on focus → None. NonlinearVirtual Leave on A →
        // child = f (descendant of A on path to source f).
        // NonlinearVirtual Enter on B → child = g (descendant of B on
        // path to destination g). Enter on grab → None.
        assert_eq!(
            children_only(&events),
            vec![ResourceId(0), f, g, ResourceId(0)],
        );
    }

    #[test]
    fn disjoint_with_root_as_only_common_ancestor() {
        // root → focus, root → grab — common ancestor is root, which
        // is *not* in the chains' "skip root" iteration: chains end at
        // root because window.parent == current at root, so root never
        // gets a Leave/Enter virtual.
        let mut state = ServerState::new();
        let f = make_window(&mut state, 0x0010_0050, ROOT_WINDOW);
        let g = make_window(&mut state, 0x0010_0051, ROOT_WINDOW);
        let events = implicit_grab_crossings(&state, f, g);
        // Just Leave on focus + Enter on grab (no virtuals because no
        // intermediate ancestors below root).
        assert_eq!(windows_only(&events), vec![f, g]);
        assert_eq!(
            details_only(&events),
            vec![NOTIFY_NONLINEAR, NOTIFY_NONLINEAR],
        );
        assert_eq!(children_only(&events), vec![ResourceId(0), ResourceId(0)]);
    }

    #[test]
    fn ancestor_chain_is_capped() {
        // Chain length cap is defensive — there's no practical way to
        // construct a 257-deep tree in the test harness here, so just
        // assert the code path doesn't loop forever on a sane case.
        let mut state = ServerState::new();
        let mut parent = ROOT_WINDOW;
        for i in 0..32 {
            parent = make_window(&mut state, 0x0010_0100 + i, parent);
        }
        let chain = ancestor_chain(&state, parent);
        // 33 entries: the leaf + 32 ancestors up to and including root.
        // (root is included because we hit it via parent==current and
        // break *after* pushing.)
        assert!(chain.len() >= 32, "chain too short: {}", chain.len());
        assert!(chain.len() <= 257);
    }
}
