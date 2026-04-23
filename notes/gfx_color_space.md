# Color-space cleanup: make surface + framebuffer non-sRGB

**Status:** pending. Raised 2026-04-23 after `egui_wgpu` logged:

> Detected a linear (sRGBA aware) framebuffer Rgba8UnormSrgb. egui
> prefers Rgba8Unorm or Bgra8Unorm

Not a correctness bug today — egui-wgpu 0.34 has a
`fs_main_linear_framebuffer` shader variant that handles sRGB targets
(see `egui-wgpu-0.34.1/src/renderer.rs:406-411`) so the overlay still
renders with the right colors. But the pipeline is doing two
canceling sRGB conversions we don't need, and that's what the warning
is pointing at.

## What we do today

[src/gfx/mod.rs:90-97](../src/gfx/mod.rs#L90-L97):

- Surface format: prefer anything `is_srgb()` (typically
  `Bgra8UnormSrgb`).
- Framebuffer texture (the NES image upload target):
  `Rgba8UnormSrgb` at line 123.
- Fragment shader (`src/gfx/shaders/passthrough.wgsl`):
  `textureSample(…)` passthrough, no math.

The original comment claims this gives us "linear end-to-end." That
comment is lying to us.

## What actually happens

The NES palette is RGB triplets tuned for a consumer CRT — they're
already in display space (≈ sRGB). The PPU writes those bytes into
`frame_buffer` ([src/ppu.rs:662-665](../src/ppu.rs#L662-L665)).

When we upload via `queue.write_texture` into an `Rgba8UnormSrgb`
texture, hardware treats the bytes as sRGB-encoded. Then:

1. **Sample** in the fragment shader → hardware decodes sRGB → linear.
2. Shader passthrough.
3. **Write** to sRGB swapchain → hardware encodes linear → sRGB.

Steps 1 and 3 cancel. We pay for two colorspace conversions per pixel
and arrive at the original palette bytes. If we ever insert actual
linear-space math (blending, tone mapping, CRT shader work) *between*
steps 1 and 3, the current setup is principled. We don't have any
such math, and none is on the near-term roadmap.

`egui_wgpu` outputs pre-gamma-corrected sRGB bytes. On a non-sRGB
target the output lands on-screen as-is; on our sRGB target
`fs_main_linear_framebuffer` pre-decodes so the final hardware encode
produces the same result. That extra decode costs a tiny bit of
dithering quality — imperceptible on an OSD but it's the thing the
warning is about.

## The fix

Make the whole chain non-sRGB. NES palette bytes flow through raw;
egui gets its preferred `fs_main_gamma_framebuffer` path.

1. [src/gfx/mod.rs:90-97](../src/gfx/mod.rs#L90-L97) — invert the
   format filter: prefer `Rgba8Unorm` / `Bgra8Unorm`, fall back to
   whatever the adapter reports first. Update the comment.
2. [src/gfx/mod.rs:123](../src/gfx/mod.rs#L123) — change the
   framebuffer texture format from `Rgba8UnormSrgb` to `Rgba8Unorm`.
3. `passthrough.wgsl` — no shader change needed; the `textureSample`
   now gets raw bytes out and writes raw bytes in. Already a no-op
   passthrough.
4. `EguiRenderer::new` — already takes `surface_format` from step 1,
   so it automatically picks the gamma-framebuffer shader. No code
   change; the warning disappears.

Estimated touch: ~6 lines + 1 comment revision. No test changes
expected (the unit tests don't exercise the renderer).

## Verification

- Load any ROM, compare NES colors against Mesen or a palette
  reference image. They should render identically to before (the
  palette bytes are still hitting the surface unchanged — we just
  removed the two canceling sRGB ops).
- Open the F1 overlay. Text should look slightly cleaner (better
  dithering); colors should look the same as before to the naked eye.
- Terminal should no longer print the `Detected a linear (sRGBA
  aware) framebuffer` line.

## If a future CRT/NTSC filter wants linear math

Do it in the shader, not the pipeline. Sample raw palette bytes, do
`pow(color, vec3(2.2))` to enter linear space, do the filter math,
`pow(color, vec3(1.0 / 2.2))` back to display space, write. The
surface format stays non-sRGB and all the space conversions are
explicit in shader code where they can be audited. That's a cleaner
place for them than buried in the texture-format declarations.
