// SPDX-License-Identifier: GPL-3.0-or-later
//! RetroArch shader chain wrapper around `librashader-runtime-wgpu`.
//!
//! One [`ShaderRuntime`] instance corresponds to one loaded preset
//! (`.slangp` / `.glslp` / `.cgp`). The renderer holds it as
//! `Option<ShaderRuntime>` - `None` means "fall back to the built-in
//! passthrough blit". To swap presets, drop the current runtime and
//! load a new one; the chain is GPU state that cannot be re-pointed
//! at a different preset in place.
//!
//! `frame()` records all draw calls onto the caller's command encoder
//! and writes the final pass into a region of the supplied output
//! view (typically the swapchain). The viewport offset/size let us
//! clip into the unreserved region of the surface (we use this to
//! keep the menu strip rows untouched, see [`crate::gfx::Renderer`]).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use librashader_common::{Size, Viewport};
use librashader_presets::ShaderFeatures;
use librashader_runtime_wgpu::{FilterChainWgpu, WgpuOutputView};

pub struct ShaderRuntime {
    chain: FilterChainWgpu,
    path: PathBuf,
    /// Monotonic frame index handed to the chain each call. Used by
    /// shaders that animate (CRT phosphor decay, refresh-cycle
    /// effects). Wraps every ~ a year of continuous play.
    frame_count: usize,
}

impl ShaderRuntime {
    /// Parse the preset at `path` and initialise the GPU pipeline for
    /// every pass. Shader-include resolution and LUT loading are
    /// relative to the preset's directory.
    pub fn load(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        path: impl AsRef<Path>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let chain = FilterChainWgpu::load_from_path(
            &path,
            ShaderFeatures::empty(),
            device,
            queue,
            None,
        )
        .with_context(|| format!("load shader preset {}", path.display()))?;
        Ok(Self {
            chain,
            path,
            frame_count: 0,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record one frame's worth of shader draws onto `encoder`. The
    /// final pass writes pixels into the rectangle
    /// `(viewport_offset, viewport_size)` of `output_view`. Pixels
    /// outside that rectangle are left untouched - a previous render
    /// pass on the same view (clear or otherwise) is responsible for
    /// the chrome region.
    pub fn frame(
        &mut self,
        input: &wgpu::Texture,
        output_view: &wgpu::TextureView,
        output_format: wgpu::TextureFormat,
        output_size: (u32, u32),
        viewport_offset: (u32, u32),
        viewport_size: (u32, u32),
        encoder: &mut wgpu::CommandEncoder,
    ) -> Result<()> {
        let view = WgpuOutputView::new_from_raw(
            output_view,
            Size {
                width: output_size.0,
                height: output_size.1,
            },
            output_format,
        );
        let viewport = Viewport {
            x: viewport_offset.0 as f32,
            y: viewport_offset.1 as f32,
            mvp: None,
            output: view,
            size: Size {
                width: viewport_size.0,
                height: viewport_size.1,
            },
        };
        self.chain
            .frame(input, &viewport, encoder, self.frame_count, None)
            .context("librashader frame")?;
        self.frame_count = self.frame_count.wrapping_add(1);
        Ok(())
    }
}
