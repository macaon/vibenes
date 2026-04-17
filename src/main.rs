//! `vibenes` — the windowed emulator binary. Phase 6A.4 hooks the
//! PPU's real framebuffer in: step the NES until a frame completes,
//! upload the result, present, repeat. Pace is vsync via wgpu's Fifo
//! present mode; NTSC 60.0988 Hz drift is a Phase 7 (audio-pacing)
//! problem.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use vibenes::app;
use vibenes::gfx::{PresentOutcome, Renderer};
use vibenes::nes::Nes;
use vibenes::rom::Cartridge;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

const WINDOW_TITLE: &str = "vibenes";
const DEFAULT_WINDOW_WIDTH: u32 = 512;
const DEFAULT_WINDOW_HEIGHT: u32 = 480;

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
    let nes = app::build_nes(cart)?;
    eprintln!("region={:?} reset PC=${:04X}", nes.region(), nes.cpu.pc);

    let event_loop = EventLoop::new().context("create winit event loop")?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut handler = App::new(nes);
    event_loop.run_app(&mut handler).context("winit event loop")?;
    Ok(())
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
    halted_notice_shown: bool,
}

impl App {
    fn new(nes: Nes) -> Self {
        Self {
            nes,
            window: None,
            renderer: None,
            halted_notice_shown: false,
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

        renderer.upload_framebuffer(&self.nes.bus.ppu.frame_buffer);
        match renderer.render() {
            PresentOutcome::Presented | PresentOutcome::Skipped => {}
            PresentOutcome::NeedsReconfigure => renderer.reconfigure(),
            PresentOutcome::Fatal(msg) => {
                eprintln!("vibenes: {msg}");
                event_loop.exit();
            }
        }
    }
}

impl ApplicationHandler for App {
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
        self.window = Some(window);
        self.renderer = Some(renderer);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
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
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            _ => {}
        }
    }
}

impl App {
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
