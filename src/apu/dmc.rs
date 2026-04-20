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
    /// Mesen2-style `_transferStartDelay` (`DeltaModulationChannel.cpp:
    /// 266-270`): when `$4015` bit 4 enables the channel from an idle
    /// state, the DMA is NOT armed immediately — it's delayed by 2 or
    /// 3 CPU cycles based on the current cycle-count parity. The
    /// buffer-refill path (shift register empties, buffer was empty,
    /// sample still active) bypasses this delay and arms DMA right
    /// away, matching `DeltaModulationChannel.cpp:153-158`.
    ///
    /// `0` = no pending transfer-start delay. `>0` decrements once
    /// per CPU cycle in `tick_cpu`; when it reaches zero the pending
    /// address is moved into `dma_pending` and serviced normally.
    enable_dma_delay: u8,
    enable_dma_addr: u16,

    output: u8,
    enabled: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DmcStepResult {
    pub raised_irq: bool,
}

impl Dmc {
    pub fn new(region: Region) -> Self {
        // Rate tables are published in CPU cycles per bit. The timer below
        // counts down in the `P, P-1, ..., 0, reload` pattern where the
        // reload cycle is the one that shifts — so period must be stored
        // as `table_value - 1` to make one full cycle equal exactly
        // `table_value` CPU cycles. (Mesen2 does the same subtraction.)
        let period = match region {
            Region::Ntsc => DMC_RATES_NTSC[0] - 1,
            Region::Pal => DMC_RATES_PAL[0] - 1,
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
            // Nesdev APU DMC: "8 bits are used up before another sample
            // byte is required" — at reset the counter is at 8, so the
            // first bit-shift won't drain the buffer until a full byte
            // (8 × period = 3424 NTSC cycles at rate 0) has elapsed.
            // Matches Mesen2 `DeltaModulationChannel.cpp:36` and puNES
            // `apu.c` init. Was 0 here; that caused the first drain to
            // fire on the FIRST timer underflow (~428 cycles) instead
            // of after 8 underflows, throwing DMC-period-sensitive
            // tests (`dmc_dma_during_read4/sync_dmc`) off alignment.
            bits_remaining: 8,
            silence: true,
            buffer: None,
            dma_pending: None,
            enable_dma_delay: 0,
            enable_dma_addr: 0,
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

    pub fn set_enabled(&mut self, enabled: bool, cpu_cycle_odd: bool) {
        self.enabled = enabled;
        if !enabled {
            // `$4015` bit 4 = 0: drop the remaining sample and discard any
            // outstanding DMA fetch. Without clearing `dma_pending`, a
            // fetch armed just before the disable would still be serviced
            // by the bus and the byte would land in `buffer`, which the
            // next enable would then shift out — an extra stray sample.
            // The currently-buffered / mid-shift byte is *kept* (hardware
            // lets the current shift register finish naturally).
            self.bytes_remaining = 0;
            self.dma_pending = None;
            self.enable_dma_delay = 0;
        } else if self.bytes_remaining == 0 {
            self.restart_sample_pending(cpu_cycle_odd);
        }
    }

    /// Equivalent of `restart_sample` but arms the DMA with the
    /// Mesen2-style 2/3-cycle transfer-start delay instead of
    /// pending-it-immediately. Used by the `$4015` enable path;
    /// the buffer-refill path stays immediate (see `tick_cpu`).
    fn restart_sample_pending(&mut self, cpu_cycle_odd: bool) {
        self.current_addr = self.sample_addr_start;
        self.bytes_remaining = self.sample_length_cfg;
        if self.buffer.is_none() && self.dma_pending.is_none() && self.bytes_remaining > 0 {
            // Mesen2 `DeltaModulationChannel.cpp:266-270`: even cycle
            // → delay 2, odd cycle → delay 3. Our `tick_cpu`
            // decrements once per CPU cycle so the pending fetch
            // fires exactly N cycles after the enable.
            self.enable_dma_delay = if cpu_cycle_odd { 3 } else { 2 };
            self.enable_dma_addr = self.current_addr;
        }
    }

    /// Warm-reset: behaves like `set_enabled(false)` (channel silenced,
    /// pending DMA dropped) but explicitly documents the nesdev-mandated
    /// preservation of `output` — a DC offset the next $4011 write may
    /// pop against, matching hardware behavior.
    pub fn on_warm_reset(&mut self) {
        self.enabled = false;
        self.bytes_remaining = 0;
        self.dma_pending = None;
        // `output` intentionally preserved.
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
        // See `new`: stored period is the published rate minus one.
        self.period = self.rates()[idx] - 1;
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
        // Apply any pending $4015-enable transfer-start delay first
        // (Mesen2 `ProcessClock`). Once the countdown hits zero, move
        // the deferred address into `dma_pending` so the bus sees it
        // at the next read.
        if self.enable_dma_delay > 0 {
            self.enable_dma_delay -= 1;
            if self.enable_dma_delay == 0
                && self.buffer.is_none()
                && self.dma_pending.is_none()
                && self.bytes_remaining > 0
            {
                self.dma_pending = Some(self.enable_dma_addr);
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ntsc() -> Dmc {
        Dmc::new(Region::Ntsc)
    }

    #[test]
    fn period_is_stored_as_table_value_minus_one() {
        let mut d = ntsc();
        // Rate index 0: published 428 CPU cycles/bit → stored 427.
        d.write_ctrl(0x00);
        assert_eq!(d.period, 427);
        // Rate index 15: published 54 → stored 53.
        d.write_ctrl(0x0F);
        assert_eq!(d.period, 53);
    }

    /// Tick the DMC enough cycles for the Mesen2-style
    /// `enable_dma_delay` (2 on even, 3 on odd) to expire so the
    /// deferred DMA request moves into `dma_pending`. Tests that
    /// care about "after enable, DMA is armed" use this to cross the
    /// delay without manually scheduling many ticks.
    fn tick_past_enable_delay(d: &mut Dmc) {
        for _ in 0..3 {
            d.tick_cpu();
        }
    }

    #[test]
    fn enable_on_empty_arms_dma_after_transfer_start_delay() {
        let mut d = ntsc();
        d.write_sample_len(0x01); // 1*16 + 1 = 17 bytes
        d.write_sample_addr(0x00); // $C000
        assert!(d.take_dma_request().is_none());

        d.set_enabled(true, false);
        // Mesen2's `_transferStartDelay`: DMA is NOT armed immediately.
        assert!(
            d.dma_pending.is_none(),
            "DMA should be deferred by the transfer-start delay"
        );
        tick_past_enable_delay(&mut d);

        let req = d.take_dma_request().expect("DMA armed after delay");
        assert_eq!(req.addr, 0xC000);
        assert_eq!(d.bytes_remaining(), 17);
    }

    #[test]
    fn disable_clears_bytes_and_pending_dma() {
        let mut d = ntsc();
        d.write_sample_len(0x01);
        d.set_enabled(true, false);
        tick_past_enable_delay(&mut d);
        assert!(d.dma_pending.is_some(), "DMA was armed");

        d.set_enabled(false, false);

        assert_eq!(d.bytes_remaining(), 0);
        assert!(
            d.dma_pending.is_none(),
            "disable must discard the pending DMA fetch"
        );
    }

    #[test]
    fn disable_during_transfer_start_delay_cancels_the_arming() {
        // A `$4015` bit-4 clear during the transfer-start window
        // must cancel the pending DMA before it even reaches
        // `dma_pending` (otherwise enable-then-quickly-disable
        // leaks one stray sample).
        let mut d = ntsc();
        d.write_sample_len(0x01);
        d.set_enabled(true, false);
        d.set_enabled(false, false);
        tick_past_enable_delay(&mut d);
        assert!(
            d.dma_pending.is_none(),
            "cancelled enable must not leak a DMA arming"
        );
    }

    #[test]
    fn dma_complete_raises_irq_on_last_byte_when_enabled() {
        let mut d = ntsc();
        d.write_ctrl(0x80); // IRQ enabled, no loop
        d.write_sample_len(0x00); // length = 1 byte
        d.set_enabled(true, false);
        tick_past_enable_delay(&mut d);
        let _ = d.take_dma_request();

        let fire = d.dma_complete(0xAA);

        assert!(fire, "last byte must report IRQ");
        assert_eq!(d.bytes_remaining(), 0);
    }

    #[test]
    fn dma_complete_does_not_raise_irq_when_looping() {
        let mut d = ntsc();
        d.write_ctrl(0xC0); // IRQ enabled, loop set — loop wins, no IRQ
        d.write_sample_len(0x00);
        d.set_enabled(true, false);
        tick_past_enable_delay(&mut d);
        let _ = d.take_dma_request();

        let fire = d.dma_complete(0xAA);

        assert!(!fire, "looped sample must not fire IRQ");
        // restart_sample re-initialised the byte count.
        assert!(d.bytes_remaining() > 0);
    }

    #[test]
    fn sample_addr_wraps_ffff_to_8000() {
        // Drive `dma_complete` directly so we don't need to tick the
        // shift register — the wrap only depends on `current_addr`
        // advancing once per completed DMA.
        let mut d = ntsc();
        d.write_ctrl(0x00);
        d.write_sample_len(0xFF); // 0xFF*16 + 1 = 4081 bytes, plenty
        d.write_sample_addr(0xFF); // start at $C000 + 0xFF*64 = $FFC0
        d.set_enabled(true, false);
        tick_past_enable_delay(&mut d);
        let _ = d.take_dma_request();

        // The 64th completion writes to $FFFF; the 65th must wrap to $8000.
        for _ in 0..64 {
            d.dma_complete(0x55);
        }
        assert_eq!(d.current_addr, 0x8000, "expected wrap to $8000");
    }
}
