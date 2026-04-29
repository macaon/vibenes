// SPDX-License-Identifier: GPL-3.0-or-later
//! CPU snapshot - shadow of [`crate::nes::cpu::Cpu`].
//!
//! Saves: A/X/Y/SP/PC/P, cycle count, halted flag, pending interrupt
//! type, branch-taken-no-cross latch (for delayed-IRQ branch quirk).
//!
//! Drops: `halt_reason: Option<String>`. It's a debug-only diagnostic
//! string set when the CPU enters a KIL/STP unofficial opcode; on
//! load we reset it to `None` and let the next halt event repopulate
//! it. Saving it would be schema noise.

use serde::{Deserialize, Serialize};

/// Wire-format mirror of [`crate::nes::cpu::Interrupt`]. Distinct enum
/// so an internal rename of `Interrupt` doesn't silently break old
/// state files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum InterruptKindSnap {
    #[default]
    None = 0,
    Nmi = 1,
    Irq = 2,
    Reset = 3,
    Brk = 4,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct CpuSnap {
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub sp: u8,
    pub pc: u16,
    /// Status flags packed into the same wire byte the CPU pushes on
    /// the stack. Bits 4 (B) and 5 (U) are not stored on hardware
    /// either - they're synthesized at push/pop time. We round-trip
    /// the same six bits the live `StatusFlags` carries (NV-DIZC).
    pub p_bits: u8,
    pub cycles: u64,
    pub halted: bool,
    pub pending_interrupt: InterruptKindSnap,
    pub branch_taken_no_cross: bool,
}

impl CpuSnap {
    /// Capture from a live CPU. Thin wrapper over
    /// [`crate::nes::cpu::Cpu::save_state_capture`] - kept on the
    /// snap type so the orchestration in [`crate::save_state::Snapshot`]
    /// reads symmetrically.
    pub fn capture(cpu: &crate::nes::cpu::Cpu) -> Self {
        cpu.save_state_capture()
    }

    /// Apply this snapshot to a live CPU.
    pub fn apply(self, cpu: &mut crate::nes::cpu::Cpu) {
        cpu.save_state_apply(self);
    }
}
