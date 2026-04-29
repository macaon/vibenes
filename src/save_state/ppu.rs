// SPDX-License-Identifier: GPL-3.0-or-later
//! PPU snapshot - shadow of [`crate::nes::ppu::Ppu`].
//!
//! Saves: full register window ($2000-$2007), v/t/x/w internals,
//! scanline/dot/frame counters + odd-frame, OAM (256), secondary OAM
//! (32), palette (32), CIRAM (2 KiB), background pipeline latches +
//! shifters, sprite pipeline state machine + per-slot sprite
//! shifters, NMI level signal + prevent_vbl, open-bus latch + per-bit
//! refresh stamps, master PPU cycle.
//!
//! Drops: `region` (re-derived from the live `Bus` on apply),
//! `frame_buffer: Vec<u8>` (presentation-only; reconstructed on next
//! frame).

use serde::{Deserialize, Serialize};
use serde_big_array::BigArray;

#[derive(Debug, Serialize, Deserialize)]
pub struct PpuSnap {
    pub scanline: i16,
    pub dot: u16,
    pub frame: u64,
    pub odd_frame: bool,
    pub master_ppu_cycle: u64,

    pub ctrl: u8,
    pub mask: u8,
    pub status: u8,
    pub oam_addr: u8,

    pub w_latch: bool,
    pub t: u16,
    pub v: u16,
    pub fine_x: u8,
    pub data_buffer: u8,

    pub skip_last_dot_latched: bool,
    pub rendering_enabled: bool,

    pub nmi_flag: bool,
    pub prevent_vbl: bool,

    #[serde(with = "BigArray")]
    pub oam: [u8; 256],
    pub palette: [u8; 32],
    #[serde(with = "BigArray")]
    pub vram: [u8; 0x800],

    // BG pipeline latches.
    pub bg_next_nt: u8,
    pub bg_next_attr_bits: u8,
    pub bg_next_pat_lo: u8,
    pub bg_next_pat_hi: u8,

    // BG pipeline shifters.
    pub bg_pat_lo: u16,
    pub bg_pat_hi: u16,
    pub bg_attr_lo: u16,
    pub bg_attr_hi: u16,

    // Sprite pipeline output.
    pub secondary_oam: [u8; 32],
    pub sprite_count: u8,
    pub sprite_pat_lo: [u8; 8],
    pub sprite_pat_hi: [u8; 8],
    pub sprite_attr: [u8; 8],
    pub sprite_x: [u8; 8],
    pub sprite_is_zero: [bool; 8],

    // Sprite eval state machine.
    pub oam_copy_buffer: u8,
    pub sec_oam_addr: u8,
    pub sprite_addr_h: u8,
    pub sprite_addr_l: u8,
    pub oam_copy_done: bool,
    pub sprite_in_range: bool,
    pub sprite_zero_added: bool,
    pub overflow_bug_counter: u8,

    pub open_bus: u8,
    pub open_bus_refresh: [u64; 8],
}

impl Default for PpuSnap {
    fn default() -> Self {
        Self {
            scanline: 0,
            dot: 0,
            frame: 0,
            odd_frame: false,
            master_ppu_cycle: 0,
            ctrl: 0,
            mask: 0,
            status: 0,
            oam_addr: 0,
            w_latch: false,
            t: 0,
            v: 0,
            fine_x: 0,
            data_buffer: 0,
            skip_last_dot_latched: false,
            rendering_enabled: false,
            nmi_flag: false,
            prevent_vbl: false,
            oam: [0; 256],
            palette: [0; 32],
            vram: [0; 0x800],
            bg_next_nt: 0,
            bg_next_attr_bits: 0,
            bg_next_pat_lo: 0,
            bg_next_pat_hi: 0,
            bg_pat_lo: 0,
            bg_pat_hi: 0,
            bg_attr_lo: 0,
            bg_attr_hi: 0,
            secondary_oam: [0; 32],
            sprite_count: 0,
            sprite_pat_lo: [0; 8],
            sprite_pat_hi: [0; 8],
            sprite_attr: [0; 8],
            sprite_x: [0; 8],
            sprite_is_zero: [false; 8],
            oam_copy_buffer: 0,
            sec_oam_addr: 0,
            sprite_addr_h: 0,
            sprite_addr_l: 0,
            oam_copy_done: false,
            sprite_in_range: false,
            sprite_zero_added: false,
            overflow_bug_counter: 0,
            open_bus: 0,
            open_bus_refresh: [0; 8],
        }
    }
}

impl PpuSnap {
    pub fn capture(ppu: &crate::nes::ppu::Ppu) -> Self {
        ppu.save_state_capture()
    }

    pub fn apply(self, ppu: &mut crate::nes::ppu::Ppu) {
        ppu.save_state_apply(self);
    }
}
