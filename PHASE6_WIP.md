# Phase 6 WIP — PPU rendering + window + input + MMC3

Full plan: `~/.claude/plans/polymorphic-plotting-scone.md`.

## Current sub-phase: 6A

Split into atomic commits for review-ability:

- **6A.1 — Mapper A12 / IRQ-line trait infra (in progress)**
  Extend `Mapper` trait with `on_ppu_addr(addr, ppu_cycle)` + `irq_line()`
  defaults. Wire `mapper.irq_line()` into bus `tick_post_access` IRQ
  refresh. Zero behavior change (all existing mappers use defaults).
- **6A.2 — Deps + gfx skeleton**
  Add `winit`, `wgpu`, `pollster`, `bytemuck`. Stub `src/gfx/` and
  `src/app.rs` (`FrameSink` trait, `build_nes`, `run_until`). Add
  `src/bin/vibenes_gui.rs` that opens a blank window.
- **6A.3 — Passthrough render pipeline**
  256×240 offscreen texture, fullscreen-triangle vertex shader,
  passthrough fragment shader, `WgpuSink`. Window shows a static
  gradient test pattern uploaded from CPU.
- **6A.4 — PPU BG pipeline**
  Per-dot NT/AT/pattern fetches through `ppu_bus_read` choke point
  (fires `on_ppu_addr`). 16-bit pattern shifters + 8-bit attribute
  shifters. `v` increments at dots 256/257/280–304. Pixel output
  to `frame_buffer`. Sprite-0 hit stub for SMB status-bar split.
  Garbage NT fetches at dots 337–340.
- **6A gate:** SMB + Donkey Kong BG visible, `full_palette.nes`
  passes, Phase 5 regression sweep clean.

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
