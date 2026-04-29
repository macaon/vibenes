// SPDX-License-Identifier: GPL-3.0-or-later
//! APU snapshot - shadow of [`crate::nes::apu::Apu`] including all
//! five channels, the frame counter, and the per-channel envelope /
//! sweep / length-counter / DMC mid-transfer state.
//!
//! Drops: `region` (re-derived from the live bus on apply).
//!
//! The shape of these structs deliberately mirrors the live ones
//! (1:1 fields). A future refactor that splits or merges channel
//! state requires updating both the live struct AND its `*Snap`
//! mirror plus a [`crate::save_state::FORMAT_VERSION`] bump.

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct EnvelopeSnap {
    pub start: bool,
    pub loop_flag: bool,
    pub constant: bool,
    pub divider_period: u8,
    pub divider: u8,
    pub decay: u8,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct SweepSnap {
    pub enabled: bool,
    pub period: u8,
    pub divider: u8,
    pub negate: bool,
    pub shift: u8,
    pub reload: bool,
    pub ones_complement: bool,
    pub target_period: u16,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct LengthCounterSnap {
    pub counter: u8,
    pub halt: bool,
    pub enabled: bool,
    /// `Option<bool>` from the live `pending_halt` field; serialized
    /// straight through.
    pub pending_halt: Option<bool>,
    pub pending_reload: Option<u8>,
    pub counter_at_write: u8,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct PulseSnap {
    pub envelope: EnvelopeSnap,
    pub sweep: SweepSnap,
    pub length: LengthCounterSnap,
    pub duty: u8,
    pub sequencer_pos: u8,
    pub timer: u16,
    pub period: u16,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct TriangleSnap {
    pub length: LengthCounterSnap,
    pub linear_reload_flag: bool,
    pub linear_reload_value: u8,
    pub linear_counter: u8,
    pub control_flag: bool,
    pub timer: u16,
    pub period: u16,
    pub sequencer_pos: u8,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct NoiseSnap {
    pub envelope: EnvelopeSnap,
    pub length: LengthCounterSnap,
    pub lfsr: u16,
    pub mode_short: bool,
    pub timer: u16,
    pub period: u16,
}

/// DMC mid-transfer state.
///
/// Easy-to-miss fields per /nes-expert and Mesen2:
/// - `buffer: Option<u8>` - the sample byte fetched from CPU memory
///   waiting for the shift register to underflow. `None` means
///   "buffer empty, DMA armed if bytes_remaining > 0". puNES and
///   Mesen2 both serialize this.
/// - `dma_pending` and `enable_dma_delay` / `enable_dma_addr` -
///   Mesen2's `_transferStartDelay` for `$4015` enable→DMA arm.
///   Without this state the first DMA after load fires too early.
/// - `bits_remaining` - 8 at power-on; serializing it preserves
///   sub-sample-byte phase.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct DmcSnap {
    pub irq_enabled: bool,
    pub loop_flag: bool,
    pub period: u16,
    pub timer: u16,
    pub sample_addr_start: u16,
    pub sample_length_cfg: u16,
    pub current_addr: u16,
    pub bytes_remaining: u16,
    pub shift_reg: u8,
    pub bits_remaining: u8,
    pub silence: bool,
    pub buffer: Option<u8>,
    pub dma_pending: Option<u16>,
    pub enable_dma_delay: u8,
    pub enable_dma_addr: u16,
    pub output: u8,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum FrameCounterModeSnap {
    #[default]
    FourStep = 0,
    FiveStep = 1,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct FrameCounterPendingWriteSnap {
    pub value: u8,
    pub apply_at: u64,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct FrameCounterSnap {
    pub mode: FrameCounterModeSnap,
    pub irq_inhibit: bool,
    pub counter: u64,
    pub pending_write: Option<FrameCounterPendingWriteSnap>,
    pub block_ticks_until: u64,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct ApuSnap {
    pub cycle: u64,
    pub frame_counter: FrameCounterSnap,
    pub pulse1: PulseSnap,
    pub pulse2: PulseSnap,
    pub triangle: TriangleSnap,
    pub noise: NoiseSnap,
    pub dmc: DmcSnap,
    pub frame_irq: bool,
    pub dmc_irq: bool,
}

impl ApuSnap {
    pub fn capture(apu: &crate::nes::apu::Apu) -> Self {
        apu.save_state_capture()
    }

    pub fn apply(self, apu: &mut crate::nes::apu::Apu) {
        apu.save_state_apply(self);
    }
}
