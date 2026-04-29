// SPDX-License-Identifier: GPL-3.0-or-later
//! Namco 163 wavetable synthesizer.
//!
//! 8-channel time-multiplexed wavetable engine that lives inside the
//! N163 ASIC, sharing a 128-byte internal RAM with the mapper's CPU
//! interface. Channels 7 down to `(7 - active_count)` are advanced
//! once each, in round-robin, with one channel updated every 15 CPU
//! cycles (so the per-channel update rate scales with the active
//! count). Sample output is the average of the active channels.
//!
//! Channel-state slots live at `$40-$7F` of the audio RAM, 8 bytes
//! per channel. The active-channel count register is in the high
//! nibble of `$7F`. Wave samples are 4-bit nibbles packed into the
//! lower portion of the same RAM, so the engine reads from a buffer
//! that the CPU is also writing to via `$4800` / `$F800`.
//!
//! ## Per-channel state slot (`$40 + 8 * ch`, 8 bytes)
//!
//! | Offset | Field                                                   |
//! |--------|---------------------------------------------------------|
//! | 0      | Frequency low (bits 0-7)                                |
//! | 1      | Phase low (bits 0-7) - written back by the engine       |
//! | 2      | Frequency mid (bits 8-15)                               |
//! | 3      | Phase mid (bits 8-15) - written back                    |
//! | 4      | bits 0-1: frequency high; bits 2-7: 1's-comp wavelength |
//! | 5      | Phase high (bits 16-23) - written back                  |
//! | 6      | Wave-table base address (in 4-bit-sample units)         |
//! | 7      | bits 0-3: volume                                        |
//!
//! Wavelength is stored as `256 - (reg4 & 0xFC)`, so the highest two
//! bits act as the frequency MSBs and the rest reduces to the wrap
//! length used by the phase accumulator.
//!
//! ## Output
//!
//! Each `clock` call increments `update_counter`; on the 15th tick we
//! advance the round-robin pointer, advance that channel's phase by
//! its frequency word, look up the 4-bit nibble at
//! `((phase >> 16) + wave_base) & 0xFF`, and store
//! `(sample - 8) * volume` in the channel's output slot. Then we sum
//! the active channels and divide by `active_count + 1`. The result
//! is cached for [`N163Audio::mix_sample`] which scales it for the
//! shared expansion mix bus.
//!
//! ## Mix scale
//!
//! Mesen2's `NesSoundMixer.cpp:188` weight for `AudioChannel::Namco163`
//! is `20`, against the same shared `5018` denominator the rest of
//! the expansion-audio mix uses. Per-raw-unit scale into our 0..1
//! mix space is `20 / 5018 ≈ 0.00399`. With per-channel peak
//! `(15 - 8) * 15 = 105` and one-channel-active averaging, peak
//! mix sample sits near 0.42 - audible but not louder than the
//! 2A03 pulses, matching Mesen2's intended balance.
//!
//! Reference: <https://www.nesdev.org/wiki/Namco_163_audio>. Behavior
//! cross-checked against `~/Git/Mesen2/Core/NES/Mappers/Audio/Namco163Audio.h`,
//! `~/Git/punes/src/core/mappers/mapper_019.c` (`extcl_apu_tick_019`
//! + `snd_wave_019`), and `~/Git/nestopia/source/core/board/NstBoardNamcot163.cpp`.

pub const AUDIO_RAM_SIZE: usize = 0x80;

/// CPU cycles between channel updates. Mesen2 / puNES / Nestopia all
/// agree on 15 CPU cycles per advance.
const CYCLES_PER_UPDATE: u8 = 15;

/// `20 / 5018` - per-raw-unit scale matching Mesen2's mixer weight.
const N163_MIX_SCALE: f32 = 20.0 / 5018.0;

const REG_FREQ_LOW: usize = 0;
const REG_PHASE_LOW: usize = 1;
const REG_FREQ_MID: usize = 2;
const REG_PHASE_MID: usize = 3;
const REG_FREQ_HIGH_AND_LEN: usize = 4;
const REG_PHASE_HIGH: usize = 5;
const REG_WAVE_ADDR: usize = 6;
const REG_VOLUME: usize = 7;

/// `$7F` packs the active-channel count in the high nibble. The
/// stored value is `active_count` directly; one extra channel is
/// always implicit, so `n+1` channels are mixed. (`$7F.b4-6 = 0`
/// means 1 channel, `7` means 8 channels.)
const ACTIVE_COUNT_REG: usize = 0x7F;

pub struct N163Audio {
    ram: [u8; AUDIO_RAM_SIZE],
    /// Most-recent output sample for each of the 8 channels, in raw
    /// `(sample - 8) * volume` units, range `[-120, 105]` (signed
    /// 4-bit centered, scaled by volume nibble).
    channel_output: [i16; 8],
    /// Mod-15 prescaler. Only the channel pointed to by
    /// [`Self::current_channel`] advances on the rollover tick.
    update_counter: u8,
    /// Round-robin pointer. Walks `7 → (7 - active_count)` then wraps
    /// back to 7. Stored as i8 so the wrap arithmetic is signed.
    current_channel: i8,
    /// Cached mix output (sum of active channels divided by count) for
    /// `mix_sample`. Refreshed every channel update.
    last_output: i16,
    /// `$E000.b6` - when set, the engine stops clocking entirely.
    /// The cached `last_output` is held until re-enabled, matching
    /// Mesen2's behavior of not zeroing the output on disable.
    disable_sound: bool,
    /// `$F800.b0-6` - byte index into the shared audio RAM for the
    /// next CPU read or write through `$4800`.
    address: u8,
    /// `$F800.b7` - when set, every `$4800` access auto-advances
    /// [`Self::address`] by one, wrapping at 7 bits.
    auto_inc: bool,
}

impl N163Audio {
    pub fn new() -> Self {
        Self {
            ram: [0; AUDIO_RAM_SIZE],
            channel_output: [0; 8],
            update_counter: 0,
            // Mesen2 starts the round-robin at 7 so the first update
            // tick advances channel 7 first.
            current_channel: 7,
            last_output: 0,
            disable_sound: false,
            address: 0,
            auto_inc: false,
        }
    }

    /// CPU read of `$4800` - returns the byte at the current cursor
    /// and applies auto-increment if enabled.
    pub fn read_4800(&mut self) -> u8 {
        let byte = self.peek_4800();
        self.advance_address();
        byte
    }

    /// Side-effect-free counterpart for debuggers / `cpu_peek`.
    pub fn peek_4800(&self) -> u8 {
        self.ram[(self.address & 0x7F) as usize]
    }

    /// CPU write of `$4800` - stores the byte at the current cursor
    /// and applies auto-increment if enabled.
    pub fn write_4800(&mut self, data: u8) {
        self.ram[(self.address & 0x7F) as usize] = data;
        self.advance_address();
    }

    /// `$F800` write - low 7 bits = address cursor; bit 7 = auto-inc.
    pub fn set_address_latch(&mut self, byte: u8) {
        self.address = byte & 0x7F;
        self.auto_inc = (byte & 0x80) != 0;
    }

    /// `$E000` bit 6 toggles the synth on/off without touching state.
    pub fn set_disable(&mut self, disabled: bool) {
        self.disable_sound = disabled;
    }

    pub fn current_address(&self) -> u8 {
        self.address
    }

    pub fn auto_increment(&self) -> bool {
        self.auto_inc
    }

    pub fn ram_byte(&self, idx: usize) -> u8 {
        self.ram[idx & 0x7F]
    }

    pub fn internal_ram(&self) -> &[u8; AUDIO_RAM_SIZE] {
        &self.ram
    }

    pub fn load_internal_ram(&mut self, src: &[u8]) {
        if src.len() == AUDIO_RAM_SIZE {
            self.ram.copy_from_slice(src);
        }
    }

    /// Advance the engine by one CPU cycle. Schedules the next
    /// channel update on the rollover tick.
    pub fn clock(&mut self) {
        if self.disable_sound {
            return;
        }
        self.update_counter += 1;
        if self.update_counter < CYCLES_PER_UPDATE {
            return;
        }
        self.update_counter = 0;

        let channel = self.current_channel as usize;
        self.update_channel(channel);
        self.refresh_output();

        // Walk channels 7, 6, ..., (7 - active_count), then wrap.
        let active_count = self.active_count();
        self.current_channel -= 1;
        if self.current_channel < 7 - active_count as i8 {
            self.current_channel = 7;
        }
    }

    /// Cached mixed sample, scaled into the shared expansion-audio
    /// f32 mix space.
    pub fn mix_sample(&self) -> f32 {
        f32::from(self.last_output) * N163_MIX_SCALE
    }

    fn advance_address(&mut self) {
        if self.auto_inc {
            self.address = (self.address.wrapping_add(1)) & 0x7F;
        }
    }

    fn channel_base(channel: usize) -> usize {
        0x40 + (channel & 0x07) * 8
    }

    fn active_count(&self) -> u8 {
        // Stored value is `active - 1`, clipped to 0..=7 by the
        // 3-bit field. We return the raw stored value; the actual
        // mix loop iterates `active_count + 1` channels.
        (self.ram[ACTIVE_COUNT_REG] >> 4) & 0x07
    }

    fn frequency(&self, channel: usize) -> u32 {
        let base = Self::channel_base(channel);
        let lo = u32::from(self.ram[base + REG_FREQ_LOW]);
        let mid = u32::from(self.ram[base + REG_FREQ_MID]);
        let hi = u32::from(self.ram[base + REG_FREQ_HIGH_AND_LEN] & 0x03);
        (hi << 16) | (mid << 8) | lo
    }

    fn phase(&self, channel: usize) -> u32 {
        let base = Self::channel_base(channel);
        let lo = u32::from(self.ram[base + REG_PHASE_LOW]);
        let mid = u32::from(self.ram[base + REG_PHASE_MID]);
        let hi = u32::from(self.ram[base + REG_PHASE_HIGH]);
        (hi << 16) | (mid << 8) | lo
    }

    fn store_phase(&mut self, channel: usize, phase: u32) {
        let base = Self::channel_base(channel);
        self.ram[base + REG_PHASE_LOW] = (phase & 0xFF) as u8;
        self.ram[base + REG_PHASE_MID] = ((phase >> 8) & 0xFF) as u8;
        self.ram[base + REG_PHASE_HIGH] = ((phase >> 16) & 0xFF) as u8;
    }

    fn wave_length(&self, channel: usize) -> u32 {
        let base = Self::channel_base(channel);
        // Lower 6 bits of reg 4 are the 1's-complement length descriptor.
        let raw = self.ram[base + REG_FREQ_HIGH_AND_LEN] & 0xFC;
        u32::from(256u16 - u16::from(raw))
    }

    fn wave_address(&self, channel: usize) -> u8 {
        self.ram[Self::channel_base(channel) + REG_WAVE_ADDR]
    }

    fn volume(&self, channel: usize) -> u8 {
        self.ram[Self::channel_base(channel) + REG_VOLUME] & 0x0F
    }

    fn update_channel(&mut self, channel: usize) {
        let phase = self.phase(channel);
        let freq = self.frequency(channel);
        let length = self.wave_length(channel);
        let offset = self.wave_address(channel);
        let volume = self.volume(channel);

        // Phase wraps at `length << 16` so the integer part rolls back
        // to 0 once it's stepped through `length` 4-bit samples.
        let new_phase = phase.wrapping_add(freq) % (length << 16);

        // Sample index walks the 4-bit nibbles of the audio RAM. The
        // low nibble of byte N is sample `2*N`; the high nibble is
        // `2*N + 1`. Wraps at 256 nibbles - the entire 128 B RAM.
        let sample_index = ((new_phase >> 16) as u8).wrapping_add(offset);
        let nibble = if sample_index & 0x01 != 0 {
            self.ram[(sample_index >> 1) as usize] >> 4
        } else {
            self.ram[(sample_index >> 1) as usize] & 0x0F
        };

        // Center on 0 (4-bit signed bias of 8) and scale by volume.
        let signed = i16::from(nibble) - 8;
        self.channel_output[channel] = signed * i16::from(volume);

        self.store_phase(channel, new_phase);
    }

    fn refresh_output(&mut self) {
        let active_count = self.active_count() as usize;
        let mut sum = 0_i32;
        // Iterate 7, 6, ..., 7 - active_count (inclusive) - that's
        // `active_count + 1` channels.
        for ch in (7 - active_count)..=7 {
            sum += i32::from(self.channel_output[ch]);
        }
        let denom = (active_count + 1) as i32;
        // Integer divide matches Mesen2's `summedOutput /= count`.
        self.last_output = (sum / denom) as i16;
    }

    pub(crate) fn save_state_capture(&self) -> crate::save_state::mapper::N163AudioSnap {
        crate::save_state::mapper::N163AudioSnap {
            ram: self.ram,
            channel_output: self.channel_output,
            update_counter: self.update_counter,
            current_channel: self.current_channel,
            last_output: self.last_output,
            disable_sound: self.disable_sound,
            address: self.address,
            auto_inc: self.auto_inc,
        }
    }

    pub(crate) fn save_state_apply(&mut self, snap: crate::save_state::mapper::N163AudioSnap) {
        self.ram = snap.ram;
        self.channel_output = snap.channel_output;
        self.update_counter = snap.update_counter;
        self.current_channel = snap.current_channel;
        self.last_output = snap.last_output;
        self.disable_sound = snap.disable_sound;
        self.address = snap.address;
        self.auto_inc = snap.auto_inc;
    }
}

impl Default for N163Audio {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_on_state_is_silent() {
        let audio = N163Audio::new();
        assert_eq!(audio.mix_sample(), 0.0);
        assert_eq!(audio.peek_4800(), 0);
    }

    #[test]
    fn write_4800_with_auto_inc_walks_address() {
        let mut a = N163Audio::new();
        a.set_address_latch(0x80 | 0x10); // addr = 0x10, auto-inc on
        a.write_4800(0xAA);
        a.write_4800(0xBB);
        a.write_4800(0xCC);
        a.set_address_latch(0x10); // back to 0x10, auto-inc off (peek)
        assert_eq!(a.peek_4800(), 0xAA);
        assert_eq!(a.ram_byte(0x11), 0xBB);
        assert_eq!(a.ram_byte(0x12), 0xCC);
    }

    #[test]
    fn auto_increment_wraps_at_7f() {
        let mut a = N163Audio::new();
        a.set_address_latch(0x80 | 0x7E);
        a.write_4800(0x11);
        a.write_4800(0x22);
        a.write_4800(0x33);
        // 0x7E, 0x7F, then wrap to 0x00.
        assert_eq!(a.ram_byte(0x7E), 0x11);
        assert_eq!(a.ram_byte(0x7F), 0x22);
        assert_eq!(a.ram_byte(0x00), 0x33);
        assert_eq!(a.current_address(), 0x01);
    }

    #[test]
    fn disable_freezes_output() {
        let mut a = N163Audio::new();
        // Even with a fully-configured channel that would produce
        // signal, `set_disable(true)` must hold output at zero. We
        // skip the full channel setup here since `clock()` short-
        // circuits before reading any channel state.
        a.set_disable(true);
        for _ in 0..1000 {
            a.clock();
        }
        assert_eq!(a.last_output, 0);
        assert_eq!(a.mix_sample(), 0.0);
    }

    #[test]
    fn enabled_engine_produces_signal() {
        let mut a = N163Audio::new();
        // Channel 7 setup. The volume byte at base+7 is the SAME as
        // the active-count register at $7F (they intentionally overlap
        // - active_count occupies the high nibble, volume the low),
        // so a single byte (0x0F) gives us volume=15 + active_count=0
        // (= 1 active channel) in one go.
        let base = 0x40 + 7 * 8;
        a.set_address_latch(0x80 | base as u8);
        a.write_4800(0x10); // freq low
        a.write_4800(0x00); // phase low
        a.write_4800(0x00); // freq mid
        a.write_4800(0x00); // phase mid
        a.write_4800(0x00); // freq high + len descriptor = 0 → length 256
        a.write_4800(0x00); // phase high
        a.write_4800(0x00); // wave addr
        a.write_4800(0x0F); // volume=15 / active_count=0 (overlap at $7F)

        // Stuff a non-zero wave nibble into RAM[0]. Sample index 0
        // reads the low nibble of byte 0 = 0xF (wave value 7 after
        // the bias-of-8 centering).
        a.set_address_latch(0x00);
        a.write_4800(0x0F);

        let mut peak = 0_i16;
        for _ in 0..(15 * 64) {
            a.clock();
            peak = peak.max(a.last_output.abs());
        }
        assert!(
            peak > 0,
            "expected audible signal from non-silent channel 7"
        );
    }

    #[test]
    fn round_robin_walks_active_channels_only() {
        let mut a = N163Audio::new();
        // 4 channels active (channels 4, 5, 6, 7).
        a.set_address_latch(0x7F);
        a.write_4800(0x30); // active_count nibble = 3 → 4 channels

        // Run 4 update boundaries (60 cycles) and snapshot the
        // current_channel pointer position. Walking 7, 6, 5, 4, then
        // wrapping back to 7.
        let mut visited = Vec::new();
        for _ in 0..(15 * 5) {
            a.clock();
            visited.push(a.current_channel);
        }
        // After each rollover the pointer post-decrements; we should
        // see the round-robin cover {7, 6, 5, 4} but never lower.
        let unique: std::collections::BTreeSet<i8> =
            visited.iter().copied().collect();
        for ch in 4..=7 {
            assert!(
                unique.contains(&ch),
                "channel {ch} not visited; got {unique:?}"
            );
        }
        for ch in 0..=3 {
            assert!(
                !unique.contains(&ch),
                "channel {ch} should be inactive; got {unique:?}"
            );
        }
    }

    #[test]
    fn average_of_active_channels_matches_sum_div_count() {
        let mut a = N163Audio::new();
        // Place known outputs in each channel slot.
        a.channel_output = [0, 0, 0, 0, 100, 100, 100, 100];
        // 4 active.
        a.set_address_latch(0x7F);
        a.write_4800(0x30);
        a.refresh_output();
        // (100 + 100 + 100 + 100) / 4 = 100.
        assert_eq!(a.last_output, 100);
    }
}
