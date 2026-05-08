# AccuracyCoin "Implied Dummy Reads" trap (open)

## Symptom

Running `accuracy_coin` against `~/Git/nes-test-roms/AccuracyCoin.nes`
gets stuck in the test ROM's "park" landing pad:

- PC busy-loops at `$80DF` (`JMP $80DF`, the main InfiniteLoop).
- Result byte for **Implied Dummy Reads** (`$046D`) stays `$00`
  (untouched).
- The next four Page 20 result bytes (`$048B` Branch Dummy Reads,
  `$047C` JSR Edge Cases, `$0490` Internal Data Bus) also never run.

The runner detects a sustained PC at `$80DF` and aborts the frame
loop early so the report still renders, with a `note: test ROM trapped`
banner. `accuracy_coin` reports 117 / 18 / 5 untouched as of this
writing.

## What we know now (after the 2026-05-03 round)

The test ROM iterates through several sub-tests inside
`TEST_ImpliedDummyRead`. The compact PC-trace harness in
`src/bin/accuracy_coin.rs` (env-gated; the dance-iteration counter +
the `compact_trace_active` flag emit one line per instruction once we
pass dance iteration 13) shows we successfully complete **24 of the
29 implied opcodes the test exercises**:

| Sub-test | Opcodes | Status |
|---|---|---|
| Loop1 (no-bit-5) | ASL A, CLC, LSR A, CLI, DEY, TXA, TYA, TXS, INY, DEX, CLD | **all 11 pass** |
| Loop2 (bit-5) | ROL A, SEC, ROR A, SEI, TAY, TAX, CLV, TSX, INX, SED, NOP | **all 11 pass** |
| PHP | PHP | **pass** |
| PHA | PHA | **pass** |
| Loop5 (PLP/PLA) | PLP, PLA | not reached cleanly |
| Loop6 (BRK/RTI) | BRK, RTI | not reached |
| JSR test | JSR | not reached |
| RTS test | RTS | not reached |

ErrorCode reaches `$1D` after PHA (24 opcodes verified), then state
becomes incoherent (ErrorCode $00, `Copy_X` = $64, `$A5` = $29 — all
backed-up zp values from `RestoreRAM`). The CPU eventually JMPs back
into `$400F` from inside the test code (`$D4DC: JMP $400F`,
re-running the PHP test setup) but with state mismatched, then
diverges into wild execution and parks on InfiniteLoop.

The bisected fault therefore lies somewhere in the **PHA → PLP/PLA
loop transition**, not in the basic implied-dummy-read pattern. The
24 simple implied opcodes (transfers, increments, flag ops, NOP, PHP,
PHA) all see their cycle-2 PC+1 dummy read fire correctly through
$4015 and clear the frame-counter IRQ flag — that path is healthy.

The doc's earlier note about "PC reaches $80DF via RTI from $F859" is
the *aftermath*: once we end up at InfiniteLoop, the per-frame NMI
runs through ResetScroll + ApuCleanup and RTIs back to $80DF every
frame. The 3-byte stack frame at `$1ED-$1EF` is just `$80 / $DF /
$25` left there by the NMI's PC + P push when the CPU was at $80DF
— normal NMI mechanics, not an ill-formed stack craft.

## What we now know about the timing path (2026-05-04)

The polling loop in `DMASyncWith48` (`asm/AccuracyCoin.asm:16021`)
DOES eventually exit even on the failing iteration. The hang is
**later** in the Loop5 dance, not in the DMC sync polling itself.

Per-iteration evidence captured via `VIBENES_DMA_TIMING=1
accuracy_coin`:

- Loop1 / Loop2 / PHP / PHA iterations of `DMASyncWithXX_Start`'s
  `$4015 = $14` write all happen with `apu.cycle` parity **even**
  (`parity_odd=false`). Each polls for ~509 cycles before the DMA
  hijacks `LDA $4000`.
- The Loop5 first iteration's `$4015 = $14` write happens with
  `apu.cycle` parity **odd** (`parity_odd=true`). The polling
  takes ~2099 cycles to exit (4x slower) but does eventually exit.
- After polling exits, `WaitForFrameCounter` runs (~30k cycles),
  then the dance starts via `JMP $4013`. We see `DMA hits read
  $4013` at cycle 137,676,571 — confirming the dance entered.
- Between cycle 137,676,571 (dance entered) and cycle 137,793,218
  (first PC=$80DF) there is a **116k-cycle window** where the
  CPU diverges from the expected dance flow and ends up parked
  on InfiniteLoop.

So the bug is **after the polling loop exits** — somewhere in the
Loop5 dance / `BRKed5` handler / Loop5 next-iteration setup. The
DMC parity flip from even (Loop1-PHA) to odd (Loop5) shifts our
DMA timing relative to real hardware by 1 cycle, which somewhere
in that 116k-cycle window causes the CPU to take a different
branch / pop the wrong stack / land on a different PC.

Empirically tested: inverting the parity check
(`(self.cycle & 1) == 0` instead of `== 1`) does not fix the
hang and regresses one other test (`accuracy_coin` total goes
from 117/18 to 116/19 pass/fail). So the parity convention itself
is correct against most tests; the bug is some downstream cycle
accounting that produces the wrong parity at this specific
$4015 write.

## Likely fault domain

Loop5 / Loop6 / JSR-test all exercise the same open-bus dance idiom
that Loop1/Loop2 do, but with a subtly different cycle alignment and
stack-craft (Loop5 enters at `$4013`, pushes `Low(Post5)-1` for an
RTS-style return, etc.). Candidates for what we get wrong:

- **DMC DMA cycle alignment** that drifts by one cycle during the
  longer per-iteration setup of Loop5/6 (DMASyncWith68 / Clockslide_39
  / Clockslide_40 vs the smaller Clockslide_36 in Loop1).
- **`$4015` open-bus suppression on read during DMA**. We confirmed
  the static path (`bus.rs::read`) doesn't update the open-bus latch
  for $4015, but the path may behave differently when a DMC DMA cycle
  is interleaved with the read.
- **PHP/PHA stack-frame state on entry to the next sub-test**. The
  test pre-pushes between 2 and 3 bytes per sub-test; the test relies
  on `RTS` / `RTI` popping back through the right number of bytes.
  Off-by-one on a push or an extra dummy push in our model would
  derail the next sub-test exactly the way we see.

## Diagnostic harness

`src/bin/accuracy_coin.rs` carries:

- A 64-entry instruction-boundary ring buffer dumped on first
  `PC=$80DF` entry. Catches the NMI handler chain that brings us
  back to InfiniteLoop.
- A `dance_iteration` counter + `dance_trace_remaining` window that
  prints 24 instructions per `PC=$400F` entry. Reveals which
  sub-test's dance is which iteration, and what opcode each one
  actually executed.
- A `compact_trace_active` flag that flips on after dance iteration
  13 and emits a one-line trace per instruction (with WaitForVBlank
  and Clockslide spin loops collapsed). Captures the PC trail from
  PHA-success through the divergence point.

Pulled into the runner alongside the existing trap detection so a
single `accuracy_coin <rom>` run produces all three reports. Total
output is ~50 KLOC of trace lines on a trapped run; pipe stderr to a
file for analysis.

The trace harness at `tools/{trace_mesen.sh,mesen_trace.lua}` and
`src/cpu/trace.rs` is also available for cycle-by-cycle diff against
Mesen2 — what's still missing is a Mesen-side run that auto-presses
Start (Mesen's `--testRunner` doesn't drive the controller), so the
diff window can be narrowed to the suspect Loop5 entry. Mesen2 itself
also fails this test, so the diff can only validate our cycle
accuracy against another flawed implementation; for a definitive
fix we'd need a cycle-trace from real hardware (or from puNES, which
also fails).

## Where this lives in the roadmap

P0 in the AccuracyCoin remediation plan, but with reduced urgency
now that we know the basic implied-dummy-read mechanism works for
24 of 29 opcodes. The remaining failure window blocks measurement of
the last four Page 20 tests, which themselves are largely **shared
P2 fails with Mesen2** (Mesen2 also fails Branch Dummy Reads,
Implied Dummy Reads, Internal Data Bus). Real ceiling impact of
unblocking is small; correctness impact on commercial games is
unknown but unlikely — no real ROM stack-crafts an open-bus dance
through $4015 to test cycle-2 dummy reads on PLP/PLA.

## Next steps when resuming

This is genuinely a cycle-accuracy bug, not an architectural
problem. Fixing it without shortcuts requires cycle-by-cycle
trace diff against a passing reference. Concrete plan:

1. **Build the Mesen2 auto-Start trace harness.** Mesen2's
   `--testRunner` flag doesn't drive controllers, so we need a
   Lua script that:
   - Boots `AccuracyCoin.nes`.
   - Waits ~240 frames for the menu.
   - Holds Start for ~8 frames (triggers
     `AutomaticallyRunEveryTestInROM` since `menuCursorYPos` is
     `$FF` at boot).
   - Logs every CPU instruction to a file in the same format as
     `src/nes/cpu/trace.rs`'s `[M] cyc=N pc=XXXX op=XX a=...`
     output.
   - Stops when PC enters Loop5's `DMASyncWith48` polling region
     for the first time, OR after a fixed cycle budget.

   See `tools/mesen_trace.lua` for the existing instruction-trace
   template. The Mesen2 Lua API exposes `emu.setInput(player,
   button, state)` for controller injection and
   `emu.addEventCallback(callback, eventType.exec)` for
   per-instruction hooks.

2. **Capture a vibenes trace through the same window** by setting
   `VIBENES_TRACE_LIMIT=200000000 VIBENES_TRACE_START=137000000`
   while running `accuracy_coin`. The format is identical to
   Mesen2's, so a `diff` will show the first divergent cycle.

3. **Bisect the divergence.** The first cycle where our PC, A, X,
   Y, SP, P, or master clock disagrees with Mesen2's is exactly
   where some instruction or bus interaction has the wrong cycle
   count or wrong side effect. Fix that, re-run, repeat until
   AccuracyCoin's `$046D` byte gets written.

4. **Validation suite to keep green during the fix:**
   - `instr_test-v5/official_only.nes` (16/16, baseline)
   - `cpu_interrupts_v2/*` (5/5)
   - `apu_test/*` (8/8)
   - `dmc_dma_during_read4/*` (5/5)
   - `sprdma_and_dmc_dma{,_512}.nes`
   - `cargo test --release --lib` (1349 tests, baseline)

   The cycle-accuracy fix MUST keep all of these green - if a
   change breaks them, it's likely a parity-direction or
   delay-count regression that needs a more surgical patch.

5. **Why Mesen2 reportedly fails this same test** — Mesen2 also
   reports IDR as "fail" on its result-byte report, but Mesen2
   does NOT trap forever like we do. Their failure path produces
   a result byte; ours does not. So Mesen2 has the cycle drift
   too but the drift is small enough that the test ROM's failure
   handler still runs to completion. Our drift compounds harder
   somewhere - possibly multiple cycles off, not just one.
