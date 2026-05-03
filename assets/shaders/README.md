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

### `jintenji-crt/`

A 2-pass CRT shader (electron gun + phosphor mask) by jintenji,
upstream <https://github.com/jintenji/CRT-Shader-in-retroarch>. Three
mask variants ship side by side:

| Preset | Look |
|---|---|
| `crt-tetchi-grill.slangp` | Aperture-grille (Trinitron-style) |
| `crt-tetchi-shadowmask.slangp` | Shadow-mask (typical 4:3 PC monitor) |
| `crt-tetchi-slotmask.slangp` | Slot-mask (consumer TV hybrid) |

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
