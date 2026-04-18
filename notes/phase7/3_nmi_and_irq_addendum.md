# 3-nmi_and_irq addendum — reference-emulator PPU/CPU tick split

**Scope.** Read-only study of Mesen2, puNES, and nestopia to answer the
specific question: *why does our `3-nmi_and_irq.nes` output lag real
hardware by exactly one iteration?*

This addendum supersedes the "suspects: (a) APU frame-counter IRQ
assertion timing (b) CLI delay interaction" list in the prior notes.
Neither matches the evidence. The culprit is the **pre/post-access PPU
tick split** in `bus.rs`.

No source files were modified.

---

## 0. The test framework — clearing up how the 1-cycle resolution works

The prompt asked whether each iteration advances NMI arrival by 1 CPU
cycle or by 25. Read the ROM once more (`cpu_interrupts_v2/source/3-nmi_and_irq.s:46-56`
and `common/delay.s:40-55`):

```
eor #$FF        ; A = ~N
clc
adc #1+12       ; A = (~N) + 13 = 12 - N  (for N in 0..=11)
jsr sync_vbl
jsr delay_a_25_clocks   ; delays A + 25 cycles
```

`delay_a_25_clocks` (delay.s:42-55, comment "Time: A+25 clocks
(including JSR)") is **linear in A, 1-cycle granularity**. Iteration N
puts `A = 12 - N`, so iter N delays `(12-N) + 25 = 37-N` CPU cycles.
Each successive iteration delays exactly 1 CPU cycle LESS.

Net effect: after sync_vbl exits (locked to PPU VBL-set within 1/3 CPU
cycle), each iteration pushes the critical-section start 1 CPU cycle
EARLIER. Since NMI arrival is anchored to PPU VBL-set (a fixed PPU
time), **NMI appears to arrive 1 CPU cycle LATER in the critical
section per iteration**.

Hence the iter-boundary of the expected table:
  - iter 0 (37-cycle delay): NMI lands before `lda #1`  → `$23`
  - iter 1-2 (36, 35): NMI lands between `lda #1` and `clc`  → `$21`
  - iter 3-9: NMI after `clc`, before IRQ-at-nop  → `$20`
  - iter 10-11: IRQ first, NMI during IRQ handler's SEC  → `$25 / $20`

The 1-row→1-value rows (iter 0, iters 1-2, iters 10-11) correspond to
the 1-cycle-wide windows around 2-cycle instruction boundaries; the
7-row run of `$20` corresponds to the 7 CPU cycles between CLC
completion and IRQ-at-nop firing. This matches hardware exactly.

---

## 1. Mesen2 — NMI edge detection and recognition

Files: `~/Git/Mesen2/Core/NES/NesCpu.cpp`, `NesCpu.h`, `NesPpu.cpp`,
`NesPpu.h`.

### 1.1 PPU sets the flag

`NesPpu.cpp:1331-1355` — `Exec()` increments `_cycle` at the start of
each call; if the new `(scanline, cycle)` is `(_nmiScanline, 1)`
(i.e. `(241, 1)` NTSC), it runs:

```cpp
_statusFlags.VerticalBlank = true;
BeginVBlank();   // -> TriggerNmi()
```

`TriggerNmi()` at line 1254-1258: if `_control.NmiOnVerticalBlank`,
calls `_console->GetCpu()->SetNmiFlag()` which sets
`_state.NmiFlag = true` (NesCpu.h:812). This is the PPU→CPU NMI line
assertion.

### 1.2 CPU samples the edge at end-of-cycle

`NesCpu.cpp:294-315` — `EndCpuCycle()` (called after every bus access)
runs in this order:

1. Advance master clock + `ppu->Run(_masterClock - _ppuOffset)`.
2. `_prevNeedNmi = _needNmi;`     (snapshot from PREVIOUS sample)
3. Edge detect: `if (!_prevNmiFlag && _state.NmiFlag) _needNmi = true;`
4. `_prevNmiFlag = _state.NmiFlag;`
5. Refresh `_runIrq` from `_state.IrqFlag & _irqMask`.

So within one CPU cycle `N`:
  - PPU dots tick (between StartCpuCycle and the end of EndCpuCycle).
  - If any dot was `(241, 1)`, `NmiFlag` flipped 0→1 during that run.
  - At end of cycle, `_needNmi` becomes true via the edge detector.
  - `_prevNeedNmi` captures the OLD `_needNmi` — still false.

Cycle N+1's EndCpuCycle then has `_prevNeedNmi = true`. The opcode
dispatch loop (line 178) checks `if (_prevRunIrq || _prevNeedNmi)
IRQ();` AFTER an instruction completes, so NMI service begins on
whichever instruction finishes first once `_prevNeedNmi` is true.

The comments at 299-305 match nesdev's edge-detector wording verbatim
("internal signal goes high during φ1 of the cycle that follows the
one where the edge is detected"). This is the canonical model.

### 1.3 Mesen's CPU cycle is split into Start / End around the bus access

`NesCpu.cpp:241-268, 294-323` — every `MemoryRead` / `MemoryWrite`
calls `StartCpuCycle(bool forRead)` before the access and
`EndCpuCycle(bool forRead)` after.

NTSC constants (`NesCpu.cpp:73-75, 122-125`): `_startClockCount = 6`,
`_endClockCount = 6`, `cpuDivider = 12`, `ppuDivider = 4`,
`_ppuOffset = 1` (deterministic when `RandomizeCpuPpuAlignment` off).

Read-path clock math (NesCpu.cpp:296, 319):
  - StartCpuCycle(true):  `_masterClock += _startClockCount - 1 = 5`
  - EndCpuCycle(true):    `_masterClock += _endClockCount + 1   = 7`

Write-path:
  - StartCpuCycle(false): `+ _startClockCount + 1 = 7`
  - EndCpuCycle(false):   `+ _endClockCount - 1   = 5`

After each half-cycle, `ppu->Run(_masterClock - 1)` drains dots. PPU
Run loop (NesPpu.h:141-148) does `Exec(); mc += 4` inside a
`do..while(mc + 4 <= runTo)`.

**Steady-state dot split (NTSC, reads), worked out for cycle N ≥ 2**:

Let `mc` be the master clock at the start of cycle N's StartCpuCycle
(mc = 12·(N-1) + 12 initial = 12N baseline in our indexing).

| phase             | mc after |   target = mc - 1 | dots done = target/4 | new dots |
|-------------------|---------:|------------------:|---------------------:|---------:|
| Start of cycle N  |   12N+5  |            12N+4  |         3N+1         |   +2     |
| End of cycle N    |   12N+12 |            12N+11 |         3N+2         |   +1     |

(Using integer division; e.g. (12·3+4)/4 = 40/4 = 10 = 3·3+1 ✓;
(12·3+11)/4 = 47/4 = 11 = 3·3+2 ✓.)

**Mesen's read split: 2 PPU dots run BEFORE the bus access, 1 dot
runs AFTER.**

Writes give the same 2-before / 1-after split (the start-offset is
different but the integer-divided dot counts land identically).

### 1.4 Total "(241, 1) → NMI dispatch" latency in Mesen

Suppose PPU dot 1 of scanline 241 falls in CPU cycle `M`. There are
three sub-cases depending on which of the 3 dots in cycle M contains
the VBL-setting dot:

| VBL dot position in cycle M | status bit 7 at cycle M's bus read | NmiFlag set by EndCycle M? | `_needNmi` true at EndCycle | NMI dispatched after instruction ending at cycle |
|---|---|---|---|---|
| 1st of 3 (pre-access)  | set     | yes | N=M     | first op finishing at ≥ M+1 |
| 2nd of 3 (pre-access)  | set     | yes | N=M     | first op finishing at ≥ M+1 |
| 3rd of 3 (post-access) | NOT set | yes | N=M     | first op finishing at ≥ M+1 |

In all three cases the edge detector fires at the end of cycle M
(because `NmiFlag` did go 0→1 somewhere within cycle M's PPU run).
Dispatch timing is the same in all three cases.

**But** the value read from `$2002` at cycle M differs across
sub-cases — position 3 is the one where the read SEES VBL clear.
This is what sync_vbl's 27-cycle fine-sync loop uses to pin itself to
a specific sub-dot; it exits just before the cycle where `bit $2002`
reads bit 7 = 1.

---

## 2. puNES — same model as Mesen conceptually, different mechanical split

Files: `~/Git/puNES/src/core/cpu_inline.h`, `cpu.c`, `ppu.c`,
`ppu_inline.h`.

### 2.1 `tick_hw` lays down all 3 PPU dots BEFORE the access

`cpu_inline.h:2163-2213` — `tick_hw(nidx, 1)` (called once per CPU
cycle) does:

1. `cpu.opcode_cycle++`
2. **`nmi.before = nmi.high; irq.before = irq.high;`** (snapshot first)
3. `ppu_tick(nidx)` — which, in `ppu.c:192-196`, accumulates
   `machine.cpu_divide` master ticks and drains
   `while (cycles >= ppu_divide)` — i.e. **all 3 PPU dots for this CPU
   cycle run inside `ppu_tick`**.
4. `apu_tick` (NTSC), mapper hooks, etc.

`cpu_rd_mem` (`cpu_inline.h:118-236`) calls `tick_hw(nidx, 1)` **before**
the actual access for RAM (line 156), PPU register reads (line 173),
PRG-ROM (line 131), WRAM (line 217). For APU reads (line 179-184) —
`$4015` specifically — the order is reversed: access then
`tick_hw(1)` (see the comment "eseguo un tick hardware ed e'
importante che sia fatto dopo"). That's an independent special case
for the frame-counter IRQ clear-on-read race.

### 2.2 NMI edge detector

`cpu.c:118-123` (the `_IRQ_NMI_CHECK` macro, called inside tick_hw's
inline path):

```c
if (nmi.high && !nmi.before) {
    nmi.delay = TRUE;
}
```

`nmi.before = nmi.high` was snapshotted at line 2170 *before*
`ppu_tick`. So if `ppu_tick` raises `nmi.high` this cycle,
`nmi.before` is still the old value → the check sets
`nmi.delay = TRUE` → the opcode dispatcher honors it after the
current instruction.

### 2.3 Where VBL is actually set

`ppu.c:934-948` — at the end-of-scanline / frame-wrap code, when
`frame_y >= total` wraps, `r2002.vblank = 0x80` and (if
`r2000.nmi_enable`) `nmi.high = TRUE`. This wrap happens at the end
of scanline 260 → start of scanline 261, which in puNES's accounting
is "start of frame for frame_y=0" — their VBL layout differs from
Mesen's (puNES uses `vint` as the pre-render line, not 241), but the
*relative* CPU/PPU phase is the same because the same number of
master ticks have elapsed.

### 2.4 Total latency, puNES

Same as Mesen: edge detected at end of CPU cycle M (wherever the VBL
dot falls among cycle M's 3 PPU dots). Dispatched at end of the next
complete instruction.

**Key difference from Mesen's split**: puNES runs ALL 3 dots before
the bus access. That makes `$2002` reads see VBL even if the VBL dot
is the "3rd-of-3" dot in cycle M. Consequently, `sync_vbl` in puNES
exits on cycle M; in Mesen it exits on cycle M+1 for the 3rd-of-3
case.

So **puNES matches our current model, not Mesen's**. Both emulators
pass blargg CPU-interrupt tests in practice (puNES is tracked against
the same test-ROM suite), which means blargg's sync_vbl loop and
cycle-counting tolerates BOTH splits — **as long as the rest of the
emulator is internally consistent**. The question is whether *our*
emulator is internally consistent.

### 2.5 `nmi_plus_2` — not a thing

Grepped `~/Git/puNES/src/core` exhaustively. There is no `nmi_plus_2`
symbol. The fields on `nes[nidx].c.nmi` are `high`, `delay`, `before`,
`frame_x`, `cpu_cycles_from_last_nmi`. There's no 2-cycle delayed-NMI
mechanism separate from the standard edge-detector `.delay` path. The
question in the prompt was based on a misremembered detail; ignore it.

---

## 3. nestopia — schedule-driven, 1.5-cycle edge delay

Files: `~/Git/nestopia/source/core/NstCpu.cpp`, `NstPpu.cpp`, `NstBase.hpp`.

### 3.1 PPU VBL is 3 staged hclocks

`NstPpu.cpp:2600-2631` + `NstPpu.hpp:143-145`:

```
HCLOCK_VBLANK_0 = 681   → sets STATUS_VBLANKING
HCLOCK_VBLANK_1 = 682   → promotes VBLANKING bit to STATUS_VBLANK
HCLOCK_VBLANK_2 = 684   → if CTRL0_NMI & status, cpu.DoNMI(cpu.GetFrameCycles())
```

(`PPU_RP2C02_CC = 4` master ticks per dot — so these are master-tick
counts within a scanline.) The 3-step staging models the ~1 dot
between "VBL flag bit visible" and "NMI line assert" documented on
nesdev.

### 3.2 CPU schedules NMI dispatch

`NstCpu.cpp:1888-1895`:

```cpp
void Cpu::DoNMI(const Cycle cycle) {
    if (interrupt.nmiClock == CYCLE_MAX) {
        interrupt.nmiClock = cycle + cycles.InterruptEdge();
        cycles.NextRound(interrupt.nmiClock);
    }
}
```

`InterruptEdge()` at line 330-333: `return clock[0] + clock[0]/2;` —
i.e. **1.5 CPU cycles**. NMI dispatch happens when `cycles.count`
catches up to `nmiClock`.

### 3.3 NMI hijack on IRQ vector fetch

`NstCpu.cpp:1840-1858` — during IRQ vector fetch, if
`interrupt.nmiClock != CYCLE_MAX` and the NMI's scheduled dispatch
point is ≤ the current cycle, the vector is redirected to `NMI_VECTOR`.
Otherwise `nmiClock = cycles.count + 1` so it fires on the very next
cycle. This is nestopia's analog of Mesen's `_prevNeedNmi = false` at
end of BRK.

### 3.4 Total latency, nestopia

Scheduled `cycle + 1.5 cpu cycles`, snapped by `NextRound` to a CPU
boundary. Equivalent to Mesen's "end of cycle M, dispatch on next
instruction boundary" but computed via an absolute-clock schedule
instead of an edge-detector flag.

---

## 4. Absolute-cycle math for `3-nmi_and_irq` iterations

We don't need the absolute cycle at which each iteration's NMI fires;
we only need the cycle DIFFERENCE between consecutive iterations.

From §0: each iteration shortens the pre-critical-section delay by 1
CPU cycle. Iteration 0 is the "earliest NMI" row (lands at the
tightest alignment before `lda #1` executes). Iteration N shifts NMI
arrival 1 CPU cycle LATER in the critical section than iter N-1.

Critical section disassembly and cycle offsets (relative to last
instruction's last cycle; numbers from the ROM source):

```
 0: cli               (2)  ; I cleared *after* next op
 2: lda #0            (2)  ; A=0 → Z=1
 5: sta nmi_flag      (3)
 8: sta irq_flag      (3)
11: clv               (2)
13: sec               (2)  ; C=1
15: lda #1            (2)  ; A=1 → Z=0, C unchanged
17: clc               (2)  ; C=0
19: nop               (2)  ; IRQ fires here
```

The "NMI observation window" — between SEC completing (cycle 15) and
NOP starting (cycle 19) — is **4 CPU cycles wide**: cycles 15, 16,
17, 18.

- NMI arriving in cycles 12..14 (during SEC or earlier): fires before
  `lda #1`. P has Z=1, C=1 → `$23`.
- NMI arriving in cycles 15..16 (between SEC completion and CLC
  start, i.e. during `lda #1`): fires after `lda #1`. P has Z=0, C=1
  → `$21`. This is the 2-row window the test shows.
- NMI arriving in cycles 17..18 (during CLC/NOP window before IRQ):
  fires after `clc`. P has Z=0, C=0 → `$20`.
- NMI arriving after cycle 19 (after IRQ has started): `$25/$20` case.

The expected table's **1 row of `$23`** comes from a 1-cycle-wide
window somewhere around cycle 14. The **2 rows of `$21`** comes from
a 2-cycle-wide window. Each row = 1-cycle shift per iteration. The
counts confirm 1-cycle granularity.

Our output pattern (`$21, $21, $20, $20×6, $25, $25, $25`) corresponds
to the boundaries being shifted LATER by exactly 1 CPU cycle:

| iter | expected NMI-fire cycle (relative) | our actual NMI-fire cycle | shift |
|-----:|-----------------------------------:|--------------------------:|------:|
|    0 |                                 13 |                        14 |   +1  |
|    1 |                                 14 |                        15 |   +1  |
|    2 |                                 15 |                        16 |   +1  |
|  ... |                                ... |                       ... |   +1  |
|   10 |                                 23 |                        24 |   +1  |
|   11 |                                 24 |                        25 |   +1  |

Our NMI-fire cycle is consistently **1 CPU cycle LATER** than real
hardware / Mesen2 / puNES.

Equivalent framings:
- sync_vbl exits 1 CPU cycle EARLIER in ours.
- Our PPU VBL-dot latch (at `scanline 241 dot 1`) is recognized by
  our CPU 1 cycle LATER than reference emulators would recognize it
  given the same sync_vbl-exit alignment.

---

## 5. Why our emulator is consistently 1 cycle off — root cause

### 5.1 Our current tick model (bus.rs:199-242)

```
tick_pre_access():
    prev_irq_line = irq_line
    prev_nmi_pending = nmi_pending
    ppu.begin_cpu_cycle()
    for _ in 0..3: ppu.tick(mapper)       // ALL 3 dots
    if ppu.poll_nmi(): nmi_pending = true
    apu.tick_cpu_cycle()
    mapper.on_cpu_cycle()
    irq_line = apu.irq_line | mapper.irq_line

<bus access happens here>

tick_post_access():
    audio sink
```

So our split is **3 dots pre-access, 0 dots post-access**. This
matches puNES's `tick_hw → ppu_tick → access` order. Differs from
Mesen2's `Start → 2 dots → access → End → 1 dot`.

### 5.2 Why this causes a 1-cycle shift in `sync_vbl` exit

`sync_vbl` (common/sync_vbl.s:8-44) spins on `bit $2002 / bpl :-`, 27
cycles per iteration. Across 29780.67-cycle frame periods, each iter
drifts 1/3 CPU cycle in PPU-phase. Eventually one iteration's
`bit $2002` sees bit 7 = 1 on the SECOND `bit $2002` but not the
first; that's the exit point.

Consider the cycle `M` where VBL is set (PPU dot `(241, 1)`) in the
middle of the fine-sync loop. sync_vbl's decision to exit depends on
**which CPU cycle's read first sees bit 7 set**.

- In our emu: CPU cycle M's read at `$2002` sees VBL regardless of
  which of its 3 PPU dots contains `(241,1)`, because all 3 dots
  tick BEFORE the bus access. So the loop exits with the read on
  cycle M seeing bit 7.

- In Mesen2: CPU cycle M's read sees VBL only if `(241,1)` is one
  of the first 2 of the 3 dots; if it's the 3rd (post-access) dot,
  only cycle M+1's read sees it.

For the alignment in which hardware's `(241,1)` lands as the "3rd
dot of cycle M", Mesen's sync_vbl exits with the last `bit $2002`
on **cycle M+1**. Our sync_vbl exits on **cycle M**. Our sync_vbl
is **1 CPU cycle EARLY** for that alignment — exactly the observed
1-cycle shift.

### 5.3 Which alignment hardware actually has

Power-on CPU/PPU alignment is not purely deterministic on real
hardware (varies by cold/warm reset, capacitor charge, etc.). blargg's
test suite, however, uses `sync_vbl_odd` / `sync_vbl_even` variants
in SOME tests to force a specific alignment. `3-nmi_and_irq` uses
plain `sync_vbl` which tolerates any alignment — the 27-cycle
fine-sync loop always converges to the "latest `bit $2002` sees bit
7" position.

The expected-table was generated on real NTSC hardware. So whatever
alignment real hardware settles into, the result is fixed: the NMI
fires on cycle X, and shifting our emulation forward/backward by
exactly 1 cycle shifts every iteration by exactly one row.

**Our current 3-pre/0-post split puts us 1 cycle off that canonical
alignment.** Switching to Mesen's 2-pre/1-post split moves us in the
correct direction — whether it fully fixes this test is the question
in §6.

### 5.4 Why the reset-cycle experiment was inconclusive

The CLAUDE.md note — "Bumping Cpu::reset from 7 cycles to 8 cycles:
zero effect" — makes sense. The reset cycle count contributes an
absolute offset to CPU/PPU phase; sync_vbl's precision loop
eliminates any absolute offset by drifting 1/3 cycle per iteration
until it converges.

What the reset experiment CANNOT adjust is the relative **order**
in which the PPU and CPU transact within a cycle. That's exactly
what the pre/post split controls.

### 5.5 Why APU timing / CLI delay are not the culprit

- **APU frame IRQ timing**: the test leaves the frame counter in
  4-step mode (`sta SNDMODE` with A=0) and clears the flag once via
  `lda SNDCHN`. The IRQ flag assertion timing relative to NMI is not
  1-cycle-shifted by our model — we audited this in prior notes.
  The IRQ column in our output is exactly right (`00` where expected,
  `20` where expected) except for the 1-row shift — same direction
  as the NMI shift, consistent with a single upstream timing bug,
  not a separate APU issue.
- **CLI delay**: our `cli` delay-of-one-instruction behavior is
  confirmed correct by passing `1-cli_latency`. The test's CLI runs
  well before the critical section and is completely settled by
  cycle 0 of the window analyzed in §4. Not the source.

---

## 6. Minimal-surface fix plan

### 6.1 Proposal: move 1 of 3 PPU dots from pre-access to post-access

Current `tick_pre_access` runs all 3 dots; change to **2 dots pre-access,
1 dot post-access**. This matches Mesen2's split for NTSC reads/writes.

Concrete sketch (not applied — notes only):

```rust
// bus.rs tick_pre_access
fn tick_pre_access(&mut self) {
    self.prev_irq_line = self.irq_line;
    self.prev_nmi_pending = self.nmi_pending;

    self.ppu.begin_cpu_cycle();
    let ppu_ticks = self.clock.advance_cpu_cycle();
    // Run all but the last dot.
    let pre_ticks = ppu_ticks.saturating_sub(1);
    for _ in 0..pre_ticks {
        self.ppu.tick(&mut *self.mapper);
    }
    // Note: NMI poll is DEFERRED to tick_post_access so a
    // 3rd-dot VBL-set doesn't leak into the current cycle's
    // `nmi_pending`.

    self.apu.tick_cpu_cycle();
    self.mapper.on_cpu_cycle();
    self.irq_line = self.apu.irq_line() | self.mapper.irq_line();
}

fn tick_post_access(&mut self) {
    // Run the final dot AFTER the bus access.
    self.ppu.tick(&mut *self.mapper);
    if self.ppu.poll_nmi() {
        self.nmi_pending = true;
    }
    if let Some(sink) = self.audio_sink.as_mut() {
        sink.on_cpu_cycle(self.apu.output_sample());
    }
}
```

Required follow-on changes:

1. **$2002 race window.** Currently `vbl_just_set` is set in
   `ppu::tick` at `(241,1)` and cleared at `begin_cpu_cycle`. Under
   the new split, the `(241,1)` dot could run either in pre-access
   (2-dot window) or post-access (1-dot window). The $2002 read
   happens BETWEEN them. For the race-suppression hint to fire on
   the right cycle, we need `vbl_just_set` to be readable in
   `cpu_read` only when `(241,1)` has been ticked — which is
   automatically correct: if the VBL dot ran pre-access, `vbl_just_set
   = true` and the hint fires correctly; if the VBL dot runs
   post-access, `status bit 7` is not yet set when $2002 is read and
   no suppression is needed.

2. **OAM DMA stall cycles.** `tick_cycle` must call both
   pre/post halves in order. Currently it does:
   ```
   fn tick_cycle(&mut self) { self.tick_pre_access(); self.tick_post_access(); }
   ```
   That continues to work — the 3 dots are still all run, just
   split 2+1. No DMA logic change needed.

3. **NMI snapshot timing.** `prev_nmi_pending` is captured at the
   start of `tick_pre_access`. Under the new model, `nmi_pending`
   can be set in `tick_post_access`, so at the start of cycle N+1
   the snapshot sees any NMI asserted during cycle N's post-access.
   This matches real hardware's "edge detected at end of cycle N,
   recognized by cycle N+1's penultimate poll" — same timing as
   our current behavior for cycles where VBL lands in the first 2
   dots, but now correctly DEFERS one cycle when VBL is the 3rd
   dot. This is exactly the desired fix.

4. **APU tick position.** Phase 6 moved APU tick to pre-access for
   `blargg_apu_2005.08.irq_timing` (comment at bus.rs:217-231). The
   APU tick is ORTHOGONAL to where the 3rd PPU dot runs — keep APU
   in pre-access. No change needed, no regression expected for
   apu_test 08.

### 6.2 Expected test outcomes after the fix

Regression risk, by subsystem:

| Test group            | Risk            | Reasoning |
|-----------------------|-----------------|-----------|
| `instr_test-v5`       | **None**        | No PPU timing dependence. |
| `instr_misc`          | **None**        | Same. |
| `apu_test/1..8`       | **None**        | APU tick unchanged. The 08 IRQ-timing fix lives in pre-access and stays there. |
| `apu_reset/*`         | **None**        | APU-only. |
| `cpu_interrupts_v2/1` | **Low**         | Uses sync_vbl + NMI; currently passes. The 1-cycle shift either re-balances correctly (same internal consistency as Mesen, puNES ports of this test all pass) or surfaces a boundary case. Must verify. |
| `cpu_interrupts_v2/2` | **Low**         | Same. The CLC-boundary layout in test 2 has more cycle budget than test 3; likely still passes. Must verify. |
| `cpu_interrupts_v2/3` | **Target**      | Should go from FAIL to PASS. |
| `cpu_interrupts_v2/4` | **Unknown**     | Currently fails for DMC-DMA reasons (Sub-C). Shift is orthogonal but re-test is required — some DMA edge cases are PPU-phase-sensitive (Mesen notes). |
| `cpu_interrupts_v2/5` | **Unknown**     | Currently fails (branch-delays-IRQ, Sub-B). IRQ polling also runs through the same bus tick, and the 2+1 split changes when `prev_irq_line` snapshots relative to APU/mapper IRQ assertion. Must verify; might need a small compensating change. |
| Rendering tests (misc) | **Low-Medium** | Sprite 0 hit, sprite overflow, sprite DMA-to-2007 tests are sensitive to PPU dot phase vs bus accesses, but those tests aren't currently in the gating suite. |
| blargg `ppu_vbl_nmi`  | **Medium**      | Not currently tracked — but this set explicitly probes VBL race at $2002 on dots 0/1/2/3 of scanline 241. Switching our split matches Mesen's behavior and should make more of this suite pass, not fewer. |

### 6.3 Alternative minimal patch: "PPU tick for $2002 read is special"

If regressions in tests 1, 2, 4, 5 are more serious than expected, a
narrower patch is possible: keep the 3-pre split for most accesses,
but for PPU register reads specifically, run only the first 2 dots
pre-access and the 3rd dot post-access.

This is effectively what Mesen does for PPU-bus-targeted reads
(because they all go through the same MemoryRead path). It's harder
to justify semantically — real hardware doesn't know in advance
whether the access is a PPU read — but it might achieve the test-3
fix without disturbing IRQ-line timing for non-PPU accesses.

This is listed only as a fallback. The right answer is 6.1.

### 6.4 Verification checklist for whoever implements this

Before committing:

1. Run the full gating sweep from CLAUDE.md's "Do-before-starting
   checklist" — expect test 3 to move from FAIL to PASS.
2. Additionally inspect `cpu_interrupts_v2/3` output to confirm the
   row count is 12 (not 13 as we produce now) and matches the
   expected table byte-for-byte.
3. Inspect `cpu_interrupts_v2/1` and `/2` output pages for any
   single-row shift; the CRCs should still match — mismatch is a
   regression.
4. For `/5-branch_delays_irq`: the existing Sub-B plan notes talk
   about snapshotting `irq_line_at_start` and gating with
   `branch_taken_no_cross`. That logic lives in `Cpu::step`; it's
   independent of the bus tick split and should continue to work.
   But the dot where APU's frame IRQ asserts vs where the CPU's
   poll snapshots it *does* depend on the tick split — re-verify
   after the change.
5. Consider writing a unit test in `bus.rs` that specifically
   asserts "after `tick_pre_access`, PPU has ticked N-1 of the
   N dots expected for this CPU cycle" for NTSC. This pins the
   contract.

---

## 7. References (file:line)

**Mesen2**:
- `NesCpu.cpp:294-315` — EndCpuCycle, edge detector.
- `NesCpu.cpp:317-323` — StartCpuCycle.
- `NesCpu.cpp:178-180` — IRQ/NMI dispatch check at end of Exec.
- `NesCpu.cpp:198-238` — IRQ() and BRK() hijack logic.
- `NesCpu.cpp:73-75, 122-125, 141-156` — NTSC clock constants,
  `_ppuOffset=1`.
- `NesPpu.cpp:1331-1355, 1249-1258, 543-549` — VBL-set, TriggerNmi,
  $2000 write handling.
- `NesPpu.h:141-148` — PPU Run loop.

**puNES**:
- `cpu_inline.h:118-236` — cpu_rd_mem dispatch (tick_hw before
  access).
- `cpu_inline.h:2163-2213` — tick_hw body.
- `cpu.c:118-123` — NMI edge detector macro.
- `cpu.c:446-459` — IRQ vector hijack.
- `cpu.c:464-471` — NMI vector entry.
- `cpu.c:539-551` — NMI recognition at dispatch.
- `ppu.c:192-196` — ppu_tick accumulator loop (all dots in one call).
- `ppu.c:934-948` — VBL flag set / nmi.high assert.

**nestopia**:
- `NstCpu.cpp:330-333` — InterruptEdge = 1.5 cycles.
- `NstCpu.cpp:1840-1858` — IRQ/NMI vector hijack.
- `NstCpu.cpp:1888-1895` — DoNMI scheduling.
- `NstPpu.cpp:2600-2633` — HCLOCK_VBLANK_0/1/2 staging + DoNMI.
- `NstPpu.hpp:143-145` — hclock constants.
- `NstBase.hpp:321-335` — PPU master-tick constants.

**Our emulator**:
- `src/bus.rs:100-124` — read() orchestration.
- `src/bus.rs:199-242` — tick_pre_access / tick_post_access.
- `src/ppu.rs:274-294, 872-877` — tick() VBL-set + update_nmi_edge.
- `src/ppu.rs:1026-1048` — poll_nmi / take_nmi_suppress_hint /
  begin_cpu_cycle.
- `src/cpu/mod.rs:78-99` — reset (7 cycles vs Mesen's 8).

**Test ROM**:
- `cpu_interrupts_v2/source/3-nmi_and_irq.s:1-109`.
- `cpu_interrupts_v2/source/common/sync_vbl.s:1-44`.
- `cpu_interrupts_v2/source/common/delay.s:40-55`.
