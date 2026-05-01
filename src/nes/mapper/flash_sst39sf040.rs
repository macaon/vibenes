// SPDX-License-Identifier: GPL-3.0-or-later
//! SST39SF040 flash chip emulation.
//!
//! Microchip / SST 4 Mbit (512 KiB) parallel NOR flash, used as
//! the PRG store on the battery-backed UNROM-512 board (mapper 30
//! submapper 1) and the GTROM / Cheapocabra board (mapper 111).
//! This module models the chip's command-sequencer state machine,
//! the software-ID readback path, single-byte programming, and
//! both sector + chip erase. It operates on a borrowed `&mut [u8]`
//! that the caller (the mapper) hands in - matching Mesen2's
//! `FlashSST39SF040` design which holds a raw `_data` pointer into
//! the live PRG-ROM buffer.
//!
//! ## Command sequencer
//!
//! The chip enters command mode through a fixed three-write
//! unlock pattern, with addresses interpreted in the chip's
//! 32 KiB physical-address window (mask `0x7FFF`). The first two
//! writes are the same for every operation; the third selects the
//! command:
//!
//! ```text
//! Cycle 0: $5555 = $AA
//! Cycle 1: $2AAA = $55
//! Cycle 2: $5555 = <cmd>
//!     $80 -> enter Erase mode (3 more cycles required)
//!     $90 -> Software ID on  (subsequent reads return chip ID)
//!     $A0 -> Single-byte program
//!     $F0 -> Software ID off
//! ```
//!
//! In `Write` mode the next write programs `data[addr] &= value`
//! into the chip - the AND is the actual flash semantic: a NOR
//! flash cell can only flip a `1` to a `0`, never the reverse.
//! The only way to restore a `1` is sector or chip erase, which
//! resets the affected region to `$FF`.
//!
//! In `Erase` mode the unlock pattern repeats (cycles 3-4 mirror
//! 0-1), then cycle 5 selects:
//! - `$5555 = $10` -> chip erase (entire chip back to `$FF`)
//! - `<sector_base> = $30` -> 4 KiB sector erase at
//!   `addr & 0x7F000` (the SST39SF040 sector size).
//!
//! Software-ID mode is the chip's identification protocol: while
//! active, reads of `addr & 0x1FF == 0x00` return the manufacturer
//! ID (`$BF`, SST) and `0x01` return the device ID (`$B7`,
//! SST39SF040). Other reads return `$FF`. Software ID exits via
//! either the cycle-2 `$F0` command or any standalone `$F0` write.
//!
//! ## Save persistence
//!
//! Programming and erase mutate the live PRG-ROM buffer in place.
//! The mapper holds a clone of the pristine PRG-ROM and exposes
//! the diff as an IPS patch via the [`Mapper::flash_save_data`]
//! channel - same format and codepath used by the FDS disk save
//! pipeline.
//!
//! ## Save state
//!
//! Only the chip's transient state is captured here
//! (`mode`/`cycle`/`software_id`); the mutated PRG bytes live in
//! the surrounding mapper's snapshot.
//!
//! Clean-room references (behavioral only):
//! - `~/Git/Mesen2/Core/NES/Mappers/Homebrew/FlashSST39SF040.h`
//! - `~/Git/punes/src/core/SST39SF040.{c,h}`
//! - SST datasheet: SST39SF010A/SF020A/SF040 (Rev. M, June 2014)

use serde::{Deserialize, Serialize};

/// Mesen-named manufacturer ID returned in software-ID mode at
/// `addr & 0x1FF == 0x00`. SST is `$BF`.
const MANUFACTURER_ID: u8 = 0xBF;
/// Device ID returned at `addr & 0x1FF == 0x01`. SST39SF040
/// (4 Mbit) is `$B7`.
const DEVICE_ID: u8 = 0xB7;
/// Sector erase size: 4 KiB on the SST39SF040.
const SECTOR_SIZE: usize = 0x1000;
/// Address mask for the 32 KiB chip-command window.
const CMD_ADDR_MASK: u32 = 0x7FFF;
/// First unlock address.
const UNLOCK_ADDR_A: u32 = 0x5555;
/// Second unlock address.
const UNLOCK_ADDR_B: u32 = 0x2AAA;
/// Sector base mask: `addr & 0x7F000` selects the 4 KiB sector
/// containing `addr` within the 512 KiB chip.
const SECTOR_BASE_MASK: u32 = 0x7F000;

/// Chip top-level state. Inactive carts (every reset condition)
/// sit in `WaitingForCommand` with `cycle = 0`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ChipMode {
    /// Idle. The next write is interpreted as the start of an
    /// unlock sequence.
    #[default]
    WaitingForCommand = 0,
    /// Single-byte program armed. The next write is treated as
    /// data, programmed to `data[addr] &= value`, and the chip
    /// returns to [`ChipMode::WaitingForCommand`].
    Write = 1,
    /// Erase command armed. The chip needs three more writes
    /// (cycles 3-5) to actually erase a sector or the whole chip.
    Erase = 2,
}

/// SST39SF040 command-sequencer state. Owned independently of the
/// PRG-ROM bytes it operates on; the mapper passes a `&mut [u8]`
/// slice into [`FlashSst39sf040::write`] for every program / erase.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlashSst39sf040 {
    pub mode: ChipMode,
    pub cycle: u8,
    pub software_id: bool,
}

impl FlashSst39sf040 {
    /// Fresh chip in idle state. Equivalent to a power-on cycle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Read intercept. Returns `Some(byte)` while software-ID mode
    /// is active so the cart can identify the chip; otherwise
    /// `None` to signal the mapper should serve the address from
    /// regular PRG-ROM.
    pub fn read(&self, addr: u32) -> Option<u8> {
        if !self.software_id {
            return None;
        }
        match addr & 0x1FF {
            0x00 => Some(MANUFACTURER_ID),
            0x01 => Some(DEVICE_ID),
            _ => Some(0xFF),
        }
    }

    /// Reset the command sequencer to idle. Software-ID is
    /// untouched here (per Mesen) - explicit `$F0` writes are the
    /// only path out of software-ID mode.
    pub fn reset_state(&mut self) {
        self.mode = ChipMode::WaitingForCommand;
        self.cycle = 0;
    }

    /// Write to the chip at the given physical address. `data` is
    /// the live PRG-ROM buffer (mutated in place by Program and
    /// Erase commands). `addr` is the chip-physical address - the
    /// caller (mapper) is responsible for translating CPU bus
    /// addresses through the current bank-select.
    ///
    /// Returns `true` if any byte in `data` was actually changed
    /// (so the mapper can mark its flash-save dirty without
    /// snapshotting the full PRG buffer on every write).
    pub fn write(&mut self, addr: u32, value: u8, data: &mut [u8]) -> bool {
        let cmd = addr & CMD_ADDR_MASK;
        let size = data.len() as u32;
        match self.mode {
            ChipMode::WaitingForCommand => {
                self.step_command(cmd, value);
                false
            }
            ChipMode::Write => {
                let mut mutated = false;
                if addr < size {
                    let i = addr as usize;
                    let new = data[i] & value;
                    if new != data[i] {
                        data[i] = new;
                        mutated = true;
                    }
                }
                self.reset_state();
                mutated
            }
            ChipMode::Erase => self.step_erase(addr, cmd, value, data, size),
        }
    }

    /// Cycle 0-2: build up to a command. The `$F0` short-circuit
    /// at cycle 0 mirrors the datasheet's "single-cycle reset" -
    /// any standalone `$F0` write turns software-ID off and resets
    /// the sequencer.
    fn step_command(&mut self, cmd: u32, value: u8) {
        match self.cycle {
            0 => {
                if cmd == UNLOCK_ADDR_A && value == 0xAA {
                    self.cycle = 1;
                } else if value == 0xF0 {
                    self.reset_state();
                    self.software_id = false;
                }
            }
            1 if cmd == UNLOCK_ADDR_B && value == 0x55 => {
                self.cycle = 2;
            }
            2 if cmd == UNLOCK_ADDR_A => {
                self.cycle = 3;
                match value {
                    0x80 => self.mode = ChipMode::Erase,
                    0x90 => {
                        self.reset_state();
                        self.software_id = true;
                    }
                    0xA0 => self.mode = ChipMode::Write,
                    0xF0 => {
                        self.reset_state();
                        self.software_id = false;
                    }
                    _ => {}
                }
            }
            _ => self.cycle = 0,
        }
    }

    /// Cycle 3-5: complete an erase command. Cycles 3-4 are the
    /// unlock pattern (`$5555/$AA`, `$2AAA/$55`); cycle 5 selects
    /// either chip erase (`$5555/$10`) or sector erase
    /// (`<sector_base>/$30`) at the 4 KiB sector containing `addr`.
    /// Any deviation aborts back to idle. Returns `true` when a
    /// non-empty erase actually changes any byte.
    fn step_erase(
        &mut self,
        addr: u32,
        cmd: u32,
        value: u8,
        data: &mut [u8],
        size: u32,
    ) -> bool {
        match self.cycle {
            3 => {
                if cmd == UNLOCK_ADDR_A && value == 0xAA {
                    self.cycle = 4;
                } else {
                    self.reset_state();
                }
                false
            }
            4 => {
                if cmd == UNLOCK_ADDR_B && value == 0x55 {
                    self.cycle = 5;
                } else {
                    self.reset_state();
                }
                false
            }
            5 => {
                let mut mutated = false;
                if cmd == UNLOCK_ADDR_A && value == 0x10 {
                    for slot in data.iter_mut() {
                        if *slot != 0xFF {
                            *slot = 0xFF;
                            mutated = true;
                        }
                    }
                } else if value == 0x30 {
                    let offset = addr & SECTOR_BASE_MASK;
                    if offset.saturating_add(SECTOR_SIZE as u32) <= size {
                        let start = offset as usize;
                        for slot in &mut data[start..start + SECTOR_SIZE] {
                            if *slot != 0xFF {
                                *slot = 0xFF;
                                mutated = true;
                            }
                        }
                    }
                }
                self.reset_state();
                mutated
            }
            _ => {
                self.reset_state();
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16 KiB scratch buffer to mutate. Matches "small enough to
    /// be cheap, large enough to span sector + chip erase".
    fn buf() -> Vec<u8> {
        vec![0xFFu8; 16 * 1024]
    }

    /// Walk the chip through a 3-cycle unlock plus the data write
    /// for a single-byte program operation at `addr`.
    fn program(chip: &mut FlashSst39sf040, addr: u32, value: u8, data: &mut [u8]) {
        chip.write(0x5555, 0xAA, data);
        chip.write(0x2AAA, 0x55, data);
        chip.write(0x5555, 0xA0, data);
        chip.write(addr, value, data);
    }

    #[test]
    fn fresh_chip_is_idle_with_software_id_off() {
        let chip = FlashSst39sf040::new();
        assert_eq!(chip.mode, ChipMode::WaitingForCommand);
        assert_eq!(chip.cycle, 0);
        assert!(!chip.software_id);
        assert_eq!(chip.read(0x0000), None);
    }

    #[test]
    fn program_clears_bits_via_and() {
        let mut chip = FlashSst39sf040::new();
        let mut data = buf();
        // Pristine byte is 0xFF; program 0xC3 -> AND -> 0xC3.
        program(&mut chip, 0x0100, 0xC3, &mut data);
        assert_eq!(data[0x0100], 0xC3);
        // Re-program 0x81 over 0xC3 -> AND -> 0x81 (only flips
        // 1->0, never the reverse).
        program(&mut chip, 0x0100, 0x81, &mut data);
        assert_eq!(data[0x0100], 0x81);
        // Try to set bit 6 back to 1: AND with 0x40 -> 0x00.
        program(&mut chip, 0x0100, 0x40, &mut data);
        assert_eq!(data[0x0100], 0x00);
    }

    #[test]
    fn program_returns_to_idle() {
        let mut chip = FlashSst39sf040::new();
        let mut data = buf();
        program(&mut chip, 0x0100, 0xC3, &mut data);
        assert_eq!(chip.mode, ChipMode::WaitingForCommand);
        assert_eq!(chip.cycle, 0);
    }

    #[test]
    fn sector_erase_resets_4kib_block_to_ff() {
        let mut chip = FlashSst39sf040::new();
        let mut data = buf();
        // Dirty the whole sector at 0x1000-0x1FFF.
        for b in &mut data[0x1000..0x2000] {
            *b = 0x00;
        }
        // Erase command: 5555/AA, 2AAA/55, 5555/80, 5555/AA, 2AAA/55, sector/30.
        chip.write(0x5555, 0xAA, &mut data);
        chip.write(0x2AAA, 0x55, &mut data);
        chip.write(0x5555, 0x80, &mut data);
        chip.write(0x5555, 0xAA, &mut data);
        chip.write(0x2AAA, 0x55, &mut data);
        chip.write(0x1234, 0x30, &mut data);
        for (i, &b) in data[0x1000..0x2000].iter().enumerate() {
            assert_eq!(b, 0xFF, "byte 0x{:04X} not erased", 0x1000 + i);
        }
        // Adjacent sectors untouched.
        assert_eq!(data[0x0FFF], 0xFF);
        assert_eq!(data[0x2000], 0xFF);
    }

    #[test]
    fn chip_erase_resets_entire_buffer() {
        let mut chip = FlashSst39sf040::new();
        let mut data = vec![0x00u8; 8 * 1024];
        chip.write(0x5555, 0xAA, &mut data);
        chip.write(0x2AAA, 0x55, &mut data);
        chip.write(0x5555, 0x80, &mut data);
        chip.write(0x5555, 0xAA, &mut data);
        chip.write(0x2AAA, 0x55, &mut data);
        chip.write(0x5555, 0x10, &mut data);
        assert!(data.iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn software_id_returns_chip_identification() {
        let mut chip = FlashSst39sf040::new();
        let mut data = buf();
        chip.write(0x5555, 0xAA, &mut data);
        chip.write(0x2AAA, 0x55, &mut data);
        chip.write(0x5555, 0x90, &mut data);
        assert!(chip.software_id);
        assert_eq!(chip.read(0x0000), Some(MANUFACTURER_ID));
        assert_eq!(chip.read(0x0001), Some(DEVICE_ID));
        assert_eq!(chip.read(0x0002), Some(0xFF));
        // Mirrors at 0x200, 0x400, ... the same.
        assert_eq!(chip.read(0x0200), Some(MANUFACTURER_ID));
        assert_eq!(chip.read(0x0201), Some(DEVICE_ID));
    }

    #[test]
    fn software_id_exit_via_three_cycle_f0() {
        let mut chip = FlashSst39sf040::new();
        let mut data = buf();
        chip.write(0x5555, 0xAA, &mut data);
        chip.write(0x2AAA, 0x55, &mut data);
        chip.write(0x5555, 0x90, &mut data);
        assert!(chip.software_id);
        chip.write(0x5555, 0xAA, &mut data);
        chip.write(0x2AAA, 0x55, &mut data);
        chip.write(0x5555, 0xF0, &mut data);
        assert!(!chip.software_id);
        assert_eq!(chip.read(0x0000), None);
    }

    #[test]
    fn software_id_exit_via_single_f0_at_cycle_zero() {
        let mut chip = FlashSst39sf040::new();
        let mut data = buf();
        chip.write(0x5555, 0xAA, &mut data);
        chip.write(0x2AAA, 0x55, &mut data);
        chip.write(0x5555, 0x90, &mut data);
        assert!(chip.software_id);
        // Datasheet "single-cycle reset": $F0 at cycle 0 resets
        // the sequencer AND turns software-ID off in one shot.
        chip.write(0x0000, 0xF0, &mut data);
        assert!(!chip.software_id);
    }

    #[test]
    fn aborted_unlock_returns_to_cycle_zero() {
        let mut chip = FlashSst39sf040::new();
        let mut data = buf();
        chip.write(0x5555, 0xAA, &mut data);
        // Wrong second-cycle address: aborts.
        chip.write(0x1234, 0x55, &mut data);
        assert_eq!(chip.cycle, 0);
        assert_eq!(chip.mode, ChipMode::WaitingForCommand);
        // Wrong second-cycle value: aborts.
        chip.write(0x5555, 0xAA, &mut data);
        chip.write(0x2AAA, 0x42, &mut data);
        assert_eq!(chip.cycle, 0);
    }

    #[test]
    fn write_outside_buffer_does_not_panic() {
        let mut chip = FlashSst39sf040::new();
        let mut data = vec![0xFFu8; 0x100];
        // Program at addr beyond the buffer; should be silently
        // dropped (matches Mesen's `if(addr < _size)`).
        program(&mut chip, 0x10000, 0x00, &mut data);
        assert!(data.iter().all(|&b| b == 0xFF));
    }
}
