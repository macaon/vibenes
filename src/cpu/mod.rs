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

    nmi_seen: bool,
    pending_interrupt: Option<Interrupt>,
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
            nmi_seen: false,
            pending_interrupt: None,
        }
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
        self.nmi_seen = false;
        self.pending_interrupt = None;
        self.halted = false;
        self.halt_reason = None;
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
        // Edge-detect NMI using the previous cycle's latch state.
        let nmi_latched = bus.prev_nmi_pending;
        if nmi_latched && !self.nmi_seen {
            self.pending_interrupt = Some(Interrupt::Nmi);
            bus.nmi_pending = false;
            self.nmi_seen = true;
            return;
        }
        if !bus.nmi_pending && !bus.prev_nmi_pending {
            self.nmi_seen = false;
        }

        let i_for_poll = match op {
            // CLI / SEI / PLP modify I in their last cycle → polling
            // at penultimate sees the OLD value.
            0x58 | 0x78 | 0x28 => i_flag_before,
            // Everything else (including RTI, whose I change lands at
            // cycle 4 with cycles 5+6 following): current I is correct.
            _ => self.p.interrupt(),
        };
        if bus.prev_irq_line && !i_for_poll {
            self.pending_interrupt = Some(Interrupt::Irq);
        }
    }

    fn service_interrupt(&mut self, bus: &mut Bus, kind: Interrupt) {
        let vector = match kind {
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
        let lo = bus.read(vector);
        let hi = bus.read(vector.wrapping_add(1));
        self.pc = u16::from_le_bytes([lo, hi]);
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
