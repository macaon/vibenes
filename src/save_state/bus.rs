// SPDX-License-Identifier: GPL-3.0-or-later
//! Bus snapshot - shadow of [`crate::nes::bus::Bus`] excluding
//! `ppu`, `apu`, `mapper`, and `audio_sink`. Those are sibling
//! subsystems in [`crate::save_state::Snapshot`] (PPU/APU) or live
//! out-of-band: `mapper` is owned by Phase 3, and `audio_sink` is
//! host hardware that's reattached fresh on load.
//!
//! Saves: 2 KiB internal RAM, master clock, both controllers (with
//! strobe + shift register), NMI/IRQ line state including
//! cross-cycle latches, open-bus latch, full DMA state machine
//! (DMC + sprite), and the cached `mapper_id` (purely for
//! cross-validation against the file header).

use serde::{Deserialize, Serialize};
use serde_big_array::BigArray;

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct ControllerSnap {
    pub buttons: u8,
    pub strobe: bool,
    pub shifter: u8,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct MasterClockSnap {
    /// Region tag mirrored here for sanity-checking against the file
    /// header on apply. Not authoritative - the header's region is
    /// validated separately.
    pub region: super::RegionTag,
    pub master_cycles: u64,
    pub cpu_cycles: u64,
    pub ppu_cycles: u64,
    pub ppu_offset: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BusSnap {
    pub clock: MasterClockSnap,
    #[serde(with = "BigArray")]
    pub ram: [u8; 0x800],
    pub controllers: [ControllerSnap; 2],
    pub nmi_pending: bool,
    pub irq_line: bool,
    pub prev_irq_line: bool,
    pub prev_nmi_pending: bool,
    pub prev_nmi_flag: bool,
    pub open_bus: u8,
    pub need_halt: bool,
    pub need_dummy_read: bool,
    pub dmc_dma_running: bool,
    pub dmc_dma_addr: u16,
    pub sprite_dma_running: bool,
    pub sprite_dma_page: u8,
    pub in_dma_loop: bool,
    /// Mapper id at capture time. Not used for apply (the live cart's
    /// mapper id is authoritative); stored for diagnostic
    /// cross-checking against the file header.
    pub mapper_id: u16,
}

impl Default for BusSnap {
    fn default() -> Self {
        Self {
            clock: MasterClockSnap::default(),
            ram: [0; 0x800],
            controllers: [ControllerSnap::default(); 2],
            nmi_pending: false,
            irq_line: false,
            prev_irq_line: false,
            prev_nmi_pending: false,
            prev_nmi_flag: false,
            open_bus: 0,
            need_halt: false,
            need_dummy_read: false,
            dmc_dma_running: false,
            dmc_dma_addr: 0,
            sprite_dma_running: false,
            sprite_dma_page: 0,
            in_dma_loop: false,
            mapper_id: 0,
        }
    }
}

impl BusSnap {
    pub fn capture(bus: &crate::nes::bus::Bus) -> Self {
        bus.save_state_capture()
    }

    pub fn apply(self, bus: &mut crate::nes::bus::Bus) {
        bus.save_state_apply(self);
    }
}
