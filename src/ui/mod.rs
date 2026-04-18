//! egui-based menu overlay. Owns the egui context, the winit→egui
//! event adapter, and the `egui-wgpu` renderer.
//!
//! Lifecycle per frame:
//!
//! 1. `on_window_event` — forward winit events to egui before the
//!    emulator sees them. Returned `EventResponse::consumed` is the
//!    signal to short-circuit emulator input handling.
//! 2. `run` — build the UI by calling egui. Caches the resulting
//!    `FullOutput` for the subsequent paint.
//! 3. `paint` — called from inside [`crate::gfx::Renderer::render_with`]'s
//!    overlay closure. Uploads egui texture deltas, encodes the UI
//!    render pass with `LoadOp::Load` (preserving the NES blit
//!    underneath), and frees textures egui has released.
//!
//! The menubar itself is just a list of `ui.menu_button` stubs; real
//! actions land in the next sub-phase behind a `UiCommand` queue.

use egui::{Context, ViewportId};
use egui_wgpu::{Renderer as EguiRenderer, RendererOptions, ScreenDescriptor};
use egui_winit::{EventResponse, State as EguiWinit};
use winit::event::WindowEvent;
use winit::window::Window;

pub mod commands;
mod menus;
pub mod recent;

pub use commands::UiCommand;
pub use recent::RecentRoms;

pub struct UiLayer {
    ctx: Context,
    winit_state: EguiWinit,
    renderer: EguiRenderer,
    /// Captured after `run()`, consumed by `paint()`. None between
    /// paint and the next run; a missing pending output at paint time
    /// is a no-op (safer than panicking when a caller skips a frame).
    pending: Option<egui::FullOutput>,
    /// Height of the top menubar in physical pixels as of the last
    /// `run()`. Callers use this to letterbox the NES render below
    /// the menubar so it doesn't get overdrawn.
    menubar_height_px: u32,
}

impl UiLayer {
    pub fn new(
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
        window: &Window,
    ) -> Self {
        let ctx = Context::default();
        let winit_state = EguiWinit::new(
            ctx.clone(),
            ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let renderer = EguiRenderer::new(device, surface_format, RendererOptions::default());
        Self {
            ctx,
            winit_state,
            renderer,
            pending: None,
            menubar_height_px: 0,
        }
    }

    /// Physical-pixel height of the menubar as of the last `run()`.
    /// Callers use this to inset the NES render so it sits below the
    /// menubar instead of being overdrawn by it.
    pub fn menubar_height_px(&self) -> u32 {
        self.menubar_height_px
    }

    /// Forward a winit event to egui. Callers should check the returned
    /// `EventResponse::consumed` and skip their own handling when
    /// egui has taken ownership (e.g. while a text field is focused).
    pub fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> EventResponse {
        self.winit_state.on_window_event(window, event)
    }

    /// Inform egui that the window's DPI scale changed. Called from the
    /// `ScaleFactorChanged` event.
    pub fn on_scale_factor_changed(&mut self, scale_factor: f32) {
        self.ctx.set_pixels_per_point(scale_factor);
    }

    /// Build the UI for the current frame and stash the output for
    /// `paint`. Widgets push `UiCommand` variants into `cmds`; the host
    /// drains them after `paint` returns. The menubar's rendered
    /// height is captured so the host can letterbox the NES render
    /// below it.
    pub fn run(&mut self, window: &Window, recent: &RecentRoms, cmds: &mut Vec<UiCommand>) {
        let raw_input = self.winit_state.take_egui_input(window);
        let mut menubar_height_points = 0.0_f32;
        let full_output = self.ctx.run_ui(raw_input, |ui| {
            menubar_height_points = menus::build_top_menubar(ui, recent, cmds);
        });
        // egui reports layout in logical "points"; scale to physical
        // pixels and round up so we reserve a whole-pixel strip (a
        // fractional viewport offset produces a one-pixel bleed at
        // HiDPI).
        let ppp = full_output.pixels_per_point;
        self.menubar_height_px = (menubar_height_points * ppp).ceil() as u32;
        self.pending = Some(full_output);
    }

    /// Paint the UI built by the last `run` into `view`, using the
    /// already-recording `encoder`. Safe to call even if `run` was
    /// skipped — returns silently with no overlay drawn.
    pub fn paint(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        encoder: &mut wgpu::CommandEncoder,
        surface_size: (u32, u32),
        window: &Window,
    ) {
        let Some(full_output) = self.pending.take() else {
            return;
        };
        // Clipboard / cursor icon / IME state back-propagation.
        self.winit_state
            .handle_platform_output(window, full_output.platform_output);
        let pixels_per_point = full_output.pixels_per_point;
        let paint_jobs = self.ctx.tessellate(full_output.shapes, pixels_per_point);
        let screen = ScreenDescriptor {
            size_in_pixels: [surface_size.0, surface_size.1],
            pixels_per_point,
        };
        for (id, delta) in &full_output.textures_delta.set {
            self.renderer.update_texture(device, queue, *id, delta);
        }
        let _user_cmds =
            self.renderer
                .update_buffers(device, queue, encoder, &paint_jobs, &screen);
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vibenes.ui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // egui_wgpu::Renderer::render takes RenderPass<'static>.
            // `forget_lifetime` drops the compile-time borrow of
            // `encoder` — any accidental mid-pass encoder mutation
            // becomes a runtime validation error instead.
            let mut pass = pass.forget_lifetime();
            self.renderer.render(&mut pass, &paint_jobs, &screen);
        }
        for id in &full_output.textures_delta.free {
            self.renderer.free_texture(id);
        }
    }
}
