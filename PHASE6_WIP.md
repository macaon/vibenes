# Phase 6 WIP — PPU rendering + window + input + MMC3

Full plan: `~/.claude/plans/polymorphic-plotting-scone.md`.

## Status (end of 2026-04-17 session)

**Landed on `phase5-interrupt-polling`:**

- `8466e15` — 6A.1: Mapper trait `on_ppu_addr` + `irq_line` defaults, bus IRQ wire-OR
- `3b9cd9c` — 6A.2: winit blank window + `src/app.rs` skeleton
- `bb3ff86` — 6A.3: wgpu passthrough render pipeline (raw wgpu 29 + winit 0.30)
- `f343d2d` — chore: `./run` dev shortcut
- `53a6714` — 6A.4: PPU BG pipeline (fetches, shifters, v-increments, sprite-0 stub)
- `dd0441f` — 6A.4 fixup: shift range 1..=256, dot 337 reload, sprite-0 stub timing

**Visually verified:** Ski or Die copyright screen renders pristine.
SMB boots, plays through title into 1-1, background scrolls. Donkey
Kong title + NES Open Tournament Golf + R.C. Pro Am all render the
BG and animate correctly.

**Full Phase 5 regression sweep remains clean** — only `cpu_interrupts_v2`
3/4/5 tracked failures still fail. No PPU-phase collateral damage.

## Known BG glitches (remaining 6A.4 cleanup)

These three reproduce on commercial ROMs. Screenshots in the prior
session's conversation.

1. **Thin vertical line remaining (few pixels)** — mostly gone after
   the dot-337 reload fix but a residual column of wrong pixels is
   still visible on some games. Candidates:
   - fine_x off-by-one at scanline start — shifter bit position
     (`15 - fine_x`) may be wrong at dot 1 before any shift.
   - First-column-of-nametable artifact — column 0 read may pull
     from wrong nametable under scroll.

2. **Horizontal scroll glitches at status bar** — in SMB, the top
   status bar ("MARIO / WORLD / TIME") text becomes garbled during
   sideways playfield scrolling. Almost certainly sprite-0 hit
   timing or mid-frame `$2005`/`$2006` writes. SMB splits the
   screen: status bar uses one scroll, playfield another; the split
   depends on precise sprite-0 hit detection. Current stub fires at
   `scanline == oam[0].y + 1, dot == oam[0].x + 1` — may still be
   too early or too coarse for SMB's polling loop.

3. **Horizontal band below the middle of the screen** — visible in
   NES Open Tournament Golf title (screenshot shows ~20px tall
   strip of wrong/misaligned graphics crossing the title image at
   approximately y=130). Same class as #2 — scroll-split landing at
   the wrong scanline. Likely the same root cause or adjacent to it.

## Next-session diagnostic plan

**Start with #2 + #3** (same root cause likely). They're visible and
repeatable. Approach:

1. Instrument the sprite-0 stub with `eprintln!` of (oam[0].y,
   oam[0].x, scanline, dot) on stub fire. Compare against what the
   real hit would be for SMB 1-1 intro.
2. Cross-check mid-frame `$2005`/`$2006` handling. Status-bar split
   writes to `$2006` during the scanline the split happens on; our
   `cpu_write` path should land `v = t` at the right dot.
3. The real sprite-0 hit predicate (6B) may be needed earlier than
   planned. Consider doing the 5-part predicate (mesen-notes §14)
   on top of the 6A stub even before full sprite rendering — the
   sprite-0 hit only needs OAM[0] Y/X/pattern/attr, not the full
   evaluation pipeline. ~60 lines of code.

**Then #1** — narrow the remaining vertical line. Likely an hour
with `diag-bg` eprintln of shifter state at (scanline=N, dot in
{1, 2, 8, 9}) across a few scanlines.

## Sub-phase roadmap reminder

- **6A.4** (current): finish visual cleanup of 3 remaining glitches.
- **6B**: full sprite evaluation + pixel mux + real sprite-0 hit +
  sprite overflow (with hardware bug). Fixes #2 and #3 properly.
- **6B.5**: input (winit keyboard → controller).
- **6C**: accuracy polish (odd-frame skip, `$2002` race, grayscale +
  emphasis, etc).
- **6D**: MMC3 mapper — capstone, validates the A12 infra from 6A.1.

## Gating

See plan file §Verification for the full end-of-6A acceptance list.
Every commit re-runs the Phase 5 regression sweep and eyeballs it
(per `memory/feedback_regression_eyeball.md`).

## Known-fail (not touched in this phase)

- `cpu_interrupts_v2/rom_singles/3-nmi_and_irq.nes`
- `cpu_interrupts_v2/rom_singles/4-irq_and_dma.nes`
- `cpu_interrupts_v2/rom_singles/5-branch_delays_irq.nes`

Tracked in
`~/.claude/projects/-home-marcus-Git-vibenes2/memory/project_interrupts_bug.md`.
A PPU-phase commit flipping any of these to PASS is a signal to
investigate, not celebrate — could indicate we've masked the bug.

## Known limitation

`full_palette.nes` (Blargg visual PPU test) requires the "rendering
disabled + v points into palette = v's target byte is the output
color" hardware quirk. Not modeled. Revisit in 6C if needed; not a
blocker for commercial ROMs.
