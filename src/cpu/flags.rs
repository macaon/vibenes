//! 6502 status register. Stored as individual booleans so branch code
//! stays self-documenting; converted to the packed byte when pushed to
//! the stack.
//!
//! Bit layout on the stack (MSB..LSB): N V 1 B D I Z C

#[derive(Debug, Clone, Copy, Default)]
pub struct StatusFlags {
    c: bool, // carry
    z: bool, // zero
    i: bool, // interrupt-disable
    d: bool, // decimal (ADC/SBC ignore this on the 2A03, but we track writes)
    v: bool, // overflow
    n: bool, // negative
}

impl StatusFlags {
    pub fn from_bits(bits: u8) -> Self {
        Self {
            c: (bits & 0x01) != 0,
            z: (bits & 0x02) != 0,
            i: (bits & 0x04) != 0,
            d: (bits & 0x08) != 0,
            v: (bits & 0x40) != 0,
            n: (bits & 0x80) != 0,
        }
    }

    pub fn to_u8(self) -> u8 {
        let mut b = 0u8;
        if self.c {
            b |= 0x01;
        }
        if self.z {
            b |= 0x02;
        }
        if self.i {
            b |= 0x04;
        }
        if self.d {
            b |= 0x08;
        }
        // Bit 4 (B) + Bit 5 (U) handled by push/pop sites.
        if self.v {
            b |= 0x40;
        }
        if self.n {
            b |= 0x80;
        }
        b
    }

    pub fn carry(&self) -> bool {
        self.c
    }
    pub fn zero(&self) -> bool {
        self.z
    }
    pub fn interrupt(&self) -> bool {
        self.i
    }
    pub fn decimal(&self) -> bool {
        self.d
    }
    pub fn overflow(&self) -> bool {
        self.v
    }
    pub fn negative(&self) -> bool {
        self.n
    }

    pub fn set_carry(&mut self, v: bool) {
        self.c = v;
    }
    pub fn set_zero(&mut self, v: bool) {
        self.z = v;
    }
    pub fn set_interrupt(&mut self, v: bool) {
        self.i = v;
    }
    pub fn set_decimal(&mut self, v: bool) {
        self.d = v;
    }
    pub fn set_overflow(&mut self, v: bool) {
        self.v = v;
    }
    pub fn set_negative(&mut self, v: bool) {
        self.n = v;
    }
}
