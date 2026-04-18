# Phase 9 — DMC DMA conflicts (double-read, $2007 double-read, sprite+DMC)

Read-only investigation of three failing ROMs that all exercise the DMA-meets-CPU-read edge cases. Findings map each test failure to a root cause, then describe a concrete minimal fix (files/lines only — no code written). Reference emulators cited by file:line.

---

## 1. Background — DMA taxonomy (in our own words)

### 1.1 DMC DMA stall timing (NTSC)

A DMC sample fetch is a bus-master operation; when the APU's DMC unit refills its empty buffer it asserts RDY, the CPU halts, and the DMA controller runs its own read. The number of CPU cycles "stolen" depends on the phase of the CPU instruction when the DMA fires and whether sprite DMA is already in progress. puNES codifies four cases (`apu.h:211-220`, enum `dmc_types_of_dma`):

| case | cycles | when |
|---|---|---|
| `DMC_NORMAL` | 4 | standard case — DMA fires during or right after an idle CPU read |
| `DMC_CPU_WRITE` | 3 | DMA fires on a CPU *write* cycle (halt is compressed because the CPU isn't fighting for the bus on the same cycle) |
| `DMC_R4014` | 2 | DMA fires while OAM DMA is in progress (OAM DMA's existing stall cycles absorb the halt/dummy) |
| `DMC_NNL_DMA` | 1 | DMA fires on the penultimate OAM DMA cycle — only the DMC read itself is left |

Nesdev restates this as "4 cycles typical, 3/2/1 in the escape cases; +1 alignment cycle on NTSC put-cycle starts" (`reference/apu.md:156-161`).

Inside the 4-cycle `DMC_NORMAL` case, the cycles are:

1. **Halt** — CPU's RDY is pulled low; DMA replays the CPU's pending read address on the bus (the bus is still driven by the halting CPU; the halt itself looks like "a second read at the same address"). This is the cycle that produces the controller double-read bug.
2. **Dummy** — DMA issues another read at the CPU's pending read address (again bus-visible).
3. **Align** — only if the DMC read alignment is off by one cycle; inserts one more dummy read at the CPU's pending address.
4. **DMC read** — the actual sample byte fetch at `DMC.current_addr` (lives in `$8000..=$FFFF`).

Mesen2 encodes this as `_needHalt` + `_needDummyRead` booleans consumed in `NesCpu::ProcessPendingDma` (`NesCpu.cpp:325-448`). The halt cycle calls `_memoryManager->Read(readAddress, MemoryOperationType::DmaRead)` at `NesCpu.cpp:363` — `readAddress` is the CPU's pending read target, passed in as a parameter from `MemoryRead` (`NesCpu.cpp:261`). Every "still halting / still dummy" cycle inside the loop at `NesCpu.cpp:423-425, 441-443` also re-reads `readAddress`. Nestopia does the equivalent with `cpu.Peek(readAddress)` in `NstApu.cpp:2322-2328`, explicitly noting "DMC DMA during read causes 2-3 extra `$2007` reads before the real read".

### 1.2 Replaying the pending CPU read address

The DMC DMA halt/dummy/align cycles all perform a **real bus read** at the CPU's pending read address. Key citations:

- **Mesen2** passes `readAddress` as a parameter: `NesCpu::ProcessPendingDma(uint16_t readAddress, MemoryOperationType opType)` (`NesCpu.cpp:325`). Upstream call is `NesCpu::MemoryRead` (`NesCpu.cpp:261`), which threads the CPU's pending `addr` through. Halt cycle reads at `NesCpu.cpp:363`. Inside the while-loop the same `readAddress` is the source for the dummy-read arm at `NesCpu.cpp:424` (halt/dummy still pending during `getCycle`) and `NesCpu.cpp:442` (alignment during a "put" cycle).
- **puNES** tracks `DMC.dma_cycle = cpu.opcode_cycle` at the moment DMA is armed (`apu.h:232`); `$4016`/`$4017` read handlers then check `DMC.dma_cycle == 2` and perform an extra `input_rd` call (`input/nes_001.c:53-55`, `input/famicom.c:91-93`). The `$2007` path is symmetric: `cpu_inline.h:332-333` (`if (DMC.dma_cycle == 2) { repeat = 3; }`) and the `double_rd` branch at `cpu_inline.h:334-346` for page-cross double reads.
- **Nestopia** takes a `readAddress` parameter on `Apu::Dmc::DoDMA` (`NstApu.cpp:2282`) and explicitly issues `cpu.Peek(readAddress)` 2–3 times at `NstApu.cpp:2322-2327` when the DMA collides with a read cycle.

Our emulator (`src/bus.rs`) has **no equivalent**. `Bus::read` calls `service_pending_dmc_dma()` at line 109 (takes no `addr` parameter) and `service_pending_dmc_dma` just runs `tick_cycle()` four times (`src/bus.rs:335-341`) with no bus reads at the CPU's pending address. Comment at `src/bus.rs:332-334` explicitly flags this as deferred work: *"We do not replay the CPU's pending read here; the controller/MMIO double-read bug and its interaction with $4016/$4017 is a later phase."* — that phase is now.

---

## 2. Failure A — `dma_4016_read.nes`

Source: `~/Git/nes-test-roms/dmc_dma_during_read4/source/dma_4016_read.s`.

Expected: `08 08 07 08 08` (one iteration out of five loses one bit because the DMC halt-cycle extra read of `$4016` consumes it).
Our output: `08 08 08 08 08` (no bit deletion).

### 2.1 Root cause

Our `Bus::service_pending_dmc_dma` at `src/bus.rs:324-344` inserts four idle CPU cycles via `tick_cycle` but never replays the CPU's pending read address on the bus. The test relies on the halt cycle re-reading `$4016` as a bus read — that hits `Controller::read` which shifts the register by one bit (`src/bus.rs:67-74`, unconditional shift when `!strobe`). Without the replay the shifter keeps its 8 bits.

The iteration that should show `07` is the one where DMA's halt cycle aligns exactly with the cycle on which `lda $4016` was about to happen; the four other iterations push the halt into surrounding cycles where no external register responds.

### 2.2 Concrete minimal fix

1. **`src/bus.rs:108` (signature change to `Bus::read`)** — thread the CPU's pending `addr` down into `service_pending_dmc_dma`. New signature: `fn service_pending_dmc_dma(&mut self, pending_addr: u16)`.
2. **`src/bus.rs:324-344` (body of `service_pending_dmc_dma`)** — replace each of the three `tick_cycle()` halt/dummy/align stalls with a real bus read at `pending_addr`. The read must NOT re-enter DMC DMA servicing (we're already inside it — the `dmc_dma_active` guard at `src/bus.rs:325-327` already handles this, good). The simplest shape is a helper like `dma_dummy_read_at(pending_addr)` that mirrors the `match addr {...}` dispatch in `Bus::read` but does NOT call `service_pending_dmc_dma`, does NOT update `open_bus` the same way, and still runs the pre/post PPU tick split.

   Reference the Mesen2 loop at `NesCpu.cpp:399-447` — note that `_memoryManager->Read(readAddress, MemoryOperationType::DmaRead)` is used, which is a real read with side effects. The one tricky case Mesen2 handles at `NesCpu.cpp:357-364` and `NesCpu.cpp:487-508` is "two sequential reads of `$4016`/`$4017` from the same instruction should only consume one bit" (controller shifter open-bus merge). Real hardware has this quirk because `/OE` on the controller stays asserted across the two conflicting reads. On Famicom the shifter sees every read; on NES only the first. For a first pass we can ignore the NES-quirk and just replay unconditionally — the test we're trying to pass (`dma_4016_read.nes`) expects a single bit to drop per DMA-aligned iteration, and puNES confirms it at `input/nes_001.c:53-55` by checking `DMC.dma_cycle == 2` and doing one extra `input_rd`.

3. **Two-path fidelity lift (optional for A, required for C)** — if pursuing Mesen2's full model, the `_needHalt`/`_needDummyRead` booleans and the "run until both cleared" while-loop mirror `NesCpu.cpp:399-447`. This generalizes to the OAM-DMA interleave (fix C). For the 4016 test alone, a simpler structure (replay 3 times, then do the DMC read) suffices.

### 2.3 Regression risk

- **`instr_test-v5/official_only.nes`** — low. These tests don't enable DMC. DMC DMA never fires, `service_pending_dmc_dma` returns at the `take_dmc_dma_request` None branch.
- **`apu_test` 1–8** — medium. `apu_test/rom_singles/5-len_timing_mode0.nes` and `6-len_timing_mode1.nes` don't use DMC either. But `7-dmc_basics.nes`-like tests DO use DMC and will now see extra bus reads during halts. Mitigation: the replay reads must hit `Bus::read` pathways that the test scaffold (mostly reads of ROM / `$4015`) tolerates. `$4015` reads during the halt DO have a side effect — they clear frame IRQ. This is real hardware behavior and matches Mesen2 (`NesCpu.cpp:478-484` explicitly handles it).
- **`dmc_dma_during_read4/double_2007_read.nes`** — this test may change behavior when the replay reads at `$2007` start advancing the buffer. Needs to be considered together with fix B.
- **`cpu_interrupts_v2/4-irq_and_dma.nes`** — this test currently passes. It uses OAM DMA *and* DMC DMA. If the DMC fix changes the total cycle count (it might — instead of 4 `tick_cycle` calls doing no bus access, we now do 3 bus reads which still cost one CPU cycle each), net cycle count is the same. Should not regress.

### 2.4 Implementation order

A is independent of B and C in isolation. B piggy-backs on the same `pending_addr` plumbing (see §3). Do the plumbing once; turn on the replay in the fix for A, then write the PPU `$2007` detection in the fix for B on top.

---

## 3. Failure B — `double_2007_read.nes`

Source: `~/Git/nes-test-roms/dmc_dma_during_read4/source/double_2007_read.s`.

### 3.1 Test structure recap

1. `begin:` fills VRAM[$0000..$0007] with `00 11 22 33 44 55 66 77 88`, sets PPUADDR=$0001, primes the `$2007` buffer by reading once (buffer ← VRAM[$0001]=$22, v→$02), returns.
2. **Run 1**: `ldx #$00`, `lda $20F7,X`. X=0, no page cross; one $2007 read at effective=$20F7 (mirror of $2007). Returns buffer=$22, v→$03. Then `end:` prints A and reads 4 more: `33 44 55 66`. Output: `22 33 44 55 66`.
3. **Run 2**: `begin:` re-primes; `ldx #$10`, `lda $20F7,X`. X=$10, page cross from $20F7+$10=$2107. CPU issues the dummy read at `$2007` (un-fixed high byte) followed by the real read at `$2107` (also mirrors to $2007).

Expected CRC is one of four, corresponding to output patterns:
- `22 33 44 55 66` / `22 44 55 66 77`
- `22 33 44 55 66` / `22 33 44 55 66`
- `22 33 44 55 66` / `02 44 55 66 77`
- `22 33 44 55 66` / `32 44 55 66 77`

All four share the property that **the first byte of run 2 is `$22`, `$02`, or `$32`** — never `$33`. Our output `22 33 44 55 66` / `33 44 55 66 77` has `$33` as the first byte of run 2, which is outside the expected bucket.

### 3.2 Root cause

Our CPU implementation at `src/cpu/ops.rs:32-42` (`addr_abs_indexed_read`) correctly issues the bus-visible dummy read at the un-fixed-high address on page cross. Our PPU implementation at `src/ppu.rs:900-912` correctly handles a single `$2007` read (buffered return + buffer refill from v + v increment). So when the CPU does dummy+real at $2007, we do TWO full $2007 reads and advance the buffer TWICE.

Real hardware does not handle it that way. The PPU's data latch is shared between the CPU and its internal fetch path; two back-to-back CPU reads of $2007 within one instruction create a bus conflict that:

- Does NOT fully advance the buffer twice (the fetch for the second read has not completed).
- Leaves the buffer in an indeterminate state — the observed return value can be the pre-existing buffer ($22), the buffer minus two increments ($02 = VRAM[v-2*inc] bits?), or partial bits of two competing values ($32 = $22 OR'd/ANDed with something).
- Advances `v` by one step in each of the two reads (VRAM-address pointer), so subsequent reads return `44 55 66 77` or `33 44 55 66` depending on which of the two reads actually fired the `v++`.

puNES models the ambiguity at `cpu_inline.h:334-346` with a randomized branch:

```c
} else if (nes[nidx].c.cpu.double_rd) {
    WORD random = (WORD)emu_irand(10);
    value = ppu_rd_mem(v - 2 * inc);   // returns $22 = VRAM[v-2*inc] = VRAM[$01]
    if (random > 5) {
        ppu.r2007.value = ppu_rd_mem(v);  // sometimes updates buffer
        r2006_during_rendering();
    }
    return value;
}
```

Key detail: puNES sets `cpu.double_rd = TRUE` in the `_CY_` macro (`cpu.c:343-348`) which is the per-opcode page-cross hook for `abs,X`/`abs,Y`/`ind,Y` read instructions. So the PPU is told by the CPU "this is the *second* of a paired read at the same address; give me a special result". Nestopia's `Apu::Dmc::DoDMA` references `dmc_dma_during_read4/dma_2007_read` (`NstApu.cpp:2317-2328`) and issues `cpu.Peek(readAddress)` 2-3 times unconditionally — that causes the buffer to advance MORE than twice (a specific different bucket of the "depends on sync" list).

### 3.3 Concrete minimal fix

Two-part fix, both required:

1. **CPU → PPU signaling for page-cross double reads at $2007**. Introduce a per-step flag on `Cpu` (say `pending_page_cross_dummy: Option<u16>`) that `addr_abs_indexed_read` (`src/cpu/ops.rs:32-42`), `addr_abs_indexed_rmw` (`src/cpu/ops.rs:44-51`), and `addr_ind_y_read` (`src/cpu/ops.rs:62-73`) set before emitting the dummy read, and clear after the real read. The PPU's `$2007` handler (`src/ppu.rs:900-912`) checks it — or a side-band on `Bus` — and on the second read of a paired page-cross sequence, returns `self.ppu_bus_read(self.v - 2 * inc)` WITHOUT advancing the buffer a second time.

   Exactly which of the four accepted buckets we produce depends on whether we randomize (puNES) or pick a fixed outcome (Mesen2 / Nestopia). For a deterministic test we want one of the four accepted patterns consistently. The simplest picks:
   - Mesen2-alike (2 extra reads then real): output `22 33 44 55 66` / `22 44 55 66 77` → CRC `85CFD627`. This corresponds to "buffer returns previous buffer, then v has advanced by 1 (not 2), so next reads start from `44 55 66 77`". Practically: the dummy read returns buffered $22 WITHOUT advancing the buffer; the real read advances buffer normally. Actually no — to get `22 44 55 66 77` the *return from the real read* must be $22 and then the FOUR subsequent reads produce `44 55 66 77`; total v increments = 4 starting from initial (v=$02 after `begin:`). Works if dummy+real jointly advance v by TWO but return value is the (stale) pre-dummy buffer $22. This matches puNES's `double_rd` path when the `random > 5` branch takes the buffer-update (so the internal buffer is refilled for the NEXT real-read sequence, i.e. by the time `end:` reads the first of its 4, the buffer already points to VRAM[$04]=$44).

2. **Do NOT double-apply the DMC DMA pending-read replay on the same cycle.** If fix A replays `pending_addr` 3 times during a DMC halt, and that `pending_addr` happens to be a $2007 read, we would advance the buffer 3 extra times from the halt + 2 more from the real double-read. Real hardware's behavior for this compound is "depends on CPU-PPU sync" (test `dma_2007_read.s` exists but is not one of the three we're fixing here). Check `dma_2007_read.nes` behavior after fix A+B and either accept a CRC from its expected set or gate the replay to non-$2007 addresses for test determinism. Mesen2 replays at $2007 unconditionally (`NesCpu.cpp:423-425`); so does Nestopia (`NstApu.cpp:2322-2325` with the explicit comment about $2007). We should too — expect `dma_2007_read.nes` to produce one of its own 4 CRC buckets afterwards.

**File + line ranges**:

- `src/cpu/ops.rs:32-42` (`addr_abs_indexed_read`) — add flag raise before line 39's `bus.read(bad);` and clear after the caller's real read. Same for `src/cpu/ops.rs:62-73`.
- `src/ppu.rs:880-917` (`cpu_read`) — add the "double-read" branch inside the `0x07` arm. Prefer reading `self.v.wrapping_sub(2 * inc) & 0x3FFF` to match puNES; gate on a new flag on `Ppu` or a `&mut` parameter.
- `src/bus.rs:113` — if the flag lives on `Bus`, wire it so `cpu_read` can see it. A field `pub pending_page_cross_target: Option<u16>` on `Bus` is the cleanest hand-off; the CPU sets it, `Bus::read` forwards the info to `Ppu::cpu_read`, caller clears after the real read.

### 3.4 Regression risk

- **`instr_test-v5/official_only.nes`** — tests plenty of `abs,X`/`abs,Y` reads but NOT targeting PPU registers. Flag is inert outside $2007; safe.
- **Any PPU graphics test (sprite_hit, scroll, etc.)** — games that legitimately use `lda $20F7,X` with X forcing page-cross to $2007 would have to rely on the double-read quirk. No known regression target; this is a rare opcode/addr combo in real games.
- **`dma_2007_read.nes`** — see §3.3 item 2. Acceptable if it moves into its expected CRC bucket after the fix; needs verification.
- **`cpu_interrupts_v2/*`** — tests all hit ZP or abs addressing for interrupt vectors; no page-cross $2007 path.

### 3.5 Implementation order

B depends on the pending-address plumbing from A only in the sense that both add a "CPU tells Bus/PPU about a pending or in-flight read address" signal. B's signal is slightly different (per-instruction, marks the dummy of a paired read), so they're additive, not duplicate. Order: **A first** (halt-cycle replay; gives us the `pending_addr` threading), then **B** (layer the double-read flag on top).

---

## 4. Failure C — `sprdma_and_dmc_dma.nes` / `sprdma_and_dmc_dma_512.nes`

No source in `~/Git/nes-test-roms/sprdma_and_dmc_dma/` (ROM only). The test exercises OAM DMA + DMC DMA overlap and prints a table of cycle counts. Our table's tail shows `09 528 / 0A 529 / 0B 528 / 0C 529 / 0D 528 / 0E 529 / 0F 528` — alternation between 528 and 529.

### 4.1 Root cause

Expected: a table of per-iteration cycle counts that depends on the OAM-DMA-start parity AND the cycle at which DMC DMA requests arrive. The 528/529 alternation in our output almost certainly reflects the `extra_idle = (cpu_cycles() & 1) == 0` parity check at `src/bus.rs:183` firing on alternating iterations because the iteration delta between the ROM's test loops is odd.

More specifically: our `Bus::run_oam_dma` at `src/bus.rs:297-311` runs OAM DMA as `tick_cycle + [optional tick_cycle] + 256 * (read + write)`. It does NOT call `service_pending_dmc_dma` inside that loop. So if DMC DMA requests arm during OAM DMA, the request stays buffered and fires only when OAM DMA finishes — and it fires as a standalone 4-cycle stall on the first post-OAM read. That's wrong. Real hardware interleaves DMC DMA inside OAM DMA:

- OAM DMA run as "halt + [align] + 256 × (read cycle + write cycle)".
- During OAM DMA, if DMC asserts RDY, the DMC read piggy-backs on one of the OAM-DMA read cycles (DMC reads, OAM DMA waits one cycle). Mesen2 implements this by converting OAM DMA's "read cycle" into a DMC-read cycle when `_dmcDmaRunning && !_needHalt && !_needDummyRead` is true (`NesCpu.cpp:402-411`), with the halt/dummy cycles absorbed into the surrounding sprite-DMA dummy reads (`NesCpu.cpp:412-418`, `438-445`).

Under real hardware: expected total cycle count is mostly a fixed 520/528 depending on starting parity — NOT alternating 528/529 per iteration, because the DMC timing gets re-aligned by the OAM DMA's own cycle stream. Our implementation runs OAM DMA as an opaque 513/514-cycle block, then pends a DMC fetch for "afterward", which pushes the total count up by +4 per "hit" and leaves parity in the mix — hence 528=513+1+14(dmc ok but odd) / 529=514+1+14(dmc+align) (rough; the specifics depend on where in the iteration DMC fires).

Additional signal: **phase 7's `extra_idle = even` inversion** (`src/bus.rs:183` plus the test at `src/bus.rs:394-407`) may be wrong for this test. Mesen2's parity is measured inside the DMA loop (`_state.CycleCount & 0x01 == 0` meaning "get cycle"), not based on the cycle before DMA starts. Our logic that "extra_idle when cpu_cycles is EVEN" was tuned for `4-irq_and_dma.nes` — if that tuning mispredicts the halt parity in the sprite+DMC race, it will explain one of the two alternations.

### 4.2 Concrete minimal fix

This is the largest fix of the three. Shape:

1. **`src/bus.rs:297-311` (`run_oam_dma`)** — replace the opaque 513/514 loop with an explicit "get/put cycle" loop (mirroring Mesen2's while-loop at `NesCpu.cpp:399-447`). Inside the loop, check DMC request state on every "get" cycle and, if pending and past halt/dummy, use that cycle as the DMC read. OAM DMA's own read (for the current sprite byte) gets postponed one cycle.

2. **`src/bus.rs:324-344` (`service_pending_dmc_dma`)** — factor the halt/dummy/align/read phases into callable units so `run_oam_dma` can consume them interleaved with OAM DMA's bus activity. Model as `_needHalt` + `_needDummyRead` booleans (Mesen2 shape), decremented from 1 to 0 each time a suitable OAM DMA dummy/get cycle runs. Reference: `NesCpu.h:39-43` (`_needHalt`, `_needDummyRead`) and `NesCpu.cpp:527-548` (`StartDmcTransfer`/`StopDmcTransfer`).

3. **Re-test the parity invert at `src/bus.rs:183`**. After (1) + (2), the cycle-count table for `4-irq_and_dma.nes` may change (the DMC-in-OAM-DMA path now compresses what used to be `+4` to `+2` or less). If the test regresses, the parity choice needs re-derivation; the current `extra_idle = cpu_cycles() & 1 == 0` was tuned assuming no interleave. Likely new rule: `extra_idle = _state.CycleCount & 0x01 == 1` at start of OAM DMA, matching Mesen2's get/put parity directly.

**File + line ranges**:

- `src/bus.rs:297-311` — rewrite `run_oam_dma`.
- `src/bus.rs:324-344` — restructure `service_pending_dmc_dma` into callable phase chunks.
- `src/bus.rs:183` — revisit parity.
- `src/bus.rs:377-407` — unit tests may need updating (`oam_dma_halt_on_get_runs_513_beyond_sta` / `oam_dma_halt_on_put_runs_514_beyond_sta`). These encode the no-DMC baseline; the new interleaved model must still produce 513/514 when DMC is idle.

### 4.3 Regression risk

- **High — this is the riskiest fix.** Any change to OAM DMA cycle counts breaks `cpu_interrupts_v2/4-irq_and_dma.nes` by one cycle, which is the test we tuned `extra_idle` for originally (per `src/bus.rs:179-183` and the `CLAUDE.md` §Phase 5 narrative). Plan: branch off `main` (per CLAUDE.md "Branch for anything that touches every opcode path or changes the bus ↔ CPU interface"), pin each baseline test after each step.
- **`apu_test/rom_singles/7-dmc_basics.nes`** — direct DMC interaction; medium risk.
- **`cpu_interrupts_v2/4-irq_and_dma.nes`** — high risk, already pass. Must stay pass.

### 4.4 Implementation order

C depends on A (the halt/dummy cycles need the `pending_addr` replay plumbing to act like Mesen2's `NesCpu.cpp:423-425` reads inside the loop). C is independent of B (B is a PPU-side signal). Order: **A → C → B**, or **A → B → C** — both work; I'd pick A → B → C so the smaller surgical fix (B) lands and gets tested before the larger OAM-DMA rewrite (C).

---

## 5. Cross-dependencies summary

| dependency | A → B | A → C | B → C |
|---|---|---|---|
| same plumbing? | partial (both thread `pending_addr`) | yes (C re-uses halt/dummy replay) | no |
| conflict? | A's replay at $2007 changes double-read buckets | no | no |
| required order | A then B | A then C | independent |

Recommended sequence:

1. **A first** — plumb `pending_addr` into `service_pending_dmc_dma`; add 3 halt/dummy replays; get `dma_4016_read.nes` passing.
2. **B second** — add the CPU→PPU double-read flag, new $2007 branch in `Ppu::cpu_read`; accept one of the 4 `double_2007_read.nes` CRC buckets. Verify `dma_2007_read.nes` lands in its expected set (it is not in the current failing list — it's presumably already accepted or passing).
3. **C third** — OAM-DMA interleave rewrite with `_needHalt`/`_needDummyRead` semantics. Re-verify parity choice at `src/bus.rs:183` against `4-irq_and_dma.nes`, 7-dmc_basics, and sprdma_and_dmc_dma[_512].

At each step, run the phase-5 checklist from `CLAUDE.md` (instr_test-v5, instr_misc, apu_test 1-8, apu_reset, cpu_interrupts_v2) and STOP if anything below the current step regresses.

---

## 6. Reference line index

### Mesen2 (`~/Git/Mesen2/Core/NES/`)

- `NesCpu.h:39` — `_needHalt` boolean.
- `NesCpu.h:43` — `_needDummyRead` boolean.
- `NesCpu.h:820` — `StartDmcTransfer()` declaration.
- `NesCpu.cpp:254-268` — `MemoryRead` threads the pending `addr` into `ProcessPendingDma`.
- `NesCpu.cpp:261` — `ProcessPendingDma(addr, operationType)` call with pending read address.
- `NesCpu.cpp:325-448` — `ProcessPendingDma` full body.
- `NesCpu.cpp:340-347` — "skip first input clock" logic for DMA-on-$4016/$4017 on-same-address races.
- `NesCpu.cpp:349-364` — halt cycle: real memory read at `readAddress` with `MemoryOperationType::DmaRead`.
- `NesCpu.cpp:399-447` — get/put cycle loop covering interleaved DMC + OAM DMA.
- `NesCpu.cpp:402-411` — DMC read when halt/dummy already done.
- `NesCpu.cpp:412-418` — OAM DMA read during get cycle while DMC halt/dummy still pending.
- `NesCpu.cpp:419-427` — dummy read during get cycle while DMC waiting (halt/dummy).
- `NesCpu.cpp:428-446` — put cycle: OAM DMA write OR alignment dummy.
- `NesCpu.cpp:450-518` — `ProcessDmaRead` handling of internal $4015/$4016/$4017 double-read glitch.
- `NesCpu.cpp:527-532` — `StartDmcTransfer` sets `_dmcDmaRunning = _needDummyRead = _needHalt = true`.
- `NesMemoryManager.cpp:125-136` — the real read handler invoked by halt-cycle replays.

### puNES (`~/Git/puNES/src/core/`)

- `apu.h:25` — enum `dmc_types_of_dma { DMC_NORMAL, DMC_CPU_WRITE, DMC_R4014, DMC_NNL_DMA }`.
- `apu.h:209-247` — `dmc_tick` refill path; stores `DMC.dma_cycle = cpu.opcode_cycle` so later register-read handlers can detect "DMA is mid-halt".
- `apu.h:211-220` — the 4-3-2-1 stall-cycle switch.
- `apu.h:487` — `BYTE dma_cycle;` field.
- `cpu.c:343-348` — `_CY_` macro sets `cpu.double_rd = TRUE` on page-cross (`abs,X`, `abs,Y`, `ind,Y`).
- `cpu_inline.h:332-333` — `$2007` read during DMC DMA race: `repeat = 3` (triple-advance).
- `cpu_inline.h:334-346` — `double_rd` branch: `value = ppu_rd_mem(v - 2*inc)` and `if (random > 5) { update buffer }`.
- `input/nes_001.c:40-55` — `$4016` read triggers extra `input_rd` when `DMC.dma_cycle == 2`.
- `input/famicom.c:88-95` — Famicom equivalent of the same double-read (fires on every halt read, not just the first).

### Nestopia (`~/Git/nestopia/source/core/`)

- `NstApu.cpp:2282-2336` — `Apu::Dmc::DoDMA(cpu, clock, readAddress)`.
- `NstApu.cpp:2286-2311` — stall-cycle selection matching the puNES 4-3-2-1 taxonomy; explicit nesdev forum link in-source.
- `NstApu.cpp:2313-2328` — read-conflict branch with 2-3 `cpu.Peek(readAddress)` replays and the "$2007 double read" comment.

### Hardware reference (`~/.claude/skills/nes-expert/reference/`)

- `apu.md:156-161` — DMC DMA CPU-stall summary; controller double-read bug call-out.
- `apu.md:231` — "DMC DMA controller double-read: emulate it for `dmc_dma_during_read4`."
- `cpu.md:85-89` — page-crossing dummy reads are bus-visible; hits PPU/APU registers.
- `punes-notes.md:27-39` — the 4-way DMC DMA stall taxonomy with citations.
- `punes-notes.md:132-141` — `$2007` vs DMC race: triple-advance pattern.
- `punes-notes.md:143-145` — controller double-read randomization note.
- `punes-notes.md:147-149` — OAM DMA odd-cycle +1 is NTSC-only.

---

## 7. Open questions / verification notes

1. **`dma_2007_read.nes` current status**: this ROM is in `dmc_dma_during_read4/` alongside the failing `dma_4016_read` and `double_2007_read`, but was not listed as failing in the prompt. Either it passes accidentally (unlikely given B's underlying bug), or it was already listed as not-yet-attempted. Before implementing B, check its current CRC against the four documented expected CRCs.
2. **`read_write_2007.nes`**: also in `dmc_dma_during_read4/`, exercises DMC vs $2007 write path. Will react to fix A (halt-cycle replay on a write cycle is handled differently in Mesen2 — see `NesCpu.cpp:246-250` write path which does NOT call `ProcessPendingDma`). Confirm our implementation defers DMA to the next read cycle; `src/bus.rs:146-198` does not call `service_pending_dmc_dma` in `write` — good.
3. **`sprdma_and_dmc_dma` expected table values**: without the source, the expected `528` vs `513` target needs to be derived from passing it on a reference emulator once we're in the right ballpark. Prioritize getting A + C working and then tune to match.

---

End of investigation.
