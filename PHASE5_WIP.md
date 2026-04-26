# Phase 5: SMP / SPC700 + S-DSP

Audio subsystem. Independent SPC700 CPU + S-DSP audio chip running on
their own 24.576 MHz crystal, async to the 5A22. Sole contact with the
host CPU is the 4-byte `$2140-$2143` latch.

## Current state at the start of this phase

The 65C816 host CPU is architecturally complete (all 256 opcodes, all
24 addressing modes, BCD, mode switching, IPL boot signature stub for
APU ports). All 21 PeterLemon CPU test ROMs PASS via the headless
`snes_test_runner` (see `src/bin/snes_test_runner.rs`). NES regression
suite untouched and green.

What the SNES bus currently does:

- LoROM mapping at `src/snes/bus.rs::LoRomBus` (HiROM/ExHiROM warn but
  fall back to LoROM addressing - non-fatal for now)
- WRAM 128 KiB + low-bank mirror
- Frame counter on master clock with vblank-edge NMI delivery
- `$4200` NMITIMEN, `$4210` RDNMI, `$4212` HVBJOY (vblank + approximate
  hblank)
- `$4202/$4203` 8x8 multiplier and `$4204-$4206` 16/8 divider
- General-purpose DMA on `$420B MDMAEN` (modes 0-7, source-step,
  direction). HDMA is latched but not serviced.
- Slim Mode-1 BG1 renderer for headless test grading (256x224 RGBA)
- APU port latch with $AA/$BB IPL boot signature on the SMP-side; CPU
  writes go to a separate latch and are NOT echoed (no fake-SMP fiction)

What's missing on the bus side that this phase will touch:

- The `apu_smp_to_cpu` / `apu_cpu_to_smp` latches need to become real
  bidirectional ports between the host CPU and the new SMP. Replace the
  fixed boot-signature stub with the actual SMP-driven values.

What's pending on the host CPU side that this phase explicitly does
NOT touch (track in a future `PHASE_CPU_POLISH_WIP.md`):

- Indexed-addressing extra read on page cross (under-charged today)
- RMW dummy access (read, idle, dummy-read-or-write, write)
- Penultimate-cycle interrupt polling
- MVN/MVP mid-block interrupt resumption
- BCD ADC/SBC +1 cycle (Mesen2 omits, snes-cpu.md says yes - settle
  via op_timing_test)

These are off-roadmap for Phase 5. Don't conflate them.

## Why SMP next, not PPU or HDMA

- **Discrete subsystem.** Own ISA, own 64 KiB ARAM, own 64-byte IPL,
  own crystal, single 4-byte contact surface. Mirrors how we developed
  the 65C816 in isolation against `FlatBus`.
- **Single clean test gate.** `~/Git/snes-test-roms/PeterLemon/SNES-CPUTest-SPC700/`
  mirrors the 65C816 set: one ROM per instruction class. Plus
  `~/Git/snes-test-roms/blargg-spc-6/` for cycle-exact timing.
- **No entanglement.** Doesn't compete with remaining 65C816 polish,
  HDMA, full PPU. Parallel work.

## Architectural plan

### Module layout

```
src/snes/smp/
├── mod.rs           # Smp struct: registers, dispatch, IPL, ARAM
├── bus.rs           # SmpBus trait + FlatSmpBus test impl
├── ipl.rs           # 64-byte IPL boot ROM (vendored upstream blob)
├── timers.rs        # Three timers (T0/T1 8 kHz, T2 64 kHz)
└── tests.rs         # SPC700 unit tests

src/snes/dsp/
├── mod.rs           # Dsp struct: register file, voice state
├── voice.rs         # Per-voice state + envelope + BRR decoder
├── brr.rs           # BRR sample decode (9-byte block -> 16 PCM)
├── envelope.rs      # ADSR + GAIN envelope state machine
├── echo.rs          # Echo buffer + FIR filter
└── tests.rs         # DSP unit tests
```

The DSP is owned by the SMP (real hardware: DSP I/O via SMP's
`$00F2/$00F3` registers). Compose `Smp { dsp: Dsp, aram: [u8; 0x10000], ... }`.

### Bus integration

After SMP/DSP are correct in isolation:

- `LoRomBus` gains `smp: Smp` field.
- The `apu_smp_to_cpu` / `apu_cpu_to_smp` byte arrays become accessors
  on `Smp`'s port-latch state.
- `Snes::step_instruction` runs both CPUs to a sync point. Strategy:
  on every CPU access to `$2140-$2143`, run the SMP forward to "now"
  in master-clock equivalents. Plus a coarse periodic catch-up to bound
  drift between port accesses. This is the Mesen2 pattern; higan uses
  cooperative co-routines (libco) which we're not doing.

### Clock model

The SMP runs at 24.576 MHz / 24 = ~1.024 MHz instruction clock. NTSC
master is 21.477272 MHz. They drift. Pick a sync model:

- **Mesen2 pattern (recommended)**: track `smp_master_cycles` and
  `cpu_master_cycles`. On each `$2140-$2143` access, run the SMP
  forward until its master matches the CPU's, scaled by the clock
  ratio (24.576/21.477 = 1.144). This is fractional - track ratios
  with a fixed-point accumulator to avoid floating-point drift.
- **Coarse periodic catch-up**: every N CPU instructions, advance the
  SMP. Cheaper but less accurate at port-access boundaries.

Use Mesen2 pattern. Reference: `~/Git/Mesen2/Core/SNES/Spc.cpp` for
the catch-up and ratio handling.

## Sub-phase plan

### 5a: SPC700 core in isolation

**Scope:**
- `Smp` struct: A, X, Y, SP, PSW (NVPBHIZC flags), PC, and ARAM.
- `SmpBus` trait: read/write/idle (mirror of `cpu::bus::SnesBus`).
- `FlatSmpBus`: 64 KiB linear test bus.
- All 256 SPC700 opcodes. Most overlap with 6502 in shape but differ
  in encoding and add MUL, DIV, MOVW (16-bit move), DAA/DAS, BBS/BBC
  (branch on bit set/clear), CALL/RET conventions, 8-bit page-zero
  pseudo-stack, etc.
- IPL ROM at $FFC0-$FFFF (read-only when IPL enable bit is set, which
  is reset state). Vendor the canonical 64-byte upstream blob - place
  in `vendor/snes-ipl/` with attribution README mirroring the
  `vendor/emu2413/` pattern. The blob is widely circulated; cite the
  source.
- Reset: SP=$EF, PC=$FFC0 (entry of IPL), PSW with all flags clear.

**Test gate:** every `~/Git/snes-test-roms/PeterLemon/SNES-CPUTest-SPC700/*.sfc`
ROM passes via a new `src/bin/snes_spc_test_runner.rs` that mirrors
the existing `snes_test_runner.rs` shape: load ROM, drive SMP, look
for PASS/FAIL markers in ARAM or via the SMP's debug output.

**References:**
- `~/.claude/skills/nes-expert/reference/snes-apu.md` - SPC700 ISA + IPL + DSP overview
- `~/Git/Mesen2/Core/SNES/Spc.cpp`, `SpcInstructions.cpp`, `SpcTypes.h`
- `~/Git/higan/higan/sfc/smp/smp.cpp`, `smp.hpp`, `memory.cpp`
- snes.nesdev.org wiki SPC700 ISA pages

**Pitfalls:**
- SPC700 is **6502-shaped, not 6502-compatible.** Same op mnemonics
  but different opcodes. Don't reuse the 65C816 opcode table.
- The "direct page" is selectable per-bank via PSW.P bit (page 0 vs
  page 1). Most code stays at page 0.
- MOVW reads/writes 16-bit values across two consecutive zero-page
  bytes. Subtle wrap rules.
- No decimal mode flag; the D bit position is reused for something
  else (carry into bit 7 for ADC etc).
- `STOP` is similar to 65C816 STP. `SLEEP` similar to WAI but pre-empts
  on IRQ even if disabled.

### 5b: SMP timers

**Scope:**
- Three 8-bit timers: T0 and T1 tick at 8 kHz, T2 at 64 kHz.
- Each has a target ($00FA-$00FC), a 4-bit visible counter ($00FD-$00FF
  read-clears), and an enable bit in CONTROL ($00F1).
- Counter increments on each timer tick when target hit; visible
  counter increments and saturates.

**Test gate:** lidnariq's `lidnariq-smp-clock-speed-measurement` ROMs.
Plus the timer-touching subset of PeterLemon SPC700 if any.

### 5c: APU port bridge

**Scope:**
- Replace the boot-signature stub in `LoRomBus` with the real
  bidirectional latch driven by the SMP.
- Sync model: catch up SMP on every CPU `$2140-$2143` access plus a
  coarse periodic tick.
- Verify: SMW (and any commercial LoROM cart) clears the IPL handshake
  and reaches the block-transfer protocol with the SMP actually
  responding from the IPL ROM.

**Test gate:** PeterLemon SPC700 tests still pass after the bridge
lands. SMW boots past `$809A` without the fake-echo hack.

### 5d: S-DSP register file + voice state

**Scope:**
- DSP register file ($00-$7F mirrored at $80-$FF). 8 voices x 16-byte
  voice block plus master/echo/key registers.
- DSP read/write via `$00F2 DSPADDR` + `$00F3 DSPDATA`.
- Per-voice state: BRR pitch counter, current sample index, envelope
  level, key-on/off latch, etc.
- No audio output yet - just register file + voice key-on tracking.

**Test gate:** state-machine tests (registers settable + readback +
key-on/off latching). No audio comparison yet.

### 5e: BRR decoder + ADSR + Gain envelopes

**Scope:**
- BRR block decode: 9 bytes -> 16 PCM samples with predictor filters
  0-3 + shift exponent + end/loop flags.
- ADSR state machine (attack rate, decay rate, sustain level, sustain
  release rate) with the standard rate table.
- Gain mode (linear, bent-line, exponential decrease).
- Sample interpolation (Gaussian, 4-tap).

**Test gate:** decode known BRR samples and compare against
golden PCM output. Reference test vectors in
`~/Git/Mesen2/Core/SNES/DSP/` and the snes_spc reference.

### 5f: DSP mixer + echo + audio output

**Scope:**
- 32 kHz sample rate output (DSP runs at 32 kHz).
- Per-voice: BRR sample x ADSR/Gain envelope x VOL_L/R -> stereo
  contribution.
- Master volume (MVOL_L/R), echo volume (EVOL_L/R), echo feedback,
  echo FIR filter (8-tap), echo delay buffer in ARAM.
- Pitch modulation (PMON), noise generator (NON), key-on/off (KON/KOF).

**Test gate:** play known SPC dump (one of the Earthbound or SMW
soundtracks circulated as `.spc` files) and CRC-compare the output PCM
buffer against a golden capture. blargg-spc-6 ROMs.

### 5g: Host audio sink wire-up

**Scope:**
- 32 kHz DSP output -> existing band-limited resampler -> cpal sink.
- The resampler we already use for NES (in `src/audio.rs`) takes
  arbitrary source rate. Tune it to 32 kHz when SNES is loaded.

**Test gate:** SMW boots, music plays through speakers in real-time
without crackling.

## Order of operations

5a (SPC700 ISA) is the long pole. Get it correct in isolation before
any bus integration. Same shape as 65C816 development:
1. Bus trait + FlatBus
2. Register/flag/mode model + reset
3. Addressing modes
4. Opcodes by family (loads/stores, transfers, branches, ALU, etc.)
5. Test ROM headless runner
6. PeterLemon SPC700 sweep until 100% PASS

Then 5b-c are smaller. 5d-f are the DSP arc. 5g is plumbing.

## File pointers for the fresh context

- `src/snes/cpu/mod.rs` - 65C816 core. Use as the structural template
  for `src/snes/smp/mod.rs`. Same dispatch shape, same helper layout.
- `src/snes/cpu/bus.rs` - `SnesBus` trait + `FlatBus`. Mirror for
  `SmpBus` + `FlatSmpBus`.
- `src/snes/cpu/tests.rs` - test pattern. Same shape for SMP.
- `src/bin/snes_test_runner.rs` - headless harness. Clone for
  `snes_spc_test_runner.rs`.
- `vendor/emu2413/` - vendored-blob pattern with README attribution.
  Mirror this for `vendor/snes-ipl/`.

## Memory pointers

- `~/.claude/projects/-home-marcus-Git-vibenes/memory/snes_test_roms.md`
  - inventory of test ROM directories per phase.
- `~/.claude/projects/-home-marcus-Git-vibenes/memory/clean_room_policy.md`
  - porting from Mesen2/higan is permitted with attribution in source.
- `~/.claude/projects/-home-marcus-Git-vibenes/memory/feedback_no_em_dash.md`
  - never use em dash anywhere; plain `-` only.
- `~/.claude/projects/-home-marcus-Git-vibenes/memory/feedback_push_policy.md`
  - commit locally; user pushes after testing the build.
- `~/.claude/projects/-home-marcus-Git-vibenes/memory/feedback_commit_messages.md`
  - no "Cross-checked against:" footers; attributions in code, not
  commit messages.

## Project-policy reminders for the fresh context

- Same regression discipline as NES: before any commit, run the full
  green sweep. For SNES specifically: all 21 PeterLemon CPU ROMs must
  still PASS. NES regression smoke (instr_test-v5, cpu_interrupts_v2,
  apu_test, mmc3_test) must stay green.
- Clean-room-adjacent porting from Mesen2 / higan is permitted for
  significant features, with attribution in source comments citing
  the file and line range. Don't just copy-paste; paraphrase in our
  own words.
- Commit but do NOT `git push` until the user has tested the build.
  The dual-remote `origin` publishes to home + GitHub in one shot.
- No fake-it-til-you-make-it stubs. The user explicitly rejected this
  approach during Phase 4 - the IPL boot signature is the only
  hardware-modeling stub allowed because the real IPL really does
  write that. Don't fake SMP responses.

## Honest limits at handoff

- Commercial games will hang at the SPC700 block-transfer protocol
  past the boot handshake. This is expected and correct given no SMP.
  Do not add stubs to push past this; the right answer is finish
  Phase 5.
- Test ROM grading is currently visual-via-VRAM-scan for PeterLemon
  CPU ROMs. SPC700 ROMs likely use the same convention - confirm by
  inspecting one of the .asm files in `~/Git/snes-test-roms/PeterLemon/SNES-CPUTest-SPC700/`.
