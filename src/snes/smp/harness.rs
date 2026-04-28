// SPDX-License-Identifier: GPL-3.0-or-later
//! Standalone SPC harness for loading raw SPC code into [`ApuSubsystem`]
//! and stepping it without booting through the IPL ROM. Used by the
//! integration tests in `tests/peterlemon_spc700.rs` to validate the
//! SPC700 ISA against external known-good test images (PeterLemon's
//! SPC700 unit tests).
//!
//! The PeterLemon `.spc` files are NOT standard SNES SPC sound dumps -
//! `bass`'s `output "...spc"` directive emits a flat binary of the SPC
//! code starting at the seek origin (`SPCRAM = $0200`). Our harness
//! deposits those bytes verbatim into ARAM at `$0200`, points the SMP's
//! PC at the entry, and runs.
//!
//! This file lives next to the rest of the `smp` core because the
//! harness needs `pub` access to `IntegratedSmpBus`'s field-borrow
//! constructor pattern. All entry points are `pub fn` so external
//! integration tests (in `tests/`) can reach them.

use super::bus::IntegratedSmpBus;
#[cfg(test)]
use super::bus::SmpBus;
use super::state::ApuPorts;
use crate::snes::ApuSubsystem;

/// Load `bytes` into ARAM starting at `base_addr`, point the SMP's PC
/// at `base_addr`, and clear the IPL shadow so the bytes at the top
/// of ARAM (if any) read normally.
///
/// Caller-provided `apu` is mutated in place. The mailbox latches in
/// `ports` are reset to zero so the harness sees only what the SMP
/// itself produces.
pub fn load_raw_spc_image(
    apu: &mut ApuSubsystem,
    ports: &mut ApuPorts,
    bytes: &[u8],
    base_addr: u16,
) {
    let base = base_addr as usize;
    let end = base.saturating_add(bytes.len()).min(apu.aram.len());
    let copy_len = end - base;
    apu.aram[base..end].copy_from_slice(&bytes[..copy_len]);
    apu.smp.pc = base_addr;
    apu.smp.sp = 0xEF;
    apu.cycles = 0;
    // Match the bass-assembler output convention: SPC code starts
    // running with IPL shadow disabled (CONTROL.7 = 0). The IPL
    // bytes still live at $FFC0 in the ROM, but ARAM at that address
    // wins on read.
    apu.control.raw = 0x00;
    *ports = ApuPorts {
        cpu_to_smp: [0; 4],
        smp_to_cpu: [0; 4],
        pending_cpu_to_smp: [0; 4],
        pending_dirty: 0,
    };
}

/// Step the SMP for at most `max_cycles` SMP master cycles, returning
/// whenever a `should_stop` predicate over the current ports / ARAM
/// returns true. Returns the actual cycles consumed.
pub fn run_smp_until<F>(
    apu: &mut ApuSubsystem,
    ports: &mut ApuPorts,
    max_cycles: u64,
    mut should_stop: F,
) -> u64
where
    F: FnMut(&ApuSubsystem, &ApuPorts) -> bool,
{
    let start = apu.cycles;
    while apu.cycles - start < max_cycles {
        if should_stop(apu, ports) {
            break;
        }
        let cycles_before = apu.cycles;
        {
            let mut bus = IntegratedSmpBus {
                aram: &mut apu.aram,
                ipl: &apu.ipl.bytes,
                control: &mut apu.control,
                timers: &mut apu.timers,
                dsp: &mut apu.dsp,
                ports,
                cycles: &mut apu.cycles,
                mixer: &mut apu.mixer,
            };
            apu.smp.step(&mut bus);
        }
        // Same 32 kHz sample-clock advance the orchestrator does after
        // every SMP step; keeps DSP-driven tests (PeterLemon SPC700
        // ISA harness, future DSP test ROMs) producing audio samples
        // through the same path the real Snes orchestrator uses.
        let delta = (apu.cycles - cycles_before) as u32;
        apu.advance_dsp(delta);
    }
    apu.cycles - start
}

/// Convenience: run until any of the given mailbox bytes appear in
/// `smp_to_cpu[0]`. Returns the actual byte observed (or the latest
/// value if the budget was exhausted before any match).
pub fn run_smp_until_mailbox_byte(
    apu: &mut ApuSubsystem,
    ports: &mut ApuPorts,
    max_cycles: u64,
    target_bytes: &[u8],
) -> u8 {
    let targets: std::collections::HashSet<u8> = target_bytes.iter().copied().collect();
    run_smp_until(apu, ports, max_cycles, |_, ports| {
        targets.contains(&ports.smp_to_cpu[0])
    });
    ports.smp_to_cpu[0]
}

/// Force-tick the cycle counter without running an instruction. Used
/// by tests that want to advance the timers without dispatching SPC
/// code.
#[cfg(test)]
pub fn tick_idle(apu: &mut ApuSubsystem, ports: &mut ApuPorts, count: u64) {
    let cycles_before = apu.cycles;
    {
        let mut bus = IntegratedSmpBus {
            aram: &mut apu.aram,
            ipl: &apu.ipl.bytes,
            control: &mut apu.control,
            timers: &mut apu.timers,
            dsp: &mut apu.dsp,
            ports,
            cycles: &mut apu.cycles,
            mixer: &mut apu.mixer,
        };
        for _ in 0..count {
            bus.idle();
        }
    }
    let delta = (apu.cycles - cycles_before) as u32;
    apu.advance_dsp(delta);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snes::smp::ipl::Ipl;

    #[test]
    fn load_raw_spc_image_resets_pc_and_sp() {
        let mut apu = ApuSubsystem::new(Ipl::embedded());
        let mut ports = ApuPorts::RESET;
        // Tiny program: NOP, NOP, RET (just so we don't fall off).
        load_raw_spc_image(&mut apu, &mut ports, &[0x00, 0x00, 0x6F], 0x0200);
        assert_eq!(apu.smp.pc, 0x0200);
        assert_eq!(apu.smp.sp, 0xEF);
        assert_eq!(apu.aram[0x0200], 0x00);
        assert_eq!(apu.aram[0x0202], 0x6F);
        assert_eq!(apu.cycles, 0);
    }

    #[test]
    fn run_smp_until_breaks_on_predicate() {
        // Program: MOV $F4, #$42 (4 bytes); then BRA . (infinite loop).
        // After enough cycles, mailbox $F4 should hold $42 and
        // the predicate fires.
        let prog = [
            0x8F, 0x42, 0xF4, // MOV $F4, #$42
            0x2F, 0xFE, // BRA -2 (loop back to BRA)
        ];
        let mut apu = ApuSubsystem::new(Ipl::embedded());
        let mut ports = ApuPorts::RESET;
        load_raw_spc_image(&mut apu, &mut ports, &prog, 0x0200);
        let consumed = run_smp_until(&mut apu, &mut ports, 1_000_000, |_, ports| {
            ports.smp_to_cpu[0] == 0x42
        });
        assert!(consumed < 100, "should break out very quickly");
        assert_eq!(ports.smp_to_cpu[0], 0x42);
    }

    #[test]
    fn run_smp_until_mailbox_byte_matches_pass_token() {
        // Same idea, using the convenience wrapper.
        let prog = [
            0x8F, 0x01, 0xF4, // MOV $F4, #$01 (pass token)
            0x2F, 0xFE, // BRA - (spin)
        ];
        let mut apu = ApuSubsystem::new(Ipl::embedded());
        let mut ports = ApuPorts::RESET;
        load_raw_spc_image(&mut apu, &mut ports, &prog, 0x0200);
        let observed = run_smp_until_mailbox_byte(&mut apu, &mut ports, 1_000_000, &[0x01, 0x81]);
        assert_eq!(observed, 0x01);
    }
}
