# 4-irq_and_dma — Investigation Notes

**Target ROM**: `~/Git/nes-test-roms/cpu_interrupts_v2/rom_singles/4-irq_and_dma.nes`
**Source**: `~/Git/nes-test-roms/cpu_interrupts_v2/source/4-irq_and_dma.s`
**Sibling**: `4-nmi_and_dma.s` (identical scaffolding; only the interrupt line differs)
**Our status**: FAIL, hash `B33211E9`; full pass is hash `43571959`.

---

## 1. What the test actually measures

The ROM prints a table `N +M` for each iteration of the `test` macro. `N`
is the **return-address byte offset relative to `landing`** of the IRQ'd
instruction, and `M` is the `dly` parameter (0..13 and 524..527 in the
compiled output). Each iteration ends with the IRQ handler's `rti`; the
pushed PC is recovered from the stack (`tsx; dex; lda $100,x`) and
reported via `print_dec`.

Landing layout (byte offsets = the numbers in column N):

```
landing+0   NOP       2 CPU cycles
landing+1   NOP       2 CPU cycles
landing+2   LDA #$07  2 CPU cycles
landing+4   STA $4014 4 CPU cycles + 513/514 DMA   = SPRDMA
landing+7   NOP       2 CPU cycles (first post-DMA)
landing+8   NOP       2 CPU cycles
landing+9   NOP       2 CPU cycles
landing+10  SEI       the handler only reaches here for very high dly
```

`delay 532-(dly)` fires the frame IRQ at a fixed absolute cycle, and
tunes where in `landing` the CPU happens to be when the IRQ asserts.
Higher `dly` = less pre-delay = cpu enters `landing` *later* relative
to IRQ = IRQ lands further into `landing` instructions.

Expected table (from the `.s` comment block):

```
dly   column   meaning
0     0        IRQ serviced before landing starts → return = landing+0
1..2  1        NOP#1 ran → return = landing+1
3..4  2        NOP#2 ran → return = landing+2
5..6  4        LDA#07 ran → return = landing+4          (no offset 3 because LDA#imm is 2 bytes)
7..10 7        STA SPRDMA ran → return = landing+7      ← only 4 dly values
11..526  8     STA+DMA + NOP#3 ran → return = landing+8 ← ~516 dly values
527   9        + NOP#4 → return = landing+9
```

Total span of column 7 in the expected output is **four cycles** — the
four CPU cycles of STA abs before DMA begins. Once the first DMA cycle
is inside the IRQ window, the expected result jumps to column 8.

---

## 2. What our emulator is producing (decoded)

Observed fragment:

```
7 +7         ✓
7 +8         ✓
7 +9         ✓
7 +10        ✓
7 +11        ✗  expected 8
7 +12        ✗  expected 8
7 +13        ✗  expected 8
...          (hidden by the ROM's "..." elision)
8 +524       ✓
8 +525       ✓
9 +526       ✗  expected 8
9 +527       ✓
```

The two symptoms:

1. **Column 7 is too wide**. Visible: column 7 extends at least through
   `dly=13`. Given `dly=524` already reports column 8, the real column-7
   window probably extends ≈ `dly=7..~523` — i.e. **the entire DMA is
   being counted as part of STA's IRQ poll window**, when expected is
   just the 4 STA cycles before DMA.

2. **Column 9 is one cycle early**. Column 9 starts at `dly=526` in our
   output versus `dly=527` in the expected table. This is a secondary
   artefact: if column 8 spans the 2-cycle post-DMA NOP#3, then column
   9's onset depends on whether DMA took 513 or 514 cycles. A
   single-cycle shift in DMA length (or a parity-check off-by-one)
   moves every downstream column boundary by one.

Both symptoms have the same root cause class: **IRQ is being polled by
the wrong instruction across the DMA boundary.**

---

## 3. Reference behaviour

### 3a. Mesen2 (`Core/NES/NesCpu.cpp`)

- STA abs to `$4014` only sets `_needHalt=true; _spriteDmaTransfer=true`
  (`NesPpu.cpp:505 → RunDMATransfer`). **No DMA cycles run inside the
  write.**
- The DMA is materialised on the *next* `MemoryRead` via
  `ProcessPendingDma(readAddress, opType)` (`NesCpu.cpp:325-448`). That
  function runs the halt + dummy + loop of 256 read/write pairs, each
  ticking `StartCpuCycle` / `EndCpuCycle`.
- Interrupt latch semantics (`NesCpu.cpp:294-315, NesCpu.h:55-56`):
  - `EndCpuCycle` runs at the end of *every* CPU cycle, including DMA
    stall cycles.
  - `_runIrq` is set from the current interrupt line + I-flag.
  - `_prevRunIrq` is set to the *previous* `_runIrq`. This is the
    "end-of-penultimate-cycle" snapshot.
- Dispatch (`NesCpu.cpp:167-181`):
  ```
  (this->*_opTable[opCode])();              // run op handler (STA's write sets _needHalt)
  if(_prevRunIrq || _prevNeedNmi) { IRQ(); }
  ```
  The check fires **before** the next opcode fetch runs
  `ProcessPendingDma`, so the DMA stall cycles have not yet ticked
  `_prevRunIrq`. The value checked is the snapshot taken at the end of
  the *last CPU cycle of STA itself*, i.e. STA's write cycle — which
  in turn carries the `_runIrq` value that was latched one cycle
  earlier, at STA's penultimate (operand-high fetch). **That is the
  exact "end of penultimate" moment Mesen2 wants.**
- `IRQ()` calls `DummyRead()` twice, each of which triggers
  `ProcessPendingDma`, so the DMA actually runs during IRQ's first two
  dummy cycles — but the pushed PC is already STA's return address
  (landing+7). Return column = 7.
- If `_prevRunIrq` was false after STA, dispatch falls through to the
  next `Exec()`. `GetOPCode()` calls `MemoryRead(PC)` →
  `ProcessPendingDma` → DMA runs all 513/514 cycles → opcode fetch →
  `Exec` continues into NOP#3. At NOP#3's `_prevRunIrq` check
  (end-of-penultimate = end of NOP#3's opcode fetch cycle), the IRQ has
  been asserted for >=1 cycle; IRQ fires after NOP#3. Pushed PC =
  landing+8. Return column = 8.

**Summary**: Mesen samples IRQ at the end of the STA write cycle (which
equals end-of-penultimate for a post-access tick model), *before* DMA
runs. IRQs newly asserted during DMA are picked up by the next
instruction.

### 3b. puNES (`src/core/cpu_inline.h:1337-1408`)

puNES runs DMA inside the `$4014` write handler (like we do), but adds
two explicit `irq.delay` bookkeeping flags:

```c
// BEFORE DMA:
if (nes[nidx].c.irq.high && !nes[nidx].c.cpu.cycles && !nes[nidx].c.irq.before) {
    nes[nidx].c.irq.delay = TRUE;
}
...
BYTE save_irq        = nes[nidx].c.irq.high;
BYTE save_cpu_cycles = nes[nidx].c.cpu.cycles;
// DMA loop (513 + 256×2 cycles, with DMC racing at indices 253/254/255) ...
// AFTER DMA:
if (nes[nidx].c.irq.high && !(save_irq | save_cpu_cycles)) {
    nes[nidx].c.irq.delay = TRUE;
}
```

`irq.delay` causes the IRQ dispatcher (`cpu.c:528-537`) to skip the
IRQ on the current instruction boundary, so the next instruction runs
before the IRQ is taken. The pre-DMA branch handles an IRQ newly
asserted at the moment STA commits; the post-DMA branch handles an IRQ
newly asserted *during* DMA when no cycles of the source instruction
remained to be consumed. Together they produce the expected pattern:

- IRQ asserted strictly *before* STA's last cycle → recognised at end
  of STA (column 7).
- IRQ newly asserted during STA's last cycle or during DMA → pushed to
  the next instruction (column 8).

The puNES 4-way DMC taxonomy (`src/core/apu.h:25`,
`cpu_inline.h:1374-1398`) is orthogonal to the IRQ question — it only
affects the DMC stall length when DMC DMA races the tail of OAM DMA
(indices 253/254/255). For 4-irq_and_dma the DMC channel is not armed,
so this taxonomy does not apply. It *does* apply to
`dmc_dma_during_read4` / `sprdma_and_dmc_dma`.

---

## 4. Our implementation walk-through

`bus.rs::write` for `$4014` (lines 134-144):

```rust
0x4014 => {
    self.tick_post_access();          // cycle 3 of STA write ends
    let extra_idle = (self.clock.cpu_cycles() & 1) != 0;
    self.run_oam_dma(data, extra_idle);
    return;
}
```

`tick_pre_access` already ran before the match, for cycle 3 (the write
cycle). That call recorded:

```rust
self.prev_irq_line = self.irq_line;   // ← IRQ state at END of cycle 2 (STA's penult)
// ...
self.irq_line = self.apu.irq_line() | self.mapper.irq_line();   // refresh for cycle 3
```

So at entry to `0x4014 =>`, `bus.prev_irq_line` **is** the correct
end-of-penultimate snapshot. 

`run_oam_dma` (lines 228-242):

```rust
self.tick_cycle();                // idle alignment cycle
if extra_idle { self.tick_cycle(); } // odd-parity extra idle
for i in 0..=0xFFu16 {
    let byte = self.read(base | i); // 1 pre + 1 post tick
    self.write(0x2004, byte);       // 1 pre + 1 post tick
}
```

Every `tick_cycle`, `read`, and `write` calls `tick_pre_access` at the
top, which does `self.prev_irq_line = self.irq_line`. So each stall
cycle **overwrites** `bus.prev_irq_line` with a fresher IRQ snapshot.

Control returns through `write()` → `ops::execute()` → `Cpu::step`. At
the end of `step`, `poll_interrupts_at_end` reads `bus.prev_irq_line`
(cpu/mod.rs:155-193). By that point the value reflects IRQ state at the
*end of the very last DMA tick*, not STA's penultimate.

**Bug**: our IRQ poll for STA absorbs the entire DMA window, so any IRQ
that asserts during DMA is attributed to STA and produces column 7
instead of column 8.

This matches exactly the 7→8 transition that we get wrong in the
observed output.

### Why the upper boundary is also off by one

Assume the main bug is fixed so the STA poll uses the penult snapshot.
Then column 8 begins at the first cycle where IRQ has asserted by the
time NOP#3 polls at its own penult (the first cycle of NOP#3 = its
opcode fetch). NOP#3's penult-end is the cycle after DMA finishes —
either cycle 522 (even-parity DMA) or cycle 523 (odd-parity DMA).

Our observed column-9 boundary at `dly=526` vs expected `dly=527` is a
1-cycle shift on *that* transition. This is almost certainly DMA
parity / length arithmetic, not a separate bug:

- `run_oam_dma` uses `extra_idle = (cpu_cycles() & 1) != 0` at entry
  to the $4014 match arm. At that moment `cpu_cycles` already includes
  the write cycle just ticked. Parity of the *next* cycle (the first
  DMA idle) = `cpu_cycles & 1`. If the test ROM expects DMA entry
  parity to be computed pre-write (before STA's 4th tick), our result
  flips.
- Nesdev's "513 normal, 514 on odd" is specified relative to "the
  cycle DMA begins", which is the first idle — i.e. the cycle *after*
  STA's write. So our condition is defensible, but the blargg test
  clearly targets a specific parity model and a 1-cycle flip breaks
  it.
- The existing unit tests in `src/bus.rs` (lines 300-332) lock in our
  current interpretation; they may need revisiting if we change the
  parity.

This secondary discrepancy cannot be fully resolved from the visible
output alone — we need instrumentation (dump DMA start parity, the
IRQ-assertion cycle, and NOP#3's poll cycle for a few iterations
around dly=524..527) to decide whether the fix is a parity flip or a
DMA-length adjustment. Flag as **sub-C.2** after the main fix lands.

---

## 5. Why this is *not* caused by other suspects

Running through the hypothesis list in the prompt:

- **IRQ polling disabled during DMC DMA stall**: test 4-irq_and_dma
  doesn't use DMC, so this cannot explain the symptom. It may still be
  relevant for `5-branch_delays_irq` or for future `dmc_*` tests, but
  not here.
- **IRQ polling enabled during DMC DMA stall but wrong edge**: same —
  no DMC traffic.
- **OAM DMA odd-parity off-by-one**: plausibly contributes to the
  **1-cycle** upper-boundary shift, but does not explain the **3+
  cycle** lower-boundary spread.
- **$4016/$4017 double-read bug**: the test reads neither register
  during the landing block, so out of scope.
- **DMC "halt" cycle missing**: test doesn't use DMC.
- **phase-6 APU reorder causing double-ticking**: confirmed not the
  case. `tick_cycle` = `tick_pre_access` + `tick_post_access`, and APU
  ticks exactly once in `tick_pre_access`. No double tick. The only
  ordering effect is that IRQ-line refresh now happens in pre-access,
  so `prev_irq_line` is set from the state *as seen just before* this
  cycle's bus access, matching Mesen's `_prevRunIrq` semantics. The
  phase-6 reorder is *why* our snapshot is correct at STA's penult
  entry — it is not the bug.

---

## 6. Concrete fix plan

### Main fix (sub-C.1): snapshot + restore IRQ poll across DMA

**File**: `src/bus.rs`
**Function**: the `$4014` write arm of `Bus::write`.

**Current**:

```rust
0x4014 => {
    self.tick_post_access();
    let extra_idle = (self.clock.cpu_cycles() & 1) != 0;
    self.run_oam_dma(data, extra_idle);
    return;
}
```

**Proposed**:

```rust
0x4014 => {
    self.tick_post_access();
    // Snapshot the STA's end-of-penult interrupt poll *before* DMA
    // ticks advance `prev_*`. Restored at the end so the CPU's
    // `poll_interrupts_at_end` sees STA's natural poll window — not
    // the aggregate of the 513/514 DMA stall cycles. Matches the
    // Mesen2 model where `_prevRunIrq` is evaluated at the top of
    // the next `Exec()`, before `ProcessPendingDma` runs.
    let saved_prev_irq  = self.prev_irq_line;
    let saved_prev_nmi  = self.prev_nmi_pending;
    let extra_idle = (self.clock.cpu_cycles() & 1) != 0;
    self.run_oam_dma(data, extra_idle);
    self.prev_irq_line   = saved_prev_irq;
    self.prev_nmi_pending = saved_prev_nmi;
    return;
}
```

This is the minimal, surgical fix. It preserves all other DMA
timing/PPU/APU behaviour and changes only the CPU-visible interrupt
poll snapshot.

**Risk note**: NOPs AFTER the STA will see a `prev_irq_line` that
starts fresh from the STA-era snapshot and then gets advanced by
their own `tick_pre_access`. Specifically, NOP#3's opcode-fetch
`tick_pre_access` sets `prev_irq_line = self.irq_line` — and
`self.irq_line` is the live value (unchanged by our restore). So
NOP#3's penult poll correctly sees "IRQ has been asserted for N
cycles by now". This is what we want.

**Expected post-fix trace** (dly=7..13):

```
dly   IRQ cycle (into landing)   STA's prev_irq_line at end-of-STA   column
7     STA cycle 0 (6)            true  (asserted before STA started)  7
8     STA cycle 1 (7)            true                                 7
9     STA cycle 2 (8, penult)    true                                 7
10    STA cycle 3 (9, last)      false (latch rolled to new cycle)    → picked up by NOP#3, col 8
```

Hmm — the 4th column-7 row (`dly=10`) means IRQ asserted at STA's
*last* cycle (cycle 3 of STA) must still produce column 7. That only
works if `prev_irq_line` at end of cycle 3 reflects IRQ state from end
of cycle 2. Which it does: `tick_pre_access` for cycle 3 samples state
at end of cycle 2 into `prev_irq_line`. So "IRQ asserted exactly at
end of cycle 3" was *not yet visible in prev_irq_line*, but IRQ
asserted at cycle 3 means end-of-cycle-3's `irq_line` is true.

After our restore, STA's `poll_interrupts_at_end` reads
`prev_irq_line` = state at end of cycle 2. At IRQ cycle 3, state at
end of cycle 2 is *still false* (IRQ asserts one cycle in the future).
So STA does NOT take the IRQ → next insn NOP#3 takes it → column 8.

But expected says dly=10 → column 7. So the "end-of-penult" snapshot
at end of cycle 2 should somehow be true when IRQ asserts at cycle 3.
Contradiction.

Recheck: what does `dly=10` actually correspond to in IRQ cycles? The
mapping depends on a constant offset C from the CLI frame. From the
table:

- dly=0 → column 0 → IRQ hits before NOP#1 polls (at end of `end:`
  routine's last instruction). Say IRQ cycle = -1 or 0 relative to
  landing.
- dly=1 → column 1 → IRQ at landing cycle 0 (end of NOP#1's penult).
- dly=2 → column 1 → IRQ at landing cycle 1 (NOP#1's last). Polled by
  NOP#2's penult (cycle 2). Taken after NOP#2 → column 2? But expected
  is column 1.

Reading more carefully: "column 1 at dly=2" means the IRQ was taken
after NOP#1. For that to happen, IRQ must be asserted at end of cycle
0 (NOP#1's penult) or earlier. So `dly=2 → IRQ at landing cycle 0` —
i.e. the dly-to-IRQ-cycle mapping is `IRQ cycle = dly - 2`. dly=0
→ IRQ at cycle -2 (before landing), dly=7 → IRQ cycle 5, dly=10 → IRQ
cycle 8 (STA's penult end = end of cycle 2 of STA = landing cycle 8).

With that mapping:

```
dly   IRQ landing cycle   instruction at IRQ        column expected
7     5                   LDA #$07 (cycles 4,5)     7 (LDA ran, post-LDA poll misses; STA's penult takes it)
8     6                   STA cycle 0               7
9     7                   STA cycle 1               7
10    8                   STA cycle 2 (penult)      7
11    9                   STA cycle 3 (last)        8 (too late for STA; NOP#3 takes it)
...
```

So dly=10 → IRQ at landing cycle 8 (STA's penult-end). STA's
`prev_irq_line` at end of STA would reflect end-of-cycle-2 snapshot,
which = end of STA's penult = the moment IRQ asserts. `prev_irq_line`
= false one cycle earlier, true at end of penult. Our `tick_pre_access`
for cycle 3 writes `prev_irq_line = irq_line` where `irq_line` is the
live value from the *previous* tick_pre_access's refresh (end of cycle
2). If IRQ asserted at end of cycle 2, `self.irq_line` was set to true
inside cycle 2's tick_pre_access → cycle 3's tick_pre_access copies
that into prev_irq_line → **true**. ✓

So after the snapshot-restore fix:

- dly=10 (IRQ at cycle 8 = STA penult end): STA sees `prev_irq_line=true`,
  takes IRQ, column 7. ✓
- dly=11 (IRQ at cycle 9 = STA last): STA's prev_irq_line is still the
  value at end of cycle 2 = false (no IRQ there yet). STA does NOT take
  IRQ. NOP#3 runs. NOP#3's penult-end (cycle 523 or 524) sees IRQ
  asserted long ago → takes it → column 8. ✓

The fix produces exactly the expected boundary.

### Secondary fix (sub-C.2): upper-boundary 1-cycle shift

Only tackle after sub-C.1 lands and passes the lower boundary. Steps:

1. Add debug prints in `run_oam_dma` for the first few test
   iterations: DMA entry cycle, parity decision, total DMA length,
   NOP#3's penult cycle.
2. Cross-check `dly=524..527` cycle-by-cycle against Mesen2's debugger
   (`mesen --debugger` on the same ROM → run to frame counter IRQ
   assertion, single-step to see when `_prevRunIrq` latches).
3. The likely fix is one of:
   - Compute `extra_idle` *before* `tick_post_access` (use parity of
     cycle BEFORE STA's write commits).
   - Change the DMA idle count model from "1 + 1-if-odd" to a more
     precise "align to next get-cycle" logic (Mesen2-style: the DMA
     loop decides per-iteration whether it's on a get or put cycle).
4. Update the two `oam_dma_*_parity_is_*_cycles` unit tests in
   `src/bus.rs` to match the corrected model. Current tests assert
   513/514 *ticks in our run_oam_dma* = we may discover the real model
   is off by one relative to nesdev's "513/514 from DMA start".

Do not combine sub-C.2 with sub-C.1 in the same commit; the clean
column-7 fix is independently verifiable (no-regression sweep + a fresh
snapshot of `4-irq_and_dma` output showing column 7 capped at dly=10).

---

## 7. Files / surfaces touched

Sub-C.1 (the primary fix):

- `src/bus.rs` — `$4014` write arm of `Bus::write` (lines 134-144).
  ~6 new lines (snapshot + restore + comment). No other files change.

Sub-C.2 (secondary, if needed):

- `src/bus.rs` — `$4014` write arm + `run_oam_dma` + the two
  `oam_dma_*_parity_*` unit tests (lines 300-332).

CPU core changes are **not** needed. DMC channel changes are **not**
needed for this test.

---

## 8. Regression surface (re-run before committing)

Per `CLAUDE.md`'s "do-before-starting checklist":

| Test suite                                                            | Risk after sub-C.1 | Why                                                   |
|-----------------------------------------------------------------------|--------------------|-------------------------------------------------------|
| `instr_test-v5/official_only.nes`                                     | None               | No DMA in regression; IRQ poll unchanged otherwise.   |
| `instr_misc/instr_misc.nes`                                           | None               | Same.                                                 |
| `apu_test/rom_singles/*.nes`                                          | None               | None exercise $4014 + IRQ overlap.                    |
| `apu_reset/*.nes`                                                     | None               | Reset path untouched.                                 |
| `cpu_interrupts_v2/rom_singles/1-cli_latency.nes`                     | None               | No $4014.                                             |
| `cpu_interrupts_v2/rom_singles/2-nmi_and_brk.nes`                     | None               | No $4014.                                             |
| `cpu_interrupts_v2/rom_singles/3-nmi_and_irq.nes`                     | None               | No $4014. (Still failing for unrelated reasons per `CLAUDE.md` Phase 5 §2.) |
| `cpu_interrupts_v2/rom_singles/4-irq_and_dma.nes`                     | **Target**         | Expected to go from FAIL to PASS (or get much closer).|
| `cpu_interrupts_v2/rom_singles/5-branch_delays_irq.nes`               | None               | Unrelated path.                                       |
| `dmc_dma_during_read4/*.nes`                                          | Low                | Uses DMC DMA, not OAM. No code path shared by the fix.|
| `sprdma_and_dmc_dma/*.nes`                                            | **Medium**         | Mixes OAM + DMC. The NMI-variant test `4-nmi_and_dma` also benefits — verify it doesn't regress (it currently fails or is untested; re-check after fix). |

Also re-run the two unit tests in `src/bus.rs`:

- `oam_dma_even_parity_is_513_cycles` — should remain 514 ticks total.
- `oam_dma_odd_parity_is_514_cycles` — should remain 515 ticks total.

Both assertions depend only on `cpu_cycles` arithmetic and are
independent of the `prev_irq_line` snapshot, so sub-C.1 should not
touch them.

### Other ROMs that run OAM DMA heavily

Any commercial game does 1 OAM DMA per vblank; a regression would
manifest as false IRQ latching immediately after every vblank on
IRQ-using mappers (MMC3, FME-7, VRC IRQ). None of these currently ship
as test ROMs in our harness, so the acceptance gate is `4-irq_and_dma`
passing + no regression on the table above.

---

## 9. Open questions / follow-ups

1. **Secondary 1-cycle shift on column 9**. Investigate after the
   primary fix lands (instrument dly=524..527 iterations). Likely a
   parity check order-of-operations, not a logic error.

2. **`4-nmi_and_dma`** (the sibling NMI test). Same scaffolding, same
   expected pattern on the NMI side. The same bug almost certainly
   applies: `prev_nmi_pending` is advanced by DMA stall cycles, so STA
   SPRDMA's NMI poll absorbs NMI edges that occurred during DMA. The
   snapshot-restore fix covers both by saving/restoring
   `prev_nmi_pending` alongside `prev_irq_line` (as shown in the
   sub-C.1 diff). Verify the NMI variant separately after the IRQ
   variant passes.

3. **Mesen2 ordering alternative**. A cleaner long-term refactor would
   be to mirror Mesen's "DMA runs lazily inside the next MemoryRead"
   model: the `$4014` handler just sets a flag; the next `bus.read`
   (or the next instruction's opcode fetch) consumes it. This matches
   Mesen's interrupt-timing guarantees by construction (no snapshot
   gymnastics needed) and naturally integrates with DMC DMA's existing
   `service_pending_dmc_dma` entry point. Defer to Phase 7 if we find
   other edge cases the snapshot-restore approach cannot handle.

4. **puNES's 4-way DMC stall taxonomy**. Our `service_pending_dmc_dma`
   uses a flat 4-cycle stall, which is the `DMC_NORMAL` case. The
   other three variants (`DMC_CPU_WRITE`, `DMC_R4014`, `DMC_NNL_DMA`)
   apply when DMC DMA races the tail of OAM DMA. Not exercised by
   `4-irq_and_dma`, but required by `sprdma_and_dmc_dma`. Track as a
   separate task.

---

## 10. TL;DR

- **Bug**: `Bus::write`'s `$4014` arm runs 513/514 DMA cycles inside
  the STA write. Every stall cycle's `tick_pre_access` overwrites
  `bus.prev_irq_line`. By the time STA's `poll_interrupts_at_end`
  runs, the IRQ-line snapshot reflects the end of DMA, not STA's
  penultimate cycle. Any IRQ asserted during DMA is mis-attributed to
  STA, yielding column 7 where the expected is column 8.
- **Fix** (sub-C.1): snapshot `bus.prev_irq_line` and
  `bus.prev_nmi_pending` at entry to the `$4014` handler (after
  `tick_post_access`, before `run_oam_dma`), and restore them after
  DMA. Minimal, surgical, ≈6 lines in `src/bus.rs`, no CPU or DMC
  changes.
- **Secondary**: a 1-cycle shift on the column-8→9 boundary remains
  after sub-C.1 and is tracked as sub-C.2 (likely parity
  order-of-operations in `run_oam_dma`).
- **Regression risk**: low. OAM DMA timing is unchanged; only the CPU's
  view of the interrupt poll snapshot is corrected. Full apu_test /
  apu_reset / instr_test / cpu_interrupts_v2 tests 1-3 should all
  remain PASS.
