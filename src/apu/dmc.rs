//! DMC (delta modulation channel).
//!
//! A 7-bit output level that changes by ±2 for each bit shifted out of a
//! shift register. When the shift register empties it reloads from a
//! buffer byte; when the buffer empties the channel requests a DMA from
//! the CPU. Non-looping samples that complete can assert a DMC IRQ.

use crate::clock::Region;

use super::DmcDmaRequest;

const DMC_RATES_NTSC: [u16; 16] = [
    428, 380, 340, 320, 286, 254, 226, 214, 190, 160, 142, 128, 106, 84, 72, 54,
];

const DMC_RATES_PAL: [u16; 16] = [
    398, 354, 316, 298, 276, 236, 210, 198, 176, 148, 132, 118, 98, 78, 66, 50,
];

#[derive(Debug)]
pub struct Dmc {
    region: Region,

    irq_enabled: bool,
    loop_flag: bool,
    period: u16,
    timer: u16,

    sample_addr_start: u16,
    sample_length_cfg: u16,

    current_addr: u16,
    bytes_remaining: u16,

    shift_reg: u8,
    bits_remaining: u8,
    silence: bool,

    buffer: Option<u8>,
    dma_pending: Option<u16>,

    output: u8,
    enabled: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DmcStepResult {
    pub raised_irq: bool,
}

impl Dmc {
    pub fn new(region: Region) -> Self {
        let period = match region {
            Region::Ntsc => DMC_RATES_NTSC[0],
            Region::Pal => DMC_RATES_PAL[0],
        };
        Self {
            region,
            irq_enabled: false,
            loop_flag: false,
            period,
            timer: period,
            sample_addr_start: 0xC000,
            sample_length_cfg: 1,
            current_addr: 0xC000,
            bytes_remaining: 0,
            shift_reg: 0,
            bits_remaining: 0,
            silence: true,
            buffer: None,
            dma_pending: None,
            output: 0,
            enabled: false,
        }
    }

    fn rates(&self) -> &'static [u16; 16] {
        match self.region {
            Region::Ntsc => &DMC_RATES_NTSC,
            Region::Pal => &DMC_RATES_PAL,
        }
    }

    pub fn bytes_remaining(&self) -> u16 {
        self.bytes_remaining
    }

    pub fn irq_enabled(&self) -> bool {
        self.irq_enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.bytes_remaining = 0;
        } else if self.bytes_remaining == 0 {
            self.restart_sample();
        }
    }

    fn restart_sample(&mut self) {
        self.current_addr = self.sample_addr_start;
        self.bytes_remaining = self.sample_length_cfg;
        if self.buffer.is_none() && self.dma_pending.is_none() && self.bytes_remaining > 0 {
            self.dma_pending = Some(self.current_addr);
        }
    }

    pub fn write_ctrl(&mut self, data: u8) {
        self.irq_enabled = (data & 0x80) != 0;
        self.loop_flag = (data & 0x40) != 0;
        let idx = (data & 0x0F) as usize;
        self.period = self.rates()[idx];
    }

    pub fn write_output(&mut self, data: u8) {
        self.output = data & 0x7F;
    }

    pub fn write_sample_addr(&mut self, data: u8) {
        self.sample_addr_start = 0xC000u16.wrapping_add((data as u16) << 6);
    }

    pub fn write_sample_len(&mut self, data: u8) {
        self.sample_length_cfg = ((data as u16) << 4).wrapping_add(1);
    }

    pub fn take_dma_request(&mut self) -> Option<DmcDmaRequest> {
        self.dma_pending.take().map(|addr| DmcDmaRequest { addr })
    }

    /// The bus calls this after fulfilling a DMA request. Returns true if
    /// the sample completed and a DMC IRQ should be raised.
    pub fn dma_complete(&mut self, byte: u8) -> bool {
        self.buffer = Some(byte);
        let mut fire_irq = false;
        self.current_addr = if self.current_addr == 0xFFFF {
            0x8000
        } else {
            self.current_addr + 1
        };
        self.bytes_remaining = self.bytes_remaining.saturating_sub(1);
        if self.bytes_remaining == 0 {
            if self.loop_flag {
                self.restart_sample();
            } else if self.irq_enabled {
                fire_irq = true;
            }
        }
        fire_irq
    }

    /// Tick the CPU-rate timer. Shift one bit per timer underflow, refill
    /// the shift register from the buffer when it empties (and request a
    /// DMA to refill the buffer).
    pub fn tick_cpu(&mut self) -> DmcStepResult {
        let result = DmcStepResult::default();
        if self.timer == 0 {
            self.timer = self.period;
            if !self.silence {
                let bit = self.shift_reg & 1;
                if bit == 0 {
                    if self.output >= 2 {
                        self.output -= 2;
                    }
                } else if self.output <= 125 {
                    self.output += 2;
                }
            }
            self.shift_reg >>= 1;
            if self.bits_remaining > 0 {
                self.bits_remaining -= 1;
            }
            if self.bits_remaining == 0 {
                self.bits_remaining = 8;
                match self.buffer.take() {
                    Some(byte) => {
                        self.shift_reg = byte;
                        self.silence = false;
                    }
                    None => {
                        self.silence = true;
                    }
                }
                if self.buffer.is_none() && self.bytes_remaining > 0 && self.dma_pending.is_none()
                {
                    self.dma_pending = Some(self.current_addr);
                }
            }
        } else {
            self.timer -= 1;
        }
        result
    }

    pub fn output(&self) -> u8 {
        self.output
    }
}
