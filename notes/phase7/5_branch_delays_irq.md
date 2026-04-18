# 5-branch_delays_irq: investigation notes

**Scope.** Read-only analysis of why
`cpu_interrupts_v2/rom_singles/5-branch_delays_irq.nes` fails (hash
`C98BA29A`, code 1 = FAIL). Tests 1 (`1-cli_latency`) and 2
(`2-nmi_and_brk`) are green; tests 3, 4, 5 are open. Our existing
branch-delays-IRQ quirk (`src/cpu/mod.rs:183-187`) is present but
slightly too aggressive — it suppresses one IRQ per taken-no-cross
branch that real hardware would still take. The `test_jmp` /
`test_branch_not_taken` / `test_branch_taken_pagecross` subtests all
match hardware exactly; **only `test_branch_taken` is off**, and by a
single row.

The full expected/observed diff, the measurement methodology, and the
concrete one-word fix are below.

---

## 1. Test methodology (how the ROM measures you)

Source: `~/Git/nes-test-roms/cpu_interrupts_v2/source/5-branch_delays_irq.s:64-99`.

Each subtest runs the `begin` routine, which:

1. Prints iteration number (A = 0..9) — the `T+` column.
2. Saves SP to `saved_s` (so the IRQ handler can drop the caller frame
   and return to `main`).
3. Calls `sync_apu` to align the APU frame counter divide-by-two.
4. Writes `$4017 = $40` then `$4017 = $00` → frame mode 0 with IRQ
   enabled, reset at a known APU phase (`sync_apu.s`).
5. Reads `$4015` to clear the latched frame-IRQ flag.
6. `CLI` — clears I so IRQs can fire.
7. Pulls the pushed iteration byte, XORs with `$0F` (so higher T+ =
   *lower* A = *shorter* delay), calls `delay_a_25_clocks` (A+25 CPU
   cycles), then `delay 29788-15` — the total delay is tuned so that
   the frame-counter IRQ lands *just before* the test instructions run.
   Each T+ step = +1 A = **−1 CPU cycle** of delay → the IRQ arrives
   **+1 CPU cycle** later in the subsequent instruction stream.

`begin` then `RTS`s; 10 iterations are run via
`loop_n_times routine, 10` (macro in `common/macros.inc:79`).

The IRQ handler at `5-branch_delays_irq.s:199-220`:

```
irq:    delay 29830-9-6 + 2
        ldx #7
:       dex
        delay 29831 - 13
        bit $4015
        bit $4015
        bvc :-
        jsr print_x       ; → "CK" column
        pla               ; discard PCL of the JSR-to-begin frame
        pla               ; discard PCH
        and #$0F          ; mask low nibble of PCH — no, wait
        jsr print_a       ; → "PC" column
        ; ... restore SP to saved_s + 2 and RTS
```

Hmm — look more carefully. After the IRQ fires the stack from the
top down is: `[PCH, PCL, P, saved_return_PCH, saved_return_PCL, ...]`.
The handler `pla; pla` discards the IRQ's P and PCL. Then reads A
from the NEXT byte (`pla` loads A), which is the pushed **PCH** of
the IRQ stack frame. But the A it prints is `pla eor #$0F`... no wait,
re-read:

```
        pla               ; A <- P (discard)
        pla               ; A <- PCL (this is what we print, low nibble)
        and #$0F
        jsr print_a
```

Wait actually the handler in `5-branch_delays_irq.s:210-214`:

```
        pla
        pla
        and #$0F
        jsr print_a
```

Two `pla`s pop P then PCL; `and #$0F` masks the PCL to a low nibble,
then prints. So the **PC** column is **PCL & $0F** — the low nibble
of the pushed PC (which is the address of the NEXT instruction to
execute when IRQ was recognized).

The **CK** column is the X register at exit from the busy-wait loop.
X starts at 7, decrements each iteration; loop runs ~29831 CPU cycles
per iteration and exits when frame IRQ reasserts (`bvc :-` on bit 6
of `$4015`). Bigger X on exit = fewer loop iterations = handler
returned *earlier in the frame window* = IRQ fired *earlier* relative
to the new frame counter.

### Why the pattern tells us cycle counts

As T+ advances by 1, the IRQ-arrival time slides by +1 CPU cycle. The
IRQ is caught by the *next* poll-sample point that occurs **at or
after** the arrival. Within a single instruction's poll window (=
number of cycles whose tail is covered by that instruction's
end-of-penultimate sample), several consecutive T+ values land on the
same poll. When T+ increments past the cutoff, the IRQ defers to the
next instruction's penultimate — PC in the print column jumps to the
next instruction's successor address.

The number of consecutive T+ rows sharing the same `PC` therefore
equals the number of CPU cycles of "gap" between one poll and the
next. For sequential instructions this equals the later instruction's
cycle count (penultimate of instruction N+1 is N+1's cycles after
instruction N's penultimate, minus 1 for N's trailing cycle — but
since instruction N's trailing cycle is also covered by instruction
N+1's window, the count simplifies to **N+1's cycle count**).

So a table with windows of size 2, 3, 2, 3 means the instructions
executed were (some 2-cycle), (some 3-cycle), (some 2-cycle), (some
3-cycle). The `CK` column wraps from 1 back up to some larger value
each time a window boundary coincides with a whole frame crossing —
each "up-jump" in CK indicates how many CPU cycles were skipped to
reach the next poll.

---

## 2. Expected vs observed — row by row

The expected table is copied verbatim from the assembler comments in
`5-branch_delays_irq.s:8-57` (also in `readme.txt:126-176`). "Diff"
column marks rows where our output matches (`=`) or diverges (`≠`).

### 2a. test_jmp — PASS content-wise (but ROM still fails overall)

```
test_jmp
Layout: NOP@$03 (2cyc)  JMP@$04 (3cyc)  NOP@$07 (2cyc)  JMP@$08 (3cyc)

T+  | expected     | observed    | diff
----|--------------|-------------|-----
00  | 02 04  NOP   | 02 04       | =
01  | 01 04        | 01 04       | =
02  | 03 07  JMP   | 03 07       | =
03  | 02 07        | 02 07       | =
04  | 01 07        | 01 07       | =
05  | 02 08  NOP   | 02 08       | =
06  | 01 08        | 01 08       | =
07  | 03 08  JMP   | 03 08       | =  (PC=$08 because IRQ is caught by
                                       NOP@$07's poll, deferred from
                                       JMP@$04's trail; post-NOP PC=$08)
08  | 02 08        | 02 08       | =
09  | 01 08        | 01 08       | =
```

Every row matches. The CLAUDE.md handoff comment that "test_jmp
shares root cause with test 3" appears to be **stale** — current
code gets test_jmp bit-for-bit correct. (It may have been true at a
prior commit before Phase-6 bus-split work landed.)

The `PC=$08` at T+=07..09 is subtle: the instruction-fetch cycle of
`JMP` at `$08` is the final cycle, but the penultimate-IRQ-poll of
the *preceding* `NOP@$07` (2-cycle) catches the IRQ first and pushes
the post-NOP PC, which is `$08`. The three rows (T+=07..09) show the
same PC=$08 because the window between NOP's poll and JMP's poll is 3
cycles (NOP's tail + JMP's first 2 = 3). Everything lines up.

### 2b. test_branch_not_taken — PASS

```
Layout: CLC@$03 (2)  BCS@$04 not taken (2)  NOP@$06 (2)  LDA$100@$07 (4)  JMP@$0A

T+  | expected     | observed    | diff
----|--------------|-------------|-----
00  | 02 04  CLC   | 02 04       | =
01  | 01 04        | 01 04       | =
02  | 02 06  BCS   | 02 06       | =
03  | 01 06        | 01 06       | =
04  | 02 07  NOP   | 02 07       | =
05  | 01 07        | 01 07       | =
06  | 04 0A  LDA   | 04 0A       | =
07  | 03 0A        | 03 0A       | =
08  | 02 0A        | 02 0A       | =
09  | 01 0A        | 01 0A  JMP  | =
```

All 10 rows identical. Not-taken BCS takes 2 cycles, no branch-delay
quirk applies; IRQ is polled normally at its penultimate.

### 2c. test_branch_taken_pagecross — PASS

```
Layout: CLC@$0D (2)  BCC to $00 (4, page cross)  LDA $100 (4)  JMP

T+  | expected     | observed    | diff
----|--------------|-------------|-----
00  | 02 0D  CLC   | 02 0D       | =
01  | 01 0D        | 01 0D       | =
02  | 04 00  BCC   | 04 00       | =  (4-cycle window — matches BCC+X)
03  | 03 00        | 03 00       | =
04  | 02 00        | 02 00       | =
05  | 01 00        | 01 00       | =
06  | 04 03  LDA   | 04 03       | =
07  | 03 03        | 03 03       | =
08  | 02 03        | 02 03       | =
09  | 01 03        | 01 03       | =
```

All rows identical. Page-crossing branch = 4 cycles, **quirk does
not apply** — our `branch()` only sets `branch_taken_no_cross` when
`!page_crossed` (`src/cpu/ops.rs:303-307`).

### 2d. test_branch_taken — FAIL by exactly one row

```
Layout: CLC@$03 (2)  BCC to $07 (3, taken, no cross)  LDA $100 (4)  JMP@$0A

T+  | expected      | observed     | diff
----|---------------|--------------|-----
00  | 02 04  CLC    | 02 04        | =
01  | 01 04         | 01 04        | =
02  | 03 07  BCC    | 03 07        | =
03  | 02 07         | 06 0A        | ≠  ← only divergent row
04  | 05 0A  LDA ** | 05 0A        | =
05  | 04 0A         | 04 0A        | =
06  | 03 0A         | 03 0A        | =
07  | 02 0A         | 02 0A        | =
08  | 01 0A         | 01 0A        | =
09  | 03 0A  JMP    | 03 0A        | =
```

Expected window sizes: CLC=2 rows, BCC=2 rows, LDA=5 rows, JMP=1 row
(total 10). The **"LDA = 5 rows"** is the signature of the quirk —
without the quirk LDA would be 4, but the quirk defers one T+ slot
from BCC's tail into LDA's window.

Observed window sizes: CLC=2 rows, BCC=**1 row**, LDA=**6 rows**,
JMP=1 row (total 10). **We're over-suppressing by exactly one row**:
at T+=03 the IRQ should still fire at BCC's penultimate (pushing
PC=$07), but we defer it to LDA (PC=$0A).

The `CK=6` at T+=03 (vs expected `CK=2`) reflects the same event
viewed from the handler's busy-wait loop: the IRQ arrives at the
start of LDA's window instead of the end of BCC's, so it gets
serviced in the *next* frame period — the handler spins through an
extra whole frame, and X exits 4 higher (CK=6 vs CK=2, delta +4 is
the next-frame wrap offset for this alignment).

---

## 3. Current implementation — what we do now

`src/cpu/mod.rs:126-193`, `src/cpu/ops.rs:291-309`.

### Current suppression condition

`poll_interrupts_at_end` (`src/cpu/mod.rs:185-192`):

```rust
let suppress_by_branch = self.branch_taken_no_cross
    && !self.irq_line_at_start
    && bus.prev_irq_line;
self.branch_taken_no_cross = false;

if bus.prev_irq_line && !i_for_poll && !suppress_by_branch {
    self.pending_interrupt = Some(Interrupt::Irq);
}
```

Where:
- `self.branch_taken_no_cross` — set by `branch()` when a branch was
  taken and `!page_crossed` (`src/cpu/ops.rs:303-307`).
- `self.irq_line_at_start` — captured at **start of `step()`**
  (`src/cpu/mod.rs:140`), **before the opcode fetch's `tick_pre_access`**.
  Thus it is the IRQ line as of **end of cycle 0** (i.e., end of the
  previous instruction's last cycle).
- `bus.prev_irq_line` — at end of the instruction, this is the IRQ
  line as of **end of the penultimate cycle** (= end of cycle 2 for
  a 3-cycle branch). Captured each `tick_pre_access` as the "IRQ
  going into this cycle" snapshot (`src/bus.rs:176`).

### Window that our condition suppresses

For a 3-cycle taken-no-cross branch, the condition fires iff
`irq_line_at_start == false` AND `prev_irq_line (= end-of-cycle-2) == true`.
That means the IRQ **rose at some point during cycles 1 or 2**. Our
model collapses the "cycle 1 rising edge" and "cycle 2 rising edge"
cases into one.

### What Mesen2 actually does

`~/Git/Mesen2/Core/NES/NesCpu.cpp:294-315`:

```cpp
void NesCpu::EndCpuCycle(bool forRead) {
    _masterClock += ...;
    ...
    _prevRunIrq = _runIrq;                                    // cycle-1 state
    _runIrq = ((_state.IrqFlag & _irqMask) > 0 && !CheckFlag(PSFlags::Interrupt));
                                                              // cycle-2 state
}
```

`~/Git/Mesen2/Core/NES/NesCpu.h:432-448`:

```cpp
void BranchRelative(bool branch) {
    int8_t offset = (int8_t)GetOperand();            // cycles 1, 2 done
    if (branch) {
        // "a taken non-page-crossing branch ignores IRQ/NMI during its
        //  last clock, so that next instruction executes before the IRQ"
        if (_runIrq && !_prevRunIrq) {               // rising edge during cycle 2 ONLY
            _runIrq = false;
        }
        DummyRead();                                 // cycle 3
        if (CheckPageCrossed(PC(), offset)) DummyRead();
        SetPC(PC() + offset);
    }
}
```

At the point the `_runIrq && !_prevRunIrq` check runs:

- `GetOperand()` has just ticked the operand-fetch cycle (cycle 2).
  That cycle's `EndCpuCycle` has already run — so `_prevRunIrq` holds
  the *end-of-cycle-1* latch and `_runIrq` holds the *end-of-cycle-2*
  latch (per the assignment order in `EndCpuCycle`).
- Suppression condition: "IRQ was **not** armed at end of cycle 1
  AND **is** armed at end of cycle 2" = "IRQ rose during cycle 2
  itself (= the penultimate cycle)".

The quirk **does not** fire when the IRQ was already high at end of
cycle 1 — real hardware lets that through the penultimate poll.
Mesen2's comment tracks the classic wiki phrasing exactly: the branch
skips the poll on **its last clock**, not on its entire duration.

### puNES's equivalent

`~/Git/puNES/src/core/cpu.c:114-127`:

```c
#define BRC(flag, condition) \
    BYTE offset = _RDP;           /* cycle 2 operand fetch */ \
    WORD adr0 = nes[nidx].c.cpu.PC.w + (SBYTE)offset; \
    if ((!flag) != condition) { \
        BYTE cross = !((adr0 & 0xFF00) == (nes[nidx].c.cpu.PC.w & 0xFF00)); \
        if (!cross) { \
            if (nes[nidx].c.nmi.high && !nes[nidx].c.nmi.before) { \
                nes[nidx].c.nmi.delay = TRUE; \
            } else if (!(nes[nidx].c.irq.inhibit & 0x04) \
                       && nes[nidx].c.irq.high && !nes[nidx].c.irq.before) { \
                nes[nidx].c.irq.delay = TRUE; \
            } \
        } \
        ...
```

`.before` is refreshed at the **start of every `tick_hw`**
(`~/Git/puNES/src/core/cpu_inline.h:2168-2171`):

```c
INLINE static void tick_hw(BYTE nidx, BYTE value) {
    ...
    tick_hw_start:
    nes[nidx].c.cpu.opcode_cycle++;
    nes[nidx].c.nmi.before = nes[nidx].c.nmi.high;       // pre-tick snapshot
    nes[nidx].c.irq.before = nes[nidx].c.irq.high;
    ...
    apu_tick(&value);                                    // tick may set .high
    ...
}
```

After the operand-fetch `tick_hw`, `.before` = IRQ line **going
into** the cycle 2 tick = state at **end of cycle 1**, while `.high`
= state **after** the cycle 2 tick = state at **end of cycle 2**.
The BRC check `.high && !.before` is therefore "rose during cycle 2"
— **exactly the same as Mesen2**.

### Our miss

We compare `bus.prev_irq_line` (end of cycle 2, correct) against
`self.irq_line_at_start` (end of cycle 0 — **one cycle too early**).
When the IRQ rises during cycle 1, we see `irq_line_at_start == false`
and `prev_irq_line == true`, suppress → defer to LDA. Both reference
emulators see `_prevRunIrq == true` (cycle-1 end already high), *no*
suppression → fire at BCC's penultimate normally.

T+=03 is exactly that case: the frame IRQ's rising edge, aligned by
sync_apu and `begin`'s delay, falls inside BCC's cycle 1.

---

## 4. Phase-6 interaction — ruled out

The Phase-6 split (`bus.rs:175-208`) moved the APU tick from
`tick_post_access` to `tick_pre_access`. That affects WHEN
`bus.irq_line` is recomputed relative to the CPU's bus access, but it
does **not** shift what `bus.prev_irq_line` represents at the poll
point: `prev_irq_line` is still captured as the FIRST action of
`tick_pre_access`, so it holds the IRQ line "going in" to the current
cycle = end of previous cycle = what the reference emulators call
`_prevRunIrq`/`.before`.

In other words, our `bus.prev_irq_line` at poll-end is semantically
equivalent to Mesen2's `_prevRunIrq` at `Exec()` end-of-instruction.
The problem is solely in *which* older snapshot we compare it
against — `irq_line_at_start` (too old by 1 cycle) vs the correct
"just before the penultimate" sample.

### Regression guardrails for test_jmp / not-taken / pagecross

All three currently pass. The fix must only change the suppression
rule **inside the taken-no-cross branch path**, which is already
guarded by `self.branch_taken_no_cross` being set only in
`src/cpu/ops.rs:303-307`. JMP (`0x4C`, `0x6C`) doesn't touch that
flag, nor does a not-taken branch or a page-crossing branch. So the
fix cannot regress the three passing subtests.

---

## 5. Fix direction (concrete, minimal, single commit)

**Replace the "was the IRQ low at instruction start?" snapshot with a
"was the IRQ low at end of cycle N-2?" snapshot, captured inline in
`branch()`.** Equivalent to what Mesen2 / puNES do.

### Option A — capture inside `branch()`

`src/cpu/ops.rs:291-309` becomes (sketch):

```rust
fn branch(cpu: &mut Cpu, bus: &mut Bus, condition: bool) {
    let offset = cpu.fetch_byte(bus) as i8;   // cycle 2 done here
    if condition {
        // Snapshot: bus.prev_irq_line right now == IRQ line at end of
        // cycle 1 (= Mesen2 _prevRunIrq at BranchRelative check).
        let irq_low_before_penult = !bus.prev_irq_line;
        bus.read(cpu.pc);                      // cycle 3 dummy read
        let new_pc = (cpu.pc as i32).wrapping_add(offset as i32) as u16;
        let page_crossed = (cpu.pc & 0xFF00) != (new_pc & 0xFF00);
        if page_crossed {
            let bad = (cpu.pc & 0xFF00) | (new_pc & 0x00FF);
            bus.read(bad);
        } else if irq_low_before_penult {
            // IRQ was low at end of cycle 1; if it latches at
            // penultimate, the latch was a cycle-2 rising edge →
            // suppress this instruction's poll.
            cpu.mark_branch_taken_no_cross();
        }
        cpu.pc = new_pc;
    }
}
```

And in `src/cpu/mod.rs` remove `irq_line_at_start` entirely:
`poll_interrupts_at_end` simplifies to

```rust
let suppress = self.branch_taken_no_cross && bus.prev_irq_line;
self.branch_taken_no_cross = false;
if bus.prev_irq_line && !i_for_poll && !suppress {
    self.pending_interrupt = Some(Interrupt::Irq);
}
```

(Drop the `!self.irq_line_at_start` factor and the field itself.)

### Option B — inline suppression like Mesen2

More faithful to the reference, but requires `poll_interrupts_at_end`
to be refactored so the branch can call it directly to clear a
pending-latch flag. Option A is the smaller change.

### Why Option A is correct

- `bus.prev_irq_line` at the moment after `fetch_byte` returns == end
  of cycle 1 state.  Proof:
  - `fetch_byte` → `bus.read` → `tick_pre_access` at the start of the
    read, which does `self.prev_irq_line = self.irq_line` *before*
    the APU/mapper tick for this cycle (`src/bus.rs:176,205-207`).
  - The `self.irq_line` captured there is what it was at the end of
    the previous cycle.
  - For the operand-fetch read (cycle 2), "previous cycle" = cycle 1.
    So at function entry to the read, `bus.prev_irq_line` = end of
    cycle 1 — and it stays that value until the NEXT `tick_pre_access`
    (which is the dummy read's `bus.read(cpu.pc)` below).
- The quirk set-condition becomes "taken, no page cross, IRQ was low
  at end of cycle 1" — **identical** to Mesen2 and puNES.

### Test surface

Currently the `taken_no_cross_branch_delays_irq_by_one_instruction`
unit test (`src/cpu/mod.rs:323-371`) passes under our incorrect rule
because it calls `bus.apu.set_frame_irq_for_test(true)` BETWEEN
`CLC.step()` and `BCC.step()`, yet `bus.irq_line` is only refreshed
inside the next `tick_pre_access`. The test comment even documents
that the IRQ "goes high during BCC" but treats both a cycle-1 and a
cycle-2 rise as equivalent.

Under the fix (Option A), this specific timing will **stop
suppressing** — `bus.prev_irq_line` at the snapshot point will be
`true` (the cycle-1 tick already set the APU frame IRQ high during
BCC's opcode fetch), so `!bus.prev_irq_line` is false and the flag
isn't set. **This test will need to be rewritten** to actually
arrange a cycle-2 rising edge — e.g. schedule the frame IRQ to assert
precisely at BCC's operand-fetch cycle. Simplest path: run enough
CLC-prefix instructions so that `sync_apu`-like alignment lands the
rising edge within cycle 2.

The counter-example test
(`branch_not_taken_does_not_delay_irq`) stays valid: a not-taken
branch never sets `branch_taken_no_cross`.

### Regression surface

- `apu_test/rom_singles/1-len_ctr.nes` … `8-dmc.nes` — none of them
  exercise the branch-delays quirk; they care about APU latch
  timing. Should be unaffected.
- `cpu_interrupts_v2/rom_singles/1-cli_latency.nes`,
  `2-nmi_and_brk.nes` — neither tests the quirk; both should stay
  green.
- `cpu_interrupts_v2/rom_singles/3-nmi_and_irq.nes` — separate root
  cause (NMI/IRQ interleave alignment); this fix doesn't help or
  hurt it.
- `cpu_interrupts_v2/rom_singles/4-irq_and_dma.nes` — DMA stall
  semantics; unrelated.
- `instr_test-v5/official_only.nes` — doesn't test interrupt timing
  per opcode; unaffected.
- `blargg_apu_2005/.../03,07,08.nes` (APU IRQ-timing suite, out of
  scope until audio path is wired, but they do test IRQ polls) —
  **could** be sensitive; the fix tightens the suppression window
  (suppresses fewer IRQs), so any ROM relying on the "too-eager"
  behavior could regress. Running them on the phase-5 branch before
  merging is prudent.

---

## 6. Walking the T+=03 case end-to-end (for confidence)

Reconstructed timeline for iteration 3 of `test_branch_taken`:

1. `begin` returns → `RTS` lands PC at CLC@$03. By construction of
   `delay_a_25_clocks` with A = 3 XOR $0F = $0C = 12, the delay is
   37 CPU cycles + the subsequent `delay 29788-15`. Net effect: the
   frame-counter IRQ **rising edge** falls into what becomes cycle 1
   of BCC@$04 (one cycle later than T+=02, which landed the rising
   edge into CLC's tail / BCC's opcode fetch boundary — see §1 of
   readme for the hand-verified offset).

2. CLC@$03 runs. 2 cycles. At its penultimate (end of cycle 1), the
   frame IRQ has NOT yet risen (arrival is 1 cycle after T+=02).
   `bus.prev_irq_line = false`, poll doesn't fire. CLC completes,
   PC=$04.

3. `Cpu::step` begins for BCC. `irq_line_at_start` snapshot taken:
   `bus.irq_line` = false (IRQ still hasn't risen). **This is our
   bug** — by the time BCC's cycle 1 tick finishes, the IRQ **has**
   risen, but our snapshot is frozen at "before cycle 1 even
   started".

4. BCC cycle 1 (opcode fetch $90): `tick_pre_access` snapshots
   `prev_irq_line = false`, then APU tick → `irq_line = true` (frame
   IRQ just hit). End of cycle 1 state: line HIGH.

5. BCC cycle 2 (operand fetch $02): `tick_pre_access` snapshots
   `prev_irq_line = true` (end of cycle 1 state, HIGH), then APU tick
   (no further change) → `irq_line = true`. End of cycle 2 state:
   HIGH.

6. `branch()` evaluates condition (carry clear, take the branch).
   Calls `bus.read(cpu.pc)` — dummy read cycle 3: `tick_pre_access`
   snapshots `prev_irq_line = true`, `irq_line = true`. Page not
   crossed → sets `branch_taken_no_cross = true`.

7. `poll_interrupts_at_end`:
   - `bus.prev_irq_line` = true (end of cycle 2, penultimate — correct).
   - `irq_line_at_start` = false (captured at step() entry — **stale**).
   - `suppress_by_branch = true && !false && true = true`. **We
     suppress.** IRQ deferred.

8. LDA$100 runs next; at its penultimate (end of cycle 3), the IRQ is
   still held high (APU level-asserted). Latch fires. Service runs,
   pushing PC=$0A.

The correct (Mesen2-matching) behavior at step 7 would be to compare
against the **end-of-cycle-1** snapshot (= `bus.prev_irq_line` at the
time `branch()` returns from `fetch_byte`). That value is **true**,
so the quirk condition is *not* met, no suppression, BCC's penultimate
poll fires, pushed PC = $07. Matches the expected row.

---

## 7. Summary

| subtest                         | verdict | diff                                                |
|---------------------------------|---------|-----------------------------------------------------|
| `test_jmp`                      | PASS    | —                                                   |
| `test_branch_not_taken`         | PASS    | —                                                   |
| `test_branch_taken_pagecross`   | PASS    | —                                                   |
| `test_branch_taken`             | FAIL    | 1 row: T+=03 expected `02 07`, observed `06 0A`     |

Root cause: **one-cycle-too-wide suppression window** in the
branch-delays-IRQ quirk. Our snapshot `irq_line_at_start` captures
the IRQ line at **instruction entry** (end of cycle 0). The correct
reference is **end of cycle 1** (= `bus.prev_irq_line` at the moment
the branch's operand fetch returns). Capturing the later snapshot
inside `branch()` and testing `!that_snapshot` in place of
`!irq_line_at_start` reproduces Mesen2's `_prevRunIrq` /
puNES's `.before` semantics exactly.

Files touched by the fix:
- `/home/marcus/Git/vibenes2/src/cpu/ops.rs` — capture
  `bus.prev_irq_line` inside `branch()` after `fetch_byte`, gate the
  `mark_branch_taken_no_cross()` call on `!that`.
- `/home/marcus/Git/vibenes2/src/cpu/mod.rs` — remove
  `irq_line_at_start` field and its uses; simplify
  `poll_interrupts_at_end`'s suppression expression to just
  `self.branch_taken_no_cross && bus.prev_irq_line`.
- `/home/marcus/Git/vibenes2/src/cpu/mod.rs` (tests) — rewrite the
  `taken_no_cross_branch_delays_irq_by_one_instruction` unit test
  so the IRQ's rising edge lands in the branch's **cycle 2**, not
  cycle 1 (which no longer suppresses under the corrected rule).

CLAUDE.md guidance to update after the fix lands:

- Remove the "test_jmp shares root cause with test 3" claim; they
  don't.
- Phase-5 Sub-B section can be marked complete once the fix passes
  `cpu_interrupts_v2/5-branch_delays_irq` and the existing suite
  (apu_test 1-8, apu_reset all 6, instr_test-v5 official_only,
  instr_misc, cpu_interrupts 1-2).
