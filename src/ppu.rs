use crate::clock::Region;
use crate::mapper::Mapper;
use crate::rom::Mirroring;

pub const FRAME_WIDTH: usize = 256;
pub const FRAME_HEIGHT: usize = 240;

const DOTS_PER_SCANLINE: u16 = 341;
const VBLANK_SCANLINE: i16 = 241;

/// Minimal 2C02 PPU stub: enough to tick the frame timing, raise VBlank +
/// NMI, and service the CPU-visible register window at $2000-$2007. Rendering
/// into the framebuffer is a no-op for now.
pub struct Ppu {
    region: Region,
    scanline: i16,
    dot: u16,
    frame: u64,
    odd_frame: bool,

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

    /// Advance one PPU dot. On NTSC the bus calls this 3× per CPU cycle; on
    /// PAL the rate averages 3.2× per CPU cycle (the bus handles the phase).
    pub fn tick(&mut self, _mapper: &mut dyn Mapper) {
        let pre_render = self.region.pre_render_scanline();
        if self.scanline == VBLANK_SCANLINE && self.dot == 1 {
            self.status |= 0x80;
        }
        if self.scanline == pre_render && self.dot == 1 {
            self.status &= !0xE0;
        }
        self.update_nmi_edge();

        self.dot += 1;
        if self.dot >= DOTS_PER_SCANLINE {
            self.dot = 0;
            self.scanline += 1;
            if self.scanline >= self.region.scanlines_per_frame() {
                self.scanline = 0;
                self.frame = self.frame.wrapping_add(1);
                self.odd_frame = !self.odd_frame;
            }
        }
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
