//! Pre-$6000-protocol blargg test reporter.
//!
//! The `blargg_apu_2005.07.30` suite (and a handful of other early
//! blargg ROMs) predates the `$6000`/`DE B0 61` status-byte handshake.
//! These tests report via on-screen text only: a hand-rolled PPU font
//! is uploaded to CHR-RAM at boot, then tile IDs in nametable 0 spell
//! out the result. The last instruction is always a tight `JMP
//! forever:` loop, so our cue for "done" is a stable PC.
//!
//! This module provides:
//!   * `StuckPcDetector` — a cheap heuristic that fires once the CPU
//!     has been trapped in a small PC window for long enough that the
//!     final screen is definitely drawn.
//!   * `read_nametable_ascii` — reads the 32×30 tile layout of
//!     nametable 0 and interprets each tile ID as its ASCII code. This
//!     works because blargg's devcart font uploads pattern `N` to the
//!     glyph for ASCII char `N` (`'0'` lands at tile `$30`, `' '` at
//!     tile `$20`, etc.), which is the simplest PPU text pipeline and
//!     what every ROM in the 2005 suite uses.
//!   * `extract_result_code` — parses "result: N" (or just the first
//!     on-screen digit if no label is present) into a result byte.
//!
//! The CHR-RAM font-upload detail means we do *not* need a separate
//! glyph table: the ROM and the font are in one-to-one agreement. If
//! a future ROM uses a different mapping, the scanner will emit
//! garbage but not crash — `extract_result_code` returns `None` and
//! the caller can fall back to dumping the raw nametable.

use crate::nes::Nes;

/// How many consecutive polls the PC must stay inside a 16-byte
/// window before we call the test "done". Each poll happens every
/// `POLL_INTERVAL_CYCLES` CPU cycles on the caller side, so tuning
/// this is in multiples of roughly 10k cycles. 30 polls ≈ 300k CPU
/// cycles ≈ 10 NTSC frames — long enough for the final screen to be
/// drawn, short enough that a short test doesn't wait too much past
/// its own settling.
pub const STUCK_POLL_THRESHOLD: u32 = 30;

/// A PC is considered "in the same window" if it stays within this
/// many bytes of the first anchor sample. `forever:` loops are
/// typically 3 bytes (`JMP abs`) but a `SEI`/`CLI` prefix can push
/// the window up a bit; 16 bytes is generous.
const STUCK_PC_WINDOW: u16 = 16;

/// Cheap detector for "CPU has reached the test's final loop".
///
/// Sample `cpu.pc` at a regular cadence (from the caller). The first
/// sample becomes the anchor. Subsequent samples inside
/// `STUCK_PC_WINDOW` of the anchor increment the stability counter;
/// any sample that escapes the window resets the anchor.
///
/// Resets cheaply so the caller can reuse it across ROMs.
pub struct StuckPcDetector {
    anchor_pc: Option<u16>,
    stable_polls: u32,
}

impl StuckPcDetector {
    pub fn new() -> Self {
        Self {
            anchor_pc: None,
            stable_polls: 0,
        }
    }

    /// Feed one PC sample. Returns `true` once the PC has stayed put
    /// for `STUCK_POLL_THRESHOLD` consecutive polls.
    pub fn observe(&mut self, pc: u16) -> bool {
        match self.anchor_pc {
            None => {
                self.anchor_pc = Some(pc);
                self.stable_polls = 1;
            }
            Some(anchor) => {
                let distance = pc.wrapping_sub(anchor);
                let reverse = anchor.wrapping_sub(pc);
                if distance < STUCK_PC_WINDOW || reverse < STUCK_PC_WINDOW {
                    self.stable_polls = self.stable_polls.saturating_add(1);
                } else {
                    self.anchor_pc = Some(pc);
                    self.stable_polls = 1;
                }
            }
        }
        self.stable_polls >= STUCK_POLL_THRESHOLD
    }
}

impl Default for StuckPcDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// True if nametable 0 contains at least one printable ASCII byte
/// (letter, digit, punctuation). Used to rule out the case where the
/// PC has stalled in an early delay loop (`sync_apu`'s 30k-cycle
/// delay alone is enough to fool the stuck-PC heuristic), before any
/// text has been drawn.
pub fn nametable_has_text(nes: &Nes) -> bool {
    let vram = nes.bus.ppu.debug_vram();
    // Only scan the 32×30 visible portion of nametable 0.
    for row in 0..30 {
        for col in 0..32 {
            let b = vram[row * 32 + col];
            if matches!(b, 0x21..=0x7E) {
                return true;
            }
        }
    }
    false
}

/// Read nametable 0 (32 columns × 30 rows) and return the text. Each
/// tile byte is emitted as a character: printable ASCII bytes pass
/// through, `$00` becomes `' '` (blargg fills the untouched area with
/// zeroes), everything else is replaced with `.` so garbled output is
/// still visible in logs.
pub fn read_nametable_ascii(nes: &Nes) -> String {
    let vram = nes.bus.ppu.debug_vram();
    let mut out = String::with_capacity(33 * 30);
    for row in 0..30 {
        for col in 0..32 {
            // Nametable 0 starts at CIRAM offset 0.
            let byte = vram[row * 32 + col];
            out.push(tile_byte_to_ascii(byte));
        }
        out.push('\n');
    }
    out
}

fn tile_byte_to_ascii(b: u8) -> char {
    match b {
        0x00 => ' ',
        0x20..=0x7E => b as char,
        _ => '.',
    }
}

/// Pull a result code from scanned text. The 2005-era ROMs use the
/// `report_final_result` routine in blargg's devcart loader, which
/// prints the byte via `debug_byte` as `$hh` (lowercase hex). That's
/// the primary path.
///
/// Fallback: if no `$hh` marker is present, look for a `result: N`
/// label (some ROMs label explicitly), then for the first standalone
/// ASCII digit. These cover every pre-$6000 ROM in the APU suite.
pub fn extract_result_code(text: &str) -> Option<u8> {
    if let Some(byte) = first_hex_byte(text) {
        return Some(byte);
    }
    let lower = text.to_ascii_lowercase();
    if let Some(pos) = lower.find("result") {
        let after = &text[pos..];
        if let Some(d) = after.chars().find(|c| c.is_ascii_digit()) {
            return Some(d.to_digit(10).unwrap() as u8);
        }
    }
    for ch in text.chars() {
        if let Some(d) = ch.to_digit(10) {
            return Some(d as u8);
        }
    }
    None
}

/// Find the first `$hh` token and return its value as a byte. `hh`
/// must be exactly two hex digits (matching `debug_byte`'s fixed
/// two-nibble output) so we don't accidentally eat a longer address
/// like `$1234`.
fn first_hex_byte(text: &str) -> Option<u8> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 2 < bytes.len() {
            let hi = bytes[i + 1];
            let lo = bytes[i + 2];
            let nibble_hi = ascii_hex_digit(hi);
            let nibble_lo = ascii_hex_digit(lo);
            if let (Some(h), Some(l)) = (nibble_hi, nibble_lo) {
                // Reject three-or-more-digit hex runs so `$1234` isn't
                // misread as the byte `$12`.
                let next = bytes.get(i + 3).copied().unwrap_or(0);
                if ascii_hex_digit(next).is_none() {
                    return Some((h << 4) | l);
                }
            }
        }
        i += 1;
    }
    None
}

fn ascii_hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detector_requires_sustained_stability() {
        let mut d = StuckPcDetector::new();
        assert!(!d.observe(0x8000));
        for _ in 1..STUCK_POLL_THRESHOLD - 1 {
            assert!(!d.observe(0x8003));
        }
        assert!(d.observe(0x8003));
    }

    #[test]
    fn detector_resets_on_escape() {
        let mut d = StuckPcDetector::new();
        for _ in 0..STUCK_POLL_THRESHOLD - 1 {
            d.observe(0x8000);
        }
        // Escape the window.
        d.observe(0x9000);
        // One more sample at the new anchor is not enough.
        assert!(!d.observe(0x9002));
    }

    #[test]
    fn detector_tolerates_small_pc_drift() {
        let mut d = StuckPcDetector::new();
        // A 3-byte JMP loop: PC bounces between anchor, anchor+1, anchor+2.
        d.observe(0x8000);
        for i in 0..STUCK_POLL_THRESHOLD {
            // cycle through the loop addresses
            let pc = 0x8000 + (i as u16 % 3);
            let done = d.observe(pc);
            if i == STUCK_POLL_THRESHOLD - 2 {
                assert!(done, "should fire once enough stable samples accumulate");
            }
        }
    }

    #[test]
    fn tile_byte_to_ascii_handles_printable_and_empty() {
        assert_eq!(tile_byte_to_ascii(0x00), ' ');
        assert_eq!(tile_byte_to_ascii(0x31), '1');
        assert_eq!(tile_byte_to_ascii(b'A'), 'A');
        assert_eq!(tile_byte_to_ascii(0xFF), '.');
    }

    #[test]
    fn extract_result_code_reads_labeled_digit() {
        assert_eq!(extract_result_code("result: 1\n"), Some(1));
        assert_eq!(extract_result_code("RESULT 3"), Some(3));
        assert_eq!(extract_result_code("hi\nresult=7 extra"), Some(7));
    }

    #[test]
    fn extract_result_code_falls_back_to_first_digit() {
        assert_eq!(extract_result_code("no label here\n2 beeps"), Some(2));
        assert_eq!(extract_result_code("   nope   "), None);
    }

    #[test]
    fn extract_result_code_prefers_debug_byte_hex() {
        // blargg's devcart: `report_final_result` → `debug_byte` prints
        // the code as `$hh` (two hex digits, lowercase). That must win
        // over the leading `0` that would otherwise fool the digit path.
        assert_eq!(extract_result_code("  $01  \n"), Some(1));
        assert_eq!(extract_result_code("result: $04\n"), Some(4));
        assert_eq!(extract_result_code("  $ff  \n"), Some(0xff));
    }

    #[test]
    fn first_hex_byte_ignores_long_addresses() {
        // A three-or-more digit token like `$1234` is not a
        // debug_byte output; skip it.
        assert_eq!(first_hex_byte("jmp $1234 then $0a"), Some(0x0a));
        assert_eq!(first_hex_byte("pc=$8000"), None);
    }
}
