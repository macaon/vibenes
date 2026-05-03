// SPDX-License-Identifier: GPL-3.0-or-later
//! egui-based overlay menu. The menu is hidden during normal play and
//! shown as a centered modal over a darkened freeze-frame when the
//! user opens it (F1 by default - wired in [`crate::main`]).
//!
//! Lifecycle per frame:
//!
//! 1. `on_window_event` - forward winit events to egui before the
//!    emulator sees them.
//! 2. `run` - build the UI by calling egui. When the overlay is open,
//!    keyboard nav (↑/↓/Enter/Backspace) is consumed inside the egui
//!    pass and resolves to commands pushed into `cmds`.
//! 3. `paint` - called from inside [`crate::gfx::Renderer::render_with`]'s
//!    overlay closure. Uploads egui texture deltas, encodes the UI
//!    render pass with `LoadOp::Load` (preserving the NES blit
//!    underneath), and frees textures egui has released.

use std::time::{Duration, Instant};

use egui::{Context, ViewportId};
use egui_wgpu::{Renderer as EguiRenderer, RendererOptions, ScreenDescriptor};
use egui_winit::{EventResponse, State as EguiWinit};
use winit::event::WindowEvent;
use winit::window::Window;

use crate::nes::clock::Region;
use crate::video::VideoSettings;

pub mod commands;
pub mod menubar;
mod menus;
pub mod recent;
pub mod recent_shaders;

pub use commands::UiCommand;
pub use menubar::{MenuBarParams, MENU_BAR_HEIGHT_LOGICAL};
pub use menus::{DebugStatus, OverlayState};
pub use recent::RecentRoms;

/// Logical navigation directions the overlay understands. Queued by
/// the host from non-keyboard sources (gamepad D-pad, face buttons)
/// and translated to synthesized egui key events inside
/// [`UiLayer::run`]. Keeps egui out of the host's public API.
#[derive(Clone, Copy, Debug)]
pub enum NavKey {
    Up,
    Down,
    Select,
    Back,
}

/// Transient one-line message shown at the top of the screen while
/// the overlay is closed. Used by save-state save/load to surface
/// "Saved slot 3" / "Wrong ROM" / "No state in slot 5" without
/// requiring the user to open the menu.
///
/// The `kind` field drives the background tint (info vs error).
/// We deliberately don't expose `Toast` outside this module - the
/// host pushes strings via [`UiLayer::show_toast_info`] and
/// [`UiLayer::show_toast_error`].
struct Toast {
    message: String,
    expires_at: Instant,
    kind: ToastKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Info,
    Error,
}

/// How long a toast stays on screen. Picked to cover the time it
/// takes a user to look up after pressing F2 / F3 - long enough to
/// read, short enough not to block the screen if multiple actions
/// fire in quick succession.
const TOAST_DURATION: Duration = Duration::from_millis(2500);

pub struct UiLayer {
    ctx: Context,
    winit_state: EguiWinit,
    renderer: EguiRenderer,
    /// Captured after `run()`, consumed by `paint()`. None between
    /// paint and the next run; a missing pending output at paint time
    /// is a no-op (safer than panicking when a caller skips a frame).
    pending: Option<egui::FullOutput>,
    overlay: OverlayState,
    /// egui events queued by the host between frames (e.g. from the
    /// gamepad). Prepended to `raw_input.events` at the start of
    /// `run()` so the existing menu `consume_key` logic picks them
    /// up unchanged - no second code path for gamepad navigation.
    queued_events: Vec<egui::Event>,
    /// Active transient toast. Cleared inside `run()` once
    /// [`Instant::now`] passes [`Toast::expires_at`].
    toast: Option<Toast>,
}

impl UiLayer {
    pub fn new(
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
        window: &Window,
    ) -> Self {
        let ctx = Context::default();
        register_pixel_font(&ctx);
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
            queued_events: Vec::new(),
            toast: None,
        }
    }

    /// Show a brief informational toast at the top of the screen.
    /// Replaces any currently-displayed toast - intended for
    /// quick-fire actions (save / load slot) where the user only
    /// cares about the most recent feedback.
    pub fn show_toast_info(&mut self, message: impl Into<String>) {
        self.toast = Some(Toast {
            message: message.into(),
            expires_at: Instant::now() + TOAST_DURATION,
            kind: ToastKind::Info,
        });
        // Force a repaint so the toast appears immediately rather
        // than waiting for the next NES frame tick.
        self.ctx.request_repaint();
    }

    /// Same as [`Self::show_toast_info`] but draws on a red tint.
    /// Used for save-state failures where silence would leave the
    /// user wondering whether F3 did anything.
    pub fn show_toast_error(&mut self, message: impl Into<String>) {
        self.toast = Some(Toast {
            message: message.into(),
            expires_at: Instant::now() + TOAST_DURATION,
            kind: ToastKind::Error,
        });
        self.ctx.request_repaint();
    }

    /// Queue a synthetic navigation key (press + release) to be fed
    /// into egui on the next `run()`. Used for gamepad menu nav -
    /// the overlay then handles it via the same `consume_key` path
    /// the keyboard uses. Calling this while the overlay is closed
    /// is a no-op in practice: the menu only consumes these keys
    /// when it's showing, and egui discards them otherwise.
    pub fn queue_nav(&mut self, nav: NavKey) {
        let key = match nav {
            NavKey::Up => egui::Key::ArrowUp,
            NavKey::Down => egui::Key::ArrowDown,
            NavKey::Select => egui::Key::Enter,
            NavKey::Back => egui::Key::Backspace,
        };
        for pressed in [true, false] {
            self.queued_events.push(egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            });
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

    /// Open the overlay directly on the Disk submenu. Wired to the F4
    /// hotkey; FDS games commonly need quick disk-side swaps mid-play
    /// and tabbing through the root menu each time would be tedious.
    pub fn open_disk_menu(&mut self) {
        self.overlay.open_disk();
    }

    /// Open the overlay directly on the Debug submenu. Wired to F12
    /// in the host so dev diagnostics (scanline ruler, OAM dump)
    /// are one keystroke away.
    pub fn open_debug_menu(&mut self) {
        self.overlay.open_debug();
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
        fds: Option<crate::nes::FdsInfo>,
        debug: DebugStatus,
        menu_bar: MenuBarParams<'_>,
        cmds: &mut Vec<UiCommand>,
    ) {
        // Keep ctx's pixels_per_point in sync with winit's current
        // scale factor - prevents pointer coords and layout from
        // drifting apart on fractional scaling or delayed
        // ScaleFactorChanged events.
        let scale_factor = window.scale_factor() as f32;
        if (self.ctx.pixels_per_point() - scale_factor).abs() > 1e-4 {
            self.ctx.set_pixels_per_point(scale_factor);
        }

        let mut raw_input = self.winit_state.take_egui_input(window);
        // Splice in any host-queued events (gamepad nav). They go
        // at the front so they're processed before whatever egui
        // has synthesized itself this frame.
        if !self.queued_events.is_empty() {
            let queued = std::mem::take(&mut self.queued_events);
            let mut combined = queued;
            combined.append(&mut raw_input.events);
            raw_input.events = combined;
        }
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
        // Drop the toast if it's expired before this frame paints.
        // Done outside `run_ui` so the closure stays Fn-friendly
        // (egui can otherwise panic on borrow conflicts when the
        // toast field is mutated mid-frame).
        if let Some(t) = self.toast.as_ref() {
            if Instant::now() >= t.expires_at {
                self.toast = None;
            }
        }
        let toast = self.toast.as_ref().map(|t| {
            (t.message.clone(), t.kind)
        });
        let full_output = self.ctx.run_ui(raw_input, |ui| {
            // Menu strip first - it claims the top region of the
            // screen via `Panel::top` showing inside the
            // root Ui. The centered modal overlay below uses
            // `egui::Area`s and renders over (or under,
            // depending on order) the menu.
            menubar::run(ui, &menu_bar, cmds);
            // No persistent chrome past the menu strip: the
            // overlay is drawn via `egui::Area`s inside
            // `run_overlay`, which accesses the Context directly
            // rather than nesting under a top-level `ui`.
            menus::run_overlay(
                &self.ctx,
                &mut self.overlay,
                video,
                region,
                recent,
                nes_loaded,
                fds,
                debug,
                cmds,
            );
            // Toast layer sits above the overlay so a save-state
            // confirmation surfaced via the F1 menu's "Save state"
            // entry would still be visible. Drawn as a self-contained
            // Area so it doesn't disturb menu layout.
            if let Some((msg, kind)) = toast.as_ref() {
                draw_toast(&self.ctx, msg, *kind);
            }
        });
        self.pending = Some(full_output);
    }

    /// Paint the UI built by the last `run` into `view`, using the
    /// already-recording `encoder`. Safe to call even if `run` was
    /// skipped - returns silently with no overlay drawn.
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
            // `encoder` - any accidental mid-pass encoder mutation
            // becomes a runtime validation error instead.
            let mut pass = pass.forget_lifetime();
            self.renderer.render(&mut pass, &paint_jobs, &screen);
        }
        for id in &full_output.textures_delta.free {
            self.renderer.free_texture(id);
        }
    }
}

/// Render a transient one-line toast at the top-center of the
/// screen. Background tint = blue for info, red for error - keeps
/// the visual register consistent across save-state outcomes.
/// Uses a self-contained `egui::Area` so it doesn't perturb the
/// overlay menu's layout when both are visible at once.
fn draw_toast(ctx: &Context, message: &str, kind: ToastKind) {
    let (bg_fill, text_color) = match kind {
        ToastKind::Info => (
            egui::Color32::from_rgba_premultiplied(20, 28, 60, 220),
            egui::Color32::from_rgb(220, 220, 240),
        ),
        ToastKind::Error => (
            egui::Color32::from_rgba_premultiplied(80, 18, 18, 220),
            egui::Color32::from_rgb(255, 220, 200),
        ),
    };
    egui::Area::new(egui::Id::new("vibenes.toast"))
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 24.0))
        .order(egui::Order::Foreground)
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::default()
                .fill(bg_fill)
                .corner_radius(egui::CornerRadius::same(6))
                .inner_margin(egui::Margin {
                    left: 14,
                    right: 14,
                    top: 8,
                    bottom: 8,
                })
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(message)
                            .monospace()
                            .size(16.0)
                            .color(text_color),
                    );
                });
        });
}

/// Install VT323 (SIL OFL 1.1) as the first family for
/// `FontFamily::Monospace`. The overlay uses `FontId::monospace(...)`
/// for all its text, so this pins our pixel-font look without
/// touching call sites. License + copyright travel with the font in
/// `assets/fonts/VT323-OFL.txt`.
fn register_pixel_font(ctx: &Context) {
    const VT323_TTF: &[u8] = include_bytes!("../../assets/fonts/VT323-Regular.ttf");
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "vt323".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(VT323_TTF)),
    );
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "vt323".to_owned());
    ctx.set_fonts(fonts);
}
