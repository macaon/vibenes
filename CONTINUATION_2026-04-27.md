# Continuation handoff — 2026-04-27

Picking up SPC700 + S-DSP work from another machine. This doc captures
the state of vibenes2 *as of `commit at root of branch main`* and the
priorities for the next session.

## What landed today (committed + pushed to gitea + github)

| Commit  | Phase | Summary                                              |
|---------|-------|------------------------------------------------------|
| 3db2e21 | 5b.1  | Integrated SMP bus + register-file state             |
| fe3038d | 5b.2  | SPC700 runs alongside the 5A22 in `Snes::step_*`     |
| bf50519 | 5b.3  | SPC700 ISA complete (256/256 opcodes); IPL boots     |
| 7cce04c | 5c.1  | S-DSP register layout + structured accessors         |
| (this)  | val   | PeterLemon SPC700 harness + B-flag fix on BRK        |

`cargo test --lib` = **784 passing**, `cargo test --release --test
peterlemon_spc700` = **7/7 passing** (ADC, AND, DEC, EOR, INC, ORA,
SBC against PeterLemon's known-good SPC700 ISA tests).

## What this session got wrong, and the lesson

I freewheeled the SPC700 implementation across phases 5b.1..5c.1: the
comments saying `// per Mesen2 X` and `// per Anomie` are
documentation-of-intent, NOT verified ports against the actual
sources. The PeterLemon ADC test failure mid-session was the proof:
the harness was loading the standard 66 KiB `.spc` sound-file format
(complete with ASCII header) into ARAM at `$0200` and executing the
header text as opcodes. **The ISA itself was fine** — the PeterLemon
tests now pass once the harness loads the raw 13 KiB binary instead
— but the failure mode was indistinguishable from a deep ISA bug
until the file format issue was unwound.

The user reset me on this explicitly:

> Did you use /nes-expert and reference Mesen2 and Higan like I told
> you or have you been freewheeling the implementation?
>
> ... we do NOT hallucinate our way to an accurate emulator.

A new memory note enforces this for SNES work going forward:
`feedback_consult_snes_references.md`. The mandate: open Mesen2 +
higan + `~/.claude/skills/nes-expert/reference/snes-apu.md` BEFORE
writing implementation code, not after.

## State of the audit

The audit kicked off but is not finished. What's been verified
against `~/Git/Mesen2/Core/SNES/Spc.Instructions.cpp` and
`~/Git/higan/higan/component/processor/spc700/instructions.cpp`:

- ✅ `op_clrv` / CLRV ($E0): clears both V AND H. Match.
- ✅ `op_brk` ($0F): cycle pattern matches higan; **fixed live B
  flag bug** (was leaving B clear on entry; higan sets B=1 live).
- ✅ ADC/SBC byte arithmetic + flag formulae: V via
  `(a^r) & (b^r) & 0x80`, H via low-nibble carry. Both match higan
  algebraically.
- ✅ POP A / POP X / POP Y: do NOT touch flags (only POP PSW does).
- ✅ MOV (X) / MOV (X)+ / MOV (X)+, A / MOV [dp+X] etc. — cycle
  counts match higan; bus-access ORDER may differ within a phase
  but equivalent for ARAM access patterns.
- ✅ Timer 0/1/2 model: Mesen2 uses (rate=128/16, timerInc=2,
  toggle stage1 ^ ClockTimer). My model accumulates 1 per access
  with divider 128/16 and stage1 ticks straight to target. The two
  produce identical observable counter behaviour (both fire stage2
  every 128 accesses for T0/T1, every 16 for T2).
- ✅ BBS/BBC/CBNE/DBNZ: cycle order vs higan `instructionBranchBit`
  matches. Standard 5/7-cycle branch.
- ✅ JMP [!abs+X] ($1F): cycle count + bus order match higan.

Still unaudited / partially audited:

- 🟡 **Older opcode families** (CMP A/X/Y, AND/OR/EOR all 36
  variants, INC/DEC memory, ASL/LSR/ROL/ROR, MOVW/ADDW/SUBW/CMPW/
  INCW/DECW, MUL, DIV, all bit ops SET1/CLR1/TSET1/TCLR1/AND1/OR1/
  EOR1/NOT1/MOV1, CALL/PCALL/TCALL/RETI, DAA/DAS/XCN). These were
  written before the audit started; PeterLemon ADC/AND/SBC/INC/DEC/
  ORA/EOR exercise some but not all paths.
- 🟡 **`IntegratedSmpBus::read_io` / `write_io`** ($F0-$FF dispatch).
  CONTROL bit 4/5 input-latch clears, DSP $F2/$F3 round-trip, F8/F9
  as plain ARAM, timer target writes / counter read-clear. I cited
  Mesen2 in the comments but did not verify against `Spc.cpp` lines
  158-180 / 230-345 line-by-line.
- 🟡 **Reset state**: `Snes::ApuSubsystem::new` resets the SMP at
  power-on by reading the IPL reset vector. Mesen2 has more
  intricate power-on initialization (RAM contents random per
  hardware quirks doc); we zero-fill, which matches higan behaviour
  but breaks Sailor Moon / Death Brade / Power Drive (per
  `snes-apu.md` Pitfall #7 — needs follow-up).

## Outstanding genuine bugs from the audit so far

1. ✅ **Fixed**: BRK B-flag live state (committed in this session's
   final push).
2. None other found yet — but audit is incomplete. The fact that
   PeterLemon ADC..SBC passes is strong but not exhaustive evidence.

## What to do next session

Priority order:

1. **Finish the audit.** Open Mesen2 + higan side-by-side and walk
   through every opcode family I marked 🟡 above. Save the audit
   notes inline in this doc as you go (turn 🟡 into ✅ or open new
   bug entries). The right time is BEFORE Phase 5c moves further.

2. **Add more PeterLemon-style validations** for the families the
   current 7 tests don't exercise:
   - CMP variants
   - 16-bit YA word ops (MOVW/ADDW/SUBW/CMPW/INCW/DECW)
   - MUL/DIV (DIV's /512 quirk especially)
   - Bit ops + branch-on-bit
   - CALL/PCALL/TCALL/RETI
   - DAA/DAS/XCN
   PeterLemon may have more `.spc` test ROMs in
   `~/Git/snes-test-roms/PeterLemon/` — check first; otherwise
   hand-craft small validators inside `tests/peterlemon_spc700.rs`
   alongside the existing ones.

3. **Resume Phase 5c.** Sub-phase plan still on the todo list:
   - 5c.2: BRR sample decoder (pure, testable, in `dsp/brr.rs`)
   - 5c.3: voice pitch counter + Gaussian 4-point interpolation
   - 5c.4: ADSR / GAIN envelope generator with 31-rate table
   - 5c.5: master mix → 32 kHz stereo through `AudioSink`
   - 5c.6: echo unit (FIR + delay buffer in ARAM)
   - 5c.7: noise + pitch modulation
   When you start 5c.2, OPEN `~/Git/Mesen2/Core/SNES/Dsp/Dsp*.cpp`
   and `~/Git/higan/higan/component/processor/spc700/` (DSP code is
   actually in `~/Git/higan/higan/sfc/dsp/` and the modular `nall/`)
   FIRST. Cite line numbers in commit messages.

4. **Standard `.spc` file parser.** The harness currently only
   accepts the raw-binary nested layout; a real `.spc` parser would
   let us drop in any third-party test ROM (e.g. blargg's
   `spc_smp.sfc` extracted to `.spc`, or any commercial game's
   sound dump). Header layout is in `snes-apu.md` lines 228-237.

## Repository state at handoff

```
$ git status
On branch main
nothing to commit, working tree clean (after this session's push)

$ cargo test --lib --release  # green, 784 tests
$ cargo test --release --test peterlemon_spc700  # green, 7 tests
```

## Mental model reminders

- The SPC700 + S-DSP run on their own crystal, async to the 5A22.
  In our orchestration (`Snes::step_instruction`), each CPU
  instruction's master-cycle cost gets divided by `SMP_MASTER_DIVIDER
  = 21` to set the SMP catch-up target. The SMP runs full
  instructions to reach that target; cycle-perfect timing is good
  enough for SPC ISA test ROMs but not for tight games that probe
  the exact SPC↔CPU timing relationship.
- The integrated SMP bus is constructed transiently per SMP step
  from disjoint mutable fields of `Snes`. No `Rc<RefCell<>>` —
  field-borrows suffice because the orchestrator interleaves CPU
  and SMP, never running them concurrently.
- DSP register file is a 128-byte flat array. Per-voice/global
  accessors live in `src/snes/smp/dsp.rs` (Phase 5c.1). The actual
  voice runtime + BRR + envelope + mixer have not been written.

## Reference cheat-sheet (open these FIRST next session)

- Mesen2: `~/Git/Mesen2/Core/SNES/Spc*.{cpp,h}` (instructions, timer
  template, register table) and `~/Git/Mesen2/Core/SNES/Dsp/Dsp*.cpp`
  (DSP voice runtime, ENVX/OUTX, echo, BRR).
- higan: `~/Git/higan/higan/component/processor/spc700/` (instruction
  dispatch + algorithms + cycle-accurate read/write/idle pattern)
  and `~/Git/higan/higan/sfc/dsp/` (DSP voice processing).
- Reference doc: `~/.claude/skills/nes-expert/reference/snes-apu.md`
  (architecture overview + register tables + pitfalls).
- bsnes-plus is NOT on this machine (`~/Git/bsnes*` returns nothing).
  If you want a third reference, check whether it can be cloned —
  but Mesen2 + higan are sufficient.
