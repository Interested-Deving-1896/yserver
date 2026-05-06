# X Test Suite (xts5) baseline

First run of the X.Org X Test Suite against `ynest`, captured 2026-05-06
right after XTEST landed.

## How to reproduce

1. Build xts5 against the local checkout at `/home/jos/Projects/xts`
   (the existing meson build under `build/`).
2. From the yserver checkout: `just xts-ynest scenario=Xproto` — this
   builds release `ynest`, boots it on `:99` with a 1024×768 host
   container, runs `tools/xts-run.sh` (which invokes `xts/check.sh`),
   tears down ynest on exit.
3. Result tree lands in `/home/jos/Projects/xts/results/<timestamp>/`
   with a `summary` file alongside the raw `journal`.
4. Re-run with a different scenario by overriding the recipe arg:
   `just xts-ynest scenario=Xlib3`.

## Result columns

The xts-report layout: `CASES | TESTS | PASS | UNSUP | UNTST | NOTIU |
WARN | FIP | FAIL | UNRES | UNIN | ABORT`. CASES is test cases; TESTS
is "test purposes" (each case has 1+ purposes — closer to the unit
count we care about). Stable success is PASS; everything else is some
flavour of not-passing.

## Run history (ynest, `Xproto` scenario)

| Date       | PASS | FAIL | UNRES | UNIN | NORES | Notes |
|------------|-----:|-----:|------:|-----:|------:|-------|
| 2026-05-06 |    1 |  210 |   160 |   11 |     7 | First run after XTEST landed. |
| 2026-05-06 |    1 |   74 |   296 |   11 |     7 | After `BadLength` enforcement at the top of `process_request` (per-opcode length table for opcodes 1–127). 136 tests moved FAIL → UNRES: each AllocColor-style probe runs 2 native + 2 BE sub-checks; previously the native sub-checks FAILed (BadLength not raised) so the test result was FAIL; now those sub-checks pass but the BE sub-checks still UNRES on connection rejection, leaving the test as UNRES. Real BadLength progress; PASS count gated on big-endian client support. |

(Total tests: 122 cases / 389 purposes throughout.)

The full scenario completes in ~4 minutes. **The headline is that
ynest stays up through the entire battery** — no panics, no hangs,
no socket disconnects beyond what the tests themselves induced. From
a "does the server survive xts" angle, that's the win.

The sole PASS is `OpenDisplay 2`. The remaining outcome is masked by
the structural bugs below. The first row was the baseline cause;
struck-through rows are fixed but their tests still UNRES because of
a remaining gate further up.

| Failure mode | REPORT lines | Cause |
|---|---|---|
| big-endian client connection rejected | 483 | xts opens a second connection in reversed byte sex to test byte-swap handling; ynest's setup handshake refuses (by design — see `setup_thread.rs`). Until this is fixed, every test that runs both native+BE sub-checks UNRES'es regardless of native correctness. |
| ~~`BadLength` not raised~~ | ~~433 (250 + 183)~~ | **Fixed 2026-05-06.** Per-opcode length contract enforced for opcodes 1–127 in `process_request`; under-length and (for fixed opcodes) over-length headers now reply `BadLength`. |
| `Expose` not delivered | 131 | Specific Expose-generation gaps (~30-ish unique tests). |

The other ~200 individual FAILs are spread across grab semantics,
screen-saver state, error-code edge cases, etc.

## Quick-win path (in priority order)

1. ~~**`BadLength` enforcement.**~~ Done 2026-05-06; behaviour
   verified against all per-opcode under/over-length probes. The
   PASS count did not move because the same tests also probe
   reversed byte sex; see #2.
2. **Big-endian client byte-order at the wire reader.** Now the
   gating issue: with `BadLength` correct, ~136 tests UNRES purely
   because their second connection (BE) is refused. Implementation
   would need swap-tables for request bodies, replies, events, and
   the setup-success encoder. Larger surface but unblocks a clear
   set of tests.
3. **`Expose` correctness pass.** Smaller bucket; specific bugs
   rather than a single missing primitive.

## Known caveats

- The xts results dir lives outside the yserver tree
  (`/home/jos/Projects/xts/results/`). The `summary` from this
  baseline is checked in at `docs/xts-baseline-summary.txt` for
  reproducibility.
- `Xproto` is the most "protocol-shaped" scenario. Other categories
  (Xlib*, Xt*, XI, SHAPE) will have different failure profiles —
  Xt-suite tests will mostly UNRES because the toolkit's font /
  resource expectations diverge sharply from ynest's stubs.

## Not yet measured

- **`yserver` (KMS) baseline.** The KMS backend runs only inside
  `vng`, so running xts against it requires either building xts
  inside the guest's rootfs or tunnelling a guest DISPLAY to the
  host. Deferred — once the structural quick-wins land we expect
  KMS numbers to be lower than ynest (no `RENDER`-via-host fallback,
  fewer extension stubs), and the comparison is only interesting
  after those are fixed.
