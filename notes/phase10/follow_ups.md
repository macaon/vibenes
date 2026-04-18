# Phase 10 follow-ups (MMC3)

Two MMC3 test ROMs still fail after phases 10A/10B ship. Documented
here so a future phase can resume cold.

Status snapshot (branch `phase10-mmc3` @ commit 388f7f0):

| suite | ROM | status |
|---|---|---|
| mmc3_test | 1-clocking | PASS |
| mmc3_test | 2-details | PASS |
| mmc3_test | 3-A12_clocking | PASS |
| mmc3_test | 4-scanline_timing | FAIL #3 |
| mmc3_test | 5-MMC3 | PASS |
| mmc3_test | 6-MMC6 | FAIL #3 |
| mmc3_test_2 | 1-clocking | PASS |
| mmc3_test_2 | 2-details | PASS |
| mmc3_test_2 | 3-A12_clocking | PASS |
| mmc3_test_2 | 4-scanline_timing | FAIL #3 |
| mmc3_test_2 | 5-MMC3 | PASS |
| mmc3_test_2 | 6-MMC3_alt | FAIL #2 |

---

## F1 — 4-scanline_timing #3 off-by-one PPU cycle

**Symptom.** `4-scanline_timing` tests IRQ fire relative to VBL flag
set, at 1-PPU-cycle resolution. Tests 2 and 3 of the suite form a
boundary check:

| test | delay | expected | observed |
|---|---|---|---|
| 2 | `scanline_0_08 - 1` (6975) | no IRQ ($22) | no IRQ ($22) ✅ |
| 3 | `scanline_0_08` (6976) | IRQ ($21) | no IRQ ($22) ❌ |

Our IRQ fires LATER than PPU cycle 6976 after VBL-flag-set. The
mechanism is correct (A12 watcher clocks the counter, Rev B fires on
counter-hits-zero, PPU notifies on $2006/$2007), but the exact PPU
dot at which `bus.irq_line` becomes true — and hence the first CPU
cycle on which `prev_irq_line` latches it — is shifted.

**Configuration.** CTRL=$08 (sprites at $1000). Counter latch=0 with
reload armed, IRQ enabled. Rendering enabled mid-VBL. First filtered
A12 rise = first sprite pat-lo fetch of scanline 0 (dot 262).

**Suspects (diagnose before touching code):**

1. **`on_ppu_addr` call placement.** We call it at the TOP of
   `ppu_bus_read`/`ppu_bus_write`. The filter's `ppu_cycle` is
   `self.master_ppu_cycle` BEFORE `self.master_ppu_cycle += 1`. So
   the timestamp is "at the START of this dot". Mesen2 samples
   `_console->GetMasterClock()` which is CPU-cycle count — their
   filter compares in CPU cycles (>=3). A 1-PPU-cycle misalignment
   may come from storing the fall-timestamp one dot early/late
   vs their CPU-cycle-granular storage.

2. **`tick_pre_access` vs `tick_post_access` split.** A12 rises
   during the final PPU dot of a CPU cycle (post-access) are
   visible only to the NEXT CPU cycle's `prev_irq_line` snapshot.
   If dot 262 lands as a post-access dot, we delay the IRQ by
   one full CPU cycle (= 3 PPU cycles). The test's 1-cycle miss
   makes this MORE likely than a full-CPU-cycle delay, but worth
   ruling out with instrumentation.

3. **Sprite-fetch dot boundary.** Our sprite pattern-lo fetch is
   coded at `(self.dot - 257) % 8 == 5` (i.e. dots 262, 270, ...).
   Nesdev says pat-lo fetch issues its address on dot 5 of each
   8-dot slot. If Mesen2 issues it on dot 4 (or 6), our A12 rise
   timestamp is 1 PPU cycle off. Cross-check against Mesen2
   `NesPpu.cpp` `ProcessSpriteEvaluation` or similar.

**Diagnostic steps.**
1. Add a `#[cfg(debug_assertions)]` log in `Mmc3::clock_irq_counter`
   that prints `(scanline, dot, counter_in, counter_out, fire)`
   when `cfg!(feature="mmc3_trace")` or similar.
2. Run `4-scanline_timing.nes` under the trace; note the PPU cycle
   at which the first fire occurs.
3. Compare with Mesen2 running the same ROM (same trace point).
4. If the delta is exactly 1 PPU cycle, adjust where `on_ppu_addr`
   samples the timestamp (before vs after the `master_ppu_cycle +=
   1` in `tick`). If 3 PPU cycles, the `tick_pre_access` /
   `tick_post_access` split is routing the fetch into the wrong
   half.

**Out of scope until diagnosed.** Touching the PPU tick split or
the A12-watcher storage timestamp risks regressing
`cpu_interrupts_v2/3-nmi_and_irq` (which relies on mid-cycle PPU
state visibility) and `mmc3_test/3-A12_clocking` (which validates
the filter itself). Branch before touching.

---

## F2 — Rev A / MMC6 submapper detection (6-MMC3_alt, 6-MMC6)

**Symptom.** `Mmc3::alt_irq_behavior = true` passes the unit test
`rev_a_does_not_refire_on_reload_from_zero`, and is the correct
semantics for the two failing ROMs. But there's no activation path
at runtime — iNES 1.0 headers don't expose submapper, and iNES 2.0
for these ROMs would use submapper 3 (per Mesen's scheme) which our
`src/rom.rs` already parses but these test ROMs ship as iNES 1.0.

Mesen's approach: a ROM-hash database maps specific PRG/CHR hashes
to chip names (e.g. "MMC3A"), and `InitMapper` does a prefix-match
on the chip-name string to set `_forceMmc3RevAIrqs = true`. This is
heavyweight for us.

**Lightweight options:**

1. **CLI/env override.** A `VIBENES_MMC3_FORCE_REV_A=1` env var read
   in `Mmc3::new` (plus a CLI flag in `test_runner`). Good enough
   for running the two failing test ROMs; zero risk to the normal
   path.
2. **Per-ROM filename heuristic.** If the ROM path contains "alt"
   or "Alt", flip to Rev A. Quick but fragile.
3. **iNES 2.0 submapper detection (proper fix).** Submapper 3 =
   MMC3 Rev A. Already plumbed through `Cartridge::submapper`;
   MMC3 just needs to read it in `new`. Doesn't help these specific
   ROMs (they're iNES 1.0) but is the long-term right answer for
   NES 2.0 ROMs.
4. **ROM-hash database.** Eventually — when we want to ship a
   general MMC3 emulator that "just works" for real games.

**MMC6 (`6-MMC6.nes`) note.** The test name is misleading — it
actually tests Rev A semantics ("IRQ shouldn't occur when reloading
after counter normally reaches 0"). MMC6's distinctive feature
(1 KB on-chip PRG-RAM with per-half enable/protect at $7000-$7FFF)
is not exercised here; implementing that is Phase 10E, independent
of this Rev A fix.

---

## F3 — mmc3_irq_tests suite (not a gating suite)

`~/Git/nes-test-roms/mmc3_irq_tests/*` all time out under
`test_runner`. These are NSF-style audio-output-only test ROMs —
they report via APU beeps rather than nametable text. Out of scope
until we have audio capture; no action required for Phase 10
correctness.

---

## Cross-reference

- `src/mapper/mmc3.rs` — implementation
- `src/ppu.rs:981,994` — `$2006`/`$2007` A12 hooks
- `src/bus.rs:290` — mapper `irq_line` wire-OR in `tick_pre_access`
- `~/Git/Mesen2/Core/NES/Mappers/Nintendo/MMC3.h:300-335` — Mesen2's
  current MMC3 filter + IRQ state (uses CPU-cycle granular
  `GetMasterClock()` ≥ 3, equivalent to our 10 PPU cycles at NTSC)
- `~/Git/Mesen2/Core/NES/Mappers/A12Watcher.h` — older template,
  PPU-cycle granular `minDelay=10`, same net filter
- `~/.claude/skills/nes-expert/reference/mappers.md §Mapper 4`
- `~/.claude/skills/nes-expert/reference/mesen-notes.md §20-21`
