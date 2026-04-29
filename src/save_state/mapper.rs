// SPDX-License-Identifier: GPL-3.0-or-later
//! Mapper snapshot variants.
//!
//! [`MapperState`] is a tagged enum with one variant per mapper
//! struct under `crate::nes::mapper::*`. Phase 3a (this file)
//! covers the ten most-common mappers - NROM (0), MMC1 (1),
//! UxROM (2), CNROM (3), MMC3 (4), MMC5 (5), AxROM (7), MMC2 (9),
//! MMC4 (10), GxROM (66) - which together cover the bulk of the
//! commercial NES library plus every blargg / nesdev test ROM we
//! gate accuracy on.
//!
//! Phase 3b will fill in the remaining variants (VRC1 / VRC2_4 /
//! VRC3 / VRC6 / VRC7 / FDS / Bandai-FCG / Jaleco-SS88006 /
//! Namco-163 / FME-7 / Sunsoft-5B / Rambo-1 / IremG101 /
//! TaitoTC0190 / Mapper037). Until then, those mappers report
//! `Unsupported` from [`crate::nes::mapper::Mapper::save_state_capture`]
//! and [`crate::save_state::save_to_slot`] returns
//! [`crate::save_state::SaveStateError::UnsupportedMapper`].
//!
//! ## Invariants of the apply path
//!
//! - We never serialize `prg_rom` / `chr` (when ROM, not RAM) -
//!   those come from the cart and are static for the run.
//! - We never serialize derived data that's a pure function of
//!   other saved state. MMC5 in particular has a `prg_slots`
//!   resolved-window cache that's recomputed from `prg_mode` +
//!   `prg_regs` on apply via the live mapper's existing
//!   `update_prg_banks` helper.
//! - We do serialize `mirroring` (the dynamic value, distinct from
//!   the cart's hardwired field) for mappers that mutate it (MMC1,
//!   MMC3, AxROM, FME-7, etc.).
//! - PRG-RAM contents are saved (some carts have battery; even
//!   non-battery RAM is part of the live state).

use serde::{Deserialize, Serialize};
use serde_big_array::BigArray;

use crate::nes::rom::Mirroring;

/// Wire-format mirror of [`crate::nes::rom::Mirroring`]. Distinct
/// enum so a future internal rename of `Mirroring` doesn't silently
/// invalidate save files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum MirroringSnap {
    #[default]
    Horizontal = 0,
    Vertical = 1,
    SingleScreenLower = 2,
    SingleScreenUpper = 3,
    FourScreen = 4,
}

impl MirroringSnap {
    pub fn from_live(m: Mirroring) -> Self {
        match m {
            Mirroring::Horizontal => Self::Horizontal,
            Mirroring::Vertical => Self::Vertical,
            Mirroring::SingleScreenLower => Self::SingleScreenLower,
            Mirroring::SingleScreenUpper => Self::SingleScreenUpper,
            Mirroring::FourScreen => Self::FourScreen,
        }
    }

    pub fn to_live(self) -> Mirroring {
        match self {
            Self::Horizontal => Mirroring::Horizontal,
            Self::Vertical => Mirroring::Vertical,
            Self::SingleScreenLower => Mirroring::SingleScreenLower,
            Self::SingleScreenUpper => Mirroring::SingleScreenUpper,
            Self::FourScreen => Mirroring::FourScreen,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct NromSnap {
    pub prg_ram: Vec<u8>,
    pub mirroring: MirroringSnap,
    pub chr_ram_data: Vec<u8>,
    pub save_dirty: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct UxromSnap {
    pub prg_ram: Vec<u8>,
    pub mirroring: MirroringSnap,
    pub chr_ram_data: Vec<u8>,
    pub bank: u8,
    pub save_dirty: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CnromSnap {
    pub prg_ram: Vec<u8>,
    pub mirroring: MirroringSnap,
    pub chr_ram_data: Vec<u8>,
    pub chr_bank: u8,
    pub save_dirty: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AxromSnap {
    pub chr_ram_data: Vec<u8>,
    pub bank: u8,
    pub mirroring: MirroringSnap,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GxromSnap {
    pub chr_ram_data: Vec<u8>,
    pub mirroring: MirroringSnap,
    pub prg_bank: u8,
    pub chr_bank: u8,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Mmc1Snap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub shift: u8,
    pub shift_count: u8,
    pub control: u8,
    pub chr_bank_0: u8,
    pub chr_bank_1: u8,
    pub prg_bank: u8,
    pub mirroring: MirroringSnap,
    pub cycle_counter: u64,
    pub last_write_cycle: Option<u64>,
    pub save_dirty: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Mmc2Snap {
    pub chr_ram_data: Vec<u8>,
    pub mirroring: MirroringSnap,
    pub prg_bank: u8,
    pub left_fd: u8,
    pub left_fe: u8,
    pub right_fd: u8,
    pub right_fe: u8,
    pub left_latch: u8,
    pub right_latch: u8,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Mmc3Snap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub bank_select: u8,
    pub bank_regs: [u8; 8],
    pub mirroring: MirroringSnap,
    pub prg_ram_enabled: bool,
    pub prg_ram_write_protected: bool,
    pub irq_latch: u8,
    pub irq_counter: u8,
    pub irq_reload: bool,
    pub irq_enabled: bool,
    pub irq_line: bool,
    pub a12_low_since: Option<u64>,
    pub reg_a001: u8,
    pub save_dirty: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Mmc4Snap {
    pub chr_ram_data: Vec<u8>,
    pub mirroring: MirroringSnap,
    pub prg_ram: Vec<u8>,
    pub save_dirty: bool,
    pub prg_bank: u8,
    pub left_fd: u8,
    pub left_fe: u8,
    pub right_fd: u8,
    pub right_fe: u8,
    pub left_latch: u8,
    pub right_latch: u8,
}

/// MMC5 is the largest mapper variant. We do NOT serialize the
/// derived `prg_slots` / `prg_ram_slot` window resolution table -
/// those are recomputed from `prg_mode` + `prg_regs` +
/// `prg_ram_protect*` on apply via the existing `update_prg_banks`
/// helper. Same for `last_fetch_kind` (a transient latch reset to
/// `Idle` on apply).
#[derive(Debug, Serialize, Deserialize)]
pub struct Mmc5Snap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub mirroring: MirroringSnap,
    pub prg_mode: u8,
    pub prg_regs: [u8; 5],
    pub prg_ram_protect1: u8,
    pub prg_ram_protect2: u8,
    pub chr_mode: u8,
    pub chr_bg_regs: [u8; 8],
    pub chr_spr_regs: [u8; 4],
    pub chr_upper: u8,
    pub exram_mode: u8,
    pub nt_mapping: u8,
    pub fill_tile: u8,
    pub fill_color: u8,
    #[serde(with = "BigArray")]
    pub exram: [u8; 0x400],
    pub irq_target: u8,
    pub irq_enable: bool,
    pub irq_pending: bool,
    pub scanline_counter: u8,
    pub in_frame: bool,
    pub need_in_frame: bool,
    pub last_ppu_addr: u16,
    pub nt_read_counter: u8,
    pub ppu_idle_counter: u8,
    pub mult_a: u8,
    pub mult_b: u8,
    pub save_dirty: bool,
}

impl Default for Mmc5Snap {
    fn default() -> Self {
        Self {
            prg_ram: Vec::new(),
            chr_ram_data: Vec::new(),
            mirroring: MirroringSnap::default(),
            prg_mode: 0,
            prg_regs: [0; 5],
            prg_ram_protect1: 0,
            prg_ram_protect2: 0,
            chr_mode: 0,
            chr_bg_regs: [0; 8],
            chr_spr_regs: [0; 4],
            chr_upper: 0,
            exram_mode: 0,
            nt_mapping: 0,
            fill_tile: 0,
            fill_color: 0,
            exram: [0; 0x400],
            irq_target: 0,
            irq_enable: false,
            irq_pending: false,
            scanline_counter: 0,
            in_frame: false,
            need_in_frame: false,
            last_ppu_addr: 0,
            nt_read_counter: 0,
            ppu_idle_counter: 0,
            mult_a: 0,
            mult_b: 0,
            save_dirty: false,
        }
    }
}

// ============================================================
// Phase 3b: VRC family + FDS + Bandai/Jaleco/Namco/Sunsoft/etc.
// ============================================================

/// Common VRC IRQ helper - shared by VRC2_4, VRC6, VRC7. Exact 1:1
/// of the live private struct in their respective mapper files.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct VrcIrqSnap {
    pub reload_value: u8,
    pub counter: u8,
    pub prescaler: i16,
    pub enabled: bool,
    pub enabled_after_ack: bool,
    pub cycle_mode: bool,
    pub irq_line: bool,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct Vrc6PulseSnap {
    pub volume: u8,
    pub duty_cycle: u8,
    pub ignore_duty: bool,
    pub frequency: u16,
    pub enabled: bool,
    pub timer: i32,
    pub step: u8,
    pub frequency_shift: u8,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct Vrc6SawSnap {
    pub accumulator_rate: u8,
    pub accumulator: u8,
    pub frequency: u16,
    pub enabled: bool,
    pub timer: i32,
    pub step: u8,
    pub frequency_shift: u8,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct Vrc6AudioSnap {
    pub pulse1: Vrc6PulseSnap,
    pub pulse2: Vrc6PulseSnap,
    pub saw: Vrc6SawSnap,
    pub halt_audio: bool,
    pub last_output: u8,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Vrc1Snap {
    pub chr_ram_data: Vec<u8>,
    pub prg_banks: [u8; 3],
    pub chr_banks: [u8; 2],
    pub mirroring: MirroringSnap,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Vrc24Snap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub microwire_latch: u8,
    pub prg_reg_0: u8,
    pub prg_reg_1: u8,
    pub prg_mode: u8,
    pub chr_lo: [u8; 8],
    pub chr_hi: [u8; 8],
    pub mirroring: MirroringSnap,
    pub irq: VrcIrqSnap,
    pub save_dirty: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Vrc3Snap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub mirroring: MirroringSnap,
    pub prg_bank: u8,
    pub irq_latch: u16,
    pub irq_counter: u16,
    pub irq_enabled: bool,
    pub irq_enable_on_ack: bool,
    pub small_counter: bool,
    pub irq_line: bool,
    pub save_dirty: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Vrc6Snap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub prg_8000_16k: u8,
    pub prg_c000_8k: u8,
    pub chr_regs: [u8; 8],
    pub banking_mode: u8,
    pub mirroring: MirroringSnap,
    pub irq: VrcIrqSnap,
    pub audio: Vrc6AudioSnap,
    pub save_dirty: bool,
}

/// VRC7. The OPLL itself (emu2413 C state) is captured via a
/// 64-byte register-file shadow that the live `Vrc7` maintains
/// alongside writes - see [`crate::nes::mapper::vrc7::Vrc7::save_state_capture`]
/// for the replay-on-apply contract.
#[derive(Debug, Serialize, Deserialize)]
pub struct Vrc7Snap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub prg_banks: [u8; 3],
    pub chr_banks: [u8; 8],
    pub mirroring: MirroringSnap,
    pub prg_ram_enable: bool,
    pub audio_muted: bool,
    pub irq: VrcIrqSnap,
    pub opll_pending_reg: u8,
    /// Shadow of the 64 OPLL register slots. On apply we drive
    /// these back through `Opll::write` to reinstate phase / patch
    /// state without saving emu2413's internal C struct.
    #[serde(with = "BigArray")]
    pub opll_regs: [u8; 64],
    pub last_sample: i16,
    pub clock_acc: u32,
    pub save_dirty: bool,
}

impl Default for Vrc7Snap {
    fn default() -> Self {
        Self {
            prg_ram: Vec::new(),
            chr_ram_data: Vec::new(),
            prg_banks: [0; 3],
            chr_banks: [0; 8],
            mirroring: MirroringSnap::default(),
            prg_ram_enable: false,
            audio_muted: false,
            irq: VrcIrqSnap::default(),
            opll_pending_reg: 0,
            opll_regs: [0; 64],
            last_sample: 0,
            clock_acc: 0,
            save_dirty: false,
        }
    }
}

// ---- Eeprom24C0X (Bandai FCG) ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum EepromChipSnap {
    #[default]
    C24C01 = 0,
    C24C02 = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum EepromModeSnap {
    #[default]
    Idle = 0,
    ChipAddress = 1,
    Address = 2,
    Read = 3,
    Write = 4,
    SendAck = 5,
    WaitAck = 6,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct EepromSnap {
    pub chip: EepromChipSnap,
    pub bytes: Vec<u8>,
    pub mode: EepromModeSnap,
    pub next_mode: EepromModeSnap,
    pub chip_address: u8,
    pub address: u8,
    pub data: u8,
    pub counter: u8,
    pub output: u8,
    pub prev_scl: u8,
    pub prev_sda: u8,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BandaiFcgSnap {
    pub chr_ram_data: Vec<u8>,
    pub mirroring: MirroringSnap,
    pub prg_bank: u8,
    pub chr_regs: [u8; 8],
    pub irq_enabled: bool,
    pub irq_counter: u16,
    pub irq_reload: u16,
    pub irq_line: bool,
    pub eeprom: Option<EepromSnap>,
    pub save_dirty: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct JalecoSnap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub mirroring: MirroringSnap,
    pub prg_banks: [u8; 3],
    pub chr_banks: [u8; 8],
    pub irq_reload: [u8; 4],
    pub irq_counter: u16,
    pub irq_counter_size: u8,
    pub irq_enabled: bool,
    pub irq_line: bool,
    pub save_dirty: bool,
}

// ---- Sunsoft 5B audio (FME-7) ----

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct Sunsoft5bEnvelopeSnap {
    pub period: u16,
    pub count: u8,
    pub attack: u8,
    pub alternate: bool,
    pub hold: bool,
    pub holding: bool,
    pub sub_cycle: u8,
    pub timer: i32,
    pub output: u8,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct Sunsoft5bNoiseSnap {
    pub period: u8,
    pub sub_cycle: u8,
    pub timer: i32,
    pub lfsr: u32,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct Sunsoft5bAudioSnap {
    pub volume_lut: [u8; 16],
    pub envelope_lut: [u8; 32],
    pub current_register: u8,
    pub write_disabled: bool,
    pub registers: [u8; 16],
    pub timer: [i32; 3],
    pub tone_step: [u8; 3],
    pub process_tick: bool,
    pub envelope: Sunsoft5bEnvelopeSnap,
    pub noise: Sunsoft5bNoiseSnap,
    pub last_output: i16,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Fme7Snap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub command: u8,
    pub work_ram_value: u8,
    pub prg_banks: [u8; 3],
    pub chr_banks: [u8; 8],
    pub mirroring: MirroringSnap,
    pub irq_counter: u16,
    pub irq_enabled: bool,
    pub irq_counter_enabled: bool,
    pub irq_line: bool,
    pub audio: Sunsoft5bAudioSnap,
    pub save_dirty: bool,
}

// ---- N163 audio (Namco 163) ----

#[derive(Debug, Serialize, Deserialize)]
pub struct N163AudioSnap {
    #[serde(with = "BigArray")]
    pub ram: [u8; 0x80],
    pub channel_output: [i16; 8],
    pub update_counter: u8,
    pub current_channel: i8,
    pub last_output: i16,
    pub disable_sound: bool,
    pub address: u8,
    pub auto_inc: bool,
}

impl Default for N163AudioSnap {
    fn default() -> Self {
        Self {
            ram: [0; 0x80],
            channel_output: [0; 8],
            update_counter: 0,
            current_channel: 0,
            last_output: 0,
            disable_sound: false,
            address: 0,
            auto_inc: false,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Namco163Snap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub audio: N163AudioSnap,
    pub not_namco340: bool,
    pub prg_banks: [u8; 3],
    pub chr_banks: [u8; 12],
    pub low_chr_nt_mode: bool,
    pub high_chr_nt_mode: bool,
    pub write_protect: u8,
    pub irq_counter: u16,
    pub irq_line: bool,
    pub mirroring: MirroringSnap,
    pub save_dirty: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Rambo1Snap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub bank_regs: [u8; 16],
    pub bank_select: u8,
    pub mirroring: MirroringSnap,
    pub irq_latch: u8,
    pub irq_counter: u8,
    pub irq_reload_pending: bool,
    pub irq_enabled: bool,
    pub irq_cycle_mode: bool,
    pub irq_pending_delay: u8,
    pub irq_line: bool,
    pub cpu_prescaler: u8,
    pub force_clock: bool,
    pub a12_low_since: Option<u64>,
    pub save_dirty: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct IremG101Snap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub mirroring: MirroringSnap,
    pub prg_mode: u8,
    pub prg_reg0: u8,
    pub prg_reg1: u8,
    pub chr_regs: [u8; 8],
    pub save_dirty: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TaitoTc0190Snap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub mirroring: MirroringSnap,
    pub prg_reg0: u8,
    pub prg_reg1: u8,
    pub chr_2k: [u8; 2],
    pub chr_1k: [u8; 4],
    pub save_dirty: bool,
}

/// Mapper 037 wraps an MMC3 with an outer-block latch. We carry
/// the full inner MMC3 snapshot plus the 3-bit `block` latch.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Mapper037Snap {
    pub inner: Mmc3Snap,
    pub block: u8,
}

/// TxSROM (mapper 118) wraps an MMC3 with per-NT-slot CIRAM
/// routing latched at `$8001` write time. The 4-byte `nt_cache`
/// is the only state TxSROM adds on top of the inner MMC3.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TxsromSnap {
    pub inner: Mmc3Snap,
    pub nt_cache: [u8; 4],
}

/// TQROM (mapper 119) wraps an MMC3 with 8 KiB of on-cart
/// CHR-RAM. Each CHR bank value's bits 6/7 select ROM vs RAM
/// per slot at PPU read time. The full RAM buffer is part of
/// the snapshot so writes (Mall Madness map updates, pinball
/// ramp-lit animations) survive a round trip.
#[derive(Debug, Serialize, Deserialize)]
pub struct TqromSnap {
    pub inner: Mmc3Snap,
    #[serde(with = "BigArray")]
    pub chr_ram: [u8; 0x2000],
}

impl Default for TqromSnap {
    fn default() -> Self {
        Self {
            inner: Mmc3Snap::default(),
            chr_ram: [0; 0x2000],
        }
    }
}

/// Taito TC0690 (mapper 48) wraps an MMC3 with a translated
/// register surface and a CPU-cycle delay on IRQ assertion.
/// Submapper 0 (Flintstones / Captain Saver / default) uses
/// a 22-cycle delay; submapper 1 (The Jetsons) uses a 6-cycle
/// delay AND adds `+1` to the inverted reload value.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Tc0690Snap {
    pub inner: Mmc3Snap,
    pub submapper: u8,
    pub irq_delay: u8,
    pub delayed_irq_line: bool,
    pub prev_inner_irq: bool,
}

/// Jaleco JF-17 / JF-19 (mappers 72 + 92). Captures the live PRG
/// and CHR bank values, the prev-write rising-edge gates, the
/// PCB wiring (`switchable_high` distinguishes mapper 92 from
/// mapper 72), and any CHR-RAM bytes for the rare CHR-RAM build.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct JalecoJf17Snap {
    pub chr_ram_data: Vec<u8>,
    pub prg_bank: u8,
    pub chr_bank: u8,
    pub prev_prg_gate: bool,
    pub prev_chr_gate: bool,
    /// `false` for JF-17 (mapper 72), `true` for JF-19 (mapper 92).
    pub switchable_high: bool,
}

/// Sunsoft-2 (mapper 89). One register at $8000-$FFFF carrying
/// PRG bank, single-screen mirroring, and CHR bank. Bus-conflict
/// AND is applied at write time, so the latched value is what we
/// store here. CHR-RAM bytes captured for completeness (no known
/// retail cart uses CHR-RAM on this mapper).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Sunsoft2Snap {
    pub chr_ram_data: Vec<u8>,
    pub reg: u8,
    pub mirroring: MirroringSnap,
}

/// Sunsoft-1 (mapper 184). One CHR-banking register at
/// `$6000-$7FFF` plus optional CHR-RAM bytes (none of the known
/// retail carts use CHR-RAM, but we capture if present for
/// homebrew completeness).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Sunsoft1Snap {
    pub chr_ram_data: Vec<u8>,
    pub reg: u8,
}

/// Bandai 74*161/161/32 (mappers 70 + 152). Captures the
/// single 8-bit latch, the auto-promoted mirroring-control flag
/// (mapper 70 only), the live mirroring, and any CHR-RAM bytes
/// (mapper 70 carts that ship without CHR-ROM).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Bandai74161Snap {
    pub chr_ram_data: Vec<u8>,
    pub reg: u8,
    pub mirroring_control: bool,
    pub mirroring: MirroringSnap,
}

/// Irem H3001 (mapper 65). Captures PRG-RAM, optional CHR-RAM,
/// PRG/CHR bank registers, mirroring, and the live IRQ
/// down-counter state (latch + counter + enable + line).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct IremH3001Snap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub prg_regs: [u8; 3],
    pub chr_regs: [u8; 8],
    pub mirroring: MirroringSnap,
    pub irq_enabled: bool,
    pub irq_counter: u16,
    pub irq_latch: u16,
    pub irq_line: bool,
    pub save_dirty: bool,
}

/// Taito X1-005 (mappers 80 + 207). Captures the chip's
/// 128-byte on-cart WRAM (battery-backed on save-bearing
/// carts), the bank-register file at `$7EF0-$7EFF`, the
/// `$A3` permission latch that gates WRAM access, the
/// effective mirroring (mapper 80) plus the per-NT-slot
/// CIRAM cache (mapper 207's bit-7-driven routing), and the
/// variant flag so a cross-mapper apply (80 -> 207) is
/// rejected even if the file header check missed.
#[derive(Debug, Serialize, Deserialize)]
pub struct TaitoX1005Snap {
    pub alternate_mirroring: bool,
    #[serde(with = "BigArray")]
    pub wram: [u8; 128],
    pub chr_ram_data: Vec<u8>,
    pub chr_2k_regs: [u8; 2],
    pub chr_1k_regs: [u8; 4],
    pub prg_regs: [u8; 3],
    pub mirroring: MirroringSnap,
    pub nt_cache: [u8; 4],
    pub ram_permission: u8,
    pub save_dirty: bool,
}

impl Default for TaitoX1005Snap {
    fn default() -> Self {
        Self {
            alternate_mirroring: false,
            wram: [0; 128],
            chr_ram_data: Vec::new(),
            chr_2k_regs: [0; 2],
            chr_1k_regs: [0; 4],
            prg_regs: [0; 3],
            mirroring: MirroringSnap::default(),
            nt_cache: [0; 4],
            ram_permission: 0,
            save_dirty: false,
        }
    }
}

/// Taito X1-017 (mapper 82). Captures the chip's 5 KiB
/// battery-backed WRAM (five 1 KiB banks gated by three
/// permission latches), the CHR bank file, the CHR mode swap
/// bit, and the shifted PRG bank values.
#[derive(Debug, Serialize, Deserialize)]
pub struct TaitoX1017Snap {
    #[serde(with = "BigArray")]
    pub wram: [u8; 5 * 1024],
    pub chr_ram_data: Vec<u8>,
    pub chr_regs: [u8; 6],
    pub chr_mode: u8,
    pub ram_permission: [u8; 3],
    pub prg_regs: [u8; 3],
    pub mirroring: MirroringSnap,
    pub save_dirty: bool,
}

impl Default for TaitoX1017Snap {
    fn default() -> Self {
        Self {
            wram: [0; 5 * 1024],
            chr_ram_data: Vec::new(),
            chr_regs: [0; 6],
            chr_mode: 0,
            ram_permission: [0; 3],
            prg_regs: [0; 3],
            mirroring: MirroringSnap::default(),
            save_dirty: false,
        }
    }
}

/// Namco 118 family variant tag. Mirrors the live
/// [`crate::nes::mapper::namco_118::Variant`] so the on-disk schema
/// is decoupled from the live struct's enum layout. Used to
/// reject cross-variant `apply` (the file header's mapper-id
/// check is the primary guard, this is belt-and-suspenders).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum Namco118VariantSnap {
    #[default]
    Mapper206 = 0,
    Mapper88 = 1,
    Mapper95 = 2,
    Mapper154 = 3,
}

/// Namco 118 family (mappers 88 / 95 / 154 / 206). Captures the
/// 8 bank registers, the bank-select latch, current mirroring
/// (used directly by mapper 154's dynamic single-screen toggle
/// and as a placeholder for mapper 95's per-slot override),
/// PRG-RAM, and CHR-RAM (when applicable).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Namco118Snap {
    pub variant: Namco118VariantSnap,
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub bank_regs: [u8; 8],
    pub bank_select: u8,
    pub mirroring: MirroringSnap,
    pub save_dirty: bool,
}

/// Sunsoft-3 (mapper 67). Tracks 4× 2 KiB CHR banks, 16 KiB PRG
/// bank, mirroring, and the 16-bit IRQ counter (with its
/// two-write-toggle latch) used by *Fantasy Zone II*.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Sunsoft3Snap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub prg_bank: u8,
    pub chr_banks: [u8; 4],
    pub mirroring: MirroringSnap,
    pub irq_toggle: bool,
    pub irq_counter: u16,
    pub irq_enabled: bool,
    pub irq_line: bool,
    pub save_dirty: bool,
}

/// Sunsoft-4 (mapper 68). Tracks 4x 2 KiB CHR banks plus the 2
/// nametable-replacement registers, the NTRAM enable bit, the
/// PRG bank + RAM-enable gate, and the Sunsoft-Maeda licensing
/// chip's keep-alive timer / external-bank-select state used by
/// submapper-1 carts (Sugoro Quest et al.). Standard 128-KiB
/// carts (After Burner II, Maharaja, Ripple Island) leave the
/// licensing fields at zero / false throughout the run.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Sunsoft4Snap {
    pub prg_ram: Vec<u8>,
    pub chr_ram_data: Vec<u8>,
    pub prg_bank: u8,
    pub prg_ram_enabled: bool,
    pub chr_banks: [u8; 4],
    pub nt_regs: [u8; 2],
    pub use_chr_for_nametables: bool,
    pub mirroring: MirroringSnap,
    pub licensing_timer: u32,
    pub using_external_rom: bool,
    pub external_page: u8,
    pub save_dirty: bool,
}

// ---- FDS audio + disk + IRQ ----

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct FdsEnvelopeUnitSnap {
    pub speed: u8,
    pub gain: u8,
    pub envelope_off: bool,
    pub volume_increase: bool,
    pub frequency: u16,
    pub timer: u32,
    pub master_speed: u8,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FdsModChannelSnap {
    pub env: FdsEnvelopeUnitSnap,
    pub counter: i8,
    pub modulation_disabled: bool,
    #[serde(with = "BigArray")]
    pub mod_table: [u8; 64],
    pub mod_table_position: u8,
    pub overflow_counter: u16,
    pub output: i32,
}

impl Default for FdsModChannelSnap {
    fn default() -> Self {
        Self {
            env: FdsEnvelopeUnitSnap::default(),
            counter: 0,
            modulation_disabled: false,
            mod_table: [0; 64],
            mod_table_position: 0,
            overflow_counter: 0,
            output: 0,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FdsAudioSnap {
    #[serde(with = "BigArray")]
    pub wave_table: [u8; 64],
    pub wave_write_enabled: bool,
    pub volume: FdsEnvelopeUnitSnap,
    pub modulator: FdsModChannelSnap,
    pub disable_envelopes: bool,
    pub halt_waveform: bool,
    pub master_volume: u8,
    pub wave_overflow_counter: u16,
    pub wave_position: u8,
    pub last_output: u8,
}

impl Default for FdsAudioSnap {
    fn default() -> Self {
        Self {
            wave_table: [0; 64],
            wave_write_enabled: false,
            volume: FdsEnvelopeUnitSnap::default(),
            modulator: FdsModChannelSnap::default(),
            disable_envelopes: false,
            halt_waveform: false,
            master_volume: 0,
            wave_overflow_counter: 0,
            wave_position: 0,
            last_output: 0,
        }
    }
}

/// FDS snapshot. Big - includes the full per-side disk image
/// (`disk_sides`) so writes the player has done to the disk are
/// preserved on round-trip.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FdsSnap {
    pub prg_ram: Vec<u8>,
    pub chr_ram: Vec<u8>,
    /// Currently-loaded disk-side payload. Each element is one
    /// gapped scan-ready side; `disk_sides.len()` matches the live
    /// cart's side count and is validated on apply.
    pub disk_sides: Vec<Vec<u8>>,
    pub disk_number: u32,
    pub disk_position: u32,
    pub delay: u32,
    pub crc_accumulator: u16,
    pub previous_crc_control: bool,
    pub gap_ended: bool,
    pub scanning_disk: bool,
    pub transfer_complete: bool,
    pub end_of_head: bool,
    pub read_data_reg: u8,
    pub write_data_reg: u8,
    pub bad_crc: bool,
    pub irq_reload_value: u16,
    pub irq_counter: u16,
    pub irq_enabled: bool,
    pub irq_repeat_enabled: bool,
    pub disk_reg_enabled: bool,
    pub sound_reg_enabled: bool,
    pub motor_on: bool,
    pub reset_transfer: bool,
    pub read_mode: bool,
    pub mirroring: MirroringSnap,
    pub crc_control: bool,
    pub disk_ready: bool,
    pub disk_irq_enabled: bool,
    pub timer_irq_line: bool,
    pub disk_irq_line: bool,
    pub audio: FdsAudioSnap,
    pub ext_con_reg: u8,
    pub pending_insert_side: Option<u8>,
    pub pending_insert_cycles: u32,
    pub save_dirty: bool,
}

/// Tagged union of supported mapper snapshots.
///
/// One variant per implemented mapper module. Adding a new mapper
/// to Phase 3 is a four-step change: add the `*Snap` struct above,
/// add the variant here, override `Mapper::save_state_capture` /
/// `save_state_apply` on the live mapper, and bump
/// [`crate::save_state::FORMAT_VERSION`].
///
/// `Unsupported(u16)` is the fallback for mappers Phase 3a hasn't
/// covered yet. Carrying the live `mapper_id` lets the error path
/// surface a useful message ("save states for mapper 19 not yet
/// supported") without stuffing it into the error type.
#[derive(Debug, Serialize, Deserialize)]
pub enum MapperState {
    Nrom(NromSnap),
    Uxrom(UxromSnap),
    Cnrom(CnromSnap),
    Axrom(AxromSnap),
    Gxrom(GxromSnap),
    Mmc1(Mmc1Snap),
    Mmc2(Mmc2Snap),
    Mmc3(Mmc3Snap),
    Mmc4(Mmc4Snap),
    Mmc5(Box<Mmc5Snap>),
    Vrc1(Vrc1Snap),
    Vrc24(Box<Vrc24Snap>),
    Vrc3(Vrc3Snap),
    Vrc6(Box<Vrc6Snap>),
    Vrc7(Box<Vrc7Snap>),
    Fme7(Box<Fme7Snap>),
    BandaiFcg(Box<BandaiFcgSnap>),
    Jaleco(Box<JalecoSnap>),
    JalecoJf17(JalecoJf17Snap),
    Namco163(Box<Namco163Snap>),
    Rambo1(Box<Rambo1Snap>),
    Bandai74161(Bandai74161Snap),
    IremG101(IremG101Snap),
    IremH3001(Box<IremH3001Snap>),
    TaitoTc0190(TaitoTc0190Snap),
    Mapper037(Box<Mapper037Snap>),
    Fds(Box<FdsSnap>),
    Sunsoft1(Sunsoft1Snap),
    Sunsoft2(Sunsoft2Snap),
    Sunsoft3(Sunsoft3Snap),
    Sunsoft4(Sunsoft4Snap),
    Namco118(Namco118Snap),
    Txsrom(Box<TxsromSnap>),
    Tqrom(Box<TqromSnap>),
    Tc0690(Box<Tc0690Snap>),
    TaitoX1005(Box<TaitoX1005Snap>),
    TaitoX1017(Box<TaitoX1017Snap>),
    /// Mapper variant not covered by any phase yet. Carries the
    /// live mapper id from [`crate::nes::bus::Bus::mapper_id`]
    /// for error messaging.
    Unsupported(u16),
}

impl Default for MapperState {
    fn default() -> Self {
        // Default to NROM with empty data - matches the structural
        // shape of an iNES mapper-0 cart with no PRG-RAM.
        Self::Nrom(NromSnap::default())
    }
}
