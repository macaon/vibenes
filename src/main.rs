// SPDX-License-Identifier: GPL-3.0-or-later
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
use vibenes::config::Config;
use vibenes::gfx::{PresentOutcome, Renderer};
use vibenes::nes::Nes;
use vibenes::rom::Cartridge;
use vibenes::ui::{NavKey, RecentRoms, UiCommand, UiLayer};
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
    let cli = parse_args();
    let rom_path = cli.rom_path.clone();

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
        Some(path) => match Cartridge::load_with_fds_bios(path, cli.fds_bios.as_deref())
            .with_context(|| format!("loading ROM {}", path.display()))
        {
            Ok(cart) => {
                eprintln!("loaded: {}", cart.describe());
                // CRITICAL: the CLI-load path MUST attach save
                // metadata before handing the Nes to App. Without
                // this, `save_battery` early-returns on every
                // trigger because `save_meta` is None — which is
                // exactly what slipped past review and caused
                // Zelda (et al.) to never persist progress. The
                // File-menu path does its own attach inside
                // `App::load_rom`; this is the companion.
                let crc = cart.prg_chr_crc32;
                let mut nes = app::build_nes(cart)?;
                nes.attach_save_metadata(path.to_path_buf(), crc);
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

/// Parsed command-line arguments. Add fields here when new flags
/// land — keeping all CLI state in one struct keeps `run()` free of
/// arg-parsing tangles.
struct CliArgs {
    rom_path: Option<PathBuf>,
    /// `--fds-bios <path>`. Overrides the XDG / ROM-dir BIOS search
    /// for this session only. Also accepted via the
    /// `VIBENES_FDS_BIOS` environment variable (see
    /// [`vibenes::fds::bios`]).
    fds_bios: Option<PathBuf>,
}

fn parse_args() -> CliArgs {
    let mut rom_path = None;
    let mut fds_bios = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--fds-bios" {
            fds_bios = args.next().map(PathBuf::from);
        } else if rom_path.is_none() {
            rom_path = Some(PathBuf::from(arg));
        } else {
            eprintln!(
                "vibenes: ignoring extra positional argument '{}'",
                PathBuf::from(&arg).display()
            );
        }
    }
    CliArgs { rom_path, fds_bios }
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
    /// Runtime configuration. Today it's just [`Default::default`];
    /// when a settings UI lands this gets loaded from disk (see
    /// [`vibenes::config`]).
    config: Config,
    /// Frames since the last periodic safety flush. Reset on every
    /// `nes.save_battery()` call from the autosave hook (whether or
    /// not it wrote — either way we've consulted the mapper). The
    /// authoritative save triggers are quit + ROM swap; this
    /// counter only exists to narrow the SIGKILL-data-loss window.
    frames_since_autosave: u32,
    /// Last deadline we handed to `ControlFlow::WaitUntil`. Used to
    /// suppress redundant `set_control_flow` calls — winit's
    /// calloop backend (Wayland) treats every call as a potential
    /// timer-source re-registration, and a timer that fires during
    /// the hand-over between "removed" and "added" logs
    /// `Received an event for non-existence source` in calloop.
    /// `None` means we haven't set `WaitUntil` yet this session
    /// (startup default is `Poll`).
    last_wait_deadline: Option<Instant>,
    /// Controller-1 bits sourced from the keyboard. Updated from
    /// winit key events; merged with `gamepad_bits_p1` once per
    /// frame and written to the NES controller shifter.
    keyboard_bits_p1: u8,
    /// Controller-1 bits sourced from the first connected gamepad
    /// (see `poll_gamepad`). Re-sampled once per frame.
    gamepad_bits_p1: u8,
    /// gilrs runtime. `None` if initialization failed (headless
    /// systems, missing permissions on evdev, etc.) — the
    /// keyboard path keeps working.
    gamepad: Option<gilrs::Gilrs>,
    /// Most-recently-active gamepad id. Set whenever an input event
    /// (button or axis) is drained from gilrs. This is how we avoid
    /// polling a phantom HID device (e.g. a Keychron keyboard dock
    /// that Linux classifies as a gamepad) that enumerates before
    /// the real controller — the first device that actually reports
    /// input wins.
    active_pad: Option<gilrs::GamepadId>,
    /// Edge-triggered overlay toggle request from the gamepad's
    /// Mode button (Xbox "Guide" / PlayStation "PS" / 8BitDo
    /// "Home"). Set inside `poll_gamepad` on press, consumed by
    /// `advance_and_present` right before the overlay-open state is
    /// re-sampled. Steam may still grab the same button at the OS
    /// level — this just gives the emulator its own binding.
    pending_menu_toggle: bool,
    /// Edge-triggered menu navigation events from the gamepad
    /// (D-pad + South/East face buttons). Drained after
    /// `poll_gamepad` and fed into the overlay via
    /// [`UiLayer::queue_nav`]. Queuing regardless of overlay state
    /// is safe: the overlay's `consume_key` only fires when
    /// showing, and unused egui input is silently dropped.
    pending_nav: Vec<NavKey>,
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
        let (mut nes, pending_audio_sink) = match (nes, audio_sink) {
            (Some(mut nes), Some(mut sink)) => {
                sink.set_cpu_clock(nes.region().cpu_clock_hz());
                nes.attach_audio(sink);
                (Some(nes), None)
            }
            (nes, sink) => (nes, sink),
        };
        let config = Config::default();
        // If the CLI-loaded Nes has save metadata attached (battery
        // cart), restore any existing `.sav` before the event loop
        // runs a single instruction. Mirrors the load-after-attach
        // sequence inside `load_rom` so both entry paths produce the
        // same state. No-op for non-battery carts.
        if let Some(nes) = nes.as_mut() {
            match nes.load_battery(&config.save) {
                Ok(true) => {
                    if let Some(p) = nes.save_path(&config.save) {
                        eprintln!("vibenes: loaded battery save from {}", p.display());
                    }
                }
                Ok(false) => {}
                Err(e) => eprintln!("vibenes: load save failed: {e:#}"),
            }
            match nes.load_disk(&config.save) {
                Ok(true) => {
                    if let Some(p) = nes.disk_save_path(&config.save) {
                        eprintln!("vibenes: loaded FDS disk save from {}", p.display());
                    }
                }
                Ok(false) => {}
                Err(e) => eprintln!("vibenes: load disk save failed: {e:#}"),
            }
            if nes.bus.mapper.save_data().is_some() {
                if let Some(p) = nes.save_path(&config.save) {
                    eprintln!("vibenes: battery save file → {}", p.display());
                }
            }
            if nes.bus.mapper.disk_save_data().is_some() {
                if let Some(p) = nes.disk_save_path(&config.save) {
                    eprintln!("vibenes: FDS disk save file → {}", p.display());
                }
            }
        }
        let gamepad = match gilrs::Gilrs::new() {
            Ok(g) => {
                let count = g.gamepads().count();
                if count > 0 {
                    eprintln!("gamepad: {count} connected (P1 uses the first)");
                }
                Some(g)
            }
            Err(e) => {
                eprintln!("vibenes: gamepad init failed ({e}) — keyboard only");
                None
            }
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
            config,
            frames_since_autosave: 0,
            last_wait_deadline: None,
            keyboard_bits_p1: 0,
            gamepad_bits_p1: 0,
            gamepad,
            active_pad: None,
            pending_menu_toggle: false,
            pending_nav: Vec::new(),
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
        // Drain gamepad events every frame, regardless of overlay
        // state. The Mode button (Xbox Guide / 8BitDo Home) toggles
        // the overlay via `pending_menu_toggle`, so it must work
        // from inside the menu as well as from gameplay. Held-state
        // bits are only committed to the NES when the overlay is
        // closed — same discipline as the keyboard path.
        self.poll_gamepad();
        if self.pending_menu_toggle {
            self.pending_menu_toggle = false;
            if let Some(ui) = self.ui.as_mut() {
                ui.toggle_overlay();
            }
        }
        if !self.pending_nav.is_empty() {
            let nav = std::mem::take(&mut self.pending_nav);
            if let Some(ui) = self.ui.as_mut() {
                for n in nav {
                    ui.queue_nav(n);
                }
            }
        }

        let overlay_open = self
            .ui
            .as_ref()
            .is_some_and(|ui| ui.is_overlay_open());

        if !overlay_open {
            self.commit_controller_p1();
        }

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
                // Periodic autosave. `save_battery` is a cheap
                // no-op when the mapper's dirty flag is clear, so
                // the typical game-at-idle case costs nothing. A
                // dirty 8-32 KiB write hits the disk in <1 ms on
                // SSD, well within the 300 ms audio ring, so we
                // stay on the frame-loop thread for now.
                // Periodic safety flush (3 min @ 60 fps by default).
                // The authoritative save triggers are quit + ROM
                // swap — this just narrows the SIGKILL-data-loss
                // window. Skip entirely when the interval is 0.
                if self.config.save.autosave_every_n_frames > 0 {
                    self.frames_since_autosave =
                        self.frames_since_autosave.saturating_add(1);
                    if self.frames_since_autosave >= self.config.save.autosave_every_n_frames {
                        self.frames_since_autosave = 0;
                        match nes.save_battery(&self.config.save) {
                            Ok(true) => {
                                if let Some(p) = nes.save_path(&self.config.save) {
                                    eprintln!(
                                        "vibenes: periodic battery save → {}",
                                        p.display()
                                    );
                                }
                            }
                            Ok(false) => {}
                            Err(e) => eprintln!("vibenes: autosave failed: {e:#}"),
                        }
                        match nes.save_disk(&self.config.save) {
                            Ok(true) => {
                                if let Some(p) = nes.disk_save_path(&self.config.save) {
                                    eprintln!(
                                        "vibenes: periodic FDS disk save → {}",
                                        p.display()
                                    );
                                }
                            }
                            Ok(false) => {}
                            Err(e) => eprintln!("vibenes: disk autosave failed: {e:#}"),
                        }
                    }
                }
            }
        }
        if let Some(nes) = self.nes.as_ref() {
            renderer.upload_framebuffer(&nes.bus.ppu.frame_buffer);
        }

        let mut cmds: Vec<UiCommand> = Vec::new();
        let surface_size = renderer.surface_size();
        // Snapshot the FDS drive state (if any) for the overlay. The
        // UI needs an owned copy rather than a borrow into `nes` so
        // the egui render closure stays flexible about borrows.
        let fds_info = self.nes.as_ref().and_then(|n| n.fds_info());
        if let (Some(ui), Some(window)) = (self.ui.as_mut(), self.window.as_ref()) {
            ui.run(
                window,
                surface_size,
                &self.recent_roms,
                &self.video,
                region,
                nes_loaded,
                fds_info,
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
        // (which would busy-loop on fast monitors).
        //
        // Two subtleties vs. the naive "flip between Poll and
        // WaitUntil" pattern:
        //
        // 1. **Always WaitUntil, never Poll.** `Poll` means the
        //    event loop busy-polls until we explicitly switch to
        //    `WaitUntil` again — which we used to do for one frame
        //    each time the deadline had already passed. That single-
        //    frame mode flip is what churns calloop's timer source
        //    on Wayland ("Received an event for non-existence
        //    source: TokenInner { id: 3, version: N }"). A past
        //    deadline passed to `WaitUntil` wakes immediately on
        //    every backend, so `Poll` buys us nothing.
        //
        // 2. **Debounce redundant `set_control_flow` calls.** Even
        //    sticking to `WaitUntil`, calling `set_control_flow`
        //    with an unchanged deadline re-registers the timer on
        //    calloop, which has the same race. `last_wait_deadline`
        //    lets us skip the call when the deadline hasn't moved.
        let Some(deadline) = self.next_frame_deadline else {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        };
        if Instant::now() >= deadline {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        if self.last_wait_deadline != Some(deadline) {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
            self.last_wait_deadline = Some(deadline);
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
            WindowEvent::CloseRequested => {
                self.shutdown("exit");
                event_loop.exit();
            }
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
                } else if code == KeyCode::F4 && state == ElementState::Pressed {
                    // Jump straight into the Disk submenu. Only useful
                    // on FDS carts — but harmless otherwise: the
                    // submenu will show "(not an FDS cart)" and Esc
                    // backs out.
                    if let Some(ui) = self.ui.as_mut() {
                        ui.open_disk_menu();
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
                        // Escape is a shutdown path too — flush
                        // battery RAM before exit like the X button
                        // and F1→Quit already do. Without this,
                        // Esc-to-quit silently loses progress even
                        // after the CLI-metadata-attach fix.
                        self.shutdown("exit");
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
            UiCommand::Quit => {
                self.shutdown("quit");
                event_loop.exit();
            }
            UiCommand::SetScale(n) => {
                self.video = self.video.with_scale(n);
                self.pending_window_resize = true;
            }
            UiCommand::SetAspectRatio(par_mode) => {
                self.video = self.video.with_par_mode(par_mode);
                self.pending_window_resize = true;
            }
            UiCommand::Reset => self.reset_nes(),
            UiCommand::FdsEject => {
                if let Some(nes) = self.nes.as_mut() {
                    nes.fds_eject();
                }
            }
            UiCommand::FdsInsert(side) => {
                if let Some(nes) = self.nes.as_mut() {
                    nes.fds_insert(side);
                }
            }
        }
    }

    fn reset_nes(&mut self) {
        if let Some(nes) = self.nes.as_mut() {
            nes.reset();
            self.halted_notice_shown = false;
            eprintln!("vibenes: reset (PC=${:04X})", nes.cpu.pc);
        }
    }

    /// Flush dirty battery RAM to disk before an exit event. `kind`
    /// gets dropped into the log line so we can tell X / F1 / Esc
    /// apart. Silent no-op when no cart is loaded or nothing is
    /// dirty; logs the save path on success; logs and swallows on
    /// failure so the event-loop tear-down always proceeds.
    fn flush_battery_save(&mut self, kind: &str) {
        let Some(nes) = self.nes.as_mut() else { return };
        match nes.save_battery(&self.config.save) {
            Ok(true) => {
                if let Some(p) = nes.save_path(&self.config.save) {
                    eprintln!("vibenes: saved {} on {kind}", p.display());
                }
            }
            Ok(false) => {
                if nes.bus.mapper.save_data().is_some() {
                    eprintln!("vibenes: {kind} — no battery RAM changes to save");
                }
            }
            Err(e) => eprintln!("vibenes: shutdown save failed: {e:#}"),
        }
        match nes.save_disk(&self.config.save) {
            Ok(true) => {
                if let Some(p) = nes.disk_save_path(&self.config.save) {
                    eprintln!("vibenes: saved FDS {} on {kind}", p.display());
                }
            }
            Ok(false) => {
                if nes.bus.mapper.disk_save_data().is_some() {
                    eprintln!("vibenes: {kind} — no FDS disk changes to save");
                }
            }
            Err(e) => eprintln!("vibenes: shutdown disk save failed: {e:#}"),
        }
    }

    /// Clean shutdown: flush the save, then drop heavy resources in a
    /// dependency-safe order **before** the event loop tears down.
    ///
    /// Winit/wgpu/egui on Wayland has a recurring class of segfaults
    /// during the implicit drop cascade at process exit — typically
    /// the cpal PipeWire stream's callback fires once more between
    /// the sink's producer dropping and the consumer stopping, or an
    /// egui-wgpu buffer releases into a device that's already gone.
    /// Taking each `Option` field to `None` here forces destructors
    /// to run at a known point in a known order, so any panic /
    /// segfault surfaces with an obvious stack trace instead of a
    /// post-exit crash the user has no handle on.
    ///
    /// Drop order (each `.take()` triggers the field's destructor):
    ///  1. UI (egui-wgpu renderer — uses wgpu device/queue)
    ///  2. Renderer (wgpu surface tied to the window)
    ///  3. Nes (audio sink = ringbuf producer)
    ///  4. Audio stream (cpal consumer; ring has no producer by now)
    ///  5. Window (last remaining Arc clone released)
    fn shutdown(&mut self, kind: &str) {
        self.flush_battery_save(kind);
        self.ui = None;
        self.renderer = None;
        self.nes = None;
        self.pending_audio_sink = None;
        self._audio_stream = None;
        self.window = None;
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
        let incoming_crc = cart.prg_chr_crc32;
        let region = match self.nes.as_mut() {
            Some(nes) => {
                // Flush the outgoing cart's battery RAM and FDS disk
                // save before we drop its mapper. After this we can't
                // recover the bytes — swap_cartridge consumes the
                // mapper.
                if let Err(e) = nes.save_battery(&self.config.save) {
                    eprintln!("vibenes: save before swap failed: {e:#}");
                }
                if let Err(e) = nes.save_disk(&self.config.save) {
                    eprintln!("vibenes: disk save before swap failed: {e:#}");
                }
                if let Err(e) = nes.swap_cartridge(cart) {
                    eprintln!("vibenes: swap failed: {e:#}");
                    return;
                }
                nes.attach_save_metadata(path.to_path_buf(), incoming_crc);
                match nes.load_battery(&self.config.save) {
                    Ok(true) => {
                        if let Some(p) = nes.save_path(&self.config.save) {
                            eprintln!("vibenes: loaded battery save from {}", p.display());
                        }
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!("vibenes: load save failed: {e:#}"),
                }
                match nes.load_disk(&self.config.save) {
                    Ok(true) => {
                        if let Some(p) = nes.disk_save_path(&self.config.save) {
                            eprintln!("vibenes: loaded FDS disk save from {}", p.display());
                        }
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!("vibenes: load disk save failed: {e:#}"),
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
                nes.attach_save_metadata(path.to_path_buf(), incoming_crc);
                match nes.load_battery(&self.config.save) {
                    Ok(true) => {
                        if let Some(p) = nes.save_path(&self.config.save) {
                            eprintln!("vibenes: loaded battery save from {}", p.display());
                        }
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!("vibenes: load save failed: {e:#}"),
                }
                match nes.load_disk(&self.config.save) {
                    Ok(true) => {
                        if let Some(p) = nes.disk_save_path(&self.config.save) {
                            eprintln!("vibenes: loaded FDS disk save from {}", p.display());
                        }
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!("vibenes: load disk save failed: {e:#}"),
                }
                let region = nes.region();
                self.nes = Some(nes);
                region
            }
        };
        self.frames_since_autosave = 0;
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
        // Surface the save path eagerly for battery carts so users
        // know where to look. No-op for non-battery carts.
        if let Some(nes) = self.nes.as_ref() {
            if nes.bus.mapper.save_data().is_some() {
                if let Some(p) = nes.save_path(&self.config.save) {
                    eprintln!("vibenes: battery save file → {}", p.display());
                }
            }
        }
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
        match state {
            ElementState::Pressed => self.keyboard_bits_p1 |= bit,
            ElementState::Released => self.keyboard_bits_p1 &= !bit,
        }
        self.commit_controller_p1();
    }

    /// Poll the first connected gamepad and project its state onto
    /// NES controller-1 bits. Fixed mapping, Xbox-first:
    ///
    /// | gilrs logical Button       | Xbox label | NES        |
    /// | -------------------------- | ---------- | ---------- |
    /// | South                      | A          | A (0x01)   |
    /// | North                      | X          | B (0x02)   |
    /// | Select / Back              | Back/View  | Select     |
    /// | Start / Menu               | Menu       | Start      |
    /// | D-pad                      | D-pad      | D-pad      |
    /// | Left stick (± `DEADBAND`)  | LS         | D-pad      |
    ///
    /// Note on North vs West: the user's 8BitDo Ultimate 2 emits
    /// `Button::North` for the physical X (left) face button under
    /// gilrs' default mapping — opposite to the usual evdev
    /// convention where `West = X`. Binding NES B to `North`
    /// matches this controller; a future remapping UI will handle
    /// pads that follow the other convention.
    ///
    /// Called every frame before `step_until_frame`. Draining
    /// `next_event()` is required for connect/disconnect state to
    /// propagate inside gilrs.
    fn poll_gamepad(&mut self) {
        use gilrs::{Axis, Button, EventType};
        const DEADBAND: f32 = 0.5;
        let Some(g) = self.gamepad.as_mut() else { return };
        // Drain events. Track the most recent id that emitted an
        // actual input event — this is the reliable signal for
        // "which of the enumerated HID devices is really the
        // player's gamepad". Both naive "pick first" and "has South
        // button mapped" fail here: Linux enumerates Keychron's
        // keyboard dock as a gamepad ahead of the real 8BitDo, and
        // gilrs' default mapping auto-binds the fallback device's
        // HID axes too, so static inspection can't tell them apart.
        // Whichever one the user physically moves is the real one.
        let debug = std::env::var_os("VIBENES_GAMEPAD_DEBUG").is_some();
        while let Some(ev) = g.next_event() {
            match ev.event {
                EventType::ButtonPressed(Button::Mode, _) => {
                    self.active_pad = Some(ev.id);
                    self.pending_menu_toggle = true;
                    if debug {
                        eprintln!("gamepad[{:?}] Mode pressed -> menu toggle", ev.id);
                    }
                }
                EventType::ButtonPressed(btn, _) => {
                    self.active_pad = Some(ev.id);
                    // Edge-triggered menu nav. South/East are the
                    // Xbox A/B face buttons: South = confirm, East
                    // = back (matches the dominant convention
                    // across modern emulators, Steam Big Picture,
                    // and the Nintendo-style physical layout once
                    // rotated). D-pad up/down drives the cursor.
                    match btn {
                        Button::DPadUp => self.pending_nav.push(NavKey::Up),
                        Button::DPadDown => self.pending_nav.push(NavKey::Down),
                        Button::South => self.pending_nav.push(NavKey::Select),
                        Button::East => self.pending_nav.push(NavKey::Back),
                        _ => {}
                    }
                    if debug {
                        eprintln!("gamepad[{:?}] pressed {:?}", ev.id, btn);
                    }
                }
                EventType::ButtonReleased(..)
                | EventType::AxisChanged(..)
                | EventType::ButtonChanged(..) => {
                    self.active_pad = Some(ev.id);
                    if debug {
                        eprintln!("gamepad[{:?}] {:?}", ev.id, ev.event);
                    }
                }
                EventType::Connected => {
                    if debug {
                        eprintln!("gamepad[{:?}] connected", ev.id);
                    }
                }
                EventType::Disconnected => {
                    if self.active_pad == Some(ev.id) {
                        self.active_pad = None;
                    }
                    if debug {
                        eprintln!("gamepad[{:?}] disconnected", ev.id);
                    }
                }
                _ => {}
            }
        }
        // Prefer the most-recently-active pad; if nothing has moved
        // yet this session, fall back to a heuristic (non-keyboard
        // name or a mapped face button) so the first-frame press
        // still works.
        let mut bits = 0u8;
        let pad = self
            .active_pad
            .and_then(|id| g.connected_gamepad(id).map(|p| (id, p)))
            .or_else(|| {
                g.gamepads().find(|(_, p)| {
                    let n = p.name().to_ascii_lowercase();
                    !n.contains("keyboard") && !n.contains("keychron")
                })
            })
            .or_else(|| {
                g.gamepads().find(|(_, p)| {
                    p.button_code(Button::South).is_some()
                        || p.axis_code(Axis::LeftStickX).is_some()
                })
            });
        if let Some((id, pad)) = pad {
            if debug {
                eprintln!(
                    "gamepad: polling id={:?} name={:?}", id, pad.name(),
                );
            }
            if pad.is_pressed(Button::South)     { bits |= 0x01; }
            if pad.is_pressed(Button::North)     { bits |= 0x02; }
            if pad.is_pressed(Button::Select)    { bits |= 0x04; }
            if pad.is_pressed(Button::Start)     { bits |= 0x08; }
            if pad.is_pressed(Button::DPadUp)    { bits |= 0x10; }
            if pad.is_pressed(Button::DPadDown)  { bits |= 0x20; }
            if pad.is_pressed(Button::DPadLeft)  { bits |= 0x40; }
            if pad.is_pressed(Button::DPadRight) { bits |= 0x80; }
            let x = pad.value(Axis::LeftStickX);
            let y = pad.value(Axis::LeftStickY);
            if y >  DEADBAND { bits |= 0x10; }
            if y < -DEADBAND { bits |= 0x20; }
            if x < -DEADBAND { bits |= 0x40; }
            if x >  DEADBAND { bits |= 0x80; }
        }
        self.gamepad_bits_p1 = bits;
    }

    /// Write the OR of keyboard + gamepad bits into the NES
    /// controller-1 latch. Safe to call while `self.nes` is `None`
    /// (the CLI "no ROM yet" startup case); the merge simply
    /// no-ops.
    fn commit_controller_p1(&mut self) {
        let Some(nes) = self.nes.as_mut() else { return };
        nes.bus.controllers[0].buttons = self.keyboard_bits_p1 | self.gamepad_bits_p1;
    }
}
