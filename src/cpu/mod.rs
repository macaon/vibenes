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
        // On reset: SP decremented by 3 (push ops that don't write), I=1,
        // PC loaded from reset vector at $FFFC/$FFFD. Real hardware also
        // performs 7 cycles of dummy reads before PC is valid.
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
    /// Returns Ok(()) on success, Err with a description on an unhandled
    /// opcode so the caller can shut down gracefully.
    pub fn step(&mut self, bus: &mut Bus) -> Result<(), String> {
        if self.halted {
            return Ok(());
        }
        self.poll_interrupts(bus);
        if let Some(kind) = self.pending_interrupt.take() {
            self.service_interrupt(bus, kind);
            return Ok(());
        }
        let op = self.fetch_byte(bus);
        ops::execute(self, bus, op).map_err(|msg| {
            self.halted = true;
            self.halt_reason = Some(msg.clone());
            msg
        })?;
        self.cycles = bus.clock.cpu_cycles();
        Ok(())
    }

    fn poll_interrupts(&mut self, bus: &mut Bus) {
        if bus.nmi_pending && !self.nmi_seen {
            self.pending_interrupt = Some(Interrupt::Nmi);
            bus.nmi_pending = false;
            self.nmi_seen = true;
            return;
        }
        if !bus.nmi_pending {
            self.nmi_seen = false;
        }
        if bus.irq_line && !self.p.interrupt() {
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
