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

use crate::clock::Region;
use crate::mapper::Mapper;
use crate::rom::Mirroring;

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

    nmi_previous: bool,
    pub nmi_edge: bool,

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

    // --- Sprite-0 hit detection shortcut. Real hardware folds the
    //     sprite-0 hit test into full sprite evaluation + pixel mux;
    //     we pre-fetch just OAM[0]'s pattern at dot 257 for the NEXT
    //     scanline and check per-pixel overlap in `render_pixel`. This
    //     is enough for SMB's status-bar split and Golf's scroll split
    //     without waiting for Phase 6B's full sprite pipeline.
    sprite0_active: bool,
    sprite0_x: u8,
    sprite0_hflip: bool,
    sprite0_pat_lo: u8,
    sprite0_pat_hi: u8,

    pub frame_buffer: Vec<u8>,
    open_bus: u8,
}

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
            nmi_previous: false,
            nmi_edge: false,
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
            sprite0_active: false,
            sprite0_x: 0,
            sprite0_hflip: false,
            sprite0_pat_lo: 0,
            sprite0_pat_hi: 0,
            frame_buffer: vec![0; FRAME_WIDTH * FRAME_HEIGHT * 4],
            open_bus: 0,
        }
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
            self.status |= 0x80;
        }
        if is_pre && self.dot == 1 {
            // Clear VBlank, sprite-0 hit, sprite overflow.
            self.status &= !0xE0;
        }
        self.update_nmi_edge();

        // Pixel output runs on every visible dot so backdrop fills the
        // framebuffer even when rendering is disabled. Only the BG
        // fetch/shift/increment machinery is gated on rendering.
        if visible && self.dot >= 1 && self.dot <= 256 {
            self.render_pixel(rendering);
        }

        if rendering && (visible || is_pre) {
            // BG shift runs on every rendering dot 1..=256 (AFTER the
            // pixel is read out) and on the prefetch dots 322..=337.
            // The shift-after-render order means pixel (x) uses the
            // shifter state built up by the previous dot's shift — if
            // we instead started at dot 2, dots 1 and 2 would render
            // the same pixel and each tile's left column would double.
            let shift_now = (self.dot >= 1 && self.dot <= 256)
                || (self.dot >= 322 && self.dot <= 337);
            if shift_now {
                self.shift_bg();
            }

            // BG fetch dots: 1..=256 (for this scanline) and 321..=336
            // (pre-fetch two tiles of next scanline). Fetch step chosen
            // by (dot-1) % 8.
            let fetch_region = (self.dot >= 1 && self.dot <= 256)
                || (self.dot >= 321 && self.dot <= 336);
            if fetch_region {
                match (self.dot - 1) % 8 {
                    0 => {
                        // At tile start — reload shifters first (except
                        // on dot 1 / 321 which begin a fresh region
                        // with no pending latches).
                        if self.dot != 1 && self.dot != 321 {
                            self.reload_bg_shifters();
                        }
                        let addr = 0x2000 | (self.v & 0x0FFF);
                        self.bg_next_nt = self.ppu_bus_read(addr, mapper);
                    }
                    2 => {
                        let at_addr = 0x23C0
                            | (self.v & 0x0C00)
                            | ((self.v >> 4) & 0x38)
                            | ((self.v >> 2) & 0x07);
                        let at_byte = self.ppu_bus_read(at_addr, mapper);
                        // Pre-extract the 2-bit palette selector for
                        // this tile's quadrant so the reload step
                        // doesn't need v after it's been incremented.
                        let shift = ((self.v >> 4) & 4) | (self.v & 2);
                        self.bg_next_attr_bits = ((at_byte >> shift) & 3) as u8;
                    }
                    4 => {
                        let addr = self.bg_pattern_addr(self.bg_next_nt);
                        self.bg_next_pat_lo = self.ppu_bus_read(addr, mapper);
                    }
                    6 => {
                        let addr = self.bg_pattern_addr(self.bg_next_nt) + 8;
                        self.bg_next_pat_hi = self.ppu_bus_read(addr, mapper);
                    }
                    7 => {
                        self.inc_coarse_x();
                    }
                    _ => {}
                }
            }

            if self.dot == 256 {
                self.inc_y();
            }
            if self.dot == 257 {
                // Horizontal v ← t copy (coarse X + NT-select bit 10).
                self.v = (self.v & !0x041F) | (self.t & 0x041F);
                // Also reload the shifter with the final fetch of this
                // scanline so dot 321+ pre-fetch cycles start clean.
                self.reload_bg_shifters();
                // Pre-fetch sprite-0's pattern for the next scanline.
                // On real hardware sprite pattern fetches happen across
                // dots 257–320; we do only the sprite-0 fetch here as a
                // single lookup. Drives A12 correctly for MMC3.
                self.fetch_sprite0_for_next_scanline(mapper);
            }
            if is_pre && self.dot >= 280 && self.dot <= 304 {
                // Vertical v ← t copy (fine Y + coarse Y + NT-select
                // bit 11). Repeated across a range to match hardware.
                self.v = (self.v & !0x7BE0) | (self.t & 0x7BE0);
            }
            // Garbage NT fetches at dots 337 and 339 keep the address
            // bus honest — MMC5 uses these slots, MMC3 sees the same
            // A12 timeline it would on hardware.
            if self.dot == 337 || self.dot == 339 {
                let addr = 0x2000 | (self.v & 0x0FFF);
                let _ = self.ppu_bus_read(addr, mapper);
            }
            // Reload the second pre-fetched tile into the shifter.
            // Without this, tile 1 of every scanline is never loaded,
            // so pixels 8..=15 of every scanline render as backdrop —
            // the "vertical black gap through the middle of the image"
            // bug users see as holes in SMB's ground. The reload
            // schedule has to cover dot 337 alongside the regular
            // 9/17/.../257 and 329 dots.
            if self.dot == 337 {
                self.reload_bg_shifters();
            }
        }

        self.master_ppu_cycle = self.master_ppu_cycle.wrapping_add(1);
        self.dot += 1;
        if self.dot >= DOTS_PER_SCANLINE {
            self.dot = 0;
            self.scanline += 1;
            if self.scanline >= scanlines {
                self.scanline = 0;
                self.frame = self.frame.wrapping_add(1);
                self.odd_frame = !self.odd_frame;
            }
        }
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
        let bg_enabled = rendering && (self.mask & 0x08) != 0;
        let sp_enabled = rendering && (self.mask & 0x10) != 0;

        // BG pixel selection. The 16-bit shifter's bit (15 - fine_x)
        // is the current screen position; fine_x pans left within the
        // 2-tile visible window.
        let mut bg_pattern: u8 = 0;
        let mut bg_palette: u8 = 0;
        if bg_enabled {
            // Left-column BG clip: when $2001 bit 1 is clear, the first
            // 8 pixels force transparent BG (backdrop shows through).
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

        // Sprite-0 hit: pixel-precise predicate (nesdev / mesen-notes §14).
        //   1. Both BG + sprite rendering enabled.
        //   2. Sprite-0 is active on this scanline (pattern fetched at 257).
        //   3. Current x is within sprite-0's 8-pixel horizontal extent.
        //   4. Both left-8 clip flags must allow the pixel through
        //      (BG clip = $2001.1, sprite clip = $2001.2).
        //   5. Sprite-0 pixel is opaque (pattern != 0).
        //   6. BG pixel is opaque (pattern != 0).
        //   7. x != 255 — real hardware's famous "dot 256" suppression.
        //   8. Sprite-0 hit flag not already latched.
        if bg_enabled
            && sp_enabled
            && self.sprite0_active
            && x >= self.sprite0_x as usize
            && x < self.sprite0_x as usize + 8
            && x != 255
            && (self.status & 0x40) == 0
            && bg_pattern != 0
        {
            let clip_bg_left = (self.mask & 0x02) == 0 && x < 8;
            let clip_sp_left = (self.mask & 0x04) == 0 && x < 8;
            if !clip_bg_left && !clip_sp_left {
                let col = x - self.sprite0_x as usize;
                let col = if self.sprite0_hflip { 7 - col } else { col };
                let sp_bit = (7 - col) as u8;
                let sp_p0 = (self.sprite0_pat_lo >> sp_bit) & 1;
                let sp_p1 = (self.sprite0_pat_hi >> sp_bit) & 1;
                if (sp_p0 | sp_p1) != 0 {
                    self.status |= 0x40;
                }
            }
        }

        let color_idx = if bg_pattern != 0 {
            (bg_palette << 2) | bg_pattern
        } else {
            0
        };
        let pal_addr = if color_idx == 0 { 0x3F00 } else { 0x3F00 | color_idx as u16 };
        let pal_byte = (self.read_palette(pal_addr) & 0x3F) as usize;
        let [r, g, b] = NES_PALETTE[pal_byte];
        let i = (y * FRAME_WIDTH + x) * 4;
        self.frame_buffer[i] = r;
        self.frame_buffer[i + 1] = g;
        self.frame_buffer[i + 2] = b;
        self.frame_buffer[i + 3] = 0xFF;
    }

    /// Pre-fetch sprite-0's pattern row for the *next* scanline. Called
    /// at dot 257 of visible and pre-render scanlines (pre-render sets
    /// up scanline 0). Honors 8×8 vs 8×16 sprite size, vertical flip,
    /// and either pattern table per `$2000` bit 3 (8×8) or tile bit 0
    /// (8×16). H-flip is remembered for per-pixel decoding.
    fn fetch_sprite0_for_next_scanline(&mut self, mapper: &mut dyn Mapper) {
        let next_scanline = self.scanline + 1;
        if next_scanline < 0 || next_scanline >= FRAME_HEIGHT as i16 {
            self.sprite0_active = false;
            return;
        }
        let sy = self.oam[0] as i16;
        let tile = self.oam[1];
        let attr = self.oam[2];
        let sx = self.oam[3];
        let height: i16 = if (self.ctrl & 0x20) != 0 { 16 } else { 8 };

        // Sprite appears on screen at scanlines sy+1 .. sy+height. Our
        // "row within sprite" for the target scanline is next - sy - 1.
        let row = next_scanline - sy - 1;
        if row < 0 || row >= height {
            self.sprite0_active = false;
            return;
        }
        let vflip = (attr & 0x80) != 0;
        let hflip = (attr & 0x40) != 0;
        let fine_y: u16 = if vflip {
            (height - 1 - row) as u16
        } else {
            row as u16
        };

        let addr: u16 = if height == 16 {
            // 8×16: tile bit 0 picks the pattern table; bit 7..1 select
            // a 2-tile pair. fine_y 0..7 → top tile, 8..15 → bottom tile.
            let table = ((tile as u16) & 0x01) << 12;
            let tile_num = (tile as u16) & 0xFE;
            let (tile_off, row_in_tile) = if fine_y < 8 {
                (tile_num, fine_y)
            } else {
                (tile_num + 1, fine_y - 8)
            };
            table | (tile_off << 4) | row_in_tile
        } else {
            // 8×8: $2000 bit 3 picks the sprite pattern table.
            let table = ((self.ctrl as u16) & 0x08) << 9;
            table | ((tile as u16) << 4) | fine_y
        };

        self.sprite0_pat_lo = self.ppu_bus_read(addr, mapper);
        self.sprite0_pat_hi = self.ppu_bus_read(addr + 8, mapper);
        self.sprite0_x = sx;
        self.sprite0_hflip = hflip;
        self.sprite0_active = true;
    }

    fn update_nmi_edge(&mut self) {
        let asserted = (self.ctrl & 0x80) != 0 && (self.status & 0x80) != 0;
        if asserted && !self.nmi_previous {
            self.nmi_edge = true;
        }
        self.nmi_previous = asserted;
    }

    pub fn cpu_read(&mut self, addr: u16, mapper: &mut dyn Mapper) -> u8 {
        let reg = addr & 0x0007;
        let value = match reg {
            0x02 => {
                let v = (self.status & 0xE0) | (self.open_bus & 0x1F);
                self.status &= !0x80;
                self.w_latch = false;
                self.update_nmi_edge();
                v
            }
            0x04 => self.oam[self.oam_addr as usize],
            0x07 => {
                let addr = self.v & 0x3FFF;
                let result = if addr >= 0x3F00 {
                    self.data_buffer = self.ppu_bus_read(addr.wrapping_sub(0x1000), mapper);
                    self.read_palette(addr)
                } else {
                    let buffered = self.data_buffer;
                    self.data_buffer = self.ppu_bus_read(addr, mapper);
                    buffered
                };
                self.increment_v();
                result
            }
            _ => self.open_bus,
        };
        self.open_bus = value;
        value
    }

    pub fn cpu_write(&mut self, addr: u16, data: u8, mapper: &mut dyn Mapper) {
        let reg = addr & 0x0007;
        self.open_bus = data;
        match reg {
            0x00 => {
                self.ctrl = data;
                self.t = (self.t & 0xF3FF) | (((data as u16) & 0x03) << 10);
                self.update_nmi_edge();
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
                }
                self.w_latch = !self.w_latch;
            }
            0x07 => {
                let addr = self.v & 0x3FFF;
                self.ppu_bus_write(addr, data, mapper);
                self.increment_v();
            }
            _ => {}
        }
    }

    fn increment_v(&mut self) {
        let step: u16 = if (self.ctrl & 0x04) != 0 { 32 } else { 1 };
        self.v = self.v.wrapping_add(step) & 0x7FFF;
    }

    fn ppu_bus_read(&mut self, addr: u16, mapper: &mut dyn Mapper) -> u8 {
        let addr = addr & 0x3FFF;
        // Every address the PPU drives on its bus is a chance for an
        // A12-sensitive mapper (MMC3, MMC5) to count the edge.
        mapper.on_ppu_addr(addr, self.master_ppu_cycle);
        match addr {
            0x0000..=0x1FFF => mapper.ppu_read(addr),
            0x2000..=0x3EFF => {
                let i = self.nametable_index(addr & 0x0FFF, mapper.mirroring());
                self.vram[i]
            }
            0x3F00..=0x3FFF => self.read_palette(addr),
            _ => 0,
        }
    }

    fn ppu_bus_write(&mut self, addr: u16, data: u8, mapper: &mut dyn Mapper) {
        let addr = addr & 0x3FFF;
        mapper.on_ppu_addr(addr, self.master_ppu_cycle);
        match addr {
            0x0000..=0x1FFF => mapper.ppu_write(addr, data),
            0x2000..=0x3EFF => {
                let mirroring = mapper.mirroring();
                let i = self.nametable_index(addr & 0x0FFF, mirroring);
                self.vram[i] = data;
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

    pub fn poll_nmi(&mut self) -> bool {
        let edge = self.nmi_edge;
        self.nmi_edge = false;
        edge
    }

    pub fn oam_write(&mut self, data: u8) {
        self.oam[self.oam_addr as usize] = data;
        self.oam_addr = self.oam_addr.wrapping_add(1);
    }
}

fn palette_index(addr: u16) -> usize {
    let i = (addr & 0x001F) as usize;
    match i {
        0x10 | 0x14 | 0x18 | 0x1C => i - 0x10,
        _ => i,
    }
}
