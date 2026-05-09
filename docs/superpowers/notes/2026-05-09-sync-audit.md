# XSync handler audit (Phase 4.2.2 baseline)

Date: 2026-05-09
Reference: `crates/yserver-protocol/src/x11/sync.rs`,
`crates/yserver-core/src/core_loop/process_request.rs:handle_sync_request`.

Phase 4.2.2 needs `Fence` lifecycle (Create/Destroy/Trigger/Reset/Await
plus QueryFence) plus the DRI3 `FenceFromFD`/`FDFromFence` import path
(Task 19) and `ImportSyncobj`/`FreeSyncobj` (Task 20).

## Request inventory

| Opcode | Name                | Status         | Notes |
|-------:|---------------------|----------------|-------|
| 0  | `Initialize`            | handled        | replies with negotiated min(client, server). |
| 1  | `ListSystemCounters`    | empty stub     | always returns 0 counters. ServerTime/IDLETIME could be added but no demand yet. |
| 2  | `CreateCounter`         | handled        | XID stashed in `ServerState::sync_counters`. |
| 3  | `SetCounter`            | handled        | mutates the counter in place. |
| 4  | `ChangeCounter`         | handled        | saturating_add delta. |
| 5  | `QueryCounter`          | handled        | reads stored value or 0. |
| 6  | `DestroyCounter`        | handled        | removes from map. |
| 7  | `Await`                 | **non-blocking stub** | does nothing; clients that block on this will hang their flow. Phase 4.2.2 needs a real implementation when XSync `Fence`s are available so 2-client trigger/await testing works. |
| 8  | `CreateAlarm`           | handled        | XID stashed; alarm fields are zero-init. |
| 9  | `ChangeAlarm`           | partial        | sets state=0 on the alarm; doesn't actually drive state machine. |
| 10 | `QueryAlarm`            | handled        | returns whatever `SyncAlarm` carries. |
| 11 | `DestroyAlarm`          | handled        | removes from map. |
| 12 | `SetPriority`           | stub (no-op)   | acceptable. |
| 13 | `GetPriority`           | handled        | returns 0. |
| 14 | `CreateFence`           | **unimplemented** | Phase 4.2.2 Task 17 lands the wire opcodes + a server-only `FenceState` (booleon `triggered`). |
| 15 | `DestroyFence`          | **unimplemented** | Task 17. |
| 16 | `SetCounter` (alt opcode used pre-3.1?) | n/a | dri3proto / sync 3.1 don't use this. |
| 17 | `Pad`                   | n/a            | reserved. |
| 18 | `TriggerFence`          | **unimplemented** | Task 17. |
| 19 | `ResetFence`            | **unimplemented** | Task 17. |
| 20 | `QueryFence`            | **unimplemented** | Task 17. |
| 21 | `AwaitFence`            | **unimplemented** | Task 17 — multi-fence wait. |

Per `syncproto` v3.1, `CreateFence`'s wire body is
`(picture: XID, initially_triggered: BOOL, pad: 3)`; in the X11 dialect
the fence resource is a u32 XID and the `initially_triggered` bool
fits in the high nibble of the bottom byte.

## Out of scope for 4.2.2

- Driving `Await` against actual signaling — Phase 4.2.2 lands the
  trigger/await machinery for the new `Fence` resource only. Counter-
  based `Await` is a follow-up.
- Multi-counter alarms with non-trivial state transitions.
- `ListSystemCounters` populating ServerTime — Phase 5 territory.

## Plan-task → audit-row mapping

- Task 17 (Fence lifecycle) handles opcodes 14, 15, 18, 19, 20, 21.
- Task 18 (KMS-side VkSemaphore import) is non-wire — adds the
  Vulkan helpers `import_sync_file` / `import_drm_syncobj` /
  `export_sync_file` consumed by Tasks 19 / 20.
- Task 19 (`FenceFromFD` / `FDFromFence`) is on the DRI3 dispatcher,
  not SYNC. The fence XID itself was created via `CreateFence`
  (Task 17) but its underlying VkSemaphore comes from the
  client-supplied `sync_file` fd.
- Task 20 (`ImportSyncobj` / `FreeSyncobj`) is also DRI3-side, with
  the same VkSemaphore-backed plumbing but timeline-typed.
