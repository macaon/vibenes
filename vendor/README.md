# Vendored crates

Third-party Rust crates copied into this tree because the upstream
release isn't compatible with the rest of our dependency stack. Each
sub-directory is a verbatim snapshot of a published crate plus the
minimum patches needed to compile against our pinned versions.

The Cargo entry point is the `[patch.crates-io]` block in the
top-level `Cargo.toml`, which redirects affected dependents to the
local copies here.

## Current vendored crates

### `librashader-common` and `librashader-runtime-wgpu`

- **Upstream**: <https://github.com/SnowflakePowered/librashader>
- **Vendored from**: master @ `76462c0` (carries version `0.10.1`)
- **License**: MPL-2.0 OR GPL-3.0-only (compatible with our GPL-3.0-or-later)

#### Why we vendor

Upstream `librashader-runtime-wgpu` 0.10.1 (the latest stable, also
the master branch) pins `wgpu = "28"`. Our top-level stack is on
`wgpu = "29"` because `egui-wgpu = "0.34"` is the only egui-wgpu
release that targets wgpu 29 - and `egui-wgpu` skipped wgpu 28
entirely (0.33 was on 27, 0.34 jumped to 29). No combination of
upstream releases lines up. We bump just the two librashader crates
that touch wgpu types directly:

- `librashader-common` (only when its `wgpu` feature is enabled, which
  pulls in `wgpu-types`)
- `librashader-runtime-wgpu` (the runtime that owns
  `wgpu::Device`/`Queue`/`Texture` API surface)

Everything else in the librashader workspace (`presets`, `preprocess`,
`reflect`, `runtime`, `cache`) stays on crates.io unchanged - those
crates don't see `wgpu` in their public API.

#### Patches applied

Tracked in the vendored `Cargo.toml`s and the Rust sources:

1. `librashader-common/Cargo.toml`: `wgpu-types = "29"` (was `"28"` via
   workspace), and inlined the `windows` / `objc2` / `objc2-metal`
   versions that the workspace inherited.
2. `librashader-runtime-wgpu/Cargo.toml`: `wgpu = "29"` (was `"28"`),
   `librashader-common` points at the path-vendored sibling.
3. `librashader-runtime-wgpu/src/graphics_pipeline.rs`: wgpu 29 changed
   `PipelineLayoutDescriptor::bind_group_layouts` from
   `&[&BindGroupLayout]` to `&[Option<&BindGroupLayout>]`. We wrap each
   layout in `Some` since both slots are always populated.

Total source delta: 1 file, ~3 lines. The bulk of this vendoring is
manifest tweaks, not code changes.

#### How to drop this vendoring

When `librashader-runtime-wgpu` releases on crates.io with `wgpu = "29"`
support:

1. Delete `vendor/librashader-common/` and
   `vendor/librashader-runtime-wgpu/`.
2. Remove the `[patch.crates-io]` block from the top-level `Cargo.toml`.
3. Bump our `librashader-runtime-wgpu` dependency line to the new version.
4. `cargo update -p librashader-common -p librashader-runtime-wgpu`.

Upstream tracking: watch
<https://github.com/SnowflakePowered/librashader/blob/master/Cargo.toml>
for the workspace `wgpu` pin to bump past 28.
