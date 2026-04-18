//! `vibenes` — the windowed emulator binary. Phase 6A.4 hooks the
//! PPU's real framebuffer in: step the NES until a frame completes,
//! upload the result, present, repeat. Pace is vsync via wgpu's Fifo
//! present mode; NTSC 60.0988 Hz drift is a Phase 7 (audio-pacing)
//! problem.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use vibenes::app;
use vibenes::audio;
use vibenes::clock::Region;
use vibenes::gfx::{PresentOutcome, Renderer};
use vibenes::nes::Nes;
use vibenes::rom::Cartridge;
use vibenes::ui::{RecentRoms, UiCommand, UiLayer};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

const WINDOW_TITLE: &str = "vibenes";
const DEFAULT_WINDOW_WIDTH: u32 = 512;
const DEFAULT_WINDOW_HEIGHT: u32 = 480;

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
    let rom_path = parse_args()?;
    let cart = Cartridge::load(&rom_path)
        .with_context(|| format!("loading ROM {}", rom_path.display()))?;
    eprintln!("loaded: {}", cart.describe());
    let mut nes = app::build_nes(cart)?;
    eprintln!("region={:?} reset PC=${:04X}", nes.region(), nes.cpu.pc);

    // Try to open a host audio stream. If the host has no audio device
    // (headless CI, WSL without PulseAudio, etc.) we still want the
    // emulator to run — just silently.
    let audio_stream = match audio::start(nes.region().cpu_clock_hz()) {
        Ok((sink, stream)) => {
            eprintln!(
                "audio: {} Hz × {} ch",
                stream.sample_rate, stream.channels
            );
            nes.attach_audio(sink);
            Some(stream)
        }
        Err(e) => {
            eprintln!("vibenes: audio disabled ({e:#})");
            None
        }
    };

    let event_loop = EventLoop::new().context("create winit event loop")?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut handler = App::new(nes, audio_stream, rom_path);
    event_loop.run_app(&mut handler).context("winit event loop")?;
    Ok(())
}

fn frame_period_for(region: Region) -> Duration {
    match region {
        Region::Ntsc => NTSC_FRAME_PERIOD,
        Region::Pal => PAL_FRAME_PERIOD,
    }
}

fn parse_args() -> Result<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    match args.next() {
        Some(p) => Ok(PathBuf::from(p)),
        None => bail!("usage: vibenes <rom.nes>"),
    }
}

/// Window + renderer + NES owner. Each RedrawRequested drives the NES
/// for one PPU frame and presents `nes.bus.ppu.frame_buffer`.
struct App {
    nes: Nes,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    ui: Option<UiLayer>,
    halted_notice_shown: bool,
    /// Per-region frame period (NTSC ≈ 16.639 ms, PAL ≈ 19.997 ms).
    /// Seeded from the cartridge's TV-system at construction; updated
    /// when a ROM swap changes regions.
    frame_period: Duration,
    /// Deadline for the next frame's present. Advanced by one
    /// `frame_period` each completed frame so drift stays pinned to
    /// wall-clock rather than accumulating.
    next_frame_deadline: Option<Instant>,
    /// Keeps the cpal output stream alive. Dropping this silences the
    /// device — hence why it lives on the App owner rather than a
    /// local inside `run()`.
    _audio_stream: Option<audio::AudioStream>,
    /// Most-recently-loaded ROMs, shown in the File menu. Seeded with
    /// the path passed on the command line.
    recent_roms: RecentRoms,
}

impl App {
    fn new(
        nes: Nes,
        audio_stream: Option<audio::AudioStream>,
        initial_rom: PathBuf,
    ) -> Self {
        let frame_period = frame_period_for(nes.region());
        let mut recent_roms = RecentRoms::default();
        recent_roms.push(initial_rom);
        Self {
            nes,
            window: None,
            renderer: None,
            ui: None,
            halted_notice_shown: false,
            frame_period,
            next_frame_deadline: None,
            _audio_stream: audio_stream,
            recent_roms,
        }
    }

    fn advance_and_present(&mut self, event_loop: &ActiveEventLoop) {
        let renderer = match self.renderer.as_mut() {
            Some(r) => r,
            None => return,
        };

        if !self.nes.cpu.halted {
            if let Err(msg) = self.nes.step_until_frame() {
                if !self.halted_notice_shown {
                    eprintln!("vibenes: CPU error: {msg}");
                    self.halted_notice_shown = true;
                }
            } else if self.nes.cpu.halted && !self.halted_notice_shown {
                let reason = self
                    .nes
                    .cpu
                    .halt_reason
                    .clone()
                    .unwrap_or_else(|| "halted".to_string());
                eprintln!("vibenes: CPU halted: {reason}");
                self.halted_notice_shown = true;
            }
        }
        // Hand the frame's audio to the ring so the cpal callback can
        // drain it before the next wakeup.
        self.nes.end_audio_frame();

        renderer.upload_framebuffer(&self.nes.bus.ppu.frame_buffer);
        // Build the UI outside the overlay closure so we can borrow
        // `self.ui` mutably here and again inside the closure without
        // a conflicting double-borrow of `self`. Commands produced by
        // egui widgets are drained after paint so emulator mutations
        // don't race with any in-flight borrows.
        let mut cmds: Vec<UiCommand> = Vec::new();
        if let (Some(ui), Some(window)) = (self.ui.as_mut(), self.window.as_ref()) {
            ui.run(window, &self.recent_roms, &mut cmds);
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
        let attrs = Window::default_attributes()
            .with_title(WINDOW_TITLE)
            .with_inner_size(winit::dpi::LogicalSize::new(
                DEFAULT_WINDOW_WIDTH,
                DEFAULT_WINDOW_HEIGHT,
            ));
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
                if code == KeyCode::Escape && state == ElementState::Pressed {
                    event_loop.exit();
                } else if consumed_by_ui {
                    // egui owns this event (text field, menu nav, etc.).
                    // Do not forward to the NES controller or reset.
                } else if code == KeyCode::KeyR && state == ElementState::Pressed {
                    self.nes.reset();
                    self.halted_notice_shown = false;
                    eprintln!("vibenes: reset (PC=${:04X})", self.nes.cpu.pc);
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
        if let Err(e) = self.nes.swap_cartridge(cart) {
            eprintln!("vibenes: swap failed: {e:#}");
            return;
        }
        // The new ROM may have a different TV system; re-pin the frame
        // deadline to its cadence and reset the deadline anchor so we
        // don't eat the full delta at once.
        self.frame_period = frame_period_for(self.nes.region());
        self.next_frame_deadline = None;
        self.halted_notice_shown = false;
        self.recent_roms.push(path.to_path_buf());
        eprintln!(
            "vibenes: loaded {} (region={:?} PC=${:04X})",
            path.display(),
            self.nes.region(),
            self.nes.cpu.pc
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
        let c = &mut self.nes.bus.controllers[0];
        match state {
            ElementState::Pressed => c.buttons |= bit,
            ElementState::Released => c.buttons &= !bit,
        }
    }
}
