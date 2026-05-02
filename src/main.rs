// SPDX-License-Identifier: GPL-3.0-or-later
//! `vibenes` - the windowed emulator binary. Phase 6A.4 hooks the
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
use vibenes::nes::clock::Region;
use vibenes::config::Config;
use vibenes::debug_overlay;
use vibenes::gfx::{PresentOutcome, Renderer};
use vibenes::input::{HotplugNotice, InputConfig, InputRuntime};
use vibenes::nes::Nes;
use vibenes::nes::rom::Cartridge;
use vibenes::settings;
use vibenes::ui::{DebugStatus, NavKey, RecentRoms, UiCommand, UiLayer};
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
/// Fifo caps to the *monitor* refresh rate - anything above 60 Hz
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
    // first ROM is chosen from the File menu - no perceptible delay.
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

    // CLI-supplied path: identify the console first so we route the
    // load to the right core. SNES carts are loaded directly here so
    // the windowed binary boots straight into the game; NES carts
    // continue down the existing branch.
    let mut initial_snes: Option<vibenes::snes::Snes> = None;
    if let Some(path) = rom_path.as_deref() {
        match vibenes::core::system::detect_system(path) {
            Ok(vibenes::core::system::System::Snes) => {
                match vibenes::snes::rom::Cartridge::load(path) {
                    Ok(cart) => {
                        eprintln!("vibenes: loaded {}", cart.describe());
                        initial_snes = Some(vibenes::snes::Snes::from_cartridge(cart));
                    }
                    Err(e) => {
                        eprintln!("vibenes: failed to parse SNES ROM {}: {e:#}", path.display())
                    }
                }
            }
            Ok(vibenes::core::system::System::Nes) => {} // fall through
            Err(e) => eprintln!("vibenes: {e:#}"),
        }
    }

    let initial_nes = match rom_path.as_deref() {
        Some(path) if matches!(
            vibenes::core::system::detect_system(path),
            Ok(vibenes::core::system::System::Nes)
        ) => match Cartridge::load_with_fds_bios(path, cli.fds_bios.as_deref())
            .with_context(|| format!("loading ROM {}", path.display()))
        {
            Ok(cart) => {
                eprintln!("loaded: {}", cart.describe());
                // CRITICAL: the CLI-load path MUST attach save
                // metadata before handing the Nes to App. Without
                // this, `save_battery` early-returns on every
                // trigger because `save_meta` is None - which is
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
            eprintln!("vibenes: no ROM specified - use File → Open ROM…");
            None
        }
        // Non-NES path (SNES or unknown): handled in the
        // detect_system branch above; here we just leave the
        // window opening with no NES loaded.
        Some(_) => None,
    };

    let event_loop = EventLoop::new().context("create winit event loop")?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut handler = App::new(initial_nes, audio_sink, audio_stream, rom_path);
    if let Some(snes) = initial_snes {
        handler.attach_initial_snes(snes);
    }
    event_loop.run_app(&mut handler).context("winit event loop")?;
    Ok(())
}

fn frame_period_for(region: Region) -> Duration {
    match region {
        Region::Ntsc => NTSC_FRAME_PERIOD,
        Region::Pal => PAL_FRAME_PERIOD,
    }
}

/// Translate a `core::Region` (NTSC/PAL only) into the NES
/// timing-aware [`Region`] this crate uses for frame pacing. SNES
/// region detection lives at the cross-core layer; the NES helpers
/// then dispatch on the same NTSC/PAL distinction.
fn map_region(r: vibenes::core::Region) -> Region {
    match r {
        vibenes::core::Region::Ntsc => Region::Ntsc,
        vibenes::core::Region::Pal => Region::Pal,
    }
}

/// Map a `KeyCode::Digit0..Digit9` to a save-state slot index in
/// `0..=9`, returning `None` for non-digit keys. The numpad codes
/// (`Numpad0`/etc.) are deliberately not accepted here - they can
/// be wired separately if a user requests it; mixing both today
/// would just create more surface area for the no-bare-digit-on-
/// controller invariant to break.
fn digit_keycode_to_slot(code: KeyCode) -> Option<u8> {
    match code {
        KeyCode::Digit0 => Some(0),
        KeyCode::Digit1 => Some(1),
        KeyCode::Digit2 => Some(2),
        KeyCode::Digit3 => Some(3),
        KeyCode::Digit4 => Some(4),
        KeyCode::Digit5 => Some(5),
        KeyCode::Digit6 => Some(6),
        KeyCode::Digit7 => Some(7),
        KeyCode::Digit8 => Some(8),
        KeyCode::Digit9 => Some(9),
        _ => None,
    }
}

/// Parsed command-line arguments. Add fields here when new flags
/// land - keeping all CLI state in one struct keeps `run()` free of
/// arg-parsing tangles.
struct CliArgs {
    rom_path: Option<PathBuf>,
    /// `--fds-bios <path>`. Overrides the XDG / ROM-dir BIOS search
    /// for this session only. Also accepted via the
    /// `VIBENES_FDS_BIOS` environment variable (see
    /// [`vibenes::nes::fds::bios`]).
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
    /// SNES core slot, populated when a `.smc`/`.sfc`/`.fig`/`.swc` cart
    /// loads. NES and SNES are mutually exclusive: loading a SNES cart
    /// drops any current `Nes` and vice versa. The render/step path
    /// branches on which slot is filled.
    snes: Option<vibenes::snes::Snes>,
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
    /// device - hence why it lives on the App owner rather than a
    /// local inside `run()`.
    _audio_stream: Option<audio::AudioStream>,
    /// Most-recently-loaded ROMs, shown in the File menu. Seeded with
    /// the path passed on the command line (if any).
    recent_roms: RecentRoms,
    /// Integer scale + pixel aspect ratio. Window inner size equals
    /// `video.content_size(region)` exactly - no chrome to subtract,
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
    /// not it wrote - either way we've consulted the mapper). The
    /// authoritative save triggers are quit + ROM swap; this
    /// counter only exists to narrow the SIGKILL-data-loss window.
    frames_since_autosave: u32,
    /// Last deadline we handed to `ControlFlow::WaitUntil`. Used to
    /// suppress redundant `set_control_flow` calls - winit's
    /// calloop backend (Wayland) treats every call as a potential
    /// timer-source re-registration, and a timer that fires during
    /// the hand-over between "removed" and "added" logs
    /// `Received an event for non-existence source` in calloop.
    /// `None` means we haven't set `WaitUntil` yet this session
    /// (startup default is `Poll`).
    last_wait_deadline: Option<Instant>,
    /// Unified input runtime: holds the gilrs handle, the
    /// keyboard-state map, the loaded `input.toml` config, and the
    /// hot-plug-resolved P1/P2 gamepad ids. Replaces the previous
    /// keyboard_bits / gamepad_bits / active_pad fields. The
    /// settings UI reads + mutates the bindings here once it
    /// arrives in phase 2.
    input: InputRuntime,
    /// Edge-triggered overlay toggle request from the gamepad's
    /// Mode button (Xbox "Guide" / PlayStation "PS" / 8BitDo
    /// "Home"). Set inside `poll_gamepad` on press, consumed by
    /// `advance_and_present` right before the overlay-open state is
    /// re-sampled. Steam may still grab the same button at the OS
    /// level - this just gives the emulator its own binding.
    pending_menu_toggle: bool,
    /// Edge-triggered menu navigation events from the gamepad
    /// (D-pad + South/East face buttons). Drained after
    /// `poll_gamepad` and fed into the overlay via
    /// [`UiLayer::queue_nav`]. Queuing regardless of overlay state
    /// is safe: the overlay's `consume_key` only fires when
    /// showing, and unused egui input is silently dropped.
    pending_nav: Vec<NavKey>,
    /// When true, paint scanline + dot coordinate rulers directly
    /// into the NES framebuffer before upload. Toggled from the F12
    /// Debug submenu - used to read off the exact line a rendering
    /// artifact lives on without counting pixels.
    show_scanline_ruler: bool,
    /// Scratch copy of the PPU framebuffer used to paint the debug
    /// overlay without disturbing the PPU's own buffer. Empty
    /// until the ruler is first toggled on; allocated once and
    /// reused.
    debug_fb_scratch: Vec<u8>,
    /// Frames remaining in the OAM-dump burst armed from the Debug
    /// submenu. The post-frame hook prints OAM and decrements;
    /// covers the 30 Hz sprite-flicker cycle so a single probe
    /// doesn't miss the "on" half.
    oam_dump_frames: u8,
    /// Simple FPS counter: number of presented frames since
    /// `fps_window_start`. Reported to stderr once per wall-clock
    /// second when `fps_print_enabled` is on (set via the
    /// `VIBENES_FPS` env var). Used to diagnose frame-rate drift -
    /// e.g. PAL ROMs running at the host monitor's refresh rather
    /// than the 50 Hz target.
    fps_window_start: Option<Instant>,
    fps_frames: u32,
    /// CPU cycle count snapshot at `fps_window_start`. Diff'd with the
    /// current value once per second to compute emulated cycles/sec -
    /// should converge on the region's CPU clock (~1.79 MHz NTSC,
    /// ~1.66 MHz PAL).
    fps_cpu_cycles_at_window_start: u64,
    fps_ppu_frames_at_window_start: u64,
    fps_print_enabled: bool,
    /// Current save-state slot (0..=9). F2 saves to this slot, F3
    /// loads from it. Bare digit keys 0..9 retarget the slot. Loaded
    /// from `settings.kv` on startup and persisted on every change
    /// so the choice survives a session.
    save_state_slot: u8,
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
        // off, and the ring drains faster than it fills). The SNES
        // path doesn't care about CPU clock - it produces samples at
        // a fixed 32 kHz that the sink resamples to host rate.
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
            match nes.load_flash(&config.save) {
                Ok(true) => {
                    if let Some(p) = nes.flash_save_path(&config.save) {
                        eprintln!("vibenes: loaded PRG-flash save from {}", p.display());
                    }
                }
                Ok(false) => {}
                Err(e) => eprintln!("vibenes: load flash save failed: {e:#}"),
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
            if nes.bus.mapper.flash_save_data().is_some() {
                if let Some(p) = nes.flash_save_path(&config.save) {
                    eprintln!("vibenes: PRG-flash save file → {}", p.display());
                }
            }
        }
        // Load (or eagerly write) the user's input bindings before
        // building the runtime, so the runtime sees any sticky
        // gamepad UUIDs in its boot-time auto-assign sweep.
        let input_path = InputConfig::default_path();
        let input_cfg = match input_path.as_ref() {
            Some(p) => InputConfig::load_or_init(p),
            None => InputConfig::default(),
        };
        let input = InputRuntime::new(input_cfg, input_path.clone());
        if let Some(p) = input_path.as_ref() {
            eprintln!("vibenes: input bindings → {}", p.display());
        }
        if let Some(g) = input.gilrs() {
            let count = g.gamepads().count();
            if count > 0 {
                eprintln!("gamepad: {count} connected");
            }
        } else {
            eprintln!("vibenes: gamepad runtime unavailable - keyboard only");
        }
        Self {
            nes,
            snes: None,
            window: None,
            renderer: None,
            ui: None,
            halted_notice_shown: false,
            frame_period,
            next_frame_deadline: None,
            pending_audio_sink,
            _audio_stream: audio_stream,
            recent_roms,
            // Persisted preferences: load before the window is created
            // so the initial inner_size already matches the user's
            // chosen scale rather than briefly opening at the default
            // and resizing on the first frame.
            // Persisted preferences read in one pass; a single
            // `settings.kv` read populates both the video scale and
            // the active save-state slot.
            video: {
                let s = settings::load();
                VideoSettings::default().with_scale(s.scale)
            },
            save_state_slot: settings::load().save_state_slot,
            pending_window_resize: false,
            config,
            frames_since_autosave: 0,
            last_wait_deadline: None,
            input,
            pending_menu_toggle: false,
            pending_nav: Vec::new(),
            show_scanline_ruler: false,
            debug_fb_scratch: Vec::new(),
            oam_dump_frames: 0,
            fps_window_start: None,
            fps_frames: 0,
            fps_cpu_cycles_at_window_start: 0,
            fps_ppu_frames_at_window_start: 0,
            fps_print_enabled: std::env::var("VIBENES_FPS").is_ok(),
        }
    }

    fn region_opt(&self) -> Option<Region> {
        self.nes.as_ref().map(Nes::region)
    }

    /// Physical inner size of the window - exactly the NES content
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
        // closed - same discipline as the keyboard path.
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
            self.commit_controllers();
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
            if let Some(snes) = self.snes.as_mut() {
                if let Err(e) = vibenes::core::Core::step_until_frame(snes) {
                    if !self.halted_notice_shown {
                        eprintln!("vibenes: SNES error: {e}");
                        self.halted_notice_shown = true;
                    }
                }
                // Drain the APU's accumulated 32 kHz stereo samples
                // into the host audio sink so the cpal callback can
                // present them before the next wakeup.
                vibenes::core::Core::end_audio_frame(snes);
            }
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
                // OAM dump burst, armed via the Debug submenu. Each
                // frame in the burst dumps the full visible OAM to
                // stderr - covers 30 Hz sprite flicker so a single
                // probe doesn't miss the "on" cycle.
                if self.oam_dump_frames > 0 {
                    self.oam_dump_frames -= 1;
                    let oam = nes.bus.ppu.debug_oam();
                    eprintln!(
                        "vibenes: OAM frame {} (slot: Y tile attr X)",
                        nes.bus.ppu.frame()
                    );
                    for slot in 0..64 {
                        let base = slot * 4;
                        let y = oam[base];
                        let t = oam[base + 1];
                        let a = oam[base + 2];
                        let x = oam[base + 3];
                        if y < 240 {
                            eprintln!(
                                "  {slot:02}: Y={y:3} tile=${t:02X} attr=${a:02X} X={x:3}"
                            );
                        }
                    }
                }
                // Periodic autosave. `save_battery` is a cheap
                // no-op when the mapper's dirty flag is clear, so
                // the typical game-at-idle case costs nothing. A
                // dirty 8-32 KiB write hits the disk in <1 ms on
                // SSD, well within the 300 ms audio ring, so we
                // stay on the frame-loop thread for now.
                // Periodic safety flush (3 min @ 60 fps by default).
                // The authoritative save triggers are quit + ROM
                // swap - this just narrows the SIGKILL-data-loss
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
                        match nes.save_flash(&self.config.save) {
                            Ok(true) => {
                                if let Some(p) = nes.flash_save_path(&self.config.save) {
                                    eprintln!(
                                        "vibenes: periodic PRG-flash save → {}",
                                        p.display()
                                    );
                                }
                            }
                            Ok(false) => {}
                            Err(e) => eprintln!("vibenes: flash autosave failed: {e:#}"),
                        }
                    }
                }
            }
        }
        if let Some(nes) = self.nes.as_ref() {
            let fb = &nes.bus.ppu.frame_buffer;
            if self.show_scanline_ruler {
                // Copy into a scratch buffer so we don't taint the
                // PPU's own framebuffer (it'd persist for one frame
                // when rendering pauses, e.g. with the menu open).
                if self.debug_fb_scratch.len() != fb.len() {
                    self.debug_fb_scratch.resize(fb.len(), 0);
                }
                self.debug_fb_scratch.copy_from_slice(fb);
                debug_overlay::draw_scanline_ruler(&mut self.debug_fb_scratch);
                renderer.upload_framebuffer(&self.debug_fb_scratch);
            } else {
                renderer.upload_framebuffer(fb);
            }
        } else if let Some(snes) = self.snes.as_ref() {
            // SNES output is 256x224; the renderer's texture is sized
            // for the NES at 256x240. Pad the SNES content with 8 black
            // rows on top + 8 on bottom so the existing pipeline
            // displays it at the right vertical aspect. A
            // configurable-dim upload lands when the windowed app
            // supports more than two cores.
            let snes_fb = snes.framebuffer_for_host();
            let nes_bytes = 256 * 240 * 4;
            if self.debug_fb_scratch.len() != nes_bytes {
                self.debug_fb_scratch.resize(nes_bytes, 0);
            }
            // Clear so the borders are black even after a previous
            // NES frame.
            for px in self.debug_fb_scratch.chunks_exact_mut(4) {
                px.copy_from_slice(&[0, 0, 0, 0xFF]);
            }
            let dst_start = 8 * 256 * 4;
            self.debug_fb_scratch[dst_start..dst_start + snes_fb.len()]
                .copy_from_slice(snes_fb);
            renderer.upload_framebuffer(&self.debug_fb_scratch);
        }

        let mut cmds: Vec<UiCommand> = Vec::new();
        let surface_size = renderer.surface_size();
        // Snapshot the FDS drive state (if any) for the overlay. The
        // UI needs an owned copy rather than a borrow into `nes` so
        // the egui render closure stays flexible about borrows.
        let fds_info = self.nes.as_ref().and_then(|n| n.fds_info());
        let debug_status = DebugStatus {
            scanline_ruler_on: self.show_scanline_ruler,
        };
        if let (Some(ui), Some(window)) = (self.ui.as_mut(), self.window.as_ref()) {
            ui.run(
                window,
                surface_size,
                &self.recent_roms,
                &self.video,
                region,
                nes_loaded,
                fds_info,
                debug_status,
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
        // Advance the turbo oscillator. Tied to presented frames
        // (not emulated frames) so the visible cadence matches the
        // user's monitor refresh.
        self.input.end_frame();
        if self.fps_print_enabled {
            self.fps_frames = self.fps_frames.saturating_add(1);
            if self.fps_window_start.is_none() {
                self.fps_window_start = Some(now);
                if let Some(nes) = self.nes.as_ref() {
                    self.fps_cpu_cycles_at_window_start = nes.bus.clock.cpu_cycles();
                    self.fps_ppu_frames_at_window_start = nes.bus.ppu.frame();
                }
            }
            let start = self.fps_window_start.unwrap();
            if now.duration_since(start) >= Duration::from_secs(1) {
                let elapsed = now.duration_since(start).as_secs_f64();
                let target_hz = 1.0 / self.frame_period.as_secs_f64();
                let (cpu_hz, ppu_fps) = self
                    .nes
                    .as_ref()
                    .map(|nes| {
                        let cyc = nes.bus.clock.cpu_cycles()
                            - self.fps_cpu_cycles_at_window_start;
                        let frames = nes.bus.ppu.frame()
                            - self.fps_ppu_frames_at_window_start;
                        (cyc as f64 / elapsed, frames as f64 / elapsed)
                    })
                    .unwrap_or((0.0, 0.0));
                eprintln!(
                    "vibenes: host_fps={:.2} target={:.2} ppu_fps={:.2} cpu_hz={:.0}",
                    self.fps_frames as f64 / elapsed,
                    target_hz,
                    ppu_fps,
                    cpu_hz,
                );
                self.fps_frames = 0;
                self.fps_window_start = Some(now);
                if let Some(nes) = self.nes.as_ref() {
                    self.fps_cpu_cycles_at_window_start = nes.bus.clock.cpu_cycles();
                    self.fps_ppu_frames_at_window_start = nes.bus.ppu.frame();
                }
            }
        }
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
        //    `WaitUntil` again - which we used to do for one frame
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
        // effective PAR. No menubar reserve - the in-game overlay
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
        // override - we always want it to exit the app.
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

                // F1 always toggles the overlay regardless of state -
                // the user expects a single "menu" key to work both
                // ways.
                if code == KeyCode::F1 && state == ElementState::Pressed {
                    if let Some(ui) = self.ui.as_mut() {
                        ui.toggle_overlay();
                    }
                } else if code == KeyCode::F12 && state == ElementState::Pressed {
                    // Open the Debug submenu directly (scanline
                    // ruler, OAM dump, etc.). One hotkey beats
                    // burning F2/F3/F5/F6 on individual diagnostics.
                    if let Some(ui) = self.ui.as_mut() {
                        ui.open_debug_menu();
                    }
                } else if code == KeyCode::F4 && state == ElementState::Pressed {
                    // Jump straight into the Disk submenu. Only useful
                    // on FDS carts - but harmless otherwise: the
                    // submenu will show "(not an FDS cart)" and Esc
                    // backs out.
                    if let Some(ui) = self.ui.as_mut() {
                        ui.open_disk_menu();
                    }
                } else if code == KeyCode::F2 && state == ElementState::Pressed {
                    // Save the current NES state to the active slot.
                    // The keyboard handler runs between RedrawRequested
                    // events, so by the time F2 is observed we're at a
                    // frame boundary - no mid-frame snapshot risk.
                    self.save_state_to_active_slot();
                } else if code == KeyCode::F3 && state == ElementState::Pressed {
                    // Load the active slot's state. Validation runs
                    // before any mutation; on failure the cart is
                    // left untouched.
                    self.load_state_from_active_slot();
                } else if code == KeyCode::Escape && state == ElementState::Pressed {
                    // Esc backs out of the overlay (or closes it from
                    // root); when the overlay is closed it quits the
                    // app, matching the prior behavior.
                    if overlay_open {
                        if let Some(ui) = self.ui.as_mut() {
                            ui.back_or_close_overlay();
                        }
                    } else {
                        // Escape is a shutdown path too - flush
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
                } else if state == ElementState::Pressed
                    && digit_keycode_to_slot(code).is_some()
                {
                    // Bare digit keys 0..=9 retarget the active save
                    // slot. Standard NES controllers have no digit
                    // buttons so this can't shadow gameplay input.
                    // Persisted to settings.kv so the choice is
                    // sticky across launches.
                    if let Some(slot) = digit_keycode_to_slot(code) {
                        self.select_save_state_slot(slot);
                    }
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
                // Do NOT immediately request another redraw here -
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
                // Persist the post-clamp value, not the raw `n` -
                // `with_scale` already snapped it into MIN..=MAX.
                let to_save = settings::Settings {
                    scale: self.video.scale,
                    save_state_slot: self.save_state_slot,
                };
                if let Err(e) = settings::save(&to_save) {
                    eprintln!("vibenes: save settings failed: {e:#}");
                }
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
            UiCommand::ToggleScanlineRuler => {
                self.show_scanline_ruler = !self.show_scanline_ruler;
                eprintln!(
                    "vibenes: scanline ruler {}",
                    if self.show_scanline_ruler { "on" } else { "off" }
                );
            }
            UiCommand::DumpOamBurst(frames) => {
                self.oam_dump_frames = frames;
                eprintln!("vibenes: OAM dump armed ({frames} frames)");
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

    /// Save the current NES state to the active slot. F2 binding.
    /// No-op (with stderr notice + on-screen error toast) when no
    /// NES is loaded or the active mapper isn't covered by
    /// [`vibenes::save_state`] yet.
    fn save_state_to_active_slot(&mut self) {
        let Some(nes) = self.nes.as_ref() else {
            eprintln!("vibenes: no ROM loaded - F2 ignored");
            self.show_error_toast("No ROM loaded");
            return;
        };
        let Some(slot) = vibenes::save_state::Slot::new(self.save_state_slot) else {
            // Defensive: settings parse already clamps to 0..=9.
            eprintln!(
                "vibenes: invalid save slot {} (must be 0..9)",
                self.save_state_slot
            );
            return;
        };
        match vibenes::save_state::save_to_slot(nes, &self.config.save, slot) {
            Ok(path) => {
                eprintln!(
                    "vibenes: saved state slot {} → {}",
                    self.save_state_slot,
                    path.display()
                );
                self.show_info_toast(format!("Saved slot {}", self.save_state_slot));
            }
            Err(e) => {
                eprintln!(
                    "vibenes: save state slot {} failed: {e}",
                    self.save_state_slot
                );
                self.show_error_toast(format!(
                    "Save slot {} failed: {e}",
                    self.save_state_slot
                ));
            }
        }
    }

    /// Load and apply the active slot's save state. F3 binding.
    /// Validation (header magic / version / ROM CRC / mapper id)
    /// runs before any state is touched - on `Err` the live `nes`
    /// is left exactly as it was before the keypress (also true on
    /// late apply failures: the in-memory backup-on-load rollback
    /// inside [`vibenes::save_state::load_and_apply_from_slot`]
    /// restores the pre-call state byte-for-byte).
    fn load_state_from_active_slot(&mut self) {
        let Some(nes) = self.nes.as_mut() else {
            eprintln!("vibenes: no ROM loaded - F3 ignored");
            self.show_error_toast("No ROM loaded");
            return;
        };
        let Some(slot) = vibenes::save_state::Slot::new(self.save_state_slot) else {
            eprintln!(
                "vibenes: invalid save slot {} (must be 0..9)",
                self.save_state_slot
            );
            return;
        };
        match vibenes::save_state::load_and_apply_from_slot(nes, &self.config.save, slot) {
            Ok(()) => {
                let pc = nes.cpu.pc;
                eprintln!(
                    "vibenes: loaded state slot {} (PC=${:04X})",
                    self.save_state_slot, pc,
                );
                // A loaded state may have been captured at a halted
                // instruction; reset the throttle so the next
                // halted-CPU detection re-arms.
                self.halted_notice_shown = false;
                self.show_info_toast(format!("Loaded slot {}", self.save_state_slot));
            }
            Err(e) => {
                eprintln!(
                    "vibenes: load state slot {} failed: {e}",
                    self.save_state_slot
                );
                // Discriminate "no save in this slot" vs other
                // errors so the toast is actionable.
                let toast_msg = match &e {
                    vibenes::save_state::SaveStateError::Io(io_err)
                        if io_err.kind() == std::io::ErrorKind::NotFound =>
                    {
                        format!("No state in slot {}", self.save_state_slot)
                    }
                    _ => format!("Load slot {} failed: {e}", self.save_state_slot),
                };
                self.show_error_toast(toast_msg);
            }
        }
    }

    /// Switch the active save-state slot and persist the choice to
    /// `settings.kv`. Out-of-range slots are dropped silently - the
    /// caller (digit-key handler) only ever passes 0..=9 anyway.
    fn select_save_state_slot(&mut self, slot: u8) {
        if slot >= vibenes::save_state::SLOT_COUNT {
            return;
        }
        if self.save_state_slot == slot {
            return;
        }
        self.save_state_slot = slot;
        eprintln!("vibenes: save slot → {slot}");
        self.show_info_toast(format!("Slot {slot}"));
        // Persist alongside scale. Failure is logged but doesn't
        // block - the choice still applies to this session.
        let cfg = settings::Settings {
            scale: self.video.scale,
            save_state_slot: slot,
        };
        if let Err(e) = settings::save(&cfg) {
            eprintln!("vibenes: settings save failed: {e:#}");
        }
    }

    /// Push an info toast through the egui overlay if it's been
    /// constructed. Silently no-op pre-window-open so calls during
    /// startup (e.g. CLI ROM load races) don't crash.
    fn show_info_toast(&mut self, msg: impl Into<String>) {
        if let Some(ui) = self.ui.as_mut() {
            ui.show_toast_info(msg);
        }
    }

    fn show_error_toast(&mut self, msg: impl Into<String>) {
        if let Some(ui) = self.ui.as_mut() {
            ui.show_toast_error(msg);
        }
    }

    /// Flush dirty battery RAM, FDS disk diff, and PRG-flash diff
    /// to disk before an exit event. `kind` gets dropped into the
    /// log line so we can tell X / F1 / Esc apart. Silent no-op
    /// when no cart is loaded or nothing is dirty; logs the save
    /// path on success; logs and swallows on failure so the
    /// event-loop tear-down always proceeds. The function keeps
    /// its `flush_battery_save` name for grep continuity even
    /// though it now drains all three persistence channels.
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
                    eprintln!("vibenes: {kind} - no battery RAM changes to save");
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
                    eprintln!("vibenes: {kind} - no FDS disk changes to save");
                }
            }
            Err(e) => eprintln!("vibenes: shutdown disk save failed: {e:#}"),
        }
        match nes.save_flash(&self.config.save) {
            Ok(true) => {
                if let Some(p) = nes.flash_save_path(&self.config.save) {
                    eprintln!("vibenes: saved PRG-flash {} on {kind}", p.display());
                }
            }
            Ok(false) => {
                if nes.bus.mapper.flash_save_data().is_some() {
                    eprintln!("vibenes: {kind} - no PRG-flash changes to save");
                }
            }
            Err(e) => eprintln!("vibenes: shutdown flash save failed: {e:#}"),
        }
    }

    /// Clean shutdown: flush the save, then drop heavy resources in a
    /// dependency-safe order **before** the event loop tears down.
    ///
    /// Winit/wgpu/egui on Wayland has a recurring class of segfaults
    /// during the implicit drop cascade at process exit - typically
    /// the cpal PipeWire stream's callback fires once more between
    /// the sink's producer dropping and the consumer stopping, or an
    /// egui-wgpu buffer releases into a device that's already gone.
    /// Taking each `Option` field to `None` here forces destructors
    /// to run at a known point in a known order, so any panic /
    /// segfault surfaces with an obvious stack trace instead of a
    /// post-exit crash the user has no handle on.
    ///
    /// Drop order (each `.take()` triggers the field's destructor):
    ///  1. UI (egui-wgpu renderer - uses wgpu device/queue)
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

    /// Hand a SNES core to a freshly-built `App`. Mirrors how `Nes`
    /// is plumbed through `App::new`, but kept off the constructor
    /// so existing call sites don't need to thread an `Option<Snes>`
    /// they never use.
    fn attach_initial_snes(&mut self, mut snes: vibenes::snes::Snes) {
        self.frame_period = frame_period_for(map_region(snes.region()));
        // Hand off the pending host audio sink (held since startup
        // when no NES was loaded) to the SNES so the first frame
        // already produces audible output.
        if let Some(sink) = self.pending_audio_sink.take() {
            vibenes::core::Core::attach_audio(&mut snes, sink);
        }
        self.snes = Some(snes);
        self.pending_window_resize = true;
    }

    fn load_rom(&mut self, path: &Path) {
        // Console identification first. SNES carts route through the
        // SNES core; NES carts continue down the existing flow.
        match vibenes::core::system::detect_system(path) {
            Ok(vibenes::core::system::System::Snes) => {
                match vibenes::snes::rom::Cartridge::load(path) {
                    Ok(cart) => {
                        eprintln!("vibenes: loaded {}", cart.describe());
                        // Reclaim the audio sink before dropping
                        // whichever core is active so the cpal stream
                        // keeps producing rather than going silent.
                        // Sources, in priority order: an active NES,
                        // an active SNES (rare - you'd be reloading),
                        // or the pending slot (no core attached yet).
                        let reclaimed_sink = self
                            .nes
                            .as_mut()
                            .and_then(Nes::detach_audio)
                            .or_else(|| {
                                self.snes
                                    .as_mut()
                                    .and_then(vibenes::snes::Snes::detach_audio)
                            })
                            .or_else(|| self.pending_audio_sink.take());
                        // Drop any active NES (flushing all three
                        // persistence channels first) so the SNES has
                        // the framebuffer pipeline to itself.
                        if let Some(mut nes) = self.nes.take() {
                            if let Err(e) = nes.save_battery(&self.config.save) {
                                eprintln!("vibenes: save before SNES swap failed: {e:#}");
                            }
                            if let Err(e) = nes.save_disk(&self.config.save) {
                                eprintln!(
                                    "vibenes: disk save before SNES swap failed: {e:#}"
                                );
                            }
                            if let Err(e) = nes.save_flash(&self.config.save) {
                                eprintln!(
                                    "vibenes: flash save before SNES swap failed: {e:#}"
                                );
                            }
                        }
                        let mut snes = vibenes::snes::Snes::from_cartridge(cart);
                        if let Some(sink) = reclaimed_sink {
                            vibenes::core::Core::attach_audio(&mut snes, sink);
                        }
                        self.frame_period =
                            frame_period_for(map_region(snes.region()));
                        self.snes = Some(snes);
                        self.recent_roms.push(path.to_path_buf());
                        self.halted_notice_shown = false;
                        self.pending_window_resize = true;
                    }
                    Err(e) => eprintln!(
                        "vibenes: failed to parse SNES ROM {}: {e:#}",
                        path.display()
                    ),
                }
                return;
            }
            Ok(vibenes::core::system::System::Nes) => {
                // Fall through to NES load. If a SNES was running,
                // reclaim its audio sink (so the cpal stream stays
                // alive) and drop the SNES so the NES owns the pipeline.
                if let Some(mut snes) = self.snes.take() {
                    if let Some(sink) = snes.detach_audio() {
                        // Park the sink in the pending slot. The
                        // first-load NES branch below will pick it up
                        // and retune it for the NES region clock.
                        self.pending_audio_sink = Some(sink);
                    }
                }
            }
            Err(e) => {
                eprintln!("vibenes: {e:#}");
                return;
            }
        }
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
                // Flush the outgoing cart's battery RAM, FDS disk
                // save, and PRG-flash save before we drop its mapper.
                // After this we can't recover the bytes -
                // swap_cartridge consumes the mapper.
                if let Err(e) = nes.save_battery(&self.config.save) {
                    eprintln!("vibenes: save before swap failed: {e:#}");
                }
                if let Err(e) = nes.save_disk(&self.config.save) {
                    eprintln!("vibenes: disk save before swap failed: {e:#}");
                }
                if let Err(e) = nes.save_flash(&self.config.save) {
                    eprintln!("vibenes: flash save before swap failed: {e:#}");
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
                match nes.load_flash(&self.config.save) {
                    Ok(true) => {
                        if let Some(p) = nes.flash_save_path(&self.config.save) {
                            eprintln!(
                                "vibenes: loaded PRG-flash save from {}",
                                p.display()
                            );
                        }
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!("vibenes: load flash save failed: {e:#}"),
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
                match nes.load_flash(&self.config.save) {
                    Ok(true) => {
                        if let Some(p) = nes.flash_save_path(&self.config.save) {
                            eprintln!(
                                "vibenes: loaded PRG-flash save from {}",
                                p.display()
                            );
                        }
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!("vibenes: load flash save failed: {e:#}"),
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

    /// Update the input runtime's keyboard state on every winit
    /// key edge. Bindings live in `input.toml` (see
    /// [`vibenes::input`]); this function no longer hardcodes a
    /// keyboard map. Key-repeat events are filtered at the call
    /// site, so each call here is a real edge.
    ///
    /// `R` triggers a warm reset (the console's Reset button) and
    /// is handled in `window_event` before this function runs.
    fn apply_controller_input(&mut self, code: KeyCode, state: ElementState) {
        self.input.note_key(code, state == ElementState::Pressed);
        self.commit_controllers();
    }

    /// Drain gilrs events, surface hot-plug toasts / nav presses to
    /// the host, and refresh both controllers from the input
    /// runtime. Called every frame before `step_until_frame`.
    /// Bindings come from `input.toml` (see [`vibenes::input`]);
    /// this function no longer hardcodes a button map.
    ///
    /// Two classes of gilrs event are handled outside the binding
    /// system because they're UI concerns rather than NES input:
    /// - **`Button::Mode`** (Xbox Guide / PS PS / 8BitDo Home) toggles
    ///   the overlay.
    /// - **DPad up/down + South/East face buttons** while the
    ///   overlay is open feed the menu navigator.
    fn poll_gamepad(&mut self) {
        use gilrs::{Button, EventType};
        let debug = std::env::var_os("VIBENES_GAMEPAD_DEBUG").is_some();
        // Pre-pass: scrape Mode + nav button presses straight from
        // gilrs's event queue. We do this by peeking at events
        // before InputRuntime drains the queue - except gilrs
        // delivers each event exactly once, so we have to choose
        // one consumer. Solution: drain here once, dispatch the
        // hot-plug events into the runtime via a notice list, and
        // handle Mode + nav inline.
        let mut hotplug_after: Vec<(gilrs::GamepadId, gilrs::EventType)> = Vec::new();
        if let Some(g) = self.input.gilrs_mut() {
            while let Some(ev) = g.next_event() {
                match ev.event {
                    EventType::ButtonPressed(Button::Mode, _) => {
                        self.pending_menu_toggle = true;
                        if debug {
                            eprintln!("gamepad[{:?}] Mode pressed -> menu toggle", ev.id);
                        }
                    }
                    EventType::ButtonPressed(btn, _) => {
                        // Menu nav while the overlay is open. We
                        // forward edges unconditionally; the UI
                        // layer ignores them when the overlay is
                        // closed.
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
                    EventType::Connected | EventType::Disconnected => {
                        // Defer hot-plug bookkeeping to the
                        // InputRuntime; it owns slot routing.
                        hotplug_after.push((ev.id, ev.event));
                    }
                    _ => {}
                }
            }
        }
        // Re-inject hot-plug events into the runtime as if it had
        // drained them itself. We do this by calling drain_events,
        // which inspects `next_event` on the gilrs queue - but we've
        // already drained it. To keep a single source of truth, we
        // re-implement the connect/disconnect handling against the
        // saved events here so the runtime's slot state stays
        // current.
        for (id, ev) in &hotplug_after {
            if let Some(notice) = self.input.handle_synthetic_event(*id, ev) {
                match &notice {
                    HotplugNotice::Assigned { player, name } => {
                        eprintln!("vibenes: P{player} = {name}");
                    }
                    HotplugNotice::Disconnected { player, name } => {
                        if let Some(p) = player {
                            eprintln!("vibenes: P{p} controller disconnected ({name})");
                        } else if debug {
                            eprintln!("gamepad disconnected: {name}");
                        }
                    }
                    HotplugNotice::Ignored { name } => {
                        // Surfaced unconditionally (not gated on
                        // VIBENES_GAMEPAD_DEBUG) so the user
                        // understands why a freshly-plugged pad
                        // isn't moving the player. Two reasons hit
                        // this path: P1 and P2 are both currently
                        // bound to (other) live controllers, or
                        // the device matched our keyboard-as-HID
                        // filter (Keychron docks etc).
                        eprintln!(
                            "vibenes: ignoring controller {name:?} \
                             (both slots in use - disconnect one to free it)"
                        );
                    }
                }
            }
        }
        self.commit_controllers();
    }

    /// Refresh both NES controllers from the input runtime. Safe to
    /// call while `self.nes` is `None`; the assignment just no-ops.
    fn commit_controllers(&mut self) {
        let p1 = self.input.compute_player_bits(1);
        let p2 = self.input.compute_player_bits(2);
        if let Some(nes) = self.nes.as_mut() {
            nes.bus.controllers[0].buttons = p1;
            nes.bus.controllers[1].buttons = p2;
        }
    }
}
