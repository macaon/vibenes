# 3-nmi_and_irq: investigation notes

**Scope.** Read-only analysis of why `cpu_interrupts_v2/rom_singles/3-nmi_and_irq.nes`
fails. No source changes in this session. Tests 1 (`1-cli_latency`) and 2
(`2-nmi_and_brk`) pass, so the core IRQ / BRK hijack logic is solid; the
failure mode for test 3 is much narrower than `CLAUDE.md`'s stale summary
suggested.

## 1. Observed vs expected

Source: `/home/marcus/Git/nes-test-roms/cpu_interrupts_v2/source/3-nmi_and_irq.s:1-16`.
The ROM prints the column header `NMI BRK` (line 105) and then runs the
`test` routine 12 times via `loop_n_times test,12` (line 107, macro
at `common/macros.inc:79`). Each iteration prints the pushed P at entry
to NMI (`print_x`) and to IRQ (`print_y`).

The ROM's CRC check (line 108) uses target `$B7B2ED22` — must hash a
specific 12-row pattern. The `.s` header documents that pattern:

| iter | expected NMI | expected BRK/IRQ | observed NMI | observed BRK/IRQ |
|-----:|-------------:|-----------------:|-------------:|-----------------:|
|    0 |         `23` |             `00` |         `21` |             `00` |
|    1 |         `21` |             `00` |         `21` |             `00` |
|    2 |         `21` |             `00` |         `20` |             `00` |
|    3 |         `20` |             `00` |         `20` |             `00` |
|    4 |         `20` |             `00` |         `20` |             `00` |
|    5 |         `20` |             `00` |         `20` |             `00` |
|    6 |         `20` |             `00` |         `20` |             `00` |
|    7 |         `20` |             `00` |         `20` |             `00` |
|    8 |         `20` |             `00` |         `20` |             `00` |
|    9 |         `20` |             `00` |         `25` |             `20` |
|   10 |         `25` |             `20` |         `25` |             `20` |
|   11 |         `25` |             `20` |         `25` |             `20` |

(Observed from a fresh run today, `target/release/test_runner
.../3-nmi_and_irq.nes` — 12 rows, starting `21 00`, ending with three
`25 20` rows.)

**Diagnosis in one line.** Our output is the expected output **shifted
by exactly one iteration**: iter `i` produces the result real hardware
produces for iter `i+1`. Each iteration of the outer loop slides the NMI
arrival by **1 CPU cycle** (see §3), so a 1-iteration shift ==
**NMI recognized ~1 CPU cycle earlier than real hardware**.

This is a symmetric, systemic 1-cycle advance in NMI delivery, not a
random timing glitch in one iteration. That is very different from the
failure mode CLAUDE.md's handoff described ("alternating pass/fail with
anomalous early fires on odd iterations") — that session probably saw an
older bug; the present-day failure is a clean one-cycle shift.

## 2. Decoding the P values

The NMI and IRQ handlers (`3-nmi_and_irq.s:29-46`) store the pushed
flags byte (pulled and re-pushed) into `nmi_flag` / `irq_flag`, which
the test body then prints. So the observed bytes are the flags register
as saved by the BRK/IRQ/NMI stack push, with `B=0`, `U=1` for IRQ/NMI
(confirmed by our `service_interrupt` in `src/cpu/mod.rs:207-215`).

| byte | bits            | C | Z | I | U | meaning in context                                               |
|-----:|-----------------|:-:|:-:|:-:|:-:|------------------------------------------------------------------|
| `23` | 0010_0011       | 1 | 1 | 0 | 1 | interrupted before `lda #1` (Z+C from prior `sec`, I=0 from CLI) |
| `21` | 0010_0001       | 1 | 0 | 0 | 1 | interrupted after `lda #1` (loaded 1 → Z=0)                      |
| `20` | 0010_0000       | 0 | 0 | 0 | 1 | interrupted after `clc`                                          |
| `25` | 0010_0101       | 1 | 0 | 1 | 1 | interrupted inside IRQ handler after `sec` (I=1, C=1, Z=0)       |

The critical program sequence (`3-nmi_and_irq.s:77-87`):

```
   clv            ; V = 0
   sec            ; C = Z = 1  (Z from sta/adc earlier, left set by sec's carry)
;  <-- NMI at iter 0 (expected "23") lands here
   lda #1         ; Z cleared, A=1, C still 1
;  <-- NMI at iter 1-2 (expected "21") lands here
   clc            ; C cleared
;  <-- NMI at iter 3-9 (expected "20") lands here, IRQ fires somewhere in this window
   nop            ; IRQ nominally vectors here
;  <-- iter 10-11: IRQ already handled, then NMI hits inside handler after SEC → "25"
```

So flavours map cleanly onto the window of CPU cycles at which NMI is
recognised relative to those four landmark instructions. When NMI is
recognised 1 CPU cycle earlier than hardware, the "window boundary" the
NMI crosses is shifted: an iteration whose NMI ought to fall in the
`lda #1`→`clc` window (`21`) now falls in the `sec`→`lda #1` window
(`23`), except — here is the interesting bit — the shift is in the
**opposite direction**. Observed `21` on iter 0 means NMI fires LATER,
not earlier, than expected.

Resolving the sign: the iterations use `loop_n_times test,12` which
passes `A = 0,1,2,...,11`. Inside `test` (`3-nmi_and_irq.s:51-55`)
the delay formula is:

```
eor #$FF        ; A' = ~A        (A=0 → A'=$FF; A=11 → A'=$F4)
clc
adc #1+12       ; A'' = A' + 13 + 0   (A=0 → 12; A=11 → 1)
jsr sync_vbl
jsr delay_a_25_clocks   ; delays A''+25 cycles
```

So iter 0 uses a 37-cycle delay, iter 11 uses a 26-cycle delay (roughly
— see `common/delay.s:43-55`). LARGER delay between `sync_vbl` and the
critical section means the VBL-triggered NMI fires RELATIVELY EARLIER
inside the critical section; iter 0 is therefore the earliest-NMI case
and expects the pre-`lda #1` value `23`.

A 1-CPU-cycle-LATER NMI in our emulator at iter 0 shifts that
recognition past the `lda #1` boundary, so we report `21`. The
symmetry extends down the table: our iter 9 crosses into the `sec` in
IRQ handler window, producing `25` instead of `20`. Net: our NMI
recognition is roughly **one CPU cycle later than real hardware**.

(Earlier I misread the direction — the `21, 21, 20, ...` pattern
starting with `21 00` looks like "one ahead" when skimmed, but actually
the missing first-row-`23` at the top means everything moved DOWN, i.e.
our NMI fires LATE.)

## 3. What the expected algorithm looks like

Compiled from `reference/cpu.md:106-112`, `reference/mesen-notes.md:85-88`,
Mesen2 `NesCpu.cpp:178-239, 294-315`, puNES `cpu.c:442-471, 514-560`.

### 3a. NMI edge latch

Real 6502:

1. During φ2 of each CPU cycle (second half), the CPU samples the NMI
   input line.
2. If it was high last cycle and low this cycle (active-low: so line
   dropping = edge), the internal `NeedNmi` latch goes high during φ1
   of the FOLLOWING cycle, and stays high until serviced.
3. At the end of each cycle, hardware copies `NeedNmi` to `PrevNeedNmi`.
   The CPU checks `PrevNeedNmi` when deciding to service.

Mesen2 `NesCpu.cpp:294-309` implements this as: at `EndCpuCycle`, run
the PPU forward to end of cycle, THEN `_prevNeedNmi = _needNmi`, THEN
check `!_prevNmiFlag && _state.NmiFlag` and latch `_needNmi = true` if
so. The ORDER matters: `_prevNeedNmi` picks up yesterday's `_needNmi`
before today's edge detection.

puNES models it with `nmi.before` / `nmi.delay` / `nmi.high` (see
`cpu.c:122-124` in the `before_ck` helper and `cpu.c:539-551` in the
interrupt check at op start).

### 3b. IRQ recognition

Level-triggered; no latch. CPU polls during φ2 of the second-to-last
cycle (penultimate). If the IRQ line is low AND I-flag is clear, the
CPU runs an IRQ sequence instead of fetching the next opcode. The
pushed P has B=0, U=1.

Mesen2: `_runIrq = ((_state.IrqFlag & _irqMask) > 0 && !I)` at end of
each cycle. `_prevRunIrq` is yesterday's value. Service when
`_prevRunIrq` is set at Exec() entry (NesCpu.cpp:178).

### 3c. Hijack window (IRQ → NMI)

Both Mesen2 and puNES check NMI status `_needNmi` / `nmi.high` AFTER
pushing PCH+PCL but BEFORE pushing P, and use that single check to
decide whether to load from `$FFFA` or `$FFFE`. Mesen2 NesCpu.cpp:
198-217:

```
Push((uint16_t)(PC()));       // push PCH, PCL (2 cycles)
if(_needNmi) {                 // hijack decision
    _needNmi = false;
    Push(PS | Reserved);
    ...
    SetPC(MemoryReadWord(NMIVector));
} else {
    Push(PS | Reserved);
    ...
    SetPC(MemoryReadWord(IRQVector));
}
```

puNES `cpu.c:442-460` captures `flagNMI` from `nes.c.nmi.high` BEFORE
pushing anything (at the top of `_IRQ` macro), then does `_PSP` + `_PSH`
which pushes PC+PS, then chooses vector. Both hit the hijack window
around cycles 4–5 of the 7-cycle service sequence.

### 3d. Post-service late-NMI deferral

Mesen2 explicitly clears `_prevNeedNmi = false` at the END of BRK
(NesCpu.cpp:238): "Ensure we don't start an NMI right after running a
BRK instruction (first instruction in IRQ handler must run first —
needed for nmi_and_brk test)." Equivalent deferral sits inside IRQ()
on path 2 (the non-hijack fallback) — nothing is explicit because
`_needNmi` stays set for the next cycle's `_prevNeedNmi` copy, but the
first instruction of the handler runs before the second IRQ() call.

puNES encodes the same thing differently via `nmi.delay`: at
`cpu.c:457-459`, after an IRQ (not hijacked) services, if NMI still
high, set `nmi.delay = TRUE`. Then at op-start (`cpu.c:546-550`) that
delay is cleared without triggering.

## 4. Our algorithm

### 4a. NMI edge in PPU

`src/ppu.rs:282-294`: at `scanline == 241 && dot == 1`, set
`status |= 0x80` and mark `vbl_just_set = true` (for the $2002-read
race suppression). Then call `update_nmi_edge()` (line 294), which
compares `asserted = ctrl7 && status7` against `nmi_previous` and sets
`nmi_edge = true` on the rising edge (line 874-877).

### 4b. Bus cycle split

`src/bus.rs:175-208` (`tick_pre_access`) runs at the START of each
bus access:

1. Snapshot `prev_irq_line = irq_line` and `prev_nmi_pending = nmi_pending`
   (lines 176-177). This captures end-of-previous-cycle state.
2. `ppu.begin_cpu_cycle()` clears per-cycle race markers.
3. `clock.advance_cpu_cycle()` returns the number of PPU dots owed
   (line 185-187), and we tick the PPU that many times BEFORE the CPU
   access.
4. `ppu.poll_nmi()` consumes `nmi_edge` and sets `bus.nmi_pending = true`
   (lines 189-191).
5. APU + mapper tick, then `irq_line = apu.irq_line() | mapper.irq_line()`.

`tick_post_access` (lines 214-218) is only an audio sink drain;
interrupt state doesn't move here.

### 4c. Polling at end of instruction

`src/cpu/mod.rs:126-150` (`Cpu::step`):
- Line 135: snapshot `i_flag_before = p.interrupt()` so CLI/SEI/PLP
  see the OLD I for the penultimate-cycle poll.
- Line 140: snapshot `irq_line_at_start` for the branch quirk.
- Line 141: fetch opcode (first bus read).
- Line 142-146: run opcode body.
- Line 147: `poll_interrupts_at_end` reads `bus.prev_nmi_pending` first
  (line 161): if true, set `pending_interrupt = Nmi`, clear
  `bus.nmi_pending`, return (line 162-164).
- Line 190: else check IRQ `prev_irq_line && !i_for_poll && !suppress_by_branch`,
  set `pending_interrupt = Irq`.

`src/cpu/mod.rs:195-239` (`service_interrupt`) performs the 7-cycle
sequence: two dummy reads at PC, push PCH, push PCL, push P, hijack
check on `prev_nmi_pending` AT boundary between push-P and
vector-low-fetch (line 224), then fetch vector. Line 236-238:
`prev_nmi_pending = false` at end of service to implement the same
post-service deferral as Mesen2.

Separately, `src/cpu/ops.rs:971-998` has an INLINE BRK implementation
that replicates the hijack check on line 982. The two implementations
(IRQ vs BRK-inline) are intentionally parallel.

### 4d. Why this model is "NMI-late by 1 cycle" for test 3

Look at the PPU tick ordering in `tick_pre_access`. Say the PPU reaches
`scanline 241, dot 1` during CPU cycle `N`:

- Pre-access of cycle N:
  - `prev_nmi_pending = nmi_pending` at start (still false — edge hasn't
    fired yet this frame).
  - PPU ticks 3 dots; one of those ticks sets `nmi_edge = true`.
  - `poll_nmi()` sets `bus.nmi_pending = true` at end of pre-access.
- CPU bus access of cycle N: completes.
- Pre-access of cycle N+1:
  - `prev_nmi_pending = nmi_pending = true` (captures end-of-N).
- Instruction whose FIRST bus access is cycle N+1: when it finishes,
  `poll_interrupts_at_end` reads `bus.prev_nmi_pending`. That value
  reflects end-of-cycle-N state if the instruction is a 2-cycle op
  (penultimate = N+1 start), or later-cycle state for longer ops.

Real hardware: NMI detected via rising-edge sample during φ2 of some
cycle M. Mesen2 advances PPU inside `EndCpuCycle` AFTER the CPU's
memory op, THEN samples the edge. If VBL is set at dot 1 of 241, the
dot lands DURING the cycle where `EndCpuCycle` runs the PPU. So
Mesen2's "the CPU cycle that sees the VBL edge" is one cycle later
than ours, because we run all 3 dots at the START of the cycle.

In other words: a dot 1 event in our model lives at the START of CPU
cycle X, while in Mesen2's model (which is closer to hardware) that
event lives at the END of CPU cycle X (equivalent to start of X+1).
So real hardware's `_needNmi` goes true one cycle LATER than our
`bus.nmi_pending`, and hence the CPU's `_prevNeedNmi` sees it one
cycle LATER than our `prev_nmi_pending`.

**That makes us NMI-late? Wait — the reasoning says we'd be NMI-early,
not late.** Let me re-walk it.

- We set `nmi_pending` at end-of-pre-access of cycle N (all 3 dots
  ticked, including dot 1 of 241).
- `prev_nmi_pending` sampled at pre-access of cycle N+1 is TRUE.
- Poll at end of instruction: an instruction whose last cycle is N+1
  sees `prev_nmi_pending` captured at pre-access of N+1 = true.
  Since "last cycle" here equals the cycle at which the final bus
  access completes, the instruction that ends by using cycle N+1 as
  its final cycle will latch NMI.

Real hw (Mesen2 model): NMI edge detected at φ2 (end) of some cycle M.
`_needNmi` set after that cycle. `_prevNeedNmi` becomes true at end of
M+1. An instruction polls at penultimate = M+1, last cycle = M+2, so
the instruction that ends at M+2 sees the NMI.

If our cycle N == Mesen2's cycle M (dot 1 inside both), then our
recognition lands at instruction-end-cycle N+1, Mesen's at M+2 = N+2.
So WE see NMI one cycle EARLIER.

But the test output says we recognize NMI LATER, not earlier. Something
is off in my walk. Let me reconsider.

One possibility: our `prev_nmi_pending` snapshot is TOO EARLY. Because
`tick_pre_access` snapshots at the very start of the cycle, BEFORE
ticking the PPU, `prev_nmi_pending` during cycle N reflects end-of-N-1
(not current end-of-penultimate of the instruction). Then `poll_nmi()`
sets `nmi_pending = true` during cycle N. At end of instruction, the
CPU's poll reads `bus.prev_nmi_pending` AFTER ops::execute has run —
so it reads the LAST snapshot, i.e. `prev_nmi_pending` at start of the
instruction's FINAL cycle.

So the CPU poll sees "end of penultimate" via this snapshot — NOT end
of final cycle. For a 2-cycle instruction starting at cycle K (so
cycles K and K+1), `prev_nmi_pending` is snapshotted at pre-access of
K (value at end of K-1) and at pre-access of K+1 (value at end of K).
The poll after execute() reads the LATEST = end of K. That's end of
penultimate for a 2-cycle op ending at K+1. Correct.

Now: if NMI edge latches `nmi_pending = true` DURING cycle M (the
edge happens during pre-access of cycle M, before the CPU's bus
access), then at pre-access of cycle M+1 we snapshot
`prev_nmi_pending = true`. For an instruction whose penultimate is
M+1, CPU poll reads that `true` and services NMI at end of final
cycle M+2. So recognition boundary: penultimate >= M+1, i.e., final
cycle >= M+2, i.e., instruction-end-cycle >= M+2. Any instruction that
ends at cycle M+2 or later will pick up the NMI.

Mesen2: edge at φ2 of cycle M → `_needNmi = true` at end of M →
`_prevNeedNmi = true` at end of M+1 → Exec() of the NEXT instruction
sees `_prevNeedNmi` and runs IRQ(). The NEXT instruction's Exec() is
called after completing the current instruction. If the current
instruction ends at cycle K-1 (last cycle), then Exec() of the next
runs at cycle K. So we need K >= M+2.

Identical! Both require the next instruction's Exec() to start at
cycle M+2 or later, equivalent to the previous instruction ending at
cycle M+1 at earliest.

So algorithmically, given the same M (cycle at which dot 1 lands),
timing should MATCH. The discrepancy must come from WHAT VALUE OF M we
pick, i.e., which CPU cycle the PPU is considered to be at dot 1 of
241.

### 4e. Where the 1-cycle drift really comes from

Our `Bus::tick_pre_access` runs all 3 NTSC dots at the START of the
CPU cycle. Mesen2 runs the PPU at BOTH `StartCpuCycle` and `EndCpuCycle`,
splitting dots across the cycle according to a precise master-clock
phase (NesCpu.cpp:319, 297). The PPU advance inside `EndCpuCycle` is
what actually carries dot 1 of 241 into the cycle during which the
CPU's edge detector samples the line.

Because Mesen2's PPU reaches dot 1 of 241 LATE in a CPU cycle (in
`EndCpuCycle` of some cycle M'), and our PPU reaches it EARLY (in
`tick_pre_access` of some cycle N), the mapping is:

```
our cycle N   ≡   Mesen's cycle (M' - 1)
```

...so our `nmi_pending` latch happens one cycle EARLY relative to the
CPU cycle in which hardware latches `_needNmi`. Therefore our CPU
sees NMI one cycle EARLY, not late.

But our TEST result says the opposite — iter 0 reports `21` (NMI past
`lda #1`) when hardware expects `23` (NMI before `lda #1`). That's NMI
**later** in our model.

**This is the central puzzle** — and it's the same shape as the
"off-by-one-cycle" note in project memory (`memory/off_by_one_cycle_diagnostic.md`).
The right answer is probably NOT "NMI is too late in our model"; it's
"one or more of the test's timing primitives drifts by one cycle, and
the NMI happens to line up as if late." Candidate culprits:

1. **`sync_vbl` alignment.** The sync routine (`common/sync_vbl.s`)
   uses a 27-cycle polling loop on `$2002` bit 7. It expects the CPU
   to observe the VBL flag at a precise PPU-dot offset. If our PPU
   tick ordering puts the VBL flag visible in `$2002` reads one CPU
   cycle earlier than real hardware, `sync_vbl` will align itself
   offset by one CPU cycle — and downstream `delay_a_25_clocks` will
   compute delays from that offset. NMI recognition timing would be
   the same, but the sync point would be shifted, causing an effective
   "NMI fires later" relative to the test.

2. **$2002 race.** Our PPU uses `vbl_just_set` + `nmi_suppress_hint`
   (`src/ppu.rs:85-91`) to model the "$2002 read on dot 1 of 241
   returns 0 and suppresses NMI" race. `sync_vbl` POLLS $2002 tightly;
   if our race is misaligned vs hardware's exact single-dot window,
   sync_vbl's exit point is one cycle off. See `NesPpu.cpp:290` in
   Mesen2: the clear-VBlank-in-return-value logic is
   `_scanline == _nmiScanline && _cycle < 3` — a 3-cycle window, not
   1. Our `vbl_just_set` is only set during the exact dot 1 tick and
   cleared by `begin_cpu_cycle` at the start of the NEXT CPU cycle.
   That gives roughly a 1-CPU-cycle window (maybe 2–3 PPU dots depending
   on phase). Mesen2's is "cycle 0, 1, 2" = 3 PPU dots = 1 CPU cycle.
   So they might match, but it's worth instrumenting.

3. **CPU reset cycle alignment.** Our reset (`src/cpu/mod.rs:83-105`)
   burns 5 dummy reads + 2 vector reads = 7 CPU bus cycles. Mesen2
   does 8 (`NesCpu.cpp:161`). That 1-cycle difference propagates
   through every test that relies on a known number of CPU cycles
   from power-on to the first instruction.

    **This is probably the root cause** — a 1-cycle-early CPU start
    means every subsequent sync_vbl alignment and hence every
    NMI-relative-to-test-code window is shifted by 1 CPU cycle. It
    would present EXACTLY as a 1-iteration shift across all 12 rows
    of the test table, symmetrically. Tests 1 and 2 pass because their
    expected answers don't depend on the precise single-cycle
    positioning of NMI (test 2's expected table has all results
    aligned on 2-cycle boundaries, so ±1 cycle keeps each answer in
    the same bucket). Test 3's `sec`/`lda #1`/`clc` critical section
    is 2+2+2 cycles — exactly the single-cycle resolution that catches
    a 1-cycle startup drift.

## 5. Hypothesis ranking

1. **CPU reset cycle count off-by-one.** Our `Cpu::reset` does 7 ticks
   (5 dummy + 2 vector = 7). Mesen2 does 8 (`NesCpu.cpp:161`:
   `for(int i = 0; i < 8; i++) { StartCpuCycle(true); EndCpuCycle(true); }`).
   Nesdev docs say 7 cycles, but Mesen2 (which passes this test)
   burns 8 — one of those 8 is presumably the internal-op cycle that
   nesdev sometimes glosses over. Most likely cause.
2. **VBL-start NMI edge: 1-cycle-early latch due to pre-access tick.**
   All 3 NTSC dots ticked in `tick_pre_access` means any dot 1 event
   is visible to the CPU of the SAME cycle (because pre-access precedes
   the CPU's read). Hardware would see it at the END of the cycle,
   which effectively is "during the next cycle's latch." We'd be
   NMI-early by 1 cycle — which, inverted through the sync_vbl
   alignment loop, could present as NMI-late in the test.
3. **`$2002` race window width.** Marginal; only matters for the
   `sync_vbl` exit loop and probably fine given tests 1+2 pass.
4. **APU-tick-before-access changing IRQ timing relative to NMI.**
   Very unlikely given iter 10-11 (`25 20`) pass. If IRQ timing were
   off, the IRQ column would fail consistently.
5. **Branch-delays-IRQ flag getting set by a `bpl :-` inside sync_vbl
   and mis-suppressing a poll.** `sync_vbl.s:18-19` is `bpl :-`, a
   taken-no-cross branch. If the suppress logic fires incorrectly
   here, interrupt polling could drift by an instruction. But since
   CLI is done AFTER `sync_vbl` (`3-nmi_and_irq.s:69`), IRQ isn't
   enabled while sync_vbl runs — so this can't affect the test.

## 6. Concrete fix plan

Primary target: `/home/marcus/Git/vibenes2/src/cpu/mod.rs:83-105`.
Adjust reset to burn 8 CPU cycles instead of 7:

```rust
// was: 5 dummy reads + 2 vector reads = 7 cycles
// need: 1 more dummy cycle before the vector reads (matches Mesen2
// NesCpu.cpp:161 and puNES `cpu_exe_op` reset path which both do 8).
for _ in 0..6 { bus.read(0x00FF); }    // 6 dummy reads
let lo = bus.read(0xFFFC);              // cycle 7
let hi = bus.read(0xFFFD);              // cycle 8
```

Expected behaviour change per iteration of the table:
- All 12 rows shift by 1 iteration in the PASSING direction:
  iter 0 → `23` (match), iter 1 → `21` (match), ..., iter 11 → `25` (match).
- Net: test PASSES; CRC of 12-row output hashes to `$B7B2ED22`.

Risk: adding a cycle to reset may regress any test that measures
from-reset timing with cycle granularity. The apu_reset suite measures
in 8-cycle-ish windows (`apu_reset/*.nes`) — `reset_timing` especially.
If that suite regresses, the dummy read should go EARLIER in the reset
sequence (before the 5 existing dummies) rather than between the last
dummy and the vector fetch — position only matters for the precise
bus-address sequence, not the cycle count.

Alternative if (1) isn't right: target
`/home/marcus/Git/vibenes2/src/bus.rs:175-208`. Split the PPU ticks so
the first two dots of the CPU cycle run in `tick_pre_access` and the
third dot runs in `tick_post_access`. This moves the
"dot 1 of scanline 241" event half a CPU cycle later, approximating
Mesen2's end-of-cycle placement. This is structural — it would also
affect `$2002`-race timing and NMI visibility in reads during the
race cycle. HIGH regression risk; tests 1+2, apu_test 08, and
blargg_apu_2005 all depend on the current ordering.

Start with (1) — it's minimal-surface and most-likely-correct.

## 7. Regression surface

Before committing any fix for test 3, rerun:

- cpu_interrupts_v2 1-cli_latency, 2-nmi_and_brk (must stay PASS).
- cpu_interrupts_v2 4-irq_and_dma, 5-branch_delays_irq (still FAIL —
  confirm no new failure shape).
- apu_test 1-6 (APU channel + frame counter — tied to reset-to-first-
  instruction cycle offset via frame counter parity).
- apu_reset all six (especially `4017_timing` and `4017_written` —
  both measure from reset; a reset-cycle-count change WILL move the
  critical windows).
- instr_test-v5/official_only (must stay 16/16).
- instr_misc.
- blargg_apu_2005.03 (IRQ flag timing) and 07 (IRQ timing under
  various $4017 writes) — both sensitive to frame-counter/reset
  alignment.
- Unit tests: `cargo test --lib --release` — currently 14+, including
  the `taken_no_cross_branch_delays_irq*` pair added with phase 5.

If the reset-cycle-count change regresses apu_reset.`4017_timing`,
invert: add a dummy cycle at the FRONT of reset (before the 5 existing
dummies) so the bus-address sequence `5x$00FF → $FFFC → $FFFD` is
preserved; only the pre-stall shifts. Our existing 5 dummy reads
already target `$00FF`; adding a 6th is safe.

## 8. What to instrument before touching code

Add a trace hook around the first iteration of the test 3 loop:

- At every `Cpu::step` entry, log `(cycle, pc, i_flag)`.
- At every `poll_interrupts_at_end` entry, log
  `(cycle, pc, prev_nmi_pending, prev_irq_line, pending_interrupt)`.
- At every NMI service entry, log
  `(cycle, pc_before_push, nmi_pending, prev_nmi_pending)`.

Compare the trace for iter 0 with the expected sequence:
- Iteration 0 should see NMI at some PC just before `lda #1` (around
  the `sec` at line 77 of the source). Observe which PC actually
  triggers NMI service. If it's `lda #1`+2 or `clc`+1, that
  confirms the 1-cycle drift hypothesis.

Alternatively (less invasive) add a one-shot CSV dump of `(cycle,
sl, dot, status_bit7)` around PPU scanline 240→241 transition during
the first `test` iteration. Compare against hand-computed expected
cycle of dot 1.

## 9. Cross-references

- Mesen2 reset path: `NesCpu.cpp:161` (8 cycles burned before first
  opcode fetch). Relevant comment: "The CPU takes 8 cycles before it
  starts executing the ROM's code after a reset/power up."
- Mesen2 NMI edge detect: `NesCpu.cpp:301-309` (end-of-cycle latch).
- Mesen2 IRQ hijack: `NesCpu.cpp:198-217` (check after PC push, before
  P push).
- Mesen2 post-BRK defer: `NesCpu.cpp:238`.
- Mesen2 VBL set dot: `NesPpu.cpp:1339-1342` (cycle == 1 AND scanline
  == nmiScanline) — `nmiScanline = 241` (NesPpu.cpp:169).
- Mesen2 $2002-race clear: `NesPpu.cpp:290` (cycle 0, 1, 2 window).
- puNES reset ref point: `cpu.c:1019-1024` (warm reset clears IRQ/NMI
  state, forces I=1).
- puNES IRQ/NMI service: `cpu.c:442-471` — `_IRQ` macro with NMI
  hijack via `flagNMI`, `NMI` macro standalone. `cpu.c:514-560` is
  the per-op entry check.
- Our CPU step: `src/cpu/mod.rs:126-150`.
- Our CPU service: `src/cpu/mod.rs:195-239`.
- Our BRK inline: `src/cpu/ops.rs:971-998`.
- Our bus split: `src/bus.rs:175-218`.
- Our PPU VBL edge: `src/ppu.rs:282-294`, `src/ppu.rs:872-878`.
- Our reset: `src/cpu/mod.rs:83-105`.
- Our test 3 output (current): 12 rows starting `21 00 / 21 00`,
  ending three `25 20` — confirmed reproducible at HEAD.

## 10. Sanity checks the fix author should do before concluding

- Run the passing suite after the change. Record diff; it should be
  empty except for the target test.
- Recompute the expected CRC of the ROM's printed output to match
  `$B7B2ED22`. (Can be done externally by pasting the 12-row sequence
  into a CRC32 calc; blargg's implementation is in `common/crc.s`.)
- Confirm the single new dummy read in reset is NOT visible to any
  MMIO-side-effecting address. Currently `$00FF` is RAM (safe).
  Anything in the $4016/$4017 range would silently shift controller
  shift state.

-- end notes --
