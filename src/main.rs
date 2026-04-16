use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use vibenes::nes::Nes;
use vibenes::rom::Cartridge;

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
    let mut args = std::env::args_os().skip(1);
    let rom_path: PathBuf = match args.next() {
        Some(p) => PathBuf::from(p),
        None => bail!("usage: vibenes <rom.nes>"),
    };
    let cart = Cartridge::load(&rom_path)
        .with_context(|| format!("loading ROM {}", rom_path.display()))?;
    eprintln!("loaded: {}", cart.describe());
    if cart.mapper_id != 0 {
        bail!("mapper {} not yet supported", cart.mapper_id);
    }

    let mut nes = Nes::from_cartridge(cart)?;
    eprintln!(
        "region={:?} reset PC=${:04X}",
        nes.region(),
        nes.cpu.pc
    );

    // Stub runtime: step a bounded number of cycles so we prove the loop
    // works without an actual window yet. wgpu integration is next.
    for _ in 0..60 {
        if let Err(msg) = nes.run_cycles(29_781) {
            eprintln!("halt: {}", msg);
            return Err(anyhow::anyhow!(msg));
        }
        if nes.cpu.halted {
            if let Some(reason) = &nes.cpu.halt_reason {
                eprintln!("halt: {}", reason);
                return Err(anyhow::anyhow!(reason.clone()));
            }
            break;
        }
    }
    eprintln!(
        "ran {} CPU cycles across {} frames",
        nes.bus.clock.cpu_cycles(),
        nes.bus.ppu.frame()
    );
    Ok(())
}
