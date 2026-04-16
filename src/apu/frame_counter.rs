//! APU frame counter / sequencer.
//!
//! Fires quarter-frame and half-frame clocks at specific CPU-cycle
//! offsets from the last reset, and asserts the frame IRQ during mode 0
//! across a 3-cycle window. Timing matches Mesen2's step table
//! (`Core/NES/APU/ApuFrameCounter.h`). `$4017` writes are deferred 3 or 4
//! CPU cycles based on whether the write lands on an even or odd cycle.

use crate::clock::Region;

#[derive(Debug, Default, Clone, Copy)]
pub struct FrameEvent {
    pub quarter: bool,
    pub half: bool,
    pub set_frame_irq: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    FourStep,
    FiveStep,
}

#[derive(Debug)]
pub struct FrameCounter {
    region: Region,
    mode: Mode,
    irq_inhibit: bool,
    /// Cycles since the last sequencer reset. Starts at 0 after reset,
    /// increments every CPU cycle, compared against `step_targets`.
    counter: u64,
    /// Pending `$4017` write waiting for its parity-dependent delay.
    pending_write: Option<PendingWrite>,
    /// Mesen guard: `_blockFrameCounterTick=2`. After the sequencer
    /// clocks a step, additional ticks are suppressed for 2 CPU cycles.
    block_ticks_until: u64,
}

#[derive(Debug, Clone, Copy)]
struct PendingWrite {
    value: u8,
    apply_at: u64,
}

impl FrameCounter {
    pub fn new(region: Region) -> Self {
        // Nesdev: "After reset or power-up, APU acts as if $4017 were
        // written with $00 from 9 to 12 clocks before the first
        // instruction begins." Mesen2 models this as a pending $4017
        // write with a 3-cycle apply delay scheduled at cycle 3.
        // Without this, the first frame IRQ on power fires 3 cycles
        // too early and blargg's 4017_timing measures count=5 instead
        // of count≈8.
        let pending_write = Some(PendingWrite {
            value: 0x00,
            apply_at: 3,
        });
        Self {
            region,
            mode: Mode::FourStep,
            irq_inhibit: false,
            counter: 0,
            pending_write,
            block_ticks_until: 0,
        }
    }

    /// Restart the divider on warm reset. Mesen2 model: the stored mode
    /// (5-step vs 4-step) is preserved, but IRQ-inhibit is forced off
    /// (the test ROM `apu_reset/4017_written` relies on this behavior
    /// — "At reset, $4017 mode is unchanged, but IRQ inhibit flag is
    /// sometimes cleared"). The 3-cycle apply delay kicks in as for a
    /// normal $4017 write.
    pub fn reset_on_cpu_reset(&mut self, cycle: u64) {
        let value = if self.mode == Mode::FiveStep { 0x80 } else { 0 };
        let parity_odd = (cycle & 1) == 1;
        self.write_4017(value, cycle, parity_odd);
    }

    pub fn write_4017(&mut self, value: u8, cycle: u64, parity_odd: bool) {
        // Mesen2's 2-way parity split. Mesen starts counting after the
        // write, so "4 CPU cycles" there means apply on the 4th tick
        // counting from the one immediately after the write. We record
        // the absolute cycle at which to apply, so we subtract 1 to
        // match: apply at write_cycle + 3 (odd) or write_cycle + 2 (even).
        let delay = if parity_odd { 3 } else { 2 };
        self.pending_write = Some(PendingWrite {
            value,
            apply_at: cycle.wrapping_add(delay),
        });
    }

    /// Advance one CPU cycle. Apply any pending `$4017` reset first, then
    /// step the sequencer.
    pub fn tick(&mut self, cycle: u64) -> FrameEvent {
        if let Some(pending) = self.pending_write {
            if pending.apply_at == cycle {
                self.pending_write = None;
                self.mode = if (pending.value & 0x80) != 0 {
                    Mode::FiveStep
                } else {
                    Mode::FourStep
                };
                self.irq_inhibit = (pending.value & 0x40) != 0;
                self.counter = 0;
                self.block_ticks_until = cycle.wrapping_add(2);
                if self.mode == Mode::FiveStep {
                    // Mode-1 fires a half+quarter clock immediately at reset.
                    return FrameEvent {
                        quarter: true,
                        half: true,
                        set_frame_irq: false,
                    };
                }
                return FrameEvent::default();
            }
        }

        if cycle < self.block_ticks_until {
            self.counter = self.counter.wrapping_add(1);
            return FrameEvent::default();
        }

        self.counter = self.counter.wrapping_add(1);

        let mut event = step_event(self.region, self.mode, self.counter);
        if event.set_frame_irq && self.irq_inhibit {
            event.set_frame_irq = false;
        }

        let wrap = mode_period(self.region, self.mode);
        if self.counter >= wrap {
            self.counter = 0;
        }

        event
    }
}

fn mode_period(region: Region, mode: Mode) -> u64 {
    match (region, mode) {
        (Region::Ntsc, Mode::FourStep) => 29830,
        (Region::Ntsc, Mode::FiveStep) => 37282,
        (Region::Pal, Mode::FourStep) => 33254,
        (Region::Pal, Mode::FiveStep) => 41566,
    }
}

fn step_event(region: Region, mode: Mode, counter: u64) -> FrameEvent {
    let mut ev = FrameEvent::default();
    match region {
        Region::Ntsc => match mode {
            Mode::FourStep => match counter {
                7457 => ev.quarter = true,
                14913 => {
                    ev.quarter = true;
                    ev.half = true;
                }
                22371 => ev.quarter = true,
                29828 => ev.set_frame_irq = true,
                29829 => {
                    ev.quarter = true;
                    ev.half = true;
                    ev.set_frame_irq = true;
                }
                29830 => ev.set_frame_irq = true,
                _ => {}
            },
            Mode::FiveStep => match counter {
                7457 => ev.quarter = true,
                14913 => {
                    ev.quarter = true;
                    ev.half = true;
                }
                22371 => ev.quarter = true,
                37281 => {
                    ev.quarter = true;
                    ev.half = true;
                }
                _ => {}
            },
        },
        Region::Pal => match mode {
            Mode::FourStep => match counter {
                8313 => ev.quarter = true,
                16627 => {
                    ev.quarter = true;
                    ev.half = true;
                }
                24939 => ev.quarter = true,
                33252 => ev.set_frame_irq = true,
                33253 => {
                    ev.quarter = true;
                    ev.half = true;
                    ev.set_frame_irq = true;
                }
                33254 => ev.set_frame_irq = true,
                _ => {}
            },
            Mode::FiveStep => match counter {
                8313 => ev.quarter = true,
                16627 => {
                    ev.quarter = true;
                    ev.half = true;
                }
                24939 => ev.quarter = true,
                41565 => {
                    ev.quarter = true;
                    ev.half = true;
                }
                _ => {}
            },
        },
    }
    ev
}
