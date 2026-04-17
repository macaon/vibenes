//! `vibenes` — the windowed emulator binary. Phase 6A.3 stands up the
//! wgpu passthrough pipeline and presents a static diagnostic pattern;
//! 6A.4 hooks the PPU's real framebuffer in.

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

/// Window + renderer + NES owner. 6A.3 uploads a static diagnostic
/// pattern once on init; 6A.4 swaps that for `nes.bus.ppu.frame_buffer`
/// each completed frame.
struct App {
    _nes: Nes,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
}

impl App {
    fn new(nes: Nes) -> Self {
        Self {
            _nes: nes,
            window: None,
            renderer: None,
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
        // One-shot upload of the diagnostic pattern. 6A.4 replaces this
        // with per-frame `upload_framebuffer(&ppu.frame_buffer)`.
        renderer.upload_framebuffer(&vibenes::gfx::diagnostic_pattern());
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
                        physical_key: PhysicalKey::Code(KeyCode::Escape),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => event_loop.exit(),
            WindowEvent::Resized(new_size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(new_size);
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = self.renderer.as_mut() {
                    match r.render() {
                        PresentOutcome::Presented | PresentOutcome::Skipped => {}
                        PresentOutcome::NeedsReconfigure => r.reconfigure(),
                        PresentOutcome::Fatal(msg) => {
                            eprintln!("vibenes: {msg}");
                            event_loop.exit();
                        }
                    }
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            _ => {}
        }
    }
}
