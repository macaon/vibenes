// SPDX-License-Identifier: GPL-3.0-or-later
//! Ricoh 2C02 PPU. Implements the register window at $2000–$2007, per-dot
//! background rendering, frame timing, and the NMI edge latch used by
//! the CPU. Sprite evaluation / pixel mux and sprite-0 hit land in 6B;
//! this file carries a coarse sprite-0 stub so games that gate on the
//! flag (SMB status-bar split) still progress past boot.
//!
//! Every PPU bus read calls `Mapper::on_ppu_addr(addr, ppu_cycle)` so
//! A12-sensitive mappers (MMC3 scanline IRQ, MMC2/MMC4 CHR latch) have
//! the stream of address-bus events they need. This is the hook that
//! lets MMC3 land as pure implementation in Phase 6D.
//!
//! References while writing (behavioral only, per `CLAUDE.md`):
//! - `~/.claude/skills/nes-expert/reference/ppu.md` §4 loopy registers,
//!   §6 attribute table quadrant math, §9 sprite evaluation (for 6B),
//!   §12 scanline timing.
//! - `~/.claude/skills/nes-expert/reference/mesen-notes.md` §14
//!   sprite-0 five-part predicate (for 6B).

use crate::nes::clock::Region;
use crate::nes::mapper::{Mapper, NametableSource, NametableWriteTarget, PpuFetchKind};
use crate::nes::rom::Mirroring;

pub const FRAME_WIDTH: usize = 256;
pub const FRAME_HEIGHT: usize = 240;

const DOTS_PER_SCANLINE: u16 = 341;
const VBLANK_SCANLINE: i16 = 241;

/// 64-entry NES master palette → sRGB. Widely-cited approximation of the
/// 2C02's NTSC output. Exact values vary by emulator; this is close to
/// the "classic" look shared by many modern emulators. Entries $0D, $1D,
/// $2D, $3D are black on hardware regardless of row.
const NES_PALETTE: [[u8; 3]; 64] = [
    [0x62, 0x62, 0x62], [0x00, 0x2E, 0x98], [0x0C, 0x11, 0xC2], [0x3B, 0x00, 0xC2],
    [0x65, 0x00, 0x9E], [0x7D, 0x00, 0x4E], [0x7D, 0x00, 0x00], [0x65, 0x1F, 0x00],
    [0x3B, 0x37, 0x00], [0x0C, 0x4B, 0x00], [0x00, 0x52, 0x00], [0x00, 0x4B, 0x28],
    [0x00, 0x37, 0x69], [0x00, 0x00, 0x00], [0x00, 0x00, 0x00], [0x00, 0x00, 0x00],
    [0xAB, 0xAB, 0xAB], [0x19, 0x65, 0xEC], [0x3D, 0x3F, 0xFF], [0x73, 0x20, 0xFF],
    [0xA6, 0x13, 0xDF], [0xC5, 0x14, 0x8C], [0xC5, 0x24, 0x2B], [0xA6, 0x47, 0x00],
    [0x73, 0x6B, 0x00], [0x3D, 0x86, 0x00], [0x19, 0x90, 0x22], [0x00, 0x88, 0x72],
    [0x00, 0x6D, 0xC5], [0x00, 0x00, 0x00], [0x00, 0x00, 0x00], [0x00, 0x00, 0x00],
    [0xFF, 0xFF, 0xFF], [0x67, 0xB6, 0xFF], [0x8B, 0x8F, 0xFF], [0xC1, 0x6F, 0xFF],
    [0xF4, 0x62, 0xFF], [0xFF, 0x63, 0xDA], [0xFF, 0x74, 0x79], [0xF4, 0x97, 0x15],
    [0xC1, 0xBA, 0x0F], [0x8B, 0xD5, 0x1E], [0x67, 0xE0, 0x6E], [0x56, 0xD8, 0xBE],
    [0x5B, 0xBC, 0xFF], [0x5A, 0x5A, 0x5A], [0x00, 0x00, 0x00], [0x00, 0x00, 0x00],
    [0xFF, 0xFF, 0xFF], [0xBD, 0xDC, 0xFF], [0xCC, 0xCC, 0xFF], [0xE3, 0xBC, 0xFF],
    [0xF8, 0xB6, 0xFF], [0xFF, 0xB6, 0xEA], [0xFF, 0xBD, 0xC2], [0xF8, 0xC9, 0x9A],
    [0xE3, 0xDB, 0x92], [0xCC, 0xEC, 0x97], [0xBD, 0xF2, 0xBB], [0xB3, 0xEE, 0xDF],
    [0xB5, 0xE0, 0xFF], [0xB8, 0xB8, 0xB8], [0x00, 0x00, 0x00], [0x00, 0x00, 0x00],
];

pub struct Ppu {
    region: Region,
    scanline: i16,
    dot: u16,
    frame: u64,
    odd_frame: bool,

    /// Monotonic PPU-dot counter, incremented once per [`Ppu::tick`].
    /// Fed to `Mapper::on_ppu_addr` so A12-sensitive mappers can time
    /// their rising-edge filter (MMC3 requires ≥10 PPU cycles of
    /// A12-low before a rise counts).
    master_ppu_cycle: u64,

    ctrl: u8,
    mask: u8,
    status: u8,
    oam_addr: u8,

    w_latch: bool,
    t: u16,
    v: u16,
    fine_x: u8,
    data_buffer: u8,

    /// Latched by the pre-render dot-339 tick on odd NTSC frames
    /// when rendering was enabled at entry, consumed at end-of-tick
    /// to skip dot 340 and wrap to (0, 0) of the next frame.
    /// Sampling at dot-339 entry rather than at dot-340 exit gives
    /// the same visibility window as Mesen2's cycle-339 branch
    /// (NesPpu.cpp:946-954).
    skip_last_dot_latched: bool,
    /// Delayed view of `(mask & 0x18) != 0` - mirrors Mesen2's
    /// `_renderingEnabled` (NesPpu.cpp:1458-1461). A `$2001` write
    /// updates `mask` immediately but the effective "rendering
    /// enabled" signal lags by one PPU cycle, because Mesen2 applies
    /// the update via `UpdateState` at the END of the CURRENT tick.
    /// Required by `ppu_vbl_nmi/10-even_odd_timing` #3/#5 where a
    /// `$2001` write landing exactly at pre-render dot 339 must NOT
    /// influence that same dot's skip decision.
    rendering_enabled: bool,

    /// Live NMI level signal. Mirrors Mesen2's `NmiFlag` on the CPU
    /// (see `NesPpu.cpp:547-549, 588, 891, 1257`). Set by the
    /// `(nmiScanline, 1)` VBlank-start tick when `ctrl & 0x80 != 0`,
    /// by a `$2000` write with bit 7 set while VBlank is already set.
    /// Cleared by a `$2000` write with bit 7 clear, by a `$2002` read
    /// (together with clearing the VBlank status bit), and by the
    /// pre-render VBlank-clear tick. Rising-edge detection lives on
    /// the bus in `tick_post_access`, where a false→true transition
    /// across CPU-cycle boundaries latches `bus.nmi_pending`.
    nmi_flag: bool,
    /// Armed when a `$2002` read observes the PPU one PPU-dot before
    /// the VBlank flag would be set (scanline 241, dot 0 or 1 with VBL
    /// not yet set this frame). The upcoming VBlank-set tick sees this
    /// and skips both the status-flag set and the `vbl_just_set`
    /// marker - so both the current read AND any follow-up read in the
    /// same frame observe bit 7 = 0, and NMI never asserts. Matches
    /// Mesen2's `_preventVblFlag` (NesPpu.cpp:592, 1340). Required by
    /// `ppu_vbl_nmi/02-vbl_set_time.nes`.
    prevent_vbl: bool,

    oam: [u8; 256],
    palette: [u8; 32],
    vram: [u8; 0x800],

    // --- BG pipeline latches (filled by dot-3/5/7 fetches, consumed by
    //     the shifter reload at the start of the next 8-dot group).
    bg_next_nt: u8,
    bg_next_attr_bits: u8, // 2-bit palette selector pre-extracted at AT fetch
    bg_next_pat_lo: u8,
    bg_next_pat_hi: u8,

    // --- BG pipeline shifters. Pattern shifters are 16-bit; attribute
    //     shifters are 16-bit too, with the current tile's palette bit
    //     replicated across the low 8 bits on reload. Current pixel is
    //     at bit (15 - fine_x); we shift left by 1 every render dot.
    bg_pat_lo: u16,
    bg_pat_hi: u16,
    bg_attr_lo: u16,
    bg_attr_hi: u16,

    // --- Sprite pipeline.
    // Secondary OAM holds the 8 sprites selected for the next
    // scanline's rendering; populated by the per-dot evaluation
    // state machine at dots 1–256. The per-slot shifter arrays
    // (`sprite_pat_*`, `sprite_attr`, `sprite_x`, `sprite_is_zero`)
    // hold the data the mux consumes during dots 1–256 of the scanline
    // after the eval.
    secondary_oam: [u8; 32],
    sprite_count: u8,
    sprite_pat_lo: [u8; 8],
    sprite_pat_hi: [u8; 8],
    sprite_attr: [u8; 8],
    sprite_x: [u8; 8],
    sprite_is_zero: [bool; 8],
    // --- Sprite evaluation state machine (dots 1–256 of every
    // rendering scanline). Matches Mesen2 NesPpu.cpp:1004–1130 including
    // the 2C02 diagonal-sweep sprite overflow bug.
    // `oam_copy_buffer`: byte latched from primary OAM on odd cycles,
    // consumed on even cycles.
    oam_copy_buffer: u8,
    // `sec_oam_addr`: write cursor into `secondary_oam` (0..32). Reaches
    // 32 when 8 sprites have been copied, which flips the state machine
    // into its overflow-detection / bug-emulation branch.
    sec_oam_addr: u8,
    // `sprite_addr_h`: 6-bit primary OAM sprite index cursor (0..63).
    sprite_addr_h: u8,
    // `sprite_addr_l`: 2-bit byte-within-sprite cursor (0..3).
    sprite_addr_l: u8,
    // `oam_copy_done`: set when the state machine has walked all 64
    // primary OAM sprites for this scanline's evaluation.
    oam_copy_done: bool,
    // `sprite_in_range`: set when the sprite currently being evaluated
    // has its Y coordinate in-range for the next scanline; drives
    // whether the following 3 bytes get copied into secondary OAM.
    sprite_in_range: bool,
    // `sprite_zero_added`: set when the first sprite whose Y was
    // copied into secondary OAM at cycle 66 was primary OAM[0]. Feeds
    // `sprite_is_zero[0]` at evaluation end.
    sprite_zero_added: bool,
    // `overflow_bug_counter`: 2-bit countdown that runs after the
    // sprite-overflow flag is latched, emulating the "realignment"
    // quirk in the 2C02 state machine before it stops scanning.
    overflow_bug_counter: u8,

    pub frame_buffer: Vec<u8>,
    /// PPU I/O open bus - the "decay register" (nesdev). CPU reads of
    /// unimplemented / write-only register bits return the last value
    /// this bus was driven with. Per-bit decay timers in
    /// [`Ppu::open_bus_refresh`] track when each bit was last refreshed
    /// and zero the bit after ~600ms of not being driven.
    open_bus: u8,
    /// Timestamp (in master PPU dots) at which each bit of
    /// [`Ppu::open_bus`] was last refreshed. A bit is considered
    /// decayed once `master_ppu_cycle - open_bus_refresh[bit]` exceeds
    /// [`OPEN_BUS_DECAY_PPU_DOTS`], after which reads that consult
    /// that bit return 0 (the capacitor discharged).
    open_bus_refresh: [u64; 8],
}

/// PPU open-bus decay threshold in PPU dots. NTSC PPU runs at
/// ~5.369 MHz, so 3.2 M dots ≈ 596 ms - well inside the "< 1 second"
/// window asserted by `ppu_open_bus` tests 3/5/7/9 while still longer
/// than the 100 × 10 ms burst loops those tests use. Real hardware
/// varies per unit and with temperature (~600 ms typical per nesdev).
const OPEN_BUS_DECAY_PPU_DOTS: u64 = 3_200_000;

impl Ppu {
    pub fn new(region: Region) -> Self {
        Self {
            region,
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
            nmi_flag: false,
            skip_last_dot_latched: false,
            rendering_enabled: false,
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
            secondary_oam: [0xFF; 32],
            sprite_count: 0,
            sprite_pat_lo: [0; 8],
            sprite_pat_hi: [0; 8],
            sprite_attr: [0; 8],
            sprite_x: [0; 8],
            sprite_is_zero: [false; 8],
            oam_copy_buffer: 0xFF,
            sec_oam_addr: 0,
            sprite_addr_h: 0,
            sprite_addr_l: 0,
            oam_copy_done: false,
            sprite_in_range: false,
            sprite_zero_added: false,
            overflow_bug_counter: 0,
            frame_buffer: vec![0; FRAME_WIDTH * FRAME_HEIGHT * 4],
            open_bus: 0,
            open_bus_refresh: [0; 8],
        }
    }

    /// Refresh the selected bits of the open-bus decay register.
    /// For each bit of `mask` that's set, copy the corresponding bit
    /// of `value` into [`Ppu::open_bus`] and reset that bit's decay
    /// timer. Bits not in the mask keep both their stored value and
    /// their last-refresh timestamp - i.e. they keep decaying.
    fn refresh_open_bus_bits(&mut self, mask: u8, value: u8) {
        let now = self.master_ppu_cycle;
        self.open_bus = (self.open_bus & !mask) | (value & mask);
        for i in 0..8 {
            if (mask >> i) & 1 == 1 {
                self.open_bus_refresh[i] = now;
            }
        }
    }

    /// Return the open-bus byte with per-bit decay applied - any bit
    /// whose last refresh is older than [`OPEN_BUS_DECAY_PPU_DOTS`]
    /// reads back as 0. Used when a read needs to source bits from
    /// decay (e.g. low 5 bits of `$2002`) or to return pure decay for
    /// write-only registers.
    fn open_bus_decayed(&self) -> u8 {
        let now = self.master_ppu_cycle;
        let mut v = self.open_bus;
        for i in 0..8 {
            if now.saturating_sub(self.open_bus_refresh[i]) > OPEN_BUS_DECAY_PPU_DOTS {
                v &= !(1 << i);
            }
        }
        v
    }

    pub fn reset(&mut self) {
        self.ctrl = 0;
        self.mask = 0;
        self.w_latch = false;
        self.data_buffer = 0;
        self.odd_frame = false;
        self.scanline = 0;
        self.dot = 0;
        // BG pipeline state isn't explicitly reset on real hardware; a
        // few games write to $2001 during the hidden scanlines expecting
        // stale shifter contents. Zeroing is a reasonable default.
        self.bg_pat_lo = 0;
        self.bg_pat_hi = 0;
        self.bg_attr_lo = 0;
        self.bg_attr_hi = 0;
    }

    pub fn frame(&self) -> u64 {
        self.frame
    }

    pub fn scanline(&self) -> i16 {
        self.scanline
    }

    pub fn dot(&self) -> u16 {
        self.dot
    }

    /// Read-only accessors for debug / diagnostic tooling (frame_dump
    /// binary). Not used by the emulation loop - the `tick` pipeline
    /// mutates these directly.
    pub fn debug_mask(&self) -> u8 {
        self.mask
    }
    pub fn debug_ctrl(&self) -> u8 {
        self.ctrl
    }
    pub fn debug_status(&self) -> u8 {
        self.status
    }
    pub fn debug_scroll(&self) -> (u16, u16, u8) {
        (self.v, self.t, self.fine_x)
    }
    pub fn debug_palette(&self) -> &[u8; 32] {
        &self.palette
    }
    pub fn debug_vram(&self) -> &[u8; 0x800] {
        &self.vram
    }
    pub fn debug_oam(&self) -> &[u8; 256] {
        &self.oam
    }

    /// Advance one PPU dot. On NTSC the bus calls this 3× per CPU cycle;
    /// on PAL the rate averages 3.2× per CPU cycle (the bus handles the
    /// phase). The NES's famously interleaved scanline pipeline is laid
    /// out dot-by-dot here rather than coalesced, so mid-scanline
    /// register writes (status-bar splits, etc.) see the same pipeline
    /// state as real hardware.
    pub fn tick(&mut self, mapper: &mut dyn Mapper) {
        let pre_render = self.region.pre_render_scanline();
        let scanlines = self.region.scanlines_per_frame();
        let rendering = (self.mask & 0x18) != 0;
        let visible = self.scanline >= 0 && self.scanline < FRAME_HEIGHT as i16;
        let is_pre = self.scanline == pre_render;

        // VBlank / status flag edges.
        if self.scanline == VBLANK_SCANLINE && self.dot == 1 {
            if !self.prevent_vbl {
                self.status |= 0x80;
                // VBL set + NMI enabled raises the NMI level signal
                // (Mesen2 `TriggerNmi`, NesPpu.cpp:1254-1258). The
                // bus's end-of-cycle edge detector latches this into
                // `bus.nmi_pending`. A $2002 read in the SAME cycle
                // clears this flag before the detector runs, which is
                // how real hardware suppresses the NMI for that frame.
                if (self.ctrl & 0x80) != 0 {
                    self.nmi_flag = true;
                }
            }
            // `prevent_vbl` is a one-shot: it applies only to this
            // frame's VBL-set tick. Whether we honored it or the flag
            // never set anyway (status already clear), reset so the
            // next frame can set normally.
            self.prevent_vbl = false;
        }
        if is_pre && self.dot == 1 {
            // Clear VBlank, sprite-0 hit, sprite overflow, and lower
            // NMI level. Matches Mesen2 `ProcessScanline` case for
            // scanline=-1, cycle=1 (NesPpu.cpp:889-892).
            self.status &= !0xE0;
            self.nmi_flag = false;
        }

        // Sample the odd-frame-skip decision at the START of dot 339
        // processing, mirroring Mesen2 (`NesPpu.cpp:946-954` - the
        // check lives inside the cycle-339 ProcessTileLoading branch,
        // before any subsequent advance). Consumed at end-of-tick to
        // skip dot 340.
        // Use the 1-tick-delayed `rendering_enabled` (not `self.mask`
        // directly) so a `$2001` write landing in the same CPU cycle
        // as dot 339 does NOT influence this dot's skip decision -
        // matches Mesen2's `UpdateState` deferral (NesPpu.cpp:1425).
        if self.region == Region::Ntsc
            && is_pre
            && self.dot == 339
            && self.odd_frame
            && self.rendering_enabled
        {
            self.skip_last_dot_latched = true;
        }

        // Order per dot on real hardware (Mesen2 NesPpu.cpp:867–957):
        //   1. LoadTileInfo (includes BG shifter reload at case 1).
        //   2. DrawPixel.
        //   3. ShiftTileRegisters.
        // Reload-before-draw is critical: at dot 1 the newly-loaded
        // tile must sit in bits 7–0 before the 8 subsequent shifts
        // walk it into bits 15–8 by dot 9. With reload-after-draw the
        // shifter accumulates only 7 shifts before dot 9 reads bit 15,
        // producing the "1-pixel right shift" seen as a thin vertical
        // line at tile boundaries.

        if rendering && (visible || is_pre) {
            // --- (1) BG fetch / shifter reload / increments ---
            let fetch_region = (self.dot >= 1 && self.dot <= 256)
                || (self.dot >= 321 && self.dot <= 336);
            if fetch_region {
                match (self.dot - 1) % 8 {
                    0 => {
                        // Reload shifter's low byte with the pat data
                        // fetched during the previous 8-cycle group.
                        // After 8 prior shifts the low byte is 0, so
                        // MASK+OR here is equivalent to Mesen2's `|=`.
                        self.reload_bg_shifters();
                        let addr = 0x2000 | (self.v & 0x0FFF);
                        self.bg_next_nt =
                            self.ppu_bus_read(addr, PpuFetchKind::BgNametable, mapper);
                    }
                    2 => {
                        let at_addr = 0x23C0
                            | (self.v & 0x0C00)
                            | ((self.v >> 4) & 0x38)
                            | ((self.v >> 2) & 0x07);
                        let at_byte =
                            self.ppu_bus_read(at_addr, PpuFetchKind::BgAttribute, mapper);
                        // Pre-extract the 2-bit palette selector for
                        // this tile's quadrant so the reload step
                        // doesn't need v after it's been incremented.
                        let shift = ((self.v >> 4) & 4) | (self.v & 2);
                        self.bg_next_attr_bits = ((at_byte >> shift) & 3) as u8;
                    }
                    4 => {
                        let addr = self.bg_pattern_addr(self.bg_next_nt);
                        self.bg_next_pat_lo =
                            self.ppu_bus_read(addr, PpuFetchKind::BgPattern, mapper);
                    }
                    6 => {
                        let addr = self.bg_pattern_addr(self.bg_next_nt) + 8;
                        self.bg_next_pat_hi =
                            self.ppu_bus_read(addr, PpuFetchKind::BgPattern, mapper);
                    }
                    7 => {
                        self.inc_coarse_x();
                    }
                    _ => {}
                }
            }

            // Per-dot sprite evaluation state machine (dots 1–256).
            // Dots 1–64: clear secondary OAM. Dots 65–256: scan
            // primary OAM for up to 8 in-range sprites for the next
            // scanline, with the 2C02 diagonal-sweep overflow bug on
            // the 9th+ in-range sprite.
            if self.dot >= 1 && self.dot <= 256 {
                self.sprite_eval_tick();
            }
            if self.dot == 256 {
                self.inc_y();
            }
            if self.dot == 257 {
                // Horizontal v ← t copy (coarse X + NT-select bit 10).
                self.v = (self.v & !0x041F) | (self.t & 0x041F);
            }
            // Sprite pattern fetch across dots 257–320 in eight 8-dot
            // slots, one per sprite. Per Mesen2 (NesPpu.cpp:899–933)
            // and nesdev, each slot issues four 2-cycle memory
            // accesses; the FIRST cycle of each access is when the
            // address goes on the bus (and A12 transitions). Slot 0
            // issues: garbage NT at dot 257, garbage AT at dot 259,
            // sprite pat-lo at dot 261, sprite pat-hi at dot 263.
            //
            // The exact dots matter for MMC3 A12 counter filtering -
            // batching all fetches at dot 257 would collapse 8 A12
            // rises into one, and firing one dot late (dots 258/260/
            // 262/264) shifts the first A12 rise of scanline 0 by
            // exactly one PPU cycle, which shows up as an off-by-one
            // failure on `mmc3_test/4-scanline_timing #3`. The BG
            // fetch block above uses `(dot - 1) % 8 == {0, 2, 4, 6}`
            // - first-dot-of-access semantics - and this block is
            // aligned the same way.
            //
            // OAMADDR is held at 0 throughout this window (nesdev).
            if self.dot >= 257 && self.dot <= 320 {
                self.oam_addr = 0;
                let slot = ((self.dot - 257) / 8) as usize;
                match (self.dot - 257) % 8 {
                    0 => {
                        // Garbage NT fetch - drives A12 low. Tagged
                        // as a SpriteNametable so MMC5's IRQ detector
                        // (sub-C) can ignore it and only count the
                        // real BG NT reads at dots 337/339/1.
                        let addr = 0x2000 | (self.v & 0x0FFF);
                        let _ = self.ppu_bus_read(addr, PpuFetchKind::SpriteNametable, mapper);
                    }
                    2 => {
                        // Garbage AT fetch - drives A12 low.
                        let at_addr = 0x23C0
                            | (self.v & 0x0C00)
                            | ((self.v >> 4) & 0x38)
                            | ((self.v >> 2) & 0x07);
                        let _ =
                            self.ppu_bus_read(at_addr, PpuFetchKind::SpriteAttribute, mapper);
                    }
                    4 => self.fetch_sprite_pattern_slot(slot, false, mapper),
                    6 => self.fetch_sprite_pattern_slot(slot, true, mapper),
                    _ => {}
                }
            }
            if is_pre && self.dot >= 280 && self.dot <= 304 {
                // Vertical v ← t copy (fine Y + coarse Y + NT-select
                // bit 11). Repeated across a range to match hardware.
                self.v = (self.v & !0x7BE0) | (self.t & 0x7BE0);
            }
            // Garbage NT fetches at dots 337 and 339 keep the address
            // bus honest - MMC5 uses these as part of its 3-same-NT
            // scanline signature (tagged BgNametable), MMC3 sees the
            // same A12 timeline it would on hardware.
            if self.dot == 337 || self.dot == 339 {
                let addr = 0x2000 | (self.v & 0x0FFF);
                let _ = self.ppu_bus_read(addr, PpuFetchKind::BgNametable, mapper);
            }
        }

        // --- (2) Pixel output. Uses the shifter state AFTER the
        // reload at match-arm 0 so the freshly-loaded tile's first
        // pixel sits at bit 15 by dot 9 (after 8 shifts from dot 1).
        if visible && self.dot >= 1 && self.dot <= 256 {
            self.render_pixel(rendering);
        }

        // Sprite-eval finalize. Runs AFTER `render_pixel` so the
        // rightmost pixel of the current scanline (dot 256 = column
        // 255) still sees `sprite_count` from the previous
        // scanline's eval_end. If we ran this inside eval_tick at
        // dot 256 (the natural place), `sprite_count` would be
        // updated for the NEXT scanline before the current
        // scanline's last pixel rendered - dropping any sprite at
        // X >= 248 that depended on a slot index >= the new count.
        // Reproduced as a flickering pixel at the right edge in
        // Goemon Gaiden 2's name-entry screen.
        if rendering && (visible || is_pre) && self.dot == 256 {
            self.sprite_eval_end();
        }

        // --- (3) Shift. Per-dot shift-by-1 during rendering dots
        // 1..=256 and pre-fetch dots 322..=336. NOT at dot 337:
        // shifting there would run 9 bits between the dot-329 reload
        // and dot-1 render of the next scanline, dropping the MSB of
        // every "tile 0" and producing a thin vertical line at x=7.
        // Mesen2 uses two shift-by-8 bursts at dots 328/336 for the
        // same 16-bit net advance across the prefetch window.
        if rendering && (visible || is_pre) {
            let shift_now = (self.dot >= 1 && self.dot <= 256)
                || (self.dot >= 322 && self.dot <= 336);
            if shift_now {
                self.shift_bg();
            }
        }

        self.master_ppu_cycle = self.master_ppu_cycle.wrapping_add(1);
        self.dot += 1;
        // NTSC odd-frame dot skip: when rendering is enabled at the
        // START of dot 339 on the pre-render scanline of an odd
        // frame, the last dot (340) is skipped - the pre-render ends
        // at 339 instead of 340. Decision point matches Mesen2
        // `ProcessTileLoading` (NesPpu.cpp:946-954), which samples
        // `IsRenderingEnabled()` inside the cycle-339 branch before
        // any subsequent tick advances. We latch the decision at the
        // top of tick when entering dot 339 (before dot-339
        // processing) so that an intervening mid-cycle `$2001` write
        // that lands AFTER our pre-access dots of the containing CPU
        // cycle does not retroactively flip the decision. Required by
        // `ppu_vbl_nmi/10-even_odd_timing` #3/#5.
        //
        // `self.skip_last_dot_latched` is set above (when entering
        // dot 339) - see the block earlier in tick().
        if self.skip_last_dot_latched {
            self.skip_last_dot_latched = false;
            self.dot = 0;
            self.scanline = 0;
            self.frame = self.frame.wrapping_add(1);
            self.odd_frame = !self.odd_frame;
        } else if self.dot >= DOTS_PER_SCANLINE {
            self.dot = 0;
            self.scanline += 1;
            if self.scanline >= scanlines {
                self.scanline = 0;
                self.frame = self.frame.wrapping_add(1);
                self.odd_frame = !self.odd_frame;
            }
        }

        // Apply any `$2001` write that happened during this PPU cycle
        // to the delayed `rendering_enabled` signal. Done at end of
        // tick so the NEXT tick (and any rendering-dependent decision
        // it makes) sees the new state - matching Mesen2's
        // `UpdateState` running at the end of each Exec cycle
        // (NesPpu.cpp:1361-1362, 1458-1461).
        self.rendering_enabled = (self.mask & 0x18) != 0;
    }

    fn bg_pattern_addr(&self, tile: u8) -> u16 {
        // `$2000` bit 4 picks the BG pattern table (0x0000 or 0x1000).
        let table = ((self.ctrl as u16) & 0x10) << 8;
        let fine_y = (self.v >> 12) & 0x07;
        table | ((tile as u16) << 4) | fine_y
    }

    fn shift_bg(&mut self) {
        self.bg_pat_lo <<= 1;
        self.bg_pat_hi <<= 1;
        self.bg_attr_lo <<= 1;
        self.bg_attr_hi <<= 1;
    }

    fn reload_bg_shifters(&mut self) {
        self.bg_pat_lo = (self.bg_pat_lo & 0xFF00) | self.bg_next_pat_lo as u16;
        self.bg_pat_hi = (self.bg_pat_hi & 0xFF00) | self.bg_next_pat_hi as u16;
        let lo = if (self.bg_next_attr_bits & 1) != 0 { 0xFF } else { 0x00 };
        let hi = if (self.bg_next_attr_bits & 2) != 0 { 0xFF } else { 0x00 };
        self.bg_attr_lo = (self.bg_attr_lo & 0xFF00) | lo;
        self.bg_attr_hi = (self.bg_attr_hi & 0xFF00) | hi;
    }

    fn inc_coarse_x(&mut self) {
        // loopy: if coarse_x == 31, wrap to 0 and flip NT bit 10.
        if (self.v & 0x001F) == 31 {
            self.v &= !0x001F;
            self.v ^= 0x0400;
        } else {
            self.v = self.v.wrapping_add(1);
        }
    }

    fn inc_y(&mut self) {
        // loopy: if fine_y < 7, bump it; else clear fine_y and bump
        // coarse_y (with the 29/31 nametable-toggle quirk).
        if (self.v & 0x7000) != 0x7000 {
            self.v = self.v.wrapping_add(0x1000);
        } else {
            self.v &= !0x7000;
            let mut y = (self.v & 0x03E0) >> 5;
            if y == 29 {
                y = 0;
                self.v ^= 0x0800;
            } else if y == 31 {
                y = 0;
            } else {
                y += 1;
            }
            self.v = (self.v & !0x03E0) | (y << 5);
        }
    }

    fn render_pixel(&mut self, rendering: bool) {
        let x = (self.dot - 1) as usize;
        let y = self.scanline as usize;

        // Rendering-disabled palette peek (Mesen2 DefaultNesPpu.h:24-34,
        // nesdev "Forced blanking"): if rendering is off AND the PPU's
        // internal `v` register points into $3F00-$3FFF, the pixel
        // shown is the palette byte at `v` instead of the normal
        // backdrop. Visual palette tests (blargg full_palette.nes)
        // use this to sweep all 64 entries by writing `v` via $2006
        // during each scanline.
        if !rendering && (self.v & 0x3F00) == 0x3F00 {
            let pal_byte =
                (self.read_palette(0x3F00 | (self.v & 0x1F)) & 0x3F) as usize;
            let [r, g, b] = NES_PALETTE[pal_byte];
            let i = (y * FRAME_WIDTH + x) * 4;
            self.frame_buffer[i] = r;
            self.frame_buffer[i + 1] = g;
            self.frame_buffer[i + 2] = b;
            self.frame_buffer[i + 3] = 0xFF;
            return;
        }

        let bg_enabled = rendering && (self.mask & 0x08) != 0;
        let sp_enabled = rendering && (self.mask & 0x10) != 0;

        // --- BG pixel ---
        let mut bg_pattern: u8 = 0;
        let mut bg_palette: u8 = 0;
        if bg_enabled {
            let clip_bg_left = (self.mask & 0x02) == 0 && x < 8;
            if !clip_bg_left {
                let bit = 15 - self.fine_x;
                let p0 = ((self.bg_pat_lo >> bit) & 1) as u8;
                let p1 = ((self.bg_pat_hi >> bit) & 1) as u8;
                bg_pattern = (p1 << 1) | p0;
                if bg_pattern != 0 {
                    let a0 = ((self.bg_attr_lo >> bit) & 1) as u8;
                    let a1 = ((self.bg_attr_hi >> bit) & 1) as u8;
                    bg_palette = (a1 << 1) | a0;
                }
            }
        }

        // --- Sprite pixel: first opaque sprite in secondary-OAM order ---
        //     PLUS independent sprite-0 opacity sampling for the hit flag.
        //     Per nesdev: sprite-0 hit is "sprite 0 opaque AND BG opaque"
        //     - it is NOT gated on sprite 0 winning the priority mux. If
        //     sprite 3 also covers the pixel and comes first in secondary
        //     OAM, sprite 3 wins the display mux but sprite 0 still sets
        //     the hit flag. NES Open Tournament Golf's boot wait loop
        //     depends on this independence.
        let mut sp_pattern: u8 = 0;
        let mut sp_palette: u8 = 0;
        let mut sp_priority_behind: bool = false;
        let mut sprite0_opaque_here = false;
        if sp_enabled {
            let clip_sp_left = (self.mask & 0x04) == 0 && x < 8;
            if !clip_sp_left {
                let px = x as i16;
                let mut picked = false;
                for i in 0..self.sprite_count as usize {
                    let sx = self.sprite_x[i] as i16;
                    if px >= sx && px < sx + 8 {
                        let col = (px - sx) as u8;
                        let hflip = (self.sprite_attr[i] & 0x40) != 0;
                        let bit = if hflip { col } else { 7 - col };
                        let p0 = (self.sprite_pat_lo[i] >> bit) & 1;
                        let p1 = (self.sprite_pat_hi[i] >> bit) & 1;
                        let pat = (p1 << 1) | p0;
                        if pat != 0 {
                            if self.sprite_is_zero[i] {
                                sprite0_opaque_here = true;
                            }
                            if !picked {
                                sp_pattern = pat;
                                sp_palette = self.sprite_attr[i] & 0x03;
                                sp_priority_behind = (self.sprite_attr[i] & 0x20) != 0;
                                picked = true;
                            }
                            // Continue scanning so sprite-0 opacity is
                            // sampled even when an earlier sprite wins
                            // the priority mux.
                        }
                    }
                }
            }
        }

        // --- Sprite-0 hit (nesdev-correct, independent of priority mux) ---
        // Both rendering enables on, sprite-0 opaque at this pixel, BG
        // opaque at this pixel, x != 255, and if EITHER left-8 clip
        // flag is active the pixel must be in x >= 8.
        if bg_enabled
            && sp_enabled
            && sprite0_opaque_here
            && bg_pattern != 0
            && x != 255
            && (self.status & 0x40) == 0
        {
            let clip_any_left =
                ((self.mask & 0x02) == 0 || (self.mask & 0x04) == 0) && x < 8;
            if !clip_any_left {
                self.status |= 0x40;
            }
        }

        // --- Pixel mux. Sprite priority bit (attr $20) picks BG over
        //     sprite when both are opaque; if BG is transparent the
        //     sprite always wins (backdrop never beats a sprite). ---
        let bg_opaque = bg_pattern != 0;
        let sp_opaque = sp_pattern != 0;
        let color_idx: u8 = if !bg_opaque && !sp_opaque {
            0
        } else if !bg_opaque {
            0x10 | (sp_palette << 2) | sp_pattern
        } else if !sp_opaque {
            (bg_palette << 2) | bg_pattern
        } else if sp_priority_behind {
            (bg_palette << 2) | bg_pattern
        } else {
            0x10 | (sp_palette << 2) | sp_pattern
        };

        let pal_addr = if color_idx == 0 {
            0x3F00
        } else {
            0x3F00 | color_idx as u16
        };
        let pal_byte = (self.read_palette(pal_addr) & 0x3F) as usize;
        let [r, g, b] = NES_PALETTE[pal_byte];
        let i = (y * FRAME_WIDTH + x) * 4;
        self.frame_buffer[i] = r;
        self.frame_buffer[i + 1] = g;
        self.frame_buffer[i + 2] = b;
        self.frame_buffer[i + 3] = 0xFF;
    }

    /// Per-dot sprite evaluation tick. Called once per dot during
    /// rendering-enabled scanlines at dots 1–256. Implements the
    /// 2C02's real state machine including the diagonal-sweep
    /// sprite-overflow bug. Mirrors Mesen2 `ProcessSpriteEvaluation`
    /// (NesPpu.cpp:1004–1130).
    ///
    /// Dots 1–64: clear secondary OAM one byte per even cycle
    /// (primary OAM reads are disabled during this window).
    /// Dots 65–256: alternating odd-cycle read from primary OAM,
    /// even-cycle process. The processor copies Y + 3 bytes of the
    /// first 8 in-range sprites into secondary OAM, sets the
    /// sprite-overflow flag when a 9th in-range sprite is found, and
    /// then continues scanning with the buggy diagonal sweep that
    /// produces the well-known false-positive / false-negative
    /// overflow behavior on real hardware.
    fn sprite_eval_tick(&mut self) {
        let dot = self.dot;

        // Dots 1–64: clear secondary OAM.
        if dot < 65 {
            self.oam_copy_buffer = 0xFF;
            if (dot & 1) == 0 {
                // Even dot: write 0xFF to secondary OAM.
                let idx = ((dot - 1) >> 1) as usize;
                self.secondary_oam[idx] = 0xFF;
            }
            return;
        }

        // Dots 65–256: primary OAM scan.
        if (dot & 1) == 1 {
            // Odd cycle: read from primary OAM. At dot 65, also
            // initialize the state machine from current OAMADDR -
            // this reproduces the behavior where writing $2003 with
            // a non-multiple-of-4 value before eval starts causes
            // misaligned scans (see oam_flicker_test_reenable).
            if dot == 65 {
                self.sprite_eval_start();
            }
            self.oam_copy_buffer = self.oam[self.oam_addr as usize];
            return;
        }

        // Even cycle (66..=256): process the latched byte.
        let height: i16 = if (self.ctrl & 0x20) != 0 { 16 } else { 8 };
        // Eval treats the pre-render scanline as -1 (matches Mesen2's
        // convention). Our internal `scanline` stores pre-render as
        // 261 / 311; map it back to -1 for the range check so sprites
        // on scanline 0 are never wrongly selected during pre-render.
        let sl = if self.scanline == self.region.pre_render_scanline() {
            -1
        } else {
            self.scanline
        };
        let y = self.oam_copy_buffer as i16;

        if self.oam_copy_done {
            // Phase drained - still increment the primary-OAM high
            // cursor so OAMADDR keeps advancing as on real HW. When
            // secondary OAM is full, reads come from it instead (OAM
            // write-disable turns writes into reads).
            self.sprite_addr_h = (self.sprite_addr_h + 1) & 0x3F;
            if self.sec_oam_addr >= 32 {
                self.oam_copy_buffer =
                    self.secondary_oam[(self.sec_oam_addr & 0x1F) as usize];
            }
        } else {
            // Range check gated on `sprite_in_range` so we only latch
            // it on the Y byte of each sprite (the first byte fetched
            // for that sprite).
            if !self.sprite_in_range && sl >= y && sl < y + height {
                self.sprite_in_range = !self.oam_copy_done;
            }

            if self.sec_oam_addr < 32 {
                // Still have secondary-OAM room - copy the byte.
                self.secondary_oam[self.sec_oam_addr as usize] = self.oam_copy_buffer;

                if self.sprite_in_range {
                    if dot == 66 {
                        // The first Y latched into secondary at this
                        // eval was in range: record that sprite 0 is
                        // in the secondary OAM (drives sprite-0 hit).
                        // This fires even when OAMADDR was non-zero at
                        // eval start (Mesen2 NesPpu.cpp:1040–1045).
                        self.sprite_zero_added = true;
                    }
                    self.sprite_addr_l += 1;
                    self.sec_oam_addr += 1;

                    if self.sprite_addr_l >= 4 {
                        self.sprite_addr_h = (self.sprite_addr_h + 1) & 0x3F;
                        self.sprite_addr_l = 0;
                        if self.sprite_addr_h == 0 {
                            self.oam_copy_done = true;
                        }
                    }

                    if (self.sec_oam_addr & 3) == 0 {
                        // Completed 4-byte copy for this sprite.
                        self.sprite_in_range = false;
                        if self.sprite_addr_l != 0 {
                            // Misaligned-start quirk: if the last byte
                            // read (treated as Y) would also be in
                            // range, we keep the low 2 bits of the
                            // address and continue misinterpreting the
                            // next sprite's bytes as Y.
                            let in_range = sl >= y && sl < y + height;
                            if !in_range {
                                self.sprite_addr_l = 0;
                            }
                        }
                    }
                } else {
                    // Not in range - skip to the next sprite.
                    self.sprite_addr_h = (self.sprite_addr_h + 1) & 0x3F;
                    self.sprite_addr_l = 0;
                    if self.sprite_addr_h == 0 {
                        self.oam_copy_done = true;
                    }
                }
            } else {
                // Secondary OAM full - overflow-detect branch.
                // Writes-disabled: reads come back from secondary OAM
                // instead of the just-latched primary byte.
                self.oam_copy_buffer =
                    self.secondary_oam[(self.sec_oam_addr & 0x1F) as usize];

                if self.oam_copy_done {
                    // Move through remaining primary sprites after
                    // overflow processing completed.
                    self.sprite_addr_h = (self.sprite_addr_h + 1) & 0x3F;
                    self.sprite_addr_l = 0;
                } else if self.sprite_in_range {
                    // 9th sprite is actually in range → real overflow.
                    self.status |= 0x20;
                    self.sprite_addr_l += 1;
                    if self.sprite_addr_l == 4 {
                        self.sprite_addr_h = (self.sprite_addr_h + 1) & 0x3F;
                        self.sprite_addr_l = 0;
                    }
                    // The 2C02 "realignment" after overflow: for 3
                    // more sprites it keeps fetching bytes from the
                    // same position, then restarts at a clean index.
                    if self.overflow_bug_counter == 0 {
                        self.overflow_bug_counter = 3;
                    } else {
                        self.overflow_bug_counter -= 1;
                        if self.overflow_bug_counter == 0 {
                            self.oam_copy_done = true;
                            self.sprite_addr_l = 0;
                        }
                    }
                } else {
                    // Diagonal-sweep bug: with secondary full and the
                    // current sprite not in range, the state machine
                    // increments BOTH H and L. This causes the famous
                    // false-positive / false-negative overflow reports
                    // that games like Smurfs and Spy Hunter exploit.
                    self.sprite_addr_h = (self.sprite_addr_h + 1) & 0x3F;
                    self.sprite_addr_l = (self.sprite_addr_l + 1) & 3;
                    if self.sprite_addr_h == 0 {
                        self.oam_copy_done = true;
                    }
                }
            }
        }

        self.oam_addr = (self.sprite_addr_l & 3) | (self.sprite_addr_h << 2);

        // NOTE: sprite_eval_end is intentionally NOT called here.
        // It must run AFTER render_pixel at dot 256, otherwise
        // updating `sprite_count` for the next scanline clobbers
        // the count the rightmost pixel still needs to consult.
        // Caller `tick` invokes `sprite_eval_end` post-render.
    }

    /// Reset eval state at dot 65. Called once per scanline.
    fn sprite_eval_start(&mut self) {
        self.sprite_zero_added = false;
        self.sprite_in_range = false;
        self.sec_oam_addr = 0;
        self.overflow_bug_counter = 0;
        self.oam_copy_done = false;
        self.sprite_addr_h = (self.oam_addr >> 2) & 0x3F;
        self.sprite_addr_l = self.oam_addr & 3;
    }

    /// Finalize eval at dot 256. Commits `sprite_count` and
    /// `sprite_is_zero[0]` so the sprite fetch phase (dots 257–320)
    /// knows which slots to fetch.
    fn sprite_eval_end(&mut self) {
        self.sprite_count = ((self.sec_oam_addr >> 2) as u8).min(8);
        for s in &mut self.sprite_is_zero {
            *s = false;
        }
        if self.sprite_zero_added && self.sprite_count > 0 {
            self.sprite_is_zero[0] = true;
        }
    }

    /// Fetch one sprite slot's pattern byte (low plane when
    /// `high = false`, high plane otherwise). Called at dots 5 and 7
    /// of each 8-dot slot in the 257–320 sprite-fetch window.
    ///
    /// For slots beyond `sprite_count`, we still issue a read against
    /// the sprite pattern table at tile $FF so MMC3's A12 counter
    /// sees the expected rising edge. Returned data for unused slots
    /// is discarded.
    fn fetch_sprite_pattern_slot(&mut self, slot: usize, high: bool, mapper: &mut dyn Mapper) {
        let next = self.scanline + 1;
        let height: i16 = if (self.ctrl & 0x20) != 0 { 16 } else { 8 };
        let (tile, attr, x, y) = if slot < self.sprite_count as usize {
            (
                self.secondary_oam[slot * 4 + 1],
                self.secondary_oam[slot * 4 + 2],
                self.secondary_oam[slot * 4 + 3],
                self.secondary_oam[slot * 4] as i16,
            )
        } else {
            // Dummy fetch for empty slots - tile $FF, Y at next scanline
            // so the fine-y math lands at row 0.
            (0xFFu8, 0u8, 0u8, (next - 1) as i16)
        };

        let row = (next - 1 - y).clamp(0, height - 1);
        let vflip = (attr & 0x80) != 0;
        let fine_y: u16 = if vflip {
            (height - 1 - row) as u16
        } else {
            row as u16
        };
        let addr: u16 = if height == 16 {
            let table = ((tile as u16) & 0x01) << 12;
            let tile_num = (tile as u16) & 0xFE;
            let (tile_off, row_in_tile) = if fine_y < 8 {
                (tile_num, fine_y)
            } else {
                (tile_num + 1, fine_y - 8)
            };
            table | (tile_off << 4) | row_in_tile
        } else {
            let table = ((self.ctrl as u16) & 0x08) << 9;
            table | ((tile as u16) << 4) | fine_y
        };

        let addr = if high { addr + 8 } else { addr };
        // MMC5 only routes through its sprite CHR bank set in 8×16
        // sprite mode; in 8×8 mode a "sprite" pattern fetch is
        // behaviorally a BG-side fetch (same bank set). Baking that
        // decision into the fetch tag keeps the mapper dumb - it
        // simply trusts the kind.
        let kind = if height == 16 {
            PpuFetchKind::SpritePattern
        } else {
            PpuFetchKind::BgPattern
        };
        let byte = self.ppu_bus_read(addr, kind, mapper);
        if slot < self.sprite_count as usize {
            if high {
                self.sprite_pat_hi[slot] = byte;
            } else {
                self.sprite_pat_lo[slot] = byte;
                // Latch attr/x on the first fetch of each slot.
                self.sprite_attr[slot] = attr;
                self.sprite_x[slot] = x;
            }
        }
    }

    pub fn cpu_read(&mut self, addr: u16, mapper: &mut dyn Mapper) -> u8 {
        self.cpu_read_inner(addr, mapper, false)
    }

    /// `cpu_read` variant that the bus calls for the page-cross dummy
    /// read emitted by `abs,X` / `abs,Y` / `(zp),Y`. For `$2007`, real
    /// hardware's aborted read doesn't advance the PPU's internal buffer
    /// state cleanly - the CPU re-reads before the PPU has a chance to
    /// refill from VRAM. Blargg's `dmc_dma_during_read4/double_2007_read`
    /// accepts any of four buckets; returning the current buffer
    /// without advancing `v` / refilling lands us in the `22 33 44 55 66
    /// / 22 33 44 55 66` bucket (CRC `F018C287`). For every other
    /// register, the dummy read has the same side effects as a real
    /// read (`$4016` still shifts; `$2002` still clears VBL/w).
    pub fn cpu_read_dummy(&mut self, addr: u16, mapper: &mut dyn Mapper) -> u8 {
        self.cpu_read_inner(addr, mapper, true)
    }

    fn cpu_read_inner(
        &mut self,
        addr: u16,
        mapper: &mut dyn Mapper,
        is_dummy: bool,
    ) -> u8 {
        let reg = addr & 0x0007;
        match reg {
            0x02 => {
                // $2002 read: bits 5-7 come from PPU status (VBlank,
                // sprite-0, sprite-overflow); bits 0-4 come from the
                // open-bus decay register. The read refreshes bits
                // 5-7 of decay with the status bits but LEAVES BITS
                // 0-4 decaying - matches `ppu_open_bus` tests 6/7.
                //
                // Also clears VBL, clears NMI level, and resets the
                // $2005/$2006 write toggle. Arms `prevent_vbl` when
                // the read lands exactly on the CPU cycle whose
                // post-access dot will raise VBL - Mesen2's
                // `UpdateStatusFlag` `_cycle == 0` branch
                // (NesPpu.cpp:590-593); their `_cycle` is the
                // last-processed dot, so their 0 maps to our
                // next-to-process dot 1.
                let status_high = self.status & 0xE0;
                let bus_low = self.open_bus_decayed() & 0x1F;
                let v = status_high | bus_low;
                self.refresh_open_bus_bits(0xE0, status_high);
                if self.scanline == VBLANK_SCANLINE && self.dot == 1 && (self.status & 0x80) == 0 {
                    self.prevent_vbl = true;
                }
                self.status &= !0x80;
                self.nmi_flag = false;
                self.w_latch = false;
                v
            }
            0x04 => {
                // OAM read. Sprite attribute bytes (OAM[4n+2]) have
                // bits 2-4 unimplemented in hardware and always read
                // as 0; every other byte reads the stored value
                // verbatim. The read refreshes all 8 bits of decay
                // with the returned (masked) value - `ppu_open_bus`
                // tests 10/11.
                let raw = self.oam[self.oam_addr as usize];
                let value = if self.oam_addr & 0x03 == 0x02 {
                    raw & 0xE3
                } else {
                    raw
                };
                self.refresh_open_bus_bits(0xFF, value);
                value
            }
            0x07 if is_dummy => {
                // Dummy read at $2007 mirror: return the current
                // buffer without advancing v or refilling (see
                // `cpu_read_dummy`). Refresh decay with the returned
                // value anyway - matches the read's observable
                // effect on the I/O bus.
                let v = self.data_buffer;
                self.refresh_open_bus_bits(0xFF, v);
                v
            }
            0x07 => {
                let vram_addr = self.v & 0x3FFF;
                let (returned, refresh_mask) = if vram_addr >= 0x3F00 {
                    // Palette read: low 6 bits come from palette,
                    // high 2 bits keep decaying. Buffer refills from
                    // the mirrored nametable byte behind palette RAM.
                    // When $2001 bit 0 (greyscale) is set, the PPU
                    // masks the lower 4 bits of the returned palette
                    // value to zero - AccuracyCoin "Palette RAM
                    // Quirks" #6. Writes are not masked (#7).
                    self.data_buffer =
                        self.ppu_bus_read(vram_addr.wrapping_sub(0x1000), PpuFetchKind::Idle, mapper);
                    let mut palette = self.read_palette(vram_addr) & 0x3F;
                    if self.mask & 0x01 != 0 {
                        palette &= 0x30;
                    }
                    let bus_high = self.open_bus_decayed() & 0xC0;
                    (palette | bus_high, 0x3Fu8)
                } else {
                    // Non-palette read: return buffered byte, refill
                    // buffer from current v. All 8 bits of decay
                    // refreshed with the returned value.
                    let buffered = self.data_buffer;
                    self.data_buffer = self.ppu_bus_read(vram_addr, PpuFetchKind::Idle, mapper);
                    (buffered, 0xFFu8)
                };
                self.refresh_open_bus_bits(refresh_mask, returned);
                self.increment_v();
                // Post-increment v appears on the PPU address bus
                // outside rendering - gives MMC3's A12 watcher
                // another edge to observe.
                mapper.on_ppu_addr(self.v & 0x3FFF, self.master_ppu_cycle, PpuFetchKind::Idle);
                returned
            }
            // Write-only registers ($2000, $2001, $2003, $2005, $2006).
            // Reads return pure decay with NO refresh - `ppu_open_bus`
            // tests 3 and 5.
            _ => self.open_bus_decayed(),
        }
    }

    pub fn cpu_write(&mut self, addr: u16, data: u8, mapper: &mut dyn Mapper) {
        let reg = addr & 0x0007;
        // Every CPU write to a PPU register drives all 8 bits of the
        // I/O bus, so the decay register is fully refreshed with the
        // written value - `ppu_open_bus` test 2.
        self.refresh_open_bus_bits(0xFF, data);
        match reg {
            0x00 => {
                self.ctrl = data;
                self.t = (self.t & 0xF3FF) | (((data as u16) & 0x03) << 10);
                // Mesen2 `SetControlRegister` (NesPpu.cpp:543-550):
                // disabling NMI-on-VBlank immediately clears the NMI
                // level; enabling it while VBlank is already set
                // immediately raises it. Both effects are live inside
                // the current CPU cycle, so the end-of-cycle edge
                // detector sees the post-write state.
                if (data & 0x80) == 0 {
                    self.nmi_flag = false;
                } else if (self.status & 0x80) != 0 {
                    self.nmi_flag = true;
                }
            }
            0x01 => {
                self.mask = data;
            }
            0x03 => {
                self.oam_addr = data;
            }
            0x04 => {
                self.oam[self.oam_addr as usize] = data;
                self.oam_addr = self.oam_addr.wrapping_add(1);
            }
            0x05 => {
                if !self.w_latch {
                    self.t = (self.t & 0xFFE0) | ((data as u16) >> 3);
                    self.fine_x = data & 0x07;
                } else {
                    self.t = (self.t & 0x8FFF) | (((data as u16) & 0x07) << 12);
                    self.t = (self.t & 0xFC1F) | (((data as u16) & 0xF8) << 2);
                }
                self.w_latch = !self.w_latch;
            }
            0x06 => {
                if !self.w_latch {
                    self.t = (self.t & 0x00FF) | (((data as u16) & 0x3F) << 8);
                } else {
                    self.t = (self.t & 0xFF00) | (data as u16);
                    self.v = self.t;
                    // Real hardware reflects the new `v` on the PPU
                    // address bus for a cycle after the second $2006
                    // write, so A12-sensitive mappers (MMC3) observe
                    // the bit-12 toggle. Without this notification the
                    // mmc3_test "clocked via PPUADDR" gate fails.
                    mapper.on_ppu_addr(
                        self.v & 0x3FFF,
                        self.master_ppu_cycle,
                        PpuFetchKind::Idle,
                    );
                }
                self.w_latch = !self.w_latch;
            }
            0x07 => {
                let addr = self.v & 0x3FFF;
                self.ppu_bus_write(addr, data, PpuFetchKind::Idle, mapper);
                self.increment_v();
                // Post-increment `v` is placed on the address bus
                // outside rendering - another A12 opportunity for
                // MMC3 (Mesen2 NesPpu.cpp ProcessPpuDataAccess).
                mapper.on_ppu_addr(self.v & 0x3FFF, self.master_ppu_cycle, PpuFetchKind::Idle);
            }
            _ => {}
        }
    }

    fn increment_v(&mut self) {
        // When the CPU accesses $2007 during rendering on a visible or
        // pre-render scanline, the PPU performs the same coarse-X +
        // fine-Y "end of tile fetch" increment that the rendering
        // pipeline does at dot 256, ignoring the $2000 bit-2 step.
        // AccuracyCoin "$2007 read w/ rendering" #2 gates on this.
        let rendering = (self.mask & 0x18) != 0;
        let pre_render = self.region.pre_render_scanline();
        let on_render_line = (self.scanline >= 0 && self.scanline < FRAME_HEIGHT as i16)
            || self.scanline == pre_render;
        if rendering && on_render_line {
            self.inc_coarse_x();
            self.inc_y();
            return;
        }
        let step: u16 = if (self.ctrl & 0x04) != 0 { 32 } else { 1 };
        self.v = self.v.wrapping_add(step) & 0x7FFF;
    }

    fn ppu_bus_read(&mut self, addr: u16, kind: PpuFetchKind, mapper: &mut dyn Mapper) -> u8 {
        let addr = addr & 0x3FFF;
        // Every address the PPU drives on its bus is a chance for an
        // A12-sensitive mapper (MMC3, MMC5) to count the edge. MMC5
        // also uses the fetch `kind` to route between its BG and
        // sprite CHR bank sets and to detect scanlines via the
        // 3-consecutive-NT-fetch signature.
        mapper.on_ppu_addr(addr, self.master_ppu_cycle, kind);
        match addr {
            0x0000..=0x1FFF => mapper.ppu_read(addr),
            0x2000..=0x3EFF => {
                // Give the mapper first dibs on the nametable byte -
                // MMC5 uses this for `$5105` NT slot mapping,
                // fill-mode, and ExRAM-as-NT. `Default` means use
                // CIRAM via the cart's mirroring() configuration (the
                // pre-MMC5 path).
                let slot = ((addr >> 10) & 0x03) as u8;
                let offset = (addr & 0x03FF) as usize;
                match mapper.ppu_nametable_read(slot, offset as u16) {
                    NametableSource::Default => {
                        let i = self.nametable_index(addr & 0x0FFF, mapper.mirroring());
                        self.vram[i]
                    }
                    NametableSource::CiramA => self.vram[offset],
                    NametableSource::CiramB => self.vram[0x400 + offset],
                    NametableSource::Byte(b) => b,
                }
            }
            0x3F00..=0x3FFF => self.read_palette(addr),
            _ => 0,
        }
    }

    fn ppu_bus_write(&mut self, addr: u16, data: u8, kind: PpuFetchKind, mapper: &mut dyn Mapper) {
        let addr = addr & 0x3FFF;
        mapper.on_ppu_addr(addr, self.master_ppu_cycle, kind);
        match addr {
            0x0000..=0x1FFF => mapper.ppu_write(addr, data),
            0x2000..=0x3EFF => {
                let slot = ((addr >> 10) & 0x03) as u8;
                let offset = (addr & 0x03FF) as usize;
                match mapper.ppu_nametable_write(slot, offset as u16, data) {
                    NametableWriteTarget::Default => {
                        let mirroring = mapper.mirroring();
                        let i = self.nametable_index(addr & 0x0FFF, mirroring);
                        self.vram[i] = data;
                    }
                    NametableWriteTarget::CiramA => self.vram[offset] = data,
                    NametableWriteTarget::CiramB => self.vram[0x400 + offset] = data,
                    NametableWriteTarget::Consumed => {}
                }
            }
            0x3F00..=0x3FFF => self.write_palette(addr, data),
            _ => {}
        }
    }

    fn read_palette(&self, addr: u16) -> u8 {
        self.palette[palette_index(addr)]
    }

    fn write_palette(&mut self, addr: u16, data: u8) {
        self.palette[palette_index(addr)] = data & 0x3F;
    }

    fn nametable_index(&self, offset: u16, mirroring: Mirroring) -> usize {
        let table = (offset / 0x0400) as usize;
        let inner = (offset % 0x0400) as usize;
        let mapped = match mirroring {
            Mirroring::Horizontal => match table {
                0 | 1 => 0,
                _ => 1,
            },
            Mirroring::Vertical => table & 1,
            Mirroring::SingleScreenLower => 0,
            Mirroring::SingleScreenUpper => 1,
            Mirroring::FourScreen => table & 1,
        };
        mapped * 0x0400 + inner
    }

    /// Live PPU NMI level signal. The bus samples this at the end of
    /// every CPU cycle; a false→true transition across cycle boundaries
    /// latches `bus.nmi_pending`.
    pub fn nmi_flag(&self) -> bool {
        self.nmi_flag
    }

    pub fn oam_write(&mut self, data: u8) {
        self.oam[self.oam_addr as usize] = data;
        self.oam_addr = self.oam_addr.wrapping_add(1);
    }

    /// Capture the PPU's full live state into a serde-friendly
    /// shadow struct. Excludes `region` (re-derived from the bus on
    /// apply) and `frame_buffer` (presentation-only - the next render
    /// pass repopulates it).
    pub(crate) fn save_state_capture(&self) -> crate::save_state::PpuSnap {
        crate::save_state::PpuSnap {
            scanline: self.scanline,
            dot: self.dot,
            frame: self.frame,
            odd_frame: self.odd_frame,
            master_ppu_cycle: self.master_ppu_cycle,
            ctrl: self.ctrl,
            mask: self.mask,
            status: self.status,
            oam_addr: self.oam_addr,
            w_latch: self.w_latch,
            t: self.t,
            v: self.v,
            fine_x: self.fine_x,
            data_buffer: self.data_buffer,
            skip_last_dot_latched: self.skip_last_dot_latched,
            rendering_enabled: self.rendering_enabled,
            nmi_flag: self.nmi_flag,
            prevent_vbl: self.prevent_vbl,
            oam: self.oam,
            palette: self.palette,
            vram: self.vram,
            bg_next_nt: self.bg_next_nt,
            bg_next_attr_bits: self.bg_next_attr_bits,
            bg_next_pat_lo: self.bg_next_pat_lo,
            bg_next_pat_hi: self.bg_next_pat_hi,
            bg_pat_lo: self.bg_pat_lo,
            bg_pat_hi: self.bg_pat_hi,
            bg_attr_lo: self.bg_attr_lo,
            bg_attr_hi: self.bg_attr_hi,
            secondary_oam: self.secondary_oam,
            sprite_count: self.sprite_count,
            sprite_pat_lo: self.sprite_pat_lo,
            sprite_pat_hi: self.sprite_pat_hi,
            sprite_attr: self.sprite_attr,
            sprite_x: self.sprite_x,
            sprite_is_zero: self.sprite_is_zero,
            oam_copy_buffer: self.oam_copy_buffer,
            sec_oam_addr: self.sec_oam_addr,
            sprite_addr_h: self.sprite_addr_h,
            sprite_addr_l: self.sprite_addr_l,
            oam_copy_done: self.oam_copy_done,
            sprite_in_range: self.sprite_in_range,
            sprite_zero_added: self.sprite_zero_added,
            overflow_bug_counter: self.overflow_bug_counter,
            open_bus: self.open_bus,
            open_bus_refresh: self.open_bus_refresh,
        }
    }

    pub(crate) fn save_state_apply(&mut self, snap: crate::save_state::PpuSnap) {
        self.scanline = snap.scanline;
        self.dot = snap.dot;
        self.frame = snap.frame;
        self.odd_frame = snap.odd_frame;
        self.master_ppu_cycle = snap.master_ppu_cycle;
        self.ctrl = snap.ctrl;
        self.mask = snap.mask;
        self.status = snap.status;
        self.oam_addr = snap.oam_addr;
        self.w_latch = snap.w_latch;
        self.t = snap.t;
        self.v = snap.v;
        self.fine_x = snap.fine_x;
        self.data_buffer = snap.data_buffer;
        self.skip_last_dot_latched = snap.skip_last_dot_latched;
        self.rendering_enabled = snap.rendering_enabled;
        self.nmi_flag = snap.nmi_flag;
        self.prevent_vbl = snap.prevent_vbl;
        self.oam = snap.oam;
        self.palette = snap.palette;
        self.vram = snap.vram;
        self.bg_next_nt = snap.bg_next_nt;
        self.bg_next_attr_bits = snap.bg_next_attr_bits;
        self.bg_next_pat_lo = snap.bg_next_pat_lo;
        self.bg_next_pat_hi = snap.bg_next_pat_hi;
        self.bg_pat_lo = snap.bg_pat_lo;
        self.bg_pat_hi = snap.bg_pat_hi;
        self.bg_attr_lo = snap.bg_attr_lo;
        self.bg_attr_hi = snap.bg_attr_hi;
        self.secondary_oam = snap.secondary_oam;
        self.sprite_count = snap.sprite_count;
        self.sprite_pat_lo = snap.sprite_pat_lo;
        self.sprite_pat_hi = snap.sprite_pat_hi;
        self.sprite_attr = snap.sprite_attr;
        self.sprite_x = snap.sprite_x;
        self.sprite_is_zero = snap.sprite_is_zero;
        self.oam_copy_buffer = snap.oam_copy_buffer;
        self.sec_oam_addr = snap.sec_oam_addr;
        self.sprite_addr_h = snap.sprite_addr_h;
        self.sprite_addr_l = snap.sprite_addr_l;
        self.oam_copy_done = snap.oam_copy_done;
        self.sprite_in_range = snap.sprite_in_range;
        self.sprite_zero_added = snap.sprite_zero_added;
        self.overflow_bug_counter = snap.overflow_bug_counter;
        self.open_bus = snap.open_bus;
        self.open_bus_refresh = snap.open_bus_refresh;
        // frame_buffer left untouched - next render pass repopulates.
    }
}

fn palette_index(addr: u16) -> usize {
    let i = (addr & 0x001F) as usize;
    match i {
        0x10 | 0x14 | 0x18 | 0x1C => i - 0x10,
        _ => i,
    }
}
