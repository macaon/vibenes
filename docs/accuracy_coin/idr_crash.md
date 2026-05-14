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
banner. `accuracy_coin` currently reports 116 / 19 / 5 untouched after
the 2026-05-08 reset/apu-cycle fix (was 117 / 18 / 5 before; the
delta is one Loop1-class test that was passing by accident under
the previous dual-bug compensation).

`$80DF` itself is just `InfiniteLoop` (asm:467) - the rom's normal
"wait for NMI" park between tests and on the result screen. The
"trap" detection in the runner abort isn't seeing a panic landing
pad, it's seeing the rom sit in InfiniteLoop for >32 frames without
$046D ever being written, which is the symptom of IDR not completing.

## Smoking-gun cycle (2026-05-13)

With `bus.open_bus` added to `src/nes/cpu/trace.rs` (as the `ob=`
field) the exact moment the IDR dance derails is now visible. From
the cycle-by-cycle trace at Loop5 iter 1 dance entry:

```
cyc=132813225 pc=$4013 ob=$40 ...   ← PHA opcode fetch (DMC byte $48 on bus)
cyc=132813232 pc=$4014 ob=$28 ...   ← PHA pushed A=$28, bus latched $28
cyc=132813236 pc=$4015 ob=$28 ...   ← bus still $28
cyc=132813242 pc=$4061 ...          ← JSR'd to $4061 (wrong target)
```

The test rom's choreography (asm line 12229 comment): the open-bus
dance at `JMP $4013` is supposed to fetch PHA at $4013 (DMC DMA
puts $48 on the bus), then PLP at $4014 (bus stays $28 from the
push), then **BRK at $4015** ($4015 returns
`status | (open_bus & 0x20)` - to get `$00` (BRK) the bus latch
must have bit 5 clear). DMC byte `$48 = 0100 1000` has bit 5 = 0,
so a fresh DMC fetch on the bus latch at $4015 read time yields
BRK and the rom routes into TEST_ImpliedDummyRead_BRKed5 cleanly.

What we do instead: the DMC DMA fetches its byte ~5 CPU cycles too
early - the byte lands at the bus latch during PHA's fetch (cyc
132813225 area), then PHA pushes A=$28, **bus latch is now $28**,
and by the $4015 read at cyc 132813236 the latch still reflects
`$28` (the PLP-popped opcode byte). `$28 & $20 = $20`, so $4015
returns `$20` - the JSR opcode. JSR's operand reads at $4016 and
$4017 land at $61 / $40 respectively, JSR'ing to `$4061`. The rom
ends up deep in unrelated test code, runs through corrupted
state, eventually parks at `$80DF` without writing `$046D`.

### Root-cause direction

Our DMC DMA byte arrives on the data bus at a 5-cycle-earlier
hardware moment than Mesen2's. The rom's "0 cycles until DMA"
calibration (asm line 11857) is choreographed against the
Mesen2-equivalent / hardware DMA-fire cycle, not ours. The
candidate fixes that need investigating - in priority order:

1. **DMC DMA halt-cycle count** - our `process_pending_dma` may be
   running the halt + dummy + fetch in fewer total cycles than
   Mesen2 does, so the fetch lands earlier. Check
   `NesCpu.cpp:325-448` for cycle exactness.
2. **Final-byte-after-loop-off path** - the IDR test sets
   `$4010 = $0F` (loop off) during WaitForFrameCounterFlag, so the
   final DMA after loop-off is what the dance reads. Our timing
   of that specific final fetch (relative to bit-counter wrap)
   may be off.
3. **DMC sample-duplication glitch interaction** - the length-1
   glitch added in `ebeebe5` triggers a `disable_delay = 3` that
   subtly delays/cancels the final DMA. Worth a "remove the
   glitch, see what changes" experiment.

The fix isn't blind cycle-tweaking; with `ob=` visible in the
trace, the test is now: nudge DMA timing so `ob=$48` at the
`$4015` fetch (cyc 132813236 in current state). Any of the three
candidates above could close that 5-cycle gap.

## What changed on 2026-05-08

Two real cycle-accuracy bugs were fixed (commit `7d37180`):

1. CPU reset ran 7 cycles instead of Mesen2's documented 8
   (NesCpu.cpp:160-164). Each missing reset cycle is +3 PPU dots;
   without the 8th tick, PPU + APU sat one full CPU cycle behind
   from power-on forever, with the drift compounding through
   `DMASync` polling loops.
2. With `cpu_cycles` initialised to `u64::MAX` so the 8 reset reads
   land first instruction at cycle 7 (matching Mesen), `apu.cycle`
   still started at 0 and ran +1 ahead of `cpu_cycles` permanently.
   That inverted the parity passed to `Dmc::set_enabled` at every
   `$4015` write, mis-shifting DMC DMA arming by one CPU cycle and
   breaking `cpu_interrupts_v2/4-irq_and_dma`. Aligning `apu.cycle`
   init to `u64::MAX` restores the contract that `apu.cycle ==
   cpu_cycles` at observation time.

`4-irq_and_dma` went from FAIL back to PASS as a result, plus all
1349 lib tests, all `apu_test` 8/8, all `apu_reset` 6/6, all
`ppu_vbl_nmi` 10/10, `mmc3_test` 6/6, `mmc3_test_2` 6/6,
`dmc_dma_during_read4` 5/5, both `sprdma_and_dmc_dma` ROMs, and
`instr_test-v5` 16/16 stayed green.

But: the IDR trap was **not** unblocked by the fix. The rom now
reaches dance iteration 15 (vs prior 13-14), `ErrorCode` at trap
is `$07` (vs prior `$1D`), and the first `$80DF` lands at cycle
~140.4M (vs prior ~139.5M). Different symptom, same end state.

## Why we can't currently get a full Mesen2 reference trace

`tools/mesen_trace_acc.lua` was built to drive Start through the
auto-Start menu so Mesen2's `--testRunner` mode runs the test
suite. It works for short ROMs but **Mesen2's testRunner exits at
about 5.2 M CPU cycles** (~175 NTSC frames, ~3 seconds) regardless
of the script's `LIMIT_CYCLES`. AccuracyCoin's auto-Start press
happens at frame ~240 (~7.2 M cycles), so the testRunner gives up
before the test suite even begins. Verified empirically by setting
`LIMIT=200000000 START=0` and observing the trace cuts off at
cyc ≈ 5.2 M with PC sitting in the menu's `InfiniteLoop`.

## What we learned on 2026-05-09 (Mesen2 trace diff attempt)

The earlier note about the testRunner exiting at 5.2 M cycles was
wrong - the script was using `emu.stop(0)` instead of the documented
`emu.exit(0)`. Once that was fixed (and stdout-buffering worked
around by deferring `addMemoryCallback` registration until just
before `START_CYCLES`), the harness can capture Mesen2 traces at
arbitrary cycle ranges.

**But the diff revealed a much bigger gap than the original Loop5
investigation suspected:** vibenes runs 2-3 M cycles **ahead** of
Mesen2 by the time the IDR test starts. Concrete evidence (post-
2026-05-08 reset/apu fix):

- vibenes hits IDR Loop1 iter 1 at `cyc = 139,438,083`.
- vibenes hits IDR PHA dance entry at `cyc = 140,167,299`.
- vibenes traps at `$80DF` at `cyc = 140,354,348`.
- Mesen2 traced over `cyc 138,000,000 - 141,750,000` shows **zero**
  `pc=400F` entries. Mesen2 is sitting in `ClockslideFromWord`
  (`pc=$FD71/$FD72` DEY/BNE loop) for more than 4 M cycles, well
  before reaching IDR Loop1.

So the cycle-by-cycle drift is at least 2.3 M cycles long before
the Loop5 dance corruption ever happens. Reasoning purely about
"Loop5 first iter's $4015 parity" was missing the upstream
discrepancy.

The implication: this isn't a single off-by-one DMC parity bug.
Some test (or several) earlier in the suite consume different cycle
counts on vibenes vs Mesen2, and the accumulated drift puts vibenes
into a different spot in Mesen's overall cycle timeline by the time
IDR runs. The IDR test rom's hand-crafted DMA-sync dances assume
specific cycle alignments that hold on real hardware (and on Mesen2
closely enough that it produces a clean fail-byte rather than a
hang); on vibenes the alignment is wrong enough that the dance
produces opcodes that loop back to `JMP $400F` indefinitely.

Practical next steps to actually localise the upstream divergence:

1. Find where vibenes and Mesen2 first disagree on a cycle count
   for the same PC. The trace harness produces matching `[M]`-line
   format - run both with `START=0` and a moderate `LIMIT` (say
   30 M cycles, ~150 frames past auto-Start), then `diff` the
   first divergent line. That gives us the test (and likely the
   instruction) where the drift began.
2. The drift accumulator is most likely an early test that polls
   a flag that flips at a different cycle on vibenes vs Mesen2 -
   classic suspects are `$2002` VBlank flag, the APU frame-counter
   IRQ flag, or DMC sample-loop alignment. Once the offending
   test is identified, the fix may be small (a few-cycle PPU dot
   shift, a frame-counter event-table tweak, or a DMC fetch
   tick-count off by one).
3. Mesen2 testRunner runs at ~1.5% real-time speed (about 1 M
   cycles per 70 s wall time), so capturing a full
   `cyc 0 - 30 M` Mesen2 trace takes ~35 minutes wall time but is
   feasible. The `tools/mesen_trace_acc.{lua,sh}` harness now
   handles this correctly with `emu.exit` + deferred-arm.

Available reference options:

- **Mesen2 testRunner (now usable)**: emu.exit + deferred-arm
  fix in `tools/mesen_trace_acc.lua` makes long-window captures
  possible. Wall time grows linearly with the cycle window so
  staying close to `START=0` is best.
- **puNES / Nestopia**: per prior notes, both also FAIL this test.
  Useful as a sanity-check but not authoritative.
- **Real hardware capture**: closest to authoritative. Out of
  scope for a local session.

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
