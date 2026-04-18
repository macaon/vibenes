//! 6502 (Ricoh 2A03/2A07) CPU core.
//!
//! Timing model: every `bus.read` / `bus.write` costs exactly one CPU
//! cycle; extra "internal" cycles are modeled as dummy reads against the
//! correct bus address (matching the real chip). The CPU therefore does
//! not return cycle counts — cycles are tallied in the master clock by
//! the bus.

pub mod flags;
pub mod ops;

use crate::bus::Bus;

use self::flags::StatusFlags;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    Nmi,
    Irq,
    Reset,
    Brk,
}

#[derive(Debug)]
pub struct Cpu {
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub sp: u8,
    pub pc: u16,
    pub p: StatusFlags,

    pub cycles: u64,
    pub halted: bool,
    pub halt_reason: Option<String>,

    pending_interrupt: Option<Interrupt>,

    /// Set by `ops::branch()` when the current instruction is a taken
    /// branch with no page cross (3-cycle form) AND the IRQ line was
    /// still low one cycle before the penultimate. Consumed and
    /// cleared by `poll_interrupts_at_end`. Gates the "branch-delays-
    /// IRQ" suppression — see `ops::branch` for the sample window
    /// rationale and the Mesen2/puNES references.
    branch_taken_no_cross: bool,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu {
    pub fn new() -> Self {
        Self {
            a: 0,
            x: 0,
            y: 0,
            sp: 0xFD,
            pc: 0,
            p: StatusFlags::from_bits(0x24),
            cycles: 0,
            halted: false,
            halt_reason: None,
            pending_interrupt: None,
            branch_taken_no_cross: false,
        }
    }

    /// Mark the current instruction as a taken branch with no page
    /// cross. Called from `ops::branch`. Only consumed by the next
    /// `poll_interrupts_at_end`.
    pub(crate) fn mark_branch_taken_no_cross(&mut self) {
        self.branch_taken_no_cross = true;
    }

    pub fn reset(&mut self, bus: &mut Bus) {
        // Real 6502 reset = 7 cycles: 2 dummy opcode/operand fetches,
        // 3 dummy stack "pushes" (reads because write is suppressed on
        // RESET), then the low and high vector reads from $FFFC/$FFFD.
        // We reproduce the 5 dummy cycles as bus reads so the APU /
        // PPU see the correct cycle count. The read addresses don't
        // matter (side effects fall through open bus / RAM), but we
        // stick to $00FF which is the post-decrement stack slot on
        // real hardware — lets future stack-watching tests agree.
        for _ in 0..5 {
            let _ = bus.read(0x00FF);
        }
        let lo = bus.read(0xFFFC);
        let hi = bus.read(0xFFFD);
        self.pc = u16::from_le_bytes([lo, hi]);
        self.sp = self.sp.wrapping_sub(3);
        self.p.set_interrupt(true);
        self.pending_interrupt = None;
        self.halted = false;
        self.halt_reason = None;
        self.branch_taken_no_cross = false;
    }

    /// Run one instruction (or service a pending interrupt).
    ///
    /// Interrupt polling model: interrupts are sampled at the end of
    /// the **penultimate** CPU cycle of the instruction (real 6502),
    /// not between instructions. We approximate this by:
    /// - Reading `bus.prev_irq_line` / `bus.prev_nmi_pending` at the
    ///   end of each instruction — those fields capture end-of-previous-
    ///   cycle state, which after an N-cycle instruction equals the
    ///   end of cycle N-1 (the penultimate).
    /// - Using the I-flag value that was active during the penultimate
    ///   cycle. For most instructions this equals the current I flag.
    ///   CLI/SEI/PLP modify I in their **last** cycle, so penultimate-I
    ///   is the *old* value; we snapshot it before `ops::execute`. RTI
    ///   modifies I in cycle 4 of 6, so penultimate is post-change and
    ///   the current (new) I flag is correct.
    ///
    /// The pending interrupt is stored on the CPU and serviced at the
    /// top of the next `step()`, matching the hardware "fetch the
    /// interrupt vector in place of the next opcode" sequence.
    pub fn step(&mut self, bus: &mut Bus) -> Result<(), String> {
        if self.halted {
            return Ok(());
        }
        if let Some(kind) = self.pending_interrupt.take() {
            self.service_interrupt(bus, kind);
            self.cycles = bus.clock.cpu_cycles();
            return Ok(());
        }
        let i_flag_before = self.p.interrupt();
        let op = self.fetch_byte(bus);
        ops::execute(self, bus, op).map_err(|msg| {
            self.halted = true;
            self.halt_reason = Some(msg.clone());
            msg
        })?;
        self.poll_interrupts_at_end(bus, op, i_flag_before);
        self.cycles = bus.clock.cpu_cycles();
        Ok(())
    }

    /// Poll the NMI/IRQ lines using state captured at the end of the
    /// penultimate cycle. `op` and `i_flag_before` pick the correct I
    /// flag value for instructions that mutate it (see `step` docs).
    fn poll_interrupts_at_end(&mut self, bus: &mut Bus, op: u8, i_flag_before: bool) {
        // NMI uses an edge-triggered latch: `bus.nmi_pending` is set
        // once per rising edge by the PPU and cleared on service. Like
        // Mesen2's `_needNmi`, we just consume it on latch — the line
        // being held asserted won't produce a new edge, so we don't
        // need a separate "already serviced" flag.
        if bus.prev_nmi_pending {
            self.pending_interrupt = Some(Interrupt::Nmi);
            bus.nmi_pending = false;
            return;
        }

        let i_for_poll = match op {
            // CLI / SEI / PLP modify I in their last cycle → polling
            // at penultimate sees the OLD value.
            0x58 | 0x78 | 0x28 => i_flag_before,
            // Everything else (including RTI, whose I change lands at
            // cycle 4 with cycles 5+6 following): current I is correct.
            _ => self.p.interrupt(),
        };

        // Branch-delays-IRQ quirk: on a taken branch with no page
        // cross (3-cycle form), the 6502 suppresses IRQ recognition
        // when the IRQ was newly asserted *during the penultimate
        // cycle*. The gate that decides whether the quirk applies
        // lives in `branch()` itself — it compares the IRQ line
        // one cycle before the penultimate; `branch_taken_no_cross`
        // is only set when that check passes. Here at the final
        // poll, suppression is unconditional once the flag is set
        // and IRQ is high at the penultimate (the usual poll input).
        // References:
        //   Mesen2 NesCpu.h:432-448 (BranchRelative)
        //   puNES cpu.c:114-144 (BRC macro)
        let suppress_by_branch = self.branch_taken_no_cross && bus.prev_irq_line;
        self.branch_taken_no_cross = false;

        if bus.prev_irq_line && !i_for_poll && !suppress_by_branch {
            self.pending_interrupt = Some(Interrupt::Irq);
        }
    }

    fn service_interrupt(&mut self, bus: &mut Bus, kind: Interrupt) {
        let mut vector = match kind {
            Interrupt::Nmi => 0xFFFA,
            Interrupt::Reset => 0xFFFC,
            Interrupt::Irq | Interrupt::Brk => 0xFFFE,
        };
        // 2 internal cycles of dummy read on the real chip.
        bus.read(self.pc);
        bus.read(self.pc);
        // Push PCH, PCL.
        self.push(bus, (self.pc >> 8) as u8);
        self.push(bus, (self.pc & 0xFF) as u8);
        // Push status (B flag set only for BRK/PHP).
        let mut status = self.p.to_u8();
        status |= 0x20; // unused flag always set on push
        if matches!(kind, Interrupt::Brk) {
            status |= 0x10;
        } else {
            status &= !0x10;
        }
        self.push(bus, status);
        self.p.set_interrupt(true);
        // NMI hijack: real 6502 latches the vector choice during the
        // push phase of a BRK/IRQ sequence. We sample at the boundary
        // between cycle 5 (push P) and cycle 6 (vector-low fetch), using
        // `prev_nmi_pending` so the window caps at end-of-cycle-4 state.
        // On hijack the pushed P is unchanged (B flag already reflects
        // BRK=1 / IRQ=0) and the NMI latch is consumed. NMI cannot
        // hijack its own service (already consumed at poll time).
        if matches!(kind, Interrupt::Brk | Interrupt::Irq) && bus.prev_nmi_pending {
            vector = 0xFFFA;
            bus.nmi_pending = false;
        }
        let lo = bus.read(vector);
        let hi = bus.read(vector.wrapping_add(1));
        self.pc = u16::from_le_bytes([lo, hi]);
        // Suppress post-service NMI latch: an NMI that arrived too
        // late to hijack this sequence is deferred to *after* the
        // handler's first instruction, not serviced back-to-back.
        // Matches Mesen2's `_prevNeedNmi = false` at end of BRK
        // (NesCpu.cpp:238) and implicitly after IRQ service.
        if matches!(kind, Interrupt::Brk | Interrupt::Irq) {
            bus.prev_nmi_pending = false;
        }
    }

    #[inline]
    pub fn fetch_byte(&mut self, bus: &mut Bus) -> u8 {
        let byte = bus.read(self.pc);
        self.pc = self.pc.wrapping_add(1);
        byte
    }

    #[inline]
    pub fn fetch_word(&mut self, bus: &mut Bus) -> u16 {
        let lo = self.fetch_byte(bus);
        let hi = self.fetch_byte(bus);
        u16::from_le_bytes([lo, hi])
    }

    #[inline]
    pub fn push(&mut self, bus: &mut Bus, data: u8) {
        bus.write(0x0100 | self.sp as u16, data);
        self.sp = self.sp.wrapping_sub(1);
    }

    #[inline]
    pub fn pop(&mut self, bus: &mut Bus) -> u8 {
        self.sp = self.sp.wrapping_add(1);
        bus.read(0x0100 | self.sp as u16)
    }

    #[inline]
    pub fn set_zn(&mut self, value: u8) {
        self.p.set_zero(value == 0);
        self.p.set_negative((value & 0x80) != 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Bus;
    use crate::clock::Region;
    use crate::mapper::nrom::Nrom;
    use crate::rom::{Cartridge, Mirroring, TvSystem};

    /// Build a 32 KiB PRG-ROM with the given program at the start of
    /// the bank, reset vector → `$8000`, IRQ/BRK vector → `$9000`,
    /// NMI vector → `$A000`. Rest filled with NOP so runaway PC lands
    /// on something harmless.
    fn cart_with_program(program: &[u8]) -> Cartridge {
        let mut prg = vec![0xEAu8; 0x8000];
        prg[..program.len()].copy_from_slice(program);
        // Vectors live at the end of the 32 KiB image ($FFFA..$FFFF).
        prg[0x7FFA] = 0x00; // NMI lo
        prg[0x7FFB] = 0xA0; // NMI hi → $A000
        prg[0x7FFC] = 0x00; // RESET lo
        prg[0x7FFD] = 0x80; // RESET hi → $8000
        prg[0x7FFE] = 0x00; // IRQ/BRK lo
        prg[0x7FFF] = 0x90; // IRQ/BRK hi → $9000
        Cartridge {
            prg_rom: prg,
            chr_rom: vec![0u8; 0x2000],
            chr_ram: false,
            mapper_id: 0,
            submapper: 0,
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: 0x2000,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
        }
    }

    fn build_cpu_and_bus(program: &[u8]) -> (Cpu, Bus) {
        let cart = cart_with_program(program);
        let mut bus = Bus::new(Box::new(Nrom::new(cart)), Region::Ntsc);
        let mut cpu = Cpu::new();
        cpu.reset(&mut bus);
        (cpu, bus)
    }

    /// IRQ already asserted before a taken-no-cross branch is NOT
    /// suppressed — the branch-delays-IRQ quirk only applies when
    /// the line rises *during* the branch's penultimate cycle. An
    /// IRQ that was already high at entry polls normally.
    ///
    /// `cpu_interrupts_v2/5-branch_delays_irq.nes` is the full
    /// quirk oracle (it rotates the IRQ-assert cycle across every
    /// position in the branch); this unit test only guards the
    /// already-high lower edge so a regression flags here before
    /// the ROM-level sweep.
    #[test]
    fn taken_no_cross_branch_with_irq_already_high_fires_normally() {
        // $8000: BCC +$02  (carry clear after reset → taken to $8004)
        // $8002..$8003: NOP (skipped)
        // $8004..: NOP      (target)
        let (mut cpu, mut bus) = build_cpu_and_bus(&[
            0x90, 0x02, // BCC +$02
            0xEA, 0xEA, // NOPs (skipped)
            0xEA, 0xEA, // NOP (target), NOP
        ]);
        cpu.p.set_interrupt(false);

        // Force IRQ high before BCC starts. The first bus tick of
        // BCC's opcode fetch picks frame_irq up through the APU
        // tick, so by the end of cycle 1 `bus.irq_line` is high —
        // i.e. `branch()`'s "one cycle before penultimate" sample
        // (taken right after the operand fetch) sees the line
        // already asserted, and does NOT mark the quirk.
        bus.apu.set_frame_irq_for_test(true);

        cpu.step(&mut bus).expect("BCC");
        assert_eq!(cpu.pc, 0x8004, "BCC taken, no page cross");
        assert!(
            matches!(cpu.pending_interrupt, Some(Interrupt::Irq)),
            "IRQ high before the penultimate must latch normally"
        );
    }

    /// A not-taken branch is 2 cycles; no quirk applies. IRQ asserted
    /// before the branch must fire at the branch's penultimate.
    #[test]
    fn branch_not_taken_does_not_delay_irq() {
        // $8000: CLC
        // $8001: BCS +$02    (not taken — carry is clear)
        // $8003..: NOP
        let (mut cpu, mut bus) = build_cpu_and_bus(&[
            0x18, // CLC
            0xB0, 0x02, // BCS +$02 — not taken
            0xEA, 0xEA, 0xEA, 0xEA,
        ]);
        cpu.p.set_interrupt(false);

        cpu.step(&mut bus).expect("CLC");
        bus.apu.set_frame_irq_for_test(true);

        cpu.step(&mut bus).expect("BCS not taken");
        assert_eq!(cpu.pc, 0x8003, "not-taken branch skipped operand");
        assert!(
            matches!(cpu.pending_interrupt, Some(Interrupt::Irq)),
            "not-taken branch: IRQ should latch normally at penultimate"
        );
    }
}
