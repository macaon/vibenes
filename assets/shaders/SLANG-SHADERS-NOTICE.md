# slang-shaders attribution

Several preset chains under this directory tree (`edge-smoothing/`,
`ntsc/`, `pal/`, plus the shared `stock.slang` and `interpolation/`
deps) are bundled verbatim from
[libretro/slang-shaders](https://github.com/libretro/slang-shaders).
The upstream repository has no top-level licence file: each shader
carries its own copyright header, and the per-shader licences mix
GPL-2.0+, GPL-3.0+, MIT, and LGPL-2.1+. We bundle each shader with
its original header intact - look at the top of any individual
`.slang` for the authoritative attribution.

This document summarises the upstream authors and broad licence
buckets so they can be credited in one place, but it is **not** a
replacement for the per-file headers - those govern.

| Bundled subset | Upstream path | Authors | Licence |
|---|---|---|---|
| `edge-smoothing/eagle/` (2xSaI, Super 2xSaI, Super Eagle) | `edge-smoothing/eagle/` | Derek Liauw Kie Fa (Kreed); slang ports by various Libretro contributors | GPL-2.0+ |
| `edge-smoothing/hqx/` (Hq2x, Hq3x, Hq4x) | `edge-smoothing/hqx/` | Maxim Stepin; slang port by Hyllian and others | LGPL-2.1+ |
| `edge-smoothing/scalenx/` (Scale2x, Scale3x) | `edge-smoothing/scalenx/` | Andrea Mazzoleni; slang port by Hyllian and others | GPL-2.0+ |
| `edge-smoothing/xbrz/` (xBRZ 2x/4x/5x/6x linear variants) | `edge-smoothing/xbrz/` | Hyllian (Sergio G.D.B.) + Zenju (xBRZ algorithm) | MIT |
| `ntsc/blargg.slangp` | `ntsc/` | Shay Green (blargg) C library; metallic77 GLSL port; NewRisingSun and blargg | LGPL-2.1+ |
| `ntsc/ntsc-256px-{composite,svideo}.slangp` | `ntsc/` | Themaister (Hans-Kristian Arntzen) | GPL-3.0+ |
| `pal/pal-r57shell.slangp` | `pal/` | r57shell (PAL emulation), ported to slang | GPL-2.0+ |
| `interpolation/shaders/bicubic*` | `interpolation/` | Various (b-spline / catmull-rom interpolators commonly attributed to Hyllian and Themaister) | Various permissive |
| `stock.slang` | (root) | Themaister | Public-domain trivial passthrough |

Where the upstream slang-shaders repo gives no licence header for a
file, we treat it as inheriting the C-library licence the algorithm
was originally published under (e.g. `nes_ntsc` is LGPL-2.1+, so the
GLSL port under `ntsc/blargg.slangp` is treated the same).

## How we use it

We are GPL-3.0-or-later, which is compatible with all of the
licences above (GPL-3.0 is a strict superset of GPL-2.0 in
permissions for the user; LGPL and MIT are permissive). When we
ship a binary release, the bundled `.slang` and `.slangp` files
travel alongside the binary unmodified, with their headers intact -
we are an aggregating distributor, not a re-licenser.

## Bundled subset (working with our wgpu 29 stack)

A handful of upstream presets were tested but **not** bundled
because they either reference unsupported wgpu/naga features or
produce broken output through our shader runtime:

- `pal/pal-singlepass.slangp` - texture-binding error during
  reflection (`InvalidResourceType`).
- `handheld/gameboy-color-dot-matrix.slangp` and
  `handheld/gameboy-advance-dot-matrix.slangp` - mattakins' v1.1
  dot-matrix shader uses `Special type is not registered within
  the module` features that naga rejects during validation.
- `ntsc/artifact-colors.slangp` - loads cleanly but renders a
  split image (the preset's forced 640px-wide intermediate pass
  doesn't reproject correctly through our viewport math). Other
  NTSC presets (blargg, Themaister's 256px composite/svideo) are
  bundled instead.

These can still be loaded via `Browse...` if the user has a
slang-shaders checkout - we just don't ship them as defaults.
