//! `vibenes` — the windowed emulator binary. Phase 6A.2 opens a blank
//! window and holds the loaded NES; rendering and per-frame stepping
//! land in 6A.3 (wgpu pipeline) and 6A.4 (PPU background pipeline).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use vibenes::app;
use vibenes::nes::Nes;
use vibenes::rom::Cartridge;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

const WINDOW_TITLE: &str = "vibenes";
// NES native resolution is 256×240. Open at 2× by default so pixels are
// visible on a modern display; wgpu sampler in 6A.3 will handle integer
// scaling when the window is resized.
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

/// Window + NES owner. 6A.2 is deliberately inert — we just keep the NES
/// alive so the render-path scaffolding in 6A.3 has something to draw
/// from. The `_nes` field becomes active once we wire frame stepping.
struct App {
    _nes: Nes,
    window: Option<Window>,
}

impl App {
    fn new(nes: Nes) -> Self {
        Self {
            _nes: nes,
            window: None,
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
        match event_loop.create_window(attrs) {
            Ok(w) => self.window = Some(w),
            Err(e) => {
                eprintln!("vibenes: failed to create window: {e}");
                event_loop.exit();
            }
        }
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
                        physical_key: PhysicalKey::Code(KeyCode::Escape),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                // No wgpu surface yet — 6A.3 adds the passthrough pipeline.
                // Keep the redraw pump alive so the window stays responsive.
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            _ => {}
        }
    }
}
