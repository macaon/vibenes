//! wgpu passthrough renderer. Owns the surface, device, and a single
//! 256×240 offscreen texture that callers update each frame with the
//! NES framebuffer. A fullscreen-triangle render pass samples the
//! texture and blits it to the swap chain.
//!
//! The architecture is deliberately set up for future post-process
//! shader stages: the offscreen texture is an input, the swap-chain
//! surface is the final output. Additional stages (CRT filter, NTSC
//! decode, scanlines) slot in between as ping-pong render passes on
//! intermediate textures. 6A.3 ships the direct blit; shader stages
//! come later.

use std::sync::Arc;

use anyhow::{Context, Result};
use winit::dpi::PhysicalSize;
use winit::window::Window;

pub const NES_WIDTH: u32 = 256;
pub const NES_HEIGHT: u32 = 240;
const NES_FRAMEBUFFER_BYTES: usize = (NES_WIDTH as usize) * (NES_HEIGHT as usize) * 4;

/// Outcome of a [`Renderer::render`] call. Callers should propagate this
/// to the event loop: transient states (Timeout / Occluded) can be
/// ignored, `SurfaceLost` / `Outdated` should trigger a reconfigure,
/// `Fatal` should exit the app.
pub enum PresentOutcome {
    Presented,
    Skipped,
    NeedsReconfigure,
    Fatal(String),
}

/// Owns the wgpu surface + pipeline. Construct on `resumed`, call
/// [`Renderer::resize`] on window resize events, and [`Renderer::render`]
/// once per frame after writing the latest framebuffer via
/// [`Renderer::upload_framebuffer`].
pub struct Renderer {
    // The window must outlive the surface. Arc keeps it alive as long
    // as any clone exists; the surface holds one clone internally via
    // the `'static` lifetime bound.
    _window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    framebuffer_texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        pollster::block_on(Self::new_async(window))
    }

    async fn new_async(window: Arc<Window>) -> Result<Self> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(Arc::clone(&window))
            .context("create wgpu surface")?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .context("no suitable wgpu adapter")?;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("vibenes.device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits()),
                experimental_features: wgpu::ExperimentalFeatures::default(),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
            })
            .await
            .context("wgpu request_device")?;

        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        let surface_caps = surface.get_capabilities(&adapter);
        // Prefer sRGB so the fragment shader's linear output is gamma-
        // corrected on present. The offscreen framebuffer is also sRGB,
        // so the pipeline is linear end-to-end until we present.
        let surface_format = surface_caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(surface_caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width,
            height,
            // FIFO = vsync. NTSC 60.0988 Hz drift vs the monitor's
            // 60 Hz is a Phase 7 (audio-driven pacing) problem.
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let framebuffer_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vibenes.framebuffer"),
            size: wgpu::Extent3d {
                width: NES_WIDTH,
                height: NES_HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let framebuffer_view =
            framebuffer_texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Nearest-neighbor sampling preserves the pixel look at integer
        // and non-integer scales. Shader stages can add linear filtering
        // or CRT scanlines explicitly.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vibenes.sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("vibenes.bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                        count: None,
                    },
                ],
            });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vibenes.bg"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&framebuffer_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vibenes.passthrough"),
            source: wgpu::ShaderSource::Wgsl(
                include_str!("shaders/passthrough.wgsl").into(),
            ),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vibenes.pll"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vibenes.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: None,
        });

        Ok(Self {
            _window: window,
            surface,
            device,
            queue,
            config,
            pipeline,
            framebuffer_texture,
            bind_group,
        })
    }

    pub fn resize(&mut self, new_size: PhysicalSize<u32>) {
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }
        self.config.width = new_size.width;
        self.config.height = new_size.height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Re-apply the current config. Used after a Lost / Outdated present.
    pub fn reconfigure(&mut self) {
        self.surface.configure(&self.device, &self.config);
    }

    /// Upload a 256×240 RGBA8 buffer as the next frame's source texture.
    /// Callers must hand in exactly `NES_WIDTH * NES_HEIGHT * 4` bytes;
    /// anything else is a programming error.
    pub fn upload_framebuffer(&self, pixels: &[u8]) {
        assert_eq!(
            pixels.len(),
            NES_FRAMEBUFFER_BYTES,
            "framebuffer must be {}×{}×4 = {} bytes",
            NES_WIDTH,
            NES_HEIGHT,
            NES_FRAMEBUFFER_BYTES
        );
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.framebuffer_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(NES_WIDTH * 4),
                rows_per_image: Some(NES_HEIGHT),
            },
            wgpu::Extent3d {
                width: NES_WIDTH,
                height: NES_HEIGHT,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Underlying wgpu device. Used by overlay layers (egui) that need
    /// to build their own pipelines and upload their own resources.
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// Underlying wgpu queue. See [`Renderer::device`].
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Surface format in use. Overlay layers need this to match their
    /// fragment output target.
    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// Current surface extent in physical pixels (width, height).
    pub fn surface_size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// Acquire the next swapchain texture, blit the NES framebuffer into
    /// it (clearing the surface first), invoke `on_overlay` with the
    /// same encoder + view so an overlay can paint on top with
    /// `LoadOp::Load`, then submit and present.
    ///
    /// The overlay closure is handed `(device, queue, view, encoder,
    /// (surface_w, surface_h))` — everything an egui-style pass needs
    /// to upload buffers and encode a second render pass.
    pub fn render_with<F>(&mut self, on_overlay: F) -> PresentOutcome
    where
        F: FnOnce(&wgpu::Device, &wgpu::Queue, &wgpu::TextureView, &mut wgpu::CommandEncoder, (u32, u32)),
    {
        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                t
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return PresentOutcome::Skipped;
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                return PresentOutcome::NeedsReconfigure;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return PresentOutcome::Fatal("surface validation error".into());
            }
        };
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vibenes.encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vibenes.present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        on_overlay(
            &self.device,
            &self.queue,
            &view,
            &mut encoder,
            (self.config.width, self.config.height),
        );
        self.queue.submit(std::iter::once(encoder.finish()));
        surface_texture.present();
        PresentOutcome::Presented
    }

    /// Convenience wrapper around `render_with` for callers that have no
    /// overlay to encode. Equivalent to `render_with(|_, _, _, _, _| {})`.
    pub fn render(&mut self) -> PresentOutcome {
        self.render_with(|_, _, _, _, _| {})
    }
}

/// Static diagnostic pattern used in 6A.3 before the PPU actually draws
/// anything. Vertical gradient with a horizontal red→green→blue stripe
/// at scanline 120 so we can see scale, orientation, and sRGB handling
/// at a glance.
pub fn diagnostic_pattern() -> Vec<u8> {
    let mut buf = vec![0u8; NES_FRAMEBUFFER_BYTES];
    for y in 0..NES_HEIGHT as usize {
        for x in 0..NES_WIDTH as usize {
            let i = (y * NES_WIDTH as usize + x) * 4;
            let stripe_y = NES_HEIGHT as usize / 2;
            if y == stripe_y {
                let third = NES_WIDTH as usize / 3;
                if x < third {
                    buf[i] = 0xFF;
                } else if x < 2 * third {
                    buf[i + 1] = 0xFF;
                } else {
                    buf[i + 2] = 0xFF;
                }
            } else {
                let v = (y * 255 / (NES_HEIGHT as usize - 1)) as u8;
                buf[i] = v;
                buf[i + 1] = v;
                buf[i + 2] = v;
            }
            buf[i + 3] = 0xFF;
        }
    }
    buf
}
