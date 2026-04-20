//! `vibenes` — the windowed emulator binary. Phase 6A.4 hooks the
//! PPU's real framebuffer in: step the NES until a frame completes,
//! upload the result, present, repeat. Pace is vsync via wgpu's Fifo
//! present mode; NTSC 60.0988 Hz drift is a Phase 7 (audio-pacing)
//! problem.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use vibenes::app;
use vibenes::audio;
use vibenes::clock::Region;
use vibenes::gfx::{PresentOutcome, Renderer};
use vibenes::nes::Nes;
use vibenes::rom::Cartridge;
use vibenes::ui::{RecentRoms, UiCommand, UiLayer};
use vibenes::video::VideoSettings;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

const WINDOW_TITLE: &str = "vibenes";

/// NTSC NES frame period: 1 / (master 21477272 Hz ÷ 4 dots ÷ 89342
/// dots per frame) ≈ 16.639 ms. We pace the emulation loop to this
/// explicitly rather than relying on wgpu's Fifo present mode, since
/// Fifo caps to the *monitor* refresh rate — anything above 60 Hz
/// (144 Hz gaming displays, 120 Hz laptops, etc.) would run the
/// emulator faster than real hardware.
const NTSC_FRAME_PERIOD: Duration = Duration::from_nanos(16_639_267);
/// PAL equivalent: 33247.5 CPU cycles ÷ (26601712 / 16) Hz ≈ 19.997
/// ms per frame. Used when the loaded ROM is PAL.
const PAL_FRAME_PERIOD: Duration = Duration::from_nanos(19_997_194);

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vibenes: {:#}", e);
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<()> {
    let rom_path = parse_args();

    // Audio stream opens eagerly with the NTSC clock. If the user
    // later loads a PAL ROM the sink is re-tuned inside swap_cartridge
    // so pitch stays correct. Opening up-front (before any ROM is
    // loaded) means the cpal device handle is ready the moment the
    // first ROM is chosen from the File menu — no perceptible delay.
    let (audio_sink, audio_stream) = match audio::start(Region::Ntsc.cpu_clock_hz()) {
        Ok((sink, stream)) => {
            eprintln!("audio: {} Hz × {} ch", stream.sample_rate, stream.channels);
            (Some(sink), Some(stream))
        }
        Err(e) => {
            eprintln!("vibenes: audio disabled ({e:#})");
            (None, None)
        }
    };

    let initial_nes = match rom_path.as_deref() {
        Some(path) => match Cartridge::load(path)
            .with_context(|| format!("loading ROM {}", path.display()))
        {
            Ok(cart) => {
                eprintln!("loaded: {}", cart.describe());
                let nes = app::build_nes(cart)?;
                eprintln!("region={:?} reset PC=${:04X}", nes.region(), nes.cpu.pc);
                Some(nes)
            }
            Err(e) => {
                // Fall back to a no-ROM launch so the user can pick a
                // valid ROM from the File menu instead of exiting.
                eprintln!("vibenes: {e:#}");
                None
            }
        },
        None => {
            eprintln!("vibenes: no ROM specified — use File → Open ROM…");
            None
        }
    };

    let event_loop = EventLoop::new().context("create winit event loop")?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut handler = App::new(initial_nes, audio_sink, audio_stream, rom_path);
    event_loop.run_app(&mut handler).context("winit event loop")?;
    Ok(())
}

fn frame_period_for(region: Region) -> Duration {
    match region {
        Region::Ntsc => NTSC_FRAME_PERIOD,
        Region::Pal => PAL_FRAME_PERIOD,
    }
}

fn parse_args() -> Option<PathBuf> {
    std::env::args_os().skip(1).next().map(PathBuf::from)
}

/// Window + renderer + optional NES owner. Each RedrawRequested drives
/// the NES (if loaded) for one PPU frame and presents the PPU frame
/// buffer, or a black surface when no ROM has been loaded yet.
struct App {
    nes: Option<Nes>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    ui: Option<UiLayer>,
    halted_notice_shown: bool,
    /// Per-region frame period (NTSC ≈ 16.639 ms, PAL ≈ 19.997 ms).
    /// Defaults to NTSC when no ROM is loaded so the idle repaint rate
    /// still matches a reasonable cadence. Updated on ROM load / swap.
    frame_period: Duration,
    /// Deadline for the next frame's present. Advanced by one
    /// `frame_period` each completed frame so drift stays pinned to
    /// wall-clock rather than accumulating.
    next_frame_deadline: Option<Instant>,
    /// Host audio sink. Held here until the first ROM loads, at which
    /// point it moves into the `Nes`. On subsequent swaps it stays
    /// inside `Nes` and is re-tuned via `swap_cartridge`.
    pending_audio_sink: Option<audio::AudioSink>,
    /// Keeps the cpal output stream alive. Dropping this silences the
    /// device — hence why it lives on the App owner rather than a
    /// local inside `run()`.
    _audio_stream: Option<audio::AudioStream>,
    /// Most-recently-loaded ROMs, shown in the File menu. Seeded with
    /// the path passed on the command line (if any).
    recent_roms: RecentRoms,
    /// Integer scale + pixel aspect ratio. Window inner size equals
    /// `video.content_size(region)` exactly — no chrome to subtract,
    /// no fractional scales.
    video: VideoSettings,
    /// Set when video settings or region change so the post-frame hook
    /// requests a window resize once and clears. Avoids per-frame
    /// `request_inner_size` calls that on some compositors cause a
    /// resize-storm and disrupt frame pacing.
    pending_window_resize: bool,
}

impl App {
    fn new(
        nes: Option<Nes>,
        audio_sink: Option<audio::AudioSink>,
        audio_stream: Option<audio::AudioStream>,
        initial_rom: Option<PathBuf>,
    ) -> Self {
        let frame_period = frame_period_for(nes.as_ref().map_or(Region::Ntsc, Nes::region));
        let mut recent_roms = RecentRoms::default();
        if let Some(p) = initial_rom {
            recent_roms.push(p);
        }
        // If a ROM was loaded at startup, attach the sink to it now so
        // the first frame already produces audio. Otherwise hold the
        // sink here until load_rom fires on the first File → Open.
        // The sink is tuned to NTSC at `audio::start` time; a PAL ROM
        // on the command line must retune before attach, otherwise
        // BlipBuf resamples at the wrong CPU clock rate (pitch ~7.6%
        // off, and the ring drains faster than it fills).
        let (nes, pending_audio_sink) = match (nes, audio_sink) {
            (Some(mut nes), Some(mut sink)) => {
                sink.set_cpu_clock(nes.region().cpu_clock_hz());
                nes.attach_audio(sink);
                (Some(nes), None)
            }
            (nes, sink) => (nes, sink),
        };
        Self {
            nes,
            window: None,
            renderer: None,
            ui: None,
            halted_notice_shown: false,
            frame_period,
            next_frame_deadline: None,
            pending_audio_sink,
            _audio_stream: audio_stream,
            recent_roms,
            video: VideoSettings::default(),
            pending_window_resize: false,
        }
    }

    fn region_opt(&self) -> Option<Region> {
        self.nes.as_ref().map(Nes::region)
    }

    /// Physical inner size of the window — exactly the NES content
    /// area at the current scale + effective PAR. No menubar reserve;
    /// the in-game overlay paints on top of the framebuffer.
    fn desired_window_size(&self) -> PhysicalSize<u32> {
        let (cw, ch) = self.video.content_size(self.region_opt());
        PhysicalSize::new(cw, ch)
    }

    /// One-shot window resize triggered by the `pending_window_resize`
    /// flag. Set the flag from any code path that changes the desired
    /// size: scale / PAR commands, ROM region change.
    ///
    /// Fires immediately. The overlay re-centers itself on the new
    /// window size every frame, so mid-menu scale / aspect changes
    /// just redraw the card in the resized window without visual
    /// glitches.
    fn apply_pending_resize(&mut self) {
        if !self.pending_window_resize {
            return;
        }
        self.pending_window_resize = false;
        let Some(window) = self.window.as_ref() else { return };
        let _ = window.request_inner_size(self.desired_window_size());
    }

    fn advance_and_present(&mut self, event_loop: &ActiveEventLoop) {
        // Snapshot inputs before grabbing mutable borrows.
        let region = self.region_opt();
        let nes_loaded = self.nes.is_some();
        let overlay_open = self
            .ui
            .as_ref()
            .is_some_and(|ui| ui.is_overlay_open());

        let renderer = match self.renderer.as_mut() {
            Some(r) => r,
            None => return,
        };

        // Defensive surface sync. `WindowEvent::Resized` is not always
        // delivered on Wayland after `request_inner_size` (the
        // compositor may transition size via `Configured` without
        // winit surfacing a Resized event). If we render at stale
        // surface dimensions while `window.inner_size()` has moved
        // on, egui lays out the overlay in one coord space but paints
        // into a smaller swapchain, and pointer hit-tests drift by
        // the inner/surface ratio. Re-configuring before the frame
        // keeps surface ≡ window at all times.
        if let Some(window) = self.window.as_ref() {
            let inner = window.inner_size();
            let (sw, sh) = renderer.surface_size();
            if inner.width != sw || inner.height != sh {
                renderer.resize(inner);
            }
        }

        // Step + audio only when not paused by the overlay. The
        // framebuffer freezes on the last presented frame, which the
        // overlay then dims via a translucent pass.
        if !overlay_open {
            if let Some(nes) = self.nes.as_mut() {
                if !nes.cpu.halted {
                    if let Err(msg) = nes.step_until_frame() {
                        if !self.halted_notice_shown {
                            eprintln!("vibenes: CPU error: {msg}");
                            self.halted_notice_shown = true;
                        }
                    } else if nes.cpu.halted && !self.halted_notice_shown {
                        let reason = nes
                            .cpu
                            .halt_reason
                            .clone()
                            .unwrap_or_else(|| "halted".to_string());
                        eprintln!("vibenes: CPU halted: {reason}");
                        self.halted_notice_shown = true;
                    }
                }
                // Hand the frame's audio to the ring so the cpal
                // callback can drain it before the next wakeup.
                nes.end_audio_frame();
            }
        }
        if let Some(nes) = self.nes.as_ref() {
            renderer.upload_framebuffer(&nes.bus.ppu.frame_buffer);
        }

        let mut cmds: Vec<UiCommand> = Vec::new();
        let surface_size = renderer.surface_size();
        if let (Some(ui), Some(window)) = (self.ui.as_mut(), self.window.as_ref()) {
            ui.run(
                window,
                surface_size,
                &self.recent_roms,
                &self.video,
                region,
                nes_loaded,
                &mut cmds,
            );
        }
        let ui_window = self.window.clone();
        let ui = self.ui.as_mut();
        let outcome = renderer.render_with(|device, queue, view, encoder, size| {
            if let (Some(ui), Some(window)) = (ui, ui_window.as_ref()) {
                ui.paint(device, queue, view, encoder, size, window);
            }
        });
        match outcome {
            PresentOutcome::Presented | PresentOutcome::Skipped => {}
            PresentOutcome::NeedsReconfigure => renderer.reconfigure(),
            PresentOutcome::Fatal(msg) => {
                eprintln!("vibenes: {msg}");
                event_loop.exit();
            }
        }
        for cmd in cmds {
            self.apply_ui_command(cmd, event_loop);
        }
        self.apply_pending_resize();

        // Advance the frame deadline for NTSC/PAL-accurate pacing. If
        // we've fallen more than a couple of frames behind (heavy host
        // contention, long GC pause, whatever), reset the deadline
        // anchor to "now" instead of accumulating slip.
        let now = Instant::now();
        let next = self
            .next_frame_deadline
            .map(|d| d + self.frame_period)
            .unwrap_or(now + self.frame_period);
        self.next_frame_deadline = Some(if next + self.frame_period < now {
            now + self.frame_period
        } else {
            next
        });
    }
}

impl ApplicationHandler for App {
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Drive the frame cadence here rather than calling
        // `request_redraw` immediately inside `RedrawRequested`
        // (which would busy-loop on fast monitors). If the deadline
        // has arrived, request a redraw now; otherwise tell winit to
        // wake us up exactly when it arrives.
        let Some(deadline) = self.next_frame_deadline else {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        };
        let now = Instant::now();
        if now >= deadline {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::Poll);
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Initial inner size = NES content area at the current scale +
        // effective PAR. No menubar reserve — the in-game overlay
        // paints on top of the framebuffer when opened.
        let attrs = Window::default_attributes()
            .with_title(WINDOW_TITLE)
            .with_resizable(false)
            .with_inner_size(self.desired_window_size());
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("vibenes: failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };
        let renderer = match Renderer::new(Arc::clone(&window)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("vibenes: failed to init wgpu: {e:#}");
                event_loop.exit();
                return;
            }
        };
        let ui = UiLayer::new(renderer.device(), renderer.surface_format(), &window);
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.ui = Some(ui);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        // egui gets first look at every event so text fields, menu
        // navigation, clipboard, etc. work. If it consumes the event
        // we skip emulator-side handling entirely. Escape is a hard
        // override — we always want it to exit the app.
        let consumed_by_ui = match (self.ui.as_mut(), self.window.as_ref()) {
            (Some(ui), Some(window)) => ui.on_window_event(window, &event).consumed,
            _ => false,
        };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(ui) = self.ui.as_mut() {
                    ui.on_scale_factor_changed(scale_factor as f32);
                }
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state,
                        repeat: false,
                        ..
                    },
                ..
            } => {
                let overlay_open = self
                    .ui
                    .as_ref()
                    .is_some_and(|ui| ui.is_overlay_open());

                // F1 always toggles the overlay regardless of state —
                // the user expects a single "menu" key to work both
                // ways.
                if code == KeyCode::F1 && state == ElementState::Pressed {
                    if let Some(ui) = self.ui.as_mut() {
                        ui.toggle_overlay();
                    }
                } else if code == KeyCode::Escape && state == ElementState::Pressed {
                    // Esc backs out of the overlay (or closes it from
                    // root); when the overlay is closed it quits the
                    // app, matching the prior behavior.
                    if overlay_open {
                        if let Some(ui) = self.ui.as_mut() {
                            ui.back_or_close_overlay();
                        }
                    } else {
                        event_loop.exit();
                    }
                } else if overlay_open {
                    // Overlay handles arrow / Enter / Backspace inside
                    // its egui pass via `consume_key`. NES controller
                    // input is gated off so Z/X/Enter etc. don't leak
                    // into the cartridge while the menu is up.
                } else if consumed_by_ui {
                    // egui owns this event (e.g. clipboard). Don't
                    // forward to the NES.
                } else if code == KeyCode::KeyR && state == ElementState::Pressed {
                    self.reset_nes();
                } else {
                    self.apply_controller_input(code, state);
                }
            }
            WindowEvent::Resized(new_size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(new_size);
                }
            }
            WindowEvent::RedrawRequested => {
                self.advance_and_present(event_loop);
                // Do NOT immediately request another redraw here —
                // `about_to_wait` drives the next frame off
                // `next_frame_deadline` so we stay pinned to
                // NTSC/PAL rate regardless of monitor refresh.
            }
            _ => {}
        }
    }
}

impl App {
    fn apply_ui_command(&mut self, cmd: UiCommand, event_loop: &ActiveEventLoop) {
        match cmd {
            UiCommand::OpenRomDialog => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("NES ROM", &["nes"])
                    .pick_file()
                {
                    self.load_rom(&path);
                }
            }
            UiCommand::OpenRom(path) => self.load_rom(&path),
            UiCommand::Quit => event_loop.exit(),
            UiCommand::SetScale(n) => {
                self.video = self.video.with_scale(n);
                self.pending_window_resize = true;
            }
            UiCommand::SetAspectRatio(par_mode) => {
                self.video = self.video.with_par_mode(par_mode);
                self.pending_window_resize = true;
            }
            UiCommand::Reset => self.reset_nes(),
        }
    }

    fn reset_nes(&mut self) {
        if let Some(nes) = self.nes.as_mut() {
            nes.reset();
            self.halted_notice_shown = false;
            eprintln!("vibenes: reset (PC=${:04X})", nes.cpu.pc);
        }
    }

    fn load_rom(&mut self, path: &Path) {
        let cart = match Cartridge::load(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("vibenes: failed to load {}: {e:#}", path.display());
                return;
            }
        };
        eprintln!("loaded: {}", cart.describe());
        let region = match self.nes.as_mut() {
            Some(nes) => {
                if let Err(e) = nes.swap_cartridge(cart) {
                    eprintln!("vibenes: swap failed: {e:#}");
                    return;
                }
                nes.region()
            }
            None => {
                // First-load path: build a fresh Nes and attach the
                // audio sink we've been holding since startup. Tune the
                // sink to this ROM's clock before handing it over so
                // the very first sample is correctly pitched.
                let mut nes = match Nes::from_cartridge(cart) {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("vibenes: build failed: {e:#}");
                        return;
                    }
                };
                if let Some(mut sink) = self.pending_audio_sink.take() {
                    sink.set_cpu_clock(nes.region().cpu_clock_hz());
                    nes.attach_audio(sink);
                }
                let region = nes.region();
                self.nes = Some(nes);
                region
            }
        };
        // The new ROM may have a different TV system; re-pin the frame
        // deadline to its cadence and reset the deadline anchor so we
        // don't eat the full delta at once. The window may also need
        // to resize when PAR is Auto and the region flipped between
        // NTSC and PAL.
        self.frame_period = frame_period_for(region);
        self.next_frame_deadline = None;
        self.halted_notice_shown = false;
        self.pending_window_resize = true;
        self.recent_roms.push(path.to_path_buf());
        let pc = self.nes.as_ref().map(|n| n.cpu.pc).unwrap_or(0);
        eprintln!(
            "vibenes: loaded {} (region={:?} PC=${:04X})",
            path.display(),
            region,
            pc,
        );
    }

    /// Map keyboard keys to NES controller-1 bits. The NES shifter
    /// reads LSB-first in this order: A, B, Select, Start, Up, Down,
    /// Left, Right (see `Controller::read`). Layout mirrors the
    /// common Mesen / FCEUX default:
    ///
    /// | Key                 | NES button |
    /// | ------------------- | ---------- |
    /// | X                   | A          |
    /// | Z                   | B          |
    /// | Right Shift         | Select     |
    /// | Enter / Return      | Start      |
    /// | Arrow keys          | D-pad      |
    ///
    /// `R` triggers a warm reset (the console's Reset button) and is
    /// handled in `window_event` before this function runs.
    ///
    /// Key-repeat events are ignored (filtered at the call site) so
    /// holding a button doesn't toggle it. Only physical press/release
    /// edges flip the bit.
    fn apply_controller_input(&mut self, code: KeyCode, state: ElementState) {
        let bit: u8 = match code {
            KeyCode::KeyX => 0x01,        // A
            KeyCode::KeyZ => 0x02,        // B
            KeyCode::ShiftRight => 0x04,  // Select
            KeyCode::Enter => 0x08,       // Start
            KeyCode::ArrowUp => 0x10,
            KeyCode::ArrowDown => 0x20,
            KeyCode::ArrowLeft => 0x40,
            KeyCode::ArrowRight => 0x80,
            _ => return,
        };
        let Some(nes) = self.nes.as_mut() else { return };
        let c = &mut nes.bus.controllers[0];
        match state {
            ElementState::Pressed => c.buttons |= bit,
            ElementState::Released => c.buttons &= !bit,
        }
    }
}
