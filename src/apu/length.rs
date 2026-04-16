//! Shared 5-bit length counter used by pulse, triangle, and noise
//! channels.

pub const LENGTH_TABLE: [u8; 32] = [
    10, 254, 20, 2, 40, 4, 80, 6, 160, 8, 60, 10, 14, 12, 26, 14, 12, 16, 24, 18, 48, 20, 96, 22,
    192, 24, 72, 26, 16, 28, 32, 30,
];

#[derive(Debug, Default)]
pub struct LengthCounter {
    counter: u8,
    halt: bool,
    enabled: bool,
}

impl LengthCounter {
    pub fn set_halt(&mut self, halt: bool) {
        self.halt = halt;
    }

    /// `$4015` write of the channel-enable bit. Clearing enable forces
    /// the counter to 0 and prevents reloads; setting enable just opens
    /// the gate (the counter keeps its current value).
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.counter = 0;
        }
    }

    /// Warm-reset equivalent of clearing the `$4015` enable latch without
    /// triggering the "force counter to 0" side effect. On real hardware,
    /// a /RES pulse clears the latch directly — the counter value is
    /// retained (see blargg `apu_reset/len_ctrs_enabled`).
    pub fn clear_enable_latch_only(&mut self) {
        self.enabled = false;
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Load from the 5-bit LLLLL field of a length write. If the half-frame
    /// clock fired on the *same* APU cycle and the counter is already
    /// nonzero, real hardware silently drops the load (blargg length-race
    /// rule); we honor that via `same_cycle_as_clock`.
    pub fn load(&mut self, load_index: u8, same_cycle_as_clock: bool) {
        if !self.enabled {
            return;
        }
        if same_cycle_as_clock && self.counter != 0 {
            return;
        }
        self.counter = LENGTH_TABLE[(load_index & 0x1F) as usize];
    }

    pub fn clock_half_frame(&mut self) {
        if !self.halt && self.counter > 0 {
            self.counter -= 1;
        }
    }

    pub fn value(&self) -> u8 {
        self.counter
    }

    pub fn is_nonzero(&self) -> bool {
        self.counter > 0
    }
}
