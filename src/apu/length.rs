//! Shared 5-bit length counter used by pulse, triangle, and noise
//! channels.
//!
//! Writes to the halt bit and length-reload bits are **staged**, not
//! applied immediately. Each bus write records a pending change; the
//! APU commits the pending changes once per CPU cycle, after the
//! frame counter has had a chance to fire a half-frame clock for the
//! cycle. This preserves two blargg-documented invariants that our
//! old "bus-write applies immediately" model violated:
//!
//! 1. **Halt after clock** (`blargg_apu_2005.07.30/10.len_halt_timing`):
//!    if a channel's halt bit is written on the same cycle as the
//!    half-frame clock, the length counter must still be decremented
//!    by that clock — the halt change takes effect on the *next*
//!    clock, not this one.
//! 2. **Reload ignored during clock** (`.../11.len_reload_timing`):
//!    if the length counter is written on the same cycle as the
//!    half-frame clock AND the counter is non-zero, the reload is
//!    silently dropped (the clock's decrement wins). This is what
//!    the Mesen2 `_previousValue`/`_reloadValue` pair tests by
//!    comparing counter against "what it was when the write ran".

pub const LENGTH_TABLE: [u8; 32] = [
    10, 254, 20, 2, 40, 4, 80, 6, 160, 8, 60, 10, 14, 12, 26, 14, 12, 16, 24, 18, 48, 20, 96, 22,
    192, 24, 72, 26, 16, 28, 32, 30,
];

#[derive(Debug, Default)]
pub struct LengthCounter {
    counter: u8,
    halt: bool,
    enabled: bool,
    /// Staged halt value — a channel `write_ctrl` records the new
    /// halt here; `commit_pending` applies it after any same-cycle
    /// half-frame clock has run. `None` = no change pending.
    pending_halt: Option<bool>,
    /// Staged length-counter reload (pre-table index). `commit_pending`
    /// drops it if the counter has changed between write and commit
    /// (which means a half-frame clock fired on the same cycle as
    /// the write and decremented a non-zero counter). `None` = no
    /// change pending.
    pending_reload: Option<u8>,
    /// Counter snapshot at the moment `stage_reload` was called, used
    /// to detect a decrement between write and commit. Only
    /// meaningful while `pending_reload.is_some()`.
    counter_at_write: u8,
}

impl LengthCounter {
    /// Stage a halt-bit change. Applied at end-of-cycle by
    /// `commit_pending`, after any same-cycle half-frame clock ran.
    pub fn stage_halt(&mut self, halt: bool) {
        self.pending_halt = Some(halt);
    }

    /// `$4015` write of the channel-enable bit. Clearing enable forces
    /// the counter to 0 and prevents reloads; setting enable just opens
    /// the gate (the counter keeps its current value). Clearing also
    /// drops any pending reload — a disabled channel can't load.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.counter = 0;
            self.pending_reload = None;
        }
    }

    /// Warm-reset equivalent of clearing the `$4015` enable latch without
    /// triggering the "force counter to 0" side effect. On real hardware,
    /// a /RES pulse clears the latch directly — the counter value is
    /// retained (see blargg `apu_reset/len_ctrs_enabled`).
    pub fn clear_enable_latch_only(&mut self) {
        self.enabled = false;
    }

    /// Stage a length-reload from the 5-bit LLLLL field of a length
    /// write. Records the current counter so `commit_pending` can drop
    /// the reload if a half-frame clock decrements the counter between
    /// the write and the commit.
    pub fn stage_reload(&mut self, load_index: u8) {
        if !self.enabled {
            return;
        }
        self.pending_reload = Some(load_index & 0x1F);
        self.counter_at_write = self.counter;
    }

    /// Half-frame clock: decrement if not halted and not already zero.
    /// Uses the `halt` value *as of this cycle's start* — a same-cycle
    /// halt write is not visible yet because `commit_pending` runs
    /// strictly after this.
    pub fn clock_half_frame(&mut self) {
        if !self.halt && self.counter > 0 {
            self.counter -= 1;
        }
    }

    /// End-of-cycle commit: apply staged halt and reload. Called by
    /// the APU after `clock_half_frame` has had its chance to fire
    /// for the current cycle.
    pub fn commit_pending(&mut self) {
        if let Some(new_halt) = self.pending_halt.take() {
            self.halt = new_halt;
        }
        if let Some(load_index) = self.pending_reload.take() {
            // Drop the reload if the counter was decremented since the
            // write. That only happens when a half-frame clock fired
            // on the same cycle AND the counter was non-zero at the
            // time of the write.
            if self.enabled && self.counter == self.counter_at_write {
                self.counter = LENGTH_TABLE[load_index as usize];
            }
        }
    }

    pub fn is_nonzero(&self) -> bool {
        self.counter > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> LengthCounter {
        let mut lc = LengthCounter::default();
        lc.set_enabled(true);
        lc
    }

    #[test]
    fn reload_outside_clock_applies() {
        let mut lc = enabled();
        lc.stage_reload(0); // LENGTH_TABLE[0] = 10
        lc.commit_pending();
        assert_eq!(lc.counter, 10);
    }

    #[test]
    fn reload_during_same_cycle_clock_ignored_when_nonzero() {
        // Test-11 scenario 5: write a reload while a half-frame clock
        // fires, counter was non-zero. The clock wins.
        let mut lc = enabled();
        lc.counter = 10;
        lc.stage_reload(4); // LENGTH_TABLE[4] = 40
        lc.clock_half_frame(); // 10 -> 9
        lc.commit_pending();
        assert_eq!(lc.counter, 9, "clock's decrement wins over reload");
    }

    #[test]
    fn reload_during_same_cycle_clock_applies_when_zero() {
        // Test-11 scenario 4: write a reload while a clock fires,
        // counter was zero. Clock decrement does nothing (saturates
        // at 0), so counter == counter_at_write and reload applies.
        let mut lc = enabled();
        lc.counter = 0;
        lc.stage_reload(4);
        lc.clock_half_frame();
        lc.commit_pending();
        assert_eq!(lc.counter, 40);
    }

    #[test]
    fn halt_staged_does_not_prevent_same_cycle_clock() {
        // Test-10 scenario 3: write halt=true on the same cycle as
        // the clock; the clock must still decrement (halt applies AFTER).
        let mut lc = enabled();
        lc.counter = 5;
        lc.halt = false;
        lc.stage_halt(true);
        lc.clock_half_frame();
        lc.commit_pending();
        assert_eq!(lc.counter, 4);
        assert!(lc.halt, "halt applied after the clock");
    }

    #[test]
    fn unhalt_staged_does_not_trigger_same_cycle_clock() {
        // Test-10 scenario 4: write halt=false on the same cycle as
        // the clock; the clock must NOT fire because halt is still
        // true at clock time.
        let mut lc = enabled();
        lc.counter = 5;
        lc.halt = true;
        lc.stage_halt(false);
        lc.clock_half_frame(); // halt still true => no decrement
        lc.commit_pending();
        assert_eq!(lc.counter, 5);
        assert!(!lc.halt);
    }

    #[test]
    fn disabled_channel_drops_pending_reload() {
        let mut lc = enabled();
        lc.stage_reload(0);
        lc.set_enabled(false);
        lc.commit_pending();
        assert_eq!(lc.counter, 0);
    }
}
