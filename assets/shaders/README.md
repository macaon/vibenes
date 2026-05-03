# Bundled shader presets

RetroArch shader presets shipped with vibenes. Each subdirectory is a
self-contained preset chain - the `.slangp` (or `.glslp` / `.cgp`)
file is the entry point that the shader runtime reads, and it
references sibling shader sources by relative path.

To select one at startup (until the in-app shader picker lands):

```
VIBENES_SHADER=assets/shaders/jintenji-crt/crt-tetchi-shadowmask.slangp \
  ./target/release/vibenes path/to/rom.nes
```

Any other RetroArch preset on disk works too - point `VIBENES_SHADER`
at it directly.

## Inventory

### slang-shaders bundled subset

A curated mirror of the libretro
[`slang-shaders`](https://github.com/libretro/slang-shaders) repository
covering the puNES "Software Filter" menu equivalents: pixel-art
scalers, NTSC simulators, and a PAL filter. Copied verbatim from
upstream with the directory layout preserved so the `.slangp`
relative paths resolve unchanged.

| Directory | Presets | Roughly equivalent puNES filter |
|---|---|---|
| `edge-smoothing/eagle/` | 2xsai, super-2xsai, supereagle | 2xSaI, Super 2xSaI, Super Eagle |
| `edge-smoothing/hqx/` | hq2x, hq3x, hq4x | Hq2X, Hq3X, Hq4X |
| `edge-smoothing/scalenx/` | scale2x, scale3x | Scale2X, Scale3X (no Scale4x in slang) |
| `edge-smoothing/xbrz/` | 2/4/5/6 xbrz-linear | xBRZ 2X-6X |
| `ntsc/` | blargg, ntsc-256px-composite, ntsc-256px-svideo | NTSC RGB / Composite / S-Video |
| `pal/` | pal-r57shell | PAL TV variants |

Per-shader licences vary across GPL-2.0+, LGPL-2.1+, GPL-3.0+, MIT.
Each `.slang` retains its upstream copyright header. See
[`SLANG-SHADERS-NOTICE.md`](SLANG-SHADERS-NOTICE.md) for the full
attribution table and notes on the three upstream presets we tested
but couldn't bundle (wgpu/naga incompatibility - see the notice).

### `crt-guest-advanced-fast/`

The "fast" variant of guest(r)'s widely-recommended CRT shader.
9-pass chain (~210 KB, including 4 LUT PNGs from the advanced
package). Hits a sweet spot between the heavier `advanced` variant
(12 passes) and the stripped `fastest` (5 passes); runs comfortably
on integrated GPUs while delivering the full mask + bloom +
deconvergence look. Exposes ~50 tweakable parameters (mask type,
scanline strength, glow, geometry curvature, etc.) - we ship
upstream defaults until the in-app parameter UI lands.

Source: <https://github.com/libretro/slang-shaders/tree/master/crt>
(`crt-guest-advanced-fast.slangp` + `shaders/guest/fast/*.slang` +
`shaders/guest/advanced/lut/*.png`). Paths in the `.slangp` were
rewritten to siblings so the chain is self-contained.

License: GPL-2.0-or-later (see `crt-guest-advanced-fast/LICENSE`).

### `jintenji-crt/`

A 2-pass CRT shader (electron gun + phosphor mask) by jintenji,
upstream <https://github.com/jintenji/CRT-Shader-in-retroarch>. Three
mask variants exist upstream (grill, shadowmask, slotmask); we ship
only the aperture-grille (Trinitron-style) variant because shadowmask
and slotmask interact poorly with the NES's low pixel resolution -
the masks dominate the image and obscure the actual artwork.

| Preset | Look |
|---|---|
| `crt-tetchi-grill.slangp` | Aperture-grille (Trinitron-style) |

Adjust `Mask Scale` (1x-4x) once the in-app parameter UI lands; for
now the default scale renders at 1x.

License: MIT (see `jintenji-crt/LICENSE`).

## Adding more presets

Drop any RetroArch preset directory under `assets/shaders/<name>/`.
Anything that loads in RetroArch with the `slang` runtime should load
here too. License compatibility: vibenes is GPL-3.0-or-later, so MIT,
BSD, MPL-2.0, and GPL-2.0+/GPL-3.0+ presets are all fine to bundle;
include the upstream license file alongside the preset and credit
the author in this README.
