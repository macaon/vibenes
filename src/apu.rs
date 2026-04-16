use crate::clock::Region;

/// Minimal 2A03 APU stub. Accepts register writes, reports status as zero,
/// and never asserts IRQ. Real mixing, sequencing, and the frame-counter IRQ
/// will come later; for now we just keep the bus quiet.
#[derive(Debug)]
pub struct Apu {
    region: Region,
    registers: [u8; 0x18],
    status: u8,
    frame_irq: bool,
    cycles: u64,
}

impl Apu {
    pub fn new(region: Region) -> Self {
        Self {
            region,
            registers: [0; 0x18],
            status: 0,
            frame_irq: false,
            cycles: 0,
        }
    }

    pub fn reset(&mut self) {
        self.registers = [0; 0x18];
        self.status = 0;
        self.frame_irq = false;
        self.cycles = 0;
    }

    pub fn tick_cpu_cycle(&mut self) {
        // One CPU cycle of APU work. The APU internally runs at CPU/2 for
        // half-frame/quarter-frame events; we ignore that until sound lands.
        self.cycles = self.cycles.wrapping_add(1);
    }

    pub fn read_status(&mut self) -> u8 {
        let v = self.status;
        self.frame_irq = false;
        self.status &= !0x40;
        v
    }

    pub fn write_reg(&mut self, addr: u16, data: u8) {
        let idx = match addr {
            0x4000..=0x4013 => (addr - 0x4000) as usize,
            0x4015 => 0x15,
            0x4017 => 0x17,
            _ => return,
        };
        self.registers[idx] = data;
        if addr == 0x4015 {
            // Writing $4015 clears the DMC IRQ bit on real hardware; we just
            // mirror the mask into status for now.
            self.status = (self.status & 0xC0) | (data & 0x1F);
        }
        if addr == 0x4017 {
            // Clear frame IRQ if the inhibit bit is set.
            if (data & 0x40) != 0 {
                self.frame_irq = false;
                self.status &= !0x40;
            }
        }
    }

    pub fn irq_line(&self) -> bool {
        self.frame_irq
    }

    pub fn region(&self) -> Region {
        self.region
    }
}
