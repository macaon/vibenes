//! egui-based overlay menu. The menu is hidden during normal play and
//! shown as a centered modal over a darkened freeze-frame when the
//! user opens it (F1 by default — wired in [`crate::main`]).
//!
//! Lifecycle per frame:
//!
//! 1. `on_window_event` — forward winit events to egui before the
//!    emulator sees them.
//! 2. `run` — build the UI by calling egui. When the overlay is open,
//!    keyboard nav (↑/↓/Enter/Backspace) is consumed inside the egui
//!    pass and resolves to commands pushed into `cmds`.
//! 3. `paint` — called from inside [`crate::gfx::Renderer::render_with`]'s
//!    overlay closure. Uploads egui texture deltas, encodes the UI
//!    render pass with `LoadOp::Load` (preserving the NES blit
//!    underneath), and frees textures egui has released.

use egui::{Context, ViewportId};
use egui_wgpu::{Renderer as EguiRenderer, RendererOptions, ScreenDescriptor};
use egui_winit::{EventResponse, State as EguiWinit};
use winit::event::WindowEvent;
use winit::window::Window;

use crate::clock::Region;
use crate::video::VideoSettings;

pub mod commands;
mod menus;
pub mod recent;

pub use commands::UiCommand;
pub use menus::OverlayState;
pub use recent::RecentRoms;

pub struct UiLayer {
    ctx: Context,
    winit_state: EguiWinit,
    renderer: EguiRenderer,
    /// Captured after `run()`, consumed by `paint()`. None between
    /// paint and the next run; a missing pending output at paint time
    /// is a no-op (safer than panicking when a caller skips a frame).
    pending: Option<egui::FullOutput>,
    overlay: OverlayState,
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
            overlay: OverlayState::default(),
        }
    }

    /// Whether the overlay is currently shown. The host pauses
    /// emulation while this is true and routes input to the overlay
    /// instead of the NES controller.
    pub fn is_overlay_open(&self) -> bool {
        self.overlay.open
    }

    /// Toggle the overlay (open → closed, closed → root screen).
    /// Wired to F1 in the host.
    pub fn toggle_overlay(&mut self) {
        self.overlay.toggle();
    }

    /// Back out of the current submenu (or close from root). Wired to
    /// the Esc key by the host when the overlay is open.
    pub fn back_or_close_overlay(&mut self) {
        self.overlay.back_or_close();
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

    /// Build the UI for the current frame. When the overlay is open,
    /// renders the dim layer + menu and dispatches user selections
    /// into `cmds`. When closed, this still runs an empty egui pass so
    /// input flows through cleanly.
    pub fn run(
        &mut self,
        window: &Window,
        surface_size: (u32, u32),
        recent: &RecentRoms,
        video: &VideoSettings,
        region: Option<Region>,
        nes_loaded: bool,
        cmds: &mut Vec<UiCommand>,
    ) {
        // Keep ctx's pixels_per_point in sync with winit's current
        // scale factor — prevents pointer coords and layout from
        // drifting apart on fractional scaling or delayed
        // ScaleFactorChanged events.
        let scale_factor = window.scale_factor() as f32;
        if (self.ctx.pixels_per_point() - scale_factor).abs() > 1e-4 {
            self.ctx.set_pixels_per_point(scale_factor);
        }

        let mut raw_input = self.winit_state.take_egui_input(window);
        // Pin egui's screen_rect to the wgpu surface we'll render
        // into. Belt-and-suspenders alongside the per-frame surface
        // sync in `advance_and_present`: if `window.inner_size()`
        // briefly diverges from the surface, egui still lays out in
        // surface space.
        let logical_w = surface_size.0 as f32 / scale_factor;
        let logical_h = surface_size.1 as f32 / scale_factor;
        raw_input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(logical_w, logical_h),
        ));
        let full_output = self.ctx.run_ui(raw_input, |_ui| {
            // No persistent chrome: the overlay is the only UI element.
            // It's drawn via `egui::Area`s inside `run_overlay`, which
            // accesses the Context directly rather than nesting under
            // a top-level `ui`.
            menus::run_overlay(
                &self.ctx,
                &mut self.overlay,
                video,
                region,
                recent,
                nes_loaded,
                cmds,
            );
        });
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
